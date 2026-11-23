//! A review's required checks on a provider elsewhere (DigitalOcean's
//! Harness Runtime), placed there because the node's own runtime was short
//! of room (`session::placing::checks`).
//!
//! **Prepare once, then fork.** One sandbox is made from the template of
//! the image the checks run in (the repository's prepared snapshot, whose
//! `/work` and `/cache` the template holds), and only the candidate's tree
//! is sent in, over the sandbox's own connection, onto that `/work`. The
//! repository's `prepare` commands run there with the preparation's own
//! egress grant, through the sandbox's tunnel to the node's egress proxy.
//! The grant is revoked as preparation ends, the tunnels are closed, the
//! sandbox is checkpointed, its run is ended on the node's side, and one
//! child is forked from the checkpoint per check that is to run: each with
//! a run and a token of its own, no tunnel and no grant, so a check has no
//! way out of its sandbox at all. The checks run at once, one per child.
//!
//! **Evidence is derived.** The provider rebuilt the image as its template,
//! so what it confirms is `ImageIdentity::Derived { from, actual }`. A
//! check's record is kept under the key of the digest it was derived from,
//! marked derived (`checks::derived_key`): evidence from here is reused only
//! by checks run here from the same image, never by the node's own runtime,
//! and never the other way round.
//!
//! **What does not run here goes back.** Anything that stops the checks
//! before any of them has run (the image names no digest, the sandbox is
//! not made or not isolated as required, the checkpoint or every fork
//! fails) is [`Forked::Unplaced`]: the caller runs them on the node's own
//! runtime instead. Rows already written for them are settled first, as
//! interrupted, which is never evidence.

use std::{path::Path, sync::Arc, time::Duration};

use serde_json::json;
use tracon_sandbox::node::SandboxServer;

use super::checks::{
    cancelled_result, check_inputs, derived_key, hash, immutable_image_identity, judge,
    record_reuse, reuse_key, run_preparation, until_stopped, with_candidate_tree, Cancel,
    CheckDefinition, CheckReport, CheckResult, Elsewhere, Pending, Preparation, PrepareFailure,
    Stopped,
};
use crate::{
    boundary::Backend,
    config::Config,
    environment::RepoEnvironment,
    provider::{
        capabilities::ImageIdentityKind, ImageIdentity, ProviderKind, Requirements, SandboxProvider,
    },
    runner::{sandbox_files::FilesStore, tunnels::TunnelSpec, Mount, Runner, RunnerCommand},
    sandbox_images::{Held, Making},
    store::{now_ms, CandidateRow, CheckRunRow, Store},
    telemetry::{telemetry, Purpose},
};

/// A provider elsewhere that runs a review's checks as forks of one
/// prepared sandbox, and what each of its sandboxes is made with.
pub struct Forks {
    pub kind: ProviderKind,
    pub provider: Arc<dyn SandboxProvider>,
    /// The node's sandbox identity its sandboxes connect to.
    pub server: Arc<SandboxServer>,
    /// Where the candidate's tree is kept while it is sent in.
    pub files: FilesStore,
    /// The way to the node's gateways, opened on the prepared sandbox while
    /// its `prepare` commands run and never on a check's.
    pub tunnels: Vec<TunnelSpec>,
    /// What every sandbox's isolation, read back, is held to.
    pub requirements: Requirements,
    pub making: Making,
    /// Where a sandbox holds the tree: `/work`.
    pub workdir: String,
}

/// What became of checks placed elsewhere.
pub enum Forked {
    /// They ran there (or reused evidence from there): the report, or the
    /// node failing to record it.
    Ran(Box<Result<CheckReport, String>>),
    /// Nothing ran there, for this reason: they are the node's own
    /// runtime's to run.
    Unplaced(String),
}

impl Forked {
    fn ran(report: Result<CheckReport, String>) -> Self {
        Forked::Ran(Box::new(report))
    }
}

/// How one forked check ended.
enum Ended {
    Output(std::process::Output),
    /// It never started, or its sandbox failed under it: why.
    Failed(String),
    Cancelled {
        reason: String,
        killed: String,
    },
    TimedOut,
}

/// Run `environment`'s checks of `candidate` (whose tree is `snapshot`) on
/// `forks`, prepared once and forked per check; `backend` opens the
/// preparation's egress grant on the node's egress proxy.
#[allow(clippy::too_many_arguments)]
pub async fn run_forked(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    candidate: &CandidateRow,
    session_id: Option<&str>,
    environment: &RepoEnvironment,
    snapshot: &Path,
    force_rerun: bool,
    cancel: &dyn Cancel,
    forks: &Forks,
) -> Forked {
    let ran = forked(
        backend,
        cfg,
        store,
        candidate,
        session_id,
        environment,
        snapshot,
        force_rerun,
        cancel,
        forks,
    )
    .await;
    match ran {
        Ok(forked) => forked,
        Err(error) => Forked::ran(Err(error)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn forked(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    candidate: &CandidateRow,
    session_id: Option<&str>,
    environment: &RepoEnvironment,
    snapshot: &Path,
    force_rerun: bool,
    cancel: &dyn Cancel,
    forks: &Forks,
) -> Result<Forked, String> {
    let kind = forks.kind.as_str();
    let image = environment.image.clone();
    let commands = environment.checks.clone();
    if commands.is_empty() {
        return Ok(Forked::Unplaced("there are no checks to run".into()));
    }
    let Some(from) = immutable_image_identity(&image) else {
        return Ok(Forked::Unplaced(format!(
            "{image} names no digest for {kind} to make its sandboxes from"
        )));
    };
    // What the provider promises of every sandbox's image, which the
    // evidence is keyed on before any sandbox confirms it.
    let exact = forks.provider.capabilities().image_identity == ImageIdentityKind::Exact;
    let image_key = match exact {
        true => from.clone(),
        false => derived_key(&from),
    };
    let timeout = Duration::from_secs(environment.timeout_secs);
    let inputs_json = serde_json::to_string(&check_inputs(cfg, environment))
        .map_err(|error| error.to_string())?;
    let report = |results: Vec<CheckResult>,
                  cancelled: Option<String>,
                  prepared: Option<Preparation>,
                  elsewhere: Option<Elsewhere>,
                  reused: bool| CheckReport {
        candidate_id: candidate.id.clone(),
        all_required_passed: cancelled.is_none()
            && results.len() == commands.len()
            && results.iter().all(|result| result.ok),
        required_count: commands.len(),
        results,
        reused,
        image: image.clone(),
        image_source: environment.image_source,
        cancelled,
        prepared,
        no_room: None,
        elsewhere,
    };
    // Nothing new is started for work that has been withdrawn.
    if let Some(reason) = cancel.cancelled() {
        return Ok(Forked::ran(Ok(report(
            vec![cancelled_result(&commands[0], &reason, 0)],
            Some(reason),
            None,
            None,
            false,
        ))));
    }

    // Which checks run, and which reuse evidence derived from this image
    // before: only looked up here, so checks that cannot be placed after
    // all leave nothing recorded for the node's own runtime to repeat.
    let mut reusing = Vec::new();
    let mut pending = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let definition_json = serde_json::to_string(&CheckDefinition {
            command: command.clone(),
            timeout_secs: timeout.as_secs(),
        })
        .map_err(|error| error.to_string())?;
        let definition_hash = hash(&definition_json);
        let key = reuse_key(
            &candidate.head_sha,
            &definition_hash,
            &image_key,
            &inputs_json,
        );
        let rerun_of = match force_rerun {
            true => store
                .latest_check_for_identity(
                    &candidate.id,
                    &definition_hash,
                    Some(&image),
                    &inputs_json,
                )
                .map_err(|error| error.to_string())?
                .map(|run| run.id),
            false => None,
        };
        let prior = match force_rerun {
            true => None,
            false => store
                .latest_reusable_check(&candidate.id, &key)
                .map_err(|error| error.to_string())?,
        };
        let check = Pending {
            index,
            command: command.clone(),
            definition_json,
            definition_hash,
            reuse_key: Some(key),
            rerun_of,
        };
        match prior {
            Some(source) => reusing.push((check, source)),
            None => pending.push(check),
        }
    }
    let mut results: Vec<Option<CheckResult>> = vec![None; commands.len()];
    let record_reused = |results: &mut Vec<Option<CheckResult>>,
                         reusing: Vec<(Pending, CheckRunRow)>|
     -> Result<bool, String> {
        let any = !reusing.is_empty();
        for (check, source) in reusing {
            let index = check.index;
            results[index] = Some(record_reuse(
                store,
                candidate,
                session_id,
                check,
                (&image, &inputs_json),
                source,
                (kind, environment.repo.as_deref()),
            )?);
        }
        Ok(any)
    };
    if pending.is_empty() {
        let reused = record_reused(&mut results, reusing)?;
        return Ok(Forked::ran(Ok(report(
            results.into_iter().flatten().collect(),
            None,
            None,
            None,
            reused,
        ))));
    }

    // The sandbox they are prepared in, from the image's template, with the
    // candidate's tree sent onto its `/work`. Made before anything is
    // recorded: one that cannot be made leaves the checks to the node.
    let run_id = uuid::Uuid::now_v7().to_string();
    let tag = &hash(&run_id)[..12];
    let volume = crate::config::resource_name(&format!("check-{run_id}"));
    let release = |volume: &str| {
        if let Err(error) = forks.files.remove(volume) {
            tracing::debug!(%volume, %error, "a check's tree was left for a later sweep");
        }
    };
    if let Err(error) = forks.files.import(&volume, snapshot) {
        release(&volume);
        return Ok(Forked::Unplaced(format!(
            "the candidate's tree could not be staged for {kind}: {error}"
        )));
    }
    let parent = Held::start_making(
        forks.provider.clone(),
        Some(&forks.files),
        forks.server.clone(),
        &crate::config::resource_name(&format!("c-p-{tag}")),
        &image,
        &[Mount::volume(volume.clone(), &forks.workdir, false)],
        &forks.making,
    )
    .await;
    let parent = match parent {
        Ok(parent) => parent.at(&forks.workdir),
        Err(why) => {
            release(&volume);
            return Ok(Forked::Unplaced(why));
        }
    };
    // The candidate's tree is in the sandbox now; the node's copy is
    // nobody's.
    release(&volume);
    let confirmed = async {
        parent
            .verify(&forks.requirements)
            .await
            .map_err(|why| format!("{kind}'s sandbox is not isolated as required: {why}"))?;
        let identity = parent.identity().await?;
        let ran_from = match &identity {
            ImageIdentity::Exact { digest } if exact => digest,
            ImageIdentity::Derived { from, .. } if !exact => from,
            other => {
                return Err(format!(
                    "{kind} confirmed {other:?}, not the kind of image identity it promises"
                ))
            }
        };
        if *ran_from != from {
            return Err(format!(
                "{kind}'s sandbox runs an image from {ran_from}, not {image}"
            ));
        }
        if !environment.prepare.is_empty() {
            parent.open_tunnels(&forks.tunnels).await?;
        }
        Ok(identity)
    }
    .await;
    let identity = match confirmed {
        Ok(identity) => identity,
        Err(why) => {
            parent.finish().await;
            return Ok(Forked::Unplaced(why));
        }
    };
    let any_reused = record_reused(&mut results, reusing)?;

    // From here each check has a record, running until it settles.
    let mut runs = Vec::with_capacity(pending.len());
    for check in &pending {
        let id = uuid::Uuid::now_v7().to_string();
        store
            .insert_check_run(&CheckRunRow {
                id: id.clone(),
                candidate_id: Some(candidate.id.clone()),
                session_id: session_id.map(str::to_string),
                command: Some(check.command.clone()),
                definition_json: check.definition_json.clone(),
                definition_hash: Some(check.definition_hash.clone()),
                execution_image: Some(image.clone()),
                inputs_json: Some(inputs_json.clone()),
                reuse_key: check.reuse_key.clone(),
                outcome: "running".into(),
                source_outcome: None,
                exit_code: None,
                log: String::new(),
                duration_ms: None,
                started_ms: now_ms(),
                finished_ms: None,
                rerun_of: check.rerun_of.clone(),
                reused_from_id: None,
                metadata_json: json!({
                    "reused": false,
                    "explicit_rerun": force_rerun,
                    "execution_backend": kind,
                    "image_identity": identity,
                    "candidate_tree": candidate.tree_sha,
                })
                .to_string(),
            })
            .map_err(|error| error.to_string())?;
        runs.push(id);
    }
    // Rows settled as interrupted when the checks go back to the node:
    // never evidence, and never left running.
    let fall_back = |why: String| {
        for id in &runs {
            let _ = store.finish_check_run(
                id,
                "interrupted",
                None,
                None,
                &format!("not run on {kind}: {why}"),
                Some(0),
                &with_candidate_tree(
                    candidate,
                    json!({ "execution_backend": kind, "runner_error": why, "fell_back": true }),
                ),
            );
        }
        Forked::Unplaced(why)
    };

    // Preparation, with the grant `run_preparation` opens and revokes
    // as its last step ends.
    let preparing = std::time::Instant::now();
    let prepared = match environment.prepare.is_empty() {
        true => None,
        false => {
            let outcome = run_preparation(
                backend,
                &parent,
                environment,
                ("", ""),
                &crate::config::resource_name(&format!("p-{tag}")),
                tokio::time::Instant::now() + timeout,
                cancel,
                "no check was run",
                Purpose::Check,
            )
            .await
            .map_err(|stopped| match stopped {
                Stopped::TimedOut => Stopped::Unprepared(PrepareFailure::said(format!(
                    "timed out after {}s",
                    environment.timeout_secs
                ))),
                stopped => stopped,
            });
            Some((outcome, preparing.elapsed().as_millis() as u64))
        }
    };
    // Nothing forked from it reaches the node's gateways.
    parent.close_tunnels();
    let preparation = prepared.as_ref().map(|(outcome, ms)| Preparation {
        ok: outcome.is_ok(),
        ms: *ms,
        failure: match outcome {
            Err(Stopped::Unprepared(failure)) => Some(failure.clone()),
            _ => None,
        },
    });
    if let Some((Err(stopped), _)) = &prepared {
        parent.finish().await;
        let mut cancelled = None;
        for (check, id) in pending.iter().zip(&runs) {
            let result = match stopped {
                Stopped::Cancelled { reason, killed } => {
                    store
                        .cancel_running_check(
                            id,
                            &format!("cancelled: {reason}"),
                            &with_candidate_tree(
                                candidate,
                                json!({
                                    "execution_backend": kind,
                                    "cancelled": reason,
                                    "killed": killed,
                                }),
                            ),
                        )
                        .map_err(|error| error.to_string())?;
                    cancelled = Some(reason.clone());
                    cancelled_result(&check.command, reason, 0)
                }
                Stopped::Unprepared(failure) => {
                    store
                        .finish_check_run(
                            id,
                            "not_run",
                            None,
                            None,
                            &format!("not run: {}", failure.reason()),
                            Some(0),
                            &with_candidate_tree(
                                candidate,
                                json!({
                                    "execution_backend": kind,
                                    "not_run": "preparation",
                                    "prepared_with": environment.prepare,
                                    "preparation": failure,
                                }),
                            ),
                        )
                        .map_err(|error| error.to_string())?;
                    CheckResult {
                        command: check.command.clone(),
                        ok: false,
                        exit: None,
                        outcome: "not_run".into(),
                        tail: format!("not run: {}", failure.reason()),
                        failures: String::new(),
                        ms: 0,
                        reused_from: None,
                        missing_tool: None,
                    }
                }
                Stopped::TimedOut => unreachable!("a preparation's timeout is its failure"),
            };
            results[check.index] = Some(result);
        }
        return Ok(Forked::ran(Ok(report(
            results.into_iter().flatten().collect(),
            cancelled,
            preparation,
            Some(Elsewhere {
                provider: forks.kind,
                identity,
                forks: 0,
            }),
            any_reused,
        ))));
    }

    // The checkpoint every check starts from. Its own run ends here: a
    // fork holds a copy of its supervisor, which must never come back as it.
    let checkpoint = match parent.checkpoint().await {
        Ok(checkpoint) => checkpoint,
        Err(why) => {
            parent.finish().await;
            return Ok(fall_back(why));
        }
    };
    parent.end_run();
    let names: Vec<String> = pending
        .iter()
        .zip(&runs)
        .map(|(check, id)| {
            crate::config::resource_name(&format!("c-{}-{}", check.index, &hash(id)[..12]))
        })
        .collect();
    let children = Held::forked(
        forks.provider.clone(),
        forks.server.clone(),
        &checkpoint,
        &image,
        &names,
        &forks.making,
    )
    .await;
    if let Err(error) = forks.provider.forget_snapshot(&checkpoint).await {
        tracing::debug!(checkpoint = %checkpoint.0, %error, "a checkpoint was left for the sweep");
    }
    parent.finish().await;
    let made = children.iter().filter(|child| child.is_ok()).count();
    // Not one child to run a check in: the node runs them all.
    if made == 0 {
        let why = children
            .iter()
            .find_map(|child| child.as_ref().err().cloned())
            .unwrap_or_else(|| "nothing was forked".into());
        return Ok(fall_back(why));
    }

    // Every check at once, each in its own child.
    // From the snapshot's template, the default branch's build output is in
    // the child's own `/cache`, a copy no other child or run reads.
    let env = match environment.prepare.is_empty() {
        true => Vec::new(),
        false => super::checks::check_cache(environment, "").0,
    };
    let ran = pending
        .iter()
        .zip(children)
        .zip(&names)
        .map(|((check, child), name)| {
            let env = env.clone();
            let image = &image;
            async move {
                let started = std::time::Instant::now();
                let child = match child {
                    Ok(child) => child.at(&forks.workdir),
                    Err(why) => return (Ended::Failed(why), started.elapsed()),
                };
                if let Err(why) = child.verify(&forks.requirements).await {
                    child.finish().await;
                    return (
                        Ended::Failed(format!("its sandbox is not isolated as required: {why}")),
                        started.elapsed(),
                    );
                }
                let cmd = RunnerCommand {
                    argv: super::output::merged_shell(&check.command),
                    env,
                    workdir: Some(forks.workdir.clone()),
                    name: name.clone(),
                    image: Some(image.to_string()),
                    ..Default::default()
                };
                let deadline = tokio::time::Instant::now() + timeout;
                let ended = match until_stopped(
                    child.run_capture(cmd),
                    deadline,
                    cancel,
                    Some((&child as &dyn Runner, name.as_str())),
                )
                .await
                {
                    Ok(Ok(output)) => Ended::Output(output),
                    Ok(Err(error)) => Ended::Failed(format!("could not run: {error}")),
                    Err(Some(reason)) => Ended::Cancelled {
                        reason,
                        killed: name.clone(),
                    },
                    Err(None) => Ended::TimedOut,
                };
                child.finish().await;
                (ended, started.elapsed())
            }
        });
    let ended = futures_util::future::join_all(ran).await;

    let mut cancelled = None;
    for ((check, id), (ended, took)) in pending.iter().zip(&runs).zip(ended) {
        let duration_ms = i64::try_from(took.as_millis()).unwrap_or(i64::MAX);
        let (outcome, exit, tail_text, mut metadata) = match ended {
            Ended::Cancelled { reason, killed } => {
                store
                    .cancel_running_check(
                        id,
                        &format!("cancelled: {reason}"),
                        &with_candidate_tree(
                            candidate,
                            json!({
                                "execution_backend": kind,
                                "cancelled": reason,
                                "killed": killed,
                            }),
                        ),
                    )
                    .map_err(|error| error.to_string())?;
                telemetry().check_finished(
                    "review",
                    environment.repo.as_deref(),
                    "cancelled",
                    Some(took),
                );
                results[check.index] = Some(cancelled_result(
                    &check.command,
                    &reason,
                    duration_ms.max(0) as u64,
                ));
                cancelled = Some(reason);
                continue;
            }
            Ended::Output(output) => judge(&output, &check.command, environment, timeout, kind),
            Ended::Failed(why) => (
                "interrupted",
                None,
                why.clone(),
                json!({ "execution_backend": kind, "runner_error": why }),
            ),
            Ended::TimedOut => (
                "interrupted",
                None,
                format!("timed out after {}s", timeout.as_secs()),
                json!({
                    "execution_backend": kind,
                    "timeout_secs": timeout.as_secs(),
                    "interrupted": "timeout",
                }),
            ),
        };
        // Its cache was the template's own, in a sandbox of its own that is
        // gone now, never a volume of the node's.
        if let Some(fields) = metadata.as_object_mut() {
            fields.remove("cache_volume");
        }
        metadata["image_identity"] = json!(identity);
        metadata["fork"] = json!(id);
        let recorded = store
            .finish_check_run(
                id,
                outcome,
                None,
                exit.map(i64::from),
                &tail_text,
                Some(duration_ms),
                &with_candidate_tree(candidate, metadata.clone()),
            )
            .map_err(|error| error.to_string())?;
        if !recorded {
            // Settled by the controller while it ran: what was written down
            // first stands, as on the node's own runtime.
            results[check.index] = Some(cancelled_result(
                &check.command,
                "result arrived after the run was recorded",
                duration_ms.max(0) as u64,
            ));
            cancelled.get_or_insert_with(|| "the run was settled while it ran".into());
            continue;
        }
        telemetry().check_finished("review", environment.repo.as_deref(), outcome, Some(took));
        results[check.index] = Some(CheckResult {
            command: check.command.clone(),
            ok: outcome == "passed",
            exit,
            outcome: outcome.into(),
            tail: tail_text,
            failures: metadata
                .get("failures")
                .and_then(|failures| failures.as_str())
                .unwrap_or_default()
                .to_string(),
            ms: duration_ms.max(0) as u64,
            reused_from: None,
            missing_tool: metadata
                .get("missing_tool")
                .and_then(|tool| tool.as_str())
                .map(str::to_string),
        });
    }
    Ok(Forked::ran(Ok(report(
        results.into_iter().flatten().collect(),
        cancelled,
        preparation,
        Some(Elsewhere {
            provider: forks.kind,
            identity,
            forks: made,
        }),
        any_reused,
    ))))
}

/// [`super::checks::run_required_for`] placed on DigitalOcean's `runtime`,
/// every sandbox held to `requirements`: the repository's environment
/// resolved as the node's own runtime would, then the template of the
/// image it names waited for (at most
/// [`CHECK_TEMPLATE_WITHIN`](crate::provider::digitalocean::runtime::CHECK_TEMPLATE_WITHIN)),
/// then [`run_forked`].
#[allow(clippy::too_many_arguments)]
pub async fn run_on_digitalocean(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    candidate: &CandidateRow,
    session_id: Option<&str>,
    repo: Option<&Path>,
    snapshot: &Path,
    force_rerun: bool,
    cancel: &dyn Cancel,
    runtime: &crate::provider::digitalocean::runtime::DoRuntime,
    requirements: Requirements,
) -> Forked {
    use crate::provider::digitalocean::runtime::{CHECK_TEMPLATE_POLL, CHECK_TEMPLATE_WITHIN};
    let environment = match crate::repo_image::environment(backend, cfg, store, repo).await {
        Ok(environment) => environment,
        Err(why) => return Forked::Unplaced(why),
    };
    if let Err(why) = runtime
        .check_template(
            &environment.image,
            CHECK_TEMPLATE_WITHIN,
            CHECK_TEMPLATE_POLL,
            cancel,
        )
        .await
    {
        return Forked::Unplaced(why);
    }
    let files = backend
        .sandbox_files_store()
        .unwrap_or_else(FilesStore::for_node);
    let forks = match runtime.check_forks(cfg, requirements, files, environment.repo.clone()) {
        Ok(forks) => forks,
        Err(why) => return Forked::Unplaced(why),
    };
    run_forked(
        backend,
        cfg,
        store,
        candidate,
        session_id,
        &environment,
        snapshot,
        force_rerun,
        cancel,
        &forks,
    )
    .await
}
