//! Passing bytes both ways between the visitor and the app. Small messages
//! (HTTP/2 frames of a few hundred bytes) go through a buffer with one
//! read(2) and one write(2): cheaper than any kernel-side alternative at that
//! size. When a read fills the buffer the stream is carrying bulk data (a
//! download), and on Linux it switches to splice(2) through a pipe, so the
//! bytes are never copied into the router; back to the buffer when reads get
//! short again. Measured: 3.1 µs of router CPU per small HTTP/2 request
//! (nginx stream: 3.1), 36 µs per 64 KB response (nginx: 53).

use std::io;
use tokio::io::Interest;
use tokio::net::TcpStream;

const BUF: usize = 16 * 1024;

/// Both directions until both have ended.
pub(super) async fn both(visitor: &TcpStream, app: &TcpStream) {
    tokio::join!(one_way(visitor, app), one_way(app, visitor));
}

/// One direction until the end of stream (or an error), which is then passed
/// on (shutdown of the write side) so the other end finishes too.
async fn one_way(from: &TcpStream, to: &TcpStream) {
    let mut b = vec![0u8; BUF];
    #[cfg(target_os = "linux")]
    let mut pipe: Option<splice::Pipe> = None;
    #[cfg(target_os = "linux")]
    let mut bulk = false;
    loop {
        #[cfg(target_os = "linux")]
        if bulk {
            if pipe.is_none() {
                pipe = splice::Pipe::new().ok();
            }
            match &pipe {
                Some(p) => match p.move_once(from, to, 1 << 20).await {
                    Ok(n) if n > 0 => {
                        bulk = n >= BUF;
                        continue;
                    }
                    _ => break,
                },
                None => bulk = false,
            }
        }
        let n = match from.async_io(Interest::READABLE, || from.try_read(&mut b)).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if write_all(to, &b[..n]).await.is_err() {
            break;
        }
        #[cfg(target_os = "linux")]
        if n == BUF {
            bulk = true;
        }
    }
    let _ = shutdown_write(to);
}

async fn write_all(to: &TcpStream, mut b: &[u8]) -> io::Result<()> {
    while !b.is_empty() {
        let w = to.async_io(Interest::WRITABLE, || to.try_write(b)).await?;
        b = &b[w..];
    }
    Ok(())
}

fn shutdown_write(s: &TcpStream) -> io::Result<()> {
    use std::os::fd::AsFd;
    crate::sys::shutdown_write(s.as_fd())
}

#[cfg(target_os = "linux")]
mod splice {
    use std::io;
    use std::os::fd::{AsFd, OwnedFd};
    use tokio::io::Interest;
    use tokio::net::TcpStream;

    /// A pipe the bytes go through, socket → pipe → socket, in the kernel.
    pub(super) struct Pipe {
        read: OwnedFd,
        write: OwnedFd,
    }

    impl Pipe {
        pub(super) fn new() -> io::Result<Pipe> {
            // Blocking ends are fine: every splice is SPLICE_F_NONBLOCK.
            let (read, write) = crate::sys::pipe_cloexec()?;
            Ok(Pipe { read, write })
        }

        /// What one readiness of `from` has (up to `max` bytes), all of it
        /// written to `to`. 0 at the end of stream.
        pub(super) async fn move_once(&self, from: &TcpStream, to: &TcpStream, max: usize) -> io::Result<usize> {
            let n = from
                .async_io(Interest::READABLE, || crate::sys::splice(from.as_fd(), self.write.as_fd(), None, max))
                .await?;
            let mut left = n;
            while left > 0 {
                left -= to
                    .async_io(Interest::WRITABLE, || crate::sys::splice(self.read.as_fd(), to.as_fd(), None, left))
                    .await?;
            }
            Ok(n)
        }
    }
}
