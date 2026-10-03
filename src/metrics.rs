//! Process metrics (read through `platform`) and the optional Prometheus endpoint.

use crate::control::Status;
use std::fmt::Write as _;

pub use crate::platform::{ProcStats, proc_stats};

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
    gauge(
        "warden_workers_draining",
        "Old processes a rollout replaced that are still draining (closing connections, finishing requests).",
        "gauge",
        vec![(String::new(), s.draining.len() as f64)],
    );
    gauge(
        "warden_workers_draining_rss_bytes",
        "Resident memory of the old processes still draining, together.",
        "gauge",
        vec![(String::new(), s.draining.iter().filter_map(|w| w.rss_bytes).sum::<u64>() as f64)],
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
    // Hot standbys: gauges of their own (not worker series).
    let standbys = &s.standbys;
    if !standbys.is_empty() {
        let ready = standbys.iter().filter(|w| w.state == crate::control::STANDBY).count();
        gauge(
            "warden_standbys_ready",
            "Hot standbys ready to take over a crashed worker.",
            "gauge",
            vec![(String::new(), ready as f64)],
        );
        gauge(
            "warden_standbys_rss_bytes",
            "Resident memory of the hot standbys, together.",
            "gauge",
            vec![(String::new(), standbys.iter().filter_map(|w| w.rss_bytes).sum::<u64>() as f64)],
        );
        gauge(
            "warden_standby_restarts_total",
            "Standbys started again after one crashed or failed its checks.",
            "counter",
            vec![(String::new(), standbys.first().map(|w| w.restarts).unwrap_or(0) as f64)],
        );
    }
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
    // Like prom-client's nodejs_eventloop_lag_*_seconds, per worker, over the last heartbeat (~1 s).
    let secs = |ms: f64| ms / 1000.0;
    gauge(
        "warden_worker_event_loop_delay_p50_seconds",
        "Median event-loop delay of the worker over the last second (from the shim's heartbeat).",
        "gauge",
        per(&|w| w.loop_delay.map(|d| secs(d.p50_ms))),
    );
    gauge(
        "warden_worker_event_loop_delay_p99_seconds",
        "99th percentile event-loop delay of the worker over the last second.",
        "gauge",
        per(&|w| w.loop_delay.map(|d| secs(d.p99_ms))),
    );
    gauge(
        "warden_worker_event_loop_delay_max_seconds",
        "Largest event-loop delay of the worker over the last second.",
        "gauge",
        per(&|w| w.loop_delay.map(|d| secs(d.max_ms))),
    );
    // Responses by status class, where Warden counts them (its static server,
    // Node through the shim): the app's, since the supervisor started.
    if let Some(r) = &s.requests {
        let t = &r.total;
        gauge(
            "warden_responses_total",
            "Responses the app's workers sent, by status class (404 also counts in 4xx), since the supervisor started.",
            "counter",
            [
                ("2xx", t.ok),
                ("3xx", t.redirect),
                ("4xx", t.client_error),
                ("404", t.not_found),
                ("5xx", t.server_error),
            ]
            .into_iter()
            .map(|(class, n)| (format!(",class=\"{class}\""), n as f64))
            .collect(),
        );
    }
    let port = |f: &dyn Fn(&crate::control::PortStats) -> Option<f64>| -> Vec<(String, f64)> {
        s.ports.iter().filter_map(|p| f(p).map(|v| (format!(",port=\"{}\"", p.port), v))).collect()
    };
    if !s.ports.is_empty() {
        gauge(
            "warden_port_connections",
            "TCP connections established on the app's port.",
            "gauge",
            port(&|p| p.connections.map(f64::from)),
        );
        gauge(
            "warden_port_backlog",
            "Connections waiting for a worker to accept them, on the app's port.",
            "gauge",
            port(&|p| Some(f64::from(p.backlog))),
        );
        gauge(
            "warden_port_backlog_max",
            "How many connections may wait on the app's port (listen backlogs together).",
            "gauge",
            port(&|p| Some(f64::from(p.max_backlog))),
        );
        gauge(
            "warden_port_drops_total",
            "What the kernel discarded at the app's listening sockets, nearly always connections that found the accept queue full, since they were made.",
            "counter",
            port(&|p| p.drops.map(|d| d as f64)),
        );
    }
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
                loop_delay: Some(crate::control::LoopDelay { p50_ms: 0.25, p99_ms: 12.5, max_ms: 40.0 }),
                listening: Vec::new(),
                requests: None,
            }],
            release: None,
            standbys: vec![],
            draining: vec![],
            start_failed: None,
            user: None,
            build: None,
            cwd: None,
            watching: false,
            requests: None,
            ports: vec![],
        };
        let t = render_prometheus(&st);
        assert!(t.contains("warden_workers{app=\"api\"} 2"));
        assert!(t.contains("warden_worker_restarts_total{app=\"api\",worker=\"1\"} 2"));
        assert!(t.contains("warden_worker_up{app=\"api\",worker=\"1\"} 1"));
        assert!(t.contains("# TYPE warden_worker_crashes_total counter"));
        assert!(t.contains("warden_worker_healthy{app=\"api\",worker=\"1\"} 0"));
        assert!(t.contains("warden_worker_event_loop_delay_p50_seconds{app=\"api\",worker=\"1\"} 0.00025"), "{t}");
        assert!(t.contains("warden_worker_event_loop_delay_p99_seconds{app=\"api\",worker=\"1\"} 0.0125"), "{t}");
        assert!(t.contains("warden_worker_event_loop_delay_max_seconds{app=\"api\",worker=\"1\"} 0.04"), "{t}");
        assert!(t.contains("# TYPE warden_worker_event_loop_delay_p99_seconds gauge"));
        assert!(!t.contains("standby"), "no standby series without standbys");
        assert!(!t.contains("warden_responses_total") && !t.contains("warden_port_"), "none where nothing counts");

        // Responses and ports, where Warden has them.
        let mut counted = st.clone();
        let total = crate::control::Responses { ok: 100, redirect: 2, client_error: 7, not_found: 5, server_error: 1 };
        counted.requests = Some(crate::control::RequestStats { rate: 1.0, minute: total, total });
        counted.ports = vec![crate::control::PortStats {
            port: 3000,
            connections: Some(12),
            backlog: 3,
            max_backlog: 1024,
            drops: Some(4),
        }];
        let t = render_prometheus(&counted);
        for line in [
            "warden_responses_total{app=\"api\",class=\"2xx\"} 100",
            "warden_responses_total{app=\"api\",class=\"404\"} 5",
            "warden_responses_total{app=\"api\",class=\"5xx\"} 1",
            "# TYPE warden_responses_total counter",
            "warden_port_connections{app=\"api\",port=\"3000\"} 12",
            "warden_port_backlog{app=\"api\",port=\"3000\"} 3",
            "warden_port_backlog_max{app=\"api\",port=\"3000\"} 1024",
            "warden_port_drops_total{app=\"api\",port=\"3000\"} 4",
        ] {
            assert!(t.contains(line), "{line} in {t}");
        }

        // Hot standbys get gauges of their own, never worker series.
        let mut st = st;
        let standby = |state: &str, rss| WorkerStatus {
            id: 1,
            state: state.into(),
            pid: Some(9),
            uptime_secs: Some(1),
            restarts: 3,
            crashes: 3,
            rss_bytes: Some(rss),
            cpu_seconds: Some(0.1),
            cpu_percent: None,
            last_exit: None,
            healthy: None,
            loop_delay: None,
            listening: Vec::new(),
            requests: None,
        };
        st.standbys.push(standby(crate::control::STANDBY, 1000));
        st.standbys.push(standby(crate::control::WARMING, 500));
        let t = render_prometheus(&st);
        assert_eq!(t.matches("warden_worker_up{").count(), 1, "{t}");
        assert!(t.contains("warden_standbys_ready{app=\"api\"} 1"), "{t}");
        assert!(t.contains("warden_standbys_rss_bytes{app=\"api\"} 1500"), "{t}");
        assert!(t.contains("warden_standby_restarts_total{app=\"api\"} 3"), "{t}");

        // Old processes draining after a rollout: counted, never worker series.
        assert!(t.contains("warden_workers_draining{app=\"api\"} 0"), "always present: {t}");
        let mut old = standby(crate::control::DRAINING, 2048);
        old.id = 1;
        st.draining.push(old);
        let t = render_prometheus(&st);
        assert!(t.contains("warden_workers_draining{app=\"api\"} 1"), "{t}");
        assert!(t.contains("warden_workers_draining_rss_bytes{app=\"api\"} 2048"), "{t}");
        assert_eq!(t.matches("warden_worker_up{").count(), 1, "{t}");
    }
}
