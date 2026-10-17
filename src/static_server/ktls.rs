//! kTLS for `[static]` sites on Linux: after the handshake, record
//! encryption moves into the kernel, and bodies go by sendfile again —
//! the two costs TLS added (userspace copies, per-byte crypto) disappear
//! from the hot path.
//!
//! Shape, and the fallback that matters more than the feature: decided per
//! connection BEFORE anything is consumed. The worker peeks at the
//! ClientHello; kTLS only when it offers TLS 1.3 with an AES-GCM suite the
//! worker probed the kernel for at startup (the server prefers AES-GCM,
//! so what the peek gates is what gets negotiated). Everything else —
//! older or ChaCha-only clients, an unparseable hello, a failed probe,
//! non-Linux — serves through the userspace TLS path unchanged. Past that
//! gate the socket is committed (the handshake consumes the rustls
//! endpoint), so a failure there closes that connection; the gates make it
//! unreachable in practice, and the worker's startup line says which suites
//! go kernel.
//!
//! Soundness of the handover: the handshake driver feeds the unbuffered
//! rustls endpoint exclusively from MSG_PEEK copies, and consumes from the
//! socket exactly what rustls consumed. Nothing is ever read ahead, so at
//! conversion time the wire sits precisely where the extracted sequence
//! numbers say it does — pipelined client bytes included.

use rustls::server::UnbufferedServerConnection;
use std::os::fd::AsFd;
use std::sync::Arc;

/// Cipher suites that matter here (what clients offer in TLS 1.3).
const AES_128_GCM_SHA256: u16 = 0x1301;
const AES_256_GCM_SHA384: u16 = 0x1302;
/// The most one TLS record carries on the wire.
const RECORD_CAP: usize = 18_432;
/// The most of a ClientHello the gate looks at (a bigger one falls back to
/// the userspace path, correctly).
const HELLO_CAP: usize = 16 * 1024 + 5;

/// What a peeked ClientHello offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Offer {
    /// ALPN lists `h2`: with the server's fixed offer (`h2`, then
    /// `http/1.1`) that is what gets negotiated.
    pub http2: bool,
    pub tls13: bool,
    pub gcm128: bool,
    pub gcm256: bool,
}

/// The kernel's kTLS abilities, probed once per worker at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Kernel {
    pub gcm128: bool,
    pub gcm256: bool,
}

/// kTLS exactly when the client offers TLS 1.3 with an AES-GCM suite the
/// kernel was probed for. (ChaCha20 always goes userspace: no kernel struct
/// for it here.)
pub(super) fn viable(offer: &Offer, kernel: Kernel) -> bool {
    offer.tls13 && ((offer.gcm128 && kernel.gcm128) || (offer.gcm256 && kernel.gcm256))
}

fn take<'b>(b: &mut &'b [u8], n: usize) -> Option<&'b [u8]> {
    if b.len() < n {
        return None;
    }
    let (head, tail) = b.split_at(n);
    *b = tail;
    Some(head)
}

fn u16_at(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// The ClientHello's version, cipher list and ALPN. Anything unexpected —
/// wrong record, a hello split across records, trailing garbage — is None:
/// the userspace path, correctly.
pub(super) fn parse_client_hello(bytes: &[u8]) -> Option<Offer> {
    let head = take(&mut &bytes[..], 5)?;
    if head[0] != 22 {
        return None; // not a handshake record
    }
    let rlen = u16_at(&head[3..]) as usize;
    let mut body = bytes.get(5..5 + rlen)?;
    if take(&mut body, 4).map(|hs| hs[0]) != Some(1) {
        return None; // not a ClientHello
    }
    // client_version (2) + random (32).
    take(&mut body, 34)?;
    // session_id.
    let n = *take(&mut body, 1)?.first()?;
    take(&mut body, n as usize)?;
    // cipher_suites.
    let n = u16_at(take(&mut body, 2)?) as usize;
    let suites = take(&mut body, n)?;
    if suites.len() % 2 != 0 {
        return None;
    }
    // compression_methods.
    let n = *take(&mut body, 1)?.first()?;
    take(&mut body, n as usize)?;
    // extensions.
    let n = u16_at(take(&mut body, 2)?) as usize;
    let mut exts = take(&mut body, n)?;
    if !body.is_empty() {
        return None;
    }
    let (mut tls13, mut http2, mut gcm128, mut gcm256) = (false, false, false, false);
    for chunk in suites.chunks_exact(2) {
        match u16_at(chunk) {
            AES_128_GCM_SHA256 => gcm128 = true,
            AES_256_GCM_SHA384 => gcm256 = true,
            _ => {}
        }
    }
    while !exts.is_empty() {
        let kind = u16_at(take(&mut exts, 2)?);
        let n = u16_at(take(&mut exts, 2)?) as usize;
        let mut data = take(&mut exts, n)?;
        match kind {
            // supported_versions: a length-prefixed list of versions.
            0x002b => {
                let count = *take(&mut data, 1)?.first()? as usize;
                if data.len() != count || count % 2 != 0 {
                    return None;
                }
                tls13 = data.chunks_exact(2).any(|v| u16_at(v) == 0x0304);
            }
            // application_layer_protocol_negotiation: a length-prefixed
            // list of length-prefixed names.
            0x0010 => {
                let n = u16_at(take(&mut data, 2)?) as usize;
                let mut names = take(&mut data, n)?;
                if !data.is_empty() {
                    return None;
                }
                while !names.is_empty() {
                    let n = *take(&mut names, 1)?.first()? as usize;
                    http2 = http2 || take(&mut names, n)? == b"h2";
                }
            }
            _ => {}
        }
    }
    Some(Offer { http2, tls13, gcm128, gcm256 })
}

/// The worker's kTLS abilities: configuring a throwaway socket exactly as a
/// connection would be, once, instead of discovering a missing feature per
/// connection (or worse, past the point of falling back).
#[cfg(target_os = "linux")]
pub(super) fn probe() -> Kernel {
    Kernel { gcm128: configure_tx_probe(128), gcm256: configure_tx_probe(256) }
}

/// Elsewhere there is no kernel TLS: every connection goes userspace.
#[cfg(not(target_os = "linux"))]
pub(super) fn probe() -> Kernel {
    Kernel { gcm128: false, gcm256: false }
}

/// What a kTLS attempt leaves behind: served-or-closed past the gate
/// (`Handled`), the untouched stream for the buffered path (`Userspace`),
/// or a kernel-owned socket with the peeked ALPN (`Kernel`).
pub(super) enum Out {
    Handled,
    Userspace(tokio::net::TcpStream),
    Kernel { std: std::net::TcpStream, http2: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UpError {
    /// The peer went away, or sent something the handshake cannot use.
    Closed,
    /// The handshake failed, or the kernel refused the handover.
    Tls,
}

/// Peek, gate, and (past the gate) handshake and hand over. Nothing is
/// consumed on the way to `Userspace`; past it the socket is committed.
/// Off Linux the probe is always negative, so this is one peek per
/// connection and then the buffered path — the same shape everywhere.
pub(super) async fn attempt(
    stream: tokio::net::TcpStream,
    config: &Arc<rustls::ServerConfig>,
    kernel: Kernel,
    flight_cap: usize,
) -> Out {
    let offer = match peek_offer(&stream).await {
        Some(offer) if viable(&offer, kernel) => offer,
        _ => return Out::Userspace(stream),
    };
    match upgrade(stream, config, offer, flight_cap).await {
        Ok((std, http2)) => Out::Kernel { std, http2 },
        Err(_) => Out::Handled,
    }
}

/// Peek at what is waiting (never consuming it) for the gate.
pub(super) async fn peek_offer(stream: &tokio::net::TcpStream) -> Option<Offer> {
    let mut buf = vec![0u8; HELLO_CAP];
    // Level-triggered patience, one shot: whatever has arrived decides. A
    // hello still in flight falls back to the userspace path, correctly.
    stream.readable().await.ok()?;
    let n = crate::sys::peek(stream.as_fd(), &mut buf).ok()?;
    parse_client_hello(&buf[..n])
}

/// Run the handshake without ever reading ahead (every byte is peeked first
/// and consumed exactly as rustls consumes it), flush the server's flight
/// including its session tickets, and hand the socket to the kernel. The
/// caller wraps this in the head timeout, as with the buffered handshake.
async fn upgrade(
    mut stream: tokio::net::TcpStream,
    config: &Arc<rustls::ServerConfig>,
    offer: Offer,
    flight_cap: usize,
) -> Result<(std::net::TcpStream, bool), UpError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut conn = UnbufferedServerConnection::new(config.clone()).map_err(|_| UpError::Tls)?;
    // Bytes peeked but not yet fed; the server's encoded flight; scratch
    // space for peeks.
    let mut staged = Vec::<u8>::new();
    let mut out = vec![0u8; flight_cap];
    let mut tmp = vec![0u8; RECORD_CAP];
    let mut pending = 0usize;
    let mut flushing = false;
    let mut rounds = 0u32;
    // Whether the next pass may need fresh input: only rustls being blocked
    // on input earns a wait. Peeking unconditionally (to transmit first,
    // encode first) meets stale Tokio readiness — the raw peek bypasses
    // Tokio's bookkeeping, and a read that consumes everything in one go
    // leaves the ready flag set — so an unconditional peek dies on a
    // `WouldBlock` that means nothing.
    let mut want_input = true; // the first pass stages the hello
    let mut traffic_streak = 0u32; // consecutive post-handshake passes
    loop {
        if rounds > 4096 {
            return Err(UpError::Tls); // never converges: not a handshake
        }
        rounds += 1;
        // What rustls may see this pass: leading complete records, stopping
        // after the first record past ChangeCipherSpec — one flight step at
        // a time — but only when the previous pass left rustls blocked on
        // input. Encodes and transmissions drain its queue with no input at
        // all, and past the handshake everything is application bytes the
        // kernel must receive: a request pipelined with the Finished stays
        // peeked but unfed (feeding it would consume it into userspace and
        // move the wire past the extracted sequence numbers, and rustls
        // refuses it). In the flush phase nothing is fed at all.
        let feed = if flushing || !want_input { 0 } else { feed_len(&staged) };
        if !flushing && feed == 0 && !staged.is_empty() {
            // An absurd declared length is never a record: not ours. A
            // fragment still arriving just needs another pass.
            if staged.len() >= 5 && u16_at(&staged[3..5]) as usize + 5 > RECORD_CAP {
                return Err(UpError::Closed);
            }
        }
        if want_input && !flushing && staged.len() < HELLO_CAP.min(RECORD_CAP) {
            stream.readable().await.map_err(|_| UpError::Closed)?;
            match crate::sys::peek(stream.as_fd(), &mut tmp) {
                Ok(n) if n > staged.len() => {
                    staged.extend_from_slice(&tmp[staged.len()..n]);
                }
                Err(e) if e.kind() != std::io::ErrorKind::WouldBlock => {
                    // A genuine refusal with nothing staged and nothing to
                    // send ends the attempt; with work staged, run rustls
                    // on it and let the consume/write below report it.
                    if staged.is_empty() && pending == 0 {
                        return Err(UpError::Closed);
                    }
                }
                _ => {
                    // Nothing new past what is staged — a spurious wake
                    // (stale readiness: the raw peek bypasses Tokio's
                    // bookkeeping) or a hello still in flight. Only a pass
                    // with nothing staged and nothing to send pauses, so a
                    // stuck-ready flag becomes a millisecond wait instead
                    // of a hot spin.
                    if staged.is_empty() && pending == 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                }
            }
        }
        #[derive(Clone, Copy)]
        enum Step {
            Encoded,
            Transmit,
            Wait,
            Traffic,
            Abort,
        }
        let status = conn.process_tls_records(&mut staged[..feed]);
        let used = status.discard;
        let step = match status.state {
            Ok(rustls::unbuffered::ConnectionState::EncodeTlsData(mut e)) => {
                // One chunk of a possibly multi-part flight (ServerHello,
                // CCS, the encrypted remainder queue separately): append it,
                // because each encode writes at the buffer start. Transmit
                // arrives once the queue is empty and writes out everything
                // accumulated. A second encode on the same state errors.
                let wrote = e.encode(&mut out[pending..]).map_err(|_| UpError::Tls)?;
                pending += wrote;
                Step::Encoded
            }
            Ok(rustls::unbuffered::ConnectionState::TransmitTlsData(t)) => {
                t.done();
                Step::Transmit
            }
            Ok(rustls::unbuffered::ConnectionState::BlockedHandshake) => Step::Wait,
            Ok(rustls::unbuffered::ConnectionState::WriteTraffic(_)) => Step::Traffic,
            _ => Step::Abort,
        };
        if matches!(step, Step::Abort) {
            return Err(UpError::Tls);
        }
        // The status borrow ends with the match: catch the socket and the
        // buffer up to exactly what rustls consumed, every pass — including
        // the quiescent one, so the wire is aligned for the handover.
        {
            let mut gone = vec![0u8; used];
            stream.read_exact(&mut gone).await.map_err(|_| UpError::Closed)?;
            staged.drain(..used);
        }
        match step {
            Step::Transmit => {
                AsyncWriteExt::write_all(&mut stream, &out[..pending]).await.map_err(|_| UpError::Closed)?;
                pending = 0;
            }
            Step::Wait => {
                if flushing {
                    break; // quiescent: the tickets are out
                }
            }
            Step::Traffic => {
                // The handshake is done; further passes flush what the
                // server still owes (its session tickets) with empty input.
                // rustls reports traffic on every post-handshake pass — it
                // never goes quiet — so the second consecutive traffic
                // state ends the flush: encodes take priority over it, so
                // back-to-back traffic means the queue drained.
                traffic_streak += 1;
                flushing = true;
                if traffic_streak >= 2 {
                    break; // quiescent: the tickets are out
                }
            }
            Step::Encoded | Step::Abort => {
                traffic_streak = 0;
            }
        }
        // Only rustls being blocked on input earns a wait for fresh input:
        // after an encode the next pass must transmit, after a transmit it
        // must re-run rustls — peeking first would meet stale readiness and
        // die on a meaningless `WouldBlock`.
        want_input = matches!(step, Step::Wait);
    }
    convert(stream, conn, offer)
}

/// Leading complete records in what was peeked, stopping after the first
/// record past ChangeCipherSpec: the only bytes the driver feeds rustls
/// this pass. Plain handshake records (the ClientHello) feed freely; past
/// them everything is encrypted, so any complete record type feeds — the
/// client's Finished arrives as application_data on the outside. Stops at
/// a record still arriving, leaving whatever follows (a request pipelined
/// with the Finished) on the wire for the kernel.
fn feed_len(staged: &[u8]) -> usize {
    let mut feed = 0;
    let mut rest = staged;
    while rest.len() >= 5 {
        let (typ, len) = (rest[0], u16_at(&rest[3..5]) as usize + 5);
        if len > RECORD_CAP || rest.len() < len {
            break; // absurd, or still arriving
        }
        feed += len;
        rest = &rest[len..];
        if typ != 22 && typ != 20 {
            break; // one encrypted step (or an alert, which rustls refuses)
        }
    }
    feed
}

/// Convert a handshook connection: verify the gate's prediction against
/// what was negotiated, extract secrets with their sequence numbers, and
/// configure the kernel for both directions.
fn convert(
    stream: tokio::net::TcpStream,
    conn: UnbufferedServerConnection,
    offer: Offer,
) -> Result<(std::net::TcpStream, bool), UpError> {
    // The gate predicted TLS 1.3 with AES-GCM; believe, then verify — the
    // kernel config below is only built for those.
    let (secrets, kconn) = conn.dangerous_into_kernel_connection().map_err(|_| UpError::Tls)?;
    if kconn.protocol_version() != rustls::ProtocolVersion::TLSv1_3 {
        return Err(UpError::Tls);
    }
    let gcm: u16 = match kconn.negotiated_cipher_suite().suite() {
        rustls::CipherSuite::TLS13_AES_128_GCM_SHA256 => 128,
        rustls::CipherSuite::TLS13_AES_256_GCM_SHA384 => 256,
        _ => return Err(UpError::Tls),
    };
    configure(&stream, gcm, &secrets)?;
    let std = stream.into_std().map_err(|_| UpError::Tls)?;
    // Back to nonblocking: both serve paths (HTTP/1.1 and HTTP/2) drive
    // this socket from the async executor, which a blocking socket would
    // stall. (`into_std` leaves it blocking.)
    std.set_nonblocking(true).map_err(|_| UpError::Tls)?;
    Ok((std, offer.http2))
}

/// Off Linux there is nothing to configure with: the gate never passes, so
/// this is unreachable, but it keeps one code shape everywhere.
#[cfg(not(target_os = "linux"))]
fn configure(_stream: &tokio::net::TcpStream, _gcm: u16, _secrets: &rustls::ExtractedSecrets) -> Result<(), UpError> {
    Err(UpError::Tls)
}

/// Pick the sequence number, key, and nonce for one direction out of the
/// extracted secrets — a free function rather than a closure, because only
/// a function signature ties the returned borrows to the secrets (a
/// closure leaves the compiler unable to prove that).
#[cfg(target_os = "linux")]
fn direction(secret: &(u64, rustls::ConnectionTrafficSecrets), gcm: u16) -> Option<(u64, &[u8], &[u8])> {
    use rustls::ConnectionTrafficSecrets::*;
    match (&secret.1, gcm) {
        (Aes128Gcm { key, iv }, 128) | (Aes256Gcm { key, iv }, 256) => Some((secret.0, key.as_ref(), iv.as_ref())),
        _ => None,
    }
}

/// Hand the socket to the kernel through `sys` (where every `unsafe`
/// call lives): the extracted secrets with their sequence numbers, for the
/// suite the gate verified. Any refusal closes the connection upstream.
#[cfg(target_os = "linux")]
fn configure(stream: &tokio::net::TcpStream, gcm: u16, secrets: &rustls::ExtractedSecrets) -> Result<(), UpError> {
    let cipher = match gcm {
        128 => crate::sys::KTLS_CIPHER_AES_GCM_128,
        256 => crate::sys::KTLS_CIPHER_AES_GCM_256,
        _ => return Err(UpError::Tls),
    };
    let (tx_seq, tx_key, tx_iv) = direction(&secrets.tx, gcm).ok_or(UpError::Tls)?;
    let (rx_seq, rx_key, rx_iv) = direction(&secrets.rx, gcm).ok_or(UpError::Tls)?;
    let tx = crate::sys::KtlsDirection { key: tx_key, iv: tx_iv, seq: tx_seq };
    let rx = crate::sys::KtlsDirection { key: rx_key, iv: rx_iv, seq: rx_seq };
    crate::sys::ktls_takeover(stream.as_fd(), cipher, &tx, &rx).map_err(|_| UpError::Tls)
}

/// The probe behind `probe()`: a connected loopback pair, one side
/// configured exactly as a connection would be (both directions, dummy
/// key, real question). An unconnected socket cannot take the config, so
/// a socket going nowhere would answer "no" on every kernel.
#[cfg(target_os = "linux")]
fn configure_tx_probe(bits: u16) -> bool {
    use crate::sys;
    use std::os::fd::AsFd;
    let (cipher, key_len) = match bits {
        128 => (sys::KTLS_CIPHER_AES_GCM_128, 16),
        256 => (sys::KTLS_CIPHER_AES_GCM_256, 32),
        _ => return false,
    };
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else { return false };
    let Ok(addr) = listener.local_addr() else { return false };
    let Ok(client) = std::net::TcpStream::connect(addr) else { return false };
    let Ok((_server, _)) = listener.accept() else { return false };
    let key = vec![0u8; key_len];
    let iv = [0u8; 12];
    sys::ktls_set_ulp(client.as_fd()).is_ok()
        && sys::ktls_set_crypto(client.as_fd(), true, cipher, &key, &iv, 0).is_ok()
        && sys::ktls_set_crypto(client.as_fd(), false, cipher, &key, &iv, 0).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ClientHello offering TLS 1.3 with AES-128-GCM and ALPN `h2`,
    /// built field by field (SNI `localhost`).
    fn hello(tls13: bool, suites: &[u16], alpn: &[&[u8]]) -> Vec<u8> {
        let mut body = vec![0x03, 0x01]; // legacy client_version
        body.extend_from_slice(&[7u8; 32]); // random
        body.push(0); // no session id
        body.extend_from_slice(&(suites.len() as u16 * 2).to_be_bytes());
        for s in suites {
            body.extend_from_slice(&s.to_be_bytes());
        }
        body.push(1); // compression_methods
        body.push(0);
        let mut exts = Vec::new();
        // supported_versions.
        exts.extend_from_slice(&[0x00, 0x2b]);
        let mut versions = Vec::new();
        if tls13 {
            versions.extend_from_slice(&[0x03, 0x04]);
        }
        versions.extend_from_slice(&[0x03, 0x03]);
        exts.extend_from_slice(&(versions.len() as u16 + 1).to_be_bytes());
        exts.push(versions.len() as u8);
        exts.extend_from_slice(&versions);
        // application_layer_protocol_negotiation.
        if !alpn.is_empty() {
            let mut names = Vec::new();
            for name in alpn {
                names.push(name.len() as u8);
                names.extend_from_slice(name);
            }
            exts.extend_from_slice(&[0x00, 0x10]);
            exts.extend_from_slice(&(names.len() as u16 + 2).to_be_bytes());
            exts.extend_from_slice(&(names.len() as u16).to_be_bytes());
            exts.extend_from_slice(&names);
        }
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);
        let mut rec = vec![22, 0x03, 0x01];
        rec.extend_from_slice(&(body.len() as u16 + 4).to_be_bytes());
        rec.push(1); // one ClientHello message, then its 24-bit length
        let n = (body.len() as u32).to_be_bytes();
        rec.extend_from_slice(&n[1..]);
        rec.extend_from_slice(&body);
        rec
    }

    #[test]
    fn hellos_parse_to_their_offer() {
        let full = hello(true, &[0x1301, 0x1302, 0x1303], &[b"h2", b"http/1.1"]);
        assert_eq!(parse_client_hello(&full), Some(Offer { http2: true, tls13: true, gcm128: true, gcm256: true }));
        let h1 = hello(true, &[0x1301], &[b"http/1.1"]);
        assert_eq!(parse_client_hello(&h1), Some(Offer { http2: false, tls13: true, gcm128: true, gcm256: false }));
        let old = hello(false, &[0x1301], &[b"h2"]);
        assert_eq!(parse_client_hello(&old).map(|o| o.tls13), Some(false));
        let chacha = hello(true, &[0x1303], &[b"h2"]);
        let o = parse_client_hello(&chacha).unwrap();
        assert!(!o.gcm128 && !o.gcm256, "ChaCha-only offers no AES-GCM");
    }

    #[test]
    fn garbage_hellos_decline() {
        assert_eq!(parse_client_hello(b""), None);
        assert_eq!(parse_client_hello(b"GET / HTTP/1.1\r\n\r\n"), None);
        let mut truncated = hello(true, &[0x1301], &[b"h2"]);
        truncated.truncate(truncated.len() - 3);
        assert_eq!(parse_client_hello(&truncated), None, "a hello still in flight");
    }

    #[test]
    fn gate_needs_tls13_and_a_probed_suite() {
        let kernel = Kernel { gcm128: true, gcm256: false };
        let o = Offer { http2: true, tls13: true, gcm128: true, gcm256: false };
        assert!(viable(&o, kernel));
        assert!(!viable(&Offer { tls13: false, ..o }, kernel), "no 1.2");
        assert!(!viable(&Offer { gcm128: false, gcm256: true, ..o }, kernel), "unprobed suite");
        assert!(!viable(&o, Kernel { gcm128: false, gcm256: false }), "no kernel");
    }

    /// The handover on a real loopback socket (Linux only: the kernel is
    /// the thing under test): the socket reports ULP `tls` afterwards, and
    /// both directions go through the kernel — the server reads plaintext
    /// the client encrypted, and the client decrypts plaintext the server
    /// wrote. A second connection with the same client store resumes,
    /// which is also what proves the flushed tickets arrived.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn kernel_takes_over_both_directions() {
        use super::super::tls;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let probe = probe();
        assert!(probe.gcm128 && probe.gcm256, "this kernel takes AES-GCM kTLS");
        let dir = tls::test_dir("ktls");
        let (cert, key) = tls::test_cert_files(&dir);
        let cert_pem = std::fs::read(&cert).unwrap();
        let config = tls::load(&cert, &key).expect("the test chain loads");
        let listener = std::sync::Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
        let addr = listener.local_addr().unwrap();
        let connector = tls::test_client(b"http/1.1", &cert_pem);

        /// One handover: the client's handshake kind, the server's
        /// plaintext socket, and the client's end for traffic.
        async fn one(
            listener: std::sync::Arc<tokio::net::TcpListener>,
            addr: std::net::SocketAddr,
            config: &Arc<rustls::ServerConfig>,
            connector: &tokio_rustls::TlsConnector,
        ) -> (rustls::HandshakeKind, std::net::TcpStream, tokio_rustls::client::TlsStream<tokio::net::TcpStream>)
        {
            let server = tokio::spawn({
                let config = config.clone();
                async move {
                    let (stream, _) = listener.accept().await.expect("a connection");
                    let offer = peek_offer(&stream).await.expect("a ClientHello");
                    assert!(offer.tls13 && offer.gcm256, "the test client offers 1.3 + GCM");
                    let (std, http2) = upgrade(stream, &config, offer, 1 << 16).await.expect("the handover runs");
                    assert!(!http2, "no ALPN h2 offered");
                    assert_eq!(ulp_name(&std), "tls", "the kernel took the socket");
                    // This test drives the socket with blocking std IO;
                    // serving uses it nonblocking (see `convert`).
                    std.set_nonblocking(false).expect("blocking test IO");
                    std
                }
            });
            let io = tokio::net::TcpStream::connect(addr).await.unwrap();
            let client = connector.connect("localhost".try_into().unwrap(), io).await.expect("the client handshakes");
            let kind = client.get_ref().1.handshake_kind().unwrap();
            (kind, server.await.unwrap(), client)
        }

        // Round 1: full handshake, then traffic both ways — the server's
        // reads are plaintext the client encrypted (kernel RX), and the
        // client's reads are plaintext the server wrote (kernel TX).
        let (kind, mut server, mut client) =
            tokio::time::timeout(std::time::Duration::from_secs(10), one(listener.clone(), addr, &config, &connector))
                .await
                .expect("the first handover runs");
        assert_eq!(kind, rustls::HandshakeKind::Full);
        client.write_all(b"ping").await.unwrap();
        server.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut four = [0u8; 4];
        std::io::Read::read_exact(&mut server, &mut four).unwrap();
        assert_eq!(&four, b"ping");
        std::io::Write::write_all(&mut server, b"pong").unwrap();
        client.read_exact(&mut four).await.unwrap();
        assert_eq!(&four, b"pong");

        // Round 2 on the same client store resumes: the flushed tickets
        // arrived, or this abbreviates nothing.
        let (kind, _, _) =
            tokio::time::timeout(std::time::Duration::from_secs(10), one(listener.clone(), addr, &config, &connector))
                .await
                .expect("the second handover runs");
        assert_eq!(kind, rustls::HandshakeKind::Resumed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    fn ulp_name(stream: &std::net::TcpStream) -> String {
        use std::os::fd::AsFd;
        crate::sys::ktls_ulp_name(stream.as_fd()).expect("ULP is readable")
    }
}
