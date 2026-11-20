//! Which node a CLI command changes. A shell set up for a scratch node
//! (`TRACON_STATE_DIR`, `TRACON_LISTEN`) once rotated the live node's
//! operator token on :7420, because the CLI read only `TRACON_URL`.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tracon::{
    broker::Broker,
    config::Config,
    http::{
        api::AppState,
        auth::{self, AuthState},
    },
    mcp::Tools,
    session::Manager,
    store::Store,
    stream::Bus,
};

use support::fake::FakeAdapter;

/// A node serving the operator API on a free loopback port, under the id
/// given, counting the requests it answers and keeping every body posted to
/// `/api/sessions`. With `answer_sessions` set, it answers those itself with
/// a row made from the request, as a node that started the session would,
/// instead of starting one.
struct Served {
    url: String,
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    state: AppState,
    posted: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    answer_sessions: Arc<std::sync::atomic::AtomicBool>,
    /// How many more posts to `/api/sessions` to answer as a node whose
    /// peer did not answer in time: a 504 whose outcome is unknown.
    unknown: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_node(node_id: &str) -> Served {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let cfg = Arc::new(Config::default());
    let tools = Arc::new(Tools {
        broker: Broker::default().shared(),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
        upstream: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg.clone(),
        node_id.into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let state = AppState {
        manager,
        cfg,
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: node_id.into(),
        tools,
        mesh: None,
        auth: Arc::new(AuthState::load(&store, "127.0.0.1".into())),
        enroll: Default::default(),
    };
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let posted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let answer_sessions = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let unknown = Arc::new(AtomicUsize::new(0));
    let (keep, answer, self_id) = (posted.clone(), answer_sessions.clone(), node_id.to_string());
    let unknowing = unknown.clone();
    let app = tracon::http::router(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::guard,
        ))
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let counted = counted.clone();
                let (keep, answer, self_id) = (keep.clone(), answer.clone(), self_id.clone());
                let unknowing = unknowing.clone();
                async move {
                    use axum::response::IntoResponse;
                    counted.fetch_add(1, Ordering::SeqCst);
                    if req.method() != axum::http::Method::POST
                        || req.uri().path() != "/api/sessions"
                    {
                        return next.run(req).await;
                    }
                    let (parts, body) = req.into_parts();
                    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                    let spec: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    keep.lock().unwrap().push(spec.clone());
                    let unanswered = unknowing
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_ok();
                    if unanswered {
                        let error = serde_json::json!({ "error": {
                            "code": 504,
                            "kind": "outcome_unknown",
                            "message": format!(
                                "node {} did not answer in time, so it is not known whether it \
                                 created the session",
                                spec["node_id"].as_str().unwrap_or("?")
                            ),
                            "request_id": spec["request_id"],
                        } });
                        return (axum::http::StatusCode::GATEWAY_TIMEOUT, axum::Json(error))
                            .into_response();
                    }
                    if !answer.load(Ordering::SeqCst) {
                        let req = axum::extract::Request::from_parts(parts, bytes.into());
                        return next.run(req).await;
                    }
                    let row = serde_json::json!({
                        "id": "0190aaaa-0000-7000-8000-00000000c0de",
                        "node_id": spec["node_id"].as_str().unwrap_or(&self_id),
                        "channel": spec["channel"],
                        "work_item_id": spec["work_item_id"],
                        "repo_path": spec["repo_path"],
                        "branch": spec["branch"].as_str().unwrap_or("feat/tracon-00000000c0de"),
                        "phase": spec["phase"],
                        "model": spec["model"].as_str().unwrap_or("m/bound"),
                        "harness_id": spec["harness"].as_str().unwrap_or("fake"),
                        "state": "starting",
                    });
                    (axum::http::StatusCode::CREATED, axum::Json(row)).into_response()
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Served {
        url: format!("http://{addr}"),
        addr,
        hits,
        state,
        posted,
        answer_sessions,
        unknown,
        task,
    }
}

/// Whether the node has an operator token, as `GET /api/auth/sessions` says.
async fn has_token(node: &Served) -> bool {
    let v: serde_json::Value = reqwest::get(format!("{}/api/auth/sessions", node.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["configured"].as_bool().unwrap()
}

/// A scratch state directory with an identity of its own, and that id.
fn scratch_state(name: &str) -> (PathBuf, String) {
    let dir = state::scratch(&format!("cli-target-{name}"));
    let (id, _) =
        tracon::mesh::identity::load_or_generate_at(&dir.join("node-identity.seed")).unwrap();
    (dir, id.node_id())
}

/// The CLI in a shell set up for the scratch node: its state and config
/// directories, and none of the variables a test sets on purpose.
fn tracon(state_dir: &Path, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_tracon"));
    cmd.args(args)
        .env("TRACON_STATE_DIR", state_dir)
        .env("TRACON_CONFIG_DIR", state_dir)
        .env_remove("TRACON_URL")
        .env_remove("TRACON_LISTEN")
        .env_remove("TRACON_TOKEN")
        .env("RUST_LOG", "error")
        .stdin(std::process::Stdio::null());
    cmd
}

fn text(out: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn write_serve_file(state_dir: &Path, url: &str, node_id: &str, pid: u32) {
    let record = tracon::node_target::ServeRecord {
        url: url.into(),
        node_id: node_id.into(),
        pid,
    };
    std::fs::write(
        tracon::node_target::serve_file(state_dir),
        serde_json::to_string(&record).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn with_tracon_listen_auth_issue_changes_the_scratch_node() {
    let (dir, id) = scratch_state("listen");
    let scratch = serve_node(&id).await;
    let live = serve_node(&"f".repeat(64)).await;

    let out = tracon(&dir, &["auth", "issue", "--yes"])
        .env("TRACON_LISTEN", scratch.addr.to_string())
        .output()
        .await
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stdout.starts_with("trc1."), "{stdout}");
    assert!(
        stderr.contains(&format!(
            "Rotating the operator token of {} at {}",
            tracon::names::node(&id, None),
            scratch.url
        )),
        "{stderr}"
    );
    assert!(has_token(&scratch).await);
    assert!(!has_token(&live).await);
    assert_eq!(
        live.hits.load(Ordering::SeqCst),
        1,
        "only has_token's own read"
    );
}

#[tokio::test]
async fn with_only_the_state_dir_auth_issue_finds_the_node_through_serve_json() {
    let (dir, id) = scratch_state("serve-json");
    let scratch = serve_node(&id).await;
    // This process is alive: the record stands for a running `serve`.
    write_serve_file(&dir, &scratch.url, &id, std::process::id());

    let out = tracon(&dir, &["auth", "issue", "--yes"])
        .output()
        .await
        .unwrap();
    let (_, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(has_token(&scratch).await);
}

#[tokio::test]
async fn a_tracon_url_naming_another_node_is_refused_and_changes_nothing() {
    let (dir, id) = scratch_state("other-node");
    let _scratch = serve_node(&id).await;
    let live = serve_node(&"f".repeat(64)).await;

    let out = tracon(&dir, &["auth", "issue", "--yes"])
        .env("TRACON_URL", &live.url)
        .output()
        .await
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert!(!out.status.success());
    assert!(!stdout.contains("trc1."), "no token was printed: {stdout}");
    assert!(
        stderr.contains(&format!(
            "TRACON_URL points at {}",
            tracon::names::node(&"f".repeat(64), None)
        )) && stderr.contains(&format!("is {}", tracon::names::node(&id, None)))
            && stderr.contains("set TRACON_URL to the node you mean"),
        "{stderr}"
    );
    assert!(!has_token(&live).await);

    // Revoke and every other write go through the same check.
    let out = tracon(&dir, &["auth", "revoke", "--yes"])
        .env("TRACON_URL", &live.url)
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    let out = tracon(&dir, &["work", "add", "nothing"])
        .env("TRACON_URL", &live.url)
        .output()
        .await
        .unwrap();
    let (_, stderr) = text(&out);
    assert!(!out.status.success());
    assert!(
        stderr.contains("set TRACON_URL to the node you mean"),
        "{stderr}"
    );
}

#[tokio::test]
async fn without_a_terminal_auth_issue_needs_yes() {
    let (dir, id) = scratch_state("no-tty");
    let scratch = serve_node(&id).await;

    for args in [&["auth", "issue"][..], &["auth", "revoke"][..]] {
        let out = tracon(&dir, args)
            .env("TRACON_URL", &scratch.url)
            .output()
            .await
            .unwrap();
        let (stdout, stderr) = text(&out);
        assert!(!out.status.success(), "{args:?}");
        assert!(!stdout.contains("trc1."), "{stdout}");
        assert!(stderr.contains("pass --yes"), "{stderr}");
        assert!(!has_token(&scratch).await);
    }
}

#[tokio::test]
async fn the_old_url_flag_still_works_and_warns() {
    let (dir, id) = scratch_state("old-flag");
    let scratch = serve_node(&id).await;

    let out = tracon(
        &dir,
        &[
            "auth",
            "issue",
            "--yes",
            "--url",
            "https://node.example.ts.net",
        ],
    )
    .env("TRACON_URL", &scratch.url)
    .output()
    .await
    .unwrap();
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("--url is now --public-url"), "{stderr}");
    assert!(
        stdout.contains("https://node.example.ts.net/#token=trc1."),
        "{stdout}"
    );
    assert!(has_token(&scratch).await);

    let out = tracon(
        &dir,
        &[
            "auth",
            "issue",
            "--yes",
            "--public-url",
            "https://node.example.ts.net",
        ],
    )
    .env("TRACON_URL", &scratch.url)
    .output()
    .await
    .unwrap();
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("--url is now"), "{stderr}");
    assert!(
        stdout.contains("https://node.example.ts.net/#token=trc1."),
        "{stdout}"
    );
}

#[tokio::test]
async fn a_serve_json_naming_a_dead_pid_is_ignored_with_a_warning() {
    let (dir, id) = scratch_state("dead-pid");
    let scratch = serve_node(&id).await;
    // A pid that was, and is no longer.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    write_serve_file(&dir, &scratch.url, &id, dead);

    // Read-only, so whatever answers the default address is not changed.
    let out = tracon(&dir, &["auth", "sessions"]).output().await.unwrap();
    let (_, stderr) = text(&out);
    assert!(
        stderr.contains(&format!(
            "serve.json in {} names pid {dead}, which is not running",
            dir.display()
        )),
        "{stderr}"
    );
    assert_eq!(scratch.hits.load(Ordering::SeqCst), 0);
    assert!(!has_token(&scratch).await);
}

/// Driving another node through a port-forward from a machine that runs its
/// own: `TRACON_URL` names it, the state directory is left at its default,
/// and the write goes through though the ids differ.
#[tokio::test]
async fn an_explicit_tracon_url_from_the_default_state_dir_is_not_checked() {
    let home = state::scratch("cli-target-default-home");
    let default_dir = home.join("state").join("tracon");
    std::fs::create_dir_all(&default_dir).unwrap();
    tracon::mesh::identity::load_or_generate_at(&default_dir.join("node-identity.seed")).unwrap();
    let forwarded = serve_node(&"e".repeat(64)).await;

    let out = tracon(&home, &["auth", "issue", "--yes"])
        .env_remove("TRACON_STATE_DIR")
        .env("XDG_STATE_HOME", home.join("state"))
        .env("HOME", &home)
        .env("TRACON_URL", &forwarded.url)
        .output()
        .await
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stdout.starts_with("trc1."), "{stdout}");
    assert!(has_token(&forwarded).await);
}

/// A named state directory with no identity (its node never started, or the
/// path is mistyped) doesn't fall through to whatever node answers: only a
/// node `TRACON_URL` names is written to.
#[tokio::test]
async fn a_state_dir_with_no_identity_writes_only_to_tracon_url() {
    let dir = state::scratch("cli-target-no-identity");
    let other = serve_node(&"d".repeat(64)).await;

    let out = tracon(&dir, &["auth", "issue", "--yes"])
        .env("TRACON_LISTEN", other.addr.to_string())
        .output()
        .await
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert!(!out.status.success());
    assert!(!stdout.contains("trc1."), "{stdout}");
    assert!(
        stderr.contains("holds no node identity") && stderr.contains("set TRACON_URL"),
        "{stderr}"
    );
    assert!(!has_token(&other).await);
    assert!(
        !dir.join("node-identity.seed").exists(),
        "the check created no identity"
    );

    // A CLI used remotely, with TRACON_URL and TRACON_TOKEN, still works.
    let out = tracon(&dir, &["auth", "issue", "--yes"])
        .env("TRACON_URL", &other.url)
        .output()
        .await
        .unwrap();
    let (_, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(has_token(&other).await);
}

// `tracon work start`: a thin client over `POST /api/sessions`, resolving
// what the Composer resolves. The node's own start (readiness, plan, model,
// forwarding to a peer) is tested where it lives; here the node answers the
// post itself, so what is checked is what the CLI sent.

const PEER_A: &str = "ab12cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const PEER_B: &str = "ab12dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const REMOTE: &str = "https://github.com/example/widgets";

/// A node holding a `widgets` project with a forge remote, two peers whose
/// ids share a prefix (`homelab`, unreachable, and `bazzite`), and the
/// CLI's state directory for it.
async fn work_node(name: &str) -> (Served, PathBuf) {
    let (dir, id) = scratch_state(name);
    let node = serve_node(&id).await;
    let store = node.state.store();
    store
        .put_node(&support::rows::node_row(&id, "scratch"))
        .unwrap();
    let mut homelab = support::rows::node_row(PEER_A, "homelab");
    homelab.is_self = 0;
    homelab.reachable = 0;
    store.put_node(&homelab).unwrap();
    let mut bazzite = support::rows::node_row(PEER_B, "bazzite");
    bazzite.is_self = 0;
    store.put_node(&bazzite).unwrap();
    store
        .project_put(&tracon::store::ProjectRow {
            id: "proj-widgets".into(),
            channel: "personal".into(),
            name: "widgets".into(),
            remote_url: Some(REMOTE.into()),
            created_ms: 1,
        })
        .unwrap();
    (node, dir)
}

/// A task on `personal`, in the project given; its full id.
async fn task(node: &Served, title: &str, project: Option<&str>) -> String {
    let v: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/api/work", node.url))
        .json(&serde_json::json!({ "channel": "personal", "title": title, "project_id": project }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["id"].as_str().unwrap().to_string()
}

async fn work_start(node: &Served, dir: &Path, args: &[&str]) -> (bool, String, String) {
    let mut all = vec!["work", "start"];
    all.extend_from_slice(args);
    let out = tracon(dir, &all)
        .env("TRACON_URL", &node.url)
        .output()
        .await
        .unwrap();
    let (stdout, stderr) = text(&out);
    (out.status.success(), stdout, stderr)
}

fn posted(node: &Served) -> Vec<serde_json::Value> {
    node.posted.lock().unwrap().clone()
}

#[tokio::test]
async fn work_start_plans_a_task_without_a_plan_in_its_projects_repository() {
    let (node, dir) = work_node("work-start-plan").await;
    node.answer_sessions.store(true, Ordering::SeqCst);
    let id = task(&node, "supervise plugins", Some("proj-widgets")).await;

    let (ok, stdout, stderr) = work_start(&node, &dir, &[&id[..8]]).await;
    assert!(ok, "{stderr}");
    let sent = posted(&node);
    assert_eq!(sent.len(), 1);
    let spec = sent[0].as_object().unwrap();
    assert_eq!(spec["channel"], "personal");
    assert_eq!(spec["repo_path"], REMOTE);
    assert_eq!(spec["work_item_id"], id.as_str());
    assert_eq!(spec["phase"], "plan");
    for unset in [
        "branch",
        "base",
        "model",
        "harness",
        "node_id",
        "budget_tokens",
    ] {
        assert!(!spec.contains_key(unset), "{unset} was sent: {spec:?}");
    }
    assert!(stdout.contains("plan (no plan yet)"), "{stdout}");
    assert!(
        stdout.contains("node         scratch\n"),
        "the node by name: {stdout}"
    );
    assert!(stdout.contains("feat/tracon-00000000c0de"), "{stdout}");
    assert!(stdout.contains("m/bound"), "the resolved model: {stdout}");
    assert!(
        stdout.contains(&format!(
            "{}/sessions/0190aaaa-0000-7000-8000-00000000c0de",
            node.url
        )),
        "{stdout}"
    );
}

#[tokio::test]
async fn work_start_executes_once_a_plan_exists_on_the_latest_sessions_branch() {
    let (node, dir) = work_node("work-start-execute").await;
    node.answer_sessions.store(true, Ordering::SeqCst);
    let id = task(&node, "supervise plugins", Some("proj-widgets")).await;
    let store = node.state.store();
    // A plan session, then a later one on another branch.
    for (sid, branch, created) in [
        ("s-old", "feat/old", 1_000),
        ("s-new", "feat/plan-u2", 2_000),
    ] {
        let mut row = support::rows::session_row(sid, &node.state.node_id, "personal");
        row.work_item_id = Some(id.clone());
        row.branch = branch.into();
        row.phase = "plan".into();
        row.state = "ended".into();
        row.created_ms = created;
        store.insert_session(&row).unwrap();
    }
    // A plan slug whose document is gone is no plan.
    let slug = tracon::corpus::work::plan_slug(&id);
    tracon::corpus::work::set_plan(
        store,
        node.state.manager.bus(),
        &node.state.node_id,
        &id,
        &slug,
    )
    .unwrap();
    let (ok, _, stderr) = work_start(&node, &dir, &[&id]).await;
    assert!(ok, "{stderr}");
    assert_eq!(posted(&node)[0]["phase"], "plan");

    let res = reqwest::Client::new()
        .put(format!("{}/api/docs/personal/{slug}", node.url))
        .json(&serde_json::json!({ "body": "# Plan\n\nDo it." }))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "{}", res.text().await.unwrap());

    let (ok, stdout, stderr) = work_start(&node, &dir, &[&id[..8]]).await;
    assert!(ok, "{stderr}");
    let spec = &posted(&node)[1];
    assert_eq!(spec["phase"], "execute");
    assert_eq!(spec["branch"], "feat/plan-u2");
    assert!(stdout.contains("execute (the task has a plan)"), "{stdout}");
    assert!(stdout.contains("feat/plan-u2"), "{stdout}");

    // What is given wins over what is found.
    let (ok, _, stderr) = work_start(
        &node,
        &dir,
        &[
            &id[..8],
            "--phase",
            "plan",
            "--branch",
            "feat/again",
            "--base",
            "develop",
            "--model",
            "m/a",
            "--harness",
            "native",
            "--budget",
            "5000",
        ],
    )
    .await;
    assert!(ok, "{stderr}");
    let spec = &posted(&node)[2];
    assert_eq!(spec["phase"], "plan");
    assert_eq!(spec["branch"], "feat/again");
    assert_eq!(spec["base"], "develop");
    assert_eq!(spec["model"], "m/a");
    assert_eq!(spec["harness"], "native");
    assert_eq!(spec["budget_tokens"], 5000);
}

#[tokio::test]
async fn work_start_resolves_the_node_by_name_or_prefix_and_refuses_an_ambiguous_one() {
    let (node, dir) = work_node("work-start-node").await;
    node.answer_sessions.store(true, Ordering::SeqCst);
    let id = task(&node, "supervise plugins", Some("proj-widgets")).await;

    let (ok, stdout, stderr) = work_start(&node, &dir, &[&id[..8], "--node", "BAZZITE"]).await;
    assert!(ok, "{stderr}");
    assert_eq!(posted(&node)[0]["node_id"], PEER_B);
    assert!(stdout.contains("node         bazzite\n"), "{stdout}");
    assert!(!stderr.contains("not reachable"), "{stderr}");

    let (ok, _, stderr) = work_start(&node, &dir, &[&id[..8], "--node", "ab12c"]).await;
    assert!(ok, "{stderr}");
    assert_eq!(posted(&node)[1]["node_id"], PEER_A);
    assert!(stderr.contains("node homelab is not reachable"), "{stderr}");

    let (ok, _, stderr) = work_start(&node, &dir, &[&id[..8], "--node", "self"]).await;
    assert!(ok, "{stderr}");
    assert_eq!(posted(&node)[2]["node_id"], node.state.node_id.as_str());

    // A node is also found by the generated name the node's own pages and
    // messages give it.
    let generated = tracon::names::node(PEER_B, None);
    let (ok, _, stderr) = work_start(&node, &dir, &[&id[..8], "--node", &generated]).await;
    assert!(ok, "{stderr}");
    assert_eq!(posted(&node)[3]["node_id"], PEER_B);

    for (text, says) in [
        ("ab12", "names more than one node"),
        ("nowhere", "no node \"nowhere\""),
    ] {
        let (ok, _, stderr) = work_start(&node, &dir, &[&id[..8], "--node", text]).await;
        assert!(!ok, "{text}");
        assert!(stderr.contains(says), "{stderr}");
        assert!(
            stderr.contains("homelab") && stderr.contains("bazzite"),
            "the known nodes are listed: {stderr}"
        );
    }
    assert_eq!(
        posted(&node).len(),
        4,
        "nothing was posted for a refused --node"
    );
}

#[tokio::test]
async fn work_start_without_a_project_needs_repo_and_json_prints_the_row() {
    let (node, dir) = work_node("work-start-repo").await;
    node.answer_sessions.store(true, Ordering::SeqCst);
    let id = task(&node, "loose task", None).await;

    let (ok, _, stderr) = work_start(&node, &dir, &[&id[..8]]).await;
    assert!(!ok);
    assert!(
        stderr.contains("has no project") && stderr.contains("--repo"),
        "{stderr}"
    );
    assert!(posted(&node).is_empty());

    let (ok, stdout, stderr) = work_start(
        &node,
        &dir,
        &[
            &id[..8],
            "--repo",
            "https://github.com/example/other",
            "--json",
        ],
    )
    .await;
    assert!(ok, "{stderr}");
    assert_eq!(
        posted(&node)[0]["repo_path"],
        "https://github.com/example/other"
    );
    let row: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(row["id"], "0190aaaa-0000-7000-8000-00000000c0de");
    assert_eq!(row["work_item_id"], id.as_str());
}

#[tokio::test]
async fn work_start_prints_the_nodes_refusal_and_fails() {
    let (node, dir) = work_node("work-start-refused").await;
    let id = task(&node, "supervise plugins", Some("proj-widgets")).await;

    // The node itself answers: execute on a task with no plan.
    let (ok, stdout, stderr) = work_start(
        &node,
        &dir,
        &[&id[..8], "--phase", "execute", "--model", "m/a"],
    )
    .await;
    assert!(!ok, "{stdout}");
    assert_eq!(posted(&node).len(), 1);
    assert!(
        stderr.contains("execute needs a plan: run a plan session for this task first"),
        "{stderr}"
    );
    assert!(!stdout.contains("started"), "{stdout}");
}

/// A start whose node could not say whether its peer made the session is
/// sent once more with the same request id, which cannot make a second one;
/// every start carries one, a given `--request-id` as given.
#[tokio::test]
async fn work_start_retries_an_unknown_outcome_with_the_same_request_id() {
    let (node, dir) = work_node("work-start-unknown-once").await;
    node.answer_sessions.store(true, Ordering::SeqCst);
    let id = task(&node, "supervise plugins", Some("proj-widgets")).await;
    node.unknown.store(1, Ordering::SeqCst);

    let (ok, stdout, stderr) = work_start(&node, &dir, &[&id[..8], "--node", "bazzite"]).await;
    assert!(ok, "{stderr}");
    let sent = posted(&node);
    assert_eq!(sent.len(), 2, "{sent:?}");
    let request_id = sent[0]["request_id"].as_str().unwrap().to_string();
    assert!(!request_id.is_empty());
    assert_eq!(sent[1]["request_id"], request_id.as_str());
    assert_eq!(sent[1]["node_id"], PEER_B);
    assert!(stderr.contains(&request_id), "{stderr}");
    assert!(stdout.contains("started 0190aaaa"), "{stdout}");

    // A fresh start is a fresh request; a given id is sent as given.
    let (ok, _, stderr) = work_start(&node, &dir, &[&id[..8], "--node", "bazzite"]).await;
    assert!(ok, "{stderr}");
    assert_ne!(posted(&node)[2]["request_id"], request_id.as_str());
    let (ok, _, stderr) = work_start(
        &node,
        &dir,
        &[&id[..8], "--node", "bazzite", "--request-id", "given-1"],
    )
    .await;
    assert!(ok, "{stderr}");
    assert_eq!(posted(&node)[3]["request_id"], "given-1");
}

/// Still unknown after the retry: the start fails, and says how to check
/// and how to retry without starting a second session.
#[tokio::test]
async fn work_start_that_stays_unknown_names_the_check_and_the_safe_retry() {
    let (node, dir) = work_node("work-start-unknown").await;
    node.answer_sessions.store(true, Ordering::SeqCst);
    let id = task(&node, "supervise plugins", Some("proj-widgets")).await;
    node.unknown.store(usize::MAX, Ordering::SeqCst);

    let (ok, stdout, stderr) = work_start(&node, &dir, &[&id[..8], "--node", "bazzite"]).await;
    assert!(!ok, "{stdout}");
    let sent = posted(&node);
    assert_eq!(sent.len(), 2, "one retry, no more: {sent:?}");
    let request_id = sent[0]["request_id"].as_str().unwrap();
    assert_eq!(sent[1]["request_id"], request_id);
    let task_name = tracon::names::task(&id);
    assert!(
        stderr.contains(&format!("tracon work show {task_name} --channel personal")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("--request-id {request_id}")),
        "{stderr}"
    );
    assert!(stderr.contains("may still be created"), "{stderr}");
    assert!(!stdout.contains("started"), "{stdout}");
}
