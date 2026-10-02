//! Time limits for connections that are waiting for a request, without a
//! timer per request.
//!
//! A connection waiting for a request head has a limit: 10 s for the first
//! request (a client that connects and says nothing, or trickles a head in),
//! 15 s between requests on a kept-alive connection. The obvious way is a
//! `tokio::time::timeout` around every wait. That costs a timer-wheel insert
//! and removal per request, and a connection that lives for a few hundred
//! microseconds also made the timer driver wake its own thread, an eventfd
//! `write(2)` per connection (seen with strace, one system call in seven).
//!
//! Instead each connection is entered here once, and records, with one
//! atomic store per request, when it began waiting and for how long it may.
//! A task ticking once a second (`Idle::sweep`) finds the connections over
//! their limit and shuts their sockets down; the pending read then returns
//! end-of-file and the connection ends like any other.
//!
//! The clock is whole seconds as of the last sweep, and a wait is stamped with
//! the *next* second, so a limit of `L` seconds ends between `L` and `L + 2`
//! after the wait began (never early, while the sweep runs about on time),
//! which is all a 10 or 15 second limit needs.
//!
//! The clock only moves when the sweep runs, so a worker that was stalled (a
//! blocking read of a hung disk, a stopped process, a full log pipe) has a
//! stale clock: connections that began waiting during the stall would look
//! as old as it. A sweep that comes `STALLED` or more seconds after the one
//! before gives every waiting connection a fresh wait and closes none.
//!
//! The descriptor is shut down from outside the task that owns it, so the
//! entry must be gone before the owner closes the descriptor, or a reused
//! number could be shut down. `Guard` does that: it is dropped (and removes
//! the entry, under the same lock the sweep holds while shutting down) before
//! the stream it was registered for (see `serve`).

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the sweep needs to know about one connection.
struct Conn {
    fd: RawFd,
    /// 0: not waiting (a request is being served, which has no limit).
    /// Otherwise `limit_secs << 32 | since_secs`: the second (on the sweep's
    /// clock) the wait began, and how long it may last.
    waiting: AtomicU64,
}

#[derive(Default)]
struct Slots {
    items: Vec<Option<Arc<Conn>>>,
    free: Vec<usize>,
}

/// A sweep this many seconds after the previous one found the worker stalled.
/// (Ticks a second apart read as 1 or, with a little jitter, 2.)
const STALLED: u32 = 3;

pub struct Idle {
    start: Instant,
    /// Whole seconds since `start`, as of the last sweep. Reading it is a
    /// load, where `Instant::now()` is a clock read for every request.
    now: AtomicU32,
    slots: Mutex<Slots>,
}

/// A registered connection. Dropping it removes the entry.
pub struct Guard {
    idle: Arc<Idle>,
    slot: usize,
    conn: Arc<Conn>,
}

fn pack(limit_secs: u64, since: u32) -> u64 {
    (limit_secs.max(1) << 32) | since as u64
}

/// What a wait that begins during second `now` is stamped with: the next one,
/// so the clock (which lags by up to a second) cannot make it end early.
fn stamp(now: u32) -> u32 {
    now.saturating_add(1)
}

impl Idle {
    pub fn new() -> Arc<Idle> {
        Arc::new(Idle { start: Instant::now(), now: AtomicU32::new(0), slots: Mutex::new(Slots::default()) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Slots> {
        // Nothing in here can panic while the lock is held; a poisoned lock
        // would still hold consistent data.
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Enter the connection on `fd`. Not waiting yet: `Guard::waiting`.
    pub fn register(self: &Arc<Self>, fd: RawFd) -> Guard {
        let conn = Arc::new(Conn { fd, waiting: AtomicU64::new(0) });
        let mut s = self.lock();
        let slot = match s.free.pop() {
            Some(i) => {
                s.items[i] = Some(conn.clone());
                i
            }
            None => {
                s.items.push(Some(conn.clone()));
                s.items.len() - 1
            }
        };
        drop(s);
        Guard { idle: self.clone(), slot, conn }
    }

    /// Shut down every connection that has waited longer than its limit;
    /// returns how many. `Instant` is monotonic, so a clock step cannot make
    /// this fire early.
    pub fn sweep(&self) -> usize {
        self.sweep_at(u32::try_from(self.start.elapsed().as_secs()).unwrap_or(u32::MAX))
    }

    /// `sweep`, at second `now` of the clock.
    fn sweep_at(&self, now: u32) -> usize {
        let before = self.now.swap(now, Ordering::Relaxed);
        let stalled = now.saturating_sub(before) >= STALLED;
        let s = self.lock();
        let mut closed = 0;
        for c in s.items.iter().flatten() {
            let w = c.waiting.load(Ordering::Relaxed);
            if w == 0 {
                continue;
            }
            let (limit, since) = (w >> 32, (w & 0xffff_ffff) as u32);
            if stalled {
                // The worker was not running: nobody here has been quiet.
                c.waiting.store(pack(limit, stamp(now)), Ordering::Relaxed);
                continue;
            }
            if (now as u64).saturating_sub(since as u64) < limit {
                continue;
            }
            // A request sitting unread in the socket: the client was not quiet,
            // the worker was behind (it has not been to this connection since
            // the data came). One more tick, not a whole new wait, so a client
            // cannot keep a connection by trickling bytes in.
            if crate::sys::has_unread(c.fd) {
                let since = stamp(now).saturating_sub(u32::try_from(limit).unwrap_or(u32::MAX));
                c.waiting.store(pack(limit, since), Ordering::Relaxed);
                continue;
            }
            // Still under the lock: the owner cannot have closed the
            // descriptor, its Guard has not been dropped.
            let _ = crate::sys::shutdown_both(c.fd);
            c.waiting.store(0, Ordering::Relaxed);
            closed += 1;
        }
        closed
    }
}

impl Guard {
    /// The connection begins waiting for a request head, for at most `limit`.
    pub fn waiting(&self, limit: Duration) {
        let since = stamp(self.idle.now.load(Ordering::Relaxed));
        self.conn.waiting.store(pack(limit.as_secs(), since), Ordering::Relaxed);
    }

    /// A request head has arrived: no limit while it is served.
    pub fn busy(&self) {
        self.conn.waiting.store(0, Ordering::Relaxed);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut s = self.idle.lock();
        s.items[self.slot] = None;
        s.free.push(self.slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    /// The state of the entry: limit and the second it was stamped with.
    fn state(g: &Guard) -> (u64, u32) {
        let w = g.conn.waiting.load(Ordering::Relaxed);
        (w >> 32, (w & 0xffff_ffff) as u32)
    }

    /// Whether the peer sees end-of-file (the connection was shut down).
    fn closed(peer: &mut UnixStream) -> bool {
        peer.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut buf = [0u8; 1];
        match peer.read(&mut buf) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => {
                assert!(matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));
                false
            }
        }
    }

    #[test]
    fn a_connection_over_its_limit_is_shut_down_and_one_under_it_is_not() {
        let idle = Idle::new();
        let (a, mut peer_a) = pair();
        let (b, mut peer_b) = pair();
        let ga = idle.register(a.as_raw_fd());
        let gb = idle.register(b.as_raw_fd());
        ga.waiting(Duration::from_secs(2));
        gb.waiting(Duration::from_secs(100));
        // Began during second 0, stamped 1: a's 2 s are up when the clock reads 3.
        assert_eq!(idle.sweep_at(1), 0);
        assert_eq!(idle.sweep_at(2), 0, "the wait began at some point in second 0: not two seconds yet");
        assert!(!closed(&mut peer_a));
        assert_eq!(idle.sweep_at(3), 1);
        assert!(closed(&mut peer_a), "a's peer sees end-of-file");
        assert!(!closed(&mut peer_b), "b is untouched");
        assert_eq!(idle.sweep_at(4), 0, "a is not shut down twice");
        drop((ga, gb));
    }

    #[test]
    fn a_wait_is_never_cut_short_by_a_clock_that_lags() {
        // The clock reads 7 (as of the last sweep) though it may be 7.9 s: the
        // wait is stamped 8, and a 3 s limit is up when the clock reads 11.
        let idle = Idle::new();
        let (a, _peer) = pair();
        let g = idle.register(a.as_raw_fd());
        idle.sweep_at(7);
        g.waiting(Duration::from_secs(3));
        assert_eq!(state(&g), (3, 8));
        assert_eq!(idle.sweep_at(8), 0);
        assert_eq!(idle.sweep_at(10), 0);
        assert_eq!(idle.sweep_at(11), 1);
    }

    #[test]
    fn a_busy_connection_has_no_limit() {
        let idle = Idle::new();
        let (a, _peer) = pair();
        let g = idle.register(a.as_raw_fd());
        g.waiting(Duration::from_secs(1));
        g.busy();
        assert_eq!(idle.sweep_at(1), 0);
        assert_eq!(idle.sweep_at(2), 0);
        assert_eq!(idle.sweep_at(1000 - 1), 0, "a long gap is a stall, not a reason to close a busy connection");
    }

    #[test]
    fn a_connection_with_a_request_waiting_unread_gets_one_more_tick_only() {
        let idle = Idle::new();
        let (a, mut peer) = pair();
        let g = idle.register(a.as_raw_fd());
        g.waiting(Duration::from_secs(5)); // stamped 1
        peer.write_all(b"GET / HTTP/1.1\r\n").unwrap();
        for now in 1..=5 {
            assert_eq!(idle.sweep_at(now), 0);
        }
        assert_eq!(idle.sweep_at(6), 0, "over its limit, but the data is unread: the worker was behind");
        assert_eq!(state(&g), (5, 2), "due again at the next sweep, not a whole new wait");
        // Once the data has been read and nothing else comes, it expires.
        let mut s = a.try_clone().unwrap();
        let mut buf = [0u8; 16]; // "GET / HTTP/1.1\r\n"
        s.read_exact(&mut buf).unwrap();
        assert_eq!(idle.sweep_at(7), 1);
        assert!(closed(&mut peer));
    }

    #[test]
    fn data_unread_at_every_sweep_keeps_a_connection_until_it_is_read() {
        // The renewal is a tick at a time: a connection with data unread at
        // every sweep is kept (the worker is that far behind), and once the
        // data is read the very next sweep closes it.
        let idle = Idle::new();
        let (a, mut peer) = pair();
        let g = idle.register(a.as_raw_fd());
        g.waiting(Duration::from_secs(2));
        peer.write_all(b"x").unwrap();
        for now in 1..20 {
            assert_eq!(idle.sweep_at(now), 0, "second {now}");
        }
        let mut s = a.try_clone().unwrap();
        s.read_exact(&mut [0u8; 1]).unwrap();
        assert_eq!(idle.sweep_at(20), 1);
    }

    #[test]
    fn a_stalled_worker_closes_nobody_and_everybody_gets_a_fresh_wait() {
        let idle = Idle::new();
        let (a, mut peer_a) = pair();
        let (b, mut peer_b) = pair();
        let ga = idle.register(a.as_raw_fd());
        let gb = idle.register(b.as_raw_fd());
        let gc = idle.register(b.as_raw_fd()); // busy: stays 0
        idle.sweep_at(10);
        ga.waiting(Duration::from_secs(2)); // stamped 11
        gc.busy();
        // The worker stops for 30 s. The clock reads 10 until the sweep, so a
        // connection that begins waiting after the stall is stamped 11 too.
        gb.waiting(Duration::from_secs(2));
        assert_eq!(idle.sweep_at(40), 0, "everything looks 29 s old, and nothing is");
        assert!(!closed(&mut peer_a) && !closed(&mut peer_b));
        assert_eq!(state(&ga), (2, 41));
        assert_eq!(state(&gb), (2, 41));
        assert_eq!(state(&gc).0, 0, "a busy one stays busy");
        assert_eq!(idle.sweep_at(41), 0);
        assert_eq!(idle.sweep_at(42), 0);
        assert_eq!(idle.sweep_at(43), 2, "two real seconds without a request: now they go");
        drop((ga, gb, gc));
    }

    #[test]
    fn sweeps_a_little_late_are_not_a_stall() {
        let idle = Idle::new();
        let (a, _peer) = pair();
        let g = idle.register(a.as_raw_fd());
        idle.sweep_at(10);
        g.waiting(Duration::from_secs(3)); // 11
        assert_eq!(idle.sweep_at(12), 0, "one tick came a second late");
        assert_eq!(state(&g), (3, 11), "not renewed");
        assert_eq!(idle.sweep_at(14), 1);
    }

    #[test]
    fn slots_are_reused_and_a_dropped_guard_leaves_nothing_to_shut_down() {
        let idle = Idle::new();
        let (a, _pa) = pair();
        let g1 = idle.register(a.as_raw_fd());
        g1.waiting(Duration::from_secs(1));
        drop(g1);
        assert_eq!(idle.sweep_at(10), 0, "its entry is gone");
        let (b, _pb) = pair();
        let g2 = idle.register(b.as_raw_fd());
        assert_eq!(g2.slot, 0, "the freed slot is taken again");
        assert_eq!(idle.lock().items.len(), 1);
    }

    #[test]
    fn the_real_clock_sweep_runs() {
        let idle = Idle::new();
        assert_eq!(idle.sweep(), 0);
    }
}
