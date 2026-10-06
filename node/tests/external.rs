//! A harness the operator runs themselves, through the operator door: every
//! call stands alone, with no session, is offered the channel's tools minus a
//! review session's verdict, is logged to the channel's external log under
//! the lane it gives, and a call the policy does not name becomes an approval
//! on the same queue as any other.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness_with, Harness};
use support::http::call;
use support::state;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use tracon::config::Config;
use tracon::stream::Frame;

fn enabled() -> Config {
    let mut cfg = Config::default();
    cfg.external.enabled = true;
    cfg
}

async fn mcp(app: &axum::Router, channel: &str, body: Value) -> (StatusCode, Value) {
    let (status, v, _) = mcp_as(app, channel, None, body).await;
    (status, v)
}

/// A call from the client that echoes `client` as its `Mcp-Session-Id`, and
/// the id the door answered with.
async fn mcp_as(
    app: &axum::Router,
    channel: &str,
    client: Option<&str>,
    body: Value,
) -> (StatusCode, Value, Option<String>) {
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/external/{channel}"))
        .header("host", "127.0.0.1:7420")
        .header("content-type", "application/json");
    if let Some(client) = client {
        req = req.header("mcp-session-id", client);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let echoed = res
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        echoed,
    )
}

fn ping() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"ping"})
}

/// No call leaves a session behind.
fn assert_no_sessions(h: &Harness) {
    let rows: Vec<_> = h
        .store
        .list_sessions(None)
        .unwrap()
        .into_iter()
        .filter(|s| s.harness_id == "external")
        .collect();
    assert!(rows.is_empty(), "{rows:?}");
}

/// A call labelled with `lane`, as Claude Code's `headersHelper` sends it.
async fn mcp_labelled(app: &axum::Router, channel: &str, lane: &str, body: Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/external/{channel}"))
        .header("host", "127.0.0.1:7420")
        .header("content-type", "application/json")
        .header("x-tracon-agent", lane)
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The channel's external log, oldest first.
async fn log(h: &Harness, channel: &str) -> Vec<Value> {
    let (_, v) = call(
        &h.operator,
        "GET",
        &format!("/api/external/{channel}/events"),
        None,
    )
    .await;
    v["events"].as_array().cloned().unwrap_or_default()
}

async fn list(app: &axum::Router, channel: &str) -> Vec<String> {
    let (_, v) = mcp(
        app,
        channel,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    let empty = vec![];
    v["result"]["tools"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

fn tool_call(name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":name,"arguments":args}})
}

#[tokio::test]
async fn a_node_that_does_not_answer_external_harnesses_says_which_key_turns_it_on() {
    state::isolate();
    let h = harness_with(Config::default()).await;
    let (status, v) = mcp(
        &h.operator,
        "work",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("[external]"), "{message}");
    assert_no_sessions(&h);
}

#[tokio::test]
async fn a_channel_this_node_does_not_hold_is_refused_in_words() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (status, v) = mcp(
        &h.operator,
        "nope",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("create or enroll"), "{message}");
}

/// Connecting and calling leave no session and hand out no id: every request
/// stands alone.
#[tokio::test]
async fn calls_leave_no_session_and_hand_out_no_id() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (status, _, echoed) = mcp_as(
        &h.operator,
        "work",
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(echoed, None);
    for _ in 0..3 {
        let (status, _, echoed) = mcp_as(&h.operator, "work", None, ping()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(echoed, None);
    }
    mcp(
        &h.operator,
        "work",
        tool_call("recall", json!({ "query": "x" })),
    )
    .await;
    assert_no_sessions(&h);

    let (_, queue) = call(&h.operator, "GET", "/api/queue", None).await;
    assert!(queue["running"].as_array().unwrap().is_empty(), "{queue}");

    // It ran against no repository; the picker must not offer an empty path.
    let (_, repos) = call(&h.operator, "GET", "/api/repos/recent", None).await;
    assert!(
        !repos["repos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["repo_path"] == json!("")),
        "{repos}"
    );
}
#[tokio::test]
async fn an_attachment_is_offered_the_channels_tools_but_not_a_review_sessions_verdict() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let names = list(&h.operator, "work").await;
    for wanted in [
        "recall",
        "doc_read",
        "work_ready",
        "work_discover",
        "work_close",
        "submit_review",
        "review_status",
    ] {
        assert!(
            names.contains(&wanted.to_string()),
            "{wanted} not in {names:?}"
        );
    }
    assert!(!names.contains(&"review_verdict".to_string()), "{names:?}");

    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call(
            "review_verdict",
            json!({ "verdict": "approve", "summary": "x" }),
        ),
    )
    .await;
    assert_eq!(v["result"]["isError"], json!(true));
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("review session"), "{text}");

    // A submission without a worktree says which argument it needs.
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("submit_review", json!({ "title": "x" })),
    )
    .await;
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("worktree is required"), "{text}");
}

fn sh(dir: &std::path::Path, script: &str) {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository under `<scratch>/root` with a bare origin, and a linked
/// worktree outside that root, on a branch one commit beyond main.
fn repo_with_worktree(name: &str) -> (std::path::PathBuf, String) {
    let dir = state::scratch(&format!("external-{name}"));
    std::fs::create_dir_all(dir.join("root")).unwrap();
    sh(&dir, "git init -q --bare -b main origin.git");
    sh(
        &dir,
        "git clone -q origin.git root/repo && cd root/repo && git checkout -qB main \
         && git config user.email t@e && git config user.name t \
         && echo base > a.txt && git add -A && git commit -qm base && git push -q origin main \
         && git worktree add -b feat/x ../../wt 2>/dev/null \
         && cd ../../wt && printf 'one\\ntwo\\n' >> a.txt && git add -A && git commit -qm work",
    );
    (
        dir.join("root"),
        dir.join("wt").to_string_lossy().into_owned(),
    )
}

fn with_roots(root: &std::path::Path) -> Config {
    let mut cfg = enabled();
    cfg.external.repo_roots = vec![root.to_path_buf()];
    cfg
}

fn submit_args(worktree: &str) -> Value {
    json!({
        "title": "feat: x", "body": "why", "provider": "github",
        "project": "owner/name", "base": "main", "worktree": worktree
    })
}

/// A tool result: whether it is an error, and what it said.
fn outcome(v: &Value) -> (bool, Value) {
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
    (
        v["result"]["isError"] == json!(true),
        serde_json::from_str(text).unwrap_or(json!(text)),
    )
}

#[tokio::test]
async fn an_external_harness_puts_its_own_worktree_up_for_review() {
    state::isolate();
    let (root, wt) = repo_with_worktree("submit");
    let h = harness_with(with_roots(&root)).await;
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("submit_review", submit_args(&wt)),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    // One file changed by two added lines: the count is files, not lines.
    assert_eq!(out["files"], 1, "{out}");
    assert_eq!(out["added"], 2, "{out}");
    assert_eq!(out["removed"], 0, "{out}");
    let r = h
        .store
        .get_review(out["review_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(r.diff.contains("a.txt"), "{}", r.diff);
    let target: Value = serde_json::from_str(&r.target).unwrap();
    assert_eq!(target["branch"], "feat/x");
    let canonical = std::fs::canonicalize(&wt).unwrap();
    assert_eq!(target["worktree"], json!(canonical.to_string_lossy()));
    // The operator's toolchain is where its checks run, not a container.
    assert!(r.checks_json.is_none());
    assert_eq!(r.session_id, None);
}

#[tokio::test]
async fn a_worktree_whose_repository_is_outside_the_roots_is_refused() {
    state::isolate();
    let (root, wt) = repo_with_worktree("outside");
    let elsewhere = root.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let h = harness_with(with_roots(&elsewhere)).await;
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("submit_review", submit_args(&wt)),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(err);
    assert!(out.to_string().contains("repo_roots"), "{out}");
    let (_, queue) = call(&h.operator, "GET", "/api/queue", None).await;
    assert!(queue["reviews"].as_array().unwrap().is_empty(), "{queue}");
}

/// What one external harness submitted belongs to the channel's external
/// callers: whatever lane asks after it is answered, and its card names the
/// lane that submitted it.
#[tokio::test]
async fn a_review_belongs_to_the_channels_external_callers() {
    state::isolate();
    let (root, wt) = repo_with_worktree("owned");
    let h = harness_with(with_roots(&root)).await;
    let v = mcp_labelled(
        &h.operator,
        "work",
        "repo:feat/x",
        tool_call("submit_review", submit_args(&wt)),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    let review = out["review_id"].as_str().unwrap().to_string();
    let row = h.store.get_review(&review).unwrap().unwrap();
    assert_eq!(row.lane.as_deref(), Some("repo:feat/x"));
    assert_eq!(row.session_id, None);

    let v = mcp_labelled(
        &h.operator,
        "work",
        "repo:other",
        tool_call(
            "review_status",
            json!({ "review_id": review, "wait_secs": 0 }),
        ),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert_eq!(out["state"], "new");
}
#[tokio::test]
async fn an_external_caller_closes_an_item_by_id() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (status, _) = call(
        &h.operator,
        "POST",
        "/api/work",
        Some(json!({ "title": "finish it", "channel": "work" })),
    )
    .await;
    assert!(status.is_success(), "{status}");
    let item = h.store.work_ready("work", None).unwrap()[0].item.id.clone();

    let (_, v) = mcp(&h.operator, "work", tool_call("work_close", json!({}))).await;
    let (err, out) = outcome(&v);
    assert!(err);
    assert!(out.to_string().contains("id"), "{out}");

    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("work_close", json!({ "id": item, "summary": "done" })),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert_eq!(out["state"], "closed");
    assert!(
        log(&h, "work")
            .await
            .iter()
            .any(|e| e["kind"] == "work_closed"),
        "the close is on the channel's external log"
    );
}
/// Ask for something the operator decides: the call answers at once with an
/// approval and runs nothing.
async fn ask(h: &Harness, name: &str, args: Value) -> String {
    let (_, v) = mcp(&h.operator, "work", tool_call(name, args)).await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert_eq!(out["state"], "awaiting_operator", "{out}");
    out["approval_id"].as_str().unwrap().to_string()
}

async fn answer(h: &Harness, id: &str, body: Value) -> (StatusCode, Value) {
    call(
        &h.operator,
        "POST",
        &format!("/api/permissions/{id}/answer"),
        Some(body),
    )
    .await
}

/// `approval_status` for one id, waiting up to `wait` seconds.
async fn approval(h: &Harness, id: &str, wait: u64) -> Value {
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call(
            "approval_status",
            json!({ "approval_id": id, "wait_secs": wait }),
        ),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    out
}

/// What an approval settled as, waiting for the run to finish.
async fn settled(h: &Harness, id: &str) -> Value {
    for _ in 0..100 {
        let out = approval(h, id, 0).await;
        if !matches!(out["state"].as_str(), Some("still_waiting" | "running")) {
            return out;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("approval {id} never settled");
}

/// A tool the policy does not decide is held for the operator. The call does
/// not wait: it says so at once, the card is on the queue, and allowing it
/// runs the call and keeps its result for the caller.
#[tokio::test]
async fn a_call_the_operator_decides_returns_at_once_and_runs_once_allowed() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-x", "body": "hello" }),
    )
    .await;
    assert!(h.store.doc_get("work", "plan-x").unwrap().is_none());
    let queue = h.store.open_permission_views().unwrap();
    let card = queue.iter().find(|p| p.id == id).expect("a card for it");
    assert_eq!(card.kind.as_deref(), Some("tool"));
    assert!(card.title.starts_with("doc_write"), "{}", card.title);
    assert_eq!(approval(&h, &id, 0).await["state"], "still_waiting");
    assert_eq!(h.store.get_approval(&id).unwrap().unwrap().session_id, None);

    let (status, _) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    let out = settled(&h, &id).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(out["result"]["slug"], "plan-x", "{out}");
    assert_eq!(
        h.store.doc_get("work", "plan-x").unwrap().unwrap().body,
        "hello"
    );
    assert!(h
        .store
        .open_permission_views()
        .unwrap()
        .iter()
        .all(|p| p.id != id));
}

#[tokio::test]
async fn an_edited_answer_runs_the_tool_with_the_operators_words() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-x", "body": "the agent's draft" }),
    )
    .await;
    let (status, _) = answer(
        &h,
        &id,
        json!({
            "option_id": "allow_once",
            "arguments": { "slug": "plan-x", "body": "the operator's words" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let out = settled(&h, &id).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(out["edited_arguments"]["body"], "the operator's words");
    let doc = h.store.doc_get("work", "plan-x").unwrap().unwrap();
    assert_eq!(doc.body, "the operator's words");
}

#[tokio::test]
async fn an_edit_is_held_to_the_same_refusals_as_the_call() {
    state::isolate();
    let h = harness_with(enabled()).await;
    for rule in h.tools.policy.write().rules.iter_mut() {
        rule.matches.retain(|m| m != "retain");
    }
    let id = ask(
        &h,
        "retain",
        json!({ "kind": "fact", "scope": "global", "body": "a draft" }),
    )
    .await;
    let (status, _) = answer(
        &h,
        &id,
        json!({
            "option_id": "allow_once",
            "arguments": { "kind": "fact", "scope": "global", "body": "see .claude/settings" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let out = settled(&h, &id).await;
    assert_eq!(out["state"], "failed", "{out}");
    assert!(
        out["reason"]
            .as_str()
            .is_some_and(|r| r.contains("refused by policy")),
        "{out}"
    );
}

#[tokio::test]
async fn a_refused_call_says_the_operator_refused_it() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-x", "body": "hello" }),
    )
    .await;
    let (status, _) = answer(&h, &id, json!({ "option_id": "reject_once" })).await;
    assert_eq!(status, StatusCode::OK);
    let out = approval(&h, &id, 0).await;
    assert_eq!(out["state"], "rejected");
    assert!(
        out["reason"]
            .as_str()
            .is_some_and(|r| r.contains("did not allow")),
        "{out}"
    );
    // Answered once is answered.
    let (status, _) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_ne!(status, StatusCode::OK);
    assert!(h.store.doc_get("work", "plan-x").unwrap().is_none());
}

#[tokio::test]
async fn a_refusal_carries_the_operators_reason() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = ask(&h, "doc_write", json!({ "slug": "plan-x", "body": "hi" })).await;
    let (status, _) = answer(
        &h,
        &id,
        json!({ "option_id": "reject_once", "reason": "  this belongs in plan-y  " }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let out = approval(&h, &id, 0).await;
    assert_eq!(out["state"], "rejected", "{out}");
    assert_eq!(out["reason"], "this belongs in plan-y", "{out}");
    assert!(
        out["message"]
            .as_str()
            .is_some_and(|m| m.contains("nothing ran")),
        "{out}"
    );
}

/// Asking for changes settles the call with the operator's notes; revising
/// it and calling again is a new approval.
#[tokio::test]
async fn a_request_for_changes_returns_notes_and_a_revised_call_asks_anew() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let args = json!({ "slug": "plan-x", "body": "draft one" });
    let id = ask(&h, "doc_write", args.clone()).await;
    let (status, v) = answer(&h, &id, json!({ "option_id": "request_changes" })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["fields"][0]["field"], "notes", "{v}");
    assert_eq!(approval(&h, &id, 0).await["state"], "still_waiting");

    let (status, _) = answer(
        &h,
        &id,
        json!({ "option_id": "request_changes", "notes": "add a rollout section" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let out = approval(&h, &id, 0).await;
    assert_eq!(out["state"], "changes_requested", "{out}");
    assert_eq!(out["notes"], "add a rollout section", "{out}");
    assert!(
        out["message"]
            .as_str()
            .is_some_and(|m| m.contains("call doc_write again")),
        "{out}"
    );
    assert!(h.store.doc_get("work", "plan-x").unwrap().is_none());
    assert!(h
        .store
        .open_permission_views()
        .unwrap()
        .iter()
        .all(|p| p.id != id));

    let again = ask(&h, "doc_write", args).await;
    assert_ne!(again, id, "a settled approval is not asked again");
    let revised = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-x", "body": "draft two\n\n## Rollout" }),
    )
    .await;
    assert_ne!(revised, id);
}

#[tokio::test]
async fn an_edit_that_does_not_fit_the_tool_is_refused_and_still_waits() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-x", "body": "draft" }),
    )
    .await;
    let (status, v) = answer(
        &h,
        &id,
        json!({ "option_id": "allow_once", "arguments": { "slug": "plan-x", "body": 5 } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let fields = v["error"]["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 1, "{v}");
    assert_eq!(fields[0]["field"], "body", "{v}");
    assert_eq!(approval(&h, &id, 0).await["state"], "still_waiting");
    assert!(h.store.doc_get("work", "plan-x").unwrap().is_none());

    let (status, _) = answer(
        &h,
        &id,
        json!({ "option_id": "allow_once", "arguments": { "slug": "plan-x", "body": "fixed" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settled(&h, &id).await["state"], "succeeded");
}

#[tokio::test]
async fn an_edit_may_not_change_what_the_call_acts_on() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = ask(
        &h,
        "issue_comment",
        json!({ "key": "WRK-1", "body": "a comment" }),
    )
    .await;
    let (status, v) = answer(
        &h,
        &id,
        json!({
            "option_id": "allow_once",
            "arguments": { "key": "WRK-2", "body": "a comment" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["fields"][0]["field"], "key", "{v}");
    assert_eq!(approval(&h, &id, 0).await["state"], "still_waiting");
}

/// The page an approval opens on: everything the operator needs to decide
/// it, claimed while it is open and freed when they leave.
#[tokio::test]
async fn opening_an_approval_claims_it_and_leaving_releases_it() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = ask(
        &h,
        "issue_create",
        json!({ "project": "WRK", "type": "Task", "summary": "Rotate the keys", "description": "h1. Why" }),
    )
    .await;
    let card = h
        .store
        .open_permission_views()
        .unwrap()
        .into_iter()
        .find(|p| p.id == id)
        .unwrap();
    assert_eq!(card.title, "issue_create WRK Task: Rotate the keys");

    let (status, v) = call(&h.operator, "GET", &format!("/api/approvals/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["approval"]["id"], id.as_str());
    assert_eq!(v["arguments"]["summary"], "Rotate the keys");
    assert_eq!(v["format"], "jira_wiki");
    assert_eq!(v["prose_fields"], json!(["description"]));
    assert_eq!(
        v["locked_fields"],
        json!(["key", "project", "repo", "number", "iid"])
    );
    assert_eq!(v["input_schema"]["required"][0], "project");
    assert!(v["approval"]["claimed_ms"].is_i64(), "{v}");
    assert!(v["claimed_before_ms"].is_null(), "{v}");
    assert!(v["options"]
        .as_array()
        .unwrap()
        .iter()
        .any(|o| o["option_id"] == "request_changes"));
    let (_, again) = call(&h.operator, "GET", &format!("/api/approvals/{id}"), None).await;
    assert!(again["claimed_before_ms"].is_i64(), "{again}");

    let (status, _) = call(
        &h.operator,
        "POST",
        &format!("/api/approvals/{id}/release"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h
        .store
        .get_approval(&id)
        .unwrap()
        .unwrap()
        .claimed_ms
        .is_none());

    let (status, _) = call(&h.operator, "GET", "/api/approvals/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A document write opens with its body as the prose and its slug locked: the
/// body may be rewritten, the document it lands in may not.
#[tokio::test]
async fn a_document_card_locks_its_slug_and_takes_an_edited_body() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-foo", "body": "# Draft" }),
    )
    .await;
    let (_, v) = call(&h.operator, "GET", &format!("/api/approvals/{id}"), None).await;
    assert_eq!(v["approval"]["title"], "doc_write plan-foo");
    assert_eq!(v["format"], "markdown");
    assert_eq!(v["prose_fields"], json!(["body"]));
    let locked = v["locked_fields"].as_array().unwrap();
    assert!(
        locked.contains(&json!("slug")) && locked.contains(&json!("if_hash")),
        "{v}"
    );

    let (status, v) = answer(
        &h,
        &id,
        json!({ "option_id": "allow_once", "arguments": { "slug": "plan-bar", "body": "# Draft" } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["fields"][0]["field"], "slug", "{v}");
    let (status, _) = answer(
        &h,
        &id,
        json!({ "option_id": "allow_once", "arguments": { "slug": "plan-foo", "body": "# Final" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settled(&h, &id).await["state"], "succeeded");
    assert_eq!(
        h.store.doc_get("work", "plan-foo").unwrap().unwrap().body,
        "# Final"
    );
    assert!(h.store.doc_get("work", "plan-bar").unwrap().is_none());
}

/// `retain` runs unattended under the shipped agreements, but a node's policy
/// can ask for it; then its kind is locked and its body is the prose.
#[tokio::test]
async fn a_memory_card_locks_its_kind_and_takes_an_edited_body() {
    state::isolate();
    let h = harness_with(enabled()).await;
    for rule in h.tools.policy.write().rules.iter_mut() {
        rule.matches.retain(|m| m != "retain");
    }
    let id = ask(
        &h,
        "retain",
        json!({ "kind": "fact", "scope": "global", "body": "The importer retries twice." }),
    )
    .await;
    let (_, v) = call(&h.operator, "GET", &format!("/api/approvals/{id}"), None).await;
    assert_eq!(
        v["approval"]["title"],
        "retain fact: The importer retries twice."
    );
    assert_eq!(v["prose_fields"], json!(["body"]));
    assert!(v["locked_fields"]
        .as_array()
        .unwrap()
        .contains(&json!("kind")));
    assert_eq!(v["input_schema"]["required"], json!(["kind", "body"]));

    let (status, v) = answer(
        &h,
        &id,
        json!({ "option_id": "allow_once", "arguments": { "kind": "lesson", "body": "b" } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["fields"][0]["field"], "kind", "{v}");
    let edit =
        json!({ "kind": "fact", "scope": "global", "body": "The importer retries three times." });
    let (status, _) = answer(
        &h,
        &id,
        json!({ "option_id": "allow_once", "arguments": edit }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let out = settled(&h, &id).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(out["edited_arguments"], edit);
}

/// A client that retries — or a reconnect that asks again — does not queue a
/// second card for the same request.
#[tokio::test]
async fn asking_again_while_it_waits_is_the_same_approval() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let args = json!({ "slug": "plan-x", "body": "hello" });
    let first = ask(&h, "doc_write", args.clone()).await;
    let again = ask(&h, "doc_write", args).await;
    assert_eq!(first, again);
    let other = ask(&h, "doc_write", json!({ "slug": "plan-x", "body": "else" })).await;
    assert_ne!(first, other);
    let cards = h.store.open_permission_views().unwrap();
    assert_eq!(cards.iter().filter(|p| p.id == first).count(), 1);
    assert_eq!(cards.len(), 2);
}

#[tokio::test]
async fn an_unanswered_approval_expires() {
    state::isolate();
    let mut cfg = enabled();
    cfg.session.approval_expiry_secs = 1;
    let h = harness_with(cfg).await;
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-x", "body": "hello" }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let out = approval(&h, &id, 0).await;
    assert_eq!(out["state"], "expired", "{out}");
    assert!(h.store.open_permission_views().unwrap().is_empty());
    let (status, _) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_ne!(status, StatusCode::OK);
    assert!(h.store.doc_get("work", "plan-x").unwrap().is_none());
}

/// One wait covers several approvals and ends when any one is decided.
#[tokio::test]
async fn one_wait_covers_a_list_and_returns_when_any_is_decided() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let a = ask(&h, "doc_write", json!({ "slug": "plan-a", "body": "a" })).await;
    let b = ask(&h, "doc_write", json!({ "slug": "plan-b", "body": "b" })).await;
    let app = h.operator.clone();
    let ids = json!([a.clone(), b.clone()]);
    let started = std::time::Instant::now();
    let waiting = tokio::spawn(async move {
        mcp(
            &app,
            "work",
            tool_call(
                "approval_status",
                json!({ "approval_ids": ids, "wait_secs": 30 }),
            ),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    answer(&h, &b, json!({ "option_id": "reject_once" })).await;
    let (_, v) = waiting.await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert_eq!(out["still_waiting"], false);
    let states: Vec<(String, String)> = out["approvals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["approval_id"].as_str().unwrap().to_string(),
                v["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        states,
        [(a, "still_waiting".into()), (b, "rejected".into())]
    );
}

/// An approval runs long after it was asked, so the channel's Stop is checked
/// again when it runs: allowing it on a stopped channel runs nothing.
#[tokio::test]
async fn an_approval_does_not_run_on_a_stopped_channel() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-x", "body": "hello" }),
    )
    .await;
    let (status, _) = call(&h.operator, "POST", "/api/external/work/stop", None).await;
    assert!(status.is_success(), "{status}");
    let (status, _) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    for _ in 0..100 {
        let row = h.store.get_approval(&id).unwrap().unwrap();
        if row.state != "running" && row.state != "pending" {
            assert_eq!(row.state, "failed", "{row:?}");
            assert!(
                row.reason.as_deref().is_some_and(|r| r.contains("stopped")),
                "{row:?}"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(h.store.doc_get("work", "plan-x").unwrap().is_none());
}
/// A GitHub-shaped forge for one pull request whose head the test can move.
async fn fake_github(head: Arc<std::sync::Mutex<String>>) -> std::net::SocketAddr {
    let pulls = head.clone();
    let app = axum::Router::new()
        .route(
            "/repos/o/n/pulls/1",
            axum::routing::get(move || {
                let head = pulls.lock().unwrap().clone();
                async move { axum::Json(json!({ "number": 1, "head": { "sha": head } })) }
            }),
        )
        .route(
            "/repos/o/n/pulls/1/merge",
            axum::routing::put(|| async {
                axum::Json(json!({ "merged": true, "sha": "merged1", "message": "ok" }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn with_github(h: &Harness, head: &str) -> Arc<std::sync::Mutex<String>> {
    let head = Arc::new(std::sync::Mutex::new(head.to_string()));
    let addr = fake_github(head.clone()).await;
    h.tools.broker.write().unwrap().put(
        "gh",
        tracon::broker::Credential {
            env: [
                ("GH_TOKEN".to_string(), "t".to_string()),
                ("GITHUB_API".to_string(), format!("http://{addr}")),
            ]
            .into_iter()
            .collect(),
            channels: vec!["work".into()],
            ..Default::default()
        },
    );
    head
}

fn merge_args(operation_id: &str) -> Value {
    json!({
        "repo": "o/n", "number": 1, "head_sha": "abcdef1",
        "operation_id": operation_id,
    })
}

/// A scoped grant still decides a call on the spot: it runs inline and the
/// caller gets its result, with no approval in between.
#[tokio::test]
async fn a_call_a_grant_covers_runs_inline() {
    state::isolate();
    let h = harness_with(enabled()).await;
    with_github(&h, "abcdef1").await;
    h.store
        .authority_grant_insert(&tracon::store::AuthorityGrantRow {
            id: "g1".into(),
            action: "merge".into(),
            verdict: "allow".into(),
            target: "github:o/n:pr:1".into(),
            channel: "work".into(),
            session_id: None,
            revision: None,
            expires_ms: None,
            revoked_ms: None,
            reason: "test".into(),
            created_ms: tracon::store::now_ms(),
        })
        .unwrap();
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("pr_merge", merge_args("op-grant-1")),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert_eq!(out["merged"], true, "{out}");
    assert!(h.store.open_permission_views().unwrap().is_empty());
}

/// What was true when the call was asked is checked again when it runs: a
/// pull request whose head moved in between is not merged on the old approval.
#[tokio::test]
async fn an_approved_merge_checks_the_head_again_when_it_runs() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let head = with_github(&h, "abcdef1").await;
    let id = ask(&h, "pr_merge", merge_args("op-recheck-1")).await;
    *head.lock().unwrap() = "fedcba9".into();
    answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    let out = settled(&h, &id).await;
    assert_eq!(out["state"], "failed", "{out}");
    assert!(
        out["reason"]
            .as_str()
            .is_some_and(|r| r.contains("head changed")),
        "{out}"
    );

    // The same request with the head where it was approved merges.
    *head.lock().unwrap() = "abcdef1".into();
    let id = ask(&h, "pr_merge", merge_args("op-recheck-2")).await;
    answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    let out = settled(&h, &id).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(out["result"]["merged"], true, "{out}");
}

/// A call that finishes is not also logged as abandoned.
#[tokio::test]
async fn a_finished_call_is_not_logged_as_abandoned() {
    state::isolate();
    let h = harness_with(enabled()).await;
    mcp(
        &h.operator,
        "work",
        tool_call("recall", json!({ "query": "x" })),
    )
    .await;
    let statuses: Vec<Value> = log(&h, "work")
        .await
        .into_iter()
        .filter(|e| e["kind"] == "tool_result")
        .map(|e| e["payload"]["status"].clone())
        .collect();
    assert_eq!(statuses, [json!("ok")]);
}

/// A channel-wide pause an older node left behind fences exactly like a Stop,
/// and Start lifts it.
#[tokio::test]
async fn a_legacy_channel_pause_fences_like_a_stop_until_start() {
    state::isolate();
    let h = harness_with(enabled()).await;
    h.store
        .channel_put("work", &[], r#"{"external_paused":true}"#)
        .unwrap();
    h.store.node_channel_add("n1", "work").unwrap();
    let (status, _) = mcp(&h.operator, "work", ping()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, view) = call(&h.operator, "GET", "/api/external", None).await;
    assert!(
        view["stopped"].as_array().unwrap().contains(&json!("work")),
        "{view}"
    );

    let (status, _) = call(&h.operator, "POST", "/api/external/work/start", None).await;
    assert!(status.is_success(), "{status}");
    assert!(h.manager.bindings("work")["external_paused"].is_null());
    let (status, _) = mcp(&h.operator, "work", ping()).await;
    assert_eq!(status, StatusCode::OK);
}
/// Stop refuses every call on the channel, whatever lane, until Start.
#[tokio::test]
async fn a_channel_stop_refuses_every_call_until_start() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (status, _) = call(&h.operator, "POST", "/api/external/work/stop", None).await;
    assert!(status.is_success(), "{status}");
    let (status, v) = mcp(&h.operator, "work", ping()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("tracon external start"), "{message}");
    let (_, view) = call(&h.operator, "GET", "/api/external", None).await;
    assert_eq!(view["stopped"], json!(["work"]), "{view}");
    // Another channel is untouched.
    let (status, _) = mcp(&h.operator, "personal", ping()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = call(&h.operator, "POST", "/api/external/work/start", None).await;
    assert!(status.is_success(), "{status}");
    assert!(h.manager.bindings("work")["external_stopped"].is_null());
    let (status, _) = mcp(&h.operator, "work", ping()).await;
    assert!(status.is_success(), "{status}");
    let (_, view) = call(&h.operator, "GET", "/api/external", None).await;
    assert_eq!(view["stopped"], json!([]), "{view}");

    // The older route still starts it.
    call(&h.operator, "POST", "/api/external/work/stop", None).await;
    let (status, _) = call(&h.operator, "DELETE", "/api/external/work/stop", None).await;
    assert!(status.is_success(), "{status}");
    let (status, _) = mcp(&h.operator, "work", ping()).await;
    assert!(status.is_success(), "{status}");
}
#[tokio::test]
async fn stopping_a_channel_this_node_does_not_hold_is_refused() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (status, _) = call(&h.operator, "POST", "/api/external/nope/stop", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn what_the_node_was_asked_to_do_lands_in_the_channels_log() {
    state::isolate();
    let h = harness_with(enabled()).await;
    mcp(
        &h.operator,
        "work",
        tool_call("recall", json!({ "query": "x" })),
    )
    .await;
    let events = log(&h, "work").await;
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
    assert_eq!(kinds, ["tool_call", "tool_result"], "{events:?}");
    assert!(log(&h, "personal").await.is_empty());
}
/// A harness that labels its calls is shown by its label: on each call it
/// makes, and as a lane in the external view.
#[tokio::test]
async fn a_labelled_harness_is_shown_by_its_lane() {
    state::isolate();
    let h = harness_with(enabled()).await;
    mcp_labelled(
        &h.operator,
        "work",
        "  tracon:feat/x  ",
        tool_call("recall", json!({ "query": "x" })),
    )
    .await;
    let events = log(&h, "work").await;
    assert!(
        events.iter().all(|e| e["lane"] == "tracon:feat/x"),
        "{events:?}"
    );
    let (_, external) = call(&h.operator, "GET", "/api/external", None).await;
    assert_eq!(external["lanes"][0]["lane"], "tracon:feat/x", "{external}");
    assert_eq!(external["lanes"][0]["channel"], "work", "{external}");
    assert_eq!(external["lanes"][0]["calls"], 1, "{external}");
}
/// The card a labelled harness raises says which lane asked for it.
#[tokio::test]
async fn an_approval_carries_the_lane_that_asked() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let req = Request::builder()
        .method("POST")
        .uri("/mcp/external/work")
        .header("host", "127.0.0.1:7420")
        .header("content-type", "application/json")
        .header("x-tracon-agent", "tracon:feat/x")
        .body(Body::from(
            tool_call("doc_write", json!({ "slug": "plan-x", "body": "hi" })).to_string(),
        ))
        .unwrap();
    let res = h.operator.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let (err, out) = outcome(&serde_json::from_slice(&bytes).unwrap());
    assert!(!err, "{out}");
    let id = out["approval_id"].as_str().unwrap();
    let row = h.store.get_approval(id).unwrap().unwrap();
    assert_eq!(row.lane.as_deref(), Some("tracon:feat/x"));
}

#[tokio::test]
async fn the_door_answers_one_post_per_message_and_opens_no_stream() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let req = Request::builder()
        .method("GET")
        .uri("/mcp/external/work")
        .header("host", "127.0.0.1:7420")
        .body(Body::empty())
        .unwrap();
    let res = h.operator.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
}

/// A lane whose harness process is still alive reads as running. The pid
/// comes in its own header, or, from an older helper, on the end of the label,
/// which is split off so the lane is the same either way.
#[tokio::test]
async fn a_lane_whose_process_is_alive_reads_as_running() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let pid = std::process::id().to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/mcp/external/work")
        .header("host", "127.0.0.1:7420")
        .header("content-type", "application/json")
        .header("x-tracon-agent", "repo:feat/x")
        .header("x-tracon-agent-pid", pid.as_str())
        .body(Body::from(
            tool_call("recall", json!({ "query": "x" })).to_string(),
        ))
        .unwrap();
    assert_eq!(
        h.operator.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    mcp_labelled(
        &h.operator,
        "work",
        &format!("repo:feat/y#{pid}"),
        tool_call("recall", json!({ "query": "y" })),
    )
    .await;
    mcp(
        &h.operator,
        "work",
        tool_call("recall", json!({ "query": "z" })),
    )
    .await;

    let (_, view) = call(&h.operator, "GET", "/api/external", None).await;
    let lanes = view["lanes"].as_array().unwrap();
    let by = |lane: Value| lanes.iter().find(|l| l["lane"] == lane).cloned().unwrap();
    assert_eq!(by(json!("repo:feat/x"))["running"], 1, "{view}");
    assert_eq!(by(json!("repo:feat/y"))["running"], 1, "{view}");
    assert_eq!(by(Value::Null)["running"], Value::Null, "{view}");
}

/// Lanes begin at a tool call: connecting and listing tools leave no trace.
#[tokio::test]
async fn the_external_view_lists_only_lanes_that_called_a_tool() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (_, before) = call(&h.operator, "GET", "/api/external", None).await;
    assert_eq!(before["enabled"], json!(true));
    assert!(before["lanes"].as_array().unwrap().is_empty());
    assert!(before["channels"]
        .as_array()
        .unwrap()
        .contains(&json!("work")));

    mcp(&h.operator, "work", ping()).await;
    list(&h.operator, "work").await;
    let (_, idle) = call(&h.operator, "GET", "/api/external", None).await;
    assert!(idle["lanes"].as_array().unwrap().is_empty(), "{idle}");

    mcp(
        &h.operator,
        "work",
        tool_call("recall", json!({ "query": "x" })),
    )
    .await;
    let (_, after) = call(&h.operator, "GET", "/api/external", None).await;
    let lanes = after["lanes"].as_array().unwrap();
    assert_eq!(lanes.len(), 1, "{after}");
    assert_eq!(lanes[0]["channel"], json!("work"));
    assert_eq!(lanes[0]["lane"], Value::Null, "an unlabelled caller");
}
/// Belt and braces on the surface the mode adds: an attachment is offered the
/// forge and tracker verbs only when the channel has the credential for them.
#[tokio::test]
async fn an_unbound_channel_is_offered_no_brokered_tools() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let names = list(&h.operator, "personal").await;
    for brokered in ["query", "issue", "issue_update", "mr_status"] {
        assert!(
            !names.contains(&brokered.to_string()),
            "{brokered} in {names:?}"
        );
    }
    // The node's own corpus needs no credential, so it is still there.
    assert!(names.contains(&"recall".to_string()), "{names:?}");
}

/// The regression this file exists for: an interface that is already open
/// learns about a card only from the stream. Publishing the session without
/// the queue leaves it showing a session that is waiting on you and nothing
/// to answer, which is exactly what it looked like in the wild.
#[tokio::test]
async fn a_card_reaches_an_interface_that_is_already_open() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let mut frames = h.bus.subscribe();

    let app = h.operator.clone();
    tokio::spawn(async move {
        mcp(
            &app,
            "work",
            tool_call("doc_write", json!({ "slug": "plan-x", "body": "hello" })),
        )
        .await
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let queued = loop {
        let frame = tokio::time::timeout_at(deadline, frames.recv())
            .await
            .expect("the queue frame never arrived")
            .expect("the bus closed");
        if let Frame::Queue { waiting } = frame {
            if !waiting.is_empty() {
                break waiting;
            }
        }
    };
    assert!(
        queued[0].title.starts_with("doc_write"),
        "{}",
        queued[0].title
    );
}

/// A client that still sends a session id from an older node is answered as
/// any other: the id names nothing here.
#[tokio::test]
async fn a_session_id_a_client_still_sends_is_ignored() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (status, _, echoed) = mcp_as(&h.operator, "work", Some("has space"), ping()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(echoed, None);
    assert_no_sessions(&h);

    let req = Request::builder()
        .method("DELETE")
        .uri("/mcp/external/work")
        .header("host", "127.0.0.1:7420")
        .header("mcp-session-id", "agent-b")
        .body(Body::empty())
        .unwrap();
    let res = h.operator.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
}
/// The probe a newer client sends before `initialize` is refused as an
/// unknown method, so the client falls back, and it attaches nothing: no
/// session row, and no id handed out.
#[tokio::test]
async fn a_discovery_probe_attaches_nothing() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (status, body, echoed) = mcp_as(
        &h.operator,
        "work",
        None,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "server/discover" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], -32601, "{body}");
    assert_eq!(echoed, None);
    assert_no_sessions(&h);
}

/// A harness with no session has no session scope to remember into; it says
/// which scope it has.
#[tokio::test]
async fn an_external_caller_cannot_retain_into_a_session_scope() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call(
            "retain",
            json!({ "kind": "fact", "body": "x", "scope": "session" }),
        ),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(err, "{out}");
    assert!(out.to_string().contains("global"), "{out}");
}
