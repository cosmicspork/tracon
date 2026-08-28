//! The review contract end to end: capture from a real worktree, a verdict
//! through the real API, and publication through a stub CLI that records what
//! the node ran and with what environment.
//!
//! The stub is the point. A test that let `gh` fall back to the operator's own
//! keyring would prove the opposite of what is being claimed: the assertion is
//! that the node passes the *brokered* credential and the *approved* bytes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::{
    config::Config,
    mcp::Tools,
    session::Manager,
    store::{now_ms, NodeRow, ReviewRow, SessionRow, Store},
    stream::Bus,
};

struct Fixture {
    app: axum::Router,
    harness: axum::Router,
    manager: Manager,
    store: Arc<Store>,
    dir: std::path::PathBuf,
    worktree: String,
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

/// A worktree with a commit beyond its base, a bare origin to push to, and a
/// stub `gh` on PATH that records how it was called.
async fn fixture(name: &str, credentials: &str) -> Fixture {
    fixture_with(name, credentials, |_| {}).await
}

async fn fixture_with(name: &str, credentials: &str, tweak: fn(&mut Config)) -> Fixture {
    // Per test: these run in parallel and each needs its own repo, its own
    // stub CLI, and its own log to assert against.
    let dir = std::env::temp_dir().join(format!("tracon-review-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // `-b main` and an explicit checkout: git's default branch name is a local
    // setting, and CI does not share this machine's.
    sh(&dir, "git init -q --bare -b main origin.git");
    sh(
        &dir,
        "git clone -q origin.git wt && cd wt && git checkout -qB main \
         && git config user.email t@e && git config user.name t \
         && echo base > a.txt && git add -A && git commit -qm base && git push -q origin main \
         && git checkout -qb feat/x && echo change >> a.txt && git add -A && git commit -qm work",
    );
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("gh"),
        "#!/bin/sh\n\
         { echo \"ARGS: $*\"; echo \"GH_TOKEN=$GH_TOKEN\"; } >> \"$(dirname \"$0\")/../gh.log\"\n\
         echo https://github.test/pull/1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let worktree = dir.join("wt").to_string_lossy().into_owned();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&NodeRow {
            id: "n1".into(),
            name: "t".into(),
            state: "ready".into(),
            failed_check: None,
            failed_detail: None,
            harness_id: "omp".into(),
            harness_pinned: "18.0.4".into(),
            harness_found: Some("18.0.4".into()),
            models_json: None,
            checked_at_ms: Some(now_ms()),
            is_self: 1,
            x25519_pub: None,
            last_seen_ms: None,
            reachable: 1,
        })
        .unwrap();
    store
        .insert_session(&SessionRow {
            id: "s1".into(),
            node_id: "n1".into(),
            channel: "work".into(),
            work_item_id: None,
            repo_path: dir.join("wt").to_string_lossy().into_owned(),
            worktree_path: Some(worktree.clone()),
            branch: "feat/x".into(),
            harness_id: "omp".into(),
            harness_version: "18.0.4".into(),
            harness_session_id: None,
            container_name: None,
            model: "m".into(),
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
        })
        .unwrap();

    // Point the node at this test's stub rather than mutating PATH, which is
    // process-global and races when these run in parallel.
    let mut cfg = Config::default();
    cfg.publish.gh = bin.join("gh").to_string_lossy().into_owned();
    // Checks run through the local runner in the worktree itself; the
    // default `just check` is not what a test fixture has.
    cfg.supervision.checks = vec!["test -f a.txt".into()];
    tweak(&mut cfg);
    let cfg = Arc::new(cfg);
    let tools = Arc::new(Tools {
        broker: Arc::new(toml::from_str(credentials).unwrap()),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg.clone(),
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let _ = tools.session.set(tracon::mcp::SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    let adapter: Arc<dyn tracon::adapter::HarnessAdapter> =
        Arc::new(tracon::adapter::omp::OmpAdapter::new("18.0.4"));
    manager.set_adapter(adapter.clone());
    let state = tracon::http::api::AppState {
        manager: manager.clone(),
        cfg,
        adapter,
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: std::sync::Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
    };
    let app = tracon::http::router(state.clone());
    let harness = tracon::http::harness_router(state);

    Fixture {
        app,
        harness,
        manager,
        store,
        dir,
        worktree,
    }
}

const WITH_GH: &str = r#"
    [credentials.gh]
    channels = ["work"]
    [credentials.gh.env]
    GH_TOKEN = "brokered-token-not-the-operators"
"#;

const WITHOUT_GH: &str = r#"
    [credentials.consulta]
    channels = ["work"]
"#;

impl Fixture {
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder().method(method).uri(uri);
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// What the agent sees when it asks about its review.
    async fn status_tool(&self, review_id: &str) -> Value {
        let ctx = tracon::mcp::CallContext {
            session_id: "s1".into(),
            channel: "work".into(),
            node_id: "n1".into(),
        };
        tracon::mcp::review::call(
            &self.store,
            &self.manager,
            &ctx,
            "review_status",
            &json!({ "review_id": review_id, "wait_secs": 0 }),
        )
        .await
        .unwrap()
    }

    async fn submit(&self) -> String {
        let capture = tracon::review::capture(&self.worktree, "main", "feat/x")
            .await
            .unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        self.store
            .insert_review(&ReviewRow {
                id: id.clone(),
                session_id: "s1".into(),
                node_id: "n1".into(),
                channel: "work".into(),
                kind: "pr".into(),
                title: "feat: the thing".into(),
                body: "what the diff does not say".into(),
                edited_title: None,
                edited_body: None,
                provider: "github".into(),
                target: json!({
                    "provider": "github", "project": "owner/name",
                    "base": "main", "branch": "feat/x"
                })
                .to_string(),
                diff: capture.diff,
                files: serde_json::to_string(&capture.files).unwrap(),
                head_sha: capture.head_sha,
                base_ref: "main".into(),
                added: capture.added,
                removed: capture.removed,
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
            })
            .unwrap();
        id
    }

    /// A tool call as the harness makes it, for the given session.
    async fn tool(&self, sid: &str, name: &str, args: Value) -> Value {
        let token = self.manager.register_tool_token_for_test(sid, "work").await;
        let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
        let req = Request::builder()
            .method("POST")
            .uri(format!("/mcp/{sid}"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = self.harness.clone().oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
        let is_err = v["result"]["isError"] == true;
        serde_json::from_str(text)
            .map(|parsed: Value| {
                if is_err {
                    json!({"error": parsed})
                } else {
                    parsed
                }
            })
            .unwrap_or(json!({ "error": text }))
    }

    fn submit_args(&self) -> Value {
        json!({"title": "feat: the thing", "body": "why", "provider": "github", "project": "owner/name", "base": "main"})
    }

    fn event_kinds(&self, sid: &str) -> Vec<String> {
        self.store
            .events_after(sid, 0, 500)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }

    fn gh_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("gh.log")).unwrap_or_default()
    }
}

#[tokio::test]
async fn a_review_waits_in_the_queue_until_it_is_decided() {
    const FN: &str = "a_review_waits_in_the_queue_until_it_is_decided";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;

    let (_, queue) = f.call("GET", "/api/queue", None).await;
    assert_eq!(queue["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(queue["reviews"][0]["id"], id.as_str());

    // Opening claims it, which is a metric rather than a lock.
    let (status, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["review"]["diff"].as_str().unwrap().contains("a.txt"));
    assert!(body["stale"].as_array().unwrap().is_empty());
    assert!(f
        .store
        .get_review(&id)
        .unwrap()
        .unwrap()
        .claimed_ms
        .is_some());
}

#[tokio::test]
async fn approving_publishes_the_approved_bytes_with_the_brokered_credential() {
    const FN: &str = "approving_publishes_the_approved_bytes_with_the_brokered_credential";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({
                "verdict": "approve",
                // The operator edited the title before approving.
                "title": "feat: the thing, renamed",
                "body": "what the diff does not say"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["published"], "https://github.test/pull/1");

    let log = f.gh_log();
    // What was published is what was approved, not what was submitted.
    assert!(log.contains("feat: the thing, renamed"), "{log}");
    assert!(!log.contains("--title feat: the thing "), "{log}");
    // And it went with the broker's token, not whatever the operator has.
    assert!(
        log.contains("GH_TOKEN=brokered-token-not-the-operators"),
        "{log}"
    );
    assert!(log.contains("pr create"), "{log}");

    // The branch actually reached the origin.
    let out = std::process::Command::new("git")
        .args([
            "-C",
            f.dir.join("origin.git").to_str().unwrap(),
            "branch",
            "--list",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("feat/x"));

    let r = f.store.get_review(&id).unwrap().unwrap();
    assert_eq!(r.state, "approved");
    assert_eq!(r.approved_title(), "feat: the thing, renamed");
}

#[tokio::test]
async fn a_second_verdict_cannot_overwrite_the_first() {
    const FN: &str = "a_second_verdict_cannot_overwrite_the_first";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;
    let (status, _) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "reject", "reason": "not yet" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already"));
    // Nothing was published by the second attempt.
    assert!(!f.gh_log().contains("pr create"));
}

#[tokio::test]
async fn a_rejection_needs_a_reason() {
    const FN: &str = "a_rejection_needs_a_reason";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;
    let (status, _) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "reject", "reason": "   " })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "new");
}

#[tokio::test]
async fn a_branch_that_moved_after_submit_cannot_be_approved() {
    const FN: &str = "a_branch_that_moved_after_submit_cannot_be_approved";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;
    // The agent kept working after submitting.
    sh(
        std::path::Path::new(&f.worktree),
        "echo more >> a.txt && git add -A && git commit -qm later",
    );

    let (status, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["stale"].as_array().unwrap(), &vec![json!("a.txt")]);

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("changed since submit"));
    assert!(
        !f.gh_log().contains("pr create"),
        "nothing may be published"
    );
}

#[tokio::test]
async fn without_a_brokered_credential_approval_publishes_nothing() {
    const FN: &str = "without_a_brokered_credential_approval_publishes_nothing";
    let f = fixture(FN, WITHOUT_GH).await;
    let id = f.submit().await;
    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"]["message"].as_str().unwrap().contains("gh"));
    // The review stays open and decidable: the operator approved, the node could
    // not publish, so the publish claim is undone and the card returns to the
    // queue (as claimed, since the operator just acted on it).
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "claimed");
    let (_, queue) = f.call("GET", "/api/queue", None).await;
    assert_eq!(queue["reviews"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn requesting_changes_keeps_one_evolving_thread() {
    const FN: &str = "requesting_changes_keeps_one_evolving_thread";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;

    // Asking for changes without saying what to change teaches nothing.
    let (status, _) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "revise" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "revise", "reason": "name the file for what it holds" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "revising");

    // It stays in the queue, so the thread is one card rather than two.
    let (_, queue) = f.call("GET", "/api/queue", None).await;
    assert_eq!(queue["reviews"].as_array().unwrap().len(), 1);
    let r = f.store.get_review(&id).unwrap().unwrap();
    assert_eq!(r.state, "revising");
    assert_eq!(
        r.verdict_reason.as_deref(),
        Some("name the file for what it holds")
    );

    // Nothing was published while changes were pending.
    assert!(!f.gh_log().contains("pr create"));

    // The agent resubmits the same review after doing the work.
    sh(
        std::path::Path::new(&f.worktree),
        "echo more >> a.txt && git add -A && git commit -qm revised",
    );
    let capture = tracon::review::capture(&f.worktree, "main", "feat/x")
        .await
        .unwrap();
    f.store
        .revise_review(
            &id,
            &capture.diff,
            &serde_json::to_string(&capture.files).unwrap(),
            &capture.head_sha,
            capture.added,
            capture.removed,
        )
        .unwrap();

    // Back to new, and no longer stale: the resubmission is what is reviewed.
    let (_, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert!(body["stale"].as_array().unwrap().is_empty());
    assert_eq!(body["review"]["state"], "claimed");
    assert!(
        body["review"]["verdict_reason"].is_null(),
        "the old note is cleared"
    );
}

#[tokio::test]
async fn two_concurrent_approvals_publish_the_change_once() {
    const FN: &str = "two_concurrent_approvals_publish_the_change_once";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;

    // Two operators (or two taps) approve at once. The publish claim is atomic,
    // so the change is opened once, not once per request.
    let body = json!({ "verdict": "approve" });
    let uri = format!("/api/reviews/{id}/verdict");
    let (a, b) = tokio::join!(
        f.call("POST", &uri, Some(body.clone())),
        f.call("POST", &uri, Some(body.clone())),
    );
    let mut statuses = [a.0, b.0];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "exactly one approval wins"
    );

    // The forge was asked to open the change exactly once.
    let opens = f.gh_log().matches("pr create").count();
    assert_eq!(opens, 1, "published exactly once");
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "approved");
}

#[tokio::test]
async fn a_revising_review_can_be_rejected_and_the_result_is_honest() {
    const FN: &str = "a_revising_review_can_be_rejected_and_the_result_is_honest";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;

    // Changes requested: the review is now waiting on the agent.
    let (status, _) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "revise", "reason": "split it" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "revising");

    // The operator can still reject it, and what the API reports is what the
    // store did — no success returned for a row that did not change.
    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "reject", "reason": "abandon this approach" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "rejected");
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "rejected");

    // A second reject now that it is decided is refused, not silently accepted.
    let (status, _) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "reject", "reason": "again" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn publish_pins_the_reviewed_commit_and_refuses_a_moved_branch() {
    const FN: &str = "publish_pins_the_reviewed_commit_and_refuses_a_moved_branch";
    let f = fixture(FN, WITH_GH).await;
    let mut cfg = Config::default();
    cfg.publish.gh = f.dir.join("bin/gh").to_string_lossy().into_owned();
    let broker = toml::from_str::<tracon::broker::Broker>(WITH_GH)
        .unwrap()
        .shared();
    let target = tracon::review::publish::Target {
        provider: "github".into(),
        project: "owner/name".into(),
        base: "main".into(),
        branch: "feat/x".into(),
    };
    // A head_sha that is not the worktree's HEAD stands in for a branch that
    // moved between approval and publish.
    let err = tracon::review::publish::publish(
        &broker,
        &cfg,
        "work",
        "n1",
        &f.worktree,
        &target,
        "0000000000000000000000000000000000000000",
        "t",
        "b",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            tracon::review::publish::PublishError::BranchMoved { .. }
        ),
        "{err}"
    );
    // Nothing was pushed or opened.
    assert!(!f.gh_log().contains("pr create"));
}

#[tokio::test]
async fn a_claim_releases_when_the_operator_leaves() {
    const FN: &str = "a_claim_releases_when_the_operator_leaves";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;

    f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "claimed");

    let (status, _) = f
        .call("POST", &format!("/api/reviews/{id}/release"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let r = f.store.get_review(&id).unwrap().unwrap();
    assert_eq!(r.state, "new");
    assert!(r.claimed_ms.is_none(), "a released claim measures nothing");

    // Still in the queue: releasing is not deciding.
    let (_, queue) = f.call("GET", "/api/queue", None).await;
    assert_eq!(queue["reviews"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_claim_from_a_vanished_client_lapses() {
    const FN: &str = "a_claim_from_a_vanished_client_lapses";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;
    f.call("GET", &format!("/api/reviews/{id}"), None).await;

    // Nothing is stale within the grace period.
    assert!(f.store.stale_claims(60_000).unwrap().is_empty());
    // Past it, the sweeper finds it.
    let stale = f.store.stale_claims(-1).unwrap();
    assert_eq!(stale, std::slice::from_ref(&id));
    f.store.release_review(&id).unwrap();
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "new");
}

#[tokio::test]
async fn submit_runs_the_checks_first_and_a_failure_refuses_the_submission() {
    const FN: &str = "submit_runs_the_checks_first_and_a_failure_refuses_the_submission";
    let f = fixture(FN, WITH_GH).await;
    // The worktree's own list wins over the node's, and this one fails.
    std::fs::create_dir_all(f.dir.join("wt/.tracon")).unwrap();
    std::fs::write(
        f.dir.join("wt/.tracon/checks"),
        "# project checks\ntest -f a.txt\nsh -c 'echo boom >&2; exit 3'\n",
    )
    .unwrap();
    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let err = v["error"].as_str().unwrap_or_default().to_string();
    assert!(err.contains("check failed"), "{v}");
    assert!(err.contains("exit 3") && err.contains("boom"), "{v}");
    assert!(f.store.open_reviews().unwrap().is_empty());
    let kinds = f.event_kinds("s1");
    assert_eq!(
        kinds.iter().filter(|k| *k == "check_result").count(),
        2,
        "stops at the first failure: {kinds:?}"
    );
    assert!(kinds.contains(&"check_started".to_string()));
    assert!(kinds.contains(&"review_rejected".to_string()));
    assert_eq!(
        f.store.get_session("s1").unwrap().unwrap().state,
        "running",
        "back to running after the check"
    );

    // Fix the check: the submission goes through, with the checks on the row.
    std::fs::write(f.dir.join("wt/.tracon/checks"), "test -f a.txt\n").unwrap();
    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let id = v["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();
    assert_eq!(v["review_session"]["state"], "none", "{v}");
    let r = f.store.get_review(&id).unwrap().unwrap();
    let checks: Vec<Value> = serde_json::from_str(r.checks_json.as_deref().unwrap()).unwrap();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["command"], "test -f a.txt");
    assert_eq!(checks[0]["ok"], true);
}

#[tokio::test]
async fn a_diff_over_the_cap_is_refused_before_any_check_runs() {
    const FN: &str = "a_diff_over_the_cap_is_refused_before_any_check_runs";
    let f = fixture_with(FN, WITH_GH, |c| c.review.max_diff_lines = 0).await;
    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let err = v["error"].as_str().unwrap_or_default();
    assert!(err.contains("the cap is 0 lines"), "{v}");
    assert!(err.contains("Split the change"), "{v}");
    assert!(f.store.open_reviews().unwrap().is_empty());
    let kinds = f.event_kinds("s1");
    assert!(kinds.contains(&"review_rejected".to_string()));
    assert!(!kinds.contains(&"check_started".to_string()), "{kinds:?}");
}

#[tokio::test]
async fn a_bound_review_model_spawns_a_fresh_review_session_whose_verdict_lands_on_the_card() {
    const FN: &str =
        "a_bound_review_model_spawns_a_fresh_review_session_whose_verdict_lands_on_the_card";
    let f = fixture(FN, WITH_GH).await;
    f.store
        .channel_put(
            "work",
            b"",
            &json!({"phases": {"review": {"model": "m/reviewer", "budget_tokens": 5000}}})
                .to_string(),
        )
        .unwrap();
    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let id = v["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();
    assert_eq!(v["review_session"]["state"], "started", "{v}");
    let rsid = v["review_session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = f.store.get_review(&id).unwrap().unwrap();
    assert_eq!(r.review_session_id.as_deref(), Some(rsid.as_str()));
    let rs = f.store.get_session(&rsid).unwrap().unwrap();
    assert_eq!(rs.phase, "review");
    assert_eq!(rs.model, "m/reviewer");
    assert_eq!(rs.budget_tokens, 5000);
    assert_eq!(rs.review_id.as_deref(), Some(id.as_str()));
    assert_eq!(
        rs.repo_path,
        f.store.get_session("s1").unwrap().unwrap().repo_path
    );
    // Its worktree is at the reviewed commit, on its own branch.
    for _ in 0..300 {
        if f.store
            .get_session(&rsid)
            .unwrap()
            .unwrap()
            .worktree_path
            .is_some()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let rs = f.store.get_session(&rsid).unwrap().unwrap();
    let wt = rs.worktree_path.clone().expect("review worktree");
    let head = std::process::Command::new("git")
        .args(["-C", &wt, "rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), r.head_sha);
    assert!(rs.branch.starts_with("review/"));

    // A review session sees only reading tools and its verdict.
    let v = f.tool(&rsid, "submit_review", f.submit_args()).await;
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not offered to a review session"),
        "{v}"
    );
    let v = f
        .tool(
            &rsid,
            "review_verdict",
            json!({"verdict": "request_changes", "summary": "a.txt grew without a test",
                   "findings": [{"path": "a.txt", "line": 2, "severity": "should", "note": "cover it"}]}),
        )
        .await;
    assert_eq!(v["recorded"], true, "{v}");
    let r = f.store.get_review(&id).unwrap().unwrap();
    let verdict: Value = serde_json::from_str(r.ai_verdict_json.as_deref().unwrap()).unwrap();
    assert_eq!(verdict["verdict"], "request_changes");
    assert_eq!(verdict["model"], "m/reviewer");
    assert_eq!(verdict["findings"][0]["path"], "a.txt");
    assert!(f.event_kinds(&rsid).contains(&"review_verdict".to_string()));
    // The human's verdict is untouched: the review is still open.
    assert_eq!(f.store.open_reviews().unwrap().len(), 1);
    // An execute session cannot give one.
    let v = f
        .tool(
            "s1",
            "review_verdict",
            json!({"verdict": "approve", "summary": "x"}),
        )
        .await;
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("only a review session"),
        "{v}"
    );
}

#[tokio::test]
async fn publishing_closes_the_item_the_session_holds() {
    const FN: &str = "publishing_closes_the_item_the_session_holds";
    let f = fixture(FN, WITH_GH).await;
    let item = tracon::corpus::work::create(
        &f.store,
        &Bus::new(),
        "n1",
        tracon::corpus::work::NewWork {
            channel: "work".into(),
            project_id: None,
            title: "The thing".into(),
            body: String::new(),
            deps: vec![],
            priority: 0,
            discovered_from: None,
            discovered_by_session: None,
        },
    )
    .unwrap();
    f.store
        .conn()
        .execute(
            "UPDATE session SET work_item_id = ?1 WHERE id = 's1'",
            [&item.id],
        )
        .unwrap();
    let id = f.submit().await;
    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let closed = f.store.work_get(&item.id).unwrap().unwrap();
    assert_eq!(closed.state, "closed");
    assert_eq!(closed.closed_by_session.as_deref(), Some("s1"));
    assert!(f.event_kinds("s1").contains(&"work_closed".to_string()));
}

/// An edited diff is a request for changes carrying a patch. What matters is
/// that the bytes survive the round trip intact — a patch is whitespace, and
/// `git apply` calls one with a missing final newline corrupt.
#[tokio::test]
async fn an_edited_diff_reaches_the_agent_and_still_applies() {
    const FN: &str = "an_edited_diff_reaches_the_agent_and_still_applies";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;

    // What the interface builds: the file as submitted, with one line changed.
    let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n";
    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({
                "verdict": "revise",
                "reason": "call it what it is",
                "patch": patch,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "revising");

    let row = f.store.get_review(&id).unwrap().unwrap();
    assert_eq!(
        row.revision_patch.as_deref(),
        Some(patch),
        "the patch must arrive byte for byte, trailing newline included"
    );

    // And the agent is told to apply it, not merely that changes were asked for.
    let status = f.status_tool(&id).await;
    assert_eq!(status["state"], "changes_requested");
    assert_eq!(status["patch"], patch);
    assert_eq!(status["notes"], "call it what it is");
    assert!(
        status["message"].as_str().unwrap().contains("git apply"),
        "the agent should be told how to apply it: {status}"
    );
}

/// Asking for changes without editing anything is unchanged: notes, no patch.
#[tokio::test]
async fn asking_for_changes_without_an_edit_carries_no_patch() {
    const FN: &str = "asking_for_changes_without_an_edit_carries_no_patch";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;
    let (status, _) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "revise", "reason": "rename the thing" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let row = f.store.get_review(&id).unwrap().unwrap();
    assert!(row.revision_patch.is_none());
    let s = f.status_tool(&id).await;
    assert!(s["patch"].is_null());
    assert!(!s["message"].as_str().unwrap().contains("git apply"));
}

/// A resubmission replaces the diff the patch described, so the patch goes
/// with it rather than lingering against text that no longer exists.
#[tokio::test]
async fn resubmitting_clears_the_patch() {
    const FN: &str = "resubmitting_clears_the_patch";
    let f = fixture(FN, WITH_GH).await;
    let id = f.submit().await;
    f.call(
        "POST",
        &format!("/api/reviews/{id}/verdict"),
        Some(json!({ "verdict": "revise", "reason": "x", "patch": "--- a/a\n+++ b/a\n" })),
    )
    .await;
    assert!(f
        .store
        .get_review(&id)
        .unwrap()
        .unwrap()
        .revision_patch
        .is_some());

    f.store
        .revise_review(&id, "new diff", "[]", "deadbeef", 1, 1)
        .unwrap();
    assert!(
        f.store
            .get_review(&id)
            .unwrap()
            .unwrap()
            .revision_patch
            .is_none(),
        "a patch must not outlive the diff it described"
    );
}
