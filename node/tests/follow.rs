//! Following a published request on its forge: the first look is the
//! baseline, each later change is recorded on the submitting session and
//! named, and a request that closed is not read again.

#[path = "support/mod.rs"]
mod support;
use support::{harness::harness, rows::session_row};

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};
use tracon::broker::Credential;
use tracon::store::publication::PublicationBegin;
use tracon::store::ReviewRow;

/// The pull request the stub serves, and how many times it was read.
#[derive(Default)]
struct Forge {
    pr: Value,
    checks: Value,
    reads: usize,
}

type Shared = Arc<Mutex<Forge>>;

async fn stub(base: &str, forge: Shared) {
    let pr = {
        let forge = forge.clone();
        move || {
            let forge = forge.clone();
            async move {
                let mut f = forge.lock().unwrap();
                f.reads += 1;
                Json(f.pr.clone())
            }
        }
    };
    let checks = {
        let forge = forge.clone();
        move || {
            let forge = forge.clone();
            async move { Json(forge.lock().unwrap().checks.clone()) }
        }
    };
    let app = Router::new()
        .route("/repos/owner/name/pulls/7", get(pr))
        .route("/repos/owner/name/commits/abc/check-runs", get(checks))
        .route(
            "/repos/owner/name/pulls/7/reviews",
            get(|| async { Json(json!([])) }),
        );
    let l = tokio::net::TcpListener::bind(base).await.unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
}

fn review(node: &str) -> ReviewRow {
    let now = tracon::store::now_ms();
    ReviewRow {
        id: "r1".into(),
        session_id: Some("s1".into()),
        node_id: node.into(),
        channel: "personal".into(),
        kind: "pr".into(),
        title: "feat: the thing".into(),
        body: String::new(),
        edited_title: None,
        edited_body: None,
        provider: "github".into(),
        target: "{}".into(),
        diff: "+x".into(),
        files: "[]".into(),
        head_sha: "abc".into(),
        base_ref: "main".into(),
        added: 1,
        removed: 0,
        state: "published".into(),
        verdict_reason: None,
        publish_result: None,
        claimed_ms: None,
        created_ms: now,
        created_mono_ms: 0,
        resolved_mono_ms: None,
        updated_ms: now,
        checks_json: None,
        review_session_id: None,
        ai_verdict_json: None,
        revision_patch: None,
        lane: None,
    }
}

fn set(forge: &Shared, draft: bool, state: &str, merged: bool, comments: u64, test: &str) {
    let mut f = forge.lock().unwrap();
    f.pr = json!({
        "number": 7, "title": "t", "state": state, "draft": draft, "merged": merged,
        "head": { "ref": "feat", "sha": "abc" }, "base": { "ref": "main" },
        "comments": comments, "html_url": "https://github.test/owner/name/pull/7",
    });
    f.checks = json!({ "check_runs": [
        { "name": "test", "status": "completed", "conclusion": test },
    ] });
}

#[tokio::test]
async fn a_published_request_is_followed_until_it_closes() {
    let h = harness().await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let forge = Shared::default();
    set(&forge, true, "open", false, 0, "success");
    stub(&addr, forge.clone()).await;
    h.tools.broker.write().unwrap().put(
        "gh",
        Credential {
            env: BTreeMap::from([
                ("GH_TOKEN".to_string(), "t".to_string()),
                ("GITHUB_API".to_string(), format!("http://{addr}")),
            ]),
            channels: vec!["personal".to_string()],
            ..Credential::default()
        },
    );
    let node = h.manager.node_id().to_string();
    h.store
        .insert_session(&session_row("s1", &node, "personal"))
        .unwrap();
    h.store.insert_review(&review(&node)).unwrap();
    h.store
        .publication_begin(&PublicationBegin {
            id: "p1",
            review_id: "r1",
            revision_id: None,
            candidate_id: "c1",
            channel: "personal",
            node_id: &node,
            provider: "github",
            project: "owner/name",
            base: "main",
            branch: "feat",
            head_sha: "abc",
            instance: "i",
        })
        .unwrap();
    h.store
        .publication_opened("p1", "https://github.test/owner/name/pull/7")
        .unwrap();

    // What it already was is the baseline, not news.
    assert!(tracon::follow::tick(&h.tools).await.is_empty());
    let baseline = h
        .store
        .latest_event_payload("s1", "forge_follow", "p1")
        .unwrap()
        .expect("the first look is recorded");
    assert_eq!(baseline["snapshot"]["draft"], true);

    set(&forge, false, "open", false, 2, "failure");
    let moved = tracon::follow::tick(&h.tools).await;
    assert_eq!(moved.len(), 1);
    assert_eq!(
        moved[0].changes,
        ["marked ready", "CI failed on `test`", "2 new comments"]
    );
    let recorded = h
        .store
        .latest_event_payload("s1", "forge_follow", "p1")
        .unwrap()
        .unwrap();
    assert_eq!(recorded["review_id"], "r1");
    assert_eq!(recorded["changes"][1], "CI failed on `test`");

    // Nothing new is nothing recorded.
    assert!(tracon::follow::tick(&h.tools).await.is_empty());

    set(&forge, false, "closed", true, 2, "success");
    let merged = tracon::follow::tick(&h.tools).await;
    assert_eq!(merged[0].changes, ["merged", "CI passed"]);
    let reads = forge.lock().unwrap().reads;
    assert!(tracon::follow::tick(&h.tools).await.is_empty());
    assert_eq!(
        forge.lock().unwrap().reads,
        reads,
        "a closed request is not read again"
    );
}
