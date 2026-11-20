//! The GitLab tools against a stub of its API: the token reaches only the
//! stub and only on the endpoints the tools use. Jira is the jira-review
//! plugin's, tested in its own repository and through `plugin_tools`.

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
    // A draft by its title, as GitLab reads one: the last title written wins.
    if path.ends_with("/merge_requests/8") {
        let title = bodies
            .0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(m, p, _)| m == "PUT" && p.ends_with("/merge_requests/8"))
            .and_then(|(_, _, b)| b["title"].as_str().map(str::to_string))
            .unwrap_or_else(|| "Draft: Add eight".into());
        return (
            axum::http::StatusCode::OK,
            Json(json!({ "iid": 8, "state": "opened", "title": title,
                "draft": title.starts_with("Draft: "), "sha": "def456",
                "web_url": "https://gitlab.example/g/p/-/merge_requests/8" })),
        );
    }
    // GitLab takes the new title and still reads the merge request as a draft.
    if path.ends_with("/merge_requests/11") {
        return (
            axum::http::StatusCode::OK,
            Json(
                json!({ "iid": 11, "state": "opened", "title": "Draft: Stuck",
                "draft": true, "sha": "def456" }),
            ),
        );
    }
    if path.ends_with("/merge_requests/10") && method == "GET" {
        return (
            axum::http::StatusCode::OK,
            Json(json!({ "iid": 10, "state": "merged", "title": "Draft: x", "draft": true })),
        );
    }
    if path.ends_with("/merge_requests/7/discussions") && method == "GET" {
        return (
            axum::http::StatusCode::OK,
            Json(json!([
                { "id": "aa11", "individual_note": true, "notes": [
                    { "id": 1, "body": "general note", "author": { "username": "rev" }, "created_at": "t" }] },
                { "id": "bb22", "individual_note": true, "notes": [
                    { "id": 2, "body": "added 1 commit", "system": true }] },
                { "id": "cc33", "individual_note": false, "notes": [
                    { "id": 3, "body": "rename this", "resolvable": true, "resolved": false,
                      "author": { "username": "rev" }, "created_at": "t",
                      "position": { "new_path": "src/a.rs", "new_line": 12 } }] }
            ])),
        );
    }
    if path.ends_with("/merge_requests/7/discussions/cc33/notes") && method == "POST" {
        return (
            axum::http::StatusCode::CREATED,
            Json(json!({ "id": 44, "body": "renamed" })),
        );
    }
    if path.ends_with("/merge_requests/7/discussions/cc33") && method == "PUT" {
        return (
            axum::http::StatusCode::OK,
            Json(json!({ "id": "cc33", "notes": [
                { "id": 3, "resolvable": true, "resolved": true },
                { "id": 44, "resolvable": true, "resolved": true }] })),
        );
    }
    if path.ends_with("/merge_requests") && method == "GET" {
        return (
            axum::http::StatusCode::OK,
            Json(json!([{ "iid": 7, "title": "Add thing", "draft": false,
                "target_branch": "main", "web_url": "https://gitlab.example/g/p/-/merge_requests/7" }])),
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
        [credentials.gh]
        channels = ["work"]
        [credentials.gh.env]
        GITHUB_API = "{base}"
        GH_TOKEN = "gh-secret"

        [credentials.glab]
        channels = ["work"]
        [credentials.glab.env]
        GITLAB_HOST = "{base}"
        GITLAB_TOKEN = "glpat-secret"
        "#
    );
    let tools = Tools {
        broker: Arc::new(toml::from_str(&creds).unwrap()),
        cfg: Arc::new(tracon::config::Config::default()),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
        upstream: Default::default(),
    };
    (tools, seen, bodies, base)
}

/// The shipped bundle asks for every write, and these tests have no session to
/// ask on. They are about what reaches the forge, so they run under a policy
/// that allows the verbs outright.
fn allowing(names: &str) -> std::sync::Arc<parking_lot::RwLock<tracon::policy::Policy>> {
    std::sync::Arc::new(parking_lot::RwLock::new(
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

fn paths(seen: &Seen) -> Vec<String> {
    seen.0
        .lock()
        .unwrap()
        .iter()
        .map(|(m, p, _)| format!("{m} {p}"))
        .collect()
}

fn ctx(channel: &str, node: &str) -> CallContext {
    CallContext::session("s", channel, node)
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
    ] {
        assert!(work.contains(&n.to_string()), "{n} missing from {work:?}");
    }
    // The node offers no Jira tool of its own, whatever the channel binds.
    assert!(!work.iter().any(|n| n.starts_with("issue")), "{work:?}");
    let elsewhere = names("work", "n2");
    assert!(elsewhere.contains(&"mr_status".to_string()));
    assert!(names("personal", "n1").is_empty());
}

/// A tool that is offered but not routed fails only after the operator was
/// asked about it; every offered name must reach its family's handler.
#[tokio::test]
async fn every_offered_tool_is_routed_to_a_handler() {
    state::isolate();
    let (mut t, _, _) = rig().await;
    let names: Vec<String> = t
        .list("work", "n1")
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"pr_status".to_string()), "{names:?}");
    let quoted: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
    t.policy = allowing(&quoted.join(", "));
    let c = ctx("work", "n1");
    for name in &names {
        let (_, v) = call(&t, &c, name, json!({})).await;
        assert!(
            !v.to_string().contains("no tool named"),
            "{name} is offered but not routed: {v}"
        );
    }
}

/// The GitLab half of #600: a draft merge request is marked ready, asked
/// first, by its title, and turned back the same way.
#[tokio::test]
async fn mr_ready_waits_on_the_operator_then_marks_the_draft_ready_and_back() {
    state::isolate();
    let (mut t, seen, bodies, _) = rig_with_bodies().await;
    let c = ctx("work", "n1");
    let args = json!({ "project": "g/p", "iid": 8 });
    let (err, v) = call(&t, &c, "mr_ready", args.clone()).await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("approval"), "{v}");
    assert!(seen.0.lock().unwrap().is_empty(), "nothing reached GitLab");

    t.policy = allowing(r#""mr_ready""#);
    let puts = || -> Vec<Value> {
        bodies
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, ..)| m == "PUT")
            .map(|(_, _, b)| b.clone())
            .collect()
    };
    let (err, v) = call(&t, &c, "mr_ready", args.clone()).await;
    assert!(!err, "{v}");
    assert_eq!(v["draft"], false);
    assert_eq!(v["head_sha"], "def456");
    assert_eq!(v["already"], false);
    assert_eq!(puts(), vec![json!({ "title": "Add eight" })]);

    // Ready already: answered as it stands, with no second write.
    let (err, v) = call(&t, &c, "mr_ready", args).await;
    assert!(!err, "{v}");
    assert_eq!(v["already"], true);
    assert_eq!(puts().len(), 1);

    let back = json!({ "project": "g/p", "iid": 8, "ready": false });
    let (err, v) = call(&t, &c, "mr_ready", back.clone()).await;
    assert!(!err, "{v}");
    assert_eq!(v["draft"], true);
    assert_eq!(puts()[1], json!({ "title": "Draft: Add eight" }));
    let (_, v) = call(&t, &c, "mr_ready", back).await;
    assert_eq!(v["already"], true, "{v}");
    assert_eq!(puts().len(), 2);

    let (err, v) = call(&t, &c, "mr_ready", json!({ "project": "g/p", "iid": 10 })).await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("is merged"), "{v}");
    assert_eq!(puts().len(), 2);
}

#[tokio::test]
async fn mr_ready_is_an_error_when_gitlab_still_reads_a_draft_after_the_retitle() {
    state::isolate();
    let (mut t, _, bodies, _) = rig_with_bodies().await;
    let c = ctx("work", "n1");
    // A `ready` the card cannot name is refused before anyone is asked.
    let (err, v) = call(
        &t,
        &c,
        "mr_ready",
        json!({ "project": "g/p", "iid": 11, "ready": "false" }),
    )
    .await;
    assert!(err);
    let said = v.as_str().unwrap_or_default();
    assert!(said.contains("ready is true or false"), "{v}");
    assert!(!said.contains("approval"), "{v}");

    t.policy = allowing(r#""mr_ready""#);
    let (err, v) = call(&t, &c, "mr_ready", json!({ "project": "g/p", "iid": 11 })).await;
    assert!(err);
    assert!(
        v.as_str()
            .unwrap_or_default()
            .contains("but still reads !11 as a draft"),
        "{v}"
    );
    let puts: Vec<Value> = bodies
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, ..)| m == "PUT")
        .map(|(_, _, b)| b.clone())
        .collect();
    assert_eq!(puts, vec![json!({ "title": "Stuck" })]);
}

#[tokio::test]
async fn the_token_reaches_only_the_stub_and_only_on_the_read_and_comment_endpoints() {
    state::isolate();
    let (mut t, seen, _) = rig().await;
    t.policy = allowing(r#""mr_status", "mr_comment""#);
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

    let seen = seen.0.lock().unwrap().clone();
    let paths: Vec<String> = seen.iter().map(|(m, p, _)| format!("{m} {p}")).collect();
    assert_eq!(
        paths,
        vec![
            "GET /api/v4/projects/g%2Fp/merge_requests/7",
            "GET /api/v4/projects/g%2Fp/merge_requests/7/approvals",
            "GET /api/v4/projects/g%2Fp/merge_requests/7/notes",
            "POST /api/v4/projects/g%2Fp/merge_requests/7/notes",
        ]
    );
    // Every GitLab call carried the token as a header.
    assert!(seen.iter().all(|(_, _, a)| a == "glpat-secret"));
    // And nothing in the surface can merge.
    assert!(!paths
        .iter()
        .any(|p| p.contains("/merge") && !p.contains("merge_requests")));
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
    assert!(seen.0.lock().unwrap().is_empty());
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
async fn mr_discussions_reads_threads_and_comments_without_system_notes() {
    state::isolate();
    let (t, seen, _) = rig().await;
    // Shipped policy: a read runs unattended.
    let c = ctx("work", "n1");
    let (err, v) = call(
        &t,
        &c,
        "mr_discussions",
        json!({ "project": "g/p", "iid": 7 }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["comments"].as_array().unwrap().len(), 1);
    assert_eq!(v["comments"][0]["body"], "general note");
    assert_eq!(v["threads"].as_array().unwrap().len(), 1);
    assert_eq!(v["threads"][0]["id"], "cc33");
    assert_eq!(v["threads"][0]["path"], "src/a.rs");
    assert_eq!(v["threads"][0]["resolved"], false);
    let seen = seen.0.lock().unwrap().clone();
    assert!(
        seen[0].1.ends_with("/merge_requests/7/discussions"),
        "{seen:?}"
    );
    assert_eq!(seen[0].2, "glpat-secret");
}

#[tokio::test]
async fn an_mr_reply_waits_on_the_operator_then_replies_and_resolves() {
    state::isolate();
    let (mut t, seen, bodies, _) = rig_with_bodies().await;
    let c = ctx("work", "n1");
    let args = json!({ "project": "g/p", "iid": 7, "discussion_id": "cc33",
                       "body": "renamed", "resolve": true });
    let (err, v) = call(&t, &c, "mr_reply", args.clone()).await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("approval"), "{v}");
    assert!(seen.0.lock().unwrap().is_empty(), "nothing reached GitLab");

    t.policy = allowing(r#""mr_reply""#);
    let (err, v) = call(&t, &c, "mr_reply", args).await;
    assert!(!err, "{v}");
    assert_eq!(v["note_id"], 44);
    assert_eq!(v["resolved"], true);
    let sent = bodies.0.lock().unwrap().clone();
    assert_eq!(sent[0].0, "POST");
    assert_eq!(sent[0].2["body"], "renamed");
    assert_eq!(sent[1].0, "PUT");
    assert_eq!(sent[1].2["resolved"], true);

    let (err, _) = call(
        &t,
        &c,
        "mr_reply",
        json!({ "project": "g/p", "iid": 7, "discussion_id": "../../notes", "body": "b" }),
    )
    .await;
    assert!(err, "a discussion id is hex, never a path");
    assert_eq!(bodies.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn mr_for_branch_finds_the_open_merge_request() {
    state::isolate();
    let (t, seen, _) = rig().await;
    let (err, v) = call(
        &t,
        &ctx("work", "n1"),
        "mr_for_branch",
        json!({ "project": "g/p", "branch": "feat/thing" }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["open"][0]["iid"], 7);
    let seen = seen.0.lock().unwrap().clone();
    assert!(seen[0].1.ends_with("/merge_requests"), "{seen:?}");
}
