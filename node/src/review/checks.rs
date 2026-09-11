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
    let runner = backend.runner(Vec::new());
    // The identity actually confirmed against the runtime for this backend,
    // never the configured string alone: a backend that does not (or
    // cannot) run the configured image must never have its evidence pinned
    // as if it had (`LocalRunner` reports a local, never-pinnable identity;
    // Podman/Kubernetes resolve the real digest they ran).
    let resolved_image = runner.resolved_image().await;
    let recorded_image = resolved_image.clone().unwrap_or_else(|| raw_image.clone());
    let pinned_image = resolved_image.as_deref().and_then(immutable_image_identity);
    let inputs = check_inputs(cfg);
    let inputs_json = serde_json::to_string(&inputs).map_err(|error| error.to_string())?;
    let timeout = Duration::from_secs(cfg.supervision.timeout_secs.max(1));
    let mut results = Vec::with_capacity(commands.len());
    let mut any_reused = false;

    for (index, command) in commands.iter().enumerate() {
        let definition = CheckDefinition {
            command: command.clone(),
            timeout_secs: timeout.as_secs(),
        };
        let definition_json =
            serde_json::to_string(&definition).map_err(|error| error.to_string())?;
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
                    Some(&recorded_image),
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
                    execution_image: Some(recorded_image.clone()),
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
            execution_image: Some(recorded_image.clone()),
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
        store
            .insert_check_run(&run)
            .map_err(|error| error.to_string())?;

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
        let (outcome, exit, tail_text, metadata) = match tokio::time::timeout(
            timeout,
            runner.run_capture(cmd),
        )
        .await
        {
            Ok(Ok(output)) => {
                let mut output_text = String::from_utf8_lossy(&output.stdout).into_owned();
                output_text.push_str(&String::from_utf8_lossy(&output.stderr));
                let outcome = if output.status.success() {
                    "passed"
                } else {
                    "failed"
                };
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
/// `backend` is asked for the same confirmed image identity `run_required`
/// pinned on, never a string reconstructed from configuration alone.
pub async fn review_required_checks_current(
    store: &Store,
    backend: &dyn Backend,
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
        return Err(
            "review's current source is not the immutable revision that was checked".into(),
        );
    }
    let candidate = store
        .candidate(&revision.candidate_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "review candidate is missing".to_string())?;
    if candidate.head_sha != review.head_sha {
        return Err("review candidate does not match the review's current source".into());
    }
    let resolved_image = backend.runner(Vec::new()).resolved_image().await;
    let pinned_image = resolved_image.as_deref().and_then(immutable_image_identity);
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
                "required check `{command}` has no execution image confirmed against the runtime \
                 (reported: {}); configure and verify an image digest before approval",
                resolved_image.as_deref().unwrap_or("none")
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

    /// The container/pod name `run_required` builds for a check
    /// (`tracon-c-{index}-{hash}`) becomes, unmodified apart from
    /// `KubeRunner::run_capture`'s `-{pid}` suffix, both the Kubernetes pod
    /// name and the `tracon.dev/session` label value in `KubeSpec::pod`
    /// (29c4086). Both are capped at 63 characters; confirm the bound holds
    /// even at the largest realistic index and process id.
    #[test]
    fn kube_check_pod_name_fits_the_63_char_label_bound() {
        for index in [0usize, 1, 9, 42, 999] {
            let run_id = uuid::Uuid::now_v7().to_string();
            let runner_name = format!("tracon-c-{index}-{}", &hash(&run_id)[..12]);
            let worst_case = format!("{runner_name}-{}", u32::MAX);
            assert!(
                worst_case.len() <= 63,
                "{worst_case:?} is {} characters, over the Kubernetes label/name limit",
                worst_case.len()
            );
        }
    }

    /// A backend that confirms a real, content-addressed image identity
    /// grants reuse even when the operator's configured string is a mutable
    /// tag; the configured text is never the source of truth.
    #[tokio::test]
    async fn reuse_is_keyed_on_what_the_backend_confirmed_not_a_config_string() {
        use crate::boundary::{Backend, BoundaryError, BoundaryReport};
        use crate::runner::{Mount, Runner, RunnerCommand, RunnerError, Spawned};
        use async_trait::async_trait;
        use std::sync::Arc;

        struct FakeRunner;
        #[async_trait]
        impl Runner for FakeRunner {
            async fn spawn(&self, _cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
                Err(RunnerError::Other("not used by this test".into()))
            }
            async fn run_capture(
                &self,
                _cmd: RunnerCommand,
            ) -> Result<std::process::Output, RunnerError> {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(0),
                        stdout: b"ok".to_vec(),
                        stderr: Vec::new(),
                    })
                }
            }
            async fn kill(&self, _name: &str) -> Result<(), RunnerError> {
                Ok(())
            }
            async fn resolved_image(&self) -> Option<String> {
                Some(
                    "registry/example@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                )
            }
        }

        struct FakeBackend;
        #[async_trait]
        impl Backend for FakeBackend {
            fn kind(&self) -> &'static str {
                "fake"
            }
            async fn setup(&self, _cfg: &Config, _rebuild: bool) -> Result<(), BoundaryError> {
                Ok(())
            }
            async fn check_all(&self, _cfg: &Config, _deep: bool) -> BoundaryReport {
                BoundaryReport { checks: Vec::new() }
            }
            fn runner(&self, _extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
                Arc::new(FakeRunner)
            }
            fn harness_host(&self) -> String {
                "fake".into()
            }
            async fn import_volume(
                &self,
                _volume: &str,
                _source: &std::path::Path,
            ) -> Result<(), BoundaryError> {
                Ok(())
            }
            async fn export_volume(
                &self,
                _volume: &str,
                _destination: &std::path::Path,
            ) -> Result<(), BoundaryError> {
                Ok(())
            }
            fn harness_home(&self) -> String {
                "/home/harness".into()
            }
            async fn reconcile(&self, _names: &[String]) {}
        }

        let store = Store::open_in_memory().unwrap();
        let candidate = CandidateRow {
            id: "cand1".into(),
            head_sha: "sha1".into(),
            tree_sha: Some("tree1".into()),
            channel: "ch".into(),
            owner_session_id: "s1".into(),
            source_kind: "git".into(),
            captured_ms: now_ms(),
            capture_json: "{}".into(),
        };
        store.insert_candidate(&candidate).unwrap();
        let snapshot = std::env::temp_dir().join(format!(
            "tracon-checks-reuse-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&snapshot).unwrap();

        let mut cfg = Config::default();
        cfg.runtime.kind = RuntimeKind::Podman;
        // Deliberately a mutable tag, never pinnable on its own: the backend's
        // confirmed identity is what must grant reuse, not this string.
        cfg.boundary.harness_image = "registry/example:latest".into();
        cfg.supervision.checks = vec!["true".into()];
        let backend = FakeBackend;

        let first = run_required(&backend, &cfg, &store, &candidate, &snapshot, false)
            .await
            .unwrap();
        assert!(first.all_required_passed);
        assert!(
            !first.reused,
            "the first run has no prior evidence to reuse"
        );

        let second = run_required(&backend, &cfg, &store, &candidate, &snapshot, false)
            .await
            .unwrap();
        assert!(
            second.reused,
            "the backend confirmed the same pinned identity across both runs"
        );
        assert_eq!(second.results[0].outcome, "reused");

        let runs = store.check_runs_for_candidate(&candidate.id).unwrap();
        assert!(
            runs.iter().any(|run| run.execution_image.as_deref()
                == Some("registry/example@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
            "the recorded identity is what the backend reported, never the raw config string: {runs:?}"
        );

        let _ = std::fs::remove_dir_all(&snapshot);
    }

    /// `LocalRunner` (and any backend that cannot confirm an image) must
    /// never grant reuse, even when the operator's configuration happens to
    /// contain a digest-shaped string for an image that backend never runs.
    #[tokio::test]
    async fn a_backend_with_no_confirmed_image_is_never_treated_as_pinned() {
        let store = Store::open_in_memory().unwrap();
        let candidate = CandidateRow {
            id: "cand2".into(),
            head_sha: "sha2".into(),
            tree_sha: Some("tree2".into()),
            channel: "ch".into(),
            owner_session_id: "s1".into(),
            source_kind: "git".into(),
            captured_ms: now_ms(),
            capture_json: "{}".into(),
        };
        store.insert_candidate(&candidate).unwrap();
        let snapshot = std::env::temp_dir().join(format!(
            "tracon-checks-local-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&snapshot).unwrap();

        let mut cfg = Config::default();
        cfg.runtime.kind = RuntimeKind::Podman;
        // A digest-shaped string in config, but this backend never runs any
        // image at all — it must not borrow that string's pinning.
        cfg.boundary.harness_image =
            "registry/example@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        cfg.supervision.checks = vec!["true".into()];
        let backend = crate::runner::local::LocalBackend;

        let first = run_required(&backend, &cfg, &store, &candidate, &snapshot, false)
            .await
            .unwrap();
        assert!(first.all_required_passed);
        let second = run_required(&backend, &cfg, &store, &candidate, &snapshot, false)
            .await
            .unwrap();
        assert!(
            !second.reused,
            "a backend with no confirmed image identity must never be treated as pinned"
        );
        let runs = store.check_runs_for_candidate(&candidate.id).unwrap();
        assert!(
            runs.iter().all(|run| run.execution_image.as_deref() == Some("local:direct-execution")),
            "the recorded identity is LocalRunner's own, never the unrelated config string: {runs:?}"
        );

        let _ = std::fs::remove_dir_all(&snapshot);
    }
}
