//! Process metrics from /proc and the optional Prometheus endpoint.

use crate::control::Status;
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy)]
pub struct ProcStats {
    pub rss_bytes: u64,
    /// utime + stime in seconds.
    pub cpu_seconds: f64,
}

/// RSS and CPU time of `pid`. Linux only.
pub fn proc_stats(pid: u32) -> Option<ProcStats> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let resident: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ticks = parse_stat_cpu_ticks(&stat)?;
        let (page, hz) = (crate::sys::page_size(), crate::sys::clock_ticks());
        Some(ProcStats { rss_bytes: resident * page, cpu_seconds: ticks as f64 / hz as f64 })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// utime + stime (clock ticks) from the contents of /proc/<pid>/stat.
/// The command name may contain spaces and parens, so split after the last ')'.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by `proc_stats`, Linux only
pub fn parse_stat_cpu_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // f[0] is field 3 (state); utime is field 14, stime field 15.
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

pub fn render_prometheus(s: &Status) -> String {
    let mut o = String::with_capacity(2048);
    let app = escape(&s.app);
    let mut gauge = |name: &str, help: &str, kind: &str, rows: Vec<(String, f64)>| {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}");
        for (labels, v) in rows {
            let _ = writeln!(o, "{name}{{app=\"{app}\"{labels}}} {v}");
        }
    };
    gauge("warden_workers", "Configured workers.", "gauge", vec![(String::new(), s.workers_configured as f64)]);
    gauge(
        "warden_workers_ready",
        "Workers listening and running.",
        "gauge",
        vec![(String::new(), s.workers_ready as f64)],
    );
    gauge("warden_uptime_seconds", "Supervisor uptime.", "gauge", vec![(String::new(), s.uptime_secs as f64)]);
    if let Some(h) = s.healthy {
        gauge(
            "warden_app_healthy",
            "Last health-check verdict (1 healthy, 0 unhealthy).",
            "gauge",
            vec![(String::new(), h as u8 as f64)],
        );
    }
    if let Some(r) = s.supervisor_rss_bytes {
        gauge("warden_supervisor_rss_bytes", "Supervisor resident memory.", "gauge", vec![(String::new(), r as f64)]);
    }
    let per = |f: &dyn Fn(&crate::control::WorkerStatus) -> Option<f64>| -> Vec<(String, f64)> {
        s.workers.iter().filter_map(|w| f(w).map(|v| (format!(",worker=\"{}\"", w.id), v))).collect()
    };
    gauge(
        "warden_worker_up",
        "1 if the worker is running.",
        "gauge",
        per(&|w| Some((w.state == "RUNNING") as u8 as f64)),
    );
    gauge("warden_worker_restarts_total", "Restarts of this worker.", "counter", per(&|w| Some(w.restarts as f64)));
    gauge(
        "warden_worker_crashes_total",
        "Unexpected exits of this worker.",
        "counter",
        per(&|w| Some(w.crashes as f64)),
    );
    gauge(
        "warden_worker_uptime_seconds",
        "Seconds since the worker started.",
        "gauge",
        per(&|w| w.uptime_secs.map(|u| u as f64)),
    );
    gauge(
        "warden_worker_rss_bytes",
        "Resident memory of the worker's process (shared by all Workers in worker mode).",
        "gauge",
        per(&|w| w.rss_bytes.map(|r| r as f64)),
    );
    gauge("warden_worker_cpu_seconds_total", "CPU time of the worker's process.", "counter", per(&|w| w.cpu_seconds));
    gauge(
        "warden_worker_healthy",
        "Per-worker health check verdict (1 healthy, 0 failing).",
        "gauge",
        per(&|w| w.healthy.map(|h| h as u8 as f64)),
    );
    if let Some(r) = &s.last_rollout {
        gauge(
            "warden_last_rollout_success",
            "1 if the last reload/restart/replacement succeeded.",
            "gauge",
            vec![(format!(",kind=\"{}\"", r.kind), r.ok as u8 as f64)],
        );
    }
    gauge(
        "warden_log_lines_dropped_total",
        "Log lines dropped because stdout could not keep up.",
        "counter",
        vec![(String::new(), s.log_lines_dropped as f64)],
    );
    gauge(
        "warden_health_suspended",
        "1 while replacements are held because most workers fail health (dependency outage).",
        "gauge",
        vec![(String::new(), s.health_suspended as u8 as f64)],
    );
    gauge(
        "warden_rollout_in_progress",
        "1 while a rollout is running.",
        "gauge",
        vec![(String::new(), s.rollout.is_some() as u8 as f64)],
    );
    o
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Minimal HTTP server for `GET /metrics`. Gets a fresh snapshot per scrape.
pub async fn serve<F, Fut>(addr: std::net::SocketAddr, snapshot: F) -> std::io::Result<()>
where
    F: Fn() -> Fut + 'static,
    Fut: std::future::Future<Output = Option<Status>>,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(addr).await?;
    crate::info!("metrics endpoint listening", addr = addr);
    let snapshot = std::rc::Rc::new(snapshot);
    loop {
        let Ok((mut sock, _)) = listener.accept().await else { continue };
        let snapshot = snapshot.clone();
        crate::guard::spawn_request("metrics request", async move {
            let mut buf = vec![0u8; 4096];
            let mut n = 0;
            let read = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while n < buf.len() {
                    let r = sock.read(&mut buf[n..]).await?;
                    if r == 0 {
                        break;
                    }
                    n += r;
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Ok::<_, std::io::Error>(())
            })
            .await;
            if !matches!(read, Ok(Ok(()))) {
                return;
            }
            let head = String::from_utf8_lossy(&buf[..n]);
            let path = head.split_whitespace().nth(1).unwrap_or("");
            let (status, body) = if path == "/metrics" || path.starts_with("/metrics?") {
                match snapshot().await {
                    Some(s) => ("200 OK", render_prometheus(&s)),
                    None => ("503 Service Unavailable", "supervisor busy\n".into()),
                }
            } else {
                ("404 Not Found", "not found\n".into())
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/plain; version=0.0.4\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::WorkerStatus;

    #[test]
    fn stat_parsing_handles_spaces_in_comm() {
        let stat = "1234 (my (weird) app) S 1 1234 1234 0 -1 4194304 100 0 0 0 250 50 0 0 20 0 5 0 100 1000 200";
        assert_eq!(parse_stat_cpu_ticks(stat), Some(300));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_self() {
        let s = proc_stats(std::process::id()).unwrap();
        assert!(s.rss_bytes > 0);
    }

    #[test]
    fn prometheus_text() {
        let st = Status {
            app: "api".into(),
            namespace: "default".into(),
            mode: "process".into(),
            config_path: None,
            launched: "terminal".into(),
            unit: None,
            stopped: false,
            log_file: None,
            version: "0".into(),
            pid: 1,
            uptime_secs: 10,
            workers_configured: 2,
            workers_ready: 1,
            healthy: Some(true),
            supervisor_rss_bytes: Some(1024),
            host: None,
            reloading: false,
            shutting_down: false,
            health_suspended: false,
            log_lines_dropped: 0,
            rollout: None,
            last_rollout: None,
            workers: vec![WorkerStatus {
                id: 1,
                state: "RUNNING".into(),
                pid: Some(5),
                uptime_secs: Some(3),
                restarts: 2,
                crashes: 1,
                rss_bytes: Some(4096),
                cpu_seconds: Some(0.5),
                cpu_percent: None,
                last_exit: None,
                healthy: Some(false),
            }],
        };
        let t = render_prometheus(&st);
        assert!(t.contains("warden_workers{app=\"api\"} 2"));
        assert!(t.contains("warden_worker_restarts_total{app=\"api\",worker=\"1\"} 2"));
        assert!(t.contains("warden_worker_up{app=\"api\",worker=\"1\"} 1"));
        assert!(t.contains("# TYPE warden_worker_crashes_total counter"));
        assert!(t.contains("warden_worker_healthy{app=\"api\",worker=\"1\"} 0"));
    }
}
