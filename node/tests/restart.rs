//! Changing a running node: the keys that apply at once, the ones that wait
//! for a restart and say so, and restarting it without cutting work off.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use support::fake::FakeAdapter;
use support::rows::session_row;
use tracon::{
    broker::Broker,
    config::{Config, KeyPath, Op},
    http::{
        api::AppState,
        auth::{self, AuthState},
    },
    mcp::Tools,
    session::{Manager, NewSession, SessionError},
    store::{SessionPatch, Store},
    stream::Bus,
};

const LOCAL: &str = "127.0.0.1:5000";
const TOKEN: &str = "trc1.restart-test";

struct Node {
    app: axum::Router,
    state: AppState,
    supervisor: Arc<Fake>,
}

/// Stands in for systemd, launchd or the kubelet: counts the restarts it was
/// asked for, or refuses to exist.
#[derive(Default)]
struct Fake {
    restarts: AtomicUsize,
    missing: bool,
}

impl tracon::restart::Supervisor for Fake {
    fn mechanism(&self) -> Result<&'static str, String> {
        match self.missing {
            true => Err("nothing supervises this node".into()),
            false => Ok("fake"),
        }
    }

    fn restart(&self) -> Result<(), String> {
        self.restarts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn node_with(supervisor: Fake) -> Node {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let cfg = Arc::new(Config::default());
    let tools = Arc::new(Tools {
        broker: Broker::default().shared(),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg.clone(),
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let supervisor = Arc::new(supervisor);
    manager.restart().set_supervisor(supervisor.clone());
    manager.restart().set_poll(Duration::from_millis(10));
    let state = AppState {
        manager,
        cfg,
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: Arc::new(AuthState::new("127.0.0.1".into(), Some(auth::hash(TOKEN)))),
        enroll: Default::default(),
    };
    let app = tracon::http::router(state.clone()).layer(axum::middleware::from_fn_with_state(
        state.clone(),
        auth::guard,
    ));
    Node {
        app,
        state,
        supervisor,
    }
}

fn node() -> Node {
    node_with(Fake::default())
}

async fn call(n: &Node, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:7420")
        .header("authorization", format!("Bearer {TOKEN}"));
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let mut req = b
        .body(match body {
            Some(v) => Body::from(v.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let addr: SocketAddr = LOCAL.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    let res = n.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn file_hash() -> String {
    tracon::config::edit::file_hash(&Config::config_path()).unwrap()
}

fn file_text() -> String {
    std::fs::read_to_string(Config::config_path()).unwrap_or_default()
}

/// The node as `serve` leaves it: started from the file as it is now.
fn boot_from_file(n: &Node) {
    n.state.cfg.set_boot_text(&file_text());
}

async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

fn spec() -> NewSession {
    NewSession {
        channel: "personal".into(),
        repo_path: "/r".into(),
        branch: None,
        base: None,
        work_item_id: None,
        model: "m".into(),
        budget_tokens: None,
        initial_prompt: None,
        node_id: None,
        phase: Default::default(),
        review_id: None,
        base_sha: None,
        workspace_id: None,
        parent_session: None,
        continued_from: None,
        harness: None,
        on_exhaustion: None,
        told_reviews: Vec::new(),
    }
}

#[tokio::test]
async fn live_keys_apply_to_the_running_node_without_a_restart() {
    let _file = state::config_lock().await;
    let n = node();
    let before = n.state.cfg.live().session.budget_tokens;
    let (_, read) = call(&n, "GET", "/api/config", None).await;
    let (s, v) = call(
        &n,
        "PUT",
        "/api/config",
        Some(json!({
            "expected_hash": read["hash"],
            "session": { "budget_tokens": before + 1 },
            "review": { "max_diff_lines": 1234 },
            "supervision": { "checks": ["just live"] },
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["restart_required"], false, "{v}");
    let live = n.state.cfg.live();
    assert_eq!(live.session.budget_tokens, before + 1);
    assert_eq!(live.review.max_diff_lines, 1234);
    assert_eq!(live.supervision.checks, vec!["just live".to_string()]);
    // Seen by every part of the node holding the same config.
    assert_eq!(n.state.manager.cfg().live().review.max_diff_lines, 1234);
    // And what the node resolves a repository's checks with.
    let env = tracon::environment::environment_for(
        n.state.manager.cfg(),
        n.state.store(),
        Some(std::path::Path::new("/nowhere")),
    );
    assert_eq!(env.checks, vec!["just live".to_string()]);

    // An edit made by another process (the CLI, an editor) is taken in once
    // two looks see the same version.
    tracon::config::edit(
        n.state.store(),
        &[Op::set(KeyPath::keys(&["review", "max_files"]), 77_i64)],
        None,
        "test (another process)",
    )
    .unwrap();
    assert_ne!(n.state.cfg.live().review.max_files, 77);
    n.state.cfg.refresh_live_from(&Config::config_path());
    assert_ne!(n.state.cfg.live().review.max_files, 77);
    n.state.cfg.refresh_live_from(&Config::config_path());
    assert_eq!(n.state.cfg.live().review.max_files, 77);
}

#[tokio::test]
async fn restart_only_keys_raise_the_pending_restart_and_live_ones_do_not() {
    let _file = state::config_lock().await;
    let n = node();
    boot_from_file(&n);
    let (s, v) = call(&n, "GET", "/api/restart", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["config"]["keys"], json!([]), "{v}");
    assert_eq!(v["restart"]["available"], true, "{v}");
    assert_eq!(v["restart"]["mechanism"], "fake", "{v}");

    let outcome = tracon::config::edit(
        n.state.store(),
        &[
            Op::set(KeyPath::keys(&["session", "claim_grace_secs"]), 99_i64),
            Op::set(KeyPath::keys(&["gateway", "proxy_port"]), 4199_i64),
        ],
        None,
        "test",
    )
    .unwrap();
    assert!(outcome.restart_required);
    n.state.cfg.apply_edit(&outcome);
    let (_, v) = call(&n, "GET", "/api/restart", None).await;
    assert_eq!(v["config"]["keys"], json!(["gateway.proxy_port"]), "{v}");
    assert_eq!(v["config"]["hash"], json!(file_hash()));
    assert_ne!(v["config"]["hash"], v["config"]["boot_hash"]);
    // The live one already applies; the restart one is as the node started.
    assert_eq!(n.state.cfg.live().session.claim_grace_secs, 99);
    assert_ne!(n.state.cfg.live().gateway.proxy_port, 4199);

    // Put back, nothing is owed.
    let outcome = tracon::config::edit(
        n.state.store(),
        &[Op::remove(KeyPath::keys(&["gateway", "proxy_port"]))],
        None,
        "test",
    )
    .unwrap();
    n.state.cfg.apply_edit(&outcome);
    let (_, v) = call(&n, "GET", "/api/restart", None).await;
    assert_eq!(v["config"]["keys"], json!([]), "{v}");
}

#[tokio::test]
async fn when_idle_waits_for_running_turns_and_refuses_new_work_meanwhile() {
    let _file = state::config_lock().await;
    let n = node();
    let store = n.state.store().clone();
    let mut busy = session_row("s-busy", "n1", "personal");
    busy.turn_active = 1;
    store.insert_session(&busy).unwrap();
    store
        .insert_session(&session_row("s-idle", "n1", "personal"))
        .unwrap();

    let (s, v) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "mode": "when_idle", "expected_hash": file_hash() })),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["plan"]["mode"], "when_idle", "{v}");
    assert_eq!(v["plan"]["state"], "draining", "{v}");
    assert_eq!(v["busy"]["turns"], json!(["s-busy"]), "{v}");

    // While it waits: no new session, no new turn.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(n.supervisor.restarts.load(Ordering::SeqCst), 0);
    let manager = &n.state.manager;
    match manager.create(spec()).await {
        Err(SessionError::NodeRefused(why)) => assert!(why.contains("restarting"), "{why}"),
        other => panic!("a new session was not refused: {other:?}"),
    }
    match manager.prompt("s-idle", "more".into()).await {
        Err(SessionError::NodeRefused(why)) => assert!(why.contains("restarting"), "{why}"),
        other => panic!("a new turn was not refused: {other:?}"),
    }

    // The turn ends: the restart goes ahead, once.
    store
        .update_session(
            "s-busy",
            SessionPatch {
                turn_active: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
    eventually("the restart", || {
        n.supervisor.restarts.load(Ordering::SeqCst) == 1
    })
    .await;
    let (_, v) = call(&n, "GET", "/api/restart", None).await;
    assert_eq!(v["plan"]["state"], "restarting", "{v}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(n.supervisor.restarts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_restart_that_waits_too_long_is_abandoned_and_work_resumes() {
    let _file = state::config_lock().await;
    let n = node();
    let mut busy = session_row("s-long", "n1", "personal");
    busy.turn_active = 1;
    n.state.store().insert_session(&busy).unwrap();
    let (s, v) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "expected_hash": file_hash(), "timeout_secs": 1 })),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert!(n.state.manager.restart().draining());
    eventually("the restart to be abandoned", || {
        !n.state.manager.restart().draining()
    })
    .await;
    assert_eq!(n.supervisor.restarts.load(Ordering::SeqCst), 0);
    let (_, v) = call(&n, "GET", "/api/restart", None).await;
    assert!(
        v["last_error"]
            .as_str()
            .unwrap_or_default()
            .contains("1 turn"),
        "{v}"
    );
}

#[tokio::test]
async fn a_waiting_restart_can_be_withdrawn() {
    let _file = state::config_lock().await;
    let n = node();
    let mut busy = session_row("s-w", "n1", "personal");
    busy.turn_active = 1;
    n.state.store().insert_session(&busy).unwrap();
    let (s, _) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "expected_hash": file_hash() })),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (s, v) = call(&n, "DELETE", "/api/admin/restart", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["plan"], Value::Null, "{v}");
    assert!(!n.state.manager.restart().draining());
    let (s, _) = call(&n, "DELETE", "/api/admin/restart", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(n.supervisor.restarts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn now_names_the_sessions_it_ends_and_needs_them_confirmed() {
    let _file = state::config_lock().await;
    let n = node();
    let store = n.state.store();
    let mut working = session_row("s-now", "n1", "personal");
    working.turn_active = 1;
    store.insert_session(&working).unwrap();
    let mut put_away = session_row("s-away", "n1", "personal");
    put_away.state = "suspended".into();
    store.insert_session(&put_away).unwrap();
    store.ensure_peer_node("n2").unwrap();
    store
        .insert_session(&session_row("s-peer", "n2", "personal"))
        .unwrap();

    let (s, v) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "mode": "now", "expected_hash": file_hash() })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["confirm_required"], true, "{v}");
    let ids: Vec<&str> = v["closing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["s-now"], "{v}");
    assert!(!n.state.manager.restart().draining());

    let (s, v) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "mode": "now", "expected_hash": file_hash(), "confirm": true })),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    // A turn is running, and `now` does not wait for it.
    eventually("the restart", || {
        n.supervisor.restarts.load(Ordering::SeqCst) == 1
    })
    .await;
}

#[tokio::test]
async fn a_restart_is_refused_against_a_file_the_caller_did_not_see_or_without_a_supervisor() {
    let _file = state::config_lock().await;
    let n = node();
    let (s, v) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "expected_hash": "0".repeat(64) })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("changed since it was read"),
        "{v}"
    );
    assert!(!n.state.manager.restart().draining());

    let orphan = node_with(Fake {
        missing: true,
        ..Default::default()
    });
    let (s, v) = call(
        &orphan,
        "POST",
        "/api/admin/restart",
        Some(json!({ "expected_hash": file_hash() })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nothing supervises"),
        "{v}"
    );
    let (_, v) = call(&orphan, "GET", "/api/restart", None).await;
    assert_eq!(v["restart"]["available"], false, "{v}");
}

#[tokio::test]
async fn restarting_is_done_at_the_node_by_an_administrator() {
    let _file = state::config_lock().await;
    let n = node();
    // No credential.
    let req = Request::builder()
        .method("POST")
        .uri("/api/admin/restart")
        .header("host", "127.0.0.1:7420")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "expected_hash": file_hash() }).to_string(),
        ))
        .unwrap();
    let mut req = req;
    req.extensions_mut()
        .insert(ConnectInfo::<SocketAddr>(LOCAL.parse().unwrap()));
    let res = n.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    // Off the machine a bearer token is not enough: an administrator there
    // confirms with a passkey from a signed-in browser (`AdminAction`).
    let req = Request::builder()
        .method("POST")
        .uri("/api/admin/restart")
        .header("host", "tracon.example.com")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "expected_hash": file_hash() }).to_string(),
        ))
        .unwrap();
    let mut req = req;
    req.extensions_mut().insert(ConnectInfo::<SocketAddr>(
        "203.0.113.7:5000".parse().unwrap(),
    ));
    let res = n.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["kind"], "step_up_unavailable", "{v}");
    assert!(!n.state.manager.restart().draining());
    assert_eq!(n.supervisor.restarts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn when_idle_waits_for_a_running_check_too() {
    let _file = state::config_lock().await;
    let n = node();
    let store = n.state.store().clone();
    store
        .insert_check_run(&tracon::store::CheckRunRow {
            id: "run-1".into(),
            candidate_id: None,
            session_id: Some("s-c".into()),
            command: Some("just check".into()),
            definition_json: json!({ "command": "just check" }).to_string(),
            definition_hash: None,
            execution_image: None,
            inputs_json: None,
            reuse_key: None,
            outcome: "running".into(),
            source_outcome: None,
            exit_code: None,
            log: String::new(),
            duration_ms: None,
            started_ms: tracon::store::now_ms(),
            finished_ms: None,
            rerun_of: None,
            reused_from_id: None,
            metadata_json: "{}".into(),
        })
        .unwrap();
    let (s, v) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "expected_hash": file_hash() })),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["busy"]["checks"], 1, "{v}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(n.supervisor.restarts.load(Ordering::SeqCst), 0);
    store
        .finish_check_run("run-1", "passed", None, Some(0), "", Some(1), &json!({}))
        .unwrap();
    eventually("the restart", || {
        n.supervisor.restarts.load(Ordering::SeqCst) == 1
    })
    .await;
}

#[tokio::test]
async fn a_restart_is_abandoned_if_the_file_changed_while_it_waited() {
    let _file = state::config_lock().await;
    let n = node();
    let mut busy = session_row("s-edit", "n1", "personal");
    busy.turn_active = 1;
    n.state.store().insert_session(&busy).unwrap();
    let (s, v) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "expected_hash": file_hash() })),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    // While it waits, the file changes under it.
    tracon::config::edit(
        n.state.store(),
        &[Op::set(
            KeyPath::keys(&["node_name"]),
            "edited-while-waiting",
        )],
        None,
        "test",
    )
    .unwrap();
    n.state
        .store()
        .update_session(
            "s-edit",
            SessionPatch {
                turn_active: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
    eventually("the restart to be abandoned", || {
        !n.state.manager.restart().draining()
    })
    .await;
    assert_eq!(n.supervisor.restarts.load(Ordering::SeqCst), 0);
    let (_, v) = call(&n, "GET", "/api/restart", None).await;
    assert!(
        v["last_error"]
            .as_str()
            .unwrap_or_default()
            .contains("changed since the restart was asked for"),
        "{v}"
    );
}

#[tokio::test]
async fn a_restart_the_supervisor_never_did_stops_refusing_work() {
    let _file = state::config_lock().await;
    let n = node();
    n.state
        .manager
        .restart()
        .set_watchdog(Duration::from_millis(300));
    let (s, v) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "expected_hash": file_hash() })),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    eventually("the supervisor to be asked", || {
        n.supervisor.restarts.load(Ordering::SeqCst) == 1
    })
    .await;
    // Handed over, and the node is still here: not withdrawable yet.
    let (s, _) = call(&n, "DELETE", "/api/admin/restart", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(n.state.manager.restart().draining());
    // Past the watchdog it gives up and takes work again.
    eventually("the watchdog", || !n.state.manager.restart().draining()).await;
    let (_, v) = call(&n, "GET", "/api/restart", None).await;
    assert!(
        v["last_error"]
            .as_str()
            .unwrap_or_default()
            .contains("still running"),
        "{v}"
    );
}

/// A review's required checks run one after another, so between two of them
/// none is running; the review is still in flight, and the restart waits for
/// all of it.
#[tokio::test]
async fn when_idle_waits_for_a_review_between_its_checks() {
    let _file = state::config_lock().await;
    let n = node();
    let store = n.state.store().clone();
    let checking = n.state.manager.checking().clone();
    let entry = checking.begin(
        "r-two",
        "s-review",
        "c1",
        vec!["just lint".into(), "just test".into()],
    );
    let run = |id: &str, outcome: &str| tracon::store::CheckRunRow {
        id: id.into(),
        candidate_id: None,
        session_id: Some("s-review".into()),
        command: Some(id.into()),
        definition_json: json!({ "command": id }).to_string(),
        definition_hash: None,
        execution_image: None,
        inputs_json: None,
        reuse_key: None,
        outcome: outcome.into(),
        source_outcome: None,
        exit_code: None,
        log: String::new(),
        duration_ms: None,
        started_ms: tracon::store::now_ms(),
        finished_ms: None,
        rerun_of: None,
        reused_from_id: None,
        metadata_json: "{}".into(),
    };
    store
        .insert_check_run(&run("just lint", "running"))
        .unwrap();
    let (s, v) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "expected_hash": file_hash() })),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["busy"]["reviews"], 1, "{v}");
    // The first check ends; the second has not started: nothing is running,
    // and the review is not done.
    store
        .finish_check_run(
            "just lint",
            "passed",
            None,
            Some(0),
            "",
            Some(1),
            &json!({}),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(n.supervisor.restarts.load(Ordering::SeqCst), 0);
    // The second check, started after the restart was asked for, is waited on.
    store
        .insert_check_run(&run("just test", "running"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(n.supervisor.restarts.load(Ordering::SeqCst), 0);
    store
        .finish_check_run(
            "just test",
            "passed",
            None,
            Some(0),
            "",
            Some(1),
            &json!({}),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(n.supervisor.restarts.load(Ordering::SeqCst), 0);
    entry.finish(Ok(json!({ "review_id": "r-two" })), true);
    eventually("the restart", || {
        n.supervisor.restarts.load(Ordering::SeqCst) == 1
    })
    .await;
}

#[tokio::test]
async fn a_new_review_is_refused_while_a_restart_waits() {
    let _file = state::config_lock().await;
    let n = node();
    let mut busy = session_row("s-sub", "n1", "personal");
    busy.turn_active = 1;
    n.state.store().insert_session(&busy).unwrap();
    let (s, _) = call(
        &n,
        "POST",
        "/api/admin/restart",
        Some(json!({ "expected_hash": file_hash() })),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let ctx = tracon::mcp::CallContext::session("s-sub", "personal", "n1");
    let err = tracon::mcp::review::call(
        n.state.store(),
        &n.state.manager,
        &ctx,
        "submit_review",
        &json!({ "title": "t", "body": "b" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("submit_review is not taken"), "{err}");
    assert!(err.contains("restarting"), "{err}");
}

/// The repro the last-known-good restore was reviewed against: further
/// `tracon serve` runs on a running node's state directory, each failing,
/// must not count as failed starts of its file, let alone replace it.
#[test]
fn a_second_serve_on_the_same_state_neither_counts_nor_restores() {
    let dir = state::scratch("a_second_serve_on_the_same_state");
    let config = dir.join("node.toml");
    std::fs::write(&config, "node_name = \"running\"\n").unwrap();
    std::fs::write(dir.join("node.toml.last-good"), "node_name = \"old\"\n").unwrap();
    let paths = tracon::config::last_good::Paths {
        config: config.clone(),
        boots: dir.join("boot-attempts.json"),
    };
    // The running node.
    let _held = tracon::config::last_good::claim(&paths).unwrap();
    for _ in 0..5 {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_tracon"))
            .args(["serve", "--listen", "127.0.0.1:0"])
            .env("TRACON_STATE_DIR", &dir)
            .env("TRACON_CONFIG_DIR", &dir)
            .env("RUST_LOG", "error")
            .output()
            .unwrap();
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("another node owns"), "{stderr}");
    }
    assert!(!dir.join("boot-attempts.json").exists());
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "node_name = \"running\"\n"
    );
    let failed = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("failed")
        })
        .count();
    assert_eq!(failed, 0);
}
