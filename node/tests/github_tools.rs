//! The GitHub tools against a stub of its API: the token and the User-Agent
//! GitHub requires reach only the stub, a comment waits on the operator, and
//! nothing in the surface can merge.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::{Arc, Mutex};

use axum::{http::HeaderMap, routing::any, Json, Router};
use serde_json::{json, Value};
use tracon::mcp::{CallContext, Tools};

/// Method, path and query, authorization, user-agent.
type Seen = Arc<Mutex<Vec<(String, String, String, String)>>>;

async fn stub(
    axum::extract::State(seen): axum::extract::State<Seen>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> (axum::http::StatusCode, Json<Value>) {
    let header = |h: &str| {
        headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let target = match uri.query() {
        Some(q) => format!("{}?{q}", uri.path()),
        None => uri.path().to_string(),
    };
    seen.lock().unwrap().push((
        method.to_string(),
        target,
        header("authorization"),
        header("user-agent"),
    ));
    let path = uri.path();
    let ok = |v: Value| (axum::http::StatusCode::OK, Json(v));
    match (method.as_str(), path) {
        ("GET", "/repos/owner/name/pulls/7") => ok(json!({
            "number": 7, "title": "Add thing", "state": "open", "draft": false,
            "merged": false, "mergeable": true, "mergeable_state": "clean",
            "head": { "ref": "feat/thing", "sha": "abc123" }, "base": { "ref": "main" },
            "comments": 2, "html_url": "https://github.test/owner/name/pull/7"
        })),
        ("GET", "/repos/owner/name/commits/abc123/check-runs") => ok(json!({ "check_runs": [
            { "name": "test", "status": "completed", "conclusion": "success" },
            { "name": "lint", "status": "completed", "conclusion": "failure" },
            { "name": "e2e", "status": "in_progress", "conclusion": null }
        ] })),
        ("POST", "/repos/owner/name/issues/7/comments") => (
            axum::http::StatusCode::CREATED,
            Json(json!({ "id": 5, "html_url": "https://github.test/owner/name/pull/7#c5" })),
        ),
        ("GET", "/repos/owner/name/actions/runs") => ok(json!({ "workflow_runs": [{
            "id": 900, "name": "ci", "status": "completed", "conclusion": "success",
            "event": "push", "head_branch": "main", "head_sha": "abc123",
            "html_url": "https://github.test/owner/name/actions/runs/900",
            "created_at": "2026-09-10T00:00:00Z"
        }] })),
        _ => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "message": "Not Found" })),
        ),
    }
}

async fn rig() -> (Tools, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/{*path}", any(stub))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    let creds = format!(
        r#"
        [credentials.gh]
        channels = ["work"]
        [credentials.gh.env]
        GITHUB_API = "{base}"
        GH_TOKEN = "gh-secret"
        "#
    );
    let tools = Tools {
        broker: Arc::new(toml::from_str(&creds).unwrap()),
        cfg: Arc::new(tracon::config::Config::default()),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    };
    (tools, seen)
}

fn allowing(names: &str) -> Arc<std::sync::RwLock<tracon::policy::Policy>> {
    Arc::new(std::sync::RwLock::new(
        toml::from_str(&format!(
            r#"
            version = 9
            [[rule]]
            id = "test-allow"
            verdict = "allow"
            reason = "Under test."
            kinds = ["tool"]
            matches = [{names}]
            "#
        ))
        .unwrap(),
    ))
}

fn ctx(channel: &str) -> CallContext {
    CallContext {
        session_id: "s".into(),
        channel: channel.into(),
        node_id: "n1".into(),
    }
}

async fn call(t: &Tools, c: &CallContext, name: &str, args: Value) -> (bool, Value) {
    let res = t
        .handle(
            c,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),
        )
        .await
        .unwrap();
    let text = res["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    (
        res["result"]["isError"] == true,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[tokio::test]
async fn the_github_tools_are_offered_to_the_bound_channel() {
    state::isolate();
    let (t, _) = rig().await;
    let names: Vec<String> = t
        .list("work", "n1")
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    for n in ["pr_status", "pr_comment", "run_status"] {
        assert!(names.contains(&n.to_string()), "{n} missing from {names:?}");
    }
    assert!(t.list("personal", "n1").is_empty());
}

#[tokio::test]
async fn pr_status_rolls_up_the_checks_on_the_head_commit() {
    state::isolate();
    let (mut t, seen) = rig().await;
    t.policy = allowing(r#""pr_status""#);
    let (err, v) = call(
        &t,
        &ctx("work"),
        "pr_status",
        json!({ "repo": "owner/name", "number": 7 }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["state"], "open");
    assert_eq!(v["head"], "feat/thing");
    assert_eq!(v["checks"]["passed"], 1);
    assert_eq!(v["checks"]["failed"], 1);
    assert_eq!(v["checks"]["pending"], 1);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen
        .iter()
        .all(|(_, _, auth, _)| auth == "Bearer gh-secret"));
    assert!(seen.iter().all(|(_, _, _, ua)| !ua.is_empty()));
}

#[tokio::test]
async fn a_comment_waits_on_the_operator_and_then_posts_once() {
    state::isolate();
    let (mut t, seen) = rig().await;
    let c = ctx("work");
    let args = json!({ "repo": "owner/name", "number": 7, "body": "looks fine" });
    let (err, v) = call(&t, &c, "pr_comment", args.clone()).await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("approval"), "{v}");
    assert!(seen.lock().unwrap().is_empty(), "nothing reached GitHub");

    t.policy = allowing(r#""pr_comment""#);
    let (err, v) = call(&t, &c, "pr_comment", args).await;
    assert!(!err, "{v}");
    assert_eq!(v["id"], 5);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "POST");
    assert_eq!(seen[0].1, "/repos/owner/name/issues/7/comments");
}

#[tokio::test]
async fn run_status_asks_for_a_branch_and_refuses_a_bad_repo_before_any_request() {
    state::isolate();
    let (mut t, seen) = rig().await;
    t.policy = allowing(r#""run_status""#);
    let c = ctx("work");
    let (err, v) = call(
        &t,
        &c,
        "run_status",
        json!({ "repo": "owner/name", "branch": "main" }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["runs"][0]["conclusion"], "success");
    assert_eq!(
        seen.lock().unwrap()[0].1,
        "/repos/owner/name/actions/runs?branch=main&per_page=10"
    );

    let (err, _) = call(
        &t,
        &c,
        "run_status",
        json!({ "repo": "../etc", "branch": "main" }),
    )
    .await;
    assert!(err);
    let (err, _) = call(&t, &c, "run_status", json!({ "repo": "owner/name" })).await;
    assert!(err);
    assert_eq!(seen.lock().unwrap().len(), 1, "the bad calls sent nothing");
}
