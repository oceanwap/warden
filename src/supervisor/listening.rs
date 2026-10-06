//! What each worker listens on, for `Status.workers[].listening`.
//!
//! The OS adapter walks a worker's process tree (a wrapper such as `npm run
//! start` is the process Warden started; the server is below it), which
//! reads `/proc` (each process's sockets, and on Linux the listeners of the
//! namespace from one netlink request for all the workers) or asks libproc
//! for each process. A status request arrives every second while wardend or
//! a GUI watches, and a process with many connections has many sockets, so:
//!
//! - the first reading of a worker is taken on the spot (so `warden start`
//!   can print the port it waited for), and a worker that has listened on
//!   nothing is asked again quickly while it is young, since its port is
//!   about to appear;
//! - after that a reading is kept two seconds, and a stale one is *served*
//!   while a blocking thread reads the next, never on the supervisor's loop.

use crate::control::Listener;
use crate::platform;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a reading is kept.
pub(super) const TTL: Duration = Duration::from_secs(2);
/// How long a reading without any listener is kept while the worker is
/// young (see [`YOUNG`]).
pub(super) const TTL_EMPTY: Duration = Duration::from_millis(250);
/// A worker is young for this long after it starts.
pub(super) const YOUNG: Duration = Duration::from_secs(30);

/// Which sockets are Warden's own: the private health sockets of an app,
/// `<runtime dir>/<app>.h<instance>-<worker>.sock` (the shim's and the
/// static server's).
#[derive(Clone)]
pub(super) struct Own {
    pub(super) runtime_dir: PathBuf,
    pub(super) app: String,
}

impl Own {
    fn owns(&self, socket: &str) -> bool {
        let path = Path::new(socket);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        path.parent() == Some(self.runtime_dir.as_path())
            && name.starts_with(&format!("{}.h", self.app))
            && name.ends_with(".sock")
    }
}

#[derive(Default)]
struct Inner {
    by_pid: HashMap<u32, (Instant, Vec<Listener>)>,
    /// Pids a blocking thread is reading now.
    refreshing: HashSet<u32>,
}

#[derive(Clone, Default)]
pub(super) struct ListenerCache {
    inner: Arc<Mutex<Inner>>,
}

/// What the cache has for a pid.
#[derive(Debug, PartialEq)]
enum Plan {
    Fresh(Vec<Listener>),
    Stale(Vec<Listener>),
    Unknown,
}

impl ListenerCache {
    /// The listeners of the process tree of `pid`, without Warden's own
    /// sockets. `young`: the worker started recently.
    #[cfg(test)]
    pub(super) fn of(&self, pid: u32, now: Instant, young: bool, own: &Own) -> Vec<Listener> {
        self.of_each(&[(pid, young)], now, own).remove(0)
    }

    /// [`Self::of`] for each worker (`(pid, young)`), in order: those to read
    /// are read together, the OS's socket tables once for all of them.
    pub(super) fn of_each(&self, workers: &[(u32, bool)], now: Instant, own: &Own) -> Vec<Vec<Listener>> {
        let plans: Vec<Plan> = workers.iter().map(|(pid, young)| self.plan(*pid, now, *young)).collect();
        let (mut unknown, mut stale) = (Vec::new(), Vec::new());
        for ((pid, _), plan) in workers.iter().zip(&plans) {
            match plan {
                // A pid twice (worker mode's threads share one process) is read once.
                Plan::Unknown if !unknown.contains(pid) => unknown.push(*pid),
                Plan::Stale(_) if self.claim(*pid) => stale.push(*pid),
                _ => {}
            }
        }
        if !stale.is_empty() {
            let (cache, own) = (self.clone(), own.clone());
            // Off the supervisor's loop: it reads /proc for every process of the trees.
            tokio::task::spawn_blocking(move || {
                cache.refresh_each(&stale, platform::listeners_each, &own);
            });
        }
        let read: HashMap<u32, Vec<Listener>> = if unknown.is_empty() {
            HashMap::new()
        } else {
            unknown.iter().copied().zip(self.refresh_each(&unknown, platform::listeners_each, own)).collect()
        };
        workers
            .iter()
            .zip(plans)
            .map(|((pid, _), plan)| match plan {
                Plan::Fresh(found) | Plan::Stale(found) => found,
                Plan::Unknown => read.get(pid).cloned().unwrap_or_default(),
            })
            .collect()
    }

    fn plan(&self, pid: u32, now: Instant, young: bool) -> Plan {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match g.by_pid.get(&pid) {
            None => Plan::Unknown,
            Some((at, found)) => {
                let ttl = if found.is_empty() && young { TTL_EMPTY } else { TTL };
                if now.saturating_duration_since(*at) < ttl {
                    Plan::Fresh(found.clone())
                } else {
                    Plan::Stale(found.clone())
                }
            }
        }
    }

    /// Take the right to read `pid`: false when a thread already is.
    fn claim(&self, pid: u32) -> bool {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).refreshing.insert(pid)
    }

    /// Read `pid` now and keep the answer.
    #[cfg(test)]
    fn refresh(
        &self,
        pid: u32,
        lookup: impl FnOnce(u32) -> Option<Vec<platform::Listener>>,
        own: &Own,
    ) -> Vec<Listener> {
        self.refresh_each(&[pid], |pids| vec![lookup(pids[0])], own).remove(0)
    }

    /// Read `pids` now (`lookup` answers for each, in order) and keep the answers.
    fn refresh_each(
        &self,
        pids: &[u32],
        lookup: impl FnOnce(&[u32]) -> Vec<Option<Vec<platform::Listener>>>,
        own: &Own,
    ) -> Vec<Vec<Listener>> {
        let mut answers = lookup(pids).into_iter();
        let found: Vec<Vec<Listener>> =
            pids.iter().map(|_| visible(answers.next().flatten().unwrap_or_default(), own)).collect();
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        for (pid, found) in pids.iter().zip(&found) {
            g.by_pid.insert(*pid, (now, found.clone()));
            g.refreshing.remove(pid);
        }
        found
    }

    /// Forget the processes that are gone.
    pub(super) fn retain(&self, live: &HashSet<u32>) {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).by_pid.retain(|pid, _| live.contains(pid));
    }
}

/// The adapter's listeners as the protocol's, without Warden's own sockets.
fn visible(found: Vec<platform::Listener>, own: &Own) -> Vec<Listener> {
    found
        .into_iter()
        .filter_map(|l| match l {
            platform::Listener::Tcp { port, addr } => Some(Listener::Tcp { addr: addr.to_string(), port }),
            platform::Listener::Unix { path } => (!own.owns(&path)).then_some(Listener::Unix { path }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(port: u16) -> platform::Listener {
        platform::Listener::Tcp { port, addr: "0.0.0.0".parse().unwrap() }
    }

    fn own(dir: &str) -> Own {
        Own { runtime_dir: PathBuf::from(dir), app: "api".into() }
    }

    #[test]
    fn only_wardens_health_sockets_are_not_the_apps() {
        let found = vec![
            tcp(3000),
            platform::Listener::Unix { path: "/run/warden/api.h12-1.sock".into() },
            platform::Listener::Unix { path: "/run/warden/api.sock".into() },
            platform::Listener::Unix { path: "/run/warden/api.h12-1.sock.bak".into() },
            platform::Listener::Unix { path: "/run/warden/other.h12-1.sock".into() },
            platform::Listener::Unix { path: "/run/warden/sub/api.h12-1.sock".into() },
            platform::Listener::Unix { path: "/tmp/app.sock".into() },
        ];
        let got = visible(found, &own("/run/warden"));
        let paths: Vec<&str> = got
            .iter()
            .filter_map(|l| if let Listener::Unix { path } = l { Some(path.as_str()) } else { None })
            .collect();
        assert_eq!(
            paths,
            [
                "/run/warden/api.sock",
                "/run/warden/api.h12-1.sock.bak",
                "/run/warden/other.h12-1.sock",
                "/run/warden/sub/api.h12-1.sock",
                "/tmp/app.sock"
            ],
            "the app's own sockets stay, even in the runtime directory, and in /tmp when [control] puts it there"
        );
        assert_eq!(got.len(), 6);
    }

    #[test]
    fn a_runtime_directory_that_is_everything_hides_only_the_health_sockets() {
        // `[control] socket = "/tmp/api.sock"` makes /tmp the runtime directory, and a bare
        // `api.sock` makes it empty: neither may hide the app's own sockets.
        for dir in ["/tmp", ""] {
            let own = own(dir);
            assert!(!own.owns("/x/y.sock") && !own.owns("/tmp/mine.sock") && !own.owns("mine.sock"), "{dir:?}");
        }
        assert!(own("/tmp").owns("/tmp/api.h1-1.sock"));
        assert!(own("").owns("api.h1-1.sock"));
    }

    #[test]
    fn a_reading_is_kept_two_seconds_and_an_empty_one_of_a_young_worker_a_quarter() {
        let cache = ListenerCache::default();
        let own = own("/run/warden");
        let t0 = Instant::now();
        assert_eq!(cache.plan(7, t0, true), Plan::Unknown);
        assert!(cache.refresh(7, |_| Some(vec![]), &own).is_empty());
        let stored = |cache: &ListenerCache| cache.inner.lock().unwrap().by_pid[&7].0;
        let at = stored(&cache);
        assert_eq!(cache.plan(7, at + Duration::from_millis(100), true), Plan::Fresh(vec![]));
        assert_eq!(cache.plan(7, at + Duration::from_millis(300), true), Plan::Stale(vec![]));
        // An old worker with nothing is not asked every second: the same two seconds.
        assert_eq!(cache.plan(7, at + Duration::from_millis(300), false), Plan::Fresh(vec![]));
        assert_eq!(cache.plan(7, at + Duration::from_millis(2100), false), Plan::Stale(vec![]));
        // Found: kept two seconds either way.
        cache.refresh(7, |_| Some(vec![tcp(3000)]), &own);
        let at = stored(&cache);
        let one = vec![Listener::Tcp { addr: "0.0.0.0".into(), port: 3000 }];
        assert_eq!(cache.plan(7, at + Duration::from_millis(1900), true), Plan::Fresh(one.clone()));
        assert_eq!(cache.plan(7, at + Duration::from_millis(2100), true), Plan::Stale(one));
    }

    #[test]
    fn a_stale_reading_is_served_while_one_thread_reads_the_next() {
        let cache = ListenerCache::default();
        assert!(cache.claim(7), "the first to ask reads");
        assert!(!cache.claim(7), "the others do not");
        cache.refresh(7, |_| None, &own("/x")); // an unreadable process: nothing, and the claim is released
        assert!(cache.claim(7));
    }

    #[test]
    fn processes_that_are_gone_are_forgotten() {
        let cache = ListenerCache::default();
        for pid in [1, 2, 3] {
            cache.refresh(pid, |_| Some(vec![tcp(80)]), &own("/x"));
        }
        cache.retain(&HashSet::from([2]));
        let kept: Vec<u32> = cache.inner.lock().unwrap().by_pid.keys().copied().collect();
        assert_eq!(kept, [2]);
    }

    #[tokio::test]
    async fn a_stale_reading_is_replaced_by_a_blocking_thread() {
        let cache = ListenerCache::default();
        let own = own("/x");
        // This process listens on something, so the real adapter finds it.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let me = std::process::id();
        if !platform::current().capabilities().listening_ports {
            return;
        }
        cache.refresh(me, |_| Some(vec![]), &own);
        let later = Instant::now() + Duration::from_secs(10);
        // The stale (empty) answer is served at once, and a thread reads the real one.
        assert!(cache.of(me, later, false, &own).is_empty());
        let t0 = Instant::now();
        loop {
            let found = cache.of(me, Instant::now(), false, &own);
            if found.iter().any(|f| matches!(f, Listener::Tcp { port: p, .. } if *p == port)) {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "the refresh never arrived: {found:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
