//! An MCP server outside the node, offered through the node's own endpoint.
//! A fake server runs in-process; the node lists its tools under the
//! server's name, runs an allowed call with the brokered credential, holds an
//! asked one as an approval carrying its exact arguments, refuses a denied
//! one without reaching the server, and says so when the server is down.

#[path = "support/mod.rs"]
mod support;

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json,
};
use serde_json::{json, Value};
use tracon::{
    broker::SharedBroker,
    config::{Config, McpServer},
    mcp::{CallContext, SessionAccess, Tools},
    session::{Manager, OperatorAnswer},
    store::{approvals, Store},
    stream::Bus,
};

#[derive(Default)]
struct Fake {
    /// Each call the server received: tool, arguments, Authorization.
    calls: parking_lot::Mutex<Vec<(String, Value, Option<String>)>>,
    sessions: parking_lot::Mutex<Vec<String>>,
    initialized: AtomicUsize,
    /// Forget every session, as a restarted server does.
    forget: AtomicBool,
}

async fn mcp(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
    Json(msg): Json<Value>,
) -> Response {
    let method = msg["method"].as_str().unwrap_or_default().to_string();
    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if method == "initialize" {
        let n = fake.initialized.fetch_add(1, Ordering::SeqCst);
        let id = format!("session-{n}");
        fake.sessions.lock().push(id.clone());
        fake.forget.store(false, Ordering::SeqCst);
        return (
            [("mcp-session-id", id)],
            Json(json!({
                "jsonrpc": "2.0", "id": msg["id"],
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "fake", "version": "1" },
                },
            })),
        )
            .into_response();
    }
    let known = session
        .as_ref()
        .is_some_and(|s| fake.sessions.lock().contains(s));
    if !known || fake.forget.load(Ordering::SeqCst) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if msg.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    match method.as_str() {
        "tools/list" => Json(json!({
            "jsonrpc": "2.0", "id": msg["id"],
            "result": { "tools": [
                {
                    "name": "echo",
                    "description": "Says back what it is given.",
                    "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] },
                    "_meta": { "anthropic/alwaysLoad": true },
                },
                { "name": "create_issue", "description": "Opens an issue.", "inputSchema": { "type": "object", "properties": { "title": { "type": "string" } } } },
                { "name": "delete_issue", "description": "Deletes an issue.", "inputSchema": { "type": "object" } },
                { "name": "issues.search", "inputSchema": null },
                { "name": "broken", "inputSchema": { "type": "object" } },
            ] },
        }))
        .into_response(),
        "tools/call" => {
            let tool = msg["params"]["name"].as_str().unwrap_or_default().to_string();
            let args = msg["params"]["arguments"].clone();
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            fake.calls.lock().push((tool.clone(), args.clone(), auth));
            let result = if tool == "broken" {
                json!({ "content": [{ "type": "text", "text": "it broke" }], "isError": true })
            } else {
                json!({ "content": [{ "type": "text", "text": format!("{tool}: {args}") }] })
            };
            // Answered as an event stream, after a notification of its own.
            let body = format!(
                "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
                json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} }),
                json!({ "jsonrpc": "2.0", "id": msg["id"], "result": result }),
            );
            ([("content-type", "text/event-stream")], body).into_response()
        }
        _ => Json(json!({ "jsonrpc": "2.0", "id": msg["id"], "error": { "code": -32601, "message": "no" } }))
            .into_response(),
    }
}

async fn fake_server() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = axum::Router::new()
        .route("/mcp", post(mcp))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/mcp"), fake)
}

/// An address nothing listens on.
async fn closed_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/mcp")
}

struct Rig {
    store: Arc<Store>,
    tools: Arc<Tools>,
    ctx: CallContext,
}

fn entry(name: &str, url: &str) -> McpServer {
    McpServer {
        name: name.into(),
        url: url.into(),
        channels: vec!["personal".into()],
        credential: Some("fake".into()),
        allow: vec!["echo".into(), "broken".into()],
        deny: vec!["delete_*".into()],
        ..Default::default()
    }
}

fn rig(servers: Vec<McpServer>) -> Rig {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    store.channel_put("personal", &[], "{}").unwrap();
    store.channel_put("work", &[], "{}").unwrap();
    store
        .insert_session(&support::rows::session_row("s1", "n1", "personal"))
        .unwrap();
    store
        .insert_session(&support::rows::session_row("s2", "n1", "work"))
        .unwrap();
    let cfg = Arc::new(Config {
        mcp_server: servers,
        ..Default::default()
    });
    let broker: SharedBroker = Arc::new(
        toml::from_str(
            r#"
            [credentials.fake]
            channels = ["personal"]
            [credentials.fake.env]
            TOKEN = "s3cret"
        "#,
        )
        .unwrap(),
    );
    let tools = Arc::new(Tools {
        broker,
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
        upstream: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg,
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let _ = tools.session.set(SessionAccess {
        store: store.clone(),
        manager,
    });
    Rig {
        store,
        tools,
        ctx: CallContext::session("s1", "personal", "n1"),
    }
}

impl Rig {
    async fn rpc(&self, ctx: &CallContext, method: &str, params: Value) -> Value {
        self.tools
            .handle(
                ctx,
                &json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }),
            )
            .await
            .unwrap()["result"]
            .clone()
    }

    async fn listed(&self, ctx: &CallContext) -> Vec<Value> {
        self.rpc(ctx, "tools/list", json!({})).await["tools"]
            .as_array()
            .unwrap()
            .clone()
    }

    async fn call(&self, name: &str, args: Value) -> Value {
        self.rpc(
            &self.ctx,
            "tools/call",
            json!({ "name": name, "arguments": args }),
        )
        .await
    }
}

fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn a_registered_servers_tools_are_listed_under_its_name_and_deferred() {
    let (url, _fake) = fake_server().await;
    let rig = rig(vec![entry("fake", &url)]);
    let tools = rig.listed(&rig.ctx).await;
    let named = |name: &str| tools.iter().find(|t| t["name"] == name).cloned();

    let echo = named("fake__echo").expect("the server's tool is offered under its name");
    assert!(echo["description"].as_str().unwrap().contains("fake"));
    assert_eq!(echo["inputSchema"]["required"], json!(["text"]));
    // Deferred behind tool search like the node's other non-core tools: the
    // server's own claim to be always loaded is not carried through.
    assert!(echo.get("_meta").is_none(), "{echo}");
    assert!(named("fake__create_issue").is_some());
    // A name a function cannot carry is spelled one it can.
    let search = named("fake__issues_search").unwrap();
    assert_eq!(search["inputSchema"]["type"], "object");
    // The node's own tools are still there, and the core ones still loaded.
    assert!(named("doc_read").is_some());
    assert!(named("submit_review").unwrap()["_meta"]["anthropic/alwaysLoad"] == true);

    // A channel the server does not serve is offered none of it.
    let work = CallContext::session("s2", "work", "n1");
    assert!(rig
        .listed(&work)
        .await
        .iter()
        .all(|t| !t["name"].as_str().unwrap().starts_with("fake__")));
    // Nor is a plan session.
    let plan = rig
        .tools
        .list_for("personal", "n1", tracon::session::Phase::Plan);
    assert!(plan
        .iter()
        .all(|t| !t["name"].as_str().unwrap().starts_with("fake__")));
}

#[tokio::test]
async fn an_allowed_call_runs_with_the_brokered_credential() {
    let (url, fake) = fake_server().await;
    let rig = rig(vec![entry("fake", &url)]);
    let out = rig.call("fake__echo", json!({ "text": "hi" })).await;
    assert_eq!(out["isError"], false, "{out}");
    assert_eq!(text(&out), r#"echo: {"text":"hi"}"#);
    let calls = fake.calls.lock().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "echo");
    assert_eq!(calls[0].1, json!({ "text": "hi" }));
    assert_eq!(calls[0].2.as_deref(), Some("Bearer s3cret"));

    // A result the server marks as an error reaches the caller as one.
    let out = rig.call("fake__broken", json!({})).await;
    assert_eq!(out["isError"], true);
    assert!(text(&out).contains("it broke"), "{out}");
}

#[tokio::test]
async fn an_asked_call_is_held_as_an_approval_with_its_exact_arguments() {
    let (url, fake) = fake_server().await;
    let rig = rig(vec![entry("fake", &url)]);
    let args = json!({ "title": "Rotate keys", "labels": ["ops", "urgent"] });
    let out = rig.call("fake__create_issue", args.clone()).await;
    let held: Value = serde_json::from_str(&text(&out)).unwrap();
    assert_eq!(held["state"], "awaiting_operator", "{held}");
    // Nothing reached the server while it waits.
    assert!(fake.calls.lock().is_empty());

    let id = held["approval_id"].as_str().unwrap();
    let card = rig.store.get_approval(id).unwrap().unwrap();
    assert_eq!(card.tool, "fake__create_issue");
    assert_eq!(card.state, approvals::PENDING);
    assert_eq!(
        serde_json::from_str::<Value>(&card.arguments).unwrap(),
        args
    );
    assert!(card.title.contains("Rotate keys"), "{}", card.title);

    // Allowed, it runs once with what the operator saw.
    rig.tools
        .answer_approval(
            id,
            OperatorAnswer {
                option_id: tracon::adapter::types::OPTION_ALLOW_ONCE.into(),
                arguments: None,
                reason: None,
                notes: None,
                arguments_hash: None,
                by: None,
            },
        )
        .await
        .unwrap();
    let mut settled = None;
    for _ in 0..100 {
        let row = rig.store.get_approval(id).unwrap().unwrap();
        if row.state != approvals::PENDING && row.state != approvals::RUNNING {
            settled = Some(row);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let settled = settled.expect("the approved call settles");
    assert_eq!(settled.state, approvals::SUCCEEDED, "{:?}", settled.reason);
    let calls = fake.calls.lock().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "create_issue");
    assert_eq!(calls[0].1, args);
}

#[tokio::test]
async fn a_denied_call_is_refused_without_reaching_the_server() {
    let (url, fake) = fake_server().await;
    let rig = rig(vec![entry("fake", &url)]);
    let out = rig.call("fake__delete_issue", json!({ "id": 7 })).await;
    assert_eq!(out["isError"], true);
    let said = text(&out);
    assert!(said.contains("mcp_server.fake.deny"), "{said}");
    assert!(fake.calls.lock().is_empty());
    assert!(rig.store.approvals_of_session("s1").unwrap().is_empty());

    // A signed policy denial reaches another server's tools as it reaches
    // the node's own: the shipped bundle refuses a push by substring.
    let out = rig
        .call("fake__echo", json!({ "text": "git push origin main" }))
        .await;
    assert_eq!(out["isError"], true);
    assert!(text(&out).contains("review-before-publish"), "{out}");
    assert!(fake.calls.lock().is_empty());
}

#[tokio::test]
async fn a_server_that_is_down_is_reported_and_never_asked_about() {
    let rig = rig(vec![entry("gone", &closed_url().await)]);
    let tools = rig.listed(&rig.ctx).await;
    assert!(tools
        .iter()
        .all(|t| !t["name"].as_str().unwrap().starts_with("gone__")));
    // The node's own tools are unaffected.
    assert!(tools.iter().any(|t| t["name"] == "doc_read"));

    let out = rig
        .call("gone__create_issue", json!({ "title": "x" }))
        .await;
    assert_eq!(out["isError"], true);
    assert!(text(&out).contains("cannot be reached"), "{out}");
    assert!(rig.store.approvals_of_session("s1").unwrap().is_empty());

    let status = rig.tools.upstream_status();
    assert_eq!(status[0]["name"], "gone");
    assert_eq!(status[0]["listed"], false);
    assert!(status[0]["error"].as_str().unwrap().contains("unreachable"));
}

#[tokio::test]
async fn a_server_that_forgets_its_session_is_started_again() {
    let (url, fake) = fake_server().await;
    let rig = rig(vec![entry("fake", &url)]);
    rig.call("fake__echo", json!({ "text": "one" })).await;
    let before = fake.initialized.load(Ordering::SeqCst);
    fake.forget.store(true, Ordering::SeqCst);
    let out = rig.call("fake__echo", json!({ "text": "two" })).await;
    assert_eq!(out["isError"], false, "{out}");
    assert!(fake.initialized.load(Ordering::SeqCst) > before);
    assert_eq!(fake.calls.lock().len(), 2);
}

#[tokio::test]
async fn the_status_names_each_tools_answer() {
    let (url, _fake) = fake_server().await;
    let rig = rig(vec![entry("fake", &url)]);
    rig.tools.refresh_upstream("n1", true).await;
    let status = rig.tools.upstream_status();
    let tool = |name: &str| {
        status[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["remote"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(status[0]["listed"], true);
    assert_eq!(tool("echo")["decision"], "allow");
    assert_eq!(tool("create_issue")["decision"], "ask");
    assert_eq!(tool("create_issue")["setting"], Value::Null);
    assert_eq!(tool("delete_issue")["decision"], "deny");
}
