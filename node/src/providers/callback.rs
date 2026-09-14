use std::{
    convert::Infallible,
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

use http_body_util::Full;
use hyper::{
    body::Bytes, body::Incoming, service::service_fn, Method, Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use reqwest::Url;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

const INVALID_CALLBACK: &str = "Invalid OAuth callback.";
const WRONG_STATE: &str = "OAuth state did not match.";
const CALLBACK_GONE: &str = "This sign-in is no longer waiting for a callback.";

#[derive(Clone, PartialEq, Eq)]
pub struct CallbackTarget {
    redirect: Url,
    state: String,
    port: u16,
}

impl CallbackTarget {
    pub fn parse(authorization_url: &str) -> Result<Self, CallbackError> {
        let authorization = Url::parse(authorization_url)
            .map_err(|_| CallbackError::InvalidTarget("invalid authorization URL"))?;
        let redirect_values = values(&authorization, "redirect_uri");
        let state_values = values(&authorization, "state");
        if redirect_values.len() != 1 || redirect_values[0].is_empty() {
            return Err(CallbackError::InvalidTarget(
                "authorization URL must contain one redirect_uri",
            ));
        }
        if state_values.len() != 1 || state_values[0].is_empty() {
            return Err(CallbackError::InvalidTarget(
                "authorization URL must contain one non-empty state",
            ));
        }

        let redirect_text = &redirect_values[0];
        let redirect = Url::parse(redirect_text)
            .map_err(|_| CallbackError::InvalidTarget("invalid redirect_uri"))?;
        if redirect.scheme() != "http" {
            return Err(CallbackError::InvalidTarget("redirect_uri must use http"));
        }
        if !redirect.username().is_empty() || redirect.password().is_some() {
            return Err(CallbackError::InvalidTarget(
                "redirect_uri must not contain userinfo",
            ));
        }
        if redirect.query().is_some() || redirect.fragment().is_some() {
            return Err(CallbackError::InvalidTarget(
                "redirect_uri must not contain a query or fragment",
            ));
        }
        let host = redirect
            .host_str()
            .ok_or(CallbackError::InvalidTarget("redirect_uri has no host"))?;
        if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
            return Err(CallbackError::InvalidTarget(
                "redirect_uri host is not loopback",
            ));
        }
        if redirect.path().is_empty() {
            return Err(CallbackError::InvalidTarget("redirect_uri has no path"));
        }
        let port = explicit_port(redirect_text).ok_or(CallbackError::InvalidTarget(
            "redirect_uri has no explicit port",
        ))?;

        Ok(Self {
            redirect,
            state: state_values[0].clone(),
            port,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn validate<B>(&self, request: &Request<B>) -> Result<CallbackOutcome, Rejection> {
        if request.method() != Method::GET || request.uri().path() != self.redirect.path() {
            return Err(Rejection::Invalid);
        }
        let Some(query) = request.uri().query() else {
            return Err(Rejection::Invalid);
        };
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        let states = pair_values(&pairs, "state");
        if states.len() != 1 || *states[0] != self.state {
            return Err(Rejection::WrongState);
        }
        let codes = pair_values(&pairs, "code");
        let errors = pair_values(&pairs, "error");
        match (codes.as_slice(), errors.as_slice()) {
            ([code], []) if !code.is_empty() => {
                let mut forwarded = self.redirect.clone();
                forwarded
                    .query_pairs_mut()
                    .append_pair("code", code)
                    .append_pair("state", &self.state);
                Ok(CallbackOutcome::Code(forwarded.to_string()))
            }
            ([], [error]) if !error.is_empty() => Ok(CallbackOutcome::Denied),
            _ => Err(Rejection::Invalid),
        }
    }
}

fn values(url: &Url, key: &str) -> Vec<String> {
    url.query_pairs()
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .collect()
}

fn pair_values<'a>(pairs: &'a [(String, String)], key: &str) -> Vec<&'a String> {
    pairs
        .iter()
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value)
        .collect()
}

fn explicit_port(value: &str) -> Option<u16> {
    let authority = value.strip_prefix("http://")?.split('/').next()?;
    let port = if authority.starts_with('[') {
        authority.rsplit_once("]:")?.1
    } else {
        authority.rsplit_once(':')?.1
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    port.parse().ok()
}

#[derive(Debug, thiserror::Error)]
pub enum CallbackError {
    #[error("{0}")]
    InvalidTarget(&'static str),
    #[error("local callback port {0} is already in use")]
    AddrInUse(u16),
    #[error("local callback listener could not start")]
    Listener,
}

pub enum CallbackOutcome {
    Code(String),
    Denied,
}

pub struct CaptureRequest {
    pub outcome: CallbackOutcome,
    pub reply: oneshot::Sender<CaptureReply>,
}

#[derive(Debug)]
pub struct CaptureReply {
    pub status: StatusCode,
    pub text: &'static str,
}

impl CaptureReply {
    pub const fn success() -> Self {
        Self {
            status: StatusCode::OK,
            text: "Sign-in complete. You can close this window.",
        }
    }

    pub const fn denied() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            text: "Sign-in was not authorized. You can close this window.",
        }
    }

    pub const fn failed() -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            text: "Sign-in could not be completed. Return to tracon and try again.",
        }
    }
}

pub enum CaptureEvent {
    Request(CaptureRequest),
    ListenerFailed,
}

#[derive(Clone)]
pub struct CallbackCapture {
    cancel: CancellationToken,
}

impl CallbackCapture {
    pub async fn start(
        target: CallbackTarget,
    ) -> Result<(Self, mpsc::UnboundedReceiver<CaptureEvent>), CallbackError> {
        let port = target.port();
        let ipv4 = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .map_err(|error| listener_error(&error, port))?;
        let (ipv4, ipv6) = bind_ipv6_beside(ipv4, port)?;
        Self::serve(target, ipv4, ipv6)
    }

    /// Serve a callback on loopback listeners that are already bound.
    ///
    /// Binding is a separate step from serving so a caller that does not have
    /// a port in mind binds port 0 once and hands the listener over, instead
    /// of asking the kernel for a free port, closing it, and naming it again a
    /// moment later — a gap in which anything else on the host can take it.
    fn serve(
        target: CallbackTarget,
        ipv4: std::net::TcpListener,
        ipv6: Option<std::net::TcpListener>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<CaptureEvent>), CallbackError> {
        let ipv4 = into_tokio(ipv4)?;
        let ipv6 = ipv6.map(into_tokio).transpose()?;

        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded_channel();
        spawn_listener(ipv4, Arc::new(target.clone()), cancel.clone(), tx.clone());
        if let Some(listener) = ipv6 {
            spawn_listener(listener, Arc::new(target), cancel.clone(), tx);
        }
        Ok((Self { cancel }, rx))
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

/// Bind the IPv6 half of the callback beside an IPv4 half already bound to
/// `port`, and release the IPv4 half if the IPv6 half cannot be had.
///
/// The IPv4 listener is taken by value: on every failing path this function
/// holds the only handle to it, so the port cannot stay bound behind a login
/// that never started.
fn bind_ipv6_beside(
    ipv4: std::net::TcpListener,
    port: u16,
) -> Result<(std::net::TcpListener, Option<std::net::TcpListener>), CallbackError> {
    match bind_ipv6(port) {
        Ok(listener) => Ok((ipv4, Some(listener))),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            drop(ipv4);
            Err(CallbackError::AddrInUse(port))
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
            ) =>
        {
            Ok((ipv4, None))
        }
        Err(_) => {
            drop(ipv4);
            Err(CallbackError::Listener)
        }
    }
}

fn listener_error(error: &io::Error, port: u16) -> CallbackError {
    if error.kind() == io::ErrorKind::AddrInUse {
        CallbackError::AddrInUse(port)
    } else {
        CallbackError::Listener
    }
}

fn bind_ipv6(port: u16) -> io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(true)?;
    socket.bind(&SocketAddr::from((Ipv6Addr::LOCALHOST, port)).into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

fn into_tokio(listener: std::net::TcpListener) -> Result<TcpListener, CallbackError> {
    listener
        .set_nonblocking(true)
        .map_err(|_| CallbackError::Listener)?;
    TcpListener::from_std(listener).map_err(|_| CallbackError::Listener)
}

fn spawn_listener(
    listener: TcpListener,
    target: Arc<CallbackTarget>,
    cancel: CancellationToken,
    tx: mpsc::UnboundedSender<CaptureEvent>,
) {
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = cancel.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            let (stream, _) = match accepted {
                Ok(pair) => pair,
                Err(_) => {
                    if !cancel.is_cancelled() {
                        let _ = tx.send(CaptureEvent::ListenerFailed);
                    }
                    break;
                }
            };
            let target = target.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let service =
                    service_fn(move |request| handle_request(request, target.clone(), tx.clone()));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}

async fn handle_request(
    request: Request<Incoming>,
    target: Arc<CallbackTarget>,
    tx: mpsc::UnboundedSender<CaptureEvent>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let reply = match target.validate(&request) {
        Ok(outcome) => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(CaptureEvent::Request(CaptureRequest {
                    outcome,
                    reply: reply_tx,
                }))
                .is_err()
            {
                fixed(StatusCode::GONE, CALLBACK_GONE)
            } else {
                match reply_rx.await {
                    Ok(reply) => fixed(reply.status, reply.text),
                    Err(_) => fixed(StatusCode::GONE, CALLBACK_GONE),
                }
            }
        }
        Err(Rejection::WrongState) => fixed(StatusCode::FORBIDDEN, WRONG_STATE),
        Err(Rejection::Invalid) => fixed(StatusCode::BAD_REQUEST, INVALID_CALLBACK),
    };
    Ok(reply)
}

fn fixed(status: StatusCode, text: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(text.as_bytes())))
        .expect("fixed callback response")
}

#[derive(Debug)]
enum Rejection {
    Invalid,
    WrongState,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> CallbackTarget {
        CallbackTarget::parse(
            "https://provider.example/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A18443%2Foauth%2Fcallback&state=secret",
        )
        .unwrap()
    }

    fn target_at(port: u16) -> CallbackTarget {
        CallbackTarget::parse(&format!(
            "https://provider.example/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A{port}%2Foauth%2Fcallback&state=secret"
        ))
        .unwrap()
    }

    /// Bind an IPv4 loopback listener and name it in a target, in that order.
    ///
    /// Every unit test in this crate runs in one process, so a port learned
    /// from a listener that was then closed is not the test's to keep: the
    /// kernel hands the same ephemeral port to the next socket that asks, and
    /// on a busy runner that is another test, not this one. The listener is
    /// returned still bound so the caller passes it to
    /// [`CallbackCapture::serve`] rather than asking for the port again.
    fn bound_loopback_target() -> (std::net::TcpListener, CallbackTarget, u16) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, target_at(port), port)
    }

    #[test]
    fn parses_only_strict_loopback_targets() {
        assert_eq!(target().port(), 18443);
        for value in [
            "https://provider.example/authorize?redirect_uri=https%3A%2F%2Flocalhost%3A1%2Fcb&state=s",
            "https://provider.example/authorize?redirect_uri=http%3A%2F%2Fexample.com%3A1%2Fcb&state=s",
            "https://provider.example/authorize?redirect_uri=http%3A%2F%2Flocalhost%2Fcb&state=s",
            "https://provider.example/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcb%3Fx%3D1&state=s",
            "https://provider.example/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcb&state=",
            "https://provider.example/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcb&state=a&state=b",
        ] {
            assert!(CallbackTarget::parse(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn reconstructs_forwarded_url_from_trusted_target() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("http://attacker.example/oauth/callback?code=a%2Fb%3Fc&state=secret&extra=ignored")
            .body(())
            .unwrap();
        let CallbackOutcome::Code(url) = target().validate(&request).unwrap() else {
            panic!("expected code");
        };
        assert_eq!(
            url,
            "http://127.0.0.1:18443/oauth/callback?code=a%2Fb%3Fc&state=secret"
        );
    }

    #[test]
    fn rejects_wrong_method_path_state_and_ambiguous_results() {
        for uri in [
            "/wrong?code=x&state=secret",
            "/oauth/callback?code=x&state=wrong",
            "/oauth/callback?code=x&code=y&state=secret",
            "/oauth/callback?code=x&error=denied&state=secret",
            "/oauth/callback?state=secret",
        ] {
            let request = Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(())
                .unwrap();
            assert!(target().validate(&request).is_err(), "accepted {uri}");
        }
        let request = Request::builder()
            .method(Method::POST)
            .uri("/oauth/callback?code=x&state=secret")
            .body(())
            .unwrap();
        assert!(target().validate(&request).is_err());

        let denied = Request::builder()
            .method(Method::GET)
            .uri("/oauth/callback?error=access_denied&state=secret")
            .body(())
            .unwrap();
        assert!(matches!(
            target().validate(&denied),
            Ok(CallbackOutcome::Denied)
        ));
    }

    #[tokio::test]
    async fn reports_an_occupied_callback_port() {
        let occupied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = occupied.local_addr().unwrap().port();
        let target = CallbackTarget::parse(&format!(
            "https://provider.example/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A{port}%2Fcallback&state=secret"
        ))
        .unwrap();
        assert!(matches!(
            CallbackCapture::start(target).await,
            Err(CallbackError::AddrInUse(value)) if value == port
        ));
    }

    #[tokio::test]
    async fn wrong_state_does_not_consume_the_listener_and_success_is_fixed() {
        let (listener, target, port) = bound_loopback_target();
        let (capture, mut events) = CallbackCapture::serve(target, listener, None).unwrap();

        let rejected = reqwest::get(format!(
            "http://127.0.0.1:{port}/oauth/callback?code=nope&state=wrong"
        ))
        .await
        .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        assert_eq!(rejected.text().await.unwrap(), WRONG_STATE);
        assert!(events.try_recv().is_err());

        let request = tokio::spawn(async move {
            reqwest::get(format!(
                "http://127.0.0.1:{port}/oauth/callback?code=a%2Fb&state=secret"
            ))
            .await
            .unwrap()
        });
        let CaptureEvent::Request(pending) =
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("listener failed");
        };
        let CallbackOutcome::Code(forwarded) = pending.outcome else {
            panic!("expected an authorization code");
        };
        assert_eq!(
            forwarded,
            format!("http://127.0.0.1:{port}/oauth/callback?code=a%2Fb&state=secret")
        );
        pending.reply.send(CaptureReply::success()).unwrap();
        let accepted = request.await.unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        assert_eq!(
            accepted.text().await.unwrap(),
            "Sign-in complete. You can close this window."
        );
        capture.stop();
    }

    #[tokio::test]
    async fn denial_response_is_redacted() {
        let (listener, target, port) = bound_loopback_target();
        let (capture, mut events) = CallbackCapture::serve(target, listener, None).unwrap();
        let request = tokio::spawn(async move {
            reqwest::get(format!(
                "http://127.0.0.1:{port}/oauth/callback?error=provider_secret_reason&state=secret"
            ))
            .await
            .unwrap()
        });
        let CaptureEvent::Request(pending) =
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("listener failed");
        };
        assert!(matches!(pending.outcome, CallbackOutcome::Denied));
        pending.reply.send(CaptureReply::denied()).unwrap();
        let denied = request.await.unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let text = denied.text().await.unwrap();
        assert_eq!(
            text,
            "Sign-in was not authorized. You can close this window."
        );
        assert!(!text.contains("provider_secret_reason"));
        capture.stop();
    }

    /// The fixed loopback range the collision test reserves a port in, and the
    /// lock that makes it one test's at a time.
    ///
    /// The collision test is the one test here that cannot work from an
    /// already-bound port alone: proving the IPv4 half was *released* means
    /// binding it again afterwards, and between the release and that rebind
    /// the port is genuinely free. Two things close that window. The range is
    /// below both platforms' ephemeral ranges (macOS 49152-65535, Linux
    /// 32768-60999), so no `bind(port 0)` anywhere on the host is ever handed
    /// one of these ports; and the lock keeps any other test that reserves
    /// from this range out while one holds it. Every unit test in this crate
    /// runs in one process, so a fixed range is shared state — but it is one
    /// narrow resource, and the lock is over it, not over the test binary.
    ///
    /// Any future test that wants a known-free fixed port belongs on this
    /// lock and this range.
    static FIXED_LOOPBACK_RANGE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    const FIXED_LOOPBACK_PORTS: std::ops::Range<u16> = 19_500..19_600;

    /// Reserve a port in [`FIXED_LOOPBACK_PORTS`] that is free on both
    /// loopback families, holding *both* halves: a v6-only occupier on
    /// `[::1]:port`, and an IPv4 listener on `127.0.0.1:port` for the caller
    /// to hand to the code under test. `None` when this machine has no IPv6
    /// loopback to collide on.
    ///
    /// Returning the IPv4 listener bound rather than probing it and closing
    /// it is the point: the port is this test's without interruption from the
    /// moment it is chosen until the code under test is given it, so the only
    /// thing that can free it afterwards is that code.
    ///
    /// The occupier is v6-only on purpose: the collision under test is an
    /// IPv6-loopback collision, and leaving `IPV6_V6ONLY` at the platform
    /// default would make the precondition depend on `net.inet6.ip6.v6only`
    /// (macOS) or `net.ipv6.bindv6only` (Linux).
    fn reserved_collision_port() -> Option<(std::net::TcpListener, std::net::TcpListener, u16)> {
        for port in FIXED_LOOPBACK_PORTS {
            let Ok(socket) = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)) else {
                return None;
            };
            if socket.set_only_v6(true).is_err()
                || socket
                    .bind(&SocketAddr::from((Ipv6Addr::LOCALHOST, port)).into())
                    .is_err()
                || socket.listen(8).is_err()
            {
                continue;
            }
            // The capture binds IPv4 before IPv6, so the IPv4 half has to be
            // free as well or the run would stop before the collision.
            let Ok(ipv4) = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)) else {
                continue;
            };
            return Some((socket.into(), ipv4, port));
        }
        None
    }

    /// The local address `descriptor` is bound to, when it is still an
    /// internet socket this process holds.
    ///
    /// `getsockname` consults the descriptor table before anything a
    /// descriptor names, so a descriptor that was closed answers `EBADF`, and
    /// one this process has since reused for something else answers
    /// `ENOTSOCK` or some other address. All of those mean released, and none
    /// of them is a question about protocol state.
    #[cfg(unix)]
    fn bound_address(descriptor: std::os::fd::RawFd) -> io::Result<SocketAddr> {
        // SAFETY: the closure writes only into the storage socket2 provides,
        // and `getsockname` validates the descriptor before it touches
        // anything — which is the point here, since the descriptor under test
        // is expected to be closed.
        let (_, address) = unsafe {
            socket2::SockAddr::try_init(|storage, length| {
                match libc::getsockname(descriptor, storage.cast(), length) {
                    -1 => Err(io::Error::last_os_error()),
                    _ => Ok(()),
                }
            })
        }?;
        address
            .as_socket()
            .ok_or_else(|| io::Error::other("not an internet socket"))
    }

    /// Bind `127.0.0.1:port` the way production would were it to ask for the
    /// port again: a fresh socket with `SO_REUSEADDR` set deliberately rather
    /// than left to `std`, which sets it on every Unix listener anyway.
    ///
    /// This is a diagnostic, never an assertion — what the kernel makes of
    /// the port is exactly the timing-dependent answer this test stopped
    /// relying on.
    #[cfg(unix)]
    fn rebind_with_reuse_address(port: u16) -> io::Result<std::net::TcpListener> {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_address(true)?;
        socket.bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)).into())?;
        socket.listen(1)?;
        Ok(socket.into())
    }

    /// Everything a failure of the release assertion needs to be diagnosable
    /// from a CI log alone, on a platform it cannot be reproduced on: what
    /// the descriptor answers now, what the kernel makes of the port itself,
    /// the options in effect on both halves, and where it ran.
    #[cfg(unix)]
    fn release_diagnostics(
        descriptor: std::os::fd::RawFd,
        port: u16,
        reuse_address: io::Result<bool>,
        occupier: &std::net::TcpListener,
    ) -> String {
        let occupier = socket2::SockRef::from(occupier);
        let rebind = match rebind_with_reuse_address(port) {
            Ok(_) => "accepted".to_owned(),
            Err(error) => format!("refused with {error:?}"),
        };
        format!(
            "the IPv6 collision on {port} left the IPv4 half held: descriptor {descriptor} answers \
             {bound:?}, and its SO_REUSEADDR was {reuse_address:?}; a fresh SO_REUSEADDR bind of \
             127.0.0.1:{port} was {rebind}; the IPv6 occupier holds {occupied:?} with IPV6_V6ONLY \
             {only_v6:?}; platform {os}/{arch}",
            bound = bound_address(descriptor),
            occupied = occupier.local_addr().map(|address| address.as_socket()),
            only_v6 = occupier.only_v6(),
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
        )
    }

    /// A collision on the IPv6 half must not leave the IPv4 half held. Held
    /// by *this process* is the guarantee, so the descriptor handed over is
    /// what the test asks about — not whether the port can be bound again.
    ///
    /// Binding it again is what kept failing on the macOS runner, and not for
    /// any fault in the code under test. `close(2)` invalidates a descriptor
    /// at once on every Unix, but freeing the protocol control block that
    /// holds `127.0.0.1:port` is the kernel's own business afterwards: Linux
    /// unhashes a listening socket inside `close`, so the port is free the
    /// instant it returns, while macOS detaches the pcb asynchronously and
    /// the port can still read as taken a moment after this process has let
    /// go of it. `SO_REUSEADDR` does not cover that case: `std` sets it on
    /// every Unix listener, so both binds here already had it, and on BSD it
    /// relaxes `TIME_WAIT` and wildcard-against-specific conflicts, never a
    /// pcb still in the table. Nothing else in the run explains it either —
    /// the occupier is v6-only on `::1`, and the reservation proved
    /// `127.0.0.1:port` bindable beside it a moment earlier.
    ///
    /// Unix only: descriptors are, and the node ships for Linux and macOS.
    #[cfg(unix)]
    #[test]
    fn an_ipv6_collision_releases_the_ipv4_listener() {
        use std::os::fd::AsRawFd;

        let _range = FIXED_LOOPBACK_RANGE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((occupied, ipv4, port)) = reserved_collision_port() else {
            return;
        };
        let descriptor = ipv4.as_raw_fd();
        let reuse_address = socket2::SockRef::from(&ipv4).reuse_address();

        // `bind_ipv6_beside` is the whole of the release logic; `start` is it
        // plus the IPv4 bind this test has already done.
        match bind_ipv6_beside(ipv4, port) {
            Err(CallbackError::AddrInUse(value)) if value == port => {}
            Err(other) => panic!("expected the IPv6 collision on {port}, got {other}"),
            Ok(_) => panic!("the occupied IPv6 loopback on {port} did not collide"),
        }

        let held = bound_address(descriptor)
            .is_ok_and(|address| address == SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        assert!(
            !held,
            "{}",
            release_diagnostics(descriptor, port, reuse_address, &occupied)
        );
        drop(occupied);
    }
}
