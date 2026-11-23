//! `check_run`: a harness the operator runs tests through the node.
//!
//! An agent outside the boundary has no container runtime of the node's to
//! run its repository's checks in, and reaching for one of its own (the Dev
//! Container CLI, a Podman machine kept running on macOS) is a second,
//! unbrokered way to run containers. So with `[external] run_containers` on,
//! the same switch that runs a repository's required checks behind an
//! external `submit_review`, the node runs them for it on request: the
//! worktree's commit is snapshot as `submit_review` snapshots it, prepared
//! as a review's candidate is, and each check (or the one command asked for)
//! run in the repository's image on a fresh copy of the prepared tree, with
//! the dependency cache read-only and no network. Placed and queued as a
//! review's checks are (`review::run_placed`).
//!
//! What it runs is evidence of nothing. The output goes back to the caller
//! and is attached to no candidate, review or session: the records it leaves
//! name no candidate, reuse no earlier run, and no later run reuses them
//! (`review::checks::run_required_for`'s `asked`).
//!
//! Only a repository with a `[[repo]]` entry is run: without one the node
//! would be guessing the image and the checks, and a harness testing in a
//! guessed environment learns nothing it can trust.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use serde_json::{json, Value};

use crate::{
    mcp::CallContext,
    review,
    session::Manager,
    store::{now_ms, CandidateRow, Store},
};

pub const CHECK_RUN: &str = "check_run";

pub fn definitions() -> Vec<Value> {
    vec![json!({
        "name": CHECK_RUN,
        "description": "Run this repository's checks, or one command, in the repository's own \
             image on the node, instead of in a container of your own. The node snapshots the \
             worktree's HEAD commit as submit_review does (commit first: uncommitted changes \
             are not in it), prepares it as the repository's [[repo]] entry says, and runs each \
             check on a fresh copy of the prepared tree with the dependency cache read-only and \
             no network. Each result has its outcome, exit status and the end of its output. \
             Nothing is attached to a review: this is for you, and submit_review runs the \
             required checks again on what it is given. Checks wait for room as a review's do, \
             so a call can take as long as the checks themselves.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "worktree": { "type": "string", "description": "The absolute path of the worktree to check, under the node's [external] repo_roots." },
                "checks": { "type": "array", "items": { "type": "string" }, "description": "Which of the repository's configured checks to run, each exactly as configured. Defaults to all of them." },
                "command": { "type": "string", "description": "One command to run instead of the configured checks, through `sh -c` in the prepared tree. Not with `checks`." },
            },
            "required": ["worktree"],
        },
    })]
}

/// The caller's interest in a run: dropped with the call, which withdraws
/// the run, so a client that gives up on the call leaves nothing running.
struct CallerStillWants(Arc<AtomicBool>);

impl review::checks::Cancel for CallerStillWants {
    fn cancelled(&self) -> Option<String> {
        self.0
            .load(Ordering::Acquire)
            .then(|| "the caller stopped waiting for check_run".into())
    }
}

/// Sets its flag when the call that holds it ends, however it ends.
struct Abandoned(Arc<AtomicBool>);

impl Drop for Abandoned {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub async fn call(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    if !ctx.is_external() {
        return Err(format!(
            "{CHECK_RUN} is for a harness you run yourself; a session's checks run when it \
             calls {}",
            super::review::SUBMIT
        ));
    }
    if !manager.cfg().external.run_containers {
        return Err(format!(
            "{CHECK_RUN} is off on this node; the operator turns it on with [external] \
             run_containers"
        ));
    }
    if let Some(why) = manager.restart().refusal() {
        return Err(format!(
            "{CHECK_RUN} is not taken while {}; call it again then",
            why.trim_start_matches("the ")
        ));
    }
    let path = args
        .get("worktree")
        .and_then(Value::as_str)
        .ok_or("worktree is required: the absolute path of the worktree to check")?;
    let roots: Vec<_> = manager
        .cfg()
        .external
        .repo_roots
        .iter()
        .map(|root| crate::config::expand_home(root))
        .collect();
    let (worktree, repo) = review::locate_repo(path, &roots)
        .await
        .map_err(|error| error.to_string())?;
    let repo = entry_repo(manager, ctx, std::path::Path::new(&repo)).await?;
    let environment = crate::environment::environment_for(manager.cfg(), store, Some(&repo));
    let asked = asked(args, &environment.checks, &repo)?;

    let head_sha = review::resolve(&worktree, "HEAD")
        .await
        .map_err(|error| error.to_string())?;
    let max_snapshot_bytes = manager.cfg().live().supervision.max_snapshot_bytes;
    let snapshot = review::snapshot_candidate(&worktree, &head_sha, max_snapshot_bytes)
        .await
        .map_err(|error| error.to_string())?;
    // A candidate in name only: what the runners read a tree's identity
    // from. It is never stored, and nothing recorded points at it.
    let candidate = CandidateRow {
        id: format!("check-run-{}", uuid::Uuid::now_v7()),
        head_sha: head_sha.clone(),
        tree_sha: Some(snapshot.tree_sha.clone()),
        channel: ctx.channel.clone(),
        owner_session_id: None,
        source_kind: CHECK_RUN.into(),
        captured_ms: now_ms(),
        capture_json: "{}".into(),
    };

    // The run is the node's from here, in a task of its own, so that a
    // client giving up on this call withdraws it (the runners kill what is
    // running) rather than dropping it mid-check.
    let abandoned = Arc::new(AtomicBool::new(false));
    let _abandoned = Abandoned(abandoned.clone());
    let running = manager.external_checks().begin_asked();
    let task = tokio::spawn({
        let store = store.clone();
        let manager = manager.clone();
        let channel = ctx.channel.clone();
        let candidate = candidate.clone();
        let asked = asked.clone();
        async move {
            let wants = CallerStillWants(abandoned);
            let entry = manager
                .checking()
                .line_entry(&candidate.id, &candidate.id, asked.clone());
            let (root, repo, asked) = (&snapshot.root, Some(repo.as_path()), &asked[..]);
            let (store_ref, manager_ref, candidate_ref, wants_ref) =
                (&store, &manager, &candidate, &wants);
            let report = super::review::run_placed(
                &manager,
                &channel,
                repo,
                &entry,
                &wants,
                || {
                    review::checks::run_required_for(
                        manager_ref.backend().as_ref(),
                        manager_ref.cfg(),
                        store_ref,
                        candidate_ref,
                        None,
                        repo,
                        root,
                        false,
                        wants_ref,
                        Some(asked),
                    )
                },
                |runtime, requirements| async move {
                    review::forked::run_on_digitalocean(
                        manager_ref.backend().as_ref(),
                        manager_ref.cfg(),
                        store_ref,
                        candidate_ref,
                        None,
                        repo,
                        root,
                        false,
                        wants_ref,
                        &runtime,
                        requirements,
                        Some(asked),
                    )
                    .await
                },
            )
            .await;
            manager.publish_queue().await;
            drop(running);
            tokio::task::spawn_blocking(move || drop(snapshot));
            report
        }
    });
    let report = task
        .await
        .map_err(|error| format!("the checks stopped without an outcome ({error})"))?
        .ok_or("the checks were withdrawn before they ran")??;
    Ok(answer(&worktree, &head_sha, &candidate, &report))
}

/// The repository whose `[[repo]]` entry the worktree's checks run under:
/// its own, or the node's clone of the same forge project's
/// (`environment::external_review_repo`), as an external review's checks
/// are. Refused when neither has one.
async fn entry_repo(
    manager: &Manager,
    ctx: &CallContext,
    repo: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let cfg = manager.cfg();
    let has_entry = |repo: &std::path::Path| cfg.repos().iter().any(|entry| entry.matches(repo));
    if has_entry(repo) {
        return Ok(repo.to_path_buf());
    }
    let (_, _, remote) =
        crate::corpus::project::identify(&ctx.channel, repo, &cfg.live().publish.git).await;
    if let Some(forge) = remote
        .as_deref()
        .and_then(crate::forge::ForgeRepo::from_remote)
    {
        let provider = forge.forge.name();
        let forge_env = review::publish::Provider::parse(provider)
            .and_then(|provider| {
                manager
                    .broker()
                    .read()
                    .unwrap()
                    .env_for(provider.credential(), &ctx.channel, &ctx.node_id)
                    .ok()
            })
            .unwrap_or_default();
        let clone =
            crate::environment::external_review_repo(cfg, repo, provider, &forge.path, &forge_env);
        if has_entry(&clone) {
            return Ok(clone);
        }
    }
    Err(format!(
        "{} has no [[repo]] entry on this node, so there is no image, preparation or checks \
         to run it with. A session on this repository drafts one with {} and proposes it to \
         the operator, or the operator adds one to node.toml; until then, test where your \
         toolchain is",
        repo.display(),
        super::setup::DRAFT
    ))
}

/// What the call asks to run: `command` alone, the configured checks it
/// names, or every configured check.
fn asked(
    args: &Value,
    configured: &[String],
    repo: &std::path::Path,
) -> Result<Vec<String>, String> {
    let command = match args.get("command") {
        None | Some(Value::Null) => None,
        Some(Value::String(command)) if !command.trim().is_empty() => {
            Some(command.trim().to_string())
        }
        Some(_) => return Err("command is one shell command, as a string".into()),
    };
    let checks = match args.get("checks") {
        None | Some(Value::Null) => None,
        Some(Value::Array(checks)) => Some(
            checks
                .iter()
                .map(|check| {
                    check
                        .as_str()
                        .map(|check| check.trim().to_string())
                        .ok_or("checks are the configured commands, as strings")
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err("checks is a list of the configured commands".into()),
    };
    let configured_list = || match configured.is_empty() {
        true => "none".to_string(),
        false => configured
            .iter()
            .map(|check| format!("`{check}`"))
            .collect::<Vec<_>>()
            .join(", "),
    };
    match (command, checks) {
        (Some(_), Some(_)) => Err("pass `checks` or `command`, not both".into()),
        (Some(command), None) => Ok(vec![command]),
        (None, Some(checks)) => {
            if checks.is_empty() {
                return Err(format!(
                    "checks is empty; leave it out to run every configured check ({})",
                    configured_list()
                ));
            }
            match checks.iter().find(|check| !configured.contains(check)) {
                Some(unknown) => Err(format!(
                    "`{unknown}` is not one of {}'s configured checks ({}); pass it as \
                     `command` to run it anyway",
                    repo.display(),
                    configured_list()
                )),
                None => Ok(checks),
            }
        }
        (None, None) if configured.is_empty() => Err(format!(
            "{} has no checks configured; pass `command` to run one",
            repo.display()
        )),
        (None, None) => Ok(configured.to_vec()),
    }
}

/// What the caller is told: each check's result, what preparation said
/// when it failed, and what they ran in and where.
fn answer(
    worktree: &str,
    head_sha: &str,
    candidate: &CandidateRow,
    report: &review::checks::CheckReport,
) -> Value {
    let passed = report.cancelled.is_none()
        && report.no_room.is_none()
        && !report.results.is_empty()
        && report.results.iter().all(|result| result.ok);
    let mut out = json!({
        "worktree": worktree,
        "head_sha": head_sha,
        "tree_sha": candidate.tree_sha,
        "passed": passed,
        "results": report.results,
        "image": report.image,
        "image_source": report.image_source,
    });
    if let Some(prepared) = &report.prepared {
        out["prepared"] = json!({
            "ok": prepared.ok,
            "ms": prepared.ms,
            "failure": prepared.failure.as_ref().map(|failure| json!({
                "reason": failure.reason(),
                "output": failure.to_string(),
                "refused": failure.refused,
            })),
        });
    }
    if let Some(elsewhere) = &report.elsewhere {
        out["ran_on"] = json!(elsewhere.provider);
    }
    if let Some(why) = &report.no_room {
        out["no_room"] = json!(why);
    }
    if let Some(why) = &report.cancelled {
        out["cancelled"] = json!(why);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> Vec<String> {
        vec!["cargo test".into(), "cargo fmt --check".into()]
    }

    #[test]
    fn a_call_asks_for_every_check_some_of_them_or_one_command() {
        let repo = std::path::Path::new("/src/app");
        assert_eq!(
            asked(&json!({}), &configured(), repo).unwrap(),
            configured()
        );
        assert_eq!(
            asked(
                &json!({"checks": ["cargo fmt --check"]}),
                &configured(),
                repo
            )
            .unwrap(),
            ["cargo fmt --check"]
        );
        assert_eq!(
            asked(&json!({"command": " ls -la "}), &configured(), repo).unwrap(),
            ["ls -la"]
        );
        // A command needs no configured checks at all.
        assert_eq!(asked(&json!({"command": "ls"}), &[], repo).unwrap(), ["ls"]);
    }

    #[test]
    fn a_call_is_refused_what_it_cannot_mean() {
        let repo = std::path::Path::new("/src/app");
        let both = asked(
            &json!({"checks": ["cargo test"], "command": "ls"}),
            &configured(),
            repo,
        )
        .unwrap_err();
        assert!(both.contains("not both"), "{both}");
        let unknown = asked(&json!({"checks": ["make"]}), &configured(), repo).unwrap_err();
        assert!(unknown.contains("`make` is not one of"), "{unknown}");
        assert!(unknown.contains("`cargo test`"), "{unknown}");
        assert!(unknown.contains("as `command`"), "{unknown}");
        let none = asked(&json!({}), &[], repo).unwrap_err();
        assert!(none.contains("pass `command`"), "{none}");
        let empty = asked(&json!({"checks": []}), &configured(), repo).unwrap_err();
        assert!(empty.contains("leave it out"), "{empty}");
    }
}
