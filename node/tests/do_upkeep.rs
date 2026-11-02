//! What the node removes from DigitalOcean on its own, against a stand-in
//! for the API: orphaned sessions at start, configs their sessions left,
//! every checkpoint but each environment's newest ready one, and templates
//! for images it no longer uses. Never another node's.

#[path = "support/mod.rs"]
mod support;

use tracon::provider::digitalocean::templates::Templates;
use tracon::provider::digitalocean::{upkeep, Owner};

use support::do_api::FakeDo;

const IMAGE: &str =
    "ghcr.io/example/tracon-do-sandbox@sha256:1111111111111111222222222222222222222222222222222222222222222222";

#[tokio::test]
async fn orphans_go_at_start_and_only_this_nodes() {
    let fake = FakeDo::start().await;
    let owner = Owner::new("tracon", "abcd1234");
    let (mine, theirs, hand) = {
        let mut api = fake.api.lock().unwrap();
        (
            api.put_session("tracon-h-a-nabcd1234", "SESSION_STATUS_READY"),
            api.put_session("tracon-h-a-nffff0000", "SESSION_STATUS_READY"),
            api.put_session("by-hand", "SESSION_STATUS_PAUSED"),
        )
    };
    let swept = upkeep::remove_orphans(&fake.client(), &owner)
        .await
        .unwrap();
    assert_eq!(swept.sessions, ["tracon-h-a-nabcd1234"]);
    assert_eq!(swept.configs.len(), 1);
    let api = fake.api.lock().unwrap();
    assert!(!api.sessions.contains_key(&mine));
    assert!(api.sessions.contains_key(&theirs));
    assert!(api.sessions.contains_key(&hand));
}

#[tokio::test]
async fn a_sweep_keeps_one_checkpoint_per_environment_and_no_leftovers() {
    let fake = FakeDo::start().await;
    let owner = Owner::new("tracon", "abcd1234");
    let templates = Templates::new(IMAGE, "sandbox", &owner.template_stem()).unwrap();
    let (keep, older, failed, pending, theirs_cp, leftover, live_config) = {
        let mut api = fake.api.lock().unwrap();
        // A preparation's sandboxes: one environment across two sessions.
        let a = api.put_session("tracon-prep-0f3a-11-nabcd1234", "SESSION_STATUS_PAUSED");
        let b = api.put_session("tracon-prep-0f3a-12-nabcd1234", "SESSION_STATUS_READY");
        let older = api.put_checkpoint(&a, "READY", "2026-11-02T01:00:00Z", 51_000_000_000);
        let failed = api.put_checkpoint(&a, "FAILED", "2026-11-02T01:30:00Z", 0);
        let keep = api.put_checkpoint(&b, "READY", "2026-11-02T02:00:00Z", 51_000_000_000);
        let pending = api.put_checkpoint(&b, "PENDING", "2026-11-02T03:00:00Z", 0);
        // Another node's session and checkpoints are not this node's.
        let other = api.put_session("tracon-prep-0f3a-nffff0000", "SESSION_STATUS_READY");
        let theirs_cp = api.put_checkpoint(&other, "READY", "2026-11-01T00:00:00Z", 51_000_000_000);
        // A session that was removed left its config behind.
        let gone = api.put_session("tracon-h-gone-nabcd1234", "SESSION_STATUS_READY");
        let leftover = api.sessions[&gone]["config_id"]
            .as_str()
            .unwrap()
            .to_string();
        api.sessions.remove(&gone);
        let live_config = api.sessions[&b]["config_id"].as_str().unwrap().to_string();
        // A template for an image no longer used, and the current one.
        api.put_template(
            &format!("{}-3333333333333333", owner.template_stem()),
            "TEMPLATE_STATUS_READY",
        );
        api.put_template(templates.name(), "TEMPLATE_STATUS_READY");
        (
            keep,
            older,
            failed,
            pending,
            theirs_cp,
            leftover,
            live_config,
        )
    };

    let swept = upkeep::sweep(&fake.client(), &owner, &templates, tracon::store::now_ms())
        .await
        .unwrap();
    assert!(swept.failed.is_empty(), "{:?}", swept.failed);
    assert_eq!(swept.freed_bytes, 51_000_000_000);
    let mut deleted = swept.checkpoints.clone();
    deleted.sort();
    let mut expected = vec![older.clone(), failed.clone()];
    expected.sort();
    assert_eq!(deleted, expected);
    {
        let api = fake.api.lock().unwrap();
        let all: Vec<String> = api
            .checkpoints
            .values()
            .flatten()
            .map(|c| c["checkpoint_id"].as_str().unwrap().to_string())
            .collect();
        for kept in [&keep, &pending, &theirs_cp] {
            assert!(all.contains(kept), "{kept} was removed");
        }
        assert!(api.deleted_configs.contains(&leftover));
        assert!(!api.deleted_configs.contains(&live_config));
        // Sessions of this node still run, so no template goes yet.
        assert!(api.deleted_templates.is_empty());
    }

    // Once none runs, the stale template goes and the current one stays.
    fake.api
        .lock()
        .unwrap()
        .sessions
        .retain(|_, s| !owner.owns(s["name"].as_str().unwrap_or("")));
    let swept = upkeep::sweep(&fake.client(), &owner, &templates, tracon::store::now_ms())
        .await
        .unwrap();
    assert_eq!(
        swept.templates,
        [format!("{}-3333333333333333", owner.template_stem())]
    );
    let api = fake.api.lock().unwrap();
    assert!(api
        .templates
        .values()
        .any(|t| t["name"] == templates.name()));
}
