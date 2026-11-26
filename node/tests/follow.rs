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
        publication_failed: None,
        after: Vec::new(),
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

async fn mcp(app: &axum::Router, sid: &str, token: &str, name: &str, args: Value) -> Value {
    use tower::ServiceExt;
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/mcp/{sid}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(text).unwrap_or(json!({ "raw": text }))
}

const URL: &str = "https://github.test/owner/name/pull/7";

/// A forge serving PR 7 open, a session `s1` holding a new item, and its
/// review published as `p1`, carrying candidate `c1`, as approval leaves it.
async fn published_item(
    acceptance: &str,
) -> (
    support::harness::Harness,
    Shared,
    tracon_sync::work::WorkItem,
) {
    let h = harness().await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let forge = Shared::default();
    set(&forge, false, "open", false, 0, "success");
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
    let item = tracon::corpus::work::create(
        &h.store,
        &h.bus,
        &node,
        tracon::corpus::work::NewWork {
            channel: "personal".into(),
            project_id: None,
            title: "Ship the thing".into(),
            body: String::new(),
            deps: vec![],
            priority: 0,
            discovered_from: None,
            discovered_by_session: None,
            discovered_by_lane: None,
        },
    )
    .unwrap();
    let item = tracon::corpus::work::update(
        &h.store,
        &h.bus,
        &node,
        &item.id,
        tracon::corpus::work::Patch {
            acceptance: Some(acceptance.into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let mut session = session_row("s1", &node, "personal");
    session.work_item_id = Some(item.id.clone());
    h.store.insert_session(&session).unwrap();
    h.store.insert_review(&review(&node)).unwrap();
    open_publication(&h, "p1", "c1");
    (h, forge, item)
}

/// A publication of review `r1` that opened PR 7, and the item marked
/// published with its candidate, as approving it does.
fn open_publication(h: &support::harness::Harness, id: &str, candidate: &str) {
    let node = h.manager.node_id().to_string();
    h.store
        .publication_begin(&PublicationBegin {
            id,
            review_id: "r1",
            revision_id: None,
            candidate_id: candidate,
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
    h.store.publication_opened(id, URL).unwrap();
    let item = h
        .store
        .get_session("s1")
        .unwrap()
        .unwrap()
        .work_item_id
        .unwrap();
    tracon::corpus::work::record_published(&h.store, &h.bus, &node, &item, URL, candidate).unwrap();
}

/// A plain coding task ends at merge: published, it reads as waiting on the
/// merge (to the work views and to agents); merged, it closes and the
/// session holding it is told, with no step left for the operator.
#[tokio::test]
async fn a_merge_closes_the_task_it_published() {
    let (h, forge, item) = published_item("merge").await;
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;

    assert!(tracon::follow::tick(&h.tools).await.is_empty());
    let open = h.store.work_get(&item.id).unwrap().unwrap();
    assert_eq!(open.state, "open", "publishing does not close it");
    assert_eq!(open.change_state.as_deref(), Some("published"));
    assert_eq!(open.change_url.as_deref(), Some(URL));
    assert_eq!(open.landing(), Some(tracon_sync::work::Landing::Merge));

    // The agent reads the published-but-unmerged state.
    let shown = mcp(&h.harness, "s1", &token, "work_show", json!({})).await;
    assert_eq!(shown["acceptance"], "merge", "{shown}");
    assert_eq!(shown["change"]["url"], URL, "{shown}");
    assert_eq!(shown["change"]["state"], "published", "{shown}");
    assert_eq!(shown["change"]["awaiting"], "merge", "{shown}");
    let listed = mcp(
        &h.harness,
        "s1",
        &token,
        "work_list",
        json!({ "state": "published" }),
    )
    .await;
    assert_eq!(listed["items"][0]["id"], item.id, "{listed}");
    assert_eq!(
        listed["items"][0]["change"]["awaiting"], "merge",
        "{listed}"
    );
    // And so does the operator's ledger.
    let (_, v) = support::http::call(&h.operator, "GET", "/api/work?channel=personal", None).await;
    assert_eq!(v["items"][0]["change_state"], "published", "{v}");

    set(&forge, false, "closed", true, 0, "success");
    tracon::follow::tick(&h.tools).await;
    let closed = h.store.work_get(&item.id).unwrap().unwrap();
    assert_eq!(closed.state, "closed", "the merge closed it");
    assert_eq!(closed.change_state.as_deref(), Some("merged"));
    assert_eq!(closed.closed_by_session, None);
    let said: Vec<String> = h
        .store
        .events_after("s1", 0, 500)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(said.iter().any(|k| k == "work_closed"), "{said:?}");
}

/// An item set to need acceptance stays open after its merge until the
/// operator accepts it, and the acceptance is bound to the candidate it
/// judged: a new candidate inherits no earlier verdict.
#[tokio::test]
async fn a_task_that_needs_acceptance_waits_for_it_on_the_candidate_that_merged() {
    let (h, forge, item) = published_item("operator").await;
    let operator = |method: &'static str, uri: String, body: Option<Value>| {
        let app = h.operator.clone();
        async move { support::http::call(&app, method, &uri, body).await }
    };
    assert!(tracon::follow::tick(&h.tools).await.is_empty());

    // Accepted before it merged: the verdict is on `c1`.
    let (status, v) = operator(
        "POST",
        format!("/api/work/{}/accept", item.id),
        Some(json!({ "candidate": "c1" })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{v}");
    assert_eq!(v["state"], "open", "accepted, but not merged yet");
    assert_eq!(v["accepted_candidate"], "c1");

    // A new revision is published as `c2`: the verdict on `c1` is none on it.
    open_publication(&h, "p2", "c2");
    let now = h.store.work_get(&item.id).unwrap().unwrap();
    assert!(!now.accepted(), "a new candidate inherits no verdict");
    let (status, v) = operator(
        "POST",
        format!("/api/work/{}/accept", item.id),
        Some(json!({ "candidate": "c1" })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "{v}");

    // It merges: the look at the earlier publication says nothing of `c2`,
    // and the one at `c2` leaves the item open, waiting on the operator.
    set(&forge, false, "closed", true, 0, "success");
    tracon::follow::tick(&h.tools).await;
    let merged = h.store.work_get(&item.id).unwrap().unwrap();
    assert_eq!(merged.state, "open", "it waits for acceptance");
    assert_eq!(merged.change_state.as_deref(), Some("merged"));
    assert_eq!(
        merged.landing(),
        Some(tracon_sync::work::Landing::Acceptance)
    );
    let (_, view) = operator("GET", format!("/api/work/{}/continuation", item.id), None).await;
    assert_eq!(view["actions"]["accept"], "c2", "{view}");
    assert_eq!(view["next"]["kind"], "accept", "{view}");

    let (status, v) = operator(
        "POST",
        format!("/api/work/{}/accept", item.id),
        Some(json!({ "candidate": "c2" })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{v}");
    assert_eq!(v["state"], "closed", "{v}");
    let (_, view) = operator("GET", format!("/api/work/{}/continuation", item.id), None).await;
    assert_eq!(view["actions"]["accept"], Value::Null, "{view}");
    assert_eq!(view["next"]["kind"], "done", "{view}");
}
