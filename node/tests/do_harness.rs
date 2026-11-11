//! The Harness Runtime client against a loopback stand-in for the API that
//! answers with recorded bodies and keeps every request it was sent.
//!
//! Fixtures in `fixtures/do_harness/`, by provenance:
//! - Recorded from the live API with `doctl --trace` (2026-11-02), with ids,
//!   the account's user id, addresses and balance replaced: `session_create`,
//!   `session_get`, `sessions_list`, `configs_list`, `templates_list`,
//!   `checkpoints_list_empty`, `sizes`, `balance`; `fork` from doctl's
//!   output of a real fork; `error_unknown_field` and `error_not_found` are
//!   the raw bodies of a refused `region` manifest and an unknown session.
//! - Written from the request and response types of godo v1.233.0 (the
//!   library doctl calls the API with) for the calls not recorded: the rest.
//!   `checkpoints_list_51gb` has the 51 GB size an implicit checkpoint was
//!   recorded at, once as a number and once as a string.

#[path = "support/mod.rs"]
mod support;

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use serde_json::{json, Value};

use tracon::do_harness::cleanup::{self, CONFIG_MIN_AGE_MS};
use tracon::do_harness::{
    BuildStatus, CheckpointKind, CheckpointStatus, Client, DoError, Egress, ExecRequest, Manifest,
    SessionFilter, SessionStatus, TemplateStatus,
};

const TOKEN: &str = "dop_v1_fixture";
const GB51: u64 = 54_760_833_024;
const SESSION: &str = "0190c0de-0000-7000-8000-000000000001";

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/do_harness/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    query: Option<String>,
    authorization: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

impl Seen {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

/// A canned answer: status, body, and extra headers.
type Canned = (u16, String, Vec<(&'static str, String)>);

#[derive(Default)]
struct Fake {
    /// `"METHOD /path"` or `"METHOD /path?query"`, the latter preferred.
    routes: HashMap<String, Canned>,
    seen: Mutex<Vec<Seen>>,
}

struct Api {
    fake: Arc<Fake>,
    client: Client,
}

impl Api {
    fn seen(&self) -> Vec<Seen> {
        self.fake.seen.lock().clone()
    }

    fn only(&self) -> Seen {
        let seen = self.seen();
        assert_eq!(seen.len(), 1, "{seen:#?}");
        seen.into_iter().next().unwrap()
    }
}

struct Routes(Fake);

impl Routes {
    fn new() -> Self {
        Self(Fake::default())
    }

    fn on(mut self, route: &str, status: u16, body: impl Into<String>) -> Self {
        self.0
            .routes
            .insert(route.to_string(), (status, body.into(), Vec::new()));
        self
    }

    fn with_header(mut self, route: &str, name: &'static str, value: &str) -> Self {
        self.0
            .routes
            .get_mut(route)
            .expect("route declared first")
            .2
            .push((name, value.to_string()));
        self
    }

    async fn serve(self) -> Api {
        let fake = Arc::new(self.0);
        let app = axum::Router::new()
            .fallback(answer)
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::with_base_url(&format!("http://{addr}"), TOKEN).unwrap();
        Api { fake, client }
    }
}

async fn answer(
    State(fake): State<Arc<Fake>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    fake.seen.lock().push(Seen {
        method: method.to_string(),
        path: uri.path().to_string(),
        query: uri.query().map(str::to_string),
        authorization: header("authorization"),
        content_type: header("content-type"),
        body: body.to_vec(),
    });
    let with_query = format!("{method} {uri}");
    let without = format!("{method} {}", uri.path());
    let Some((status, body, extra)) = fake
        .routes
        .get(&with_query)
        .or_else(|| fake.routes.get(&without))
    else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            format!("no route for {with_query}"),
        )
            .into_response();
    };
    let mut response = (
        StatusCode::from_u16(*status).unwrap(),
        [("content-type", "application/json")],
        body.clone(),
    )
        .into_response();
    for (name, value) in extra {
        response.headers_mut().insert(*name, value.parse().unwrap());
    }
    response
}

fn spike_manifest() -> Manifest {
    Manifest {
        size: Some("mars-1vcpu-1gb".into()),
        keep_warm: true,
        egress: Some(Egress::Rules {
            allow_hosts: Vec::new(),
            allow_ips: vec!["192.0.2.10".into()],
            vpc_uuid: None,
            subnet_uuid: None,
        }),
        ..Manifest::bare("tracon-spike-l1")
    }
}

// ---- sessions ----

#[tokio::test]
async fn create_sends_the_manifest_and_reads_the_session_back() {
    let api = Routes::new()
        .on(
            "POST /v2/harness-runtime/sessions",
            200,
            fixture("session_create"),
        )
        .with_header(
            "POST /v2/harness-runtime/sessions",
            "location",
            "https://api-ric1.digitalocean.com/v2/agents/sessions/x",
        )
        .serve()
        .await;
    let session = api.client.create_session(&spike_manifest()).await.unwrap();

    let sent = api.only();
    assert_eq!(sent.path, "/v2/harness-runtime/sessions");
    assert_eq!(sent.authorization.as_deref(), Some("Bearer dop_v1_fixture"));
    assert_eq!(sent.content_type.as_deref(), Some("application/x-yaml"));
    let expected: Value = serde_json::from_str(&fixture("session_create_request")).unwrap();
    assert_eq!(sent.json(), expected);

    assert_eq!(session.session_id, SESSION);
    assert_eq!(session.status, SessionStatus::Ready);
    assert_eq!(session.agent_kind, "AGENT_KIND_NONE");
    assert!(session.keep_warm);
    assert_eq!(
        session.config_id.as_deref(),
        Some("0190c0de-0000-7000-8000-000000000003")
    );
    assert_eq!(session.warnings.len(), 1);
    assert!(session.location.as_deref().unwrap().contains("api-ric1"));
}

#[tokio::test]
async fn a_refused_manifest_names_the_unknown_field() {
    let api = Routes::new()
        .on(
            "POST /v2/harness-runtime/sessions",
            400,
            fixture("error_unknown_field"),
        )
        .serve()
        .await;
    let error = api
        .client
        .create_session(&Manifest::bare("x"))
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(400));
    assert_eq!(error.unknown_field(), Some("region"));
    let DoError::Api { id, request_id, .. } = &error else {
        panic!("{error:?}")
    };
    assert_eq!(id.as_deref(), Some("bad_request"));
    assert!(request_id.is_some());
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn create_from_config_posts_json() {
    let api = Routes::new()
        .on(
            "POST /v2/harness-runtime/sessions",
            200,
            fixture("session_create"),
        )
        .serve()
        .await;
    api.client
        .create_session_from_config("tracon-run-1", "0190c0de-0000-7000-8000-0000000000a5")
        .await
        .unwrap();
    let sent = api.only();
    assert_eq!(sent.content_type.as_deref(), Some("application/json"));
    assert_eq!(
        sent.json(),
        json!({ "name": "tracon-run-1", "config_id": "0190c0de-0000-7000-8000-0000000000a5" })
    );
}

#[tokio::test]
async fn get_list_and_find_sessions() {
    let api = Routes::new()
        .on(
            &format!("GET /v2/harness-runtime/sessions/{SESSION}"),
            200,
            fixture("session_get"),
        )
        .on(
            "GET /v2/harness-runtime/sessions",
            200,
            fixture("sessions_list"),
        )
        .serve()
        .await;
    let session = api.client.get_session(SESSION).await.unwrap();
    assert_eq!(session.name, "tracon-spike-l1");
    assert!(session.warnings.is_empty());

    let all = api
        .client
        .list_sessions(&SessionFilter::default())
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0], session);

    let found = api.client.find_session("tracon-spike-l1").await.unwrap();
    assert_eq!(found.unwrap().session_id, SESSION);
    assert!(api.client.find_session("other").await.unwrap().is_none());

    let queries: Vec<_> = api.seen().into_iter().map(|s| s.query).collect();
    assert_eq!(
        queries,
        [
            None,
            None,
            Some("name=tracon-spike-l1".into()),
            Some("name=other".into())
        ]
    );
}

#[tokio::test]
async fn listing_follows_page_tokens_and_tolerates_unknown_statuses() {
    let page1 = json!({
        "sessions": [{ "session_id": "a", "status": "SESSION_STATUS_HIBERNATING", "novel": 1 }],
        "next_page_token": "p2"
    });
    let page2 = json!({ "sessions": [{ "session_id": "b", "status": "SESSION_STATUS_PAUSED",
                        "pause_reason": "zero_balance" }], "next_page_token": "" });
    let api = Routes::new()
        .on("GET /v2/harness-runtime/sessions", 200, page1.to_string())
        .on(
            "GET /v2/harness-runtime/sessions?page_token=p2",
            200,
            page2.to_string(),
        )
        .serve()
        .await;
    let sessions = api
        .client
        .list_sessions(&SessionFilter::default())
        .await
        .unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(
        sessions[0].status,
        SessionStatus::Other("SESSION_STATUS_HIBERNATING".into())
    );
    assert!(!sessions[0].status.is_terminal());
    assert_eq!(sessions[1].pause_reason.as_deref(), Some("zero_balance"));
}

#[tokio::test]
async fn a_repeated_page_token_is_an_error_not_a_loop() {
    let page = json!({ "sessions": [], "next_page_token": "same" });
    let api = Routes::new()
        .on("GET /v2/harness-runtime/sessions", 200, page.to_string())
        .serve()
        .await;
    let error = api
        .client
        .list_sessions(&SessionFilter::default())
        .await
        .unwrap_err();
    assert!(matches!(error, DoError::Decode { .. }), "{error:?}");
}

#[tokio::test]
async fn delete_pause_and_resume() {
    let by_id = format!("/v2/harness-runtime/sessions/{SESSION}");
    let api = Routes::new()
        .on(&format!("DELETE {by_id}"), 204, "")
        .on(&format!("POST {by_id}/pause"), 200, "{}")
        .on(&format!("POST {by_id}/resume"), 200, "{}")
        .on(
            "DELETE /v2/harness-runtime/sessions/gone",
            404,
            fixture("error_not_found"),
        )
        .on(
            "GET /v2/harness-runtime/sessions/gone",
            404,
            fixture("error_not_found"),
        )
        .serve()
        .await;
    api.client.delete_session(SESSION).await.unwrap();
    api.client.pause_session(SESSION).await.unwrap();
    api.client.resume_session(SESSION).await.unwrap();
    // Removing what is already gone succeeds; asking for it does not.
    api.client.delete_session("gone").await.unwrap();
    let error = api.client.get_session("gone").await.unwrap_err();
    assert!(error.is_not_found());
    assert!(error.to_string().contains("session not found"));

    let seen = api.seen();
    let calls: Vec<_> = seen
        .iter()
        .map(|s| format!("{} {}", s.method, s.path))
        .collect();
    assert_eq!(
        calls[..3],
        [
            format!("DELETE {by_id}"),
            format!("POST {by_id}/pause"),
            format!("POST {by_id}/resume"),
        ]
    );
    assert_eq!(seen[1].json(), json!({}));
}

#[tokio::test]
async fn fork_sends_the_checkpoint_and_count() {
    let api = Routes::new()
        .on(
            &format!("POST /v2/harness-runtime/sessions/{SESSION}/fork"),
            200,
            fixture("fork"),
        )
        .serve()
        .await;
    let children = api
        .client
        .fork_session(SESSION, Some("0190c0de-0000-7000-8000-0000000000c1"), 2)
        .await
        .unwrap();
    assert_eq!(
        api.only().json(),
        json!({ "count": 2, "from_checkpoint_id": "0190c0de-0000-7000-8000-0000000000c1" })
    );
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].parent_session_id.as_deref(), Some(SESSION));
    assert!(children[0].fork_id.is_some());

    for count in [0, 5] {
        assert!(matches!(
            api.client.fork_session(SESSION, None, count).await,
            Err(DoError::Invalid(_))
        ));
    }
    assert_eq!(
        api.seen().len(),
        1,
        "an invalid fork is refused before sending"
    );
}

#[tokio::test]
async fn exec_runs_one_command() {
    let api = Routes::new()
        .on(
            &format!("POST /v2/harness-runtime/sessions/{SESSION}/sandbox/exec"),
            200,
            fixture("exec"),
        )
        .serve()
        .await;
    let request = ExecRequest {
        argv: vec!["sh".into(), "-c".into(), "echo hello".into()],
        workdir: Some("/workspace".into()),
        timeout_seconds: Some(30),
    };
    let output = api.client.exec(SESSION, &request).await.unwrap();
    assert_eq!(
        api.only().json(),
        json!({ "argv": ["sh", "-c", "echo hello"], "workdir": "/workspace", "timeout_seconds": 30 })
    );
    assert_eq!(output.exit_code, 3);
    assert_eq!(output.stdout, "hello\n");
    assert!(matches!(
        api.client.exec(SESSION, &ExecRequest::default()).await,
        Err(DoError::Invalid(_))
    ));
}

#[tokio::test]
async fn sizes_and_balance() {
    let api = Routes::new()
        .on(
            "GET /v2/harness-runtime/sessions/sandbox/sizes",
            200,
            fixture("sizes"),
        )
        .on(
            "GET /v2/customers/my/prepayment_config",
            200,
            fixture("balance"),
        )
        .serve()
        .await;
    let sizes = api.client.sizes().await.unwrap();
    assert_eq!(sizes.len(), 5);
    assert_eq!(sizes[0].slug, "mars-1vcpu-1gb");
    assert_eq!(sizes[0].memory_mb, 1024);
    assert_eq!(sizes[4].vcpus, 16);
    let balance = api.client.balance().await.unwrap();
    assert_eq!(balance.balance.as_deref(), Some("10.00"));
    assert!(balance.eligible);
    assert!(!balance.blocked);
}

// ---- agent configs ----

#[tokio::test]
async fn configs_list_create_delete_and_their_sessions() {
    let id = "0190c0de-0000-7000-8000-000000000003";
    let api = Routes::new()
        .on(
            "GET /v2/harness-runtime/configs",
            200,
            fixture("configs_list"),
        )
        .on(
            "POST /v2/harness-runtime/configs",
            200,
            fixture("config_create"),
        )
        .on(
            &format!("DELETE /v2/harness-runtime/configs/{id}"),
            200,
            "{}",
        )
        .on(
            &format!("GET /v2/harness-runtime/configs/{id}/sessions"),
            200,
            fixture("sessions_list"),
        )
        .serve()
        .await;
    let configs = api.client.list_configs().await.unwrap();
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0].id, id);
    assert_eq!(
        configs[0].agentspec_schema_version,
        "agents.digitalocean.com/flat.v1"
    );

    let manifest = Manifest {
        size: Some("mars-2vcpu-4gb".into()),
        ..Manifest::bare("tracon-prepared")
    };
    let config = api
        .client
        .create_config("tracon-prepared", &manifest)
        .await
        .unwrap();
    assert_eq!(config.summary.name, "tracon-prepared");
    assert_eq!(config.manifest["size"], "mars-2vcpu-4gb");

    api.client.delete_config(id).await.unwrap();
    let sessions = api.client.list_config_sessions(id).await.unwrap();
    assert_eq!(sessions[0].config_id.as_deref(), Some(id));

    let seen = api.seen();
    let create = seen[1].json();
    assert_eq!(create["name"], "tracon-prepared");
    let stored: Value = serde_json::from_str(create["manifest_yaml"].as_str().unwrap()).unwrap();
    assert_eq!(
        stored,
        json!({ "name": "tracon-prepared", "harness": "none", "size": "mars-2vcpu-4gb" })
    );
    assert_eq!(seen[2].method, "DELETE");
}

#[tokio::test]
async fn leftover_configs_are_deleted_and_nothing_else() {
    let api = Routes::new()
        .on(
            "GET /v2/harness-runtime/configs",
            200,
            fixture("configs_list_gc"),
        )
        .on(
            "GET /v2/harness-runtime/sessions",
            200,
            fixture("sessions_list_gc"),
        )
        .on(
            "DELETE /v2/harness-runtime/configs/0190c0de-0000-7000-8000-0000000000a2",
            204,
            "",
        )
        .on(
            "DELETE /v2/harness-runtime/configs/0190c0de-0000-7000-8000-0000000000a3",
            204,
            "",
        )
        .serve()
        .await;
    let now = tracon::store::now_ms();
    let report =
        cleanup::delete_leftover_configs(&api.client, |c| c.name.starts_with("tracon-"), now)
            .await
            .unwrap();

    // a1 has a paused session on it, a4 was not made by the node.
    let deleted: Vec<_> = report.deleted.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(deleted, ["tracon-removed", "tracon-destroyed"]);
    assert!(report.failed.is_empty());
    let deletes: Vec<_> = api
        .seen()
        .into_iter()
        .filter(|s| s.method == "DELETE")
        .map(|s| s.path)
        .collect();
    assert_eq!(
        deletes,
        [
            "/v2/harness-runtime/configs/0190c0de-0000-7000-8000-0000000000a2",
            "/v2/harness-runtime/configs/0190c0de-0000-7000-8000-0000000000a3",
        ]
    );
}

#[tokio::test]
async fn a_config_made_moments_ago_is_spared() {
    let api = Routes::new()
        .on(
            "GET /v2/harness-runtime/configs",
            200,
            fixture("configs_list_gc"),
        )
        .on(
            "GET /v2/harness-runtime/sessions",
            200,
            fixture("sessions_list_gc"),
        )
        .on(
            "DELETE /v2/harness-runtime/configs/0190c0de-0000-7000-8000-0000000000a2",
            204,
            "",
        )
        .serve()
        .await;
    // Just inside the grace period of the newer orphan (created
    // 2026-11-02T01:39:00Z); the older one is past it.
    let created: i64 = 1_793_583_540_000;
    let report = cleanup::delete_leftover_configs(
        &api.client,
        |c| c.name.starts_with("tracon-"),
        created + CONFIG_MIN_AGE_MS - 1,
    )
    .await
    .unwrap();
    let deleted: Vec<_> = report.deleted.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(deleted, ["tracon-removed"]);
}

// ---- checkpoints ----

#[tokio::test]
async fn checkpoints_create_list_get_delete() {
    let base = format!("/v2/harness-runtime/sessions/{SESSION}/checkpoints");
    let id = "0190c0de-0000-7000-8000-0000000000c1";
    let api = Routes::new()
        .on(&format!("POST {base}"), 200, fixture("checkpoint_create"))
        .on(
            &format!("GET {base}"),
            200,
            fixture("checkpoints_list_empty"),
        )
        .on(
            &format!("GET {base}/{id}"),
            200,
            fixture("checkpoint_create"),
        )
        .on(
            &format!("DELETE {base}/{id}"),
            200,
            fixture("checkpoint_delete"),
        )
        .serve()
        .await;
    let created = api
        .client
        .create_checkpoint(SESSION, Some("prepared"))
        .await
        .unwrap();
    assert_eq!(created.status, CheckpointStatus::Ready);
    assert_eq!(created.kind, CheckpointKind::Explicit);
    assert_eq!(created.size_bytes, GB51);
    assert!(api
        .client
        .list_checkpoints(SESSION)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        api.client
            .get_checkpoint(SESSION, id)
            .await
            .unwrap()
            .checkpoint_id,
        id
    );
    api.client.delete_checkpoint(SESSION, id).await.unwrap();
    assert_eq!(api.seen()[0].json(), json!({ "label": "prepared" }));
}

#[tokio::test]
async fn a_51_gb_checkpoint_size_reads_as_a_number_or_a_string() {
    let api = Routes::new()
        .on(
            &format!("GET /v2/harness-runtime/sessions/{SESSION}/checkpoints"),
            200,
            fixture("checkpoints_list_51gb"),
        )
        .serve()
        .await;
    let checkpoints = api.client.list_checkpoints(SESSION).await.unwrap();
    let sizes: Vec<u64> = checkpoints.iter().map(|c| c.size_bytes).collect();
    assert_eq!(sizes, [GB51, GB51, 0]);
    assert!(GB51 > u64::from(u32::MAX));
    assert_eq!(checkpoints[0].kind, CheckpointKind::Implicit);
    assert_eq!(
        checkpoints[2].error_message.as_deref(),
        Some("capture failed")
    );
}

#[tokio::test]
async fn pruning_keeps_the_newest_ready_checkpoint_per_environment() {
    let base = format!("/v2/harness-runtime/sessions/{SESSION}/checkpoints");
    let api = Routes::new()
        .on(
            &format!("GET {base}"),
            200,
            fixture("checkpoints_list_51gb"),
        )
        .on(
            &format!("DELETE {base}/0190c0de-0000-7000-8000-0000000000c1"),
            200,
            fixture("checkpoint_delete"),
        )
        // The failed checkpoint's deletion fails too: reported, not fatal.
        .on(
            &format!("DELETE {base}/0190c0de-0000-7000-8000-0000000000c2"),
            500,
            r#"{"id":"server_error","message":"try again"}"#,
        )
        .serve()
        .await;
    let prepared = tracon::do_harness::Session {
        session_id: SESSION.into(),
        name: "tracon-prep-hdr".into(),
        status: SessionStatus::Paused,
        ..Default::default()
    };
    let unrelated = tracon::do_harness::Session {
        session_id: "someone-elses".into(),
        name: "reviewer".into(),
        ..Default::default()
    };
    let report = cleanup::prune_checkpoints(&api.client, &[prepared, unrelated], |s| {
        s.name.strip_prefix("tracon-prep-").map(str::to_string)
    })
    .await
    .unwrap();

    let ids = |v: &[tracon::do_harness::Checkpoint]| {
        v.iter()
            .map(|c| c.checkpoint_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&report.kept), ["0190c0de-0000-7000-8000-0000000000c3"]);
    assert_eq!(
        ids(&report.deleted),
        ["0190c0de-0000-7000-8000-0000000000c1"]
    );
    assert_eq!(report.freed_bytes, GB51);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "0190c0de-0000-7000-8000-0000000000c2");
    assert!(
        api.seen().iter().all(|s| !s.path.contains("someone-elses")),
        "a session outside every environment is not touched"
    );
}

// ---- templates ----

#[tokio::test]
async fn templates_and_their_builds() {
    let id = "0190c0de-0000-7000-8000-0000000000d1";
    let api = Routes::new()
        .on(
            "GET /v2/harness-runtime/templates",
            200,
            fixture("templates_list"),
        )
        .on(
            "POST /v2/harness-runtime/templates",
            200,
            fixture("template_create"),
        )
        .on(
            &format!("GET /v2/harness-runtime/templates/{id}"),
            200,
            fixture("template_get"),
        )
        .on(
            &format!("PUT /v2/harness-runtime/templates/{id}"),
            200,
            fixture("template_create"),
        )
        .on(
            &format!("DELETE /v2/harness-runtime/templates/{id}"),
            200,
            fixture("template_delete"),
        )
        .on(
            &format!("GET /v2/harness-runtime/templates/{id}/builds"),
            200,
            fixture("template_builds_list"),
        )
        .on(
            &format!("GET /v2/harness-runtime/templates/{id}/builds/b2"),
            200,
            fixture("template_build_get"),
        )
        .serve()
        .await;
    assert!(api.client.list_templates().await.unwrap().is_empty());

    let created = api
        .client
        .create_template(
            "tracon-hdr-rebase-test",
            "coding-base",
            "ghcr.io/example/base:1",
        )
        .await
        .unwrap();
    assert_eq!(created.status, TemplateStatus::Pending);
    let image = created.spec.unwrap().image.unwrap();
    assert!(image.digest.unwrap().starts_with("sha256:"));

    let building = api.client.get_template(id).await.unwrap();
    assert_eq!(building.status, TemplateStatus::Building);

    api.client
        .update_template(id, None, Some("ghcr.io/example/base:2"))
        .await
        .unwrap();
    assert!(matches!(
        api.client.update_template(id, None, None).await,
        Err(DoError::Invalid(_))
    ));
    api.client.delete_template(id).await.unwrap();

    let builds = api.client.list_template_builds(id).await.unwrap();
    assert_eq!(builds[0].status, BuildStatus::Succeeded);
    let failed = api.client.get_template_build(id, "b2").await.unwrap();
    assert_eq!(failed.status, BuildStatus::Failed);
    assert!(failed.error.unwrap().contains("unauthorized"));

    let seen = api.seen();
    assert_eq!(
        seen[1].json(),
        json!({ "name": "tracon-hdr-rebase-test", "base_template": "coding-base",
                "source_oci_ref": "ghcr.io/example/base:1" })
    );
    assert_eq!(seen[3].method, "PUT");
    assert_eq!(
        seen[3].json(),
        json!({ "source_oci_ref": "ghcr.io/example/base:2" })
    );
}

// ---- transport ----

#[tokio::test]
async fn a_redirect_is_not_followed_with_the_token() {
    let api = Routes::new()
        .on("GET /v2/harness-runtime/templates", 302, "")
        .with_header(
            "GET /v2/harness-runtime/templates",
            "location",
            "/elsewhere",
        )
        .serve()
        .await;
    let error = api.client.list_templates().await.unwrap_err();
    assert_eq!(error.status(), Some(302));
    assert_eq!(api.seen().len(), 1);
}

#[tokio::test]
async fn ids_that_would_change_the_route_are_refused_before_sending() {
    let api = Routes::new().serve().await;
    for id in ["../configs", "a/b", "x?y=1", ""] {
        assert!(matches!(
            api.client.get_session(id).await,
            Err(DoError::Invalid(_))
        ));
    }
    assert!(api.seen().is_empty());
}
