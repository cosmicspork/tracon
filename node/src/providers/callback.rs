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
        let ipv4 = bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)), port).await?;
        let ipv6 = match bind_ipv6(port) {
            Ok(listener) => Some(listener),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                drop(ipv4);
                return Err(CallbackError::AddrInUse(port));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
                ) =>
            {
                None
            }
            Err(_) => {
                drop(ipv4);
                return Err(CallbackError::Listener);
            }
        };

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

async fn bind(address: SocketAddr, port: u16) -> Result<TcpListener, CallbackError> {
    TcpListener::bind(address).await.map_err(|error| {
        if error.kind() == io::ErrorKind::AddrInUse {
            CallbackError::AddrInUse(port)
        } else {
            CallbackError::Listener
        }
    })
}

fn bind_ipv6(port: u16) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((Ipv6Addr::LOCALHOST, port)).into())?;
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
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
        let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let (capture, mut events) = CallbackCapture::start(target_at(port)).await.unwrap();

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
        let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let (capture, mut events) = CallbackCapture::start(target_at(port)).await.unwrap();
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

    #[tokio::test]
    async fn an_ipv6_collision_releases_the_ipv4_listener() {
        let Ok(occupied) = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) else {
            return;
        };
        let port = occupied.local_addr().unwrap().port();
        assert!(matches!(
            CallbackCapture::start(target_at(port)).await,
            Err(CallbackError::AddrInUse(value)) if value == port
        ));
        drop(occupied);
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .expect("partial IPv4 listener was released");
    }
}
