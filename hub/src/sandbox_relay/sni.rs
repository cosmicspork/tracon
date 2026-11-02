//! Just enough of a TLS ClientHello to read the server name: record headers,
//! the handshake header, and the extension list up to `server_name`. Nothing
//! is decrypted (a ClientHello is plaintext) and nothing is kept but the name;
//! the caller forwards every byte it read unchanged.

/// The longest ClientHello the relay waits for. Real ones are under 2 KiB even
/// with post-quantum key shares; this bounds what an unauthenticated
/// connection can make the hub buffer.
pub const MAX_HELLO: usize = 64 * 1024;

/// TLSPlaintext.length may not exceed 2^14.
const MAX_RECORD: usize = 1 << 14;
const CONTENT_HANDSHAKE: u8 = 22;
const HANDSHAKE_CLIENT_HELLO: u8 = 1;
const EXT_SERVER_NAME: u16 = 0;
const NAME_TYPE_HOST: u8 = 0;

#[derive(Debug, PartialEq, Eq)]
pub enum Hello {
    /// Read more and ask again.
    Incomplete,
    /// The lowercased host name the client asked for.
    Sni(String),
    /// Not a ClientHello naming a host; the connection is closed unanswered.
    Refused(&'static str),
}

/// What the bytes read so far from a connection say about its server name.
pub fn server_name(buf: &[u8]) -> Hello {
    let mut handshake = Vec::new();
    let mut at = 0;
    loop {
        let Some(header) = buf.get(at..at + 5) else {
            return incomplete(buf);
        };
        if header[0] != CONTENT_HANDSHAKE {
            return Hello::Refused("not a TLS handshake");
        }
        if header[1] != 3 {
            return Hello::Refused("not a TLS record version");
        }
        let len = u16::from_be_bytes([header[3], header[4]]) as usize;
        if len == 0 || len > MAX_RECORD {
            return Hello::Refused("bad record length");
        }
        let Some(fragment) = buf.get(at + 5..at + 5 + len) else {
            return incomplete(buf);
        };
        handshake.extend_from_slice(fragment);
        at += 5 + len;

        if handshake.len() < 4 {
            continue;
        }
        if handshake[0] != HANDSHAKE_CLIENT_HELLO {
            return Hello::Refused("first handshake message is not a ClientHello");
        }
        let body_len = u32::from_be_bytes([0, handshake[1], handshake[2], handshake[3]]) as usize;
        if 4 + body_len > MAX_HELLO {
            return Hello::Refused("ClientHello too large");
        }
        if let Some(body) = handshake.get(4..4 + body_len) {
            return parse_body(body);
        }
    }
}

fn incomplete(buf: &[u8]) -> Hello {
    // Record headers on top of the largest hello: anything past that is not a
    // hello still arriving.
    if buf.len() > MAX_HELLO + 5 * (MAX_HELLO / MAX_RECORD + 1) {
        Hello::Refused("ClientHello too large")
    } else {
        Hello::Incomplete
    }
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = rest;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    fn vec8(&mut self) -> Option<&'a [u8]> {
        let n = self.u8()? as usize;
        self.take(n)
    }
    fn vec16(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()? as usize;
        self.take(n)
    }
}

fn parse_body(body: &[u8]) -> Hello {
    let mut c = Cursor(body);
    let walk = (|| {
        c.take(2 + 32)?; // legacy_version, random
        c.vec8()?; // legacy_session_id
        c.vec16()?; // cipher_suites
        c.vec8()?; // legacy_compression_methods
        if c.0.is_empty() {
            return Some(None);
        }
        let mut exts = Cursor(c.vec16()?);
        while !exts.0.is_empty() {
            let kind = exts.u16()?;
            let data = exts.vec16()?;
            if kind == EXT_SERVER_NAME {
                let mut list = Cursor(Cursor(data).vec16()?);
                while !list.0.is_empty() {
                    let name_type = list.u8()?;
                    let name = list.vec16()?;
                    if name_type == NAME_TYPE_HOST {
                        return Some(Some(name));
                    }
                }
                return Some(None);
            }
        }
        Some(None)
    })();
    match walk {
        None => Hello::Refused("malformed ClientHello"),
        Some(None) => Hello::Refused("no server name"),
        Some(Some(name)) => match host(name) {
            Some(h) => Hello::Sni(h),
            None => Hello::Refused("server name is not a host name"),
        },
    }
}

fn host(raw: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(raw).ok()?;
    let s = s.strip_suffix('.').unwrap_or(s);
    let ok = !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    ok.then(|| s.to_ascii_lowercase())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn vec16(b: &[u8]) -> Vec<u8> {
        let mut v = (b.len() as u16).to_be_bytes().to_vec();
        v.extend_from_slice(b);
        v
    }

    /// A ClientHello body with these extensions.
    fn body(exts: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut b = vec![3, 3];
        b.extend_from_slice(&[7; 32]);
        b.push(32);
        b.extend_from_slice(&[9; 32]);
        b.extend(vec16(&[0x13, 0x01, 0x13, 0x02]));
        b.extend_from_slice(&[1, 0]);
        let mut all = Vec::new();
        for (kind, data) in exts {
            all.extend_from_slice(&kind.to_be_bytes());
            all.extend(vec16(data));
        }
        b.extend(vec16(&all));
        b
    }

    fn sni_ext(name: &str) -> (u16, Vec<u8>) {
        let mut entry = vec![NAME_TYPE_HOST];
        entry.extend(vec16(name.as_bytes()));
        (EXT_SERVER_NAME, vec16(&entry))
    }

    fn handshake(body: &[u8]) -> Vec<u8> {
        let mut h = vec![HANDSHAKE_CLIENT_HELLO];
        h.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        h.extend_from_slice(body);
        h
    }

    /// The handshake split into records of at most `chunk` bytes.
    fn records(hs: &[u8], chunk: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for part in hs.chunks(chunk) {
            out.extend_from_slice(&[CONTENT_HANDSHAKE, 3, 1]);
            out.extend_from_slice(&(part.len() as u16).to_be_bytes());
            out.extend_from_slice(part);
        }
        out
    }

    pub(crate) fn client_hello(name: &str) -> Vec<u8> {
        records(
            &handshake(&body(&[(10, vec![0, 2, 0, 29]), sni_ext(name)])),
            MAX_RECORD,
        )
    }

    #[test]
    fn reads_the_name_and_waits_for_the_rest() {
        let hello = client_hello("ABC.Relay.example.");
        assert_eq!(server_name(&hello), Hello::Sni("abc.relay.example".into()));
        for cut in [0, 3, 5, 40, hello.len() - 1] {
            assert_eq!(server_name(&hello[..cut]), Hello::Incomplete, "cut {cut}");
        }
        // Bytes after the hello (an early-data record) change nothing.
        let mut more = hello.clone();
        more.extend_from_slice(&[23, 3, 3, 0, 1, 0]);
        assert_eq!(server_name(&more), Hello::Sni("abc.relay.example".into()));
    }

    #[test]
    fn a_hello_fragmented_across_records_is_reassembled() {
        let hs = handshake(&body(&[sni_ext("node.relay.example")]));
        let split = records(&hs, 7);
        assert_eq!(server_name(&split), Hello::Sni("node.relay.example".into()));
        assert_eq!(server_name(&split[..split.len() - 2]), Hello::Incomplete);
    }

    #[test]
    fn anything_else_is_refused() {
        assert!(matches!(
            server_name(b"GET / HTTP/1.1\r\n\r\n"),
            Hello::Refused(_)
        ));
        // No server_name extension.
        let none = records(&handshake(&body(&[(10, vec![0, 2, 0, 29])])), MAX_RECORD);
        assert_eq!(server_name(&none), Hello::Refused("no server name"));
        // A name that is not a host name.
        assert!(matches!(
            server_name(&client_hello("a b")),
            Hello::Refused(_)
        ));
        // A ServerHello where a ClientHello belongs.
        let mut wrong = client_hello("x.example");
        wrong[5] = 2;
        assert!(matches!(server_name(&wrong), Hello::Refused(_)));
        // A declared hello larger than the cap.
        let big = [CONTENT_HANDSHAKE, 3, 1, 0, 4, 1, 0xff, 0xff, 0xff];
        assert_eq!(server_name(&big), Hello::Refused("ClientHello too large"));
        // Truncated inside the body.
        let mut short = handshake(&body(&[sni_ext("x.example")]));
        short.truncate(short.len() - 3);
        let n = (short.len() - 4) as u32;
        short[1..4].copy_from_slice(&n.to_be_bytes()[1..]);
        assert_eq!(
            server_name(&records(&short, MAX_RECORD)),
            Hello::Refused("malformed ClientHello")
        );
    }
}
