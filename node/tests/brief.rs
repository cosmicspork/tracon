//! The product brief through its surfaces.
//!
//! The claims under test are about the record, not the prose in it: a brief is
//! optional and an item without one is whole; every line says who is behind it
//! and the node refuses the two claims a session cannot honestly make; a line
//! points at evidence and a reference this node has never seen is said to be
//! unknown rather than rendered as a link; and the document is the record —
//! edited by hand, it reads back as the same structure, with whatever did not
//! fit the shape still in it.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness, Harness};
use support::http::call;
use support::state;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::corpus::brief::{self, Author, EntryInput, RefInput, SectionInput};
use tracon::store::{now_ms, SessionRow};

async fn mcp(app: &axum::Router, sid: &str, token: &str, name: &str, args: Value) -> Value {
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/{sid}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(text).unwrap_or(json!({ "raw": text, "error": v["result"]["isError"] }))
}

fn session_row(id: &str, item: Option<&str>) -> SessionRow {
    SessionRow {
        id: id.into(),
        node_id: "n1".into(),
        channel: "personal".into(),
        work_item_id: item.map(str::to_string),
        repo_path: "/nonexistent/repo".into(),
        worktree_path: None,
        branch: "feat/x".into(),
        harness_id: "fake".into(),
        harness_version: "1.0.0".into(),
        harness_agent: None,
        harness_found: None,
        harness_protocol: None,
        harness_session_id: None,
        container_name: None,
        model: "m/a".into(),
        project_id: None,
        phase: "execute".into(),
        policy_version: None,
        review_id: None,
        budget_tokens: 1000,
        tokens_used: 0,
        cost_usd: None,
        context_used: None,
        context_size: None,
        state: "running".into(),
        end_reason: None,
        last_error: None,
        turn_active: 0,
        draft: None,
        draft_updated_ms: None,
        created_ms: now_ms(),
        started_mono_ms: Some(0),
        ended_mono_ms: None,
        updated_ms: now_ms(),
        archived_ms: None,
        legacy_ms: None,
        parent_session: None,
        continued_from: None,
        manifest_digest: None,
    }
}

async fn an_item(h: &Harness, title: &str) -> String {
    let (st, v) = call(
        &h.operator,
        "POST",
        "/api/work",
        Some(json!({ "channel": "personal", "title": title })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v["id"].as_str().unwrap().to_string()
}

fn section(field: &str, entries: Value) -> Value {
    json!({ "field": field, "entries": entries })
}

#[tokio::test]
async fn an_item_has_no_brief_until_one_is_written_and_keeps_working_without_it() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;

    // Absent is an answer. Nothing about the item is degraded by it.
    let (st, v) = call(&h.operator, "GET", &format!("/api/work/{id}/brief"), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["brief"], Value::Null);
    let (_, item) = call(&h.operator, "GET", &format!("/api/work/{id}"), None).await;
    assert_eq!(item["brief"], Value::Null);
    assert_eq!(item["item"]["readiness"]["state"], "ready");
    assert_eq!(item["item"]["brief_slug"], Value::Null);

    // Unlinking a brief that was never there is a 404, not a silent success.
    let (st, _) = call(
        &h.operator,
        "DELETE",
        &format!("/api/work/{id}/brief"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // The first write starts the document and links the item to it.
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({
            "title": "Overnight alert triage",
            "sections": [section(
                "problem",
                json!([{ "provenance": "observed", "text": "Alerts arrive in arrival order, so the urgent one is found by scrolling",
                         "refs": [{ "kind": "doc", "value": "meeting-ops-ride-along" }] }]),
            )],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let slug = v["brief"]["slug"].as_str().unwrap().to_string();
    assert_eq!(slug, brief::slug_for(&id));
    let item = h.store.work_get(&id).unwrap().unwrap();
    assert_eq!(item.brief_slug.as_deref(), Some(slug.as_str()));

    // It is a document, of kind `brief`, and it reads as Markdown that says
    // what it does not yet say.
    let (st, doc) = call(
        &h.operator,
        "GET",
        &format!("/api/docs/personal/{slug}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{doc}");
    assert_eq!(doc["kind"], "brief");
    let body = doc["body"].as_str().unwrap();
    assert!(
        body.starts_with("# Brief: Overnight alert triage\n"),
        "{body}"
    );
    assert!(
        body.contains("- observed: Alerts arrive in arrival order"),
        "{body}"
    );
    assert!(body.contains("[doc:meeting-ops-ride-along]"), "{body}");
    assert!(body.contains("_no success criteria stated_"), "{body}");
    // The same document read as a brief carries the structure with it.
    assert_eq!(doc["brief"]["sections"][1]["field"], "problem");

    // Every section is answered, including the five that say nothing yet.
    let (_, v) = call(&h.operator, "GET", &format!("/api/work/{id}/brief"), None).await;
    let absent: Vec<&str> = v["brief"]["absent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["field"].as_str().unwrap())
        .collect();
    assert_eq!(
        absent,
        vec![
            "intended_user",
            "source_references",
            "constraints",
            "success_criteria",
            "unresolved_questions"
        ],
        "{v}"
    );
    assert_eq!(v["brief"]["counts"]["observed"], 1);
    assert!(
        v["summary"]
            .as_str()
            .unwrap()
            .contains("no success criteria stated"),
        "{v}"
    );

    // Unlinking leaves the document where it is; the prose was the
    // operator's, and a removed link is not a reason to delete it.
    let (st, gone) = call(
        &h.operator,
        "DELETE",
        &format!("/api/work/{id}/brief"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{gone}");
    assert_eq!(gone["document_kept"], true);
    assert!(h.store.work_get(&id).unwrap().unwrap().brief_slug.is_none());
    assert!(h.store.doc_get("personal", &slug).unwrap().is_some());
}

#[tokio::test]
async fn each_line_says_who_is_behind_it_and_what_it_rests_on() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    let cited = an_item(&h, "Rank alerts by severity").await;
    // One reference this node holds, one it has never seen.
    tracon::mcp::docs::write_document(
        &h.store,
        h.manager.bus(),
        "n1",
        "personal",
        "meeting-ops-ride-along",
        "# Ride-along notes\n\nThey triage by scrolling.",
        None,
        false,
        None,
    )
    .unwrap();

    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({
            "sections": [
                section("intended_user", json!([
                    { "provenance": "observed", "text": "Field ops leads on the overnight shift",
                      "refs": [{ "kind": "doc", "value": "meeting-ops-ride-along" }] },
                ])),
                section("problem", json!([
                    { "provenance": "inferred", "text": "Arrival order buries the urgent alert",
                      "refs": [{ "kind": "doc", "value": "meeting-ops-never-happened" },
                               { "kind": "work", "value": cited },
                               { "kind": "url", "value": "https://example.test/ticket/4821" }] },
                ])),
                section("success_criteria", json!([
                    { "provenance": "decided", "text": "Triage in under two minutes" },
                ])),
                section("unresolved_questions", json!([
                    { "text": "Who owns the escalation path?" },
                ])),
            ],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let counts = &v["brief"]["counts"];
    assert_eq!(counts["observed"], 1);
    assert_eq!(counts["inferred"], 1);
    assert_eq!(counts["decided"], 1);
    assert_eq!(
        counts["unattributed"], 1,
        "a line written with no marker is not promoted to one: {v}"
    );

    let refs = &v["brief"]["sections"][1]["entries"][0]["refs"];
    assert_eq!(refs[0]["value"], "meeting-ops-never-happened");
    assert_eq!(
        refs[0]["known"], false,
        "a reference this node has never seen says so: {refs}"
    );
    assert_eq!(refs[1]["known"], true);
    assert_eq!(refs[1]["label"], "Rank alerts by severity");
    assert!(
        refs[2].get("known").is_none(),
        "a URL is not this node's to vouch for: {refs}"
    );
    let user = &v["brief"]["sections"][0]["entries"][0]["refs"][0];
    assert_eq!(user["known"], true);
    assert_eq!(user["label"], "Ride-along notes");

    // A reference the node cannot make sense of is refused by name rather
    // than written into the document as prose.
    let (st, err) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({
            "sections": [section("problem", json!([
                { "text": "x", "refs": [{ "kind": "ticket", "value": "4821" }] },
            ]))],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["error"]["message"].as_str().unwrap().contains("ticket"),
        "{err}"
    );
    let (st, err) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({ "sections": [section("wishes", json!([{ "text": "x" }]))] })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{err}");
}

#[tokio::test]
async fn an_edit_against_a_stale_read_is_refused_with_the_current_hash() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    let (_, first) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({ "sections": [section("problem", json!([{ "text": "first" }]))] })),
    )
    .await;
    let stale = first["brief"]["hash"].as_str().unwrap().to_string();
    let (_, second) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({ "sections": [section("problem", json!([{ "text": "second" }]))] })),
    )
    .await;
    let current = second["brief"]["hash"].as_str().unwrap().to_string();
    assert_ne!(stale, current);

    let (st, err) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({
            "if_hash": stale,
            "sections": [section("problem", json!([{ "text": "third" }]))],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{err}");
    assert!(
        err["error"]["message"].as_str().unwrap().contains(&current),
        "{err}"
    );
    let (_, v) = call(&h.operator, "GET", &format!("/api/work/{id}/brief"), None).await;
    assert_eq!(
        v["brief"]["sections"][1]["entries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn a_hand_edited_brief_reads_back_whole() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    let slug = brief::slug_for(&id);
    // No node was involved in writing this: it is a Markdown file someone
    // typed, in a section order of their own, with a heading this module has
    // never heard of.
    let (st, wrote) = call(
        &h.operator,
        "PUT",
        &format!("/api/docs/personal/{slug}"),
        Some(json!({
            "body": "# Overnight alert triage\n\nAfter two ride-alongs.\n\n## SUCCESS  CRITERIA\n\nJudged on the shift, not in staging.\n\n- decided: triage in under two minutes\n\n## Rollout notes\n\nStart with one team.\n",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{wrote}");
    // The document alone does not link the item; the link is the operator's.
    assert!(h.store.work_get(&id).unwrap().unwrap().brief_slug.is_none());
    let (_, linked) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({})),
    )
    .await;
    let brief = &linked["brief"];
    assert_eq!(brief["title"], "Overnight alert triage");
    assert_eq!(brief["preamble"], "After two ride-alongs.");
    let criteria = brief["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["field"] == "success_criteria")
        .unwrap();
    assert_eq!(criteria["notes"], "Judged on the shift, not in staging.");
    assert_eq!(criteria["entries"][0]["provenance"], "decided");
    assert!(
        brief["extra"]
            .as_str()
            .unwrap()
            .contains("Start with one team."),
        "what did not fit the shape is kept: {brief}"
    );
    let body = h.store.doc_get("personal", &slug).unwrap().unwrap().body;
    assert!(
        body.contains("## Rollout notes"),
        "and written back: {body}"
    );
    assert!(body.contains("## Intended user"), "{body}");
}

#[tokio::test]
async fn a_session_reads_the_brief_and_is_held_to_what_it_can_claim() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({
            "sections": [section("success_criteria", json!([
                { "provenance": "decided", "text": "Triage in under two minutes" },
            ]))],
        })),
    )
    .await;
    h.store
        .insert_session(&session_row("s1", Some(&id)))
        .unwrap();
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;

    // Reading is free: the shipped agreements name `brief_read`.
    let v = mcp(&h.harness, "s1", &token, "brief_read", json!({})).await;
    assert_eq!(
        v["brief"]["sections"][4]["entries"][0]["text"], "Triage in under two minutes",
        "{v}"
    );
    assert!(v["summary"].as_str().unwrap().contains("1 decided"), "{v}");

    // Writing one is not: the agreements name no `brief_note`, so it is put
    // to the operator — and with no live session to carry the question here,
    // refused rather than run.
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        "brief_note",
        json!({ "field": "problem", "provenance": "inferred", "text": "the sort is the cause" }),
    )
    .await;
    assert_eq!(v["error"], true, "{v}");

    // And what a session may claim is the node's rule, not the agent's: a
    // session never records a decision, and never an observation with
    // nothing to point at.
    let note = |provenance: &str, refs: Vec<RefInput>| SectionInput {
        field: "problem".into(),
        notes: None,
        replace: false,
        entries: Some(vec![EntryInput {
            provenance: Some(provenance.into()),
            text: "they gave up and phoned the on-call".into(),
            refs,
        }]),
    };
    let session = Author::Session("s1".into());
    let decided = brief::append_for_item(
        &h.store,
        h.manager.bus(),
        "n1",
        &id,
        vec![note("decided", vec![])],
        &session,
    );
    assert!(decided
        .unwrap_err()
        .to_string()
        .contains("may not record an operator decision"));
    let bare = brief::append_for_item(
        &h.store,
        h.manager.bus(),
        "n1",
        &id,
        vec![note("observed", vec![])],
        &session,
    );
    assert!(bare
        .unwrap_err()
        .to_string()
        .contains("points at something"));

    let cited = brief::append_for_item(
        &h.store,
        h.manager.bus(),
        "n1",
        &id,
        vec![note(
            "observed",
            vec![RefInput {
                kind: "doc".into(),
                value: "meeting-ops-ride-along".into(),
            }],
        )],
        &session,
    )
    .expect("an observation that points at something is recorded");
    let entry = &cited.brief.section(brief::Field::Problem).unwrap().entries[0];
    assert_eq!(entry.provenance, brief::Provenance::Observed);
    assert!(
        entry
            .refs
            .iter()
            .any(|r| r.kind == "session" && r.value == "s1"),
        "the document records which session wrote the line: {entry:?}"
    );
    // The operator's decision is still the operator's, and is not attributed
    // to the session that happened to be running.
    let criteria = cited.brief.section(brief::Field::SuccessCriteria).unwrap();
    assert_eq!(criteria.entries[0].provenance, brief::Provenance::Decided);
    assert!(criteria.entries[0].refs.is_empty());

    // A session that holds no item has no brief to read, and is told so.
    h.store.insert_session(&session_row("s2", None)).unwrap();
    let t2 = h
        .manager
        .register_tool_token_for_test("s2", "personal")
        .await;
    let v = mcp(&h.harness, "s2", &t2, "brief_read", json!({})).await;
    assert_eq!(v["error"], true, "{v}");
}

#[tokio::test]
async fn a_section_can_be_rewritten_and_a_link_the_node_cannot_follow_is_said_out_loud() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    let (_, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({
            "sections": [section(
                "constraints",
                json!([{ "provenance": "decided", "text": "offline is the normal case" }]),
            )],
        })),
    )
    .await;
    assert_eq!(v["brief"]["counts"]["decided"], 1);

    // A line added is added; a section replaced is replaced. Both are the
    // operator's to do, and neither silently rewrites the other's section.
    let (_, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({
            "sections": [{
                "field": "constraints",
                "replace": true,
                "entries": [{ "provenance": "decided", "text": "offline is the only case" }],
            }],
        })),
    )
    .await;
    let constraints = v["brief"]["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["field"] == "constraints")
        .unwrap()
        .clone();
    assert_eq!(constraints["entries"].as_array().unwrap().len(), 1);
    assert_eq!(
        constraints["entries"][0]["text"],
        "offline is the only case"
    );

    // An emptied section says it is empty rather than vanishing.
    let (_, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/brief"),
        Some(json!({
            "sections": [{ "field": "constraints", "replace": true, "entries": [] }],
        })),
    )
    .await;
    assert!(
        v["brief"]["absent"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["field"] == "constraints"),
        "{v}"
    );

    // The document can go — deleted here, not yet replicated on another node.
    // The item still points at it, and that is what the reader is told.
    let slug = brief::slug_for(&id);
    let (st, _) = call(
        &h.operator,
        "DELETE",
        &format!("/api/docs/personal/{slug}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, v) = call(&h.operator, "GET", &format!("/api/work/{id}/brief"), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["brief"], Value::Null);
    assert_eq!(
        v["linked_slug"], slug,
        "the link the node cannot follow is still reported: {v}"
    );
}
