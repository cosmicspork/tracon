//! Connecting a provider: the harness's login runs as a node-owned subprocess,
//! the operator's paste-back reaches its stdin, and what it stores is lifted
//! into the broker as an `oauth` credential pinned to this node.

#[path = "support/mod.rs"]
mod support;
use support::login_fake::LoginFake;
use support::state;

use std::sync::Arc;

use tracon::{
    broker::Broker, config::Config, providers::Providers, runner::local::LocalBackend, stream::Bus,
};

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
