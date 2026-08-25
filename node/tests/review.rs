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
    stream::Hub,
};

struct Fixture {
    app: axum::Router,
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
    let cfg = Arc::new(cfg);
    let tools = Arc::new(Tools {
        broker: Arc::new(toml::from_str(credentials).unwrap()),
        cfg: cfg.clone(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Hub::new(),
        cfg.clone(),
        "n1".into(),
        tools.clone(),
        Default::default(),
    );
    let _ = tools.session.set(tracon::mcp::SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    let app = tracon::http::router(tracon::http::api::AppState {
        manager,
        cfg,
        adapter: Arc::new(tracon::adapter::omp::OmpAdapter::new("18.0.4")),
        node_id: "n1".into(),
        tools,
    });

    Fixture {
        app,
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
            })
            .unwrap();
        id
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
    let broker: tracon::broker::Broker = toml::from_str(WITH_GH).unwrap();
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
