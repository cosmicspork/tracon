//! Setting a repository up once, through the tools a session calls: a draft
//! read from the default branch, a trial of it, and a proposal the node
//! writes only once the operator allows it.

#[path = "support/mod.rs"]
mod support;
use support::{harness::harness, harness::Harness, rows::session_row};

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
    let mut session = session_row("s1", "n1", "personal");
    session.repo_path = root.to_string_lossy().to_string();
    h.store.insert_session(&session).unwrap();
    dir
}

async fn tool(h: &Harness, name: &str, args: Value) -> Result<Value, String> {
    let ctx = tracon::mcp::CallContext::session("s1", "personal", "n1");
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
        }),
    )
    .await
    .unwrap();
    assert_eq!(asked["state"], "awaiting_operator", "{asked}");
    let id = asked["approval_id"].as_str().unwrap().to_string();
    assert!(!h.manager.cfg().repos().iter().any(|e| e.matches(&repo)));

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
