//! The fleet under test: one throwaway `WARDEN_HOME`, an app per feature,
//! each run by its own background supervisor, and wardend.

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Settings every app shares. Short timeouts so faults resolve quickly:
/// the watchdog fires 4 s after the last heartbeat, a drain ends long-lived
/// connections after 1 s.
pub const WATCHDOG_S: u64 = 4;
pub const GRACE_S: u64 = 10;

#[derive(Debug, Clone)]
pub struct AppSpec {
    pub name: &'static str,
    pub port: u16,
    /// Workers when no fault is in progress (`scale` restores it).
    pub count: usize,
    pub standby: usize,
    pub worker_mode: bool,
    pub static_site: bool,
    /// Paths the keep-alive and new-connection clients request (None: no load).
    pub load: Option<(&'static str, &'static str)>,
    /// Long-lived client paths (`/ws`, SSE paths).
    pub longlived: &'static [&'static str],
    /// The chaos app (`bench/chaos/app.ts`): `/flood`, `CHAOS_HEALTH_FAIL`.
    pub chaos_app: bool,
    /// Its worker output goes straight to files (`worker_output = "direct"`).
    pub direct: bool,
    /// Deliberately crash-looped by the `crash-loop` fault.
    pub crashy: bool,
    /// Its working directory is a `current` symlink into `releases/`.
    pub releases: bool,
}

pub struct CliOut {
    /// None: killed after the timeout.
    pub code: Option<i32>,
    pub out: String,
    pub ms: f64,
}

impl CliOut {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
    pub fn brief(&self) -> String {
        let code = self.code.map(|c| c.to_string()).unwrap_or_else(|| "timeout".into());
        let text: String = self.out.trim().chars().take(600).collect();
        format!("exit {code}: {text}")
    }
}

pub struct Fleet {
    pub bin: PathBuf,
    pub home: PathBuf,
    /// The apps' log files (`[logging] file`, `out_file`): a small tmpfs when
    /// one could be mounted.
    pub logs: PathBuf,
    pub tmpfs: bool,
    pub apps: Vec<AppSpec>,
    /// Worker counts the fleet should have when no fault is in progress.
    pub expected: Mutex<BTreeMap<String, usize>>,
    pub env: Vec<(String, String)>,
    /// While this file exists, the crashy app exits at start.
    pub crash_flag: PathBuf,
    /// The config text each app was started with.
    pub good_config: BTreeMap<String, String>,
}

/// Run `cmd`, killing it after `timeout`. Output is stdout + stderr.
pub fn run_cmd(mut cmd: Command, timeout: Duration) -> CliOut {
    let t0 = Instant::now();
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return CliOut { code: None, out: format!("cannot run: {e}"), ms: 0.0 },
    };
    let read = |p: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut p) = p {
                let _ = p.read_to_string(&mut s);
            }
            s
        })
    };
    let ho = read(child.stdout.take().map(|o| Box::new(o) as Box<dyn Read + Send>));
    let he = read(child.stderr.take().map(|e| Box::new(e) as Box<dyn Read + Send>));
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code(),
            Ok(None) if t0.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break None,
        }
    };
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let out = ho.join().unwrap_or_default() + &he.join().unwrap_or_default();
    CliOut { code, out, ms }
}

impl Fleet {
    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(args).env_remove("WARDEN_CONFIG").env_remove("BUN_OPTIONS").current_dir(&self.home);
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c
    }

    pub fn cli(&self, args: &[&str], timeout: Duration) -> CliOut {
        run_cmd(self.command(args), timeout)
    }

    pub fn spec(&self, name: &str) -> Option<&AppSpec> {
        self.apps.iter().find(|a| a.name == name)
    }

    pub fn config_path(&self, name: &str) -> PathBuf {
        self.home.join(format!("{name}.toml"))
    }

    pub fn expected(&self, name: &str) -> usize {
        let base = self.spec(name).map(|a| a.count).unwrap_or(1);
        self.expected.lock().map(|e| e.get(name).copied().unwrap_or(base)).unwrap_or(base)
    }

    pub fn set_expected(&self, name: &str, n: usize) {
        if let Ok(mut e) = self.expected.lock() {
            e.insert(name.to_string(), n);
        }
    }

    /// `warden list --json`: every app's entry (its `status` is null when
    /// the supervisor does not answer).
    pub fn list(&self) -> Result<Vec<Value>, String> {
        let r = self.cli(&["list", "--json"], Duration::from_secs(15));
        if !r.ok() {
            return Err(format!("warden list --json: {}", r.brief()));
        }
        serde_json::from_str::<Value>(&r.out)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .ok_or_else(|| format!("warden list --json printed no JSON array: {}", r.brief()))
    }

    pub fn status(&self, name: &str) -> Option<Value> {
        self.list().ok()?.into_iter().find(|a| a["app"] == name).map(|a| a["status"].clone()).filter(|s| !s.is_null())
    }

    /// `warden daemon status --json`, or None when wardend does not answer.
    pub fn wardend(&self) -> Option<Value> {
        let r = self.cli(&["daemon", "status", "--json"], Duration::from_secs(10));
        if !r.ok() {
            return None;
        }
        serde_json::from_str(&r.out).ok()
    }

    pub fn wardend_pid(&self) -> Option<u32> {
        self.wardend()?["hello"]["pid"].as_u64().map(|p| p as u32)
    }

    /// Why the fleet is not where it should be (empty: every app has all its
    /// workers ready, its standbys available, no rollout running, and wardend
    /// runs and sees every app running).
    pub fn not_ready(&self, list: &[Value], wardend: Option<&Value>) -> Vec<String> {
        let mut why = Vec::new();
        for spec in &self.apps {
            let name = spec.name;
            let Some(entry) = list.iter().find(|a| a["app"] == name) else {
                why.push(format!("{name}: not listed"));
                continue;
            };
            let st = &entry["status"];
            if st.is_null() {
                why.push(format!("{name}: supervisor not answering ({})", entry["error"].as_str().unwrap_or("?")));
                continue;
            }
            if st["stopped"] == true {
                why.push(format!("{name}: stopped"));
            }
            let want = self.expected(name);
            let workers = st["workers"].as_array().cloned().unwrap_or_default();
            if workers.len() != want {
                why.push(format!("{name}: {} workers, expected {want}", workers.len()));
            }
            for w in &workers {
                if w["state"] != "RUNNING" {
                    why.push(format!("{name} worker {}: {}", w["id"], w["state"].as_str().unwrap_or("?")));
                }
            }
            if st["workers_ready"].as_u64() != Some(want as u64) {
                why.push(format!("{name}: {} of {want} ready", st["workers_ready"]));
            }
            if !st["rollout"].is_null() {
                why.push(format!("{name}: {} in progress ({})", st["rollout"]["kind"], st["rollout"]["phase"]));
            }
            if spec.standby > 0 {
                let ready = st["standbys"].as_array().map(|s| s.iter().filter(|x| x["state"] == "STANDBY").count());
                if ready != Some(spec.standby) {
                    let states: Vec<String> = st["standbys"]
                        .as_array()
                        .map(|s| s.iter().map(|x| x["state"].as_str().unwrap_or("?").to_string()).collect())
                        .unwrap_or_default();
                    why.push(format!("{name}: standbys {states:?}, expected {} STANDBY", spec.standby));
                }
            }
            if spec.worker_mode && st["host"].is_null() {
                why.push(format!("{name}: no host process"));
            }
        }
        match wardend {
            None => why.push("wardend: not answering".into()),
            Some(d) => {
                for spec in &self.apps {
                    let a = d["apps"].as_array().and_then(|a| a.iter().find(|x| x["name"] == spec.name));
                    let state = a.and_then(|a| a["state"].as_str()).unwrap_or("absent");
                    if state != "running" {
                        why.push(format!("wardend sees {}: {state}", spec.name));
                    }
                }
            }
        }
        why
    }

    /// Poll until the whole fleet is ready: Ok(seconds waited), or the last
    /// reasons it was not after `bound`.
    pub fn wait_ready(&self, bound: Duration) -> Result<f64, Vec<String>> {
        let t0 = Instant::now();
        let mut last = vec!["never checked".to_string()];
        while t0.elapsed() < bound {
            match self.list() {
                Ok(list) => {
                    let d = self.wardend();
                    last = self.not_ready(&list, d.as_ref());
                    if last.is_empty() {
                        return Ok(t0.elapsed().as_secs_f64());
                    }
                }
                Err(e) => last = vec![e],
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        Err(last)
    }
}

/// A free TCP port on 127.0.0.1 (bound, read, released).
pub fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("finding a free port: {e}"))
}

/// Config text for one app, as sections of `key = value` lines.
pub fn config_text(spec: &AppSpec, home: &Path, logs: &Path, crash_flag: &Path) -> String {
    let mut s: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let q = |p: &Path| format!("{:?}", p.display().to_string());
    let apps = home.join("apps");
    let mut app = vec![format!("name = \"{}\"", spec.name), format!("port = {}", spec.port)];
    let mut workers = vec![format!("count = {}", spec.count)];
    if spec.static_site {
        s.entry("static").or_default().extend([format!("root = {}", q(&home.join("site"))), "io = \"uring\"".into()]);
    } else if spec.worker_mode {
        app.push(format!("entry = {}", q(&apps.join("app.ts"))));
        workers.push("mode = \"worker\"".into());
    } else if spec.releases {
        app.push("args = [\"app.ts\"]".into());
        app.push(format!("working_directory = {}", q(&home.join("releases/current"))));
    } else if spec.name == "api-node" {
        app.push("command = \"node\"".into());
        app.push(format!("args = [{}]", q(&apps.join("longlived_node.mjs"))));
    } else if !spec.chaos_app {
        app.push(format!("args = [{}]", q(&apps.join("longlived.ts"))));
    } else {
        app.push(format!("args = [{}]", q(&apps.join("app.ts"))));
    }
    if spec.crashy {
        app.push(format!("env = {{ CHAOS_CRASH_FILE = {} }}", q(crash_flag)));
        s.entry("restart").or_default().extend([
            "max_restarts = 3".into(),
            "backoff_initial = 50".into(),
            "backoff_max = 200".into(),
            "failed_cooldown = 3600".into(),
        ]);
    } else {
        s.entry("restart").or_default().extend(["backoff_initial = 50".into(), "backoff_max = 2000".into()]);
    }
    if spec.standby > 0 {
        workers.push(format!("standby = {}", spec.standby));
    }
    s.insert("app", app);
    s.insert("workers", workers);
    s.entry("shutdown").or_default().extend([format!("grace_period = {GRACE_S}"), "long_lived_timeout = 1".into()]);
    s.entry("watchdog").or_default().push(format!("timeout = {WATCHDOG_S}"));
    s.entry("reload").or_default().extend([
        "health_passes = 2".into(),
        "health_interval_ms = 300".into(),
        "canary_soak = 2".into(),
        "timeout = 60".into(),
    ]);
    if spec.name == "api-node" {
        s.entry("reload").or_default().push("surge = 2".into());
    }
    if !spec.static_site && !spec.crashy {
        s.entry("health").or_default().extend([
            "enabled = true".into(),
            "path = \"/health\"".into(),
            "interval = 2".into(),
            "timeout = 2".into(),
            "failure_threshold = 3".into(),
            "initial_delay = 2".into(),
        ]);
    }
    // The apps' output goes to the log directory (a tmpfs: the disk-full
    // fault fills it); Warden's own lines go to the supervisor's stdout,
    // the background log in state/logs (on disk), which the report reads.
    // The rotation settings apply to that log too: big enough that it does
    // not rotate during a run (the report must see every line), with a line
    // budget that keeps a flood's share small. Direct output never passes
    // through Warden, so that app's files rotate small (the log-flood fault
    // checks they stay within the bound).
    let mut logging = vec![format!("out_file = {}", q(&logs.join(format!("{}-out.log", spec.name))))];
    if spec.direct {
        logging.extend(["worker_output = \"direct\"".into(), "per_worker_files = true".into()]);
    } else {
        logging.push("max_lines_per_sec = 2000".into());
    }
    s.insert("logging", logging);
    s.insert(
        "logging.rotate",
        vec![format!("max_size = \"{}\"", if spec.direct { "256K" } else { "64M" }), "keep = 1".into()],
    );
    let mut text = "# chaos soak app (cargo xtask chaos); written by xtask/src/chaos/fleet.rs\n".to_string();
    // [app] first, the rest in a stable order.
    for key in std::iter::once("app").chain(s.keys().copied().filter(|k| *k != "app").collect::<Vec<_>>()) {
        if let Some(lines) = s.get(key) {
            text += &format!("\n[{key}]\n{}\n", lines.join("\n"));
        }
    }
    text
}

/// The apps of the soak; ports are picked free.
pub fn specs() -> Result<Vec<AppSpec>, String> {
    let base = AppSpec {
        name: "",
        port: 0,
        count: 2,
        standby: 0,
        worker_mode: false,
        static_site: false,
        load: Some(("/whoami", "/whoami")),
        longlived: &[],
        chaos_app: false,
        direct: false,
        crashy: false,
        releases: false,
    };
    let mut v = vec![
        // Bun, process mode, a hot standby, release pinning.
        AppSpec { name: "api-bun", standby: 1, chaos_app: true, releases: true, ..base.clone() },
        // Node, process mode, surge rollouts, WebSocket and SSE clients.
        AppSpec { name: "api-node", count: 3, longlived: &["/ws", "/sse"], ..base.clone() },
        // Bun.serve WebSockets and every kind of SSE body.
        AppSpec { name: "ws-bun", longlived: &["/ws", "/ws", "/sse", "/sse-direct", "/sse-gen"], ..base.clone() },
        // Bun worker mode (threads in one host process).
        AppSpec { name: "threads", worker_mode: true, chaos_app: true, ..base.clone() },
        // Warden's static file server (cache, io_uring).
        AppSpec { name: "site", static_site: true, load: Some(("/index.html", "/assets/app.js")), ..base.clone() },
        // worker_output = "direct": output spliced into files.
        AppSpec { name: "direct", chaos_app: true, direct: true, ..base.clone() },
        // Crash-looped on purpose; no client load.
        AppSpec { name: "crashy", count: 1, chaos_app: true, crashy: true, load: None, ..base },
    ];
    for a in &mut v {
        a.port = free_port()?;
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_have_one_table_each() {
        let specs = specs().unwrap();
        for s in &specs {
            let t = config_text(s, Path::new("/h"), Path::new("/h/logs"), Path::new("/h/crash"));
            let mut seen = std::collections::HashSet::new();
            for l in t.lines().filter(|l| l.starts_with('[')) {
                assert!(seen.insert(l.to_string()), "{} repeats {l}:\n{t}", s.name);
            }
            assert!(t.contains("[app]") && t.contains(&format!("name = \"{}\"", s.name)), "{t}");
        }
        let node = specs.iter().find(|s| s.name == "api-node").unwrap();
        let t = config_text(node, Path::new("/h"), Path::new("/h/logs"), Path::new("/h/crash"));
        assert!(t.contains("surge = 2") && t.contains("command = \"node\""), "{t}");
    }
}
