//! `warden-gui`: Warden's native GUI (a client of wardend). See gui/README.md.

use warden_gui::app::Options;

const USAGE: &str = "\
warden-gui: every app on a host and its live monitoring, from wardend.

Usage: warden-gui [--socket PATH] [--warden PATH] [--theme system|warden|light|dark]
                  [--ssh USER@HOST [--remote-socket PATH] [--remote-warden PATH]]

  --socket PATH         wardend's socket on this machine (default: where `warden` puts it)
  --warden PATH         the warden CLI here, for Add app, Edit config, Start wardend
                        (default: next to warden-gui, else on PATH)
  --theme NAME          system (default): the desktop's own colors on macOS and GNOME/Ubuntu,
                        light or dark as it is; warden: Warden's palette, light or dark as the
                        desktop is; light or dark: Warden's palette, fixed (or WARDEN_GUI_THEME)
  --ssh USER@HOST       a remote host, through `ssh -L` (your SSH agent and keys; never a password)
  --remote-socket PATH  wardend's socket there (default: /run/warden/wardend.sock, root's wardend)
  --remote-warden PATH  the warden CLI there (default: warden, on its PATH)
  -V, --version         print the version

Environment: ICED_BACKEND=tiny-skia forces the CPU renderer when built with the GPU one.
";

fn parse(args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let mut o = Options::default();
    let mut args = args.peekable();
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value; see --help"));
        match a.as_str() {
            "--socket" => o.socket = Some(value("--socket")?.into()),
            "--warden" => o.warden = Some(value("--warden")?.into()),
            "--theme" => {
                let v = value("--theme")?;
                o.theme = Some(
                    warden_gui::system::Source::parse(&v)
                        .ok_or_else(|| format!("--theme is system, warden, light or dark, not {v:?}"))?,
                );
            }
            "--ssh" => o.ssh = Some(value("--ssh")?),
            "--remote-socket" => o.remote_socket = Some(value("--remote-socket")?),
            "--remote-warden" => o.remote_warden = Some(value("--remote-warden")?),
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("warden-gui {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => return Err(format!("unknown argument {other:?}; see --help")),
        }
    }
    Ok(Some(o))
}

fn main() -> std::process::ExitCode {
    let opts = match parse(std::env::args().skip(1)) {
        Ok(Some(o)) => o,
        Ok(None) => return std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("warden-gui: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    match warden_gui::run(opts) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "warden-gui: the window could not be opened: {e}. A desktop session (Wayland or X11) is needed; over \
                 SSH use the GUI on your own machine with --ssh instead"
            );
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Option<Options>, String> {
        parse(s.split_whitespace().map(String::from))
    }

    #[test]
    fn arguments() {
        assert_eq!(p("").unwrap(), Some(Options::default()));
        let light = warden_gui::system::Source {
            colors: warden_gui::system::Colors::Warden,
            mode: warden_gui::system::Mode::Light,
        };
        assert_eq!(p("--theme light").unwrap().unwrap().theme, Some(light));
        assert!(p("--theme pink").unwrap_err().contains("system, warden, light or dark"));
        let o = p("--ssh deploy@web-1 --remote-socket /run/user/1000/warden/wardend.sock").unwrap().unwrap();
        assert_eq!(o.ssh.as_deref(), Some("deploy@web-1"));
        assert_eq!(o.remote_socket.as_deref(), Some("/run/user/1000/warden/wardend.sock"));
        assert_eq!(p("--socket /tmp/w.sock").unwrap().unwrap().socket, Some("/tmp/w.sock".into()));
        assert_eq!(p("--warden /opt/w/warden").unwrap().unwrap().warden, Some("/opt/w/warden".into()));
        assert!(p("--ssh").unwrap_err().contains("needs a value"));
        assert!(p("--bogus").unwrap_err().contains("unknown"));
        assert_eq!(p("--version").unwrap(), None);
    }
}
