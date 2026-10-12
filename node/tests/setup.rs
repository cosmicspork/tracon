//! Setting a repository up once, through the tools a session calls: a draft
//! read from the default branch, a trial of it, and a proposal the node
//! writes only once the operator allows it.

#[path = "support/mod.rs"]
mod support;
use support::{
    harness::{harness, harness_on, Harness},
    rows::session_row,
};

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tracon::boundary::{
    Backend, BoundaryError, BoundaryReport, BuildFailure, BuiltImage, ImageBuild, ImageBuilder,
};
use tracon::config::Config;
use tracon::runner::{local::LocalBackend, Mount, Runner};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// A repository whose default branch has a `just check` recipe and a Cargo
/// lockfile, and a session `s1` working in it.
fn repository(h: &Harness) -> tempfile::TempDir {
    repository_for(h, "s1")
}

/// As `repository`, for a session of the test's own. Trials are the node's,
/// not the store's, and one session runs one at a time: a test that waits on
/// its own trial does not share a session with another test's.
fn repository_for(h: &Harness, session_id: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("justfile"), "check:\n    cargo test\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(root.join("Cargo.lock"), "").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
    // What the working tree says after the commit is not the default branch.
    std::fs::write(root.join("package.json"), r#"{"scripts":{"test":"x"}}"#).unwrap();
    let mut session = session_row(session_id, "n1", "personal");
    session.repo_path = root.to_string_lossy().to_string();
    h.store.insert_session(&session).unwrap();
    dir
}

async fn tool(h: &Harness, name: &str, args: Value) -> Result<Value, String> {
    tool_as(h, "s1", name, args).await
}

async fn tool_as(h: &Harness, session_id: &str, name: &str, args: Value) -> Result<Value, String> {
    let ctx = tracon::mcp::CallContext::session(session_id, "personal", "n1");
    h.tools.call(&ctx, name, &args).await
}

async fn answer(h: &Harness, id: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/permissions/{id}/answer"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = h.operator.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_draft_is_read_from_the_default_branch() {
    let h = harness().await;
    let dir = repository(&h);
    let draft = tool(&h, "repo_setup_draft", json!({})).await.unwrap();
    assert_eq!(draft["entry"]["checks"], json!(["just check"]), "{draft}");
    assert_eq!(draft["entry"]["prepare"], json!(["cargo fetch --locked"]));
    assert_eq!(draft["entry"]["egress"], json!(["crates"]));
    assert_eq!(draft["entry"]["path"], json!(dir.path().to_string_lossy()));
    assert!(draft["current"].is_null());
    assert!(
        !draft.to_string().contains("package.json"),
        "the working tree was read: {draft}"
    );
}

#[tokio::test]
async fn a_proposal_is_written_only_once_the_operator_allows_it() {
    let h = harness().await;
    let dir = repository(&h);
    let repo = dir.path().to_path_buf();
    let asked = tool(
        &h,
        "repo_setup_propose",
        json!({
            "checks": ["just check"],
            "prepare": ["cargo fetch --locked"],
            "egress": ["crates"],
            "why": "the trial passed",
            "run_notes": "  `just dev` serves it on :8080; `just seed` loads the fixtures.  ",
        }),
    )
    .await
    .unwrap();
    assert_eq!(asked["state"], "awaiting_operator", "{asked}");
    let id = asked["approval_id"].as_str().unwrap().to_string();
    assert!(!h.manager.cfg().repos().iter().any(|e| e.matches(&repo)));
    let (_, notes, _) = h.store.manifest_contents("personal").unwrap();
    assert!(notes.is_empty(), "the notes waited for the operator too");

    // Nothing an agent sends writes it: the write runs only from the card.
    let card = h.store.get_approval(&id).unwrap().unwrap();
    let arguments: Value = serde_json::from_str(&card.arguments).unwrap();
    assert_eq!(arguments["repo"], json!(repo.to_string_lossy()));

    let (status, body) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut settled = None;
    for _ in 0..100 {
        let row = h.store.get_approval(&id).unwrap().unwrap();
        if row.is_settled() {
            settled = Some(row);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let settled = settled.expect("the approval settles");
    assert_eq!(settled.state, "succeeded", "{:?}", settled.reason);
    let repos = h.manager.cfg().repos();
    let entry = repos.iter().find(|e| e.matches(&repo)).expect("written");
    assert_eq!(
        entry.checks.as_deref(),
        Some(&["just check".to_string()][..])
    );
    assert_eq!(entry.egress, ["crates"]);
    assert!(!entry.session_egress);
    let file = tracon::config::Config::try_load().unwrap();
    assert!(file.repo.iter().any(|e| e.matches(&repo)));

    // How to run it became a standing note on the session's channel.
    let (_, notes, _) = h.store.manifest_contents("personal").unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].name, tracon::mcp::setup::run_notes_name(&repo));
    assert_eq!(
        notes[0].body,
        "`just dev` serves it on :8080; `just seed` loads the fixtures."
    );
}

#[tokio::test]
async fn a_proposal_the_node_would_refuse_raises_no_card() {
    let h = harness().await;
    let _dir = repository(&h);
    let refused = tool(
        &h,
        "repo_setup_propose",
        json!({ "checks": ["just check"], "image": "node:22" }),
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains("digest") || refused.contains("pinned"),
        "{refused}"
    );
    assert!(h.store.open_permission_views().unwrap().is_empty());
    // And one that opened its sessions' egress is not something it can ask.
    let schema = h
        .tools
        .list("personal", "n1")
        .into_iter()
        .find(|tool| tool["name"] == "repo_setup_propose")
        .unwrap();
    assert!(schema["inputSchema"]["properties"]
        .get("session_egress")
        .is_none());
}

#[tokio::test]
async fn a_trial_runs_each_check_and_names_what_it_could_not_reach() {
    let h = harness().await;
    let _dir = repository(&h);
    let trial = tool(
        &h,
        "repo_setup_try",
        json!({
            "checks": ["test -f justfile", "exit 3"],
            "egress": ["crates"],
            "wait_secs": 30,
        }),
    )
    .await
    .unwrap();
    assert_eq!(trial["state"], "failed", "{trial}");
    assert_eq!(trial["steps"][0]["ok"], true, "{trial}");
    assert_eq!(trial["steps"][1]["exit"], 3, "{trial}");
    // This session holds no grant, so preparation would have reached nothing.
    assert!(trial["egress_tried"].as_array().unwrap().is_empty());
    assert!(trial["egress_not_tried"]
        .as_array()
        .unwrap()
        .contains(&json!("crates.io")));
    let again = tool(
        &h,
        "repo_setup_try",
        json!({ "trial_id": trial["id"], "wait_secs": 0 }),
    )
    .await
    .unwrap();
    assert_eq!(again["id"], trial["id"]);
}

/// The local backend, with a builder that records what it was asked to build
/// instead of running Podman.
#[derive(Default)]
struct Building {
    built: Mutex<Vec<String>>,
}

impl Building {
    fn builds(&self) -> Vec<String> {
        self.built.lock().unwrap().clone()
    }
}

#[async_trait]
impl ImageBuilder for Building {
    async fn build(&self, build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
        let mut built = self.built.lock().unwrap();
        built.push(build.tag.to_string());
        let name = build.tag.rsplit_once(':').unwrap().0;
        Ok(BuiltImage {
            image: format!("{name}@sha256:{:064x}", built.len()),
            log_tail: "built".into(),
        })
    }
    async fn exists(&self, _image: &str) -> bool {
        true
    }
    async fn identity(&self, _image: &str) -> Option<String> {
        None
    }
    async fn remove(&self, _image: &str) {}
}

#[async_trait]
impl Backend for Building {
    fn kind(&self) -> &'static str {
        "building"
    }
    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
        LocalBackend.setup(cfg, rebuild).await
    }
    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport {
        LocalBackend.check_all(cfg, deep).await
    }
    fn runner(&self, extra: Vec<Mount>) -> Arc<dyn Runner> {
        LocalBackend.runner(extra)
    }
    fn runner_for(&self, harness_id: &str, extra: Vec<Mount>) -> Arc<dyn Runner> {
        LocalBackend.runner_for(harness_id, extra)
    }
    fn image_builder(&self) -> Option<&dyn ImageBuilder> {
        Some(self)
    }
    async fn import_volume(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        LocalBackend.import_volume(volume, source).await
    }
    async fn export_volume(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        LocalBackend.export_volume(volume, destination).await
    }
    async fn clone_volume(&self, source: &str, destination: &str) -> Result<(), BoundaryError> {
        LocalBackend.clone_volume(source, destination).await
    }
    async fn remove_volume(&self, volume: &str) -> Result<(), BoundaryError> {
        LocalBackend.remove_volume(volume).await
    }
    fn harness_host(&self) -> String {
        LocalBackend.harness_host()
    }
    fn harness_home(&self) -> String {
        LocalBackend.harness_home()
    }
    async fn reconcile(&self, names: &[String]) {
        LocalBackend.reconcile(names).await
    }
}

/// A dev Dockerfile on the default branch.
fn dev_dockerfile(root: &std::path::Path) {
    std::fs::create_dir_all(root.join(".devcontainer")).unwrap();
    std::fs::write(root.join(".devcontainer/Dockerfile"), "FROM debian\n").unwrap();
    git(root, &["add", ".devcontainer"]);
    git(root, &["commit", "-qm", "devcontainer"]);
}

async fn settled(h: &Harness, id: &str) -> tracon::store::approvals::ApprovalRow {
    for _ in 0..100 {
        let row = h.store.get_approval(id).unwrap().unwrap();
        if row.is_settled() {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the approval did not settle");
}

/// A first-time setup is proven in the image its Dockerfile builds, not in
/// the harness image: the build is held for the operator, since it runs with
/// the network outside the boundary, and once built the same Dockerfile is
/// tried again without asking or building.
#[tokio::test]
async fn a_dockerfile_is_built_for_a_trial_once_the_operator_allows_it() {
    let backend = Arc::new(Building::default());
    let h = harness_on(Config::default(), backend.clone()).await;
    let dir = repository_for(&h, "s-build");
    dev_dockerfile(dir.path());
    let args = json!({
        "dockerfile": ".devcontainer/Dockerfile",
        "checks": ["test -f justfile"],
        "wait_secs": 30,
    });

    let held = tool_as(&h, "s-build", "repo_setup_try", args.clone())
        .await
        .unwrap();
    assert_eq!(held["state"], "awaiting_operator", "{held}");
    assert!(backend.builds().is_empty(), "built before it was allowed");
    let id = held["approval_id"].as_str().unwrap().to_string();
    let card = h.store.get_approval(&id).unwrap().unwrap();
    assert!(
        card.title.starts_with("repo_setup_try ")
            && card
                .title
                .contains(": build .devcontainer/Dockerfile from HEAD at "),
        "{}",
        card.title
    );
    let asked: Value = serde_json::from_str(&card.arguments).unwrap();
    assert_eq!(asked["repo"], json!(dir.path().to_string_lossy()));
    assert_eq!(asked["checks"], json!(["test -f justfile"]));
    assert!(asked.get("wait_secs").is_none(), "{asked}");
    // Asking again while it waits is the same card.
    let again = tool_as(&h, "s-build", "repo_setup_try", args.clone())
        .await
        .unwrap();
    assert_eq!(again["approval_id"], held["approval_id"]);

    // What the card names is what is built: not another Dockerfile.
    let mut edited = asked.clone();
    edited["dockerfile"] = json!("Dockerfile");
    let (status, _) = answer(
        &h,
        &id,
        json!({ "option_id": "allow_once", "arguments": edited }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, body) = answer(&h, &id, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let settled = settled(&h, &id).await;
    assert_eq!(settled.state, "succeeded", "{:?}", settled.reason);
    let started: Value = serde_json::from_str(settled.result.as_deref().unwrap()).unwrap();
    let trial = tool_as(
        &h,
        "s-build",
        "repo_setup_try",
        json!({ "trial_id": started["id"], "wait_secs": 30 }),
    )
    .await
    .unwrap();
    assert_eq!(trial["state"], "passed", "{trial}");
    assert_eq!(trial["image_source"], "repository Dockerfile", "{trial}");
    assert_eq!(trial["steps"][0]["phase"], "build", "{trial}");
    assert_eq!(trial["steps"][1]["phase"], "check", "{trial}");
    let builds = backend.builds();
    assert_eq!(builds.len(), 2, "{builds:?}");
    assert!(trial["image"]
        .as_str()
        .unwrap()
        .starts_with(builds[1].rsplit_once(':').unwrap().0));

    // The same Dockerfile again: no card, no build, the same image.
    let reused = tool_as(&h, "s-build", "repo_setup_try", args)
        .await
        .unwrap();
    assert_eq!(reused["state"], "passed", "{reused}");
    assert_eq!(reused["image"], trial["image"]);
    assert_eq!(reused["image_source"], "repository Dockerfile");
    assert_eq!(reused["steps"][0]["phase"], "check", "{reused}");
    assert_eq!(backend.builds().len(), 2);
}

/// A runtime that cannot build says so, rather than trying the draft in the
/// harness image and failing on a toolchain the Dockerfile would have had.
#[tokio::test]
async fn a_dockerfile_is_never_tried_in_the_harness_image_instead() {
    let h = harness().await;
    let dir = repository_for(&h, "s-unbuilt");
    dev_dockerfile(dir.path());
    let trial = tool_as(
        &h,
        "s-unbuilt",
        "repo_setup_try",
        json!({
            "dockerfile": ".devcontainer/Dockerfile",
            "checks": ["true"],
            "wait_secs": 30,
        }),
    )
    .await
    .unwrap();
    assert_eq!(trial["state"], "failed", "{trial}");
    assert!(
        trial["error"]
            .as_str()
            .unwrap()
            .contains("cannot build images"),
        "{trial}"
    );
    assert!(trial["steps"].as_array().unwrap().is_empty(), "{trial}");
    assert!(h.store.open_permission_views().unwrap().is_empty());
}

/// The operator's own entry already names this Dockerfile, and the node builds
/// that one for its next run without asking: a trial of it builds it the same
/// way, as the repository's image, with no card.
#[tokio::test]
async fn a_dockerfile_the_entry_already_names_is_built_without_a_card() {
    let backend = Arc::new(Building::default());
    let h = harness_on(Config::default(), backend.clone()).await;
    let dir = repository_for(&h, "s-held");
    dev_dockerfile(dir.path());
    h.manager.cfg().live_repo.set(vec![tracon::config::Repo {
        path: dir.path().to_path_buf(),
        dockerfile: Some(".devcontainer/Dockerfile".into()),
        checks: Some(vec!["just check".into()]),
        ..Default::default()
    }]);
    let trial = tool_as(
        &h,
        "s-held",
        "repo_setup_try",
        json!({
            "dockerfile": ".devcontainer/Dockerfile",
            "checks": ["test -f justfile"],
            "wait_secs": 30,
        }),
    )
    .await
    .unwrap();
    assert_eq!(trial["state"], "passed", "{trial}");
    assert_eq!(trial["steps"][0]["phase"], "build", "{trial}");
    assert_eq!(trial["image_source"], "repository Dockerfile");
    assert!(h.store.open_permission_views().unwrap().is_empty());
    let base = h
        .store
        .ready_repo_image(&dir.path().to_string_lossy(), "base")
        .unwrap()
        .expect("built as the repository's image");
    assert_eq!(trial["image"], json!(base.image));
    assert_eq!(backend.builds().len(), 2);
}
