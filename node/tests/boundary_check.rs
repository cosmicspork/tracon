//! `POST /api/boundary/check`, which `tracon check-boundary` asks when the
//! node serving on this host holds what a check run in its own process
//! would need: `deep` reaches the backend's checks as the CLI's flag does.

#[path = "support/mod.rs"]
mod support;
use support::harness::harness_on;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::boundary::{Backend, BoundaryError, BoundaryReport, CheckId, CheckResult};
use tracon::config::Config;
use tracon::runner::{local::LocalBackend, Mount, Runner};

/// The local backend, whose checks say whether they were asked to be deep.
#[derive(Default)]
struct Depth {
    asked: Mutex<Vec<bool>>,
}

#[async_trait]
impl Backend for Depth {
    fn kind(&self) -> &'static str {
        "local"
    }
    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
        LocalBackend.setup(cfg, rebuild).await
    }
    async fn check_all(&self, _cfg: &Config, deep: bool) -> BoundaryReport {
        self.asked.lock().unwrap().push(deep);
        BoundaryReport {
            checks: vec![CheckResult::ok(
                CheckId::Runtime,
                if deep { "deep" } else { "shallow" },
            )],
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

async fn check(app: &axum::Router, body: Option<Value>) -> Value {
    let request = Request::post("/api/boundary/check").header("host", "127.0.0.1");
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let res = app.clone().oneshot(request).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// With no body, as the interface asks, the checks are the quick ones;
/// `{"deep": true}`, as `check-boundary --deep` asks, runs the deep ones.
#[tokio::test]
async fn a_boundary_check_runs_deep_only_when_asked() {
    let depth = Arc::new(Depth::default());
    let h = harness_on(Config::default(), depth.clone()).await;
    let detail = |v: &Value| v["checks"]["checks"][0]["detail"].clone();

    assert_eq!(detail(&check(&h.operator, None).await), "shallow");
    assert_eq!(
        detail(&check(&h.operator, Some(json!({"deep": false}))).await),
        "shallow"
    );
    let deep = check(&h.operator, Some(json!({"deep": true}))).await;
    assert_eq!(detail(&deep), "deep", "{deep}");
    assert_eq!(deep["checks"]["checks"][0]["id"], "runtime");
    assert!(depth.asked.lock().unwrap().contains(&true));
}
