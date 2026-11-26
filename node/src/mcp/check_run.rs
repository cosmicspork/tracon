//! `check_run`: a harness the operator runs tests through the node.
//!
//! An agent outside the boundary has no container runtime of the node's to
//! run its repository's checks in, and reaching for one of its own (the Dev
//! Container CLI, a Podman machine kept running on macOS) is a second,
//! unbrokered way to run containers. So with `[external] check_run` on, the
//! node runs them for it on request: the
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
//!
//! A run outlives the call that began it. Checks can take far longer than a
//! harness's MCP client waits on one call, so `check_run` blocks only for
//! the status tools' cap (`wait::cap`) and otherwise hands back a `run_id`,
//! which `check_run_status` waits on and `check_run_cancel` withdraws. A
//! run nobody asks after for [`IDLE`] is withdrawn, and a finished one's
//! answer is kept for [`KEEP`] ([`Runs`]).

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

use crate::{
    mcp::{wait, CallContext},
    review,
    session::Manager,
    store::{now_ms, CandidateRow, Store},
};

pub const CHECK_RUN: &str = "check_run";
pub const STATUS: &str = "check_run_status";
pub const CANCEL: &str = "check_run_cancel";

/// How long a run goes on with nobody asking for its result before it is
/// withdrawn: a harness that stopped polling has stopped wanting it.
pub const IDLE: Duration = Duration::from_secs(10 * 60);

/// How long a finished run's answer is kept for a `check_run_status` to
/// fetch.
pub const KEEP: Duration = Duration::from_secs(30 * 60);

/// Why a run [`IDLE`] for its window is withdrawn.
pub const IDLE_REASON: &str = "nobody asked for check_run's result for 10 minutes";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": CHECK_RUN,
            "description": format!("Run this repository's checks, or one command, in the repository's own \
                 image on the node, instead of in a container of your own. The node snapshots the \
                 worktree's HEAD commit as submit_review does (commit first: uncommitted changes \
                 are not in it), prepares it as the repository's [[repo]] entry says, and runs each \
                 check on a fresh copy of the prepared tree with the dependency cache read-only and \
                 no network. Each result has its outcome, exit status and the end of its output. \
                 Nothing is attached to a review: this is for you, and submit_review runs the \
                 required checks again on what it is given. Checks wait for room as a review's do. \
                 A call blocks at most {} seconds, or longer when your client says it allows more; \
                 checks that run longer return status `running` and a `run_id`, and keep running: \
                 fetch the result with {STATUS}, or stop them with {CANCEL}.", wait::MAX_WAIT_SECS),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "worktree": { "type": "string", "description": "The absolute path of the worktree to check, under the node's [external] repo_roots." },
                    "checks": { "type": "array", "items": { "type": "string" }, "description": "Which of the repository's configured checks to run, each exactly as configured. Defaults to all of them." },
                    "command": { "type": "string", "description": "One command to run instead of the configured checks, through `sh -c` in the prepared tree. Not with `checks`." },
                },
                "required": ["worktree"],
            },
        }),
        json!({
            "name": STATUS,
            "description": format!("Wait for a {CHECK_RUN} that returned a `run_id`. Returns its result, in \
                 the shape {CHECK_RUN} returns it, once the checks finish, or status `still_waiting`. \
                 A run nobody asks after for 10 minutes is withdrawn, and a finished run's result is \
                 kept for 30 minutes; after that, or after the node restarts, the run is unknown: \
                 call {CHECK_RUN} again. Blocks at most {} seconds a call, or longer when your \
                 client says it allows more; call again to keep waiting.", wait::MAX_WAIT_SECS),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run_id": { "type": "string", "description": format!("The `run_id` {CHECK_RUN} returned.") },
                    "wait_secs": { "type": "integer", "minimum": 0, "description": wait::wait_secs_description() },
                },
                "required": ["run_id"],
            },
        }),
        json!({
            "name": CANCEL,
            "description": format!("Stop a {CHECK_RUN} that returned a `run_id`: what is running is killed \
                 and what is waiting for room never starts."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run_id": { "type": "string", "description": format!("The `run_id` {CHECK_RUN} returned.") },
                },
                "required": ["run_id"],
            },
        }),
    ]
}

/// `check_run`'s runs, by run id: each detached from the call that began
/// it, so a client that gives up on a call leaves the run going for a
/// later [`STATUS`] to collect. Held in memory only (on
/// `manager.external_checks()`); a restart forgets them, as it stops them.
pub struct Runs {
    table: Mutex<HashMap<String, Arc<Run>>>,
    idle: Duration,
    keep: Duration,
}

impl Default for Runs {
    fn default() -> Self {
        Self::with_windows(IDLE, KEEP)
    }
}

impl Runs {
    /// A table whose runs are withdrawn after `idle` with nobody asking, and
    /// whose answers are kept for `keep` once finished.
    pub fn with_windows(idle: Duration, keep: Duration) -> Self {
        Self {
            table: Mutex::default(),
            idle,
            keep,
        }
    }

    /// Begin `run` as `id`, for `channel`. It is handed its run, which is
    /// what it reads whether it has been withdrawn from.
    pub fn start<F, Fut>(&self, id: &str, channel: &str, run: F) -> Arc<Run>
    where
        F: FnOnce(Arc<Run>) -> Fut,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        let entry = Arc::new(Run {
            channel: channel.to_string(),
            idle: self.idle,
            asked: Mutex::new(Instant::now()),
            waiting: Default::default(),
            withdrawn: Mutex::default(),
            done: tokio::sync::watch::Sender::new(None),
            finished: Mutex::default(),
        });
        let running = tokio::spawn(run(entry.clone()));
        tokio::spawn({
            let entry = entry.clone();
            async move {
                let out = running.await.unwrap_or_else(|error| {
                    Err(format!("the checks stopped without an outcome ({error})"))
                });
                *entry.finished.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                entry.done.send_replace(Some(out));
            }
        });
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut table);
        table.insert(id.to_string(), entry.clone());
        entry
    }

    /// The run `id`, if `channel` began it and its answer is still kept;
    /// asked after as of now. Another channel's run reads as unknown.
    pub fn find(&self, id: &str, channel: &str) -> Option<Arc<Run>> {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut table);
        let run = table.get(id).filter(|run| run.channel == channel)?.clone();
        run.touch();
        Some(run)
    }

    /// Drop the answers kept longer than the table keeps them.
    fn prune(&self, table: &mut HashMap<String, Arc<Run>>) {
        table.retain(|_, run| {
            run.finished
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none_or(|at| at.elapsed() < self.keep)
        });
    }
}

/// One `check_run`'s run: whether it has been withdrawn, and its answer
/// once it has one.
pub struct Run {
    channel: String,
    idle: Duration,
    /// When a call last asked after the run.
    asked: Mutex<Instant>,
    /// Calls waiting on it now, each of which is asking after it.
    waiting: std::sync::atomic::AtomicUsize,
    withdrawn: Mutex<Option<String>>,
    done: tokio::sync::watch::Sender<Option<Result<Value, String>>>,
    finished: Mutex<Option<Instant>>,
}

impl Run {
    /// Someone still wants the result.
    pub fn touch(&self) {
        *self.asked.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// Withdraw the run for `why`; the first reason given stands.
    pub fn withdraw(&self, why: &str) {
        self.withdrawn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(|| why.to_string());
    }

    /// Whether it has finished.
    pub fn finished(&self) -> bool {
        self.done.borrow().is_some()
    }

    /// Its answer, waiting at most `secs` for it. The run counts as asked
    /// after for as long as the wait lasts.
    pub async fn wait(&self, secs: u64) -> Option<Result<Value, String>> {
        struct Waiting<'a>(&'a Run);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.touch();
                self.0
                    .waiting
                    .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            }
        }
        self.waiting
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let _waiting = Waiting(self);
        let mut done = self.done.subscribe();
        let out = tokio::time::timeout(Duration::from_secs(secs), done.wait_for(Option::is_some))
            .await
            .ok()?
            .ok()?
            .clone();
        out
    }
}

impl review::checks::Cancel for Run {
    fn cancelled(&self) -> Option<String> {
        let idle = self
            .asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
            >= self.idle
            && self.waiting.load(std::sync::atomic::Ordering::Acquire) == 0;
        if idle && !self.finished() {
            self.withdraw(IDLE_REASON);
        }
        self.withdrawn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

pub async fn call(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    if !ctx.is_external() {
        return Err(format!(
            "{name} is for a harness you run yourself; a session's checks run when it \
             calls {}",
            super::review::SUBMIT
        ));
    }
    if !manager.cfg().external.check_run {
        return Err(format!(
            "{name} is off on this node; the operator turns it on with [external] \
             check_run"
        ));
    }
    let runs = manager.external_checks().asked_runs();
    match name {
        STATUS => status(runs, ctx, args).await,
        CANCEL => cancel(runs, ctx, args),
        _ => start(store, manager, ctx, args).await,
    }
}

/// The run a call names, or why there is none to name.
fn named(runs: &Runs, ctx: &CallContext, args: &Value) -> Result<Arc<Run>, String> {
    let id = args
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or(format!("run_id is required: the one {CHECK_RUN} returned"))?;
    runs.find(id, &ctx.channel).ok_or(format!(
        "no {CHECK_RUN} run {id} is known here: its result is kept 30 minutes after it \
         finishes and a run nobody asks after for 10 minutes is withdrawn, and a node restart \
         forgets every run, so it expired or was lost. Call {CHECK_RUN} again"
    ))
}

async fn status(runs: &Runs, ctx: &CallContext, args: &Value) -> Result<Value, String> {
    let run = named(runs, ctx, args)?;
    match run.wait(wait::wait_secs(ctx, args)).await {
        Some(out) => out,
        None => Ok(json!({
            "status": "still_waiting",
            "run_id": args["run_id"],
            "note": wait::capped(ctx),
        })),
    }
}

fn cancel(runs: &Runs, ctx: &CallContext, args: &Value) -> Result<Value, String> {
    let run = named(runs, ctx, args)?;
    run.withdraw(&format!("the caller withdrew it with {CANCEL}"));
    Ok(json!({ "status": "cancelled" }))
}

async fn start(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
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
    // from. It is never stored, and nothing recorded points at it. Its id
    // is the run's.
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

    // The run is the node's from here, in a task of its own and detached
    // from this call: a client giving up on the call leaves it running for
    // check_run_status to collect, and it is withdrawn (the runners kill
    // what is running) by check_run_cancel or when nobody asks after it.
    let running = manager.external_checks().begin_asked();
    let run = manager
        .external_checks()
        .asked_runs()
        .start(&candidate.id, &ctx.channel, {
            let store = store.clone();
            let manager = manager.clone();
            let channel = ctx.channel.clone();
            let candidate = candidate.clone();
            let worktree = worktree.clone();
            move |wants| async move {
                let entry =
                    manager
                        .checking()
                        .line_entry(&candidate.id, &candidate.id, asked.clone());
                let (root, repo, asked) = (&snapshot.root, Some(repo.as_path()), &asked[..]);
                let (store_ref, manager_ref, candidate_ref, wants_ref) =
                    (&store, &manager, &candidate, &*wants);
                let report = super::review::run_placed(
                    &manager,
                    &channel,
                    repo,
                    &entry,
                    wants_ref,
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
                let report = report.ok_or("the checks were withdrawn before they ran")??;
                Ok(answer(&worktree, &head_sha, &candidate, &report))
            }
        });
    match run.wait(wait::cap(ctx)).await {
        Some(out) => out,
        None => Ok(running_answer(ctx, &candidate.id)),
    }
}

/// What a `check_run` whose checks outlast its wait answers.
fn running_answer(ctx: &CallContext, run_id: &str) -> Value {
    json!({
        "status": "running",
        "run_id": run_id,
        "note": wait::capped(ctx),
        "next": format!("call {STATUS} with this run_id for the result, or {CANCEL} to stop it"),
    })
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

    use review::checks::Cancel as _;

    /// A run that goes on until it is withdrawn, answering why.
    async fn until_withdrawn(run: Arc<Run>) -> Result<Value, String> {
        loop {
            if let Some(why) = run.cancelled() {
                return Ok(json!({ "cancelled": why }));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn a_run_outlasts_the_call_that_began_it_and_is_collected_by_its_id() {
        let runs = Runs::default();
        let (finish, finished) = tokio::sync::oneshot::channel::<()>();
        let run = runs.start("check-run-1", "work", |_| async move {
            finished.await.ok();
            Ok(json!({ "passed": true }))
        });
        // The call's wait runs out with the checks still going.
        assert_eq!(run.wait(0).await, None);
        assert!(!run.finished());
        // Another channel's id reads as unknown, and so does an id never
        // given.
        assert!(runs.find("check-run-1", "other").is_none());
        assert!(runs.find("check-run-2", "work").is_none());
        finish.send(()).unwrap();
        let found = runs.find("check-run-1", "work").unwrap();
        assert_eq!(found.wait(5).await, Some(Ok(json!({ "passed": true }))));
        // The answer is kept for the next ask too.
        let again = runs.find("check-run-1", "work").unwrap();
        assert_eq!(again.wait(0).await, Some(Ok(json!({ "passed": true }))));
    }

    #[tokio::test]
    async fn a_withdrawn_run_stops_with_the_reason_it_was_given() {
        let runs = Runs::default();
        let run = runs.start("check-run-1", "work", until_withdrawn);
        assert_eq!(run.wait(0).await, None);
        runs.find("check-run-1", "work")
            .unwrap()
            .withdraw("the caller withdrew it with check_run_cancel");
        assert_eq!(
            run.wait(5).await,
            Some(Ok(
                json!({ "cancelled": "the caller withdrew it with check_run_cancel" })
            ))
        );
    }

    #[tokio::test]
    async fn a_run_nobody_asks_after_is_withdrawn() {
        let runs = Runs::with_windows(Duration::from_millis(50), KEEP);
        let run = runs.start("check-run-1", "work", until_withdrawn);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(run.finished());
        assert_eq!(
            run.wait(0).await,
            Some(Ok(json!({ "cancelled": IDLE_REASON })))
        );
        // One that is asked after keeps going.
        let polled = runs.start("check-run-2", "work", until_withdrawn);
        for _ in 0..6 {
            assert_eq!(polled.wait(0).await, None);
            tokio::time::sleep(Duration::from_millis(20)).await;
            runs.find("check-run-2", "work").unwrap();
        }
        assert!(polled.cancelled().is_none());
        polled.withdraw("done");
        // So does one a call is waiting on, however long the call waits.
        let waited = runs.start("check-run-3", "work", until_withdrawn);
        let (finish, finished) = tokio::sync::oneshot::channel::<()>();
        let held = runs.start("check-run-4", "work", |run| async move {
            finished.await.ok();
            Ok(json!({ "cancelled": run.cancelled() }))
        });
        let wait = tokio::spawn({
            let waited = waited.clone();
            async move { waited.wait(1).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(waited.cancelled().is_none());
        finish.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            held.wait(0).await,
            Some(Ok(json!({ "cancelled": IDLE_REASON })))
        );
        waited.withdraw("done");
        assert!(wait.await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_finished_answer_is_dropped_once_its_window_passes() {
        let runs = Runs::with_windows(IDLE, Duration::from_millis(20));
        let run = runs.start("check-run-1", "work", |_| async { Ok(json!({})) });
        assert!(run.wait(5).await.is_some());
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(runs.find("check-run-1", "work").is_none());
    }

    #[tokio::test]
    async fn a_run_that_panics_answers_that_it_stopped_without_an_outcome() {
        let runs = Runs::default();
        let run = runs.start("check-run-1", "work", |_| async {
            if true {
                panic!("boom");
            }
            Ok(json!({}))
        });
        let out = run.wait(5).await.unwrap().unwrap_err();
        assert!(out.contains("without an outcome"), "{out}");
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
