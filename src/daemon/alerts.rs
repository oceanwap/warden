//! wardend's alerts (docs/protocol.md, "Alerts"): rules read from
//! `<config dir>/wardend.toml`, and the alerts derived from what wardend
//! already sees (worker and rollout events, supervisor deaths, statuses).
//! Delivery (a command, or a webhook through curl) is in `notify.rs`.
//!
//! Everything here is a plain function of events and time: no I/O, so the
//! rules (crash loops, throttling, recovery) are tested without processes.

use super::notify::{Delivery, Payload, Target};
use crate::control::{RolloutOutcome, Status};
use crate::events::{SupervisorEvent, WorkerEvent};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The alert rules' file, in the config directory.
pub(crate) const FILE_NAME: &str = "wardend.toml";
/// A crash loop: this many worker crashes of one app …
pub(crate) const CRASH_LOOP_CRASHES: usize = 3;
/// … within this long (both settable in `[crash_loop]`).
pub(crate) const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(300);
/// `recovered`: all workers ready this long after an alert.
pub(crate) const RECOVERED_AFTER: Duration = Duration::from_secs(60);
/// Default `min_interval`.
pub(crate) const DEFAULT_MIN_INTERVAL: Duration = Duration::from_secs(300);
/// A crash whose detail holds this word was an OOM kill
/// (docs/protocol.md: `oom-killed …` at the start of the exit reason).
pub(crate) const OOM_MARKER: &str = "oom-killed";
/// Bounds on what the file may ask for.
const MAX_RULES: usize = 64;
const MAX_FILE: u64 = 256 * 1024;
const MAX_INTERVAL: Duration = Duration::from_secs(7 * 86_400);
/// Throttle entries kept at most (rules × apps × kinds); idle ones go first.
const MAX_THROTTLES: usize = 4096;
/// Longest `detail` put into an alert.
const MAX_DETAIL: usize = 600;

pub(crate) fn path() -> PathBuf {
    crate::fleet::config_dir().join(FILE_NAME)
}

// ------------------------------------------------------------------- kinds

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum AlertKind {
    CrashLoop,
    GaveUp,
    Died,
    Unresponsive,
    WorkerFailed,
    RolloutFailed,
    Oom,
    Recycled,
    Unhealthy,
    Recovered,
}

impl AlertKind {
    pub const ALL: [AlertKind; 10] = [
        AlertKind::CrashLoop,
        AlertKind::GaveUp,
        AlertKind::Died,
        AlertKind::Unresponsive,
        AlertKind::WorkerFailed,
        AlertKind::RolloutFailed,
        AlertKind::Oom,
        AlertKind::Recycled,
        AlertKind::Unhealthy,
        AlertKind::Recovered,
    ];

    /// The name in `on = [...]` and in the alert's `kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::CrashLoop => "crash_loop",
            AlertKind::GaveUp => "gave_up",
            AlertKind::Died => "died",
            AlertKind::Unresponsive => "unresponsive",
            AlertKind::WorkerFailed => "worker_failed",
            AlertKind::RolloutFailed => "rollout_failed",
            AlertKind::Oom => "oom",
            AlertKind::Recycled => "recycled",
            AlertKind::Unhealthy => "unhealthy",
            AlertKind::Recovered => "recovered",
        }
    }

    /// In the alert's text.
    fn label(self) -> &'static str {
        match self {
            AlertKind::CrashLoop => "crash loop",
            AlertKind::GaveUp => "wardend gave up restarting it",
            AlertKind::Died => "supervisor died",
            AlertKind::Unresponsive => "supervisor unresponsive",
            AlertKind::WorkerFailed => "worker failed",
            AlertKind::RolloutFailed => "rollout failed",
            AlertKind::Oom => "OOM kill",
            AlertKind::Recycled => "worker recycled",
            AlertKind::Unhealthy => "worker unhealthy",
            AlertKind::Recovered => "recovered",
        }
    }

    fn parse(s: &str) -> Option<AlertKind> {
        AlertKind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// The app is in trouble until it is healthy again (`recovered` follows).
    fn is_trouble(self) -> bool {
        !matches!(self, AlertKind::Recovered | AlertKind::Recycled | AlertKind::RolloutFailed)
    }

    fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

// ------------------------------------------------------------------ config

/// One `[[alert]]`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rule {
    /// `name`, else `alert #N`.
    pub name: Arc<str>,
    kinds: u16,
    /// Only these apps; empty: all.
    pub apps: Vec<String>,
    pub target: Arc<Target>,
    pub min_interval: Duration,
}

impl Rule {
    fn wants(&self, kind: AlertKind, app: &str) -> bool {
        self.kinds & kind.bit() != 0 && (self.apps.is_empty() || self.apps.iter().any(|a| a == app))
    }

    pub fn kinds(&self) -> Vec<AlertKind> {
        AlertKind::ALL.into_iter().filter(|k| self.kinds & k.bit() != 0).collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Config {
    pub rules: Vec<Rule>,
    pub crash_loop_crashes: usize,
    pub crash_loop_window: Duration,
    /// Not errors, but worth saying (curl missing, plain http, unknown apps).
    pub notes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            rules: Vec::new(),
            crash_loop_crashes: CRASH_LOOP_CRASHES,
            crash_loop_window: CRASH_LOOP_WINDOW,
            notes: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSpec {
    #[serde(default)]
    alert: Vec<toml::Spanned<RuleSpec>>,
    #[serde(default)]
    crash_loop: Option<CrashLoopSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CrashLoopSpec {
    #[serde(default)]
    crashes: Option<u32>,
    #[serde(default)]
    window: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSpec {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    on: Vec<String>,
    #[serde(default)]
    apps: Vec<String>,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    webhook: Option<String>,
    #[serde(default)]
    min_interval: Option<String>,
}

/// `30s`, `5m`, `1h30m`, `500ms`, `1d`; a bare number is seconds.
pub(crate) fn parse_duration(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(n) = t.parse::<u64>() {
        return Ok(Duration::from_secs(n));
    }
    let mut total = Duration::ZERO;
    let mut rest = t;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if digits == 0 {
            return Err(format!("{s:?} is not a duration (like 30s, 5m, 1h30m)"));
        }
        let n: u64 = rest[..digits].parse().map_err(|_| format!("{s:?}: number too large"))?;
        rest = &rest[digits..];
        let unit = rest.find(|c: char| c.is_ascii_digit()).unwrap_or(rest.len());
        let ms = match &rest[..unit] {
            "ms" => 1,
            "s" => 1000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            u => return Err(format!("{s:?}: unknown unit {u:?} (ms, s, m, h, d)")),
        };
        rest = &rest[unit..];
        total = total.saturating_add(Duration::from_millis(n.saturating_mul(ms)));
    }
    Ok(total)
}

/// `5m`, `1h`, `90s` (for messages).
pub(crate) fn show_duration(d: Duration) -> String {
    let s = d.as_secs();
    if d.subsec_millis() != 0 || s == 0 {
        format!("{}ms", d.as_millis())
    } else if s % 3600 == 0 {
        format!("{}h", s / 3600)
    } else if s % 60 == 0 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// `scheme://host/…`: a webhook URL as it may be logged (its path often
/// is the secret).
pub(crate) fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return "<webhook>".into() };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    format!("{scheme}://{host}/…")
}

/// Where `prog` would be run from: as given when it has a `/`, else the
/// first match on `PATH`.
pub(crate) fn find_program(prog: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let runnable = |p: &Path| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if prog.contains('/') {
        let p = PathBuf::from(prog);
        return runnable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(prog)).find(|p| runnable(p))
}

fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())].iter().filter(|b| **b == b'\n').count() + 1
}

/// `line 3, column 11: invalid type …`, without the excerpt of the file the
/// parser would show (it could be the line with a webhook's secret URL).
fn toml_error(text: &str, e: &toml::de::Error) -> String {
    let msg = e.message().trim().replace('\n', "; ");
    let mut out = match e.span() {
        Some(span) => {
            let line = line_of(text, span.start);
            let line_start = text[..span.start.min(text.len())].rfind('\n').map_or(0, |i| i + 1);
            let column = text.get(line_start..span.start).map_or(1, |s| s.chars().count() + 1);
            format!("line {line}, column {column}: {msg}")
        }
        None => msg,
    };
    if out.contains("expected a sequence") {
        out += " (`on`, `apps` and `command` are arrays: command = [\"/path/to/program\", \"arg\"])";
    }
    out
}

/// Read `path`. `Ok(None)`: there is no such file (no rules).
pub(crate) fn load(path: &Path) -> Result<Option<Config>, Vec<String>> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(vec![format!("cannot read {}: {e}", path.display())]),
    };
    if meta.len() > MAX_FILE {
        return Err(vec![format!(
            "{} is {} KB; an alert file is at most {} KB",
            path.display(),
            meta.len() / 1024,
            MAX_FILE / 1024
        )]);
    }
    let text = std::fs::read_to_string(path).map_err(|e| vec![format!("cannot read {}: {e}", path.display())])?;
    parse(&text).map(Some)
}

/// Parse and check the whole file; every problem is reported, not just the first.
pub(crate) fn parse(text: &str) -> Result<Config, Vec<String>> {
    let spec: FileSpec = toml::from_str(text).map_err(|e| vec![toml_error(text, &e)])?;
    let mut errors = Vec::new();
    let mut cfg = Config::default();
    if let Some(cl) = spec.crash_loop {
        if let Some(n) = cl.crashes {
            if !(2..=1000).contains(&n) {
                errors.push(format!("[crash_loop] crashes = {n}: use 2 to 1000"));
            }
            cfg.crash_loop_crashes = n.clamp(2, 1000) as usize;
        }
        if let Some(w) = cl.window {
            match parse_duration(&w) {
                Ok(d) if d >= Duration::from_secs(10) && d <= Duration::from_secs(86_400) => cfg.crash_loop_window = d,
                Ok(_) => errors.push(format!("[crash_loop] window = {w:?}: use 10s to 24h")),
                Err(e) => errors.push(format!("[crash_loop] window: {e}")),
            }
        }
    }
    if spec.alert.len() > MAX_RULES {
        errors.push(format!("{} [[alert]] rules; at most {MAX_RULES}", spec.alert.len()));
    }
    let mut curl_noted = false;
    for (i, spanned) in spec.alert.into_iter().take(MAX_RULES).enumerate() {
        let line = line_of(text, spanned.span().start);
        let r = spanned.into_inner();
        let name: Arc<str> = match &r.name {
            Some(n) if !n.trim().is_empty() && n.len() <= 64 => n.trim().into(),
            Some(n) if n.len() > 64 => {
                errors.push(format!("alert #{} (line {line}): name is longer than 64 characters", i + 1));
                format!("alert #{}", i + 1).into()
            }
            _ => format!("alert #{}", i + 1).into(),
        };
        let at = if r.name.is_some() {
            format!("alert #{} {name:?} (line {line})", i + 1)
        } else {
            format!("alert #{} (line {line})", i + 1)
        };
        let mut kinds = 0u16;
        if r.on.is_empty() {
            errors
                .push(format!("{at}: `on` is missing or empty; e.g. on = [\"crash_loop\", \"gave_up\"] or [\"all\"]"));
        }
        for k in &r.on {
            match (k.as_str(), AlertKind::parse(k)) {
                ("all", _) => kinds |= AlertKind::ALL.iter().fold(0, |m, k| m | k.bit()),
                (_, Some(kind)) => kinds |= kind.bit(),
                (_, None) => errors.push(format!(
                    "{at}: unknown event {k:?} in `on`; known: all, {}",
                    AlertKind::ALL.map(AlertKind::as_str).join(", ")
                )),
            }
        }
        for a in &r.apps {
            if a.trim().is_empty() {
                errors.push(format!("{at}: an empty app name in `apps`"));
            }
        }
        let min_interval = match r.min_interval.as_deref().map(parse_duration) {
            None => DEFAULT_MIN_INTERVAL,
            Some(Ok(d)) if d <= MAX_INTERVAL => d,
            Some(Ok(_)) => {
                errors.push(format!("{at}: min_interval is longer than 7 days"));
                DEFAULT_MIN_INTERVAL
            }
            Some(Err(e)) => {
                errors.push(format!("{at}: min_interval: {e}"));
                DEFAULT_MIN_INTERVAL
            }
        };
        let target = match (r.command, r.webhook) {
            (Some(_), Some(_)) => {
                errors.push(format!("{at}: has both `command` and `webhook`; use one per rule (two rules for both)"));
                None
            }
            (None, None) => {
                errors.push(format!(
                    "{at}: needs `command = [\"/path/to/program\", \"arg\"]` or `webhook = \"https://…\"`"
                ));
                None
            }
            (Some(argv), None) => match check_command(&argv) {
                Ok(()) => Some(Target::Command(argv)),
                Err(e) => {
                    errors.push(format!("{at}: {e}"));
                    None
                }
            },
            (None, Some(url)) => match check_webhook(&url) {
                Ok(()) => {
                    if url.starts_with("http://") && !is_loopback_url(&url) {
                        cfg.notes.push(format!(
                            "{at}: the webhook is plain http://, so alerts (and the URL's token) cross the network \
                             unencrypted; use https://"
                        ));
                    }
                    if !curl_noted && find_program("curl").is_none() {
                        curl_noted = true;
                        cfg.notes.push(format!(
                            "{at}: webhooks are sent with curl, which is not on PATH here; install it (`apt install \
                             curl`, `dnf install curl`) or alerts to webhooks fail"
                        ));
                    }
                    Some(Target::Webhook(url))
                }
                Err(e) => {
                    errors.push(format!("{at}: {e}"));
                    None
                }
            },
        };
        if let Some(target) = target {
            cfg.rules.push(Rule { name, kinds, apps: r.apps, target: Arc::new(target), min_interval });
        }
    }
    if errors.is_empty() { Ok(cfg) } else { Err(errors) }
}

fn check_command(argv: &[String]) -> Result<(), String> {
    let Some(prog) = argv.first().filter(|p| !p.trim().is_empty()) else {
        return Err("`command` is empty; e.g. command = [\"/usr/local/bin/notify\", \"--channel\", \"ops\"]".into());
    };
    if argv.iter().any(|a| a.contains('\0')) {
        return Err("`command` contains a NUL byte".into());
    }
    if prog.contains('/') && !prog.starts_with('/') {
        return Err(format!("command {prog:?} is a relative path; use an absolute one"));
    }
    if find_program(prog).is_none() {
        return Err(if prog.contains('/') {
            format!("command {prog:?} does not exist or is not executable")
        } else {
            format!("command {prog:?} is not on PATH; use an absolute path (wardend's PATH may differ from yours)")
        });
    }
    Ok(())
}

fn check_webhook(url: &str) -> Result<(), String> {
    let shown = redact_url(url);
    let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else {
        return Err(format!("webhook {shown} must start with https:// (or http://)"));
    };
    if url.len() > 2048 {
        return Err(format!("webhook {shown} is longer than 2048 characters"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("webhook {shown} contains spaces or control characters"));
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").rsplit('@').next().unwrap_or("");
    if host.is_empty() || host.starts_with(':') {
        return Err(format!("webhook {shown} has no host"));
    }
    Ok(())
}

fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or("");
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("");
    host.starts_with("127.") || host.starts_with("localhost") || host.starts_with("[::1]")
}

// ------------------------------------------------------------------ engine

/// What wardend tracks per app to derive alerts.
#[derive(Debug, Default)]
struct AppState {
    /// Worker crashes within the crash-loop window, oldest first (bounded).
    crashes: VecDeque<Instant>,
    /// In a crash loop (alerted); crashes meanwhile are counted.
    looping: Option<u32>,
    /// An alert that is not yet followed by `recovered`.
    trouble: Option<AlertKind>,
    /// All workers ready since then (for `recovered`).
    healthy_since: Option<Instant>,
}

/// Held back by `min_interval`.
#[derive(Debug)]
struct Throttle {
    last_sent: Instant,
    min_interval: Duration,
    /// Events held back since `last_sent`, and the newest of them.
    held: u32,
    first_held_ms: u64,
    newest: Option<Payload>,
}

/// The rules, and the state to apply them. Alerts to send collect in
/// `outbox`; wardend hands them to the notifier.
pub(crate) struct Alerts {
    config: Config,
    host: Arc<str>,
    apps: HashMap<Arc<str>, AppState>,
    throttle: HashMap<(usize, Arc<str>, AlertKind), Throttle>,
    pub outbox: Vec<Delivery>,
}

impl Alerts {
    pub fn new(config: Config) -> Alerts {
        Alerts { config, host: hostname().into(), apps: HashMap::new(), throttle: HashMap::new(), outbox: Vec::new() }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// New rules. Held-back counts belong to the old rules: dropped.
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
        self.throttle.clear();
    }

    /// wardend forgot the app (deleted).
    pub fn forget(&mut self, app: &str) {
        self.apps.remove(app);
    }

    pub fn worker(&mut self, app: &Arc<str>, worker: usize, event: WorkerEvent, detail: Option<&str>, now: Instant) {
        let detail = detail.unwrap_or("");
        match event {
            WorkerEvent::Crashed => {
                if is_oom(detail) {
                    self.raise(
                        app,
                        AlertKind::Oom,
                        format!("worker {worker} was killed for lack of memory: {detail}"),
                        now,
                    );
                }
                let (n, window) = (self.config.crash_loop_crashes, self.config.crash_loop_window);
                let st = self.apps.entry(app.clone()).or_default();
                while st.crashes.front().is_some_and(|t| now.saturating_duration_since(*t) >= window) {
                    st.crashes.pop_front();
                }
                st.crashes.push_back(now);
                while st.crashes.len() > n {
                    st.crashes.pop_front();
                }
                let starts = match st.looping.as_mut() {
                    Some(count) => {
                        *count += 1;
                        false
                    }
                    None => st.crashes.len() >= n,
                };
                if starts {
                    st.looping = Some(0);
                    let msg = format!(
                        "{n} worker crashes within {}; the last: worker {worker} {}",
                        show_duration(window),
                        if detail.is_empty() { "crashed" } else { detail }
                    );
                    self.raise(app, AlertKind::CrashLoop, msg, now);
                }
            }
            WorkerEvent::Failed => {
                let why = if detail.is_empty() { "too many restarts".to_string() } else { detail.to_string() };
                let msg = format!("worker {worker} is left down: {why}; `warden reset {app}` retries it now");
                self.raise(app, AlertKind::WorkerFailed, msg, now);
            }
            WorkerEvent::Unhealthy | WorkerEvent::Hung => {
                let what = if event == WorkerEvent::Hung { "hung" } else { "unhealthy" };
                let msg = format!("worker {worker} {what}: {detail}; Warden replaces it");
                self.raise(app, AlertKind::Unhealthy, msg, now);
            }
            _ => {}
        }
    }

    /// wardend began watching the app (at its start, or a supervisor
    /// started meanwhile): workers already FAILED went by unseen.
    pub fn attached(&mut self, app: &Arc<str>, s: &Status, now: Instant) {
        for w in s.workers.iter().filter(|w| w.state == "FAILED") {
            let last = w.last_exit.as_deref().unwrap_or("unknown");
            let msg = format!(
                "worker {} is left down (it was FAILED when wardend began watching; last exit: {last}); `warden \
                 reset {app}` retries it now",
                w.id
            );
            self.raise(app, AlertKind::WorkerFailed, msg, now);
        }
    }

    pub fn supervisor(&mut self, app: &Arc<str>, event: SupervisorEvent, detail: &str, now: Instant) {
        let kind = match event {
            SupervisorEvent::Died => AlertKind::Died,
            SupervisorEvent::GaveUp => AlertKind::GaveUp,
            SupervisorEvent::Unresponsive => AlertKind::Unresponsive,
            _ => return,
        };
        let msg = match kind {
            AlertKind::GaveUp => format!("{detail}; fix the error in its log, then `warden start {app}`"),
            AlertKind::Unresponsive => format!("{detail}; its workers probably still serve"),
            _ => detail.to_string(),
        };
        self.raise(app, kind, msg, now);
    }

    pub fn rollout_done(&mut self, app: &Arc<str>, o: &RolloutOutcome, now: Instant) {
        if !o.ok {
            self.raise(app, AlertKind::RolloutFailed, format!("{} #{}: {}", o.kind, o.seq, o.message), now);
        } else if o.kind == "replace" && (o.message.contains("max_memory") || o.message.contains("max_lifetime")) {
            self.raise(app, AlertKind::Recycled, o.message.clone(), now);
        }
    }

    /// Each status: an app in trouble that has every worker ready for
    /// `RECOVERED_AFTER` has recovered.
    pub fn status(&mut self, app: &Arc<str>, s: &Status, now: Instant) {
        let Some(st) = self.apps.get_mut(app) else { return };
        if st.trouble.is_none() {
            return;
        }
        let healthy = !s.stopped
            && !s.shutting_down
            && s.rollout.is_none()
            && s.workers_configured > 0
            && s.workers_ready >= s.workers_configured;
        if !healthy {
            st.healthy_since = None;
            return;
        }
        let since = *st.healthy_since.get_or_insert(now);
        if now.saturating_duration_since(since) < RECOVERED_AFTER {
            return;
        }
        let after = st.trouble.take().map(AlertKind::as_str).unwrap_or("an alert");
        let crashes = match st.looping.take() {
            Some(n) if n > 0 => format!(", and {n} more crashes during the loop"),
            _ => String::new(),
        };
        st.crashes.clear();
        st.healthy_since = None;
        let msg = format!(
            "all {} workers ready for {} (after {after}{crashes})",
            s.workers_configured,
            show_duration(RECOVERED_AFTER)
        );
        self.raise(app, AlertKind::Recovered, msg, now);
    }

    /// Every second: send what `min_interval` held back once it has passed,
    /// and end crash loops that went quiet.
    pub fn tick(&mut self, now: Instant) {
        let window = self.config.crash_loop_window;
        for st in self.apps.values_mut() {
            if st.looping.is_some() && st.crashes.back().is_none_or(|t| now.saturating_duration_since(*t) >= window) {
                st.looping = None;
                st.crashes.clear();
            }
        }
        let rules = &self.config.rules;
        let outbox = &mut self.outbox;
        self.throttle.retain(|(rule, _, _), t| {
            if now.saturating_duration_since(t.last_sent) < t.min_interval {
                return true;
            }
            let (Some(mut p), Some(r)) = (t.newest.take(), rules.get(*rule)) else { return false };
            p.count = t.held;
            p.first_ms = t.first_held_ms;
            p.text = summary_text(&p);
            outbox.push(Delivery { rule: r.name.clone(), target: r.target.clone(), payload: Arc::new(p) });
            t.held = 0;
            t.last_sent = now;
            // Kept for one more interval: the next event is held, not sent.
            true
        });
    }

    fn raise(&mut self, app: &Arc<str>, kind: AlertKind, detail: String, now: Instant) {
        let st = self.apps.entry(app.clone()).or_default();
        if kind.is_trouble() {
            st.trouble = Some(kind);
            st.healthy_since = None;
        }
        let detail = truncate(&detail, MAX_DETAIL);
        let at_ms = crate::events::now_ms();
        for (i, rule) in self.config.rules.iter().enumerate() {
            if !rule.wants(kind, app) {
                continue;
            }
            let mut p = Payload {
                text: String::new(),
                kind: kind.as_str(),
                app: app.to_string(),
                host: self.host.to_string(),
                detail: detail.clone(),
                at_ms,
                count: 1,
                first_ms: at_ms,
                rule: rule.name.to_string(),
            };
            let key = (i, app.clone(), kind);
            if let Some(t) = self.throttle.get_mut(&key) {
                if now.saturating_duration_since(t.last_sent) < t.min_interval {
                    if t.held == 0 {
                        t.first_held_ms = at_ms;
                    }
                    t.held = t.held.saturating_add(1);
                    t.newest = Some(p);
                    continue;
                }
                // Due, and not flushed yet (between ticks): fold the held ones in.
                if t.held > 0 {
                    p.count += t.held;
                    p.first_ms = t.first_held_ms;
                }
            }
            p.text = summary_text(&p);
            if !rule.min_interval.is_zero() {
                self.throttle.insert(
                    key,
                    Throttle {
                        last_sent: now,
                        min_interval: rule.min_interval,
                        held: 0,
                        first_held_ms: 0,
                        newest: None,
                    },
                );
            }
            self.outbox.push(Delivery { rule: rule.name.clone(), target: rule.target.clone(), payload: Arc::new(p) });
        }
        if self.throttle.len() > MAX_THROTTLES {
            // Idle entries (nothing held) first; then whatever is oldest.
            self.throttle.retain(|_, t| t.held > 0);
            while self.throttle.len() > MAX_THROTTLES {
                let Some(k) = self.throttle.iter().min_by_key(|(_, t)| t.last_sent).map(|(k, _)| k.clone()) else {
                    break;
                };
                self.throttle.remove(&k);
            }
        }
    }
}

fn is_oom(detail: &str) -> bool {
    detail.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).any(|w| w.eq_ignore_ascii_case(OOM_MARKER))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// `web-1: api crash loop: 3 worker crashes within 5m; …` (and `×4 since 12:00:01`).
fn summary_text(p: &Payload) -> String {
    let label = AlertKind::parse(p.kind).map(AlertKind::label).unwrap_or(p.kind);
    let mut t = format!("{}: {} {label}: {}", p.host, p.app, p.detail);
    if p.count > 1 {
        t += &format!(" (×{} since {})", p.count, clock(p.first_ms));
    }
    t
}

/// `12:00:01`, local time.
fn clock(at_ms: u64) -> String {
    match crate::sys::localtime((at_ms / 1000) as i64) {
        Some(tm) => format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
        None => "--:--:--".into(),
    }
}

/// This host's name, for the alert's text.
pub(crate) fn hostname() -> String {
    #[cfg(target_os = "linux")]
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok();
    #[cfg(not(target_os = "linux"))]
    let name = std::process::Command::new("/bin/hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string());
    name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "this host".into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    pub(crate) fn config(text: &str) -> Config {
        parse(text).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn kinds(a: &Alerts) -> Vec<(&'static str, String, u32)> {
        a.outbox.iter().map(|d| (d.payload.kind, d.payload.app.clone(), d.payload.count)).collect()
    }

    const ALL_SH: &str = "[[alert]]\non = [\"all\"]\ncommand = [\"/bin/sh\", \"-c\", \"cat\"]\nmin_interval = \"0s\"\n";

    #[test]
    fn durations() {
        assert_eq!(parse_duration("5m"), Ok(secs(300)));
        assert_eq!(parse_duration("1h30m"), Ok(secs(5400)));
        assert_eq!(parse_duration("90"), Ok(secs(90)));
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_duration("0s"), Ok(Duration::ZERO));
        assert!(parse_duration("5 minutes").is_err());
        assert!(parse_duration("m5").is_err());
        assert!(parse_duration("").is_err());
        assert_eq!(show_duration(secs(300)), "5m");
        assert_eq!(show_duration(secs(7200)), "2h");
        assert_eq!(show_duration(secs(90)), "90s");
    }

    #[test]
    fn urls_are_redacted_to_the_host() {
        assert_eq!(redact_url("https://hooks.slack.com/services/T0/B0/secret"), "https://hooks.slack.com/…");
        assert_eq!(redact_url("https://user:pw@example.com:8443/x?token=1"), "https://example.com:8443/…");
        assert_eq!(redact_url("garbage"), "<webhook>");
    }

    #[test]
    fn a_full_config_parses() {
        let c = config(
            r#"
[crash_loop]
crashes = 4
window = "2m"

[[alert]]
name = "ops"
on = ["crash_loop", "gave_up", "oom"]
apps = ["api"]
command = ["/bin/sh", "-c", "cat > /dev/null"]
min_interval = "10m"

[[alert]]
on = ["all"]
webhook = "https://hooks.slack.com/services/T0/B0/x"
"#,
        );
        assert_eq!((c.crash_loop_crashes, c.crash_loop_window), (4, secs(120)));
        assert_eq!(c.rules.len(), 2);
        let r = &c.rules[0];
        assert_eq!((&*r.name, r.apps.as_slice(), r.min_interval), ("ops", &["api".to_string()][..], secs(600)));
        assert_eq!(r.kinds(), [AlertKind::CrashLoop, AlertKind::GaveUp, AlertKind::Oom]);
        assert!(r.wants(AlertKind::Oom, "api") && !r.wants(AlertKind::Oom, "web") && !r.wants(AlertKind::Died, "api"));
        assert_eq!(&*c.rules[1].name, "alert #2");
        assert_eq!(c.rules[1].kinds().len(), AlertKind::ALL.len());
        assert_eq!(c.rules[1].min_interval, DEFAULT_MIN_INTERVAL);
        assert!(matches!(&*c.rules[1].target, Target::Webhook(u) if u.ends_with("/x")));
        assert_eq!(parse("").unwrap().rules.len(), 0, "an empty file: no rules");
    }

    #[test]
    fn config_errors_are_all_reported_with_where_and_how() {
        let errs = parse(
            r#"
[[alert]]
on = ["crash_loop", "explode"]
command = ["/bin/sh"]
webhook = "https://example.com/x"

[[alert]]
name = "pager"
on = []
command = ["notify-that-does-not-exist-anywhere"]
min_interval = "5 minutes"

[[alert]]
on = ["all"]
webhook = "ftp://example.com/secret-token"

[[alert]]
on = ["all"]
"#,
        )
        .unwrap_err();
        let all = errs.join("\n");
        for want in [
            "alert #1 (line 2): unknown event \"explode\"",
            "alert #1 (line 2): has both `command` and `webhook`",
            "alert #2 \"pager\" (line 7): `on` is missing or empty",
            "alert #2 \"pager\" (line 7): min_interval: \"5 minutes\"",
            "is not on PATH",
            "alert #3 (line 13): webhook ftp://example.com/… must start with https://",
            "alert #4 (line 17): needs `command",
        ] {
            assert!(all.contains(want), "{want:?} missing from:\n{all}");
        }
        assert!(!all.contains("secret-token"), "URLs are redacted in errors too:\n{all}");
        // Syntax errors and unknown keys come from the TOML parser, with the line.
        let e = parse("[[alert]]\non = [\"all\"\n").unwrap_err().join("\n");
        assert!(e.contains("line 2"), "{e}");
        let e = parse("[[alert]]\non = [\"all\"]\ncomand = [\"/bin/true\"]\n").unwrap_err().join("\n");
        assert!(e.contains("comand"), "{e}");
        let e = parse("[[alert]]\non = [\"all\"]\ncommand = \"/bin/true --x\"\n").unwrap_err().join("\n");
        assert!(e.contains("command"), "{e}");
        assert!(parse("[crash_loop]\ncrashes = 1\n").is_err());
        assert!(parse("[[alert]]\non=[\"all\"]\ncommand=[\"bin/true\"]\n").unwrap_err()[0].contains("relative path"));
        // Plain http to another host is allowed, with a note.
        let c = config("[[alert]]\non = [\"all\"]\nwebhook = \"http://example.com/hook\"\n");
        assert!(c.notes.iter().any(|n| n.contains("unencrypted")), "{:?}", c.notes);
        assert!(
            config("[[alert]]\non = [\"all\"]\nwebhook = \"http://127.0.0.1:9/h\"\n")
                .notes
                .iter()
                .all(|n| !n.contains("unencrypted"))
        );
    }

    #[test]
    fn a_crash_loop_alerts_once_until_it_ends() {
        let mut a = Alerts::new(config(ALL_SH));
        let api: Arc<str> = "api".into();
        let t0 = Instant::now();
        a.worker(&api, 1, WorkerEvent::Crashed, Some("exit code 1"), t0);
        a.worker(&api, 1, WorkerEvent::Crashed, Some("exit code 1"), t0 + secs(1));
        assert!(a.outbox.is_empty(), "two crashes are not a loop");
        a.worker(&api, 2, WorkerEvent::Crashed, Some("exit code 3"), t0 + secs(2));
        assert_eq!(kinds(&a), [("crash_loop", "api".into(), 1)]);
        let p = &a.outbox[0].payload;
        assert!(p.detail.contains("3 worker crashes within 5m") && p.detail.contains("worker 2 exit code 3"), "{p:?}");
        assert!(p.text.ends_with(&format!("api crash loop: {}", p.detail)), "{}", p.text);
        a.outbox.clear();
        for i in 3..20 {
            a.worker(&api, 1, WorkerEvent::Crashed, Some("exit code 1"), t0 + secs(i));
        }
        assert!(a.outbox.is_empty(), "one alert per loop, however long it lasts");
        // Crashes spread out more than the window apart never make a loop.
        let mut b = Alerts::new(config(ALL_SH));
        for i in 0..10 {
            b.worker(&api, 1, WorkerEvent::Crashed, None, t0 + secs(200 * i));
        }
        assert!(b.outbox.is_empty());
        // Quiet for the window: the loop is over, and a new one alerts again.
        a.tick(t0 + secs(20 + 300));
        for i in 0..3 {
            a.worker(&api, 1, WorkerEvent::Crashed, None, t0 + secs(400 + i));
        }
        assert_eq!(kinds(&a), [("crash_loop", "api".into(), 1)]);
    }

    #[test]
    fn duplicates_within_min_interval_are_counted_and_sent_after_it() {
        let text = "[[alert]]\non = [\"worker_failed\", \"died\"]\ncommand = [\"/bin/sh\", \"-c\", \"cat\"]\nmin_interval = \"5m\"\n";
        let mut a = Alerts::new(config(text));
        let (api, web): (Arc<str>, Arc<str>) = ("api".into(), "web".into());
        let t0 = Instant::now();
        a.worker(&api, 1, WorkerEvent::Failed, Some("too many restarts (3 in 60s)"), t0);
        a.worker(&api, 2, WorkerEvent::Failed, None, t0 + secs(10));
        a.worker(&api, 1, WorkerEvent::Failed, None, t0 + secs(20));
        a.worker(&web, 1, WorkerEvent::Failed, None, t0 + secs(30));
        a.supervisor(&api, SupervisorEvent::Died, "without `bye`", t0 + secs(40));
        a.worker(&api, 1, WorkerEvent::Unhealthy, Some("x"), t0 + secs(41));
        // Per (rule, app, kind): api's second and third failure are held back.
        assert_eq!(
            kinds(&a),
            [("worker_failed", "api".into(), 1), ("worker_failed", "web".into(), 1), ("died", "api".into(), 1)]
        );
        a.outbox.clear();
        a.tick(t0 + secs(299));
        assert!(a.outbox.is_empty(), "still within min_interval");
        a.tick(t0 + secs(300));
        assert_eq!(kinds(&a), [("worker_failed", "api".into(), 2)], "the held ones, in one alert");
        assert!(a.outbox[0].payload.text.contains("(×2 since "), "{}", a.outbox[0].payload.text);
        a.outbox.clear();
        // The interval restarted with that alert: the next one is held again …
        a.worker(&api, 1, WorkerEvent::Failed, None, t0 + secs(310));
        assert!(a.outbox.is_empty());
        // … and when it is due before a tick sent it, it goes with the next event.
        a.worker(&api, 1, WorkerEvent::Failed, None, t0 + secs(601));
        assert_eq!(kinds(&a), [("worker_failed", "api".into(), 2)]);
        a.outbox.clear();
        // Nothing held: nothing is sent at the end of the interval, and the entry goes.
        a.tick(t0 + secs(2000));
        a.tick(t0 + secs(2400));
        assert!(a.outbox.is_empty() && a.throttle.is_empty(), "{:?}", a.throttle);
    }

    #[test]
    fn kinds_from_events_and_app_filters() {
        let text = "[[alert]]\non = [\"all\"]\napps = [\"api\"]\ncommand = [\"/bin/sh\"]\nmin_interval = \"0\"\n";
        let mut a = Alerts::new(config(text));
        let (api, web): (Arc<str>, Arc<str>) = ("api".into(), "web".into());
        let t = Instant::now();
        a.worker(&api, 1, WorkerEvent::Crashed, Some("oom-killed (memory.max 512M): signal 9 (SIGKILL)"), t);
        a.worker(&api, 1, WorkerEvent::Crashed, Some("signal 9 (SIGKILL), oom-killed"), t);
        a.worker(&api, 1, WorkerEvent::Crashed, Some("exit code 1 (not oom-killedx)"), t + secs(400));
        a.worker(&api, 1, WorkerEvent::Hung, Some("no heartbeat for 10s"), t);
        a.worker(&api, 1, WorkerEvent::Ready, None, t);
        a.supervisor(&api, SupervisorEvent::GaveUp, "10 deaths in 600 s", t);
        a.supervisor(&api, SupervisorEvent::Unresponsive, "no answer", t);
        a.supervisor(&api, SupervisorEvent::Found, "", t);
        let o = |ok, kind: &str, message: &str| RolloutOutcome {
            seq: 4,
            kind: kind.into(),
            ok,
            message: message.into(),
            duration_secs: 1.0,
        };
        a.rollout_done(&api, &o(false, "reload", "reload failed at worker 1: …. Rolled back"), t);
        a.rollout_done(&api, &o(true, "replace", "worker replaced (memory 600 MB > max_memory 512 MB) in 1.2s"), t);
        a.rollout_done(&api, &o(true, "reload", "4 workers replaced"), t);
        a.worker(&web, 1, WorkerEvent::Failed, None, t);
        let got: Vec<&str> = a.outbox.iter().map(|d| d.payload.kind).collect();
        assert_eq!(got, ["oom", "oom", "unhealthy", "gave_up", "unresponsive", "rollout_failed", "recycled"]);
        assert!(a.outbox[5].payload.detail.starts_with("reload #4: reload failed"), "{:?}", a.outbox[5].payload);
    }

    #[test]
    fn recovered_follows_trouble_once_the_app_is_healthy_for_a_minute() {
        let mut a = Alerts::new(config(ALL_SH));
        let api: Arc<str> = "api".into();
        let t0 = Instant::now();
        let mut s = super::super::policy::tests::status(false, false);
        s.workers_configured = 2;
        s.workers_ready = 2;
        a.status(&api, &s, t0);
        assert!(a.outbox.is_empty(), "nothing happened: nothing recovered");
        a.supervisor(&api, SupervisorEvent::Died, "kill -9", t0);
        a.outbox.clear();
        a.status(&api, &s, t0 + secs(5));
        s.workers_ready = 1;
        a.status(&api, &s, t0 + secs(40));
        s.workers_ready = 2;
        a.status(&api, &s, t0 + secs(41));
        a.status(&api, &s, t0 + secs(100));
        assert!(a.outbox.is_empty(), "a dip restarts the minute");
        a.status(&api, &s, t0 + secs(101));
        assert_eq!(kinds(&a), [("recovered", "api".into(), 1)]);
        assert!(a.outbox[0].payload.detail.contains("all 2 workers ready for 1m (after died)"), "{:?}", a.outbox[0]);
        a.outbox.clear();
        a.status(&api, &s, t0 + secs(500));
        assert!(a.outbox.is_empty(), "once");
        // A recycle is no trouble: nothing to recover from.
        let o = RolloutOutcome {
            seq: 1,
            kind: "replace".into(),
            ok: true,
            message: "worker replaced (max_lifetime reached) in 1s".into(),
            duration_secs: 1.0,
        };
        a.rollout_done(&api, &o, t0 + secs(600));
        a.status(&api, &s, t0 + secs(600));
        a.status(&api, &s, t0 + secs(700));
        assert_eq!(kinds(&a), [("recycled", "api".into(), 1)]);
    }

    #[test]
    fn workers_failed_before_wardend_watched_are_reported() {
        let mut a = Alerts::new(config(ALL_SH));
        let api: Arc<str> = "api".into();
        let mut s = super::super::policy::tests::status(false, false);
        s.workers = ["RUNNING", "FAILED"]
            .iter()
            .enumerate()
            .map(|(i, state)| crate::control::WorkerStatus {
                id: i + 1,
                state: state.to_string(),
                pid: None,
                uptime_secs: None,
                restarts: 3,
                crashes: 4,
                rss_bytes: None,
                cpu_seconds: None,
                cpu_percent: None,
                last_exit: Some("exit code 1".into()),
                healthy: None,
            })
            .collect();
        a.attached(&api, &s, Instant::now());
        assert_eq!(kinds(&a), [("worker_failed", "api".into(), 1)]);
        assert!(a.outbox[0].payload.detail.starts_with("worker 2 is left down (it was FAILED"), "{:?}", a.outbox[0]);
    }

    #[test]
    fn new_rules_replace_old_ones_and_their_throttles() {
        let mut a = Alerts::new(config(ALL_SH));
        let api: Arc<str> = "api".into();
        let t = Instant::now();
        a.supervisor(&api, SupervisorEvent::Died, "x", t);
        assert_eq!(a.outbox.len(), 1);
        a.set_config(Config::default());
        a.supervisor(&api, SupervisorEvent::Died, "x", t);
        assert_eq!(a.outbox.len(), 1, "no rules: nothing");
        a.forget("api");
        assert!(a.apps.is_empty());
    }
}
