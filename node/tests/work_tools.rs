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

/// What the stub was sent, for the writes: method, path, body.
#[derive(Clone, Default)]
struct Bodies(Arc<Mutex<Vec<(String, String, Value)>>>);

async fn stub(
    State((seen, bodies)): State<(Seen, Bodies)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: String,
) -> (axum::http::StatusCode, Json<Value>) {
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
    bodies.0.lock().unwrap().push((
        method.to_string(),
        path.to_string(),
        serde_json::from_str(&body).unwrap_or(Value::Null),
    ));
    // An edit answers 204 with no body; a rejected one carries `errors`.
    if path == "/rest/api/2/issue/WRK-1" && method == "PUT" {
        return (axum::http::StatusCode::NO_CONTENT, Json(Value::Null));
    }
    if path == "/rest/api/2/issue/WRK-9" && method == "PUT" {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "errorMessages": [], "errors": { "priority": "Specify a valid value" } })),
        );
    }
    if path == "/rest/api/2/issue" && method == "POST" {
        return (
            axum::http::StatusCode::CREATED,
            Json(json!({ "id": "1042", "key": "WRK-9" })),
        );
    }
    // Cloud's search; a query marked `legacy` plays a Data Center that lacks it.
    if path == "/rest/api/3/search/jql" && uri.query().unwrap_or("").contains("legacy") {
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "errorMessages": ["not here"] })),
        );
    }
    if path == "/rest/api/3/search/jql" || path == "/rest/api/2/search" {
        return (
            axum::http::StatusCode::OK,
            Json(json!({ "issues": [{ "key": "WRK-1", "fields": {
                "summary": "Do the thing", "status": { "name": "In Progress" },
                "priority": { "name": "Medium" }, "issuetype": { "name": "Task" },
                "assignee": { "displayName": "J" }, "parent": { "key": "WRK-100" } } }] })),
        );
    }
    if path.contains("/repository/tags/") {
        return if path.ends_with("/v1.0") {
            (axum::http::StatusCode::OK, Json(json!({ "name": "v1.0" })))
        } else {
            (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({ "message": "404 Tag Not Found" })),
            )
        };
    }
    if path.ends_with("/jobs/901/trace") {
        let log: String = (1..=200).map(|n| format!("line {n}\n")).collect();
        return (axum::http::StatusCode::OK, Json(json!(log)));
    }
    if path.ends_with("/pipeline") && method == "POST" {
        return (
            axum::http::StatusCode::CREATED,
            Json(
                json!({ "id": 56, "status": "created", "web_url": "https://gitlab.example/g/p/-/pipelines/56" }),
            ),
        );
    }
    if path.ends_with("/pipelines") && method == "GET" {
        return (
            axum::http::StatusCode::OK,
            Json(json!([{ "id": 55, "ref": "main", "status": "running" }])),
        );
    }
    if path.ends_with("/pipelines/55/jobs") {
        return (
            axum::http::StatusCode::OK,
            Json(json!([
                { "id": 901, "name": "test", "stage": "test", "status": "success" },
                { "id": 902, "name": "deploy", "stage": "deploy", "status": "running" }
            ])),
        );
    }
    if path.ends_with("/pipelines/55") {
        return (
            axum::http::StatusCode::OK,
            Json(
                json!({ "id": 55, "ref": "main", "sha": "abc123", "status": "running",
                "source": "web", "web_url": "https://gitlab.example/g/p/-/pipelines/55" }),
            ),
        );
    }
    (
        axum::http::StatusCode::OK,
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
        }),
    )
}

async fn rig() -> (Tools, Seen, String) {
    let (t, seen, _, base) = rig_with_bodies().await;
    (t, seen, base)
}

async fn rig_with_bodies() -> (Tools, Seen, Bodies, String) {
    let seen = Seen::default();
    let bodies = Bodies::default();
    let app = Router::new()
        .route("/{*path}", any(stub))
        .with_state((seen.clone(), bodies.clone()));
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
    (tools, seen, bodies, base)
}

/// The shipped bundle asks for every write, and these tests have no session to
/// ask on. They are about what reaches Jira, so they run under a policy that
/// allows the verbs outright; that comments and edits *do* ask is asserted
/// separately, below.
fn allowing(names: &str) -> std::sync::Arc<std::sync::RwLock<tracon::policy::Policy>> {
    std::sync::Arc::new(std::sync::RwLock::new(
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
    for n in [
        "mr_status",
        "mr_comment",
        "pipeline_status",
        "job_trace",
        "pipeline_run",
        "issue",
        "issue_search",
        "issue_comment",
    ] {
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
    let (mut t, seen, _) = rig().await;
    t.policy = allowing(r#""mr_status", "mr_comment", "issue", "issue_comment""#);
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

#[tokio::test]
async fn a_write_to_the_tracker_waits_on_the_operator() {
    state::isolate();
    // The shipped bundle names the reads and nothing else, so a comment, an
    // edit, and a new issue all reach the queue. With no session to ask on,
    // that shows up as the call refusing to run unattended.
    let (t, seen, _) = rig().await;
    let c = ctx("work", "n1");
    for (name, args) in [
        ("issue_comment", json!({"key": "WRK-1", "body": "on it"})),
        ("issue_update", json!({"key": "WRK-1", "summary": "New"})),
        (
            "issue_create",
            json!({"project": "WRK", "type": "Task", "summary": "New"}),
        ),
    ] {
        let (err, v) = call(&t, &c, name, args).await;
        assert!(err, "{name} should have asked: {v}");
        assert!(
            v.as_str().unwrap_or_default().contains("approval"),
            "{name}: {v}"
        );
    }
    assert!(seen.0.lock().unwrap().is_empty(), "nothing reached Jira");
}

#[tokio::test]
async fn an_edit_sends_only_the_fields_it_names_and_never_a_transition() {
    state::isolate();
    let (mut t, seen, bodies, _) = rig_with_bodies().await;
    t.policy = allowing(r#""issue_update""#);
    let (err, v) = call(
        &t,
        &ctx("work", "n1"),
        "issue_update",
        json!({
            "key": "WRK-1",
            "summary": "Rewritten",
            "priority": "High",
            "labels": ["needs-review", "exchange"],
            "parent": "WRK-100",
        }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["key"], "WRK-1");

    let bodies = bodies.0.lock().unwrap().clone();
    let (method, path, body) = bodies.last().unwrap();
    assert_eq!(method, "PUT");
    assert_eq!(path, "/rest/api/2/issue/WRK-1");
    let f = &body["fields"];
    assert_eq!(f["summary"], "Rewritten");
    assert_eq!(f["priority"], json!({ "name": "High" }));
    assert_eq!(f["parent"], json!({ "key": "WRK-100" }));
    assert_eq!(f["labels"], json!(["needs-review", "exchange"]));
    assert!(f.get("status").is_none());
    assert!(f.get("key").is_none());

    let paths: Vec<String> = seen
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|(m, p, _)| format!("{m} {p}"))
        .collect();
    assert!(
        !paths.iter().any(|p| p.contains("/transitions")),
        "{paths:?}"
    );
}

#[tokio::test]
async fn an_edit_refuses_a_field_it_does_not_write_before_anything_is_sent() {
    state::isolate();
    let (mut t, seen, _) = rig().await;
    t.policy = allowing(r#""issue_update""#);
    let c = ctx("work", "n1");

    let (err, v) = call(
        &t,
        &c,
        "issue_update",
        json!({"key": "WRK-1", "status": "Done"}),
    )
    .await;
    assert!(err);
    let text = v.as_str().unwrap();
    assert!(text.contains("status"), "{text}");
    assert!(text.contains("issue_comment"), "{text}");

    let (err, v) = call(
        &t,
        &c,
        "issue_update",
        json!({"key": "WRK-1", "labels": ["needs review"]}),
    )
    .await;
    assert!(err);
    assert!(v.as_str().unwrap().contains("whitespace"), "{v}");

    let (err, v) = call(&t, &c, "issue_update", json!({"key": "WRK-1"})).await;
    assert!(err);
    assert!(v.as_str().unwrap().contains("at least one"), "{v}");

    assert!(seen.0.lock().unwrap().is_empty(), "nothing reached Jira");
}

#[tokio::test]
async fn a_new_issue_carries_its_project_and_type_and_comes_back_with_a_link() {
    state::isolate();
    let (mut t, _, bodies, base) = rig_with_bodies().await;
    t.policy = allowing(r#""issue_create""#);
    let (err, v) = call(
        &t,
        &ctx("work", "n1"),
        "issue_create",
        json!({"project": "WRK", "type": "Task", "summary": "Do the thing"}),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["key"], "WRK-9");
    assert_eq!(v["url"], json!(format!("{base}/browse/WRK-9")));

    let bodies = bodies.0.lock().unwrap().clone();
    let (method, path, body) = bodies.last().unwrap();
    assert_eq!(method, "POST");
    assert_eq!(path, "/rest/api/2/issue");
    assert_eq!(body["fields"]["project"], json!({ "key": "WRK" }));
    assert_eq!(body["fields"]["issuetype"], json!({ "name": "Task" }));
}

fn paths(seen: &Seen) -> Vec<String> {
    seen.0
        .lock()
        .unwrap()
        .iter()
        .map(|(m, p, _)| format!("{m} {p}"))
        .collect()
}

#[tokio::test]
async fn a_search_returns_one_compact_row_per_issue() {
    state::isolate();
    let (mut t, seen, _) = rig().await;
    t.policy = allowing(r#""issue_search""#);
    let c = ctx("work", "n1");
    let (err, v) = call(
        &t,
        &c,
        "issue_search",
        json!({ "jql": "project = WRK AND statusCategory != Done" }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(
        v["issues"][0],
        json!({ "key": "WRK-1", "type": "Task", "summary": "Do the thing",
                "status": "In Progress", "priority": "Medium", "assignee": "J",
                "parent": "WRK-100" })
    );
    assert_eq!(paths(&seen), vec!["GET /rest/api/3/search/jql"]);

    // A Data Center without the newer endpoint gets the older one.
    let (err, v) = call(&t, &c, "issue_search", json!({ "jql": "legacy" })).await;
    assert!(!err, "{v}");
    assert_eq!(v["issues"][0]["key"], "WRK-1");
    assert_eq!(
        paths(&seen)[1..],
        ["GET /rest/api/3/search/jql", "GET /rest/api/2/search"]
    );
}

#[tokio::test]
async fn pipeline_status_reads_the_latest_run_for_a_ref_with_its_jobs() {
    state::isolate();
    let (mut t, seen, _) = rig().await;
    t.policy = allowing(r#""pipeline_status""#);
    let (err, v) = call(
        &t,
        &ctx("work", "n1"),
        "pipeline_status",
        json!({ "project": "g/p", "ref": "main" }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["id"], 55);
    assert_eq!(v["status"], "running");
    assert_eq!(v["jobs"][1]["name"], "deploy");
    assert_eq!(
        paths(&seen),
        vec![
            "GET /api/v4/projects/g%2Fp/pipelines",
            "GET /api/v4/projects/g%2Fp/pipelines/55",
            "GET /api/v4/projects/g%2Fp/pipelines/55/jobs",
        ]
    );
}

#[tokio::test]
async fn a_job_trace_returns_the_end_of_the_log() {
    state::isolate();
    let (mut t, _, _) = rig().await;
    t.policy = allowing(r#""job_trace""#);
    let (err, v) = call(
        &t,
        &ctx("work", "n1"),
        "job_trace",
        json!({ "project": "g/p", "job_id": 901, "kib": 1 }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["truncated"], true);
    let log = v["log"].as_str().unwrap();
    assert!(log.contains("line 200"), "{log}");
    assert!(!log.contains("line 1\\n"), "{log}");
}

#[tokio::test]
async fn running_a_pipeline_refuses_a_tag_and_a_production_variable_before_posting() {
    state::isolate();
    let (mut t, seen, bodies, _) = rig_with_bodies().await;
    t.policy = allowing(r#""pipeline_run""#);
    let c = ctx("work", "n1");

    let (err, v) = call(
        &t,
        &c,
        "pipeline_run",
        json!({ "project": "g/p", "ref": "v1.0" }),
    )
    .await;
    assert!(err);
    assert!(v.as_str().unwrap().contains("tag"), "{v}");
    let (err, v) = call(
        &t,
        &c,
        "pipeline_run",
        json!({ "project": "g/p", "ref": "main", "variables": { "environment": "production" } }),
    )
    .await;
    assert!(err);
    assert!(v.as_str().unwrap().contains("production"), "{v}");
    assert!(
        !paths(&seen).iter().any(|p| p.starts_with("POST")),
        "nothing was run"
    );

    let (err, v) = call(
        &t,
        &c,
        "pipeline_run",
        json!({ "project": "g/p", "ref": "main", "variables": { "DEPLOY": "staging" } }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["id"], 56);
    let bodies = bodies.0.lock().unwrap().clone();
    let (method, path, body) = bodies.last().unwrap();
    assert_eq!(method, "POST");
    assert_eq!(path, "/api/v4/projects/g%2Fp/pipeline");
    assert_eq!(
        body,
        &json!({ "ref": "main", "variables": [{ "key": "DEPLOY", "value": "staging" }] })
    );
}

#[tokio::test]
async fn running_a_pipeline_waits_on_the_operator() {
    state::isolate();
    let (t, seen, _) = rig().await;
    let (err, v) = call(
        &t,
        &ctx("work", "n1"),
        "pipeline_run",
        json!({ "project": "g/p", "ref": "main" }),
    )
    .await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("approval"), "{v}");
    assert!(seen.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_refusal_from_jira_carries_the_field_it_names() {
    state::isolate();
    let (mut t, _, _) = rig().await;
    t.policy = allowing(r#""issue_update""#);
    // WRK-9's stub rejects the edit with a per-field message, which is where
    // Jira puts a bad priority or a parent it will not accept.
    let (err, v) = call(
        &t,
        &ctx("work", "n1"),
        "issue_update",
        json!({"key": "WRK-9", "priority": "Nope"}),
    )
    .await;
    assert!(err);
    let text = v.as_str().unwrap();
    assert!(text.contains("priority"), "{text}");
    assert!(text.contains("Specify a valid value"), "{text}");
}
