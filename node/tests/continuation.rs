//! The work-level continuation view and its three verbs: continue, change
//! approach, abandon — for a work item, and for plain sessions that never
//! named one.

#[path = "support/mod.rs"]
mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};
use support::harness::{harness, Harness};
use support::http::call;
use support::rows::{review_row, session_row};
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

fn new_item(h: &Harness, title: &str, deps: Vec<String>) -> tracon_sync::work::WorkItem {
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
            discovered_by_lane: None,
        },
    )
    .unwrap()
}

/// What a plan session leaves behind: the plan document, linked to the item.
fn write_plan(h: &Harness, item: &str) -> String {
    let slug = tracon::corpus::work::plan_slug(item);
    tracon::corpus::write(
        &h.store,
        &h.bus,
        "n1",
        "personal",
        "document",
        tracon_sync::ChangeOp::Upsert,
        &slug,
        json!({"channel": "personal", "slug": slug, "kind": "plan",
               "title": "Plan", "body": "Add the table, then the view.",
               "pinned": false, "hash": "p", "created_ms": now_ms(), "updated_ms": now_ms()}),
    )
    .unwrap();
    tracon::corpus::work::set_plan(&h.store, &h.bus, "n1", item, &slug).unwrap();
    slug
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
    let first = new_item(&h, "Lay the schema", vec![]);
    let second = new_item(&h, "Build the ledger", vec![first.id.clone()]);

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

    let (_, v) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{}/continuation", first.id),
        None,
    )
    .await;
    assert_eq!(v["next"]["kind"], "start");
    assert_eq!(
        v["next"]["text"], "Nothing has run yet: plan it.",
        "execute is not offered on an item with no plan"
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
    assert_eq!(
        v["next"]["kind"], "continue",
        "no plan was recorded, so execute would be refused"
    );
    write_plan(&h, &first.id);
    let (_, v) = call(&h.operator, "GET", "/api/sessions/p/continuation", None).await;
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

/// Continuing after a plan session that wrote its plan executes the plan in
/// that session's workspace; a plan session cut off before it finished goes
/// on planning.
#[tokio::test]
async fn continuing_a_written_plan_executes_it() {
    let h = harness().await;
    ready(&h).await;
    let item = new_item(&h, "Lay the schema", vec![]);
    let slug = write_plan(&h, &item.id);
    let mut planned = ended("p", 1, "phase_done", None);
    planned.phase = "plan".into();
    planned.work_item_id = Some(item.id.clone());
    h.store.insert_session(&planned).unwrap();

    let (_, v) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{}/continuation", item.id),
        None,
    )
    .await;
    assert_eq!(v["next"]["kind"], "execute");
    assert_eq!(v["actions"]["continue_from"], "p");

    let (st, row) = call(
        &h.operator,
        "POST",
        "/api/continuation/continue",
        Some(json!({ "session_id": "p" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{row}");
    assert_eq!(row["phase"], "execute");
    assert_eq!(row["continued_from"], "p");
    assert_eq!(row["repo_path"], "workspace://p");
    assert_eq!(row["work_item_id"], item.id.as_str());
    let note = h
        .store
        .get_draft(row["id"].as_str().unwrap())
        .unwrap()
        .map(|(text, _)| text)
        .unwrap_or_default();
    assert!(note.starts_with("Carry out work item"), "{note}");
    assert!(note.contains(&format!("document `{slug}`")), "{note}");
    assert!(note.contains("`submit_review`"), "{note}");

    let other = new_item(&h, "Build the ledger", vec![]);
    let mut cut = ended("q", 2, "node_restart", None);
    cut.phase = "plan".into();
    cut.work_item_id = Some(other.id.clone());
    h.store.insert_session(&cut).unwrap();
    let (st, row) = call(
        &h.operator,
        "POST",
        "/api/continuation/continue",
        Some(json!({ "session_id": "q" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{row}");
    assert_eq!(row["phase"], "plan");
}

/// A review the operator sent back for changes outlives the attempt that
/// submitted it: continuing that attempt, generically or by its written plan,
/// hands the review to the new one and tells it the notes, that a patch came
/// with them, and to resubmit under the same id. A review already decided
/// stays with the attempt it was decided for.
#[tokio::test]
async fn continuing_an_attempt_hands_its_review_in_revision_to_the_next() {
    let h = harness().await;
    ready(&h).await;
    let item = new_item(&h, "Lay the schema", vec![]);
    write_plan(&h, &item.id);
    let mut planned = ended("p", 1, "phase_done", None);
    planned.phase = "plan".into();
    planned.work_item_id = Some(item.id.clone());
    h.store.insert_session(&planned).unwrap();
    h.store
        .insert_session(&ended("a", 2, "killed_user", None))
        .unwrap();
    for (review, session, patch) in [
        ("rv-p", "p", None),
        ("rv-a", "a", Some("diff --git a/x b/x\n")),
    ] {
        let mut sent_back = review_row(review, session, "revising");
        sent_back.verdict_reason = Some(format!("Rename the table in {review}."));
        sent_back.revision_patch = patch.map(str::to_string);
        h.store.insert_review(&sent_back).unwrap();
    }
    h.store
        .insert_review(&review_row("rv-done", "a", "approved"))
        .unwrap();

    for (from, review, plan, patch) in [
        ("p", "rv-p", true, "No edited diff is attached."),
        ("a", "rv-a", false, "An edited diff is attached"),
    ] {
        let (st, row) = call(
            &h.operator,
            "POST",
            "/api/continuation/continue",
            Some(json!({ "session_id": from })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{row}");
        let next = row["id"].as_str().unwrap();
        let note = h
            .store
            .get_draft(next)
            .unwrap()
            .map(|(text, _)| text)
            .unwrap_or_default();
        assert_eq!(note.starts_with("Carry out work item"), plan, "{note}");
        assert!(note.contains(&format!("`{review}`")), "{note}");
        assert!(
            note.contains(&format!("Rename the table in {review}.")),
            "{note}"
        );
        assert!(note.contains(patch), "{note}");
        assert!(note.contains("`review_status`"), "{note}");
        assert!(note.contains("same review_id"), "{note}");
        let moved = h.store.get_review(review).unwrap().unwrap();
        assert_eq!(moved.session_id.as_deref(), Some(next));
        assert_eq!(moved.state, "revising");
        let taken = h
            .store
            .session_events_of_kind(next, "review_taken_over")
            .unwrap();
        assert_eq!(taken.len(), 1, "{taken:?}");
        assert_eq!(taken[0].payload["review_id"], review);
        assert_eq!(taken[0].payload["from_session"], from);
        assert_eq!(taken[0].payload["to_session"], next);
    }
    let decided = h.store.get_review("rv-done").unwrap().unwrap();
    assert_eq!(decided.session_id.as_deref(), Some("a"));
}

/// A published review says what the forge last showed of its request, so the
/// work's links can say a pull request is merged without opening it.
#[tokio::test]
async fn a_published_review_carries_the_forge_state_of_its_request() {
    let h = harness().await;
    h.store
        .insert_session(&ended("a", 1, "phase_done", None))
        .unwrap();
    let mut published = review_row("rv-1", "a", "published");
    published.publish_result = Some("https://github.test/owner/name/pull/149".into());
    h.store.insert_review(&published).unwrap();
    h.store
        .publication_begin(&tracon::store::publication::PublicationBegin {
            id: "p1",
            review_id: "rv-1",
            revision_id: None,
            candidate_id: "c1",
            channel: "personal",
            node_id: "n1",
            provider: "github",
            project: "owner/name",
            base: "main",
            branch: "feat",
            head_sha: "abc",
            instance: "i",
        })
        .unwrap();
    let (_, v) = call(&h.operator, "GET", "/api/sessions/a/continuation", None).await;
    assert_eq!(
        v["evidence"]["reviews"][0]["forge_state"],
        Value::Null,
        "{v}"
    );

    h.store
        .append_event(&NewEvent {
            session_id: "a".into(),
            work_item_id: None,
            kind: "forge_follow".into(),
            ref_id: Some("p1".into()),
            payload: json!({ "review_id": "rv-1", "changes": ["merged"], "snapshot": { "state": "merged" } }),
            at_ms: now_ms(),
            mono_ms: 0,
        })
        .unwrap();
    let (st, v) = call(&h.operator, "GET", "/api/sessions/a/continuation", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["evidence"]["reviews"][0]["forge_state"], "merged", "{v}");
}

/// One look `follow` recorded at request #149, as `follow::tick` records it.
fn followed(h: &Harness, session: &str, at: i64, snapshot: Value) {
    h.store
        .append_event(&NewEvent {
            session_id: session.into(),
            work_item_id: None,
            kind: "forge_follow".into(),
            ref_id: Some("pub1".into()),
            payload: json!({
                "review_id": "r1",
                "url": "https://github.test/me/repo/pull/149",
                "changes": [],
                "snapshot": snapshot,
            }),
            at_ms: at,
            mono_ms: 0,
        })
        .unwrap();
}

fn pr(state: &str, check: &str, comments: u64) -> Value {
    json!({
        "state": state, "draft": false, "checks": { "Backend Checks": check },
        "decision": null, "comments": comments,
    })
}

async fn next_of(h: &Harness, item: &str) -> Value {
    let (st, v) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{item}/continuation"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

/// Once its change is published the task follows the request: the next
/// action is what the request needs, the suspended attempt is what carries
/// it on, and the task stays open until the operator closes it, which ends
/// the suspended session holding it. Archiving it from Home changes none of
/// that.
#[tokio::test]
async fn a_published_task_follows_its_request_until_the_operator_closes_it() {
    let h = harness().await;
    let item = new_item(&h, "Fix the invoices", vec![]);
    let mut s = session_row("s", "n1", "personal");
    s.state = "suspended".into();
    s.work_item_id = Some(item.id.clone());
    s.created_ms = 1;
    s.archived_ms = Some(5);
    h.store.insert_session(&s).unwrap();

    followed(&h, "s", 10, pr("open", "pending", 0));
    let v = next_of(&h, &item.id).await;
    assert_eq!(v["next"]["kind"], "merge", "{v}");
    assert_eq!(v["next"]["text"], "PR #149 is open · merge it");
    assert_eq!(v["follow"]["label"], "PR #149");
    assert_eq!(
        v["actions"]["continue_from"], "s",
        "an archived task is still continued"
    );
    assert_eq!(v["actions"]["close"], true);

    followed(&h, "s", 20, pr("open", "failed", 0));
    let v = next_of(&h, &item.id).await;
    assert_eq!(v["next"]["kind"], "continue");
    assert_eq!(
        v["next"]["text"],
        "CI failed on `Backend Checks` · continue"
    );
    assert_eq!(v["next"]["session_id"], "s");

    followed(&h, "s", 30, pr("open", "passed", 2));
    let v = next_of(&h, &item.id).await;
    assert_eq!(v["next"]["text"], "2 new comments · continue");

    // Following only reads: nothing it saw started a session.
    assert_eq!(h.store.sessions_of_work_item(&item.id).unwrap().len(), 1);

    followed(&h, "s", 40, pr("merged", "passed", 2));
    let v = next_of(&h, &item.id).await;
    assert_eq!(v["next"]["kind"], "close");
    assert_eq!(v["next"]["text"], "PR #149 merged · close the task");
    assert_eq!(
        h.store.work_get(&item.id).unwrap().unwrap().state,
        tracon_sync::work::OPEN,
        "a merge does not close the task; the operator does"
    );

    let (st, v) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{}/close", item.id),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["state"], "closed");
    let s = h.store.get_session("s").unwrap().unwrap();
    assert_eq!(s.state, "closed");
    assert_eq!(s.end_reason.as_deref(), Some("item_close"));
    let v = next_of(&h, &item.id).await;
    assert_eq!(v["next"]["kind"], "done");
    assert_eq!(v["actions"]["close"], false);
    assert_eq!(v["actions"]["continue_from"], Value::Null);
}

/// What arrived before the latest attempt started is not new to it: comments
/// a continuation was carried on to answer are not counted again.
#[tokio::test]
async fn comments_are_new_since_the_latest_attempt_started() {
    let h = harness().await;
    let item = new_item(&h, "Fix the invoices", vec![]);
    let mut first = ended("a", 1, "continued", None);
    first.work_item_id = Some(item.id.clone());
    h.store.insert_session(&first).unwrap();
    followed(&h, "a", 10, pr("open", "passed", 0));
    followed(&h, "a", 20, pr("open", "passed", 3));
    let mut second = session_row("b", "n1", "personal");
    second.state = "suspended".into();
    second.work_item_id = Some(item.id.clone());
    second.created_ms = 25;
    second.continued_from = Some("a".into());
    h.store.insert_session(&second).unwrap();

    let v = next_of(&h, &item.id).await;
    assert_eq!(v["next"]["text"], "PR #149 is open · merge it", "{v}");
    followed(&h, "b", 30, pr("open", "passed", 4));
    let v = next_of(&h, &item.id).await;
    assert_eq!(v["next"]["text"], "1 new comment · continue");
    assert_eq!(v["next"]["session_id"], "b");
}

/// Continue on a suspended attempt carries it on, told where its work
/// landed, and the suspended one ends as continued.
#[tokio::test]
async fn continuing_a_suspended_attempt_carries_it_on_and_ends_it() {
    let h = harness().await;
    ready(&h).await;
    let item = new_item(&h, "Fix the invoices", vec![]);
    write_plan(&h, &item.id);
    let mut s = session_row("s", "n1", "personal");
    s.state = "suspended".into();
    s.work_item_id = Some(item.id.clone());
    h.store.insert_session(&s).unwrap();

    let (st, row) = call(
        &h.operator,
        "POST",
        "/api/continuation/continue",
        Some(json!({ "session_id": "s" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{row}");
    assert_eq!(row["continued_from"], "s");
    assert_eq!(row["work_item_id"], item.id.as_str());
    let old = h.store.get_session("s").unwrap().unwrap();
    assert_eq!(old.state, "closed");
    assert_eq!(old.end_reason.as_deref(), Some("continued"));
}

/// Changing the approach on a suspended attempt whose work was published
/// tells the new one where it landed, as a plain Continue does.
#[tokio::test]
async fn a_changed_approach_on_published_work_is_told_where_it_landed() {
    let h = harness().await;
    ready(&h).await;
    let mut s = session_row("s", "n1", "personal");
    s.state = "suspended".into();
    h.store.insert_session(&s).unwrap();
    let mut published = review_row("rv", "s", "approved");
    published.publish_result = Some("https://github.test/me/repo/pull/149".into());
    h.store.insert_review(&published).unwrap();

    let (st, row) = call(
        &h.operator,
        "POST",
        "/api/continuation/continue",
        Some(json!({ "session_id": "s", "approach": "Answer the review comment." })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{row}");
    let note = h
        .store
        .get_draft(row["id"].as_str().unwrap())
        .unwrap()
        .map(|(text, _)| text)
        .unwrap_or_default();
    assert!(
        note.contains("https://github.test/me/repo/pull/149"),
        "{note}"
    );
    assert!(note.contains("Answer the review comment."), "{note}");
}

/// A continuation that failed says so while its request is open, rather
/// than offering the merge.
#[tokio::test]
async fn a_failed_attempt_is_not_hidden_behind_its_open_request() {
    let h = harness().await;
    let item = new_item(&h, "Fix the invoices", vec![]);
    let mut first = ended("a", 1, "budget", None);
    first.work_item_id = Some(item.id.clone());
    h.store.insert_session(&first).unwrap();
    followed(&h, "a", 10, pr("open", "passed", 0));
    let v = next_of(&h, &item.id).await;
    assert_eq!(v["next"]["kind"], "change_approach", "{v}");

    followed(&h, "a", 20, pr("merged", "passed", 0));
    let v = next_of(&h, &item.id).await;
    assert_eq!(v["next"]["kind"], "close", "{v}");
}
