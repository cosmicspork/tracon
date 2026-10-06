//! Asking before egress, rather than refusing it: a session's refused CONNECT
//! raises the operator's ask and says so in its reason, `request_egress`
//! waits on the same ask, and the operator's answer opens the host to that
//! session's own grant — once, for the session, or saved to the repository.

#[path = "support/mod.rs"]
mod support;
use support::{
    harness::{harness, harness_with, Harness},
    rows::session_row,
};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;
use tracon::gateway::proxy::{serve_granted, GrantSpec, Granted, Grants};

/// Never resolves (RFC 2606), so an admitted CONNECT answers 502 rather than
/// reaching anything.
const HOST: &str = "pypi.example.test";

struct Proxy {
    addr: std::net::SocketAddr,
    token: String,
    grants: Grants,
}

/// A session `s1` on `personal`, holding its own grant, whose refusals the
/// node watches.
async fn session_proxy(h: &Harness) -> Proxy {
    h.store
        .insert_session(&session_row("s1", "n1", "personal"))
        .unwrap();
    let grants = Grants::default();
    h.manager.watch_egress(&grants);
    let token = grants
        .issue(GrantSpec {
            client: "session".into(),
            session_id: Some("s1".into()),
            refusal: "not reachable from a session; add it to this repository's egress".into(),
            ..Default::default()
        })
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_granted(
        listener,
        Granted::dialling_any_address(grants.clone()),
    ));
    Proxy {
        addr,
        token,
        grants,
    }
}

/// The status line a CONNECT to `host` gets.
async fn connect(proxy: &Proxy, host: &str) -> String {
    use base64::Engine;
    let credentials =
        base64::engine::general_purpose::STANDARD.encode(format!("session:{}", proxy.token));
    let mut s = TcpStream::connect(proxy.addr).await.unwrap();
    s.write_all(
        format!(
            "CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\
             Proxy-Authorization: Basic {credentials}\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = vec![0u8; 4096];
    let n = s.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..n])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The approval id a refusal's reason names.
fn named_approval(line: &str) -> String {
    let at = line
        .find("(approval ")
        .unwrap_or_else(|| panic!("no approval named in {line:?}"));
    line[at + "(approval ".len()..]
        .split(')')
        .next()
        .unwrap()
        .to_string()
}

async fn answer(h: &Harness, id: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/permissions/{id}/answer"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = h.operator.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn request(h: &Harness, args: Value) -> Result<Value, String> {
    let ctx = tracon::mcp::CallContext::session("s1", "personal", "n1");
    h.tools.call(&ctx, "request_egress", &args).await
}

/// A refused CONNECT asks once, says so in the line a package manager
/// prints, and the operator's answer opens the host to this session.
#[tokio::test]
async fn a_refused_host_is_asked_once_and_the_answer_opens_it() {
    let h = harness().await;
    let proxy = session_proxy(&h).await;

    let line = connect(&proxy, HOST).await;
    assert!(line.starts_with("HTTP/1.1 403"), "{line}");
    assert!(line.contains("the operator has been asked"), "{line}");
    let id = named_approval(&line);
    // A package manager retries; the operator hears it once.
    let again = connect(&proxy, HOST).await;
    assert_eq!(named_approval(&again), id, "{again}");

    let cards = h.store.open_permission_views().unwrap();
    assert_eq!(cards.len(), 1);
    let card = &cards[0].request;
    assert_eq!(card.id, id);
    assert!(card.title.contains(HOST), "{}", card.title);
    for option in ["allow_once", "allow_session", "allow_repo", "reject_once"] {
        assert!(card.options.contains(option), "{}", card.options);
    }

    let (status, body) = answer(&h, &id, json!({ "option_id": "allow_session" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Admitted: nothing answers for the host, which is the upstream's
    // absence and not a refusal.
    let line = connect(&proxy, HOST).await;
    assert!(line.starts_with("HTTP/1.1 502"), "{line}");
    // Opened to this session, and nobody else's grant.
    assert!(proxy.grants.session_allows("s1", HOST));
    assert!(!proxy.grants.session_allows("s2", HOST));

    let settled = h.store.get_approval(&id).unwrap().unwrap();
    assert_eq!(settled.state, "succeeded", "{:?}", settled.reason);
    let result: Value = serde_json::from_str(settled.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["scope"], "session");

    let refused: Vec<Value> = h
        .store
        .events_after("s1", 0, 100)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "egress_refused")
        .map(|event| event.payload)
        .collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["host"], HOST);
    assert_eq!(refused[0]["approval_id"], id.as_str());
}

/// What the agent knows it needs it asks for first, and waits; "Allow once"
/// opens the host for the retry and no longer.
#[tokio::test]
async fn request_egress_waits_on_the_same_ask() {
    let h = harness().await;
    let proxy = session_proxy(&h).await;

    let asked = request(
        &h,
        json!({ "host": format!("https://{HOST}/simple"), "why": "pip install requests", "wait_secs": 0 }),
    )
    .await
    .unwrap();
    assert_eq!(asked["state"], "still_waiting", "{asked}");
    assert_eq!(asked["host"], HOST);
    let id = asked["approval_id"].as_str().unwrap().to_string();
    // A refused connection meanwhile finds the ask already waiting.
    assert_eq!(named_approval(&connect(&proxy, HOST).await), id);
    let card = h.store.get_approval(&id).unwrap().unwrap();
    assert!(
        card.title.contains("pip install requests"),
        "{}",
        card.title
    );

    let (status, body) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let open = request(&h, json!({ "host": HOST, "wait_secs": 0 }))
        .await
        .unwrap();
    assert_eq!(open["state"], "reachable", "{open}");
    let result: Value = serde_json::from_str(
        h.store
            .get_approval(&id)
            .unwrap()
            .unwrap()
            .result
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["scope"], "once");
    assert!(result["for_secs"].as_u64().unwrap() <= 600, "{result}");

    // Not a host name, or not port 443: nothing to put to anyone.
    for bad in ["localhost", "pypi.org:80"] {
        let refused = request(&h, json!({ "host": bad, "wait_secs": 0 })).await;
        assert!(refused.is_err(), "{bad}: {refused:?}");
    }
}

/// A refusal is the operator's answer: retried connections do not ask
/// again, and say so. An explicit request may ask anew.
#[tokio::test]
async fn a_declined_host_is_not_asked_again_by_retries() {
    let h = harness().await;
    let proxy = session_proxy(&h).await;
    let id = named_approval(&connect(&proxy, HOST).await);
    let (status, body) = answer(
        &h,
        &id,
        json!({ "option_id": "reject_once", "reason": "use the vendored copy" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let line = connect(&proxy, HOST).await;
    assert!(line.starts_with("HTTP/1.1 403"), "{line}");
    assert!(line.contains("declined"), "{line}");
    assert!(h.store.open_permission_views().unwrap().is_empty());

    let asked = request(
        &h,
        json!({ "host": HOST, "why": "the vendored copy is missing a fix", "wait_secs": 0 }),
    )
    .await
    .unwrap();
    assert_eq!(asked["state"], "still_waiting", "{asked}");
    assert_ne!(asked["approval_id"], id.as_str());
}

/// Saved to the repository, the host opens now and for the next session.
/// An entry that opens its egress to preparation only is not flipped open
/// for sessions by an answer that asked for one host.
#[tokio::test]
async fn saving_to_the_repository_adds_the_host_and_never_widens_more() {
    let h = harness().await;
    let proxy = session_proxy(&h).await;
    let id = named_approval(&connect(&proxy, HOST).await);
    let (status, body) = answer(&h, &id, json!({ "option_id": "allow_repo" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(proxy.grants.session_allows("s1", HOST));
    let repos = h.manager.cfg().repos();
    let entry = repos
        .iter()
        .find(|entry| entry.matches(std::path::Path::new("/r")))
        .expect("an entry for the session's repository");
    assert_eq!(entry.egress, [HOST]);
    assert!(entry.session_egress);
    let file = tracon::config::Config::try_load().unwrap();
    assert!(file.repo.iter().any(|entry| entry.egress == [HOST]));

    let h = harness_with(tracon::config::Config {
        repo: vec![tracon::config::Repo {
            path: "/r".into(),
            egress: vec!["crates".into()],
            ..Default::default()
        }],
        ..Default::default()
    })
    .await;
    let proxy = session_proxy(&h).await;
    let id = named_approval(&connect(&proxy, HOST).await);
    let (status, body) = answer(&h, &id, json!({ "option_id": "allow_repo" })).await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("preparation only"), "{body}");
    assert_eq!(h.store.get_approval(&id).unwrap().unwrap().state, "pending");
    assert!(!proxy.grants.session_allows("s1", HOST));
}

/// A session with no grant of its own has nothing to open.
#[tokio::test]
async fn a_session_without_a_grant_cannot_ask() {
    let h = harness().await;
    h.store
        .insert_session(&session_row("s1", "n1", "personal"))
        .unwrap();
    let refused = request(&h, json!({ "host": HOST, "wait_secs": 0 }))
        .await
        .unwrap_err();
    assert!(refused.contains("no egress grant"), "{refused}");
}

/// Refusals that arrive together, as a package manager's parallel downloads
/// do, raise one card between them.
#[tokio::test]
async fn parallel_refusals_raise_one_ask() {
    let h = harness().await;
    let proxy = std::sync::Arc::new(session_proxy(&h).await);
    let tries: Vec<_> = (0..8)
        .map(|_| {
            let proxy = proxy.clone();
            tokio::spawn(async move { connect(&proxy, HOST).await })
        })
        .collect();
    let mut named = std::collections::BTreeSet::new();
    for attempt in tries {
        named.insert(named_approval(&attempt.await.unwrap()));
    }
    assert_eq!(named.len(), 1, "{named:?}");
    assert_eq!(h.store.open_permission_views().unwrap().len(), 1);
}
