//! Project tasks, run through the cargo aliases in `.cargo/config.toml`:
//!
//!   cargo xtask bench [--quick] [--only SUITES] [--duration S] [--no-readme]
//!                     [--app-cpus L] [--loadgen-cpus L] [--loadgen oha|wrk] [--rounds N]
//!   cargo bench-all                       (the same as `cargo xtask bench`)
//!   cargo xtask profile [bench/profile.ts options] (per-request cost, perf top symbols)
//!   cargo xtask chaos [--minutes N] [--seed S] (a chaos soak; see chaos/mod.rs)
//!   cargo xtask release VERSION [OPTIONS] (also `cargo release`; see release.rs)
//!
//! `bench` checks the tools the benchmarks need, builds Warden in release
//! mode, runs every suite in `bench/` against PM2, Watt (wattpm), nginx and
//! `serve`, then writes `bench/results/latest.md`, copies each suite's raw
//! JSON to `bench/results/latest/`, and replaces the README's benchmark
//! tables (between `<!-- bench:start -->` and `<!-- bench:end -->`).
//! Nothing here is part of `cargo build`.

mod chaos;
mod release;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Instant, SystemTime};

struct Suite {
    name: &'static str,
    title: &'static str,
    about: &'static str,
    script: &'static str,
    args: &'static [&'static str],
}

const SUITES: &[Suite] = &[
    Suite {
        name: "node-http",
        title: "Node.js app (node:http)",
        about: "The same app under each manager: 4 workers, one port. PM2 in cluster mode, Watt with 4 worker threads, Warden with 4 processes.",
        script: "bench/run.ts",
        args: &["--app", "node-http"],
    },
    Suite {
        name: "nest-node",
        title: "NestJS app on Node.js",
        about: "A minimal NestJS (Express) app with the same endpoints, 4 workers.",
        script: "bench/run.ts",
        args: &["--app", "nest-node"],
    },
    Suite {
        name: "bun-http",
        title: "Bun app (Bun.serve)",
        about: "4 workers. PM2 has no cluster mode for Bun, so it runs 4 fork-mode instances sharing the port with reusePort. Watt does not run Bun apps.",
        script: "bench/run.ts",
        args: &["--app", "bun-http"],
    },
    Suite {
        name: "nest-bun",
        title: "NestJS app on Bun",
        about: "The NestJS app on Bun, 4 workers; Warden also in worker (thread) mode.",
        script: "bench/run.ts",
        args: &["--app", "nest-bun"],
    },
    Suite {
        name: "static",
        title: "Static files",
        about: "`warden serve` against nginx, `pm2 serve` and the `serve` package, 4 workers each (serve has no cluster mode).",
        script: "bench/static.ts",
        args: &["--scenarios", "warden,warden-nocache,nginx,pm2-serve,serve"],
    },
    Suite {
        name: "logs",
        title: "Log-heavy apps",
        about: "What capturing worker output costs the manager. Both write the app's stdout to a log file; warden-direct splices it there unparsed (worker_output = \"direct\").",
        script: "bench/logs.ts",
        args: &[],
    },
    Suite {
        name: "fleet",
        title: "Many apps on one host",
        about: "10 apps (one idle Node process each): the manager's memory and CPU, and how fast everyday commands answer. Warden's managers are one supervisor per app plus wardend.",
        script: "bench/fleet.ts",
        args: &[],
    },
    Suite {
        name: "longlived",
        title: "WebSockets and SSE through a rolling restart",
        about: "50 WebSocket and 50 SSE clients stay connected while all 4 workers are replaced (`pm2 reload`: cluster mode for Node, fork mode for Bun; `warden restart` with the default `long_lived_timeout` of 2 s). Each client reconnects at once. Clean: a WebSocket close frame or the SSE stream's last chunk; abnormal: cut without one (a browser reports 1006, EventSource an error).",
        script: "bench/longlived.ts",
        args: &[],
    },
];

const USAGE: &str = "\
cargo xtask TASK [OPTIONS]

TASKS:
    bench      Run every benchmark suite, update README.md (also: cargo bench-all)
    profile    What one request costs a server (CPU time, syscalls) and where the time
               goes (perf record, top symbols); Warden built with symbols
    chaos      A chaos soak: a fleet under load, random faults, invariants checked
               (docs/chaos.md)
    release    Release a version: checks, version bump, tag, push, build the macOS
               archives (on a Mac), then follow the Release workflow to the GitHub
               Release (also: cargo release)
    dist-macos Build the macOS release archives on this Mac and upload them to the
               draft release the Release workflow takes them from (also: cargo dist-macos)

`cargo xtask TASK --help` lists a task's options.
";

const BENCH_USAGE: &str = "\
cargo xtask bench [OPTIONS]      (also: cargo bench-all)

Runs every benchmark suite and updates README.md and bench/results/latest.md.

OPTIONS:
    --quick            Short runs (3 s per measurement): a smoke test, not results to publish
    --only SUITES      Comma-separated subset of: node-http, nest-node, bun-http, nest-bun,
                       static, logs, fleet, longlived
    --duration S       Seconds per load measurement (default 10)
    --no-readme        Leave README.md alone (still writes bench/results/latest.md)
    --app-cpus L       Pin the apps and their managers to these CPUs (taskset list: 0-3,6)
    --loadgen-cpus L   Pin the load generator to these CPUs
    --loadgen G        oha (default) or wrk (lighter: more CPU left to the apps)
    --rounds N         Static files: run the servers N times, interleaved, and report
                       medians (an A/B a noisy machine moves less)
    -h, --help         This help

NEEDS: Linux, bun, node >= 22.12, oha (`cargo install oha`; or wrk with --loadgen
wrk), npm (installs the managers compared with into bench/node_modules on first
run); nginx is optional; taskset (util-linux) with --app-cpus / --loadgen-cpus.
";

const PROFILE_USAGE: &str = "\
cargo xtask profile [OPTIONS]

Builds Warden with symbols (the `profiling` profile: release, not stripped) and runs
bench/profile.ts with --perf: per target, req/s, server CPU time and context switches
per request, and where the CPU time goes (perf record -e cpu-clock, top symbols).
Every option is bench/profile.ts's:

    --targets a,b      warden, warden-nocache, nginx, bun, bun-shim, node, node-shim;
                       warden@/path/bin, bun-shim@/path/shim.mjs, warden:key=value,...
    --path P           URL path (static: /index.html, /assets/app.3f9a2c1b.js, /media/video.bin)
    --requests N       Requests per measurement (default 200000)
    --rounds N         Interleaved rounds, medians reported
    --strace           Also count syscalls per request
    --call-graph       perf record -g
    --app-cpus L, --loadgen-cpus L, --loadgen oha|wrk

Example: cargo xtask profile --targets warden,nginx --path /assets/app.3f9a2c1b.js --strace
NEEDS: Linux, bun, oha, perf (linux-tools), strace with --strace; nginx for its target.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("bench") => match bench(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask: {e}");
                ExitCode::FAILURE
            }
        },
        Some("profile") => match profile(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask: {e}");
                ExitCode::FAILURE
            }
        },
        Some("chaos") => match chaos::main(&args[1..], &root()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask: {e}");
                ExitCode::FAILURE
            }
        },
        Some("release") => match release::main(&args[1..], &root()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask: {e}");
                ExitCode::FAILURE
            }
        },
        Some("dist-macos") => match release::dist_macos(&args[1..], &root()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask: {e}");
                ExitCode::FAILURE
            }
        },
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("xtask: unknown task {other:?}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
struct Options {
    quick: bool,
    only: Option<Vec<String>>,
    duration: Option<String>,
    readme: bool,
    /// `--app-cpus L`, `--loadgen-cpus L`, `--loadgen G`: passed to every suite.
    pinning: Vec<String>,
    loadgen: String,
    rounds: Option<String>,
}

/// A taskset CPU list: `0`, `0-3`, `0,2,4-7`.
fn cpu_list(v: &str) -> bool {
    !v.is_empty()
        && v.split(',').all(|part| {
            let mut ends = part.splitn(2, '-');
            let ok = |s: Option<&str>| s.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
            match (ends.next(), ends.next()) {
                (a, None) => ok(a),
                (a, b) => ok(a) && ok(b),
            }
        })
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        quick: false,
        only: None,
        duration: None,
        readme: true,
        pinning: Vec::new(),
        loadgen: "oha".into(),
        rounds: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--quick" => o.quick = true,
            "--no-readme" => o.readme = false,
            "--app-cpus" | "--loadgen-cpus" => {
                let v = it.next().ok_or(format!("{a} needs a CPU list (taskset -c syntax, e.g. 0-3)"))?;
                if !cpu_list(v) {
                    return Err(format!("{a} {v:?} is not a CPU list (taskset -c syntax, e.g. 0-3 or 0,2)"));
                }
                o.pinning.extend([a.clone(), v.clone()]);
            }
            "--loadgen" => {
                let v = it.next().ok_or("--loadgen needs oha or wrk")?;
                if v != "oha" && v != "wrk" {
                    return Err(format!("--loadgen {v:?}: oha or wrk"));
                }
                o.loadgen = v.clone();
                o.pinning.extend([a.clone(), v.clone()]);
            }
            "--rounds" => {
                let v = it.next().ok_or("--rounds needs a number")?;
                if !v.parse::<u32>().is_ok_and(|n| n >= 1) {
                    return Err(format!("--rounds {v:?} is not a number of rounds (1 or more)"));
                }
                o.rounds = Some(v.clone());
            }
            "--only" => {
                let v = it.next().ok_or("--only needs a list of suites")?;
                let names: Vec<String> = v.split(',').map(|s| s.trim().to_string()).collect();
                for n in &names {
                    if !SUITES.iter().any(|s| s.name == n) {
                        let all: Vec<&str> = SUITES.iter().map(|s| s.name).collect();
                        return Err(format!("unknown suite {n:?}; one of {}", all.join(", ")));
                    }
                }
                o.only = Some(names);
            }
            "--duration" => {
                let v = it.next().ok_or("--duration needs seconds")?;
                v.parse::<u32>().map_err(|_| format!("--duration {v:?} is not a number of seconds"))?;
                o.duration = Some(v.clone());
            }
            "-h" | "--help" => {
                print!("{BENCH_USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other:?}\n\n{BENCH_USAGE}")),
        }
    }
    Ok(o)
}

fn root() -> PathBuf {
    // xtask/ is one level below the repository root.
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

/// Output of `cmd args` (first line), or None if it can't run.
fn tool_version(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    text.lines().next().map(|l| l.trim().to_string())
}

fn run(cmd: &mut Command, what: &str) -> Result<(), String> {
    let st = cmd.status().map_err(|e| format!("{what}: {e}"))?;
    if st.success() { Ok(()) } else { Err(format!("{what} failed ({st})")) }
}

/// Everything the suites need, installing the npm packages if missing.
fn check_prerequisites(root: &Path, o: &Options) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    if !cfg!(target_os = "linux") {
        return Err("the benchmarks read /proc and use SO_REUSEPORT balancing: run them on Linux".into());
    }
    tool_version("bun", &["--version"])
        .ok_or("bun is not installed: https://bun.sh (curl -fsSL https://bun.sh/install | bash)")?;
    let node = tool_version("node", &["--version"]).ok_or("node is not installed (need 22.12 or newer)")?;
    let v: Vec<u32> = node.trim_start_matches('v').split('.').filter_map(|x| x.parse().ok()).collect();
    if v.len() < 2 || (v[0], v[1]) < (22, 12) {
        return Err(format!("node {node} is too old: the Node apps listen with reusePort, which needs 22.12+"));
    }
    if o.loadgen == "wrk" {
        tool_version("wrk", &["--version"])
            .or_else(|| Command::new("wrk").arg("--version").output().ok().map(|_| "wrk".into()))
            .ok_or("wrk (--loadgen wrk) is not installed: apt install wrk, or leave out --loadgen")?;
    } else {
        tool_version("oha", &["--version"]).ok_or("oha (the load generator) is not installed: cargo install oha")?;
    }
    if o.pinning.iter().any(|a| a.ends_with("-cpus")) {
        tool_version("taskset", &["--version"])
            .ok_or("--app-cpus / --loadgen-cpus need taskset (util-linux) on PATH")?;
    }
    let bench = root.join("bench");
    let need = ["pm2", "wattpm", "serve", "@platformatic/node"];
    if need.iter().any(|p| !bench.join("node_modules").join(p).join("package.json").exists()) {
        eprintln!("xtask: installing the process managers to compare with (npm ci in bench/)");
        run(Command::new("npm").args(["ci", "--no-audit", "--no-fund"]).current_dir(&bench), "npm ci in bench/")?;
    }
    let nest = bench.join("nest");
    if !nest.join("node_modules/@nestjs/core/package.json").exists() {
        eprintln!("xtask: installing the NestJS app's dependencies (npm ci in bench/nest/)");
        run(Command::new("npm").args(["ci", "--no-audit", "--no-fund"]).current_dir(&nest), "npm ci in bench/nest/")?;
    }
    if !["/usr/sbin/nginx", "/usr/local/sbin/nginx", "/usr/bin/nginx"].iter().any(|p| Path::new(p).exists()) {
        notes.push("nginx is not installed: the static-files table has no nginx column".to_string());
    }
    match std::fs::read_to_string("/proc/sys/net/ipv4/tcp_migrate_req").map(|s| s.trim().to_string()) {
        Ok(v) if v == "1" => {}
        Ok(_) => notes.push(
            "net.ipv4.tcp_migrate_req is 0: a rolling restart can reset a connection that was waiting in a \
             closing worker's accept queue, for every manager. `sysctl -w net.ipv4.tcp_migrate_req=1` \
             (what `warden startup` configures) makes it 0."
                .to_string(),
        ),
        Err(_) => {}
    }
    Ok(notes)
}

/// The newest `bench/results/*-<name>.json` written after `since`.
fn newest_result(root: &Path, name: &str, since: SystemTime) -> Option<PathBuf> {
    let dir = root.join("bench/results");
    let suffix = format!("-{name}.json");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(&suffix))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .filter(|(t, _)| *t >= since)
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// The CPU's name: /proc/cpuinfo's "model name" (x86), else lscpu's "Model
/// name" (ARM's /proc/cpuinfo has none: lscpu decodes the part number,
/// e.g. Neoverse-N2).
fn cpu_model() -> Option<String> {
    let field = |text: &str, key: &str| {
        text.lines()
            .find(|l| l.trim_start().starts_with(key))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty() && v != "-")
    };
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    field(&cpuinfo, "model name").or_else(|| {
        let out = Command::new("lscpu").env("LC_ALL", "C").output().ok()?;
        field(&String::from_utf8_lossy(&out.stdout), "Model name")
    })
}

fn machine() -> String {
    let cpu = cpu_model().unwrap_or_else(|| "unknown CPU".into());
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    let mem = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| t.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|k| k.parse::<u64>().ok()))
        .map(|kb| format!("{:.0} GB RAM", kb as f64 / 1_048_576.0))
        .unwrap_or_default();
    let kernel = tool_version("uname", &["-rm"]).unwrap_or_default();
    format!("{cpus} CPUs ({cpu}), {mem}, Linux {kernel}")
}

fn bench(args: &[String]) -> Result<(), String> {
    let o = parse(args)?;
    let root = root();
    let notes = check_prerequisites(&root, &o)?;
    for n in &notes {
        eprintln!("xtask: note: {n}");
    }
    eprintln!("xtask: building Warden (release)");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    run(
        Command::new(&cargo).args(["build", "--release", "--package", "warden"]).current_dir(&root),
        "cargo build --release",
    )?;

    let suites: Vec<&Suite> =
        SUITES.iter().filter(|s| o.only.as_ref().is_none_or(|only| only.iter().any(|n| n == s.name))).collect();
    let latest_dir = root.join("bench/results/latest");
    std::fs::create_dir_all(&latest_dir).map_err(|e| format!("creating {}: {e}", latest_dir.display()))?;
    let mut sections = Vec::new();
    let mut failed = Vec::new();
    let t_all = Instant::now();
    for s in &suites {
        let mut cmd = Command::new("bun");
        cmd.arg(s.script).args(s.args).current_dir(&root).stdout(Stdio::piped()).stderr(Stdio::inherit());
        // fleet and longlived measure events, not a load over time.
        if s.script != "bench/fleet.ts" && s.script != "bench/longlived.ts" {
            let d = o.duration.clone().unwrap_or_else(|| if o.quick { "3".into() } else { "10".into() });
            if s.script == "bench/logs.ts" {
                cmd.args(["--seconds", &d]);
            } else {
                cmd.args(["--duration", &d]);
            }
        }
        if o.quick && s.script == "bench/logs.ts" {
            cmd.args(["--mb", "100"]);
        }
        if o.quick && s.script == "bench/longlived.ts" {
            cmd.args(["--clients", "10"]);
        }
        cmd.args(&o.pinning);
        if let (Some(n), "bench/static.ts") = (&o.rounds, s.script) {
            cmd.args(["--rounds", n]);
        }
        eprintln!("\nxtask: === {} ({} {}) ===", s.name, s.script, s.args.join(" "));
        let since = SystemTime::now() - std::time::Duration::from_secs(1);
        let t0 = Instant::now();
        let out = cmd.output().map_err(|e| format!("running bun {}: {e}", s.script))?;
        let table = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || table.is_empty() {
            eprintln!("xtask: {} failed ({}); its section is left out", s.name, out.status);
            failed.push(s.name);
            continue;
        }
        eprintln!("{table}\nxtask: {} took {:.0} s", s.name, t0.elapsed().as_secs_f64());
        if let Some(json) = newest_result(&root, s.name, since) {
            let _ = std::fs::copy(&json, latest_dir.join(format!("{}.json", s.name)));
        }
        sections.push(format!("### {}\n\n{}\n\n{}\n", s.title, s.about, table));
    }

    let date = tool_version("date", &["-u", "+%Y-%m-%d"]).unwrap_or_default();
    let pinned = |flag: &str| o.pinning.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone());
    let placement = match (pinned("--app-cpus"), pinned("--loadgen-cpus")) {
        (None, None) => {
            "runs on the same CPUs as the apps, so compare columns with each other, not with other machines".to_string()
        }
        (apps, loadgen) => format!(
            "runs on CPUs {}, the apps and their managers on CPUs {} (taskset); compare columns with each other",
            loadgen.as_deref().unwrap_or("any"),
            apps.as_deref().unwrap_or("any")
        ),
    };
    let mut md = format!(
        "<!-- Generated by `cargo xtask bench`{}; edit xtask/src/main.rs or bench/*.ts, not this text. -->\n\n\
         Measured {date} on {}{}. The load generator ({}, 64 connections) {placement}. \
         Raw numbers: `bench/results/latest/`.\n\n",
        if o.quick { " --quick (short smoke-test runs)" } else { "" },
        machine(),
        if o.quick { ", **quick run**" } else { "" },
        o.loadgen,
    );
    if !notes.is_empty() {
        md += &notes.iter().map(|n| format!("> Note: {n}\n")).collect::<String>();
        md += "\n";
    }
    md += &sections.join("\n");
    let latest = root.join("bench/results/latest.md");
    std::fs::write(&latest, &md).map_err(|e| format!("writing {}: {e}", latest.display()))?;
    eprintln!(
        "\nxtask: wrote {} ({} suites in {:.0} min)",
        latest.display(),
        sections.len(),
        t_all.elapsed().as_secs_f64() / 60.0
    );

    if o.readme && failed.is_empty() && o.only.is_none() {
        update_readme(&root.join("README.md"), &md)?;
    } else if o.readme {
        eprintln!("xtask: README.md left as it was (only a subset ran, or a suite failed)");
    }
    if failed.is_empty() { Ok(()) } else { Err(format!("suites failed: {}", failed.join(", "))) }
}

/// `cargo xtask profile`: Warden with symbols, then bench/profile.ts --perf.
fn profile(args: &[String]) -> Result<(), String> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{PROFILE_USAGE}");
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        return Err("profiling reads /proc and runs perf: run it on Linux".into());
    }
    tool_version("bun", &["--version"]).ok_or("bun is not installed: https://bun.sh")?;
    let root = root();
    eprintln!("xtask: building Warden with symbols (cargo build --profile profiling)");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    run(
        Command::new(&cargo).args(["build", "--profile", "profiling", "--package", "warden"]).current_dir(&root),
        "cargo build --profile profiling",
    )?;
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
    let bin = target.join("profiling/warden");
    let mut cmd = Command::new("bun");
    cmd.arg("bench/profile.ts").arg("--warden").arg(&bin).current_dir(&root);
    if !args.iter().any(|a| a == "--perf") {
        cmd.arg("--perf");
    }
    run(cmd.args(args), "bun bench/profile.ts")
}

const START: &str = "<!-- bench:start -->";
const END: &str = "<!-- bench:end -->";

/// Replace the text between the README's bench markers.
fn update_readme(path: &Path, md: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let (Some(a), Some(b)) = (text.find(START), text.find(END)) else {
        return Err(format!("{} has no {START} … {END} markers; add them where the tables go", path.display()));
    };
    if b < a {
        return Err(format!("{}: {END} comes before {START}", path.display()));
    }
    let new = format!("{}{START}\n{}\n{}", &text[..a], md.trim_end(), &text[b..]);
    std::fs::write(path, new).map_err(|e| format!("writing {}: {e}", path.display()))?;
    eprintln!("xtask: updated the benchmark tables in {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readme_markers_are_replaced_in_place() {
        let dir = std::env::temp_dir().join(format!("xtask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("README.md");
        std::fs::write(&p, format!("# T\nintro\n{START}\nold tables\n{END}\nafter\n")).unwrap();
        update_readme(&p, "new tables\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("# T\nintro\n{START}\nnew tables\n{END}\nafter\n"));
        update_readme(&p, "again").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("# T\nintro\n{START}\nagain\n{END}\nafter\n"));
        std::fs::write(&p, "no markers").unwrap();
        assert!(update_readme(&p, "x").unwrap_err().contains("markers"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn options() {
        let a = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert!(parse(&a("--quick")).unwrap().quick);
        assert_eq!(parse(&a("--only static,logs")).unwrap().only.unwrap(), vec!["static", "logs"]);
        assert_eq!(parse(&a("--only longlived")).unwrap().only.unwrap(), vec!["longlived"]);
        assert!(parse(&a("--only nope")).unwrap_err().contains("unknown suite"));
        assert!(parse(&a("--duration x")).is_err());
        assert!(!parse(&a("--no-readme")).unwrap().readme);
        let o = parse(&a("--app-cpus 0-3 --loadgen-cpus 4,5 --loadgen wrk --rounds 3")).unwrap();
        assert_eq!(o.pinning, ["--app-cpus", "0-3", "--loadgen-cpus", "4,5", "--loadgen", "wrk"]);
        assert_eq!((o.loadgen.as_str(), o.rounds.as_deref()), ("wrk", Some("3")));
        assert!(parse(&a("--app-cpus x")).unwrap_err().contains("CPU list"));
        assert!(parse(&a("--app-cpus 1-")).is_err());
        assert!(parse(&a("--loadgen ab")).unwrap_err().contains("oha or wrk"));
        assert!(parse(&a("--rounds 0")).is_err());
    }
}
