//! The node's own CONNECT proxy refuses what the gateway's tinyproxy refuses:
//! unlisted hosts, other ports, and anything that is not CONNECT.

#[path = "support/mod.rs"]
mod support;
use support::state;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracon::gateway::proxy::{run, Allowlist};

async fn proxy(allow: &[String]) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let allow = Allowlist::new(allow).unwrap();
    tokio::spawn(run(l, allow));
    addr
}

async fn status_of(addr: std::net::SocketAddr, request: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 256];
    let n = s.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..n])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn unlisted_hosts_other_ports_and_plain_requests_are_refused() {
    state::isolate();
    let addr = proxy(&[r"^api\.anthropic\.com$".into()]).await;
    let line = status_of(
        addr,
        "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n",
    )
    .await;
    assert!(line.contains("403"), "{line}");
    let line = status_of(
        addr,
        "CONNECT api.anthropic.com:80 HTTP/1.1\r\nHost: api.anthropic.com:80\r\n\r\n",
    )
    .await;
    assert!(line.contains("403"), "{line}");
    let line = status_of(
        addr,
        "GET http://api.anthropic.com/ HTTP/1.1\r\nHost: api.anthropic.com\r\n\r\n",
    )
    .await;
    assert!(line.contains("403"), "{line}");
}

/// A proxy that refuses everything would pass the test above. The listed
/// host on 443 has to get past the allowlist: the proxy only ever dials 443,
/// which a test cannot bind, so what is asserted is that the answer is the
/// upstream's absence (502) and not a refusal (403).
#[tokio::test]
async fn a_listed_host_on_443_is_admitted() {
    state::isolate();
    let addr = proxy(&[r"^127\.0\.0\.1$".into()]).await;
    let line = status_of(
        addr,
        "CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\n\r\n",
    )
    .await;
    assert!(line.contains("502"), "{line}");
}
