//! Delivering alerts (docs/protocol.md, "Alerts"): a command (the alert as
//! JSON on stdin, and in `WARDEN_ALERT_*` variables) or a webhook, POSTed
//! by running `curl`. Warden carries no TLS stack of its own, on purpose:
//! binary size and attack surface.
//!
//! Bounded and off the main loop: alerts go into a queue of `QUEUE`
//! (more are dropped and counted), at most `CONCURRENT` deliveries run at
//! once, each is killed after `TIMEOUT`, and a failed one is retried once
//! after `RETRY_AFTER`. A slow or hanging command delays only alerts.

use serde::Serialize;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};

/// Alerts waiting for delivery at most.
pub(crate) const QUEUE: usize = 64;
/// Deliveries running at once at most.
pub(crate) const CONCURRENT: usize = 4;
/// A command (or curl) is killed after this long.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);
/// A failed delivery is tried once more after this long.
pub(crate) const RETRY_AFTER: Duration = Duration::from_secs(5);
/// stderr kept from a failing command, for the log.
const STDERR_KEEP: usize = 2048;

/// Where a rule sends its alerts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    /// argv; the alert as JSON on stdin.
    Command(Vec<String>),
    /// An http(s) URL; the alert POSTed as JSON by curl.
    Webhook(String),
}

impl Target {
    /// For logs: the program, or the webhook's host (its path is often the secret).
    pub fn shown(&self) -> String {
        match self {
            Target::Command(argv) => format!("command {}", argv.first().map(String::as_str).unwrap_or("?")),
            Target::Webhook(url) => format!("webhook {}", super::alerts::redact_url(url)),
        }
    }
}

/// The alert, as commands and webhooks get it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Payload {
    /// One line for people (what Slack shows).
    pub text: String,
    pub kind: &'static str,
    pub app: String,
    pub host: String,
    pub detail: String,
    /// Unix ms of the newest event this alert reports.
    pub at_ms: u64,
    /// How many events it reports: more than 1 when `min_interval` held
    /// some back.
    pub count: u32,
    /// Unix ms of the first of them.
    pub first_ms: u64,
    /// The rule that sent it (`name`, else `alert #N`).
    pub rule: String,
}

/// One alert for one rule.
#[derive(Debug, Clone)]
pub(crate) struct Delivery {
    pub rule: Arc<str>,
    pub target: Arc<Target>,
    pub payload: Arc<Payload>,
}

/// Timing, shortened by tests.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub timeout: Duration,
    pub retry_after: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { timeout: TIMEOUT, retry_after: RETRY_AFTER }
    }
}

#[derive(Default)]
struct Shared {
    dropped: Cell<u64>,
    drop_logged: Cell<Option<Instant>>,
    curl_missing_logged: Cell<bool>,
}

/// The queue's sending end. Must be created inside a `LocalSet`.
pub(crate) struct Notifier {
    tx: mpsc::Sender<Delivery>,
    shared: Rc<Shared>,
}

impl Notifier {
    pub fn start(limits: Limits) -> Notifier {
        let (tx, rx) = mpsc::channel(QUEUE);
        let shared = Rc::new(Shared::default());
        crate::guard::spawn_essential("wardend alerts", run(rx, limits, shared.clone()));
        Notifier { tx, shared }
    }

    /// Queue it, or drop it (counted, logged at most once a minute) when
    /// the queue is full: alerts never make wardend wait.
    pub fn send(&self, d: Delivery) {
        let Err(e) = self.tx.try_send(d) else { return };
        let d = match e {
            mpsc::error::TrySendError::Full(d) | mpsc::error::TrySendError::Closed(d) => d,
        };
        let n = self.shared.dropped.get() + 1;
        self.shared.dropped.set(n);
        if self.shared.drop_logged.get().is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
            self.shared.drop_logged.set(Some(Instant::now()));
            crate::error!(
                "alert queue is full; dropping alerts",
                kind = d.payload.kind,
                app = d.payload.app,
                rule = d.rule,
                dropped_so_far = n,
                queue = QUEUE,
                hint = "deliveries are slower than the alerts: check that the alert commands and webhooks answer \
                        quickly (each may take up to 10 s), or raise min_interval in wardend.toml",
            );
        }
    }
}

async fn run(mut rx: mpsc::Receiver<Delivery>, limits: Limits, shared: Rc<Shared>) {
    let slots = Arc::new(Semaphore::new(CONCURRENT));
    while let Some(d) = rx.recv().await {
        let Ok(permit) = slots.clone().acquire_owned().await else { return };
        let shared = shared.clone();
        crate::guard::spawn_request("alert delivery", async move {
            deliver(&d, limits, &shared).await;
            drop(permit);
        });
    }
}

/// Why a delivery failed, and how to fix it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Failure {
    pub why: String,
    pub hint: String,
    /// Trying again cannot help (curl is not installed).
    pub fatal: bool,
}

async fn deliver(d: &Delivery, limits: Limits, shared: &Shared) {
    let first = match attempt(d, limits).await {
        Ok(()) => {
            crate::debug!(
                "alert sent",
                kind = d.payload.kind,
                app = d.payload.app,
                rule = d.rule,
                to = d.target.shown()
            );
            return;
        }
        Err(f) => f,
    };
    // Only a missing curl is fatal: said once, not for every alert.
    if first.fatal {
        if !shared.curl_missing_logged.replace(true) {
            crate::error!(
                "cannot send alerts to webhooks: curl is not installed",
                rule = d.rule,
                to = d.target.shown(),
                error = first.why,
                hint = first.hint,
            );
        }
        return;
    }
    crate::warn!(
        "alert delivery failed; trying once more",
        kind = d.payload.kind,
        app = d.payload.app,
        rule = d.rule,
        to = d.target.shown(),
        retry_in_s = limits.retry_after.as_secs_f32(),
        error = first.why,
        hint = first.hint,
    );
    tokio::time::sleep(limits.retry_after).await;
    if let Err(f) = attempt(d, limits).await {
        crate::error!(
            "alert delivery failed twice; this alert is lost",
            kind = d.payload.kind,
            app = d.payload.app,
            rule = d.rule,
            to = d.target.shown(),
            error = f.why,
            hint = f.hint,
        );
    } else {
        crate::info!("alert sent on the second try", kind = d.payload.kind, app = d.payload.app, rule = d.rule);
    }
}

async fn attempt(d: &Delivery, limits: Limits) -> Result<(), Failure> {
    // Strings and numbers only: encoding cannot fail.
    let json = serde_json::to_string(&*d.payload).unwrap_or_default();
    match &*d.target {
        Target::Command(argv) => {
            let env = alert_env(&d.payload);
            // Its arguments are not shown: they may hold a token.
            run_command(argv, &env, json.as_bytes(), limits.timeout).await.map_err(|why| Failure {
                hint: format!(
                    "run it by hand with an alert on stdin to see what it does: `echo '{{\"kind\":\"died\",\"app\":\
                     \"test\"}}' | {} <its arguments from wardend.toml>`",
                    argv.first().map(String::as_str).unwrap_or("?")
                ),
                why,
                fatal: false,
            })
        }
        Target::Webhook(url) => post_webhook(url, &json, limits.timeout).await,
    }
}

/// `WARDEN_ALERT_*` for commands (NUL bytes would make the spawn fail).
fn alert_env(p: &Payload) -> Vec<(&'static str, String)> {
    let clean = |s: &str| s.replace('\0', " ");
    vec![
        ("WARDEN_ALERT_KIND", p.kind.to_string()),
        ("WARDEN_ALERT_APP", clean(&p.app)),
        ("WARDEN_ALERT_HOST", clean(&p.host)),
        ("WARDEN_ALERT_TEXT", clean(&p.text)),
        ("WARDEN_ALERT_DETAIL", clean(&p.detail)),
        ("WARDEN_ALERT_COUNT", p.count.to_string()),
        ("WARDEN_ALERT_AT_MS", p.at_ms.to_string()),
        ("WARDEN_ALERT_RULE", clean(&p.rule)),
    ]
}

/// How long a failed command's stderr is still read after it exited: a
/// process it left in the background may hold the pipe open for good.
const STDERR_AFTER_EXIT: Duration = Duration::from_millis(200);

/// Run `argv` with `input` on stdin. Its exit status alone says whether it
/// worked: a process it leaves in the background (holding stdout or stderr
/// open) is its own business. Killed, with its process group, if it is still
/// running after `timeout`. Err: what went wrong, in words.
pub(crate) async fn run_command(
    argv: &[String],
    env: &[(&str, String)],
    input: &[u8],
    timeout: Duration,
) -> Result<(), String> {
    let Some(prog) = argv.first() else { return Err("the command is empty".into()) };
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(&argv[1..])
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        // Its own group, so a timeout kills what it started too.
        .process_group(0)
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {prog}: {e}"))?;
    let pid = child.id();
    let stdin = child.stdin.take();
    let stderr = child.stderr.take();
    let mut err = Vec::new();
    let status = {
        let feed = async move {
            if let Some(mut s) = stdin {
                // A command that does not read its stdin is fine.
                let _ = s.write_all(input).await;
            }
        };
        // stderr is read all along, so a chatty command never blocks on a full pipe.
        let reader = read_bounded(stderr, &mut err);
        let wait = child.wait();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(feed, reader, wait, deadline);
        let (mut fed, mut read) = (false, false);
        let status = loop {
            tokio::select! {
                st = &mut wait => break Some(st),
                _ = &mut feed, if !fed => fed = true,
                _ = &mut reader, if !read => read = true,
                _ = &mut deadline => break None,
            }
        };
        // A failure's message: what stderr has within a moment of the exit.
        if matches!(&status, Some(Ok(st)) if !st.success()) && !read {
            let _ = tokio::time::timeout(STDERR_AFTER_EXIT, &mut reader).await;
        }
        status
    };
    match status {
        Some(Ok(st)) if st.success() => Ok(()),
        Some(Ok(st)) => {
            let how = exit_text(st);
            let err = String::from_utf8_lossy(&err);
            let said = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
            Err(if said.is_empty() {
                format!("{prog} failed ({how})")
            } else {
                format!("{prog} failed ({how}): {said}")
            })
        }
        Some(Err(e)) => Err(format!("waiting for {prog} failed: {e}")),
        None => {
            // Not reaped yet (`wait` did not return), so `pid` is still its
            // process group's id, whoever else is in the group.
            if let Some(pid) = pid {
                crate::sys::signal_child(pid, libc::SIGKILL, true);
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            Err(format!("{prog} did not finish within {} s; killed it", timeout.as_secs_f32()))
        }
    }
}

fn exit_text(st: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        (Some(c), _) => format!("exit code {c}"),
        (None, Some(s)) => format!("signal {}", crate::signals::name(s)),
        _ => st.to_string(),
    }
}

/// `r` until its end, keeping the first `STDERR_KEEP` bytes in `kept`
/// (reading on, so the writer never blocks on a full pipe). Stopping it
/// early keeps what was read so far.
async fn read_bounded(r: Option<tokio::process::ChildStderr>, kept: &mut Vec<u8>) {
    let Some(mut r) = r else { return };
    let mut buf = [0u8; 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let room = STDERR_KEEP.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

/// A string for curl's config syntax: in double quotes, `\` and `"` escaped.
fn curl_quote(s: &str) -> String {
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        match c {
            '\\' => q.push_str("\\\\"),
            '"' => q.push_str("\\\""),
            '\n' => q.push_str("\\n"),
            '\r' => q.push_str("\\r"),
            '\t' => q.push_str("\\t"),
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

/// The config curl reads on stdin (`-K -`): the URL and the body never
/// appear in its argv, where any local user could read them (`ps`).
pub(crate) fn curl_config(url: &str, json: &str) -> String {
    format!(
        "url = {}\nheader = \"Content-Type: application/json\"\nheader = \"Expect:\"\ndata-binary = {}\n",
        curl_quote(url),
        curl_quote(json)
    )
}

/// POST `json` to `url` with curl: `curl -fsS --max-time 10 -K -`, the
/// equivalent of `-H 'Content-Type: application/json' --data-binary @- URL`.
pub(crate) async fn post_webhook(url: &str, json: &str, timeout: Duration) -> Result<(), Failure> {
    let shown = super::alerts::redact_url(url);
    let Some(curl) = super::alerts::find_program("curl") else {
        return Err(Failure {
            why: "curl is not on wardend's PATH".into(),
            hint: "install curl (`apt install curl`, `dnf install curl`, `apk add curl`), or send alerts with \
                   `command = [...]` instead of `webhook`"
                .into(),
            fatal: true,
        });
    };
    let max_time = timeout.as_secs().max(1).to_string();
    let argv = vec![
        curl.display().to_string(),
        "-fsS".into(),
        "--max-time".into(),
        max_time,
        "--proto".into(),
        "=http,https".into(),
        "-K".into(),
        "-".into(),
    ];
    let config = curl_config(url, json);
    // curl gives up by itself at --max-time; the extra second is for its exit.
    let r = run_command(&argv, &[], config.as_bytes(), timeout + Duration::from_secs(1)).await;
    r.map_err(|why| {
        // curl names the URL in some errors: never let the secret into a log.
        let why = why.replace(url, &shown);
        let hint = if why.contains("(22)") {
            format!("{shown} answered with an HTTP error: is the webhook URL right, and not revoked?")
        } else if why.contains("(6)") || why.contains("(7)") || why.contains("(28)") {
            format!("this host cannot reach {shown}: check DNS, the firewall and any proxy (https_proxy) for wardend")
        } else {
            format!("try it by hand: curl -fsS -H 'Content-Type: application/json' -d '{{\"text\":\"test\"}}' '<the webhook URL>' (host {shown})")
        };
        Failure { why, hint, fatal: false }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};

    fn local<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        tokio::task::LocalSet::new().block_on(&rt, f)
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wardend-notify-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn payload() -> Payload {
        Payload {
            text: "web-1: api crash loop: 3 worker crashes within 5m".into(),
            kind: "crash_loop",
            app: "api".into(),
            host: "web-1".into(),
            detail: "3 worker crashes within 5m; the last: worker 2 exit code \"1\"\nand \\ more".into(),
            at_ms: 7,
            count: 1,
            first_ms: 7,
            rule: "ops".into(),
        }
    }

    /// A one-request HTTP server; what it was sent comes back on the channel.
    fn http_server(status: &'static str) -> (String, std::sync::mpsc::Receiver<(String, String)>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/services/T0/B0/s3cr3t", l.local_addr().unwrap().port());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                let mut r = std::io::BufReader::new(s.try_clone().unwrap());
                let mut head = String::new();
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    head += &line;
                }
                let mut body = vec![0; len];
                let _ = r.read_exact(&mut body);
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
                if tx.send((head, String::from_utf8_lossy(&body).into_owned())).is_err() {
                    return;
                }
            }
        });
        (url, rx)
    }

    #[test]
    fn a_webhook_gets_the_alert_as_json_through_curl() {
        if find_curl().is_none() {
            return;
        }
        let (url, rx) = http_server("200 OK");
        let json = serde_json::to_string(&payload()).unwrap();
        local(async { post_webhook(&url, &json, Duration::from_secs(5)).await }).expect("delivered");
        let (head, body) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.starts_with("POST /services/T0/B0/s3cr3t HTTP/1.1"), "{head}");
        assert!(head.to_ascii_lowercase().contains("content-type: application/json"), "{head}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["text"], "web-1: api crash loop: 3 worker crashes within 5m");
        assert_eq!(v["kind"], "crash_loop");
        assert_eq!(v["detail"], payload().detail, "quotes, backslashes and newlines survive curl's config syntax");
        assert_eq!((v["count"].as_u64(), v["rule"].as_str()), (Some(1), Some("ops")));
    }

    #[test]
    fn webhook_errors_never_show_the_url() {
        if find_curl().is_none() {
            return;
        }
        let (url, _rx) = http_server("404 Not Found");
        let f = local(async { post_webhook(&url, "{}", Duration::from_secs(5)).await }).unwrap_err();
        assert!(!f.fatal && f.why.contains("(22)") && f.hint.contains("HTTP error"), "{f:?}");
        assert!(!format!("{f:?}").contains("s3cr3t"), "{f:?}");
        // Nothing listens: a connection error, with the host only.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/hook/s3cr3t");
        let f = local(async { post_webhook(&url, "{}", Duration::from_secs(5)).await }).unwrap_err();
        assert!(f.hint.contains("cannot reach http://127.0.0.1"), "{f:?}");
        assert!(!format!("{f:?}").contains("s3cr3t"), "{f:?}");
    }

    fn find_curl() -> Option<std::path::PathBuf> {
        let c = super::super::alerts::find_program("curl");
        if c.is_none() {
            eprintln!("skipping: curl not found on PATH");
        }
        c
    }

    #[test]
    fn a_command_gets_the_alert_on_stdin_and_in_its_environment() {
        let d = dir("cmd");
        let out = d.join("alerts.jsonl");
        let script = format!(
            "cat >> '{0}'; echo \" $WARDEN_ALERT_KIND $WARDEN_ALERT_APP $WARDEN_ALERT_COUNT\" >> '{0}'",
            out.display()
        );
        let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script];
        let p = payload();
        let json = serde_json::to_string(&p).unwrap();
        local(run_command(&argv, &alert_env(&p), json.as_bytes(), Duration::from_secs(5))).expect("ran");
        let text = std::fs::read_to_string(&out).unwrap();
        let (sent, env) = text.split_once('}').unwrap();
        let v: serde_json::Value = serde_json::from_str(&format!("{sent}}}")).unwrap();
        assert_eq!(v["kind"], "crash_loop");
        assert_eq!(env.trim(), "crash_loop api 1");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failing_command_says_how_and_a_hanging_one_is_killed() {
        let argv = |s: &str| vec!["/bin/sh".to_string(), "-c".to_string(), s.to_string()];
        let e = local(run_command(&argv("echo 'no such channel' >&2; exit 3"), &[], b"{}", Duration::from_secs(5)))
            .unwrap_err();
        assert_eq!(e, "/bin/sh failed (exit code 3): no such channel");
        let e =
            local(run_command(&["/nonexistent/notify".to_string()], &[], b"{}", Duration::from_secs(5))).unwrap_err();
        assert!(e.starts_with("cannot run /nonexistent/notify"), "{e}");
        // A command that hangs, with a child of its own: both are killed at the timeout.
        let d = dir("hang");
        let pidfile = d.join("pid");
        let t0 = Instant::now();
        let e = local(run_command(
            &argv(&format!("sleep 30 & echo $! > '{}'; wait", pidfile.display())),
            &[],
            b"",
            Duration::from_millis(500),
        ))
        .unwrap_err();
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        assert!(e.contains("did not finish within 0.5 s; killed it"), "{e}");
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let t1 = Instant::now();
        while running(pid) && t1.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!running(pid), "the command's own child was killed too");
        let _ = std::fs::remove_dir_all(&d);
        // Endless stderr is read and dropped, not kept.
        let e = local(run_command(&argv("yes 'x' | head -c 100000 >&2; exit 1"), &[], b"", Duration::from_secs(5)))
            .unwrap_err();
        assert!(e.len() < 3000, "{}", e.len());
    }

    /// The pid a test's command wrote to `file` (waiting for it a little).
    fn pid_in(file: &std::path::Path) -> i32 {
        let t0 = Instant::now();
        loop {
            if let Some(p) = std::fs::read_to_string(file).ok().and_then(|s| s.trim().parse().ok()) {
                return p;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "no pid in {}", file.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_command_that_leaves_a_background_process_is_done_when_it_exits() {
        let d = dir("fork");
        let (json, times, bg) = (d.join("fork.json"), d.join("fork.times"), d.join("bg.pid"));
        // Its background process keeps stderr open for 30 s.
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "cat > '{}'; date >> '{}'; (sleep 30 & echo $! > '{}')",
                json.display(),
                times.display(),
                bg.display()
            ),
        ];
        let delivery =
            Delivery { rule: "ops".into(), target: Arc::new(Target::Command(argv)), payload: Arc::new(payload()) };
        let limits = Limits { timeout: Duration::from_secs(3), retry_after: Duration::from_millis(50) };
        let t0 = Instant::now();
        local(deliver(&delivery, limits, &Shared::default()));
        let took = t0.elapsed();
        let sleeper = pid_in(&bg);
        assert!(running(sleeper), "a command's background process is left alone");
        crate::sys::signal_child(sleeper as u32, libc::SIGKILL, false);
        assert!(took < Duration::from_secs(2), "done when the command exits, not at the timeout: {took:?}");
        assert_eq!(std::fs::read_to_string(&times).unwrap().lines().count(), 1, "sent once, not retried");
        let sent: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(sent["kind"], "crash_loop");

        // A failure says what it wrote, though stderr stays open.
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo 'no such channel' >&2; (sleep 30 & echo $! > '{}'); exit 2", bg.display()),
        ];
        let _ = std::fs::remove_file(&bg);
        let t0 = Instant::now();
        let e = local(run_command(&argv, &[], b"{}", Duration::from_secs(3))).unwrap_err();
        let took = t0.elapsed();
        crate::sys::signal_child(pid_in(&bg) as u32, libc::SIGKILL, false);
        assert_eq!(e, "/bin/sh failed (exit code 2): no such channel");
        assert!(took < Duration::from_secs(2), "{took:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Alive and not a zombie (an orphan may wait a while for its reaper).
    fn running(pid: i32) -> bool {
        #[cfg(target_os = "linux")]
        {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(s) => !s.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
                Err(_) => false,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            crate::sys::kill(pid, 0).is_ok()
        }
    }

    #[test]
    fn deliveries_are_retried_once_then_dropped() {
        let d = dir("retry");
        let count = d.join("count");
        let argv = vec!["/bin/sh".into(), "-c".into(), format!("echo x >> '{}'; exit 1", count.display())];
        let delivery =
            Delivery { rule: "ops".into(), target: Arc::new(Target::Command(argv)), payload: Arc::new(payload()) };
        let limits = Limits { timeout: Duration::from_secs(5), retry_after: Duration::from_millis(50) };
        local(deliver(&delivery, limits, &Shared::default()));
        assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 2, "tried twice, no more");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_queue_is_bounded_and_never_waits() {
        local(async {
            let d = dir("queue");
            let count = d.join("count");
            let argv = vec!["/bin/sh".into(), "-c".into(), format!("echo x >> '{}'; sleep 0.2", count.display())];
            let n = Notifier::start(Limits { timeout: Duration::from_secs(5), retry_after: Duration::from_millis(10) });
            let t0 = Instant::now();
            for _ in 0..(QUEUE * 3) {
                n.send(Delivery {
                    rule: "ops".into(),
                    target: Arc::new(Target::Command(argv.clone())),
                    payload: Arc::new(payload()),
                });
            }
            assert!(t0.elapsed() < Duration::from_millis(200), "sending never waits: {:?}", t0.elapsed());
            assert!(
                n.shared.dropped.get() >= (QUEUE * 3 - QUEUE - CONCURRENT - 1) as u64,
                "{}",
                n.shared.dropped.get()
            );
            // The queued ones are delivered, CONCURRENT at a time.
            tokio::time::sleep(Duration::from_millis(600)).await;
            let ran = std::fs::read_to_string(&count).unwrap_or_default().lines().count();
            assert!((CONCURRENT..=CONCURRENT * 3 + 1).contains(&ran), "{ran}");
            let _ = std::fs::remove_dir_all(&d);
        });
    }

    #[test]
    fn curl_config_quotes_everything() {
        let c = curl_config("https://h/x\"y", r#"{"a":"b\\c"}"#);
        let lines: Vec<&str> = c.lines().collect();
        assert_eq!(lines[0], r#"url = "https://h/x\"y""#);
        assert_eq!(lines[1], r#"header = "Content-Type: application/json""#);
        assert_eq!(lines[3], r#"data-binary = "{\"a\":\"b\\\\c\"}""#);
        assert_eq!(
            Target::Webhook("https://hooks.slack.com/services/a/b".into()).shown(),
            "webhook https://hooks.slack.com/…"
        );
        assert_eq!(
            Target::Command(vec!["/usr/bin/notify".into(), "--token".into(), "x".into()]).shown(),
            "command /usr/bin/notify"
        );
    }
}
