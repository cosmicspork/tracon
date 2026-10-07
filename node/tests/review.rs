//! The review contract end to end: capture from a real worktree, a verdict
//! through the real API, and publication through a stub CLI that records what
//! the node ran and with what environment.
//!
//! The stub is the point. A test that let `gh` fall back to the operator's own
//! keyring would prove the opposite of what is being claimed: the assertion is
//! that the node passes the *brokered* credential and the *approved* bytes.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::{
    config::Config,
    mcp::Tools,
    session::Manager,
    store::{now_ms, ReviewRow, Store},
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

// A fixture's Git is the one an agent would run, so it must honour replace refs
// even where the suite itself runs with them off: a node session sets
// GIT_NO_REPLACE_OBJECTS, and inheriting it left the planted ref inert.
fn sh(dir: &std::path::Path, script: &str) {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env_remove("GIT_NO_REPLACE_OBJECTS")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The same, returning what the script printed.
fn sh_out(dir: &std::path::Path, script: &str) -> String {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env_remove("GIT_NO_REPLACE_OBJECTS")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Point `refs/replace/<HEAD>` at a commit holding different bytes. Git
/// substitutes it for HEAD everywhere that has not opted out, so every
/// assertion after this is about whether the node opted out.
fn plant_replace_ref(worktree: &std::path::Path) -> String {
    let head = sh_out(worktree, "git rev-parse HEAD");
    sh(
        worktree,
        "echo poisoned > a.txt && git add -A && git commit -qm poison",
    );
    let poison = sh_out(worktree, "git rev-parse HEAD");
    sh(worktree, &format!("git reset -q --hard {head}"));
    sh(worktree, &format!("git replace {head} {poison}"));
    // The plant has to be live, or the assertions that follow prove nothing.
    assert_eq!(sh_out(worktree, "git show HEAD:a.txt"), "poisoned");
    head
}

/// A worktree with a commit beyond its base, a bare origin to push to, and a
/// stub `gh` on PATH that records how it was called.
async fn fixture(name: &str, credentials: &str) -> Fixture {
    fixture_with(name, credentials, |_| {}).await
}

/// The enclosing test's name, so two tests can never share a fixture
/// directory by a copy-pasted constant.
macro_rules! test_name {
    () => {{
        fn here() {}
        // Inside an async test the path is `…::name::{{closure}}::here`; the
        // closure segments are dropped, then the last one is the test.
        let full = std::any::type_name_of_val(&here);
        full.strip_suffix("::here")
            .unwrap_or(full)
            .rsplit("::")
            .find(|s| !s.starts_with('{'))
            .unwrap()
    }};
}

async fn fixture_with(name: &str, credentials: &str, tweak: fn(&mut Config)) -> Fixture {
    // Per test: these run in parallel and each needs its own repo, its own
    // stub CLI, and its own log to assert against.
    let dir = state::scratch(&format!("review-{name}"));
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
    // `publish()` builds the forge remote itself (never trusting the
    // candidate's own git config) and always pushes `https://`; this stub
    // swaps that URL for the local bare repo right before `git remote add
    // origin` runs (always the last four args of that call — see
    // `review::publish::publisher_git_inner`), so the push lands somewhere
    // real without touching the network. Every other git invocation passes
    // straight through untouched.
    std::fs::write(
        bin.join("git-remote-stub"),
        format!(
            "#!/bin/bash\n\
             args=(\"$@\")\n\
             echo \"$*\" >> \"$(dirname \"$0\")/../git.log\"\n\
             n=${{#args[@]}}\n\
             if [ \"$n\" -ge 4 ] \\\n\
             \t&& [ \"${{args[$((n-4))]}}\" = remote ] \\\n\
             \t&& [ \"${{args[$((n-3))]}}\" = add ] \\\n\
             \t&& [ \"${{args[$((n-2))]}}\" = origin ]; then\n\
             \targs[$((n-1))]='{origin}'\n\
             fi\n\
             exec git \"${{args[@]}}\"\n",
            origin = dir.join("origin.git").to_string_lossy(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            bin.join("git-remote-stub"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    // The session's workspace is a runtime-owned volume, never the host
    // checkout above: import it once through the same `LocalBackend` a real
    // node uses, so what the rest of this fixture treats as "the worktree"
    // is the mutable copy a harness would actually see, not a host bind
    // mount. `workspace_id` is unique per test: every test hardcodes session
    // id `s1`, but the state directory (and so `local-runtime/<volume>`) is
    // shared by the whole test binary.
    let workspace_id = format!("s1-{name}");
    let backend: Arc<dyn tracon::boundary::Backend> = Arc::new(tracon::runner::local::LocalBackend);
    let workspace = tracon::workspace::Workspace {
        id: workspace_id.clone(),
        volume: tracon::workspace::volume_name(&workspace_id),
        snapshot: tracon::workspace::snapshot_path(&workspace_id),
    };
    tracon::workspace::import(backend.as_ref(), &workspace, &dir.join("wt"))
        .await
        .unwrap();
    let worktree = tracon::runner::local::local_runtime_path(&workspace.volume)
        .to_string_lossy()
        .into_owned();

    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&{
            let mut r = support::rows::node_row("n1", "t");
            r.harness_id = "omp".into();
            r.harness_pinned = "18.0.4".into();
            r.harness_found = Some("18.0.4".into());
            r.models_json = None;
            r
        })
        .unwrap();
    store
        .insert_session(&{
            let mut r = support::rows::session_row("s1", "n1", "work");
            // `workspace://<id>` is how a session names a durable, runtime-owned
            // workspace rather than a host checkout (see `Manager::snapshot_workspace`).
            r.repo_path = format!("workspace://{workspace_id}");
            r.worktree_path = None;
            r.branch = "feat/x".into();
            r.harness_id = "omp".into();
            r.harness_version = "18.0.4".into();
            r.started_mono_ms = Some(0);
            r
        })
        .unwrap();

    // Point the node at this test's stub rather than mutating PATH, which is
    // process-global and races when these run in parallel.
    let mut cfg = Config::default();
    cfg.publish.gh = bin.join("gh").to_string_lossy().into_owned();
    cfg.publish.git = bin.join("git-remote-stub").to_string_lossy().into_owned();
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
        backend,
    );
    let _ = tools.session.set(tracon::mcp::SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    // A fake harness: a review session must actually run for the tool-gating
    // assertions to be about a live session rather than a start-up race.
    let adapter: Arc<dyn tracon::adapter::HarnessAdapter> = Arc::new(support::fake::FakeAdapter {
        tx: Arc::new(tokio::sync::Mutex::new(None)),
        tokens: Arc::new(tokio::sync::Mutex::new(0)),
    });
    let state = tracon::http::api::AppState {
        manager: manager.clone(),
        cfg,
        adapter,
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: std::sync::Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
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
        self.status_tool_waiting(review_id, 0).await
    }

    /// The same tool, asked to block: what an agent gets when it waits.
    async fn status_tool_waiting(&self, review_id: &str, wait_secs: u64) -> Value {
        let ctx = tracon::mcp::CallContext::session("s1", "work", "n1");
        tracon::mcp::review::call(
            &self.store,
            &self.manager,
            &ctx,
            "review_status",
            &json!({ "review_id": review_id, "wait_secs": wait_secs }),
        )
        .await
        .unwrap()
    }

    async fn submit(&self) -> String {
        self.submit_as(
            "s1",
            json!({
                "provider": "github", "project": "owner/name",
                "base": "main", "branch": "feat/x"
            }),
        )
        .await
    }

    async fn submit_as(&self, session: &str, target: Value) -> String {
        let capture = tracon::review::capture(&self.worktree, "main", "feat/x")
            .await
            .unwrap();
        // The candidate a real submission records, because publication
        // refuses to push bytes it cannot check against a reviewed tree.
        let tree = sh_out(
            std::path::Path::new(&self.worktree),
            &format!("git rev-parse {}^{{tree}}", capture.head_sha),
        );
        self.store
            .insert_candidate(&tracon::store::CandidateRow {
                id: tracon::store::candidate_id(&capture.head_sha, "work"),
                head_sha: capture.head_sha.clone(),
                tree_sha: Some(tree),
                channel: "work".into(),
                owner_session_id: Some(session.into()),
                source_kind: "git".into(),
                captured_ms: now_ms(),
                capture_json: "{}".into(),
            })
            .unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        self.store
            .insert_review(&ReviewRow {
                id: id.clone(),
                session_id: Some(session.into()),
                node_id: "n1".into(),
                channel: "work".into(),
                kind: "pr".into(),
                title: "feat: the thing".into(),
                body: "what the diff does not say".into(),
                edited_title: None,
                edited_body: None,
                provider: "github".into(),
                target: target.to_string(),
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
                lane: None,
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
        if let Some(message) = v["error"]["message"].as_str() {
            return json!({ "error": message });
        }
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

    /// Every git command the node ran, in order: what makes "it did not push
    /// a second time" an observation rather than an inference.
    fn git_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("git.log")).unwrap_or_default()
    }

    fn forget_logs(&self) {
        let _ = std::fs::remove_file(self.dir.join("git.log"));
        let _ = std::fs::remove_file(self.dir.join("gh.log"));
    }

    /// What the node calls this publication: the same review, revision,
    /// target and commit name the same one, which is what a resumed attempt
    /// relies on.
    fn publication_id(&self, review_id: &str) -> String {
        let review = self.store.get_review(review_id).unwrap().unwrap();
        let target = serde_json::from_str(&review.target).unwrap();
        let revision = self
            .store
            .latest_review_revision(review_id)
            .unwrap()
            .map(|r| r.id);
        tracon::authority::publication_id(&review, revision.as_deref(), &target)
    }

    /// The publication record as it stands.
    fn publication(&self, review_id: &str) -> tracon::store::PublicationRow {
        self.store
            .publication(&self.publication_id(review_id))
            .unwrap()
            .expect("a publication record")
    }

    /// An attempt a previous process began and never finished: exactly what a
    /// crash leaves behind, written the way the node writes it.
    fn interrupted_attempt(&self, review_id: &str, state: &str, pushed: Option<&str>) {
        let review = self.store.get_review(review_id).unwrap().unwrap();
        let target: tracon::review::publish::Target = serde_json::from_str(&review.target).unwrap();
        let id = self.publication_id(review_id);
        self.store
            .publication_begin(&tracon::store::PublicationBegin {
                id: &id,
                review_id,
                revision_id: None,
                candidate_id: &tracon::store::candidate_id(&review.head_sha, "work"),
                channel: "work",
                node_id: "n1",
                provider: &target.provider,
                project: &target.project,
                base: &target.base,
                branch: &target.branch,
                head_sha: &review.head_sha,
                // Not this process: that is what makes it interrupted rather
                // than a decision in progress.
                instance: "a-process-that-is-no-longer-running",
            })
            .unwrap();
        if let Some(sha) = pushed {
            self.store.publication_pushed(&id, sha).unwrap();
        }
        if state == "opening" {
            self.store.publication_opening(&id).unwrap();
        }
        // The review is mid-publish, as the crash left it.
        assert!(self.store.begin_publish(review_id, None).unwrap());
    }
}

#[tokio::test]
async fn a_review_waits_in_the_queue_until_it_is_decided() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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

/// A review mirrored from another node is read here but owned there: its
/// worktree path names the owner's disk, and its candidate and checks never
/// left it. Reading them locally would report "changed since submit" and
/// "evidence missing" about a change that is neither, and disable approval
/// for a verdict that is forwarded to the owner anyway.
#[tokio::test]
async fn a_mirrored_review_is_not_judged_by_what_this_node_lacks() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let local = f.submit().await;
    let mut mirrored = f.store.get_review(&local).unwrap().unwrap();
    mirrored.id = uuid::Uuid::now_v7().to_string();
    mirrored.node_id = "peer-node".into();
    mirrored.target = json!({"worktree": "/nowhere/on/this/node"}).to_string();
    f.store.insert_review(&mirrored).unwrap();

    let (status, body) = f
        .call("GET", &format!("/api/reviews/{}", mirrored.id), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["remote_owner"], "peer-node");
    assert!(body["stale"].as_array().unwrap().is_empty(), "{body}");
    assert!(body["evidence"].is_null());
    assert!(body["intent"].is_null());

    // The node's own review is still read as before.
    let (_, body) = f.call("GET", &format!("/api/reviews/{local}"), None).await;
    assert!(body["remote_owner"].is_null());
    assert!(body["intent"].is_object());
}

/// An MCP client fails the call long before a human decides, and a failed call
/// loses the turn. However long the agent asks to wait, the node comes back
/// inside the client's budget with something the agent can act on.
#[tokio::test(start_paused = true)]
async fn a_long_wait_is_capped_and_returns_the_state_instead_of_timing_out() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = f.submit().await;

    let began = tokio::time::Instant::now();
    let s = f.status_tool_waiting(&id, 600).await;
    let waited = began.elapsed();

    assert_eq!(s["review_id"], id.as_str());
    assert_eq!(s["state"], "new");
    assert_eq!(s["still_waiting"], true);
    assert_eq!(s["waited_secs"], 45);
    assert!(s["message"].as_str().unwrap().contains("review_status"));
    // Capped, not honoured: nowhere near the 600 seconds asked for, and
    // inside the shortest client timeout either supported harness gives a
    // tool call — 60 s, the MCP TS SDK default OpenCode inherits and the node
    // then states explicitly (`config-state.md` §5.4).
    assert!(
        waited < std::time::Duration::from_secs(50),
        "waited {waited:?}"
    );
    // The wait leaves the review alone, so the next call picks up where this
    // one left off.
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "new");
}

/// A harness the operator runs has no worktree on its session; the review
/// names the operator's, and everything after submit reads it from there.
#[tokio::test]
async fn an_external_review_publishes_from_the_worktree_it_names() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    f.store
        .insert_session(&{
            let mut r = support::rows::session_row("ext", "n1", "work");
            r.harness_id = "external".into();
            r.repo_path = String::new();
            r.worktree_path = None;
            r.branch = String::new();
            r.started_mono_ms = Some(0);
            r
        })
        .unwrap();
    let id = f
        .submit_as(
            "ext",
            json!({
                "provider": "github", "project": "owner/name",
                "base": "main", "branch": "feat/x", "worktree": f.worktree
            }),
        )
        .await;

    let (status, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["stale"].as_array().unwrap().is_empty(), "{body}");

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "approved");
    let log = f.gh_log();
    assert!(log.contains("ARGS: pr create"), "{log}");
    assert!(
        log.contains("GH_TOKEN=brokered-token-not-the-operators"),
        "{log}"
    );
    // The description is the approved text, with nothing appended.
    assert!(!log.contains("tracon-publication"), "{log}");
}

#[tokio::test]
async fn approving_publishes_the_approved_bytes_with_the_brokered_credential() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITHOUT_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
        worktree: None,
        change: None,
    };
    // A head_sha that is not the worktree's HEAD stands in for a branch that
    // moved between approval and publish.
    let err = tracon::review::publish::publish(
        &broker,
        &cfg,
        &tracon::review::publish::Publication {
            channel: "work",
            node_id: "n1",
            id: "testcandidate",
            candidate: &f.worktree,
            target: &target,
            head_sha: "0000000000000000000000000000000000000000",
            reviewed_tree: "1111111111111111111111111111111111111111",
            outputs: &Default::default(),
            lease: None,
            rewrite: false,
            resume: false,
            pushed: false,
            before_push: None,
            journal: &tracon::review::publish::NoJournal,
        },
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
async fn operator_required_checks_refuse_submission_and_record_each_outcome() {
    state::isolate();
    // Required checks are node policy, not something a candidate's own
    // `.tracon/checks` can redirect (see `review::checks::required_definitions`);
    // a second command that only passes once the agent has done more work
    // stands in for that policy gate.
    let f = fixture_with(test_name!(), WITH_GH, |c| {
        c.supervision.checks = vec![
            "test -f a.txt".into(),
            "test -f .tracon/allowed || (echo boom >&2; exit 3)".into(),
        ];
    })
    .await;
    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let err = v["error"].as_str().unwrap_or_default().to_string();
    assert!(err.contains("check failed"), "{v}");
    assert!(err.contains("exit 3") && err.contains("boom"), "{v}");
    assert!(f.store.open_reviews().unwrap().is_empty());
    let kinds = f.event_kinds("s1");
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "check_result").count(),
        2,
        "all configured checks leave a durable result: {kinds:?}"
    );
    assert!(kinds.contains(&"check_started".to_string()));
    assert!(kinds.contains(&"review_rejected".to_string()));
    assert_eq!(
        f.store.get_session("s1").unwrap().unwrap().state,
        "running",
        "back to running after the check"
    );

    // Fix the check by doing the work in the session's own runtime workspace
    // (never the host checkout), committed so the immutable candidate tree
    // the checks run against actually contains it: an untracked file is
    // invisible to the same Git-tree snapshot that keeps candidate identity
    // exact.
    std::fs::create_dir_all(std::path::Path::new(&f.worktree).join(".tracon")).unwrap();
    std::fs::write(
        std::path::Path::new(&f.worktree).join(".tracon/allowed"),
        "",
    )
    .unwrap();
    sh(
        std::path::Path::new(&f.worktree),
        "git add .tracon/allowed && git commit -qm allow-checks",
    );
    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let id = v["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();
    let r = f.store.get_review(&id).unwrap().unwrap();
    let checks: Vec<Value> = serde_json::from_str(r.checks_json.as_deref().unwrap()).unwrap();
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0]["command"], "test -f a.txt");
    assert_eq!(checks[1]["ok"], true);
}

/// A required check whose command the check image does not have is the
/// environment's failing, not the candidate's. The submission still stops —
/// nothing was verified — but the refusal names the missing tool and the image
/// it looked in, never tells the agent to fix its code, and records no
/// `review_rejected`: there is nothing about the change to reject.
///
/// This runs on `LocalBackend`, which executes on the host and honours no image
/// at all, so what it proves is the reporting. That the check runs in the
/// repository's toolchain image is the Podman runner's business and is covered
/// by `review::checks`' own tests.
#[tokio::test]
async fn a_check_the_image_has_no_tool_for_is_not_the_agents_failure() {
    state::isolate();
    let f = fixture_with(test_name!(), WITH_GH, |c| {
        c.supervision.checks = vec!["tracon-no-such-tool --check".into()];
    })
    .await;
    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let err = v["error"].as_str().unwrap_or_default().to_string();
    assert!(err.contains("not runnable here"), "{v}");
    assert!(
        err.contains("tracon-no-such-tool"),
        "the refusal names the tool that is missing: {v}"
    );
    assert!(
        !err.contains("Fix it and submit again"),
        "the agent is never asked to fix an environment it does not control: {v}"
    );
    assert!(f.store.open_reviews().unwrap().is_empty());

    let events = f.store.events_after("s1", 0, 500).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"check_not_runnable"), "{kinds:?}");
    assert!(
        !kinds.contains(&"review_rejected"),
        "nothing about the candidate was established, so nothing is rejected: {kinds:?}"
    );
    let not_runnable = events
        .iter()
        .find(|e| e.kind == "check_not_runnable")
        .unwrap();
    assert_eq!(not_runnable.payload["missing_tool"], "tracon-no-such-tool");
    // The durable evidence row says it could not run, which is what keeps it
    // out of `latest_reusable_check`.
    let result = events
        .iter()
        .find(|e| e.kind == "check_result")
        .expect("the check still leaves a durable result");
    assert_eq!(result.payload["outcome"], "not_runnable");
    assert_eq!(result.payload["ok"], false, "not runnable is never a pass");
    assert_eq!(
        f.store.get_session("s1").unwrap().unwrap().state,
        "running",
        "back to running, as after any check"
    );
}

/// A repository's `[[repo]]` entry replaces the node-wide checks for work
/// that came from it, and its `prepare` commands run first on the very copy
/// the check then sees. The node-wide check here can only fail, and the
/// repository's check passes only if preparation ran before it in the same
/// directory — so a review opening proves both.
///
/// The session is on a workspace, as every resumed session is, so this also
/// proves the entry is found through the session that first imported it.
#[tokio::test]
async fn a_repo_entry_names_its_own_checks_and_prepares_before_them() {
    state::isolate();
    let f = fixture_with(test_name!(), WITH_GH, |c| {
        c.supervision.checks = vec!["false".into()];
        c.repo = vec![tracon::config::Repo {
            path: "owner/prepared-app".into(),
            checks: Some(vec!["test -f fetched.txt".into()]),
            prepare: vec!["echo fetched > fetched.txt".into()],
            ..Default::default()
        }];
    })
    .await;
    let workspace = f
        .store
        .get_session("s1")
        .unwrap()
        .unwrap()
        .repo_path
        .strip_prefix("workspace://")
        .expect("the fixture session is on a workspace")
        .to_string();
    f.store
        .insert_session(&{
            let mut origin = support::rows::session_row(&workspace, "n1", "work");
            origin.repo_path = "/clones/github.com/owner/prepared-app".into();
            origin.state = "closed".into();
            origin
        })
        .unwrap();

    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    assert!(v["error"].is_null(), "{v}");
    assert_eq!(f.store.open_reviews().unwrap().len(), 1, "{v}");

    let events = f.store.events_after("s1", 0, 500).unwrap();
    let started = events
        .iter()
        .find(|e| e.kind == "check_started")
        .expect("checks ran");
    assert_eq!(
        started.payload["commands"],
        json!(["test -f fetched.txt"]),
        "the repository's checks, never the node-wide list"
    );
    let results: Vec<_> = events.iter().filter(|e| e.kind == "check_result").collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].payload["outcome"], "passed");
}

#[tokio::test]
async fn candidate_controlled_check_file_cannot_replace_operator_required_checks() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    std::fs::create_dir_all(f.dir.join("wt/.tracon")).unwrap();
    std::fs::write(f.dir.join("wt/.tracon/checks"), "sh -c 'exit 97'\n").unwrap();
    sh(
        &f.dir,
        "cd wt && git add .tracon/checks && git commit -qm candidate-check-file",
    );

    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let id = v["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();
    let r = f.store.get_review(&id).unwrap().unwrap();
    let checks: Vec<Value> = serde_json::from_str(r.checks_json.as_deref().unwrap()).unwrap();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["command"], "test -f a.txt");
    assert_eq!(checks[0]["ok"], true);
    assert!(f
        .event_kinds("s1")
        .contains(&"candidate_verified".to_string()));
}

/// The worktree is the agent's and stays writable while a submission is being
/// checked. What the checks run on has to be the tree the node captured, so a
/// write that lands after (or during) the capture cannot become the thing that
/// was verified — and the pass that tree earned cannot be carried over to the
/// next one.
#[tokio::test]
async fn a_worktree_mutated_after_capture_is_not_what_the_checks_ran_on() {
    state::isolate();
    // The check prints the file it read, so what it saw is recoverable from
    // the evidence rather than inferred from an exit code.
    let f = fixture_with(test_name!(), WITH_GH, |c| {
        c.supervision.checks = vec!["cat marker.txt".into()];
    })
    .await;
    let wt = std::path::Path::new(&f.worktree);
    std::fs::write(wt.join("marker.txt"), "committed").unwrap();
    sh(wt, "git add marker.txt && git commit -qm marker");
    let captured_head = sh_out(wt, "git rev-parse HEAD");
    let captured_tree = sh_out(wt, "git rev-parse HEAD^{tree}");

    // The agent keeps writing. Only the committed tree is captured, so this
    // is exactly the byte the check must not see.
    std::fs::write(wt.join("marker.txt"), "mutated").unwrap();

    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let first_review = v["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();
    let first_candidate = v["candidate_id"].as_str().unwrap().to_string();
    let r = f.store.get_review(&first_review).unwrap().unwrap();
    let checks: Vec<Value> = serde_json::from_str(r.checks_json.as_deref().unwrap()).unwrap();
    assert_eq!(checks[0]["tail"], "committed", "{checks:?}");
    assert_eq!(checks[0]["ok"], true, "{checks:?}");
    // The retained candidate is the same bytes the check saw: the evidence and
    // the execution agree about what the candidate is.
    let files = f.store.candidate_files(&first_candidate).unwrap();
    let marker = files.iter().find(|f| f.path == "marker.txt").unwrap();
    assert_eq!(String::from_utf8_lossy(&marker.content), "committed");

    // Now commit the mutation. It is a different tree, so it is a different
    // candidate, and the pass the old tree earned is not evidence about it.
    sh(wt, "git commit -qam mutate");
    let v = f
        .tool(
            "s1",
            "submit_review",
            json!({"title": "feat: the thing", "body": "why", "provider": "github",
                   "project": "owner/name", "base": "main", "review_id": first_review}),
        )
        .await;
    let second_candidate = v["candidate_id"].as_str().unwrap_or_else(|| panic!("{v}"));
    assert_ne!(second_candidate, first_candidate);
    assert_eq!(v["checks_reused"], false, "{v}");
    let r = f.store.get_review(&first_review).unwrap().unwrap();
    let checks: Vec<Value> = serde_json::from_str(r.checks_json.as_deref().unwrap()).unwrap();
    assert_eq!(checks[0]["tail"], "mutated", "{checks:?}");
    assert!(checks[0]["reused_from"].is_null(), "{checks:?}");

    // Each execution record names the tree it ran against, and neither is a
    // reuse of the other's.
    for (candidate_id, tree_is_captured) in [
        (&first_candidate, true),
        (&second_candidate.to_string(), false),
    ] {
        let runs = f.store.check_runs_for_candidate(candidate_id).unwrap();
        assert_eq!(runs.len(), 1, "{candidate_id}: {runs:?}");
        assert_eq!(runs[0].outcome, "passed");
        assert!(runs[0].reused_from_id.is_none());
        let metadata: Value = serde_json::from_str(&runs[0].metadata_json).unwrap();
        let stored = f.store.candidate(candidate_id).unwrap().unwrap();
        assert_eq!(metadata["candidate_tree"], json!(stored.tree_sha));
        assert_eq!(stored.head_sha == captured_head, tree_is_captured);
        assert_eq!(
            stored.tree_sha.as_deref() == Some(captured_tree.as_str()),
            tree_is_captured,
            "the captured tree is pinned to the candidate that was captured"
        );
    }
}

/// Which checks are required, what they run, and how long they get are the
/// operator's. Nothing the agent can reach — a tool argument, a file it
/// commits, a devcontainer or environment file — may remove one, weaken one,
/// or supply its result.
#[tokio::test]
async fn an_agent_cannot_weaken_the_operators_required_checks() {
    state::isolate();
    let f = fixture_with(test_name!(), WITH_GH, |c| {
        c.supervision.checks = vec!["test -f .tracon/allowed || (echo boom >&2; exit 3)".into()];
        c.supervision.timeout_secs = 600;
    })
    .await;
    let wt = std::path::Path::new(&f.worktree);
    // Every repository-side place a check definition could plausibly be read
    // from, committed so it is in the captured tree rather than merely on disk.
    std::fs::create_dir_all(wt.join(".tracon")).unwrap();
    std::fs::create_dir_all(wt.join(".devcontainer")).unwrap();
    std::fs::write(wt.join(".tracon/checks"), "true\n").unwrap();
    std::fs::write(
        wt.join(".tracon/config.toml"),
        "[supervision]\nchecks = []\ntimeout_secs = 1\n",
    )
    .unwrap();
    std::fs::write(
        wt.join("tracon.toml"),
        "[supervision]\nchecks = [\"true\"]\n",
    )
    .unwrap();
    std::fs::write(
        wt.join(".devcontainer/devcontainer.json"),
        r#"{"image":"docker.io/library/busybox:latest","supervision":{"checks":[]}}"#,
    )
    .unwrap();
    std::fs::write(wt.join(".env"), "TRACON_SUPERVISION_CHECKS=true\n").unwrap();
    std::fs::write(wt.join(".envrc"), "export TRACON_SUPERVISION_CHECKS=true\n").unwrap();
    sh(wt, "git add -A && git commit -qm plant-overrides");

    // ...and every argument shape an agent might hope the tool accepts.
    let v = f
        .tool(
            "s1",
            "submit_review",
            json!({
                "title": "feat: the thing", "body": "why", "provider": "github",
                "project": "owner/name", "base": "main",
                "checks": [], "required_checks": [], "skip_checks": true,
                "supervision": { "checks": [], "timeout_secs": 1 },
                "timeout_secs": 1,
                "check_results": [{"command": "test -f .tracon/allowed", "ok": true, "outcome": "passed"}],
                "rerun_checks": true,
            }),
        )
        .await;
    let err = v["error"].as_str().unwrap_or_default().to_string();
    assert!(err.contains("check failed"), "{v}");
    assert!(err.contains("exit 3") && err.contains("boom"), "{v}");
    assert!(f.store.open_reviews().unwrap().is_empty());

    // Exactly the operator's list ran: nothing repo-derived was added to it,
    // and nothing removed from it.
    let candidate = tracon::store::candidate_id(&sh_out(wt, "git rev-parse HEAD"), "work");
    let runs = f.store.check_runs_for_candidate(&candidate).unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].outcome, "failed");
    let definition: Value = serde_json::from_str(&runs[0].definition_json).unwrap();
    assert_eq!(
        definition["command"],
        "test -f .tracon/allowed || (echo boom >&2; exit 3)"
    );
    assert_eq!(definition["timeout_secs"], 600, "the operator's timeout");
}

/// Evidence that names an image and an outcome but not what was run cannot be
/// audited against the configuration that produced it: `command` is the check
/// definition the reuse key is built from, so it belongs on the row.
#[tokio::test]
async fn check_run_evidence_returns_the_configured_command() {
    state::isolate();
    let f = fixture_with(test_name!(), WITH_GH, |c| {
        c.supervision.checks = vec!["sh -c 'test -f a.txt'".into(), "sh -c 'exit 0'".into()];
    })
    .await;
    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let id = v["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();

    let (status, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let checks = body["evidence"]["checks"].as_array().unwrap();
    let commands: Vec<&str> = checks
        .iter()
        .map(|check| {
            check["command"]
                .as_str()
                .unwrap_or_else(|| panic!("every check must name its command: {body}"))
        })
        .collect();
    assert_eq!(commands, ["sh -c 'test -f a.txt'", "sh -c 'exit 0'"]);
    for check in checks {
        assert_eq!(check["outcome"], "passed", "{body}");
        // Exactly the string the definition hash, and so the reuse key, was
        // built from — not a command re-read from today's configuration.
        let definition: Value = serde_json::from_str(check["definition_json"].as_str().unwrap())
            .unwrap_or_else(|_| panic!("{body}"));
        assert_eq!(definition["command"], check["command"], "{body}");
    }
}

#[tokio::test]
async fn a_diff_over_the_cap_is_refused_before_any_check_runs() {
    state::isolate();
    let f = fixture_with(test_name!(), WITH_GH, |c| c.review.max_diff_lines = 0).await;
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
async fn a_diff_over_the_file_cap_is_refused() {
    state::isolate();
    let f = fixture_with(test_name!(), WITH_GH, |c| c.review.max_files = 0).await;
    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let err = v["error"].as_str().unwrap_or_default();
    assert!(err.contains("and 0 files"), "{v}");
    assert!(f.store.open_reviews().unwrap().is_empty());
}

#[tokio::test]
async fn a_bound_review_model_spawns_a_fresh_review_session_whose_verdict_lands_on_the_card() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    // Its content comes from a fresh, frozen workspace snapshotted from the
    // reviewed commit, not from the implementing session's own (still
    // mutable) checkout.
    assert_eq!(rs.repo_path, format!("workspace://review-{id}"));
    // Its worktree is at the reviewed commit, on its own branch, and the
    // session is running before any tool call is made as it.
    for _ in 0..600 {
        let rs = f.store.get_session(&rsid).unwrap().unwrap();
        if rs.worktree_path.is_some() && rs.state == "running" {
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    // And the session says where its work went, not only the review.
    assert!(f.event_kinds("s1").contains(&"published".to_string()));
    let published = f.store.session_publications("s1").unwrap();
    assert_eq!(published.len(), 1, "{published:?}");
    assert_eq!(published[0].0, id);
}

/// An edited diff is a request for changes carrying a patch. What matters is
/// that the bytes survive the round trip intact — a patch is whitespace, and
/// `git apply` calls one with a missing final newline corrupt.
#[tokio::test]
async fn an_edited_diff_reaches_the_agent_and_still_applies() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
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

/// Requirements on the review screen come from what the revision was
/// actually checked against, pinned at submit time — never the live work
/// item, which a human may re-scope while the review sits in the queue.
#[tokio::test]
async fn requirements_shown_on_review_are_pinned_to_the_revision() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let item = tracon::corpus::work::create(
        &f.store,
        &Bus::new(),
        "n1",
        tracon::corpus::work::NewWork {
            channel: "work".into(),
            project_id: None,
            title: "Original requirement".into(),
            body: "do the original thing".into(),
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

    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let id = v["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();

    let (status, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["requirements"]["id"], item.id.as_str());
    assert_eq!(body["requirements"]["title"], "Original requirement");
    assert_eq!(body["requirements"]["body"], "do the original thing");

    // The work item is re-scoped while the review sits in the queue.
    tracon::corpus::work::update(
        &f.store,
        f.manager.bus(),
        "n1",
        &item.id,
        tracon::corpus::work::Patch {
            title: Some("Rewritten requirement".into()),
            body: Some("do something else entirely".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();

    let (status, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["requirements"]["title"], "Original requirement",
        "the pinned revision must not drift with the live work item: {body}"
    );
    assert_eq!(body["requirements"]["body"], "do the original thing");
}

/// A curated demonstration is linked to the document bytes as they stood
/// when it was attached. If the document changes afterward, the review
/// screen must say so rather than silently pointing at today's bytes.
#[tokio::test]
async fn a_demonstration_reports_when_its_document_has_since_changed() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;

    let (status, _) = f
        .call(
            "PUT",
            "/api/docs/work/playbook",
            Some(json!({ "body": "# Playbook\n\noriginal content" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let v = f.tool("s1", "submit_review", f.submit_args()).await;
    let id = v["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();
    let candidate_id = v["candidate_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/evidence/candidates/{candidate_id}/demonstrations"),
            Some(json!({ "slug": "playbook", "label": "How this was verified" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let demos = body["evidence"]["demonstrations"].as_array().unwrap();
    assert_eq!(demos.len(), 1);
    assert_eq!(demos[0]["stale"], false, "{body}");

    // The document changes after the demonstration was attached.
    let (status, _) = f
        .call(
            "PUT",
            "/api/docs/work/playbook",
            Some(json!({ "body": "# Playbook\n\nrewritten content" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let demos = body["evidence"]["demonstrations"].as_array().unwrap();
    assert_eq!(
        demos[0]["stale"], true,
        "the document changed since attachment and must be shown as stale: {body}"
    );
}

#[tokio::test]
async fn a_planted_replace_ref_does_not_change_what_is_reviewed() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let worktree = std::path::PathBuf::from(&f.worktree);
    let head = plant_replace_ref(&worktree);

    let capture = tracon::review::capture(&f.worktree, "main", "feat/x")
        .await
        .unwrap();

    assert_eq!(capture.head_sha, head);
    assert!(capture.diff.contains("+change"), "{}", capture.diff);
    assert!(!capture.diff.contains("poisoned"), "{}", capture.diff);
    // The context excerpts are read by the same hardened Git, so they cannot
    // show the reviewer the replacement's source either.
    assert!(
        !capture.contexts.iter().any(|c| c.text.contains("poisoned")),
        "{:?}",
        capture.contexts
    );
}

#[tokio::test]
async fn a_planted_graft_does_not_change_what_is_reviewed() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let worktree = std::path::PathBuf::from(&f.worktree);
    let head = sh_out(&worktree, "git rev-parse HEAD");
    // A graft that makes HEAD parentless: the branch then shares no history
    // with its base, so an unhardened three-dot diff has no merge base at all
    // and the capture would fail rather than describe the change.
    sh(
        &worktree,
        "mkdir -p .git/info && git rev-parse HEAD > .git/info/grafts",
    );
    assert_eq!(
        sh_out(&worktree, "git rev-list --count HEAD"),
        "1",
        "the planted graft should be live"
    );

    let capture = tracon::review::capture(&f.worktree, "main", "feat/x")
        .await
        .unwrap();

    assert_eq!(capture.head_sha, head);
    assert!(capture.diff.contains("+change"), "{}", capture.diff);
    assert_eq!(capture.added, 1, "{}", capture.diff);
}

#[tokio::test]
async fn a_planted_replace_ref_does_not_change_what_is_pushed() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    // An external review publishes straight from the named worktree, so the
    // planted metadata is still there when publication reads it — nothing but
    // publication's own hardening stands between the replacement and origin.
    f.store
        .insert_session(&{
            let mut r = support::rows::session_row("ext", "n1", "work");
            r.harness_id = "external".into();
            r.repo_path = String::new();
            r.worktree_path = None;
            r.branch = String::new();
            r.started_mono_ms = Some(0);
            r
        })
        .unwrap();
    let id = f
        .submit_as(
            "ext",
            json!({
                "provider": "github", "project": "owner/name",
                "base": "main", "branch": "feat/x", "worktree": f.worktree
            }),
        )
        .await;
    let head = plant_replace_ref(std::path::Path::new(&f.worktree));
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().head_sha, head);

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let origin = f.dir.join("origin.git");
    assert_eq!(sh_out(&origin, "git rev-parse refs/heads/feat/x"), head);
    assert_eq!(
        sh_out(&origin, "git show refs/heads/feat/x:a.txt"),
        "base\nchange"
    );
}

#[tokio::test]
async fn a_planted_graft_does_not_change_what_is_pushed() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    f.store
        .insert_session(&{
            let mut r = support::rows::session_row("ext", "n1", "work");
            r.harness_id = "external".into();
            r.repo_path = String::new();
            r.worktree_path = None;
            r.branch = String::new();
            r.started_mono_ms = Some(0);
            r
        })
        .unwrap();
    let id = f
        .submit_as(
            "ext",
            json!({
                "provider": "github", "project": "owner/name",
                "base": "main", "branch": "feat/x", "worktree": f.worktree
            }),
        )
        .await;
    let worktree = std::path::PathBuf::from(&f.worktree);
    // A graft makes the reviewed commit look parentless, so a bundle built
    // from the grafted reading would carry the tip with none of the history
    // the commit actually names.
    sh(
        &worktree,
        "mkdir -p .git/info && git rev-parse HEAD > .git/info/grafts",
    );
    assert_eq!(sh_out(&worktree, "git rev-list --count HEAD"), "1");

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let origin = f.dir.join("origin.git");
    let head = f.store.get_review(&id).unwrap().unwrap().head_sha;
    assert_eq!(sh_out(&origin, "git rev-parse refs/heads/feat/x"), head);
    assert_eq!(
        sh_out(&origin, "git rev-list --count refs/heads/feat/x"),
        "2"
    );
}

#[tokio::test]
async fn the_exported_snapshot_carries_no_agent_written_git_metadata() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let worktree = std::path::PathBuf::from(&f.worktree);
    let head = plant_replace_ref(&worktree);
    // Everything an agent with write access to its own `.git` can leave for
    // the node — and for the operator who downloads the same snapshot.
    sh(
        &worktree,
        "mkdir -p .git/hooks .git/info .git/worktrees/w1 \
         && printf '#!/bin/sh\\ntouch /pwned\\n' > .git/hooks/pre-commit \
         && printf '[core]\\n\\tsshCommand = touch /pwned\\n' > .git/worktrees/w1/config.worktree \
         && printf '/elsewhere.git\\n' > .git/commondir \
         && printf 'deadbeef\\n' > .git/info/grafts \
         && printf '[credential]\\n\\thelper = !touch /pwned\\n' > .git/config",
    );

    // The snapshot every node-side read of this workspace goes through.
    let snapshot = f.manager.snapshot_workspace("s1").await.unwrap();

    for gone in [
        ".git/commondir",
        ".git/hooks",
        ".git/info/grafts",
        ".git/refs/replace",
        ".git/worktrees/w1/config.worktree",
    ] {
        assert!(!snapshot.join(gone).exists(), "{gone} reached the snapshot");
    }
    let config = std::fs::read_to_string(snapshot.join(".git/config")).unwrap();
    assert!(!config.contains("helper"), "{config}");
    // Objects and ordinary refs are identity and must survive — and the
    // snapshot resolves at all, which the planted `commondir` alone is enough
    // to prevent in the workspace it came from.
    assert!(snapshot.join(".git/refs/heads").exists());
    assert_eq!(sh_out(&snapshot, "git rev-parse HEAD"), head);
}

#[tokio::test]
async fn publication_refuses_a_candidate_whose_tree_is_not_the_reviewed_one() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    // The evidence the node recorded when it captured the review, standing in
    // for a candidate whose commit still resolves but whose tree is not the
    // one that was read. Recorded before the submission, because a candidate
    // is immutable: the capture that follows cannot overwrite it.
    let head = sh_out(std::path::Path::new(&f.worktree), "git rev-parse HEAD");
    f.store
        .insert_candidate(&tracon::store::CandidateRow {
            id: tracon::store::candidate_id(&head, "work"),
            head_sha: head.clone(),
            tree_sha: Some("0".repeat(40)),
            channel: "work".into(),
            owner_session_id: Some("s1".into()),
            source_kind: "git".into(),
            captured_ms: now_ms(),
            capture_json: "{}".into(),
        })
        .unwrap();
    let id = f.submit().await;

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("reviewed tree"),
        "{body}"
    );
    assert!(
        !f.gh_log().contains("pr create"),
        "nothing may be published"
    );
}

#[tokio::test]
async fn publication_proceeds_when_the_recorded_tree_is_the_one_being_pushed() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = f.submit().await;
    let review = f.store.get_review(&id).unwrap().unwrap();
    let tree = sh_out(
        std::path::Path::new(&f.worktree),
        "git rev-parse 'HEAD^{tree}'",
    );
    f.store
        .insert_candidate(&tracon::store::CandidateRow {
            id: tracon::store::candidate_id(&review.head_sha, "work"),
            head_sha: review.head_sha.clone(),
            tree_sha: Some(tree),
            channel: "work".into(),
            owner_session_id: Some("s1".into()),
            source_kind: "git".into(),
            captured_ms: now_ms(),
            capture_json: "{}".into(),
        })
        .unwrap();

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;

    // The check binds identity; it does not simply refuse everything.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        sh_out(&f.dir.join("origin.git"), "git rev-parse refs/heads/feat/x"),
        review.head_sha,
    );
}

// ---- interrupted publication ----
//
// Publication has two side effects this node cannot take back: the push, and
// the opened change. A node can die between them, or between one of them and
// the record of it. What follows is each of those crashes, arranged as the
// node would find them on the next attempt: a record another process left in
// flight, and a forge that already holds part of the work.

/// A `gh` that also answers `pr list`, which is how a resumed attempt finds a
/// change it may already have opened. What it lists comes from a file the
/// test writes, so the test decides what the forge holds.
fn gh_that_lists(f: &Fixture, listing: &str) {
    std::fs::write(f.dir.join("pr-list.json"), listing).unwrap();
    std::fs::write(
        f.dir.join("bin/gh"),
        "#!/bin/sh\n\
         { echo \"ARGS: $*\"; echo \"GH_TOKEN=$GH_TOKEN\"; } >> \"$(dirname \"$0\")/../gh.log\"\n\
         case \"$2\" in\n\
         list) cat \"$(dirname \"$0\")/../pr-list.json\" ;;\n\
         *) echo https://github.test/pull/1 ;;\n\
         esac\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f.dir.join("bin/gh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
}

/// A git that reports every push as a success without performing it: a forge
/// that accepts a push and does not keep it, which is indistinguishable from
/// the client's side until the ref is read back.
fn git_that_swallows_pushes(f: &Fixture) {
    let script = format!(
        "#!/bin/bash\n\
         args=(\"$@\")\n\
         echo \"$*\" >> \"$(dirname \"$0\")/../git.log\"\n\
         for a in \"${{args[@]}}\"; do if [ \"$a\" = push ]; then exit 0; fi; done\n\
         n=${{#args[@]}}\n\
         if [ \"$n\" -ge 4 ] \\\n\
         \t&& [ \"${{args[$((n-4))]}}\" = remote ] \\\n\
         \t&& [ \"${{args[$((n-3))]}}\" = add ] \\\n\
         \t&& [ \"${{args[$((n-2))]}}\" = origin ]; then\n\
         \targs[$((n-1))]='{origin}'\n\
         fi\n\
         exec git \"${{args[@]}}\"\n",
        origin = f.dir.join("origin.git").to_string_lossy(),
    );
    let path = f.dir.join("bin/git-remote-stub");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The crash between a successful push and the record of it. The branch is on
/// the forge; the node does not know. It must not push again, and above all
/// must not report a failure of something that already succeeded.
#[tokio::test]
async fn a_publication_interrupted_after_its_push_resumes_without_pushing_again() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = f.submit().await;
    let head = f.store.get_review(&id).unwrap().unwrap().head_sha;
    // What the interrupted attempt had already done.
    sh(
        &f.dir.join("wt"),
        &format!("git push -q origin {head}:refs/heads/feat/x"),
    );
    // The forge holds the branch but no change: the crash landed between the
    // push and the record of it.
    gh_that_lists(&f, "[]");
    f.interrupted_attempt(&id, "pending", None);
    f.forget_logs();

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["published"], "https://github.test/pull/1");

    let git = f.git_log();
    assert!(
        git.contains("ls-remote"),
        "the forge must be asked what the branch holds: {git}"
    );
    assert!(
        !git.contains("push origin"),
        "the push must not be repeated: {git}"
    );
    assert_eq!(
        f.gh_log().matches("pr create").count(),
        1,
        "the change is opened exactly once"
    );
    let record = f.publication(&id);
    assert_eq!(record.state, "opened");
    assert_eq!(record.pushed_sha.as_deref(), Some(head.as_str()));
    assert_eq!(
        f.store.get_review(&id).unwrap().unwrap().state,
        "approved",
        "the review is resolved, not left mid-publish"
    );
}

/// The crash inside the call that opens the change. A pull request may exist
/// with nothing recorded about it. The resumed attempt recognises it by the
/// reviewed commit and the approved text, and records that one rather than
/// opening a second.
#[tokio::test]
async fn a_publication_interrupted_while_opening_records_the_change_it_already_opened() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = f.submit().await;
    let head = f.store.get_review(&id).unwrap().unwrap().head_sha;
    sh(
        &f.dir.join("wt"),
        &format!("git push -q origin {head}:refs/heads/feat/x"),
    );
    gh_that_lists(
        &f,
        &json!([
            // An older change from the same branch is not this one.
            { "url": "https://github.test/pull/3", "title": "feat: the thing", "body": "what the diff does not say",
              "headRefOid": "0000000", "baseRefName": "main", "isCrossRepository": false },
            { "url": "https://github.test/pull/7", "title": "feat: the thing", "body": "what the diff does not say\r\n",
              "headRefOid": head, "baseRefName": "main", "isCrossRepository": false },
        ])
        .to_string(),
    );
    f.interrupted_attempt(&id, "opening", Some(&head));
    f.forget_logs();

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["published"], "https://github.test/pull/7",
        "the change that exists is the one reported"
    );
    let gh = f.gh_log();
    assert!(gh.contains("pr list"), "{gh}");
    assert!(
        !gh.contains("pr create"),
        "a second pull request must never be opened: {gh}"
    );
    let record = f.publication(&id);
    assert_eq!(record.state, "opened");
    assert_eq!(record.result.as_deref(), Some("https://github.test/pull/7"));
}

/// A forge that cannot be reached during recovery settles nothing. The node
/// says so, keeps the claim, and leaves a record naming what to verify —
/// rather than reporting a failure or pushing into the dark.
#[tokio::test]
async fn a_forge_that_cannot_be_reached_leaves_the_outcome_explicitly_unknown() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = f.submit().await;
    let head = f.store.get_review(&id).unwrap().unwrap().head_sha;
    sh(
        &f.dir.join("wt"),
        &format!("git push -q origin {head}:refs/heads/feat/x"),
    );
    f.interrupted_attempt(&id, "opening", Some(&head));
    // The forge goes away between the crash and the recovery.
    std::fs::rename(f.dir.join("origin.git"), f.dir.join("origin.gone")).unwrap();
    f.forget_logs();

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let message = body["error"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("verify"), "{message}");

    let record = f.publication(&id);
    assert_eq!(record.state, "uncertain");
    assert!(record.note.unwrap_or_default().contains("verify"));
    assert!(
        !f.gh_log().contains("pr create"),
        "nothing may be opened on a forge whose state is unknown"
    );
    assert_eq!(
        f.store.get_review(&id).unwrap().unwrap().state,
        "publishing",
        "an unknown outcome keeps the claim rather than inviting a blind retry"
    );
}

/// A push the forge reports as accepted but does not hold. Reading the ref
/// back is the only way to know, and the mismatch is reported rather than
/// papered over with a pull request against a commit that is not there.
#[tokio::test]
async fn a_push_the_forge_did_not_keep_is_reported() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = f.submit().await;
    git_that_swallows_pushes(&f);
    f.forget_logs();

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let message = body["error"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("feat/x"), "{message}");
    assert!(message.contains("not the reviewed commit"), "{message}");

    assert!(
        !f.gh_log().contains("pr create"),
        "nothing is opened for a commit the forge does not hold"
    );
    let record = f.publication(&id);
    assert_eq!(record.state, "failed");
    assert!(record.pushed_sha.is_none());
    assert_eq!(
        f.store.get_review(&id).unwrap().unwrap().state,
        "claimed",
        "a settled failure returns the review to the queue"
    );
}

/// A publication already recorded as open is reported, not opened again.
#[tokio::test]
async fn an_opened_publication_is_reported_rather_than_opened_again() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = f.submit().await;
    let head = f.store.get_review(&id).unwrap().unwrap().head_sha;
    f.interrupted_attempt(&id, "opening", Some(&head));
    f.store
        .publication_opened(&f.publication_id(&id), "https://github.test/pull/9")
        .unwrap();
    f.forget_logs();

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["published"], "https://github.test/pull/9");
    assert!(f.gh_log().is_empty(), "the forge was not touched at all");
    assert!(f.git_log().is_empty(), "and neither was git");
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "approved");
}

/// The agent resubmits while the operator is deciding.
///
/// An approval is of particular bytes, not of a review id. The verdict names
/// the commit the operator was reading; when a resubmission has replaced it,
/// the approval is refused rather than quietly published as the new revision,
/// and the claim itself is bound to the revision that was decided so nothing
/// that arrives later can be attributed to it.
#[tokio::test]
async fn an_approval_is_refused_when_a_resubmission_replaced_what_was_reviewed() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let submitted = f.tool("s1", "submit_review", f.submit_args()).await;
    let id = submitted["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{submitted}"))
        .to_string();
    let reviewed = f.store.get_review(&id).unwrap().unwrap().head_sha;
    let first_revision = f
        .store
        .latest_review_revision(&id)
        .unwrap()
        .expect("a submission records its revision");

    // While the operator reads that diff, the agent commits more and
    // resubmits onto the same card.
    sh(
        std::path::Path::new(&f.worktree),
        "echo more >> a.txt && git add -A && git commit -qm second",
    );
    let mut resubmit = f.submit_args();
    resubmit["review_id"] = json!(id);
    let again = f.tool("s1", "submit_review", resubmit).await;
    assert_eq!(again["state"], "new", "{again}");
    let latest = f.store.get_review(&id).unwrap().unwrap().head_sha;
    assert_ne!(latest, reviewed, "the review moved to a new revision");

    // The verdict the operator wrote names what they read.
    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve", "head_sha": reviewed })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("moved to a new revision"),
        "{body}"
    );
    assert_eq!(
        f.store.get_review(&id).unwrap().unwrap().state,
        "new",
        "a refused approval decides nothing"
    );
    assert!(
        !f.gh_log().contains("pr create"),
        "and publishes nothing: {}",
        f.gh_log()
    );

    // The claim underneath is bound the same way: the revision the operator
    // decided on is no longer the review's, so it cannot be claimed at all.
    assert!(
        !f.store
            .begin_publish(&id, Some(&first_revision.id))
            .unwrap(),
        "a publish claim for a superseded revision must lose"
    );

    // Deciding the revision that is actually current works, and publishes
    // exactly it.
    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve", "head_sha": latest })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "approved");
    let decisions = f.store.review_decisions(&id).unwrap();
    let decided = decisions.last().expect("a decision was recorded");
    assert_eq!(
        decided.revision_id,
        f.store.latest_review_revision(&id).unwrap().unwrap().id,
        "the decision is recorded against the revision it was made on"
    );
    assert_ne!(decided.revision_id, first_revision.id);
}

/// The agent resubmits the same commit with different prose and different
/// requirements while the operator's tab is open.
///
/// Nothing about the code moved, so `head_sha` cannot tell the two readings
/// apart — but the operator was shown a different title, a different
/// description and a different pinned requirement, and never read the ones
/// now on the card. A verdict is bound to the revision that was inspected,
/// not to the commit underneath it, so every verdict from the stale tab is
/// refused and the review is left waiting on a fresh reading.
#[tokio::test]
async fn a_same_commit_resubmission_with_new_prose_requires_a_fresh_reading() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let item = tracon::corpus::work::create(
        &f.store,
        &Bus::new(),
        "n1",
        tracon::corpus::work::NewWork {
            channel: "work".into(),
            project_id: None,
            title: "Original requirement".into(),
            body: "do the original thing".into(),
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

    let submitted = f.tool("s1", "submit_review", f.submit_args()).await;
    let id = submitted["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{submitted}"))
        .to_string();

    // What the operator's tab is showing: the screen names its own revision,
    // so a verdict written from it can say which reading it was.
    let (status, screen) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{screen}");
    let read_revision = screen["revision"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the review screen must name its revision: {screen}"))
        .to_string();
    let read_sha = screen["revision"]["head_sha"].as_str().unwrap().to_string();
    assert_eq!(screen["requirements"]["title"], "Original requirement");

    // The work item is re-scoped and the agent resubmits the very same
    // commit under rewritten prose.
    tracon::corpus::work::update(
        &f.store,
        f.manager.bus(),
        "n1",
        &item.id,
        tracon::corpus::work::Patch {
            title: Some("Rewritten requirement".into()),
            body: Some("do something else entirely".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let mut resubmit = f.submit_args();
    resubmit["review_id"] = json!(id);
    resubmit["title"] = json!("feat: something else entirely");
    resubmit["body"] = json!("a different case for a different change");
    let again = f.tool("s1", "submit_review", resubmit).await;
    assert_eq!(again["state"], "new", "{again}");

    let current = f
        .store
        .latest_review_revision(&id)
        .unwrap()
        .expect("the resubmission recorded its own revision");
    assert_eq!(
        current.head_sha, read_sha,
        "the commit is deliberately unchanged: it is what cannot tell these apart"
    );
    assert_ne!(
        current.id, read_revision,
        "but the revision did move, and that is what a verdict is bound to"
    );

    // Every verdict the stale tab can write is refused, including the two
    // that never reach publication.
    for verdict in [
        json!({ "verdict": "approve" }),
        json!({ "verdict": "reject", "reason": "not this" }),
        json!({ "verdict": "revise", "reason": "change this" }),
    ] {
        let mut body = verdict.clone();
        body["head_sha"] = json!(read_sha);
        body["revision_id"] = json!(read_revision);
        let (status, answer) = f
            .call("POST", &format!("/api/reviews/{id}/verdict"), Some(body))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{verdict}: {answer}");
        assert!(
            answer["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("moved to a new revision"),
            "{verdict}: {answer}"
        );
        assert_eq!(
            f.store.get_review(&id).unwrap().unwrap().state,
            "new",
            "a refused verdict decides nothing: {verdict}"
        );
    }
    assert!(
        f.store.review_decisions(&id).unwrap().is_empty(),
        "and records no decision against either revision"
    );
    assert!(
        !f.gh_log().contains("pr create"),
        "and publishes nothing: {}",
        f.gh_log()
    );

    // Reading the card again and deciding on what it now says works.
    let (status, screen) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{screen}");
    assert_eq!(screen["revision"]["id"], current.id.as_str());
    assert_eq!(screen["requirements"]["title"], "Rewritten requirement");
    let (status, answer) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({
                "verdict": "revise", "reason": "now that I have read it",
                "head_sha": read_sha, "revision_id": current.id,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let decided = f.store.review_decisions(&id).unwrap();
    assert_eq!(decided.len(), 1);
    assert_eq!(
        decided[0].revision_id, current.id,
        "the decision is recorded against the revision that was actually read"
    );
}

/// The resubmission lands after the node has read the review and before it
/// writes the verdict down.
///
/// The check that the operator's reading is still current cannot be a read
/// followed by a trusting write: the store compares the decided revision
/// inside the statement that records the decision, so a revision that arrives
/// in that window loses rather than inheriting a verdict decided against the
/// one it replaced.
#[tokio::test]
async fn a_verdict_is_compared_against_its_revision_as_it_is_recorded() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let submitted = f.tool("s1", "submit_review", f.submit_args()).await;
    let id = submitted["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{submitted}"))
        .to_string();
    let read = f.store.latest_review_revision(&id).unwrap().unwrap();

    let mut resubmit = f.submit_args();
    resubmit["review_id"] = json!(id);
    resubmit["title"] = json!("feat: rewritten while the verdict was in flight");
    f.tool("s1", "submit_review", resubmit).await;
    let current = f.store.latest_review_revision(&id).unwrap().unwrap();
    assert_ne!(current.id, read.id);

    // Straight at the store, as the write itself sees it.
    assert!(
        !f.store
            .request_changes(&id, "note", None, Some(&read.id))
            .unwrap(),
        "a request for changes bound to a superseded revision must not land"
    );
    assert!(
        !f.store
            .resolve_review(
                &id,
                "rejected",
                Some("no"),
                None,
                None,
                None,
                0,
                Some(&read.id)
            )
            .unwrap(),
        "nor a rejection"
    );
    assert_eq!(
        f.store.get_review(&id).unwrap().unwrap().state,
        "new",
        "and neither changed the row"
    );
    assert!(
        f.store
            .request_changes(&id, "note", None, Some(&current.id))
            .unwrap(),
        "the revision that is actually current decides"
    );
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "revising");
}

/// Whether a process id still names something on this host. `kill -0` is the
/// portable ask; shelling out keeps it working on macOS as well as Linux.
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Poll until `f` holds, or give up after roughly `secs`. Checks run as real
/// subprocesses, so there is nothing to await on them from here.
async fn until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..(secs * 50) {
        if f() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    f()
}

/// The operator stops a session while its required checks are executing.
///
/// Three things have to hold together: the process the node started is really
/// gone — its whole group, so a command that backgrounded work does not
/// survive its own cancellation — the evidence row says `cancelled` rather
/// than inventing a pass or a failure, and a result arriving afterwards
/// cannot overwrite what the cancellation recorded.
#[tokio::test]
async fn stopping_a_session_mid_check_cancels_the_run_and_kills_its_process_group() {
    state::isolate();
    let f = fixture_with(test_name!(), WITH_GH, |c| {
        // A backgrounded grandchild in the same process group, whose pid the
        // test can watch. Killing only the direct child would leave it
        // running, reparented and alive; killing the group takes it too.
        // Written relative to the check's own workdir, which is the scratch
        // copy of the candidate the runner mounts at /work.
        c.supervision.checks = vec!["sleep 30 & echo $! > child.pid; wait".into()];
    })
    .await;
    let f = Arc::new(f);

    let submitting = tokio::spawn({
        let f = f.clone();
        async move { f.tool("s1", "submit_review", f.submit_args()).await }
    });

    // Wait until the check is genuinely running: the session says so, and the
    // command has written the pid of the work it backgrounded.
    let candidate = {
        let head = sh_out(std::path::Path::new(&f.worktree), "git rev-parse HEAD");
        tracon::store::candidate_id(&head, "work")
    };
    let pid_file = |f: &Fixture| -> Option<String> {
        let run = f.store.check_runs_for_candidate(&candidate).ok()?.pop()?;
        let volume = format!("tracon-check-{}", run.id);
        std::fs::read_to_string(
            tracon::runner::local::local_runtime_path(&volume).join("child.pid"),
        )
        .ok()
        .map(|pid| pid.trim().to_string())
        .filter(|pid| !pid.is_empty())
    };
    assert!(
        until(10, || pid_file(&f).is_some()).await,
        "the check never started; the session is {:?}",
        f.store.get_session("s1").unwrap().unwrap().state
    );
    let pid = pid_file(&f).unwrap();
    assert!(
        alive(&pid),
        "the test is watching a process that is running"
    );
    assert_eq!(
        f.store.get_session("s1").unwrap().unwrap().state,
        "waiting_on_check"
    );

    // The operator stops the session while the check is executing.
    f.manager.stop("s1").await.unwrap();

    let submitted = submitting.await.unwrap();
    let error = submitted["error"].as_str().unwrap_or_default().to_string();
    assert!(
        error.contains("cancelled"),
        "the submission reports the cancellation rather than a verdict: {submitted}"
    );
    assert!(
        f.store.open_reviews().unwrap().is_empty(),
        "a cancelled run opens no review"
    );

    // The process group the node started is gone.
    assert!(
        until(10, || !alive(&pid)).await,
        "the check's process group outlived its cancellation (pid {pid})"
    );

    // The evidence says cancelled — not passed, not failed, not still
    // running — so nothing can later read it as proof.
    let runs = f.store.check_runs_for_candidate(&candidate).unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].outcome, "cancelled");
    let kinds = f.event_kinds("s1");
    assert!(kinds.contains(&"check_cancelled".to_string()), "{kinds:?}");
    assert!(
        !kinds.contains(&"candidate_verified".to_string()),
        "a cancelled run verifies nothing: {kinds:?}"
    );

    // A result that arrives after the cancellation is dropped rather than
    // becoming the outcome.
    assert!(
        !f.store
            .finish_check_run(
                &runs[0].id,
                "passed",
                None,
                Some(0),
                "late",
                Some(1),
                &json!({}),
            )
            .unwrap(),
        "a late result must not be accepted"
    );
    assert_eq!(
        f.store.check_run(&runs[0].id).unwrap().unwrap().outcome,
        "cancelled",
        "and must not overwrite what the cancellation recorded"
    );

    // The session the operator stopped stays stopped: the check finishing
    // afterwards does not put it back into `running`.
    let session = f.store.get_session("s1").unwrap().unwrap();
    assert_eq!(session.state, "closed");
    assert_eq!(session.end_reason.as_deref(), Some("killed_user"));
    assert!(
        kinds.contains(&"late_refused".to_string()),
        "the refused transition is recorded: {kinds:?}"
    );
}

// ---- updating an existing change ----

/// A `gh` that answers the REST calls publication makes for an existing
/// change. The pull request is `pr.json`, the open changes for a branch are
/// `open.json`, and its comments are `comments.json`, all files the test
/// writes: the test decides what the forge holds. Branches are read from, and
/// a new change's base must exist in, the fixture's bare `origin.git`.
fn gh_forge(f: &Fixture, pull: Value) {
    std::fs::write(f.dir.join("pr.json"), pull.to_string()).unwrap();
    std::fs::write(
        f.dir.join("bin/gh"),
        "#!/bin/sh\n\
         d=\"$(dirname \"$0\")/..\"\n\
         { echo \"ARGS: $*\"; echo \"GH_TOKEN=$GH_TOKEN\"; } >> \"$d/gh.log\"\n\
         if [ \"$1\" = api ]; then\n\
         case \"$3 $4\" in\n\
         'GET repos/owner/name/pulls?'*) cat \"$d/open.json\" 2>/dev/null || echo '[]' ;;\n\
         'GET repos/owner/name/pulls/'*) cat \"$d/pr.json\" ;;\n\
         'GET repos/owner/name/issues/'*) cat \"$d/comments.json\" 2>/dev/null || echo '[]' ;;\n\
         'GET repos/owner/name/git/ref/heads/'*)\n\
         b=\"${4#repos/owner/name/git/ref/heads/}\"\n\
         if s=\"$(git --git-dir=\"$d/origin.git\" rev-parse --verify -q \"refs/heads/$b\")\"; then\n\
         printf '{\"object\":{\"sha\":\"%s\"}}\\n' \"$s\"\n\
         else echo 'gh: Not Found (HTTP 404)' >&2; exit 1; fi ;;\n\
         *) echo '{}' ;;\n\
         esac\n\
         exit 0\n\
         fi\n\
         base=; prev=\n\
         for a in \"$@\"; do [ \"$prev\" = --base ] && base=\"$a\"; prev=\"$a\"; done\n\
         if [ -n \"$base\" ] && ! git --git-dir=\"$d/origin.git\" rev-parse --verify -q \"refs/heads/$base\" >/dev/null; then\n\
         echo 'pull request create failed: GraphQL: Base ref must be a branch' >&2; exit 1\n\
         fi\n\
         echo https://github.test/pull/1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f.dir.join("bin/gh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
}

/// Pull request 7 for `feat/x` into `main`, whose branch holds `head`.
fn pull_seven(head: &str) -> Value {
    json!({
        "html_url": "https://github.test/pull/7",
        "state": "open",
        "head": { "ref": "feat/x", "sha": head, "repo": { "full_name": "owner/name" } },
        "base": { "ref": "main" },
    })
}

impl Fixture {
    /// The commit the session's branch was cut from.
    fn base_sha(&self) -> String {
        sh_out(std::path::Path::new(&self.worktree), "git rev-parse main")
    }

    /// What the forge's `feat/x` holds.
    fn forge_branch(&self) -> String {
        sh_out(
            &self.dir,
            "git --git-dir=origin.git rev-parse refs/heads/feat/x",
        )
    }

    /// Make the forge's `feat/x` hold `sha`, as a push from elsewhere would.
    fn forge_holds(&self, sha: &str) {
        sh(
            std::path::Path::new(&self.worktree),
            &format!(
                "git push -qf {} {sha}:refs/heads/feat/x",
                self.dir.join("origin.git").display()
            ),
        );
    }

    async fn submit_update(&self, extra: Value) -> Value {
        let mut args = self.submit_args();
        args["change"] = json!(7);
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        self.tool("s1", "submit_review", args).await
    }

    async fn approve(&self, id: &str) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve" })),
        )
        .await
    }
}

fn review_id(submitted: &Value) -> String {
    submitted["review_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{submitted}"))
        .to_string()
}

/// The branch already has a pull request. Approval pushes to it, replaces its
/// description, and comments — and never opens a second one.
#[tokio::test]
async fn an_update_pushes_to_the_open_change_and_sends_what_was_asked() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let base = f.base_sha();
    f.forge_holds(&base);
    gh_forge(&f, pull_seven(&base));

    let submitted = f
        .submit_update(json!({ "forge": {
            "description": { "title": "feat: the thing, revised", "body": "now with more" },
            "comment": "This revision handles the edge case.",
        }}))
        .await;
    let id = review_id(&submitted);
    let review = f.store.get_review(&id).unwrap().unwrap();
    let target: tracon::review::publish::Target = serde_json::from_str(&review.target).unwrap();
    let change = target
        .change
        .expect("the review names the change it updates");
    assert_eq!(change.number, 7);
    assert_eq!(change.url, "https://github.test/pull/7");
    f.forget_logs();

    let (status, body) = f.approve(&id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["published"], "https://github.test/pull/7");
    assert_eq!(
        f.forge_branch(),
        review.head_sha,
        "the change's branch holds the reviewed commit"
    );

    let gh = f.gh_log();
    assert!(
        !gh.contains("pr create"),
        "no second change is opened: {gh}"
    );
    assert!(gh.contains("api -X PATCH repos/owner/name/pulls/7"), "{gh}");
    assert!(gh.contains("title=feat: the thing, revised"), "{gh}");
    assert!(
        gh.contains("api -X POST repos/owner/name/issues/7/comments"),
        "{gh}"
    );
    assert!(gh.contains("This revision handles the edge case."), "{gh}");
    assert!(
        !f.git_log().contains("--force"),
        "a fast-forward is never forced: {}",
        f.git_log()
    );
    let decision = f.store.review_decisions(&id).unwrap().pop().unwrap();
    let outputs: tracon::review::publish::Outputs =
        serde_json::from_str(decision.outputs_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        outputs.comment.as_deref(),
        Some("This revision handles the edge case.")
    );
}

/// An update that asks for nothing on the forge only pushes: the review's
/// title and body are the operator's, not the change's.
#[tokio::test]
async fn an_update_without_forge_outputs_only_pushes() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let base = f.base_sha();
    f.forge_holds(&base);
    gh_forge(&f, pull_seven(&base));
    let id = review_id(&f.submit_update(json!({})).await);
    f.forget_logs();

    let (status, body) = f.approve(&id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let gh = f.gh_log();
    assert!(!gh.contains("PATCH"), "{gh}");
    assert!(!gh.contains("comments"), "{gh}");
    assert!(!gh.contains("pr create"), "{gh}");
    assert_eq!(
        f.forge_branch(),
        f.store.get_review(&id).unwrap().unwrap().head_sha
    );
}

/// The operator edits what goes to the forge; the edit is what is sent.
#[tokio::test]
async fn the_operators_edited_outputs_are_what_the_forge_receives() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let base = f.base_sha();
    f.forge_holds(&base);
    gh_forge(&f, pull_seven(&base));
    let id = review_id(
        &f.submit_update(json!({ "forge": { "comment": "agent's wording" } }))
            .await,
    );
    f.forget_logs();

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve", "outputs": { "comment": "operator's wording" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let gh = f.gh_log();
    assert!(gh.contains("operator's wording"), "{gh}");
    assert!(!gh.contains("agent's wording"), "{gh}");
}

/// A commit already on the change is a prose-only update: nothing is pushed,
/// and the comment still lands.
#[tokio::test]
async fn a_prose_only_update_pushes_nothing() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let head = sh_out(std::path::Path::new(&f.worktree), "git rev-parse HEAD");
    f.forge_holds(&head);
    gh_forge(&f, pull_seven(&head));
    let id = review_id(
        &f.submit_update(json!({ "forge": { "comment": "ready again" } }))
            .await,
    );
    f.forget_logs();

    let (status, body) = f.approve(&id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!f.git_log().contains("push origin"), "{}", f.git_log());
    let gh = f.gh_log();
    assert!(gh.contains("body=ready again\n"), "{gh}");
    assert!(!gh.contains("tracon-publication"), "{gh}");
}

/// A resumed publication finds the comment it already posted by its text and
/// does not post it twice.
#[tokio::test]
async fn a_resumed_update_does_not_comment_twice() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let base = f.base_sha();
    f.forge_holds(&base);
    gh_forge(&f, pull_seven(&base));
    let id = review_id(
        &f.submit_update(json!({ "forge": { "comment": "once" } }))
            .await,
    );
    let review = f.store.get_review(&id).unwrap().unwrap();
    let revision = f.store.latest_review_revision(&id).unwrap().unwrap().id;
    // The interrupted attempt pushed and commented, then the node died.
    f.forge_holds(&review.head_sha);
    let publication = f.publication_id(&id);
    std::fs::write(
        f.dir.join("comments.json"),
        json!([{ "body": "once" }]).to_string(),
    )
    .unwrap();
    f.store
        .publication_begin(&tracon::store::PublicationBegin {
            id: &publication,
            review_id: &id,
            revision_id: Some(&revision),
            candidate_id: &tracon::store::candidate_id(&review.head_sha, "work"),
            channel: "work",
            node_id: "n1",
            provider: "github",
            project: "owner/name",
            base: "main",
            branch: "feat/x",
            head_sha: &review.head_sha,
            instance: "a-process-that-is-no-longer-running",
        })
        .unwrap();
    f.store
        .publication_pushed(&publication, &review.head_sha)
        .unwrap();
    f.store.publication_opening(&publication).unwrap();
    assert!(f.store.begin_publish(&id, Some(&revision)).unwrap());
    f.forget_logs();

    let (status, body) = f.approve(&id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let gh = f.gh_log();
    assert!(
        gh.contains("GET repos/owner/name/issues/7/comments"),
        "{gh}"
    );
    assert!(
        !gh.contains("POST"),
        "the comment is not posted again: {gh}"
    );
    assert_eq!(f.publication(&id).state, "opened");
}

/// A branch that already has an open change cannot silently get a second
/// one: submit says which change to name.
#[tokio::test]
async fn submitting_without_the_open_change_is_refused_with_its_number() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    gh_forge(&f, pull_seven(&f.base_sha()));
    std::fs::write(
        f.dir.join("open.json"),
        json!([{ "number": 7, "html_url": "https://github.test/pull/7" }]).to_string(),
    )
    .unwrap();
    let submitted = f.tool("s1", "submit_review", f.submit_args()).await;
    let error = submitted["error"].to_string();
    assert!(error.contains("pass change: 7"), "{submitted}");
}

/// Only an open change of this branch, in this repository, into this base can
/// be updated.
#[tokio::test]
async fn a_closed_or_foreign_change_is_refused_at_submit() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let base = f.base_sha();
    for (pull, expected) in [
        (
            json!({ "state": "closed", "head": { "ref": "feat/x", "sha": base,
                    "repo": { "full_name": "owner/name" } }, "base": { "ref": "main" } }),
            "is not open",
        ),
        (
            json!({ "state": "open", "head": { "ref": "feat/x", "sha": base,
                    "repo": { "full_name": "someone/fork" } }, "base": { "ref": "main" } }),
            "another repository",
        ),
        (
            json!({ "state": "open", "head": { "ref": "feat/y", "sha": base,
                    "repo": { "full_name": "owner/name" } }, "base": { "ref": "main" } }),
            "is for branch feat/y",
        ),
    ] {
        gh_forge(&f, pull);
        let submitted = f.submit_update(json!({})).await;
        assert!(
            submitted["error"].to_string().contains(expected),
            "{submitted}"
        );
    }
}

/// History the change holds that this branch does not build on is replaced
/// only when the agent says so, and then only over exactly that commit.
#[tokio::test]
async fn a_rewrite_must_be_asked_for_and_is_forced_only_over_its_lease() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    // The change's branch holds a commit this worktree's branch does not
    // descend from: its history was rewritten since.
    let host = f.dir.join("wt");
    sh(
        &host,
        "git checkout -qb elsewhere main && echo other > b.txt && git add -A \
         && git commit -qm other && git push -qf origin elsewhere:refs/heads/feat/x",
    );
    let lease = sh_out(&host, "git rev-parse elsewhere");
    gh_forge(&f, pull_seven(&lease));

    let refused = f.submit_update(json!({})).await;
    assert!(
        refused["error"].to_string().contains("rewrite: true"),
        "{refused}"
    );

    let id = review_id(&f.submit_update(json!({ "rewrite": true })).await);
    let revision = f.store.latest_review_revision(&id).unwrap().unwrap();
    let intent: tracon::review::publish::Intent =
        serde_json::from_str(revision.intent_json.as_deref().unwrap()).unwrap();
    assert!(intent.rewrite);
    assert_eq!(intent.lease.as_deref(), Some(lease.as_str()));
    f.forget_logs();

    let (status, body) = f.approve(&id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        f.git_log()
            .contains(&format!("--force-with-lease=refs/heads/feat/x:{lease}")),
        "{}",
        f.git_log()
    );
    assert_eq!(
        f.forge_branch(),
        f.store.get_review(&id).unwrap().unwrap().head_sha
    );
}

/// Somebody pushed to the change after this revision was submitted. The push
/// would throw their work away, so publication refuses and says why.
#[tokio::test]
async fn an_update_whose_lease_was_lost_is_refused() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let base = f.base_sha();
    f.forge_holds(&base);
    gh_forge(&f, pull_seven(&base));
    let id = review_id(&f.submit_update(json!({})).await);
    let host = f.dir.join("wt");
    sh(
        &host,
        "git checkout -qb theirs main && echo theirs > c.txt && git add -A \
         && git commit -qm theirs && git push -qf origin theirs:refs/heads/feat/x",
    );
    let theirs = sh_out(&host, "git rev-parse theirs");
    f.forget_logs();

    let (status, body) = f.approve(&id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("somebody else pushed"),
        "{body}"
    );
    assert!(!f.git_log().contains("push origin"), "{}", f.git_log());
    assert_eq!(f.forge_branch(), theirs, "their work is still there");
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "claimed");
}

/// What a revision's intent says it may replace on the forge.
fn intent(f: &Fixture, id: &str) -> tracon::review::publish::Intent {
    let revision = f.store.latest_review_revision(id).unwrap().unwrap();
    serde_json::from_str(revision.intent_json.as_deref().unwrap()).unwrap()
}

/// A stacked review whose base was merged and deleted pushes its branch and
/// then fails to open. Rebased onto main, the same review is resubmitted
/// against main and replaces the branch it pushed, leased on exactly that.
#[tokio::test]
async fn a_stacked_review_whose_base_is_gone_is_retargeted_and_rewritten() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    gh_forge(&f, pull_seven(&f.base_sha()));
    let wt = std::path::Path::new(&f.worktree);
    sh(
        wt,
        &format!(
            "git checkout -qb stack main && echo stacked > s.txt && git add -A \
             && git commit -qm stacked && git push -q {} stack:refs/heads/stack \
             && git update-ref refs/remotes/origin/stack stack \
             && git checkout -q feat/x && git rebase -q stack",
            f.dir.join("origin.git").display()
        ),
    );
    let mut args = f.submit_args();
    args["base"] = json!("stack");
    let id = review_id(&f.tool("s1", "submit_review", args).await);
    sh(
        &f.dir,
        "git --git-dir=origin.git update-ref -d refs/heads/stack",
    );

    let (status, body) = f.approve(&id).await;
    assert_ne!(status, StatusCode::OK, "{body}");
    let stale = f.forge_branch();
    let review = f.store.get_review(&id).unwrap().unwrap();
    assert_eq!(
        stale, review.head_sha,
        "the push landed before the open failed"
    );
    assert_eq!(review.state, "claimed");

    sh(wt, "git rebase -q --onto origin/main stack feat/x");
    let mut args = f.submit_args();
    args["review_id"] = json!(id);
    args["rewrite"] = json!(true);
    let resubmitted = f.tool("s1", "submit_review", args).await;
    assert_eq!(review_id(&resubmitted), id);
    let review = f.store.get_review(&id).unwrap().unwrap();
    assert_eq!(review.base_ref, "main");
    let target: tracon::review::publish::Target = serde_json::from_str(&review.target).unwrap();
    assert_eq!(target.base, "main");
    let intent = intent(&f, &id);
    assert!(intent.rewrite);
    assert_eq!(intent.lease.as_deref(), Some(stale.as_str()));
    f.forget_logs();

    let (status, body) = f.approve(&id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        f.git_log()
            .contains(&format!("--force-with-lease=refs/heads/feat/x:{stale}")),
        "{}",
        f.git_log()
    );
    assert!(f.gh_log().contains("--base main"), "{}", f.gh_log());
    assert_eq!(f.forge_branch(), review.head_sha);
}

/// A rewrite of a branch no change was opened for is still leased: somebody
/// pushing to it after submit makes publication refuse rather than force.
#[tokio::test]
async fn a_rewrite_without_a_change_is_refused_once_its_lease_is_lost() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    gh_forge(&f, pull_seven(&f.base_sha()));
    let host = f.dir.join("wt");
    sh(
        &host,
        "git checkout -qb elsewhere main && echo other > b.txt && git add -A \
         && git commit -qm other && git push -qf origin elsewhere:refs/heads/feat/x",
    );
    let lease = sh_out(&host, "git rev-parse elsewhere");
    let mut args = f.submit_args();
    args["rewrite"] = json!(true);
    let id = review_id(&f.tool("s1", "submit_review", args).await);
    let intent = intent(&f, &id);
    assert!(intent.rewrite);
    assert_eq!(intent.lease.as_deref(), Some(lease.as_str()));
    sh(
        &host,
        "git checkout -qb theirs main && echo theirs > c.txt && git add -A \
         && git commit -qm theirs && git push -qf origin theirs:refs/heads/feat/x",
    );
    let theirs = sh_out(&host, "git rev-parse theirs");
    f.forget_logs();

    let (status, body) = f.approve(&id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("somebody else pushed"),
        "{body}"
    );
    assert!(!f.git_log().contains("push origin"), "{}", f.git_log());
    assert_eq!(f.forge_branch(), theirs, "their work is still there");
}

/// Once a review updates an opened change, the change's base is the forge's
/// to move: a resubmission naming another is refused and says why.
#[tokio::test]
async fn a_resubmission_cannot_move_an_opened_changes_base() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let base = f.base_sha();
    f.forge_holds(&base);
    gh_forge(&f, pull_seven(&base));
    let id = review_id(&f.submit_update(json!({})).await);

    let refused = f
        .submit_update(json!({ "review_id": id, "base": "release" }))
        .await;
    assert!(
        refused["error"]
            .to_string()
            .contains("pull request 7, which merges into main"),
        "{refused}"
    );
    let review = f.store.get_review(&id).unwrap().unwrap();
    let target: tracon::review::publish::Target = serde_json::from_str(&review.target).unwrap();
    assert_eq!(target.base, "main");
}

/// Work shown with `show_work` reaches the review its session submits, read
/// from the node's own snapshot and stored as an HTML bundle, and is marked
/// stale once the candidate moves past the commit it was shown at.
#[tokio::test]
async fn shown_work_is_on_the_review_until_the_candidate_moves() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let worktree = std::path::Path::new(&f.worktree);
    sh(
        worktree,
        "mkdir -p show/img && printf '<title>After</title><img src=img/a.png>' > show/index.html \
         && printf 'png' > show/img/a.png",
    );

    let shown = f
        .tool(
            "s1",
            "show_work",
            json!({
                "title": "The new screen",
                "markdown": "Before it said nothing; ![after](https://example.test/x.png) now it does.",
                "paths": ["show"],
                "entry": "show/index.html",
            }),
        )
        .await;
    assert_eq!(shown["format"], "html", "{shown}");
    assert_eq!(shown["files"], 2, "{shown}");
    let head = sh_out(worktree, "git rev-parse HEAD");
    assert_eq!(shown["head_sha"], head.as_str());
    assert!(f.event_kinds("s1").contains(&"work_shown".to_string()));

    let id = review_id(&f.tool("s1", "submit_review", f.submit_args()).await);
    let (status, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["shown_work"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["stale"], false, "{body}");
    assert_eq!(items[0]["title"], "The new screen");
    let slug = items[0]["document_slug"].as_str().unwrap().to_string();
    assert!(slug.starts_with("shown-"), "{slug}");

    // The page is an ordinary HTML bundle: served only through the isolated
    // preview origin, with every file it loads beside it.
    let (status, doc) = f.call("GET", &format!("/api/docs/work/{slug}"), None).await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    assert_eq!(doc["format"], "html");
    assert_eq!(doc["entry_path"], "show/index.html");
    let mut paths: Vec<_> = doc["bundle_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();
    assert_eq!(paths, ["show/img/a.png", "show/index.html"]);

    // An agent cannot rewrite what it showed after the fact.
    let refused = f
        .tool(
            "s1",
            "doc_write",
            json!({ "slug": slug, "body": "# edited" }),
        )
        .await;
    assert!(
        refused["error"].to_string().contains("show_work"),
        "{refused}"
    );

    // The session carries it too, read against nothing.
    let (_, session) = f.call("GET", "/api/sessions/s1", None).await;
    assert_eq!(session["shown_work"].as_array().unwrap().len(), 1);

    // The agent keeps working and resubmits: what it showed is now about a
    // commit that is no longer the candidate.
    sh(
        worktree,
        "echo more >> a.txt && git add a.txt && git commit -qm later",
    );
    let mut resubmit = f.submit_args();
    resubmit["review_id"] = json!(id);
    let again = f.tool("s1", "submit_review", resubmit).await;
    assert_eq!(again["state"], "new", "{again}");
    let (_, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    let items = body["shown_work"].as_array().unwrap();
    assert_eq!(items[0]["stale"], true, "{body}");
    assert!(
        items[0]["stale_reason"]
            .as_str()
            .unwrap()
            .contains(&head[..12]),
        "{body}"
    );

    // Shown again at the new commit, it is current, beside the stale one.
    let fresh = f
        .tool(
            "s1",
            "show_work",
            json!({ "title": "Again", "markdown": "still works", "review_id": id }),
        )
        .await;
    assert_eq!(fresh["format"], "markdown", "{fresh}");
    let (_, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    let stale: Vec<_> = body["shown_work"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["stale"].as_bool().unwrap())
        .collect();
    assert_eq!(stale, [true, false], "{body}");
}

/// Plain files come back under a listing the node writes; nothing is read
/// from outside the workspace or from Git's own directory. (A workspace
/// holding a symbolic link is refused at export, before any of this; the
/// unit tests cover a link met here anyway.)
#[tokio::test]
async fn shown_files_are_listed_and_never_read_from_outside_the_workspace() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let worktree = std::path::Path::new(&f.worktree);
    sh(worktree, "mkdir -p out && echo report > out/report.txt");

    let shown = f
        .tool(
            "s1",
            "show_work",
            json!({ "title": "The report", "paths": ["out"] }),
        )
        .await;
    assert_eq!(shown["format"], "files", "{shown}");
    assert_eq!(shown["files"], 1, "{shown}");
    let (_, session) = f.call("GET", "/api/sessions/s1", None).await;
    let slug = session["shown_work"][0]["document_slug"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, doc) = f.call("GET", &format!("/api/docs/work/{slug}"), None).await;
    assert_eq!(doc["entry_path"], "tracon-shown.html", "{doc}");

    for (args, says) in [
        (json!({ "title": "x", "paths": ["../etc"] }), "relative"),
        (json!({ "title": "x", "paths": [".git/config"] }), "Git"),
        (json!({ "title": "x" }), "nothing to show"),
        (
            json!({ "title": "x", "paths": ["out"], "entry": "out/report.txt.html" }),
            "not one of the files",
        ),
    ] {
        let refused = f.tool("s1", "show_work", args.clone()).await;
        assert!(
            refused["error"].to_string().contains(says),
            "{args}: {refused}"
        );
    }
}

/// A review is shown work by whoever submitted it, and by no one else.
#[tokio::test]
async fn work_cannot_be_shown_on_another_sessions_review() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    f.store
        .insert_session(&{
            let mut r = support::rows::session_row("s2", "n1", "work");
            r.repo_path = format!("workspace://s1-{}", test_name!());
            r.branch = "feat/x".into();
            r.harness_id = "omp".into();
            r.harness_version = "18.0.4".into();
            r.started_mono_ms = Some(0);
            r
        })
        .unwrap();
    let id = review_id(&f.tool("s1", "submit_review", f.submit_args()).await);
    let refused = f
        .tool(
            "s2",
            "show_work",
            json!({ "title": "x", "markdown": "y", "review_id": id }),
        )
        .await;
    assert!(
        refused["error"].to_string().contains("another session"),
        "{refused}"
    );
}

/// A harness the operator runs has no session snapshot: it shows work on a
/// review it submitted, read from that review's worktree, and nowhere else.
#[tokio::test]
async fn an_external_harness_shows_work_on_its_review_from_its_worktree() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    f.store
        .insert_session(&{
            let mut r = support::rows::session_row("ext", "n1", "work");
            r.harness_id = "external".into();
            r.repo_path = String::new();
            r.worktree_path = None;
            r.branch = String::new();
            r.started_mono_ms = Some(0);
            r
        })
        .unwrap();
    let id = f
        .submit_as(
            "ext",
            json!({
                "provider": "github", "project": "owner/name",
                "base": "main", "branch": "feat/x", "worktree": f.worktree
            }),
        )
        .await;
    sh(std::path::Path::new(&f.worktree), "echo notes > notes.txt");
    let access = tracon::mcp::SessionAccess {
        store: f.store.clone(),
        manager: f.manager.clone(),
    };
    let ctx = tracon::mcp::CallContext::external(Some("laptop".into()), "work", "n1");

    let refused = tracon::mcp::show::call(&access, &ctx, &json!({ "title": "x", "markdown": "y" }))
        .await
        .unwrap_err();
    assert!(refused.contains("review_id"), "{refused}");

    let shown = tracon::mcp::show::call(
        &access,
        &ctx,
        &json!({ "title": "Notes", "paths": ["notes.txt"], "review_id": id }),
    )
    .await
    .unwrap();
    assert_eq!(shown["format"], "files", "{shown}");

    let (_, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    let items = body["shown_work"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["lane"], "laptop");
    assert_eq!(items[0]["stale"], false, "{body}");
    assert_eq!(items[0]["files"][0]["path"], "notes.txt");
}

/// A fresh submission of a commit already waiting for a verdict, with the
/// same words but another base, revises that review onto the new base while
/// no change is open for it.
#[tokio::test]
async fn a_retry_naming_another_base_retargets_the_waiting_review() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    sh(
        std::path::Path::new(&f.worktree),
        &format!(
            "git push -q {} main:refs/heads/release \
             && git update-ref refs/remotes/origin/release main",
            f.dir.join("origin.git").display()
        ),
    );
    let id = review_id(&f.tool("s1", "submit_review", f.submit_args()).await);

    let mut args = f.submit_args();
    args["base"] = json!("release");
    let retargeted = f.tool("s1", "submit_review", args).await;
    assert_eq!(review_id(&retargeted), id);
    assert_eq!(retargeted.get("already_submitted"), None, "{retargeted}");
    let review = f.store.get_review(&id).unwrap().unwrap();
    assert_eq!(review.state, "new");
    assert_eq!(review.base_ref, "release");
    let target: tracon::review::publish::Target = serde_json::from_str(&review.target).unwrap();
    assert_eq!(target.base, "release");
    assert_eq!(f.store.review_revisions(&id).unwrap().len(), 2);
    assert_eq!(f.store.open_reviews().unwrap().len(), 1);
}

/// The same retry for a review that updates an opened change is refused like
/// a resubmission naming it would be, and leaves the change's base alone.
#[tokio::test]
async fn a_retry_naming_another_base_cannot_move_an_opened_change() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let base = f.base_sha();
    f.forge_holds(&base);
    gh_forge(&f, pull_seven(&base));
    let id = review_id(&f.submit_update(json!({})).await);

    let refused = f.submit_update(json!({ "base": "release" })).await;
    assert!(
        refused["error"]
            .to_string()
            .contains("pull request 7, which merges into main"),
        "{refused}"
    );
    let review = f.store.get_review(&id).unwrap().unwrap();
    let target: tracon::review::publish::Target = serde_json::from_str(&review.target).unwrap();
    assert_eq!(target.base, "main");
    assert_eq!(f.store.review_revisions(&id).unwrap().len(), 1);
}

/// A retry naming the base the review already has, or none, is still the
/// same submission.
#[tokio::test]
async fn a_retry_with_the_same_or_no_base_is_already_submitted() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = review_id(&f.tool("s1", "submit_review", f.submit_args()).await);

    let same = f.tool("s1", "submit_review", f.submit_args()).await;
    assert_eq!(review_id(&same), id);
    assert_eq!(same["already_submitted"], json!(true), "{same}");

    let mut args = f.submit_args();
    args.as_object_mut().unwrap().remove("base");
    let absent = f.tool("s1", "submit_review", args).await;
    assert_eq!(review_id(&absent), id);
    assert_eq!(absent["already_submitted"], json!(true), "{absent}");
    assert_eq!(f.store.review_revisions(&id).unwrap().len(), 1);
}

// ---- the operator's unsent words ----

/// Feedback left on one device is on the node, so a reconnect or another
/// device finds it intact; a save from a device that did not see the latest
/// one is refused with it; and a recorded verdict clears it.
#[tokio::test]
async fn a_review_draft_survives_a_reconnect_and_is_cleared_by_the_verdict() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = review_id(&f.tool("s1", "submit_review", f.submit_args()).await);
    let revision = f.store.latest_review_revision(&id).unwrap().unwrap().id;
    let draft_uri = format!("/api/reviews/{id}/draft");

    let (status, empty) = f.call("GET", &draft_uri, None).await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert!(empty["draft"].is_null());

    let (status, saved) = f
        .call(
            "PUT",
            &draft_uri,
            Some(json!({ "base_version": 0, "revision_id": revision,
                         "draft": { "reason": "handle the empty case", "title": "edited" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["version"], 1);

    // The other device, which loaded before that save, is told rather than
    // overwriting it.
    let (status, conflict) = f
        .call(
            "PUT",
            &draft_uri,
            Some(json!({ "base_version": 0, "draft": { "reason": "something else" } })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(
        conflict["draft"]["draft"]["reason"],
        "handle the empty case"
    );

    // A reconnect reads it back intact.
    let (_, held) = f.call("GET", &draft_uri, None).await;
    assert_eq!(held["draft"]["draft"]["reason"], "handle the empty case");
    assert_eq!(held["draft"]["revision_id"], json!(revision));
    assert_eq!(held["draft"]["version"], 1);

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "revise", "reason": "handle the empty case" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, after) = f.call("GET", &draft_uri, None).await;
    assert!(after["draft"].is_null(), "{after}");

    let (status, _) = f
        .call(
            "PUT",
            "/api/reviews/nope/draft",
            Some(json!({ "draft": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// What a review says about publishing before anyone approves it: whether a
/// forge token is bound here, where to bind one, and never that the token may
/// push — only the forge knows that.
#[tokio::test]
async fn a_review_says_whether_a_forge_token_is_bound_before_approval() {
    state::isolate();
    let f = fixture(test_name!(), WITHOUT_GH).await;
    let id = f.submit().await;
    let (status, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let readiness = &body["publication"]["readiness"];
    assert_eq!(readiness["ready"], false, "{readiness}");
    assert!(
        readiness["problem"]
            .as_str()
            .unwrap()
            .contains("no GitHub token"),
        "{readiness}"
    );
    assert!(
        readiness["settings"]
            .as_str()
            .unwrap()
            .ends_with("#connections"),
        "{readiness}"
    );
    assert!(body["publication"]["latest"].is_null(), "{body}");

    let f = fixture(&format!("{}-bound", test_name!()), WITH_GH).await;
    let id = f.submit().await;
    let (_, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    let readiness = &body["publication"]["readiness"];
    assert_eq!(readiness["ready"], true, "{readiness}");
    assert!(
        readiness["note"]
            .as_str()
            .unwrap()
            .contains("only known when GitHub answers"),
        "a bound token is not reported as permission: {readiness}"
    );
}

/// Refused before it spoke to the forge: that is a different outcome from
/// the forge refusing, and the operator is told which.
#[tokio::test]
async fn a_publication_refused_before_the_forge_is_reported_as_not_attempted() {
    state::isolate();
    let f = fixture(test_name!(), WITHOUT_GH).await;
    let id = f.submit().await;
    let (status, _) = f.approve(&id).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    let (_, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    let latest = &body["publication"]["latest"];
    assert_eq!(latest["outcome"], "not_attempted", "{latest}");
    assert_eq!(latest["recoverable"], true, "{latest}");
    assert!(
        latest["remedy"]
            .as_str()
            .unwrap()
            .contains("Nothing reached"),
        "{latest}"
    );
}

/// A publication the forge did not keep is retried exactly as approved: the
/// operator's own title goes out again, nothing new is taken from the retry,
/// and the review is settled as published.
#[tokio::test]
async fn a_failed_publication_is_retried_as_it_was_approved() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = f.submit().await;
    let stub = f.dir.join("bin/git-remote-stub");
    let working = std::fs::read(&stub).unwrap();
    git_that_swallows_pushes(&f);
    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/verdict"),
            Some(json!({ "verdict": "approve", "title": "the operator's title" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");

    let (_, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    let latest = body["publication"]["latest"].clone();
    assert_eq!(latest["outcome"], "failed", "{latest}");
    assert_eq!(latest["recoverable"], true, "{latest}");

    std::fs::write(&stub, working).unwrap();
    f.forget_logs();
    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/publication/recover"),
            Some(json!({ "publication_id": latest["id"], "title": "smuggled" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let gh = f.gh_log();
    assert!(gh.contains("the operator's title"), "{gh}");
    assert!(!gh.contains("smuggled"), "{gh}");
    assert_eq!(f.publication(&id).state, "opened");
    assert_eq!(f.store.get_review(&id).unwrap().unwrap().state, "approved");

    let (status, _) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/publication/recover"),
            Some(json!({ "publication_id": latest["id"] })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a landed publication is not retried"
    );
}

/// An attempt whose outcome is unknown holds its claim, so approving again is
/// refused; the recovery action is the way out, and it looks at the forge
/// before repeating anything.
#[tokio::test]
async fn an_uncertain_publication_is_recovered_by_looking_first() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = f.submit().await;
    let review = f.store.get_review(&id).unwrap().unwrap();
    let head = review.head_sha.clone();
    let revision = f
        .store
        .latest_review_revision(&id)
        .unwrap()
        .map(|revision| revision.id);
    sh(
        &f.dir.join("wt"),
        &format!("git push -q origin {head}:refs/heads/feat/x"),
    );
    let publication = f.publication_id(&id);
    f.store
        .publication_begin(&tracon::store::PublicationBegin {
            id: &publication,
            review_id: &id,
            revision_id: revision.as_deref(),
            candidate_id: &tracon::store::candidate_id(&head, "work"),
            channel: "work",
            node_id: "n1",
            provider: "github",
            project: "owner/name",
            base: "main",
            branch: "feat/x",
            head_sha: &head,
            instance: "a-process-that-is-no-longer-running",
        })
        .unwrap();
    f.store.publication_pushed(&publication, &head).unwrap();
    f.store.publication_opening(&publication).unwrap();
    assert!(f.store.begin_publish(&id, revision.as_deref()).unwrap());
    std::fs::rename(f.dir.join("origin.git"), f.dir.join("origin.gone")).unwrap();

    let (status, body) = f.approve(&id).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(f.publication(&id).state, "uncertain");
    let (status, _) = f.approve(&id).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "approving again is not how an unknown outcome is settled"
    );

    let (_, body) = f.call("GET", &format!("/api/reviews/{id}"), None).await;
    let latest = &body["publication"]["latest"];
    assert_eq!(latest["outcome"], "uncertain", "{latest}");
    assert_eq!(latest["recoverable"], true, "{latest}");

    std::fs::rename(f.dir.join("origin.gone"), f.dir.join("origin.git")).unwrap();
    gh_that_lists(&f, "[]");
    f.forget_logs();
    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/publication/recover"),
            Some(json!({ "publication_id": publication })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !f.git_log().contains("push origin"),
        "the push that landed is not repeated: {}",
        f.git_log()
    );
    assert_eq!(f.publication(&id).state, "opened");
}

/// A resubmission after a failed attempt is new bytes: retrying the old
/// approval would publish something nobody approved, so it is refused.
#[tokio::test]
async fn a_retry_is_refused_once_a_newer_revision_arrives() {
    state::isolate();
    let f = fixture(test_name!(), WITH_GH).await;
    let id = review_id(&f.tool("s1", "submit_review", f.submit_args()).await);
    git_that_swallows_pushes(&f);
    let (status, _) = f.approve(&id).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let publication = f.store.publications_for_review(&id).unwrap()[0].id.clone();
    sh(
        std::path::Path::new(&f.worktree),
        "echo more >> a.txt && git add -A && git commit -qm second",
    );
    let mut resubmit = f.submit_args();
    resubmit["review_id"] = json!(id);
    let again = f.tool("s1", "submit_review", resubmit).await;
    assert_eq!(again["state"], "new", "{again}");

    let (status, body) = f
        .call(
            "POST",
            &format!("/api/reviews/{id}/publication/recover"),
            Some(json!({ "publication_id": publication })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("newer revision"),
        "{body}"
    );
}
