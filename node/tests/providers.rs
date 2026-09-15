//! Connecting a provider: the node runs the subscription's OAuth sign-in
//! itself, against a fake of the provider's endpoints, and keeps the tokens in
//! the broker as an `oauth` credential pinned to this node.

#[path = "support/mod.rs"]
mod support;
use support::oauth_fake::{self, OAuthFake};
use support::state;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracon::{
    broker::{Broker, Credential, SharedBroker, KIND_OAUTH},
    config::Config,
    oauth::Endpoints,
    providers::{LoginCompletion, LoginOwner, ProviderError, Providers},
    stream::Bus,
};

fn providers(endpoints: Endpoints) -> (Arc<Providers>, SharedBroker, Bus) {
    let bus = Bus::new();
    let broker = Broker::default().shared();
    let p = Providers::with_endpoints(
        Arc::new(Config::default()),
        broker.clone(),
        proto::envelope::DataKey::from_bytes([9u8; 32]),
        endpoints,
        "n1".into(),
        bus.clone(),
    );
    (p, broker, bus)
}

fn listed(p: &Providers, name: &str) -> serde_json::Value {
    p.list_private()
        .into_iter()
        .find(|value| value["name"] == name)
        .unwrap()
}

fn env(broker: &SharedBroker, credential: &str, key: &str) -> Option<String> {
    broker
        .read()
        .unwrap()
        .get(credential)
        .and_then(|c| c.env.get(key).cloned())
}

async fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// A node the operator's browser is not on: the redirect goes to Anthropic's
/// own page, which shows `code#state`, and the paste is exchanged by the node.
/// A refresh then renews in place and keeps the bindings.
#[tokio::test]
async fn an_anthropic_sign_in_from_another_device_completes_by_paste_and_refreshes() {
    state::isolate();
    let fake = OAuthFake::start().await;
    let (p, broker, bus) = providers(fake.endpoints());
    let mut frames = bus.subscribe();
    let phone = LoginOwner::Peer("phone".into());
    let mut stale = Credential {
        kind: KIND_OAUTH.into(),
        provider: Some("anthropic".into()),
        ..Default::default()
    };
    stale.env.insert("STALE".into(), "must-go".into());
    broker.write().unwrap().put("anthropic", stale);

    let result = p
        .connect("anthropic", vec!["work".into()], phone.clone(), false)
        .await
        .unwrap();
    assert_eq!(result.completion, LoginCompletion::Paste);
    assert_eq!(result.completion_note, None);
    let authorized = fake.authorize(&result.url);
    assert_eq!(
        authorized.redirect,
        tracon::oauth::anthropic::HOSTED_REDIRECT
    );
    assert_eq!(authorized.scope, "user:inference");
    assert!(!result.url.contains("expires_in"), "{}", result.url);

    // Resuming returns the same sign-in; anyone else is refused.
    assert_eq!(
        p.connect("anthropic", vec![], phone.clone(), false)
            .await
            .unwrap(),
        result
    );
    assert!(matches!(
        p.connect("anthropic", vec![], LoginOwner::Local, true)
            .await
            .unwrap_err(),
        ProviderError::Busy(_)
    ));
    assert!(matches!(
        p.code("anthropic", "x", &LoginOwner::Local)
            .await
            .unwrap_err(),
        ProviderError::WrongOwner
    ));
    let public = p
        .list_public()
        .into_iter()
        .find(|value| value["name"] == "anthropic")
        .unwrap();
    assert_eq!(public["state"], "pending");
    assert!(public.get("url").is_none());

    // A code from another sign-in, and one the provider refuses, both leave
    // this one waiting for the right paste.
    let refused = p
        .code(
            "anthropic",
            &format!("{}#not-this-state", oauth_fake::GOOD_CODE),
            &phone,
        )
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("different sign-in"),
        "{refused}"
    );
    let refused = p
        .code(
            "anthropic",
            &format!("wrong-code#{}", authorized.state),
            &phone,
        )
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("invalid_grant"), "{refused}");
    assert_eq!(listed(&p, "anthropic")["state"], "pending");

    p.code(
        "anthropic",
        &format!("  {}#{} \n", oauth_fake::GOOD_CODE, authorized.state),
        &phone,
    )
    .await
    .unwrap();
    {
        let b = broker.read().unwrap();
        let c = b.get("anthropic").unwrap();
        assert_eq!(c.kind, "oauth");
        assert_eq!(c.channels, ["work"]);
        assert_eq!(c.nodes, ["n1"]);
        assert_eq!(c.identity.as_deref(), Some(oauth_fake::EMAIL));
        assert!(!c.env.contains_key("STALE"));
        assert!(!c.env.contains_key("CHATGPT_ACCOUNT_ID"));
    }
    assert_eq!(
        env(&broker, "anthropic", "ACCESS_TOKEN").as_deref(),
        Some("sk-ant-access-1")
    );
    assert_eq!(
        env(&broker, "anthropic", "REFRESH_TOKEN").as_deref(),
        Some("sk-ant-refresh-1")
    );
    assert_eq!(listed(&p, "anthropic")["state"], "connected");
    let mut seen = 0;
    while let Ok(frame) = frames.try_recv() {
        if matches!(frame, tracon::stream::Frame::Providers { .. }) {
            seen += 1;
        }
    }
    assert!(seen >= 2, "providers frames: {seen}");

    // Two hours left: not due now, due inside the half hour ahead.
    let refreshed: Arc<Mutex<Vec<(String, Credential)>>> = Arc::default();
    let sink = refreshed.clone();
    p.set_on_refreshed(Box::new(move |name, credential| {
        sink.lock()
            .unwrap()
            .push((name.to_string(), credential.clone()));
    }));
    let now = tracon::store::now_ms();
    assert!(p.due_for_refresh(now).is_empty());
    assert_eq!(p.due_for_refresh(now + 100 * 60 * 1000), ["anthropic"]);
    p.refresh("anthropic").await.unwrap();
    assert_eq!(
        env(&broker, "anthropic", "ACCESS_TOKEN").as_deref(),
        Some("sk-ant-access-2")
    );
    assert_eq!(
        env(&broker, "anthropic", "REFRESH_TOKEN").as_deref(),
        Some("sk-ant-refresh-2")
    );
    {
        let handed = refreshed.lock().unwrap();
        assert_eq!(handed.len(), 1);
        assert_eq!(handed[0].0, "anthropic");
        assert_eq!(handed[0].1.channels, ["work"]);
        assert_eq!(
            handed[0].1.env.get("ACCESS_TOKEN").map(String::as_str),
            Some("sk-ant-access-2")
        );
    }

    assert!(matches!(
        p.disconnect("anthropic", &phone).await.unwrap_err(),
        ProviderError::RemoteDisconnect
    ));
    p.disconnect("anthropic", &LoginOwner::Local).await.unwrap();
    assert!(broker.read().unwrap().get("anthropic").is_none());
}

/// The browser is on this node's host: Anthropic redirects to a loopback
/// listener the node opened, and the sign-in finishes without a paste.
#[tokio::test]
async fn an_anthropic_sign_in_on_this_host_returns_to_a_local_listener() {
    state::isolate();
    let fake = OAuthFake::start().await;
    let (p, broker, _bus) = providers(fake.endpoints());

    let result = p
        .connect("anthropic", vec!["work".into()], LoginOwner::Local, true)
        .await
        .unwrap();
    assert_eq!(result.completion, LoginCompletion::LocalCallback);
    let authorized = fake.authorize(&result.url);
    let redirect = reqwest::Url::parse(&authorized.redirect).unwrap();
    assert_eq!(redirect.host_str(), Some("localhost"));
    assert_eq!(redirect.path(), "/callback");
    let port = redirect.port().unwrap();

    let callback = |code: &str, state: &str| {
        reqwest::get(format!(
            "http://127.0.0.1:{port}/callback?code={code}&state={state}"
        ))
    };
    let forged = callback(oauth_fake::GOOD_CODE, "forged").await.unwrap();
    assert_eq!(forged.status(), reqwest::StatusCode::FORBIDDEN);
    let done = callback(oauth_fake::GOOD_CODE, &authorized.state)
        .await
        .unwrap();
    assert_eq!(done.status(), reqwest::StatusCode::OK);
    assert_eq!(
        env(&broker, "anthropic", "ACCESS_TOKEN").as_deref(),
        Some("sk-ant-access-1")
    );
    assert_eq!(listed(&p, "anthropic")["state"], "connected");
    // The listener goes with the sign-in.
    assert!(callback(oauth_fake::GOOD_CODE, &authorized.state)
        .await
        .is_err());
}

/// Codex on this host: the browser returns to OpenAI's one registered loopback
/// port, and the account id every Codex request carries is read from the
/// tokens.
#[tokio::test]
async fn a_codex_sign_in_on_this_host_returns_to_a_local_listener() {
    state::isolate();
    let fake = OAuthFake::start().await;
    let (p, broker, _bus) = providers(fake.endpoints());

    let result = p
        .connect("openai-codex", vec!["work".into()], LoginOwner::Local, true)
        .await
        .unwrap();
    assert_eq!(result.completion, LoginCompletion::LocalCallback);
    assert!(result
        .url
        .starts_with(&format!("{}/openai/oauth/authorize?", fake.base)));
    let authorized = fake.authorize(&result.url);
    let redirect = reqwest::Url::parse(&authorized.redirect).unwrap();
    assert_eq!(redirect.path(), "/auth/callback");

    // What the browser's address bar shows is pasteable too.
    p.code(
        "openai-codex",
        &format!(
            "{}?code={}&state={}",
            authorized.redirect,
            oauth_fake::GOOD_CODE,
            authorized.state
        ),
        &LoginOwner::Local,
    )
    .await
    .unwrap();
    assert_eq!(
        env(&broker, "openai-codex", "CHATGPT_ACCOUNT_ID").as_deref(),
        Some(oauth_fake::ACCOUNT)
    );
    assert_eq!(
        env(&broker, "openai-codex", "REFRESH_TOKEN").as_deref(),
        Some("codex-refresh-1")
    );
    assert_eq!(listed(&p, "openai-codex")["identity"], oauth_fake::EMAIL);

    p.refresh("openai-codex").await.unwrap();
    assert_eq!(
        env(&broker, "openai-codex", "REFRESH_TOKEN").as_deref(),
        Some("codex-refresh-2")
    );
    assert_eq!(
        env(&broker, "openai-codex", "CHATGPT_ACCOUNT_ID").as_deref(),
        Some(oauth_fake::ACCOUNT)
    );
}

/// Codex anywhere else: a device code, which the node polls for until the
/// operator approves it. There is nothing to paste.
#[tokio::test]
async fn a_codex_sign_in_from_another_device_completes_by_device_code() {
    state::isolate();
    let fake = OAuthFake::start().await;
    let (p, broker, _bus) = providers(fake.endpoints());
    let phone = LoginOwner::Peer("phone".into());

    let result = p
        .connect("openai-codex", vec!["work".into()], phone.clone(), false)
        .await
        .unwrap();
    assert_eq!(result.completion, LoginCompletion::DeviceCode);
    assert_eq!(result.url, format!("{}/openai/codex/device", fake.base));
    assert_eq!(result.device_code.as_deref(), Some(oauth_fake::DEVICE_CODE));
    assert!(p
        .code("openai-codex", "anything", &phone)
        .await
        .unwrap_err()
        .to_string()
        .contains("nothing to paste"));

    eventually("a pending poll", || fake.with(|f| f.device_polls) >= 1).await;
    assert!(broker.read().unwrap().get("openai-codex").is_none());
    fake.with(|f| f.device_approved = true);
    eventually("the device grant", || {
        env(&broker, "openai-codex", "ACCESS_TOKEN").is_some()
    })
    .await;
    assert_eq!(
        env(&broker, "openai-codex", "CHATGPT_ACCOUNT_ID").as_deref(),
        Some(oauth_fake::ACCOUNT)
    );
    assert_eq!(listed(&p, "openai-codex")["state"], "connected");
}

/// OpenAI's app registers one loopback port. When something else holds it,
/// the sign-in is still offered, by device code, and says why.
#[tokio::test]
async fn a_codex_sign_in_whose_port_is_taken_falls_back_to_a_device_code() {
    state::isolate();
    let fake = OAuthFake::start().await;
    let occupier = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupier.local_addr().unwrap().port();
    let (p, _broker, _bus) = providers(Endpoints {
        codex_callback_port: port,
        ..fake.endpoints()
    });

    let result = p
        .connect("openai-codex", vec![], LoginOwner::Local, true)
        .await
        .unwrap();
    assert_eq!(result.completion, LoginCompletion::DeviceCode);
    assert!(
        result
            .completion_note
            .as_deref()
            .is_some_and(|note| note.contains(&port.to_string())),
        "{result:?}"
    );
    p.disconnect("openai-codex", &LoginOwner::Local)
        .await
        .unwrap();
    drop(occupier);
}

/// A denied device sign-in fails, and a cancelled one stops polling and keeps
/// nothing even if it is approved afterwards.
#[tokio::test]
async fn a_denied_or_cancelled_device_sign_in_keeps_nothing() {
    state::isolate();
    let fake = OAuthFake::start().await;
    let (p, broker, _bus) = providers(fake.endpoints());

    p.connect("openai-codex", vec![], LoginOwner::Local, false)
        .await
        .unwrap();
    fake.with(|f| f.device_denied = true);
    eventually("the denial", || {
        listed(&p, "openai-codex")["state"] == "failed"
    })
    .await;
    assert!(listed(&p, "openai-codex")["error"]
        .as_str()
        .unwrap()
        .contains("not authorized"));

    fake.with(|f| {
        f.device_denied = false;
        f.device_polls = 0;
    });
    p.connect("openai-codex", vec![], LoginOwner::Local, false)
        .await
        .unwrap();
    eventually("a pending poll", || fake.with(|f| f.device_polls) >= 1).await;
    p.disconnect("openai-codex", &LoginOwner::Local)
        .await
        .unwrap();
    let polls = fake.with(|f| {
        f.device_approved = true;
        f.device_polls
    });
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(fake.with(|f| f.device_polls) <= polls + 1);
    assert!(broker.read().unwrap().get("openai-codex").is_none());
    assert_eq!(listed(&p, "openai-codex")["state"], "disconnected");
}

#[tokio::test]
async fn invalid_providers_and_malformed_pastes_are_refused() {
    state::isolate();
    let fake = OAuthFake::start().await;
    let (p, broker, _bus) = providers(fake.endpoints());
    assert!(matches!(
        p.connect("openai", vec![], LoginOwner::Local, false)
            .await
            .unwrap_err(),
        ProviderError::NoLogin(_)
    ));
    assert!(matches!(
        p.connect("nope", vec![], LoginOwner::Local, false)
            .await
            .unwrap_err(),
        ProviderError::Unknown(_)
    ));
    assert!(matches!(
        p.code("anthropic", "x", &LoginOwner::Local)
            .await
            .unwrap_err(),
        ProviderError::NotPending(_)
    ));

    p.connect("anthropic", vec![], LoginOwner::Local, false)
        .await
        .unwrap();
    for bad in ["", "one\ntwo", "two words", "#only-state"] {
        assert!(
            matches!(
                p.code("anthropic", bad, &LoginOwner::Local)
                    .await
                    .unwrap_err(),
                ProviderError::Failed(_)
            ),
            "{bad:?}"
        );
    }
    p.disconnect("anthropic", &LoginOwner::Local).await.unwrap();
    assert!(broker.read().unwrap().is_empty());

    // A newer sign-in replaces a cancelled one; the old code is not its code.
    let first = p
        .connect("anthropic", vec![], LoginOwner::Local, false)
        .await
        .unwrap();
    let old_state = fake.authorize(&first.url).state;
    p.disconnect("anthropic", &LoginOwner::Local).await.unwrap();
    let second = p
        .connect("anthropic", vec![], LoginOwner::Local, false)
        .await
        .unwrap();
    assert_ne!(first.url, second.url);
    fake.authorize(&second.url);
    assert!(p
        .code(
            "anthropic",
            &format!("{}#{old_state}", oauth_fake::GOOD_CODE),
            &LoginOwner::Local
        )
        .await
        .is_err());
}

/// A refresh the provider refuses — the refresh token expired, was revoked, or
/// was already rotated by another holder — stops the loop and asks for a new
/// sign-in; so does a token that never had a refresh token.
#[tokio::test]
async fn a_credential_that_cannot_be_renewed_asks_for_a_new_sign_in() {
    state::isolate();
    let fake = OAuthFake::start().await;
    fake.with(|f| f.expires_in = 60);
    let (p, broker, _bus) = providers(fake.endpoints());

    let result = p
        .connect("anthropic", vec!["work".into()], LoginOwner::Local, false)
        .await
        .unwrap();
    let authorized = fake.authorize(&result.url);
    p.code(
        "anthropic",
        &format!("{}#{}", oauth_fake::GOOD_CODE, authorized.state),
        &LoginOwner::Local,
    )
    .await
    .unwrap();
    assert_eq!(p.due_for_refresh(tracon::store::now_ms()), ["anthropic"]);

    fake.with(|f| f.reject_refresh = true);
    let loop_task = tokio::spawn(p.clone().refresh_loop());
    eventually("the loop to notice", || {
        listed(&p, "anthropic")["state"] != "connected"
    })
    .await;
    loop_task.abort();
    let state = listed(&p, "anthropic");
    assert_eq!(state["state"], "needs_reconnect");
    assert!(state["error"]
        .as_str()
        .unwrap()
        .contains("connect the provider again"));
    assert_eq!(fake.with(|f| f.refreshes), 1);
    assert!(p.due_for_refresh(tracon::store::now_ms()).is_empty());
    assert!(broker.read().unwrap().get("anthropic").is_some());

    // A year-long `claude setup-token` credential has no refresh token.
    {
        let mut b = broker.write().unwrap();
        let mut c = b.get("anthropic").unwrap().clone();
        c.env.remove("REFRESH_TOKEN");
        b.put("anthropic", c);
    }
    assert!(matches!(
        p.refresh("anthropic").await.unwrap_err(),
        ProviderError::ReconnectRequired(_)
    ));
}

/// A refresh rotates the refresh token and revokes the access token it
/// replaced, so a credential shared across nodes has one renewer: the node
/// that signed in, first in `nodes`. Another holder waits for its copy.
#[test]
fn only_the_node_that_signed_in_renews_a_shared_credential() {
    state::isolate();
    let (p, broker, _bus) = providers(Endpoints::default());
    let soon = tracon::store::now_ms() + 60 * 1000;
    let credential = |nodes: &[&str]| {
        let mut c = Credential {
            kind: KIND_OAUTH.into(),
            provider: Some("anthropic".into()),
            channels: vec!["work".into()],
            nodes: nodes.iter().map(|n| n.to_string()).collect(),
            expires_ms: Some(soon),
            ..Default::default()
        };
        c.env.insert("ACCESS_TOKEN".into(), "at".into());
        c.env.insert("REFRESH_TOKEN".into(), "rt".into());
        c
    };
    let now = tracon::store::now_ms();

    broker
        .write()
        .unwrap()
        .put("anthropic", credential(&["n1", "n2"]));
    assert_eq!(p.due_for_refresh(now), ["anthropic"]);
    broker
        .write()
        .unwrap()
        .put("anthropic", credential(&["n2", "n1"]));
    assert!(p.due_for_refresh(now).is_empty());
    broker.write().unwrap().put("anthropic", credential(&[]));
    assert_eq!(p.due_for_refresh(now), ["anthropic"]);
}

/// The catalogue a node with only a Codex subscription connected can offer:
/// the providers it cannot spend on are never wired at all.
#[test]
fn only_a_connected_provider_is_wired() {
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
    // Wiring `openai` with no credential behind it is what put Codex models
    // under that provider: the harness offers everything it is handed.
    assert_eq!(
        wiring
            .providers
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["openai-codex"]
    );
    assert_eq!(wiring.token, "tok");
    assert!(!wiring
        .env
        .iter()
        .any(|(key, _)| key.starts_with("ANTHROPIC_")));
}
