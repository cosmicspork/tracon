//! `web_fetch`: the node fetches a page for a session, deny by default.
//!
//! A URL the channel's fetch allow list does not cover raises a card with
//! three answers (fetch once, save to channel, deny); a saved pattern fetches
//! unasked afterwards; each redirect is held to the same policy; every fetch
//! is recorded on the session as evidence. And a session's refused request
//! at the egress proxy says to use `web_fetch` in its body.

#[path = "support/mod.rs"]
mod support;
use support::{
    harness::{harness_on, Harness},
    rows::session_row,
};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;
use tracon::{
    boundary::{Backend, BoundaryError, BoundaryReport},
    config::Config,
    gateway::proxy::{serve_granted, GrantSpec, Granted, Grants, Refusal},
    runner::{Mount, Runner},
};

/// `LocalBackend`, except that the node may dial loopback, where the test's
/// pages are: the local backend refuses this machine, as Podman does.
struct Loopback {
    inner: tracon::runner::local::LocalBackend,
}

#[async_trait]
impl Backend for Loopback {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
        self.inner.setup(cfg, rebuild).await
    }
    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport {
        self.inner.check_all(cfg, deep).await
    }
    fn runner(&self, extra: Vec<Mount>) -> Arc<dyn Runner> {
        self.inner.runner(extra)
    }
    fn runner_for(&self, harness_id: &str, extra: Vec<Mount>) -> Arc<dyn Runner> {
        self.inner.runner_for(harness_id, extra)
    }
    async fn import_volume(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        self.inner.import_volume(volume, source).await
    }
    async fn export_volume(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        self.inner.export_volume(volume, destination).await
    }
    fn harness_host(&self) -> String {
        self.inner.harness_host()
    }
    fn harness_home(&self) -> String {
        self.inner.harness_home()
    }
    async fn reconcile(&self, names: &[String]) {
        self.inner.reconcile(names).await
    }
    fn egress_refusal(&self) -> Refusal {
        Refusal::nothing()
    }
}

async fn setup() -> Harness {
    let h = harness_on(
        Config::default(),
        Arc::new(Loopback {
            inner: tracon::runner::local::LocalBackend,
        }),
    )
    .await;
    h.store
        .insert_session(&session_row("s1", "n1", "personal"))
        .unwrap();
    h
}

/// Pages: `/page` answers, `/hop` redirects to `/page` on the same origin,
/// `/away` redirects to the same page under another host name.
async fn pages() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let response = match path.as_str() {
                    "/page" => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_string(),
                    "/hop" => "HTTP/1.1 301 Moved\r\nLocation: /page\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                    "/away" => format!("HTTP/1.1 302 Found\r\nLocation: http://localhost:{}/page\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", addr.port()),
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

async fn fetch(h: &Harness, args: Value) -> Result<Value, String> {
    let ctx = tracon::mcp::CallContext::session("s1", "personal", "n1");
    h.tools.call(&ctx, "web_fetch", &args).await
}

async fn answer(h: &Harness, id: &str, body: Value) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/api/permissions/{id}/answer"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    // At the node: saving to the channel is an admin answer, which only a
    // local caller makes without a passkey.
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            5000,
        ))));
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

/// The approval once the node has run what was allowed.
async fn settled(h: &Harness, id: &str) -> tracon::store::approvals::ApprovalRow {
    for _ in 0..200 {
        let row = h.store.get_approval(id).unwrap().unwrap();
        if !matches!(row.state.as_str(), "pending" | "running") {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("approval {id} never settled");
}

fn evidence(h: &Harness) -> Vec<Value> {
    h.store
        .events_after("s1", 0, 100)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "web_fetched")
        .map(|event| event.payload)
        .collect()
}

/// Deny by default: the first fetch is a card with three answers, and
/// "once" fetches that URL and nothing more.
#[tokio::test]
async fn a_url_off_the_allow_list_is_asked_and_once_fetches_it_once() {
    let h = setup().await;
    let addr = pages().await;
    let url = format!("http://{addr}/page");

    let asked = fetch(&h, json!({ "url": url })).await.unwrap();
    assert_eq!(asked["state"], "awaiting_operator", "{asked}");
    let id = asked["approval_id"].as_str().unwrap().to_string();
    let cards = h.store.open_permission_views().unwrap();
    assert_eq!(cards.len(), 1);
    let options: Value = serde_json::from_str(&cards[0].request.options).unwrap();
    let kinds: Vec<&str> = options
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["option_id"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["allow_once", "allow_channel", "reject_once"]);
    let held = h.store.get_approval(&id).unwrap().unwrap();
    let args: Value = serde_json::from_str(&held.arguments).unwrap();
    assert_eq!(args["pattern"], format!("http://{addr}/*"));

    let (status, body) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let done = settled(&h, &id).await;
    assert_eq!(done.state, "succeeded", "{:?}", done.reason);
    let result: Value = serde_json::from_str(done.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["status"], 200);
    assert_eq!(result["body"], "hello");
    let digest = result["sha256"].as_str().unwrap().to_string();
    assert_eq!(digest.len(), 64);

    let seen = evidence(&h);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0]["url"], url.as_str());
    assert_eq!(seen[0]["final_url"], url.as_str());
    assert_eq!(seen[0]["status"], 200);
    assert_eq!(seen[0]["sha256"], digest.as_str());
    assert_eq!(seen[0]["decided_by"], "operator");
    assert_eq!(seen[0]["approval_id"], id.as_str());

    // Once is once: the same URL is asked again.
    let again = fetch(&h, json!({ "url": url })).await.unwrap();
    assert_eq!(again["state"], "awaiting_operator", "{again}");
}

/// Saving to the channel adds the pattern, fetches, and lets later fetches it
/// covers through unasked; a redirect it does not cover stops and is named.
#[tokio::test]
async fn saving_to_the_channel_fetches_unasked_from_then_on() {
    let h = setup().await;
    let addr = pages().await;
    let asked = fetch(&h, json!({ "url": format!("http://{addr}/hop") }))
        .await
        .unwrap();
    let id = asked["approval_id"].as_str().unwrap().to_string();
    // The operator narrows nothing, but may not save a pattern that misses
    // the URL.
    let (status, body) = answer(
        &h,
        &id,
        json!({
            "option_id": "allow_channel",
            "arguments": { "url": format!("http://{addr}/hop"), "pattern": "https://elsewhere.test/*" },
        }),
    )
    .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert_eq!(h.store.get_approval(&id).unwrap().unwrap().state, "pending");

    let (status, body) = answer(&h, &id, json!({ "option_id": "allow_channel" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let done = settled(&h, &id).await;
    assert_eq!(done.state, "succeeded", "{:?}", done.reason);
    let result: Value = serde_json::from_str(done.result.as_deref().unwrap()).unwrap();
    // The redirect stayed on the saved origin, so it was followed.
    assert_eq!(result["final_url"], format!("http://{addr}/page"));
    assert_eq!(result["redirects"], json!([format!("http://{addr}/page")]));

    let grants = h.store.authority_grants(false).unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].action, "fetch");
    assert_eq!(grants[0].target, format!("http://{addr}/*"));
    assert_eq!(grants[0].channel, "personal");

    // Unasked now.
    let page = fetch(&h, json!({ "url": format!("http://{addr}/page") }))
        .await
        .unwrap();
    assert_eq!(page["body"], "hello", "{page}");
    assert!(h.store.open_permission_views().unwrap().is_empty());
    let last = evidence(&h).pop().unwrap();
    assert_eq!(last["decided_by"], "grant");

    // A redirect to a host the list does not cover is not followed.
    let refused = fetch(&h, json!({ "url": format!("http://{addr}/away") }))
        .await
        .unwrap_err();
    assert!(refused.contains("localhost"), "{refused}");
    assert!(
        refused.contains("call web_fetch with that URL"),
        "{refused}"
    );
    let last = evidence(&h).pop().unwrap();
    assert!(last["error"].as_str().unwrap().contains("localhost"));
}

/// Deny settles the card and fetches nothing; a pattern the channel denies
/// is refused at once, whatever else allows it.
#[tokio::test]
async fn deny_fetches_nothing_and_a_denied_pattern_is_refused_unasked() {
    let h = setup().await;
    let addr = pages().await;
    let asked = fetch(&h, json!({ "url": format!("http://{addr}/page") }))
        .await
        .unwrap();
    let id = asked["approval_id"].as_str().unwrap().to_string();
    let (status, body) = answer(
        &h,
        &id,
        json!({ "option_id": "reject_once", "reason": "not that site" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        h.store.get_approval(&id).unwrap().unwrap().state,
        "rejected"
    );
    assert!(evidence(&h).is_empty());

    for (verdict, target) in [
        ("allow", format!("http://{addr}/*")),
        ("deny", "127.0.0.1".into()),
    ] {
        h.store
            .authority_grant_insert(&tracon::store::AuthorityGrantRow {
                id: uuid::Uuid::now_v7().to_string(),
                action: "fetch".into(),
                verdict: verdict.into(),
                target,
                channel: "personal".into(),
                session_id: None,
                revision: None,
                expires_ms: None,
                revoked_ms: None,
                reason: "no local pages".into(),
                created_ms: tracon::store::now_ms(),
            })
            .unwrap();
    }
    let refused = fetch(&h, json!({ "url": format!("http://{addr}/page") }))
        .await
        .unwrap_err();
    assert!(refused.contains("no local pages"), "{refused}");
    assert!(h.store.open_permission_views().unwrap().is_empty());
}

/// A session's refused request at the egress proxy reads, in its body, as a
/// pointer to `web_fetch` rather than a bare 403.
#[tokio::test]
async fn the_proxys_refusal_body_points_a_session_at_web_fetch() {
    use base64::Engine;
    let grants = Grants::default();
    let token = grants
        .issue(GrantSpec {
            client: "session".into(),
            session_id: Some("s1".into()),
            refusal: "not reachable from a session".into(),
            ..Default::default()
        })
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_granted(
        listener,
        Granted::dialling_any_address(grants.clone()),
    ));
    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("session:{token}"));
    for request in [
        format!(
            "CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\
             Proxy-Authorization: Basic {credentials}\r\n\r\n"
        ),
        // What `curl http://…` sends through a proxy.
        format!(
            "GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n\
             Proxy-Authorization: Basic {credentials}\r\nConnection: close\r\n\r\n"
        ),
    ] {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut said = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut said)).await;
        let said = String::from_utf8_lossy(&said);
        assert!(said.starts_with("HTTP/1.1 403"), "{said}");
        assert!(said.contains("call the node's web_fetch tool"), "{said}");
    }
}
