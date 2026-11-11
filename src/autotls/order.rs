//! One ACME order over HTTP-01 (instant-acme). The challenge answer is a file
//! in `challenges/` that the port 80 responder of any supervisor serves.

use crate::config::Tls;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
    RetryPolicy,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Removes the token files when the order ends, however it ends.
struct Tokens(Vec<PathBuf>);

impl Drop for Tokens {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

async fn account(t: &Tls, base: &Path) -> Result<Account, String> {
    let url = t.directory_url();
    // A root certificate for a private CA or a test server (Pebble).
    let builder = match std::env::var_os("WARDEN_ACME_ROOT") {
        Some(p) => Account::builder_with_root(p),
        None => Account::builder(),
    }
    .map_err(|e| format!("ACME client: {e}"))?;
    let name: String = url.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).take(80).collect();
    let path = base.join(format!("account-{name}.json"));
    if let Ok(bytes) = std::fs::read(&path) {
        let creds: AccountCredentials =
            serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        return builder.from_credentials(creds).await.map_err(|e| format!("ACME account: {e}"));
    }
    let contact = format!("mailto:{}", t.email);
    let (account, creds) = builder
        .create(
            &NewAccount { contact: &[&contact], terms_of_service_agreed: true, only_return_existing: false },
            url,
            None,
        )
        .await
        .map_err(|e| format!("creating the ACME account: {e}"))?;
    let json = serde_json::to_vec_pretty(&creds).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(base).map_err(|e| format!("{}: {e}", base.display()))?;
    super::store::write_atomic_private(&path, &json).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(account)
}

/// Runs `tls.dns_hook` with `present` or `cleanup` for one TXT record.
async fn hook(argv: &[String], what: &str, domain: &str, value: &str) -> Result<(), String> {
    let run = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .arg(what)
        .env("WARDEN_ACME_DOMAIN", domain)
        .env("WARDEN_ACME_RECORD", format!("_acme-challenge.{domain}"))
        .env("WARDEN_ACME_VALUE", value)
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(Duration::from_secs(120), run).await {
        Err(_) => Err(format!("tls.dns_hook {what} took more than 120 s")),
        Ok(Err(e)) => Err(format!("tls.dns_hook {}: {e}", argv[0])),
        Ok(Ok(s)) if !s.success() => Err(format!("tls.dns_hook {what} failed: {s}")),
        Ok(Ok(_)) => Ok(()),
    }
}

/// Orders a certificate for `t.hosts`: the chain and its private key (PEM).
pub async fn issue(t: &Tls, base: &Path, challenges: &Path) -> Result<(String, String), String> {
    let mut records = vec![];
    let result = order(t, base, challenges, &mut records).await;
    for (domain, value) in records {
        if let Some(argv) = &t.dns_hook {
            if let Err(e) = hook(argv, "cleanup", &domain, &value).await {
                crate::warn!("DNS challenge record not removed", reason = e);
            }
        }
    }
    result
}

async fn order(
    t: &Tls,
    base: &Path,
    challenges: &Path,
    records: &mut Vec<(String, String)>,
) -> Result<(String, String), String> {
    let account = account(t, base).await?;
    let ids: Vec<Identifier> = t.hosts.iter().map(|h| Identifier::Dns(h.clone())).collect();
    let mut order = account.new_order(&NewOrder::new(&ids)).await.map_err(|e| format!("new order: {e}"))?;
    std::fs::create_dir_all(challenges).map_err(|e| format!("{}: {e}", challenges.display()))?;
    let mut tokens = Tokens(vec![]);
    let mut authzs = order.authorizations();
    while let Some(a) = authzs.next().await {
        let mut a = a.map_err(|e| format!("authorization: {e}"))?;
        match a.status {
            AuthorizationStatus::Pending => {}
            AuthorizationStatus::Valid => continue,
            s => return Err(format!("authorization is {s:?}")),
        }
        if let Some(argv) = &t.dns_hook {
            let mut ch = a.challenge(ChallengeType::Dns01).ok_or("the CA offers no DNS-01 challenge")?;
            let Identifier::Dns(domain) = ch.identifier().identifier.clone() else {
                return Err("not a DNS name".into());
            };
            let value = ch.key_authorization().dns_value();
            records.push((domain.clone(), value.clone()));
            hook(argv, "present", &domain, &value).await?;
            tokio::time::sleep(Duration::from_secs(t.dns_wait)).await;
            ch.set_ready().await.map_err(|e| format!("challenge: {e}"))?;
            continue;
        }
        let mut ch = a.challenge(ChallengeType::Http01).ok_or("the CA offers no HTTP-01 challenge")?;
        // The CA names the file: nothing but a token's own characters may reach the path.
        if ch.token.is_empty() || !ch.token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err("the CA sent a challenge token that is not a plain token".into());
        }
        let file = challenges.join(&ch.token);
        std::fs::write(&file, ch.key_authorization().as_str()).map_err(|e| format!("{}: {e}", file.display()))?;
        tokens.0.push(file);
        ch.set_ready().await.map_err(|e| format!("challenge: {e}"))?;
    }
    let policy = RetryPolicy::new().timeout(Duration::from_secs(120));
    let status = order.poll_ready(&policy).await.map_err(|e| format!("validation: {e}"))?;
    if status != OrderStatus::Ready {
        return Err(format!("the CA could not validate the names (order is {status:?})"));
    }
    let key = order.finalize().await.map_err(|e| format!("finalize: {e}"))?;
    let chain = order.poll_certificate(&policy).await.map_err(|e| format!("certificate: {e}"))?;
    Ok((chain, key))
}
