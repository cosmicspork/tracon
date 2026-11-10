//! The WebSocket carrier: the node's TLS inside a WebSocket to the hub's own
//! URL (`/v0/relay/connect`), for a sandbox whose only way out is an HTTP
//! proxy that admits the hub by name, in front of a hub that sits behind a
//! proxy speaking only HTTP.
//!
//! What this adds is outer: an HTTP `CONNECT` through the proxy the
//! environment names, TLS to the hub checked against the public web's roots,
//! and the WebSocket handshake. The connection it yields carries the same
//! TLS, pinned to the node's certificate, as any other carrier; the hub
//! routes it by that TLS's SNI and cannot read it.
//!
//! Bytes travel as binary messages; an empty one is end of stream in that
//! direction (TCP half-close), and so is a close.

use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use super::BoxedIo;

/// The largest binary message sent: what the hub accepts
/// (`proto::relay::MAX_MESSAGE`).
const MAX_MESSAGE: usize = 64 * 1024;
/// The most a proxy's answer to `CONNECT` may hold.
const MAX_PROXY_ANSWER: usize = 16 * 1024;

/// Where a WebSocket carrier dials: the relay's URL, and the HTTP proxy in
/// front of it, settled when the sandbox starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayUrl {
    url: String,
    tls: bool,
    host: String,
    port: u16,
    proxy: Option<Proxy>,
}

#[derive(Clone, PartialEq, Eq)]
struct Proxy {
    host: String,
    port: u16,
    /// `user:password`, sent as basic `Proxy-Authorization`.
    credentials: Option<String>,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

impl RelayUrl {
    /// `url` (`wss://` or `ws://`) dialled directly.
    pub fn direct(url: &str) -> Result<Self, String> {
        let shown = redacted(url);
        let uri: http::Uri = url
            .parse()
            .map_err(|e| format!("{shown:?} is not a URL: {e}"))?;
        let tls = match uri.scheme_str() {
            Some("wss") => true,
            Some("ws") => false,
            _ => return Err(format!("{shown:?} is not a ws:// or wss:// URL")),
        };
        // A fully qualified name's trailing dot is no part of the name a
        // proxy admits or a certificate is for.
        let host = uri
            .host()
            .map(|h| h.trim_start_matches('[').trim_end_matches(']'))
            .map(|h| h.trim_end_matches('.'))
            .filter(|h| !h.is_empty())
            .ok_or_else(|| format!("{shown:?} names no host"))?
            .to_ascii_lowercase();
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
        Ok(RelayUrl {
            url: url.to_string(),
            tls,
            host,
            port,
            proxy: None,
        })
    }

    /// `url` through the proxy `env` names for its scheme, unless its
    /// no-proxy list covers the host. `TRACON_SANDBOX_PROXY` and
    /// `TRACON_SANDBOX_NO_PROXY` come first: a bootstrap's copy of the
    /// provider's settings, for a supervisor started without them.
    pub fn from_env(url: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut out = Self::direct(url)?;
        let first = |names: &[&str]| names.iter().find_map(|n| env(n).filter(|v| !v.is_empty()));
        let proxy = match out.tls {
            true => first(&["TRACON_SANDBOX_PROXY", "HTTPS_PROXY", "https_proxy"]),
            false => first(&["TRACON_SANDBOX_PROXY", "HTTP_PROXY", "http_proxy"]),
        };
        let no_proxy = first(&["TRACON_SANDBOX_NO_PROXY", "NO_PROXY", "no_proxy"]);
        if let Some(proxy) = proxy {
            if !no_proxy.is_some_and(|list| bypasses(&list, &out.host, out.port)) {
                out.proxy = Some(Proxy::parse(&proxy)?);
            }
        }
        Ok(out)
    }

    /// The URL, without the proxy.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether a proxy carries it, as `host:port`.
    pub fn proxy(&self) -> Option<String> {
        self.proxy
            .as_ref()
            .map(|p| format!("{}:{}", p.host, p.port))
    }

    /// Dial: the proxy's tunnel or a TCP connection, TLS to the hub against
    /// the web's roots, then the WebSocket.
    pub async fn connect(&self) -> io::Result<BoxedIo> {
        self.connect_with(web_roots()).await
    }

    /// [`RelayUrl::connect`] with the outer TLS trusting what `tls` does.
    pub async fn connect_with(&self, tls: Arc<rustls::ClientConfig>) -> io::Result<BoxedIo> {
        let tcp = match &self.proxy {
            None => TcpStream::connect((self.host.as_str(), self.port)).await?,
            Some(proxy) => proxy.tunnel(&self.host, self.port).await?,
        };
        tcp.set_nodelay(true)?;
        let stream: BoxedIo = match self.tls {
            false => Box::new(tcp),
            true => {
                let name = match self.host.parse::<IpAddr>() {
                    Ok(ip) => rustls::pki_types::ServerName::IpAddress(ip.into()),
                    Err(_) => rustls::pki_types::ServerName::try_from(self.host.clone())
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                };
                Box::new(
                    tokio_rustls::TlsConnector::from(tls)
                        .connect(name, tcp)
                        .await?,
                )
            }
        };
        // Each message goes out as it is sent, as bytes written to a TCP
        // connection do, rather than waiting on a flush the TLS above may
        // not make.
        let config = WebSocketConfig::default().write_buffer_size(0);
        let (socket, _) =
            tokio_tungstenite::client_async_with_config(self.url.as_str(), stream, Some(config))
                .await
                .map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("WebSocket to {}: {e}", redacted(&self.url)),
                    )
                })?;
        Ok(Box::new(WsIo::new(socket)))
    }
}

impl std::fmt::Display for RelayUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let url = redacted(&self.url);
        match &self.proxy {
            None => write!(f, "websocket {url}"),
            Some(proxy) => write!(f, "websocket {url} via {proxy:?}"),
        }
    }
}

impl Proxy {
    /// `http://[user:password@]host[:port][/path]`, or the same without a
    /// scheme; a path is ignored. A proxy reached over TLS is not supported.
    fn parse(text: &str) -> Result<Self, String> {
        let shown = redacted(text);
        let rest = match text.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
            Some(_) => return Err(format!("proxy {shown:?} is not an http:// proxy")),
            None => text,
        };
        let (userinfo, rest) = match rest.rsplit_once('@') {
            Some((userinfo, rest)) => (Some(userinfo), rest),
            None => (None, rest),
        };
        let hostport = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let (host, port) = split_host_port(hostport)
            .ok_or_else(|| format!("proxy {shown:?} has no host, or a port that is not one"))?;
        let credentials = match userinfo {
            None => None,
            Some(userinfo) => {
                let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
                let decode = |part: &str| {
                    percent_decode(part)
                        .ok_or_else(|| format!("proxy {shown:?} has a malformed user or password"))
                };
                Some(format!("{}:{}", decode(user)?, decode(password)?))
            }
        };
        Ok(Proxy {
            host: host.trim_end_matches('.').to_string(),
            port: port.unwrap_or(80),
            credentials,
        })
    }

    /// A TCP connection through the proxy to `host:port`.
    async fn tunnel(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        let mut tcp = TcpStream::connect((self.host.as_str(), self.port)).await?;
        let target = match host.contains(':') {
            true => format!("[{host}]:{port}"),
            false => format!("{host}:{port}"),
        };
        let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
        if let Some(credentials) = &self.credentials {
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
            request.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
        }
        request.push_str("\r\n");
        tcp.write_all(request.as_bytes()).await?;
        // One byte at a time: nothing past the answer's blank line may be
        // taken from what the tunnel then carries.
        let mut answer = Vec::with_capacity(256);
        while !answer.ends_with(b"\r\n\r\n") {
            if answer.len() >= MAX_PROXY_ANSWER {
                return Err(io::Error::other("the proxy's answer is too long"));
            }
            let byte = tcp.read_u8().await?;
            answer.push(byte);
        }
        let status = String::from_utf8_lossy(&answer);
        let status = status.lines().next().unwrap_or_default();
        let code = status.split_whitespace().nth(1);
        if !status.starts_with("HTTP/1.") || code != Some("200") {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("the proxy refused CONNECT {target}: {status}"),
            ));
        }
        Ok(tcp)
    }
}

/// `text` with whatever precedes the last `@` of its authority, a user and
/// password, replaced by `***`: for every error and log line that names a
/// URL. Anything before the last `@` is taken as that, so a password with
/// a raw `/` or `@` is hidden whole.
fn redacted(text: &str) -> String {
    let (scheme, rest) = match text.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, text),
    };
    let rest = match rest.rfind('@') {
        Some(at) => format!("***@{}", &rest[at + 1..]),
        None => rest.to_string(),
    };
    match scheme {
        Some(scheme) => format!("{scheme}://{rest}"),
        None => rest,
    }
}

/// `%XX` escapes decoded, as a URL's user and password are written.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = text.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `host[:port]`, `[v6]` or `[v6]:port`.
fn split_host_port(text: &str) -> Option<(String, Option<u16>)> {
    let (host, port) = if let Some(rest) = text.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else if text.matches(':').count() == 1 {
        let (host, port) = text.split_once(':')?;
        (host, Some(port))
    } else {
        (text, None)
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        None => None,
        Some(port) => Some(port.parse().ok()?),
    };
    Some((host.to_ascii_lowercase(), port))
}

/// Whether a `NO_PROXY` list covers `host:port`: `*`, the host itself, a
/// domain it is under (`example.com`, `.example.com`, `*.example.com`), or
/// an exact address; an entry with a port matches that port only.
pub fn bypasses(list: &str, host: &str, port: u16) -> bool {
    let host = host.to_ascii_lowercase();
    list.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            let Some((name, only)) = split_host_port(entry) else {
                return false;
            };
            if only.is_some_and(|only| only != port) {
                return false;
            }
            let name = name.trim_start_matches("*.").trim_start_matches('.');
            host == name || host.ends_with(&format!(".{name}"))
        })
}

/// The outer TLS: the public web's roots, HTTP/1.1 for the upgrade.
fn web_roots() -> Arc<rustls::ClientConfig> {
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Arc::new(config)
        })
        .clone()
}

/// A WebSocket as a byte stream.
pub struct WsIo<S> {
    socket: WebSocketStream<S>,
    pending: Bytes,
    eof: bool,
    shut: bool,
}

impl<S> WsIo<S> {
    pub fn new(socket: WebSocketStream<S>) -> Self {
        WsIo {
            socket,
            pending: Bytes::new(),
            eof: false,
            shut: false,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if !this.pending.is_empty() {
                let n = this.pending.len().min(buf.remaining());
                buf.put_slice(&this.pending.split_to(n));
                return Poll::Ready(Ok(()));
            }
            if this.eof {
                return Poll::Ready(Ok(()));
            }
            match ready!(this.socket.poll_next_unpin(cx)) {
                Some(Ok(Message::Binary(b))) if b.is_empty() => this.eof = true,
                Some(Ok(Message::Binary(b))) => this.pending = b,
                Some(Ok(Message::Close(_))) | None => this.eof = true,
                Some(Ok(Message::Text(_))) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a text message on a byte stream",
                    )))
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Poll::Ready(Err(io::Error::other(e))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.shut {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        ready!(self.socket.poll_ready_unpin(cx)).map_err(io::Error::other)?;
        let n = buf.len().min(MAX_MESSAGE);
        self.socket
            .start_send_unpin(Message::Binary(Bytes::copy_from_slice(&buf[..n])))
            .map_err(io::Error::other)?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.socket.poll_flush_unpin(cx).map_err(io::Error::other)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.shut {
            ready!(self.socket.poll_ready_unpin(cx)).map_err(io::Error::other)?;
            self.socket
                .start_send_unpin(Message::Binary(Bytes::new()))
                .map_err(io::Error::other)?;
            self.shut = true;
        }
        self.socket.poll_flush_unpin(cx).map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    /// DigitalOcean's, as a sandbox there sees it.
    const DO_NO_PROXY: &str = "169.254.169.254,localhost,127.0.0.1,.vpc-endpoint.internal.digitalocean.com,otlp-insights.digitalocean.com";

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn no_proxy_lists_are_read_as_curl_reads_them() {
        assert!(!bypasses(DO_NO_PROXY, "tracon-hub.0x69.xyz", 443));
        assert!(bypasses(DO_NO_PROXY, "169.254.169.254", 80));
        assert!(bypasses(DO_NO_PROXY, "LocalHost", 443));
        assert!(bypasses(
            DO_NO_PROXY,
            "x.vpc-endpoint.internal.digitalocean.com",
            443
        ));
        assert!(!bypasses(DO_NO_PROXY, "evil-digitalocean.com", 443));
        assert!(bypasses("example.com", "hub.example.com", 443));
        assert!(!bypasses("example.com", "notexample.com", 443));
        assert!(bypasses("*.example.com", "hub.example.com", 443));
        assert!(bypasses("hub:8443", "hub", 8443));
        assert!(!bypasses("hub:8443", "hub", 443));
        assert!(bypasses("[::1]", "::1", 443));
        assert!(bypasses(" , *", "anything", 1));
        assert!(!bypasses("", "anything", 1));
    }

    #[test]
    fn the_proxy_is_the_environments_unless_its_no_proxy_says_otherwise() {
        let url = "wss://tracon-hub.0x69.xyz/v0/relay/connect";
        let on_do = env(&[
            ("HTTPS_PROXY", "http://169.254.169.254:3128"),
            ("NO_PROXY", DO_NO_PROXY),
        ]);
        let relay = RelayUrl::from_env(url, on_do).unwrap();
        assert_eq!(relay.proxy().as_deref(), Some("169.254.169.254:3128"));
        assert_eq!(
            (relay.host.as_str(), relay.port),
            ("tracon-hub.0x69.xyz", 443)
        );
        assert_eq!(relay.url(), url);

        // The bootstrap's copy comes first; lower case is read too.
        let both = env(&[
            ("TRACON_SANDBOX_PROXY", "10.0.0.1:3128"),
            ("https_proxy", "http://other:1"),
        ]);
        assert_eq!(
            RelayUrl::from_env(url, both).unwrap().proxy().as_deref(),
            Some("10.0.0.1:3128")
        );
        let lower = env(&[("https_proxy", "http://user:pw@proxy.internal")]);
        let relay = RelayUrl::from_env(url, lower).unwrap();
        assert_eq!(relay.proxy().as_deref(), Some("proxy.internal:80"));
        assert!(
            !format!("{relay:?} {relay}").contains("pw"),
            "credentials stay out of logs"
        );

        // ws:// takes HTTP_PROXY, not HTTPS_PROXY.
        let plain = "ws://hub.example:8080/v0/relay/connect";
        let https_only = env(&[("HTTPS_PROXY", "http://p:3128")]);
        assert_eq!(RelayUrl::from_env(plain, https_only).unwrap().proxy(), None);

        let excluded = env(&[("HTTPS_PROXY", "http://p:3128"), ("NO_PROXY", "0x69.xyz")]);
        assert_eq!(RelayUrl::from_env(url, excluded).unwrap().proxy(), None);

        for bad in [
            "https://hub/v0/relay/connect",
            "hub:443",
            "wss:///x",
            "not a url",
        ] {
            assert!(RelayUrl::direct(bad).is_err(), "{bad}");
        }
        let tls_proxy = env(&[("HTTPS_PROXY", "https://p:3128")]);
        assert!(RelayUrl::from_env(url, tls_proxy).is_err());

        // A fully qualified name's trailing dot is dropped.
        let dotted = RelayUrl::direct("wss://Hub.Example.net./v0/relay/connect").unwrap();
        assert_eq!(dotted.host, "hub.example.net");
    }

    #[test]
    fn a_proxy_url_is_read_as_curl_reads_it() {
        let p = Proxy::parse("http://us%40er:p%3Ass%2Fw@proxy.internal:3128/").unwrap();
        assert_eq!((p.host.as_str(), p.port), ("proxy.internal", 3128));
        assert_eq!(p.credentials.as_deref(), Some("us@er:p:ss/w"));
        // A path, query or fragment is ignored rather than refused.
        for text in [
            "http://proxy.internal:3128/some/path",
            "proxy.internal:3128/",
            "http://proxy.internal:3128?x=1",
        ] {
            let p = Proxy::parse(text).unwrap();
            assert_eq!(
                (p.host.as_str(), p.port),
                ("proxy.internal", 3128),
                "{text}"
            );
            assert_eq!(p.credentials, None);
        }
        let user_only = Proxy::parse("http://alice@p:1").unwrap();
        assert_eq!(user_only.credentials.as_deref(), Some("alice:"));
        assert!(Proxy::parse("http://a:%zz@p:1").is_err());
    }

    #[test]
    fn proxy_credentials_never_reach_an_error_or_a_log_line() {
        for text in [
            "https://alice:hunter2@p:3128",
            "http://alice:hunter2@p:notaport",
            "http://alice:hunter2@",
            "http://alice:hun/ter2@p:1x",
            "http://alice:%zzhunter2@p:1",
        ] {
            let error = Proxy::parse(text).err().unwrap();
            assert!(
                !error.contains("hunter2") && !error.contains("ter2"),
                "{error}"
            );
            assert!(!error.contains("alice"), "{error}");
        }
        let url = "wss://hub.example/v0/relay/connect";
        let relay = RelayUrl::from_env(url, env(&[("HTTPS_PROXY", "http://alice:hunter2@p:3128")]))
            .unwrap();
        let shown = format!(
            "{relay} {relay:?} {}",
            crate::carrier::Dial::WebSocket(relay.clone())
        );
        assert!(
            !shown.contains("hunter2") && !shown.contains("alice"),
            "{shown}"
        );
        assert_eq!(
            redacted("http://alice:hunter2@p:3128/x"),
            "http://***@p:3128/x"
        );
        assert_eq!(redacted("p:3128"), "p:3128");
    }

    /// A self-signed certificate for `localhost` and the client config that
    /// trusts it as the outer TLS would trust a public one.
    fn localhost_tls() -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = certified.cert.der().clone();
        let key =
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.into())
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        (Arc::new(server), Arc::new(client))
    }

    /// A WebSocket server standing in for the hub's relay route: echoes each
    /// binary message, and answers a half-close with `bye` and its own.
    /// Records the path each upgrade asked for.
    async fn relay_server(
        tls: Option<Arc<rustls::ServerConfig>>,
    ) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let paths: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = paths.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let (tls, seen) = (tls.clone(), seen.clone());
                tokio::spawn(async move {
                    let io: BoxedIo = match tls {
                        Some(tls) => Box::new(
                            tokio_rustls::TlsAcceptor::from(tls)
                                .accept(tcp)
                                .await
                                .unwrap(),
                        ),
                        None => Box::new(tcp),
                    };
                    // The callback's error type is tungstenite's to choose.
                    #[allow(clippy::result_large_err)]
                    let record =
                        |req: &tokio_tungstenite::tungstenite::handshake::server::Request, res| {
                            seen.lock().unwrap().push(req.uri().path().to_string());
                            Ok(res)
                        };
                    let mut ws = tokio_tungstenite::accept_hdr_async(io, record)
                        .await
                        .unwrap();
                    while let Some(Ok(msg)) = ws.next().await {
                        match msg {
                            Message::Binary(b) if b.is_empty() => {
                                ws.send(Message::Binary(Bytes::from_static(b"bye")))
                                    .await
                                    .unwrap();
                                ws.send(Message::Binary(Bytes::new())).await.unwrap();
                            }
                            Message::Binary(b) => ws.send(Message::Binary(b)).await.unwrap(),
                            _ => break,
                        }
                    }
                });
            }
        });
        (addr, paths)
    }

    /// An HTTP CONNECT proxy admitting `allow` only, as DigitalOcean's does;
    /// records each CONNECT's target.
    async fn connect_proxy(allow: &'static str) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let targets: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = targets.clone();
        tokio::spawn(async move {
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        head.push(client.read_u8().await.unwrap());
                    }
                    let head = String::from_utf8(head).unwrap();
                    let target = head.split_whitespace().nth(1).unwrap().to_string();
                    seen.lock().unwrap().push(target.clone());
                    let host = target.rsplit_once(':').unwrap().0;
                    if host != allow {
                        let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
                        return;
                    }
                    let port = target.rsplit_once(':').unwrap().1;
                    let mut upstream =
                        TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap()))
                            .await
                            .unwrap();
                    client
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });
        (addr, targets)
    }

    /// Bytes through the carrier and back, then a half-close answered.
    async fn exchange(mut io: BoxedIo) {
        let big = vec![b'x'; 200_000];
        io.write_all(&big).await.unwrap();
        io.flush().await.unwrap();
        let mut back = vec![0u8; big.len()];
        io.read_exact(&mut back).await.unwrap();
        assert_eq!(back, big);
        io.shutdown().await.unwrap();
        let mut rest = Vec::new();
        io.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"bye");
    }

    #[tokio::test]
    async fn a_plain_websocket_carries_bytes_and_half_closes() {
        let (addr, paths) = relay_server(None).await;
        let url = format!("ws://{addr}/v0/relay/connect");
        let dial = crate::carrier::Dial::WebSocket(RelayUrl::direct(&url).unwrap());
        exchange(dial.connect().await.unwrap()).await;
        // Each connect is a fresh dial.
        exchange(dial.connect().await.unwrap()).await;
        assert_eq!(*paths.lock().unwrap(), ["/v0/relay/connect"; 2]);
    }

    #[tokio::test]
    async fn wss_through_a_connect_proxy_and_around_it() {
        let (server_tls, client_tls) = localhost_tls();
        let (addr, paths) = relay_server(Some(server_tls)).await;
        let (proxy, targets) = connect_proxy("localhost").await;
        let url = format!("wss://localhost:{}/v0/relay/connect", addr.port());
        let proxied = env(&[
            ("HTTPS_PROXY", &format!("http://{proxy}")),
            ("NO_PROXY", "127.0.0.1,.internal"),
        ]);
        let relay = RelayUrl::from_env(&url, proxied).unwrap();
        exchange(relay.connect_with(client_tls.clone()).await.unwrap()).await;
        assert_eq!(
            *targets.lock().unwrap(),
            [format!("localhost:{}", addr.port())]
        );

        // NO_PROXY naming the host: dialled directly, the proxy untouched.
        let around = env(&[
            ("HTTPS_PROXY", &format!("http://{proxy}")),
            ("NO_PROXY", "localhost"),
        ]);
        let relay = RelayUrl::from_env(&url, around).unwrap();
        assert_eq!(relay.proxy(), None);
        exchange(relay.connect_with(client_tls.clone()).await.unwrap()).await;
        assert_eq!(targets.lock().unwrap().len(), 1);
        assert_eq!(paths.lock().unwrap().len(), 2);

        // A host the proxy does not admit: its refusal is the error.
        let elsewhere = format!("wss://127.0.0.1:{}/v0/relay/connect", addr.port());
        let refused = RelayUrl::from_env(&elsewhere, env(&[("HTTPS_PROXY", &proxy.to_string())]))
            .unwrap()
            .connect_with(client_tls.clone())
            .await
            .err()
            .unwrap();
        assert!(refused.to_string().contains("403"), "{refused}");

        // The outer TLS is checked: the web's roots do not hold this one.
        let direct = RelayUrl::direct(&url).unwrap();
        assert!(direct.connect().await.is_err());
    }
}
