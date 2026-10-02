//! `warden migrate-wattpm` run as the real binary against Watt projects built in a temp directory.
//!
//! What comes out is checked the way a user would: every config goes through `warden check -c`,
//! env files are 0600, no secret reaches a config or the report, and a bad input fails with a clear
//! message. Two tests need wattpm 3.71.0 (`npm ci` in bench/): one compares the conversion with what
//! Watt's own config loader resolves, the other starts a real runtime and runs Warden's copy next to it.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_warden");
const RUNTIME: &str = "https://schemas.platformatic.dev/@platformatic/runtime/3.71.0.json";
const NODE: &str = "https://schemas.platformatic.dev/@platformatic/node/3.71.0.json";
const SECRET: &str = "s3cr3t value";

fn bench_modules() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/node_modules")
}

fn have_node() -> bool {
    Command::new("node").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// wattpm 3.71.0 needs Node.js 22.19 or newer (its `engines`).
fn node_runs_wattpm() -> bool {
    let Ok(o) = Command::new("node").arg("--version").output() else { return false };
    let v = String::from_utf8_lossy(&o.stdout);
    let mut parts = v.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    major > 22 || (major == 22 && minor >= 19)
}

fn have_wattpm() -> bool {
    bench_modules().join(".bin/wattpm").exists()
        && bench_modules().join("@platformatic/runtime/index.js").exists()
        && have_node()
        && node_runs_wattpm()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn get(port: u16, path: &str) -> Option<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(1)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    if !buf.starts_with("HTTP/1.1 2") {
        return None;
    }
    buf.split_once("\r\n\r\n").map(|(_, b)| b.to_string())
}

fn wait_for(what: &str, secs: u64, f: impl Fn() -> bool) {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// A scratch directory (short path: Warden's control sockets have a length limit), removed on drop.
struct Dir {
    root: PathBuf,
}

impl Dir {
    fn new(name: &str) -> Dir {
        let root = std::env::temp_dir().join(format!("wm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Dir { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    fn json(&self, rel: &str, v: Value) -> PathBuf {
        self.write(rel, &serde_json::to_string_pretty(&v).unwrap())
    }

    /// An application directory with a Node entry file.
    fn app(&self, rel: &str, main: &str, body: &str) {
        let name = Path::new(rel).file_name().unwrap().to_str().unwrap().to_string();
        self.json(&format!("{rel}/package.json"), json!({"name": name, "type": "module", "main": main}));
        self.json(&format!("{rel}/watt.json"), json!({"$schema": NODE}));
        self.write(&format!("{rel}/{main}"), body);
    }

    fn warden(&self, args: &[&str]) -> Out {
        self.warden_in(&self.root, args)
    }

    fn warden_in(&self, cwd: &Path, args: &[&str]) -> Out {
        self.warden_env(cwd, args, &[])
    }

    fn warden_env(&self, cwd: &Path, args: &[&str], extra: &[(&str, &std::ffi::OsStr)]) -> Out {
        let home = self.path("whome");
        let out = Command::new(BIN)
            .args(args)
            .envs(extra.iter().copied())
            .env("WARDEN_HOME", &home)
            .env("WARDEN_RUNTIME_DIR", home.join("run"))
            .env_remove("WARDEN_CONFIG")
            .env("WARDEN_NO_DAEMON", "1")
            .env("NO_COLOR", "1")
            .current_dir(cwd)
            .output()
            .unwrap();
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        }
    }

    /// `warden check -c <config>` on one written config: it must be accepted.
    fn check(&self, config: &Path) -> String {
        let out = self.warden(&["check", "-c", config.to_str().unwrap()]);
        assert_eq!(
            out.code,
            0,
            "warden check -c {} rejected what migrate-wattpm wrote:\n{}\n{}",
            config.display(),
            out.all(),
            std::fs::read_to_string(config).unwrap_or_default()
        );
        assert!(out.stdout.contains(": ok ("), "{}", out.all());
        out.stdout
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["kill", "--yes"])
            .env("WARDEN_HOME", self.path("whome"))
            .env("WARDEN_RUNTIME_DIR", self.path("whome/run"))
            .env("WARDEN_NO_DAEMON", "1")
            .output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

fn toml(path: &Path) -> toml::Table {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())).parse().unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The shop project: an entry point from `applications`, two applications found by `autoload` (one
/// excluded), a `.env` with a secret, a placeholder port, and per-application settings.
fn shop(d: &Dir) -> PathBuf {
    d.json("shop/package.json", json!({"name": "shop", "private": true, "type": "module"}));
    d.write("shop/.env", &format!("PORT=4100\nDB_URL=\"postgres://user:{SECRET}@db/shop\"\n"));
    d.app("shop/web", "index.js", "");
    d.app("shop/services/api", "server.js", "");
    d.write("shop/services/api/.env", "API_TOKEN=tok-123\n");
    d.app("shop/services/jobs", "main.js", "");
    d.app("shop/services/scratch", "x.js", "");
    d.json(
        "shop/watt.json",
        json!({
            "$schema": RUNTIME,
            "entrypoint": "web",
            "server": {"hostname": "0.0.0.0", "port": "{PORT}"},
            "workers": 2,
            "env": {"SHARED": "yes"},
            "applications": [{"id": "web", "path": "./web", "env": {"GREETING": "hello {DB_URL}"}}],
            "autoload": {"path": "./services", "exclude": ["scratch"],
                         "mappings": {"jobs": {"id": "worker"}}}
        }),
    );
    d.json("shop/services/jobs/watt.json", json!({"$schema": NODE, "runtime": {"workers": 1}}));
    d.path("shop")
}

#[test]
fn every_config_it_writes_passes_warden_check_and_keeps_secrets_out() {
    let d = Dir::new("shop");
    let root = shop(&d);
    let out_dir = d.path("out");
    let r = d.warden(&["migrate-wattpm", root.to_str().unwrap(), "--out", out_dir.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.all());

    for name in ["web", "api", "worker"] {
        let cfg = out_dir.join(format!("{name}.toml"));
        assert!(cfg.is_file(), "{name}.toml missing: {}", r.all());
        d.check(&cfg);
        let t = toml(&cfg);
        assert_eq!(t["app"]["namespace"].as_str(), Some("shop"), "{name}");
        assert_eq!(t["app"]["env_file"].as_str(), Some(format!("{name}.env").as_str()));
        assert_eq!(mode(&out_dir.join(format!("{name}.env"))), 0o600, "{name}.env must be private");
    }
    assert!(!out_dir.join("scratch.toml").exists(), "an excluded directory is not an application");
    assert!(!out_dir.join("jobs.toml").exists(), "the mapping renames jobs to worker");

    // Entry point: the placeholder port from .env, the runtime's worker count, the entry file.
    let web = toml(&out_dir.join("web.toml"));
    assert_eq!(web["app"]["port"].as_integer(), Some(4100));
    assert_eq!(web["workers"]["count"].as_integer(), Some(2));
    assert_eq!(web["app"]["command"].as_str(), Some("node"));
    assert!(web["app"]["args"].as_array().unwrap().last().unwrap().as_str().unwrap().ends_with("shop/web/index.js"));
    // An application's own `workers` (in its watt.json: a single-app key) is not the runtime's.
    assert_eq!(toml(&out_dir.join("api.toml"))["workers"]["count"].as_integer(), Some(2));
    // Internal applications have no port: Watt reached them in the process.
    assert!(toml(&out_dir.join("api.toml"))["app"].get("port").is_none());

    // The env files hold the merged environment in Watt's order, with placeholders resolved.
    let web_env = std::fs::read_to_string(out_dir.join("web.env")).unwrap();
    assert!(web_env.contains("SHARED=yes"), "{web_env}");
    assert!(web_env.contains(&format!("GREETING=\"hello postgres://user:{SECRET}@db/shop\"")), "{web_env}");
    assert!(web_env.contains(&format!("DB_URL=\"postgres://user:{SECRET}@db/shop\"")), "{web_env}");
    assert!(web_env.contains("NODE_ENV=production"), "{web_env}");
    let api_env = std::fs::read_to_string(out_dir.join("api.env")).unwrap();
    assert!(api_env.contains("API_TOKEN=tok-123"), "{api_env}");
    assert!(!api_env.contains("PORT="), "PORT belongs to the entry point only: {api_env}");

    // The secret is in the env files only.
    for f in ["web.toml", "api.toml", "worker.toml", "MIGRATION-wattpm.md"] {
        let text = std::fs::read_to_string(out_dir.join(f)).unwrap();
        assert!(!text.contains(SECRET) && !text.contains("tok-123"), "{f} has a secret:\n{text}");
    }
    assert!(!r.all().contains(SECRET), "{}", r.all());

    // The report lists every application and what has no equivalent.
    let report = std::fs::read_to_string(out_dir.join("MIGRATION-wattpm.md")).unwrap();
    for needle in ["web", "api", "worker", "mesh", "plt.local", "wattpm stop shop"] {
        assert!(report.contains(needle), "report lacks {needle:?}:\n{report}");
    }
    assert!(r.stdout.contains("report:"), "{}", r.stdout);
}

#[test]
fn dry_run_prints_everything_and_writes_nothing() {
    let d = Dir::new("dry");
    let root = shop(&d);
    let out_dir = d.path("out");
    let r = d.warden(&["migrate-wattpm", root.to_str().unwrap(), "--dry-run", "--out", out_dir.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.all());
    assert!(!out_dir.exists(), "--dry-run wrote files");
    assert!(!d.path("whome").exists(), "--dry-run touched Warden's directory");
    assert!(r.stdout.contains("[app]") && r.stdout.contains("name = \"api\""), "{}", r.stdout);
    assert!(r.stdout.contains("MIGRATION-wattpm.md"), "{}", r.stdout);
    // Env files are listed by variable name, never by value.
    assert!(r.stdout.contains("DB_URL") && r.stdout.contains("API_TOKEN"), "{}", r.stdout);
    assert!(!r.all().contains(SECRET) && !r.all().contains("tok-123"), "a dry run printed a secret:\n{}", r.all());
}

#[test]
fn the_default_output_directory_is_wardens_config_dir() {
    let d = Dir::new("home");
    // A single application: named after its package.json, no namespace; --prefix renames it.
    d.json("p/watt.json", json!({"$schema": NODE, "application": {"commands": {"production": "node index.js"}}}));
    d.json("p/package.json", json!({"name": "@acme/billing", "main": "index.js"}));
    d.write("p/index.js", "");
    let r = d.warden(&["migrate-wattpm", d.path("p").to_str().unwrap(), "--prefix", "wm"]);
    assert_eq!(r.code, 0, "{}", r.all());
    let cfg = d.path("whome/wm-billing.toml");
    assert!(
        cfg.is_file(),
        "{}\n{:?}",
        r.all(),
        std::fs::read_dir(d.path("whome")).map(|e| e.flatten().map(|x| x.file_name()).collect::<Vec<_>>())
    );
    d.check(&cfg);
    let t = toml(&cfg);
    assert_eq!(t["app"]["name"].as_str(), Some("wm-billing"));
    assert!(t["app"].get("namespace").is_none(), "one application needs no namespace");
}

#[test]
fn what_cannot_run_alone_is_reported_and_the_exit_code_says_so() {
    let d = Dir::new("skip");
    d.json("p/package.json", json!({"name": "mix", "type": "module"}));
    d.app("p/web", "index.js", "");
    d.json("p/site/package.json", json!({"name": "site", "scripts": {"start": "next start"}}));
    d.json("p/site/watt.json", json!({"$schema": "https://schemas.platformatic.dev/@platformatic/next/3.71.0.json"}));
    d.json(
        "p/front/watt.json",
        json!({"$schema": "https://schemas.platformatic.dev/@platformatic/gateway/3.71.0.json",
        "gateway": {"applications": [{"id": "web", "proxy": {"prefix": "/"}}]}}),
    );
    d.json("p/front/package.json", json!({"name": "front"}));
    d.json(
        "p/watt.json",
        json!({"$schema": RUNTIME, "entrypoint": "front",
               "applications": [{"id": "front", "path": "./front"}, {"id": "web", "path": "./web"}, {"id": "site", "path": "./site"}]}),
    );
    let out_dir = d.path("out");
    let r = d.warden(&["migrate-wattpm", d.path("p").to_str().unwrap(), "--out", out_dir.to_str().unwrap()]);
    assert_eq!(r.code, 1, "an application that was not converted must not exit 0:\n{}", r.all());
    // The one that can run is written and accepted.
    d.check(&out_dir.join("web.toml"));
    assert!(!out_dir.join("front.toml").exists() && !out_dir.join("site.toml").exists());
    assert!(r.stderr.contains("front: not converted") && r.stderr.contains("site: not converted"), "{}", r.stderr);
    let report = std::fs::read_to_string(out_dir.join("MIGRATION-wattpm.md")).unwrap();
    assert!(report.contains("front (not converted)") && report.contains("site (not converted)"), "{report}");
    assert!(report.to_lowercase().contains("gateway") && report.contains("/"), "{report}");
    assert!(report.contains("--command"), "the report must say how to give the framework app a command:\n{report}");

    // Giving the framework app its production command converts it.
    let r = d.warden(&[
        "migrate-wattpm",
        d.path("p").to_str().unwrap(),
        "--out",
        out_dir.to_str().unwrap(),
        "--overwrite",
        "--apps",
        "site",
        "--command",
        "site=node server.js --port 3000",
    ]);
    assert_eq!(r.code, 0, "{}", r.all());
    d.check(&out_dir.join("site.toml"));
    let t = toml(&out_dir.join("site.toml"));
    assert_eq!(t["app"]["command"].as_str(), Some("node"));
    assert_eq!(
        t["app"]["args"].as_array().unwrap().iter().filter_map(|a| a.as_str()).collect::<Vec<_>>()[0..1],
        ["server.js"][..]
    );
}

#[test]
fn invalid_config_files_fail_with_the_file_and_what_to_do() {
    let d = Dir::new("bad");
    // Comments and a trailing comma: Watt's own .json reader rejects them too.
    d.write("a/watt.json", "{\n  // the entry point\n  \"entrypoint\": \"web\",\n}\n");
    let r = d.warden(&["migrate-wattpm", d.path("a").to_str().unwrap(), "--dry-run"]);
    assert_eq!(r.code, 1, "{}", r.all());
    assert!(r.stderr.contains("a/watt.json") && r.stderr.contains("not valid JSON"), "{}", r.stderr);
    assert!(r.stderr.contains("line 2") && r.stderr.contains(".json5"), "the line and the way out: {}", r.stderr);

    // A real syntax error says where.
    d.write("b/watt.json", "{\"entrypoint\": \"web\",\n \"applications\": [ {\"id\": \"web\" \"path\": \"x\"} ]}");
    let r = d.warden(&["migrate-wattpm", d.path("b").to_str().unwrap(), "--dry-run"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("not valid JSON") && r.stderr.contains("line 2"), "{}", r.stderr);

    // The same comments are fine in watt.json5.
    d.app("c/web", "index.js", "");
    d.write(
        "c/watt.json5",
        "{\n  // the entry point\n  \"$schema\": \"https://schemas.platformatic.dev/@platformatic/runtime/3.71.0.json\",\n  \"entrypoint\": \"web\",\n  /* one app */\n  \"applications\": [{\"id\": \"web\", \"path\": \"./web\"},],\n}\n",
    );
    let r = d.warden(&["migrate-wattpm", d.path("c").to_str().unwrap(), "--dry-run"]);
    assert_eq!(r.code, 0, "{}", r.all());
    // Unquoted keys and single quotes are JSON5 proper: said so, with what to do.
    d.write("c2/watt.json5", "{ entrypoint: 'web' }\n");
    let r = d.warden(&["migrate-wattpm", d.path("c2").to_str().unwrap(), "--dry-run"]);
    assert_eq!(r.code, 1, "{}", r.all());
    assert!(r.stderr.contains("watt.json5") && r.stderr.contains("save the config as watt.json"), "{}", r.stderr);

    // A directory with no config, a missing path, a project with nothing enabled.
    std::fs::create_dir_all(d.path("empty")).unwrap();
    let r = d.warden(&["migrate-wattpm", d.path("empty").to_str().unwrap(), "--dry-run"]);
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("watt.json") && r.stderr.contains("platformatic.json"),
        "names what it looked for: {}",
        r.stderr
    );
    let r = d.warden(&["migrate-wattpm", d.path("nowhere").to_str().unwrap(), "--dry-run"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("no such file"), "{}", r.stderr);
    d.write("e/watt.yaml", "entrypoint: web\n");
    let r = d.warden(&["migrate-wattpm", d.path("e").to_str().unwrap(), "--dry-run"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("YAML"), "{}", r.stderr);
}

#[test]
fn existing_configs_are_kept_unless_overwrite_is_given() {
    let d = Dir::new("over");
    let root = shop(&d);
    let out_dir = d.path("out");
    let args = ["migrate-wattpm", root.to_str().unwrap(), "--out", out_dir.to_str().unwrap()];
    assert_eq!(d.warden(&args).code, 0);
    std::fs::write(out_dir.join("api.toml"), "# edited by hand\n").unwrap();
    let r = d.warden(&args);
    assert_eq!(r.code, 1, "{}", r.all());
    assert!(r.stderr.contains("api.toml exists") && r.stderr.contains("--overwrite"), "{}", r.stderr);
    assert_eq!(std::fs::read_to_string(out_dir.join("api.toml")).unwrap(), "# edited by hand\n");
    let mut again = args.to_vec();
    again.push("--overwrite");
    let r = d.warden(&again);
    assert_eq!(r.code, 0, "{}", r.all());
    d.check(&out_dir.join("api.toml"));
    assert_eq!(mode(&out_dir.join("api.env")), 0o600);
}

#[test]
fn watt_2_services_a_single_application_and_platformatic_json_are_read() {
    let d = Dir::new("old");
    // `services` was Watt 2's name for `applications`; platformatic.json its file name.
    d.app("p/web", "index.js", "");
    d.app("p/api", "index.js", "");
    d.json("p/package.json", json!({"name": "legacy"}));
    d.json(
        "p/platformatic.json",
        json!({"$schema": "https://schemas.platformatic.dev/@platformatic/runtime/2.0.0.json",
               "entrypoint": "web", "server": {"port": 3042},
               "services": [{"id": "web", "path": "./web"}, {"id": "api", "path": "./api"}]}),
    );
    let out_dir = d.path("out");
    let r = d.warden(&["migrate-wattpm", d.path("p").to_str().unwrap(), "--out", out_dir.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.all());
    d.check(&out_dir.join("web.toml"));
    d.check(&out_dir.join("api.toml"));
    assert_eq!(toml(&out_dir.join("web.toml"))["app"]["port"].as_integer(), Some(3042));
    // The file can be named directly, too.
    let r = d.warden(&["migrate-wattpm", d.path("p/platformatic.json").to_str().unwrap(), "--dry-run"]);
    assert_eq!(r.code, 0, "{}", r.all());
    // And a project that is one application (no runtime config) is wrapped like Watt wraps it.
    d.app("s", "index.js", "");
    let r = d.warden(&["migrate-wattpm", d.path("s").to_str().unwrap(), "--dry-run"]);
    assert_eq!(r.code, 0, "{}", r.all());
    assert!(r.stdout.contains("name = \"s\""), "{}", r.stdout);
}

#[test]
fn the_command_line_is_documented_and_refuses_what_it_cannot_do() {
    let d = Dir::new("cli");
    let help = d.warden(&["--help"]);
    assert!(help.stdout.contains("migrate-wattpm"), "{}", help.stdout);
    for flag in ["--cutover", "--dry-run", "--overwrite", "--command"] {
        assert!(help.stdout.contains(flag), "{flag} missing from --help");
    }
    let r = d.warden(&["migrate-wattpm", "--help"]);
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("migrate-wattpm") && r.stdout.contains("MIGRATION-wattpm.md"), "{}", r.stdout);

    // same-port would stop the whole runtime: not offered, and the reason is given.
    let r = d.warden(&["migrate-wattpm", "--cutover", "same-port"]);
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("same-port") && r.stderr.contains("wattpm stop"), "{}", r.stderr);
    let r = d.warden(&["migrate-wattpm", "--dry-run", "--cutover", "overlap"]);
    assert_eq!(r.code, 2, "{}", r.all());
    let r = d.warden(&["migrate-wattpm", "--no-such-flag"]);
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("--no-such-flag"), "{}", r.stderr);
    // An id that is not in the project is named, with the ones that are.
    let root = shop(&d);
    let r = d.warden(&["migrate-wattpm", root.to_str().unwrap(), "--dry-run", "--apps", "nope"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("nope") && r.stderr.contains("web, api, worker"), "{}", r.stderr);
}

/// Watt's own loader is the reference: the application ids, the entry point, the worker counts, the
/// port and the directories it resolves are what the converter must have written.
#[test]
fn the_conversion_matches_what_wattpms_own_loader_resolves() {
    if !have_wattpm() {
        eprintln!("skipping: needs node and `npm ci` in bench/ (wattpm)");
        return;
    }
    let d = Dir::new("oracle");
    let root = shop(&d);
    std::os::unix::fs::symlink(bench_modules(), root.join("node_modules")).unwrap();
    let m = bench_modules();
    let script = d.write(
        "oracle.mjs",
        &format!(
            "import {{ loadConfiguration }} from {:?};\n\
             const cfg = await loadConfiguration(process.argv[2], null, {{ production: true, allowMissingEntrypoint: true }});\n\
             console.log(JSON.stringify({{ entrypoint: cfg.entrypoint, port: cfg.server?.port, workers: cfg.workers,\n\
               apps: cfg.applications.map((a) => ({{ id: a.id, path: a.path, workers: a.workers, entrypoint: a.entrypoint, env: a.env }})) }}));\n",
            m.join("@platformatic/runtime/index.js").to_str().unwrap()
        ),
    );
    let o = Command::new("node").arg(&script).arg(&root).env_remove("PORT").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let watt: Value = serde_json::from_slice(&o.stdout).unwrap();

    let out_dir = d.path("out");
    let r = d.warden(&["migrate-wattpm", root.to_str().unwrap(), "--out", out_dir.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.all());

    let apps = watt["apps"].as_array().unwrap();
    let mut ids: Vec<&str> = apps.iter().map(|a| a["id"].as_str().unwrap()).collect();
    ids.sort();
    assert_eq!(ids, ["api", "web", "worker"], "{watt}");
    for a in apps {
        let id = a["id"].as_str().unwrap();
        let t = toml(&out_dir.join(format!("{id}.toml")));
        assert_eq!(
            std::fs::canonicalize(t["app"]["working_directory"].as_str().unwrap()).unwrap(),
            std::fs::canonicalize(a["path"].as_str().unwrap()).unwrap(),
            "{id}: directory"
        );
        assert_eq!(t["workers"]["count"].as_integer(), a["workers"]["static"].as_i64(), "{id}: workers");
        assert_eq!(
            a["entrypoint"].as_bool().unwrap(),
            t["app"].get("port").is_some(),
            "{id}: only the entry point has a port"
        );
        if a["entrypoint"].as_bool().unwrap() {
            // PORT comes from the project's .env (4100): a PORT in the environment would win over it, in Watt and here.
            assert_eq!(t["app"]["port"].as_integer(), watt["port"].as_i64(), "{id}: port");
            assert_eq!(id, watt["entrypoint"].as_str().unwrap());
        }
        // What Watt hands the application as `env`, resolved, is in its env file.
        let env = std::fs::read_to_string(out_dir.join(format!("{id}.env"))).unwrap();
        for (k, v) in a["env"].as_object().into_iter().flatten() {
            assert!(env.contains(&format!("{k}=")), "{id}: {k}={v} is not in {id}.env:\n{env}");
        }
    }
}

/// A runtime started by the real wattpm, then Warden's copy beside it: it serves on another port with
/// the same environment, Watt is left running, and the failure path leaves nothing behind.
#[test]
fn a_cutover_starts_wardens_copy_next_to_a_running_wattpm_and_never_stops_it() {
    if !have_wattpm() {
        eprintln!("skipping: needs node and `npm ci` in bench/ (wattpm)");
        return;
    }
    let d = Dir::new("cut");
    let (watt_port, new_port) = (free_port(), free_port());
    let server = |name: &str| {
        format!(
            "import http from 'node:http';\n\
             http.createServer((q, r) => r.end(`{name}|${{process.env.GREETING}}|${{process.env.API_TOKEN ?? ''}}`)).listen({{ port: {port}, host: '127.0.0.1' }});\n",
            port = if name == "web" { "Number(process.env.PORT)" } else { "0" }
        )
    };
    d.json("shop/package.json", json!({"name": "shop", "private": true, "type": "module"}));
    d.write("shop/.env", &format!("PORT={watt_port}\nWHO=world\n"));
    d.app("shop/web", "index.mjs", &server("web"));
    d.app("shop/api", "index.mjs", &server("api"));
    d.json(
        "shop/watt.json",
        json!({"$schema": RUNTIME, "entrypoint": "web", "server": {"hostname": "127.0.0.1", "port": "{PORT}"},
               "workers": 2, "logger": {"level": "warn"},
               "applications": [{"id": "web", "path": "./web", "env": {"GREETING": "hello {WHO}"}},
                                {"id": "api", "path": "./api", "workers": 1, "env": {"API_TOKEN": SECRET}}]}),
    );
    std::os::unix::fs::symlink(bench_modules(), d.path("shop/node_modules")).unwrap();

    struct Watt(Child);
    impl Drop for Watt {
        fn drop(&mut self) {
            let _ = Command::new("kill").arg("-TERM").arg(self.0.id().to_string()).status();
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(10) && self.0.try_wait().ok().flatten().is_none() {
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let watt = Watt(
        Command::new(bench_modules().join(".bin/wattpm"))
            .args(["start", d.path("shop").to_str().unwrap()])
            .current_dir(d.path("shop"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for("wattpm to serve", 60, || get(watt_port, "/").is_some());
    let under_watt = get(watt_port, "/").unwrap();
    assert_eq!(under_watt, "web|hello world|", "the reference: what the application sees under Watt");

    let out_dir = d.path("out");
    let r = d.warden(&[
        "migrate-wattpm",
        d.path("shop").to_str().unwrap(),
        "--out",
        out_dir.to_str().unwrap(),
        "--cutover",
        &format!("new-port:{new_port}"),
    ]);
    assert_eq!(r.code, 0, "{}", r.all());
    assert!(r.stdout.contains("web: online") && r.stdout.contains("api: online"), "{}", r.stdout);
    assert!(r.stdout.contains("wattpm was not touched"), "{}", r.stdout);

    // Warden's copy sees the same environment as the application did under Watt, and serves on the new port.
    wait_for("Warden's copy to serve", 30, || get(new_port, "/").is_some());
    assert_eq!(get(new_port, "/").unwrap(), under_watt);
    // Watt is still there, on its port, with the same answer.
    assert_eq!(get(watt_port, "/").as_deref(), Some(under_watt.as_str()));
    let report = std::fs::read_to_string(out_dir.join("MIGRATION-wattpm.md")).unwrap();
    assert!(report.contains(&new_port.to_string()), "{report}");

    // The apps are Warden's now: listed, in one namespace, with the secret only in the env file.
    let list = d.warden(&["list", "--json"]);
    assert_eq!(list.code, 0, "{}", list.all());
    let rows: Value = serde_json::from_str(&list.stdout).unwrap();
    let names: Vec<&str> = rows.as_array().unwrap().iter().filter_map(|a| a["app"].as_str()).collect();
    assert!(names.contains(&"web") && names.contains(&"api"), "{names:?}");
    assert!(!std::fs::read_to_string(out_dir.join("api.toml")).unwrap().contains(SECRET));
    assert_eq!(mode(&out_dir.join("api.env")), 0o600);
    // wattpm still answers its own management commands for the runtime.
    let ps = Command::new(bench_modules().join(".bin/wattpm")).arg("ps").current_dir(d.path("shop")).output().unwrap();
    assert!(String::from_utf8_lossy(&ps.stdout).contains("shop"), "{}", String::from_utf8_lossy(&ps.stdout));
    drop(watt);
}

// ------------------------------------------------------------------ secrets in a command

const TOKEN: &str = "s3cr3t-value-123";

/// Every file under `dir` (not its env files): what a reader of the output directory can see.
fn files_but_env(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_file() && p.extension().is_none_or(|x| x != "env") {
            v.push((p.clone(), std::fs::read_to_string(&p).unwrap()));
        }
    }
    v
}

/// A project whose applications put `{API_TOKEN}` (from `.env`) in a shell command, a plain command,
/// and in `arguments`, `execArgv` and `nodeOptions`.
fn secret_project(d: &Dir) -> PathBuf {
    d.json("p/package.json", json!({"name": "vault", "private": true, "type": "module"}));
    d.write("p/.env", &format!("API_TOKEN={TOKEN}\n"));
    for id in ["chain", "plain", "flags"] {
        d.app(&format!("p/{id}"), "index.js", "");
    }
    d.json(
        "p/chain/watt.json",
        json!({"$schema": NODE, "application": {"commands": {"production": "npm run build && node index.js --token={API_TOKEN}"}}}),
    );
    d.json(
        "p/plain/watt.json",
        json!({"$schema": NODE, "application": {"commands": {"production": "node index.js --token={API_TOKEN}"}}}),
    );
    d.json(
        "p/watt.json",
        json!({"$schema": RUNTIME, "entrypoint": "chain", "server": {"port": 4300},
               "applications": [
                   {"id": "chain", "path": "./chain"},
                   {"id": "plain", "path": "./plain", "env": {"PORT": "4301"}},
                   {"id": "flags", "path": "./flags", "env": {"PORT": "4302"},
                    "execArgv": ["--title={API_TOKEN}"], "nodeOptions": "--require={API_TOKEN}",
                    "arguments": ["--token={API_TOKEN}"]}]}),
    );
    d.path("p")
}

#[test]
fn a_secret_in_a_command_reaches_only_the_env_file_and_a_private_config() {
    let d = Dir::new("secret");
    let root = secret_project(&d);
    let out_dir = d.path("out");
    let r = d.warden(&["migrate-wattpm", root.to_str().unwrap(), "--out", out_dir.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.all());

    // A shell command reads the value from the env file: its config has none, and is an ordinary file.
    let chain = toml(&out_dir.join("chain.toml"));
    assert_eq!(chain["app"]["command"].as_str(), Some("sh"));
    assert_eq!(chain["app"]["args"][1].as_str(), Some("npm run build && node index.js --token=\"${API_TOKEN}\""));
    assert_eq!(mode(&out_dir.join("chain.toml")), 0o644);
    assert!(std::fs::read_to_string(out_dir.join("chain.env")).unwrap().contains(&format!("API_TOKEN={TOKEN}")));
    // The other forms have no way to read it: the value is in the config, which is as private as the env file.
    for id in ["plain", "flags"] {
        let text = std::fs::read_to_string(out_dir.join(format!("{id}.toml"))).unwrap();
        assert!(text.contains(TOKEN), "{id}.toml needs the value:\n{text}");
        assert_eq!(mode(&out_dir.join(format!("{id}.toml"))), 0o600, "{id}.toml holds a secret");
        assert_eq!(mode(&out_dir.join(format!("{id}.env"))), 0o600);
        d.check(&out_dir.join(format!("{id}.toml")));
    }
    d.check(&out_dir.join("chain.toml"));

    // Nothing else holds it: not the report, not the screen.
    for (path, text) in files_but_env(&out_dir) {
        let private = mode(&path) == 0o600;
        assert!(private || !text.contains(TOKEN), "{} is world-readable and has the secret:\n{text}", path.display());
    }
    let report = std::fs::read_to_string(out_dir.join("MIGRATION-wattpm.md")).unwrap();
    assert_eq!(mode(&out_dir.join("MIGRATION-wattpm.md")), 0o644);
    assert!(!report.contains(TOKEN) && report.contains("***"), "{report}");
    assert!(!r.all().contains(TOKEN), "the output has the secret:\n{}", r.all());

    // --dry-run prints the same without the value.
    let dry = d.warden(&["migrate-wattpm", root.to_str().unwrap(), "--dry-run"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(!dry.all().contains(TOKEN), "a dry run printed the secret:\n{}", dry.all());
    assert!(dry.stdout.contains("--token=***") && dry.stdout.contains("${API_TOKEN}"), "{}", dry.stdout);
}

#[test]
fn an_invalid_config_is_reported_without_its_text() {
    let d = Dir::new("invalid");
    d.json("p/package.json", json!({"name": "vault", "type": "module"}));
    d.write("p/.env", &format!("API_TOKEN={TOKEN}\n"));
    d.app("p/web", "index.js", "");
    d.json(
        "p/web/watt.json",
        json!({"$schema": NODE, "application": {"commands": {"production": "node index.js --token={API_TOKEN}"}}}),
    );
    // Four workers with consecutive ports from 65535 do not fit: the generated config is refused.
    d.json(
        "p/watt.json",
        json!({"$schema": RUNTIME, "entrypoint": "web", "workers": 4,
               "server": {"port": 65535, "portAssignment": "perWorkerIncrement"},
               "applications": [{"id": "web", "path": "./web"}]}),
    );
    let out_dir = d.path("out");
    let r = d.warden(&["migrate-wattpm", d.path("p").to_str().unwrap(), "--out", out_dir.to_str().unwrap()]);
    assert_eq!(r.code, 1, "{}", r.all());
    assert!(r.stderr.contains("generated config is invalid") && r.stderr.contains("65535"), "{}", r.stderr);
    for out in [r.all(), std::fs::read_to_string(out_dir.join("MIGRATION-wattpm.md")).unwrap()] {
        assert!(!out.contains(TOKEN), "{out}");
        assert!(
            !out.contains("working_directory") && !out.contains("args = ["),
            "the config text is in the output:\n{out}"
        );
    }
    assert!(!out_dir.join("web.toml").exists());
}

#[test]
fn a_line_break_in_a_command_cannot_change_the_config() {
    let d = Dir::new("lf");
    d.json("p/package.json", json!({"name": "lf", "type": "module"}));
    d.app("p/web", "index.js", "");
    for (name, command) in [
        ("chained", "npm run build &&\nnode server.js"),
        ("inject", "node server.js;\n[limits]\nmax_memory = 1\n# done"),
    ] {
        d.json("p/web/watt.json", json!({"$schema": NODE, "application": {"commands": {"production": command}}}));
        d.json(
            "p/watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web", "applications": [{"id": "web", "path": "./web"}]}),
        );
        let out_dir = d.path(&format!("out-{name}"));
        let r = d.warden(&["migrate-wattpm", d.path("p").to_str().unwrap(), "--out", out_dir.to_str().unwrap()]);
        assert_eq!(r.code, 0, "{name}: {}", r.all());
        let cfg = out_dir.join("web.toml");
        d.check(&cfg);
        let t = toml(&cfg);
        assert!(
            t.get("limits").is_none(),
            "{name}: the command wrote a table:\n{}",
            std::fs::read_to_string(&cfg).unwrap()
        );
        assert!(t["app"]["args"][1].as_str().unwrap().contains('\n'), "{name}: the command is kept as it was");
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.lines().next().unwrap().starts_with("# Written by `warden migrate-wattpm`"), "{text}");
        assert_eq!(text.lines().filter(|l| l.starts_with('#')).count(), 1, "{name}: one comment line\n{text}");
    }
}

// ------------------------------------------------------------------ what is written, and where

#[test]
fn an_existing_env_file_is_kept_unless_overwrite_is_given() {
    let d = Dir::new("envkeep");
    d.json("p/package.json", json!({"name": "keep", "type": "module"}));
    d.app("p/web", "index.js", "");
    d.json(
        "p/watt.json",
        json!({"$schema": RUNTIME, "entrypoint": "web", "applications": [{"id": "web", "path": "./web"}]}),
    );
    let out_dir = d.path("out");
    d.write("out/web.env", "KEEP=me\n");
    let project = d.path("p");
    let args = ["migrate-wattpm", project.to_str().unwrap(), "--out", out_dir.to_str().unwrap()];
    // No web.toml yet, but the env file is somebody's: the app is left alone, and the exit code says so.
    let r = d.warden(&args);
    assert_eq!(r.code, 1, "{}", r.all());
    assert!(r.stderr.contains("web.env exists") && r.stderr.contains("--overwrite"), "{}", r.stderr);
    assert_eq!(std::fs::read_to_string(out_dir.join("web.env")).unwrap(), "KEEP=me\n");
    assert!(!out_dir.join("web.toml").exists(), "a config without its env file is not written");
    // --overwrite replaces both, and the env file is private.
    let r = d.warden(&[args[0], args[1], args[2], args[3], "--overwrite"]);
    assert_eq!(r.code, 0, "{}", r.all());
    assert!(std::fs::read_to_string(out_dir.join("web.env")).unwrap().contains("NODE_ENV=production"));
    assert_eq!(mode(&out_dir.join("web.env")), 0o600);
    d.check(&out_dir.join("web.toml"));
}

#[test]
fn a_symbolic_link_is_never_written_through() {
    use std::os::unix::fs::symlink;
    let d = Dir::new("link");
    d.json("p/package.json", json!({"name": "ln", "type": "module"}));
    d.app("p/web", "index.js", "");
    d.json(
        "p/watt.json",
        json!({"$schema": RUNTIME, "entrypoint": "web", "applications": [{"id": "web", "path": "./web"}]}),
    );
    let out_dir = d.path("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let victim = d.write("victim.txt", "somebody else's file\n");
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    let still = |what: &str| {
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "somebody else's file\n",
            "{what}: the file behind the link changed"
        );
        assert_eq!(mode(&victim), 0o644, "{what}: the file behind the link was chmod-ed");
    };
    let project = d.path("p");
    for overwrite in [false, true] {
        let mut args = vec!["migrate-wattpm", project.to_str().unwrap(), "--out", out_dir.to_str().unwrap()];
        if overwrite {
            args.push("--overwrite");
        }
        // An env file that is a link to a file, and a config that is a link to nothing.
        let (env_link, toml_link) = (out_dir.join("web.env"), out_dir.join("web.toml"));
        let _ = std::fs::remove_file(&env_link);
        let _ = std::fs::remove_file(&toml_link);
        symlink(&victim, &env_link).unwrap();
        let r = d.warden(&args);
        assert_eq!(r.code, 1, "{overwrite}: {}", r.all());
        assert!(r.stderr.contains("symbolic link") && r.stderr.contains("web.env"), "{}", r.stderr);
        still("env link");
        assert!(std::fs::symlink_metadata(&env_link).unwrap().file_type().is_symlink());
        assert!(!toml_link.exists(), "nothing is written for an app whose files cannot be");
        std::fs::remove_file(&env_link).unwrap();
        let nowhere = d.path("nowhere.toml");
        symlink(&nowhere, &toml_link).unwrap();
        let r = d.warden(&args);
        assert_eq!(r.code, 1, "{overwrite}: {}", r.all());
        assert!(r.stderr.contains("symbolic link") && r.stderr.contains("web.toml"), "{}", r.stderr);
        assert!(!nowhere.exists(), "a dangling link was written through");
        assert!(!out_dir.join("web.env").exists(), "the env file is not written when the config cannot be");
    }
}

#[test]
fn pm2_migrate_never_writes_through_a_symbolic_link_either() {
    use std::os::unix::fs::symlink;
    let d = Dir::new("pm2link");
    d.write("srv/api.js", "");
    d.json(
        "pm2/dump.pm2",
        json!([{"name": "api", "pm_exec_path": d.path("srv/api.js"), "pm_cwd": d.path("srv"), "exec_mode": "fork_mode",
                "env": {"NODE_ENV": "production", "DB_URL": "postgres://x"}}]),
    );
    let out_dir = d.path("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let victim = d.write("victim.txt", "somebody else's file\n");
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    symlink(&victim, out_dir.join("api.env")).unwrap();
    symlink(d.path("nowhere.toml"), out_dir.join("api.toml")).unwrap();
    let pm2_home = d.path("pm2");
    let r = d.warden_env(
        &d.root,
        &["pm2-migrate", "--from", "dump", "--out", out_dir.to_str().unwrap(), "--overwrite"],
        &[("PM2_HOME", pm2_home.as_os_str())],
    );
    assert_ne!(r.code, 0, "{}", r.all());
    assert!(r.all().contains("symbolic link"), "{}", r.all());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "somebody else's file\n");
    assert_eq!(mode(&victim), 0o644, "the file behind the link was chmod-ed");
    assert!(!d.path("nowhere.toml").exists(), "a dangling link was written through");
}

#[test]
fn a_variable_that_is_not_utf8_does_not_crash_either_migration() {
    use std::os::unix::ffi::OsStrExt;
    let d = Dir::new("utf8");
    d.json("p/package.json", json!({"name": "u", "type": "module"}));
    d.app("p/web", "index.js", "");
    d.json(
        "p/watt.json",
        json!({"$schema": RUNTIME, "entrypoint": "web", "applications": [{"id": "web", "path": "./web"}]}),
    );
    let bad = std::ffi::OsStr::from_bytes(b"\xff\xfe broken");
    let r = d.warden_env(&d.root, &["migrate-wattpm", d.path("p").to_str().unwrap(), "--dry-run"], &[("BAD_VAR", bad)]);
    assert_eq!(r.code, 0, "{}", r.all());
    assert!(r.stdout.contains("name = \"web\""), "{}", r.stdout);

    d.write("srv/api.js", "");
    d.json(
        "pm2/dump.pm2",
        json!([{"name": "api", "pm_exec_path": d.path("srv/api.js"), "pm_cwd": d.path("srv"), "exec_mode": "fork_mode",
                "env": {"NODE_ENV": "production"}}]),
    );
    let pm2_home = d.path("pm2");
    let r = d.warden_env(
        &d.root,
        &["pm2-migrate", "--from", "dump", "--dry-run"],
        &[("PM2_HOME", pm2_home.as_os_str()), ("BAD_VAR", bad)],
    );
    assert_eq!(r.code, 0, "{}", r.all());
    assert!(r.stdout.contains("name = \"api\""), "{}", r.stdout);
}

#[test]
fn a_name_that_is_a_directory_is_renamed_and_warden_refuses_it() {
    let d = Dir::new("dots");
    d.json("p/package.json", json!({"name": "dots", "type": "module"}));
    d.app("p/web", "index.js", "");
    d.json(
        "p/watt.json",
        json!({"$schema": RUNTIME, "entrypoint": "..", "applications": [{"id": "..", "path": "./web"}]}),
    );
    let out_dir = d.path("out");
    let r = d.warden(&["migrate-wattpm", d.path("p").to_str().unwrap(), "--out", out_dir.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.all());
    assert!(
        out_dir.join("app-dotdot.toml").is_file(),
        "{}\n{:?}",
        r.all(),
        std::fs::read_dir(&out_dir).unwrap().flatten().collect::<Vec<_>>()
    );
    assert!(!out_dir.join("...toml").exists() && !out_dir.join("..toml").exists());
    d.check(&out_dir.join("app-dotdot.toml"));
    assert_eq!(toml(&out_dir.join("app-dotdot.toml"))["app"]["name"].as_str(), Some("app-dotdot"));
    let report = std::fs::read_to_string(out_dir.join("MIGRATION-wattpm.md")).unwrap();
    assert!(report.contains("names a directory"), "{report}");

    // A config that names itself that is refused by `warden check`.
    for name in [".", ".."] {
        let cfg = d.write("bad.toml", &format!("[app]\nname = {name:?}\ncommand = \"sleep\"\nargs = [\"1\"]\n"));
        let r = d.warden(&["check", "-c", cfg.to_str().unwrap()]);
        assert_ne!(r.code, 0, "{name}: {}", r.all());
        assert!(r.all().contains("only dots"), "{name}: {}", r.all());
    }
}

// ------------------------------------------------------------------ cutover

/// A server on `PORT` that answers its own name.
fn named_server(name: &str) -> String {
    format!(
        "import http from 'node:http';\n\
         http.createServer((q, r) => r.end('{name}')).listen(Number(process.env.PORT), '127.0.0.1');\n"
    )
}

/// Three applications: the entry point `web`, an internal `api`, and `ok`; each is given its port.
fn three(d: &Dir, web: u16, api: u16, ok: u16) -> PathBuf {
    d.json("p/package.json", json!({"name": "trio", "private": true, "type": "module"}));
    for id in ["web", "api", "ok"] {
        d.app(&format!("p/{id}"), "index.mjs", &named_server(id));
    }
    d.json(
        "p/watt.json",
        json!({"$schema": RUNTIME, "entrypoint": "web", "server": {"hostname": "127.0.0.1", "port": web},
               "applications": [{"id": "web", "path": "./web"},
                                {"id": "api", "path": "./api", "env": {"PORT": api.to_string()}},
                                {"id": "ok", "path": "./ok", "env": {"PORT": ok.to_string()}}]}),
    );
    d.path("p")
}

fn running(d: &Dir) -> Vec<String> {
    let list = d.warden(&["list", "--json"]);
    let rows: Value = serde_json::from_str(&list.stdout).unwrap_or(Value::Null);
    rows.as_array()
        .into_iter()
        .flatten()
        // `status` is null for an app whose supervisor does not answer.
        .filter(|a| !a["status"].is_null())
        .filter_map(|a| a["app"].as_str().map(String::from))
        .collect()
}

#[test]
fn a_cutover_does_not_start_two_programs_on_one_port() {
    if !have_node() {
        eprintln!("skipping: needs node");
        return;
    }
    let d = Dir::new("clash");
    let (shared, ok) = (free_port(), free_port());
    // `web` is on `server.port`, `api` on the same port from its own env.
    let root = three(&d, shared, shared, ok);
    let out_dir = d.path("out");
    let r = d.warden(&[
        "migrate-wattpm",
        root.to_str().unwrap(),
        "--out",
        out_dir.to_str().unwrap(),
        "--cutover",
        "overlap",
    ]);
    assert_eq!(r.code, 1, "{}", r.all());
    assert!(
        r.stderr.contains("web: not started")
            && r.stderr.contains("api: not started")
            && r.stderr.contains(&format!("port {shared}")),
        "{}",
        r.stderr
    );
    assert_eq!(get(shared, "/"), None, "something was started on the shared port");
    // The one that has a port of its own is not held back.
    wait_for("`ok` to serve", 20, || get(ok, "/").is_some());
    assert_eq!(get(ok, "/").as_deref(), Some("ok"));
    let names = running(&d);
    assert!(
        names.contains(&"ok".to_string()) && !names.contains(&"web".to_string()) && !names.contains(&"api".to_string()),
        "{names:?}"
    );
}

#[test]
fn a_cutover_port_that_is_an_internal_applications_own_is_a_conflict_too() {
    if !have_node() {
        eprintln!("skipping: needs node");
        return;
    }
    let d = Dir::new("newport");
    let (web, api, ok) = (free_port(), free_port(), free_port());
    let root = three(&d, web, api, ok);
    let out_dir = d.path("out");
    // The new port of the entry point is the one `api` listens on.
    let r = d.warden(&[
        "migrate-wattpm",
        root.to_str().unwrap(),
        "--out",
        out_dir.to_str().unwrap(),
        "--cutover",
        &format!("new-port:{api}"),
    ]);
    assert_eq!(r.code, 1, "{}", r.all());
    assert!(r.stderr.contains("web: not started") && r.stderr.contains("api: not started"), "{}", r.stderr);
    assert!(r.stderr.contains("new-port"), "it says what to change: {}", r.stderr);
    assert_eq!(get(api, "/"), None);
    wait_for("`ok` to serve", 20, || get(ok, "/").is_some());
}

#[test]
fn a_cutover_that_fails_stops_what_it_started_and_says_so() {
    if !have_node() {
        eprintln!("skipping: needs node");
        return;
    }
    let d = Dir::new("failcut");
    let (web, api, ok) = (free_port(), free_port(), free_port());
    let root = three(&d, web, api, ok);
    // `ok` crashes at start.
    d.write("p/ok/index.mjs", "process.exit(1);\n");
    let out_dir = d.path("out");
    let new_web = free_port();
    let r = d.warden(&[
        "migrate-wattpm",
        root.to_str().unwrap(),
        "--out",
        out_dir.to_str().unwrap(),
        "--cutover",
        &format!("new-port:{new_web}"),
    ]);
    assert_eq!(r.code, 1, "{}", r.all());
    assert!(r.stderr.contains("ok: Warden's workers did not come up"), "{}", r.stderr);
    // What is left running is said, and it is nothing: web and api were stopped again.
    assert!(
        r.stderr.contains("Stopped web, api") && r.stderr.contains("nothing of this run is left running"),
        "{}",
        r.stderr
    );
    assert!(!r.stdout.contains("Warden serves"), "{}", r.stdout);
    wait_for("web and api to be stopped", 20, || running(&d).is_empty());
    assert!(get(new_web, "/").is_none() && get(api, "/").is_none());
    // The configs stay, and the failing one is named in what to do next.
    assert!(out_dir.join("web.toml").is_file() && out_dir.join("ok.toml").is_file());
    assert!(r.stderr.contains("warden start"), "{}", r.stderr);
}

#[test]
fn a_cutover_of_apps_that_run_already_says_they_keep_their_previous_config() {
    if !have_node() {
        eprintln!("skipping: needs node");
        return;
    }
    let d = Dir::new("again");
    let (web, api, ok) = (free_port(), free_port(), free_port());
    let root = three(&d, web, api, ok);
    let out_dir = d.path("out");
    let args = ["migrate-wattpm", root.to_str().unwrap(), "--out", out_dir.to_str().unwrap(), "--cutover", "overlap"];
    let r = d.warden(&args);
    assert_eq!(r.code, 0, "{}", r.all());
    wait_for("web", 20, || get(web, "/").is_some());
    assert!(r.stdout.contains("Warden serves 3 app(s)") && r.stdout.contains("warden stop web api ok"), "{}", r.stdout);

    let r = d.warden(&[args[0], args[1], args[2], args[3], args[4], args[5], "--overwrite"]);
    assert_eq!(r.code, 0, "{}", r.all());
    for name in ["web", "api", "ok"] {
        assert!(
            r.stdout.contains(&format!("{name}: already running with its previous config"))
                && r.stdout.contains(&format!("warden reload {name}")),
            "{}",
            r.stdout
        );
    }
    assert!(!r.stdout.contains("starting next to Watt"), "nothing was started again:\n{}", r.stdout);
    assert!(!r.stdout.contains("Warden serves"), "{}", r.stdout);
    assert_eq!(get(web, "/").as_deref(), Some("web"));
}
