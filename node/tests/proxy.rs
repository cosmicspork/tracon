//! The node's own CONNECT proxy refuses what the gateway's tinyproxy refuses:
//! unlisted hosts, other ports, and anything that is not CONNECT.

#[path = "support/mod.rs"]
mod support;
use support::state;
use support::{harness::harness, rows::session_row};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracon::gateway::proxy::{run, serve_granted, Allowlist, GrantSpec, Granted, Grants};

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

/// The granted proxy, dialling any address: a test's upstream can only be on
/// loopback, which the real one refuses.
async fn granted(grants: &Grants) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(serve_granted(
        l,
        Granted::dialling_any_address(grants.clone()),
    ));
    addr
}

fn basic(user: &str, token: &str) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
    format!("Proxy-Authorization: Basic {encoded}\r\n")
}

/// Everything the proxy sends back for one request on a fresh connection.
async fn exchange(addr: std::net::SocketAddr, request: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = s.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn connect(host: &str, credentials: &str) -> String {
    format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n{credentials}\r\n")
}

/// Each client is filtered by its own grant. What one was given, another's
/// credentials do not open; the refusal says what to change and is reported
/// as that client's; and no credentials at all open nothing.
#[tokio::test]
async fn a_grant_opens_its_own_hosts_and_nobody_elses() {
    state::isolate();
    let grants = Grants::default();
    let refused = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = refused.clone();
    grants.on_refusal(std::sync::Arc::new(move |grant, host| {
        seen.lock()
            .unwrap()
            .push((grant.session_id.clone(), host.to_string()));
    }));
    let prepare = grants
        .issue(GrantSpec {
            client: "prepare".into(),
            hosts: vec!["127.0.0.1".into()],
            refusal: "not reachable from preparation; add it to this repository's egress".into(),
            ..Default::default()
        })
        .unwrap();
    let session = grants
        .issue(GrantSpec {
            client: "session".into(),
            session_id: Some("s1".into()),
            patterns: vec![r"^api\.anthropic\.com$".into()],
            refusal: "not reachable from a session; add it to this repository's egress".into(),
            ..Default::default()
        })
        .unwrap();
    let addr = granted(&grants).await;

    // Admitted: nothing listens on 443 here, so the answer is the upstream's
    // absence and not a refusal.
    let admitted = exchange(addr, &connect("127.0.0.1", &basic("prepare", &prepare))).await;
    assert!(admitted.starts_with("HTTP/1.1 502"), "{admitted}");

    // The session's own credentials do not open what preparation was given.
    let other = exchange(addr, &connect("127.0.0.1", &basic("session", &session))).await;
    assert!(
        other.starts_with(
            "HTTP/1.1 403 not reachable from a session; add it to this repository's egress"
        ),
        "{other}"
    );
    assert!(
        other.ends_with("add it to this repository's egress\n"),
        "{other}"
    );
    assert_eq!(
        *refused.lock().unwrap(),
        [(Some("s1".to_string()), "127.0.0.1".to_string())]
    );

    // A user name proves nothing, and neither does a revoked or absent token.
    for credentials in [
        String::new(),
        basic("prepare", "not-a-token"),
        basic("prepare", &session[..32]),
    ] {
        let challenged = exchange(addr, &connect("127.0.0.1", &credentials)).await;
        assert!(challenged.starts_with("HTTP/1.1 407"), "{challenged}");
        assert!(
            challenged
                .to_ascii_lowercase()
                .contains("proxy-authenticate: basic"),
            "{challenged}"
        );
    }
    grants.revoke(&prepare);
    let revoked = exchange(addr, &connect("127.0.0.1", &basic("prepare", &prepare))).await;
    assert!(revoked.starts_with("HTTP/1.1 407"), "{revoked}");
    // Only CONNECT to 443, whoever asks.
    let port = exchange(
        addr,
        &format!(
            "CONNECT api.anthropic.com:80 HTTP/1.1\r\nHost: api.anthropic.com:80\r\n{}\r\n",
            basic("session", &session)
        ),
    )
    .await;
    assert!(port.starts_with("HTTP/1.1 403"), "{port}");
}

/// Git and browsers present credentials only once challenged, and on the
/// connection the challenge came back on. A challenge that closed it would
/// lock both out.
#[tokio::test]
async fn a_challenge_is_answered_on_the_same_connection() {
    state::isolate();
    let grants = Grants::default();
    let token = grants
        .issue(GrantSpec {
            client: "prepare".into(),
            hosts: vec!["127.0.0.1".into()],
            ..Default::default()
        })
        .unwrap();
    let addr = granted(&grants).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut buf = vec![0u8; 4096];
    s.write_all(connect("127.0.0.1", "").as_bytes())
        .await
        .unwrap();
    let n = s.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 407"));
    s.write_all(connect("127.0.0.1", &basic("prepare", &token)).as_bytes())
        .await
        .unwrap();
    let n = s.read(&mut buf).await.unwrap();
    let second = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(second.starts_with("HTTP/1.1 502"), "{second}");
}

/// A name that resolves to this machine is not dialled, whatever the grant
/// says: the proxy runs in the node's process, where loopback is the node.
#[tokio::test]
async fn a_granted_name_that_is_this_machine_is_refused() {
    state::isolate();
    let grants = Grants::default();
    let token = grants
        .issue(GrantSpec {
            client: "qa".into(),
            hosts: vec!["127.0.0.1".into(), "localhost".into()],
            plain_http: true,
            ..Default::default()
        })
        .unwrap();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(serve_granted(l, Granted::new(grants.clone())));
    for host in ["127.0.0.1", "localhost"] {
        let line = exchange(addr, &connect(host, &basic("qa", &token))).await;
        assert!(
            line.starts_with("HTTP/1.1 403 resolves to an address on this machine"),
            "{host}: {line}"
        );
    }
}

/// A QA target's own `http://` origin is forwarded as the origin would be
/// asked directly: the path alone, and none of what was said to the proxy.
/// A grant that does not ask for plain HTTP gets none.
#[tokio::test]
async fn a_plain_http_origin_is_forwarded_only_for_a_grant_that_names_it() {
    state::isolate();
    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = origin.local_addr().unwrap().port();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let heard = asked.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = origin.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).await.unwrap();
            *heard.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            let _ = s
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                )
                .await;
        }
    });
    let grants = Grants::default();
    let qa = grants
        .issue(GrantSpec {
            client: "qa".into(),
            hosts: vec!["127.0.0.1".into()],
            plain_http: true,
            ..Default::default()
        })
        .unwrap();
    let prepare = grants
        .issue(GrantSpec {
            client: "prepare".into(),
            hosts: vec!["127.0.0.1".into()],
            ..Default::default()
        })
        .unwrap();
    let addr = granted(&grants).await;
    let get = |credentials: String| {
        format!(
            "GET http://127.0.0.1:{port}/app?x=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{credentials}Connection: close\r\n\r\n"
        )
    };

    let refused = exchange(addr, &get(basic("prepare", &prepare))).await;
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    assert!(asked.lock().unwrap().is_empty());

    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(get(basic("qa", &qa)).as_bytes()).await.unwrap();
    let mut answer = String::new();
    s.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with("hello"), "{answer}");
    let asked = asked.lock().unwrap().clone();
    assert!(asked.starts_with("GET /app?x=1 HTTP/1.1"), "{asked}");
    assert!(
        !asked.to_ascii_lowercase().contains("proxy-authorization"),
        "{asked}"
    );
}

/// A package manager retries, and the operator needs to hear a refusal once.
/// The proxy knows which grant asked, so what a session was refused lands on
/// that session — and what a preparation was refused lands on none.
#[tokio::test]
async fn a_sessions_refusals_are_recorded_on_it_once_per_host() {
    let h = harness().await;
    h.store
        .insert_session(&session_row("s1", "n1", "personal"))
        .unwrap();
    let grants = Grants::default();
    grants.on_refusal(h.manager.egress_refusal_observer());
    let session = grants
        .issue(GrantSpec {
            client: "session".into(),
            session_id: Some("s1".into()),
            refusal: "not reachable from a session; add it to this repository's egress".into(),
            ..Default::default()
        })
        .unwrap();
    let prepare = grants
        .issue(GrantSpec {
            client: "prepare".into(),
            ..Default::default()
        })
        .unwrap();
    let addr = granted(&grants).await;
    for (host, user, token) in [
        ("registry.npmjs.org", "session", &session),
        ("registry.npmjs.org", "session", &session),
        ("crates.io", "session", &session),
        ("pypi.org", "prepare", &prepare),
    ] {
        let line = exchange(addr, &connect(host, &basic(user, token))).await;
        assert!(line.starts_with("HTTP/1.1 403"), "{line}");
    }
    let refused: Vec<String> = h
        .store
        .events_after("s1", 0, 100)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "egress_refused")
        .map(|event| event.payload["host"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(refused, ["registry.npmjs.org", "crates.io"]);
}
