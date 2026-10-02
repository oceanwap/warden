//! Tying a child process to the window: on Linux the kernel sends it SIGTERM when its parent
//! dies (`PR_SET_PDEATHSIG`), so an `ssh -N` tunnel does not outlive a window that was killed (a
//! crash, SIGKILL, a log-out); `kill_on_drop` covers only a window that closes cleanly.
//!
//! macOS has no such call: there a tunnel left by a killed window stays until `ssh` is stopped by
//! hand (its socket sits in the folder named `warden-gui-<uid>`; the process is the `ssh -N -L` that
//! names it). Not for the commands run over ssh (`warden update --yes` on a server): ending those
//! with the window would stop them half way.
//!
//! This is the only place the GUI has `unsafe` (everywhere else it is denied), as `src/sys.rs` is
//! for Warden itself. `Command::pre_exec` is unsafe because its closure runs between fork and exec,
//! where only async-signal-safe calls are allowed; this closure makes two system calls and nothing
//! else.

/// `cmd`'s child gets SIGTERM when the thread that starts it ends. That thread must live as long
/// as the window does: a thread of the async runtime (a blocking-pool thread, which idles out,
/// would end the tunnel with it). Elsewhere than Linux it does nothing.
#[cfg(target_os = "linux")]
pub fn with_parent(cmd: &mut std::process::Command) {
    use rustix::process::{Signal, getpid, getppid, set_parent_process_death_signal};
    use std::os::unix::process::CommandExt;
    let parent = getpid();
    // SAFETY: the closure makes two raw system calls (prctl, getppid), allocates nothing, takes no
    // lock and captures one integer; the errors it returns are plain OS codes.
    unsafe {
        cmd.pre_exec(move || {
            set_parent_process_death_signal(Some(Signal::TERM))?;
            // The parent may have gone between fork and prctl, and then nobody will send the signal.
            if getppid() != Some(parent) {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub fn with_parent(cmd: &mut std::process::Command) {
    let _ = cmd;
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn wait(child: &mut std::process::Child, limit: Duration) -> Option<std::process::ExitStatus> {
        let t0 = Instant::now();
        while t0.elapsed() < limit {
            if let Ok(Some(st)) = child.try_wait() {
                return Some(st);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn a_child_gets_sigterm_when_the_thread_that_started_it_ends() {
        let mut child = std::thread::spawn(|| {
            let mut cmd = Command::new("sleep");
            cmd.arg("60");
            with_parent(&mut cmd);
            cmd.spawn().expect("sleep starts")
        })
        .join()
        .unwrap();
        let st = wait(&mut child, Duration::from_secs(5)).expect("the child ended with its parent thread");
        assert_eq!(st.signal(), Some(15), "by SIGTERM: {st:?}");
    }

    #[test]
    fn a_child_lives_on_while_its_parent_does() {
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        with_parent(&mut cmd);
        let mut child = cmd.spawn().expect("sleep starts");
        assert!(wait(&mut child, Duration::from_millis(300)).is_none(), "still running");
        child.kill().unwrap();
        let _ = child.wait();
    }

    #[test]
    fn a_child_started_without_it_outlives_its_thread() {
        let mut child =
            std::thread::spawn(|| Command::new("sleep").arg("60").spawn().expect("sleep starts")).join().unwrap();
        assert!(wait(&mut child, Duration::from_millis(300)).is_none(), "nothing ended it");
        child.kill().unwrap();
        let _ = child.wait();
    }
}
