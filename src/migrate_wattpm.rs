//! `warden migrate-wattpm`: turn a Platformatic Watt (`wattpm`) project into Warden configs.
//!
//! Source: the project's runtime config (`watt.json`, `platformatic.json`, ...: JSON, JSON5 with
//! comments, or TOML) and each application's own config, under `applications` / `services` / `web`
//! and `autoload`. It is read the way Watt 3.71.0 reads it: `{NAME}` placeholders from `.env`
//! files and the environment, the `workers` default that each application inherits, `enabled`
//! per environment, the entry point of a Node.js application. Nothing is run: not wattpm, not node.
//!
//! Output per application: `<app>.toml`, `<app>.env` (mode 0600: env values never go into the
//! config) and one `MIGRATION-wattpm.md` listing every setting as mapped, approximated (with the
//! difference), unsupported (with the reason) or to check. What Watt does and Warden does not
//! (composition behind one entry point, the `*.plt.local` mesh, undici interceptors, the Gateway)
//! is never dropped silently: it is in the report, and an application that cannot run on its own
//! is not converted and says why.
//!
//! `--cutover overlap|new-port:<port>` starts Warden's copy. It never stops wattpm: Watt's unit is
//! the whole runtime, `wattpm stop` takes every application down at once, and Warden could not
//! bring it back with the command line and environment it had.

use crate::cli::{Args, Command, StartOpts};
use crate::config::Config;
use crate::fleet::{self, Launch};
use crate::migrate::{Cutover, env_quote, parse_cutover, toml_str, warden_start, warden_stop};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The report's name. `pm2-migrate` writes MIGRATION.md into the same directory.
const REPORT: &str = "MIGRATION-wattpm.md";

/// Warden's limit for a number of seconds (`grace_period`, `ready_timeout`): one hour.
const MAX_SECONDS_MS: u64 = 3_600_000;
/// The longest restart delay whose `backoff_max` (16 times it) stays under Warden's one-hour limit.
const MAX_RESTART_DELAY_MS: u64 = MAX_SECONDS_MS / 16;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Opts {
    /// The project directory, or its config file (default: the current directory).
    pub path: Option<PathBuf>,
    /// The config file's name in the directory (wattpm's `-c`).
    pub config: Option<String>,
    /// A `.env` file for the placeholders and the environment (wattpm's `-e`).
    pub env_file: Option<PathBuf>,
    /// Only these applications (by Watt id).
    pub apps: Vec<String>,
    /// Where configs go (default: Warden's config directory).
    pub out: Option<PathBuf>,
    /// Put in front of every Warden app name (`shop` + `web` = `shop-web`).
    pub prefix: Option<String>,
    /// `--command <id>=<command line>`: what to run for an application whose type has no command
    /// of its own in its config (a Next.js application: `next start`).
    pub commands: Vec<(String, String)>,
    pub dry_run: bool,
    pub overwrite: bool,
    pub cutover: Option<Cutover>,
}

/// `warden migrate-wattpm ...` (its options differ from the other commands').
pub fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut o = Opts::default();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("migrate-wattpm: {a} needs a value"));
        match a.as_str() {
            "-c" | "--config" => o.config = Some(value()?),
            "-e" | "--env" => o.env_file = Some(value()?.into()),
            "--apps" | "--only" => {
                o.apps.extend(value()?.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from))
            }
            "--out" => o.out = Some(value()?.into()),
            "--prefix" => o.prefix = Some(value()?),
            "--command" => {
                let kv = value()?;
                match kv.split_once('=') {
                    Some((id, cmd)) if !id.trim().is_empty() && !cmd.trim().is_empty() => {
                        o.commands.push((id.trim().to_string(), cmd.trim().to_string()))
                    }
                    _ => {
                        return Err(format!(
                            "migrate-wattpm: --command {kv:?}: expected <application id>=<command line>"
                        ));
                    }
                }
            }
            "--dry-run" => o.dry_run = true,
            "--overwrite" => o.overwrite = true,
            "--cutover" => {
                let mode = parse_cutover(&value()?)?;
                if mode == Cutover::SamePort {
                    return Err("migrate-wattpm: --cutover same-port is not available: it would have to stop \
                                the Watt runtime (every application in it) and start it again if Warden's copy \
                                fails, with the command line and environment it had, which Warden cannot know. \
                                Use overlap, or new-port:<port>, then `wattpm stop` yourself"
                        .into());
                }
                o.cutover = Some(mode);
            }
            "-h" | "--help" => return Ok(Args { command: Command::Help, ..crate::cli::empty_args() }),
            s if s.starts_with('-') => return Err(format!("migrate-wattpm: unknown option {s}")),
            path => {
                if o.path.is_some() {
                    return Err("migrate-wattpm: takes one project directory or config file".into());
                }
                o.path = Some(path.into());
            }
        }
    }
    if o.dry_run && o.cutover.is_some() {
        return Err("migrate-wattpm: --dry-run changes nothing, so it can't be combined with --cutover".into());
    }
    Ok(Args { command: Command::WattpmMigrate(Box::new(o)), ..crate::cli::empty_args() })
}

// ------------------------------------------------------------------ notes

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// Carried over, with a difference.
    Approximated,
    /// Watt does this and Warden cannot.
    Unsupported,
    /// Carried over or left out in a way that needs a look.
    Check,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Approximated => "approximated",
            Kind::Unsupported => "unsupported",
            Kind::Check => "check",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Note {
    field: String,
    kind: Kind,
    text: String,
}

fn note(field: &str, kind: Kind, text: impl Into<String>) -> Note {
    Note { field: field.into(), kind, text: text.into() }
}

// ------------------------------------------------------------- JSON helpers

fn jstr(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

/// A number, or a string holding one (Watt coerces types when it validates a config).
fn jnum(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn jbool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s == "true" => Some(true),
        Value::String(s) if s == "false" => Some(false),
        _ => None,
    }
}

fn jstrings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

// ------------------------------------------------------- reading config files

/// File names Watt looks for, in its order: `watt.<ext>`, `platformatic.<ext>` for each format, then
/// the same with a suffix (`watt.runtime.json`, ...).
fn candidates() -> Vec<String> {
    const EXT: [&str; 6] = ["json", "json5", "yaml", "yml", "toml", "tml"];
    const SUFFIX: [&str; 6] = ["runtime", "service", "application", "db", "gateway", "composer"];
    let mut v = Vec::new();
    for e in EXT {
        v.push(format!("watt.{e}"));
        v.push(format!("platformatic.{e}"));
    }
    for s in SUFFIX {
        for e in EXT {
            v.push(format!("watt.{s}.{e}"));
            v.push(format!("platformatic.{s}.{e}"));
        }
    }
    v
}

fn find_config_file(dir: &Path) -> Option<String> {
    candidates().into_iter().find(|f| dir.join(f).is_file())
}

/// `//` and `/* */` comments and trailing commas out of a JSON5-ish text (strings are left alone).
/// Line breaks stay, so an error still names the right line.
fn strip_jsonc(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let c: Vec<char> = text.chars().collect();
    // Comments first, so that a comma followed by a comment and a closing brace is found below.
    let mut bare = String::with_capacity(text.len());
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            q @ ('"' | '\'') => {
                bare.push(q);
                i += 1;
                while i < c.len() {
                    bare.push(c[i]);
                    if c[i] == '\\' && i + 1 < c.len() {
                        bare.push(c[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if c[i - 1] == q {
                        break;
                    }
                }
            }
            '/' if c.get(i + 1) == Some(&'/') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if c.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                    if c[i] == '\n' {
                        bare.push('\n');
                    }
                    i += 1;
                }
                i += 2;
            }
            ch => {
                bare.push(ch);
                i += 1;
            }
        }
    }
    // A comma whose next non-blank character closes an object or an array.
    let c: Vec<char> = bare.chars().collect();
    let mut out = String::with_capacity(bare.len());
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            q @ ('"' | '\'') => {
                out.push(q);
                i += 1;
                while i < c.len() {
                    out.push(c[i]);
                    if c[i] == '\\' && i + 1 < c.len() {
                        out.push(c[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if c[i - 1] == q {
                        break;
                    }
                }
            }
            ',' => {
                let mut j = i + 1;
                while j < c.len() && c[j].is_whitespace() {
                    j += 1;
                }
                if !matches!(c.get(j), Some('}') | Some(']')) {
                    out.push(',');
                }
                i += 1;
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}

/// A config file as JSON, whatever its format. Watt's `.json` reader is strict (no comments).
fn read_config(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let v: Value = match ext.as_str() {
        "json" => serde_json::from_str(&text).map_err(|e| {
            let hint = if serde_json::from_str::<Value>(&strip_jsonc(&text)).is_ok() {
                " It has comments or a trailing comma, which Watt's own reader rejects in a .json file too: remove \
                 them, or rename the file to .json5 (Watt reads that with comments)."
            } else {
                ""
            };
            format!("{}: not valid JSON: {e}.{hint}", path.display())
        })?,
        "json5" => serde_json::from_str(&strip_jsonc(&text)).map_err(|e| {
            format!(
                "{}: {e}. migrate-wattpm reads JSON5 with comments and trailing commas; for other JSON5 syntax \
                 (unquoted keys, single-quoted strings) save the config as watt.json first",
                path.display()
            )
        })?,
        "toml" | "tml" => {
            let t: toml::Value =
                toml::from_str(&text).map_err(|e| format!("{}: not valid TOML: {}", path.display(), e.message()))?;
            serde_json::to_value(t).map_err(|e| format!("{}: {e}", path.display()))?
        }
        "yaml" | "yml" => {
            return Err(format!(
                "{}: YAML configs are not read by migrate-wattpm. Watt also reads JSON: save the same settings as \
                 watt.json (or watt.json5, or TOML) and run it again",
                path.display()
            ));
        }
        other => return Err(format!("{}: .{other} is not a config format Watt reads", path.display())),
    };
    if !v.is_object() {
        return Err(format!("{}: a Watt config is an object at the top level", path.display()));
    }
    Ok(v)
}

/// (module, version) a config names with `module` or a known `$schema` (Watt's rule).
fn module_of(cfg: &Value) -> Option<(String, Option<String>)> {
    if let Some(m) = cfg.get("module").and_then(jstr) {
        return Some((m.to_string(), None));
    }
    let s = cfg.get("$schema").and_then(jstr)?;
    let version = |seg: &str| seg.strip_suffix(".json").unwrap_or(seg).trim_start_matches('v').to_string();
    if let Some(rest) = s.strip_prefix("https://schemas.platformatic.dev/@platformatic/") {
        let (module, last) = rest.rsplit_once('/')?;
        return Some((format!("@platformatic/{module}"), Some(version(last))));
    }
    if let Some(rest) = s.strip_prefix("https://schemas.platformatic.dev/wattpm/") {
        return Some(("@platformatic/runtime".into(), Some(version(rest))));
    }
    if let Some(rest) = s.strip_prefix("https://platformatic.dev/schemas/") {
        let (v, module) = rest.split_once('/')?;
        let module = module.split(['?', '#']).next().unwrap_or(module);
        let module = if module == "wattpm" { "runtime" } else { module };
        return Some((format!("@platformatic/{module}"), Some(v.trim_start_matches('v').to_string())));
    }
    None
}

// ------------------------------------------------------- environment, placeholders

/// What `{NAME}` placeholders see: a `.env` file, then the environment of this command.
#[derive(Debug, Clone, Default)]
struct Vars {
    map: BTreeMap<String, String>,
    /// Names only the `.env` file has (the environment of this command does not).
    file_only: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
struct Placeholders {
    /// Names that got their value from the environment of this command, not from a file.
    from_process: BTreeSet<String>,
    /// Names with no value anywhere: Watt substitutes an empty string.
    unset: BTreeSet<String>,
    /// Every string that had a value substituted into it, by the string it became.
    strings: BTreeMap<String, Subst>,
}

/// A config string with a value from the environment or a `.env` file in it. Warden has no
/// placeholders, so the value would land in the config as plain text; this remembers what went in,
/// so that it is never printed, and what the string was, so that a shell command can read the value
/// from the env file instead.
#[derive(Debug, Clone, Default)]
struct Subst {
    /// The string as the config has it, placeholders and all.
    template: String,
    /// Every value that went into it (nested ones included) that could be a secret.
    values: Vec<String>,
    /// The placeholders of the template itself, in order.
    first: Vec<Filled>,
}

#[derive(Debug, Clone, Default)]
struct Filled {
    name: String,
    /// What Watt puts in its place.
    value: String,
    /// Could be a secret: it came from the environment or a `.env` file and is not a port-like number.
    secret: bool,
}

/// A value worth hiding: not empty, and not a number as short as a port or a count.
fn is_secret_value(v: &str) -> bool {
    let short_number = v.len() <= 5 && v.bytes().all(|b| b.is_ascii_digit());
    !v.is_empty() && !short_number
}

/// `text` with every one of `secrets` (as written, and as a TOML or JSON string would escape it)
/// replaced by `***`.
fn mask(text: &str, secrets: &BTreeSet<String>) -> String {
    let mut forms: Vec<String> = Vec::new();
    for v in secrets.iter().filter(|v| !v.is_empty()) {
        forms.push(v.clone());
        let quoted = toml_str(v);
        forms.push(quoted[1..quoted.len() - 1].to_string());
    }
    forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
    forms.dedup();
    let mut out = text.to_string();
    for f in forms.iter().filter(|f| !f.is_empty()) {
        out = out.replace(f.as_str(), "***");
    }
    out
}

fn holds_secret(text: &str, secrets: &BTreeSet<String>) -> bool {
    mask(text, secrets) != text
}

/// The `.env` Watt would read: the given one, else the nearest `.env` from `root` upwards.
fn find_dotenv(root: &Path, explicit: Option<&Path>) -> Result<Option<PathBuf>, String> {
    if let Some(p) = explicit {
        let p = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
        return if p.is_file() { Ok(Some(p)) } else { Err(format!("{}: no such env file", p.display())) };
    }
    let mut cur = root.to_path_buf();
    // Watt stops before the root of the file system.
    while cur.parent().is_some() {
        if cur.join(".env").is_file() {
            return Ok(Some(cur.join(".env")));
        }
        cur = cur.parent().map(Path::to_path_buf).unwrap_or_default();
    }
    Ok(std::env::current_dir().ok().map(|d| d.join(".env")).filter(|p| p.is_file()))
}

fn load_vars(root: &Path, explicit: Option<&Path>) -> Result<Vars, String> {
    let mut v = Vars::default();
    if let Some(f) = find_dotenv(root, explicit)? {
        let text = std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        v.map = parse_dotenv(&text);
    }
    let file_keys: BTreeSet<String> = v.map.keys().cloned().collect();
    for (k, val) in crate::migrate::env_utf8() {
        v.map.insert(k, val);
    }
    v.file_only = file_keys.into_iter().filter(|k| std::env::var_os(k).is_none()).collect();
    v.map.insert("PLT_ROOT".into(), root.display().to_string());
    Ok(v)
}

/// A `.env` file as Node's `util.parseEnv` reads it: `KEY=value`, `export`, `#` comments, single,
/// double and backtick quotes (which may span lines; `\n` in double quotes is a newline).
fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    let text = text.replace("\r\n", "\n");
    let mut out = BTreeMap::new();
    let mut rest: &str = &text;
    while !rest.is_empty() {
        let (line, after) = rest.split_once('\n').unwrap_or((rest, ""));
        rest = after;
        let l = line.trim_start();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("export ").map(str::trim_start).unwrap_or(l);
        let key_len = l.find(|c: char| !(c.is_ascii_alphanumeric() || "_.-".contains(c))).unwrap_or(l.len());
        if key_len == 0 {
            continue;
        }
        let key = &l[..key_len];
        let after_key = l[key_len..].trim_start();
        let value = match after_key.strip_prefix('=') {
            Some(v) => v,
            None => match after_key.strip_prefix(':') {
                Some(v) if v.starts_with(char::is_whitespace) => v,
                _ => continue,
            },
        };
        let v = value.trim_start();
        let value = match v.chars().next() {
            Some(q @ ('"' | '\'' | '`')) => {
                let mut body = String::new();
                let mut cur = v[1..].to_string();
                loop {
                    // The closing quote: for a double quote, the first one not escaped.
                    let b = cur.as_bytes();
                    let end = (0..b.len()).find(|&i| b[i] == q as u8 && !(q == '"' && i > 0 && b[i - 1] == b'\\'));
                    if let Some(end) = end {
                        body.push_str(&cur[..end]);
                        break;
                    }
                    body.push_str(&cur);
                    match rest.split_once('\n') {
                        Some((next, after)) => {
                            body.push('\n');
                            cur = next.to_string();
                            rest = after;
                        }
                        None if !rest.is_empty() => {
                            body.push('\n');
                            cur = rest.to_string();
                            rest = "";
                        }
                        None => break,
                    }
                }
                if q == '"' { body.replace("\\n", "\n") } else { body }
            }
            _ => v.split('#').next().unwrap_or("").trim().to_string(),
        };
        out.insert(key.to_string(), value);
    }
    out
}

/// The `{NAME}` or `{{NAME}}` Watt substitutes: (start, end, name).
fn find_template(s: &str) -> Option<(usize, usize, &str)> {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] != b'{' {
            continue;
        }
        // One or two opening braces (two first, as a greedy match backs off).
        for open in [2usize, 1] {
            if i + open > b.len() || !b[i..i + open].iter().all(|c| *c == b'{') {
                continue;
            }
            let start = i + open;
            let mut j = start;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j == start {
                continue;
            }
            let mut close = 0;
            while close < 2 && j + close < b.len() && b[j + close] == b'}' {
                close += 1;
            }
            if close > 0 {
                return Some((i, j + close, &s[start..j]));
            }
        }
    }
    None
}

/// Watt's substitution: the first placeholder is replaced, the result is scanned again (a value
/// that holds a placeholder is replaced too; a loop stops after 32 rounds). `values` collects what
/// came from the environment or a `.env` file.
fn expand(s: &str, vars: &Vars, ph: &mut Placeholders, values: &mut Vec<String>) -> String {
    let mut s = s.to_string();
    for _ in 0..32 {
        let Some((start, end, name)) = find_template(&s) else { break };
        let name = name.to_string();
        let value = match vars.map.get(&name) {
            Some(v) => {
                if name != "PLT_ROOT" {
                    if !vars.file_only.contains(&name) {
                        ph.from_process.insert(name.clone());
                    }
                    values.push(v.clone());
                }
                v.clone()
            }
            None => {
                ph.unset.insert(name.clone());
                String::new()
            }
        };
        s.replace_range(start..end, &value);
    }
    s
}

fn replace_str(s: &str, vars: &Vars, ph: &mut Placeholders) -> String {
    let mut values = Vec::new();
    let out = expand(s, vars, ph, &mut values);
    values.retain(|v| is_secret_value(v));
    if !values.is_empty() {
        values.sort();
        values.dedup();
        // The placeholders of the string itself, each as Watt fills it in.
        let mut first = Vec::new();
        let mut rest = s;
        while let Some((start, end, name)) = find_template(rest) {
            let value = expand(&rest[start..end], vars, &mut Placeholders::default(), &mut Vec::new());
            let secret = name != "PLT_ROOT" && vars.map.contains_key(name) && is_secret_value(&value);
            first.push(Filled { name: name.to_string(), value, secret });
            rest = &rest[end..];
        }
        ph.strings.insert(out.clone(), Subst { template: s.to_string(), values, first });
    }
    out
}

/// Every string value in a config, like Watt's `replaceEnv` (keys are left as they are).
fn replace_env(v: &mut Value, vars: &Vars, ph: &mut Placeholders) {
    match v {
        Value::String(s) => *s = replace_str(s, vars, ph),
        Value::Array(a) => a.iter_mut().for_each(|x| replace_env(x, vars, ph)),
        Value::Object(m) => m.values_mut().for_each(|x| replace_env(x, vars, ph)),
        _ => {}
    }
}

// ------------------------------------------------------------------ the project

/// One application as the runtime config lists it (or `autoload` finds it).
#[derive(Debug, Clone)]
struct Entry {
    id: String,
    /// Its directory; none for an external one that was not resolved.
    dir: Option<PathBuf>,
    /// Its own config file.
    config: Option<PathBuf>,
    /// The entry's keys: `workers`, `env`, `restartOnError`... (autoload mappings included).
    raw: Value,
    /// Where it came from: "applications", "services", "web", "autoload".
    via: &'static str,
}

#[derive(Debug)]
struct Project {
    config_path: PathBuf,
    root: PathBuf,
    /// `name` in the project's package.json, else the directory's name.
    name: String,
    version: Option<String>,
    /// A config of one application that Watt wraps in a runtime of its own.
    single: bool,
    /// The runtime's settings: the config itself, or the `runtime` object of a single application.
    runtime: Value,
    entries: Vec<Entry>,
    /// What the project's `.env` file holds.
    dotenv: BTreeMap<String, String>,
    /// What `{NAME}` placeholders see (the `.env` file, then the environment of this command).
    vars: Vars,
    entrypoint: Option<String>,
    notes: Vec<Note>,
    placeholders: Placeholders,
}

/// Watt's `applicationTypes` (3.71.0), in the order it detects them from package.json.
const TYPES: &[(&str, &str, &[&str])] = &[
    ("@platformatic/nest", "NestJS", &["@nestjs/core"]),
    ("@platformatic/next", "Next.js", &["next"]),
    ("@platformatic/remix", "Remix", &["@remix-run/dev"]),
    ("@platformatic/astro", "Astro", &["astro"]),
    ("@platformatic/react-router", "React Router", &["@react-router/dev"]),
    ("@platformatic/nuxt", "Nuxt", &["nuxt"]),
    ("@platformatic/tanstack", "TanStack Start", &["@tanstack/react-start"]),
    ("@platformatic/nitro", "Nitro", &["nitro", "nitropack"]),
    ("@platformatic/vite", "Vite", &["vite"]),
    ("@platformatic/gateway", "Platformatic Gateway", &["@platformatic/gateway", "@platformatic/composer"]),
    ("@platformatic/service", "Platformatic Service", &["@platformatic/service"]),
    ("@platformatic/db", "Platformatic DB", &["@platformatic/db"]),
    ("@platformatic/php", "Platformatic PHP", &["@platformatic/php"]),
    ("@platformatic/ai-warp", "AI-Warp", &["@platformatic/ai-warp"]),
    ("@platformatic/pg-hooks", "Platformatic PostgreSQL Hooks", &["@platformatic/pg-hooks"]),
    ("@platformatic/rabbitmq-hooks", "Platformatic RabbitMQ Hooks", &["@platformatic/rabbitmq-hooks"]),
    ("@platformatic/kafka-hooks", "Platformatic Kafka Hooks", &["@platformatic/kafka-hooks"]),
];

fn type_label(module: &str) -> String {
    match module {
        "@platformatic/node" => "Node.js".into(),
        "@platformatic/composer" => "Platformatic Gateway (composer)".into(),
        m => TYPES.iter().find(|t| t.0 == m).map(|t| t.1.to_string()).unwrap_or_else(|| m.to_string()),
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The type Watt detects for a directory with no config naming one.
fn detect_type(dir: &Path) -> String {
    let pkg = read_json(&dir.join("package.json")).unwrap_or(Value::Null);
    let has =
        |dep: &str| ["dependencies", "devDependencies"].iter().any(|k| pkg.get(k).and_then(|d| d.get(dep)).is_some());
    TYPES
        .iter()
        .find(|t| t.2.iter().any(|d| has(d)))
        .map(|t| t.0.to_string())
        .unwrap_or_else(|| "@platformatic/node".into())
}

/// Watt's rule for `enabled`, in production (`wattpm start`): a boolean, a string (anything but
/// "false"), or an object per environment.
fn is_enabled(raw: &Value) -> bool {
    match raw.get("enabled") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s != "false",
        Some(Value::Object(m)) => m.get("production").and_then(Value::as_bool).unwrap_or(true),
        Some(Value::Bool(b)) => *b,
        Some(_) => true,
    }
}

fn app_dir(root: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    let joined = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

fn project_name(root: &Path) -> String {
    read_json(&root.join("package.json"))
        .and_then(|p| p.get("name").and_then(jstr).map(String::from))
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "watt".into())
}

fn load_project(o: &Opts) -> Result<Project, String> {
    let given = o.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let (root, config_path) = if given.is_file() {
        let abs = std::fs::canonicalize(&given).map_err(|e| format!("{}: {e}", given.display()))?;
        (abs.parent().map(Path::to_path_buf).unwrap_or_default(), abs)
    } else if given.is_dir() {
        let root = std::fs::canonicalize(&given).map_err(|e| format!("{}: {e}", given.display()))?;
        let file = match &o.config {
            Some(c) => c.clone(),
            None => find_config_file(&root).ok_or_else(|| {
                format!(
                    "no Watt config in {}: looked for watt.json, platformatic.json (and .json5, .yaml, .toml, \
                     watt.runtime.json...). Give the project directory or the config file, or -c <name>",
                    root.display()
                )
            })?,
        };
        let path = root.join(&file);
        if !path.is_file() {
            return Err(format!("{}: no such file", path.display()));
        }
        (root, path)
    } else {
        return Err(format!("{}: no such file or directory", given.display()));
    };

    let mut config = read_config(&config_path)?;
    // `envfile` in the config names the env file the placeholders use, as `-e` does.
    let envfile = config.get("envfile").and_then(jstr).map(PathBuf::from).or_else(|| o.env_file.clone());
    let vars = load_vars(&root, envfile.as_deref())?;
    let dotenv = match find_dotenv(&root, envfile.as_deref())? {
        Some(f) => parse_dotenv(&std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?),
        None => BTreeMap::new(),
    };
    let mut ph = Placeholders::default();
    replace_env(&mut config, &vars, &mut ph);

    let module = module_of(&config);
    let version = module.as_ref().and_then(|m| m.1.clone());
    let mut notes = Vec::new();
    let has_apps = ["applications", "services", "web", "autoload"].iter().any(|k| config.get(*k).is_some());
    let is_runtime = match &module {
        Some((m, _)) => m == "@platformatic/runtime",
        None if has_apps => {
            notes.push(note(
                "$schema",
                Kind::Check,
                "the config names no `$schema` or `module`: Watt reads a file like that as one application's config, \
                 so it would not start this project; add \"$schema\": \"https://schemas.platformatic.dev/@platformatic/runtime/<version>.json\"",
            ));
            true
        }
        None => {
            return Err(format!(
                "{}: not a Watt config: it has no `applications`, `services` or `autoload`, and no `$schema` or \
                 `module` naming an application type",
                config_path.display()
            ));
        }
    };

    let name = project_name(&root);
    let mut p = Project {
        config_path: config_path.clone(),
        root: root.clone(),
        name,
        version,
        single: !is_runtime,
        runtime: Value::Null,
        entries: Vec::new(),
        dotenv,
        vars: vars.clone(),
        entrypoint: None,
        notes,
        placeholders: Placeholders::default(),
    };

    if is_runtime {
        p.runtime = config.clone();
        p.entries = runtime_entries(&root, &config)?;
        p.entrypoint = config.get("entrypoint").and_then(jstr).map(String::from);
    } else {
        // One application: Watt wraps it in a runtime named after its package.
        let mut runtime =
            config.get("runtime").cloned().filter(Value::is_object).unwrap_or_else(|| Value::Object(Map::new()));
        if let (Some(server), Some(m)) = (config.get("server").filter(|s| s.is_object()), runtime.as_object_mut()) {
            let mut kept = Map::new();
            for k in ["hostname", "port", "http2", "https"] {
                if let Some(x) = server.get(k) {
                    kept.insert(k.into(), x.clone());
                }
            }
            if !kept.is_empty() {
                m.insert("server".into(), Value::Object(kept));
            }
        }
        let raw =
            runtime.get("application").cloned().filter(Value::is_object).unwrap_or_else(|| Value::Object(Map::new()));
        let id = read_json(&root.join("package.json"))
            .and_then(|p| p.get("name").and_then(jstr).map(String::from))
            .map(|n| match n.strip_prefix('@').and_then(|r| r.split_once('/')) {
                Some((_, rest)) => rest.to_string(),
                None => n,
            })
            .unwrap_or_else(|| "main".into());
        p.entrypoint = Some(id.clone());
        p.entries =
            vec![Entry { id, dir: Some(root.clone()), config: Some(config_path.clone()), raw, via: "the project" }];
        p.runtime = runtime;
    }
    p.placeholders = ph;
    Ok(p)
}

/// The applications of a runtime config: `applications`, `services` and `web` (all three are read),
/// then those `autoload` finds in a directory, merged by id as Watt does.
fn runtime_entries(root: &Path, config: &Value) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = Vec::new();
    let external = config
        .get("resolvedApplicationsBasePath")
        .or_else(|| config.get("resolvedServicesBasePath"))
        .and_then(jstr)
        .unwrap_or("external")
        .to_string();
    for (key, via) in [("applications", "applications"), ("services", "services"), ("web", "web")] {
        let Some(list) = config.get(key) else { continue };
        let Some(list) = list.as_array() else {
            return Err(format!("`{key}` must be a list of applications"));
        };
        for (i, raw) in list.iter().enumerate() {
            let Some(id) = raw.get("id").and_then(jstr) else {
                return Err(format!("{key}[{i}]: an application needs an `id`"));
            };
            let dir = match raw.get("path").and_then(jstr) {
                Some(p) => Some(app_dir(root, p)),
                None if raw.get("url").is_some() => Some(root.join(&external).join(id))
                    .filter(|d| d.is_dir())
                    .map(|d| app_dir(root, &d.display().to_string())),
                None => None,
            };
            let cfg = match (raw.get("config").and_then(jstr), &dir) {
                (Some(c), Some(d)) => Some(d.join(c)),
                (None, Some(d)) => find_config_file(d).map(|f| d.join(f)),
                _ => None,
            };
            if let Some(prev) = entries.iter().find(|e| e.id == id) {
                return Err(format!("two applications have the id {id:?} ({} and {key})", prev.via));
            }
            entries.push(Entry { id: id.to_string(), dir, config: cfg, raw: raw.clone(), via });
        }
    }
    if let Some(al) = config.get("autoload").filter(|a| a.is_object()) {
        let Some(path) = al.get("path").and_then(jstr) else {
            return Err("`autoload` needs a `path`".into());
        };
        let base = if Path::new(path).is_absolute() { PathBuf::from(path) } else { root.join(path) };
        let exclude = jstrings(al.get("exclude"));
        let mappings = al.get("mappings").and_then(Value::as_object);
        let mut dirs: Vec<(String, PathBuf)> = std::fs::read_dir(&base)
            .map_err(|e| format!("autoload path {}: {e}", base.display()))?
            .filter_map(Result::ok)
            // Watt skips what is not a directory, symbolic links to one included.
            .filter(|d| d.file_type().is_ok_and(|t| t.is_dir()))
            .map(|d| (d.file_name().to_string_lossy().to_string(), d.path()))
            .collect();
        dirs.sort();
        for (name, path) in dirs {
            if exclude.contains(&name) {
                continue;
            }
            let mapping = mappings.and_then(|m| m.get(&name)).cloned().unwrap_or(Value::Null);
            let id = mapping.get("id").and_then(jstr).map(String::from).unwrap_or_else(|| name.clone());
            let real = std::fs::canonicalize(&path).unwrap_or(path.clone());
            let cfg = match mapping.get("config").and_then(jstr) {
                Some(c) => Some(real.join(c)),
                None => find_config_file(&real).map(|f| real.join(f)),
            };
            let mut raw = match mapping {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            raw.insert("id".into(), Value::String(id.clone()));
            match entries.iter_mut().find(|e| e.id == id) {
                Some(existing) => {
                    // The same id twice is one application only if both name the same directory.
                    if existing.dir.as_ref().is_some_and(|d| *d != real) {
                        return Err(format!(
                            "the application id {id:?} is listed with the path {} and autoloaded from {}: Watt \
                             refuses that, give one of them another id",
                            existing.dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default(),
                            real.display()
                        ));
                    }
                    if existing.dir.is_none() {
                        existing.dir = Some(real);
                    }
                    if existing.config.is_none() {
                        existing.config = cfg;
                    }
                    if let Some(m) = existing.raw.as_object_mut() {
                        for (k, v) in raw {
                            m.entry(k).or_insert(v);
                        }
                    }
                }
                None => {
                    entries.push(Entry { id, dir: Some(real), config: cfg, raw: Value::Object(raw), via: "autoload" })
                }
            }
        }
    }
    entries.retain(|e| is_enabled(&e.raw));
    Ok(entries)
}

// ------------------------------------------------------------------ one application

/// How an application is started.
#[derive(Debug, Clone, PartialEq)]
enum Run {
    /// A Node.js entry file: `node <file>`.
    Script(PathBuf),
    /// A command line: the application's `commands.production`, or `--command`.
    Command(String),
    /// A shell command that reads values of the env file as `$NAME` (the config holds no secret).
    Shell(String),
}

#[derive(Debug, Default)]
struct WattApp {
    id: String,
    dir: Option<PathBuf>,
    /// The type's module name (`@platformatic/node`).
    module: String,
    entrypoint: bool,
    run: Option<Run>,
    /// Node flags (preload, source maps, heap limit): only for a `node <file>` application.
    interpreter_args: Vec<String>,
    script_args: Vec<String>,
    port: Option<u16>,
    count: usize,
    env: BTreeMap<String, String>,
    /// Names the shared sources set that this application does not get (`PORT` of the entry point).
    dropped_env: Vec<String>,
    bad_env: Vec<String>,
    grace_ms: u64,
    autorestart: bool,
    restart_delay_ms: Option<u64>,
    ready_ms: Option<u64>,
    watch: bool,
    offset_ports: bool,
    prefer_local: bool,
    mapped: Vec<(String, String)>,
    notes: Vec<Note>,
    /// Why it is not converted.
    skip: Option<String>,
    /// The Warden app name, once generated.
    name: String,
    /// Values from the environment or a `.env` file that went into the command line or into text of
    /// the report: never printed, and the config that holds them is private.
    secrets: BTreeSet<String>,
    /// The placeholders those values filled.
    secret_names: BTreeSet<String>,
    /// How `application.commands.production` was written, when a placeholder was in it.
    cmd_sub: Option<Subst>,
    /// The other applications of this run that listen on the same port.
    port_clash: Vec<String>,
}

impl WattApp {
    /// A config string is about to reach the command line or the report: if a value went into it,
    /// remember not to print that value.
    fn taint(&mut self, ph: &Placeholders, s: &str) {
        if let Some(sub) = ph.strings.get(s) {
            self.secrets.extend(sub.values.iter().cloned());
            self.secret_names.extend(sub.first.iter().filter(|f| f.secret).map(|f| f.name.clone()));
        }
    }

    fn taint_all(&mut self, ph: &Placeholders, list: &[String]) {
        for s in list {
            self.taint(ph, s);
        }
    }

    /// `text` for the screen or the report: without the values from the environment that this
    /// application's command line and settings hold.
    fn hide(&self, text: &str) -> String {
        mask(text, &self.secrets)
    }

    fn map(&mut self, field: &str, to: impl Into<String>) {
        self.mapped.push((field.into(), to.into()));
    }
    fn note(&mut self, field: &str, kind: Kind, text: impl Into<String>) {
        self.notes.push(note(field, kind, text));
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Workers {
    count: usize,
    dynamic: bool,
}

/// Watt's `workers`: a number, a numeric string, or an object; an application inherits the
/// runtime's value for what it does not set.
fn workers_of(v: Option<&Value>, inherit: &Workers, what: &str) -> Result<Workers, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else { return Ok(inherit.clone()) };
    let positive = |x: &Value| {
        jnum(x).filter(|n| *n >= 1).ok_or_else(|| format!("{what}: workers must be a positive integer; received {x}"))
    };
    match v {
        Value::Object(m) => {
            let count = match (m.get("static"), m.get("minimum")) {
                (Some(s), _) => positive(s)? as usize,
                (None, Some(min)) => positive(min)? as usize,
                (None, None) => inherit.count,
            };
            Ok(Workers { count, dynamic: m.get("dynamic").and_then(jbool).unwrap_or(inherit.dynamic) })
        }
        other => Ok(Workers { count: positive(other)? as usize, dynamic: false }),
    }
}

/// Node's `--max-old-space-size` value (MB) for Watt's `health.maxHeapTotal`, the way Watt derives
/// its worker's old-generation limit from it: the total minus the young generation (128 MB by default).
fn heap_limit_mb(total: &Value, young: Option<&Value>) -> Option<u64> {
    let bytes = |v: &Value| -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => {
                let s = s.trim();
                let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
                let n: f64 = s[..split].trim().parse().ok()?;
                let f = match s[split..].trim().to_ascii_lowercase().as_str() {
                    "" | "b" => 1.0,
                    "kb" => 1024.0,
                    "mb" => 1024.0 * 1024.0,
                    "gb" => 1024.0 * 1024.0 * 1024.0,
                    "tb" => 1024.0f64.powi(4),
                    _ => return None,
                };
                Some(n * f)
            }
            _ => None,
        }
    };
    let total = bytes(total)?;
    let young = young.and_then(bytes).unwrap_or(134_217_728.0);
    let old = if young > 0.0 { total - young } else { total };
    (old >= 1024.0 * 1024.0).then(|| (old / (1024.0 * 1024.0)).floor() as u64)
}

/// The entry file of a Node.js application, in Watt's order: `node.main`, package.json's `main`
/// and `exports`, then `index`, `main`, `app`, `application`, `server`, `start`, `bundle`, `run`,
/// `entrypoint` with `.js`, `.mjs` or `.cjs`.
fn node_entry(dir: &Path, cfg: &Value) -> Result<PathBuf, String> {
    if let Some(m) = cfg.get("node").and_then(|n| n.get("main")).and_then(jstr) {
        return Ok(dir.join(m));
    }
    let pkg = read_json(&dir.join("package.json")).unwrap_or(Value::Null);
    let at = |path: &[&str]| -> Option<String> {
        let mut cur = &pkg;
        for k in path {
            cur = cur.get(*k)?;
        }
        cur.as_str().map(String::from)
    };
    let fields: [&[&str]; 10] = [
        &["main"],
        &["exports"],
        &["exports", "node"],
        &["exports", "import"],
        &["exports", "require"],
        &["exports", "default"],
        &["exports", ".", "node"],
        &["exports", ".", "import"],
        &["exports", ".", "require"],
        &["exports", ".", "default"],
    ];
    if let Some(f) = fields.iter().find_map(|f| at(f)) {
        return Ok(dir.join(f));
    }
    for base in ["index", "main", "app", "application", "server", "start", "bundle", "run", "entrypoint"] {
        for ext in ["js", "mjs", "cjs"] {
            let f = dir.join(format!("{base}.{ext}"));
            if f.is_file() {
                return Ok(f);
            }
        }
    }
    Err(format!(
        "no entry file: `node.main` is not set, package.json in {} has no `main` or `exports`, and there is no \
         index.js, server.js or similar",
        dir.display()
    ))
}

/// Watt's `parseCommandString` (execa's): split on spaces, a backslash keeps a space in a word.
fn split_command(cmd: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in cmd.trim().split(' ').filter(|t| !t.is_empty()) {
        match out.last_mut() {
            Some(prev) if prev.ends_with('\\') => {
                prev.pop();
                prev.push(' ');
                prev.push_str(tok);
            }
            _ => out.push(tok.to_string()),
        }
    }
    out
}

/// A file that starts with `#!` and names node: run it with `node <file>` and Warden's shim loads.
fn is_node_script(p: &Path) -> bool {
    use std::io::Read as _;
    let mut buf = [0u8; 128];
    let n = std::fs::File::open(p).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("#!") && head.lines().next().is_some_and(|l| l.contains("node"))
}

/// What a command line becomes: a program with arguments, or `sh -c` when it chains commands.
/// A program that is not on PATH is looked up in `node_modules/.bin` of the application and of the
/// project, as Watt does; one that is a Node script runs as `node <script>`.
fn launch_for_command(cmd: &str, dir: &Path, root: &Path, prefer_local: bool) -> (Launch, Vec<String>) {
    if cmd.contains("&&") || cmd.contains("||") || cmd.contains(';') {
        return (Launch::Shell(cmd.to_string()), Vec::new());
    }
    let mut tokens = split_command(cmd);
    if tokens.is_empty() {
        return (Launch::Shell(cmd.to_string()), Vec::new());
    }
    let exe = tokens.remove(0);
    if exe == "node" {
        return (Launch::Program(exe), tokens);
    }
    if prefer_local && !Path::new(&exe).is_absolute() {
        for base in [dir, root] {
            let p = base.join("node_modules").join(".bin").join(&exe);
            if p.exists() {
                let real = std::fs::canonicalize(&p).unwrap_or(p);
                if is_node_script(&real) {
                    let mut args = vec![real.display().to_string()];
                    args.extend(tokens);
                    return (Launch::Program("node".into()), args);
                }
                return (Launch::Program(real.display().to_string()), tokens);
            }
        }
    }
    (Launch::Program(exe), tokens)
}

/// An entry file that exports `create()` or `build()` and never calls `listen()`: Watt calls the
/// factory and listens for it, so `node <file>` would start nothing.
fn looks_like_factory(text: &str) -> bool {
    const FACTORY: [&str; 12] = [
        "export function create",
        "export async function create",
        "export function build",
        "export async function build",
        "export const create",
        "export const build",
        "export default function create",
        "export default async function create",
        "exports.create",
        "exports.build",
        "module.exports.create",
        "module.exports.build",
    ];
    FACTORY.iter().any(|f| text.contains(f)) && !text.contains(".listen(") && !text.contains(".listen (")
}

fn is_gateway(module: &str) -> bool {
    matches!(module, "@platformatic/gateway" | "@platformatic/composer")
}

/// Why no command can be derived for a type that has none in its config.
fn needs_command(module: &str) -> String {
    match module {
        "@platformatic/service"
        | "@platformatic/db"
        | "@platformatic/php"
        | "@platformatic/ai-warp"
        | "@platformatic/pg-hooks"
        | "@platformatic/rabbitmq-hooks"
        | "@platformatic/kafka-hooks" => format!(
            "{} is a Platformatic application that Watt starts through its own capability; there is no program \
             to run it as. Give the command with --command <id>=\"...\" if you have one",
            type_label(module)
        ),
        m => format!(
            "{} is started by Watt's {m} capability, which has no command in this config for Warden to run. Give \
             the framework's own production command with --command <id>=\"...\" (for Next.js, `next start`), or \
             set `application.commands.production` in the application's watt.json",
            type_label(m)
        ),
    }
}

/// Everything about one application, read the way Watt reads it.
fn convert(p: &Project, e: &Entry, o: &Opts, inherit: &Workers, ph: &mut Placeholders) -> WattApp {
    let mut a = WattApp {
        id: e.id.clone(),
        dir: e.dir.clone(),
        count: inherit.count,
        grace_ms: 10_000,
        autorestart: true,
        prefer_local: true,
        ..Default::default()
    };
    a.entrypoint = p.entrypoint.as_deref() == Some(e.id.as_str());
    let r = &p.runtime;

    // The application's own config (and so its type).
    let mut cfg = Value::Object(Map::new());
    let mut module: Option<String> = None;
    if let Some(path) = &e.config {
        if p.single {
            // The project's config is the application's config.
            match read_config(path) {
                Ok(mut c) => {
                    replace_env(&mut c, &p.vars, ph);
                    cfg = c;
                }
                Err(err) => a.skip = Some(err),
            }
        } else {
            match read_config(path) {
                Ok(mut c) => {
                    let base = path.parent().unwrap_or(Path::new("."));
                    match load_vars(base, None) {
                        Ok(vars) => replace_env(&mut c, &vars, ph),
                        Err(err) => a.skip = Some(err),
                    }
                    cfg = c;
                }
                Err(err) => a.skip = Some(err),
            }
        }
        module = module_of(&cfg).map(|m| m.0);
    } else if p.single {
        module = Some("@platformatic/node".into());
    }
    let module =
        module.unwrap_or_else(|| a.dir.as_deref().map(detect_type).unwrap_or_else(|| "@platformatic/node".into()));
    a.module = module.clone();
    if a.skip.is_some() {
        return a;
    }
    let Some(dir) = a.dir.clone() else {
        let url = e.raw.get("url").and_then(jstr).map(String::from);
        if let Some(u) = &url {
            // A token may be part of it (`https://{TOKEN}@host/repo.git`).
            a.taint(ph, u);
        }
        a.skip = Some(match url {
            Some(u) => format!(
                "it is external ({u}) and has not been resolved: run Watt's `resolve` on the project, then migrate \
                 again (its code is expected in {}/external/{}/)",
                p.root.display(),
                e.id
            ),
            None => "it has no `path`".into(),
        });
        return a;
    };
    if !dir.is_dir() {
        a.skip = Some(format!("{} is not a directory", dir.display()));
        return a;
    }

    let ecfg = |k: &str| e.raw.get(k).filter(|v| !v.is_null());
    let rcfg = |k: &str| r.get(k).filter(|v| !v.is_null());
    let capp = cfg.get("application").filter(|v| v.is_object()).cloned().unwrap_or(Value::Null);

    // Composition: nothing to run on its own.
    if is_gateway(&module) {
        let mut listed = Vec::new();
        for (section, key) in [
            ("gateway", "applications"),
            ("gateway", "services"),
            ("composer", "services"),
            ("composer", "applications"),
        ] {
            if let Some(Value::Array(list)) = cfg.get(section).and_then(|g| g.get(key)) {
                for item in list {
                    if let Some(id) = item.get("id").and_then(jstr) {
                        let prefix = item.get("proxy").and_then(|x| x.get("prefix")).and_then(jstr);
                        a.taint(ph, id);
                        if let Some(pre) = prefix {
                            a.taint(ph, pre);
                        }
                        listed.push(match prefix {
                            Some(pre) => format!("{id} ({pre})"),
                            None => id.to_string(),
                        });
                    }
                }
            }
        }
        a.skip = Some(format!(
            "it is a Gateway: it composes other applications behind one entry point (routing by path, merged \
             OpenAPI, requests to `<id>.plt.local` inside the process). Warden does not proxy or compose; put \
             nginx or Caddy in front of the applications, each on its own port{}",
            if listed.is_empty() { String::new() } else { format!(". It fronts: {}", listed.join(", ")) }
        ));
        return a;
    }

    // What to run.
    let override_cmd = o.commands.iter().find(|(id, _)| *id == e.id).map(|(_, c)| c.clone());
    let prod_cmd = capp.get("commands").and_then(|c| c.get("production")).and_then(jstr).map(String::from);
    a.prefer_local = capp.get("preferLocalCommands").and_then(jbool).unwrap_or(true);
    match (override_cmd, prod_cmd) {
        (Some(c), _) => {
            a.map("--command", format!("runs `{c}`"));
            a.run = Some(Run::Command(c));
        }
        (None, Some(c)) => {
            // Whether a value from the environment is in it is dealt with once the environment is known.
            a.cmd_sub = ph.strings.get(&c).cloned();
            a.map("application.commands.production", format!("runs `{c}`"));
            a.run = Some(Run::Command(c));
        }
        (None, None) if module == "@platformatic/node" => match node_entry(&dir, &cfg) {
            Ok(f) => {
                if let Some(m) = cfg.get("node").and_then(|n| n.get("main")).and_then(jstr) {
                    a.taint(ph, m);
                }
                a.map("entry", format!("node {}", f.display()));
                if std::fs::metadata(&f).is_ok_and(|m| m.len() < 2 << 20) {
                    if let Ok(text) = std::fs::read_to_string(&f) {
                        if looks_like_factory(&text) {
                            a.note(
                                "entry",
                                Kind::Check,
                                format!(
                                    "{} exports a `create()` or `build()` factory and never calls `listen()`: Watt \
                                     calls the factory and listens for it. Run as `node <file>` it starts no server; \
                                     add a `listen` call (a small server.js that does it) before you start it under \
                                     Warden",
                                    f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
                                ),
                            );
                        }
                    }
                }
                a.run = Some(Run::Script(f));
            }
            Err(err) => a.skip = Some(err),
        },
        (None, None) => a.skip = Some(needs_command(&module)),
    }
    if a.skip.is_some() {
        return a;
    }
    let scripted = matches!(a.run, Some(Run::Script(_)));
    if let Some(b) = capp.get("commands").and_then(|c| c.get("build")).and_then(jstr) {
        a.taint(ph, b);
        a.note(
            "application.commands.build",
            Kind::Check,
            format!(
                "`wattpm build` ran `{b}`; Warden does not build: run it before `warden start` and `warden reload`"
            ),
        );
    }
    if let Some(base) = capp.get("basePath").and_then(jstr) {
        a.taint(ph, base);
        a.note(
            "application.basePath",
            Kind::Unsupported,
            format!(
                "Watt serves the application under {base:?} and strips it from the paths it passes on; Warden does \
                 not touch requests, so the application gets the full path. Serve it under that prefix in the app, \
                 or strip it in your proxy"
            ),
        );
    }
    if capp.get("processSpawner").is_some() {
        a.note(
            "application.processSpawner",
            Kind::Unsupported,
            "a custom spawner for the command; Warden starts it itself",
        );
    }
    for k in ["dispatchViaHttp", "absoluteUrl"] {
        if cfg.get("node").and_then(|n| n.get(k)).and_then(jbool) == Some(true) {
            a.note(
                &format!("node.{k}"),
                Kind::Unsupported,
                "mesh setting (how other applications reach this one); there is no mesh in Warden",
            );
        }
    }
    let has_server = cfg.get("node").and_then(|n| n.get("hasServer")).and_then(jbool) != Some(false);

    // Workers.
    match workers_of(ecfg("workers"), inherit, &format!("application {:?}", e.id)) {
        Ok(w) => {
            a.count = w.count.min(1024);
            if w.count > 1024 {
                a.note("workers", Kind::Approximated, format!("{} workers; Warden runs at most 1024", w.count));
            }
            a.map("workers", format!("[workers] count = {}", a.count));
            if w.dynamic {
                a.note(
                    "workers.dynamic",
                    Kind::Unsupported,
                    format!(
                        "autoscaling between a minimum and a maximum by event-loop utilization: Warden runs a fixed \
                         count ({}); `warden scale {} <N>` changes it by hand",
                        a.count, e.id
                    ),
                );
            }
        }
        Err(err) => {
            a.skip = Some(err);
            return a;
        }
    }

    // Stop, restart, start timeouts.
    if let Some(ms) = rcfg("gracefulShutdown").and_then(|g| g.get("application")).and_then(jnum) {
        a.grace_ms = ms.clamp(1, MAX_SECONDS_MS);
        a.map("gracefulShutdown.application", format!("[shutdown] grace_period = {}", a.grace_ms.div_ceil(1000)));
        if ms > MAX_SECONDS_MS {
            a.note(
                "gracefulShutdown.application",
                Kind::Approximated,
                format!("{ms} ms is more than Warden accepts: grace_period is at most 3600 s"),
            );
        }
    } else {
        a.map("gracefulShutdown.application", "[shutdown] grace_period = 10 (Watt's default)");
    }
    match ecfg("restartOnError") {
        Some(v) => {
            let ms = match v {
                Value::Bool(true) => Some(5000),
                Value::Bool(false) => None,
                other => jnum(other).filter(|n| *n > 0),
            };
            match ms {
                None => {
                    a.autorestart = false;
                    a.map("restartOnError", "[restart] enabled = false");
                }
                Some(wanted) if wanted >= 10 => {
                    // backoff_max is 16 times the delay (at least 10 s) and may not pass an hour.
                    let ms = wanted.min(MAX_RESTART_DELAY_MS);
                    a.restart_delay_ms = Some(ms);
                    a.map("restartOnError", format!("[restart] backoff_initial = {ms}"));
                    a.note(
                        "restartOnError",
                        Kind::Approximated,
                        format!(
                            "Watt waits {wanted} ms before every restart; Warden restarts at once after the first crash \
                             and doubles the delay from {ms} ms per crash in a row (up to {} ms){}",
                            ms.saturating_mul(16).max(10_000),
                            if wanted > ms {
                                format!(". {wanted} ms is more than Warden accepts (backoff_max is at most 3600000 ms), so {ms} ms is used")
                            } else {
                                String::new()
                            }
                        ),
                    );
                }
                Some(_) => {
                    a.map("restartOnError", "restarts at once (Warden's first restart after a crash is immediate)")
                }
            }
        }
        None => a.map("restartOnError", "restarts at once (what `wattpm start` does by default)"),
    }
    a.note(
        "restart policy",
        Kind::Approximated,
        "Watt restarts a crashed worker for ever; Warden marks a worker FAILED after [restart] max_restarts (10) \
         crashes in restart_window (60 s) and retries it after failed_cooldown (300 s). Raise max_restarts if the \
         application crashes on purpose that often",
    );
    if let Some(ms) = rcfg("startTimeout").and_then(jnum) {
        if ms > 0 {
            a.ready_ms = Some(ms.min(MAX_SECONDS_MS));
            a.map(
                "startTimeout",
                format!("[workers] ready_timeout = {}", ms.min(MAX_SECONDS_MS).div_ceil(1000).max(1)),
            );
            if ms > MAX_SECONDS_MS {
                a.note(
                    "startTimeout",
                    Kind::Approximated,
                    format!("{ms} ms is more than Warden accepts: ready_timeout is at most 3600 s"),
                );
            }
        } else {
            a.note(
                "startTimeout",
                Kind::Unsupported,
                "0 turns Watt's start timeout off; Warden always has one ([workers] ready_timeout, 30 s)",
            );
        }
    }

    // Health: Watt's is event-loop and heap thresholds, not an HTTP path.
    let health = |k: &str| ecfg("health").and_then(|h| h.get(k)).or_else(|| rcfg("health").and_then(|h| h.get(k)));
    if let Some(total) = health("maxHeapTotal") {
        match heap_limit_mb(total, health("maxYoungGeneration")) {
            Some(mb) if scripted => {
                a.interpreter_args.push(format!("--max-old-space-size={mb}"));
                a.map("health.maxHeapTotal", format!("node --max-old-space-size={mb}"));
                a.note(
                    "health.maxHeapTotal",
                    Kind::Approximated,
                    "Watt caps each worker thread's V8 heap; the node flag caps each process's heap the same way (the \
                     value is Watt's total minus its young generation)",
                );
            }
            _ => a.note(
                "health.maxHeapTotal",
                Kind::Unsupported,
                "a heap limit for the worker thread; pass `--max-old-space-size` to the node command yourself",
            ),
        }
    }
    if [
        "maxELU",
        "maxHeapUsed",
        "maxEventLoopDelay",
        "maxEventLoopDelayP99",
        "maxUnhealthyChecks",
        "interval",
        "gracePeriod",
    ]
    .iter()
    .any(|k| health(k).is_some())
        || health("enabled").and_then(jbool) == Some(false)
    {
        a.note(
            "health",
            Kind::Unsupported,
            "Watt replaces a worker whose event-loop utilization or heap stays above a threshold. Warden has no such \
             check: its watchdog replaces a worker whose event loop stops answering for 60 s ([watchdog] timeout), \
             `[limits] max_memory` recycles one above a size, and `[health] path` runs an HTTP check you configure \
             (Watt has no HTTP health path to carry over)",
        );
    }

    // Flags for the node process (only when Warden runs `node <file>`).
    let mut node_flags = Vec::new();
    let exec_argv = jstrings(ecfg("execArgv"));
    a.taint_all(ph, &exec_argv);
    node_flags.extend(exec_argv);
    if let Some(n) = ecfg("nodeOptions").and_then(jstr) {
        a.taint(ph, n);
        node_flags.extend(n.split_whitespace().map(String::from));
    }
    let mut preload = jstrings(rcfg("preload"));
    preload.extend(jstrings(ecfg("preload")));
    a.taint_all(ph, &preload);
    for f in &preload {
        node_flags.push("--import".into());
        node_flags.push(app_dir(&p.root, f).display().to_string());
    }
    let source_maps = ecfg("sourceMaps").and_then(jbool).or_else(|| rcfg("sourceMaps").and_then(jbool)) == Some(true);
    if source_maps {
        node_flags.push("--enable-source-maps".into());
    }
    if !node_flags.is_empty() {
        if scripted {
            a.map("execArgv, nodeOptions, preload, sourceMaps", format!("node {}", node_flags.join(" ")));
            a.interpreter_args.extend(node_flags);
        } else if let Some(n) = ecfg("nodeOptions").and_then(jstr) {
            a.env.insert("NODE_OPTIONS".into(), n.to_string());
            a.map("nodeOptions", "NODE_OPTIONS in the app's env file");
            a.note(
                "execArgv, preload, sourceMaps",
                Kind::Unsupported,
                "these apply to the worker thread that hosts a command in Watt, not to the command; not carried over",
            );
        } else {
            a.note(
                "execArgv, preload, sourceMaps",
                Kind::Unsupported,
                "these apply to the worker thread that hosts a command in Watt, not to the command; not carried over",
            );
        }
    }
    if let Some(args) = ecfg("arguments") {
        let args = jstrings(Some(args));
        a.taint_all(ph, &args);
        if scripted {
            a.script_args = args.clone();
            a.map("arguments", format!("arguments after the script: {}", args.join(" ")));
        } else {
            a.note(
                "arguments",
                Kind::Unsupported,
                "they are the worker thread's argv in Watt; the command has its own",
            );
        }
    }

    // Watch.
    let watch = cfg
        .get("watch")
        .or_else(|| ecfg("watch"))
        .or_else(|| rcfg("watch"))
        .map(|w| match w {
            Value::Object(m) => m.get("enabled").and_then(jbool).unwrap_or(true),
            other => jbool(other).unwrap_or(false),
        })
        .unwrap_or(false);
    if watch {
        a.watch = true;
        a.map("watch", "[watch] enabled = true");
        a.note(
            "watch",
            Kind::Approximated,
            "Watt restarts the application inside its runtime when its files change; Warden does a gated rolling \
             restart (docs/watch.md). `allow` and `ignore` lists are not carried over: [watch] paths and ignore take their place",
        );
    }

    // Keys with nothing to map.
    if let Some(d) = ecfg("dependencies").filter(|d| d.as_array().is_some_and(|d| !d.is_empty())) {
        a.taint_all(ph, &jstrings(Some(d)));
        a.note(
            "dependencies",
            Kind::Unsupported,
            format!(
                "Watt starts {} before this application; Warden starts apps independently (`warden start a b` runs \
                 them together), so the application must wait for them or retry",
                jstrings(Some(d)).join(", ")
            ),
        );
    }
    if ecfg("permissions").is_some() {
        a.note(
            "permissions",
            Kind::Unsupported,
            "Node permission-model flags for the worker; add them to the node command if you need them",
        );
    }
    if ecfg("telemetry").is_some() || cfg.get("telemetry").is_some() {
        a.note(
            "telemetry",
            Kind::Unsupported,
            "Watt wires OpenTelemetry into the worker; configure the SDK in the application itself",
        );
    }
    if ecfg("management").is_some() {
        a.note(
            "management",
            Kind::Unsupported,
            "access to Watt's management API from the application; there is none in Warden",
        );
    }
    if ecfg("reuseTcpPorts").and_then(jbool) == Some(false) {
        a.note("reuseTcpPorts", Kind::Check, "Watt then stops the old worker before it starts the new one; Warden shares the port with SO_REUSEPORT whenever it can");
    }
    if ecfg("useHttp").and_then(jbool) == Some(true) || ecfg("websocket").and_then(jbool) == Some(true) {
        a.note("useHttp, websocket", Kind::Check, "Watt starts a private HTTP listener for it inside the runtime; Warden gives the application one port, below");
    }

    // Environment, then the port.
    collect_env(p, e, &dir, &mut a);
    shellify(&mut a, &dir, &p.root);
    // The listen port: the entry point's is `server.port`; an application's own PORT is its own.
    let server = rcfg("server");
    let from_env = a.env.get("PORT").and_then(|v| v.parse::<u16>().ok()).filter(|p| *p > 0);
    let entry_port =
        server.and_then(|s| s.get("port")).and_then(jnum).and_then(|n| u16::try_from(n).ok()).filter(|p| *p > 0);
    let own_port = capp.get("entrypointPort").and_then(jnum).and_then(|n| u16::try_from(n).ok()).filter(|p| *p > 0);
    if !has_server {
        a.map("node.hasServer", "no port: a background application");
    } else if a.entrypoint {
        a.port = entry_port.or(from_env).or(own_port);
        match (entry_port, a.port) {
            (Some(pt), _) => a.map("server.port", format!("port = {pt}")),
            (None, Some(pt)) => a.note(
                "server.port",
                Kind::Check,
                format!("the runtime sets no `server.port`; port = {pt} from the PORT in its environment"),
            ),
            (None, None) => a.note(
                "server.port",
                Kind::Check,
                "no `server.port` and no PORT in its environment: the application listens where its own code says, so \
                 no port is set and Warden judges it ready after min_uptime. Set `port` in the config",
            ),
        }
    } else {
        a.port = from_env.or(own_port);
        match a.port {
            Some(pt) => a.map("PORT", format!("port = {pt}")),
            None => a.note(
                "port",
                Kind::Check,
                "no port: in Watt it is reached only inside the runtime as `http://<id>.plt.local`. As its own \
                 process it needs a port of its own; set `port` in the config and point its callers at it",
            ),
        }
    }
    if a.entrypoint {
        if let Some(h) =
            server.and_then(|s| s.get("hostname")).and_then(jstr).filter(|h| !matches!(*h, "0.0.0.0" | "::" | "[::]"))
        {
            a.taint(ph, h);
            a.note(
                "server.hostname",
                Kind::Check,
                format!(
                    "Watt makes the entry point listen on {h:?}, whatever address the application asks for; Warden \
                     does not change what the application binds, which is usually every interface. Make the \
                     application bind {h:?} itself if the port must stay private"
                ),
            );
        }
        if server.and_then(|s| s.get("portAssignment")).and_then(jstr) == Some("perWorkerIncrement") {
            if a.port.is_some() && a.count > 1 {
                a.offset_ports = true;
                a.map(
                    "server.portAssignment",
                    "[workers] port_strategy = \"offset\" (worker i gets PORT = port + i - 1)",
                );
            } else {
                a.note(
                    "server.portAssignment",
                    Kind::Check,
                    "perWorkerIncrement: each worker has its own port; with one worker or no port it changes nothing",
                );
            }
        }
    }
    // Port sharing between workers is the shim's (node and bun commands only).
    let shim = match &a.run {
        Some(Run::Script(_)) => true,
        Some(Run::Command(c)) => {
            let (l, _) = launch_for_command(c, &dir, &p.root, a.prefer_local);
            match l {
                Launch::Program(x) => crate::config::is_node(&x) || crate::config::is_bun(&x),
                _ => false,
            }
        }
        Some(Run::Shell(_)) | None => false,
    };
    if a.count > 1 && a.port.is_some() && !shim && !a.offset_ports {
        a.note(
            "workers",
            Kind::Approximated,
            format!(
                "{} workers cannot share one port here: Warden shares it by loading its shim into `node` and `bun` \
                 commands, and this command is neither. One worker is configured; run more as separate apps on \
                 other ports behind your proxy",
                a.count
            ),
        );
        a.count = 1;
    }
    a
}

/// How far into a shell command text is: not in quotes, in single quotes, or in double quotes.
#[derive(Clone, Copy, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

/// The quoting state after `text`, which starts in `from`.
fn quote_after(mut q: Quote, text: &str) -> Quote {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        q = match (q, c) {
            (Quote::None, '\\') | (Quote::Double, '\\') => {
                chars.next();
                q
            }
            (Quote::None, '\'') => Quote::Single,
            (Quote::None, '"') => Quote::Double,
            (Quote::Single, '\'') | (Quote::Double, '"') => Quote::None,
            _ => q,
        };
    }
    q
}

/// A command with `{NAME}` placeholders as shell text: a value that could be a secret becomes a
/// reference to the variable `NAME` (quoted for where it stands), every other placeholder its value.
fn shell_text(sub: &Subst) -> Option<String> {
    let mut out = String::new();
    let mut rest = sub.template.as_str();
    let mut q = Quote::None;
    let mut fills = sub.first.iter();
    while let Some((start, end, _)) = find_template(rest) {
        let lit = &rest[..start];
        out += lit;
        q = quote_after(q, lit);
        let f = fills.next()?;
        if f.secret {
            out += &match q {
                Quote::None => format!("\"${{{}}}\"", f.name),
                Quote::Double => format!("${{{}}}", f.name),
                // Leave the single quotes for the reference and go back into them.
                Quote::Single => format!("'\"${{{}}}\"'", f.name),
            };
        } else {
            out += &f.value;
            q = quote_after(q, &f.value);
        }
        rest = &rest[end..];
    }
    out += rest;
    Some(out)
}

/// The command of an application has a value from the environment or a `.env` file in it. Warden has
/// no placeholders, so the value would land in the config as plain text. A shell command is written
/// to read it from the env file (mode 0600) instead; any other command keeps the value, and the
/// config that holds it is private and never printed.
fn shellify(a: &mut WattApp, dir: &Path, root: &Path) {
    let Some(Run::Command(c)) = a.run.clone() else { return };
    let Some(sub) = a.cmd_sub.take() else { return };
    a.secrets.extend(sub.values.iter().cloned());
    let secret: Vec<&Filled> = sub.first.iter().filter(|f| f.secret).collect();
    let names: Vec<String> = secret.iter().map(|f| f.name.clone()).collect();
    a.secret_names.extend(names.iter().cloned());
    let is_shell = matches!(launch_for_command(&c, dir, root, a.prefer_local).0, Launch::Shell(_));
    // The variable must hold exactly the value Watt put there: not be set to another one.
    let usable = secret
        .iter()
        .all(|f| crate::config::valid_env_name(&f.name) && a.env.get(&f.name).is_none_or(|v| *v == f.value));
    let text = if is_shell && usable { shell_text(&sub) } else { None };
    let Some(text) = text else { return };
    for f in secret {
        a.env.entry(f.name.clone()).or_insert_with(|| f.value.clone());
    }
    for m in a.mapped.iter_mut().filter(|m| m.0 == "application.commands.production") {
        m.1 = format!("runs `{text}` through `sh -c`");
    }
    a.note(
        "application.commands.production",
        Kind::Approximated,
        format!(
            "Watt fills {} in when it reads the config; Warden has no placeholders, so the command reads \
             {} from the environment and the value is in the app's env file (mode 0600), not in the config",
            names.iter().map(|n| format!("`{{{n}}}`")).collect::<Vec<_>>().join(", "),
            names.iter().map(|n| format!("`${n}`")).collect::<Vec<_>>().join(", "),
        ),
    );
    a.run = Some(Run::Shell(text));
}

/// The environment of an application, in Watt's order: the runtime's `.env`, then the application's
/// own `.env` (or `envfile`), then the runtime's `env`, then the application's `env`.
fn collect_env(p: &Project, e: &Entry, dir: &Path, a: &mut WattApp) {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut shared: BTreeSet<String> = BTreeSet::new();
    for (k, v) in &p.dotenv {
        env.insert(k.clone(), v.clone());
        shared.insert(k.clone());
    }
    let own_file = match e.raw.get("envfile").and_then(jstr) {
        Some(f) => Some(app_dir_file(&p.root, f)),
        None => Some(dir.join(".env")),
    };
    if let Some(f) = own_file.filter(|f| f.is_file() && !(*f == p.root.join(".env") && a.entrypoint && p.single)) {
        if let Ok(text) = std::fs::read_to_string(&f) {
            for (k, v) in parse_dotenv(&text) {
                shared.remove(&k);
                env.insert(k, v);
            }
        }
    }
    if let Some(m) = p.runtime.get("env").and_then(Value::as_object) {
        for (k, v) in m {
            env.insert(k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
            shared.insert(k.clone());
        }
    }
    if let Some(m) = e.raw.get("env").and_then(Value::as_object) {
        for (k, v) in m {
            env.insert(k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
            shared.remove(k);
        }
    }
    // NODE_OPTIONS set from `nodeOptions` above wins over a shared one.
    if let Some(n) = a.env.remove("NODE_OPTIONS") {
        env.insert("NODE_OPTIONS".into(), n);
    }
    // PORT is the entry point's: an internal application would bind it too and collide.
    if !a.entrypoint && shared.contains("PORT") {
        env.remove("PORT");
        a.dropped_env.push("PORT".into());
    }
    if !env.contains_key("NODE_ENV") {
        env.insert("NODE_ENV".into(), "production".into());
        a.map("NODE_ENV", "NODE_ENV=production in the env file (`wattpm start` sets it when it is not set)");
    }
    for (k, v) in env {
        if crate::config::valid_env_name(&k) {
            a.env.insert(k, v);
        } else {
            a.bad_env.push(k);
        }
    }
}

fn app_dir_file(root: &Path, f: &str) -> PathBuf {
    let p = Path::new(f);
    if p.is_absolute() { p.to_path_buf() } else { root.join(p) }
}

// ----------------------------------------------------------------- the project's own settings

/// Settings of the whole runtime that Warden has no use for, each said once.
fn project_notes(p: &Project) -> Vec<Note> {
    let mut v = p.notes.clone();
    let r = &p.runtime;
    let has = |k: &str| r.get(k).is_some_and(|x| !x.is_null());
    if let Some(u) = r.get("undici").filter(|u| u.is_object()) {
        let mods: Vec<String> = match u.get("interceptors") {
            Some(Value::Array(a)) => {
                a.iter().filter_map(|i| i.get("module").and_then(jstr).map(String::from)).collect()
            }
            Some(Value::Object(m)) => m
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(|i| i.get("module").and_then(jstr).map(String::from))
                .collect(),
            _ => Vec::new(),
        };
        let what = if mods.is_empty() { String::new() } else { format!(" ({})", mods.join(", ")) };
        v.push(note(
            "undici",
            Kind::Unsupported,
            format!(
                "Watt sets the HTTP client's global dispatcher in every worker, with these interceptors{what} and agent \
                 options. Warden leaves the application's HTTP client alone: set the dispatcher up in the application, or \
                 in a module you load with `--import` (add it to `args` in the config)"
            ),
        ));
    }
    if has("policies") {
        v.push(note("policies", Kind::Unsupported, "which application may call which over the mesh; with no mesh there is nothing to deny (network policy is yours: a firewall, or each app's own auth)"));
    }
    if has("httpCache") {
        v.push(note("httpCache", Kind::Unsupported, "a response cache shared by the applications, in the runtime's main thread; Warden has none (nginx or a CDN caches in front)"));
    }
    if has("scheduler") {
        v.push(note("scheduler", Kind::Unsupported, "HTTP cron jobs the runtime fires at an application's URL. `[restart] schedule` is a restart schedule, not a request one: use cron, a systemd timer, or a job in the application"));
    }
    if has("extensions") {
        v.push(note(
            "extensions",
            Kind::Unsupported,
            "code that runs in the runtime's main thread; there is no such thread in Warden",
        ));
    }
    if has("basePath") {
        v.push(note("basePath", Kind::Unsupported, "Watt serves the whole runtime under this path and strips it; Warden does not touch requests. Strip it in your proxy or serve under it in the app"));
    }
    for k in ["metrics", "healthProbes"] {
        if r.get(k).is_some_and(|x| !x.is_null() && x != &Value::Bool(false)) {
            v.push(note(
                k,
                Kind::Unsupported,
                "Watt's own server on the runtime (Prometheus metrics, readiness and liveness probes, default :9090). Warden's `[metrics] listen` serves its own supervisor and worker metrics; probe an app through its `port`",
            ));
        }
    }
    if has("managementApi") || has("management") {
        v.push(note("managementApi", Kind::Unsupported, "the control socket `wattpm ps`, `stop`, `logs` and `inject` talk to; Warden has its own (`warden list`, `logs`, `stop`)"));
    }
    if has("telemetry") {
        v.push(note(
            "telemetry",
            Kind::Unsupported,
            "OpenTelemetry set up by the runtime; configure the SDK in each application",
        ));
    }
    if has("verticalScaler") {
        v.push(note(
            "verticalScaler",
            Kind::Unsupported,
            "deprecated autoscaling; Warden runs fixed worker counts (`warden scale`)",
        ));
    }
    if let Some(s) = rcfg(r, "server") {
        if s.get("https").is_some() || s.get("http2").and_then(jbool) == Some(true) {
            v.push(note("server.https, http2", Kind::Unsupported, "Watt terminates TLS and HTTP/2 for the entry point; Warden does not: do it in the application or in nginx in front"));
        }
        if s.get("backlog").is_some() {
            v.push(note(
                "server.backlog",
                Kind::Check,
                "the listen backlog Watt set; the application's own `listen` decides it under Warden",
            ));
        }
    }
    if has("logger") {
        v.push(note("logger", Kind::Check, "Watt's pino settings (level, transport, redaction) configure its own logger and what it prints for the workers. Warden captures the application's stdout and stderr as they are (`warden logs`, `[logging]` files and rotation)"));
    }
    if rcfg(r, "restartOnError").is_some() {
        v.push(note("restartOnError", Kind::Check, "set on the runtime, `wattpm start` ignores it (it always restarts a crashed worker at once in production); only an application's own value is carried over"));
    }
    if rcfg(r, "reuseTcpPorts").and_then(jbool) == Some(false) {
        v.push(note("reuseTcpPorts", Kind::Check, "Watt then replaces the entry point's workers by stopping each before starting the next; Warden shares the port with SO_REUSEPORT"));
    }
    if rcfg(r, "workersRestartDelay").and_then(jnum).is_some_and(|n| n > 0) {
        v.push(note("workersRestartDelay", Kind::Check, "the pause between workers in `wattpm restart`; Warden's is `[reload] pause` (seconds, deploy only) and its health gates"));
    }
    for k in ["applicationTimeout", "messagingTimeout", "startupConcurrency"] {
        if has(k) {
            v.push(note(k, Kind::Unsupported, "mesh and messaging setting; there is no mesh in Warden"));
        }
    }
    if let Some(w) = r.get("workers").filter(|w| w.is_object()) {
        if ["total", "maxMemory", "cooldown", "gracePeriod", "scaleUpELU", "scaleDownELU"]
            .iter()
            .any(|k| w.get(*k).is_some())
        {
            v.push(note(
                "workers",
                Kind::Unsupported,
                "the autoscaler's limits (total, maxMemory, cooldown, ELU thresholds); Warden runs fixed counts",
            ));
        }
    }
    v
}

fn rcfg<'a>(r: &'a Value, k: &str) -> Option<&'a Value> {
    r.get(k).filter(|x| !x.is_null())
}

// ---------------------------------------------------------------------- generating

/// `s` as one line of a comment: no line break and no other control character.
fn one_line(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// A name that Warden cannot use because it is a directory (`.`, `..`), as it ends up after cleaning;
/// what to call the app instead.
fn directory_name(wanted: &str) -> Option<String> {
    let cleaned: String =
        wanted.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    let cleaned = cleaned.trim_matches('-');
    (!cleaned.is_empty() && cleaned.chars().all(|c| c == '.')).then(|| match cleaned.len() {
        1 => "app-dot".to_string(),
        2 => "app-dotdot".to_string(),
        n => format!("app-dots{n}"),
    })
}

/// What `Config::parse` said, without the lines of the config it quotes (a TOML syntax error shows the
/// line, and the line may hold a value from the environment) and with every known secret hidden.
fn describe_invalid(error: &str, secrets: &BTreeSet<String>) -> String {
    let kept: Vec<&str> = error
        .lines()
        .filter(|l| {
            let rest = l.trim_start().trim_start_matches(|c: char| c.is_ascii_digit());
            !(rest.starts_with('|') || rest.starts_with(" |"))
        })
        .collect();
    mask(&kept.join("\n"), secrets)
}

/// The Warden config (TOML) and env file for one application; `name` is filled in.
fn generate(a: &mut WattApp, p: &Project, o: &Opts, namespace: Option<&str>) -> Result<(String, String), String> {
    let Some(run) = a.run.clone() else { return Err("nothing to run".into()) };
    let dir = a.dir.clone().ok_or("no directory")?;
    let wanted = format!("{}{}", o.prefix.as_deref().map(|p| format!("{p}-")).unwrap_or_default(), a.id);
    let wanted = match directory_name(&wanted) {
        Some(new) => {
            a.note(
                "name",
                Kind::Approximated,
                format!(
                    "{wanted:?} names a directory, which Warden does not accept as an app name: the app is {new:?}"
                ),
            );
            new
        }
        None => wanted,
    };
    let mut so = StartOpts {
        name: Some(wanted),
        instances: Some(a.count.to_string()),
        port: a.port,
        namespace: namespace.map(String::from),
        cwd: Some(dir.clone()),
        autorestart: (!a.autorestart).then_some(false),
        kill_timeout_ms: Some(a.grace_ms),
        restart_delay_ms: a.restart_delay_ms,
        listen_timeout_ms: a.ready_ms,
        watch: a.watch,
        ..Default::default()
    };
    let launch = match &run {
        Run::Script(f) => {
            if !f.is_file() {
                return Err(format!(
                    "the entry file {} does not exist: Watt reads it after `wattpm build`, so build the application first",
                    f.display()
                ));
            }
            so.interpreter = Some("node".into());
            so.interpreter_args = a.interpreter_args.clone();
            so.script_args = a.script_args.clone();
            Launch::Script(f.clone())
        }
        Run::Command(c) => {
            let (l, args) = launch_for_command(c, &dir, &p.root, a.prefer_local);
            so.script_args = args;
            l
        }
        Run::Shell(c) => Launch::Shell(c.clone()),
    };
    let (name, text) = fleet::quick_config(&launch, &so)?;
    a.name = name.clone();
    let env_file = format!("{name}.env");
    // The header is written here, from the application and the project: the one `warden start` writes
    // names the command, which may be anything.
    let mut out = format!(
        "# Written by `warden migrate-wattpm` from the Watt application {:?} of {}. Every setting: warden.example.toml\n",
        a.id,
        one_line(&p.config_path.display().to_string())
    );
    let body = match text.split_once('\n') {
        Some((first, rest)) if first.starts_with("# Written by") => rest,
        _ => text.as_str(),
    };
    for line in body.lines() {
        out += line;
        out += "\n";
        if line.starts_with("working_directory = ") && !a.env.is_empty() {
            out += &format!("env_file = {}\n", toml_str(&env_file));
        }
        if line.starts_with("count = ") && a.offset_ports {
            out += "port_strategy = \"offset\"\n";
        }
    }
    // Never the config itself: it may hold a value from the environment.
    Config::parse(&out)
        .map_err(|e| format!("the generated config is invalid: {}", describe_invalid(&e, &a.secrets)))?;
    if holds_secret(&out, &a.secrets) {
        let names: Vec<String> = a.secret_names.iter().map(|n| format!("`{{{n}}}`")).collect();
        a.note(
            "placeholders",
            Kind::Check,
            format!(
                "{} filled in from the environment or a `.env` file, and the value is part of the command line in the \
                 config (Warden has no placeholders). The config is written with mode 0600; the value is shown as *** \
                 here and in --dry-run. If the program can read it from its environment, take it off the command \
                 line: the env file already holds the variables of your `.env`",
                if names.is_empty() {
                    "A placeholder was".to_string()
                } else {
                    format!("{} {}", names.join(", "), if names.len() == 1 { "was" } else { "were" })
                }
            ),
        );
    }
    let mut env = String::from("# Environment for this app (secrets live here, not in the config). Mode 0600.\n");
    for (k, v) in a.env.iter().filter(|(k, _)| crate::config::valid_env_name(k)) {
        env += &format!("{k}={}\n", env_quote(v));
    }
    Ok((out, env))
}

// ---------------------------------------------------------------------- report

fn cell(s: &str) -> String {
    s.replace('|', "\\|")
}

fn report(p: &Project, apps: &[WattApp], extra: &[Note]) -> String {
    let mut r = String::from("# Watt → Warden migration\n\n");
    r += &format!(
        "Source: {} ({}{}; {} application{}{})\n\n",
        p.config_path.display(),
        if p.single { "one application, wrapped in a runtime by Watt" } else { "Watt runtime config" },
        p.version.as_deref().map(|v| format!(", schema {v}")).unwrap_or_default(),
        apps.len(),
        if apps.len() == 1 { "" } else { "s" },
        p.entrypoint.as_deref().map(|e| format!(", entry point `{e}`")).unwrap_or_default(),
    );
    r += "| Watt application | Type | Result | Warden app | Port | Workers |\n|---|---|---|---|---|---|\n";
    for a in apps {
        let done = a.skip.is_none();
        r += &format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            cell(&a.id),
            cell(&type_label(&a.module)),
            if done { "converted" } else { "not converted" },
            if done { cell(&a.name) } else { "-".into() },
            if !done { "-".into() } else { a.port.map(|p| p.to_string()).unwrap_or_else(|| "none".into()) },
            if done { a.count.to_string() } else { "-".into() },
        );
    }
    r += "\n";
    for a in apps {
        if let Some(why) = &a.skip {
            r += &a.hide(&format!("## {} (not converted)\n\n- Why: {why}\n\n", a.id));
            continue;
        }
        let mut sec = format!("## {} → {}\n\n", a.id, a.name);
        sec += &format!(
            "- Role: {}\n",
            if a.entrypoint {
                "the entry point: Watt serves it on the runtime's public port".to_string()
            } else {
                "internal: Watt reaches it only inside the runtime, as `http://<id>.plt.local`".to_string()
            }
        );
        sec += &format!("- Type: {} ({})\n", type_label(&a.module), a.module);
        sec += &format!(
            "- Mode: processes ({} instance{}; {}, and Warden's worker mode is Bun only)\n",
            a.count,
            if a.count == 1 { "" } else { "s" },
            if matches!(a.run, Some(Run::Command(_) | Run::Shell(_))) {
                "Watt ran the command as a child process of the runtime"
            } else {
                "Watt ran them as worker threads of one process"
            }
        );
        if let Some(d) = &a.dir {
            sec += &format!("- Directory: {} (Watt does not change into it; Warden does)\n", d.display());
        }
        for (field, to) in &a.mapped {
            sec += &format!("- {field}: mapped: {to}\n");
        }
        if !a.env.is_empty() {
            let names: Vec<&str> = a.env.keys().map(String::as_str).collect();
            sec += &format!(
                "- Environment kept ({}): {} (values in {}.env, mode 0600)\n",
                names.len(),
                names.join(", "),
                a.name
            );
        }
        if !a.dropped_env.is_empty() {
            sec += &format!(
                "- Environment left out ({}): {}. PORT in a shared `.env` or `env` is the entry point's; this application would bind it too\n",
                a.dropped_env.len(),
                a.dropped_env.join(", ")
            );
        }
        if !a.bad_env.is_empty() {
            sec += &format!(
                "- Environment not carried over ({}): {} are not valid variable names (letters, digits and _, not starting with a digit), so an env file cannot hold them\n",
                a.bad_env.len(),
                a.bad_env.join(", ")
            );
        }
        for n in &a.notes {
            sec += &format!("- {}: {}: {}\n", n.field, n.kind.label(), n.text);
        }
        sec += "\n";
        r += &a.hide(&sec);
    }
    r += "## The runtime\n\n";
    let mut general = vec![
        note(
            "stop signal",
            Kind::Check,
            "Watt closes an application itself (its server's `close()`, an exported `close`); under Warden the worker gets SIGTERM, and for node and bun commands Warden's shim first drains connections. Clean up in a SIGTERM handler if the application relied on Watt calling it",
        ),
        note(
            "environment",
            Kind::Check,
            "in Watt a variable that is already set in the environment wins over the same one in a `.env` file; in Warden `env_file` wins over the supervisor's environment. Remove a name from the env file if you set it in the unit or the shell",
        ),
        note(
            "listen port",
            Kind::Check,
            "Watt replaces the port an entry point asks for with `server.port`; Warden passes its `port` as PORT, so the application must listen on PORT (or read its own config) for the port to match",
        ),
    ];
    if apps.len() > 1 {
        general.push(note(
            "mesh",
            Kind::Unsupported,
            "the applications call each other in the process as `http://<id>.plt.local`; there is no mesh in Warden. Any call like that must become the other application's address (its port above, or your proxy); `grep -r plt.local` finds them",
        ));
    }
    general.extend(extra.iter().cloned());
    if !p.placeholders.from_process.is_empty() {
        general.push(note(
            "placeholders",
            Kind::Check,
            format!(
                "these `{{NAME}}` placeholders were filled from the environment of this command, not from a file: {}. If wattpm runs with another environment (a unit, a container), check the values above",
                p.placeholders.from_process.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
        ));
    }
    if !p.placeholders.unset.is_empty() {
        general.push(note(
            "placeholders",
            Kind::Check,
            format!(
                "no value for {}: Watt substitutes an empty string, and so does this conversion",
                p.placeholders.unset.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
        ));
    }
    for n in &general {
        r += &format!("- {}: {}: {}\n", n.field, n.kind.label(), n.text);
    }
    r += &format!(
        "\nNext: `warden check -c <app>.toml` for each app, then start them with `warden start <app>` (or `warden migrate-wattpm --cutover overlap`, which starts them next to the running runtime). \
         When they serve, point your proxy at their ports if those changed and stop Watt: `wattpm stop {}` stops every application of the runtime. \
         Then `warden save` and `sudo warden startup` so they come back after a reboot, and remove wattpm's own service.\n",
        p.name
    );
    r
}

// -------------------------------------------------------------------------- run

pub async fn run(args: &Args, o: &Opts) -> i32 {
    match run_inner(args, o).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("warden: migrate-wattpm: {e}");
            1
        }
    }
}

/// Namespace for the apps of one project: its package name, so `warden restart <project>` is `wattpm restart`.
fn namespace_of(name: &str) -> Option<String> {
    let bare = name.strip_prefix('@').and_then(|r| r.split_once('/')).map(|(_, n)| n).unwrap_or(name);
    let ns: String =
        bare.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    let ns = ns.trim_matches('-').to_string();
    (!ns.is_empty() && ns != "all").then_some(ns)
}

/// The project, its applications, and for each converted one its config and env text.
type Converted = (Project, Vec<WattApp>, Vec<Option<(String, String)>>);

/// Read, convert and generate: the apps (with `skip` set for those that cannot run on their own),
/// and for each converted one its config and env text.
fn convert_all(o: &Opts) -> Result<Converted, String> {
    let mut p = load_project(o)?;
    if p.entries.is_empty() {
        return Err(format!("{} lists no enabled applications; nothing to migrate", p.config_path.display()));
    }
    if let Some(ep) = &p.entrypoint {
        if !p.entries.iter().any(|e| &e.id == ep) && !p.single {
            return Err(format!(
                "`entrypoint` is {ep:?}, which is not one of the applications ({})",
                p.entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
    } else if p.entries.len() == 1 {
        p.entrypoint = Some(p.entries[0].id.clone());
    }
    for want in o.apps.iter().chain(o.commands.iter().map(|(id, _)| id)) {
        if !p.entries.iter().any(|e| &e.id == want) {
            let have: Vec<&str> = p.entries.iter().map(|e| e.id.as_str()).collect();
            return Err(format!(
                "no Watt application with the id {want:?} in {}; there is: {}",
                p.config_path.display(),
                have.join(", ")
            ));
        }
    }
    let inherit = {
        let base = Workers { count: 1, dynamic: false };
        workers_of(p.runtime.get("workers"), &base, "the runtime")?
    };
    let namespace = if p.entries.len() > 1 { namespace_of(&p.name) } else { None };
    let mut ph = std::mem::take(&mut p.placeholders);
    let mut apps = Vec::new();
    let mut texts = Vec::new();
    let entries = p.entries.clone();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for e in &entries {
        if !o.apps.is_empty() && !o.apps.contains(&e.id) {
            continue;
        }
        let mut a = convert(&p, e, o, &inherit, &mut ph);
        let mut generated = None;
        if a.skip.is_none() {
            if let (Some(Cutover::NewPort(port)), true) = (o.cutover, a.entrypoint) {
                if let Some(was) = a.port.filter(|w| *w != port) {
                    a.map("--cutover", format!("port = {port} instead of {was}, so Warden's copy runs next to Watt's"));
                }
                a.port = Some(port);
            }
            match generate(&mut a, &p, o, namespace.as_deref()) {
                Ok(t) => {
                    if let Some(other) = names.insert(a.name.clone(), a.id.clone()) {
                        a.skip =
                            Some(format!("its Warden name {:?} is already taken by {other:?}; use --prefix", a.name));
                    } else {
                        generated = Some(t);
                    }
                }
                Err(err) => a.skip = Some(err),
            }
        }
        apps.push(a);
        texts.push(generated);
    }
    p.placeholders = ph;
    // Two applications on one port would both start (Warden shares a port with SO_REUSEPORT) and the
    // kernel would split the requests between two different programs.
    let ported: Vec<(String, u16)> =
        apps.iter().filter(|a| a.skip.is_none()).filter_map(|a| a.port.map(|pt| (a.id.clone(), pt))).collect();
    for a in apps.iter_mut().filter(|a| a.skip.is_none()) {
        let Some(pt) = a.port else { continue };
        let others: Vec<&str> =
            ported.iter().filter(|(id, q)| *q == pt && *id != a.id).map(|(id, _)| id.as_str()).collect();
        a.port_clash = others.iter().map(|id| id.to_string()).collect();
        if !others.is_empty() {
            a.note(
                "port",
                Kind::Check,
                format!(
                    "port {pt} is also the port of {}: Warden starts both and the kernel splits the requests between two \
                     different programs. Give each its own port",
                    others.join(", ")
                ),
            );
        }
    }
    Ok((p, apps, texts))
}

/// Why `path` may not be written: a link (writing through it would change the file it points to) or,
/// without `--overwrite`, anything that is there already.
fn blocked(path: &Path, overwrite: bool) -> Option<String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Some(format!(
            "{} is a symbolic link: not writing through it (remove it, or point --out at a real directory)",
            path.display()
        )),
        Ok(_) if !overwrite => Some(format!("{} exists; left as it is (--overwrite replaces it)", path.display())),
        _ => None,
    }
}

/// An application that was written, for the cutover.
struct Written {
    id: String,
    name: String,
    cfg: PathBuf,
    port: Option<u16>,
    /// The other applications of this run on the same port.
    clash: Vec<String>,
}

async fn run_inner(args: &Args, o: &Opts) -> Result<i32, String> {
    let (p, mut apps, texts) = convert_all(o)?;
    let out_dir = o.out.clone().unwrap_or_else(fleet::config_dir);
    let mut code = 0;
    let mut written: Vec<Written> = Vec::new();
    for (a, text) in apps.iter_mut().zip(texts) {
        let Some((toml, env)) = text else {
            eprintln!("warden: {}: not converted: {}", a.id, a.hide(a.skip.as_deref().unwrap_or("?")));
            code = 1;
            continue;
        };
        let cfg_path = out_dir.join(format!("{}.toml", a.name));
        let env_name = format!("{}.env", a.name);
        // A command line that holds a value from the environment: the config is as private as the env file.
        let private = holds_secret(&toml, &a.secrets);
        if o.dry_run {
            println!(
                "# ---- {}{}\n{}",
                cfg_path.display(),
                if private { " (the value is shown as ***; the file is written with mode 0600)" } else { "" },
                a.hide(&toml)
            );
            if !a.env.is_empty() {
                println!(
                    "# ---- {} (values not shown)\n{}",
                    out_dir.join(&env_name).display(),
                    a.env.keys().cloned().collect::<Vec<_>>().join("\n")
                );
            }
            continue;
        }
        // Nothing is written through a link, and nothing that is there is replaced without --overwrite
        // (the env file may hold what the app needs and nothing else has).
        let targets = [Some(cfg_path.clone()), Some(out_dir.join(&env_name)).filter(|_| !a.env.is_empty())];
        if let Some(why) = targets.iter().flatten().find_map(|t| blocked(t, o.overwrite)) {
            eprintln!("warden: {}: {why}", a.id);
            a.skip = Some(why);
            code = 1;
            continue;
        }
        let write = |path: &Path, text: &str, mode: u32| {
            if o.overwrite {
                crate::migrate::write_file(path, text, mode)
            } else {
                crate::migrate::write_new_file(path, text, mode)
            }
        };
        let done = (|| {
            if !a.env.is_empty() {
                write(&out_dir.join(&env_name), &env, 0o600)?;
            }
            write(&cfg_path, &toml, if private { 0o600 } else { 0o644 })
        })();
        if let Err(e) = done {
            eprintln!("warden: {}: {}", a.id, a.hide(&e));
            a.skip = Some(e);
            code = 1;
            continue;
        }
        println!(
            "{}: wrote {}{}{}",
            a.id,
            cfg_path.display(),
            if a.env.is_empty() { String::new() } else { format!(" and {env_name}") },
            if private { " (mode 0600: it holds a value from the environment)" } else { "" }
        );
        written.push(Written {
            id: a.id.clone(),
            name: a.name.clone(),
            cfg: cfg_path,
            port: a.port,
            clash: a.port_clash.clone(),
        });
    }
    let md = report(&p, &apps, &project_notes(&p));
    if o.dry_run {
        println!("# ---- {REPORT}\n{md}");
        return Ok(code);
    }
    let report_path = out_dir.join(REPORT);
    crate::migrate::write_file(&report_path, &md, 0o644)?;
    println!("report: {}", report_path.display());
    if let Some(mode) = o.cutover {
        if cutover(args, &written, mode, &p).await != 0 {
            code = 1;
        }
    }
    Ok(code)
}

/// How to name an application to `warden start`: by name in the config directory, else by its file.
fn start_handle(w: &Written) -> String {
    if w.cfg.parent() == Some(fleet::config_dir().as_path()) { w.name.clone() } else { w.cfg.display().to_string() }
}

/// Start Warden's copy next to the running runtime. Watt is never stopped here, and neither is
/// anything this run did not start.
async fn cutover(args: &Args, written: &[Written], mode: Cutover, p: &Project) -> i32 {
    let mut code = 0;
    let how = match mode {
        Cutover::NewPort(port) => format!("new-port:{port}"),
        _ => "overlap".to_string(),
    };
    // Two programs on one port would both start (Warden shares a port) and the kernel would split the
    // requests between them: none of them is started.
    let (clashing, todo): (Vec<&Written>, Vec<&Written>) = written.iter().partition(|w| !w.clash.is_empty());
    for w in &clashing {
        eprintln!(
            "warden: {}: not started: port {} is also the port of {}, and two different programs on one port would \
             split the requests between them. Give each its own port (`port` in the config{}), then `warden start {}`",
            w.id,
            w.port.map(|p| p.to_string()).unwrap_or_default(),
            w.clash.join(", "),
            if matches!(mode, Cutover::NewPort(_)) { ", or another --cutover new-port" } else { "" },
            start_handle(w),
        );
        code = 1;
    }
    let mut started: Vec<&Written> = Vec::new();
    let mut running = 0;
    let mut failed: Option<&Written> = None;
    for w in &todo {
        let app = fleet::app_from_config(&w.cfg);
        if app.socket.exists() && fleet::status_of(&app).await.is_ok() {
            // Its supervisor keeps the config it started with, whatever was written to the file since.
            println!(
                "{}: already running with its previous config; not started again. `warden reload {}` applies {}",
                w.name,
                w.name,
                w.cfg.display()
            );
            running += 1;
            continue;
        }
        println!("{}: starting next to Watt ({how})", w.name);
        if warden_start(args, &w.cfg).await != 0 {
            warden_stop(args, &w.name).await;
            failed = Some(w);
            break;
        }
        started.push(w);
    }
    if let Some(bad) = failed {
        // All or nothing: what this run started next to Watt is stopped again.
        for w in started.iter().rev() {
            warden_stop(args, &w.name).await;
        }
        let names: Vec<&str> = started.iter().map(|w| w.name.as_str()).collect();
        eprintln!(
            "warden: {}: Warden's workers did not come up; stopped them. {}Watt was not touched. Fix it (the configs \
             are written), then `warden start {}`",
            bad.name,
            if names.is_empty() {
                String::new()
            } else {
                format!(
                    "Stopped {} too, which this run had started: nothing of this run is left running. ",
                    names.join(", ")
                )
            },
            start_handle(bad),
        );
        return 1;
    }
    if !started.is_empty() {
        println!(
            "Warden serves {} app(s) next to the running Watt runtime ({}); wattpm was not touched. `warden stop {}` \
             stops them. When they answer as they should, switch your proxy if the port changed and stop Watt \
             yourself: `wattpm stop {}` stops every application of the runtime. Applications without a port run \
             twice until then.",
            started.len(),
            started.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(", "),
            started.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(" "),
            p.name
        );
    }
    if !clashing.is_empty() {
        eprintln!(
            "warden: not started: {} (the reason is above){}",
            clashing.iter().map(|w| w.id.as_str()).collect::<Vec<_>>().join(", "),
            if started.is_empty() && running == 0 { String::new() } else { "; the others were started".to_string() }
        );
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A scratch project directory, removed on drop.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!("warden-wattpm-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Tmp(std::fs::canonicalize(&d).unwrap())
        }
        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
            p
        }
        fn json(&self, rel: &str, v: Value) -> PathBuf {
            self.write(rel, &serde_json::to_string_pretty(&v).unwrap())
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const RUNTIME: &str = "https://schemas.platformatic.dev/@platformatic/runtime/3.71.0.json";
    const NODE: &str = "https://schemas.platformatic.dev/@platformatic/node/3.71.0.json";

    fn opts(t: &Tmp) -> Opts {
        Opts { path: Some(t.0.clone()), ..Default::default() }
    }

    /// An application directory with a package.json and an entry file that listens.
    fn node_app(t: &Tmp, dir: &str, main: &str) {
        t.json(&format!("{dir}/package.json"), json!({"name": dir.rsplit('/').next().unwrap(), "main": main}));
        t.write(
            &format!("{dir}/{main}"),
            "require('node:http').createServer((q, s) => s.end('ok')).listen(process.env.PORT);\n",
        );
        t.json(&format!("{dir}/watt.json"), json!({"$schema": NODE}));
    }

    fn convert_ok(t: &Tmp) -> Converted {
        convert_all(&opts(t)).unwrap()
    }

    fn app<'a>(apps: &'a [WattApp], id: &str) -> &'a WattApp {
        apps.iter().find(|a| a.id == id).unwrap_or_else(|| panic!("no app {id}"))
    }

    fn has_note(a: &WattApp, field: &str, kind: Kind) -> bool {
        a.notes.iter().any(|n| n.field == field && n.kind == kind)
    }

    #[test]
    fn a_single_application_is_wrapped_like_watt_does() {
        let t = Tmp::new("single");
        t.json("package.json", json!({"name": "@acme/shop", "main": "dist/server.js"}));
        t.write("dist/server.js", "require('http').createServer().listen(process.env.PORT)\n");
        t.json(
            "watt.json",
            json!({"$schema": NODE, "server": {"hostname": "127.0.0.1", "port": 4100},
                   "runtime": {"workers": 3, "gracefulShutdown": {"application": 4500}, "env": {"FEATURE": "on"}}}),
        );
        let (p, apps, texts) = convert_ok(&t);
        assert!(p.single);
        assert_eq!(apps.len(), 1);
        let a = &apps[0];
        assert_eq!((a.id.as_str(), a.entrypoint, a.port, a.count), ("shop", true, Some(4100), 3));
        let (toml, env) = texts[0].clone().unwrap();
        let c = Config::parse(&toml).unwrap();
        assert_eq!(
            (c.app.name.as_str(), c.app.command.as_str(), c.workers.count, c.app.port),
            ("shop", "node", 3, Some(4100))
        );
        assert_eq!(c.shutdown.grace_period, 5, "4500 ms rounds up to whole seconds");
        assert!(c.app.args.last().unwrap().ends_with("dist/server.js"), "{:?}", c.app.args);
        assert_eq!(c.app.working_directory.as_deref(), Some(t.0.as_path()));
        assert!(env.contains("FEATURE=on") && env.contains("NODE_ENV=production"), "{env}");
        assert!(has_note(a, "server.hostname", Kind::Check), "{:?}", a.notes);
    }

    #[test]
    fn applications_services_and_web_are_all_read() {
        let t = Tmp::new("keys");
        node_app(&t, "a", "index.js");
        node_app(&t, "b", "index.js");
        node_app(&t, "c", "index.js");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "a",
                   "applications": [{"id": "a", "path": "./a"}],
                   "services": [{"id": "b", "path": "./b"}],
                   "web": [{"id": "c", "path": "./c"}]}),
        );
        let (_, apps, _) = convert_ok(&t);
        let ids: Vec<&str> = apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        // The same id in two lists is refused, as Watt does.
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "applications": [{"id": "a", "path": "./a"}], "services": [{"id": "a", "path": "./b"}]}),
        );
        assert!(convert_all(&opts(&t)).unwrap_err().contains("two applications"));
    }

    #[test]
    fn autoload_finds_directories_and_honors_exclude_and_mappings() {
        let t = Tmp::new("autoload");
        node_app(&t, "services/users", "index.js");
        node_app(&t, "services/billing", "index.js");
        node_app(&t, "services/legacy", "index.js");
        node_app(&t, "services/off", "index.js");
        t.write("services/README.md", "not a directory\n");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "api", "workers": 2,
                   "autoload": {"path": "services", "exclude": ["legacy"],
                                "mappings": {"users": {"id": "api", "workers": {"static": 5}},
                                             "off": {"id": "off", "enabled": false}}}}),
        );
        let (_, apps, _) = convert_ok(&t);
        let mut ids: Vec<&str> = apps.iter().map(|a| a.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["api", "billing"], "legacy excluded, off disabled, users renamed");
        assert_eq!(app(&apps, "api").count, 5, "the mapping's workers");
        assert_eq!(app(&apps, "billing").count, 2, "the runtime's workers are inherited");
        assert!(app(&apps, "api").entrypoint && !app(&apps, "billing").entrypoint);
        // An explicit entry and an autoloaded directory with the same id are one application...
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "users",
                   "applications": [{"id": "users", "path": "./services/users", "workers": 7}],
                   "autoload": {"path": "services", "exclude": ["legacy", "off", "billing"]}}),
        );
        let (_, apps, _) = convert_ok(&t);
        assert_eq!((apps.len(), app(&apps, "users").count), (1, 7), "the explicit entry wins");
        // ...but not when they name different directories.
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "applications": [{"id": "users", "path": "./services/billing"}],
                   "autoload": {"path": "services"}}),
        );
        assert!(convert_all(&opts(&t)).unwrap_err().contains("refuses"), "id collision");
    }

    #[test]
    fn enabled_follows_watts_rule_for_production() {
        for (v, on) in [
            (json!(false), false),
            (json!("false"), false),
            (json!("no"), true),
            (json!(true), true),
            (json!({"production": false}), false),
            (json!({"development": false}), true),
        ] {
            assert_eq!(is_enabled(&json!({"enabled": v.clone()})), on, "{v}");
        }
        assert!(is_enabled(&json!({})));
    }

    #[test]
    fn ports_come_from_the_server_the_env_or_a_placeholder() {
        let t = Tmp::new("ports");
        node_app(&t, "web", "index.js");
        node_app(&t, "jobs", "index.js");
        let base = |server: Value| {
            json!({"$schema": RUNTIME, "entrypoint": "web", "server": server,
                   "applications": [{"id": "web", "path": "web"}, {"id": "jobs", "path": "jobs"}]})
        };
        // A number, a numeric string, and a placeholder filled from the project's .env.
        t.json("watt.json", base(json!({"port": 4201})));
        assert_eq!(app(&convert_ok(&t).1, "web").port, Some(4201));
        t.json("watt.json", base(json!({"port": "4202"})));
        assert_eq!(app(&convert_ok(&t).1, "web").port, Some(4202));
        t.json("watt.json", base(json!({"port": "{WATTPM_TEST_PORT}"})));
        t.write(".env", "WATTPM_TEST_PORT=4203\nSHARED=1\nPORT=9999\n");
        let (p, apps, texts) = convert_ok(&t);
        assert_eq!(app(&apps, "web").port, Some(4203));
        assert!(p.placeholders.unset.is_empty(), "{:?}", p.placeholders);
        assert!(p.placeholders.from_process.is_empty(), "filled from the file, not the environment");
        // PORT in the shared .env is the entry point's: the internal application does not get it.
        let jobs = app(&apps, "jobs");
        assert_eq!(jobs.port, None);
        assert!(!jobs.env.contains_key("PORT") && jobs.dropped_env == ["PORT"], "{:?}", jobs.env);
        assert!(has_note(jobs, "port", Kind::Check));
        let (toml, _) = texts[0].clone().unwrap();
        assert!(toml.contains("port = 4203"), "{toml}");
        // No name anywhere: empty, and said.
        t.write(".env", "");
        t.json("watt.json", base(json!({"port": "{WATTPM_TEST_UNSET_PORT}"})));
        let (p, apps, _) = convert_ok(&t);
        assert_eq!(app(&apps, "web").port, None);
        assert!(p.placeholders.unset.contains("WATTPM_TEST_UNSET_PORT"), "{:?}", p.placeholders);
        assert!(has_note(app(&apps, "web"), "server.port", Kind::Check));
        // An internal application's own PORT is its port.
        t.json("jobs/watt.json", json!({"$schema": NODE}));
        t.write("jobs/.env", "PORT=4300\n");
        t.json("watt.json", base(json!({"port": 4201})));
        assert_eq!(app(&convert_ok(&t).1, "jobs").port, Some(4300));
    }

    #[test]
    fn placeholders_follow_watts_syntax() {
        let vars = Vars {
            map: [("A", "1"), ("B_2", "two"), ("NEST", "{A}")].map(|(k, v)| (k.to_string(), v.to_string())).into(),
            file_only: ["A".to_string(), "B_2".to_string(), "NEST".to_string()].into(),
        };
        let mut ph = Placeholders::default();
        assert_eq!(replace_str("x{A}y{{B_2}}z", &vars, &mut ph), "x1ytwoz");
        assert_eq!(replace_str("{NEST}", &vars, &mut ph), "1", "a value that holds a placeholder is replaced again");
        assert_eq!(replace_str("{MISSING}!", &vars, &mut ph), "!");
        assert_eq!(replace_str("{ not one }", &vars, &mut ph), "{ not one }");
        assert_eq!(ph.unset.iter().collect::<Vec<_>>(), ["MISSING"]);
        let mut v = json!({"a": ["{A}", {"b": "{A}"}], "n": 3});
        replace_env(&mut v, &vars, &mut ph);
        assert_eq!(v, json!({"a": ["1", {"b": "1"}], "n": 3}));
        // A loop ends.
        let looped = Vars { map: [("L".to_string(), "{L}".to_string())].into(), file_only: BTreeSet::new() };
        let _ = replace_str("{L}", &looped, &mut ph);
        assert!(ph.from_process.contains("L"));
    }

    #[test]
    fn env_files_and_blocks_merge_in_watts_order() {
        let t = Tmp::new("env");
        node_app(&t, "web", "index.js");
        t.write(
            ".env",
            "FROM_ROOT=root\nOVERRIDDEN=root\nSECRET=\"two words\"\nbad-name=1\n# comment\nexport EXPORTED=yes\n",
        );
        t.write("web/.env", "OVERRIDDEN=app-file\nFROM_APP_FILE=1\nMULTI=\"a\\nb\"\n");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "env": {"RUNTIME_ENV": "r", "OVERRIDDEN": "runtime-env"},
                   "applications": [{"id": "web", "path": "web", "env": {"APP_ENV": "a", "RUNTIME_ENV": "app-wins"}}]}),
        );
        let (_, apps, texts) = convert_ok(&t);
        let a = &apps[0];
        let e = &a.env;
        assert_eq!(e["FROM_ROOT"], "root");
        assert_eq!(e["FROM_APP_FILE"], "1");
        assert_eq!(e["OVERRIDDEN"], "runtime-env", "the runtime's `env` beats both files");
        assert_eq!(e["RUNTIME_ENV"], "app-wins", "the application's `env` beats the runtime's");
        assert_eq!((e["SECRET"].as_str(), e["EXPORTED"].as_str(), e["MULTI"].as_str()), ("two words", "yes", "a\nb"));
        assert_eq!(e["NODE_ENV"], "production");
        assert_eq!(a.bad_env, ["bad-name"], "a name an env file cannot hold is left out and listed");
        // The env file Warden writes parses back to the same values, and the config holds none.
        let (toml, env) = texts[0].clone().unwrap();
        let parsed = crate::config::parse_env_file(&env).expect("Warden must be able to read it");
        assert_eq!(parsed["SECRET"], "two words");
        assert_eq!(parsed["MULTI"], "a\nb");
        assert!(!toml.contains("two words") && toml.contains("env_file = \"web.env\""), "{toml}");
        // `envfile` names the application's file; `NODE_ENV` already set is kept.
        t.write("conf/web.env", "NODE_ENV=staging\nONLY=here\n");
        t.write("web/.env", "IGNORED=1\n");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "applications": [{"id": "web", "path": "web", "envfile": "conf/web.env"}]}),
        );
        let a = &convert_ok(&t).1[0];
        assert_eq!((a.env["NODE_ENV"].as_str(), a.env["ONLY"].as_str()), ("staging", "here"));
        assert!(!a.env.contains_key("IGNORED"));
    }

    #[test]
    fn dotenv_parsing_matches_node() {
        let m = parse_dotenv(
            "A=1\nB = spaced  # inline\nC='single # not a comment'\nD=\"multi\nline\"\nE=`tick`\nexport F=g\n\n# x\nG:\th\nbad line\r\nH=\"q\\\"uote\"\n",
        );
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "spaced");
        assert_eq!(m["C"], "single # not a comment");
        assert_eq!(m["D"], "multi\nline");
        assert_eq!(m["E"], "tick");
        assert_eq!(m["F"], "g");
        assert_eq!(m["G"], "h");
        assert_eq!(m["H"], "q\\\"uote");
        assert!(!m.contains_key("bad"));
    }

    #[test]
    fn workers_inherit_and_scale_notes() {
        let base = Workers { count: 1, dynamic: false };
        let rt = workers_of(Some(&json!(4)), &base, "r").unwrap();
        assert_eq!(rt, Workers { count: 4, dynamic: false });
        assert_eq!(workers_of(None, &rt, "a").unwrap().count, 4);
        assert_eq!(workers_of(Some(&json!("6")), &rt, "a").unwrap().count, 6);
        assert_eq!(workers_of(Some(&json!({"static": 2})), &rt, "a").unwrap().count, 2);
        assert_eq!(
            workers_of(Some(&json!({"minimum": 3, "maximum": 8, "dynamic": true})), &rt, "a").unwrap(),
            Workers { count: 3, dynamic: true }
        );
        assert!(workers_of(Some(&json!(0)), &rt, "web").unwrap_err().contains("positive integer"));
        assert!(workers_of(Some(&json!("many")), &rt, "web").is_err());

        let t = Tmp::new("workers");
        node_app(&t, "web", "index.js");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "workers": {"static": 2, "dynamic": true, "total": 8},
                   "applications": [{"id": "web", "path": "web"}]}),
        );
        let (p, apps, _) = convert_ok(&t);
        assert_eq!(apps[0].count, 2);
        assert!(has_note(&apps[0], "workers.dynamic", Kind::Unsupported));
        assert!(project_notes(&p).iter().any(|n| n.field == "workers" && n.kind == Kind::Unsupported));
    }

    #[test]
    fn restart_stop_and_start_settings_map_with_their_differences() {
        let t = Tmp::new("policy");
        node_app(&t, "a", "index.js");
        node_app(&t, "b", "index.js");
        node_app(&t, "c", "index.js");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "startTimeout": 45000, "restartOnError": false, "gracefulShutdown": {"application": 20000, "runtime": 60000},
                   "applications": [{"id": "a", "path": "a", "restartOnError": 3000},
                                    {"id": "b", "path": "b", "restartOnError": false},
                                    {"id": "c", "path": "c"}]}),
        );
        let (p, apps, texts) = convert_ok(&t);
        let cfg = |i: usize| Config::parse(&texts[i].clone().unwrap().0).unwrap();
        assert_eq!(apps[0].restart_delay_ms, Some(3000));
        assert_eq!((cfg(0).restart.backoff_initial, cfg(0).restart.enabled), (3000, true));
        assert!(has_note(&apps[0], "restartOnError", Kind::Approximated));
        assert!(!cfg(1).restart.enabled, "false on the application turns restarts off");
        assert!(cfg(2).restart.enabled, "the runtime's value is ignored by `wattpm start`, so is ours");
        assert!(project_notes(&p).iter().any(|n| n.field == "restartOnError"));
        assert_eq!((cfg(0).shutdown.grace_period, cfg(0).workers.ready_timeout), (20, 45));
        assert!(has_note(&apps[0], "restart policy", Kind::Approximated));
    }

    #[test]
    fn node_entry_follows_watts_order() {
        let t = Tmp::new("entry");
        let d = &t.0;
        // Nothing to run.
        assert!(node_entry(d, &json!({})).unwrap_err().contains("no entry file"));
        // A conventional name.
        t.write("server.mjs", "");
        assert_eq!(node_entry(d, &json!({})).unwrap(), d.join("server.mjs"));
        t.write("index.cjs", "");
        assert_eq!(node_entry(d, &json!({})).unwrap(), d.join("index.cjs"), "index before server");
        // package.json beats the names; node.main beats package.json.
        t.json("package.json", json!({"main": "lib/main.js"}));
        assert_eq!(node_entry(d, &json!({})).unwrap(), d.join("lib/main.js"));
        t.json("package.json", json!({"exports": {".": {"import": "./esm.js"}}}));
        assert_eq!(node_entry(d, &json!({})).unwrap(), d.join("./esm.js"));
        t.json("package.json", json!({"exports": "./only.js", "main": "m.js"}));
        assert_eq!(node_entry(d, &json!({})).unwrap(), d.join("m.js"), "main first");
        assert_eq!(node_entry(d, &json!({"node": {"main": "src/x.ts"}})).unwrap(), d.join("src/x.ts"));
    }

    #[test]
    fn commands_become_programs_or_a_shell() {
        let t = Tmp::new("cmd");
        let dir = &t.0;
        let (l, a) = launch_for_command("node dist/main.js --flag", dir, dir, true);
        assert_eq!((l, a), (Launch::Program("node".into()), vec!["dist/main.js".to_string(), "--flag".to_string()]));
        let (l, a) = launch_for_command("npm run build && node x.js", dir, dir, true);
        assert!(matches!(l, Launch::Shell(_)) && a.is_empty());
        // A local binary that is a node script runs under node (so Warden's shim loads).
        t.write("node_modules/next/dist/bin/next", "#!/usr/bin/env node\nconsole.log(1)\n");
        std::fs::create_dir_all(t.0.join("node_modules/.bin")).unwrap();
        std::os::unix::fs::symlink("../next/dist/bin/next", t.0.join("node_modules/.bin/next")).unwrap();
        let (l, a) = launch_for_command("next start -p 3000", dir, dir, true);
        assert_eq!(l, Launch::Program("node".into()));
        assert!(a[0].ends_with("next/dist/bin/next") && a[1..] == ["start", "-p", "3000"], "{a:?}");
        // Not when local commands are off, or for a name that is nowhere.
        assert_eq!(launch_for_command("next start", dir, dir, false).0, Launch::Program("next".into()));
        assert_eq!(split_command("a  b\\ c d"), ["a", "b c", "d"], "execa's rule: spaces split, a backslash keeps one");
    }

    #[test]
    fn production_commands_overrides_and_frameworks() {
        let t = Tmp::new("types");
        node_app(&t, "api", "index.js");
        t.json("web/package.json", json!({"name": "web", "dependencies": {"next": "15"}}));
        t.json("web/watt.json", json!({"$schema": "https://schemas.platformatic.dev/@platformatic/next/3.71.0.json"}));
        t.json("cms/package.json", json!({"name": "cms"}));
        t.write("cms/server.js", "// listens elsewhere\n");
        t.json(
            "cms/watt.json",
            json!({"$schema": NODE, "application": {"commands": {"production": "node server.js --port {CMS_PORT}", "build": "npm run build"}}}),
        );
        t.write(".env", "CMS_PORT=3300\n");
        t.json("db/package.json", json!({"name": "db", "dependencies": {"@platformatic/db": "3"}}));
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web",
                   "applications": [{"id": "api", "path": "api"}, {"id": "web", "path": "web"},
                                    {"id": "cms", "path": "cms"}, {"id": "db", "path": "db"}]}),
        );
        let (_, apps, _) = convert_ok(&t);
        // Next.js with no command of its own: not converted, and the way out is named.
        let web = app(&apps, "web");
        assert!(
            web.skip.as_deref().unwrap().contains("--command") && web.skip.as_deref().unwrap().contains("next start"),
            "{:?}",
            web.skip
        );
        assert_eq!(web.module, "@platformatic/next");
        // A Platformatic DB application detected from package.json has no program either.
        assert_eq!(app(&apps, "db").module, "@platformatic/db");
        assert!(app(&apps, "db").skip.as_deref().unwrap().contains("capability"));
        // `commands.production` is used, with the placeholder filled, and its build step reported.
        let cms = app(&apps, "cms");
        assert_eq!(cms.run, Some(Run::Command("node server.js --port 3300".into())));
        assert!(has_note(cms, "application.commands.build", Kind::Check));
        // `--command` gives a skipped application its program.
        let o = Opts { commands: vec![("web".into(), "node ./srv.js".into())], ..opts(&t) };
        t.write("web/srv.js", "");
        let (_, apps, texts) = convert_all(&o).unwrap();
        assert!(app(&apps, "web").skip.is_none());
        let web_idx = apps.iter().position(|a| a.id == "web").unwrap();
        let c = Config::parse(&texts[web_idx].clone().unwrap().0).unwrap();
        assert_eq!((c.app.command.as_str(), c.app.args.as_slice()), ("node", ["./srv.js".to_string()].as_slice()));
        assert!(
            convert_all(&Opts { commands: vec![("nope".into(), "x".into())], ..opts(&t) })
                .unwrap_err()
                .contains("no Watt application")
        );
    }

    #[test]
    fn a_gateway_is_not_converted_and_names_what_it_fronted() {
        let t = Tmp::new("gateway");
        node_app(&t, "users", "index.js");
        t.json("gw/package.json", json!({"name": "gw", "dependencies": {"@platformatic/gateway": "3"}}));
        t.json(
            "gw/watt.json",
            json!({"$schema": "https://schemas.platformatic.dev/@platformatic/gateway/3.71.0.json",
                   "gateway": {"applications": [{"id": "users", "proxy": {"prefix": "/users"}}]}}),
        );
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "gw",
                   "applications": [{"id": "gw", "path": "gw"}, {"id": "users", "path": "users"}]}),
        );
        let (p, apps, texts) = convert_ok(&t);
        let gw = app(&apps, "gw");
        let why = gw.skip.as_deref().unwrap();
        assert!(why.contains("composes") && why.contains("users (/users)") && why.contains("nginx"), "{why}");
        assert!(texts[0].is_none());
        assert!(texts[1].is_some(), "the application behind it is converted");
        let md = report(&p, &apps, &project_notes(&p));
        assert!(md.contains("## gw (not converted)") && md.contains("mesh: unsupported"), "{md}");
        assert!(md.contains("| gw | Platformatic Gateway | not converted |"), "{md}");
    }

    #[test]
    fn unsupported_runtime_features_are_reported_not_dropped() {
        let t = Tmp::new("unsupported");
        node_app(&t, "web", "index.js");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web",
                   "undici": {"interceptors": [{"module": "undici-cache-interceptor", "options": {}}], "agentOptions": {"keepAliveTimeout": 1}},
                   "policies": {"deny": {"web": "web"}}, "httpCache": true,
                   "scheduler": [{"name": "x", "cron": "* * * * *", "callbackUrl": "http://web.plt.local/x"}],
                   "server": {"hostname": "0.0.0.0", "port": 3000, "https": {"key": "k", "cert": "c"}},
                   "metrics": {"port": 9091}, "managementApi": true, "logger": {"level": "debug"},
                   "applications": [{"id": "web", "path": "web", "dependencies": ["db"], "permissions": {"fs": {"read": ["."]}},
                                    "health": {"maxELU": 0.9, "maxHeapTotal": "512MB"}, "workers": {"dynamic": true, "minimum": 1}}]}),
        );
        let (p, apps, _) = convert_ok(&t);
        let fields: Vec<(String, Kind)> = project_notes(&p).into_iter().map(|n| (n.field, n.kind)).collect();
        for f in ["undici", "policies", "httpCache", "scheduler", "server.https, http2", "metrics", "managementApi"] {
            assert!(fields.contains(&(f.to_string(), Kind::Unsupported)), "{f} in {fields:?}");
        }
        assert!(fields.contains(&("logger".to_string(), Kind::Check)));
        let undici = project_notes(&p).into_iter().find(|n| n.field == "undici").unwrap();
        assert!(undici.text.contains("undici-cache-interceptor"), "{}", undici.text);
        let web = &apps[0];
        for f in ["dependencies", "permissions", "health", "workers.dynamic"] {
            assert!(has_note(web, f, Kind::Unsupported), "{f} in {:?}", web.notes);
        }
        // maxHeapTotal 512 MB minus Watt's 128 MB young generation.
        assert_eq!(web.interpreter_args, ["--max-old-space-size=384"]);
        assert!(has_note(web, "health.maxHeapTotal", Kind::Approximated));
        assert!(!web.notes.iter().any(|n| n.field == "server.hostname"), "0.0.0.0 exposes nothing new");
    }

    #[test]
    fn node_flags_preload_and_arguments_go_to_the_node_process() {
        let t = Tmp::new("flags");
        node_app(&t, "web", "index.js");
        t.write("apm.js", "");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "preload": "./apm.js", "sourceMaps": true,
                   "applications": [{"id": "web", "path": "web", "execArgv": ["--stack-trace-limit=50"], "nodeOptions": "--no-deprecation",
                                    "arguments": ["--mode", "prod"]}]}),
        );
        let (_, apps, texts) = convert_ok(&t);
        let c = Config::parse(&texts[0].clone().unwrap().0).unwrap();
        let args = &c.app.args;
        assert_eq!(&args[..2], ["--stack-trace-limit=50", "--no-deprecation"]);
        assert!(args.contains(&"--import".to_string()) && args.iter().any(|a| a.ends_with("/apm.js")), "{args:?}");
        assert!(args.contains(&"--enable-source-maps".to_string()));
        assert_eq!(&args[args.len() - 2..], ["--mode", "prod"], "arguments follow the script");
        assert!(apps[0].script_args == ["--mode", "prod"]);
    }

    #[test]
    fn config_formats_and_their_errors() {
        let t = Tmp::new("formats");
        node_app(&t, "web", "index.js");
        let body = json!({"$schema": RUNTIME, "applications": [{"id": "web", "path": "web"}]});
        // TOML.
        t.write(
            "watt.toml",
            &format!("\"$schema\" = \"{RUNTIME}\"\nworkers = 2\n\n[[applications]]\nid = \"web\"\npath = \"web\"\n"),
        );
        assert_eq!(convert_ok(&t).1[0].count, 2);
        std::fs::remove_file(t.0.join("watt.toml")).unwrap();
        // JSON5: comments and trailing commas.
        t.write(
            "watt.json5",
            &format!("// the runtime\n{{\n  \"$schema\": \"{RUNTIME}\", /* c */\n  \"workers\": 3,\n  \"applications\": [{{\"id\": \"web\", \"path\": \"web\",}}],\n}}\n"),
        );
        assert_eq!(convert_ok(&t).1[0].count, 3);
        std::fs::remove_file(t.0.join("watt.json5")).unwrap();
        // `.json` is strict, as it is in Watt: the error says where, and why a comment is the problem.
        t.write("watt.json", &format!("{{\n  // pick the entry\n  \"$schema\": \"{RUNTIME}\"\n}}\n"));
        let err = convert_all(&opts(&t)).unwrap_err();
        assert!(
            err.contains("not valid JSON")
                && err.contains("line 2")
                && err.contains("comments")
                && err.contains(".json5"),
            "{err}"
        );
        t.write("watt.json", "{\"$schema\": \"x\", \"applications\": [");
        let err = convert_all(&opts(&t)).unwrap_err();
        assert!(err.contains("not valid JSON") && !err.contains("comments"), "{err}");
        t.write("watt.json", "[1, 2]");
        assert!(convert_all(&opts(&t)).unwrap_err().contains("object"));
        // YAML is named, not guessed at.
        std::fs::remove_file(t.0.join("watt.json")).unwrap();
        t.write("watt.yaml", "applications: []\n");
        assert!(convert_all(&opts(&t)).unwrap_err().contains("YAML"));
        std::fs::remove_file(t.0.join("watt.yaml")).unwrap();
        // Nothing there.
        assert!(convert_all(&opts(&t)).unwrap_err().contains("no Watt config"));
        // A config that is neither a runtime nor an application's.
        t.json("watt.json", json!({"hello": "world"}));
        assert!(convert_all(&opts(&t)).unwrap_err().contains("not a Watt config"));
        // platformatic.json is read too; a file path works as well as a directory.
        t.json("platformatic.json", body.clone());
        std::fs::remove_file(t.0.join("watt.json")).unwrap();
        assert_eq!(convert_ok(&t).1.len(), 1);
        let by_file = Opts { path: Some(t.0.join("platformatic.json")), ..Default::default() };
        assert_eq!(convert_all(&by_file).unwrap().1.len(), 1);
        // No `$schema` but applications: read as a runtime, with a note.
        t.json("platformatic.json", json!({"applications": [{"id": "web", "path": "web"}]}));
        let (p, _, _) = convert_ok(&t);
        assert!(p.notes.iter().any(|n| n.field == "$schema"));
    }

    #[test]
    fn missing_pieces_are_named_not_guessed() {
        let t = Tmp::new("missing");
        node_app(&t, "web", "index.js");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "ghost", "applications": [{"id": "web", "path": "web"}]}),
        );
        assert!(convert_all(&opts(&t)).unwrap_err().contains("not one of the applications"));
        t.json("watt.json", json!({"$schema": RUNTIME, "applications": [{"path": "web"}]}));
        assert!(convert_all(&opts(&t)).unwrap_err().contains("needs an `id`"));
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "applications": [{"id": "web", "enabled": false, "path": "web"}]}),
        );
        assert!(convert_all(&opts(&t)).unwrap_err().contains("no enabled applications"));
        // An external application that was never resolved, a missing directory, a build that was not run.
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "applications": [{"id": "ext", "url": "https://example.com/x.git"}, {"id": "gone", "path": "nowhere"},
                                                        {"id": "web", "path": "web"}]}),
        );
        let (_, apps, _) = convert_ok(&t);
        assert!(
            app(&apps, "ext").skip.as_deref().unwrap().contains("not been resolved"),
            "{:?}",
            app(&apps, "ext").skip
        );
        assert!(app(&apps, "gone").skip.as_deref().unwrap().contains("not a directory"));
        t.json("web/package.json", json!({"name": "web", "main": "dist/server.js"}));
        let (_, apps, texts) = convert_ok(&t);
        let why = apps.iter().find(|a| a.id == "web").unwrap().skip.clone().unwrap();
        assert!(why.contains("does not exist") && why.contains("build"), "{why}");
        assert!(texts.iter().all(Option::is_none));
        // --apps picks, and names what exists when it cannot find one.
        let err = convert_all(&Opts { apps: vec!["zzz".into()], ..opts(&t) }).unwrap_err();
        assert!(err.contains("zzz") && err.contains("web"), "{err}");
    }

    #[test]
    fn an_entry_that_exports_a_factory_gets_a_warning() {
        assert!(looks_like_factory("export async function create() { return fastify() }\n"));
        assert!(looks_like_factory("module.exports.build = () => app\n"));
        assert!(!looks_like_factory("export function create() {}\napp.listen(3000)\n"));
        assert!(!looks_like_factory("http.createServer(h).listen(3000)\n"));
        let t = Tmp::new("factory");
        t.json("package.json", json!({"name": "api", "main": "app.js"}));
        t.write("app.js", "export async function create() { return fastify() }\n");
        t.json("watt.json", json!({"$schema": NODE}));
        let (_, apps, _) = convert_ok(&t);
        assert!(has_note(&apps[0], "entry", Kind::Check), "{:?}", apps[0].notes);
    }

    #[test]
    fn more_workers_than_the_port_can_share_become_one() {
        let t = Tmp::new("shim");
        t.json("package.json", json!({"name": "api", "main": "app.js"}));
        t.write("app.js", "");
        t.write("bin/serve", "#!/bin/sh\n");
        // A program that is neither node nor bun cannot share the port: one worker, and it is said.
        t.json(
            "watt.json",
            json!({"$schema": NODE, "server": {"port": 4000}, "runtime": {"workers": 4},
                   "application": {"commands": {"production": "./bin/serve --fast"}}}),
        );
        let (_, apps, _) = convert_ok(&t);
        let a = &apps[0];
        assert_eq!(a.module, "@platformatic/node");
        assert_eq!(a.run, Some(Run::Command("./bin/serve --fast".into())));
        assert_eq!(a.count, 1, "{:?}", a.notes);
        assert!(has_note(a, "workers", Kind::Approximated));
        // A node command shares it through the shim.
        t.json(
            "watt.json",
            json!({"$schema": NODE, "server": {"port": 4000}, "runtime": {"workers": 4},
                   "application": {"commands": {"production": "node app.js"}}}),
        );
        let (_, apps, _) = convert_ok(&t);
        assert_eq!(apps[0].count, 4, "{:?}", apps[0].notes);
        // With no port there is nothing to share, so the count stays.
        t.json(
            "watt.json",
            json!({"$schema": NODE, "runtime": {"workers": 4},
                   "application": {"commands": {"production": "./bin/serve"}}}),
        );
        let (_, apps, _) = convert_ok(&t);
        assert_eq!(apps[0].count, 4, "{:?}", apps[0].notes);
    }

    #[test]
    fn per_worker_ports_use_the_offset_strategy() {
        let t = Tmp::new("offset");
        node_app(&t, "web", "index.js");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "workers": 3, "server": {"port": 5000, "portAssignment": "perWorkerIncrement"},
                   "applications": [{"id": "web", "path": "web"}]}),
        );
        let (_, apps, texts) = convert_ok(&t);
        assert!(apps[0].offset_ports);
        let c = Config::parse(&texts[0].clone().unwrap().0).unwrap();
        assert_eq!(c.workers.port_strategy, crate::config::PortStrategy::Offset);
    }

    #[test]
    fn watch_and_names() {
        let t = Tmp::new("watch");
        node_app(&t, "web", "index.js");
        node_app(&t, "api", "index.js");
        t.json("package.json", json!({"name": "@acme/my shop"}));
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "applications": [{"id": "web", "path": "web", "watch": true}, {"id": "api", "path": "api"}]}),
        );
        let o = Opts { prefix: Some("shop".into()), ..opts(&t) };
        let (_, apps, texts) = convert_all(&o).unwrap();
        let web = Config::parse(&texts[0].clone().unwrap().0).unwrap();
        assert!(web.watch.enabled);
        assert_eq!(web.app.name, "shop-web");
        assert_eq!(
            web.app.namespace.as_deref(),
            Some("my-shop"),
            "the package name groups the apps for `warden restart`"
        );
        assert!(!Config::parse(&texts[1].clone().unwrap().0).unwrap().watch.enabled);
        assert!(has_note(&apps[0], "watch", Kind::Approximated));
        // Two ids that become the same Warden name: the second is not converted.
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "applications": [{"id": "a b", "path": "web"}, {"id": "a-b", "path": "api"}]}),
        );
        let (_, apps, _) = convert_ok(&t);
        assert!(apps[1].skip.as_deref().unwrap().contains("--prefix"), "{:?}", apps[1].skip);
        assert_eq!(namespace_of("@acme/shop").as_deref(), Some("shop"));
        assert_eq!(namespace_of("all"), None);
    }

    #[test]
    fn the_report_lists_every_application_and_the_limits() {
        let t = Tmp::new("report");
        node_app(&t, "web", "index.js");
        node_app(&t, "api", "index.js");
        t.write(".env", "TOKEN=abc123\n");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web", "server": {"port": "{PORT}"},
                   "applications": [{"id": "web", "path": "web"}, {"id": "api", "path": "api", "workers": 2}]}),
        );
        let (p, apps, _) = convert_ok(&t);
        let md = report(&p, &apps, &project_notes(&p));
        assert!(md.starts_with("# Watt → Warden migration"));
        assert!(md.contains("| web | Node.js | converted | web |"), "{md}");
        assert!(md.contains("## web → web") && md.contains("## api → api"), "{md}");
        assert!(md.contains("Role: the entry point") && md.contains("Role: internal"), "{md}");
        assert!(md.contains("mesh: unsupported"), "{md}");
        assert!(md.contains("no value for PORT"), "{md}");
        assert!(md.contains("Environment kept") && md.contains("TOKEN"), "{md}");
        assert!(!md.contains("abc123"), "no value in the report: {md}");
        assert!(md.contains("wattpm stop"), "{md}");
    }

    #[test]
    fn parse_arguments() {
        let parse = |a: &[&str]| parse_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let opts = |a: &[&str]| match parse(a).unwrap().command {
            Command::WattpmMigrate(o) => *o,
            c => panic!("{c:?}"),
        };
        let o = opts(&[
            "./shop",
            "-c",
            "platformatic.json",
            "-e",
            ".env.prod",
            "--apps",
            "a,b",
            "--out",
            "/tmp/x",
            "--prefix",
            "shop",
            "--command",
            "web=next start",
            "--dry-run",
        ]);
        assert_eq!(o.path.as_deref(), Some(Path::new("./shop")));
        assert_eq!(
            (o.config.as_deref(), o.env_file.as_deref(), o.apps.clone()),
            (Some("platformatic.json"), Some(Path::new(".env.prod")), vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(o.commands, [("web".to_string(), "next start".to_string())]);
        assert!(o.dry_run && o.prefix.as_deref() == Some("shop") && o.out.as_deref() == Some(Path::new("/tmp/x")));
        assert_eq!(opts(&["--cutover", "new-port:4300"]).cutover, Some(Cutover::NewPort(4300)));
        assert_eq!(opts(&["--cutover", "overlap"]).cutover, Some(Cutover::Overlap));
        assert_eq!(opts(&[]), Opts::default());
        assert!(matches!(parse(&["--help"]).unwrap().command, Command::Help));
        for (bad, why) in [
            (&["--cutover", "same-port"][..], "same-port"),
            (&["--cutover", "fast"], "overlap"),
            (&["--dry-run", "--cutover", "overlap"], "--dry-run"),
            (&["--finalize"], "unknown option"),
            (&["a", "b"], "one project"),
            (&["--command", "noequals"], "<application id>=<command line>"),
            (&["--out"], "needs a value"),
        ] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains(why), "{bad:?}: {e}");
        }
    }

    #[test]
    fn heap_limits_and_jsonc() {
        assert_eq!(heap_limit_mb(&json!("512MB"), None), Some(384));
        assert_eq!(heap_limit_mb(&json!(1073741824u64), Some(&json!("256 MB"))), Some(768));
        assert_eq!(heap_limit_mb(&json!("2gb"), Some(&json!(0))), Some(2048));
        assert_eq!(heap_limit_mb(&json!("lots"), None), None);
        assert_eq!(heap_limit_mb(&json!("64MB"), None), None, "below the young generation");
        assert_eq!(
            strip_jsonc("{\"a\": \"// not a comment\", /* x */ \"b\": [1,2,],}"),
            "{\"a\": \"// not a comment\",  \"b\": [1,2]}"
        );
        // A trailing comma before a comment and the closing brace is still a trailing comma; one in a string is not.
        let v: Value =
            serde_json::from_str(&strip_jsonc("{\"a\": [1, // one\n 2, /* two */ ],\n \"b\": \"x,}\", // last\n}"))
                .unwrap();
        assert_eq!(v, json!({"a": [1, 2], "b": "x,}"}));
        assert_eq!(
            module_of(&json!({"$schema": "https://schemas.platformatic.dev/@platformatic/node/3.71.0.json"})),
            Some(("@platformatic/node".into(), Some("3.71.0".into())))
        );
        assert_eq!(
            module_of(&json!({"$schema": "https://schemas.platformatic.dev/wattpm/v3.0.0.json"})),
            Some(("@platformatic/runtime".into(), Some("3.0.0".into())))
        );
        assert_eq!(
            module_of(&json!({"$schema": "https://platformatic.dev/schemas/v2.1.0/runtime"})),
            Some(("@platformatic/runtime".into(), Some("2.1.0".into())))
        );
        assert_eq!(module_of(&json!({"module": "@platformatic/next"})).unwrap().0, "@platformatic/next");
        assert_eq!(module_of(&json!({"$schema": "https://example.com/x.json"})), None);
        assert_eq!(candidates()[..4], ["watt.json", "platformatic.json", "watt.json5", "platformatic.json5"]);
    }

    #[test]
    fn two_applications_on_one_port_are_flagged() {
        let t = Tmp::new("sameport");
        node_app(&t, "web", "index.js");
        node_app(&t, "api", "index.js");
        node_app(&t, "jobs", "index.js");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web", "server": {"port": 5000},
                   "applications": [{"id": "web", "path": "web"},
                                    {"id": "api", "path": "api", "env": {"PORT": "5000"}},
                                    {"id": "jobs", "path": "jobs", "env": {"PORT": "5001"}}]}),
        );
        let (_, apps, _) = convert_ok(&t);
        assert_eq!(app(&apps, "api").port, Some(5000));
        assert!(has_note(app(&apps, "web"), "port", Kind::Check));
        assert!(has_note(app(&apps, "api"), "port", Kind::Check));
        assert!(!has_note(app(&apps, "jobs"), "port", Kind::Check), "{:?}", app(&apps, "jobs").notes);
    }

    #[test]
    fn awkward_values_survive_the_env_file() {
        let t = Tmp::new("awkward");
        node_app(&t, "web", "index.js");
        let values = [
            ("A", "line1\nline2\ttabbed"),
            ("B", "a\"b'c"),
            ("C", "back\\slash\\\\two"),
            ("D", "  padded  "),
            ("E", "x # y"),
            ("F", "$HOME `date` $(id)"),
            ("G", "=starts with equals"),
            ("H", ""),
            ("I", "caf\u{e9} \u{1f600}"),
        ];
        let env: serde_json::Map<String, Value> = values.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web", "applications": [{"id": "web", "path": "web", "env": env}]}),
        );
        let (_, _, texts) = convert_ok(&t);
        let (_, env_text) = texts[0].clone().unwrap();
        let parsed = crate::config::parse_env_file(&env_text).unwrap();
        for (k, v) in values {
            assert_eq!(parsed.get(k).map(String::as_str), Some(v), "{k} in\n{env_text}");
        }
    }

    #[test]
    fn what_is_generated_is_what_warden_accepts() {
        let t = Tmp::new("roundtrip");
        node_app(&t, "web", "index.js");
        node_app(&t, "worker", "index.js");
        t.write(".env", "DB_URL=\"postgres://u:p w@h/db\"\nPORT=4400\n");
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web", "workers": 2, "server": {"port": "{PORT}"},
                   "applications": [{"id": "web", "path": "web"}, {"id": "worker", "path": "worker", "workers": 1}]}),
        );
        let (_, apps, texts) = convert_ok(&t);
        for (a, text) in apps.iter().zip(texts) {
            let (toml, env) = text.unwrap();
            let c = Config::parse(&toml).unwrap_or_else(|e| panic!("{}: {e}\n{toml}", a.id));
            assert_eq!(c.app.name, a.id);
            assert!(!toml.contains("postgres"), "no secret in the config: {toml}");
            let parsed = crate::config::parse_env_file(&env).unwrap();
            assert_eq!(parsed["DB_URL"], "postgres://u:p w@h/db");
        }
    }

    const TOKEN: &str = "s3cr3t-value-123";

    /// A project with one entry point, `web`, whose own watt.json is `app_cfg`, and a `.env`.
    fn project_with(t: &Tmp, dotenv: &str, app_cfg: Value) {
        node_app(t, "web", "server.js");
        t.write(".env", dotenv);
        t.json("web/watt.json", app_cfg);
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web", "applications": [{"id": "web", "path": "web"}]}),
        );
    }

    fn production(cmd: &str) -> Value {
        json!({"$schema": NODE, "application": {"commands": {"production": cmd}}})
    }

    #[test]
    fn a_secret_in_a_shell_command_is_read_from_the_env_file_not_written_into_the_config() {
        let t = Tmp::new("shellsecret");
        project_with(
            &t,
            &format!("API_TOKEN={TOKEN}\nWHO=world\nPORT=4821\n"),
            production(
                "npm run build && node server.js --token={API_TOKEN} --who='{WHO}' --label=\"{API_TOKEN}\" --port={PORT}",
            ),
        );
        let (p, apps, texts) = convert_ok(&t);
        let (toml, env) = texts[0].clone().unwrap();
        assert!(!toml.contains(TOKEN) && !toml.contains("world"), "the config holds a value:\n{toml}");
        let c = Config::parse(&toml).unwrap();
        assert_eq!((c.app.command.as_str(), c.app.args[0].as_str()), ("sh", "-c"));
        // Each reference is quoted for where it stands; a value that is not a secret (a port) is inline.
        assert_eq!(
            c.app.args[1],
            "npm run build && node server.js --token=\"${API_TOKEN}\" --who=''\"${WHO}\"'' --label=\"${API_TOKEN}\" --port=4821"
        );
        let parsed = crate::config::parse_env_file(&env).unwrap();
        assert_eq!((parsed["API_TOKEN"].as_str(), parsed["WHO"].as_str()), (TOKEN, "world"));
        assert!(!holds_secret(&toml, &apps[0].secrets), "the config needs no mode 0600: it holds nothing secret");
        let md = report(&p, &apps, &[]);
        assert!(!md.contains(TOKEN), "{md}");
        assert!(md.contains("${API_TOKEN}") && md.contains("mode 0600"), "{md}");
        assert!(has_note(&apps[0], "application.commands.production", Kind::Approximated));
    }

    #[test]
    fn the_shell_text_of_a_command_does_what_watts_substituted_command_does() {
        // The command with the value pasted in (what Watt runs) and the one that reads it from the
        // environment (what Warden runs) print the same.
        let template = "printf '[%s]' a{T}b '{T}' \"{T}\" {T} \"x'{T}'y\" '\"{T}\"'; echo";
        let value = "s3cr3t-value-123";
        let vars = Vars { map: [("T".to_string(), value.to_string())].into(), file_only: ["T".to_string()].into() };
        let mut ph = Placeholders::default();
        let pasted = replace_str(template, &vars, &mut ph);
        let sub = ph.strings.get(&pasted).unwrap();
        let reading = shell_text(sub).unwrap();
        assert!(!reading.contains(value), "{reading}");
        let run = |cmd: &str| {
            let o = std::process::Command::new("sh").arg("-c").arg(cmd).env("T", value).output().unwrap();
            assert!(o.status.success(), "{cmd}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).to_string()
        };
        assert_eq!(run(&reading), run(&pasted), "{reading}");
        // One word, whatever the value holds: the shell does not split or expand it.
        let odd = "a b $HOME `id` 'q' \"d\"";
        let o = std::process::Command::new("sh")
            .arg("-c")
            .arg(
                shell_text(&Subst {
                    template: "printf '<%s>' {T}".into(),
                    first: vec![Filled { name: "T".into(), value: odd.into(), secret: true }],
                    ..Default::default()
                })
                .unwrap(),
            )
            .env("T", odd)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout), format!("<{odd}>"));
    }

    #[test]
    fn a_secret_in_a_program_command_is_masked_everywhere_and_the_config_is_private() {
        let t = Tmp::new("progsecret");
        project_with(&t, &format!("API_TOKEN={TOKEN}\n"), production("node server.js --token={API_TOKEN}"));
        let (p, apps, texts) = convert_ok(&t);
        let a = &apps[0];
        let (toml, _) = texts[0].clone().unwrap();
        // The program needs the value, so the config has it; the rest of Warden must not show it.
        assert!(toml.contains(TOKEN) && holds_secret(&toml, &a.secrets), "{toml}");
        assert!(!a.hide(&toml).contains(TOKEN) && a.hide(&toml).contains("--token=***"), "{}", a.hide(&toml));
        let md = report(&p, &apps, &[]);
        assert!(!md.contains(TOKEN) && md.contains("--token=***"), "{md}");
        assert!(md.contains("0600") && md.contains("{API_TOKEN}"), "the report says what happened:\n{md}");
        assert!(has_note(a, "placeholders", Kind::Check));
    }

    #[test]
    fn a_secret_in_arguments_and_exec_argv_is_masked_too() {
        let t = Tmp::new("flagsecret");
        node_app(&t, "web", "index.js");
        t.write(".env", &format!("API_TOKEN={TOKEN}\n"));
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web",
                   "applications": [{"id": "web", "path": "web", "execArgv": ["--title={API_TOKEN}"],
                                     "nodeOptions": "--require={API_TOKEN}", "arguments": ["--token={API_TOKEN}"]}]}),
        );
        let (p, apps, texts) = convert_ok(&t);
        let (toml, _) = texts[0].clone().unwrap();
        assert!(toml.contains("--title=s3cr3t") && toml.contains("--token=s3cr3t"), "{toml}");
        assert!(holds_secret(&toml, &apps[0].secrets));
        let md = report(&p, &apps, &[]);
        assert!(!md.contains(TOKEN) && md.contains("--token=***") && md.contains("--title=***"), "{md}");
    }

    #[test]
    fn a_config_that_fails_validation_is_described_without_its_text() {
        // A TOML error quotes the line it found; the message must not.
        let line = format!("[app]\nname = 'a' = '{TOKEN}'\n");
        let e = Config::parse(&line).unwrap_err();
        assert!(e.contains(TOKEN), "the premise: {e}");
        let shown = describe_invalid(&e, &BTreeSet::new());
        assert!(!shown.contains(TOKEN) && shown.contains("line 2"), "{shown}");
        // A value the message names is masked.
        let secrets: BTreeSet<String> = [TOKEN.to_string()].into();
        assert_eq!(
            describe_invalid(&format!("invalid type: string \"{TOKEN}\""), &secrets),
            "invalid type: string \"***\""
        );
    }

    #[test]
    fn a_failing_validation_prints_the_reason_and_not_the_config() {
        // `perWorkerIncrement` from the last port: the workers' ports do not fit.
        let t = Tmp::new("invalid");
        node_app(&t, "web", "index.js");
        t.write(".env", &format!("API_TOKEN={TOKEN}\n"));
        t.json("web/watt.json", production("node server.js --token={API_TOKEN}"));
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web", "workers": 4,
                   "server": {"port": 65535, "portAssignment": "perWorkerIncrement"},
                   "applications": [{"id": "web", "path": "web"}]}),
        );
        let (p, apps, texts) = convert_ok(&t);
        assert!(texts[0].is_none());
        let why = apps[0].skip.clone().unwrap();
        assert!(why.contains("the generated config is invalid") && why.contains("65535"), "{why}");
        assert!(!why.contains(TOKEN) && !why.contains("[app]") && !why.contains("args ="), "{why}");
        assert!(!report(&p, &apps, &[]).contains(TOKEN));
    }

    #[test]
    fn a_line_break_in_a_command_stays_out_of_the_header_comment() {
        let t = Tmp::new("newline");
        node_app(&t, "web", "index.js");
        for (name, command) in [
            ("a", "npm run build &&\nnode server.js"),
            ("b", "node server.js;\n[limits]\nmax_memory = 1\n# done"),
            ("c", "node server.js;\r[limits]\rmax_memory = 1"),
        ] {
            t.json(
                "watt.json",
                json!({"$schema": RUNTIME, "entrypoint": "web", "applications": [{"id": "web", "path": "web"}]}),
            );
            let mut o = opts(&t);
            o.commands = vec![("web".into(), command.into())];
            let (_, apps, texts) = convert_all(&o).unwrap();
            let (toml, _) = texts[0].clone().unwrap_or_else(|| panic!("{name}: {:?}", apps[0].skip));
            let c = Config::parse(&toml).unwrap_or_else(|e| panic!("{name}: {e}\n{toml}"));
            assert_eq!(c.limits.max_memory, 0, "{name}: the command wrote a [limits] table:\n{toml}");
            let table: toml::Table = toml.parse().unwrap_or_else(|e| panic!("{name}: {e}\n{toml}"));
            assert!(table.get("limits").is_none(), "{name}:\n{toml}");
            assert!(c.app.args.iter().any(|a| a.contains("node server.js")), "{name}: {:?}", c.app.args);
            let head: Vec<&str> = toml.lines().take(3).collect();
            assert!(
                head[0].starts_with("# Written by `warden migrate-wattpm`") && head[1].is_empty() && head[2] == "[app]",
                "{name}: {head:?}"
            );
        }
        // `warden start` writes the same header: its own is one line, too.
        for cmd in ["npm run build &&\nnode server.js", "node server.js;\n[limits]\nmax_memory = 1\n# done"] {
            let (_, text) = fleet::quick_config(&Launch::Shell(cmd.into()), &StartOpts::default()).unwrap();
            let table: toml::Table = text.parse().unwrap_or_else(|e| panic!("{e}\n{text}"));
            assert!(table.get("limits").is_none(), "{text}");
            assert_eq!(Config::parse(&text).unwrap().limits.max_memory, 0, "{text}");
            assert!(text.lines().next().unwrap().starts_with("# Written by `warden start"), "{text}");
        }
    }

    #[test]
    fn a_huge_restart_delay_is_clamped_not_a_panic() {
        let t = Tmp::new("huge");
        node_app(&t, "web", "index.js");
        for (ms, initial) in [(18_446_744_073_709_551_615u64, 225_000u64), (3_600_001, 225_000), (60_000, 60_000)] {
            t.json(
                "watt.json",
                json!({"$schema": RUNTIME, "entrypoint": "web",
                       "applications": [{"id": "web", "path": "web", "restartOnError": ms}]}),
            );
            let (_, apps, texts) = convert_ok(&t);
            let (toml, _) = texts[0].clone().unwrap_or_else(|| panic!("{ms}: {:?}", apps[0].skip));
            let c = Config::parse(&toml).unwrap_or_else(|e| panic!("{ms}: {e}\n{toml}"));
            assert_eq!(c.restart.backoff_initial, initial, "{ms}");
            assert!(c.restart.backoff_max <= 3_600_000 && c.restart.backoff_max >= c.restart.backoff_initial);
            let noted = has_note(&apps[0], "restartOnError", Kind::Approximated);
            assert!(noted, "{ms}");
            let text = &apps[0].notes.iter().find(|n| n.field == "restartOnError").unwrap().text;
            assert_eq!(text.contains("more than Warden accepts"), ms > initial, "{ms}: {text}");
        }
        // The same for the stop and start timeouts.
        t.json(
            "watt.json",
            json!({"$schema": RUNTIME, "entrypoint": "web", "startTimeout": u64::MAX, "gracefulShutdown": {"application": u64::MAX},
                   "applications": [{"id": "web", "path": "web"}]}),
        );
        let (_, _, texts) = convert_ok(&t);
        let c = Config::parse(&texts[0].clone().unwrap().0).unwrap();
        assert_eq!((c.shutdown.grace_period, c.workers.ready_timeout), (3600, 3600));
    }

    #[test]
    fn a_name_that_is_a_directory_gets_another_name() {
        let t = Tmp::new("dots");
        for (id, name) in [(".", "app-dot"), ("..", "app-dotdot"), ("...", "app-dots3"), (" .. ", "app-dotdot")] {
            node_app(&t, "web", "index.js");
            t.json(
                "watt.json",
                json!({"$schema": RUNTIME, "entrypoint": id, "applications": [{"id": id, "path": "web"}]}),
            );
            let (_, apps, texts) = convert_ok(&t);
            let (toml, _) = texts[0].clone().unwrap_or_else(|| panic!("{id:?}: {:?}", apps[0].skip));
            assert_eq!(Config::parse(&toml).unwrap().app.name, name, "{id:?}");
            assert_eq!(apps[0].name, name);
            assert!(has_note(&apps[0], "name", Kind::Approximated), "{id:?}");
        }
        // Warden itself refuses them.
        for bad in [".", "..", "..."] {
            let text = format!("[app]\nname = {bad:?}\ncommand = \"x\"\n");
            let e = Config::parse(&text).unwrap_err();
            assert!(e.contains("only dots"), "{bad}: {e}");
        }
        assert!(Config::parse("[app]\nname = \"a.b\"\ncommand = \"x\"\n").is_ok());
        assert!(Config::parse("[app]\nname = \".hidden\"\ncommand = \"x\"\n").is_ok());
    }
}
