//! The hostname a TLS connection asks for: the server_name extension of its
//! ClientHello (RFC 6066), read without decrypting anything.

/// Bytes of a TLS record header: type, version, length.
pub(super) const RECORD_HEADER: usize = 5;

/// What the bytes read so far say.
#[derive(Debug, PartialEq)]
pub(super) enum Hello {
    /// The first record is not complete yet: this many bytes in all.
    Need(usize),
    /// Complete. The hostname (lowercase), or None when the client sent none.
    Done(Option<String>),
    /// Not a TLS handshake (plain HTTP, or garbage).
    NotTls,
}

/// Parse the start of a connection. The first record of a TLS connection is
/// the ClientHello; a hello split over several records (a very large one) is
/// read from its first record only, where server_name comes early.
pub(super) fn parse(buf: &[u8]) -> Hello {
    if buf.len() < RECORD_HEADER {
        return Hello::Need(RECORD_HEADER);
    }
    if buf[0] != 0x16 || buf[1] != 0x03 {
        return Hello::NotTls;
    }
    let need = RECORD_HEADER + u16::from_be_bytes([buf[3], buf[4]]) as usize;
    if buf.len() < need {
        return Hello::Need(need);
    }
    Hello::Done(server_name(&buf[RECORD_HEADER..need]))
}

/// server_name from a handshake message: the ClientHello's fields are
/// skipped by their lengths, then the extensions are walked.
fn server_name(h: &[u8]) -> Option<String> {
    let mut r = Reader { b: h, i: 0 };
    if r.u8()? != 1 {
        return None; // not a ClientHello
    }
    r.skip(3)?; // handshake length
    r.skip(2 + 32)?; // version, random
    let sid = r.u8()? as usize;
    r.skip(sid)?;
    let suites = r.u16()? as usize;
    r.skip(suites)?;
    let comp = r.u8()? as usize;
    r.skip(comp)?;
    let ext_len = r.u16()? as usize;
    let end = (r.i + ext_len).min(h.len());
    while r.i + 4 <= end {
        let ty = r.u16()?;
        let len = r.u16()? as usize;
        if ty != 0 {
            r.skip(len)?;
            continue;
        }
        // server_name_list: length, then entries of (type, length, name).
        let mut list = Reader { b: r.take(len)?, i: 0 };
        list.skip(2)?;
        while list.i < list.b.len() {
            let kind = list.u8()?;
            let n = list.u16()? as usize;
            let name = list.take(n)?;
            if kind == 0 {
                let name = std::str::from_utf8(name).ok()?;
                return Some(name.trim_end_matches('.').to_ascii_lowercase());
            }
        }
        return None;
    }
    None
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.i)?;
        self.i += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s)
    }
}

/// A ClientHello naming `host` (or none), as a client sends it: for tests.
#[cfg(test)]
pub(crate) fn client_hello(host: Option<&str>) -> Vec<u8> {
    let mut ext = Vec::new();
    // An unrelated extension first (supported_versions), as browsers send.
    ext.extend_from_slice(&[0x00, 0x2b, 0x00, 0x03, 0x02, 0x03, 0x04]);
    if let Some(h) = host {
        let n = h.len() as u16;
        ext.extend_from_slice(&[0x00, 0x00]);
        ext.extend_from_slice(&(n + 5).to_be_bytes());
        ext.extend_from_slice(&(n + 3).to_be_bytes());
        ext.push(0);
        ext.extend_from_slice(&n.to_be_bytes());
        ext.extend_from_slice(h.as_bytes());
    }
    let mut body = vec![0x03, 0x03];
    body.extend_from_slice(&[7u8; 32]); // random
    body.push(32);
    body.extend_from_slice(&[9u8; 32]); // session id
    body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // one cipher suite
    body.extend_from_slice(&[0x01, 0x00]); // no compression
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);
    let mut hs = vec![0x01, 0x00];
    hs.extend_from_slice(&(body.len() as u16).to_be_bytes());
    hs.extend_from_slice(&body);
    let mut rec = vec![0x16, 0x03, 0x01];
    rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    rec.extend_from_slice(&hs);
    rec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_hostname_and_lowercases_it() {
        let h = client_hello(Some("API.Example.com"));
        assert_eq!(parse(&h), Hello::Done(Some("api.example.com".into())));
    }

    #[test]
    fn a_hello_without_a_name_is_complete_with_none() {
        assert_eq!(parse(&client_hello(None)), Hello::Done(None));
    }

    #[test]
    fn asks_for_the_rest_of_a_partial_record() {
        let h = client_hello(Some("a.example.com"));
        assert_eq!(parse(&h[..3]), Hello::Need(RECORD_HEADER));
        assert_eq!(parse(&h[..20]), Hello::Need(h.len()));
    }

    #[test]
    fn plain_http_is_not_tls() {
        assert_eq!(parse(b"GET / HTTP/1.1\r\n"), Hello::NotTls);
    }

    #[test]
    fn truncated_or_lying_lengths_give_no_name_instead_of_a_panic() {
        let h = client_hello(Some("a.example.com"));
        for cut in RECORD_HEADER..h.len() {
            let mut b = h[..cut].to_vec();
            // Claim the record is complete at this length.
            let n = (cut - RECORD_HEADER) as u16;
            b[3..5].copy_from_slice(&n.to_be_bytes());
            let _ = parse(&b);
        }
        let mut b = h.clone();
        let last = b.len() - 1;
        b[last - 15] = 0xff; // a name length far past the end
        let _ = parse(&b);
    }
}
