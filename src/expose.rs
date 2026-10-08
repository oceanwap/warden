//! `warden expose api.example.com --app api`: nginx in front of an app, in one
//! command. It writes the app's nginx site file (the settings of
//! `contrib/nginx.conf`, filled in for this app), checks it with `nginx -t`,
//! reloads nginx, and records the hostnames in the app's config (`[expose]`),
//! so running it again rewrites the same file. A check that fails puts the
//! previous file back and leaves nginx as it was.
//!
//! nginx stays the server on ports 80 and 443; Warden only writes its file, so
//! it is never on the request path.

use crate::cli::{Args, Command};
use crate::config::{self, Config, Expose};
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub const HELP: &str = "\
warden expose - send a hostname to an app through nginx

USAGE:
    warden expose <hostname>... --app <app> [OPTIONS]
    warden expose <hostname>... --app <app> --remove

Writes the app's nginx site file (warden-<app>.conf in nginx's conf.d), checks
it with `nginx -t`, reloads nginx, and records the hostnames in the app's config
([expose]). nginx connects to the app's port, which every worker shares, so
nothing changes in nginx when you scale or deploy. Run it again to add a
hostname or change a setting: the same file is rewritten. If `nginx -t` fails,
the previous file is put back and nginx is not reloaded.

With --acme <email>, nginx gets a Let's Encrypt certificate itself and renews it
(its ACME module, nginx.org's nginx-module-acme). With --cert and --key it uses
those files (a Cloudflare Origin CA certificate, say); a certbot certificate in
/etc/letsencrypt/live/<hostname>/ is used without asking. Either way nginx serves
HTTPS (HTTP/2) on 443 and redirects port 80 to it. Without a certificate nginx
serves plain HTTP on port 80: for TLS that ends at Cloudflare or a load balancer.

OPTIONS:
    --app <app>       The app (name or id; -c <config> works too)
    --acme <email>    HTTPS with a Let's Encrypt certificate that nginx gets and renews
                      (nginx's ACME module; port 80 must reach this host)
    --cert <file>     Certificate chain (fullchain.pem; a Cloudflare Origin CA certificate)
    --key <file>      Its private key
    --no-tls          Plain HTTP on port 80, even with a certificate recorded or found
    --websocket <path>   A path that carries WebSockets: 1 h read timeout (repeatable)
    --sse <path>      A path that streams Server-Sent Events: no buffering, 1 h (repeatable)
    --site <file>     Where the site file goes (default: <nginx conf.d>/warden-<app>.conf)
    --remove          Stop sending these hostnames to the app (all of them: the file goes)
    --no-reload       Write and check the file, but leave reloading nginx to you
    --dry-run         Print the site file and where it would go; change nothing
    -h, --help        This help

EXAMPLES:
    warden expose api.example.com --app api
    warden expose api.example.com www.example.com --app web --websocket /ws
    warden expose api.example.com --app api --acme you@example.com
    warden expose api.example.com --app api --cert /etc/ssl/cf/api.pem --key /etc/ssl/cf/api.key
    warden expose www.example.com --app web --remove
";

/// Marks a site file as Warden's: one without it is never overwritten.
const MARKER: &str = "# Written by `warden expose`";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Opts {
    pub hosts: Vec<String>,
    pub app: Option<String>,
    pub config: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub no_tls: bool,
    /// `--acme <email>`: nginx's ACME module gets the certificate.
    pub acme: Option<String>,
    pub websocket: Vec<String>,
    pub sse: Vec<String>,
    pub site: Option<PathBuf>,
    pub remove: bool,
    pub no_reload: bool,
    pub dry_run: bool,
    pub help: bool,
}

pub fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut o = Opts::default();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("expose: {flag} needs a value"))
        };
        match flag {
            "--app" | "-a" => o.app = Some(value()?),
            "-c" | "--config" => o.config = Some(value()?.into()),
            "--cert" => o.cert = Some(value()?.into()),
            "--key" => o.key = Some(value()?.into()),
            "--no-tls" => o.no_tls = true,
            "--acme" => o.acme = Some(value()?),
            "--websocket" | "--ws" => o.websocket.push(value()?),
            "--sse" => o.sse.push(value()?),
            "--site" => o.site = Some(value()?.into()),
            "--remove" => o.remove = true,
            "--no-reload" => o.no_reload = true,
            "--dry-run" => o.dry_run = true,
            "-h" | "--help" => o.help = true,
            s if s.starts_with('-') => return Err(format!("expose: unknown option {s} (warden expose --help)")),
            _ => o.hosts.extend(a.split(',').map(str::trim).filter(|h| !h.is_empty()).map(|h| h.to_lowercase())),
        }
    }
    if !o.help {
        if o.hosts.is_empty() {
            return Err("expose needs a hostname: `warden expose api.example.com --app api`".into());
        }
        if o.app.is_none() && o.config.is_none() {
            return Err("expose needs the app: `warden expose api.example.com --app api`".into());
        }
        for h in &o.hosts {
            config::valid_hostname(h).map_err(|e| format!("expose: {e}"))?;
        }
        if o.cert.is_some() != o.key.is_some() {
            return Err("expose: --cert and --key go together (the certificate chain and its key)".into());
        }
        if o.no_tls && (o.cert.is_some() || o.acme.is_some()) {
            return Err("expose: --no-tls and --cert / --acme contradict each other".into());
        }
        if o.acme.is_some() && o.cert.is_some() {
            return Err("expose: --acme gets a certificate, --cert names one: give one of them".into());
        }
        if o.remove
            && (o.cert.is_some() || o.no_tls || o.acme.is_some() || !o.websocket.is_empty() || !o.sse.is_empty())
        {
            return Err("expose: --remove takes only hostnames (and --app, --no-reload, --dry-run)".into());
        }
    }
    Ok(Args { command: Command::Expose(Box::new(o)), ..crate::cli::empty_args() })
}

// ----------------------------------------------------------------- the file

/// What the site file is made from.
#[derive(Debug, Clone, PartialEq)]
pub struct Site {
    pub app: String,
    pub port: u16,
    pub hosts: Vec<String>,
    /// HTTPS on 443 (port 80 redirects), with these files or nginx's ACME module.
    pub tls: Option<Tls>,
    pub websocket_paths: Vec<String>,
    pub sse_paths: Vec<String>,
    /// nginx 1.25.1+ takes `http2 on;`; older ones `listen 443 ssl http2`.
    pub http2_directive: bool,
    /// Also listen on IPv6 (`[::]`): not on hosts without it, where nginx
    /// would refuse the file.
    pub ipv6: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tls {
    /// Certificate chain and key files.
    Files(PathBuf, PathBuf),
    /// A Let's Encrypt certificate that nginx gets and renews itself
    /// (ngx_http_acme_module), with this contact address.
    Acme(String),
}

/// Let's Encrypt's production directory.
pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// `api.v2` → `api_v2`: nginx variable and upstream names take [A-Za-z0-9_].
fn ident(app: &str) -> String {
    app.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

/// The site file: `contrib/nginx.conf`'s settings (the comments there say
/// why), with this app's port, names and paths. The map and upstream are
/// named after the app, so several apps' files live side by side.
pub fn render(s: &Site) -> String {
    let id = ident(&s.app);
    let names = s.hosts.join(" ");
    let port = s.port;
    let mut o = String::new();
    o.push_str(&format!(
        "{MARKER} for the app {app}; it rewrites this file\n\
         # (edits here are lost): change [expose] in the app's config, or run it again.\n\
         # Settings, and why: contrib/nginx.conf and docs/proxies.md in Warden.\n\
         \n\
         map $http_upgrade $warden_{id}_connection {{\n\
         \x20   default upgrade;\n\
         \x20   \"\"      \"\";\n\
         }}\n\
         \n\
         # Every worker of {app} shares port {port} (SO_REUSEPORT): one address, never\n\
         # taken out; the backup line lets nginx retry a reset connection once.\n\
         upstream warden_{id} {{\n\
         \x20   server 127.0.0.1:{port} max_fails=0;\n\
         \x20   server 127.0.0.1:{port} max_fails=0 backup;\n\
         \x20   keepalive 32;\n\
         \x20   keepalive_timeout 4s;\n\
         \x20   keepalive_requests 1000;\n\
         }}\n\n",
        app = s.app,
    ));
    if let Some(Tls::Acme(contact)) = &s.tls {
        // HTTP-01: Let's Encrypt asks for a file on port 80, which nginx
        // answers itself (before the redirect below runs).
        o.push_str(&format!(
            "# The certificate: nginx's ACME module gets it from Let's Encrypt and renews it.\n\
             acme_issuer warden_{id} {{\n\
             \x20   uri         {LETS_ENCRYPT};\n\
             \x20   contact     {contact};\n\
             \x20   accept_terms_of_service;\n\
             }}\n\n"
        ));
    }
    let v6_80 = if s.ipv6 { "    listen [::]:80;\n" } else { "" };
    if s.tls.is_some() {
        o.push_str(&format!(
            "server {{\n\
             \x20   listen 80;\n\
             {v6_80}\
             \x20   server_name {names};\n\
             \x20   location / {{\n\
             \x20       return 301 https://$host$request_uri;\n\
             \x20   }}\n\
             }}\n\n"
        ));
    }
    o.push_str("server {\n");
    match &s.tls {
        Some(tls) => {
            let h2 = if s.http2_directive { "" } else { " http2" };
            o.push_str(&format!("    listen 443 ssl{h2};\n"));
            if s.ipv6 {
                o.push_str(&format!("    listen [::]:443 ssl{h2};\n"));
            }
            if s.http2_directive {
                o.push_str("    http2 on;\n");
            }
            o.push_str(&format!("    server_name {names};\n"));
            match tls {
                Tls::Files(cert, key) => o.push_str(&format!(
                    "    ssl_certificate     {};\n    ssl_certificate_key {};\n",
                    cert.display(),
                    key.display()
                )),
                Tls::Acme(_) => o.push_str(&format!(
                    "    acme_certificate warden_{id};\n\
                     \x20   ssl_certificate       $acme_certificate;\n\
                     \x20   ssl_certificate_key   $acme_certificate_key;\n\
                     \x20   ssl_certificate_cache max=2;\n"
                )),
            }
        }
        None => o.push_str(&format!("    listen 80;\n{v6_80}    server_name {names};\n")),
    }
    o.push_str(&format!(
        "\n\
         \x20   keepalive_timeout 75s;\n\
         \x20   client_max_body_size 10m;\n\
         \n\
         \x20   proxy_http_version 1.1;\n\
         \x20   proxy_set_header Upgrade           $http_upgrade;\n\
         \x20   proxy_set_header Connection        $warden_{id}_connection;\n\
         \x20   proxy_set_header Host              $host;\n\
         \x20   proxy_set_header X-Real-IP         $remote_addr;\n\
         \x20   proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;\n\
         \x20   proxy_set_header X-Forwarded-Proto $scheme;\n\
         \x20   proxy_set_header X-Forwarded-Host  $host;\n\
         \x20   proxy_set_header X-Forwarded-Port  $server_port;\n\
         \n\
         \x20   proxy_connect_timeout 2s;\n\
         \x20   proxy_next_upstream error timeout;\n\
         \x20   proxy_next_upstream_tries 2;\n\
         \x20   proxy_next_upstream_timeout 10s;\n\
         \n\
         \x20   location / {{\n\
         \x20       proxy_pass http://warden_{id};\n\
         \x20       proxy_read_timeout 60s;\n\
         \x20       proxy_send_timeout 60s;\n\
         \x20   }}\n"
    ));
    for p in &s.websocket_paths {
        o.push_str(&format!(
            "\n\
             \x20   location {p} {{\n\
             \x20       proxy_pass http://warden_{id};\n\
             \x20       proxy_read_timeout 1h;\n\
             \x20       proxy_send_timeout 1h;\n\
             \x20       lingering_close always;\n\
             \x20   }}\n"
        ));
    }
    for p in &s.sse_paths {
        o.push_str(&format!(
            "\n\
             \x20   location {p} {{\n\
             \x20       proxy_pass http://warden_{id};\n\
             \x20       proxy_buffering off;\n\
             \x20       proxy_cache off;\n\
             \x20       proxy_read_timeout 1h;\n\
             \x20       gzip off;\n\
             \x20   }}\n"
        ));
    }
    o.push_str("}\n");
    o
}

/// nginx's version from `nginx -v` ("nginx version: nginx/1.24.0 (Ubuntu)").
pub fn parse_nginx_version(out: &str) -> Option<(u32, u32, u32)> {
    let v = out.split("nginx/").nth(1)?;
    let v: String = v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut n = v.split('.').map(|p| p.parse::<u32>().ok());
    Some((n.next()??, n.next()??, n.next().flatten().unwrap_or(0)))
}

// ------------------------------------------------------------ the app config

/// `text` with its `[expose]` table replaced by `section` (or removed when
/// `None`); the rest of the file, comments included, stays as it was.
pub fn replace_section(text: &str, section: Option<&str>) -> String {
    let header = |l: &str| l.trim_start().starts_with('[');
    let is_expose = |l: &str| {
        let t = l.trim();
        let t = t.split('#').next().unwrap_or("").trim();
        t == "[expose]"
    };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let start = lines.iter().position(|l| is_expose(l));
    let mut out = String::new();
    match start {
        Some(i) => {
            let end = lines[i + 1..].iter().position(|l| header(l)).map(|j| i + 1 + j).unwrap_or(lines.len());
            for l in &lines[..i] {
                out.push_str(l);
            }
            if let Some(s) = section {
                out.push_str(s);
                if end < lines.len() {
                    out.push('\n');
                }
            } else if end == lines.len() {
                // Removing the last table: drop the blank lines left before it.
                let trimmed = out.trim_end_matches('\n').len();
                out.truncate(trimmed);
                out.push('\n');
            }
            for l in &lines[end..] {
                out.push_str(l);
            }
        }
        None => {
            out.push_str(text);
            if let Some(s) = section {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(s);
            }
        }
    }
    out
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn toml_list(v: &[String]) -> String {
    format!("[{}]", v.iter().map(|s| toml_str(s)).collect::<Vec<_>>().join(", "))
}

pub fn section_text(x: &Expose) -> String {
    let mut s = String::from(
        "[expose]                                     # written by `warden expose`: nginx sends these hostnames here\n",
    );
    s.push_str(&format!("hosts = {}\n", toml_list(&x.hosts)));
    for (k, v) in [("cert", &x.cert), ("key", &x.key)] {
        if let Some(p) = v {
            s.push_str(&format!("{k} = {}\n", toml_str(&p.to_string_lossy())));
        }
    }
    if let Some(a) = &x.acme {
        s.push_str(&format!("acme = {}\n", toml_str(a)));
    }
    if let Some(p) = &x.site {
        s.push_str(&format!("site = {}\n", toml_str(&p.to_string_lossy())));
    }
    if !x.websocket_paths.is_empty() {
        s.push_str(&format!("websocket_paths = {}\n", toml_list(&x.websocket_paths)));
    }
    if !x.sse_paths.is_empty() {
        s.push_str(&format!("sse_paths = {}\n", toml_list(&x.sse_paths)));
    }
    s
}

/// What `[expose]` becomes: the hostnames added (or removed), the options
/// given now over the ones recorded. `None`: no hostname left.
pub fn merge(old: Option<&Expose>, o: &Opts, found_cert: Option<(PathBuf, PathBuf)>) -> Option<Expose> {
    let mut x = old.cloned().unwrap_or_default();
    if o.remove {
        x.hosts.retain(|h| !o.hosts.contains(h));
        if x.hosts.is_empty() {
            return None;
        }
        return Some(x);
    }
    for h in &o.hosts {
        if !x.hosts.contains(h) {
            x.hosts.push(h.clone());
        }
    }
    if o.no_tls {
        x.cert = None;
        x.key = None;
        x.acme = None;
    } else if o.cert.is_some() {
        x.cert = o.cert.clone();
        x.key = o.key.clone();
        x.acme = None;
    } else if o.acme.is_some() {
        x.acme = o.acme.clone();
        x.cert = None;
        x.key = None;
    } else if x.cert.is_none() && x.acme.is_none() {
        if let Some((c, k)) = found_cert {
            x.cert = Some(c);
            x.key = Some(k);
        }
    }
    if o.site.is_some() {
        x.site = o.site.clone();
    }
    for p in &o.websocket {
        if !x.websocket_paths.contains(p) {
            x.websocket_paths.push(p.clone());
        }
    }
    for p in &o.sse {
        if !x.sse_paths.contains(p) {
            x.sse_paths.push(p.clone());
        }
    }
    Some(x)
}

// ---------------------------------------------------------------- the host

/// Where nginx reads site files from: Debian/Ubuntu/RHEL's conf.d, Homebrew's servers/.
fn default_site_dir() -> Option<PathBuf> {
    ["/etc/nginx/conf.d", "/opt/homebrew/etc/nginx/servers", "/usr/local/etc/nginx/servers"]
        .iter()
        .map(PathBuf::from)
        .find(|d| d.is_dir())
}

fn nginx_bin() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/usr/local/sbin", "/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from))
        .map(|d| d.join("nginx"))
        .find(|p| p.is_file())
}

/// A Let's Encrypt certificate (certbot's layout) for the first hostname.
fn letsencrypt(host: &str) -> Option<(PathBuf, PathBuf)> {
    let d = Path::new("/etc/letsencrypt/live").join(host.trim_start_matches("*."));
    let (c, k) = (d.join("fullchain.pem"), d.join("privkey.pem"));
    (c.exists() && k.exists()).then_some((c, k))
}

fn run_cmd(prog: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running {}: {e}", prog.display()))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(text) } else { Err(text.trim_end().to_string()) }
}

/// `systemctl reload nginx` when systemd runs it, else `nginx -s reload`.
fn reload_nginx(nginx: &Path) -> Result<String, String> {
    let systemctl = Path::new("/bin/systemctl");
    let systemctl = if systemctl.is_file() { Some(systemctl.to_path_buf()) } else { which("systemctl") };
    if let Some(sc) = systemctl {
        if run_cmd(&sc, &["is-active", "--quiet", "nginx"]).is_ok() {
            return run_cmd(&sc, &["reload", "nginx"]).map(|_| "systemctl reload nginx".into());
        }
    }
    run_cmd(nginx, &["-s", "reload"]).map(|_| format!("{} -s reload", nginx.display()))
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(cmd)).find(|p| p.is_file())
}

/// The app's config file, from `--app` (name or id) or `-c`.
fn find_config(o: &Opts) -> Result<PathBuf, String> {
    if let Some(c) = &o.config {
        return Ok(c.clone());
    }
    let want = o.app.as_deref().unwrap_or_default();
    let names = crate::fleet::resolve_names(want)?;
    let [name] = names.as_slice() else {
        return Err(format!("expose: {want:?} is {} apps; name one", names.len()));
    };
    crate::fleet::discover()
        .into_iter()
        .find(|a| &a.name == name)
        .and_then(|a| a.config)
        .ok_or_else(|| format!("expose: {name} has no config file Warden knows of; use -c <config>"))
}

/// Put `bak` back as `path`, or remove `path` when there was none.
fn restore(path: &Path, bak: Option<&Path>) {
    let r = match bak {
        Some(b) => std::fs::copy(b, path).map(|_| ()),
        None => std::fs::remove_file(path),
    };
    if let Err(e) = r {
        eprintln!("warden: could not put {} back as it was: {e}", path.display());
    }
}

pub fn run(o: &Opts) -> i32 {
    if o.help {
        print!("{HELP}");
        return 0;
    }
    match run_inner(o) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("warden: {e}");
            1
        }
    }
}

fn run_inner(o: &Opts) -> Result<(), String> {
    let cfg_path = find_config(o)?;
    let cfg = Config::load(&cfg_path)?;
    let app = cfg.app.name.clone();
    let port = cfg.app.port.ok_or_else(|| {
        format!("expose: {app} has no port (app.port in {}): nginx needs one to send requests to", cfg_path.display())
    })?;
    let old = cfg.expose.clone();
    if o.remove {
        let known = old.as_ref().map(|x| x.hosts.clone()).unwrap_or_default();
        if let Some(h) = o.hosts.iter().find(|h| !known.contains(h)) {
            return Err(format!("expose: {app} is not exposed as {h} (it has: {})", list_or_none(&known)));
        }
    }
    let found = if o.cert.is_none() && o.acme.is_none() && !o.no_tls {
        o.hosts.first().and_then(|h| letsencrypt(h))
    } else {
        None
    };
    let found_note = found.clone().filter(|_| old.as_ref().is_none_or(|x| x.cert.is_none()));
    let new = merge(old.as_ref(), o, found);
    if let Some(x) = &new {
        x.check()?;
    }

    let site = o
        .site
        .clone()
        .or_else(|| old.as_ref().and_then(|x| x.site.clone()))
        .or_else(|| default_site_dir().map(|d| d.join(format!("warden-{app}.conf"))))
        .ok_or("expose: found no nginx conf.d (/etc/nginx/conf.d, Homebrew's servers/); give --site <file>")?;

    let nginx = nginx_bin();
    let version = nginx.as_deref().and_then(|n| run_cmd(n, &["-v"]).ok()).and_then(|v| parse_nginx_version(&v));
    let text = new.as_ref().map(|x| {
        render(&Site {
            app: app.clone(),
            port,
            hosts: x.hosts.clone(),
            tls: match (&x.acme, &x.cert, &x.key) {
                (Some(a), _, _) => Some(Tls::Acme(a.clone())),
                (None, Some(c), Some(k)) => Some(Tls::Files(c.clone(), k.clone())),
                _ => None,
            },
            websocket_paths: x.websocket_paths.clone(),
            sse_paths: x.sse_paths.clone(),
            http2_directive: version.is_none_or(|v| v >= (1, 25, 1)),
            ipv6: std::net::TcpListener::bind("[::1]:0").is_ok(),
        })
    });

    // The app's config with the new [expose], checked before anything is written.
    let cfg_text = std::fs::read_to_string(&cfg_path).map_err(|e| format!("reading {}: {e}", cfg_path.display()))?;
    let new_cfg_text = replace_section(&cfg_text, new.as_ref().map(section_text).as_deref());
    Config::parse(&new_cfg_text)
        .map_err(|e| format!("expose: the config would not be valid ({e}); {} is unchanged", cfg_path.display()))?;

    if o.dry_run {
        match &text {
            Some(t) => println!("# {} (dry run: nothing written)\n{t}", site.display()),
            None => println!("{} would be removed (dry run: nothing changed)", site.display()),
        }
        println!("# {} would get:\n{}", cfg_path.display(), new.as_ref().map(section_text).unwrap_or_default());
        return Ok(());
    }

    let nginx = nginx.ok_or("expose: nginx is not installed (or not on PATH); install it, or use --dry-run")?;
    let existing = std::fs::read_to_string(&site).ok();
    if existing.as_deref().is_some_and(|t| !t.starts_with(MARKER)) {
        return Err(format!(
            "expose: {} exists and was not written by warden expose; move it away, or pick another file with --site",
            site.display()
        ));
    }
    // Kept out of nginx's folder: Homebrew's nginx loads every file in servers/.
    let bak = crate::fleet::state_dir().join("expose").join(format!("{app}.conf.bak"));
    if existing.is_some() {
        if let Some(d) = bak.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
        }
        std::fs::copy(&site, &bak).map_err(|e| format!("keeping {} as {}: {e}", site.display(), bak.display()))?;
    }
    let bak_ref = existing.is_some().then_some(bak.as_path());
    // Read before the new file is in place: nginx -T fails on an issuer without a resolver.
    let need_resolver = new.as_ref().is_some_and(|x| x.acme.is_some())
        && !site.with_file_name("warden-resolver.conf").exists()
        && !has_resolver(&run_cmd(&nginx, &["-T"]).unwrap_or_default());
    match &text {
        Some(t) => crate::fleet::write_private(&site, t, 0o644)?,
        None => {
            if existing.is_some() {
                std::fs::remove_file(&site).map_err(|e| format!("removing {}: {e}", site.display()))?;
            }
        }
    }
    // The ACME module looks up Let's Encrypt with nginx's resolver: one for
    // every app, written once when nginx has none.
    let mut resolver_file = None;
    if need_resolver {
        match add_resolver(&site) {
            Ok(f) => resolver_file = f,
            Err(e) => {
                restore(&site, bak_ref);
                return Err(e);
            }
        }
    }
    if let Err(out) = run_cmd(&nginx, &["-t"]) {
        restore(&site, bak_ref);
        if let Some(f) = &resolver_file {
            let _ = std::fs::remove_file(f);
        }
        let hint = if out.contains("unknown directive \"acme_") {
            "\nnginx has no ACME module: install it (nginx.org's package nginx-module-acme, then \
             `load_module modules/ngx_http_acme_module.so;` at the top of nginx.conf), or use --cert/--key"
        } else {
            ""
        };
        return Err(format!(
            "`nginx -t` failed, so {} is back as it was and nginx was not reloaded:\n{out}{hint}",
            site.display()
        ));
    }

    // Recorded before the reload: the file nginx has now is the one the config describes.
    let cfg_bak = cfg_path.with_extension("toml.bak");
    std::fs::copy(&cfg_path, &cfg_bak)
        .map_err(|e| format!("keeping {} as {}: {e}", cfg_path.display(), cfg_bak.display()))?;
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&cfg_path).map_err(|e| e.to_string())?.permissions(),
    );
    crate::fleet::write_private(&cfg_path, &new_cfg_text, mode & 0o7777)?;

    let reloaded = if o.no_reload {
        None
    } else {
        Some(reload_nginx(&nginx).map_err(|e| {
            format!(
                "{} is written and checked, but reloading nginx failed: {e}\nreload it yourself to apply it",
                site.display()
            )
        })?)
    };

    match &new {
        Some(x) => {
            let scheme = if x.cert.is_some() || x.acme.is_some() { "https" } else { "http" };
            for h in &x.hosts {
                println!("{scheme}://{h} -> nginx -> 127.0.0.1:{port} ({app})");
            }
            println!("site file: {}", site.display());
            if let Some((c, _)) = &found_note {
                println!("certificate: {} (found in /etc/letsencrypt; --no-tls for plain HTTP)", c.display());
            }
            if let Some(f) = &resolver_file {
                println!("resolver for the ACME module: {} (nginx had none)", f.display());
            }
            if x.acme.is_some() {
                println!(
                    "certificate: nginx gets it from Let's Encrypt and renews it (ACME module); port 80 must be \
                     reachable for its check. Until it arrives, HTTPS handshakes fail: see nginx's error log"
                );
            }
            if x.cert.is_none() && x.acme.is_none() {
                println!(
                    "plain HTTP on port 80: for TLS that ends at Cloudflare or a load balancer. For HTTPS here, \
                     run again with --acme <email> (Let's Encrypt via nginx) or --cert <fullchain.pem> --key <privkey.pem>"
                );
            }
        }
        None => println!("{app} is no longer exposed: removed {} (a copy is in {})", site.display(), bak.display()),
    }
    println!("{}: [expose] updated (previous copy: {})", cfg_path.display(), cfg_bak.display());
    match reloaded {
        Some(how) => println!("nginx reloaded ({how})"),
        None => println!("nginx not reloaded (--no-reload): run `nginx -s reload` or `systemctl reload nginx`"),
    }
    Ok(())
}

/// `resolver` for nginx's http context, which has none, from the first
/// nameserver in /etc/resolv.conf, as `warden-resolver.conf` next to the site
/// file.
fn add_resolver(site: &Path) -> Result<Option<PathBuf>, String> {
    let file = site.with_file_name("warden-resolver.conf");
    let conf = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let ns = nameserver(&conf).ok_or(
        "expose: --acme needs a DNS resolver in nginx's http block (`resolver 1.1.1.1;`), and /etc/resolv.conf \
         names no nameserver to write one from",
    )?;
    let text = format!(
        "{MARKER}: nginx's ACME module resolves Let's Encrypt with it.\n\
         # From /etc/resolv.conf; change it freely (expose writes it only when nginx has no resolver).\n\
         resolver {ns} valid=300s;\n"
    );
    crate::fleet::write_private(&file, &text, 0o644)?;
    Ok(Some(file))
}

fn has_resolver(nginx_t: &str) -> bool {
    nginx_t.lines().any(|l| l.split('#').next().unwrap_or("").trim_start().starts_with("resolver "))
}

/// The first nameserver of resolv.conf, bracketed when IPv6 (`[::1]`).
fn nameserver(resolv: &str) -> Option<String> {
    let ip = resolv.lines().find_map(|l| l.trim().strip_prefix("nameserver")?.split_whitespace().next())?;
    let ip: std::net::IpAddr = ip.split('%').next()?.parse().ok()?;
    Some(match ip {
        std::net::IpAddr::V4(a) => a.to_string(),
        std::net::IpAddr::V6(a) => format!("[{a}]"),
    })
}

fn list_or_none(v: &[String]) -> String {
    if v.is_empty() { "none".into() } else { v.join(", ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Result<Opts, String> {
        let v: Vec<String> = s.split_whitespace().map(String::from).collect();
        match parse_args(&v)?.command {
            Command::Expose(o) => Ok(*o),
            _ => unreachable!(),
        }
    }

    #[test]
    fn parses() {
        let o = args("api.example.com,www.example.com --app api --websocket /ws --sse=/events").unwrap();
        assert_eq!(o.hosts, ["api.example.com", "www.example.com"]);
        assert_eq!(o.app.as_deref(), Some("api"));
        assert_eq!(o.websocket, ["/ws"]);
        assert_eq!(o.sse, ["/events"]);
        assert!(args("API.Example.com --app api").unwrap().hosts == ["api.example.com"]);
        assert!(args("--app api").unwrap_err().contains("needs a hostname"));
        assert!(args("api.example.com").unwrap_err().contains("needs the app"));
        assert!(args("bad_host --app api").unwrap_err().contains("not a hostname"));
        assert!(args("a.com --app api --cert x").unwrap_err().contains("go together"));
        assert!(args("a.com --app api --remove --no-tls").unwrap_err().contains("--remove takes only"));
        assert_eq!(args("a.com --app api --acme me@a.com").unwrap().acme.as_deref(), Some("me@a.com"));
        assert!(args("a.com --app api --acme me@a.com --cert c --key k").unwrap_err().contains("give one"));
        assert!(args("--help").unwrap().help);
        assert!(args("a.com --app api --frob").unwrap_err().contains("unknown option"));
    }

    #[test]
    fn hostnames() {
        for ok in ["api.example.com", "*.example.com", "localhost", "a-b.c0.io"] {
            assert!(config::valid_hostname(ok).is_ok(), "{ok}");
        }
        for bad in ["", "*.", "a..b", "-a.com", "a_b.com", "a.com;", "a b", "api.*.com", "*"] {
            assert!(config::valid_hostname(bad).is_err(), "{bad}");
        }
    }

    fn site(tls: bool) -> Site {
        Site {
            app: "api.v2".into(),
            port: 3000,
            hosts: vec!["api.example.com".into(), "www.example.com".into()],
            tls: tls.then(|| Tls::Files("/c/full.pem".into(), "/c/key.pem".into())),
            websocket_paths: vec!["/ws".into()],
            sse_paths: vec!["/events".into()],
            http2_directive: true,
            ipv6: true,
        }
    }

    #[test]
    fn renders_plain_http() {
        let t = render(&site(false));
        assert!(t.starts_with(MARKER));
        assert!(t.contains("map $http_upgrade $warden_api_v2_connection {"));
        assert!(t.contains("upstream warden_api_v2 {"));
        assert!(t.contains("server 127.0.0.1:3000 max_fails=0 backup;"));
        assert!(t.contains("server_name api.example.com www.example.com;"));
        assert!(t.contains("    listen 80;\n    listen [::]:80;\n"));
        assert!(!t.contains("443"));
        assert!(t.contains("location /ws {") && t.contains("lingering_close always;"));
        assert!(t.contains("location /events {") && t.contains("proxy_buffering off;"));
        assert_eq!(t.matches('{').count(), t.matches('}').count());
    }

    #[test]
    fn renders_tls() {
        let t = render(&site(true));
        assert!(t.contains("return 301 https://$host$request_uri;"));
        assert!(t.contains("listen 443 ssl;\n") && t.contains("http2 on;"));
        assert!(t.contains("ssl_certificate     /c/full.pem;"));
        let old = render(&Site { http2_directive: false, ..site(true) });
        assert!(old.contains("listen 443 ssl http2;") && !old.contains("http2 on;"));
        let v4 = render(&Site { ipv6: false, ..site(true) });
        assert!(!v4.contains("[::]") && v4.contains("listen 80;") && v4.contains("listen 443 ssl;"));
    }

    /// The settings that make restarts invisible through nginx stay the same
    /// as in the documented contrib/nginx.conf.
    #[test]
    fn matches_contrib() {
        let contrib = include_str!("../contrib/nginx.conf");
        let t = render(&site(false));
        for d in [
            "max_fails=0 backup;",
            "keepalive 32;",
            "keepalive_timeout 4s;",
            "keepalive_requests 1000;",
            "proxy_http_version 1.1;",
            "proxy_connect_timeout 2s;",
            "proxy_next_upstream error timeout;",
            "proxy_next_upstream_tries 2;",
            "proxy_next_upstream_timeout 10s;",
            "keepalive_timeout 75s;",
            "client_max_body_size 10m;",
            "proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;",
            "lingering_close always;",
            "proxy_buffering off;",
        ] {
            assert!(contrib.contains(d), "contrib lost {d}");
            assert!(t.contains(d), "expose lost {d}");
        }
    }

    #[test]
    fn renders_acme() {
        let t = render(&Site { tls: Some(Tls::Acme("me@example.com".into())), ..site(true) });
        assert!(t.contains("\nacme_issuer warden_api_v2 {\n    uri         "));
        assert!(t.contains(&format!("uri         {LETS_ENCRYPT};")));
        assert!(t.contains("contact     me@example.com;") && t.contains("accept_terms_of_service;"));
        assert!(t.contains("acme_certificate warden_api_v2;"));
        assert!(t.contains("ssl_certificate       $acme_certificate;"));
        assert!(t.contains("ssl_certificate_key   $acme_certificate_key;"));
        assert!(t.contains("return 301 https://") && t.contains("listen 443 ssl;"));
        assert_eq!(t.matches('{').count(), t.matches('}').count());
    }

    #[test]
    fn resolvers() {
        assert_eq!(nameserver("# x\nnameserver 127.0.0.53\nnameserver 8.8.8.8\n").as_deref(), Some("127.0.0.53"));
        assert_eq!(nameserver("nameserver fe80::1%eth0\n").as_deref(), Some("[fe80::1]"));
        assert_eq!(nameserver("search lan\n"), None);
        assert!(has_resolver("http {\n    resolver 1.1.1.1;\n"));
        assert!(!has_resolver("http {\n    # resolver 1.1.1.1;\n    resolver_timeout 5s;\n"));
    }

    #[test]
    fn nginx_version() {
        assert_eq!(parse_nginx_version("nginx version: nginx/1.24.0 (Ubuntu)\n"), Some((1, 24, 0)));
        assert_eq!(parse_nginx_version("nginx version: nginx/1.31.6\n"), Some((1, 31, 6)));
        assert_eq!(parse_nginx_version("nope"), None);
    }

    #[test]
    fn section_round_trip() {
        let base = "[app]\nname = \"api\"\ncommand = \"bun\"\nport = 3000 # the port\n\n[workers]\ncount = 2\n";
        let x = Expose {
            hosts: vec!["api.example.com".into()],
            cert: Some("/c/full.pem".into()),
            key: Some("/c/key.pem".into()),
            ..Default::default()
        };
        let added = replace_section(base, Some(&section_text(&x)));
        assert!(added.starts_with(base));
        let cfg = Config::parse(&added).unwrap();
        assert_eq!(cfg.expose.as_ref(), Some(&x));

        // Replaced in place, with the tables after it kept.
        let mid = "[app]\nname = \"api\"\nport = 3000\n\n[expose] # old\nhosts = [\"old.example.com\"]\n\n[workers]\ncount = 2\n";
        let y = Expose { hosts: vec!["new.example.com".into()], ..Default::default() };
        let r = replace_section(mid, Some(&section_text(&y)));
        assert!(!r.contains("old.example.com") && r.contains("new.example.com") && r.contains("[workers]\ncount = 2"));
        assert_eq!(Config::parse(&r).unwrap().expose.unwrap().hosts, ["new.example.com"]);

        // Removed.
        let gone = replace_section(&added, None);
        assert_eq!(gone, base);
        assert!(Config::parse(&replace_section(mid, None)).unwrap().expose.is_none());
    }

    #[test]
    fn config_checks() {
        let base = "[app]\nname = \"api\"\nport = 3000\n";
        assert!(Config::parse(&format!("{base}[expose]\nhosts = []\n")).unwrap_err().contains("at least one"));
        assert!(
            Config::parse(&format!("{base}[expose]\nhosts = [\"a.com\"]\ncert = \"/x\"\n"))
                .unwrap_err()
                .contains("go together")
        );
        assert!(
            Config::parse("[app]\nname = \"api\"\n[expose]\nhosts = [\"a.com\"]\n").unwrap_err().contains("app.port")
        );
        assert!(Config::parse(&format!("{base}[expose]\nhosts = [\"a.com\"]\nwebsocket_paths = [\"ws\"]\n")).is_err());
        assert!(Config::parse(&format!("{base}[expose]\nhosts = [\"a.com\"]\nacme = \"me@a.com\"\n")).is_ok());
        assert!(
            Config::parse(&format!("{base}[expose]\nhosts = [\"*.a.com\"]\nacme = \"me@a.com\"\n"))
                .unwrap_err()
                .contains("wildcard")
        );
        assert!(Config::parse(&format!("{base}[expose]\nhosts = [\"a.com\"]\nacme = \"nope\"\n")).is_err());
    }

    #[test]
    fn merging() {
        let old = Expose { hosts: vec!["a.com".into()], ..Default::default() };
        let o =
            Opts { hosts: vec!["b.com".into(), "a.com".into()], websocket: vec!["/ws".into()], ..Default::default() };
        let m = merge(Some(&old), &o, Some(("/le/c".into(), "/le/k".into()))).unwrap();
        assert_eq!(m.hosts, ["a.com", "b.com"]);
        assert_eq!(m.cert.as_deref(), Some(Path::new("/le/c")));
        assert_eq!(m.websocket_paths, ["/ws"]);
        // --no-tls drops a recorded certificate.
        let a = merge(
            Some(&m),
            &Opts { hosts: vec!["a.com".into()], acme: Some("me@a.com".into()), ..Default::default() },
            None,
        )
        .unwrap();
        assert!(a.cert.is_none() && a.acme.as_deref() == Some("me@a.com"));
        let n =
            merge(Some(&m), &Opts { hosts: vec!["a.com".into()], no_tls: true, ..Default::default() }, None).unwrap();
        assert!(n.cert.is_none() && n.key.is_none());
        // --remove: one host, then the last.
        let r =
            merge(Some(&m), &Opts { hosts: vec!["a.com".into()], remove: true, ..Default::default() }, None).unwrap();
        assert_eq!(r.hosts, ["b.com"]);
        assert!(
            merge(Some(&r), &Opts { hosts: vec!["b.com".into()], remove: true, ..Default::default() }, None).is_none()
        );
    }
}
