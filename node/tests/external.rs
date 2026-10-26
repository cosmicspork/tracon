//! A harness the operator runs themselves, through the operator door: every
//! call stands alone, with no session, is offered the channel's tools minus a
//! review session's verdict, is logged to the channel's external log under
//! the lane it gives, and a call the policy does not name becomes an approval
//! on the same queue as any other.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness_with, Harness};
use support::http::{call, call_local};
use support::state;

use std::sync::atomic::{AtomicUsize, Ordering};
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
    // Publication is held to the tree hash, so it is there when the call
    // returns; the files a transfer carries follow once the review exists.
    let candidate_id = out["candidate_id"].as_str().unwrap();
    let candidate = h.store.candidate(candidate_id).unwrap().unwrap();
    assert!(candidate.tree_sha.is_some_and(|tree| tree.len() == 40));
    let mut materialized = false;
    for _ in 0..200 {
        let capture: Value = serde_json::from_str(
            &h.store
                .candidate(candidate_id)
                .unwrap()
                .unwrap()
                .capture_json,
        )
        .unwrap();
        if capture["materialized"] == json!(true) {
            materialized = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(materialized, "the candidate's files were never kept");
    let files = h.store.candidate_files(candidate_id).unwrap();
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0].content, b"base\none\ntwo\n");
}

/// The first work on a GitLab project nothing was ever pushed to, from a
/// worktree of a clone made while it was empty: there is no `origin/main` to
/// diff against, so the whole tree is the change, and with the base as its
/// branch it is the repository's first commit (#629).
#[tokio::test]
async fn the_first_commit_of_an_empty_repository_is_put_up_for_review_whole() {
    state::isolate();
    let dir = state::scratch("external-empty-origin");
    std::fs::create_dir_all(dir.join("root")).unwrap();
    sh(&dir, "git init -q --bare -b main origin.git");
    sh(
        &dir,
        "git clone -q origin.git root/repo 2>/dev/null && cd root/repo \
         && git config user.email t@e && git config user.name t \
         && git symbolic-ref HEAD refs/heads/feat/core-sync \
         && printf 'one\\ntwo\\n' > a.txt && git add -A && git commit -qm 'feat: core sync' \
         && git checkout -q --detach && git worktree add ../../wt feat/core-sync 2>/dev/null",
    );
    let wt = dir.join("wt").to_string_lossy().into_owned();
    let h = harness_with(with_roots(&dir.join("root"))).await;
    let mut args = submit_args(&wt);
    args["provider"] = json!("gitlab");
    args["project"] = json!("group/name");
    args["branch"] = json!("main");
    let (_, v) = mcp(&h.operator, "work", tool_call("submit_review", args)).await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert_eq!(
        (out["files"].clone(), out["added"].clone()),
        (json!(1), json!(2))
    );
    let r = h
        .store
        .get_review(out["review_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(r.diff.contains("+one"), "{}", r.diff);
    let target: Value = serde_json::from_str(&r.target).unwrap();
    assert_eq!(target["initial"], json!(true), "{target}");
    assert_eq!(target["branch"], "main");
}

#[tokio::test]
async fn a_retried_submission_answers_with_the_review_it_already_made() {
    state::isolate();
    let (root, wt) = repo_with_worktree("retry");
    let h = harness_with(with_roots(&root)).await;
    let submit = || {
        mcp(
            &h.operator,
            "work",
            tool_call("submit_review", submit_args(&wt)),
        )
    };
    // A retry while the first is still running waits for it rather than
    // racing it to a second row.
    let ((_, first), (_, second)) = tokio::join!(submit(), submit());
    let (_, first) = outcome(&first);
    let (_, second) = outcome(&second);
    let id = first["review_id"].as_str().unwrap().to_string();
    assert_eq!(second["review_id"], json!(id), "{second}");
    let (_, third) = outcome(&submit().await.1);
    assert_eq!(third["review_id"], json!(id), "{third}");
    assert_eq!(third["already_submitted"], json!(true), "{third}");
    assert_eq!(h.store.open_reviews().unwrap().len(), 1);

    // Different words for the same commit revise that review.
    let mut args = submit_args(&wt);
    args["body"] = json!("a better why");
    let (err, revised) = outcome(
        &mcp(&h.operator, "work", tool_call("submit_review", args))
            .await
            .1,
    );
    assert!(!err, "{revised}");
    assert_eq!(revised["review_id"], json!(id), "{revised}");
    assert_eq!(h.store.open_reviews().unwrap().len(), 1);
    assert_eq!(
        h.store.get_review(&id).unwrap().unwrap().body,
        "a better why"
    );
}

#[tokio::test]
async fn a_forge_that_does_not_answer_does_not_hold_up_a_submission() {
    state::isolate();
    let (root, wt) = repo_with_worktree("slow-forge");
    let bin = state::scratch("external-slow-forge-bin");
    let gh = bin.join("gh");
    let called = bin.join("called");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\n[ \"$1\" = probe ] && exit 0\necho \"$*\" >> {}\nexec sleep 30\n",
            called.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // A child another test forks while the script is being written holds it
    // open for writing until that child execs, and running it meanwhile
    // fails with "text file busy". Wait until it runs.
    for _ in 0..100 {
        if std::process::Command::new(&gh)
            .arg("probe")
            .status()
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut cfg = with_roots(&root);
    cfg.publish.gh = gh.to_string_lossy().into_owned();
    cfg.publish.forge_timeout_secs = 1;
    let h = harness_with(cfg).await;
    h.tools.broker.write().unwrap().put(
        "gh",
        tracon::broker::Credential {
            env: [("GH_TOKEN".to_string(), "t".to_string())]
                .into_iter()
                .collect(),
            channels: vec!["work".into()],
            ..Default::default()
        },
    );
    let started = std::time::Instant::now();
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("submit_review", submit_args(&wt)),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert!(out["review_id"].is_string(), "{out}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    let asked = std::fs::read_to_string(&called).unwrap_or_default();
    assert!(asked.contains("pulls?state=open"), "{asked}");
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
/// An item an external harness records against its worktree joins the
/// project a session launched on that repository is bound to, so it is in
/// that project's ready work; a detached HEAD does not matter, since only the
/// repository does. The item is marked with the harness's lane.
#[tokio::test]
async fn an_external_item_named_by_its_worktree_joins_the_project_a_session_there_gets() {
    state::isolate();
    let (root, wt) = repo_with_worktree("discover");
    sh(std::path::Path::new(&wt), "git checkout -q --detach");
    let mut cfg = with_roots(&root);
    cfg.session.worktree_root = root.parent().unwrap().join("worktrees");
    let h = harness_with(cfg).await;
    h.store
        .put_node(&support::rows::node_row("n1", "external"))
        .unwrap();
    // The node's own clone of that repository: sessions start from nothing
    // else. Neither has a forge remote here, so each is named by its
    // directory, `repo`, as `identify` falls back to.
    let clone = state::managed_clone("external-discover");
    let session = h
        .manager
        .create_with(
            tracon::session::NewSession {
                channel: "work".into(),
                repo_path: clone.to_string_lossy().into_owned(),
                branch: None,
                work_item_id: None,
                model: "m/a".into(),
                budget_tokens: Some(1000),
                initial_prompt: Some("look around".into()),
                node_id: None,
                phase: tracon::session::Phase::Execute,
                review_id: None,
                base_sha: None,
                workspace_id: None,
                parent_session: None,
                continued_from: None,
                harness: None,
                on_exhaustion: None,
                told_reviews: Vec::new(),
            },
            Arc::new(support::fake::FakeAdapter {
                tx: Arc::new(tokio::sync::Mutex::new(None)),
                tokens: Arc::new(tokio::sync::Mutex::new(0)),
            }),
        )
        .await
        .unwrap();
    let project = session
        .project_id
        .clone()
        .expect("a launched session has a project");

    let v = mcp_labelled(
        &h.operator,
        "work",
        "repo:planning",
        tool_call(
            "work_discover",
            json!({ "title": "port the tracker", "worktree": wt }),
        ),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert_eq!(out["project_id"], json!(project), "{out}");
    let item = h
        .store
        .work_get(out["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(item.project_id.as_deref(), Some(project.as_str()));
    assert_eq!(item.discovered_from, None);
    assert_eq!(item.discovered_by_session, None);
    assert_eq!(item.discovered_by_lane.as_deref(), Some("repo:planning"));
    assert!(h.store.project_get(&project).unwrap().is_some());
    let ready = h.store.work_ready("work", Some(&project)).unwrap();
    assert!(ready.iter().any(|w| w.item.id == item.id), "{ready:?}");
    tracon::session::materialize::remove(&session.id);
}

#[tokio::test]
async fn an_external_item_named_by_a_worktree_outside_the_roots_is_refused() {
    state::isolate();
    let (root, wt) = repo_with_worktree("discover-outside");
    let elsewhere = root.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let h = harness_with(with_roots(&elsewhere)).await;
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call(
            "work_discover",
            json!({ "title": "port the tracker", "worktree": wt }),
        ),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(err);
    assert!(out.to_string().contains("repo_roots"), "{out}");
    assert!(h.store.work_ready("work", None).unwrap().is_empty());
}

/// A fact an external harness retains about a repository is that project's,
/// named by a worktree as an item is, and is recalled through the same
/// worktree, not by every caller on the channel.
#[tokio::test]
async fn an_external_fact_named_by_its_worktree_is_its_projects() {
    state::isolate();
    let (root, wt) = repo_with_worktree("retain");
    let h = harness_with(with_roots(&root)).await;
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call(
            "work_discover",
            json!({ "title": "find the project", "worktree": wt }),
        ),
    )
    .await;
    let project = outcome(&v).1["project_id"].clone();
    assert!(project.is_string(), "{v}");

    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call(
            "retain",
            json!({ "kind": "fact", "body": "checks run with just check", "confidence": 0.9, "worktree": wt }),
        ),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    let m = h
        .store
        .memory_get(out["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(m.scope, "project");
    assert_eq!(m.scope_ref.as_deref(), project.as_str());

    let recalled = |v: &Value| {
        outcome(v)
            .1
            .to_string()
            .contains("checks run with just check")
    };
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("recall", json!({ "query": "checks", "worktree": wt })),
    )
    .await;
    assert!(recalled(&v), "{v}");
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("recall", json!({ "query": "checks" })),
    )
    .await;
    assert!(!recalled(&v), "{v}");
}

/// Without a worktree an external item belongs to the whole channel, and an
/// unlabelled harness is still marked as one, not taken for the operator.
#[tokio::test]
async fn an_external_item_without_a_worktree_has_no_project() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("work_discover", json!({ "title": "port the tracker" })),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert_eq!(out["project_id"], Value::Null, "{out}");
    let item = h
        .store
        .work_get(out["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(item.project_id, None);
    assert_eq!(item.discovered_by_lane.as_deref(), Some("external"));
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
    assert_eq!(v["approval"]["title"], "doc_write plan-foo · new");
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

/// Stand a Markdown document at `slug` on the harness channel, as a sync
/// write would, and return its hash.
fn seed_document(h: &Harness, slug: &str, body: &str) -> String {
    let hash = tracon::corpus::hash_body(body);
    h.store
        .write_document_change(
            "n1",
            "work",
            slug,
            "plan",
            &tracon::mcp::docs::title_of(&[], slug, body),
            body,
            &hash,
            None,
            false,
            &tracon::corpus::new_id(),
            None,
            None,
        )
        .unwrap();
    hash
}

/// A write over a document that stands is titled as an edit of it, and its
/// details carry the document as it is now, for the page to diff against.
/// An `if_hash` that no longer matches is called stale before anything runs.
#[tokio::test]
async fn a_document_edit_card_names_the_document_and_details_carry_its_current_body() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let current = "# Plan: foo\n\nOne.\nTwo.\n";
    let hash = seed_document(&h, "plan-foo", current);
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-foo", "body": "# Plan: foo\n\nOne.\nThree.\n", "if_hash": hash }),
    )
    .await;
    let (_, v) = call(&h.operator, "GET", &format!("/api/approvals/{id}"), None).await;
    assert_eq!(
        v["approval"]["title"], "doc_write plan-foo · edit · \"Plan: foo\"",
        "{v}"
    );
    assert_eq!(v["document"]["body"], current, "{v}");
    assert_eq!(v["document"]["hash"], hash, "{v}");
    assert_eq!(v["document"]["format"], "markdown", "{v}");
    assert_eq!(v["document"]["title"], "Plan: foo", "{v}");
    assert!(v["document"]["updated_ms"].as_i64().unwrap() > 0, "{v}");
    assert_eq!(v["stale"], false, "{v}");

    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-foo", "body": "# Plan: foo\n", "if_hash": "not-the-hash" }),
    )
    .await;
    let (_, v) = call(&h.operator, "GET", &format!("/api/approvals/{id}"), None).await;
    assert_eq!(v["stale"], true, "{v}");
}

/// A write to a slug nothing stands at is new: its details carry no document
/// and nothing to be stale against.
#[tokio::test]
async fn a_new_document_card_says_new_and_details_carry_no_document() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-new", "body": "# New plan" }),
    )
    .await;
    let (_, v) = call(&h.operator, "GET", &format!("/api/approvals/{id}"), None).await;
    assert_eq!(v["approval"]["title"], "doc_write plan-new · new", "{v}");
    assert!(v.as_object().unwrap().contains_key("document"), "{v}");
    assert_eq!(v["document"], Value::Null, "{v}");
    assert_eq!(v["stale"], false, "{v}");

    // Once decided, there is nothing to compare: the write has made the
    // document, and calling the call stale against itself would mislead.
    let (status, _) = answer(&h, &id, json!({ "option_id": "reject_once" })).await;
    assert_eq!(status, StatusCode::OK);
    let (_, v) = call(&h.operator, "GET", &format!("/api/approvals/{id}"), None).await;
    assert!(
        v.get("document").is_none() && v.get("stale").is_none(),
        "{v}"
    );
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

/// Write a memory on `channel` as the operator does, and return its id.
async fn seed_memory(h: &Harness, channel: &str, body: Value) -> String {
    let mut body = body;
    body["channel"] = json!(channel);
    let (status, v) = call(&h.operator, "POST", "/api/memories", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    v["id"].as_str().unwrap().to_string()
}

/// A memory's wording is fixed in place only once the operator allows it,
/// even where a rule would let the call run unattended. The card names the
/// memory, its details carry the memory as it stands for the page to diff
/// against, and allowing it (with the operator's own wording) changes the
/// body alone, as a change the channel replicates.
#[tokio::test]
async fn a_memory_amendment_is_always_asked_and_allowing_it_rewrites_only_the_body() {
    state::isolate();
    let h = harness_with(enabled()).await;
    for rule in h.tools.policy.write().rules.iter_mut() {
        if rule.matches.iter().any(|m| m == "retain") {
            rule.matches.push("memory_amend".into());
        }
    }
    let current = "The importer retries twice.\nIt logs each retry.";
    let id = seed_memory(
        &h,
        "work",
        json!({ "kind": "fact", "scope": "global", "body": current }),
    )
    .await;
    let args = json!({
        "id": id,
        "body": "The importer retries three times.\nIt logs each retry.",
        "reason": "The retry count changed in 0.38.",
    });
    let approval_id = ask(&h, "memory_amend", args.clone()).await;
    assert_eq!(h.store.memory_get(&id).unwrap().unwrap().body, current);

    let (_, v) = call(
        &h.operator,
        "GET",
        &format!("/api/approvals/{approval_id}"),
        None,
    )
    .await;
    assert_eq!(
        v["approval"]["title"],
        format!(
            "memory_amend {}: The retry count changed in 0.38.",
            &id[..8]
        ),
        "{v}"
    );
    assert_eq!(v["prose_fields"], json!(["body"]), "{v}");
    assert!(v["locked_fields"]
        .as_array()
        .unwrap()
        .contains(&json!("id")));
    assert_eq!(v["memory"]["id"], id.as_str(), "{v}");
    assert_eq!(v["memory"]["body"], current, "{v}");
    assert_eq!(v["memory"]["kind"], "fact", "{v}");
    assert_eq!(v["memory"]["scope"], "global", "{v}");
    assert_eq!(v["memory"]["state"], "active", "{v}");

    // The target is the call: pointing it at another memory is refused.
    let mut moved = args.clone();
    moved["id"] = json!("another-memory");
    let (status, v) = answer(
        &h,
        &approval_id,
        json!({ "option_id": "allow_once", "arguments": moved }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["fields"][0]["field"], "id", "{v}");

    let mut edit = args.clone();
    edit["body"] = json!("The importer retries three times.\nIt logs each retry at warn.");
    let (status, _) = answer(
        &h,
        &approval_id,
        json!({ "option_id": "allow_once", "arguments": edit }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let out = settled(&h, &approval_id).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    let m = h.store.memory_get(&id).unwrap().unwrap();
    assert_eq!(
        m.body,
        "The importer retries three times.\nIt logs each retry at warn."
    );
    assert_eq!(
        (m.kind.as_str(), m.scope.as_str(), m.state.as_str()),
        ("fact", "global", "active")
    );
    let changes = h
        .store
        .changes_of_site_after("n1", "work", 0, 1000)
        .unwrap();
    let last = changes
        .iter()
        .rfind(|c| c.table == "memory" && c.id == id)
        .unwrap();
    assert_eq!(last.row["body"], m.body.as_str(), "the edit replicates");

    // Decided, there is nothing left to compare against.
    let (_, v) = call(
        &h.operator,
        "GET",
        &format!("/api/approvals/{approval_id}"),
        None,
    )
    .await;
    assert!(v.get("memory").is_none(), "{v}");
}

/// What an amendment could not do is refused before anything is held: a
/// directive, another channel's memory, one that does not exist or is not
/// live, a body that changes nothing, and a call with no reason.
#[tokio::test]
async fn a_memory_amendment_that_cannot_run_is_refused_before_it_is_held() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let fact = seed_memory(&h, "work", json!({ "kind": "fact", "body": "a fact" })).await;
    let directive = seed_memory(&h, "work", json!({ "body": "never push to main" })).await;
    let elsewhere = seed_memory(
        &h,
        "personal",
        json!({ "kind": "fact", "body": "a personal fact" }),
    )
    .await;
    let waiting = seed_memory(
        &h,
        "work",
        json!({ "kind": "lesson", "body": "a lesson still waiting", "state": "candidate" }),
    )
    .await;
    for (args, says) in [
        (
            json!({ "id": directive, "body": "push to main", "reason": "r" }),
            "directives",
        ),
        (
            json!({ "id": elsewhere, "body": "changed", "reason": "r" }),
            "no live memory",
        ),
        (
            json!({ "id": "no-such-memory", "body": "changed", "reason": "r" }),
            "no live memory",
        ),
        (
            json!({ "id": waiting, "body": "changed", "reason": "r" }),
            "no live memory",
        ),
        (
            json!({ "id": fact, "body": " a fact ", "reason": "r" }),
            "nothing to change",
        ),
        (
            json!({ "id": fact, "body": "changed", "reason": " " }),
            "reason",
        ),
    ] {
        let (_, v) = mcp(&h.operator, "work", tool_call("memory_amend", args.clone())).await;
        let (err, out) = outcome(&v);
        assert!(err, "{args}: {out}");
        assert!(out.to_string().contains(says), "{args}: {out}");
    }
    assert!(h.store.open_approval_cards("[]").unwrap().is_empty());
}

/// A memory deleted while its amendment waits is shown as gone, and allowing
/// the amendment then fails rather than bringing it back.
#[tokio::test]
async fn an_amendment_of_a_memory_deleted_while_it_waits_fails_when_allowed() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let id = seed_memory(&h, "work", json!({ "kind": "fact", "body": "a fact" })).await;
    let approval_id = ask(
        &h,
        "memory_amend",
        json!({ "id": id, "body": "a better fact", "reason": "wording" }),
    )
    .await;
    let (status, _) = call(&h.operator, "DELETE", &format!("/api/memories/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, v) = call(
        &h.operator,
        "GET",
        &format!("/api/approvals/{approval_id}"),
        None,
    )
    .await;
    assert!(v.as_object().unwrap().contains_key("memory"), "{v}");
    assert_eq!(v["memory"], Value::Null, "{v}");
    let (status, _) = answer(&h, &approval_id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    let out = settled(&h, &approval_id).await;
    assert_eq!(out["state"], "failed", "{out}");
    assert!(h
        .store
        .memory_get(&id)
        .unwrap()
        .is_none_or(|m| m.deleted == 1));
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
    github_at(h, addr);
    head
}

/// A GitHub-shaped forge whose merge answers that the base branch moved the
/// first `moves` times it is asked, as it does when another merge lands
/// during this one. Counts the merges asked.
async fn racing_github(moves: usize) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let merges = asked.clone();
    let app = axum::Router::new()
        .route(
            "/repos/o/n/pulls/1",
            axum::routing::get(|| async {
                axum::Json(json!({ "number": 1, "head": { "sha": "abcdef1" } }))
            }),
        )
        .route(
            "/repos/o/n/pulls/1/merge",
            axum::routing::put(move || {
                let n = merges.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < moves {
                        (
                            StatusCode::METHOD_NOT_ALLOWED,
                            axum::Json(json!({
                                "message": "Base branch was modified. Review and try the merge again."
                            })),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            axum::Json(json!({ "merged": true, "sha": "merged1", "message": "ok" })),
                        )
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, asked)
}

fn github_at(h: &Harness, addr: std::net::SocketAddr) {
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

/// GitHub refuses a merge whose base moved while it ran, as it does when
/// several approved merges into one repository land together. Nothing was
/// merged, so the approved merge is asked again at the same head and lands.
#[tokio::test]
async fn an_approved_merge_retries_when_the_base_moved_under_it() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (addr, asked) = racing_github(1).await;
    github_at(&h, addr);
    let id = ask(&h, "pr_merge", merge_args("op-race-1")).await;
    answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    let out = settled(&h, &id).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(out["result"]["merged"], true, "{out}");
    assert_eq!(asked.load(Ordering::SeqCst), 2);
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

    let (status, _) = call_local(&h.operator, "POST", "/api/external/work/start", None).await;
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

    let (status, _) = call_local(&h.operator, "POST", "/api/external/work/start", None).await;
    assert!(status.is_success(), "{status}");
    assert!(h.manager.bindings("work")["external_stopped"].is_null());
    let (status, _) = mcp(&h.operator, "work", ping()).await;
    assert!(status.is_success(), "{status}");
    let (_, view) = call(&h.operator, "GET", "/api/external", None).await;
    assert_eq!(view["stopped"], json!([]), "{view}");

    // The older route still starts it.
    call(&h.operator, "POST", "/api/external/work/stop", None).await;
    let (status, _) = call_local(&h.operator, "DELETE", "/api/external/work/stop", None).await;
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

/// A Jira that offers WRK-1 one transition, and records every request.
async fn fake_jira(seen: Arc<std::sync::Mutex<Vec<String>>>) -> std::net::SocketAddr {
    let app = axum::Router::new().route(
        "/rest/api/2/issue/WRK-1/transitions",
        axum::routing::any(move |method: axum::http::Method| {
            seen.lock().unwrap().push(method.to_string());
            async {
                axum::Json(json!({ "transitions": [
                    { "id": "21", "name": "Start review",
                      "to": { "name": "In Review", "statusCategory": { "name": "In Progress" } } },
                ] }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A held Jira transition reads as the status it moves the issue to, which
/// Jira is asked for when the call is held; nothing is transitioned by asking.
/// An id Jira does not offer is titled by the id.
#[tokio::test]
async fn a_held_transition_is_titled_by_the_status_it_leads_to() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let addr = fake_jira(seen.clone()).await;
    h.tools.broker.write().unwrap().put(
        "jira",
        tracon::broker::Credential {
            env: [
                ("JIRA_URL".to_string(), format!("http://{addr}")),
                ("JIRA_EMAIL".to_string(), "me@example.com".to_string()),
                ("JIRA_TOKEN".to_string(), "t".to_string()),
            ]
            .into_iter()
            .collect(),
            channels: vec!["work".into()],
            ..Default::default()
        },
    );
    let args = |id: &str| json!({ "key": "WRK-1", "transition_id": id, "operation_id": format!("transition-{id}") });

    let id = ask(&h, "issue_transition", args("21")).await;
    let row = h.store.get_approval(&id).unwrap().unwrap();
    assert_eq!(row.title, "issue_transition WRK-1: move to In Review");

    let id = ask(&h, "issue_transition", args("99")).await;
    let row = h.store.get_approval(&id).unwrap().unwrap();
    assert_eq!(row.title, "issue_transition WRK-1: transition 99");

    assert_eq!(*seen.lock().unwrap(), ["GET", "GET"]);
}

/// Seed an item on `channel` through the operator's API and return its id.
async fn seed(h: &Harness, channel: &str, body: Value) -> String {
    let mut body = body;
    body["channel"] = json!(channel);
    let (status, v) = call(&h.operator, "POST", "/api/work", Some(body)).await;
    assert!(status.is_success(), "{status} {v}");
    v["id"].as_str().unwrap().to_string()
}

/// Ask for a ledger write, allow it, and return what it settled as.
async fn allowed(h: &Harness, name: &str, args: Value) -> Value {
    let id = ask(h, name, args).await;
    let (status, _) = answer(h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    settled(h, &id).await
}

/// The ledger reads run unattended and stay on the caller's channel; a
/// rewrite and a delete are each put to the operator first, and priorities
/// are read and written by the names the node gives them.
#[tokio::test]
async fn an_agent_reads_rewrites_and_deletes_items_on_its_own_channel() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let plain = seed(
        &h,
        "work",
        json!({ "title": "tidy the docs", "body": "the readme" }),
    )
    .await;
    let urgent = seed(
        &h,
        "work",
        json!({ "title": "fix the outage", "body": "lists past 100 vanish", "priority": 4 }),
    )
    .await;
    let elsewhere = seed(&h, "other", json!({ "title": "not yours" })).await;
    // Nobody named a priority, so the node's default did.
    assert_eq!(h.store.work_get(&plain).unwrap().unwrap().priority, 2);

    let (_, v) = mcp(&h.operator, "work", tool_call("work_list", json!({}))).await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert_eq!(out["total"], 2, "{out}");
    assert_eq!(
        out["items"][0]["id"],
        json!(urgent),
        "higher runs first: {out}"
    );
    assert_eq!(out["items"][0]["priority_name"], "Urgent");
    assert_eq!(out["items"][1]["priority_name"], "Planned");
    assert!(out["items"][0].get("body").is_none(), "{out}");
    assert_eq!(out["priority_levels"][0]["name"], "Urgent");

    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("work_list", json!({ "query": "README" })),
    )
    .await;
    let (_, out) = outcome(&v);
    assert_eq!(out["total"], 1, "{out}");
    assert_eq!(out["items"][0]["id"], json!(plain));

    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("work_show", json!({ "id": &urgent[..8] })),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    assert_eq!(out["id"], json!(urgent));
    assert_eq!(out["body"], "lists past 100 vanish");
    assert_eq!(out["state"], "ready");

    // Another channel's item is not this caller's to read or change.
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("work_show", json!({ "id": elsewhere })),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(err, "{out}");
    let out = allowed(
        &h,
        "work_update",
        json!({ "id": elsewhere, "title": "mine now" }),
    )
    .await;
    assert_eq!(out["state"], "failed", "{out}");
    assert_eq!(
        h.store.work_get(&elsewhere).unwrap().unwrap().title,
        "not yours"
    );

    // A rewrite waits for the operator, then lands by name.
    let id = ask(
        &h,
        "work_update",
        json!({ "id": &plain[..8], "priority": "backlog", "body": "the readme and the guide", "deps": [&urgent[..8]] }),
    )
    .await;
    let card = h
        .store
        .open_permission_views()
        .unwrap()
        .into_iter()
        .find(|p| p.id == id)
        .expect("a card for it");
    assert!(
        card.title
            .starts_with(&format!("work_update {}", &plain[..8])),
        "{}",
        card.title
    );
    let item = h.store.work_get(&plain).unwrap().unwrap();
    assert_eq!(item.priority, 2, "nothing ran before the operator answered");
    let (status, _) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settled(&h, &id).await["state"], "succeeded");
    let item = h.store.work_get(&plain).unwrap().unwrap();
    assert_eq!(item.priority, 1);
    assert_eq!(item.body, "the readme and the guide");
    assert_eq!(item.deps, vec![urgent.clone()]);

    // An item cannot wait on itself, or on one that is not there.
    let out = allowed(&h, "work_update", json!({ "id": plain, "deps": [plain] })).await;
    assert_eq!(out["state"], "failed", "{out}");
    let out = allowed(
        &h,
        "work_update",
        json!({ "id": plain, "deps": ["ffffffffff"] }),
    )
    .await;
    assert_eq!(out["state"], "failed", "{out}");

    // Closed, then reopened.
    let out = allowed(
        &h,
        "work_update",
        json!({ "id": urgent, "state": "closed" }),
    )
    .await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(h.store.work_get(&urgent).unwrap().unwrap().state, "closed");
    let out = allowed(&h, "work_update", json!({ "id": urgent, "state": "open" })).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(h.store.work_get(&urgent).unwrap().unwrap().state, "open");

    let out = allowed(&h, "work_delete", json!({ "id": &plain[..8] })).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert!(h.store.work_get(&plain).unwrap().is_none());
}

/// Items are grouped by project: an item named by a worktree joins that
/// repository's project, `/api/projects` counts what each project holds, and
/// an agent narrows the ledger or moves an item by the project's name.
#[tokio::test]
async fn items_are_grouped_and_moved_by_project_name() {
    state::isolate();
    let (root, wt) = repo_with_worktree("projects");
    let h = harness_with(with_roots(&root)).await;
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call(
            "work_discover",
            json!({ "title": "in the repo", "worktree": wt }),
        ),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err, "{out}");
    let in_repo = out["id"].as_str().unwrap().to_string();
    let project = out["project_id"].as_str().unwrap().to_string();
    let loose = seed(&h, "work", json!({ "title": "loose" })).await;

    let (_, v) = call(&h.operator, "GET", "/api/projects?channel=work", None).await;
    assert_eq!(v["projects"][0]["id"], json!(project), "{v}");
    assert_eq!(v["projects"][0]["name"], "repo", "{v}");
    assert_eq!(v["projects"][0]["open"], 1, "{v}");
    assert_eq!(v["none"]["open"], 1, "{v}");

    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("work_list", json!({ "project": "repo" })),
    )
    .await;
    let (_, out) = outcome(&v);
    assert_eq!(out["total"], 1, "{out}");
    assert_eq!(out["items"][0]["id"], json!(in_repo));
    assert_eq!(out["items"][0]["project"]["name"], "repo");
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("work_list", json!({ "worktree": wt })),
    )
    .await;
    let (_, out) = outcome(&v);
    assert_eq!(out["items"][0]["id"], json!(in_repo), "{out}");
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("work_list", json!({ "project": "none" })),
    )
    .await;
    let (_, out) = outcome(&v);
    assert_eq!(out["total"], 1, "{out}");
    assert_eq!(out["items"][0]["id"], json!(loose));

    let out = allowed(&h, "work_update", json!({ "id": loose, "project": "repo" })).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(
        h.store
            .work_get(&loose)
            .unwrap()
            .unwrap()
            .project_id
            .as_deref(),
        Some(project.as_str())
    );
    let out = allowed(&h, "work_update", json!({ "id": loose, "project": "none" })).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(h.store.work_get(&loose).unwrap().unwrap().project_id, None);

    // The operator's search narrows the same way.
    let (_, v) = call(&h.operator, "GET", "/api/work?channel=work&q=LOOSE", None).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 1, "{v}");
    let (_, v) = call(
        &h.operator,
        "GET",
        "/api/work?channel=work&project_id=none",
        None,
    )
    .await;
    assert_eq!(v["items"].as_array().unwrap().len(), 1, "{v}");
}

/// A session's item takes the session's project when it has none, and while
/// a session works on it an outside caller can neither close nor delete it.
#[tokio::test]
async fn an_item_in_use_takes_its_sessions_project_and_is_not_closed_or_deleted_from_outside() {
    state::isolate();
    let mut cfg = enabled();
    let clone = state::managed_clone("external-held");
    cfg.session.worktree_root = clone.parent().unwrap().join("worktrees");
    let h = harness_with(cfg).await;
    h.store
        .put_node(&support::rows::node_row("n1", "external"))
        .unwrap();
    let item = seed(&h, "work", json!({ "title": "held" })).await;
    let session = h
        .manager
        .create_with(
            tracon::session::NewSession {
                channel: "work".into(),
                repo_path: clone.to_string_lossy().into_owned(),
                branch: None,
                work_item_id: Some(item.clone()),
                model: "m/a".into(),
                budget_tokens: Some(1000),
                initial_prompt: None,
                node_id: None,
                phase: tracon::session::Phase::Plan,
                review_id: None,
                base_sha: None,
                workspace_id: None,
                parent_session: None,
                continued_from: None,
                harness: None,
                on_exhaustion: None,
                told_reviews: Vec::new(),
            },
            Arc::new(support::fake::FakeAdapter {
                tx: Arc::new(tokio::sync::Mutex::new(None)),
                tokens: Arc::new(tokio::sync::Mutex::new(0)),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        h.store.work_get(&item).unwrap().unwrap().project_id,
        session.project_id,
        "the item takes the project its session runs in"
    );

    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("work_close", json!({ "id": item })),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(err && out.to_string().contains("working on"), "{out}");
    let out = allowed(&h, "work_update", json!({ "id": item, "state": "closed" })).await;
    assert_eq!(out["state"], "failed", "{out}");
    let out = allowed(&h, "work_delete", json!({ "id": item })).await;
    assert_eq!(out["state"], "failed", "{out}");
    // Rewording an item in use is fine: the session keeps it.
    let out = allowed(
        &h,
        "work_update",
        json!({ "id": item, "title": "held, renamed" }),
    )
    .await;
    assert_eq!(out["state"], "succeeded", "{out}");
    let now = h.store.work_get(&item).unwrap().unwrap();
    assert_eq!(now.state, "open");
    assert_eq!(now.title, "held, renamed");
    tracon::session::materialize::remove(&session.id);
}

/// A two-file HTML plan on `work`, archived, as Docs → Import HTML and the
/// archive toggle leave one.
fn archived_html_plan(h: &Harness, slug: &str) -> tracon::store::DocumentRow {
    use tracon::corpus::html::HtmlFile;
    let (row, _) = h
        .store
        .write_html_document_change(
            &[],
            "n1",
            "work",
            slug,
            "plan-folder",
            "index.html",
            vec![
                HtmlFile {
                    path: "index.html".into(),
                    bytes: b"<title>October</title><link rel=stylesheet href=style.css>".to_vec(),
                },
                HtmlFile {
                    path: "style.css".into(),
                    bytes: b"body { color: red }".to_vec(),
                },
            ],
            None,
            true,
            Some(true),
            None,
        )
        .unwrap();
    row
}

/// Bundle rows still live for one generation of a document.
fn live_bundle_rows(h: &Harness, hash: &str) -> i64 {
    h.store
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM document_bundle_file
                     WHERE document_hash = ?1 AND deleted = 0) +
                    (SELECT COUNT(*) FROM document_bundle_chunk
                     WHERE document_hash = ?1 AND deleted = 0)",
            [hash],
            |row| row.get(0),
        )
        .unwrap()
}

/// The case that was refused after approval: Markdown, with the hash
/// `doc_read` gave, over an archived HTML plan. The card says what goes;
/// allowing it converts the document in place and retires its bundle.
#[tokio::test]
async fn markdown_written_over_an_archived_html_plan_converts_it_and_keeps_it_archived() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let html = archived_html_plan(&h, "plan-hdr-october-2026");
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("doc_read", json!({ "slug": "plan-hdr-october-2026" })),
    )
    .await;
    let (_, read) = outcome(&v);
    assert_eq!(read["format"], "html", "{read}");
    assert_eq!(read["hash"], html.hash.as_str(), "{read}");

    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-hdr-october-2026", "body": "# Markdown", "if_hash": read["hash"] }),
    )
    .await;
    let row = h.store.get_approval(&id).unwrap().unwrap();
    assert_eq!(
        row.title,
        "doc_write plan-hdr-october-2026 · edit · \"October\" (converts HTML to Markdown; drops 2 files of its HTML bundle)"
    );
    let (status, _) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    let out = settled(&h, &id).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(out["result"]["format"], "markdown", "{out}");

    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("doc_read", json!({ "slug": "plan-hdr-october-2026" })),
    )
    .await;
    let (_, read) = outcome(&v);
    assert_eq!(read["format"], "markdown", "{read}");
    assert_eq!(read["archived"], true, "{read}");
    assert_eq!(read["body"], "# Markdown", "{read}");
    assert_eq!(read["entry_path"], Value::Null, "{read}");
    assert_eq!(live_bundle_rows(&h, &html.hash), 0);
    assert_eq!(
        h.store
            .doc_get("work", "plan-hdr-october-2026")
            .unwrap()
            .unwrap()
            .id,
        html.id
    );
}

/// An agent's HTML is stored as the operator's import is, and the card shows
/// it as source with its format locked.
#[tokio::test]
async fn an_html_doc_write_is_a_single_file_bundle_shown_on_its_card_as_source() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let page = "<!doctype html><title>Report</title><script>document.title = 'x'</script>";
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "ref-report", "body": page, "format": "html" }),
    )
    .await;
    let (_, v) = call(&h.operator, "GET", &format!("/api/approvals/{id}"), None).await;
    assert_eq!(
        v["approval"]["title"],
        "doc_write ref-report · new (as HTML)"
    );
    assert_eq!(v["format"], "source", "{v}");
    assert!(
        v["locked_fields"]
            .as_array()
            .unwrap()
            .contains(&json!("format")),
        "{v}"
    );
    let (status, _) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settled(&h, &id).await["state"], "succeeded");

    let doc = h.store.doc_get("work", "ref-report").unwrap().unwrap();
    assert_eq!(doc.format, "html");
    assert_eq!(doc.title, "Report");
    assert_eq!(doc.entry_path.as_deref(), Some("ref-report.html"));
    assert_eq!(doc.source_name.as_deref(), Some("ref-report.html"));
    let files = h.store.read_html_bundle(&doc).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "ref-report.html");
    assert_eq!(files[0].bytes, page.as_bytes());

    // Replacing a multi-file bundle keeps its entry and says what it drops.
    let html = archived_html_plan(&h, "plan-folder");
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-folder", "body": "<title>One</title>", "format": "html",
                "if_hash": html.hash }),
    )
    .await;
    assert_eq!(
        h.store.get_approval(&id).unwrap().unwrap().title,
        "doc_write plan-folder · edit · \"October\" (drops 1 file of its HTML bundle)"
    );
    let (status, _) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settled(&h, &id).await["state"], "succeeded");
    let doc = h.store.doc_get("work", "plan-folder").unwrap().unwrap();
    assert_eq!(doc.entry_path.as_deref(), Some("index.html"));
    assert_eq!(doc.source_name.as_deref(), Some("plan-folder"));
    assert_eq!(doc.archived, 1);
    assert_eq!(h.store.read_html_bundle(&doc).unwrap().len(), 1);
    assert_eq!(live_bundle_rows(&h, &html.hash), 0);
}

/// A write that could not run once allowed is refused before it is held:
/// nothing reaches the operator's queue.
#[tokio::test]
async fn a_document_call_that_cannot_run_is_refused_before_it_is_held() {
    state::isolate();
    let h = harness_with(enabled()).await;
    // An over-limit HTML body is refused the same way
    // (`docs::tests::an_html_body_over_the_file_limit_is_refused_before_asking`);
    // this door's own request limit is lower still.
    for (name, args, says) in [
        (
            "doc_write",
            json!({ "slug": "ref-x", "body": "x", "format": "pdf" }),
            "format",
        ),
        ("doc_write", json!({ "slug": "Ref X", "body": "x" }), "slug"),
        ("doc_delete", json!({ "slug": "../x" }), "slug"),
        (
            "doc_delete",
            json!({ "slug": "context-0192f3a17c4e" }),
            "selected context",
        ),
        ("doc_delete", json!({ "slug": "shown-abc" }), "show_work"),
        ("doc_delete", json!({ "slug": "brief-abc" }), "brief"),
        (
            "doc_write",
            json!({ "slug": "brief-abc", "body": "<p>x</p>", "format": "html" }),
            "Markdown only",
        ),
    ] {
        let (_, v) = mcp(&h.operator, "work", tool_call(name, args)).await;
        let (err, out) = outcome(&v);
        assert!(err, "{name}: {out}");
        assert!(
            out.as_str().is_some_and(|m| m.contains(says)),
            "{name}: {out}"
        );
    }
    assert!(h.store.open_permission_views().unwrap().is_empty());
}

/// `doc_delete` is always asked, refuses a version the agent has not read,
/// and takes an HTML document's bundle with it.
#[tokio::test]
async fn doc_delete_is_asked_conflicts_on_a_stale_hash_and_removes_the_bundle() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let html = archived_html_plan(&h, "note-page");

    // Working notes are written unattended; deleting one still asks.
    let id = ask(
        &h,
        "doc_delete",
        json!({ "slug": "note-page", "if_hash": "stale" }),
    )
    .await;
    assert_eq!(
        h.store.get_approval(&id).unwrap().unwrap().title,
        "doc_delete note-page"
    );
    let (status, _) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    let out = settled(&h, &id).await;
    assert_eq!(out["state"], "failed", "{out}");
    assert!(
        out["reason"]
            .as_str()
            .is_some_and(|r| r.contains("changed since it was read")),
        "{out}"
    );
    assert!(h.store.doc_get("work", "note-page").unwrap().is_some());

    let id = ask(
        &h,
        "doc_delete",
        json!({ "slug": "note-page", "if_hash": html.hash }),
    )
    .await;
    let (status, _) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    let out = settled(&h, &id).await;
    assert_eq!(out["state"], "succeeded", "{out}");
    assert_eq!(out["result"]["deleted"], true, "{out}");
    assert!(h.store.doc_get("work", "note-page").unwrap().is_none());
    assert_eq!(live_bundle_rows(&h, &html.hash), 0);
}

/// A working note is written unattended, but not when the write would
/// convert an HTML page or drop files of its bundle: that is always asked,
/// and the held call is pinned to the document it was asked about.
#[tokio::test]
async fn a_working_note_write_that_reshapes_an_html_page_is_asked_and_pinned() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call(
            "doc_write",
            json!({ "slug": "note-plain", "body": "# Plain" }),
        ),
    )
    .await;
    let (err, out) = outcome(&v);
    assert!(!err && out["state"] != "awaiting_operator", "{out}");

    let html = archived_html_plan(&h, "note-page");
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "note-page", "body": "# Markdown" }),
    )
    .await;
    let row = h.store.get_approval(&id).unwrap().unwrap();
    assert!(
        row.title.contains("converts HTML to Markdown"),
        "{}",
        row.title
    );
    let held: Value = serde_json::from_str(&row.arguments).unwrap();
    assert_eq!(held["if_hash"], html.hash.as_str(), "{held}");
}

/// A call held without an `if_hash` runs against the document it was asked
/// about: a change made while it waited conflicts rather than being replaced.
#[tokio::test]
async fn a_held_write_conflicts_when_the_document_changed_while_it_waited() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let allow = |id: String| {
        let h = &h;
        async move {
            let (status, _) = answer(h, &id, json!({ "option_id": "allow_once" })).await;
            assert_eq!(status, StatusCode::OK);
            settled(h, &id).await
        }
    };
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-pin", "body": "# One" }),
    )
    .await;
    assert_eq!(allow(id).await["state"], "succeeded");
    let first = h.store.doc_get("work", "plan-pin").unwrap().unwrap();

    let waiting = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-pin", "body": "# Two" }),
    )
    .await;
    let id = ask(
        &h,
        "doc_write",
        json!({ "slug": "plan-pin", "body": "# Three", "if_hash": first.hash }),
    )
    .await;
    assert_eq!(allow(id).await["state"], "succeeded");

    let out = allow(waiting).await;
    assert_eq!(out["state"], "failed", "{out}");
    assert_eq!(
        h.store.doc_get("work", "plan-pin").unwrap().unwrap().body,
        "# Three"
    );
}
