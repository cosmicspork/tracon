//! Candidate-bound deterministic checks. Required commands originate only in
//! operator configuration, and every execution mounts a node-captured Git tree
//! read-only rather than the agent's mutable worktree.

use std::{path::Path, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    boundary::Backend,
    config::{Config, RuntimeKind},
    runner::{Mount, RunnerCommand},
    store::{now_ms, CandidateRow, CheckRunRow, Store},
};

/// How much of a check's combined output stays in its authoritative record.
const TAIL_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckResult {
    pub command: String,
    pub ok: bool,
    pub exit: Option<i32>,
    /// `passed`, `failed`, `interrupted`, `cancelled`, or `reused`.
    pub outcome: String,
    /// The last few KiB of stdout+stderr, or the retained output of reused
    /// evidence.
    pub tail: String,
    pub ms: u64,
    pub reused_from: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub candidate_id: String,
    pub results: Vec<CheckResult>,
    pub required_count: usize,
    pub all_required_passed: bool,
    pub reused: bool,
}

#[derive(Debug, Clone, Serialize)]
struct CheckDefinition {
    command: String,
    timeout_secs: u64,
}

/// The trusted required definitions. Candidate files never alter this list.
pub fn required_definitions(cfg: &Config) -> Vec<String> {
    cfg.supervision
        .checks
        .iter()
        .map(|command| command.trim())
        .filter(|command| !command.is_empty())
        .map(str::to_string)
        .collect()
}

/// Run every configured required check against a pre-captured candidate. Exact
/// passed *and failed* evidence may be reused only when every identity in the
/// reuse key is immutable and unchanged; an explicit rerun always creates a
/// new execution record.
pub async fn run_required(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    candidate: &CandidateRow,
    snapshot: &Path,
    force_rerun: bool,
) -> Result<CheckReport, String> {
    let commands = required_definitions(cfg);
    let raw_image = execution_image(cfg);
    let pinned_image = immutable_image_identity(&raw_image);
    let inputs = check_inputs(cfg);
    let inputs_json = serde_json::to_string(&inputs).map_err(|error| error.to_string())?;
    let timeout = Duration::from_secs(cfg.supervision.timeout_secs.max(1));
    let runner = backend.runner(Vec::new());
    let mut results = Vec::with_capacity(commands.len());
    let mut any_reused = false;

    for (index, command) in commands.iter().enumerate() {
        let definition = CheckDefinition {
            command: command.clone(),
            timeout_secs: timeout.as_secs(),
        };
        let definition_json = serde_json::to_string(&definition).map_err(|error| error.to_string())?;
        let definition_hash = hash(&definition_json);
        let reuse_key = pinned_image.as_ref().map(|image| {
            hash(&format!(
                "{}\n{}\n{}\n{}",
                candidate.head_sha, definition_hash, image, inputs_json
            ))
        });
        let rerun_of = if force_rerun {
            store
                .latest_check_for_identity(
                    &candidate.id,
                    &definition_hash,
                    Some(&raw_image),
                    &inputs_json,
                )
                .map_err(|error| error.to_string())?
                .map(|run| run.id)
        } else {
            None
        };
        let prior = if force_rerun {
            None
        } else if let Some(key) = reuse_key.as_deref() {
            store
                .latest_reusable_check(&candidate.id, key)
                .map_err(|error| error.to_string())?
        } else {
            None
        };
        let run_id = uuid::Uuid::now_v7().to_string();
        let started_ms = now_ms();

        if let Some(source) = prior {
            let source_outcome = effective_outcome(&source).to_string();
            let ok = source_outcome == "passed";
            store
                .insert_check_run(&CheckRunRow {
                    id: run_id.clone(),
                    candidate_id: Some(candidate.id.clone()),
                    session_id: candidate.owner_session_id.clone(),
                    definition_json,
                    definition_hash: Some(definition_hash),
                    execution_image: Some(raw_image.clone()),
                    inputs_json: Some(inputs_json.clone()),
                    reuse_key,
                    outcome: "reused".into(),
                    source_outcome: Some(source_outcome),
                    exit_code: source.exit_code,
                    log: source.log.clone(),
                    duration_ms: source.duration_ms,
                    started_ms,
                    finished_ms: Some(started_ms),
                    rerun_of: None,
                    reused_from_id: Some(source.id.clone()),
                    metadata_json: json!({
                        "reused": true,
                        "source_run_id": source.id,
                        "source_outcome": effective_outcome(&source),
                        "execution_backend": backend.kind(),
                    })
                    .to_string(),
                })
                .map_err(|error| error.to_string())?;
            results.push(CheckResult {
                command: command.clone(),
                ok,
                exit: source.exit_code.and_then(|code| i32::try_from(code).ok()),
                outcome: "reused".into(),
                tail: source.log,
                ms: source.duration_ms.unwrap_or_default().max(0) as u64,
                reused_from: Some(source.id),
            });
            any_reused = true;
            continue;
        }

        let run = CheckRunRow {
            id: run_id.clone(),
            candidate_id: Some(candidate.id.clone()),
            session_id: candidate.owner_session_id.clone(),
            definition_json,
            definition_hash: Some(definition_hash),
            execution_image: Some(raw_image.clone()),
            inputs_json: Some(inputs_json.clone()),
            reuse_key,
            outcome: "running".into(),
            source_outcome: None,
            exit_code: None,
            log: String::new(),
            duration_ms: None,
            started_ms,
            finished_ms: None,
            rerun_of,
            reused_from_id: None,
            metadata_json: json!({
                "reused": false,
                "explicit_rerun": force_rerun,
                "execution_backend": backend.kind(),
                "candidate_tree": candidate.tree_sha,
            })
            .to_string(),
        };
        store.insert_check_run(&run).map_err(|error| error.to_string())?;

        // A runtime volume seeded from the immutable candidate snapshot,
        // scoped to this one check execution: never a host path handed to the
        // runner. Each check gets its own writable copy so its mutations are
        // never visible to another check and never reach the candidate's own
        // read-only evidence.
        let scratch_volume = format!("tracon-check-{run_id}");
        if let Err(error) = backend.import_volume(&scratch_volume, snapshot).await {
            let message = format!("could not stage check workspace: {error}");
            store
                .finish_check_run(
                    &run_id,
                    "interrupted",
                    None,
                    None,
                    &message,
                    Some(0),
                    &json!({ "execution_backend": backend.kind(), "workspace_error": error.to_string() }),
                )
                .map_err(|error| error.to_string())?;
            results.push(CheckResult {
                command: command.clone(),
                ok: false,
                exit: None,
                outcome: "interrupted".into(),
                tail: message,
                ms: 0,
                reused_from: None,
            });
            continue;
        }
        let mount = Mount::volume(scratch_volume, "/work", false);
        let started = std::time::Instant::now();
        let runner_name = format!("tracon-c-{index}-{}", &hash(&run_id)[..12]);
        let cmd = RunnerCommand {
            argv: vec!["sh".into(), "-lc".into(), command.clone()],
            env: Vec::new(),
            mounts: vec![mount],
            workdir: Some("/work".into()),
            name: runner_name.clone(),
            image: None,
        };
        let (outcome, exit, tail_text, metadata) = match tokio::time::timeout(timeout, runner.run_capture(cmd)).await {
            Ok(Ok(output)) => {
                let mut output_text = String::from_utf8_lossy(&output.stdout).into_owned();
                output_text.push_str(&String::from_utf8_lossy(&output.stderr));
                let outcome = if output.status.success() { "passed" } else { "failed" };
                (
                    outcome,
                    output.status.code(),
                    tail(&output_text),
                    json!({ "execution_backend": backend.kind(), "timeout_secs": timeout.as_secs() }),
                )
            }
            Ok(Err(error)) => (
                "interrupted",
                None,
                format!("could not run: {error}"),
                json!({ "execution_backend": backend.kind(), "runner_error": error.to_string() }),
            ),
            Err(_) => {
                let _ = runner.kill(&runner_name).await;
                (
                    "interrupted",
                    None,
                    format!("timed out after {}s", timeout.as_secs()),
                    json!({
                        "execution_backend": backend.kind(),
                        "timeout_secs": timeout.as_secs(),
                        "interrupted": "timeout",
                    }),
                )
            }
        };
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        store
            .finish_check_run(
                &run_id,
                outcome,
                None,
                exit.map(i64::from),
                &tail_text,
                Some(duration_ms),
                &metadata,
            )
            .map_err(|error| error.to_string())?;
        let ok = outcome == "passed";
        results.push(CheckResult {
            command: command.clone(),
            ok,
            exit,
            outcome: outcome.into(),
            tail: tail_text,
            ms: duration_ms.max(0) as u64,
            reused_from: None,
        });
    }

    Ok(CheckReport {
        candidate_id: candidate.id.clone(),
        all_required_passed: !commands.is_empty()
            && results.len() == commands.len()
            && results.iter().all(|result| result.ok),
        required_count: commands.len(),
        results,
        reused: any_reused,
    })
}

/// Revalidate a review immediately before a consequential dispatch. It is the
/// same identity calculation used at execution time, so a changed operator
/// command, dependency input, runtime kind, or immutable image cannot borrow
/// a previous pass. Mutable tags are never selected as reusable evidence.
pub fn review_required_checks_current(
    store: &Store,
    review_id: &str,
    cfg: &Config,
) -> Result<(), String> {
    let required = required_definitions(cfg);
    if required.is_empty() {
        return Ok(());
    }
    let Some(revision) = store
        .latest_review_revision(review_id)
        .map_err(|error| error.to_string())?
    else {
        // Rows written by the pre-evidence schema cannot be given made-up
        // image/input provenance. New submissions always create a revision.
        return Ok(());
    };
    let review = store
        .get_review(review_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "review is missing".to_string())?;
    if revision.head_sha != review.head_sha {
        return Err("review's current source is not the immutable revision that was checked".into());
    }
    let candidate = store
        .candidate(&revision.candidate_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "review candidate is missing".to_string())?;
    if candidate.head_sha != review.head_sha {
        return Err("review candidate does not match the review's current source".into());
    }
    let raw_image = execution_image(cfg);
    let pinned_image = immutable_image_identity(&raw_image);
    let inputs_json =
        serde_json::to_string(&check_inputs(cfg)).map_err(|error| error.to_string())?;
    for command in required {
        let definition_json = serde_json::to_string(&CheckDefinition {
            command: command.clone(),
            timeout_secs: cfg.supervision.timeout_secs.max(1),
        })
        .map_err(|error| error.to_string())?;
        let definition_hash = hash(&definition_json);
        let valid = if let Some(image) = pinned_image.as_ref() {
            let reuse_key = hash(&format!(
                "{}\n{}\n{}\n{}",
                candidate.head_sha, definition_hash, image, inputs_json
            ));
            store
                .latest_reusable_check(&candidate.id, &reuse_key)
                .map_err(|error| error.to_string())?
                .is_some_and(|run| effective_outcome(&run) == "passed")
        } else {
            return Err(format!(
                "required check `{command}` used mutable execution image `{raw_image}`; configure an image digest before approval"
            ));
        };
        if !valid {
            return Err(format!(
                "required check `{command}` has no current passing evidence for candidate {}",
                candidate.head_sha
            ));
        }
    }
    Ok(())
}

fn execution_image(cfg: &Config) -> String {
    match cfg.runtime.kind {
        RuntimeKind::Podman => cfg.boundary.harness_image.clone(),
        RuntimeKind::Kubernetes => cfg.runtime.kubernetes.harness_image.clone(),
    }
}

fn immutable_image_identity(image: &str) -> Option<String> {
    let (_, digest) = image.split_once("@sha256:")?;
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("sha256:{digest}"))
}

fn check_inputs(cfg: &Config) -> serde_json::Value {
    json!({
        "runtime": match cfg.runtime.kind {
            RuntimeKind::Podman => "podman",
            RuntimeKind::Kubernetes => "kubernetes",
        },
        "timeout_secs": cfg.supervision.timeout_secs.max(1),
        "dependency_inputs": cfg.supervision.dependency_inputs,
    })
}

fn effective_outcome(run: &CheckRunRow) -> &str {
    if run.outcome == "reused" {
        run.source_outcome.as_deref().unwrap_or("interrupted")
    } else {
        &run.outcome
    }
}

fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn tail(text: &str) -> String {
    if text.len() <= TAIL_BYTES {
        return text.to_string();
    }
    let mut at = text.len() - TAIL_BYTES;
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    format!("…{}", &text[at..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_digest_images_can_form_a_reuse_identity() {
        assert!(immutable_image_identity("registry/image@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_some());
        assert!(immutable_image_identity("registry/image:latest").is_none());
    }

    #[test]
    fn required_commands_are_operator_configuration_only() {
        let mut cfg = Config::default();
        cfg.supervision.checks = vec!["  cargo test  ".into(), "".into()];
        assert_eq!(required_definitions(&cfg), vec!["cargo test".to_string()]);
    }
}
