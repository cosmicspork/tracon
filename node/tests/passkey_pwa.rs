//! Pairing, passkey enrollment, and a passkey-confirmed admin action, in a
//! real browser.
//!
//! The node is served on `localhost`, the one origin a browser lets WebAuthn
//! run on without TLS, with `[auth] public_origin` pinned to it. A browser on
//! the same machine is the operator, though, and would never be asked for a
//! passkey. So the test's own outermost layer marks every request the way a
//! reverse proxy on the node's host does (`X-Forwarded-For`), which makes
//! each one remote to the guard, exactly as a phone's request through an
//! ingress is.
//!
//! The authenticator is Chromium's virtual one, driven over CDP by
//! `spa/tests/passkey-driver.mjs`; the verifier is the node's. Skipped, saying
//! why, where there is no built SPA, no `bun`, or no Chromium.

#[path = "support/mod.rs"]
mod support;
use support::fake::FakeAdapter;
use support::state;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use serde_json::Value;

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

const TOKEN: &str = "trc1.paired-by-qr";

/// What a reverse proxy on the node's own host adds for a caller elsewhere.
async fn through_a_proxy(mut req: Request, next: Next) -> Response {
    req.headers_mut()
        .insert("x-forwarded-for", HeaderValue::from_static("198.51.100.4"));
    next.run(req).await
}

fn which(binary: &str) -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(binary))
        .find(|c| c.is_file())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paired_phone_enrolls_a_passkey_and_confirms_an_admin_action_with_it() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let spa = repo.join("spa");
    if !spa.join("node_modules/playwright-core").is_dir() {
        eprintln!(
            "skipped: the SPA's dev dependencies are not installed; run `bun install` in spa/"
        );
        return;
    }
    let index = std::fs::read_to_string(spa.join("dist/index.html")).unwrap_or_default();
    if !index.contains("/assets/") {
        eprintln!("skipped: spa/dist is the placeholder page; run `just spa`");
        return;
    }
    let Some(bun) = which("bun") else {
        eprintln!("skipped: bun is not on PATH");
        return;
    };

    state::isolate();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://localhost:{port}");

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    store.set_operator_token(Some(&auth::hash(TOKEN))).unwrap();
    let mut cfg = Config::default();
    cfg.auth.public_origin = Some(origin.clone());
    let cfg = Arc::new(cfg);
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
    let app_state = AppState {
        manager,
        cfg,
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: Arc::new(AuthState::load(&store, "127.0.0.1".into())),
        enroll: Default::default(),
    };
    let app = tracon::http::router(app_state.clone())
        .layer(axum::middleware::from_fn_with_state(app_state, auth::guard))
        .layer(axum::middleware::from_fn(through_a_proxy));
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let config = serde_json::json!({
        "origin": origin,
        "token": TOKEN,
        "timeout_ms": 20000,
        "executable": std::env::var("TRACON_CHROMIUM").ok(),
    });
    let out = tokio::process::Command::new(bun)
        .arg(spa.join("tests/passkey-driver.mjs"))
        .arg(config.to_string())
        .current_dir(&spa)
        .output()
        .await
        .expect("the driver runs");
    server.abort();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if let Some(why) = stdout.lines().find_map(|l| l.strip_prefix("SKIP ")) {
        eprintln!("skipped: {why}");
        return;
    }
    let Some(line) = stdout.lines().find_map(|l| l.strip_prefix("RESULT ")) else {
        panic!("the driver printed no result\nstdout:\n{stdout}\nstderr:\n{stderr}");
    };
    let saw: Value = serde_json::from_str(line).unwrap();
    let ctx = serde_json::to_string_pretty(&saw).unwrap();

    assert!(
        saw["failed_at"].is_null(),
        "the flow stopped part way:\n{ctx}"
    );
    // Pairing offered the first passkey without one, and the browser made it.
    assert_eq!(saw["offered"], true, "{ctx}");
    assert_eq!(saw["enrolled"], true, "{ctx}");
    assert_eq!(saw["authenticator_credentials"], 1, "{ctx}");
    assert_eq!(saw["listed"], 1, "{ctx}");
    // Removing it was refused, confirmed by the prompt, and then done.
    let action = saw["step_up_action"].as_str().unwrap_or_default();
    assert!(action.starts_with("DELETE /api/passkeys/"), "{ctx}");
    assert_eq!(saw["removed"], true, "{ctx}");
    assert_eq!(store.passkey_count().unwrap(), 0);
    let calls: Vec<(String, String, u64)> = saw["api"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["method"].as_str().unwrap().to_string(),
                c["path"].as_str().unwrap().to_string(),
                c["status"].as_u64().unwrap(),
            )
        })
        .collect();
    let deletes: Vec<u64> = calls
        .iter()
        .filter(|(m, p, _)| m == "DELETE" && p.starts_with("/api/passkeys/"))
        .map(|(_, _, s)| *s)
        .collect();
    assert_eq!(deletes, vec![403, 200], "refused, then confirmed:\n{ctx}");
    assert!(
        calls
            .iter()
            .any(|(m, p, s)| m == "POST" && p == "/api/passkeys/assert/finish" && *s == 200),
        "{ctx}"
    );
    assert!(
        saw["page_errors"].as_array().unwrap().is_empty(),
        "the page threw:\n{ctx}"
    );
}
