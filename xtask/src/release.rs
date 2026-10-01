//! `cargo xtask release VERSION` (alias `cargo release`): the checks a release
//! needs, the version bump, the release commit and the `v<VERSION>` tag, the
//! push, then the Release workflow (.github/workflows/release.yml) followed to
//! the published GitHub Release. docs/releasing.md has the whole story.
//!
//! Runs git, cargo and curl (curl talks to the GitHub API: no HTTP crate here).
//! `--dry-run` runs the read-only checks and prints every other step.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub const USAGE: &str = "\
cargo xtask release VERSION [OPTIONS]      (also: cargo release VERSION [OPTIONS])

Releases Warden: checks everything, sets VERSION in Cargo.toml, protocol/ and
gui/ (and Cargo.lock), commits \"Release vVERSION\", tags vVERSION and pushes
both. GitHub Actions (.github/workflows/release.yml) then builds and tests the
Linux archives and publishes the GitHub Release, which this follows to the end.
The macOS archives are built here, on a Mac (scripts/dist-macos.sh, after the
push) and wait in a draft release; the workflow takes them from there. On any
other machine, run `cargo xtask dist-macos` on a Mac while the workflow waits
(it waits an hour).

VERSION:
    X.Y.Z, X.Y.Z-PRE   e.g. 0.2.0 or 0.2.0-rc.1 (a leading v is fine)
    patch|minor|major  the next version after the one in Cargo.toml
    Cargo.toml's own version, if it has no tag yet, is tagged as is (no commit).

OPTIONS:
    --dry-run          Run the read-only checks, print every other step, change nothing
    --branch NAME      Release from branch NAME (default: main)
    --skip-checks      Skip the local cargo fmt / clippy / test
    --full             Also run the integration tests (cargo test --test integration)
    --no-ci-check      Don't require a green CI run for the commit
    --wait-ci          If CI is still running for the commit, wait for it
    --no-wait          Don't follow the Release workflow after pushing
    --no-macos         Don't build the macOS archives here (build them on a Mac:
                       cargo xtask dist-macos)
    --trailer TEXT     Add a trailer (\"Key: value\") to the release commit; repeatable
    -y, --yes          Don't ask for confirmation
    -h, --help         This help

GITHUB_TOKEN or GH_TOKEN, if set, authenticates the GitHub API calls (CI status,
following the workflow): a higher rate limit, and private repositories work.
NEEDS: git; curl (unless --no-ci-check --no-wait); the right to push tags to origin;
on a Mac, for the macOS archives: rustup, cargo-about and the GitHub CLI (gh auth login).
";

const MACOS_SCRIPT: &str = "scripts/dist-macos.sh";

/// Released together, always with the same version: release.yml checks both
/// `warden --version` and `warden-gui --version` against the tag. xtask/ is
/// never released and stays 0.0.0.
const MANIFESTS: &[&str] = &["Cargo.toml", "protocol/Cargo.toml", "gui/Cargo.toml"];
/// The `name:` of .github/workflows/ci.yml and release.yml.
const CI_WORKFLOW: &str = "CI";
const RELEASE_WORKFLOW: &str = "Release";
const STEPS: usize = 9;

#[derive(Debug)]
pub(crate) struct Options {
    version: String,
    branch: String,
    dry_run: bool,
    skip_checks: bool,
    full: bool,
    ci_check: bool,
    wait_ci: bool,
    follow: bool,
    macos: bool,
    yes: bool,
    trailers: Vec<String>,
}

/// `Ok(None)`: the help was printed.
pub(crate) fn parse(args: &[String]) -> Result<Option<Options>, String> {
    let mut o = Options {
        version: String::new(),
        branch: "main".into(),
        dry_run: false,
        skip_checks: false,
        full: false,
        ci_check: true,
        wait_ci: false,
        follow: true,
        macos: true,
        yes: false,
        trailers: Vec::new(),
    };
    let mut version: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "--dry-run" => o.dry_run = true,
            "--skip-checks" => o.skip_checks = true,
            "--full" => o.full = true,
            "--no-ci-check" => o.ci_check = false,
            "--wait-ci" => o.wait_ci = true,
            "--no-wait" => o.follow = false,
            "--no-macos" => o.macos = false,
            "-y" | "--yes" => o.yes = true,
            "--branch" => o.branch = it.next().ok_or("--branch needs a branch name")?.clone(),
            "--trailer" => {
                let t = it.next().ok_or("--trailer needs \"Key: value\"")?;
                let valid = t.split_once(':').is_some_and(|(k, v)| {
                    !k.trim().is_empty() && !k.trim().contains(char::is_whitespace) && !v.trim().is_empty()
                });
                if !valid {
                    return Err(format!(
                        "--trailer {t:?}: write it as \"Key: value\", e.g. \"Signed-off-by: Name <mail>\""
                    ));
                }
                o.trailers.push(t.clone());
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s:?}\n\n{USAGE}")),
            s => {
                if let Some(v) = &version {
                    return Err(format!("one version at a time: {v:?} and {s:?}"));
                }
                version = Some(s.to_string());
            }
        }
    }
    o.version =
        version.ok_or_else(|| format!("which version? e.g. cargo release 0.2.0, or cargo release patch\n\n{USAGE}"))?;
    if o.full && o.skip_checks {
        return Err("--full adds to the local checks that --skip-checks skips: pick one".into());
    }
    if o.wait_ci && !o.ci_check {
        return Err("--wait-ci waits for the CI check that --no-ci-check skips: pick one".into());
    }
    Ok(Some(o))
}

pub(crate) struct Env {
    /// Someone can answer the confirmation (stdin is a terminal).
    interactive: bool,
    token: Option<String>,
    /// The cargo that runs the checks and `cargo update` (`$CARGO`).
    cargo: String,
    /// This is a Mac: it builds the macOS archives itself.
    mac: bool,
}

pub(crate) fn main(args: &[String], root: &Path) -> Result<(), String> {
    let Some(o) = parse(args)? else { return Ok(()) };
    let token = ["GITHUB_TOKEN", "GH_TOKEN"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty());
    release(
        &o,
        root,
        &Env { interactive: std::io::stdin().is_terminal(), token, cargo: cargo_bin(), mac: cfg!(target_os = "macos") },
    )
}

/// `cargo xtask dist-macos`: the macOS archives, built here and uploaded to a
/// draft release (scripts/dist-macos.sh, which has the options).
pub(crate) fn dist_macos(args: &[String], root: &Path) -> Result<(), String> {
    let script = root.join(MACOS_SCRIPT);
    if !script.is_file() {
        return Err(format!("{} not found", script.display()));
    }
    let status = Command::new("bash")
        .arg(&script)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|e| format!("running bash {MACOS_SCRIPT}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{MACOS_SCRIPT} failed ({status})")) }
}

// ------------------------------------------------------------------ semver

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<Pre>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pre {
    Num(u64),
    Alpha(String),
}

#[derive(Debug, Clone, Copy)]
enum Part {
    Major,
    Minor,
    Patch,
}

impl Version {
    /// Semantic Versioning 2.0 without build metadata (`+…`), which has no
    /// precedence and so can't order releases.
    fn parse(s: &str) -> Result<Version, String> {
        if s.contains('+') {
            return Err("has build metadata (+…), which semver ignores when ordering versions: leave it out".into());
        }
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let nums: Vec<&str> = core.split('.').collect();
        if nums.len() != 3 {
            return Err("needs three numbers, MAJOR.MINOR.PATCH".into());
        }
        let num = |p: &str| -> Result<u64, String> {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                Err(format!("{p:?} is not a number"))
            } else if p.len() > 1 && p.starts_with('0') {
                Err(format!("{p:?} has a leading zero"))
            } else {
                p.parse().map_err(|_| format!("{p:?} is too large"))
            }
        };
        let mut v = Version { major: num(nums[0])?, minor: num(nums[1])?, patch: num(nums[2])?, pre: Vec::new() };
        if let Some(pre) = pre {
            for id in pre.split('.') {
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return Err(format!("pre-release {pre:?}: identifiers are [0-9A-Za-z-], separated by dots"));
                }
                if id.bytes().all(|b| b.is_ascii_digit()) {
                    v.pre.push(Pre::Num(num(id).map_err(|e| format!("pre-release {pre:?}: {e}"))?));
                } else {
                    v.pre.push(Pre::Alpha(id.to_string()));
                }
            }
        }
        Ok(v)
    }

    /// The next version, as npm and cargo-release count: a pre-release of
    /// that very version is released as is (1.2.0-rc.1, minor: 1.2.0).
    fn bump(&self, part: Part) -> Version {
        let pre = !self.pre.is_empty();
        let (major, minor, patch) = (self.major, self.minor, self.patch);
        let (major, minor, patch) = match part {
            Part::Major if pre && minor == 0 && patch == 0 => (major, 0, 0),
            Part::Major => (major + 1, 0, 0),
            Part::Minor if pre && patch == 0 => (major, minor, 0),
            Part::Minor => (major, minor + 1, 0),
            Part::Patch if pre => (major, minor, patch),
            Part::Patch => (major, minor, patch + 1),
        };
        Version { major, minor, patch, pre: Vec::new() }
    }
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(o.major, o.minor, o.patch)).then_with(|| {
            match (self.pre.is_empty(), o.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A pre-release comes before its release.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&o.pre) {
                        let c = match (a, b) {
                            (Pre::Num(x), Pre::Num(y)) => x.cmp(y),
                            (Pre::Num(_), Pre::Alpha(_)) => Ordering::Less,
                            (Pre::Alpha(_), Pre::Num(_)) => Ordering::Greater,
                            (Pre::Alpha(x), Pre::Alpha(y)) => x.cmp(y),
                        };
                        if c != Ordering::Equal {
                            return c;
                        }
                    }
                    self.pre.len().cmp(&o.pre.len())
                }
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, p) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match p {
                Pre::Num(n) => write!(f, "{n}")?,
                Pre::Alpha(s) => f.write_str(s)?,
            }
        }
        Ok(())
    }
}

/// The version to release, from the command line and Cargo.toml's version.
fn target_version(arg: &str, current: &Version) -> Result<Version, String> {
    match arg {
        "patch" => Ok(current.bump(Part::Patch)),
        "minor" => Ok(current.bump(Part::Minor)),
        "major" => Ok(current.bump(Part::Major)),
        _ => Version::parse(arg.strip_prefix('v').unwrap_or(arg)).map_err(|e| {
            format!("{arg:?} is not a version ({e}): give X.Y.Z (like 0.2.0), X.Y.Z-PRE (0.2.0-rc.1), or patch, minor, major")
        }),
    }
}

// ------------------------------------------------------------------ Cargo.toml and Cargo.lock

/// The byte range of the string value of `key` in the `[package]` table
/// (between the quotes). A line-based reader, enough for Cargo manifests:
/// it knows table headers and multi-line strings, so a `version` under
/// `[dependencies.x]` or inside a description is never taken.
fn package_value_range(text: &str, key: &str) -> Result<std::ops::Range<usize>, String> {
    let mut offset = 0;
    let mut table = String::new();
    let mut seen_package = false;
    let mut in_multiline: Option<&str> = None;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        if let Some(delim) = in_multiline {
            if line.matches(delim).count() % 2 == 1 {
                in_multiline = None;
            }
            continue;
        }
        if let Some(name) = table_header(line) {
            table = name;
            seen_package |= table == "package";
            continue;
        }
        if table == "package"
            && let Some(r) = string_value_range(line, key)?
        {
            return Ok(start + r.start..start + r.end);
        }
        in_multiline = ["\"\"\"", "'''"].into_iter().find(|d| line.matches(d).count() % 2 == 1);
    }
    if seen_package { Err(format!("[package] has no {key} = \"…\" line")) } else { Err("no [package] table".into()) }
}

/// `[name]` or `[[name]]` (comments and spaces ignored), else None.
fn table_header(line: &str) -> Option<String> {
    let t = line.split('#').next().unwrap_or("").trim();
    let inner =
        t.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")).or_else(|| t.strip_prefix('[')?.strip_suffix(']'))?;
    let ok = !inner.trim().is_empty()
        && inner.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ' | '"' | '\''));
    ok.then(|| inner.chars().filter(|c| *c != ' ').collect())
}

/// On a `key = "value"` line: the range of `value` within the line.
fn string_value_range(line: &str, key: &str) -> Result<Option<std::ops::Range<usize>>, String> {
    let s = line.trim_start();
    let Some(after) = s.strip_prefix(key) else { return Ok(None) };
    if after.starts_with('.') {
        return Err(format!(
            "[package] {} inherits from the workspace: set it in [workspace.package] instead",
            s.split('=').next().unwrap_or(key).trim()
        ));
    }
    let Some(value) = after.trim_start().strip_prefix('=') else { return Ok(None) };
    let value = value.trim_start();
    let quote = match value.chars().next() {
        Some(q @ ('"' | '\'')) => q,
        _ => return Err(format!("[package] {key} is not a plain string: {}", s.trim_end())),
    };
    // `value` is a suffix of `line`, starting at the opening quote.
    let open = line.len() - value.len() + 1;
    let close = line[open..].find(quote).ok_or_else(|| format!("[package] {key}: unterminated string"))?;
    Ok(Some(open..open + close))
}

/// The `[package]` string value of `key` (`name`, `version`).
fn package_value(text: &str, key: &str) -> Result<String, String> {
    package_value_range(text, key).map(|r| text[r].to_string())
}

/// `text` with `[package] key = "…"` set to `value`; nothing else changes.
fn set_package_value(text: &str, key: &str, value: &str) -> Result<String, String> {
    let r = package_value_range(text, key)?;
    Ok(format!("{}{value}{}", &text[..r.start], &text[r.end..]))
}

/// The version Cargo.lock records for the workspace package `name` (the
/// `[[package]]` entry without a `source`).
fn lock_version(lock: &str, name: &str) -> Option<String> {
    for block in lock.split("[[package]]").skip(1) {
        let (mut n, mut v, mut source) = (None, None, false);
        for line in block.lines().map(str::trim) {
            if line.starts_with('[') {
                break;
            }
            if let Some(x) = line.strip_prefix("name = ") {
                n = Some(x.trim_matches('"'));
            } else if let Some(x) = line.strip_prefix("version = ") {
                v = Some(x.trim_matches('"'));
            } else if line.starts_with("source = ") {
                source = true;
            }
        }
        if n == Some(name) && !source {
            return v.map(String::from);
        }
    }
    None
}

// ------------------------------------------------------------------ git

struct Git<'a>(&'a Path);

impl Git<'_> {
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new("git");
        c.args(args).current_dir(self.0);
        // Run from a git hook, these would point at another repository.
        for v in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR"] {
            c.env_remove(v);
        }
        c
    }

    /// stdout (end trimmed), or an error with git's stderr.
    fn out(&self, args: &[&str]) -> Result<String, String> {
        self.output(args, None)
    }

    fn output(&self, args: &[&str], input: Option<&str>) -> Result<String, String> {
        self.output_with_index(args, input, None)
    }

    /// `output`, with `GIT_INDEX_FILE` set to `index` if given.
    fn output_with_index(&self, args: &[&str], input: Option<&str>, index: Option<&Path>) -> Result<String, String> {
        let what = format!("git {}", args.join(" "));
        let mut cmd = self.command(args);
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        let mut child = cmd
            // Never wait on a password prompt: say what failed instead.
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{what}: {e}"))?;
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(text.as_bytes()).map_err(|e| format!("{what}: {e}"))?;
        }
        let o = child.wait_with_output().map_err(|e| format!("{what}: {e}"))?;
        if o.status.success() {
            Ok(String::from_utf8_lossy(&o.stdout).trim_end().to_string())
        } else {
            Err(format!("{what} failed: {}", String::from_utf8_lossy(&o.stderr).trim()))
        }
    }

    fn succeeds(&self, args: &[&str]) -> bool {
        self.command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn count(&self, range: &str) -> Result<u64, String> {
        let n = self.out(&["rev-list", "--count", range])?;
        n.parse().map_err(|_| format!("git rev-list --count {range}: {n:?}"))
    }
}

/// `owner/repo` of a GitHub remote URL (https, ssh or scp-like), else None.
fn github_repo(url: &str) -> Option<String> {
    let is_github =
        |h: &str| ["github.com", "www.github.com", "ssh.github.com"].iter().any(|g| h.eq_ignore_ascii_case(g));
    let u = url.trim();
    let path = if let Some((scheme, rest)) = u.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git" | "git+ssh") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = host.split_once(':').map_or(host, |(h, _)| h);
        if !is_github(host) {
            return None;
        }
        path
    } else {
        // user@host:owner/repo
        let (userhost, path) = u.split_once(':')?;
        let host = userhost.rsplit_once('@').map_or(userhost, |(_, h)| h);
        if !is_github(host) {
            return None;
        }
        path
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    let fine = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (fine(owner) && fine(repo)).then(|| format!("{owner}/{repo}"))
}

/// A remote URL without credentials, for printing.
fn redact(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            match authority.rsplit_once('@') {
                Some((_, host)) => format!("{scheme}://{host}/{path}"),
                None => url.to_string(),
            }
        }
        None => url.to_string(),
    }
}

#[derive(Debug, PartialEq)]
enum PushError {
    /// No right to push (the tag, the branch, or at all) from here.
    Denied,
    /// origin's branch moved since the check.
    Moved,
    Other,
}

fn classify_push(stderr: &str) -> PushError {
    let s = stderr.to_ascii_lowercase();
    const DENIED: &[&str] = &[
        "403",
        "permission",
        "denied",
        "not allowed",
        "unauthorized",
        "authentication failed",
        "could not read username",
        "refusing to allow",
        "protected branch",
        "gh006",
        "gh013",
        "hook declined",
    ];
    if DENIED.iter().any(|p| s.contains(p)) {
        PushError::Denied
    } else if ["non-fast-forward", "fetch first", "stale info"].iter().any(|p| s.contains(p)) {
        PushError::Moved
    } else {
        PushError::Other
    }
}

// ------------------------------------------------------------------ GitHub API (through curl)

#[derive(Debug)]
struct Response {
    status: u16,
    body: String,
    remaining: Option<u64>,
    reset: Option<u64>,
    retry_after: Option<u64>,
}

/// `curl --include` output: one or more header blocks (a proxy's CONNECT
/// answer, redirects), then the body. The last block is the answer.
fn parse_http(raw: &str) -> Result<Response, String> {
    let mut rest = raw;
    let mut status = None;
    let mut head = "";
    while rest.starts_with("HTTP/") {
        let end =
            [rest.find("\r\n\r\n").map(|i| (i, 4)), rest.find("\n\n").map(|i| (i, 2))].into_iter().flatten().min();
        let (h, body) = match end {
            Some((i, n)) => (&rest[..i], &rest[i + n..]),
            None => (rest, ""),
        };
        status = h.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse::<u16>().ok());
        head = h;
        rest = body;
    }
    let status = status.ok_or("no HTTP response")?;
    let header = |name: &str| {
        head.lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.trim().parse::<u64>().ok())
    };
    Ok(Response {
        status,
        body: rest.to_string(),
        remaining: header("x-ratelimit-remaining"),
        reset: header("x-ratelimit-reset"),
        retry_after: header("retry-after"),
    })
}

const CURL_MISSING: &str = "curl is not installed";

fn curl_get(url: &str, token: Option<&str>) -> Result<Response, String> {
    // Headers come in through --config on stdin, so the token never shows
    // in the process list.
    let mut child = Command::new("curl")
        .args(["--silent", "--show-error", "--include", "--location", "--max-time", "30", "--config", "-", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "{CURL_MISSING}: it reads the CI status and follows the release on the GitHub API; \
                     install it, or pass --no-ci-check --no-wait"
                )
            } else {
                format!("running curl: {e}")
            }
        })?;
    let mut config = String::from(
        "header = \"Accept: application/vnd.github+json\"\n\
         header = \"X-GitHub-Api-Version: 2022-11-28\"\n\
         header = \"User-Agent: warden-xtask-release\"\n",
    );
    if let Some(t) = token {
        config += &format!("header = \"Authorization: Bearer {}\"\n", t.replace('\\', "\\\\").replace('"', "\\\""));
    }
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(config.as_bytes()).map_err(|e| format!("curl: {e}"))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("curl {url}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    parse_http(&String::from_utf8_lossy(&out.stdout))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Seconds between polls that make `per_poll` requests each: `base`, or
/// longer if the rate limit left (`remaining` until `reset_in` seconds from
/// now) would run out first. A few requests stay in reserve for the end.
fn poll_secs(base: u64, remaining: Option<u64>, reset_in: Option<u64>, per_poll: u64) -> u64 {
    let (Some(remaining), Some(reset_in)) = (remaining, reset_in) else { return base };
    let polls = remaining.saturating_sub(5) / per_poll.max(1);
    if polls == 0 { reset_in + 1 } else { base.max(reset_in.div_ceil(polls)) }
}

struct GitHub {
    repo: String,
    token: Option<String>,
    remaining: Option<u64>,
    reset: Option<u64>,
    /// Wait for the rate limit to reset instead of failing.
    patient: bool,
}

impl GitHub {
    fn new(repo: &str, token: Option<String>) -> GitHub {
        GitHub { repo: repo.to_string(), token, remaining: None, reset: None, patient: false }
    }

    fn request(&mut self, path: &str) -> Result<Response, String> {
        let url = format!("https://api.github.com/repos/{}/{path}", self.repo);
        let (mut network_errors, mut waited) = (0, false);
        loop {
            let resp = match curl_get(&url, self.token.as_deref()) {
                Ok(r) => r,
                Err(e) if e.starts_with(CURL_MISSING) => return Err(e),
                Err(e) => {
                    network_errors += 1;
                    if network_errors == 3 {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            self.remaining = resp.remaining.or(self.remaining);
            self.reset = resp.reset.or(self.reset);
            let limited = matches!(resp.status, 403 | 429) && (resp.remaining == Some(0) || resp.retry_after.is_some());
            if !limited {
                return Ok(resp);
            }
            let wait = resp.retry_after.unwrap_or_else(|| resp.reset.map_or(60, |t| t.saturating_sub(now_secs()) + 1));
            let hint = if self.token.is_none() { " (GITHUB_TOKEN or GH_TOKEN raises the limit)" } else { "" };
            if !self.patient || waited || wait > 3600 {
                return Err(format!("GitHub API rate limit reached, for {} more min{hint}", wait.div_ceil(60)));
            }
            eprintln!("   GitHub API rate limit reached: waiting {wait} s{hint}");
            std::thread::sleep(Duration::from_secs(wait));
            waited = true;
        }
    }

    fn json(&mut self, path: &str) -> Result<Value, String> {
        let resp = self.request(path)?;
        if !(200..300).contains(&resp.status) {
            return Err(self.error(&resp, path));
        }
        serde_json::from_str(&resp.body).map_err(|e| format!("GitHub API {path}: the answer is not JSON ({e})"))
    }

    fn error(&self, resp: &Response, path: &str) -> String {
        let msg = serde_json::from_str::<Value>(&resp.body)
            .ok()
            .and_then(|v| v["message"].as_str().map(String::from))
            .unwrap_or_default();
        match resp.status {
            401 => format!("GitHub refused the token in GITHUB_TOKEN/GH_TOKEN (401 {msg}): renew it, or unset it"),
            404 if self.token.is_none() => format!(
                "GitHub answered 404 for {}: the repository is private (set GITHUB_TOKEN or GH_TOKEN) or doesn't exist",
                self.repo
            ),
            404 => {
                format!("GitHub answered 404 for {}/{path}: can the token read the repository's Actions?", self.repo)
            }
            s => format!("GitHub API {s} for {path}: {msg}"),
        }
    }

    fn poll_interval(&self, base: u64, per_poll: u64) -> Duration {
        Duration::from_secs(poll_secs(base, self.remaining, self.reset.map(|t| t.saturating_sub(now_secs())), per_poll))
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RunInfo {
    id: u64,
    status: String,
    conclusion: Option<String>,
    url: String,
}

fn run_info(v: &Value) -> Option<RunInfo> {
    Some(RunInfo {
        id: v["id"].as_u64()?,
        status: v["status"].as_str()?.to_string(),
        conclusion: v["conclusion"].as_str().map(String::from),
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
    })
}

/// The runs of `workflow` in a `GET …/actions/runs` answer (for the branch
/// or tag `head_branch`, if given), newest first.
fn runs_named(v: &Value, workflow: &str, head_branch: Option<&str>) -> Vec<RunInfo> {
    let mut runs: Vec<RunInfo> = v["workflow_runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["name"].as_str() == Some(workflow))
        .filter(|r| head_branch.is_none_or(|b| r["head_branch"].as_str() == Some(b)))
        .filter_map(run_info)
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.id));
    runs
}

#[derive(Debug, PartialEq)]
enum Ci {
    Passed(RunInfo),
    Running(RunInfo),
    Failed(RunInfo),
    Missing,
}

/// The CI verdict for a commit: passed if any CI run for it passed (a
/// re-run of a flaky job counts), else running, else the newest failure.
fn ci_state(runs: &Value) -> Ci {
    let runs = runs_named(runs, CI_WORKFLOW, None);
    if let Some(r) = runs.iter().find(|r| r.conclusion.as_deref() == Some("success")) {
        return Ci::Passed(r.clone());
    }
    if let Some(r) = runs.iter().find(|r| r.status != "completed") {
        return Ci::Running(r.clone());
    }
    runs.into_iter().next().map_or(Ci::Missing, Ci::Failed)
}

#[derive(Debug, Clone, PartialEq)]
struct Job {
    id: u64,
    name: String,
    /// queued, in_progress, … or, once completed, the conclusion.
    state: String,
    url: String,
}

fn jobs(v: &Value) -> Vec<Job> {
    v["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|j| {
            let status = j["status"].as_str()?;
            let state = match (status, j["conclusion"].as_str()) {
                ("completed", Some(c)) => c,
                _ => status,
            };
            Some(Job {
                id: j["id"].as_u64()?,
                name: j["name"].as_str()?.to_string(),
                state: state.to_string(),
                url: j["html_url"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// A release's page and its assets (name, size in bytes).
fn release_assets(v: &Value) -> (String, Vec<(String, u64)>) {
    let assets = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| Some((a["name"].as_str()?.to_string(), a["size"].as_u64().unwrap_or(0))))
        .collect();
    (v["html_url"].as_str().unwrap_or_default().to_string(), assets)
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KiB", b as f64 / 1024.0),
        b => format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0)),
    }
}

fn elapsed(t0: Instant) -> String {
    let s = t0.elapsed().as_secs();
    format!("{:>2}m{:02}s", s / 60, s % 60)
}

fn commits(n: u64) -> String {
    if n == 1 { "1 commit".into() } else { format!("{n} commits") }
}

// ------------------------------------------------------------------ the release

/// Prints the steps; collects the problems of a dry run, which goes on to
/// show every step, where a real run stops at the first.
struct Report {
    dry: bool,
    step: usize,
    problems: Vec<String>,
}

impl Report {
    fn step(&mut self, title: &str) {
        self.step += 1;
        eprintln!("\n== {}/{STEPS} {title}", self.step);
    }

    fn ok(&self, msg: &str) {
        eprintln!("   ok    {msg}");
    }

    fn note(&self, msg: &str) {
        for l in msg.lines() {
            eprintln!("   {l}");
        }
    }

    /// A command about to run, or that a dry run would run.
    fn cmd(&self, cmd: &str) {
        if self.dry {
            eprintln!("   would run: {cmd}");
        } else {
            eprintln!("   $ {cmd}");
        }
    }

    fn fail(&mut self, what: &str, fix: &str) -> Result<(), String> {
        eprintln!("   FAIL  {what}");
        for (i, l) in fix.lines().enumerate() {
            eprintln!("   {}{l}", if i == 0 { "fix:  " } else { "      " });
        }
        if self.dry {
            self.problems.push(what.to_string());
            Ok(())
        } else {
            Err(format!("release stopped at step {}/{STEPS}: {what}", self.step))
        }
    }
}

fn cargo_bin() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

fn release(o: &Options, root: &Path, env: &Env) -> Result<(), String> {
    let git = Git(root);
    let manifest = read(&root.join("Cargo.toml"))?;
    let current_str = package_value(&manifest, "version").map_err(|e| format!("Cargo.toml: {e}"))?;
    let current = Version::parse(&current_str).map_err(|e| format!("Cargo.toml: version {current_str:?} {e}"))?;
    let target = target_version(&o.version, &current)?;
    let tag = format!("v{target}");
    let push_cmd = format!("git push --atomic origin {} {tag}", o.branch);
    let mut r = Report { dry: o.dry_run, step: 0, problems: Vec::new() };
    eprintln!(
        "xtask: release {target} (Cargo.toml has {current}){}",
        if o.dry_run { ". Dry run: nothing is changed" } else { "" }
    );

    // ---------------------------------------------------------------- 1
    r.step("Preconditions");
    let git_version = git.out(&["--version"]).map_err(|_| "git is not installed (or not on PATH)".to_string())?;
    r.ok(&git_version);
    if !o.dry_run && !o.yes && !env.interactive {
        r.fail(
            "stdin is not a terminal, so nobody can answer the confirmation",
            "pass --yes (after a --dry-run, to see what it does)",
        )?;
    }
    let head = git
        .out(&["rev-parse", "--verify", "HEAD"])
        .map_err(|e| format!("{} is not a git checkout with commits: {e}", root.display()))?;
    let short = head.get(..7).unwrap_or(&head).to_string();
    let current_branch = git.out(&["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    match &current_branch {
        Some(b) if *b == o.branch => r.ok(&format!("on branch {b}")),
        Some(b) => r.fail(
            &format!("on branch {b}, not {}", o.branch),
            &format!("git switch {}; or, to release from {b}, --branch {b}", o.branch),
        )?,
        None => {
            r.fail(&format!("HEAD is detached (at {short}), not on {}", o.branch), &format!("git switch {}", o.branch))?
        }
    }
    let status = git.out(&["status", "--porcelain"])?;
    if status.is_empty() {
        r.ok("working tree clean");
    } else {
        let files: Vec<&str> = status.lines().map(|l| l.get(3..).unwrap_or(l)).collect();
        let more = if files.len() > 5 { format!(" and {} more", files.len() - 5) } else { String::new() };
        r.fail(
            &format!("uncommitted changes: {}{more}", files[..files.len().min(5)].join(", ")),
            "commit them (and push, and let CI pass), or set them aside (git stash push -u); then run this again",
        )?;
    }

    let origin = git
        .out(&["remote", "get-url", "origin"])
        .map_err(|_| "this checkout has no remote named origin: git remote add origin https://github.com/OWNER/REPO")?;
    let github = github_repo(&origin);
    match &github {
        Some(g) => r.ok(&format!("origin is github.com/{g}")),
        None if o.ci_check || o.follow => r.fail(
            &format!(
                "origin ({}) is not on github.com: the CI check and following the release use the GitHub API",
                redact(&origin)
            ),
            "pass --no-ci-check --no-wait",
        )?,
        None => r.ok(&format!("origin is {}", redact(&origin))),
    }

    // Tags here and on origin.
    let remote_tags: Option<Vec<String>> = match git.out(&["ls-remote", "--tags", "--refs", "origin"]) {
        Ok(out) => Some(out.lines().filter_map(|l| l.split_once("\trefs/tags/")).map(|(_, t)| t.to_string()).collect()),
        Err(e) => {
            r.fail(
                &format!("can't reach origin: {e}"),
                "check the network and your credentials: git ls-remote origin",
            )?;
            None
        }
    };
    let local_tags: Vec<String> = git.out(&["tag", "--list", "v*"])?.lines().map(String::from).collect();
    let here = local_tags.contains(&tag);
    let there = remote_tags.as_ref().is_some_and(|t| t.contains(&tag));
    match (here, there) {
        (true, true) => r.fail(
            &format!("tag {tag} already exists, here and on origin: {target} is released"),
            &format!("release a newer version: cargo release {}", target.bump(Part::Patch)),
        )?,
        (true, false) => {
            // An earlier attempt's release commit, if the tag is on it.
            let on_release_commit = git.out(&["rev-parse", &format!("{tag}^{{commit}}")]).is_ok_and(|c| c == head)
                && git.out(&["log", "-1", "--format=%s"]).is_ok_and(|s| s == format!("Release {tag}"));
            let reset = if on_release_commit { " && git reset --keep HEAD~1" } else { "" };
            r.fail(
                &format!("tag {tag} exists here but not on origin: an attempt whose push failed?"),
                &format!("push it: {push_cmd}\nor drop it and start over: git tag -d {tag}{reset}"),
            )?
        }
        (false, true) => r.fail(
            &format!("tag {tag} already exists on origin"),
            &format!("release a newer version (git fetch --tags shows it); cargo release {}", target.bump(Part::Patch)),
        )?,
        (false, false) if remote_tags.is_some() => r.ok(&format!("tag {tag} is free, here and on origin")),
        (false, false) => {}
    }

    // In sync with origin. A dry run compares with ls-remote; a real run
    // fetches, so it can count commits ahead and behind.
    // origin's branch as seen here: the push (step 7) is refused unless
    // origin still has exactly this (--force-with-lease).
    let mut origin_head: Option<String> = None;
    if remote_tags.is_some() {
        let b = &o.branch;
        match git.out(&["ls-remote", "--heads", "origin", &format!("refs/heads/{b}")]) {
            Err(e) => r.fail(&format!("can't reach origin: {e}"), "check the network and your credentials")?,
            Ok(out) => match out.split_whitespace().next() {
                None => r.fail(
                    &format!("origin has no branch {b}"),
                    &format!("push it (git push -u origin {b}) and let CI pass, then release"),
                )?,
                Some(remote) => {
                    origin_head = Some(remote.to_string());
                    if o.dry_run {
                        r.note(&format!("(not run in a dry run: git fetch origin {b}; compared with git ls-remote)"));
                    } else {
                        r.cmd(&format!("git fetch origin {b}"));
                        if let Err(e) = git.out(&["fetch", "--quiet", "origin", &format!("refs/heads/{b}")]) {
                            r.fail(&format!("git fetch origin failed: {e}"), "check the network and your credentials")?;
                        }
                    }
                    // What is compared: HEAD (named after its branch) with origin's `b`.
                    let here = current_branch.as_deref().unwrap_or("HEAD");
                    let vs = if here == b { "origin".to_string() } else { format!("origin's {b}") };
                    if remote == head {
                        r.ok(&format!("{here} is in sync with {vs} ({short})"));
                    } else if !git.succeeds(&["cat-file", "-e", &format!("{remote}^{{commit}}")]) {
                        r.fail(
                            &format!(
                                "origin's {b} is at {}, a commit this checkout doesn't have",
                                remote.get(..7).unwrap_or(remote)
                            ),
                            "git pull --ff-only",
                        )?;
                    } else {
                        let ahead = git.count(&format!("{remote}..HEAD"))?;
                        let behind = git.count(&format!("HEAD..{remote}"))?;
                        match (ahead, behind) {
                            (0, n) => r.fail(&format!("{here} is {} behind {vs}", commits(n)), "git pull --ff-only")?,
                            (n, 0) => r.fail(
                                &format!("{here} has {} that {vs} doesn't", commits(n)),
                                &format!(
                                    "push first (git push origin {b}) and let CI pass: the release goes on top of what origin has"
                                ),
                            )?,
                            (a, n) => r.fail(
                                &format!("{here} and {vs} have diverged ({} here, {} there)", commits(a), commits(n)),
                                "git pull --rebase (or merge), push, let CI pass, then release",
                            )?,
                        }
                    }
                }
            },
        }
    }

    // The version: newer than every v* tag, not older than Cargo.toml.
    let latest = local_tags
        .iter()
        .chain(remote_tags.iter().flatten())
        .filter_map(|t| Version::parse(t.strip_prefix('v')?).ok())
        .max();
    if !here && !there {
        match &latest {
            Some(l) if target <= *l => r.fail(
                &format!("{target} is not newer than the latest tag v{l}"),
                &format!("pick a newer version: cargo release {} (or minor, major)", l.bump(Part::Patch)),
            )?,
            Some(l) => r.ok(&format!("{target} is newer than the latest tag v{l}")),
            None => r.ok("no v* tags yet: this is the first release"),
        }
    }
    if target < current {
        r.fail(
            &format!("{target} is older than Cargo.toml's {current}"),
            &format!("release {current} (tagged as is) or a newer version"),
        )?;
    } else if target != current && latest.as_ref().is_none_or(|l| *l < current) {
        r.note(&format!("note: Cargo.toml's {current} was never tagged; `cargo release {current}` would tag it as is"));
    }

    // ---------------------------------------------------------------- 2
    // Before anything slow: a red or missing CI run stops the release here.
    r.step(&format!("CI status of {short}"));
    let mut api = github.as_ref().map(|g| GitHub::new(g, env.token.clone()));
    match api.as_mut() {
        _ if !o.ci_check => r.note("skipped (--no-ci-check)"),
        None => r.note("skipped: origin is not on GitHub"),
        Some(api) => ci_check(&mut r, api, &head, o)?,
    }

    // ---------------------------------------------------------------- 3
    r.step(&if target == current { format!("Version {target}") } else { format!("Version {current} -> {target}") });
    let mut edits: Vec<(&str, String)> = Vec::new();
    let mut names = Vec::new();
    for m in MANIFESTS {
        let p = root.join(m);
        if !p.exists() {
            continue;
        }
        let text = read(&p)?;
        let old = package_value(&text, "version").map_err(|e| format!("{m}: {e}"))?;
        if old != target.to_string() {
            names.push(package_value(&text, "name").map_err(|e| format!("{m}: {e}"))?);
            r.note(&format!("{m:<20} version = \"{old}\" -> \"{target}\""));
            edits.push((m, set_package_value(&text, "version", &target.to_string()).map_err(|e| format!("{m}: {e}"))?));
        }
    }
    let lock_tracked = git.succeeds(&["ls-files", "--error-unmatch", "Cargo.lock"]);
    // What a failure before the commit puts back (the tree was clean).
    let mut restore: Vec<&str> = edits.iter().map(|(m, _)| *m).collect();
    if lock_tracked && !edits.is_empty() {
        restore.push("Cargo.lock");
    }
    if edits.is_empty() {
        r.ok(&format!("the manifests already say {target}: {tag} goes on {short} as is, with no release commit"));
    } else if o.dry_run {
        r.note("would write them, then:");
        r.cmd("cargo update --workspace --offline   (Cargo.lock: the workspace's own entries)");
    } else {
        for (m, text) in &edits {
            std::fs::write(root.join(m), text).map_err(|e| format!("writing {m}: {e}"))?;
        }
    }

    // From the first write until the commit, a failure undoes the edit
    // (unless someone else changed the version files meanwhile).
    let mut undo = true;
    let prepared = (|| -> Result<(), String> {
        if !edits.is_empty() && !o.dry_run {
            update_lock(&mut r, &git, root, &env.cargo, &names, &target, &restore)?;
        }

        // ------------------------------------------------------------ 4
        r.step("Local checks");
        let cargo = &env.cargo;
        let mut checks: Vec<&[&str]> = vec![
            &["fmt", "--all", "--check"][..],
            &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"][..],
            &["test", "--workspace", "--bins", "--lib"][..],
        ];
        if o.full {
            checks.push(&["test", "--test", "integration"][..]);
        }
        if o.skip_checks {
            r.note("skipped (--skip-checks)");
            checks.clear();
        }
        for args in &checks {
            let label = format!("cargo {}", args.join(" "));
            r.cmd(&label);
            if o.dry_run {
                continue;
            }
            let st = Command::new(cargo).args(*args).current_dir(root).status().map_err(|e| format!("{label}: {e}"))?;
            if !st.success() {
                let fix = if args[0] == "fmt" {
                    "cargo fmt --all, commit, push, let CI pass, then release again".to_string()
                } else {
                    format!("fix it on {}, push, let CI pass, then release again", o.branch)
                };
                r.fail(&format!("{label} failed"), &fix)?;
            }
        }
        if !o.dry_run {
            // The checks must not have changed what gets committed.
            if let Err(changed) = still_as_checked(&git, root, &head, &o.branch, &edits, &restore) {
                undo = changed.undo;
                r.fail(&changed.what, &changed.fix)?;
            }
        }

        // ------------------------------------------------------------ 5
        r.step("Confirmation");
        let release_url = github
            .as_ref()
            .map_or("the GitHub Release".to_string(), |g| format!("https://github.com/{g}/releases/tag/{tag}"));
        if edits.is_empty() {
            r.note("commit  none: the manifests already say this version");
        } else {
            r.note(&format!("commit  \"Release {tag}\" on {}: {}", o.branch, restore.join(", ")));
        }
        r.note(&format!(
            "tag     {tag}, annotated \"Warden {target}\", on {}",
            if edits.is_empty() { short.as_str() } else { "that commit" }
        ));
        let lease_short = origin_head.as_deref().map_or("?", |s| s.get(..7).unwrap_or(s));
        r.note(&format!(
            "push    {} to origin's {b} and the tag, both or neither (--atomic), and only while\n        \
             origin's {b} is still at {lease_short} (--force-with-lease)",
            if edits.is_empty() { short.as_str() } else { "that commit" },
            b = o.branch,
        ));
        r.note(&format!(
            "then    GitHub Actions (release.yml) builds the CLI and the GUI for Linux x86_64/arm64 and\n        \
             macOS arm64/x86_64, writes SHA256SUMS, tests install.sh, and publishes\n        {release_url}"
        ));
        if o.dry_run {
            r.note(&format!("would ask: Release {tag}? [y/N]   (--yes skips the question)"));
        } else if o.yes {
            r.note("not asking (--yes)");
        } else {
            eprint!("   Release {tag}? [y/N] ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).map_err(|e| format!("reading the answer: {e}"))?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                return Err("not confirmed: nothing was committed, tagged or pushed".into());
            }
        }
        Ok(())
    })();
    if let Err(e) = prepared {
        if undo {
            undo_edit(&git, &restore, o.dry_run);
        }
        return Err(e);
    }

    // ---------------------------------------------------------------- 6
    r.step("Commit and tag");
    let mut args = vec!["interpret-trailers".to_string()];
    for t in &o.trailers {
        args.extend(["--trailer".to_string(), t.clone()]);
    }
    // interpret-trailers also adds any trailer configured with git's trailer.<key>.cmd.
    let message = git
        .output(&args.iter().map(String::as_str).collect::<Vec<_>>(), Some(&format!("Release {tag}\n")))
        .map(|m| m.trim().to_string() + "\n");
    let message = match message {
        Ok(m) => m,
        Err(e) => {
            undo_edit(&git, &restore, o.dry_run);
            return Err(e);
        }
    };
    // Right before committing: the checkout must still be what CI and the
    // checks ran on. Whatever moved HEAD meanwhile (a hook, an editor,
    // another terminal) must not slip unchecked commits into the release.
    if !o.dry_run
        && let Err(changed) = still_as_checked(&git, root, &head, &o.branch, &edits, &restore)
    {
        if changed.undo {
            undo_edit(&git, &restore, false);
        }
        r.fail(&changed.what, &changed.fix)?;
    }
    // What is tagged and pushed: the checked commit itself, or the release
    // commit made on exactly it.
    let mut sha = head.clone();
    if !edits.is_empty() {
        r.cmd(&format!(
            "git commit-tree <{short} with {}> -p {short} -F -; git update-ref refs/heads/{b} <it> {short}",
            restore.join(", "),
            b = o.branch
        ));
        r.note(&message.lines().map(|l| format!("  | {l}").trim_end().to_string()).collect::<Vec<_>>().join("\n"));
        if !o.dry_run {
            match commit_release(&git, root, &head, &o.branch, &restore, &message, &format!("release: Release {tag}")) {
                Ok(c) => sha = c,
                Err(e) => {
                    undo_edit(&git, &restore, false);
                    return Err(e);
                }
            }
            r.ok(&format!("committed {}", git.out(&["log", "-1", "--format=%h %s", &sha])?));
        }
    }
    let unmade = o.dry_run && !edits.is_empty();
    let sha_shown = if unmade { "<that commit>" } else { sha.as_str() };
    let sha_short = sha_shown.get(..7).filter(|_| !unmade).unwrap_or(sha_shown).to_string();
    // The exact push of step 7, for printing.
    let lease_shown = origin_head.clone().unwrap_or_else(|| format!("<origin's {}>", o.branch));
    let push_line = format!("git {}", push_args(&o.branch, &tag, sha_shown, &lease_shown).join(" "));
    let tag_message = format!("Warden {target}");
    r.cmd(&format!("git tag -a {tag} -m \"{tag_message}\" {sha_short}"));
    if !o.dry_run {
        git.out(&["tag", "-a", &tag, "-m", &tag_message, &sha]).map_err(|e| {
            format!(
                "{e}\n  {}tag it yourself (git tag -a {tag} -m \"{tag_message}\" {sha}) and push ({push_line})",
                if edits.is_empty() {
                    ""
                } else {
                    "The release commit is made: undo it (git reset --keep HEAD~1), or "
                }
            )
        })?;
        r.ok(&format!("tagged {tag} -> {sha_short}"));
    }

    // ---------------------------------------------------------------- 7
    r.step("Push");
    r.cmd(&push_line);
    if !o.dry_run {
        // A real run stops in step 1 if it can't read origin's branch.
        let lease = origin_head.as_deref().ok_or_else(|| format!("origin's {} was never read", o.branch))?;
        let args = push_args(&o.branch, &tag, &sha, lease);
        let out = git
            .command(&args.iter().map(String::as_str).collect::<Vec<_>>())
            .output()
            .map_err(|e| format!("git push: {e}"))?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        for l in stderr.lines().filter(|l| !l.trim().is_empty()) {
            eprintln!("   | {l}");
        }
        if !out.status.success() {
            let here = if edits.is_empty() {
                format!("tag {tag} is")
            } else {
                format!("the release commit and tag {tag} are")
            };
            let (what, fix) = match classify_push(&stderr) {
                PushError::Denied => (
                    format!("origin refused the push: {tag} couldn't be pushed from this environment"),
                    format!(
                        "Nothing is undone: {here} in this checkout. Where pushing tags is allowed\n\
                         (this checkout with credentials that may push tags), run:\n    {push_line}\n\
                         From another clone, run `cargo release {target}` there instead."
                    ),
                ),
                PushError::Moved => (
                    format!(
                        "origin's {b} moved since the check (it is no longer at {}): nothing was pushed (--atomic)",
                        lease.get(..7).unwrap_or(lease),
                        b = o.branch
                    ),
                    format!(
                        "see what origin has now: git fetch origin {b}; git log --oneline --graph HEAD...FETCH_HEAD\n\
                         then start over on top of it: git tag -d {tag}{}, bring {b} up to date with origin,\n\
                         let CI pass, then cargo release {target}",
                        if edits.is_empty() { "" } else { "; git reset --keep HEAD~1" },
                        b = o.branch
                    ),
                ),
                PushError::Other => (
                    "git push failed: nothing was pushed (--atomic pushes everything or nothing)".to_string(),
                    format!("{here} in this checkout; retry: {push_line}"),
                ),
            };
            r.fail(&what, &fix)?;
        }
        r.ok(&format!("pushed {sha_short} to origin's {}, and {tag}", o.branch));
    }

    // ---------------------------------------------------------------- 8
    // After the push: the workflow's Linux builds are already running, and its
    // `macos` job waits (an hour) for the archives this uploads to the draft.
    r.step("macOS archives");
    let by_hand = format!("on a Mac with {tag} checked out (git pull): cargo xtask dist-macos");
    if !o.macos {
        r.note(&format!(
            "skipped (--no-macos). The Release workflow waits up to an hour for the macOS archives: {by_hand}"
        ));
    } else if !env.mac {
        r.note(&format!(
            "this is not a Mac. The Release workflow waits up to an hour for the macOS archives: {by_hand}"
        ));
    } else if !root.join(MACOS_SCRIPT).is_file() {
        r.note(&format!("{MACOS_SCRIPT} not found: the macOS archives are not built here ({by_hand})"));
    } else {
        let cmd = format!("{MACOS_SCRIPT} --branch {}", o.branch);
        r.cmd(&cmd);
        if !o.dry_run {
            let built = dist_macos(&["--branch".to_string(), o.branch.clone()], root);
            if let Err(e) = built {
                r.fail(
                    &format!("the macOS archives were not built or uploaded ({e})"),
                    &format!(
                        "{tag} is pushed, and the Release workflow waits up to an hour for them.\n\
                         Fix the cause shown above, then run here: cargo xtask dist-macos\n\
                         (it needs rustup, cargo-about and gh logged in). If the hour passed, re-run the\n\
                         workflow afterwards (Actions > Release > Re-run all jobs)."
                    ),
                )?;
            } else {
                r.ok(&format!("the macOS archives are in the draft release {tag}"));
            }
        }
    }

    // ---------------------------------------------------------------- 9
    r.step("Release workflow");
    let workflow_url = github.as_ref().map(|g| format!("https://github.com/{g}/actions/workflows/release.yml"));
    match (o.follow, api.as_mut()) {
        (false, _) => r.note(&format!(
            "not followed (--no-wait){}",
            workflow_url.as_ref().map(|u| format!(": {u}")).unwrap_or_default()
        )),
        (true, None) => r.note("not followed: origin is not on GitHub"),
        (true, Some(_)) if o.dry_run => r.note(&format!(
            "would follow the Release run for {tag} (polling every ~30 s), then print the release URL and its assets"
        )),
        (true, Some(api)) => follow(&mut r, api, &tag, &sha)?,
    }

    if o.dry_run {
        if r.problems.is_empty() {
            eprintln!("\nxtask: dry run OK, nothing was changed. Without --dry-run, this releases {tag}.");
            Ok(())
        } else {
            Err(format!(
                "dry run: {} problem(s), each with its fix above; a real run would stop at the first:\n  - {}",
                r.problems.len(),
                r.problems.join("\n  - ")
            ))
        }
    } else {
        eprintln!(
            "\nxtask: {tag} is {}",
            if o.follow { "released" } else { "pushed; the Release workflow publishes it" }
        );
        Ok(())
    }
}

/// Step 2: CI must have passed for `sha`.
fn ci_check(r: &mut Report, api: &mut GitHub, sha: &str, o: &Options) -> Result<(), String> {
    let short = &sha[..sha.len().min(7)];
    if api.token.is_none() {
        r.note("(no GITHUB_TOKEN or GH_TOKEN: unauthenticated GitHub API, 60 requests an hour)");
    }
    api.patient = o.wait_ci && !o.dry_run;
    let t0 = Instant::now();
    let mut shown = String::new();
    loop {
        let runs = match api.json(&format!("actions/runs?head_sha={sha}&per_page=100")) {
            Ok(v) => v,
            Err(e) => {
                return r.fail(&format!("can't read the CI status: {e}"), "run this again, or pass --no-ci-check");
            }
        };
        let waiting = o.wait_ci && !o.dry_run && t0.elapsed() < Duration::from_secs(3 * 3600);
        match ci_state(&runs) {
            Ci::Passed(run) => {
                r.ok(&format!("CI passed: {}", run.url));
                return Ok(());
            }
            Ci::Failed(run) => {
                return r.fail(
                    &format!("CI {} for {short}: {}", run.conclusion.as_deref().unwrap_or("failed"), run.url),
                    &format!(
                        "fix it and release the fixed commit; if it was flaky, re-run it (gh run rerun {} --failed);\n\
                         or pass --no-ci-check",
                        run.id
                    ),
                );
            }
            Ci::Running(run) if o.wait_ci && o.dry_run => {
                r.note(&format!("CI is {} ({}): a real run would wait for it (--wait-ci)", run.status, run.url));
                return Ok(());
            }
            Ci::Running(run) if waiting => {
                if shown != run.status {
                    r.note(&format!("CI is {}: {} (waiting for it: --wait-ci)", run.status, run.url));
                    shown = run.status;
                }
            }
            Ci::Running(run) => {
                return r.fail(
                    &format!("CI is still running ({}) for {short}: {}", run.status, run.url),
                    "wait for it and run this again, or pass --wait-ci to wait here",
                );
            }
            Ci::Missing if waiting && t0.elapsed() < Duration::from_secs(600) => {
                if shown.is_empty() {
                    r.note(&format!("no CI run for {short} yet: waiting (--wait-ci)"));
                    shown = "missing".into();
                }
            }
            Ci::Missing => {
                return r.fail(
                    &format!("no CI run on GitHub for {short}"),
                    "CI runs on every push: push the commit, let CI pass, then release; or pass --no-ci-check",
                );
            }
        }
        std::thread::sleep(api.poll_interval(30, 1));
    }
}

/// Step 3, after the manifests are written: Cargo.lock follows, and the
/// release goes on with exactly the files the version touches.
fn update_lock(
    r: &mut Report,
    git: &Git,
    root: &Path,
    cargo: &str,
    names: &[String],
    target: &Version,
    restore: &[&str],
) -> Result<(), String> {
    let mut done = false;
    for args in [&["update", "--workspace", "--offline"][..], &["update", "--workspace"][..]] {
        r.cmd(&format!("cargo {}", args.join(" ")));
        let out = Command::new(cargo).args(args).current_dir(root).output().map_err(|e| format!("cargo: {e}"))?;
        if out.status.success() {
            done = true;
            break;
        }
        r.note(
            &String::from_utf8_lossy(&out.stderr).lines().map(|l| format!("  | {l}")).collect::<Vec<_>>().join("\n"),
        );
    }
    if !done {
        return r.fail(
            "cargo update --workspace failed: Cargo.lock can't follow the new version",
            "see cargo's message above; the version edit is undone",
        );
    }
    if let Ok(lock) = read(&root.join("Cargo.lock")) {
        let stale: Vec<&str> = names
            .iter()
            .filter(|n| lock_version(&lock, n).as_deref() != Some(target.to_string().as_str()))
            .map(String::as_str)
            .collect();
        if !stale.is_empty() {
            return r.fail(
                &format!("Cargo.lock still has the old version of {}", stale.join(", ")),
                "run cargo update --workspace and look at Cargo.lock; the version edit is undone",
            );
        }
        r.ok(&format!("Cargo.lock: {} -> {target}", names.join(", ")));
    }
    let status = git.out(&["status", "--porcelain"])?;
    let extra: Vec<&str> = status.lines().map(|l| l.get(3..).unwrap_or(l)).filter(|p| !restore.contains(p)).collect();
    if !extra.is_empty() {
        return r.fail(
            &format!("cargo update changed more than the version: {}", extra.join(", ")),
            "see git status; the version edit is undone",
        );
    }
    r.note(&git.out(&["diff", "--stat"])?);
    Ok(())
}

/// What changed in the checkout since the checks (see `still_as_checked`).
struct Changed {
    what: String,
    fix: String,
    /// Putting the version files back loses nobody's work.
    undo: bool,
}

/// The checkout is still what CI and the local checks ran on: HEAD is
/// `head`, on `branch`; nothing changed but the version files, and they
/// still say what step 3 wrote.
fn still_as_checked(
    git: &Git,
    root: &Path,
    head: &str,
    branch: &str,
    edits: &[(&str, String)],
    restore: &[&str],
) -> Result<(), Changed> {
    let short = |s: &str| s.get(..7).unwrap_or(s).to_string();
    let undone = if edits.is_empty() {
        "Nothing was committed, tagged or pushed."
    } else {
        "Nothing was committed, tagged or pushed, and the version edit is undone."
    };
    let changed = |what: String, fix: String| Changed { what, fix, undo: true };
    let unreadable = |e: String| {
        changed(format!("can't check that the checkout is unchanged: {e}"), format!("{undone} See git status"))
    };
    let on = git.out(&["symbolic-ref", "--quiet", "HEAD"]).unwrap_or_default();
    if on != format!("refs/heads/{branch}") {
        let now = on.strip_prefix("refs/heads/").unwrap_or(if on.is_empty() { "a detached HEAD" } else { &on });
        return Err(changed(
            format!("the checkout left {branch} during the release (it is on {now} now)"),
            format!("{undone} git switch {branch}, then release again"),
        ));
    }
    let now = git.out(&["rev-parse", "--verify", "HEAD"]).map_err(unreadable)?;
    if now != head {
        return Err(changed(
            format!(
                "HEAD moved during the release, from {} (what CI and the checks ran on) to {}",
                short(head),
                short(&now)
            ),
            format!(
                "{undone} See what came in (git log --oneline {}..HEAD); push it, let CI pass, then release again",
                short(head)
            ),
        ));
    }
    let status = git.out(&["status", "--porcelain"]).map_err(unreadable)?;
    let extra: Vec<&str> = status.lines().map(|l| l.get(3..).unwrap_or(l)).filter(|p| !restore.contains(p)).collect();
    if !extra.is_empty() {
        return Err(changed(
            format!("the working tree changed during the release: {}", extra.join(", ")),
            format!("{undone} See git status; commit or remove these, then release again"),
        ));
    }
    for (m, text) in edits {
        if read(&root.join(m)).ok().as_deref() != Some(text.as_str()) {
            return Err(Changed {
                what: format!("{m} changed during the release: it no longer has just the version edit"),
                fix: format!(
                    "Nothing was committed, tagged or pushed; the version files are left as they are.\n\
                     See git diff; put them back (git checkout HEAD -- {}), then release again",
                    restore.join(" ")
                ),
                undo: false,
            });
        }
    }
    Ok(())
}

/// Step 6: the release commit, made of exactly `head` (what CI and the
/// checks ran on) with `files` as they are in the working tree, whatever HEAD
/// or the index say meanwhile; `branch` moves to it only if it is still at
/// `head`. Plumbing, so commit hooks don't run; signed if commit.gpgsign says
/// so, as `git commit` would. Returns its sha.
fn commit_release(
    git: &Git,
    root: &Path,
    head: &str,
    branch: &str,
    files: &[&str],
    message: &str,
    reflog: &str,
) -> Result<String, String> {
    let index = root.join(git.out(&["rev-parse", "--git-path", "warden-release-index"])?);
    let _ = std::fs::remove_file(&index);
    let tree = (|| {
        git.output_with_index(&["read-tree", head], None, Some(&index))?;
        let mut add = vec!["update-index", "--add", "--"];
        add.extend(files);
        git.output_with_index(&add, None, Some(&index))?;
        git.output_with_index(&["write-tree"], None, Some(&index))
    })();
    let _ = std::fs::remove_file(&index);
    let tree = tree?;
    let sign = git.out(&["config", "--type=bool", "--get", "commit.gpgsign"]).is_ok_and(|v| v == "true");
    let mut args = vec!["commit-tree", tree.as_str(), "-p", head];
    if sign {
        args.push("-S");
    }
    args.extend(["-F", "-"]);
    let sha = git.output(&args, Some(message))?;
    git.out(&["update-ref", "-m", reflog, &format!("refs/heads/{branch}"), &sha, head]).map_err(|e| {
        format!("{e}\n  {branch} moved while the release commit was made: nothing was committed, tagged or pushed")
    })?;
    // The index still has the old version files: it follows the commit.
    let mut reset = vec!["reset", "--quiet", "--"];
    reset.extend(files);
    if let Err(e) = git.out(&reset) {
        eprintln!(
            "   note: {e}\n   git status shows the version files as changed, though they are committed: git {}",
            reset.join(" ")
        );
    }
    Ok(sha)
}

/// Step 7: exactly `sha` to origin's `branch`, and the tag, both or neither,
/// and only while origin's `branch` is still at `lease`, its sha in step 1:
/// a push to origin since then is refused, never overwritten.
fn push_args(branch: &str, tag: &str, sha: &str, lease: &str) -> Vec<String> {
    vec![
        "push".into(),
        "--atomic".into(),
        format!("--force-with-lease=refs/heads/{branch}:{lease}"),
        "origin".into(),
        format!("{sha}:refs/heads/{branch}"),
        format!("refs/tags/{tag}"),
    ]
}

/// Put the version files back as they were at HEAD (the tree was clean).
fn undo_edit(git: &Git, files: &[&str], dry_run: bool) {
    if dry_run || files.is_empty() {
        return;
    }
    let mut args = vec!["checkout", "HEAD", "--"];
    args.extend(files);
    match git.out(&args) {
        Ok(_) => eprintln!("   undone: {} are back as they were (HEAD)", files.join(", ")),
        Err(e) => eprintln!("   could not undo the version edit ({e}): git {}", args.join(" ")),
    }
}

/// Step 9: the Release run for the pushed tag, job by job, to the release.
fn follow(r: &mut Report, api: &mut GitHub, tag: &str, sha: &str) -> Result<(), String> {
    api.patient = true;
    let t0 = Instant::now();
    let workflow_url = format!("https://github.com/{}/actions/workflows/release.yml", api.repo);
    if api.token.is_none() {
        r.note("(no GITHUB_TOKEN or GH_TOKEN: polling slowly enough for 60 API requests an hour)");
    }
    // GitHub starts the run a few seconds after the push.
    let run = loop {
        match api.json(&format!("actions/runs?head_sha={sha}&event=push&per_page=100")) {
            Ok(v) => {
                if let Some(run) = runs_named(&v, RELEASE_WORKFLOW, Some(tag)).into_iter().next() {
                    break run;
                }
            }
            Err(e) => r.note(&format!("GitHub API: {e}; trying again")),
        }
        if t0.elapsed() > Duration::from_secs(600) {
            return r.fail(
                &format!("no Release run for {tag} after 10 minutes"),
                &format!("the tag is pushed; look at {workflow_url}"),
            );
        }
        std::thread::sleep(api.poll_interval(10, 1));
    };
    r.note(&format!("{} (Ctrl-C stops following, not the release)", run.url));

    let mut shown: HashMap<u64, String> = HashMap::new();
    let mut last_jobs: Vec<Job>;
    let mut errors = 0;
    let done = loop {
        // The run first, then its jobs: once the run is completed, so are they.
        let polled = api.json(&format!("actions/runs/{}", run.id)).and_then(|v| {
            let info = run_info(&v).ok_or("unexpected run JSON")?;
            Ok((info, api.json(&format!("actions/runs/{}/jobs?per_page=100", run.id))?))
        });
        match polled {
            Ok((info, jv)) => {
                errors = 0;
                last_jobs = jobs(&jv);
                for j in &last_jobs {
                    if shown.get(&j.id) != Some(&j.state) {
                        eprintln!("   [{}] {:<32} {}", elapsed(t0), j.name, j.state);
                        shown.insert(j.id, j.state.clone());
                    }
                }
                if info.status == "completed" {
                    break info;
                }
            }
            Err(e) => {
                errors += 1;
                r.note(&format!("GitHub API: {e}"));
                if errors == 10 {
                    return r.fail(
                        "lost track of the Release run (GitHub API errors)",
                        &format!("follow it at {}", run.url),
                    );
                }
            }
        }
        if t0.elapsed() > Duration::from_secs(4 * 3600) {
            return r.fail("the Release run is still going after 4 hours", &format!("follow it at {}", run.url));
        }
        std::thread::sleep(api.poll_interval(30, 2));
    };

    if done.conclusion.as_deref() == Some("success") {
        r.ok(&format!("the Release workflow passed, in {}", elapsed(t0).trim()));
        match api.request(&format!("releases/tags/{tag}")) {
            Ok(resp) if resp.status == 200 => {
                let v: Value = serde_json::from_str(&resp.body).unwrap_or_default();
                let (url, assets) = release_assets(&v);
                eprintln!("\n   Release: {url}");
                eprintln!("   Assets ({}):", assets.len());
                for (name, size) in &assets {
                    eprintln!("     {name:<44} {:>10}", human_size(*size));
                }
            }
            Ok(resp) if resp.status == 404 => {
                r.note(&format!("but GitHub has no release {tag} (was the publish job skipped?): {}", done.url))
            }
            Ok(resp) => r.note(&api.error(&resp, &format!("releases/tags/{tag}"))),
            Err(e) => r.note(&e),
        }
        Ok(())
    } else {
        for j in last_jobs.iter().filter(|j| !matches!(j.state.as_str(), "success" | "skipped")) {
            eprintln!("   failed job: {} ({}) {}", j.name, j.state, j.url);
        }
        r.fail(
            &format!("the Release workflow ended: {}: {}", done.conclusion.as_deref().unwrap_or("?"), done.url),
            &format!(
                "nothing is published unless every build and install test passes.\n\
                 If it was flaky: gh run rerun {} --failed (or Re-run failed jobs on the run page).\n\
                 If the code must change: git push --delete origin {tag} && git tag -d {tag},\n\
                 fix it, push, let CI pass, and release again.",
                done.id
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn semver_parse() {
        assert_eq!(v("0.1.0").to_string(), "0.1.0");
        assert_eq!(v("10.20.30").to_string(), "10.20.30");
        assert_eq!(v("1.2.3-rc.1").to_string(), "1.2.3-rc.1");
        assert_eq!(v("1.0.0-alpha-1.beta.0").to_string(), "1.0.0-alpha-1.beta.0");
        for bad in
            ["", "1", "1.2", "1.2.3.4", "01.2.3", "1.02.3", "1.2.x", "1.2.3-", "1.2.3-01", "1.2.3-a..b", "1.2.3-a_b"]
        {
            assert!(Version::parse(bad).is_err(), "{bad:?} parsed");
        }
        assert!(Version::parse("1.2.3+build").unwrap_err().contains("build metadata"));
        assert!(Version::parse("v1.2.3").is_err());
    }

    #[test]
    fn semver_order() {
        // The chain from semver.org, section 11, plus numeric (not text) order.
        let chain = [
            "0.9.9",
            "0.10.0",
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.1.0",
            "2.0.0",
        ];
        for w in chain.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
            assert!(v(w[1]) > v(w[0]));
        }
        assert_eq!(v("1.0.0").cmp(&v("1.0.0")), Ordering::Equal);
        let max = ["0.1.0", "0.10.0", "0.2.0", "0.10.0-rc.1"].iter().map(|s| v(s)).max().unwrap();
        assert_eq!(max, v("0.10.0"));
    }

    #[test]
    fn semver_bump() {
        let b = |s: &str, p| v(s).bump(p).to_string();
        assert_eq!(b("0.1.0", Part::Patch), "0.1.1");
        assert_eq!(b("0.1.0", Part::Minor), "0.2.0");
        assert_eq!(b("0.1.7", Part::Minor), "0.2.0");
        assert_eq!(b("0.1.7", Part::Major), "1.0.0");
        // A pre-release is released as is when that's the bump asked for.
        assert_eq!(b("1.2.3-rc.1", Part::Patch), "1.2.3");
        assert_eq!(b("1.2.0-rc.1", Part::Minor), "1.2.0");
        assert_eq!(b("1.2.3-rc.1", Part::Minor), "1.3.0");
        assert_eq!(b("2.0.0-rc.1", Part::Major), "2.0.0");
        assert_eq!(b("2.1.0-rc.1", Part::Major), "3.0.0");

        let cur = v("0.1.0");
        assert_eq!(target_version("patch", &cur).unwrap().to_string(), "0.1.1");
        assert_eq!(target_version("minor", &cur).unwrap().to_string(), "0.2.0");
        assert_eq!(target_version("major", &cur).unwrap().to_string(), "1.0.0");
        assert_eq!(target_version("v0.3.0", &cur).unwrap().to_string(), "0.3.0");
        assert_eq!(target_version("0.3.0-rc.2", &cur).unwrap().to_string(), "0.3.0-rc.2");
        assert!(target_version("latest", &cur).unwrap_err().contains("patch, minor, major"));
        assert!(target_version("0.3", &cur).is_err());
    }

    const MANIFEST: &str = "\
# The top comment stays.
[package]
name = \"warden\"
version = \"0.1.0\"   # the released version
edition = \"2024\"
description = \"\"\"
version = \"9.9.9\" in a multi-line string is text, not a key
\"\"\"
versions = \"not this one either\"

[workspace]
members = [\"protocol\", \"gui\"]

[dependencies]
serde = { version = \"1.0.229\", features = [\"derive\"] }

[dependencies.flate2]
version = \"1\"
";

    #[test]
    fn cargo_toml_version_edit_touches_only_the_package_version() {
        assert_eq!(package_value(MANIFEST, "version").unwrap(), "0.1.0");
        assert_eq!(package_value(MANIFEST, "name").unwrap(), "warden");
        let new = set_package_value(MANIFEST, "version", "0.2.0").unwrap();
        assert_eq!(new, MANIFEST.replace("version = \"0.1.0\"   #", "version = \"0.2.0\"   #"));
        let changed: Vec<(&str, &str)> = MANIFEST.lines().zip(new.lines()).filter(|(a, b)| a != b).collect();
        assert_eq!(
            changed,
            vec![("version = \"0.1.0\"   # the released version", "version = \"0.2.0\"   # the released version")]
        );

        // A dependency table before [package], CRLF line endings, odd spacing.
        let crlf =
            "[dependencies.x]\r\nversion = \"2.0\"\r\n\r\n[ package ]\r\nname = 'p'\r\n  version='0.1.0'\r\n[lib]\r\n";
        let new = set_package_value(crlf, "version", "1.0.0-rc.1").unwrap();
        assert_eq!(new, crlf.replace("version='0.1.0'", "version='1.0.0-rc.1'"));
        assert_eq!(package_value(&new, "version").unwrap(), "1.0.0-rc.1");

        assert!(package_value("[workspace]\nversion = \"1\"\n", "version").unwrap_err().contains("no [package]"));
        assert!(
            package_value("[package]\nname = \"x\"\n[dependencies]\nversion = \"1\"\n", "version")
                .unwrap_err()
                .contains("no version")
        );
        assert!(package_value("[package]\nversion.workspace = true\n", "version").unwrap_err().contains("workspace"));
        assert!(package_value("[package]\nversion = 1\n", "version").unwrap_err().contains("not a plain string"));
    }

    #[test]
    fn cargo_lock_version_of_workspace_packages() {
        let lock = "\
version = 4

[[package]]
name = \"warden\"
version = \"9.0.0\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"

[[package]]
name = \"warden\"
version = \"0.2.0\"
dependencies = [
 \"warden-protocol\",
]

[[package]]
name = \"warden-protocol\"
version = \"0.2.0\"
";
        assert_eq!(lock_version(lock, "warden").as_deref(), Some("0.2.0"));
        assert_eq!(lock_version(lock, "warden-protocol").as_deref(), Some("0.2.0"));
        assert_eq!(lock_version(lock, "warden-gui"), None);
    }

    #[test]
    fn github_owner_and_repo_from_remotes() {
        for url in [
            "https://github.com/oceanwap/warden",
            "https://github.com/oceanwap/warden.git",
            "https://github.com/oceanwap/warden/",
            "http://github.com/oceanwap/warden",
            "https://x-access-token:secret@github.com/oceanwap/warden.git",
            "git@github.com:oceanwap/warden.git",
            "git@github.com:oceanwap/warden",
            "github.com:oceanwap/warden.git",
            "ssh://git@github.com/oceanwap/warden.git",
            "ssh://git@ssh.github.com:443/oceanwap/warden.git",
            "git://github.com/oceanwap/warden.git",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("oceanwap/warden"), "{url}");
        }
        assert_eq!(github_repo("git@github.com:some-org/my.repo_2.git").as_deref(), Some("some-org/my.repo_2"));
        for url in [
            "https://gitlab.com/oceanwap/warden.git",
            "git@gitlab.com:oceanwap/warden.git",
            "/tmp/origin.git",
            "file:///tmp/origin.git",
            "https://github.com/oceanwap",
            "https://github.com/a/b/c",
            "https://github.com.evil.example/a/b",
        ] {
            assert_eq!(github_repo(url), None, "{url}");
        }
        assert_eq!(redact("https://user:secret@github.com/o/r.git"), "https://github.com/o/r.git");
        assert_eq!(redact("git@github.com:o/r.git"), "git@github.com:o/r.git");
    }

    /// The fields Warden reads, shaped as GitHub sends them (trimmed).
    const RUNS: &str = r#"{
      "total_count": 5,
      "workflow_runs": [
        {"id": 100, "name": "CI", "head_branch": "main", "head_sha": "abc", "event": "push", "status": "completed",
         "conclusion": "failure", "html_url": "https://github.com/o/r/actions/runs/100", "run_attempt": 1},
        {"id": 101, "name": "CI", "head_branch": "main", "head_sha": "abc", "event": "push", "status": "completed",
         "conclusion": "success", "html_url": "https://github.com/o/r/actions/runs/101", "run_attempt": 2},
        {"id": 102, "name": "Release", "head_branch": "v0.2.0", "head_sha": "abc", "event": "push",
         "status": "in_progress", "conclusion": null, "html_url": "https://github.com/o/r/actions/runs/102"},
        {"id": 103, "name": "Release", "head_branch": "ci-portability", "head_sha": "abc", "event": "push",
         "status": "completed", "conclusion": "success", "html_url": "https://github.com/o/r/actions/runs/103"},
        {"id": 99, "name": "Release", "head_branch": "v0.2.0", "head_sha": "abc", "event": "push",
         "status": "completed", "conclusion": "cancelled", "html_url": "https://github.com/o/r/actions/runs/99"}
      ]
    }"#;

    #[test]
    fn runs_response() {
        let runs: Value = serde_json::from_str(RUNS).unwrap();
        let run = |id: u64, status: &str, conclusion: Option<&str>| RunInfo {
            id,
            status: status.into(),
            conclusion: conclusion.map(String::from),
            url: format!("https://github.com/o/r/actions/runs/{id}"),
        };
        // A later success (a re-run) wins over an earlier failure.
        assert_eq!(ci_state(&runs), Ci::Passed(run(101, "completed", Some("success"))));
        // The Release run for the tag, newest first; not the branch's dry run.
        let release = runs_named(&runs, "Release", Some("v0.2.0"));
        assert_eq!(release, vec![run(102, "in_progress", None), run(99, "completed", Some("cancelled"))]);
        assert!(runs_named(&runs, "Release", Some("v9.9.9")).is_empty());

        let only = |keep: &[u64], extra: &str| -> Value {
            let mut v: Value = serde_json::from_str(RUNS).unwrap();
            let list = v["workflow_runs"].as_array_mut().unwrap();
            list.retain(|r| keep.contains(&r["id"].as_u64().unwrap()));
            if !extra.is_empty() {
                list.push(serde_json::from_str(extra).unwrap());
            }
            v
        };
        assert_eq!(ci_state(&only(&[100, 102], "")), Ci::Failed(run(100, "completed", Some("failure"))));
        let queued = r#"{"id": 104, "name": "CI", "head_branch": "main", "status": "queued", "conclusion": null,
                         "html_url": "https://github.com/o/r/actions/runs/104"}"#;
        assert_eq!(ci_state(&only(&[100], queued)), Ci::Running(run(104, "queued", None)));
        assert_eq!(ci_state(&only(&[102, 103], "")), Ci::Missing);
        assert_eq!(ci_state(&serde_json::from_str(r#"{"total_count": 0, "workflow_runs": []}"#).unwrap()), Ci::Missing);
        assert_eq!(ci_state(&serde_json::from_str(r#"{"message": "Not Found"}"#).unwrap()), Ci::Missing);
    }

    #[test]
    fn jobs_and_release_responses() {
        let v: Value = serde_json::from_str(
            r#"{"total_count": 3, "jobs": [
                {"id": 1, "name": "meta", "status": "completed", "conclusion": "success", "html_url": "https://j/1"},
                {"id": 2, "name": "cli linux-x86_64", "status": "in_progress", "conclusion": null, "html_url": "https://j/2"},
                {"id": 3, "name": "gui macos-arm64", "status": "completed", "conclusion": "failure", "html_url": "https://j/3"}
            ]}"#,
        )
        .unwrap();
        let states: Vec<(String, String)> = jobs(&v).into_iter().map(|j| (j.name, j.state)).collect();
        assert_eq!(
            states,
            [("meta", "success"), ("cli linux-x86_64", "in_progress"), ("gui macos-arm64", "failure")]
                .map(|(a, b)| (a.to_string(), b.to_string()))
        );

        let rel: Value = serde_json::from_str(
            r#"{"html_url": "https://github.com/o/r/releases/tag/v0.2.0", "tag_name": "v0.2.0", "assets": [
                {"name": "SHA256SUMS", "size": 1024, "browser_download_url": "https://x/SHA256SUMS"},
                {"name": "warden-0.2.0-linux-x86_64.tar.gz", "size": 2202010}
            ]}"#,
        )
        .unwrap();
        let (url, assets) = release_assets(&rel);
        assert_eq!(url, "https://github.com/o/r/releases/tag/v0.2.0");
        assert_eq!(assets, vec![("SHA256SUMS".into(), 1024), ("warden-0.2.0-linux-x86_64.tar.gz".into(), 2202010)]);
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(2202010), "2.1 MiB");
        assert_eq!(human_size(12), "12 B");
    }

    #[test]
    fn http_answers_and_rate_limits() {
        let raw = "HTTP/1.1 200 Connection Established\r\n\r\n\
                   HTTP/2 200 \r\nx-ratelimit-remaining: 57\r\nX-RateLimit-Reset: 1790824314\r\n\r\n{\"ok\": true}";
        let r = parse_http(raw).unwrap();
        assert_eq!((r.status, r.remaining, r.reset, r.retry_after), (200, Some(57), Some(1790824314), None));
        assert_eq!(r.body, "{\"ok\": true}");
        let r = parse_http("HTTP/1.1 403 Forbidden\r\nRetry-After: 60\r\nX-RateLimit-Remaining: 0\r\n\r\n{}").unwrap();
        assert_eq!((r.status, r.remaining, r.retry_after), (403, Some(0), Some(60)));
        assert!(parse_http("").is_err());

        assert_eq!(poll_secs(30, None, None, 2), 30);
        assert_eq!(poll_secs(30, Some(4990), Some(3600), 2), 30);
        // Unauthenticated: 60 an hour, 2 per poll, 5 kept back.
        assert_eq!(poll_secs(30, Some(60), Some(3600), 2), 134);
        assert_eq!(poll_secs(30, Some(6), Some(100), 2), 101);
    }

    #[test]
    fn push_errors() {
        let denied = "remote: Permission to oceanwap/warden.git denied to bot.\n\
                      fatal: unable to access 'https://github.com/oceanwap/warden/': The requested URL returned error: 403";
        assert_eq!(classify_push(denied), PushError::Denied);
        assert_eq!(
            classify_push(" ! [remote rejected] v0.2.0 -> v0.2.0 (protected tag hook declined)"),
            PushError::Denied
        );
        assert_eq!(classify_push(" ! [rejected]        main -> main (fetch first)"), PushError::Moved);
        assert_eq!(classify_push(" ! [rejected]        main -> main (non-fast-forward)"), PushError::Moved);
        assert_eq!(classify_push("fatal: unable to access: Could not resolve host: github.com"), PushError::Other);
    }

    #[test]
    fn options() {
        let a = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let o = parse(&a("0.2.0 --dry-run --yes --branch release --no-wait")).unwrap().unwrap();
        assert_eq!(
            (o.version.as_str(), o.branch.as_str(), o.dry_run, o.yes, o.follow, o.macos),
            ("0.2.0", "release", true, true, false, true)
        );
        assert!(!parse(&a("0.2.0 --no-macos")).unwrap().unwrap().macos);
        assert!(parse(&a("")).unwrap_err().contains("which version"));
        assert!(parse(&a("0.2.0 0.3.0")).unwrap_err().contains("one version"));
        assert!(parse(&a("0.2.0 --nope")).unwrap_err().contains("unknown option"));
        assert!(parse(&a("0.2.0 --full --skip-checks")).is_err());
        assert!(parse(&a("0.2.0 --wait-ci --no-ci-check")).is_err());
        assert!(parse(&a("0.2.0 --trailer nocolon")).is_err());
        let o = parse(&["patch".into(), "--trailer".into(), "Signed-off-by: A <a@b>".into()]).unwrap().unwrap();
        assert_eq!(o.trailers, vec!["Signed-off-by: A <a@b>"]);
    }

    // ---------------------------------------------------------------- the git flow, for real

    /// A Cargo workspace like Warden's in a git repository whose `origin` is
    /// a bare repository next to it.
    struct TempRepo {
        base: std::path::PathBuf,
        work: std::path::PathBuf,
        origin: std::path::PathBuf,
    }

    fn sh(dir: &Path, prog: &str, args: &[&str]) -> String {
        let mut c = Command::new(prog);
        c.args(args).current_dir(dir);
        for v in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            c.env_remove(v);
        }
        let out = c.output().unwrap_or_else(|e| panic!("{prog} {args:?}: {e}"));
        assert!(out.status.success(), "{prog} {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    impl TempRepo {
        fn new(name: &str) -> TempRepo {
            let base = std::env::temp_dir().join(format!("xtask-release-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let (work, origin) = (base.join("work"), base.join("origin.git"));
            std::fs::create_dir_all(&work).unwrap();
            sh(&base, "git", &["init", "--quiet", "--bare", "--initial-branch=main", "origin.git"]);
            sh(&work, "git", &["init", "--quiet", "--initial-branch=main"]);
            for (k, val) in [
                ("user.name", "Release Test"),
                ("user.email", "release-test@example.com"),
                ("commit.gpgsign", "false"),
                ("tag.gpgsign", "false"),
            ] {
                sh(&work, "git", &["config", k, val]);
            }
            sh(&work, "git", &["remote", "add", "origin", origin.to_str().unwrap()]);
            let files = [
                (
                    "Cargo.toml",
                    "[package]\nname = \"warden\"\nversion = \"0.1.0\"  # released with protocol/ and gui/\nedition = \"2021\"\n\n\
                     [workspace]\nmembers = [\"protocol\", \"gui\"]\n\n[dependencies]\nwarden-protocol = { path = \"protocol\" }\n",
                ),
                ("src/main.rs", "fn main() {}\n"),
                (
                    "protocol/Cargo.toml",
                    "[package]\nname = \"warden-protocol\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                ("protocol/src/lib.rs", "pub const VERSION: &str = env!(\"CARGO_PKG_VERSION\");\n"),
                (
                    "gui/Cargo.toml",
                    "[package]\nname = \"warden-gui\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                     [dependencies]\nwarden-protocol = { path = \"../protocol\" }\n",
                ),
                ("gui/src/main.rs", "fn main() {}\n"),
                (".gitignore", "/target\n"),
            ];
            for (path, text) in files {
                let p = work.join(path);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, text).unwrap();
            }
            sh(&work, &cargo_bin(), &["generate-lockfile", "--offline", "--quiet"]);
            sh(&work, "git", &["add", "-A"]);
            sh(&work, "git", &["commit", "--quiet", "-m", "init"]);
            sh(&work, "git", &["push", "--quiet", "-u", "origin", "main"]);
            TempRepo { base, work, origin }
        }

        fn release(&self, args: &[&str]) -> Result<(), String> {
            self.release_on(args, false)
        }

        /// `mac`: this is a Mac, so the macOS archives are built here.
        fn release_on(&self, args: &[&str], mac: bool) -> Result<(), String> {
            let mut args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            args.extend(["--no-ci-check", "--skip-checks", "--no-wait"].map(String::from));
            let o = parse(&args).unwrap().unwrap();
            release(&o, &self.work, &Env { interactive: false, token: None, cargo: cargo_bin(), mac })
        }

        /// A scripts/dist-macos.sh that records when it ran and what origin had
        /// by then, then exits with `code`; committed and pushed.
        fn fake_dist_script(&self, code: i32) {
            let script = format!(
                "#!/bin/sh\necho \"$@\" > ../dist-ran.txt\n\
                 git ls-remote --tags origin >> ../dist-ran.txt\nexit {code}\n"
            );
            std::fs::create_dir_all(self.work.join("scripts")).unwrap();
            std::fs::write(self.work.join(MACOS_SCRIPT), script).unwrap();
            sh(&self.work, "git", &["add", "-A"]);
            sh(&self.work, "git", &["commit", "--quiet", "-m", "fake dist script"]);
            sh(&self.work, "git", &["push", "--quiet", "origin", "main"]);
        }

        /// Every ref here and on origin, the status, and the version files.
        fn snapshot(&self) -> String {
            let mut s = sh(&self.work, "git", &["show-ref"]);
            s += "\n--\n";
            s += &sh(&self.origin, "git", &["show-ref"]);
            s += "\n--\n";
            s += &sh(&self.work, "git", &["status", "--porcelain", "--untracked-files=all"]);
            for f in ["Cargo.toml", "protocol/Cargo.toml", "gui/Cargo.toml", "Cargo.lock"] {
                s += &std::fs::read_to_string(self.work.join(f)).unwrap();
            }
            s
        }

        fn git(&self, args: &[&str]) -> String {
            sh(&self.work, "git", args)
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn release_flow_against_a_bare_origin() {
        let t = TempRepo::new("flow");
        let init = t.git(&["rev-parse", "HEAD"]);

        // A dry run checks and prints, and changes nothing.
        let before = t.snapshot();
        t.release(&["minor", "--dry-run"]).unwrap();
        assert_eq!(t.snapshot(), before);
        // Without --yes and without a terminal: refused before any change.
        assert!(t.release(&["minor"]).unwrap_err().contains("terminal"));
        assert_eq!(t.snapshot(), before);

        // Cargo.toml's version, never tagged: tagged as is, no commit.
        t.release(&["0.1.0", "--yes"]).unwrap();
        assert_eq!(t.git(&["rev-parse", "HEAD"]), init);
        assert_eq!(t.git(&["rev-parse", "v0.1.0^{commit}"]), init);
        assert_eq!(sh(&t.origin, "git", &["rev-parse", "v0.1.0^{commit}"]), init);

        // minor: a release commit with the three manifests and Cargo.lock.
        t.release(&["minor", "--yes", "--trailer", "Co-Authored-By: Test <test@example.com>"]).unwrap();
        let head = t.git(&["rev-parse", "HEAD"]);
        assert_eq!(t.git(&["rev-parse", "HEAD~1"]), init);
        assert_eq!(t.git(&["log", "-1", "--format=%B"]), "Release v0.2.0\n\nCo-Authored-By: Test <test@example.com>");
        assert_eq!(
            t.git(&["diff", "--name-only", "HEAD~1"]),
            "Cargo.lock\nCargo.toml\ngui/Cargo.toml\nprotocol/Cargo.toml"
        );
        let toml = std::fs::read_to_string(t.work.join("Cargo.toml")).unwrap();
        assert!(toml.contains("version = \"0.2.0\"  # released with protocol/ and gui/\n"), "{toml}");
        assert!(toml.contains("warden-protocol = { path = \"protocol\" }"));
        let lock = std::fs::read_to_string(t.work.join("Cargo.lock")).unwrap();
        for name in ["warden", "warden-protocol", "warden-gui"] {
            assert_eq!(lock_version(&lock, name).as_deref(), Some("0.2.0"), "{name}");
        }
        // An annotated tag on that commit, and both pushed.
        assert_eq!(t.git(&["cat-file", "-t", "v0.2.0"]), "tag");
        assert_eq!(t.git(&["tag", "-l", "--format=%(contents:subject)", "v0.2.0"]), "Warden 0.2.0");
        assert_eq!(t.git(&["rev-parse", "v0.2.0^{commit}"]), head);
        assert_eq!(sh(&t.origin, "git", &["rev-parse", "main"]), head);
        assert_eq!(sh(&t.origin, "git", &["rev-parse", "v0.2.0^{commit}"]), head);
        assert_eq!(t.git(&["status", "--porcelain"]), "");

        // Refused, with nothing changed: a version that exists, an older
        // one, a dirty tree, a commit origin doesn't have, another branch.
        let before = t.snapshot();
        assert!(t.release(&["0.2.0", "--yes"]).unwrap_err().contains("already exists"));
        assert!(t.release(&["0.1.5", "--yes"]).unwrap_err().contains("not newer than the latest tag v0.2.0"));
        assert!(t.release(&["0.2.0", "--dry-run"]).unwrap_err().contains("1 problem"));
        assert_eq!(t.snapshot(), before);
        std::fs::write(t.work.join("src/main.rs"), "fn main() { println!(); }\n").unwrap();
        assert!(t.release(&["patch", "--yes"]).unwrap_err().contains("uncommitted changes: src/main.rs"));
        t.git(&["commit", "--quiet", "-am", "local only"]);
        assert!(t.release(&["patch", "--yes"]).unwrap_err().contains("main has 1 commit that origin doesn't"));
        assert!(
            t.release(&["patch", "--yes", "--branch", "release"]).unwrap_err().contains("on branch main, not release")
        );
        // A dry run lists every problem, not just the first.
        let e = t.release(&["0.2.0", "--dry-run"]).unwrap_err();
        assert!(e.contains("2 problem(s)") && e.contains("already exists") && e.contains("origin doesn't"), "{e}");

        // Pushed, then released from there: patch.
        t.git(&["push", "--quiet", "origin", "main"]);
        t.release(&["patch", "--yes"]).unwrap();
        assert_eq!(t.git(&["log", "-1", "--format=%s"]), "Release v0.2.1");
        assert_eq!(sh(&t.origin, "git", &["rev-parse", "v0.2.1^{commit}"]), t.git(&["rev-parse", "HEAD"]));
    }

    #[test]
    fn a_refused_push_keeps_the_commit_and_the_tag() {
        let t = TempRepo::new("denied");
        let hook = t.origin.join("hooks/pre-receive");
        std::fs::write(&hook, "#!/bin/sh\necho 'Permission to oceanwap/warden.git denied to bot.' >&2\nexit 1\n")
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let origin_refs = sh(&t.origin, "git", &["show-ref"]);
        let e = t.release(&["minor", "--yes"]).unwrap_err();
        assert!(e.contains("couldn't be pushed from this environment"), "{e}");
        // Nothing undone here, nothing changed on origin.
        assert_eq!(t.git(&["log", "-1", "--format=%s"]), "Release v0.2.0");
        assert_eq!(t.git(&["rev-parse", "v0.2.0^{commit}"]), t.git(&["rev-parse", "HEAD"]));
        assert_eq!(sh(&t.origin, "git", &["show-ref"]), origin_refs);
        // Running again says to push what is there.
        assert!(t.release(&["0.2.0", "--yes"]).unwrap_err().contains("exists here but not on origin"));
        // The printed command, where pushing is allowed, finishes the job.
        std::fs::remove_file(&hook).unwrap();
        t.git(&["push", "--quiet", "--atomic", "origin", "main", "v0.2.0"]);
        assert_eq!(sh(&t.origin, "git", &["rev-parse", "v0.2.0^{commit}"]), t.git(&["rev-parse", "HEAD"]));
    }

    #[test]
    fn the_macos_archives_are_built_after_the_push_on_a_mac() {
        let t = TempRepo::new("macos");
        t.fake_dist_script(0);
        t.release_on(&["0.2.0", "--yes"], true).unwrap();
        // The script ran with the branch, and origin already had the tag.
        let ran = std::fs::read_to_string(t.base.join("dist-ran.txt")).unwrap();
        assert!(ran.starts_with("--branch main\n"), "{ran}");
        assert!(ran.contains("refs/tags/v0.2.0"), "the tag was not on origin yet: {ran}");
    }

    #[test]
    fn the_macos_archives_are_not_built_elsewhere_or_when_declined_or_in_a_dry_run() {
        for (args, mac) in [(vec!["0.2.0", "--yes"], false), (vec!["0.2.0", "--yes", "--no-macos"], true)] {
            let t = TempRepo::new("macos-skip");
            t.fake_dist_script(0);
            t.release_on(&args, mac).unwrap();
            assert!(!t.base.join("dist-ran.txt").exists(), "{args:?} on a Mac: {mac}");
            assert_eq!(t.git(&["tag", "--list"]), "v0.2.0");
        }
        let t = TempRepo::new("macos-dry");
        t.fake_dist_script(0);
        t.release_on(&["0.2.0", "--dry-run"], true).unwrap();
        assert!(!t.base.join("dist-ran.txt").exists());
        assert_eq!(t.git(&["tag", "--list"]), "");
    }

    #[test]
    fn a_failed_macos_build_leaves_the_pushed_release_and_says_what_to_do() {
        let t = TempRepo::new("macos-fail");
        t.fake_dist_script(1);
        let e = t.release_on(&["0.2.0", "--yes"], true).unwrap_err();
        assert!(e.contains("stopped at step 8/9"), "{e}");
        assert!(e.contains("macOS archives were not built or uploaded"), "{e}");
        // Nothing is undone: the tag is on origin, where the workflow waits.
        assert_eq!(sh(&t.origin, "git", &["tag", "--list"]), "v0.2.0");
        assert_eq!(t.git(&["status", "--porcelain"]), "");
    }

    #[test]
    fn a_failed_check_undoes_the_version_edit() {
        let t = TempRepo::new("undo");
        let before = t.snapshot();
        // The local checks run `cargo fmt --check`: badly formatted code fails
        // it after the version edit, which must then be undone.
        std::fs::write(t.work.join("src/main.rs"), "fn main(  ) {}\n").unwrap();
        t.git(&["commit", "--quiet", "-am", "unformatted"]);
        t.git(&["push", "--quiet", "origin", "main"]);
        let before_fmt = t.snapshot();
        assert_ne!(before, before_fmt);
        let o = parse(&["minor", "--yes", "--no-ci-check", "--no-wait"].map(String::from)).unwrap().unwrap();
        let e =
            release(&o, &t.work, &Env { interactive: false, token: None, cargo: cargo_bin(), mac: false }).unwrap_err();
        assert!(e.contains("cargo fmt --all --check failed"), "{e}");
        assert_eq!(t.snapshot(), before_fmt);
    }

    /// A cargo for the local checks: `cargo update` is the real one, fmt
    /// and clippy pass, and `cargo test` runs `on_test` (sh, in the
    /// checkout) and passes.
    fn fake_cargo(t: &TempRepo, on_test: &str) -> String {
        let path = t.base.join("fake-cargo");
        let script = format!(
            "#!/bin/sh\nset -e\ncase \"$1\" in\n  update) exec '{}' \"$@\" ;;\n  test) {on_test} ;;\nesac\nexit 0\n",
            cargo_bin()
        );
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_string()
    }

    fn release_with(t: &TempRepo, cargo: String) -> Result<(), String> {
        let o = parse(&["minor", "--yes", "--no-ci-check", "--no-wait"].map(String::from)).unwrap().unwrap();
        release(&o, &t.work, &Env { interactive: false, token: None, cargo, mac: false })
    }

    #[test]
    fn a_commit_made_during_the_checks_is_never_released() {
        let t = TempRepo::new("moved");
        let init = t.git(&["rev-parse", "HEAD"]);
        let origin_refs = sh(&t.origin, "git", &["show-ref"]);
        // Something commits while `cargo test` runs: an editor, a hook,
        // another terminal. CI and the checks never saw that commit.
        let cargo = fake_cargo(&t, "echo wip > wip.txt; git add wip.txt; git commit --quiet -m WIP");
        let e = release_with(&t, cargo).unwrap_err();
        assert!(e.contains("HEAD moved during the release"), "{e}");
        assert!(e.contains(&format!("from {}", &init[..7])), "{e}");
        // Nothing committed, tagged or pushed; the version edit is undone.
        assert_eq!(sh(&t.origin, "git", &["show-ref"]), origin_refs);
        assert_eq!(t.git(&["tag", "--list"]), "");
        assert_eq!(t.git(&["log", "-1", "--format=%s"]), "WIP");
        assert_eq!(t.git(&["rev-parse", "HEAD~1"]), init);
        assert_eq!(t.git(&["status", "--porcelain"]), "");
    }

    #[test]
    fn a_push_to_origin_during_the_release_is_refused_not_overwritten() {
        let t = TempRepo::new("lease");
        let init = t.git(&["rev-parse", "HEAD"]);
        std::fs::write(t.work.join("src/main.rs"), "fn main() { println!(); }\n").unwrap();
        t.git(&["commit", "--quiet", "-am", "bad"]);
        t.git(&["push", "--quiet", "origin", "main"]);
        let bad = t.git(&["rev-parse", "HEAD"]);
        // While the checks run, someone takes "bad" back out of origin's
        // main (a force push). A plain push of the release commit, on top of
        // "bad", would fast-forward and put it back.
        let origin = t.origin.to_str().unwrap();
        let cargo = fake_cargo(&t, &format!("git -C '{origin}' update-ref refs/heads/main {init}"));
        let e = release_with(&t, cargo).unwrap_err();
        assert!(e.contains(&format!("origin's main moved since the check (it is no longer at {})", &bad[..7])), "{e}");
        assert_eq!(sh(&t.origin, "git", &["rev-parse", "main"]), init);
        assert_eq!(sh(&t.origin, "git", &["tag", "--list"]), "");
        // Here: the release commit, on exactly the checked commit, and its tag.
        assert_eq!(t.git(&["log", "-1", "--format=%s"]), "Release v0.2.0");
        assert_eq!(t.git(&["rev-parse", "HEAD~1"]), bad);
        assert_eq!(t.git(&["rev-parse", "v0.2.0^{commit}"]), t.git(&["rev-parse", "HEAD"]));
        assert_eq!(t.git(&["status", "--porcelain"]), "");
    }
}
