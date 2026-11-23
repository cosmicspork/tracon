//! The workspace's read-only `origin`, end to end: a real `git` in a
//! "workspace" fetches, pulls and tries to push through the gateway, which
//! speaks smart HTTP to a "forge" that is `git http-backend` over a local
//! bare repository. The forge demands nothing, but records every request, so
//! what reached it (and with which credential) is what the test reads.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;

use tracon::{
    broker::{Broker, Credential},
    config::Config,
    http::api::AppState,
    mcp::Tools,
    session::Manager,
    store::Store,
    stream::Bus,
};

use support::fake::FakeAdapter;

/// `(method, path and query, authorization)` of every request the forge saw.
type Seen = Arc<Mutex<Vec<(String, String, Option<String>)>>>;

/// A Git forge: `git http-backend` serving every repository under `root`,
/// read-only (`http.receivepack` is off for an unauthenticated CGI caller).
async fn forge(root: PathBuf, seen: Seen) -> u16 {
    let app = axum::Router::new().fallback(move |req: Request<Body>| {
        let root = root.clone();
        let seen = seen.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(body, 1 << 28).await.unwrap();
            let header = |name: &str| {
                parts
                    .headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            seen.lock().unwrap().push((
                parts.method.to_string(),
                parts.uri.to_string(),
                header("authorization"),
            ));
            let mut cgi = tokio::process::Command::new("git");
            cgi.arg("http-backend")
                .env("GIT_PROJECT_ROOT", &root)
                .env("GIT_HTTP_EXPORT_ALL", "1")
                .env("PATH_INFO", parts.uri.path())
                .env("QUERY_STRING", parts.uri.query().unwrap_or(""))
                .env("REQUEST_METHOD", parts.method.as_str())
                .env("CONTENT_LENGTH", body.len().to_string())
                .env("CONTENT_TYPE", header("content-type").unwrap_or_default())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Some(encoding) = header("content-encoding") {
                cgi.env("HTTP_CONTENT_ENCODING", encoding);
            }
            if let Some(protocol) = header("git-protocol") {
                cgi.env("GIT_PROTOCOL", protocol);
            }
            let mut child = cgi.spawn().unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let writer = tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(&body).await;
            });
            let out = child.wait_with_output().await.unwrap();
            let _ = writer.await;
            cgi_response(&out.stdout)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

/// A CGI program's output as an HTTP response.
fn cgi_response(out: &[u8]) -> Response {
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| (at, 4))
        .or_else(|| out.windows(2).position(|w| w == b"\n\n").map(|at| (at, 2)));
    let Some((at, gap)) = split else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let head = String::from_utf8_lossy(&out[..at]).into_owned();
    let mut response = Response::builder();
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            let code = value.split(' ').next().unwrap().parse::<u16>().unwrap();
            response = response.status(code);
        } else {
            response = response.header(name.trim(), value);
        }
    }
    response.body(Body::from(out[at + gap..].to_vec())).unwrap()
}

/// `git` as the tests run it: no ambient configuration but `global`, no
/// proxy, never a prompt.
fn git(dir: &Path, global: &Path, args: &[&str]) -> Output {
    std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("HOME", dir)
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .output()
        .unwrap()
}

fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A commit on the forge's `main`, made from `seed`, a clone of it.
fn forge_commit(seed: &Path, none: &Path, file: &str) -> String {
    std::fs::write(seed.join(file), file).unwrap();
    ok(git(seed, none, &["add", file]));
    ok(git(seed, none, &["commit", "-q", "-m", file]));
    ok(git(seed, none, &["push", "-q", "origin", "HEAD:main"]));
    ok(git(seed, none, &["rev-parse", "HEAD"]))
}

const FORGE_TOKEN: &str = "forge-secret-for-tests";

struct Rig {
    owner: String,
    _tmp: tempfile::TempDir,
    store: Arc<Store>,
    seen: Seen,
    seed: PathBuf,
    workspace: PathBuf,
    /// The workspace's global gitconfig, as a session's is staged.
    global: PathBuf,
    none: PathBuf,
    gateway: u16,
    token: String,
    url: String,
}

async fn rig(owner: &str) -> Rig {
    state::isolate();
    let tmp = tempfile::tempdir().unwrap();
    let none = tmp.path().join("none.gitconfig");
    std::fs::write(&none, "").unwrap();

    // The forge, with one commit on main.
    let root = tmp.path().join("forge");
    std::fs::create_dir_all(root.join(owner)).unwrap();
    ok(git(
        tmp.path(),
        &none,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            &format!("forge/{owner}/r.git"),
        ],
    ));
    ok(git(
        tmp.path(),
        &none,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            &format!("forge/{owner}/other.git"),
        ],
    ));
    let seed = tmp.path().join("seed");
    ok(git(
        tmp.path(),
        &none,
        &[
            "clone",
            "-q",
            root.join(owner).join("r.git").to_str().unwrap(),
            "seed",
        ],
    ));
    forge_commit(&seed, &none, "one");
    let seen: Seen = Default::default();
    let port = forge(root.clone(), seen.clone()).await;

    // The node's managed clone of it, made over HTTP so its origin is the
    // forge's URL, as `forge::clone` makes one.
    let clone =
        tracon::forge::managed_root(&Config::state_dir()).join(format!("127.0.0.1/{owner}/r"));
    std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
    ok(git(
        tmp.path(),
        &none,
        &[
            "clone",
            "-q",
            &format!("http://127.0.0.1:{port}/{owner}/r.git"),
            clone.to_str().unwrap(),
        ],
    ));
    seen.lock().unwrap().clear();

    // The channel's forge credential, for that host.
    let broker = Broker::default().shared();
    broker.write().unwrap().put(
        "gh",
        Credential {
            env: [
                ("GH_TOKEN".to_string(), FORGE_TOKEN.to_string()),
                ("GH_HOST".to_string(), "127.0.0.1".to_string()),
            ]
            .into_iter()
            .collect(),
            channels: vec!["work".into()],
            ..Credential::default()
        },
    );

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let mut row = support::rows::session_row(owner, "n1", "work");
    row.repo_path = clone.to_string_lossy().into_owned();
    store.insert_session(&row).unwrap();
    let cfg = Arc::new(Config::default());
    let tools = Arc::new(Tools {
        broker: broker.clone(),
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
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let token = manager.register_tool_token_for_test(owner, "work").await;
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
        auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = listener.local_addr().unwrap().port();
    let app = tracon::http::harness_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // The URL a session on that clone is given, and its gitconfig.
    let url = tracon::gateway::git::origin_for(
        &Config::state_dir(),
        &clone,
        "127.0.0.1",
        gateway,
        "work",
    )
    .expect("a managed clone of a forge repository has a gateway origin");
    assert_eq!(
        url,
        format!("http://127.0.0.1:{gateway}/git/work/{owner}/r.git")
    );
    let global = tmp.path().join("session.gitconfig");
    std::fs::write(&global, tracon::gateway::git::gitconfig(&url, &token)).unwrap();

    // The workspace, seeded at launch without a remote, as the node seeds one.
    let workspace = tmp.path().join("work");
    ok(git(
        tmp.path(),
        &none,
        &["clone", "-q", "--no-local", clone.to_str().unwrap(), "work"],
    ));
    ok(git(&workspace, &none, &["remote", "remove", "origin"]));
    tracon::gateway::git::set_workspace_origin(&workspace, &url).unwrap();

    Rig {
        owner: owner.to_string(),
        _tmp: tmp,
        store,
        seen,
        seed,
        workspace,
        global,
        none,
        gateway,
        token,
        url,
    }
}

fn fetches(store: &Store, session: &str) -> Vec<serde_json::Value> {
    store
        .session_events_of_kind(session, "git_fetch")
        .unwrap()
        .into_iter()
        .map(|row| row.payload)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn origin_fetches_through_the_gateway_and_refuses_a_push() {
    let r = rig("fetcher").await;
    let ws = &r.workspace;

    // The workspace names origin, and a main that moved since launch is a
    // plain fetch away, over protocol v2 (Git's default).
    let remote = ok(git(ws, &r.global, &["remote", "get-url", "origin"]));
    assert_eq!(remote, r.url);
    let two = forge_commit(&r.seed, &r.none, "two");
    ok(git(ws, &r.global, &["fetch", "-q", "origin"]));
    assert_eq!(ok(git(ws, &r.global, &["rev-parse", "origin/main"])), two);
    let recorded = fetches(&r.store, &r.owner);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["repository"], format!("{}/r", r.owner));
    assert_eq!(recorded[0]["protocol"], "v2");
    assert_eq!(recorded[0]["fetched"][0]["sha"], two.as_str());
    // Named by every ref the forge listed at that commit: `main`, and `HEAD`
    // where the client asked for it (newer Git's ls-refs does).
    let refs: Vec<&str> = recorded[0]["fetched"][0]["refs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|name| name.as_str())
        .collect();
    assert!(refs.contains(&"refs/heads/main"), "{recorded:?}");
    assert!(
        refs.iter()
            .all(|name| ["HEAD", "refs/heads/main"].contains(name)),
        "{recorded:?}"
    );

    // Protocol v0 too, and a pull that rebases local work onto the new main.
    std::fs::write(ws.join("mine"), "mine").unwrap();
    ok(git(ws, &r.global, &["add", "mine"]));
    ok(git(ws, &r.global, &["commit", "-q", "-m", "mine"]));
    let three = forge_commit(&r.seed, &r.none, "three");
    ok(git(
        ws,
        &r.global,
        &[
            "-c",
            "protocol.version=0",
            "pull",
            "-q",
            "--rebase",
            "origin",
            "main",
        ],
    ));
    assert_eq!(
        ok(git(ws, &r.global, &["rev-parse", "HEAD~1"])),
        three,
        "the local commit is rebased onto the moved main"
    );
    let recorded = fetches(&r.store, &r.owner);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert_eq!(recorded[1]["protocol"], "v0");
    assert_eq!(recorded[1]["fetched"][0]["sha"], three.as_str());
    assert!(
        recorded[1]["fetched"][0]["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "refs/heads/main"),
        "{recorded:?}"
    );

    // Every request reached the forge with the brokered credential, and
    // never with the session's own token.
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{FORGE_TOKEN}"))
    );
    {
        let seen = r.seen.lock().unwrap();
        assert!(!seen.is_empty());
        for (_, path, auth) in seen.iter() {
            assert_eq!(auth.as_deref(), Some(basic.as_str()), "{path}");
            assert!(!path.contains(&r.token), "{path}");
        }
    }
    // The forge token is nowhere in the workspace or its configuration.
    for file in [r.global.clone(), ws.join(".git/config")] {
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains(FORGE_TOKEN), "{}: {text}", file.display());
    }

    // A push is refused at the protocol, with a message Git prints, and the
    // forge never hears of it.
    r.seen.lock().unwrap().clear();
    let pushed = git(ws, &r.global, &["push", "origin", "HEAD:main"]);
    assert!(!pushed.status.success());
    let said = String::from_utf8_lossy(&pushed.stderr);
    assert!(said.contains("submit_review"), "{said}");
    assert!(said.contains("read-only"), "{said}");
    let pushed = git(
        ws,
        &r.global,
        &["push", "origin", "HEAD:refs/heads/elsewhere"],
    );
    assert!(!pushed.status.success());
    assert!(r.seen.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_gateway_serves_the_sessions_own_repository_and_nothing_else() {
    let r = rig("scoped").await;
    let o = &r.owner;
    let header = format!("http.extraHeader=Authorization: Bearer {}", r.token);
    let base = format!("http://127.0.0.1:{}/git", r.gateway);
    for (url, why) in [
        (format!("{base}/work/{o}/other.git"), "no other"),
        (format!("{base}/personal/{o}/r.git"), "no other"),
    ] {
        let out = git(&r.workspace, &r.none, &["-c", &header, "ls-remote", &url]);
        assert!(!out.status.success(), "{url}");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(said.contains(why), "{url}: {said}");
    }
    // Without the session's own token, nothing.
    let out = git(
        &r.workspace,
        &r.none,
        &["ls-remote", &format!("{base}/work/{o}/r.git")],
    );
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("running session"), "{said}");
    // The forge heard of none of it.
    assert!(r.seen.lock().unwrap().is_empty());
    // And the session's own repository lists.
    let out = ok(git(
        &r.workspace,
        &r.none,
        &[
            "-c",
            &header,
            "ls-remote",
            &format!("{base}/work/{o}/r.git"),
        ],
    ));
    assert!(out.contains("refs/heads/main"), "{out}");
}
