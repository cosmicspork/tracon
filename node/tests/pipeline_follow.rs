//! Following a pipeline an agent started: `pipeline_run` subscribes its
//! session, each job result is recorded on it, the pipeline stopping at a
//! manual job is pushed and ends the following, and `pipeline_wait` returns
//! as soon as something moved.

#[path = "support/mod.rs"]
mod support;
use support::{
    harness::{harness, harness_with},
    rows::session_row,
};

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
            "/api/v4/projects/42/pipelines/{id}/variables",
            get(|| async { Json(json!([{ "key": "N", "value": "2" }])) }),
        )
        .route(
            "/api/v4/projects/42/pipelines/{id}/retry",
            post(|State(f): State<Shared>, Path(id): Path<i64>| async move {
                let mut f = f.lock().unwrap();
                // GitLab retries in the request: the failed job is
                // replaced by a new one, pending.
                f.status = "pending".into();
                f.jobs = vec![(3, "build", "pending")];
                Json(json!({ "id": id, "status": "pending", "web_url": "https://gitlab.test/p/9" }))
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
        matches = ["pipeline_run", "pipeline_retry", "pipeline_wait"]
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
async fn a_pipeline_the_session_retried_is_followed_from_the_retry() {
    let (h, forge, ctx) = rig().await;
    set(&forge, "failed", &[(1, "build", "failed")]);
    let retried = h
        .tools
        .call(
            &ctx,
            "pipeline_retry",
            &json!({ "project": "42", "pipeline_id": 9 }),
        )
        .await
        .unwrap();
    assert_eq!(retried["id"], 9);
    assert_eq!(retried["following"]["pipeline_id"], 9);
    let follows = h.store.pipelines_followed("n1").unwrap();
    assert_eq!(follows.len(), 1);
    assert_eq!(follows[0].started_by, "pipeline_retry");

    // The failure it retried is the baseline, not news.
    assert!(tracon::follow::tick_pipelines(&h.tools).await.is_empty());
    set(&forge, "success", &[(3, "build", "success")]);
    let done = tracon::follow::tick_pipelines(&h.tools).await;
    assert_eq!(done[0].changes, ["`build` passed", "pipeline passed"]);
    assert!(done[0].pushed);
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

/// The GitHub run the stub serves: its attempt, status and conclusion, and
/// its jobs as (id, name, status, conclusion).
#[derive(Default)]
struct Run {
    attempt: i64,
    status: &'static str,
    conclusion: Option<&'static str>,
    jobs: Vec<(i64, &'static str, &'static str, Option<&'static str>)>,
}

type SharedRun = Arc<Mutex<Run>>;

async fn github_stub(run: SharedRun) -> String {
    let app = Router::new()
        .route(
            "/repos/o/n/actions/runs/{id}",
            get(
                |State(r): State<SharedRun>, Path(id): Path<i64>| async move {
                    let r = r.lock().unwrap();
                    Json(json!({
                        "id": id, "name": "ci", "status": r.status, "conclusion": r.conclusion,
                        "run_attempt": r.attempt, "head_branch": "main", "head_sha": "abc",
                        "html_url": "https://github.test/o/n/actions/runs/900",
                    }))
                },
            ),
        )
        .route(
            "/repos/o/n/actions/runs/{id}/jobs",
            get(|State(r): State<SharedRun>| async move {
                let r = r.lock().unwrap();
                let jobs: Vec<Value> = r
                    .jobs
                    .iter()
                    .map(|(id, name, status, conclusion)| {
                        json!({ "id": id, "name": name, "status": status,
                                "conclusion": conclusion, "steps": [] })
                    })
                    .collect();
                Json(json!({ "total_count": jobs.len(), "jobs": jobs }))
            }),
        )
        .route(
            "/repos/o/n/branches/main",
            get(|| async { Json(json!({ "name": "main" })) }),
        )
        .route(
            "/repos/o/n/actions/workflows/ci.yml/runs",
            get(|| async { Json(json!({ "workflow_runs": [] })) }),
        )
        .route(
            "/repos/o/n/actions/workflows/ci.yml/dispatches",
            post(|| async {
                Json(json!({ "workflow_run_id": 901,
                             "html_url": "https://github.test/o/n/actions/runs/901" }))
            }),
        )
        .route(
            "/repos/o/n/actions/runs/{id}/rerun-failed-jobs",
            post(|| async { (axum::http::StatusCode::CREATED, Json(json!({}))) }),
        )
        .with_state(run);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    base
}

#[tokio::test]
async fn a_run_the_session_reran_is_followed_from_the_attempt_it_started() {
    let h = harness().await;
    let run = SharedRun::default();
    *run.lock().unwrap() = Run {
        attempt: 1,
        status: "completed",
        conclusion: Some("failure"),
        jobs: vec![(31, "test", "completed", Some("failure"))],
    };
    let base = github_stub(run.clone()).await;
    h.tools.broker.write().unwrap().put(
        "gh",
        Credential {
            env: BTreeMap::from([
                ("GH_TOKEN".to_string(), "t".to_string()),
                ("GITHUB_API".to_string(), base),
            ]),
            channels: vec!["personal".to_string()],
            ..Credential::default()
        },
    );
    *h.tools.policy.write() = toml::from_str(
        r#"
        version = 9
        [[rule]]
        id = "test-allow"
        verdict = "allow"
        reason = "Under test."
        kinds = ["tool"]
        matches = ["run_rerun", "run_wait"]
        "#,
    )
    .unwrap();
    let node = h.manager.node_id().to_string();
    h.store
        .insert_session(&session_row("s1", &node, "personal"))
        .unwrap();
    let ctx = CallContext::session("s1", "personal", node);

    let rerun = h
        .tools
        .call(&ctx, "run_rerun", &json!({ "repo": "o/n", "run_id": 900 }))
        .await
        .unwrap();
    assert_eq!(rerun["attempt"], 2);
    assert_eq!(rerun["following"]["run_id"], 900);
    let follows = h.store.pipelines_followed("n1").unwrap();
    assert_eq!(follows.len(), 1);
    assert_eq!(follows[0].provider, "github");

    // GitHub has not begun the new attempt: the old one's failure is not
    // this one's, and a wait for it does not return finished.
    assert!(tracon::follow::tick_pipelines(&h.tools).await.is_empty());
    let waited = h
        .tools
        .call(
            &ctx,
            "run_wait",
            &json!({ "repo": "o/n", "run_id": 900, "attempt": 2, "wait_secs": 0 }),
        )
        .await
        .unwrap();
    assert_eq!(waited["finished"], false);
    let state = waited["state"].as_str().unwrap().to_string();

    *run.lock().unwrap() = Run {
        attempt: 2,
        status: "in_progress",
        conclusion: None,
        jobs: vec![(41, "test", "in_progress", None)],
    };
    let moved = h
        .tools
        .call(
            &ctx,
            "run_wait",
            &json!({ "repo": "o/n", "run_id": 900, "attempt": 2, "since": state }),
        )
        .await
        .unwrap();
    assert_eq!(moved["changed"], true);
    assert_eq!(moved["run"]["attempt"], 2);
    assert!(tracon::follow::tick_pipelines(&h.tools).await.is_empty());

    *run.lock().unwrap() = Run {
        attempt: 2,
        status: "completed",
        conclusion: Some("success"),
        jobs: vec![(41, "test", "completed", Some("success"))],
    };
    let done = tracon::follow::tick_pipelines(&h.tools).await;
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].changes, ["`test` passed", "run passed"]);
    assert!(done[0].pushed);
    assert!(
        h.store.pipelines_followed("n1").unwrap().is_empty(),
        "a finished run is no longer followed"
    );
}

#[tokio::test]
async fn a_run_the_session_dispatched_is_followed() {
    let h = harness_with(tracon::config::Config {
        github: tracon::config::Github {
            dispatch: [("o/n".to_string(), vec!["ci.yml".to_string()])].into(),
        },
        ..Default::default()
    })
    .await;
    let run = SharedRun::default();
    *run.lock().unwrap() = Run {
        attempt: 1,
        status: "queued",
        conclusion: None,
        jobs: vec![],
    };
    let base = github_stub(run.clone()).await;
    h.tools.broker.write().unwrap().put(
        "gh",
        Credential {
            env: BTreeMap::from([
                ("GH_TOKEN".to_string(), "t".to_string()),
                ("GITHUB_API".to_string(), base),
            ]),
            channels: vec!["personal".to_string()],
            ..Credential::default()
        },
    );
    *h.tools.policy.write() = toml::from_str(
        r#"
        version = 9
        [[rule]]
        id = "test-allow"
        verdict = "allow"
        reason = "Under test."
        kinds = ["tool"]
        matches = ["run_dispatch"]
        "#,
    )
    .unwrap();
    let node = h.manager.node_id().to_string();
    h.store
        .insert_session(&session_row("s1", &node, "personal"))
        .unwrap();
    let ctx = CallContext::session("s1", "personal", node);

    let started = h
        .tools
        .call(
            &ctx,
            "run_dispatch",
            &json!({ "repo": "o/n", "workflow": "ci.yml", "ref": "main" }),
        )
        .await
        .unwrap();
    assert_eq!(started["run_id"], 901);
    assert_eq!(started["following"]["run_id"], 901);
    let follows = h.store.pipelines_followed("n1").unwrap();
    assert_eq!(follows.len(), 1);
    assert_eq!(
        (follows[0].provider.as_str(), follows[0].started_by.as_str()),
        ("github", "run_dispatch")
    );

    *run.lock().unwrap() = Run {
        attempt: 1,
        status: "completed",
        conclusion: Some("failure"),
        jobs: vec![(51, "renovate", "completed", Some("failure"))],
    };
    let done = tracon::follow::tick_pipelines(&h.tools).await;
    assert_eq!(done[0].changes, ["`renovate` failed", "run failed"]);
    assert!(h.store.pipelines_followed("n1").unwrap().is_empty());
}
