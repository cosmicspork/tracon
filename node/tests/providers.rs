//! Connecting a provider: the harness's login runs as a node-owned subprocess,
//! the operator's paste-back reaches its stdin, and what it stores is lifted
//! into the broker as an `oauth` credential pinned to this node.

#[path = "support/mod.rs"]
mod support;
use support::login_fake::LoginFake;
use support::state;

use std::sync::{atomic::Ordering, Arc};

use tracon::{
    broker::{Broker, Credential, KIND_OAUTH},
    config::Config,
    providers::{LoginCompletion, LoginOwner, Providers},
    runner::local::LocalBackend,
    stream::Bus,
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
    *fake.account_id.lock().unwrap() = Some("acct-new".into());
    let mut previous = Credential {
        kind: KIND_OAUTH.into(),
        provider: Some("anthropic".into()),
        channels: vec!["old".into()],
        nodes: vec!["old-node".into()],
        ..Default::default()
    };
    previous
        .env
        .insert("ACCESS_TOKEN".into(), "old-access".into());
    previous
        .env
        .insert("REFRESH_TOKEN".into(), "old-refresh".into());
    previous
        .env
        .insert("CHATGPT_ACCOUNT_ID".into(), "acct-old".into());
    previous.env.insert("STALE".into(), "must-go".into());
    broker.write().unwrap().put("anthropic", previous);
    let mut frames = bus.subscribe();

    let result = p
        .connect("anthropic", vec!["work".into()], LoginOwner::Local, true)
        .await
        .unwrap();
    assert_eq!(result.url, "https://login.example/anthropic");
    assert_eq!(result.completion, LoginCompletion::Paste);
    let pending = p
        .list_private()
        .into_iter()
        .find(|value| value["name"] == "anthropic")
        .unwrap();
    assert_eq!(pending["state"], "pending");
    assert_eq!(pending["url"], result.url);
    let resumed = p
        .connect("anthropic", vec![], LoginOwner::Local, true)
        .await
        .unwrap();
    assert_eq!(resumed, result);
    assert!(matches!(
        p.connect("anthropic", vec![], LoginOwner::Peer("n2".into()), true,)
            .await
            .unwrap_err(),
        tracon::providers::ProviderError::Busy(_)
    ));
    assert!(matches!(
        p.code("anthropic", "code", &LoginOwner::Peer("n2".into()))
            .await
            .unwrap_err(),
        tracon::providers::ProviderError::WrongOwner
    ));
    let public = p
        .list_public()
        .into_iter()
        .find(|value| value["name"] == "anthropic")
        .unwrap();
    assert_eq!(public["state"], "pending");
    assert!(public.get("url").is_none());
    assert!(public.get("completion").is_none());
    assert!(public.get("completion_note").is_none());
    assert!(public.get("error").is_none());

    p.code("anthropic", "  the-code \n", &LoginOwner::Local)
        .await
        .unwrap();
    // The subprocess ends and the lift happens off the request path.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if broker
                .read()
                .unwrap()
                .get("anthropic")
                .and_then(|credential| credential.env.get("ACCESS_TOKEN"))
                .is_some_and(|token| token == "anthropic:the-code")
            {
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
        assert_eq!(
            c.env.get("CHATGPT_ACCOUNT_ID").map(String::as_str),
            Some("acct-new")
        );
        assert!(!c.env.contains_key("STALE"));
        assert_eq!(c.env.get("REFRESH_TOKEN").map(String::as_str), Some("rt"));
        assert_eq!(c.identity.as_deref(), Some("op@example"));
    }
    let listed = p
        .list_private()
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
    *fake.account_id.lock().unwrap() = None;
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
        assert_eq!(
            c.env.get("CHATGPT_ACCOUNT_ID").map(String::as_str),
            Some("acct-new")
        );
        assert_eq!(c.channels, vec!["work".to_string()]);
    }
    assert!(matches!(
        p.disconnect("anthropic", &LoginOwner::Peer("n2".into()))
            .await
            .unwrap_err(),
        tracon::providers::ProviderError::RemoteDisconnect
    ));
    assert!(broker.read().unwrap().get("anthropic").is_some());

    p.disconnect("anthropic", &LoginOwner::Local).await.unwrap();
    assert!(broker.read().unwrap().get("anthropic").is_none());
}

#[tokio::test]
async fn invalid_provider_and_manual_completion_are_refused() {
    state::isolate();
    let fake = Arc::new(LoginFake::default());
    let (p, broker, _bus) = providers("failures", fake);
    assert!(matches!(
        p.connect("openai", vec![], LoginOwner::Local, false)
            .await
            .unwrap_err(),
        tracon::providers::ProviderError::NoLogin(_)
    ));
    assert!(matches!(
        p.connect("nope", vec![], LoginOwner::Local, false)
            .await
            .unwrap_err(),
        tracon::providers::ProviderError::Unknown(_)
    ));
    assert!(matches!(
        p.code("anthropic", "x", &LoginOwner::Local)
            .await
            .unwrap_err(),
        tracon::providers::ProviderError::NotPending(_)
    ));

    assert!(matches!(
        p.connect("anthropic", vec![], LoginOwner::Local, false)
            .await
            .unwrap_err(),
        tracon::providers::ProviderError::RequiresLocalCallback(_)
    ));
    p.connect("anthropic", vec![], LoginOwner::Local, true)
        .await
        .unwrap();
    assert!(matches!(
        p.code("anthropic", "", &LoginOwner::Local)
            .await
            .unwrap_err(),
        tracon::providers::ProviderError::Failed(_)
    ));
    assert!(matches!(
        p.code("anthropic", "one\ntwo", &LoginOwner::Local)
            .await
            .unwrap_err(),
        tracon::providers::ProviderError::Failed(_)
    ));
    p.disconnect("anthropic", &LoginOwner::Local).await.unwrap();
    assert!(broker.read().unwrap().is_empty());
}

#[tokio::test]
async fn remote_codex_uses_device_authorization() {
    state::isolate();
    let fake = Arc::new(LoginFake::default());
    let (p, _broker, _bus) = providers("device_login", fake);

    let result = p
        .connect(
            "openai-codex",
            vec![],
            LoginOwner::Peer("phone".into()),
            false,
        )
        .await
        .unwrap();
    assert_eq!(result.completion, LoginCompletion::DeviceCode);
    assert_eq!(result.url, "https://login.example/openai-codex-device");
    assert_eq!(result.device_code.as_deref(), Some("ABCD-1234"));

    p.disconnect("openai-codex", &LoginOwner::Peer("phone".into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_cancelled_startup_cannot_remove_the_next_login_generation() {
    state::isolate();
    let fake = Arc::new(LoginFake::default());
    fake.login_delay_ms.store(100, Ordering::SeqCst);
    let (p, _broker, _bus) = providers("startup_generation", fake.clone());

    let first = {
        let p = p.clone();
        tokio::spawn(async move {
            p.connect("anthropic", vec!["old".into()], LoginOwner::Local, true)
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while fake.login_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert!(matches!(
        p.connect("anthropic", vec![], LoginOwner::Peer("peer".into()), true,)
            .await
            .unwrap_err(),
        tracon::providers::ProviderError::Busy(_)
    ));
    p.disconnect("anthropic", &LoginOwner::Local).await.unwrap();

    let second = p
        .connect("anthropic", vec!["new".into()], LoginOwner::Local, true)
        .await
        .unwrap();
    assert!(first.await.unwrap().is_err());
    let pending = p
        .list_private()
        .into_iter()
        .find(|value| value["name"] == "anthropic")
        .unwrap();
    assert_eq!(pending["state"], "pending");
    assert_eq!(pending["url"], second.url);
    p.disconnect("anthropic", &LoginOwner::Local).await.unwrap();
}

/// The catalogue a node with only a Codex subscription connected can offer:
/// the providers it cannot spend on are never wired, and what the account
/// refuses is never listed.
#[test]
fn only_a_connected_provider_is_wired_and_the_account_decides_the_rest() {
    state::isolate();
    let cfg = Config::default();
    let shared = Broker::default().shared();
    let mut codex = Credential {
        kind: KIND_OAUTH.into(),
        provider: Some("openai-codex".into()),
        channels: vec!["work".into()],
        nodes: vec!["n1".into()],
        ..Default::default()
    };
    codex.env.insert("ACCESS_TOKEN".into(), "access".into());
    codex
        .env
        .insert("CHATGPT_ACCOUNT_ID".into(), "acct-1".into());
    shared.write().unwrap().put("openai-codex", codex);

    let broker = shared.read().unwrap();
    let wiring = tracon::gateway::model::harness_wiring(&cfg, "tracon-gw", "tok", |_, provider| {
        broker
            .inject_for_probe(&provider.credential, "n1", &provider.shape)
            .is_ok()
    });
    let wired: serde_json::Value = serde_json::from_str(&wiring.models_json).unwrap();
    assert_eq!(wired["providers"]["openai-codex"]["apiKey"], "tok");
    // Wiring `openai` with no credential behind it is what put Codex models
    // under that provider: the harness offers everything it is handed.
    assert!(wired["providers"]["openai"].is_null());
    assert!(!wiring
        .env
        .iter()
        .any(|(key, _)| key.starts_with("ANTHROPIC_")));

    let probed = ["gpt-5.3-codex-spark", "gpt-5.4", "gpt-daybreak-blue-latest"]
        .map(|model| tracon::adapter::ModelOption {
            value: format!("openai-codex/{model}"),
            name: model.into(),
        })
        .to_vec();
    let offered = tracon::gateway::model::offerable(&cfg, probed, |provider| {
        broker
            .model_credential_for(provider, "n1")
            .is_some_and(|(_, credential)| credential.kind == KIND_OAUTH)
    });
    assert_eq!(
        offered
            .iter()
            .map(|model| model.value.as_str())
            .collect::<Vec<_>>(),
        [
            "openai-codex/gpt-5.3-codex-spark",
            "openai-codex/gpt-daybreak-blue-latest"
        ]
    );
}
