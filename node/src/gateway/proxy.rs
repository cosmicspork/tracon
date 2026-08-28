//! An HTTP CONNECT proxy with a default-deny host allowlist. The same contract
//! as the gateway container's tinyproxy (`containers/gateway/tinyproxy.conf`):
//! only CONNECT, only port 443, only hosts matching an anchored entry. Nothing
//! else is forwarded — a plain `GET http://…` gets a refusal, not a fetch.

use std::net::SocketAddr;

use http_body_util::{Empty, Full};
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
}
