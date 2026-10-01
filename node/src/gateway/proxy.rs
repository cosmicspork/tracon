//! An HTTP CONNECT proxy with a default-deny host allowlist. The same contract
//! as the gateway container's tinyproxy (`containers/gateway/tinyproxy.conf`):
//! only CONNECT, only port 443, only hosts matching an anchored entry. Nothing
//! else is forwarded — a plain `GET http://…` gets a refusal, not a fetch.
//!
//! The same proxy also serves **grants** (`serve_granted`): one client, one
//! token, one set of hosts. A session, a dependency preparation and a QA
//! browser run each present their own token as proxy credentials and are
//! filtered by their own grant, so nothing one of them may reach is reachable
//! by another, and a refusal is known to be *that* client's.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

use crate::boundary::podman::setup::anchor;

/// Hosts a harness may CONNECT to. Entries are anchored regexes, exactly as
/// the gateway allowlist file holds them.
#[derive(Clone, Debug)]
pub struct Allowlist {
    hosts: Vec<regex::Regex>,
}

impl Allowlist {
    pub fn new(entries: &[String]) -> Result<Self, regex::Error> {
        let hosts = entries
            .iter()
            .map(|e| regex::Regex::new(&anchor(e)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { hosts })
    }

    pub fn allows(&self, host: &str) -> bool {
        self.hosts.iter().any(|r| r.is_match(host))
    }
}

/// The verdict on one request, before anything is dialled.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Dial this host on 443 and splice.
    Connect(String),
    Refused(&'static str),
}

pub fn decide(method: &Method, authority: Option<&str>, allow: &Allowlist) -> Decision {
    if method != Method::CONNECT {
        return Decision::Refused("only CONNECT is proxied");
    }
    let Some(authority) = authority else {
        return Decision::Refused("CONNECT needs host:port");
    };
    let Some((host, port)) = authority.rsplit_once(':') else {
        return Decision::Refused("CONNECT needs host:port");
    };
    if port != "443" {
        return Decision::Refused("only port 443 is proxied");
    }
    let host = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if !allow.allows(&host) {
        return Decision::Refused("host is not allowlisted");
    }
    Decision::Connect(host)
}

pub async fn serve(port: u16, allow: Allowlist) {
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "connect proxy could not bind");
            return;
        }
    };
    run(listener, allow).await
}

pub async fn run(listener: TcpListener, allow: Allowlist) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let allow = allow.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| handle(req, peer, allow.clone()));
            if let Err(e) = http1::Builder::new()
                .serve_connection(io, svc)
                .with_upgrades()
                .await
            {
                tracing::debug!(%peer, error = %e, "proxy connection ended");
            }
        });
    }
}

type Body = http_body_util::Either<Empty<Bytes>, Full<Bytes>>;

async fn handle(
    req: Request<Incoming>,
    peer: SocketAddr,
    allow: Allowlist,
) -> Result<Response<Body>, hyper::Error> {
    let authority = req.uri().authority().map(|a| a.as_str().to_string());
    let host = match decide(req.method(), authority.as_deref(), &allow) {
        Decision::Connect(h) => h,
        Decision::Refused(why) => {
            tracing::info!(%peer, target = authority.as_deref().unwrap_or("-"), why, "proxy refused");
            return Ok(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(http_body_util::Either::Right(Full::from(why)))
                .expect("static response"));
        }
    };
    let upstream = match TcpStream::connect((host.as_str(), 443)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%peer, %host, error = %e, "proxy could not reach host");
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(http_body_util::Either::Right(Full::from(
                    "upstream unreachable",
                )))
                .expect("static response"));
        }
    };
    tracing::info!(%peer, %host, "proxy connect");
    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let mut client = TokioIo::new(upgraded);
                let mut upstream = upstream;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            }
            Err(e) => tracing::debug!(error = %e, "proxy upgrade failed"),
        }
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(http_body_util::Either::Left(Empty::new()))
        .expect("static response"))
}

/// What one client may reach, and what it is told when it asks for more.
#[derive(Debug, Clone, Default)]
pub struct GrantSpec {
    /// Who this is, for the log and for the credentials' user name: `session`,
    /// `prepare`, `qa`.
    pub client: String,
    /// The session a refusal should be recorded against, when there is one.
    pub session_id: Option<String>,
    /// Literal host names, matched exactly.
    pub hosts: Vec<String>,
    /// Anchored patterns, as `[gateway] allow_hosts` writes them.
    pub patterns: Vec<String>,
    /// Whether a plain `http://` request to a granted host is forwarded, on
    /// whatever port it names. Only a QA target's own origin needs this; a
    /// registry is always HTTPS.
    pub plain_http: bool,
    /// The sentence a refused host gets back, as the status line's reason
    /// and the body: what to change, for whoever reads the failure.
    pub refusal: String,
}

/// A grant as the proxy holds it.
#[derive(Debug)]
pub struct Grant {
    pub client: String,
    pub session_id: Option<String>,
    hosts: Vec<String>,
    patterns: Allowlist,
    plain_http: bool,
    refusal: String,
}

impl Grant {
    fn allows(&self, host: &str) -> bool {
        self.hosts.iter().any(|granted| granted == host) || self.patterns.allows(host)
    }
}

/// Called with each host a grant refused. Not the proxy's business what
/// happens next; a session's are recorded where its operator will see them.
pub type OnRefusal = Arc<dyn Fn(&Grant, &str) + Send + Sync>;

/// Every live grant, by its token. Shared by whoever issues them and the
/// listener that honours them; a token that is not here opens nothing.
#[derive(Clone, Default)]
pub struct Grants {
    live: Arc<parking_lot::RwLock<HashMap<String, Arc<Grant>>>>,
    on_refusal: Arc<parking_lot::RwLock<Option<OnRefusal>>>,
}

impl Grants {
    /// Admit one client and return its token. The grant lasts until it is
    /// revoked; nothing expires it, because what it is for (a run, a session)
    /// knows when it is over and a timer does not.
    pub fn issue(&self, spec: GrantSpec) -> Result<String, regex::Error> {
        use rand::Rng;
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        let token = hex::encode(bytes);
        let grant = Grant {
            client: spec.client,
            session_id: spec.session_id,
            hosts: spec
                .hosts
                .iter()
                .map(|host| host.to_ascii_lowercase())
                .collect(),
            patterns: Allowlist::new(&spec.patterns)?,
            plain_http: spec.plain_http,
            refusal: spec.refusal,
        };
        self.live.write().insert(token.clone(), Arc::new(grant));
        Ok(token)
    }

    pub fn revoke(&self, token: &str) {
        self.live.write().remove(token);
    }

    pub fn on_refusal(&self, observer: OnRefusal) {
        *self.on_refusal.write() = Some(observer);
    }

    fn holder(&self, token: &str) -> Option<Arc<Grant>> {
        self.live.read().get(token).cloned()
    }

    fn refused(&self, grant: &Grant, host: &str) {
        let observer = self.on_refusal.read().clone();
        if let Some(observer) = observer {
            observer(grant, host);
        }
    }
}

/// An address on this machine, or one that only means something on its own
/// link. The proxy dials from the node's process, where `localhost` is the
/// node itself — and its operator API trusts loopback — and where the
/// link-local range holds a cloud's metadata service. A granted name that
/// resolves there is refused whatever the grant says.
pub fn internal_address(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast()
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => internal_address(&IpAddr::V4(v4)),
            None => {
                v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        },
    }
}

/// The granted proxy: the registry it honours and the addresses it will not
/// dial.
#[derive(Clone)]
pub struct Granted {
    grants: Grants,
    refuse: fn(&IpAddr) -> bool,
}

impl Granted {
    pub fn new(grants: Grants) -> Self {
        Self {
            grants,
            refuse: internal_address,
        }
    }

    /// As `new`, dialling any address. For tests, whose upstreams can only be
    /// on loopback.
    #[doc(hidden)]
    pub fn dialling_any_address(grants: Grants) -> Self {
        Self {
            grants,
            refuse: |_| false,
        }
    }
}

pub async fn serve_granted(listener: TcpListener, granted: Granted) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        spawn_granted(TokioIo::new(stream), peer.to_string(), granted.clone());
    }
}

/// The same proxy on a Unix socket, which is how a gateway container on a
/// Linux host reaches the node without a listener that faces the network.
#[cfg(unix)]
pub async fn serve_granted_unix(listener: tokio::net::UnixListener, granted: Granted) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        spawn_granted(TokioIo::new(stream), "gateway".into(), granted.clone());
    }
}

fn spawn_granted<I>(io: I, peer: String, granted: Granted)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let svc = service_fn(move |req| handle_granted(req, granted.clone()));
        if let Err(e) = http1::Builder::new()
            .serve_connection(io, svc)
            .with_upgrades()
            .await
        {
            tracing::debug!(%peer, error = %e, "granted proxy connection ended");
        }
    });
}

type Forwarded = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

fn answer(status: StatusCode, said: &str) -> Response<Forwarded> {
    let mut response = Response::builder()
        .status(status)
        .body(
            Full::from(format!("{said}\n"))
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("static response");
    // Most clients show a failed CONNECT's status line and nothing else, so
    // the sentence goes there as well as in the body.
    if let Ok(reason) = hyper::ext::ReasonPhrase::try_from(said.as_bytes()) {
        response.extensions_mut().insert(reason);
    }
    response
}

/// The token a request presents: the password of its `Proxy-Authorization:
/// Basic` credentials. The user name is the client's label and proves nothing.
fn presented_token(req: &Request<Incoming>) -> Option<String> {
    use base64::Engine;
    let header = req.headers().get(hyper::header::PROXY_AUTHORIZATION)?;
    let encoded = header.to_str().ok()?.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let credentials = String::from_utf8(decoded).ok()?;
    let (_, token) = credentials.split_once(':')?;
    Some(token.to_string())
}

async fn handle_granted(
    mut req: Request<Incoming>,
    granted: Granted,
) -> Result<Response<Forwarded>, hyper::Error> {
    let Some(grant) = presented_token(&req).and_then(|token| granted.grants.holder(&token)) else {
        // A challenge, not a refusal: Git and browsers send their credentials
        // only once asked, and on this same connection.
        let mut response = answer(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            "this proxy needs the credentials the node issued",
        );
        response.headers_mut().insert(
            hyper::header::PROXY_AUTHENTICATE,
            hyper::header::HeaderValue::from_static("Basic realm=\"tracon\""),
        );
        return Ok(response);
    };
    let tunnel = req.method() == Method::CONNECT;
    let target = match (tunnel, req.uri().scheme_str(), req.uri().host()) {
        (true, _, _) => req
            .uri()
            .authority()
            .and_then(|authority| authority.as_str().rsplit_once(':'))
            .filter(|(_, port)| *port == "443")
            .map(|(host, _)| (host.to_string(), 443)),
        (false, Some("http"), Some(host)) if grant.plain_http => {
            Some((host.to_string(), req.uri().port_u16().unwrap_or(80)))
        }
        _ => None,
    };
    let Some((host, port)) = target else {
        return Ok(answer(
            StatusCode::FORBIDDEN,
            "only CONNECT to port 443 is proxied",
        ));
    };
    let host = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if !grant.allows(&host) {
        tracing::info!(client = %grant.client, %host, "granted proxy refused");
        granted.grants.refused(&grant, &host);
        return Ok(answer(StatusCode::FORBIDDEN, &grant.refusal));
    }
    // Resolved here and dialled by address: the name is checked once, and
    // what it resolved to is what gets the connection.
    let resolved: Vec<SocketAddr> = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(found) => found.collect(),
        Err(_) => Vec::new(),
    };
    let addresses: Vec<&SocketAddr> = resolved
        .iter()
        .filter(|addr| !(granted.refuse)(&addr.ip()))
        .collect();
    if addresses.is_empty() && !resolved.is_empty() {
        tracing::warn!(client = %grant.client, %host, "granted host resolves to this machine");
        return Ok(answer(
            StatusCode::FORBIDDEN,
            "resolves to an address on this machine; not proxied",
        ));
    }
    let mut upstream = None;
    for address in &addresses {
        if let Ok(stream) = TcpStream::connect(address).await {
            upstream = Some(stream);
            break;
        }
    }
    let Some(upstream) = upstream else {
        tracing::warn!(client = %grant.client, %host, "granted proxy could not reach host");
        return Ok(answer(StatusCode::BAD_GATEWAY, "upstream unreachable"));
    };
    tracing::info!(client = %grant.client, %host, port, "granted proxy connect");
    if tunnel {
        tokio::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    let mut client = TokioIo::new(upgraded);
                    let mut upstream = upstream;
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
                Err(e) => tracing::debug!(error = %e, "proxy upgrade failed"),
            }
        });
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Empty::new().map_err(|never| match never {}).boxed())
            .expect("static response"));
    }
    // A plain request is sent on as the origin would be asked directly: its
    // path alone, and nothing that was said to the proxy rather than to it.
    let path = req
        .uri()
        .path_and_query()
        .map(|path| path.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let Ok(path) = path.parse() else {
        return Ok(answer(StatusCode::BAD_REQUEST, "unusable request target"));
    };
    *req.uri_mut() = path;
    req.headers_mut().remove(hyper::header::PROXY_AUTHORIZATION);
    req.headers_mut().remove("proxy-connection");
    let Ok((mut sender, connection)) =
        hyper::client::conn::http1::handshake(TokioIo::new(upstream)).await
    else {
        return Ok(answer(StatusCode::BAD_GATEWAY, "upstream unreachable"));
    };
    tokio::spawn(connection);
    match sender.send_request(req).await {
        Ok(response) => Ok(response.map(|body| body.boxed())),
        Err(_) => Ok(answer(StatusCode::BAD_GATEWAY, "upstream unreachable")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow() -> Allowlist {
        Allowlist::new(&[r"^api\.anthropic\.com$".into(), "api.openai.com".into()]).unwrap()
    }

    #[test]
    fn only_allowlisted_connects_on_443_pass() {
        let a = allow();
        assert_eq!(
            decide(&Method::CONNECT, Some("api.anthropic.com:443"), &a),
            Decision::Connect("api.anthropic.com".into())
        );
        // Plain entries are anchored, so a suffix cannot slip past.
        assert!(matches!(
            decide(&Method::CONNECT, Some("api.openai.com.evil.com:443"), &a),
            Decision::Refused(_)
        ));
        assert!(matches!(
            decide(&Method::CONNECT, Some("example.com:443"), &a),
            Decision::Refused(_)
        ));
        assert!(matches!(
            decide(&Method::CONNECT, Some("api.anthropic.com:80"), &a),
            Decision::Refused(_)
        ));
        // Nothing but CONNECT: no plain-HTTP fetching through the node.
        assert!(matches!(
            decide(&Method::GET, Some("api.anthropic.com:443"), &a),
            Decision::Refused(_)
        ));
    }

    #[test]
    fn this_machine_and_its_link_are_never_dialled() {
        for internal in [
            "127.0.0.1",
            "127.8.9.10",
            "0.0.0.0",
            "169.254.169.254",
            "::1",
            "::",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(internal_address(&internal.parse().unwrap()), "{internal}");
        }
        // A LAN address is somewhere else: a QA target on the operator's own
        // network is a legitimate thing to name.
        for external in ["192.168.1.20", "10.0.0.5", "1.1.1.1", "2606:4700::1111"] {
            assert!(!internal_address(&external.parse().unwrap()), "{external}");
        }
    }
}
