//! A session's own egress grant, for either harness.
//!
//! Where the runtime issues grants, a session holds one for its life,
//! whichever harness runs it: the grant's proxy URL reaches the harness as
//! secret environment (never plain), the gateway host stays off every proxy
//! through `NO_PROXY` so the model and the node's tools are reached directly,
//! and `request_egress` has the session's grant to widen. The grant goes when
//! the session does.

#[path = "support/mod.rs"]
mod support;
use support::fake::NamedFake;
use support::state;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracon::{
    boundary::{Backend, BoundaryError, BoundaryReport, EgressGrant},
    broker::Broker,
    config::{Config, Provider, SHAPE_ANTHROPIC, SHAPE_OPENAI},
    gateway::proxy::{GrantSpec, Grants},
    runner::{Mount, Runner},
    session::{Manager, NewSession, Phase},
    store::{now_ms, NodeRow, Store},
    stream::Bus,
};

const BROKER: &str = r#"
    [credentials.sub]
    kind = "oauth"
    provider = "anthropic"
    channels = ["personal"]
    [credentials.sub.env]
    ACCESS_TOKEN = "at"
    REFRESH_TOKEN = "rt"

    [credentials.oai]
    kind = "api_key"
    provider = "openai"
    channels = ["personal"]
    [credentials.oai.env]
    API_KEY = "k"
"#;

const PROXY: &str = "http://tracon-gw:7424";

/// `LocalBackend`, issuing egress grants as Podman does.
struct Granting {
    inner: tracon::runner::local::LocalBackend,
    grants: Grants,
    issued: Arc<std::sync::Mutex<Vec<GrantSpec>>>,
}

#[async_trait]
impl Backend for Granting {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
        self.inner.setup(cfg, rebuild).await
    }
    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport {
        self.inner.check_all(cfg, deep).await
    }
    fn runner(&self, extra: Vec<Mount>) -> Arc<dyn Runner> {
        self.inner.runner(extra)
    }
    fn runner_for(&self, harness_id: &str, extra: Vec<Mount>) -> Arc<dyn Runner> {
        self.inner.runner_for(harness_id, extra)
    }
    async fn import_volume(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        self.inner.import_volume(volume, source).await
    }
    async fn export_volume(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        self.inner.export_volume(volume, destination).await
    }
    fn harness_host(&self) -> String {
        "tracon-gw".into()
    }
    fn harness_home(&self) -> String {
        self.inner.harness_home()
    }
    async fn reconcile(&self, names: &[String]) {
        self.inner.reconcile(names).await
    }
    fn egress_grants(&self) -> Option<&Grants> {
        Some(&self.grants)
    }
    fn egress_grant(&self, spec: GrantSpec) -> Result<EgressGrant, BoundaryError> {
        self.issued.lock().unwrap().push(spec.clone());
        let username = spec.client.clone();
        let token = self
            .grants
            .issue(spec)
            .map_err(|error| BoundaryError::Other(error.to_string()))?;
        let (grants, revoked) = (self.grants.clone(), token.clone());
        Ok(EgressGrant::new(PROXY.into(), username, token, move || {
            grants.revoke(&revoked)
        }))
    }
}

fn provider(credential: &str, shape: &str) -> Provider {
    Provider {
        credential: credential.into(),
        upstream: "https://example.com".into(),
        shape: shape.into(),
        login: None,
        price: None,
        models: Vec::new(),
    }
}

fn repo() -> String {
    let repo = state::scratch("session-egress").join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
        vec!["config", "commit.gpgsign", "false"],
        vec!["commit", "-q", "--allow-empty", "-m", "init"],
    ] {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(&args)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }
    repo.to_string_lossy().into_owned()
}

fn spec(repo: &str, model: &str) -> NewSession {
    NewSession {
        channel: "personal".into(),
        repo_path: repo.into(),
        branch: None,
        work_item_id: None,
        model: model.into(),
        budget_tokens: None,
        initial_prompt: None,
        node_id: None,
        phase: Phase::Execute,
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

/// Both harnesses: the session holds its own grant, the harness gets its
/// proxy URL as secret environment and never as plain, the gateway host is
/// exempt from it, and the grant is gone once the session is.
#[tokio::test]
async fn every_session_holds_its_own_grant_whatever_its_harness() {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&NodeRow {
            id: "n1".into(),
            name: "test".into(),
            state: "ready".into(),
            failed_check: None,
            failed_detail: None,
            harness_id: "claude".into(),
            harness_pinned: "1.0.0".into(),
            harness_found: Some("1.0.0".into()),
            models_json: None,
            checked_at_ms: Some(now_ms()),
            is_self: 1,
            x25519_pub: None,
            last_seen_ms: None,
            reachable: 1,
            providers_json: None,
            app_version: None,
            wire_contract: None,
            policy_identity: None,
            policy_sha256: None,
            policy_receipt_v1: None,
            harnesses_json: None,
        })
        .unwrap();
    let mut cfg = Config::default();
    cfg.providers.clear();
    cfg.providers
        .insert("anthropic".into(), provider("sub", SHAPE_ANTHROPIC));
    cfg.providers
        .insert("openai".into(), provider("oai", SHAPE_OPENAI));
    let cfg = Arc::new(cfg);
    let broker: Broker = toml::from_str(BROKER).unwrap();
    let tools = Arc::new(tracon::mcp::Tools {
        broker: broker.shared(),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let issued: Arc<std::sync::Mutex<Vec<GrantSpec>>> = Default::default();
    let backend = Arc::new(Granting {
        inner: tracon::runner::local::LocalBackend,
        grants: Grants::default(),
        issued: issued.clone(),
    });
    let grants = backend.grants.clone();
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg.clone(),
        "n1".into(),
        tools.clone(),
        Default::default(),
        backend,
    );
    let _ = tools.session.set(tracon::mcp::SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    manager.record_egress_refusals();
    let mut fakes = Vec::new();
    for id in tracon::adapter::KNOWN {
        let fake = Arc::new(NamedFake::new(id));
        manager.set_adapter(fake.clone());
        fakes.push((*id, fake));
    }
    let repo = repo();

    for (model, harness) in [
        ("anthropic/claude-x", "claude"),
        ("openai/gpt-x", "opencode"),
    ] {
        let session = manager
            .create(spec(&repo, model))
            .await
            .unwrap_or_else(|e| panic!("{harness}: {e}"));
        let id = session.id.clone();
        assert_eq!(session.harness_id, harness);
        let fake = &fakes.iter().find(|(name, _)| *name == harness).unwrap().1;
        let launched = {
            let mut found = None;
            for _ in 0..500 {
                if let Some(launch) = fake.launched.lock().unwrap().last().cloned() {
                    found = Some(launch);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            found.unwrap_or_else(|| panic!("{harness} never launched"))
        };

        // The grant is this session's, and `request_egress` has it to widen.
        assert!(grants.holds_session(&id), "{harness}: no grant held");
        assert!(issued
            .lock()
            .unwrap()
            .iter()
            .any(|spec| spec.session_id.as_deref() == Some(id.as_str())));

        // Its proxy URL, with the password, is secret environment only.
        for name in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
            let value = launched
                .secret
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{harness}: {name} is not secret env"));
            assert!(value.starts_with("http://session:"), "{harness}: {value}");
            assert!(value.ends_with("@tracon-gw:7424"), "{harness}: {value}");
            assert!(
                !launched.env.iter().any(|(n, _)| n == name),
                "{harness}: {name} is also plain"
            );
        }

        // The model and the node's MCP server are on the gateway host, which
        // the runner exempts from every proxy: the grant never carries them.
        assert_eq!(
            tracon::runner::no_proxy("tracon-gw"),
            "tracon-gw,localhost,127.0.0.1,::1"
        );
        let base_urls: Vec<&String> = launched
            .env
            .iter()
            .filter(|(n, _)| n.ends_with("_BASE_URL"))
            .map(|(_, v)| v)
            .collect();
        for url in &base_urls {
            assert!(url.starts_with("http://tracon-gw:"), "{harness}: {url}");
        }

        // And `request_egress` puts a host to the operator for it, rather
        // than saying the session has no grant to open.
        let asked = tools
            .call(
                &tracon::mcp::CallContext::session(&id, "personal", "n1"),
                "request_egress",
                &serde_json::json!({ "host": "pypi.example.test", "wait_secs": 0 }),
            )
            .await
            .unwrap_or_else(|e| panic!("{harness}: {e}"));
        assert_eq!(asked["state"], "still_waiting", "{harness}: {asked}");

        manager.stop(&id).await.unwrap();
        let mut gone = false;
        for _ in 0..500 {
            if !grants.holds_session(&id) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "{harness}: the grant outlived its session");
    }
}
