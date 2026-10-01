//! Candidate-bound deterministic checks. Required commands originate only in
//! operator configuration, and every execution mounts a node-captured Git tree
//! read-only rather than the agent's mutable worktree.
//!
//! Which checks, and what they run *in*, is the repository's `[[repo]]` entry:
//! its own commands and toolchain image, or the node-wide checks and the
//! harness image where the operator has named none — and evidence is pinned
//! only on the identity the runtime confirmed for that image. A command the
//! image has no tool for is `not_runnable`: the environment's failing, never
//! the candidate's.
//!
//! A repository that names `prepare` commands has them run first, on the same
//! copy the check then runs against, with the repository's dependency cache
//! writable and its named hosts reachable. The check itself gets the cache
//! read-only and no egress: what it reads is what preparation fetched.

use std::{path::Path, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    boundary::Backend,
    config::{Config, RuntimeKind},
    environment::{cache_env, environment_for, origin_repo, scoped_egress, RepoEnvironment},
    runner::{Mount, Runner, RunnerCommand, RunnerError},
    store::{now_ms, CandidateRow, CheckRunRow, Store},
};

/// How much of a check's combined output stays in its authoritative record.
const TAIL_BYTES: usize = 4096;

/// How often a running check asks whether it should still be running. A check
/// is a subprocess in another namespace, so there is nothing to await on the
/// cancellation itself; the session row is the authority and this is how
/// often it is consulted.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// Whether the work these checks were started for is still wanted.
///
/// Checks outlive the call that asked for them — they are minutes of
/// subprocess, and the operator can pause or stop the session, or decide the
/// review, at any point during them. The runner cannot be asked politely, so
/// the authority is consulted on a poll and the process is killed.
pub trait Cancel: Send + Sync {
    /// Why the run must stop now, or `None` to carry on.
    fn cancelled(&self) -> Option<String>;
}

/// Never cancelled: for callers with nothing that can withdraw the request.
pub struct RunToCompletion;

impl Cancel for RunToCompletion {
    fn cancelled(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckResult {
    pub command: String,
    pub ok: bool,
    pub exit: Option<i32>,
    /// `passed`, `failed`, `not_runnable`, `interrupted`, `cancelled`, or
    /// `reused`.
    pub outcome: String,
    /// The last few KiB of stdout+stderr, or the retained output of reused
    /// evidence.
    pub tail: String,
    pub ms: u64,
    pub reused_from: Option<String>,
    /// For a `not_runnable` outcome: the tool the shell said it could not find,
    /// as the shell reported it. Best effort and never a claim the node
    /// verified anything — a compound command can exit 127 on its second word,
    /// and a script can exit 127 for its own reasons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_tool: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub candidate_id: String,
    pub results: Vec<CheckResult>,
    pub required_count: usize,
    pub all_required_passed: bool,
    pub reused: bool,
    /// What the checks ran in — the identity the runtime confirmed where it
    /// could — and where that image came from. A check that could not run is a
    /// fact about this image, so a reader is never left guessing which one.
    pub image: String,
    pub image_source: &'static str,
    /// Why the run stopped early, when it did. A cancelled run has no verdict
    /// to give: it is neither a pass nor a failure of the candidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CheckDefinition {
    command: String,
    timeout_secs: u64,
}

/// The trusted required definitions. Candidate files never alter this list.
///
/// This is the node-wide list. A repository's `[[repo]]` entry may name its
/// own in its place; `candidate_environment` is what a candidate actually
/// answers to.
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
    cancel: &dyn Cancel,
) -> Result<CheckReport, String> {
    // The one place a check run may build: the image for the default branch's
    // Dockerfile as it is now, so a check never runs in an image a merged
    // change has since replaced.
    let repo = origin_repo(store, &candidate.owner_session_id)?;
    let environment = crate::repo_image::environment(backend, cfg, store, repo.as_deref()).await;
    let commands = environment.checks.clone();
    let raw_image = environment.image.clone();
    let runner = backend.runner(Vec::new());
    // The identity actually confirmed against the runtime for this backend and
    // *this image*, never the configured string alone: a backend that does not
    // (or cannot) run the configured image must never have its evidence pinned
    // as if it had (`LocalRunner` reports a local, never-pinnable identity;
    // Podman/Kubernetes resolve the real digest they ran).
    let resolved_image = runner.resolved_image(Some(&raw_image)).await;
    let recorded_image = resolved_image.clone().unwrap_or_else(|| raw_image.clone());
    let pinned_image = resolved_image.as_deref().and_then(immutable_image_identity);
    let inputs = check_inputs(cfg, &environment);
    let inputs_json = serde_json::to_string(&inputs).map_err(|error| error.to_string())?;
    let timeout = Duration::from_secs(environment.timeout_secs);
    let mut results = Vec::with_capacity(commands.len());
    let mut any_reused = false;
    let mut cancelled = None;

    for (index, command) in commands.iter().enumerate() {
        // Nothing new is started for work that has been withdrawn.
        if let Some(reason) = cancel.cancelled() {
            results.push(cancelled_result(command, &reason, 0));
            cancelled = Some(reason);
            break;
        }
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
                    command: Some(command.clone()),
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
                        "candidate_tree": candidate.tree_sha,
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
                missing_tool: None,
            });
            any_reused = true;
            continue;
        }

        let run = CheckRunRow {
            id: run_id.clone(),
            candidate_id: Some(candidate.id.clone()),
            session_id: candidate.owner_session_id.clone(),
            command: Some(command.clone()),
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
            release_check_volume(backend, &scratch_volume).await;
            let message = format!("could not stage check workspace: {error}");
            store
                .finish_check_run(
                    &run_id,
                    "interrupted",
                    None,
                    None,
                    &message,
                    Some(0),
                    &with_candidate_tree(
                        candidate,
                        json!({ "execution_backend": backend.kind(), "workspace_error": error.to_string() }),
                    ),
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
                missing_tool: None,
            });
            continue;
        }
        let started = std::time::Instant::now();
        let runner_name = format!("tracon-c-{index}-{}", &hash(&run_id)[..12]);
        // What the runner will actually call this execution, not what it was
        // asked to: a kill has to name the same thing the runtime named, or
        // it stops nothing at all.
        let running_name = runner.capture_name(&runner_name);
        // One deadline for the whole execution: a preparation that eats the
        // budget leaves none for the check, rather than doubling it.
        let deadline = tokio::time::Instant::now() + timeout;
        let mut env = Vec::new();
        let mut mounts = vec![Mount::volume(scratch_volume.clone(), "/work", false)];
        let mut unprepared = None;
        if !environment.prepare.is_empty() {
            unprepared = prepare_check_copy(
                backend,
                runner.as_ref(),
                &environment,
                &scratch_volume,
                &runner_name,
                deadline,
                cancel,
            )
            .await
            .err();
            // The check reads what preparation fetched and can add nothing to
            // it: candidate code runs here, and a cache it could write is a
            // cache the next candidate's evidence would be built on.
            env = cache_env();
            env.push(("CARGO_NET_OFFLINE".into(), "true".into()));
            mounts.push(Mount::volume(
                environment.cache_volume.clone(),
                "/cache",
                true,
            ));
        }
        let cmd = RunnerCommand {
            argv: vec!["sh".into(), "-lc".into(), command.clone()],
            env,
            mounts,
            workdir: Some("/work".into()),
            name: runner_name.clone(),
            image: Some(raw_image.clone()),
            expose: None,
        };
        let finished = match unprepared {
            Some(stopped) => Err(stopped),
            None => until_stopped(
                runner.run_capture(cmd),
                deadline,
                cancel,
                Some((runner.as_ref(), &running_name)),
            )
            .await
            .map_err(|stop| match stop {
                Some(reason) => Stopped::Cancelled {
                    reason,
                    killed: running_name.clone(),
                },
                None => Stopped::TimedOut,
            }),
        };
        // Only the output is kept; the copy it ran against is nobody's now.
        release_check_volume(backend, &scratch_volume).await;
        // Cancelled: the runtime is stopped before anything is written down,
        // so the record is never ahead of what is actually running.
        if let Err(Stopped::Cancelled { reason, killed }) = finished {
            let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
            store
                .cancel_running_check(
                    &run_id,
                    &format!("cancelled: {reason}"),
                    &with_candidate_tree(
                        candidate,
                        json!({
                            "execution_backend": backend.kind(),
                            "cancelled": reason,
                            "killed": killed,
                        }),
                    ),
                )
                .map_err(|error| error.to_string())?;
            results.push(cancelled_result(
                command,
                &reason,
                duration_ms.max(0) as u64,
            ));
            cancelled = Some(reason);
            break;
        }
        let (outcome, exit, tail_text, metadata) = match finished {
            Ok(Ok(output)) => {
                let mut output_text = String::from_utf8_lossy(&output.stdout).into_owned();
                output_text.push_str(&String::from_utf8_lossy(&output.stderr));
                let code = output.status.code();
                // 127 is the shell saying it could not find the command. That
                // is the check environment's failing, not the candidate's, so
                // it is neither a pass nor a failure the agent is told to fix
                // — and `latest_reusable_check` does not accept the outcome,
                // so it can never become evidence either.
                let missing = (!output.status.success() && code == Some(127))
                    .then(|| missing_tool(command, &output_text));
                let outcome = match (output.status.success(), &missing) {
                    (true, _) => "passed",
                    (false, Some(_)) => "not_runnable",
                    (false, None) => "failed",
                };
                let mut metadata = json!({
                    "execution_backend": backend.kind(),
                    "timeout_secs": timeout.as_secs(),
                });
                if let Some(tool) = &missing {
                    metadata["missing_tool"] = json!(tool);
                    metadata["execution_image_source"] = json!(environment.image_source);
                }
                if !environment.prepare.is_empty() {
                    metadata["prepared_with"] = json!(environment.prepare);
                    metadata["cache_volume"] = json!(environment.cache_volume);
                }
                (outcome, code, tail(&output_text), metadata)
            }
            Ok(Err(error)) => (
                "interrupted",
                None,
                format!("could not run: {error}"),
                json!({ "execution_backend": backend.kind(), "runner_error": error.to_string() }),
            ),
            // Nothing about the candidate was established: the check never
            // started. `interrupted` is never a pass and never reusable.
            Err(Stopped::Unprepared(message)) => (
                "interrupted",
                None,
                message,
                json!({
                    "execution_backend": backend.kind(),
                    "interrupted": "preparation",
                    "prepared_with": environment.prepare,
                }),
            ),
            // The deadline arm already killed the runtime.
            Err(_) => (
                "interrupted",
                None,
                format!("timed out after {}s", timeout.as_secs()),
                json!({
                    "execution_backend": backend.kind(),
                    "timeout_secs": timeout.as_secs(),
                    "interrupted": "timeout",
                }),
            ),
        };
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let missing_tool = metadata
            .get("missing_tool")
            .and_then(|tool| tool.as_str())
            .map(str::to_string);
        let recorded = store
            .finish_check_run(
                &run_id,
                outcome,
                None,
                exit.map(i64::from),
                &tail_text,
                Some(duration_ms),
                &with_candidate_tree(candidate, metadata),
            )
            .map_err(|error| error.to_string())?;
        if !recorded {
            // The row left `running` while this execution was in flight —
            // cancelled by the controller between the process exiting and
            // this write. What was written down first stands; a late result
            // never becomes the outcome, and never a pass.
            let settled = store
                .check_run(&run_id)
                .map_err(|error| error.to_string())?
                .map(|run| run.outcome)
                .unwrap_or_else(|| "cancelled".into());
            tracing::warn!(
                run = %run_id, %outcome, %settled,
                "dropped a check result that arrived after the run was settled"
            );
            results.push(cancelled_result(
                command,
                &format!("result arrived after the run was recorded {settled}"),
                duration_ms.max(0) as u64,
            ));
            cancelled = Some(settled);
            break;
        }
        let ok = outcome == "passed";
        results.push(CheckResult {
            command: command.clone(),
            ok,
            exit,
            outcome: outcome.into(),
            tail: tail_text,
            ms: duration_ms.max(0) as u64,
            reused_from: None,
            missing_tool,
        });
    }

    Ok(CheckReport {
        candidate_id: candidate.id.clone(),
        // A cancelled run verifies nothing: it is neither the candidate's
        // pass nor its failure, and `results` is deliberately short of
        // `commands` so no later reader can mistake it for a complete one.
        all_required_passed: cancelled.is_none()
            && !commands.is_empty()
            && results.len() == commands.len()
            && results.iter().all(|result| result.ok),
        required_count: commands.len(),
        results,
        reused: any_reused,
        image: recorded_image,
        image_source: environment.image_source,
        cancelled,
    })
}

/// The tool the shell said was missing, read out of the shell's own wording:
/// dash says `sh: 1: just: not found` and bash says
/// `bash: line 1: just: command not found`. Neither is a contract, so the
/// command's first word is the fallback — better a name that is merely likely
/// than a message that names nothing at all.
fn missing_tool(command: &str, output: &str) -> String {
    for line in output.lines().rev() {
        let line = line.trim_end();
        for suffix in [": command not found", ": not found"] {
            if let Some(head) = line.strip_suffix(suffix) {
                if let Some(name) = head.rsplit(": ").next() {
                    let name = name.trim();
                    if !name.is_empty() {
                        return name.to_string();
                    }
                }
            }
        }
    }
    command
        .split_whitespace()
        .next()
        .unwrap_or(command)
        .to_string()
}

/// How an execution ended without its command finishing.
enum Stopped {
    Cancelled {
        reason: String,
        killed: String,
    },
    TimedOut,
    /// Preparation did not complete, so the check was never started.
    Unprepared(String),
}

/// Drive `work` until it finishes, the deadline passes (`Err(None)`), or the
/// work is no longer wanted (`Err(Some(reason))`). Both ways of stopping kill
/// the named runtime *while `work` is still held*: an identity released first
/// could be reused by the runtime before the kill names it.
async fn until_stopped<T>(
    work: impl std::future::Future<Output = T>,
    deadline: tokio::time::Instant,
    cancel: &dyn Cancel,
    kill: Option<(&dyn Runner, &str)>,
) -> Result<T, Option<String>> {
    tokio::pin!(work);
    let mut poll = tokio::time::interval(CANCEL_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let stopped = loop {
        tokio::select! {
            finished = &mut work => return Ok(finished),
            _ = tokio::time::sleep_until(deadline) => break None,
            _ = poll.tick() => {
                if let Some(reason) = cancel.cancelled() {
                    break Some(reason);
                }
            }
        }
    };
    if let Some((runner, name)) = kill {
        let _ = runner.kill(name).await;
    }
    Err(stopped)
}

/// Run the repository's `prepare` commands against one check's own copy of the
/// candidate: same image, the dependency cache writable, and the scoped egress
/// gateway open to the repository's hosts for exactly as long as they run.
///
/// The commands are the operator's; what they execute may not be — an install
/// runs the candidate's own lifecycle scripts unless the command says
/// otherwise — which is why this is the only step the cache is writable in and
/// why it holds no credential.
async fn prepare_check_copy(
    backend: &dyn Backend,
    runner: &dyn Runner,
    environment: &RepoEnvironment,
    scratch_volume: &str,
    runner_name: &str,
    deadline: tokio::time::Instant,
    cancel: &dyn Cancel,
) -> Result<(), Stopped> {
    let stopped = |stop: Option<String>, killed: &str| match stop {
        Some(reason) => Stopped::Cancelled {
            reason,
            killed: killed.to_string(),
        },
        None => Stopped::TimedOut,
    };
    // This preparation's own grant: nothing to wait for, and nothing another
    // run's hosts can widen.
    let egress = scoped_egress(backend, &environment.egress).map_err(Stopped::Unprepared)?;
    for (step, command) in environment.prepare.iter().enumerate() {
        let name = format!("{runner_name}-p{step}");
        let running = runner.capture_name(&name);
        let mut env = cache_env();
        env.extend(egress.env.iter().cloned());
        let cmd = RunnerCommand {
            argv: vec!["sh".into(), "-lc".into(), command.clone()],
            env,
            mounts: vec![
                Mount::volume(scratch_volume.to_string(), "/work", false),
                Mount::volume(environment.cache_volume.clone(), "/cache", false),
            ],
            workdir: Some("/work".into()),
            name,
            image: Some(environment.image.clone()),
            expose: None,
        };
        let finished: Result<std::process::Output, RunnerError> = until_stopped(
            runner.run_capture(cmd),
            deadline,
            cancel,
            Some((runner, &running)),
        )
        .await
        .map_err(|stop| stopped(stop, &running))?;
        let output = finished.map_err(|error| {
            Stopped::Unprepared(format!("could not run preparation `{command}`: {error}"))
        })?;
        if !output.status.success() {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            return Err(Stopped::Unprepared(format!(
                "preparation `{command}` failed (exit {}) in {} ({}); the check was not run.\n\n{}",
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "none".into()),
                environment.image,
                environment.image_source,
                tail(&text)
            )));
        }
    }
    Ok(())
}

/// What this candidate answers to: the `[[repo]]` entry for the repository its
/// session was working in, over the node-wide defaults. Resolved once per
/// report, from the candidate, so the run path and the approval path cannot
/// disagree about which checks ran or what they ran in.
pub fn candidate_environment(
    store: &Store,
    cfg: &Config,
    candidate: &CandidateRow,
) -> Result<RepoEnvironment, String> {
    let repo = origin_repo(store, &candidate.owner_session_id)?;
    Ok(environment_for(cfg, store, repo.as_deref()))
}

/// Every record a check settles as names the tree it ran against. The row a
/// check finishes as replaces the one it started as, so the candidate's tree is
/// written again here: an outcome that does not say which bytes produced it
/// cannot be audited against the candidate it is offered as evidence for.
fn with_candidate_tree(
    candidate: &CandidateRow,
    mut metadata: serde_json::Value,
) -> serde_json::Value {
    metadata["candidate_tree"] = json!(candidate.tree_sha);
    metadata
}

/// The result a cancelled execution contributes: never ok, never reusable,
/// and carrying the reason the operator will read.
/// Best effort: a runtime still releasing a killed execution's mount refuses
/// the removal, and `tracon gc` takes the volume later.
async fn release_check_volume(backend: &dyn crate::boundary::Backend, volume: &str) {
    if let Err(error) = backend.remove_volume(volume).await {
        tracing::debug!(%volume, %error, "check volume left for a later sweep");
    }
}

fn cancelled_result(command: &str, reason: &str, ms: u64) -> CheckResult {
    CheckResult {
        command: command.to_string(),
        ok: false,
        exit: None,
        outcome: "cancelled".into(),
        tail: format!("cancelled: {reason}"),
        ms,
        reused_from: None,
        missing_tool: None,
    }
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
    let Some(revision) = store
        .latest_review_revision(review_id)
        .map_err(|error| error.to_string())?
    else {
        // Rows written by the pre-evidence schema cannot be given made-up
        // image/input provenance. New submissions always create a revision.
        return Ok(());
    };
    let candidate = store
        .candidate(&revision.candidate_id)
        .map_err(|error| error.to_string())?;
    // The candidate names the repository, and the repository names the checks.
    // One the store no longer has answers to the node-wide list.
    let environment = match &candidate {
        Some(candidate) => candidate_environment(store, cfg, candidate)?,
        None => environment_for(cfg, store, None),
    };
    if environment.checks.is_empty() {
        return Ok(());
    }
    let review = store
        .get_review(review_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "review is missing".to_string())?;
    if revision.head_sha != review.head_sha {
        return Err(
            "review's current source is not the immutable revision that was checked".into(),
        );
    }
    let candidate = candidate.ok_or_else(|| "review candidate is missing".to_string())?;
    if candidate.head_sha != review.head_sha {
        return Err("review candidate does not match the review's current source".into());
    }
    let resolved_image = backend
        .runner(Vec::new())
        .resolved_image(Some(&environment.image))
        .await;
    let pinned_image = resolved_image.as_deref().and_then(immutable_image_identity);
    let inputs_json = serde_json::to_string(&check_inputs(cfg, &environment))
        .map_err(|error| error.to_string())?;
    for command in &environment.checks {
        let definition_json = serde_json::to_string(&CheckDefinition {
            command: command.clone(),
            timeout_secs: environment.timeout_secs,
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

fn immutable_image_identity(image: &str) -> Option<String> {
    let (_, digest) = image.split_once("@sha256:")?;
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("sha256:{digest}"))
}

/// Everything besides the command and the image that decides what a check
/// established. Preparation is part of it only where a repository names some:
/// a node with none keeps the identity its existing evidence was keyed on.
fn check_inputs(cfg: &Config, environment: &RepoEnvironment) -> serde_json::Value {
    let mut inputs = json!({
        "runtime": match cfg.runtime.kind {
            RuntimeKind::Podman => "podman",
            RuntimeKind::Kubernetes => "kubernetes",
        },
        "timeout_secs": environment.timeout_secs,
        "dependency_inputs": cfg.supervision.dependency_inputs,
    });
    if !environment.prepare.is_empty() {
        inputs["prepare"] = json!(environment.prepare);
        inputs["egress"] = json!(environment.egress);
    }
    inputs
}

/// What a run says about the candidate. A `reused` row is a pointer at the run
/// that actually executed, so its source's outcome is the truth it carries.
pub fn effective_outcome(run: &CheckRunRow) -> &str {
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

    /// The missing tool is read out of the shell's own report, because that is
    /// the only place it exists — and the command's first word when the shell
    /// worded it some third way, so the message always names something.
    #[test]
    fn the_missing_tool_comes_from_whichever_shell_reported_it() {
        assert_eq!(
            missing_tool("just check", "sh: 1: just: not found\n"),
            "just"
        );
        assert_eq!(
            missing_tool("just check", "bash: line 1: just: command not found\n"),
            "just"
        );
        // A compound command 127s on a later word; the shell names that word,
        // not the first one.
        assert_eq!(
            missing_tool("cd spa && bun run check", "sh: 1: bun: not found\n"),
            "bun"
        );
        // An exit 127 the shell said nothing about: the command's own first
        // word, which is a guess and is worded as the shell's report, never as
        // something the node established.
        assert_eq!(missing_tool("just check", "boom\n"), "just");
        assert_eq!(missing_tool("just check", ""), "just");
    }

    fn session_row(id: &str, repo: &str) -> crate::store::SessionRow {
        crate::store::SessionRow {
            id: id.into(),
            node_id: "n".into(),
            channel: "ch".into(),
            work_item_id: None,
            repo_path: repo.into(),
            worktree_path: None,
            branch: "b".into(),
            harness_id: "claude".into(),
            harness_version: "1".into(),
            harness_agent: None,
            harness_found: None,
            harness_protocol: None,
            harness_session_id: None,
            container_name: None,
            model: "m".into(),
            project_id: None,
            phase: "execute".into(),
            policy_version: None,
            manifest_digest: None,
            review_id: None,
            budget_tokens: 0,
            tokens_used: 0,
            cost_usd: None,
            context_used: None,
            context_size: None,
            state: "closed".into(),
            end_reason: None,
            last_error: None,
            turn_active: 0,
            draft: None,
            draft_updated_ms: None,
            created_ms: 0,
            started_mono_ms: None,
            ended_mono_ms: None,
            updated_ms: 0,
            archived_ms: None,
            legacy_ms: None,
            parent_session: None,
            continued_from: None,
        }
    }

    /// A store with the one node every test session belongs to.
    fn store_with_node() -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .put_node(
                &crate::store::NodeRow::from_json(
                    &json!({ "id": "n", "harness": { "id": "claude" } }),
                )
                .unwrap(),
            )
            .unwrap();
        store
    }

    /// A session resumed on a workspace names it, not a repository; its checks
    /// still belong to the repository the workspace was imported from, or a
    /// resumed session would lose its toolchain image and run checks in the
    /// harness image.
    #[test]
    fn a_resumed_session_is_traced_back_to_its_repository() {
        let store = store_with_node();
        store
            .insert_session(&session_row("first", "/src/app"))
            .unwrap();
        store
            .insert_session(&session_row("resumed", "workspace://first"))
            .unwrap();
        store
            .insert_session(&session_row("again", "workspace://resumed"))
            .unwrap();
        store
            .insert_session(&session_row("loop", "workspace://loop"))
            .unwrap();
        store.insert_session(&session_row("external", "")).unwrap();
        let origin = |id: &str| origin_repo(&store, id).unwrap();
        assert_eq!(origin("first"), Some("/src/app".into()));
        assert_eq!(origin("resumed"), Some("/src/app".into()));
        assert_eq!(origin("again"), Some("/src/app".into()));
        assert_eq!(origin("loop"), None);
        assert_eq!(origin("external"), None);
        assert_eq!(origin("gone"), None);
    }

    /// `not_runnable` must never become reusable evidence: nothing about the
    /// candidate was established, so there is nothing for a later submission to
    /// borrow. It holds by construction — `latest_reusable_check` accepts three
    /// outcomes and this is not one — and this test is what keeps it true if
    /// that query is ever widened.
    #[test]
    fn a_check_that_could_not_run_is_never_reusable_evidence() {
        let store = Store::open_in_memory().unwrap();
        let candidate = CandidateRow {
            id: "cand3".into(),
            head_sha: "sha3".into(),
            tree_sha: Some("tree3".into()),
            channel: "ch".into(),
            owner_session_id: "s1".into(),
            source_kind: "git".into(),
            captured_ms: now_ms(),
            capture_json: "{}".into(),
        };
        store.insert_candidate(&candidate).unwrap();
        let run = |id: &str| CheckRunRow {
            id: id.into(),
            candidate_id: Some(candidate.id.clone()),
            session_id: candidate.owner_session_id.clone(),
            command: Some("just check".into()),
            definition_json: "{}".into(),
            definition_hash: Some("def".into()),
            execution_image: Some("registry/example@sha256:aaaa".into()),
            inputs_json: Some("{}".into()),
            reuse_key: Some("key".into()),
            outcome: "running".into(),
            source_outcome: None,
            exit_code: None,
            log: String::new(),
            duration_ms: None,
            started_ms: now_ms(),
            finished_ms: None,
            rerun_of: None,
            reused_from_id: None,
            metadata_json: "{}".into(),
        };

        store.insert_check_run(&run("r1")).unwrap();
        store
            .finish_check_run(
                "r1",
                "not_runnable",
                None,
                Some(127),
                "sh: 1: just: not found",
                Some(1),
                &json!({ "missing_tool": "just" }),
            )
            .unwrap();
        assert!(
            store
                .latest_reusable_check(&candidate.id, "key")
                .unwrap()
                .is_none(),
            "a check that could not run must never be offered as evidence"
        );

        // The same row settled as a real failure *is* reusable, so the
        // assertion above is about the outcome and not about the query.
        store.insert_check_run(&run("r2")).unwrap();
        store
            .finish_check_run("r2", "failed", None, Some(1), "nope", Some(1), &json!({}))
            .unwrap();
        assert!(store
            .latest_reusable_check(&candidate.id, "key")
            .unwrap()
            .is_some());
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
            async fn resolved_image(&self, _image: Option<&str>) -> Option<String> {
                Some(
                    "registry/example@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                )
            }
        }

        #[derive(Default)]
        struct FakeBackend {
            removed: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait]
        impl Backend for FakeBackend {
            fn kind(&self) -> &'static str {
                "fake"
            }
            async fn remove_volume(&self, volume: &str) -> Result<(), BoundaryError> {
                self.removed.lock().unwrap().push(volume.to_string());
                Ok(())
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
            fn runner_for(&self, _harness_id: &str, _extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
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
        let backend = FakeBackend::default();

        let first = run_required(
            &backend,
            &cfg,
            &store,
            &candidate,
            &snapshot,
            false,
            &RunToCompletion,
        )
        .await
        .unwrap();
        assert!(first.all_required_passed);
        assert!(
            !first.reused,
            "the first run has no prior evidence to reuse"
        );
        {
            let removed = backend.removed.lock().unwrap();
            assert_eq!(
                removed.len(),
                1,
                "an executed check releases its volume: {removed:?}"
            );
            assert!(removed[0].starts_with("tracon-check-"));
        }

        let second = run_required(
            &backend,
            &cfg,
            &store,
            &candidate,
            &snapshot,
            false,
            &RunToCompletion,
        )
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

        let first = run_required(
            &backend,
            &cfg,
            &store,
            &candidate,
            &snapshot,
            false,
            &RunToCompletion,
        )
        .await
        .unwrap();
        assert!(first.all_required_passed);
        let second = run_required(
            &backend,
            &cfg,
            &store,
            &candidate,
            &snapshot,
            false,
            &RunToCompletion,
        )
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

    /// What a recording backend saw, in the order it saw it.
    #[derive(Default)]
    struct Recorded {
        log: std::sync::Mutex<Vec<String>>,
        commands: std::sync::Mutex<Vec<RunnerCommand>>,
        /// Exit status handed to a preparation command.
        prepare_exit: i32,
    }

    struct RecordingRunner(std::sync::Arc<Recorded>);

    #[async_trait::async_trait]
    impl Runner for RecordingRunner {
        async fn spawn(&self, _cmd: RunnerCommand) -> Result<crate::runner::Spawned, RunnerError> {
            Err(RunnerError::Other("not used by these tests".into()))
        }
        async fn run_capture(
            &self,
            cmd: RunnerCommand,
        ) -> Result<std::process::Output, RunnerError> {
            use std::os::unix::process::ExitStatusExt;
            let preparing = cmd
                .mounts
                .iter()
                .any(|m| m.target == "/cache" && !m.read_only);
            let exit = if preparing { self.0.prepare_exit } else { 0 };
            self.0
                .log
                .lock()
                .unwrap()
                .push(format!("run {}", cmd.argv[2]));
            self.0.commands.lock().unwrap().push(cmd);
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(exit << 8),
                stdout: Vec::new(),
                stderr: if exit == 0 {
                    Vec::new()
                } else {
                    b"error: 403 Forbidden".to_vec()
                },
            })
        }
        async fn kill(&self, _name: &str) -> Result<(), RunnerError> {
            Ok(())
        }
        async fn resolved_image(&self, image: Option<&str>) -> Option<String> {
            image.map(str::to_string)
        }
    }

    struct RecordingBackend(std::sync::Arc<Recorded>);

    #[async_trait::async_trait]
    impl Backend for RecordingBackend {
        fn kind(&self) -> &'static str {
            "recording"
        }
        async fn setup(
            &self,
            _cfg: &Config,
            _rebuild: bool,
        ) -> Result<(), crate::boundary::BoundaryError> {
            Ok(())
        }
        async fn check_all(&self, _cfg: &Config, _deep: bool) -> crate::boundary::BoundaryReport {
            crate::boundary::BoundaryReport { checks: Vec::new() }
        }
        fn runner(&self, _extra_mounts: Vec<Mount>) -> std::sync::Arc<dyn Runner> {
            std::sync::Arc::new(RecordingRunner(self.0.clone()))
        }
        fn runner_for(
            &self,
            _harness_id: &str,
            _extra_mounts: Vec<Mount>,
        ) -> std::sync::Arc<dyn Runner> {
            std::sync::Arc::new(RecordingRunner(self.0.clone()))
        }
        fn harness_host(&self) -> String {
            "gw".into()
        }
        async fn import_volume(
            &self,
            _volume: &str,
            _source: &Path,
        ) -> Result<(), crate::boundary::BoundaryError> {
            Ok(())
        }
        async fn export_volume(
            &self,
            _volume: &str,
            _destination: &Path,
        ) -> Result<(), crate::boundary::BoundaryError> {
            Ok(())
        }
        fn harness_home(&self) -> String {
            "/home/harness".into()
        }
        async fn reconcile(&self, _names: &[String]) {}
        fn egress_grant(
            &self,
            spec: crate::gateway::proxy::GrantSpec,
        ) -> Result<crate::boundary::EgressGrant, crate::boundary::BoundaryError> {
            let recorded = self.0.clone();
            recorded
                .log
                .lock()
                .unwrap()
                .push(format!("open {}", spec.hosts.join(",")));
            Ok(crate::boundary::EgressGrant::new(
                "http://gw:8890".into(),
                spec.client,
                "token".into(),
                move || recorded.log.lock().unwrap().push("close".into()),
            ))
        }
    }

    const TOOLCHAIN: &str =
        "localhost/tc@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    /// A candidate from a session in `/src/app`, on a node whose `[[repo]]`
    /// entry for it names an image, its own check, a preparation and the
    /// registry that preparation needs.
    fn prepared_repo(id: &str) -> (Store, Config, CandidateRow) {
        let store = store_with_node();
        store
            .insert_session(&session_row("s-app", "/src/app"))
            .unwrap();
        let candidate = CandidateRow {
            id: id.into(),
            head_sha: format!("sha-{id}"),
            tree_sha: Some(format!("tree-{id}")),
            channel: "ch".into(),
            owner_session_id: "s-app".into(),
            source_kind: "git".into(),
            captured_ms: now_ms(),
            capture_json: "{}".into(),
        };
        store.insert_candidate(&candidate).unwrap();
        let mut cfg = Config::default();
        cfg.runtime.kind = RuntimeKind::Podman;
        cfg.supervision.checks = vec!["node-wide check".into()];
        cfg.repo = vec![crate::config::Repo {
            path: "/src/app".into(),
            image: Some(TOOLCHAIN.into()),
            checks: Some(vec!["just check".into()]),
            timeout_secs: Some(1800),
            prepare: vec!["bun install --frozen-lockfile".into()],
            egress: vec!["npm".into()],
            ..Default::default()
        }];
        (store, cfg, candidate)
    }

    /// A repository's entry decides which checks run and what happens before
    /// them. Preparation gets the cache writable and the scoped gateway open
    /// to exactly the repository's hosts; the gateway is closed again before
    /// the check starts, and the check gets the same cache read-only with
    /// nothing pointing it at the scoped proxy.
    #[tokio::test]
    async fn preparation_fetches_with_egress_and_the_check_runs_without_it() {
        let (store, cfg, candidate) = prepared_repo("prep1");
        let recorded = std::sync::Arc::new(Recorded::default());
        let backend = RecordingBackend(recorded.clone());
        let snapshot = std::env::temp_dir();

        let report = run_required(
            &backend,
            &cfg,
            &store,
            &candidate,
            &snapshot,
            false,
            &RunToCompletion,
        )
        .await
        .unwrap();
        assert!(report.all_required_passed, "{:?}", report.results);
        assert_eq!(report.results.len(), 1);
        assert_eq!(
            report.results[0].command, "just check",
            "the repository's own check, never the node-wide one"
        );
        assert_eq!(report.image, TOOLCHAIN);
        assert_eq!(report.image_source, "repository toolchain");

        assert_eq!(
            *recorded.log.lock().unwrap(),
            [
                "open registry.npmjs.org",
                "run bun install --frozen-lockfile",
                "close",
                "run just check",
            ]
        );
        {
            let commands = recorded.commands.lock().unwrap();
            let (prepare, check) = (&commands[0], &commands[1]);
            let var = |cmd: &RunnerCommand, name: &str| {
                cmd.env
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            };
            for cmd in [prepare, check] {
                assert_eq!(cmd.image.as_deref(), Some(TOOLCHAIN));
                assert_eq!(
                    var(cmd, "BUN_INSTALL_CACHE_DIR").as_deref(),
                    Some("/cache/bun")
                );
            }
            assert_eq!(
                prepare.mounts[0].volume, check.mounts[0].volume,
                "preparation installs into the copy the check then runs against"
            );
            assert!(!prepare.mounts[1].read_only && check.mounts[1].read_only);
            assert_eq!(prepare.mounts[1].volume, check.mounts[1].volume);
            assert!(prepare.mounts[1].volume.starts_with("tracon-cache-"));
            // Its own credentials, in both spellings of the variable.
            assert_eq!(
                var(prepare, "HTTPS_PROXY").as_deref(),
                Some("http://prepare:token@gw:8890")
            );
            assert_eq!(
                var(prepare, "https_proxy").as_deref(),
                Some("http://prepare:token@gw:8890")
            );
            assert_eq!(var(check, "HTTPS_PROXY"), None);
            assert_eq!(var(check, "CARGO_NET_OFFLINE").as_deref(), Some("true"));
        }

        // The same candidate again borrows the evidence: nothing is prepared
        // for a check that does not run.
        let again = run_required(
            &backend,
            &cfg,
            &store,
            &candidate,
            &snapshot,
            false,
            &RunToCompletion,
        )
        .await
        .unwrap();
        assert!(again.reused);
        assert_eq!(recorded.commands.lock().unwrap().len(), 2);

        // A changed preparation is a changed check: the pass is not borrowed.
        let mut changed = cfg.clone();
        changed.repo[0].prepare = vec!["bun install".into()];
        let rerun = run_required(
            &backend,
            &changed,
            &store,
            &candidate,
            &snapshot,
            false,
            &RunToCompletion,
        )
        .await
        .unwrap();
        assert!(!rerun.reused);
    }

    /// A preparation that fails stops the execution there. The check is never
    /// started, the result says what failed and where, the gateway is closed,
    /// and nothing about it is evidence a later submission could borrow.
    #[tokio::test]
    async fn a_failed_preparation_never_runs_the_check_or_becomes_evidence() {
        let (store, cfg, candidate) = prepared_repo("prep2");
        let recorded = std::sync::Arc::new(Recorded {
            prepare_exit: 1,
            ..Default::default()
        });
        let backend = RecordingBackend(recorded.clone());
        let snapshot = std::env::temp_dir();

        for _ in 0..2 {
            let report = run_required(
                &backend,
                &cfg,
                &store,
                &candidate,
                &snapshot,
                false,
                &RunToCompletion,
            )
            .await
            .unwrap();
            assert!(!report.all_required_passed);
            assert!(!report.reused);
            let result = &report.results[0];
            assert_eq!(result.outcome, "interrupted");
            assert!(
                result
                    .tail
                    .contains("preparation `bun install --frozen-lockfile` failed (exit 1)"),
                "{}",
                result.tail
            );
            assert!(result.tail.contains("403 Forbidden"), "{}", result.tail);
            assert!(
                result.tail.contains("the check was not run"),
                "{}",
                result.tail
            );
        }
        assert_eq!(
            *recorded.log.lock().unwrap(),
            [
                "open registry.npmjs.org",
                "run bun install --frozen-lockfile",
                "close",
                "open registry.npmjs.org",
                "run bun install --frozen-lockfile",
                "close",
            ]
        );
    }
}
