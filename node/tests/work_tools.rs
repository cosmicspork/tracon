//! The GitLab and Jira tools against stubs of both APIs: the token reaches
//! only the stub and only on the endpoints the tools use; the verbs that
//! would merge or transition are never called because they do not exist.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::{Arc, Mutex};

use axum::{extract::State, http::HeaderMap, routing::any, Json, Router};
use serde_json::{json, Value};
use tracon::mcp::{CallContext, Tools};

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(String, String, String)>>>);

async fn stub(
    State(seen): State<Seen>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: String,
) -> Json<Value> {
    let auth = headers
        .get("private-token")
        .or_else(|| headers.get("authorization"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    seen.0
        .lock()
        .unwrap()
        .push((method.to_string(), uri.path().to_string(), auth));
    let path = uri.path();
    Json(if path.ends_with("/approvals") {
        json!({ "approved": true, "approved_by": [{ "user": { "username": "reviewer" } }] })
    } else if path.contains("/merge_requests/7") && method == "GET" {
        json!({ "iid": 7, "title": "Add thing", "state": "opened", "draft": false,
                "source_branch": "feat/thing", "target_branch": "main",
                "detailed_merge_status": "mergeable", "has_conflicts": false,
                "head_pipeline": { "status": "success" }, "user_notes_count": 2,
                "web_url": "https://gitlab.example/g/p/-/merge_requests/7" })
    } else if path.ends_with("/notes") && method == "POST" {
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        json!({ "id": 99, "created_at": "now", "body": v["body"] })
    } else if path.ends_with("/issue/WRK-1") {
        json!({ "key": "WRK-1", "fields": { "summary": "Do the thing", "status": { "name": "In Progress" },
                "assignee": { "displayName": "J" }, "description": "…", "issuetype": { "name": "Task" },
                "priority": { "name": "Medium" },
                "comment": { "comments": [{ "author": { "displayName": "A" }, "created": "t", "body": "hi" }] } } })
    } else if path.ends_with("/issue/WRK-1/comment") && method == "POST" {
        json!({ "id": "5", "created": "now" })
    } else {
        json!({ "message": "unexpected" })
    })
}

async fn rig() -> (Tools, Seen, String) {
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
        [credentials.glab]
        channels = ["work"]
        [credentials.glab.env]
        GITLAB_HOST = "{base}"
        GITLAB_TOKEN = "glpat-secret"

        [credentials.jira]
        channels = ["work"]
        nodes = ["n1"]
        [credentials.jira.env]
        JIRA_URL = "{base}"
        JIRA_EMAIL = "me@example.com"
        JIRA_TOKEN = "jira-secret"
        "#
    );
    let tools = Tools {
        broker: Arc::new(toml::from_str(&creds).unwrap()),
        cfg: Arc::new(tracon::config::Config::default()),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    };
    (tools, seen, base)
}

fn ctx(channel: &str, node: &str) -> CallContext {
    CallContext {
        session_id: "s".into(),
        channel: channel.into(),
        node_id: node.into(),
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
    let is_error = res["result"]["isError"] == true;
    (
        is_error,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[tokio::test]
async fn the_forge_and_tracker_tools_are_offered_to_the_bound_channel_and_node() {
    state::isolate();
    let (t, _, _) = rig().await;
    let names = |c: &str, n: &str| -> Vec<String> {
        t.list(c, n)
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect()
    };
    let work = names("work", "n1");
    for n in ["mr_status", "mr_comment", "issue", "issue_comment"] {
        assert!(work.contains(&n.to_string()), "{n} missing from {work:?}");
    }
    // jira is pinned to n1; glab is not.
    let elsewhere = names("work", "n2");
    assert!(elsewhere.contains(&"mr_status".to_string()));
    assert!(!elsewhere.contains(&"issue".to_string()));
    assert!(names("personal", "n1").is_empty());
}

#[tokio::test]
async fn the_token_reaches_only_the_stub_and_only_on_the_read_and_comment_endpoints() {
    state::isolate();
    let (t, seen, _) = rig().await;
    let c = ctx("work", "n1");
    let (err, v) = call(&t, &c, "mr_status", json!({"project": "g/p", "iid": 7})).await;
    assert!(!err, "{v}");
    assert_eq!(v["state"], "opened");
    assert_eq!(v["pipeline"], "success");
    assert_eq!(v["approved"], true);
    let (err, v) = call(
        &t,
        &c,
        "mr_comment",
        json!({"project": "g/p", "iid": 7, "body": "looks fine"}),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["id"], 99);
    let (err, v) = call(&t, &c, "issue", json!({"key": "WRK-1"})).await;
    assert!(!err, "{v}");
    assert_eq!(v["status"], "In Progress");
    assert_eq!(v["comments"][0]["body"], "hi");
    let (err, v) = call(
        &t,
        &c,
        "issue_comment",
        json!({"key": "WRK-1", "body": "on it"}),
    )
    .await;
    assert!(!err, "{v}");

    let seen = seen.0.lock().unwrap().clone();
    let paths: Vec<String> = seen.iter().map(|(m, p, _)| format!("{m} {p}")).collect();
    assert_eq!(
        paths,
        vec![
            "GET /api/v4/projects/g%2Fp/merge_requests/7",
            "GET /api/v4/projects/g%2Fp/merge_requests/7/approvals",
            "POST /api/v4/projects/g%2Fp/merge_requests/7/notes",
            "GET /rest/api/2/issue/WRK-1",
            "POST /rest/api/2/issue/WRK-1/comment",
        ]
    );
    // Every GitLab call carried the token as a header; every Jira call basic auth.
    assert!(seen.iter().take(3).all(|(_, _, a)| a == "glpat-secret"));
    assert!(seen.iter().skip(3).all(|(_, _, a)| a.starts_with("Basic ")));
    // And nothing in the surface can merge or transition.
    assert!(!paths
        .iter()
        .any(|p| p.contains("/merge") && !p.contains("merge_requests")));
    assert!(!paths.iter().any(|p| p.contains("/transitions")));
}

#[tokio::test]
async fn an_unbound_channel_or_node_is_refused_before_any_request() {
    state::isolate();
    let (t, seen, _) = rig().await;
    let (err, v) = call(
        &t,
        &ctx("personal", "n1"),
        "mr_status",
        json!({"project": "g/p", "iid": 7}),
    )
    .await;
    assert!(err);
    assert!(v.as_str().unwrap().contains("not bound"), "{v}");
    let (err, v) = call(&t, &ctx("work", "n2"), "issue", json!({"key": "WRK-1"})).await;
    assert!(err);
    assert!(v.as_str().unwrap().contains("this node"), "{v}");
    assert!(seen.0.lock().unwrap().is_empty());
}
