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
/// given, counting the requests it answers.
struct Served {
    url: String,
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
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
    let app = tracon::http::router(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, auth::guard))
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    next.run(req).await
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
            "Rotating the operator token of node {} at {}",
            &id[..8],
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
        stderr.contains("TRACON_URL points at node ffffffff")
            && stderr.contains(&format!("is node {}", &id[..8]))
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
