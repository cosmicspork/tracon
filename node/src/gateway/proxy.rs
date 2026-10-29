//! An HTTP CONNECT proxy with a default-deny host allowlist. The same contract
//! as the gateway container's tinyproxy (`containers/gateway/tinyproxy.conf`):
//! only CONNECT, only port 443, only hosts matching an anchored entry. Nothing
//! else is forwarded — a plain `GET http://…` gets a refusal, not a fetch.
//!
//! The same proxy also serves **grants** (`serve_granted`): one client, one
//! token, one set of hosts. A session and a dependency preparation each
//! present their own token as proxy credentials and are
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
    serve_refusing(port, allow, Refusal::nothing()).await
}

/// `serve`, never dialling what `refusal` refuses: how a pod-hosted node
/// serves its harnesses' allowlist proxy, beside the cluster's own services.
pub async fn serve_refusing(port: u16, allow: Allowlist, refusal: Refusal) {
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "connect proxy could not bind");
            return;
        }
    };
    run_refusing(listener, allow, refusal, system_resolver()).await
}

pub async fn run(listener: TcpListener, allow: Allowlist) {
    run_refusing(listener, allow, Refusal::nothing(), system_resolver()).await
}

/// `run` with what it may never dial and how it looks names up.
pub async fn run_refusing(
    listener: TcpListener,
    allow: Allowlist,
    refusal: Refusal,
    resolve: Resolve,
) {
    let proxied = Proxied {
        allow,
        refusal,
        resolve,
    };
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let proxied = proxied.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| handle(req, peer, proxied.clone()));
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

/// The allowlist proxy's state: what it admits, what it never dials, and
/// how it looks names up.
#[derive(Clone)]
struct Proxied {
    allow: Allowlist,
    refusal: Refusal,
    resolve: Resolve,
}

type Body = http_body_util::Either<Empty<Bytes>, Full<Bytes>>;

async fn handle(
    req: Request<Incoming>,
    peer: SocketAddr,
    proxied: Proxied,
) -> Result<Response<Body>, hyper::Error> {
    let refused = |why: &str| {
        Ok(Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(http_body_util::Either::Right(Full::from(why.to_string())))
            .expect("static response"))
    };
    let authority = req.uri().authority().map(|a| a.as_str().to_string());
    let host = match decide(req.method(), authority.as_deref(), &proxied.allow) {
        Decision::Connect(h) => h,
        Decision::Refused(why) => {
            tracing::info!(%peer, target = authority.as_deref().unwrap_or("-"), why, "proxy refused");
            return refused(why);
        }
    };
    let upstream = match dial(&proxied.refusal, &proxied.resolve, &host, 443).await {
        Dialled::Stream(stream) => stream,
        Dialled::Refused(why) => {
            tracing::warn!(%peer, %host, why, "proxy refused an internal host");
            return refused(why);
        }
        Dialled::Unreachable => {
            tracing::warn!(%peer, %host, "proxy could not reach host");
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

/// What came of dialling a host a proxy admitted.
enum Dialled {
    Stream(TcpStream),
    /// Refused by `Refusal`, by name or by every address it resolved to.
    Refused(&'static str),
    /// Nothing resolved, or nothing that passed answered.
    Unreachable,
}

/// Dial `host` as both proxies do: refused by name before any lookup, looked
/// up once (as absolute in a cluster), and dialled only at the addresses
/// that pass, so the addresses checked are the addresses dialled and a name
/// that answers differently the second time (DNS rebinding) is never asked a
/// second time.
async fn dial(refusal: &Refusal, resolve: &Resolve, host: &str, port: u16) -> Dialled {
    if let Some(why) = refusal.name(host) {
        return Dialled::Refused(why);
    }
    let resolved: Vec<SocketAddr> = resolve(refusal.lookup_name(host), port)
        .await
        .unwrap_or_default();
    let mut refused = None;
    let addresses: Vec<&SocketAddr> = resolved
        .iter()
        .filter(|addr| match refusal.address(&addr.ip()) {
            Some(why) => {
                refused = Some(why);
                false
            }
            None => true,
        })
        .collect();
    if let (true, Some(why)) = (addresses.is_empty(), refused) {
        return Dialled::Refused(why);
    }
    for address in addresses {
        if let Ok(stream) = TcpStream::connect(address).await {
            return Dialled::Stream(stream);
        }
    }
    Dialled::Unreachable
}

/// What one client may reach, and what it is told when it asks for more.
#[derive(Debug, Clone, Default)]
pub struct GrantSpec {
    /// Who this is, for the log and for the credentials' user name: `session`,
    /// `prepare`.
    pub client: String,
    /// The session a refusal should be recorded against, when there is one.
    pub session_id: Option<String>,
    /// The repository whose default-branch warm-up this is, when it is one: a
    /// refusal then asks the operator to save the host to that repository's
    /// egress. No other preparation asks, because no other runs merged code.
    pub warming: Option<std::path::PathBuf>,
    /// Literal host names, matched exactly.
    pub hosts: Vec<String>,
    /// Anchored patterns, as `[gateway] allow_hosts` writes them.
    pub patterns: Vec<String>,
    /// Whether a plain `http://` request to a granted host is forwarded, on
    /// whatever port it names. No current client asks for it: a registry is
    /// always HTTPS.
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
    pub warming: Option<std::path::PathBuf>,
    hosts: Vec<String>,
    patterns: Allowlist,
    plain_http: bool,
    refusal: String,
    /// Hosts the operator opened to this grant after it was issued: until it
    /// is revoked (`None`), or until a moment (`Some`).
    widened: parking_lot::RwLock<HashMap<String, Option<std::time::Instant>>>,
    /// Hosts this grant refused, in the order first refused, so whoever held
    /// it can say which when what it ran fails.
    refused: parking_lot::Mutex<Vec<String>>,
}

/// How many refused hosts a grant remembers. A preparation that hits more
/// than this is told about the first few, which is enough to act on.
const REMEMBERED_REFUSALS: usize = 8;

impl Grant {
    /// Whether this grant reaches `host`, as issued or as widened since.
    pub fn allows(&self, host: &str) -> bool {
        if self.hosts.iter().any(|granted| granted == host) || self.patterns.allows(host) {
            return true;
        }
        match self.widened.read().get(host) {
            Some(None) => true,
            Some(Some(until)) => std::time::Instant::now() < *until,
            None => false,
        }
    }

    /// The hosts this grant refused, first refused first.
    pub fn refused(&self) -> Vec<String> {
        self.refused.lock().clone()
    }

    fn remember_refusal(&self, host: &str) {
        let mut refused = self.refused.lock();
        if refused.len() < REMEMBERED_REFUSALS && !refused.iter().any(|seen| seen == host) {
            refused.push(host.to_string());
        }
    }

    fn widen(&self, host: &str, until: Option<std::time::Instant>) {
        let mut widened = self.widened.write();
        // A wider answer is never narrowed by a later, shorter one.
        if widened.get(host).is_some_and(Option::is_none) {
            return;
        }
        widened.insert(host.to_string(), until);
    }
}

/// Called with each host a grant refused. Not the proxy's business what
/// happens next; a session's are recorded where its operator will see them.
/// What it returns, when anything, replaces the grant's refusal sentence for
/// this one answer: the place to say that the operator has been asked.
pub type OnRefusal = Arc<dyn Fn(&Grant, &str) -> Option<String> + Send + Sync>;

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
            warming: spec.warming,
            hosts: spec
                .hosts
                .iter()
                .map(|host| host.to_ascii_lowercase())
                .collect(),
            patterns: Allowlist::new(&spec.patterns)?,
            plain_http: spec.plain_http,
            refusal: spec.refusal,
            widened: Default::default(),
            refused: Default::default(),
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

    /// The hosts the grant behind `token` refused so far; none for a token
    /// that is not live.
    pub fn refused_by(&self, token: &str) -> Vec<String> {
        self.holder(token)
            .map(|grant| grant.refused())
            .unwrap_or_default()
    }

    fn refused(&self, grant: &Grant, host: &str) -> Option<String> {
        grant.remember_refusal(host);
        let observer = self.on_refusal.read().clone();
        observer.and_then(|observer| observer(grant, host))
    }

    /// The live grants a session holds. Ordinarily one; none once it ended,
    /// or when its harness keeps the shared proxy and was never issued one.
    fn of_session(&self, session_id: &str) -> Vec<Arc<Grant>> {
        self.live
            .read()
            .values()
            .filter(|grant| grant.session_id.as_deref() == Some(session_id))
            .cloned()
            .collect()
    }

    /// Whether a session holds a grant of its own to widen.
    pub fn holds_session(&self, session_id: &str) -> bool {
        !self.of_session(session_id).is_empty()
    }

    /// Whether the session's grant already reaches `host`.
    pub fn session_allows(&self, session_id: &str, host: &str) -> bool {
        self.of_session(session_id)
            .iter()
            .any(|grant| grant.allows(&host.to_ascii_lowercase()))
    }

    /// Open `host` to a session's live grant, until it is revoked or until
    /// `until`. False when the session holds none: an answer that arrives
    /// after it ended opens nothing.
    pub fn widen_session(
        &self,
        session_id: &str,
        host: &str,
        until: Option<std::time::Instant>,
    ) -> bool {
        let grants = self.of_session(session_id);
        for grant in &grants {
            grant.widen(&host.to_ascii_lowercase(), until);
        }
        !grants.is_empty()
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

/// An address block, `10.0.0.0/8` or `fc00::/7`. A bare address is a block
/// of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl std::str::FromStr for Cidr {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let network: IpAddr = address
            .parse()
            .map_err(|_| format!("{text:?} is not an address block such as 10.0.0.0/8"))?;
        let most = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => most,
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|prefix| *prefix <= most)
                .ok_or_else(|| format!("{text:?} has no usable prefix length"))?,
        };
        Ok(Self { network, prefix })
    }
}

impl Cidr {
    pub fn contains(&self, ip: &IpAddr) -> bool {
        // An IPv4 address written as IPv6 is the same host.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(*ip, IpAddr::V4),
            IpAddr::V4(_) => *ip,
        };
        let masked = |bits: u128, width: u32| match self.prefix {
            0 => 0,
            prefix => bits >> (width - u32::from(prefix)),
        };
        match (self.network, ip) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                masked(u32::from(network).into(), 32) == masked(u32::from(ip).into(), 32)
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                masked(u128::from(network), 128) == masked(u128::from(ip), 128)
            }
            _ => false,
        }
    }
}

/// The private and shared address blocks a pod-hosted node refuses to dial
/// for a grant: RFC 1918, carrier-grade NAT (where some clusters put their
/// services and pods), and IPv6 unique-local. A cluster's pods, services and
/// API server, and the network the cluster sits on, are in these.
pub const PRIVATE_BLOCKS: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
    "fc00::/7",
];

/// Name suffixes that only resolve inside a cluster or a cloud's own network,
/// refused before any lookup.
pub const CLUSTER_SUFFIXES: &[&str] = &[".svc", ".cluster.local", ".internal"];

/// What the granted proxy will not dial, whatever a grant names. Decided per
/// runtime: under Podman the node's proxy sits on the operator's machine,
/// where a host on their own network is a legitimate thing to name, so only
/// this machine and its link are refused. In a pod the same proxy sits on
/// the cluster network, beside the API server, the cluster's services and
/// every other pod, none of which a grant was ever meant to reach.
#[derive(Clone, Debug)]
pub struct Refusal {
    /// Loopback, link-local, unspecified and broadcast (`internal_address`).
    this_machine: bool,
    blocks: Vec<Cidr>,
    /// Refuse names that resolve inside the cluster: the suffixes above, and
    /// a name with no dot, which a cluster resolver's search list would
    /// complete into one.
    cluster_names: bool,
}

impl Refusal {
    /// This machine and its link: the Podman node's refusal.
    pub fn this_machine() -> Self {
        Self {
            this_machine: true,
            blocks: Vec::new(),
            cluster_names: false,
        }
    }

    /// This machine, the private ranges, `extra` (`[runtime.kubernetes]
    /// internal_cidrs`, for a cluster whose pod or service range is not
    /// private), and cluster names: the pod-hosted node's refusal.
    pub fn cluster(extra: &[String]) -> Result<Self, String> {
        let blocks = PRIVATE_BLOCKS
            .iter()
            .map(|block| block.parse())
            .chain(extra.iter().map(|block| block.parse()))
            .collect::<Result<Vec<Cidr>, String>>()?;
        Ok(Self {
            this_machine: true,
            blocks,
            cluster_names: true,
        })
    }

    /// Nothing refused. For tests, whose upstreams can only be on loopback.
    #[doc(hidden)]
    pub fn nothing() -> Self {
        Self {
            this_machine: false,
            blocks: Vec::new(),
            cluster_names: false,
        }
    }

    /// Only these blocks refused. For tests, which need loopback dialled and
    /// something else not.
    #[doc(hidden)]
    pub fn only(blocks: &[&str]) -> Self {
        Self {
            this_machine: false,
            blocks: blocks.iter().map(|block| block.parse().unwrap()).collect(),
            cluster_names: false,
        }
    }

    /// Why `host` is refused before it is looked up, if it is: in a cluster,
    /// a cluster name, or an address literal that is refused anyway.
    pub fn name(&self, host: &str) -> Option<&'static str> {
        if !self.cluster_names {
            return None;
        }
        let host = host.trim_end_matches('.');
        if let Ok(ip) = host.parse::<IpAddr>() {
            return self.address(&ip);
        }
        let inside = CLUSTER_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix) || host == &suffix[1..]);
        (inside || !host.contains('.')).then_some("names a host inside the cluster; not proxied")
    }

    /// Why an address `host` resolved to is not dialled, if it is not.
    pub fn address(&self, ip: &IpAddr) -> Option<&'static str> {
        if self.this_machine && internal_address(ip) {
            return Some("resolves to an address on this machine; not proxied");
        }
        self.blocks
            .iter()
            .any(|block| block.contains(ip))
            .then_some("resolves to a private or cluster address; not proxied")
    }

    /// The name to look up. A cluster's resolver completes a relative name
    /// through its search list (`<name>.<namespace>.svc.cluster.local` first,
    /// with `ndots:5`), so in a cluster the name is looked up as absolute:
    /// what the grant names is the only name asked about.
    fn lookup_name(&self, host: &str) -> String {
        match self.cluster_names && host.parse::<IpAddr>().is_err() && !host.ends_with('.') {
            true => format!("{host}."),
            false => host.to_string(),
        }
    }
}

/// How the granted proxy turns a name into addresses: the system resolver,
/// or a test's.
pub type Resolve = Arc<
    dyn Fn(
            String,
            u16,
        ) -> futures_util::future::BoxFuture<'static, std::io::Result<Vec<SocketAddr>>>
        + Send
        + Sync,
>;

fn system_resolver() -> Resolve {
    Arc::new(|host, port| {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), port))
                .await?
                .collect())
        })
    })
}

/// The granted proxy: the registry it honours, the addresses it will not
/// dial, and how it looks names up.
#[derive(Clone)]
pub struct Granted {
    grants: Grants,
    refusal: Refusal,
    resolve: Resolve,
}

impl Granted {
    /// The Podman node's proxy: this machine and its link refused.
    pub fn new(grants: Grants) -> Self {
        Self::refusing(grants, Refusal::this_machine())
    }

    pub fn refusing(grants: Grants, refusal: Refusal) -> Self {
        Self {
            grants,
            refusal,
            resolve: system_resolver(),
        }
    }

    /// As `new`, dialling any address. For tests, whose upstreams can only be
    /// on loopback.
    #[doc(hidden)]
    pub fn dialling_any_address(grants: Grants) -> Self {
        Self::refusing(grants, Refusal::nothing())
    }

    /// Look names up with `resolve` rather than the system's. For tests.
    #[doc(hidden)]
    pub fn resolving_with(mut self, resolve: Resolve) -> Self {
        self.resolve = resolve;
        self
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
    // Before the grant: a cluster name or address is refused whatever the grant says,
    // so it never raises an ask that no answer could satisfy.
    if let Some(why) = granted.refusal.name(&host) {
        tracing::warn!(client = %grant.client, %host, "granted proxy refused a cluster name");
        return Ok(answer(StatusCode::FORBIDDEN, why));
    }
    if !grant.allows(&host) {
        tracing::info!(client = %grant.client, %host, "granted proxy refused");
        let said = granted.grants.refused(&grant, &host);
        return Ok(answer(
            StatusCode::FORBIDDEN,
            said.as_deref().unwrap_or(&grant.refusal),
        ));
    }
    let upstream = match dial(&granted.refusal, &granted.resolve, &host, port).await {
        Dialled::Stream(stream) => stream,
        Dialled::Refused(why) => {
            tracing::warn!(client = %grant.client, %host, why, "granted host resolves inside");
            return Ok(answer(StatusCode::FORBIDDEN, why));
        }
        Dialled::Unreachable => {
            tracing::warn!(client = %grant.client, %host, "granted proxy could not reach host");
            return Ok(answer(StatusCode::BAD_GATEWAY, "upstream unreachable"));
        }
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
        // A LAN address is somewhere else: a host on the operator's own
        // network is a legitimate thing to name.
        for external in ["192.168.1.20", "10.0.0.5", "1.1.1.1", "2606:4700::1111"] {
            assert!(!internal_address(&external.parse().unwrap()), "{external}");
        }
        // And the Podman refusal is exactly that, by address and by name.
        let podman = Refusal::this_machine();
        assert!(podman
            .address(&"169.254.169.254".parse().unwrap())
            .is_some());
        assert!(podman.address(&"10.0.0.5".parse().unwrap()).is_none());
        assert!(podman.name("nas.internal").is_none());
        assert!(podman.name("intranet").is_none());
        assert_eq!(podman.lookup_name("pypi.org"), "pypi.org");
    }

    #[test]
    fn an_address_block_holds_what_its_prefix_says() {
        let block: Cidr = "100.64.0.0/10".parse().unwrap();
        assert!(block.contains(&"100.64.0.1".parse().unwrap()));
        assert!(block.contains(&"100.127.255.255".parse().unwrap()));
        assert!(!block.contains(&"100.128.0.0".parse().unwrap()));
        // Written as IPv6, an IPv4 address is still in its block.
        assert!(block.contains(&"::ffff:100.64.3.4".parse().unwrap()));
        let ula: Cidr = "fc00::/7".parse().unwrap();
        assert!(ula.contains(&"fd12:3456::1".parse().unwrap()));
        assert!(!ula.contains(&"2606:4700::1111".parse().unwrap()));
        let one: Cidr = "203.0.113.7".parse().unwrap();
        assert!(one.contains(&"203.0.113.7".parse().unwrap()));
        assert!(!one.contains(&"203.0.113.8".parse().unwrap()));
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(&"8.8.8.8".parse().unwrap()));
        for bad in [
            "10.0.0.0/33",
            "10.0.0/8",
            "fc00::/129",
            "pods",
            "10.0.0.0/x",
        ] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad}");
        }
    }

    /// In a cluster the proxy sits beside the API server and every service:
    /// A grant remembers what it refused, once each and only the first few,
    /// for whoever held it to name when what it ran fails.
    #[test]
    fn a_grant_remembers_what_it_refused() {
        let grants = Grants::default();
        let token = grants
            .issue(GrantSpec {
                client: "prepare".into(),
                hosts: vec!["repo.packagist.org".into()],
                ..Default::default()
            })
            .unwrap();
        let grant = grants.holder(&token).unwrap();
        for host in ["a.example", "b.example", "a.example"] {
            assert_eq!(grants.refused(&grant, host), None);
        }
        assert_eq!(grants.refused_by(&token), ["a.example", "b.example"]);
        for n in 0..20 {
            grants.refused(&grant, &format!("h{n}.example"));
        }
        assert_eq!(grants.refused_by(&token).len(), REMEMBERED_REFUSALS);
        grants.revoke(&token);
        assert!(grants.refused_by(&token).is_empty());
    }

    /// none of it is dialled for a grant, by address or by name, and a
    /// public host still is.
    #[test]
    fn a_cluster_refuses_private_ranges_and_cluster_names() {
        let cluster = Refusal::cluster(&["203.0.113.0/24".into()]).unwrap();
        for inside in [
            "10.245.0.1",
            "172.20.1.1",
            "192.168.1.20",
            "100.64.0.10",
            "fd00::1",
            "::ffff:10.0.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "203.0.113.9",
        ] {
            assert!(
                cluster.address(&inside.parse().unwrap()).is_some(),
                "{inside}"
            );
        }
        for outside in ["104.16.0.1", "2606:4700::1111", "172.32.0.1", "100.128.0.1"] {
            assert!(
                cluster.address(&outside.parse().unwrap()).is_none(),
                "{outside}"
            );
        }
        for name in [
            "kubernetes.default.svc",
            "kubernetes.default.svc.cluster.local",
            "kubernetes.default.svc.cluster.local.",
            "metadata.google.internal",
            "buildkitd",
            "localhost",
            "svc",
        ] {
            assert!(cluster.name(name).is_some(), "{name}");
        }
        for name in [
            "registry.npmjs.org",
            "pypi.org",
            "internal.example.com",
            "1.2.3.4",
        ] {
            assert!(cluster.name(name).is_none(), "{name}");
        }
        // A refused address literal is refused before anything, too.
        assert!(cluster.name("10.0.0.5").is_some());
        assert!(cluster.name("fd00::1").is_some());
        // Looked up as absolute, so no search domain completes it.
        assert_eq!(cluster.lookup_name("pypi.org"), "pypi.org.");
        assert_eq!(cluster.lookup_name("1.2.3.4"), "1.2.3.4");
        assert!(Refusal::cluster(&["pods".into()]).is_err());
    }
}
