//! Acceptance criteria through their surfaces.
//!
//! The claims under test are about what a criterion can honestly be said to
//! rest on. A criterion is a line in the brief, named from its own text, so
//! rewording one does not let an old verdict quietly follow it. What points at
//! a criterion lives in the document, and the standing of a link is the brief's
//! existing rule: a session proposes, the operator decides. A check result can
//! raise a criterion as far as *its checks pass* and no further — the word
//! `met` comes only from a person, no agent path reaches it, and a verdict
//! about one attempt does not settle the next. And what nothing points at,
//! what nobody has judged, what the brief assumes and what it still asks are
//! carried beside the criteria, because a view listing only what is covered
//! reads as though that were everything there is.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness, Harness};
use support::http::call;
use support::state;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::corpus::brief::{self, Author, LinkInput};
use tracon::corpus::criteria::{self, key_for};
use tracon::store::{now_ms, CandidateRow, CheckRunRow, ReviewRevisionRow, ReviewRow, SessionRow};

async fn mcp(app: &axum::Router, sid: &str, token: &str, body: Value) -> Value {
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
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// One tool call, unwrapped to the JSON the tool returned.
async fn tool(app: &axum::Router, sid: &str, token: &str, name: &str, args: Value) -> Value {
    let v = mcp(
        app,
        sid,
        token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),
    )
    .await;
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(text).unwrap_or(json!({ "raw": text, "error": v["result"]["isError"] }))
}

fn session_row(id: &str, item: &str, harness_id: &str) -> SessionRow {
    SessionRow {
        id: id.into(),
        node_id: "n1".into(),
        channel: "personal".into(),
        work_item_id: Some(item.into()),
        repo_path: "/nonexistent/repo".into(),
        worktree_path: None,
        branch: "feat/x".into(),
        harness_id: harness_id.into(),
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

/// Write the brief as Markdown, the way an operator editing the document does.
/// The node parses it and writes it back canonically, so what these tests read
/// is the structure the document really carries.
async fn write_brief(h: &Harness, item: &str, markdown: &str) {
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{item}/brief"),
        Some(json!({ "markdown": markdown })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

async fn read_criteria(h: &Harness, item: &str) -> Value {
    let (st, v) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{item}/criteria"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v["criteria"].clone()
}

fn brief_body(h: &Harness, item: &str) -> String {
    let row = h.store.work_get(item).unwrap().unwrap();
    h.store
        .doc_get("personal", row.brief_slug.as_deref().unwrap())
        .unwrap()
        .unwrap()
        .body
}

/// The criterion with this text, out of the view, by the name its text gives it.
fn of(view: &Value, text: &str) -> Value {
    let key = key_for(text);
    view["criteria"]
        .as_array()
        .expect("criteria are a list")
        .iter()
        .find(|c| c["key"] == json!(key))
        .unwrap_or_else(|| panic!("no criterion {text:?} ({key}) in {view}"))
        .clone()
}

fn gap_holds(view: &Value, gap: &str, text: &str) -> bool {
    view["gaps"][gap]
        .as_array()
        .unwrap()
        .contains(&json!(key_for(text)))
}

/// A candidate of the item, owned by a session that holds it — which is how the
/// node knows an attempt is an attempt at this item. `captured_ms` is explicit
/// because which attempt is the newest is the whole point of one of these
/// tests, and two rows written in the same millisecond do not say.
fn a_candidate(h: &Harness, session: &str, head: &str, captured_ms: i64) -> String {
    let id = tracon::store::candidate_id(head, "personal");
    h.store
        .insert_candidate(&CandidateRow {
            id: id.clone(),
            head_sha: head.into(),
            tree_sha: Some("1".repeat(40)),
            channel: "personal".into(),
            owner_session_id: session.into(),
            source_kind: "git".into(),
            captured_ms,
            capture_json: "{}".into(),
        })
        .unwrap();
    id
}

fn a_check_run(h: &Harness, candidate: &str, command: &str, outcome: &str) {
    h.store
        .insert_check_run(&CheckRunRow {
            id: format!("run-{candidate}-{}", command.replace(' ', "-")),
            candidate_id: Some(candidate.into()),
            session_id: "s1".into(),
            command: Some(command.into()),
            definition_json: json!({ "command": command }).to_string(),
            definition_hash: None,
            execution_image: Some("image@sha256:0".into()),
            inputs_json: None,
            reuse_key: None,
            outcome: outcome.into(),
            source_outcome: None,
            exit_code: Some(i64::from(outcome != "passed")),
            log: String::new(),
            duration_ms: Some(1),
            started_ms: now_ms(),
            finished_ms: Some(now_ms()),
            rerun_of: None,
            reused_from_id: None,
            metadata_json: "{}".into(),
        })
        .unwrap();
}

const TRIAGE: &str = "Triage finishes in under two minutes";
const STALE: &str = "The overnight queue never shows a stale count";
const QUIET: &str = "Nobody is woken for something that can wait";

/// A brief whose success criteria carry a link of every kind the node reads, so
/// one document exercises every resolution: a configured check, a command the
/// operator never configured, a scenario, an observation, and a criterion with
/// nothing under it at all.
fn brief_markdown() -> String {
    format!(
        "# Brief: Overnight alert triage\n\n\
         ## Intended user\n\n\
         - observed: the two people on the overnight rota [doc:meeting-ops]\n\n\
         ## Problem\n\n\
         - inferred: the sort order is what makes it slow\n\n\
         ## Success criteria\n\n\
         - decided: {TRIAGE}\n\
         \x20 - decided check: just check\n\
         \x20 - inferred scenario: overnight-triage-happy-path\n\
         \x20 - observed observation: two leads gave up after forty seconds [doc:meeting-ride-along]\n\
         - decided: {STALE}\n\
         \x20 - decided check: cargo test --all\n\
         - inferred: {QUIET}\n\n\
         ## Unresolved questions\n\n\
         - Does the rota want the queue sorted by age or by blast radius?\n"
    )
}

/// A criterion is a line in the brief; what points at it is written under it in
/// the same document; and every kind of link says honestly what it resolved to.
#[tokio::test]
async fn what_points_at_a_criterion_lives_in_the_brief_beside_it() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    write_brief(&h, &id, &brief_markdown()).await;

    // The document is the record: a link round-trips as an indented line under
    // the criterion it is about, not as a row in a table beside it.
    let body = brief_body(&h, &id);
    assert!(
        body.contains(&format!(
            "- decided: {TRIAGE}\n  - decided check: just check\n"
        )),
        "{body}"
    );

    let view = read_criteria(&h, &id).await;
    let triage = of(&view, TRIAGE);
    assert_eq!(triage["standard"], "agreed", "the operator decided it");
    assert_eq!(triage["links"][0]["kind"], "check");
    assert!(
        triage["links"][0]["unresolved"].is_null(),
        "`just check` is a check the operator configured: {triage}"
    );
    assert_eq!(
        triage["links"][1]["unresolved"], "this node holds no scenario records yet",
        "a scenario is not a record this node holds yet, and says so: {triage}"
    );
    assert!(
        triage["links"][2]["outcome"].is_null() && triage["links"][2]["unresolved"].is_null(),
        "an observation is context for a person, not a result: {triage}"
    );
    // Nothing has run, so the criterion says that rather than reading as covered.
    assert_eq!(triage["coverage"], "no_result_yet");

    // A command the operator never configured is not a check this node runs,
    // and a criterion pointing only at one is left to a person.
    let stale = of(&view, STALE);
    assert_eq!(
        stale["links"][0]["unresolved"],
        "not one of the checks the operator configured, so it never runs"
    );
    assert_eq!(stale["coverage"], "awaits_judgement");

    let quiet = of(&view, QUIET);
    assert_eq!(quiet["coverage"], "nothing_points_at_it");
    assert_eq!(
        quiet["standard"], "proposed",
        "an inferred criterion is somebody's proposal, not an agreed standard"
    );

    // The gaps are carried beside the criteria, not left to be counted.
    assert!(
        gap_holds(&view, "uncovered", QUIET) && !gap_holds(&view, "uncovered", TRIAGE),
        "{view}"
    );
    assert_eq!(
        view["gaps"]["unjudged"].as_array().unwrap().len(),
        3,
        "nobody has judged any of them: {view}"
    );
    let assumptions: Vec<&str> = view["gaps"]["assumptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["text"].as_str().unwrap())
        .collect();
    assert!(
        assumptions.contains(&"the sort order is what makes it slow")
            && assumptions.contains(&QUIET),
        "an unobserved, undecided line anywhere in the brief is an assumption: {assumptions:?}"
    );
    assert_eq!(
        view["gaps"]["questions"],
        json!(["Does the rota want the queue sorted by age or by blast radius?"]),
        "the open questions are carried verbatim"
    );
    let summary = view["summary"].as_str().unwrap();
    assert!(
        summary.contains("3 criteria")
            && summary.contains("1 nothing points at them")
            && summary.contains("1 open questions"),
        "{summary}"
    );

    // The item's own screen carries the same reading, so this is not a page the
    // operator has to know to open.
    let (_, whole) = call(&h.operator, "GET", &format!("/api/work/{id}"), None).await;
    assert_eq!(whole["criteria"]["summary"], json!(summary));

    // An item with no brief has nowhere for a criterion to be written, and that
    // is an answer rather than an empty list.
    let bare = an_item(&h, "Rename the column").await;
    let (st, v) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{bare}/criteria"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["criteria"], Value::Null);

    // A brief with no success criteria says so, rather than reading as a brief
    // whose every criterion is met.
    write_brief(
        &h,
        &bare,
        "# Brief: Rename the column\n\n## Problem\n\n- inferred: the name lies\n",
    )
    .await;
    let view = read_criteria(&h, &bare).await;
    assert_eq!(view["criteria"], json!([]));
    assert!(view["absent"].is_string(), "{view}");
    assert_eq!(view["summary"], "no success criteria stated");
}

/// The distinction the whole view exists to keep: a green check is a green
/// check. Only the operator writes `met`, and nothing an agent can reach does.
#[tokio::test]
async fn passing_your_own_checks_is_not_the_customer_agreeing() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    write_brief(&h, &id, &brief_markdown()).await;
    h.store
        .insert_session(&session_row("s1", &id, "fake"))
        .unwrap();
    let candidate = a_candidate(&h, "s1", &"a".repeat(40), now_ms());
    a_check_run(&h, &candidate, "just check", "passed");

    // Every check the operator agreed to has passed, and the criterion still
    // does not say met.
    let view = read_criteria(&h, &id).await;
    assert_eq!(view["candidate"]["id"], json!(candidate));
    let triage = of(&view, TRIAGE);
    assert_eq!(triage["coverage"], "checks_pass");
    assert_eq!(triage["links"][0]["outcome"], "passed");
    assert!(triage["judgement"].is_null(), "nobody has judged it");
    assert!(
        gap_holds(&view, "unjudged", TRIAGE),
        "a green criterion nobody has judged is still unjudged: {view}"
    );
    assert!(
        view["summary"]
            .as_str()
            .unwrap()
            .contains("1 checks pass, unjudged"),
        "{view}"
    );

    // No agent-reachable path settles a criterion. A session is offered the
    // reading and the proposal, and nothing that judges. (`review_verdict` is
    // a review session's opinion of a diff, which is not a criterion's and
    // does not reach this table.)
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    let listed = mcp(
        &h.harness,
        "s1",
        &token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let mut about_criteria: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| n.starts_with("criteria_"))
        .collect();
    about_criteria.sort_unstable();
    assert_eq!(
        about_criteria,
        ["criteria_link", "criteria_read"],
        "a session reads the criteria and proposes a link, and that is all: {names:?}"
    );
    // What the session does read says so in as many words.
    let read = tool(&h.harness, "s1", &token, "criteria_read", json!({})).await;
    assert_eq!(read["criteria"]["criteria"][0]["coverage"], "checks_pass");
    assert!(
        read["summary"]
            .as_str()
            .unwrap()
            .contains("checks pass, unjudged"),
        "{read}"
    );
    // And the writer refuses a session outright, whatever reached it.
    let refused = criteria::judge(
        &h.store,
        &id,
        TRIAGE,
        Some(&candidate),
        None,
        "met",
        None,
        &Author::Session("s1".into()),
    )
    .expect_err("a session may not judge its own work");
    assert!(
        refused
            .to_string()
            .contains("does not establish that the customer agreed"),
        "{refused}"
    );

    // The operator says it, and only then does the criterion read as met.
    let (st, v) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({ "criterion": key_for(TRIAGE), "verdict": "met", "note": "watched it twice", "candidate_id": candidate })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        v["judgement"]["candidate_id"],
        json!(candidate),
        "a verdict is about an attempt, and names which"
    );
    let triage = of(&v["criteria"], TRIAGE);
    assert_eq!(triage["coverage"], "judged_met");
    assert_eq!(triage["judgement"]["note"], "watched it twice");
    assert!(!gap_holds(&v["criteria"], "unjudged", TRIAGE), "{v}");

    // A verdict is one of three words. A fourth is refused rather than stored
    // as a state nothing reads.
    let (st, v) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({ "criterion": key_for(TRIAGE), "verdict": "looks fine" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");

    // Append-only: changing one's mind keeps both, because that it changed is
    // itself worth reading.
    call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({ "criterion": key_for(TRIAGE), "verdict": "not_met", "candidate_id": candidate })),
    )
    .await;
    let rows = h.store.criterion_judgements_for_item(&id).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].verdict, "not_met", "the newest verdict stands");
    assert_eq!(
        of(&read_criteria(&h, &id).await, TRIAGE)["coverage"],
        "judged_not_met"
    );
}

/// A verdict is about the attempt someone looked at. The next attempt is not
/// covered by it, and the view says whose verdict it was rather than dropping
/// it or carrying it forward.
#[tokio::test]
async fn a_verdict_about_one_attempt_does_not_settle_the_next() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    write_brief(&h, &id, &brief_markdown()).await;
    h.store
        .insert_session(&session_row("s1", &id, "fake"))
        .unwrap();
    let first = a_candidate(&h, "s1", &"a".repeat(40), now_ms());
    a_check_run(&h, &first, "just check", "passed");
    let (st, v) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({ "criterion": TRIAGE, "verdict": "met", "candidate_id": first })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // A second attempt: the code changed since anyone looked.
    let second = a_candidate(&h, "s1", &"b".repeat(40), now_ms() + 1000);
    a_check_run(&h, &second, "just check", "failed");
    let view = read_criteria(&h, &id).await;
    assert_eq!(view["candidate"]["id"], json!(second), "the newest attempt");
    let triage = of(&view, TRIAGE);
    assert!(
        triage["judgement"].is_null(),
        "nobody has judged this attempt: {triage}"
    );
    assert_eq!(triage["earlier_judgement"]["candidate_id"], json!(first));
    assert_eq!(
        triage["coverage"], "failing",
        "a failing check is what this attempt is, whatever the last one was"
    );
    assert!(gap_holds(&view, "unjudged", TRIAGE), "{view}");

    // The first attempt still reads as judged: naming a candidate reads that
    // candidate, and the verdict did not move.
    let (st, v) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{id}/criteria?candidate={first}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(of(&v["criteria"], TRIAGE)["coverage"], "judged_met");

    // A candidate this node does not hold is a 404, not an empty reading that
    // would report every check as having produced nothing.
    let (st, _) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{id}/criteria?candidate=nope"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// An agent may propose what good means. That is all it may do, and the view
/// never reads a proposal as coverage.
#[tokio::test]
async fn an_agent_proposes_what_good_means_and_the_operator_decides_it() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    write_brief(
        &h,
        &id,
        &format!("# Brief: Overnight alert triage\n\n## Success criteria\n\n- decided: {TRIAGE}\n"),
    )
    .await;
    h.store
        .insert_session(&session_row("s1", &id, "fake"))
        .unwrap();
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;

    let call_args = json!({
        "criterion": TRIAGE,
        "kind": "check",
        "value": "just check",
    });

    // The shipped agreements name no `criteria_link`: a line in the operator's
    // own record of what the work is for is held for the operator first, and
    // nothing lands in the document until they allow it.
    let asked = tool(&h.harness, "s1", &token, "criteria_link", call_args.clone()).await;
    assert_eq!(asked["state"], "awaiting_operator", "{asked}");
    assert!(!brief_body(&h, &id).contains("just check"), "{asked}");

    // What this test is about is the line once it lands, not the asking, so the
    // agreements are widened to allow the call outright — the way the tracker
    // tool tests do. A deny still wins over this, so nothing it would refuse
    // rides in on it.
    let allow: tracon::policy::Rule = toml::from_str(
        r#"
        id = "test-allow-criteria-link"
        verdict = "allow"
        reason = "Under test."
        kinds = ["tool"]
        matches = ["criteria_link"]
        "#,
    )
    .unwrap();
    h.tools.policy.write().rules.push(allow);

    // The session's own idea of what would settle it, named by the criterion's
    // text — a key is not something an agent reading the brief has.
    let v = tool(&h.harness, "s1", &token, "criteria_link", call_args).await;
    assert!(
        v["standing"]
            .as_str()
            .unwrap_or_default()
            .contains("the operator decides"),
        "{v}"
    );
    let view = read_criteria(&h, &id).await;
    let triage = of(&view, TRIAGE);
    assert_eq!(triage["links"][0]["standard"], "proposed");
    assert_eq!(
        triage["links"][0]["provenance"], "inferred",
        "an omitted provenance is the default the tool advertises: {triage}"
    );
    assert_eq!(
        triage["coverage"], "only_proposed",
        "a proposal nobody agreed to is not coverage, however green it would go"
    );
    assert!(gap_holds(&view, "uncovered", TRIAGE), "{view}");
    assert!(
        triage["links"][0]["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "session" && r["value"] == "s1"),
        "the document records which session proposed it: {triage}"
    );

    // The two things a session cannot claim about a link are the two it cannot
    // claim about any brief line: it does not decide, and it does not report an
    // observation with nothing to point at.
    let decided = criteria::add_link(
        &h.store,
        h.manager.bus(),
        "n1",
        &id,
        TRIAGE,
        LinkInput {
            provenance: Some("decided".into()),
            kind: "check".into(),
            value: "just check".into(),
            refs: vec![],
        },
        None,
        &Author::Session("s1".into()),
    )
    .expect_err("a session does not decide what good means");
    assert!(
        decided
            .to_string()
            .contains("may not record an operator decision"),
        "{decided}"
    );
    let bare = criteria::add_link(
        &h.store,
        h.manager.bus(),
        "n1",
        &id,
        TRIAGE,
        LinkInput {
            provenance: Some("observed".into()),
            kind: "observation".into(),
            value: "two leads gave up".into(),
            refs: vec![],
        },
        None,
        &Author::Session("s1".into()),
    )
    .expect_err("an observation points at something");
    assert!(bare.to_string().contains("points at something"), "{bare}");

    // A kind the node does not read is refused rather than guessed at.
    let (st, v) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/{}/links", key_for(TRIAGE)),
        Some(json!({ "kind": "vibe", "value": "feels quick" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");

    // The operator's link is the standard, and the criterion reads differently
    // for it: the same command, now something a person agreed to.
    let (st, v) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/{}/links", key_for(TRIAGE)),
        Some(json!({ "kind": "check", "value": "just check", "provenance": "decided" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(of(&v["criteria"], TRIAGE)["coverage"], "no_result_yet");
    let body = brief_body(&h, &id);
    assert!(
        body.contains("  - inferred check: just check [session:s1]")
            && body.contains("  - decided check: just check"),
        "both stand in the document, each saying who is behind it: {body}"
    );

    // And taking the operator's back leaves only the proposal again.
    let (st, v) = call(
        &h.operator,
        "DELETE",
        &format!(
            "/api/work/{id}/criteria/{}/links/1?if_hash={}",
            key_for(TRIAGE),
            v["criteria"]["hash"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(of(&v["criteria"], TRIAGE)["coverage"], "only_proposed");
    let (st, _) = call(
        &h.operator,
        "DELETE",
        &format!(
            "/api/work/{id}/criteria/{}/links/7?if_hash={}",
            key_for(TRIAGE),
            v["criteria"]["hash"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "there is no seventh link");
}

/// A criterion is named by its text, so a reworded one is a different standard
/// — and the verdict about the old wording is reported, not carried over.
#[tokio::test]
async fn rewording_a_criterion_orphans_the_verdict_it_was_about() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    write_brief(
        &h,
        &id,
        &format!("# Brief: Overnight alert triage\n\n## Success criteria\n\n- decided: {TRIAGE}\n"),
    )
    .await;
    call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({ "criterion": TRIAGE, "verdict": "met" })),
    )
    .await;

    let reworded = "Triage finishes in under thirty seconds";
    write_brief(
        &h,
        &id,
        &format!(
            "# Brief: Overnight alert triage\n\n## Success criteria\n\n- decided: {reworded}\n"
        ),
    )
    .await;

    let view = read_criteria(&h, &id).await;
    let now = of(&view, reworded);
    assert!(
        now["judgement"].is_null() && now["earlier_judgement"].is_null(),
        "a different standard is a different criterion: {now}"
    );
    assert_eq!(now["coverage"], "nothing_points_at_it");
    let orphaned = &view["gaps"]["orphaned_judgements"];
    assert_eq!(
        orphaned[0]["criterion_text"], TRIAGE,
        "the verdict is reported with the wording it was about: {view}"
    );
    assert_eq!(orphaned[0]["verdict"], "met");
    assert!(
        view["summary"]
            .as_str()
            .unwrap()
            .contains("1 verdicts on criteria that were reworded"),
        "{view}"
    );

    // Identical lines have the same identity, and neither is safe to mutate.
    write_brief(
        &h,
        &id,
        &format!(
            "# Brief: Overnight alert triage\n\n## Success criteria\n\n- decided: {TRIAGE}\n  - decided check: just check\n- decided: {TRIAGE}\n"
        ),
    )
    .await;
    let view = read_criteria(&h, &id).await;
    assert_eq!(view["criteria"][0]["duplicate"], true);
    assert_eq!(view["criteria"][1]["duplicate"], true, "{view}");
    assert_eq!(view["criteria"][0]["links"][0]["value"], "just check");
    assert_eq!(view["criteria"][1]["links"].as_array().unwrap().len(), 0);
    // And naming it by text is now ambiguous, which is refused rather than
    // resolved to whichever line happens to come first.
    let (st, v) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({ "criterion": TRIAGE, "verdict": "met" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("more than one criterion"),
        "{v}"
    );
}

/// The screen where a person decides a change carries what that change was
/// supposed to achieve, read against the attempt on the screen.
#[tokio::test]
async fn the_review_screen_carries_the_criteria_of_the_item_it_was_pinned_to() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    write_brief(&h, &id, &brief_markdown()).await;
    // An externally-run harness, so opening the review reads the store rather
    // than trying to snapshot a runtime workspace this test does not have.
    h.store
        .insert_session(&session_row("s1", &id, "external"))
        .unwrap();
    let head = "c".repeat(40);
    let candidate = a_candidate(&h, "s1", &head, now_ms());
    a_check_run(&h, &candidate, "just check", "passed");
    h.store
        .insert_review_with_revision(
            &ReviewRow {
                id: "rv1".into(),
                session_id: "s1".into(),
                node_id: "n1".into(),
                channel: "personal".into(),
                kind: "pr".into(),
                title: "feat: triage".into(),
                body: "what the diff does not say".into(),
                edited_title: None,
                edited_body: None,
                provider: "github".into(),
                target: json!({ "project": "o/r" }).to_string(),
                diff: String::new(),
                files: "[]".into(),
                head_sha: head.clone(),
                base_ref: "main".into(),
                added: 0,
                removed: 0,
                state: "new".into(),
                verdict_reason: None,
                publish_result: None,
                claimed_ms: None,
                created_ms: now_ms(),
                created_mono_ms: 0,
                resolved_mono_ms: None,
                updated_ms: now_ms(),
                checks_json: None,
                review_session_id: None,
                ai_verdict_json: None,
                revision_patch: None,
            },
            &ReviewRevisionRow {
                id: "revision-1".into(),
                review_id: "rv1".into(),
                candidate_id: candidate.clone(),
                title: "feat: triage".into(),
                body: "what the diff does not say".into(),
                diff: String::new(),
                files: "[]".into(),
                head_sha: head,
                context_json: "[]".into(),
                requirements_work_item_id: Some(id.clone()),
                requirements_title: Some("Overnight alert triage".into()),
                requirements_body: Some(String::new()),
                requirements_hash: Some("h".into()),
                created_ms: now_ms(),
                intent_json: None,
            },
        )
        .unwrap();

    let (st, v) = call(&h.operator, "GET", "/api/reviews/rv1", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        v["criteria"]["candidate"]["id"],
        json!(candidate),
        "read against this revision's own attempt, not the branch's newest"
    );
    assert_eq!(of(&v["criteria"], TRIAGE)["coverage"], "checks_pass");
    assert_eq!(
        of(&v["criteria"], QUIET)["coverage"],
        "nothing_points_at_it",
        "what nothing points at is on the screen too, or the checks read as everything"
    );

    let mut reviewer = session_row("reviewer", &id, "fake");
    reviewer.phase = "review".into();
    reviewer.review_id = Some("rv1".into());
    h.store.insert_session(&reviewer).unwrap();
    let token = h
        .manager
        .register_tool_token_for_test("reviewer", "personal")
        .await;
    let read = tool(
        &h.harness,
        "reviewer",
        &token,
        "criteria_read",
        json!({"candidate": candidate}),
    )
    .await;
    assert_eq!(read["criteria"]["candidate"]["id"], candidate, "{read}");
    assert_eq!(of(&read["criteria"], TRIAGE)["coverage"], "checks_pass");
    let before = brief_body(&h, &id);
    let refused = tool(
        &h.harness,
        "reviewer",
        &token,
        "criteria_link",
        json!({"criterion": TRIAGE, "kind": "check", "value": "just check"}),
    )
    .await;
    assert!(
        refused
            .to_string()
            .contains("not offered to a review session"),
        "{refused}"
    );
    assert_eq!(brief_body(&h, &id), before);

    // A verdict given from that screen names the revision, so it is about what
    // was on it rather than about whatever the branch becomes next.
    let (st, judged) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({
            "criterion": key_for(TRIAGE),
            "verdict": "met",
            "candidate_id": candidate,
            "revision_id": "revision-1",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{judged}");
    let rows = h.store.criterion_judgements_for_item(&id).unwrap();
    assert_eq!(rows[0].revision_id.as_deref(), Some("revision-1"));
    assert_eq!(
        rows[0].criterion_text, TRIAGE,
        "the verdict keeps the wording it was about"
    );

    let (status, response) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(
            json!({"criterion": TRIAGE, "verdict": "met", "candidate_id": candidate,
                    "revision_id": "not-a-revision"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{response}");
    let error = criteria::judge(
        &h.store,
        &id,
        TRIAGE,
        None,
        Some("revision-1"),
        "met",
        None,
        &Author::Operator,
    )
    .expect_err("a revision cannot bind an unnamed candidate");
    assert!(matches!(error, criteria::CriteriaError::RevisionMismatch));
    let alternate = a_candidate(&h, "s1", &"d".repeat(40), now_ms() + 1);
    let (status, response) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(
            json!({"criterion": TRIAGE, "verdict": "met", "candidate_id": alternate,
                    "revision_id": "revision-1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    let other = an_item(&h, "Other").await;
    write_brief(&h, &other, &brief_markdown()).await;
    h.store
        .insert_session(&session_row("other-session", &other, "external"))
        .unwrap();
    let other_candidate = a_candidate(&h, "other-session", &"b".repeat(40), now_ms());
    let (status, response) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{other}/criteria/judgements"),
        Some(
            json!({"criterion": TRIAGE, "verdict": "met", "candidate_id": other_candidate,
                    "revision_id": "revision-1"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert!(h
        .store
        .criterion_judgements_for_item(&other)
        .unwrap()
        .is_empty());
    assert_eq!(h.store.criterion_judgements_for_item(&id).unwrap().len(), 1);

    // A revision can associate a candidate with another item even when the
    // owner session belongs to this one.
    let mut review = h.store.get_review("rv1").unwrap().unwrap();
    review.id = "rv2".into();
    let mut pinned = h.store.review_revision("revision-1").unwrap().unwrap();
    pinned.id = "revision-2".into();
    pinned.review_id = review.id.clone();
    pinned.requirements_work_item_id = Some(other.clone());
    h.store
        .insert_review_with_revision(&review, &pinned)
        .unwrap();
    let (status, scoped) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{other}/criteria?candidate={candidate}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{scoped}");
    let (status, response) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{other}/criteria/judgements"),
        Some(
            json!({"criterion": TRIAGE, "verdict": "met", "candidate_id": candidate,
                    "revision_id": "revision-2"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");

    // What a session is told before it proposes anything: how many criteria
    // there are, and how many nothing agreed points at.
    let standing = criteria::standing(&brief::Brief::parse(&brief_body(&h, &id)));
    assert_eq!(standing.total, 3);
    assert_eq!(standing.unlinked, 1, "{standing:?}");
    // Evidence exists, but a candidate from another channel is not evidence
    // for this item's criteria. The review must not pretend criteria are null.
    let mut foreign = h.store.candidate(&candidate).unwrap().unwrap();
    foreign.id = tracon::store::candidate_id(&"e".repeat(40), "work");
    foreign.head_sha = "e".repeat(40);
    foreign.channel = "work".into();
    h.store.insert_candidate(&foreign).unwrap();
    let mut bad_review = review.clone();
    bad_review.id = "rv3".into();
    let mut bad_revision = pinned.clone();
    bad_revision.id = "revision-3".into();
    bad_revision.review_id = bad_review.id.clone();
    bad_revision.candidate_id = foreign.id.clone();
    bad_revision.requirements_work_item_id = Some(id.clone());
    h.store
        .insert_review_with_revision(&bad_review, &bad_revision)
        .unwrap();
    let (status, _) = call(&h.operator, "GET", "/api/reviews/rv3", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let absent = an_item(&h, "No brief").await;
    let mut no_brief_review = review.clone();
    no_brief_review.id = "rv4".into();
    let mut no_brief_revision = pinned.clone();
    no_brief_revision.id = "revision-4".into();
    no_brief_revision.review_id = no_brief_review.id.clone();
    no_brief_revision.requirements_work_item_id = Some(absent.clone());
    h.store
        .insert_review_with_revision(&no_brief_review, &no_brief_revision)
        .unwrap();
    let (status, response) = call(&h.operator, "GET", "/api/reviews/rv4", None).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert!(response["criteria"].is_null());
    tracon::corpus::work::remove(&h.store, &h.bus, "n1", &other).unwrap();
    let (status, response) = call(&h.operator, "GET", "/api/reviews/rv2", None).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert!(response["criteria"].is_null());
}

#[tokio::test]
async fn operators_and_code_identifiers_do_not_share_verdicts() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Latency").await;
    let before = "Latency < 2 seconds";
    let after = "Latency > 2 seconds";
    write_brief(
        &h,
        &id,
        &format!("# Brief: Latency\n\n## Success criteria\n\n- decided: {before}\n"),
    )
    .await;
    let (status, _) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({"criterion": before, "verdict": "met", "candidate_id": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    write_brief(
        &h,
        &id,
        &format!("# Brief: Latency\n\n## Success criteria\n\n- decided: {after}\n"),
    )
    .await;
    let view = read_criteria(&h, &id).await;
    assert!(of(&view, after)["judgement"].is_null(), "{view}");
    assert_eq!(
        view["gaps"]["orphaned_judgements"][0]["criterion_text"],
        before
    );
    assert_ne!(key_for("Status == READY"), key_for("Status == ready"));
    assert_eq!(key_for("  Status == READY  "), key_for("Status == READY"));
}

#[tokio::test]
async fn foreign_candidate_is_not_evidence_for_another_item() {
    state::isolate();
    let h = harness().await;
    let a = an_item(&h, "A").await;
    let b = an_item(&h, "B").await;
    write_brief(&h, &a, "# Brief: A\n\n## Success criteria\n\n- decided: Latency < 2 seconds\n  - decided check: just check\n").await;
    h.store
        .insert_session(&session_row("foreign", &b, "external"))
        .unwrap();
    let foreign = a_candidate(&h, "foreign", &"f".repeat(40), now_ms());
    a_check_run(&h, &foreign, "just check", "passed");
    let (status, _) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{a}/criteria?candidate={foreign}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{a}/criteria/judgements"),
        Some(
            json!({"criterion": "Latency < 2 seconds", "verdict": "met", "candidate_id": foreign}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(h
        .store
        .criterion_judgements_for_item(&a)
        .unwrap()
        .is_empty());
    h.store
        .insert_session(&session_row("s1", &a, "external"))
        .unwrap();
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    let denied = tool(
        &h.harness,
        "s1",
        &token,
        "criteria_read",
        json!({"candidate": foreign}),
    )
    .await;
    let text = denied.to_string();
    assert!(
        text.contains("no candidate")
            && !text.contains("run-")
            && !text.contains("passed")
            && !text.contains(&"f".repeat(40)),
        "{denied}"
    );

    let mut wrong_channel = h.store.candidate(&foreign).unwrap().unwrap();
    wrong_channel.channel = "work".into();
    wrong_channel.id = tracon::store::candidate_id(&"e".repeat(40), "work");
    wrong_channel.head_sha = "e".repeat(40);
    h.store.insert_candidate(&wrong_channel).unwrap();
    let (status, _) = call(
        &h.operator,
        "GET",
        &format!("/api/work/{a}/criteria?candidate={}", wrong_channel.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let denied = tool(
        &h.harness,
        "s1",
        &token,
        "criteria_read",
        json!({"candidate": wrong_channel.id}),
    )
    .await;
    assert!(denied.to_string().contains("no candidate"));
}

#[tokio::test]
async fn candidate_less_verdict_stays_candidate_less_after_capture() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "A").await;
    write_brief(
        &h,
        &id,
        "# Brief: A\n\n## Success criteria\n\n- decided: Works\n",
    )
    .await;
    let loaded = read_criteria(&h, &id).await;
    assert!(loaded["candidate"].is_null());
    h.store
        .insert_session(&session_row("s1", &id, "external"))
        .unwrap();
    let candidate = a_candidate(&h, "s1", &"a".repeat(40), now_ms());
    let (status, written) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({"criterion": "Works", "verdict": "met", "candidate_id": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert!(written["judgement"]["candidate_id"].is_null(), "{written}");
    assert!(written["criteria"]["candidate"].is_null(), "{written}");
    let latest = read_criteria(&h, &id).await;
    assert_eq!(latest["candidate"]["id"], candidate);
    assert_ne!(of(&latest, "Works")["coverage"], "judged_met");
    let second = a_candidate(&h, "s1", &"b".repeat(40), now_ms() + 1000);
    let (status, pinned) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({"criterion": "Works", "verdict": "not_met", "candidate_id": candidate})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pinned}");
    assert_eq!(pinned["criteria"]["candidate"]["id"], candidate);
    assert_eq!(
        of(&read_criteria(&h, &id).await, "Works")["earlier_judgement"]["candidate_id"],
        candidate
    );
    assert_eq!(read_criteria(&h, &id).await["candidate"]["id"], second);
}

#[tokio::test]
async fn stale_link_index_never_deletes_the_next_link() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "A").await;
    write_brief(&h, &id, "# Brief: A\n\n## Success criteria\n\n- decided: Works\n  - decided check: A\n  - decided check: B\n  - decided check: C\n").await;
    let stale = read_criteria(&h, &id).await;
    let key = key_for("Works");
    let uri = format!("/api/work/{id}/criteria/{key}/links");
    let (status, _) = call(
        &h.operator,
        "DELETE",
        &format!("{uri}/0?if_hash={}", stale["hash"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &h.operator,
        "DELETE",
        &format!("{uri}/1?if_hash={}", stale["hash"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(&h.operator, "DELETE", &format!("{uri}/0"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let current = read_criteria(&h, &id).await;
    let values: Vec<_> = of(&current, "Works")["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|link| link["value"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(values, ["B", "C"]);
    let (status, _) = call(
        &h.operator,
        "DELETE",
        &format!("{uri}/0?if_hash={}", current["hash"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let latest = read_criteria(&h, &id).await;
    assert_eq!(of(&latest, "Works")["links"][0]["value"], "C");
}

#[tokio::test]
async fn attaching_another_brief_does_not_transfer_agreement() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "First").await;
    let other = an_item(&h, "Second").await;
    write_brief(
        &h,
        &id,
        "# Brief: First\n\n## Success criteria\n\n- decided: Works\n",
    )
    .await;
    write_brief(
        &h,
        &other,
        "# Brief: Second\n\n## Success criteria\n\n- decided: Works\n",
    )
    .await;
    let (status, _) = call(
        &h.operator,
        "POST",
        &format!("/api/work/{id}/criteria/judgements"),
        Some(json!({"criterion": "Works", "verdict": "met", "candidate_id": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let slug = h
        .store
        .work_get(&other)
        .unwrap()
        .unwrap()
        .brief_slug
        .unwrap();
    tracon::corpus::work::set_brief(&h.store, &h.bus, "n1", &id, Some(&slug)).unwrap();
    let view = read_criteria(&h, &id).await;
    assert!(of(&view, "Works")["judgement"].is_null(), "{view}");
    assert!(
        view["gaps"]["orphaned_judgements"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{view}"
    );
}

#[tokio::test]
async fn duplicate_rows_retain_their_own_link_coverage() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Duplicates").await;
    write_brief(&h, &id, "# Brief: Duplicates\n\n## Success criteria\n\n- decided: Works\n  - decided check: just check\n- decided: Works\n").await;
    let view = read_criteria(&h, &id).await;
    assert_eq!(view["criteria"][0]["duplicate"], true);
    assert_eq!(view["criteria"][1]["duplicate"], true);
    assert_eq!(view["criteria"][0]["coverage"], "no_result_yet");
    assert_eq!(view["criteria"][1]["coverage"], "nothing_points_at_it");
    assert_eq!(view["gaps"]["uncovered"].as_array().unwrap().len(), 1);
}
