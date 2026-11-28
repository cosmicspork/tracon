//! `tracon setup` on a host whose node is running: the node runs the setup
//! in its own process, which is the only place the sandbox base it runs is
//! pinned. Until it is, the node's runtime check does not pass.

#[path = "support/mod.rs"]
mod support;
use support::harness::harness_on;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use tracon::boundary::{Backend, BoundaryError, BoundaryReport, CheckId, CheckResult};
use tracon::config::Config;
use tracon::runner::{local::LocalBackend, Mount, Runner};

const CONFIGURED: &str = "ghcr.io/cosmicspork/tracon-sandbox-base:9.9.9";
const PINNED: &str = "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:\
    1111111111111111111111111111111111111111111111111111111111111111";

/// A boundary the node sets up itself, whose setup pins the base as the
/// real ones do, and counts how often it ran.
#[derive(Default)]
struct Pinning {
    setups: AtomicUsize,
}

#[async_trait]
impl Backend for Pinning {
    fn kind(&self) -> &'static str {
        "local"
    }
    async fn setup(&self, _cfg: &Config, _rebuild: bool) -> Result<(), BoundaryError> {
        self.setups.fetch_add(1, Ordering::SeqCst);
        tracon::sandbox_images::pin(CONFIGURED, PINNED);
        Ok(())
    }
    async fn converge(&self, cfg: &Config) -> Result<bool, BoundaryError> {
        self.setup(cfg, false).await.map(|_| true)
    }
    fn converges(&self) -> bool {
        true
    }
    async fn check_all(&self, _cfg: &Config, _deep: bool) -> BoundaryReport {
        BoundaryReport {
            checks: vec![CheckResult::ok(CheckId::Runtime, "images present")],
        }
    }
    fn runner(&self, extra: Vec<Mount>) -> Arc<dyn Runner> {
        LocalBackend.runner(extra)
    }
    fn runner_for(&self, harness_id: &str, extra: Vec<Mount>) -> Arc<dyn Runner> {
        LocalBackend.runner_for(harness_id, extra)
    }
    async fn import_volume(&self, v: &str, s: &std::path::Path) -> Result<(), BoundaryError> {
        LocalBackend.import_volume(v, s).await
    }
    async fn export_volume(&self, v: &str, d: &std::path::Path) -> Result<(), BoundaryError> {
        LocalBackend.export_volume(v, d).await
    }
    async fn clone_volume(&self, s: &str, d: &str) -> Result<(), BoundaryError> {
        LocalBackend.clone_volume(s, d).await
    }
    fn harness_host(&self) -> String {
        LocalBackend.harness_host()
    }
    fn harness_home(&self) -> String {
        LocalBackend.harness_home()
    }
    async fn reconcile(&self, names: &[String]) {
        LocalBackend.reconcile(names).await
    }
}

#[tokio::test]
async fn tracon_setup_asks_the_running_node_which_pins_its_base() {
    let backend = Arc::new(Pinning::default());
    let h = harness_on(Config::default(), backend.clone()).await;
    let app = tracon::http::router(h.state.clone()).layer(axum::middleware::from_fn_with_state(
        h.state.clone(),
        tracon::http::auth::guard,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let check = || async {
        reqwest::Client::new()
            .post(format!("{url}/api/boundary/check"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    };

    // Set up in some other process: the files are right, the node unpinned.
    assert_eq!(tracon::sandbox_images::active_base(), None);
    let held = check().await;
    let runtime = &held["checks"]["checks"][0];
    assert_eq!(runtime["ok"], false, "{held}");
    assert!(
        runtime["detail"]
            .as_str()
            .unwrap()
            .contains("is not pinned yet"),
        "{held}"
    );
    assert_eq!(held["state"], "refused");

    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_tracon"))
        .arg("setup")
        .env("TRACON_URL", &url)
        .env_remove("TRACON_LISTEN")
        .env_remove("TRACON_TOKEN")
        .env("RUST_LOG", "error")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains(&format!("asking the node running here ({url})")),
        "{stderr}"
    );
    assert!(stdout.contains("the node set up its boundary"), "{stdout}");
    assert!(stdout.contains("ok   runtime"), "{stdout}");

    // The node ran it, and its own pin is set.
    assert_eq!(backend.setups.load(Ordering::SeqCst), 1);
    assert_eq!(
        tracon::sandbox_images::active_base().as_deref(),
        Some(PINNED)
    );
    let passed = check().await;
    assert_eq!(passed["checks"]["checks"][0]["ok"], true, "{passed}");
    assert_eq!(passed["state"], "ready", "{passed}");
    served.abort();
}
