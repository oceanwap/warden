//! Certificates on disk: one directory per issued certificate and a `live`
//! symlink swapped atomically, so a reader sees the old files or the new ones,
//! never half of either, and a failed renewal changes nothing.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Issued certificates kept per app (the newest; `live` is always among them).
const KEEP: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub hosts: Vec<String>,
    /// Unix seconds.
    pub not_before: i64,
    pub not_after: i64,
    /// A self-signed stand-in, not a real certificate.
    pub placeholder: bool,
}

/// Validity dates and names of the first certificate of a PEM chain.
pub fn meta_of(chain_pem: &str, hosts: &[String], placeholder: bool) -> Result<Meta, String> {
    let der = rustls_pemfile::certs(&mut chain_pem.as_bytes())
        .next()
        .ok_or("no certificate in the chain")?
        .map_err(|e| format!("certificate chain: {e}"))?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).map_err(|e| format!("certificate: {e}"))?;
    let v = cert.validity();
    Ok(Meta {
        hosts: hosts.to_vec(),
        not_before: v.not_before.timestamp(),
        not_after: v.not_after.timestamp(),
        placeholder,
    })
}

pub fn read_meta(app_dir: &Path) -> Option<Meta> {
    serde_json::from_slice(&std::fs::read(app_dir.join("live").join("meta.json")).ok()?).ok()
}

/// When to renew: with a third of the lifetime left.
pub fn renew_at(m: &Meta) -> i64 {
    m.not_before + (m.not_after - m.not_before) * 2 / 3
}

/// Whether an order is due: no real certificate, other hostnames than the
/// config names, or a third of the lifetime left.
pub fn due(m: Option<&Meta>, hosts: &[String], now: i64) -> bool {
    match m {
        None => true,
        Some(m) => m.placeholder || !same_names(&m.hosts, hosts) || now >= renew_at(m),
    }
}

/// The same names, whatever order the CA put them in (a mismatch would order again every check).
fn same_names(a: &[String], b: &[String]) -> bool {
    let sorted = |v: &[String]| {
        let mut v: Vec<String> = v.iter().map(|h| h.to_ascii_lowercase()).collect();
        v.sort();
        v
    };
    sorted(a) == sorted(b)
}

/// A self-signed certificate for `hosts`, valid for a month.
pub fn placeholder(hosts: &[String]) -> Result<(String, String), String> {
    let err = |e: rcgen::Error| format!("placeholder certificate: {e}");
    let mut params = rcgen::CertificateParams::new(hosts.to_vec()).map_err(err)?;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(30);
    let key = rcgen::KeyPair::generate().map_err(err)?;
    let cert = params.self_signed(&key).map_err(err)?;
    Ok((cert.pem(), key.serialize_pem()))
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// A private file replaced in one step; the old one is kept as `<name>.bak`.
pub fn write_atomic_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("new");
    let _ = std::fs::remove_file(&tmp);
    write_private(&tmp, bytes)?;
    if path.exists() {
        let _ = std::fs::copy(path, path.with_extension("bak"));
    }
    std::fs::rename(&tmp, path)
}

/// Stores a certificate and makes it `live`. `real: false` marks a placeholder.
pub fn install(app_dir: &Path, chain_pem: &str, key_pem: &str, real: bool) -> Result<(), String> {
    let io = |what: &str| {
        let what = what.to_string();
        move |e: std::io::Error| format!("{what}: {e}")
    };
    // The names come from the certificate itself, so a changed config is noticed.
    let hosts = hosts_of(chain_pem)?;
    let meta = meta_of(chain_pem, &hosts, !real)?;
    std::fs::create_dir_all(app_dir).map_err(io(&app_dir.display().to_string()))?;
    std::fs::set_permissions(app_dir, std::fs::Permissions::from_mode(0o700)).map_err(io("tls directory"))?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let name = format!("v{nanos:020}");
    let vd = app_dir.join(&name);
    std::fs::create_dir(&vd).map_err(io("new certificate directory"))?;
    write_private(&vd.join("fullchain.pem"), chain_pem.as_bytes()).map_err(io("fullchain.pem"))?;
    write_private(&vd.join("privkey.pem"), key_pem.as_bytes()).map_err(io("privkey.pem"))?;
    write_private(&vd.join("meta.json"), &serde_json::to_vec(&meta).map_err(|e| e.to_string())?)
        .map_err(io("meta.json"))?;
    // fsync the directory so the files exist before `live` points at them.
    std::fs::File::open(&vd).and_then(|d| d.sync_all()).map_err(io("sync"))?;
    let tmp = app_dir.join("live.new");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&name, &tmp).map_err(io("live link"))?;
    std::fs::rename(&tmp, app_dir.join("live")).map_err(io("live link"))?;
    let _ = std::fs::File::open(app_dir).and_then(|d| d.sync_all());
    prune(app_dir, &name);
    Ok(())
}

/// The DNS names of a certificate's SAN (the hostnames the config asked for).
fn hosts_of(chain_pem: &str) -> Result<Vec<String>, String> {
    use x509_parser::extensions::GeneralName;
    let der = rustls_pemfile::certs(&mut chain_pem.as_bytes())
        .next()
        .ok_or("no certificate in the chain")?
        .map_err(|e| format!("certificate chain: {e}"))?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).map_err(|e| format!("certificate: {e}"))?;
    let san = cert.subject_alternative_name().map_err(|e| format!("certificate: {e}"))?;
    Ok(san
        .map(|s| {
            s.value
                .general_names
                .iter()
                .filter_map(|n| if let GeneralName::DNSName(d) = n { Some(d.to_string()) } else { None })
                .collect()
        })
        .unwrap_or_default())
}

fn prune(app_dir: &Path, live: &str) {
    let Ok(rd) = std::fs::read_dir(app_dir) else { return };
    let mut all: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('v')))
        .collect();
    all.sort();
    let n = all.len().saturating_sub(KEEP);
    for p in all.into_iter().take(n) {
        if p.file_name().is_some_and(|n| n.to_string_lossy() != live) {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("warden-autotls-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn install_swaps_live_and_keeps_three() {
        let d = dir("install");
        let hosts = vec!["a.test".to_string(), "b.test".to_string()];
        let (c, k) = placeholder(&hosts).unwrap();
        install(&d, &c, &k, false).unwrap();
        let m = read_meta(&d).unwrap();
        assert!(m.placeholder);
        assert_eq!(m.hosts, hosts);
        assert!(due(Some(&m), &hosts, 0), "a placeholder is always due");
        for _ in 0..4 {
            install(&d, &c, &k, true).unwrap();
        }
        let m = read_meta(&d).unwrap();
        assert!(!m.placeholder);
        assert!(!due(Some(&m), &hosts, m.not_before + 1));
        assert!(due(Some(&m), &["c.test".to_string()], m.not_before + 1), "other hostnames: due");
        let swapped = vec![hosts[1].clone(), hosts[0].clone()];
        assert!(!due(Some(&m), &swapped, m.not_before + 1), "the CA's order of names does not matter");
        assert!(due(Some(&m), &hosts, renew_at(&m)), "a third left: due");
        let versions = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('v'))
            .count();
        assert_eq!(versions, KEEP);
        let mode = std::fs::metadata(d.join("live/privkey.pem")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_broken_chain_changes_nothing() {
        let d = dir("broken");
        let hosts = vec!["a.test".to_string()];
        let (c, k) = placeholder(&hosts).unwrap();
        install(&d, &c, &k, true).unwrap();
        let before = std::fs::read_link(d.join("live")).unwrap();
        assert!(install(&d, "not a certificate", &k, true).is_err());
        assert_eq!(std::fs::read_link(d.join("live")).unwrap(), before);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn retries_back_off_to_twelve_hours() {
        assert_eq!(super::super::backoff(1).as_secs(), 60);
        assert_eq!(super::super::backoff(4).as_secs(), 3600);
        assert_eq!(super::super::backoff(50).as_secs(), 12 * 3600);
    }
}
