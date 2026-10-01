//! warden: a fast, crash-safe supervisor for Bun and Node apps.
//! See docs/architecture.md for the design and the findings behind it.

// Unsafe code is confined to `sys` (syscall wrappers, each tested) and the
// two `Command::pre_exec` call sites; everything else is checked by the compiler.
#![deny(unsafe_code)]

mod cli;
mod config;
mod control;
mod daemon;
mod doctor;
mod events;
mod fleet;
mod guard;
mod health;
mod ids;
mod logging;
mod logview;
mod metrics;
mod migrate;
mod networking;
mod process;
mod restart;
mod schedule;
mod signals;
mod startup;
mod static_server;
mod supervisor;
#[allow(unsafe_code)]
mod sys;
mod systemd;
mod worker;

use cli::Command;
use std::path::PathBuf;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Internal: the worker process of an app with a [static] section.
    if argv.first().map(String::as_str) == Some("serve-static") {
        std::process::exit(static_server::main());
    }
    let mut args = match cli::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("warden: {e}\n\nRun `warden --help` for the commands.");
            std::process::exit(2);
        }
    };
    // One thread is plenty: Warden only waits on children, timers and sockets.
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("warden: cannot start the async runtime: {e}");
            std::process::exit(1);
        }
    };
    let command = std::mem::replace(&mut args.command, Command::Help);
    let config_path = || args.config.clone().unwrap_or_else(|| PathBuf::from("warden.toml"));
    let code = match command {
        Command::Help => {
            print!("{}", cli::USAGE);
            0
        }
        Command::Version => {
            println!("warden {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Command::Check => {
            let path = config_path();
            match config::Config::load(&path) {
                Ok(c) => {
                    println!(
                        "{}: ok ({} × {} worker(s), socket {})",
                        path.display(),
                        c.app.name,
                        c.workers.count,
                        c.socket_path().display()
                    );
                    0
                }
                Err(e) => {
                    eprintln!("warden: {e}");
                    1
                }
            }
        }
        Command::Run => run_supervisor(&rt, &args, config_path()),
        Command::Act(action) => match single_app_precheck(&args, &action) {
            Some(code) => code,
            None => rt.block_on(fleet::act(&args, &action)),
        },
        Command::Start { what, opts } => rt.block_on(fleet::start(&args, &what, &opts)),
        Command::Serve { dir, port, opts } => rt.block_on(fleet::serve(&args, &dir, port, &opts)),
        Command::Delete { target } => rt.block_on(fleet::delete(&args, &target)),
        Command::Save => rt.block_on(fleet::save(&args)),
        Command::Resurrect => rt.block_on(fleet::resurrect(&args)),
        Command::Startup(want) => rt.block_on(startup::startup(&args, want)),
        Command::Unstartup(want) => rt.block_on(startup::unstartup(&args, want)),
        Command::Kill => rt.block_on(fleet::kill(&args)),
        Command::Top => rt.block_on(fleet::top(&args)),
        Command::Doctor => rt.block_on(doctor::run(&args)),
        Command::Pm2Migrate(ref o) => rt.block_on(migrate::run(&args, o)),
        Command::Daemon(cli::DaemonCmd::Run { background: false, resurrect }) => daemon::main(&rt, resurrect),
        Command::Daemon(cli::DaemonCmd::Run { background: true, resurrect }) => {
            rt.block_on(daemon::client::start_background(resurrect))
        }
        Command::Daemon(cli::DaemonCmd::Status) => rt.block_on(daemon::client::status(args.json)),
        Command::Daemon(cli::DaemonCmd::Stop) => rt.block_on(daemon::client::stop()),
        Command::Daemon(cli::DaemonCmd::Check) => daemon::client::check(args.config.clone()),
        Command::Daemon(cli::DaemonCmd::Reload) => rt.block_on(daemon::client::reload()),
        Command::Events { logs, interval_ms } => {
            daemon::client::events(&rt, args.target.clone(), args.json, logs, interval_ms)
        }
    };
    std::process::exit(code);
}

/// With `-c`, a config that can't be read at all is an error (never guess
/// another app's socket); a config that doesn't parse still lets the
/// command reach Warden when its socket can be found (a reload then reports
/// the config error itself and changes nothing).
fn single_app_precheck(args: &cli::Args, action: &cli::Action) -> Option<i32> {
    if args.socket.is_some() {
        return None;
    }
    let path = args.config.as_ref()?;
    match config::Config::load(path) {
        Ok(_) => None,
        Err(e) => match config::socket_path_lenient(path) {
            Some(_) => {
                if !matches!(action, cli::Action::Reload { .. }) {
                    eprintln!("warden: warning: {e}");
                }
                None
            }
            None => {
                eprintln!("warden: {e}\n(pass --config or --socket)");
                Some(2)
            }
        },
    }
}

fn run_supervisor(rt: &tokio::runtime::Runtime, args: &cli::Args, path: PathBuf) -> i32 {
    let mut c = match config::Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warden: {e}");
            return 1;
        }
    };
    if let Some(s) = args.socket.clone() {
        c.control.socket = Some(s);
    }
    logging::init(c.logging.level, c.logging.timestamps, c.log_files());
    guard::install_panic_hook();
    let cfg_path = std::fs::canonicalize(&path).unwrap_or(path);
    let run = std::panic::AssertUnwindSafe(|| rt.block_on(supervisor::run(c, Some(cfg_path))));
    let code = match std::panic::catch_unwind(run) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            error!(
                "Warden could not start",
                error = e,
                hint = "fix the problem named in `error`; `warden doctor` checks the usual causes",
            );
            1
        }
        // The panic hook has already printed where and why.
        Err(_) => {
            error!(
                "Warden's main loop panicked; exiting (workers drain and exit, systemd restarts Warden)",
                hint = "this is a Warden bug: please report it with the log lines above",
            );
            101
        }
    };
    // Queued lines would be lost on exit: give the writer a moment.
    logging::flush(std::time::Duration::from_secs(1));
    code
}
