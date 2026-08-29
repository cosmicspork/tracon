//! Connecting a provider: the harness's login runs as a node-owned subprocess,
//! the operator's paste-back reaches its stdin, and what it stores is lifted
//! into the broker as an `oauth` credential pinned to this node.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use tracon::{
    adapter::{
        AdapterError, HarnessAdapter, HarnessEvent, HarnessHandle, HarnessVersion, LaunchSpec,
        LiftedToken, LoginFlow, ModelOption,
    },
    broker::Broker,
    config::Config,
    providers::Providers,
    runner::{local::LocalBackend, Runner},
    stream::Bus,
};

/// A harness whose login prints a URL and then waits for one line on stdin;
/// the line becomes the access token it "stores".
#[derive(Default)]
struct LoginFake {
    stored: Arc<Mutex<Option<String>>>,
    refreshed: Arc<Mutex<u32>>,
}

#[async_trait]
impl HarnessAdapter for LoginFake {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn pinned_version(&self) -> &str {
        "1"
    }
    async fn version(&self, _r: &dyn Runner) -> Result<HarnessVersion, AdapterError> {
        Ok(HarnessVersion {
            found: "1".into(),
            pinned: "1".into(),
        })
    }
    async fn probe_models(
        &self,
        _r: &dyn Runner,
        _env: Vec<(String, String)>,
    ) -> Result<Vec<ModelOption>, AdapterError> {
        Ok(vec![])
    }
    async fn launch(
        &self,
        _r: &dyn Runner,
        _s: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError> {
        Err(AdapterError::Protocol("no".into()))
    }
    async fn login(
        &self,
        _r: &dyn Runner,
        provider: &str,
        _name: &str,
    ) -> Result<LoginFlow, AdapterError> {
        let (client, server) = tokio::io::duplex(1024);
        let stored = self.stored.clone();
        let provider = provider.to_string();
        let url = format!("https://login.example/{provider}");
        let output = Arc::new(Mutex::new(String::new()));
        let done = Box::pin(async move {
            let mut lines = tokio::io::BufReader::new(server).lines();
            match lines.next_line().await {
                Ok(Some(code)) if !code.is_empty() => {
                    *stored.lock().unwrap() = Some(format!("{provider}:{code}"));
                    Ok(0)
                }
                _ => Ok(1),
            }
        });
        Ok(LoginFlow {
            url,
            stdin: Box::new(client),
            done,
            output,
        })
    }
    async fn refresh(
        &self,
        _r: &dyn Runner,
        _provider: &str,
        _name: &str,
    ) -> Result<(), AdapterError> {
        *self.refreshed.lock().unwrap() += 1;
        if let Some(s) = self.stored.lock().unwrap().as_mut() {
            s.push_str("+r");
        }
        Ok(())
    }
    async fn lift(&self, _dir: &Path, _provider: &str) -> Result<LiftedToken, AdapterError> {
        let access = self
            .stored
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AdapterError::Protocol("nothing stored".into()))?;
        Ok(LiftedToken {
            access,
            refresh: Some("rt".into()),
            expires_ms: Some(tracon::store::now_ms() + 2 * 3600 * 1000),
            identity: Some("op@example".into()),
        })
    }
}

fn providers(
    name: &str,
    fake: Arc<LoginFake>,
) -> (Arc<Providers>, tracon::broker::SharedBroker, Bus) {
    let bus = Bus::new();
    let broker = Broker::default().shared();
    let cfg = Arc::new(Config::default()); // anthropic has a login; openai does not
    let p = Providers::new_in(
        state::scratch(name),
        cfg,
        broker.clone(),
        proto::envelope::DataKey::from_bytes([9u8; 32]),
        fake,
        Arc::new(LocalBackend),
        "n1".into(),
        bus.clone(),
    );
    (p, broker, bus)
}

#[tokio::test]
async fn connect_paste_back_lifts_the_token_into_the_broker() {
    state::isolate();
    let fake = Arc::new(LoginFake::default());
    let (p, broker, bus) = providers("connect_paste_back", fake.clone());
    let mut frames = bus.subscribe();

    let url = p.connect("anthropic", vec!["work".into()]).await.unwrap();
    assert_eq!(url, "https://login.example/anthropic");
    let pending = p
        .list()
        .into_iter()
        .find(|v| v["name"] == "anthropic")
        .unwrap();
    assert_eq!(pending["state"], "pending");
    assert_eq!(pending["url"], url);
    assert!(matches!(
        p.connect("anthropic", vec![]).await.unwrap_err(),
        tracon::providers::ProviderError::Busy(_)
    ));

    p.code("anthropic", "  the-code \n").await.unwrap();
    // The subprocess ends and the lift happens off the request path.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if broker.read().unwrap().get("anthropic").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("credential lifted");
    {
        let b = broker.read().unwrap();
        let c = b.get("anthropic").unwrap();
        assert_eq!(c.kind, "oauth");
        assert_eq!(c.provider.as_deref(), Some("anthropic"));
        assert_eq!(c.channels, vec!["work".to_string()]);
        assert_eq!(c.nodes, vec!["n1".to_string()]);
        assert_eq!(
            c.env.get("ACCESS_TOKEN").map(String::as_str),
            Some("anthropic:the-code")
        );
        assert_eq!(c.env.get("REFRESH_TOKEN").map(String::as_str), Some("rt"));
        assert_eq!(c.identity.as_deref(), Some("op@example"));
    }
    let listed = p
        .list()
        .into_iter()
        .find(|v| v["name"] == "anthropic")
        .unwrap();
    assert_eq!(listed["state"], "connected");
    assert_eq!(listed["identity"], "op@example");
    // The interface heard about it both times.
    let mut seen = 0;
    while let Ok(f) = frames.try_recv() {
        if matches!(f, tracon::stream::Frame::Providers { .. }) {
            seen += 1;
        }
    }
    assert!(seen >= 2, "providers frames: {seen}");

    // Refresh runs the harness's refresh and lifts again, keeping bindings.
    // The lifted token expires in two hours; it is due half an hour ahead.
    assert!(p.due_for_refresh(tracon::store::now_ms()).is_empty());
    assert!(p
        .due_for_refresh(tracon::store::now_ms() + 100 * 60 * 1000)
        .contains(&"anthropic".to_string()));
    p.refresh("anthropic").await.unwrap();
    assert_eq!(*fake.refreshed.lock().unwrap(), 1);
    {
        let b = broker.read().unwrap();
        let c = b.get("anthropic").unwrap();
        assert_eq!(
            c.env.get("ACCESS_TOKEN").map(String::as_str),
            Some("anthropic:the-code+r")
        );
        assert_eq!(c.channels, vec!["work".to_string()]);
    }

    p.disconnect("anthropic").await.unwrap();
    assert!(broker.read().unwrap().get("anthropic").is_none());
}

#[tokio::test]
async fn a_provider_without_a_login_flow_says_so_and_a_failed_login_is_reported() {
    state::isolate();
    let fake = Arc::new(LoginFake::default());
    let (p, broker, _bus) = providers("failures", fake);
    assert!(matches!(
        p.connect("openai", vec![]).await.unwrap_err(),
        tracon::providers::ProviderError::NoLogin(_)
    ));
    assert!(matches!(
        p.connect("nope", vec![]).await.unwrap_err(),
        tracon::providers::ProviderError::Unknown(_)
    ));
    assert!(matches!(
        p.code("anthropic", "x").await.unwrap_err(),
        tracon::providers::ProviderError::NotPending(_)
    ));

    p.connect("anthropic", vec![]).await.unwrap();
    // An empty paste-back makes the fake login exit non-zero.
    p.code("anthropic", "").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let v = p
                .list()
                .into_iter()
                .find(|v| v["name"] == "anthropic")
                .unwrap();
            if v["state"] == "failed" {
                assert!(v["error"].as_str().unwrap().contains("exited with 1"));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("failure recorded");
    assert!(broker.read().unwrap().is_empty());
}
