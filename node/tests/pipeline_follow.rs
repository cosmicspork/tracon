//! Following a pipeline an agent started: `pipeline_run` subscribes its
//! session, each job result is recorded on it, the pipeline stopping at a
//! manual job is pushed and ends the following, and `pipeline_wait` returns
//! as soon as something moved.

#[path = "support/mod.rs"]
mod support;
use support::{harness::harness, rows::session_row};

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tracon::broker::Credential;
use tracon::mcp::CallContext;

/// The pipeline the stub serves, and how often it was read.
#[derive(Default)]
struct Forge {
    status: String,
    jobs: Vec<(i64, &'static str, &'static str)>,
    reads: usize,
}

type Shared = Arc<Mutex<Forge>>;

fn set(forge: &Shared, status: &str, jobs: &[(i64, &'static str, &'static str)]) {
    let mut f = forge.lock().unwrap();
    f.status = status.into();
    f.jobs = jobs.to_vec();
}

async fn stub(forge: Shared) -> String {
    let app = Router::new()
        .route(
            "/api/v4/projects/42/repository/tags/{tag}",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, Json(json!({}))) }),
        )
        .route(
            "/api/v4/projects/42/pipeline",
            post(|| async {
                Json(json!({ "id": 9, "status": "created", "web_url": "https://gitlab.test/p/9" }))
            }),
        )
        .route(
            "/api/v4/projects/42/pipelines/{id}",
            get(|State(f): State<Shared>, Path(id): Path<i64>| async move {
                let mut f = f.lock().unwrap();
                f.reads += 1;
                Json(json!({
                    "id": id, "ref": "main", "sha": "abc", "status": f.status,
                    "source": "api", "web_url": "https://gitlab.test/p/9",
                }))
            }),
        )
        .route(
            "/api/v4/projects/42/pipelines/{id}/jobs",
            get(|State(f): State<Shared>| async move {
                let f = f.lock().unwrap();
                Json(Value::Array(
                    f.jobs
                        .iter()
                        .map(|(id, name, status)| {
                            json!({ "id": id, "name": name, "stage": "s", "status": status })
                        })
                        .collect(),
                ))
            }),
        )
        .with_state(forge);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    base
}

async fn rig() -> (support::harness::Harness, Shared, CallContext) {
    let h = harness().await;
    let forge = Shared::default();
    set(
        &forge,
        "created",
        &[(1, "build", "created"), (2, "deploy", "created")],
    );
    let base = stub(forge.clone()).await;
    h.tools.broker.write().unwrap().put(
        "glab",
        Credential {
            env: BTreeMap::from([
                ("GITLAB_TOKEN".to_string(), "t".to_string()),
                ("GITLAB_HOST".to_string(), base),
            ]),
            channels: vec!["personal".to_string()],
            ..Credential::default()
        },
    );
    // What a tool does is the subject here, not that running a pipeline is
    // asked: that stays the shipped policy's to say.
    *h.tools.policy.write() = toml::from_str(
        r#"
        version = 9
        [[rule]]
        id = "test-allow"
        verdict = "allow"
        reason = "Under test."
        kinds = ["tool"]
        matches = ["pipeline_run", "pipeline_wait"]
        "#,
    )
    .unwrap();
    let node = h.manager.node_id().to_string();
    h.store
        .insert_session(&session_row("s1", &node, "personal"))
        .unwrap();
    let ctx = CallContext::session("s1", "personal", node);
    (h, forge, ctx)
}

#[tokio::test]
async fn a_pipeline_the_session_ran_is_followed_until_it_stops() {
    let (h, forge, ctx) = rig().await;
    let ran = h
        .tools
        .call(
            &ctx,
            "pipeline_run",
            &json!({ "project": "42", "ref": "main" }),
        )
        .await
        .unwrap();
    assert_eq!(ran["id"], 9);
    assert_eq!(ran["following"]["pipeline_id"], 9);
    let follows = h.store.pipelines_followed("n1").unwrap();
    assert_eq!(follows.len(), 1);
    let id = follows[0].id.clone();
    assert_eq!(follows[0].session_id.as_deref(), Some("s1"));
    let first = h
        .store
        .latest_event_payload("s1", "pipeline_follow", &id)
        .unwrap()
        .expect("following is recorded on the session");
    assert_eq!(first["changes"][0], "following");

    // What it was when following began is the baseline.
    assert!(tracon::follow::tick_pipelines(&h.tools).await.is_empty());

    set(
        &forge,
        "running",
        &[(1, "build", "success"), (2, "deploy", "created")],
    );
    let moved = tracon::follow::tick_pipelines(&h.tools).await;
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].changes, ["`build` passed"]);
    assert!(!moved[0].pushed, "a job result is recorded, not pushed");

    set(
        &forge,
        "manual",
        &[(1, "build", "success"), (2, "deploy", "manual")],
    );
    let stopped = tracon::follow::tick_pipelines(&h.tools).await;
    assert_eq!(stopped[0].changes, ["waiting on `deploy` to be played"]);
    assert!(stopped[0].pushed);
    let recorded = h
        .store
        .latest_event_payload("s1", "pipeline_follow", &id)
        .unwrap()
        .unwrap();
    assert_eq!(recorded["status"], "manual");

    let reads = forge.lock().unwrap().reads;
    assert!(tracon::follow::tick_pipelines(&h.tools).await.is_empty());
    assert_eq!(
        forge.lock().unwrap().reads,
        reads,
        "a pipeline waiting on a person is not read again"
    );
}

#[tokio::test]
async fn pipeline_wait_returns_when_something_moved() {
    let (h, forge, ctx) = rig().await;
    set(&forge, "running", &[(1, "build", "running")]);
    let args = json!({ "project": "42", "pipeline_id": 9, "wait_secs": 0 });
    let first = h.tools.call(&ctx, "pipeline_wait", &args).await.unwrap();
    assert_eq!(first["changed"], false);
    assert_eq!(first["finished"], false);
    assert_eq!(first["jobs"][0]["name"], "build");
    let state = first["state"].as_str().unwrap().to_string();

    // A change between two calls is not missed: `since` is the baseline.
    set(&forge, "running", &[(1, "build", "failed")]);
    let started = std::time::Instant::now();
    let moved = h
        .tools
        .call(
            &ctx,
            "pipeline_wait",
            &json!({ "project": "42", "pipeline_id": 9, "since": state }),
        )
        .await
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(moved["changed"], true);
    assert_eq!(moved["jobs"][0]["status"], "failed");

    // A finished pipeline is returned at once.
    set(&forge, "failed", &[(1, "build", "failed")]);
    let done = h
        .tools
        .call(
            &ctx,
            "pipeline_wait",
            &json!({ "project": "42", "pipeline_id": 9 }),
        )
        .await
        .unwrap();
    assert_eq!(done["finished"], true);
    assert_eq!(done["status"], "failed");
}
