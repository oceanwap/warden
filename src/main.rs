//! warden: a small supervisor for Bun/Node HTTP workers.
//! See docs/architecture.md for the design and the findings behind it.

mod cli;
mod config;
mod control;
mod guard;
mod health;
mod logging;
mod metrics;
mod networking;
mod process;
mod restart;
mod signals;
mod supervisor;
mod systemd;
mod worker;

use cli::Command;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = match cli::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("warden: {e}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    // One thread is plenty: Warden only waits on children, timers and sockets.
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("warden: cannot start runtime: {e}");
            std::process::exit(1);
        }
    };
    let command = std::mem::replace(&mut args.command, Command::Help);
    let code = match command {
        Command::Help => {
            print!("{}", cli::USAGE);
            0
        }
        Command::Version => {
            println!("warden {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Command::Check => match config::Config::load(&args.config) {
            Ok(c) => {
                println!(
                    "{}: ok ({} × {} worker(s), socket {})",
                    args.config.display(),
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
        },
        Command::Start => match config::Config::load(&args.config) {
            Ok(mut c) => {
                if let Some(s) = args.socket.clone() {
                    c.control.socket = Some(s);
                }
                logging::init(c.logging.level, c.logging.timestamps);
                guard::install_panic_hook();
                let run = std::panic::AssertUnwindSafe(|| rt.block_on(supervisor::run(c, Some(args.config.clone()))));
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
            Err(e) => {
                eprintln!("warden: {e}");
                1
            }
        },
        Command::Client(req) => client(&rt, req, &args, false),
        Command::Workers => client(&rt, control::Request::Status, &args, true),
    };
    std::process::exit(code);
}

fn client(rt: &tokio::runtime::Runtime, req: control::Request, args: &cli::Args, table_only: bool) -> i32 {
    let socket = match &args.socket {
        Some(s) => s.clone(),
        None => match config::Config::load(&args.config) {
            Ok(c) => c.socket_path(),
            Err(e) => match config::socket_path_lenient(&args.config) {
                // A reload reports the config error itself (and changes nothing).
                Some(s) => {
                    if !matches!(req, control::Request::Reload { .. }) {
                        eprintln!("warden: warning: {e}");
                    }
                    s
                }
                None => {
                    eprintln!("warden: {e}\n(pass --config or --socket)");
                    return 2;
                }
            },
        },
    };
    rt.block_on(cli::run_client(req, socket, args.json, table_only, args.no_wait))
}
