//! The work-level continuation view and its three verbs: continue, change
//! approach, abandon — for a work item, and for plain sessions that never
//! named one.

#[path = "support/mod.rs"]
mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};
use support::harness::{harness, Harness};
use support::http::call;
use support::rows::session_row;
use tracon::store::{now_ms, NewEvent, SessionRow};

fn ended(id: &str, at: i64, end: &str, from: Option<&str>) -> SessionRow {
    let mut r = session_row(id, "n1", "personal");
    r.state = if end == "error" { "failed" } else { "closed" }.into();
    r.end_reason = Some(end.into());
    r.created_ms = at;
    r.continued_from = from.map(str::to_string);
    r.parent_session = from.map(str::to_string);
    r
}

fn event(h: &Harness, session: &str, kind: &str, payload: Value) {
    h.store
        .append_event(&NewEvent {
            session_id: session.into(),
            work_item_id: None,
            kind: kind.into(),
            ref_id: None,
            payload,
            at_ms: now_ms(),
            mono_ms: 0,
        })
        .unwrap();
}

async fn ready(h: &Harness) {
    h.store
        .conn()
        .execute("UPDATE node SET state='ready' WHERE id='n1'", [])
        .unwrap();
}

/// A plain session gains the view without an item: its lineage is the work,
/// its first prompt the intent, and what the operator decided along the way
/// is gathered from every attempt. An unrelated session stays out of it.
#[tokio::test]
async fn a_plain_lineage_reads_as_one_piece_of_work() {
    let h = harness().await;
    h.store
        .insert_session(&ended("a", 1, "killed_user", None))
        .unwrap();
    h.store
        .insert_session(&ended("b", 2, "node_restart", Some("a")))
        .unwrap();
    h.store
        .insert_session(&ended("other", 3, "killed_user", None))
        .unwrap();
    event(
        &h,
        "a",
        "user_prompt",
        json!({ "text": "Make the ledger fast\nwith details" }),
    );
    h.store
        .conn()
        .execute(
            "INSERT INTO permission_request (id, session_id, node_id, rpc_id, title, options, state,
                 answer_option_id, created_ms, created_mono_ms, expires_ms)
             VALUES ('p1', 'a', 'n1', 0, 'Bash: cargo bench', '[]', 'answered', 'allow_once', 5, 0, 0),
                    ('p2', 'a', 'n1', 0, 'Bash: rm -rf', '[]', 'expired', NULL, 6, 0, 0)",
            [],
        )
        .unwrap();
    h.store
        .conn()
        .execute(
            "INSERT INTO operator_question (id, session_id, channel, node_id, prompt, choices_json,
                 state, answer_json, created_ms, answered_ms)
             VALUES ('q1', 'b', 'personal', 'n1', 'Keep the old index?', '[]', 'answered',
                     '{\"choice\":\"drop it\"}', 7, 8)",
            [],
        )
        .unwrap();

    for from in ["a", "b"] {
        let (st, v) = call(
            &h.operator,
            "GET",
            &format!("/api/sessions/{from}/continuation"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["kind"], "session");
        assert_eq!(v["id"], "a", "the lineage is named by its first attempt");
        let ids: Vec<_> = v["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(v["intent"]["source"], "prompt");
        assert_eq!(v["intent"]["title"], "Make the ledger fast");
        assert_eq!(v["next"]["kind"], "continue");
        assert_eq!(v["next"]["session_id"], "b");
        assert_eq!(v["actions"]["continue_from"], "b");
        assert_eq!(v["workspace"]["id"], "b");
        let answered = v["decisions"]["answered"].as_array().unwrap();
        assert_eq!(answered.len(), 2, "{answered:?}");
        assert_eq!(answered[0]["asked"], "Bash: cargo bench");
        assert_eq!(answered[0]["answer"], "allow_once");
        assert_eq!(answered[1]["kind"], "question");
        assert_eq!(answered[1]["answer"], "drop it");
    }
    let (st, _) = call(&h.operator, "GET", "/api/sessions/nope/continuation", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// Continuing, or changing the approach, makes a new attempt on the same
/// workspace with the lineage on its row; a changed approach reaches it in
/// the operator's words. Once per attempt, and never from a live one.
#[tokio::test]
async fn continuing_or_changing_the_approach_carries_the_work_on_once() {
    let h = harness().await;
    ready(&h).await;
    h.store
        .insert_session(&ended("a", 1, "error", None))
        .unwrap();
    let mut live = session_row("live", "n1", "personal");
    live.created_ms = 2;
    h.store.insert_session(&live).unwrap();

    let (st, v) = call(&h.operator, "GET", "/api/sessions/a/continuation", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["next"]["kind"], "change_approach");

    let (st, row) = call(
        &h.operator,
        "POST",
        "/api/continuation/continue",
        Some(json!({ "session_id": "a", "approach": "Profile before changing the index." })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{row}");
    assert_eq!(row["continued_from"], "a");
    assert_eq!(row["repo_path"], "workspace://a");
    let next = row["id"].as_str().unwrap();
    let note = h
        .store
        .get_draft(next)
        .unwrap()
        .map(|(text, _)| text)
        .unwrap_or_default();
    assert!(note.contains("continues session a"), "{note}");
    assert!(
        note.contains("Profile before changing the index."),
        "{note}"
    );
    assert!(note.contains("inherit none of its context"), "{note}");

    let (st, v) = call(
        &h.operator,
        "POST",
        "/api/continuation/continue",
        Some(json!({ "session_id": "a" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains(next), "{v}");

    let (st, v) = call(
        &h.operator,
        "POST",
        "/api/continuation/continue",
        Some(json!({ "session_id": "live" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
}

/// Abandoning a plain lineage stops what is live, puts every attempt away,
/// and says why on the latest; the view then has nothing left to do.
#[tokio::test]
async fn abandoning_a_plain_lineage_puts_it_away_with_the_reason() {
    let h = harness().await;
    h.store
        .insert_session(&ended("a", 1, "killed_user", None))
        .unwrap();
    h.store
        .insert_session(&ended("b", 2, "error", Some("a")))
        .unwrap();
    let (st, v) = call(
        &h.operator,
        "POST",
        "/api/continuation/abandon",
        Some(json!({ "session_id": "a", "reason": "superseded by the new design" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    for id in ["a", "b"] {
        assert!(h
            .store
            .get_session(id)
            .unwrap()
            .unwrap()
            .archived_ms
            .is_some());
    }
    let abandoned = h
        .store
        .events_after("b", 0, 50)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "abandoned")
        .expect("the reason is on the latest attempt");
    assert_eq!(abandoned.payload["reason"], "superseded by the new design");
    let (_, v) = call(&h.operator, "GET", "/api/sessions/b/continuation", None).await;
    assert_eq!(v["next"]["kind"], "done");
    assert_eq!(v["actions"]["abandon"], false);
    assert!(v["actions"]["continue_from"].is_null());

    let (st, _) = call(
        &h.operator,
        "POST",
        "/api/continuation/abandon",
        Some(json!({ "session_id": "a", "item_id": "x" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// For an item the attempts are the sessions that held it, the intent is the
/// item, its blockers are named by title, a written plan points at execute,
/// and abandoning closes it.
#[tokio::test]
async fn an_items_view_names_its_blockers_and_abandoning_closes_it() {
    let h = harness().await;
    let mk = |title: &str, deps: Vec<String>| {
        tracon::corpus::work::create(
            &h.store,
            &h.bus,
            "n1",
            tracon::corpus::work::NewWork {
                channel: "personal".into(),
                project_id: None,
                title: title.into(),
                body: "why it matters".into(),
                deps,
                priority: 0,
                discovered_from: None,
                discovered_by_session: None,
            },
        )
        .unwrap()
    };
    let first = mk("Lay the schema", vec![]);
    let second = mk("Build the ledger", vec![first.id.clone()]);

    let (st, v) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{}/continuation", second.id),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["kind"], "item");
    assert_eq!(v["intent"]["title"], "Build the ledger");
    assert_eq!(v["intent"]["source"], "item");
    assert_eq!(v["next"]["kind"], "unblock");
    assert!(
        v["next"]["text"]
            .as_str()
            .unwrap()
            .contains("Lay the schema"),
        "{v}"
    );

    let mut planned = ended("p", 1, "phase_done", None);
    planned.phase = "plan".into();
    planned.work_item_id = Some(first.id.clone());
    h.store.insert_session(&planned).unwrap();
    let (_, v) = call(&h.operator, "GET", "/api/sessions/p/continuation", None).await;
    assert_eq!(
        v["kind"], "item",
        "a session on an item opens the item's work"
    );
    assert_eq!(v["id"], first.id.as_str());
    assert_eq!(v["next"]["kind"], "execute");

    let (st, v) = call(
        &h.operator,
        "POST",
        "/api/continuation/abandon",
        Some(json!({ "item_id": first.id, "reason": "not needed" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        h.store.work_get(&first.id).unwrap().unwrap().state,
        tracon_sync::work::CLOSED
    );
    let (_, v) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{}/continuation", first.id),
        None,
    )
    .await;
    assert_eq!(v["next"]["kind"], "done");
    let (st, _) = call(
        &h.operator,
        "POST",
        "/api/continuation/abandon",
        Some(json!({ "item_id": first.id })),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CONFLICT,
        "an item already closed is not abandoned twice"
    );
}
