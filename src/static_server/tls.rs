//! HTTPS for `[static]` sites: the certificate loader and the TLS accept
//! step shared by the two encrypted adaptors (HTTP/1.1 over TLS in
//! `conn.rs`, HTTP/2 in `http2.rs`). The plain-HTTP path never touches this.
//!
//! After the handshake the connection is dispatched by ALPN: `h2` goes to
//! the HTTP/2 driver, anything else (including `http/1.1`, or no ALPN from
//! an old client) goes to the HTTP/1.1 stack over the TLS stream. Nothing
//! is decrypted before this point, which is what the :443 router needs.

use std::path::Path;
use std::sync::Arc;

/// A TLS stream of any transport (a socket in production, a memory duplex
/// in tests).
pub(super) type TlsIo<IO> = tokio_rustls::server::TlsStream<IO>;

/// What the client asked for in ALPN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Negotiated {
    Http2,
    Http1,
}

/// The server's TLS configuration from a certificate chain and its key.
/// The error says which file is at fault, for the worker's startup message.
pub(super) fn load(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>, String> {
    // The process default, once (a later call says it is already set, which
    // is fine: every worker and test shares the one provider).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert_pem = std::fs::read(cert).map_err(|e| format!("static.cert {}: {e}", cert.display()))?;
    let key_pem = std::fs::read(key).map_err(|e| format!("static.key {}: {e}", key.display()))?;
    let chain: Vec<_> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("static.cert {}: {e}", cert.display()))?;
    if chain.is_empty() {
        return Err(format!("static.cert {}: no certificate in the file", cert.display()));
    }
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| format!("static.key {}: {e}", key.display()))?
        .ok_or_else(|| format!("static.key {}: no private key in the file", key.display()))?;
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| format!("static.cert/static.key: {e}"))?;
    // Session tickets: a returning client resumes instead of handshaking
    // fully. Each worker holds its own ticket keys, so a client landing on
    // another worker handshakes fully (the fallback is inherent: offering
    // no usable ticket just means a full handshake, never an error).
    cfg.ticketer = rustls::crypto::ring::Ticketer::new().map_err(|e| format!("session tickets: {e}"))?;
    // Secret extraction for the kTLS handover: the kernel takes the socket
    // with the handshake's traffic secrets and sequence numbers. Harmless
    // on connections that stay in userspace (nothing is extracted), and
    // required for the ones the kernel takes.
    cfg.enable_secret_extraction = true;
    // HTTP/2 first: a client that speaks it says so, and the H2 driver is
    // what its streams need. HTTP/1.1 (or nothing) takes the H1 stack.
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// The TLS handshake over `io` (a fresh socket, still fully encrypted).
pub(super) async fn accept<IO>(acceptor: &tokio_rustls::TlsAcceptor, io: IO) -> std::io::Result<TlsIo<IO>>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    acceptor.accept(io).await
}

/// What a finished handshake negotiated.
pub(super) fn negotiated<IO>(tls: &TlsIo<IO>) -> Negotiated {
    match tls.get_ref().1.alpn_protocol() {
        Some(b"h2") => Negotiated::Http2,
        _ => Negotiated::Http1,
    }
}

/// Test certificates, shared by the TLS tests of every adaptor: a
/// self-signed chain + key on disk, as the config names them.
#[cfg(test)]
pub(super) fn test_cert_files(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::io::Write;
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("a test certificate");
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    let mut f = std::fs::File::create(&cert).unwrap();
    f.write_all(certified.cert.pem().as_bytes()).unwrap();
    let mut f = std::fs::File::create(&key).unwrap();
    f.write_all(certified.key_pair.serialize_pem().as_bytes()).unwrap();
    (cert, key)
}

#[cfg(test)]
pub(super) fn test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("warden-tls-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Client config trusting `cert_pem` (a chain file's bytes), offering `alpn`.
#[cfg(test)]
pub(super) fn test_client(alpn: &[u8], cert_pem: &[u8]) -> tokio_rustls::TlsConnector {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert: Vec<_> = rustls_pemfile::certs(&mut std::io::Cursor::new(cert_pem)).collect::<Result<_, _>>().unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.into_iter().next().unwrap()).unwrap();
    let mut cfg = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    cfg.alpn_protocols = vec![alpn.to_vec()];
    tokio_rustls::TlsConnector::from(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_chain_and_key_load_and_mismatches_do_not() {
        let dir = test_dir("load");
        let (cert, key) = test_cert_files(&dir);
        assert!(load(&cert, &key).is_ok(), "a chain with its key loads");
        let e = load(&cert, &dir.join("missing.pem")).unwrap_err();
        assert!(e.contains("static.key"), "names the file: {e}");
        // A key that is not this certificate's: rustls refuses the pair.
        let other = test_dir("other");
        let (_, other_key) = test_cert_files(&other);
        let e = load(&cert, &other_key).unwrap_err();
        assert!(e.contains("static.cert/static.key"), "names the pair: {e}");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
    }

    /// Handshake `client` against `server` over a memory duplex: what the
    /// server negotiates for each ALPN offer.
    #[tokio::test]
    async fn alpn_dispatches_h2_and_http1() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = test_dir("alpn");
        let (cert, key) = test_cert_files(&dir);
        let cert_pem = std::fs::read(&cert).unwrap();
        let server = tokio_rustls::TlsAcceptor::from(load(&cert, &key).unwrap());
        for (offer, want) in [(b"h2".as_slice(), Negotiated::Http2), (b"http/1.1".as_slice(), Negotiated::Http1)] {
            let (client_io, server_io) = tokio::io::duplex(1 << 16);
            let connector = test_client(offer, &cert_pem);
            let acceptor = server.clone();
            let server_task =
                tokio::spawn(async move { accept(&acceptor, server_io).await.expect("the handshake runs") });
            let _client =
                connector.connect("localhost".try_into().unwrap(), client_io).await.expect("client handshakes");
            let tls = server_task.await.unwrap();
            assert_eq!(negotiated(&tls), want, "ALPN {offer:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A client that keeps its ticket store resumes: the second handshake is
    /// abbreviated on both ends (no certificate leg), the first is full.
    #[tokio::test]
    async fn tickets_resume_without_a_full_handshake() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = test_dir("resume");
        let (cert, key) = test_cert_files(&dir);
        let cert_pem = std::fs::read(&cert).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(load(&cert, &key).unwrap());
        // One client config: its in-memory ticket store persists across its
        // connections, as a browser's does.
        let connector = test_client(b"http/1.1", &cert_pem);

        // Handshake 1 is full; a byte each way after it makes sure the
        // client has processed the server's post-handshake tickets before
        // it connects again.
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let first = acceptor.clone();
        let server = tokio::spawn(async move {
            let mut tls = accept(&first, server_io).await.expect("handshake 1 runs");
            let kind = tls.get_ref().1.handshake_kind();
            let mut one = [0u8; 4];
            tokio::io::AsyncReadExt::read_exact(&mut tls, &mut one).await.unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut tls, b"pong").await.unwrap();
            kind
        });
        let connector2 = connector.clone();
        let mut client =
            connector.connect("localhost".try_into().unwrap(), client_io).await.expect("client 1 handshakes");
        assert_eq!(client.get_ref().1.handshake_kind(), Some(rustls::HandshakeKind::Full));
        client.write_all(b"ping").await.unwrap();
        let mut four = [0u8; 4];
        client.read_exact(&mut four).await.unwrap();
        assert_eq!(&four, b"pong");
        assert_eq!(server.await.unwrap(), Some(rustls::HandshakeKind::Full));

        // Handshake 2 offers the stored ticket: abbreviated on both ends.
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(async move {
            let tls = accept(&acceptor, server_io).await.expect("handshake 2 runs");
            tls.get_ref().1.handshake_kind()
        });
        let client = connector2.connect("localhost".try_into().unwrap(), client_io).await.expect("client 2 handshakes");
        assert_eq!(client.get_ref().1.handshake_kind(), Some(rustls::HandshakeKind::Resumed));
        assert_eq!(server.await.unwrap(), Some(rustls::HandshakeKind::Resumed));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
