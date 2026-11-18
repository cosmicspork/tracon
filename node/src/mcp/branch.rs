//! `fetch_branch`: bring a branch of the repository's origin up to date in
//! the session's own workspace, at any time.
//!
//! A managed workspace is a clone of the node's managed clone with no remote
//! of its own, by design: its sandbox has nothing it could fetch from. So the
//! fetch runs on the node. The managed clone is fetched from the forge with
//! the repository's credential, as for a session's start, and what the
//! workspace lacks of the branch is carried to it as a bundle, which Git in
//! the workspace fetches with no credential and nothing to reach over the
//! network: the bundle is all it reads. Only `refs/remotes/origin/<branch>`
//! moves: the working tree, the index and the local branches are the
//! agent's, which rebases, merges or checks out itself.
//!
//! Where that Git runs follows where the workspace is ([`Place`]):
//!
//! - in the session's own running sandbox, over the connection the node
//!   already has to it, while one holds the workspace: the tree the agent
//!   works in is the one that has to see the ref, and the node's copy of it
//!   is replaced by the sandbox's at the next sync;
//! - otherwise in a short-lived sandbox that mounts only the workspace and
//!   the bundle, as the workspace's owner. A workspace the node keeps (one
//!   that travels over the files channel) comes back from it as its
//!   checkout, and never while a session's sandbox holds it.
//!
//! It reads the forge and writes nothing there, so like `show_work` it is
//! dispatched before the policy is consulted and is never asked.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tracon_sandbox::node::SandboxConn;
use tracon_sandbox::protocol::{Exclusions, SpawnSpec};

use crate::{
    config::Config,
    mcp::{CallContext, SessionAccess},
    runner::{Mount, RunKind, RunLabels, Runner, RunnerCommand},
    workspace::Workspace,
};

pub const FETCH: &str = "fetch_branch";

/// Where the bundle is mounted in a sandbox of the fetch's own.
const BUNDLE_DIR: &str = "/fetch";
pub const BUNDLE_FILE: &str = "branch.bundle";

/// Where the workspace is mounted in a sandbox of the fetch's own.
const WORKDIR: &str = "/work";

/// How long each stage of a step may take: starting its sandbox or Git,
/// sending a tree, and Git itself. Git there reads a `.git` the agent
/// controls: a FIFO for `packed-refs`, or a config that sends a fetch to a
/// command that never returns, would otherwise hold the workspace's fetches
/// (and, for a volume the runtime keeps, its exports) until the node
/// restarts.
pub const STEP_LIMIT: Duration = Duration::from_secs(120);

/// What Git in the workspace runs. `$1` is the branch, `$2` its tip in the
/// managed clone and `$3` the bundle to fetch it from, or empty when the
/// workspace already holds the tip and only the ref moves. It prints the
/// commit ids the workspace's refs name (`have`), or how far its HEAD is
/// ahead of and behind the fetched tip (`fetch`). Hooks and an fsmonitor the
/// workspace names are not run: Git here acts for the node.
const SCRIPT: &str = r#"set -e
g() { git -c core.hooksPath=/dev/null -c core.fsmonitor= "$@"; }
case "$0" in
have)
  g for-each-ref --format='%(objectname)'
  g rev-parse -q --verify HEAD || true
  ;;
fetch)
  if [ -n "$3" ]; then
    g fetch -q --no-tags --no-write-fetch-head "$3" "+refs/remotes/origin/$1:refs/remotes/origin/$1" >&2
  else
    g update-ref "refs/remotes/origin/$1" "$2"
  fi
  if g rev-parse -q --verify HEAD >/dev/null; then
    g rev-list --left-right --count "HEAD...$2"
  fi
  ;;
esac
"#;

pub fn definitions() -> Vec<Value> {
    vec![json!({
        "name": FETCH,
        "description": "Bring a branch of this repository's origin up to date in your workspace, \
                        at any time: the base after your change merged, or a branch to build on \
                        (a stacked change, a teammate's work). Your workspace has no remote of \
                        its own, so `git fetch` cannot do this. Only `origin/<branch>` moves; \
                        your working tree, index and branches are untouched, so rebase, merge or \
                        check out from it yourself. Returns the fetched `head` and how far your \
                        HEAD is `ahead` of and `behind` it. submit_review with that `base` then \
                        compares against the refreshed `origin/<branch>`.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "branch": {
                    "type": "string",
                    "description": "The branch on origin, such as main or feat/x.",
                },
            },
            "required": ["branch"],
        },
    })]
}

/// Why `branch` is not a branch name Git would take, if it is not: the rules
/// of `git check-ref-format` for `refs/heads/<branch>`, and never anything
/// that starts like an option.
pub fn refusal(branch: &str) -> Option<String> {
    let bad = |why: &str| Some(format!("{branch:?} is not a branch name: {why}"));
    if branch.is_empty() {
        return Some("branch is required".into());
    }
    if branch.starts_with('-') {
        return bad("it starts with -");
    }
    if branch
        .chars()
        .any(|c| c.is_ascii_control() || " ~^:?*[\\".contains(c))
    {
        return bad("it holds a space, a control character or one of ~^:?*[\\");
    }
    if branch.contains("..") || branch.contains("@{") || branch == "@" {
        return bad("it holds .. or @{, or is @");
    }
    if branch.ends_with('.')
        || branch
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return bad("a part of it is empty, starts with . or ends with .lock or .");
    }
    None
}

/// The argv of one step of [`SCRIPT`].
fn step_argv(step: &str, rest: &[&str]) -> Vec<String> {
    let mut argv = vec!["sh".into(), "-c".into(), SCRIPT.into(), step.into()];
    argv.extend(rest.iter().map(|arg| arg.to_string()));
    argv
}

/// The environment every step runs in: no configuration but the
/// workspace's own, and never a prompt.
fn step_env() -> Vec<(String, String)> {
    vec![
        ("HOME".into(), "/tmp".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ]
}

/// The command for one step of [`SCRIPT`] in a sandbox of its own named
/// `name`, in the mount at `workdir`.
fn step_command(
    step: &str,
    rest: &[&str],
    mounts: Vec<Mount>,
    workdir: &str,
    name: &str,
) -> RunnerCommand {
    RunnerCommand {
        argv: step_argv(step, rest),
        env: step_env(),
        secret_env: Default::default(),
        mounts,
        workdir: Some(workdir.to_string()),
        image: None,
        name: name.to_string(),
        labels: RunLabels::of(RunKind::Fetch),
    }
}

/// What a step that ran out of time is answered with, once Git is known to
/// have started.
fn git_hung(limit: Duration) -> String {
    format!(
        "Git in the workspace did not finish in {}s and was stopped; \
         its .git may hold a FIFO, or a config naming a command that does not return",
        limit.as_secs()
    )
}

/// Run a step in a sandbox of its own for at most `limit`. One that runs out
/// of time is killed by the name the runtime gave it, since not every runner
/// stops a capture whose future is dropped, and the caller is answered with
/// an error so whatever it holds is let go. Which stage stalled cannot be
/// told from here: making the sandbox, sending the workspace in, or Git.
async fn bounded(
    runner: &dyn Runner,
    cmd: RunnerCommand,
    limit: Duration,
    session: &str,
) -> Result<std::process::Output, String> {
    let running = runner.capture_name(&cmd.name);
    match tokio::time::timeout(limit, runner.run_capture(cmd)).await {
        Ok(finished) => finished.map_err(|e| e.to_string()),
        Err(_) => {
            tracing::warn!(
                sandbox = %running,
                session,
                limit_s = limit.as_secs(),
                "fetch step did not finish in its sandbox; stopping it"
            );
            if let Err(error) = runner.kill(&running).await {
                tracing::warn!(%error, name = %running, "timed-out fetch step not killed");
            }
            Err(format!(
                "the fetch step did not finish within {}s (starting the sandbox, sending the \
                 workspace, or Git) and was stopped",
                limit.as_secs()
            ))
        }
    }
}

/// What the node brings the workspace for one fetch.
pub struct Bundle {
    /// The branch's tip in the managed clone.
    pub sha: String,
    /// A node directory holding [`BUNDLE_FILE`], removed once it is used;
    /// `None` when the workspace holds the tip already and only the ref
    /// moves.
    pub stage: Option<PathBuf>,
}

/// Where a fetch in a sandbox of its own keeps its bundle: a volume named as
/// that sandbox, mounted read-only beside the workspace.
#[async_trait]
pub trait BundleVolumes: Send + Sync {
    /// Make `volume` a copy of the node directory `source`.
    async fn stage(&self, volume: &str, source: &Path) -> Result<(), String>;
    /// Remove `volume`, made by `stage` or left half made by it.
    async fn remove(&self, volume: &str) -> Result<(), String>;
}

/// The runtime's own volumes, as the boundary makes and removes them.
pub struct RuntimeVolumes<'a>(pub &'a dyn crate::boundary::Backend);

#[async_trait]
impl BundleVolumes for RuntimeVolumes<'_> {
    async fn stage(&self, volume: &str, source: &Path) -> Result<(), String> {
        self.0
            .import_volume(volume, source)
            .await
            .map_err(|e| e.to_string())
    }

    async fn remove(&self, volume: &str) -> Result<(), String> {
        self.0
            .remove_volume(volume)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Where a fetch's Git runs.
pub enum Place<'a> {
    /// In the running sandbox that holds the workspace, at `dir` in it, over
    /// the node's own connection to it. Nothing is made, and nothing there
    /// but the step itself is ever stopped.
    Live { conn: SandboxConn, dir: String },
    /// In a sandbox of the fetch's own, made by `runner`.
    Sandbox {
        runner: &'a dyn Runner,
        volumes: &'a dyn BundleVolumes,
        /// The workspace, mounted where Git runs.
        workspace: Mount,
        /// Where the bundle's volume is mounted.
        bundle_dir: String,
        /// The node keeps the workspace in the runtime's place: the runner
        /// locks it per send and per take-back itself, and holding its lock
        /// here around a run would wait on that forever.
        kept: bool,
    },
}

impl Place<'_> {
    fn label(&self) -> &'static str {
        match self {
            Place::Live { .. } => "live",
            Place::Sandbox { kept: true, .. } => "sandbox, node's copy",
            Place::Sandbox { kept: false, .. } => "sandbox, runtime volume",
        }
    }
}

/// Where a fetch left the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// The tip `origin/<branch>` now names.
    pub head: String,
    /// How far the workspace's HEAD is ahead of and behind it, when it has
    /// one.
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
}

/// One fetch into one workspace.
pub struct Fetch<'a> {
    pub session: &'a str,
    /// The workspace's volume.
    pub volume: &'a str,
    /// The fetch's own name: of its sandbox, its bundle volume and its
    /// bundle's directory in a live sandbox.
    pub name: &'a str,
    pub branch: &'a str,
    /// How long each stage may take ([`STEP_LIMIT`]).
    pub limit: Duration,
}

/// One fetch of a workspace at a time: two share the bundle volume's name.
fn fetch_lock(volume: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex};
    type Locks = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;
    static LOCKS: LazyLock<Locks> = LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap()
        .entry(volume.to_string())
        .or_default()
        .clone()
}

/// The output of a step that did not succeed, as an answer.
fn failed(what: &str, out: &std::process::Output) -> String {
    format!(
        "{what} in the workspace failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// The commit ids a `have` step printed.
fn listed(out: &std::process::Output) -> Result<Vec<String>, String> {
    if !out.status.success() {
        return Err(failed("reading the refs", out));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .collect())
}

impl Fetch<'_> {
    /// Read what the workspace has, ask `bundle` for what it lacks, and
    /// fetch that into it at `place`. One line is logged for its outcome.
    pub async fn run<F, Fut>(&self, place: Place<'_>, bundle: F) -> Result<Fetched, String>
    where
        F: FnOnce(Vec<String>) -> Fut,
        Fut: Future<Output = Result<Bundle, String>>,
    {
        let started = Instant::now();
        let path = place.label();
        let one = fetch_lock(self.volume);
        let _one = one.lock().await;
        let fetched = match place {
            Place::Live { conn, dir } => self.live(&conn, &dir, bundle).await,
            Place::Sandbox {
                runner,
                volumes,
                workspace,
                bundle_dir,
                kept,
            } => {
                // A volume the runtime keeps has one writer at a time: an
                // export for a submission reads a whole tree.
                let lock = crate::workspace::volume_lock(self.volume);
                let _volume = match kept {
                    true => None,
                    false => Some(lock.lock().await),
                };
                self.in_sandbox(runner, volumes, &workspace, &bundle_dir, bundle)
                    .await
            }
        };
        let ms = started.elapsed().as_millis() as u64;
        match &fetched {
            Ok(fetched) => tracing::info!(
                session = self.session,
                branch = self.branch,
                path,
                ms,
                head = %fetched.head,
                "fetch_branch moved origin/{} in the workspace",
                self.branch
            ),
            Err(error) => tracing::warn!(
                session = self.session,
                branch = self.branch,
                path,
                ms,
                %error,
                "fetch_branch failed"
            ),
        }
        fetched
    }

    /// The steps in a sandbox of the fetch's own.
    async fn in_sandbox<F, Fut>(
        &self,
        runner: &dyn Runner,
        volumes: &dyn BundleVolumes,
        workspace: &Mount,
        bundle_dir: &str,
        bundle: F,
    ) -> Result<Fetched, String>
    where
        F: FnOnce(Vec<String>) -> Fut,
        Fut: Future<Output = Result<Bundle, String>>,
    {
        // Reading the refs writes nothing, so nothing comes back from it.
        let run = |step: &str, rest: &[&str], read_only: bool, with_bundle: bool| {
            let mut mounts = vec![Mount {
                read_only,
                ..workspace.clone()
            }];
            if with_bundle {
                mounts.push(Mount::volume(self.name, bundle_dir, true));
            }
            bounded(
                runner,
                step_command(step, rest, mounts, &workspace.target, self.name),
                self.limit,
                self.session,
            )
        };
        let have = listed(&run("have", &[], true, false).await?)?;
        let bundle = bundle(have).await?;
        let fetched = match &bundle.stage {
            Some(stage) => {
                let staged = volumes.stage(self.name, stage).await;
                let _ = std::fs::remove_dir_all(stage);
                if let Err(error) = staged {
                    // The volume is made before the copy into it, so a
                    // failed copy leaves one behind.
                    if let Err(error) = volumes.remove(self.name).await {
                        tracing::warn!(%error, volume = %self.name, "unstaged bundle's volume not removed");
                    }
                    return Err(format!("the bundle could not be staged: {error}"));
                }
                let path = format!("{bundle_dir}/{BUNDLE_FILE}");
                let fetched = run("fetch", &[self.branch, &bundle.sha, &path], false, true).await;
                if let Err(error) = volumes.remove(self.name).await {
                    tracing::warn!(%error, volume = %self.name, "fetched bundle's volume not removed");
                }
                fetched
            }
            None => run("fetch", &[self.branch, &bundle.sha, ""], false, false).await,
        }?;
        self.answer(bundle.sha, &fetched)
    }

    /// The steps in the running sandbox that holds the workspace at `dir`.
    async fn live<F, Fut>(
        &self,
        conn: &SandboxConn,
        dir: &str,
        bundle: F,
    ) -> Result<Fetched, String>
    where
        F: FnOnce(Vec<String>) -> Fut,
        Fut: Future<Output = Result<Bundle, String>>,
    {
        let have = listed(&self.live_step(conn, dir, "have", &[]).await?)?;
        let bundle = bundle(have).await?;
        let fetched = match &bundle.stage {
            Some(stage) => {
                let inside = format!("/tmp/{}-{}", self.name, uuid::Uuid::now_v7().simple());
                let sent = tokio::time::timeout(
                    self.limit,
                    conn.import_dir(stage, &inside, Exclusions::None),
                )
                .await;
                let _ = std::fs::remove_dir_all(stage);
                let fetched = match sent {
                    Ok(Ok(_)) => {
                        let path = format!("{inside}/{BUNDLE_FILE}");
                        self.live_step(conn, dir, "fetch", &[self.branch, &bundle.sha, &path])
                            .await
                    }
                    Ok(Err(error)) => Err(format!(
                        "the bundle could not be sent into the session's sandbox: {error}"
                    )),
                    Err(_) => Err(format!(
                        "sending the bundle into the session's sandbox did not finish within {}s",
                        self.limit.as_secs()
                    )),
                };
                self.live_remove(conn, &inside).await;
                fetched
            }
            None => {
                self.live_step(conn, dir, "fetch", &[self.branch, &bundle.sha, ""])
                    .await
            }
        }?;
        self.answer(bundle.sha, &fetched)
    }

    /// One step of [`SCRIPT`] in the live sandbox, in `dir`, each stage
    /// bounded on its own so that the answer names the one that stalled.
    /// Only the step's own process group is ever killed: the sandbox, its
    /// harness and its connection are the session's.
    async fn live_step(
        &self,
        conn: &SandboxConn,
        dir: &str,
        step: &str,
        rest: &[&str],
    ) -> Result<std::process::Output, String> {
        let spec = SpawnSpec {
            argv: step_argv(step, rest),
            env: step_env().into_iter().collect(),
            cwd: Some(dir.into()),
            stdin: false,
            stdout: true,
            stderr: true,
            ..SpawnSpec::default()
        };
        let process = match tokio::time::timeout(self.limit, conn.spawn(spec)).await {
            Ok(Ok(process)) => process,
            Ok(Err(error)) => {
                return Err(format!(
                    "Git could not be started in the session's sandbox: {error}"
                ))
            }
            Err(_) => {
                return Err(format!(
                    "starting Git in the session's sandbox did not finish within {}s",
                    self.limit.as_secs()
                ))
            }
        };
        match tokio::time::timeout(self.limit, process.output()).await {
            Ok(Ok(out)) => Ok(std::process::Output {
                status: crate::runner::protocol::std_status(crate::runner::protocol::exit_code(
                    out.status,
                )),
                stdout: out.stdout,
                stderr: out.stderr,
            }),
            Ok(Err(error)) => Err(format!(
                "Git in the session's sandbox could not be followed to its end: {error}"
            )),
            Err(_) => {
                tracing::warn!(
                    session = self.session,
                    step,
                    pid = process.pid(),
                    "fetch step's Git did not finish in the session's sandbox; killing it"
                );
                if let Err(error) = process.signal_group(libc::SIGKILL).await {
                    tracing::warn!(%error, session = self.session, "timed-out fetch step not killed");
                }
                Err(git_hung(self.limit))
            }
        }
    }

    /// Remove the bundle's directory from the live sandbox. A failure is
    /// logged: the directory goes with the sandbox.
    async fn live_remove(&self, conn: &SandboxConn, dir: &str) {
        let spec = SpawnSpec {
            argv: vec!["rm".into(), "-rf".into(), "--".into(), dir.into()],
            stdin: false,
            ..SpawnSpec::default()
        };
        let removed = tokio::time::timeout(self.limit, async {
            let process = conn.spawn(spec).await.map_err(|e| e.to_string())?;
            let out = process.output().await.map_err(|e| e.to_string())?;
            match out.status.success() {
                true => Ok(()),
                false => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
            }
        })
        .await
        .unwrap_or_else(|_| Err("timed out".into()));
        if let Err(error) = removed {
            tracing::warn!(%error, session = self.session, dir, "fetched bundle not removed from the session's sandbox");
        }
    }

    /// What a `fetch` step's output says, for a tip of `head`.
    fn answer(&self, head: String, out: &std::process::Output) -> Result<Fetched, String> {
        if !out.status.success() {
            return Err(failed(&format!("fetching origin/{}", self.branch), out));
        }
        let counts = String::from_utf8_lossy(&out.stdout);
        let (ahead, behind) = match counts.split_whitespace().collect::<Vec<_>>()[..] {
            [ahead, behind] => (ahead.parse::<u64>().ok(), behind.parse::<u64>().ok()),
            _ => (None, None),
        };
        Ok(Fetched {
            head,
            ahead,
            behind,
        })
    }
}

pub async fn call(
    access: &SessionAccess,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let session_id = ctx.session_id().ok_or(
        "fetch_branch fetches into a workspace this node runs; fetch in your own checkout",
    )?;
    let branch = args
        .get("branch")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if let Some(refusal) = refusal(branch) {
        return Err(refusal);
    }
    let session = access
        .store
        .get_session(session_id)
        .map_err(|e| e.to_string())?
        .ok_or("this session is gone")?;
    let repo = crate::environment::origin_repo(&access.store, Some(session_id))?
        .ok_or("this session has no repository to fetch from")?;
    let repo = crate::forge::managed_clone(&repo)?;
    let manager = &access.manager;
    let backend = manager.backend().as_ref();
    let workspace_id = session
        .repo_path
        .strip_prefix("workspace://")
        .unwrap_or(&session.id);
    let workspace = Workspace {
        id: workspace_id.to_string(),
        volume: crate::workspace::volume_name(workspace_id),
        snapshot: crate::workspace::snapshot_path(workspace_id),
    };
    let name = workspace.volume.replacen(
        &crate::config::resource_name("workspace-"),
        &crate::config::resource_name("fetch-"),
        1,
    );

    let stage = crate::workspace::staging_path(&name);
    let bundle = |have: Vec<String>| async move {
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
        let bundled = async {
            let lock = crate::forge::clone_lock(&repo);
            let _clone = lock.lock().await;
            let brokered = crate::forge::git_token_for(
                manager.broker(),
                &Config::state_dir(),
                &ctx.channel,
                &repo,
                &ctx.node_id,
            );
            let credential = match &brokered {
                Some((forge, token)) => {
                    crate::git_remote::brokered(forge.git_user(), Some(token.as_str()))
                }
                None => crate::git_remote::Credential::Anonymous,
            };
            tokio::time::timeout(
                std::time::Duration::from_secs(300),
                crate::forge::fetch_managed(&repo, &credential),
            )
            .await
            .unwrap_or_else(|_| Err("git fetch ran out of time".into()))
            .map_err(|error| format!("origin could not be fetched: {error}"))?;
            crate::forge::bundle_branch(&repo, branch, &have, &stage.join(BUNDLE_FILE)).await
        }
        .await;
        match bundled {
            Ok(bundled) if bundled.bundled => Ok(Bundle {
                sha: bundled.sha,
                stage: Some(stage),
            }),
            Ok(bundled) => {
                let _ = std::fs::remove_dir_all(&stage);
                Ok(Bundle {
                    sha: bundled.sha,
                    stage: None,
                })
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&stage);
                Err(error)
            }
        }
    };

    let fetch = Fetch {
        session: session_id,
        volume: &workspace.volume,
        name: &name,
        branch,
        limit: STEP_LIMIT,
    };
    let runner = backend.runner_for(&session.harness_id, Vec::new());
    let volumes = RuntimeVolumes(backend);
    // The session's sandbox holds the workspace's newer copy while it runs,
    // and is where the agent will look: Git runs there. With none running,
    // the node's copy (or the runtime's volume) is the workspace.
    let place = match crate::runner::sandbox_files::live_target(&workspace.volume) {
        Some((conn, dir)) => Place::Live { conn, dir },
        None => Place::Sandbox {
            runner: runner.as_ref(),
            volumes: &volumes,
            workspace: workspace.mount(WORKDIR, false),
            bundle_dir: BUNDLE_DIR.into(),
            kept: backend
                .files_for(&workspace.volume)
                .is_some_and(|store| store.holds(&workspace.volume)),
        },
    };
    let fetched = fetch.run(place, bundle).await?;
    Ok(json!({
        "branch": branch,
        "ref": format!("origin/{branch}"),
        "head": fetched.head,
        "ahead": fetched.ahead,
        "behind": fetched.behind,
        "message": format!(
            "origin/{branch} is at {:.12} in your workspace. Your working tree, index and \
             branches are as you left them: rebase, merge or check out from origin/{branch} \
             yourself.",
            fetched.head
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sh(dir: &Path, script: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// What the container runs, run here in the workspace's place.
    fn step(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(SCRIPT)
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
    }

    #[test]
    fn branch_names_git_would_refuse_are_refused() {
        for name in ["main", "feat/something", "release-1.2", "user/fix_7"] {
            assert_eq!(refusal(name), None, "{name}");
        }
        for name in [
            "",
            "-upload-pack=x",
            "--all",
            "a b",
            "a..b",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[",
            "a\\b",
            "@",
            "a@{1}",
            "/a",
            "a/",
            "a//b",
            ".a",
            "a/.b",
            "a.lock",
            "a.",
            "a\u{7}",
        ] {
            assert!(refusal(name).is_some(), "{name:?}");
        }
    }

    /// A workspace with no remote, as a session's is, is brought the new
    /// tip of a branch through a bundle holding only what it lacks: its
    /// `origin/main` moves, and nothing else does.
    #[tokio::test]
    async fn a_workspace_is_brought_a_branch_by_bundle_and_only_its_remote_ref_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = managed_clone(tmp.path(), "fetch");
        sh(
            tmp.path(),
            &format!(
                "git clone -q --no-local {} ws && cd ws && git remote remove origin \
                 && git update-ref refs/remotes/origin/main HEAD \
                 && git checkout -qb feat/x && echo mine > b.txt && git add b.txt \
                 && git -c user.name=t -c user.email=t@t commit -qm mine",
                clone.display()
            ),
        );
        let ws = tmp.path().join("ws");
        let before = sh(&ws, "git rev-parse HEAD");
        let tip = sh(
            &tmp.path().join("work"),
            "echo two > a.txt && git -c user.name=t -c user.email=t@t commit -qam two \
             && git push -q origin main && git rev-parse HEAD",
        );
        crate::forge::fetch_managed(&clone, &crate::git_remote::Credential::Anonymous)
            .await
            .unwrap();

        let listed = step(&ws, &["have"]);
        assert!(listed.status.success());
        let have: Vec<String> = String::from_utf8_lossy(&listed.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        let bundle = tmp.path().join(BUNDLE_FILE);
        let bundled = crate::forge::bundle_branch(&clone, "main", &have, &bundle)
            .await
            .unwrap();
        assert_eq!(bundled.sha, tip);
        assert!(bundled.bundled);
        // Only the one new commit travels.
        let listed = sh(
            tmp.path(),
            &format!("git bundle list-heads {}", bundle.display()),
        );
        assert!(listed.contains("refs/remotes/origin/main"), "{listed}");

        let fetched = step(&ws, &["fetch", "main", &tip, &bundle.to_string_lossy()]);
        assert!(
            fetched.status.success(),
            "{}",
            String::from_utf8_lossy(&fetched.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&fetched.stdout).trim(), "1\t1");
        assert_eq!(sh(&ws, "git rev-parse origin/main"), tip);
        assert_eq!(sh(&ws, "git rev-parse HEAD"), before);
        assert_eq!(sh(&ws, "git rev-parse --abbrev-ref HEAD"), "feat/x");
        assert_eq!(sh(&ws, "git status --porcelain"), "");

        // Fetched again, the workspace holds the tip: no bundle, the ref
        // stays where it is.
        let have = vec![tip.clone()];
        let again = crate::forge::bundle_branch(&clone, "main", &have, &bundle)
            .await
            .unwrap();
        assert!(!again.bundled);
        let moved = step(&ws, &["fetch", "main", &tip, ""]);
        assert!(moved.status.success());
        assert_eq!(sh(&ws, "git rev-parse origin/main"), tip);

        let missing = crate::forge::bundle_branch(&clone, "nope", &have, &bundle)
            .await
            .unwrap_err();
        assert!(missing.contains("origin has no branch nope"), "{missing}");
        let _ = std::fs::remove_dir_all(clone.parent().unwrap());
    }

    /// A step in a sandbox of its own that Git cannot finish, here
    /// `for-each-ref` reading a FIFO the agent put at `.git/packed-refs`, is
    /// stopped at its limit: the workspace's volume lock is let go and
    /// nothing is left reading. The answer does not blame Git, since from
    /// outside the sandbox nothing says Git ever started. (The live path's
    /// own is in `tests/protocol_runner.rs`.)
    #[cfg(unix)]
    #[tokio::test]
    async fn a_step_that_hangs_is_stopped_and_lets_the_volume_go() {
        use std::os::unix::fs::OpenOptionsExt;

        let tmp = tempfile::tempdir().unwrap();
        sh(tmp.path(), "git init -q ws && mkfifo ws/.git/packed-refs");
        let ws = tmp.path().join("ws");
        let tag = uuid::Uuid::now_v7().simple().to_string();
        let lock = crate::workspace::volume_lock(&format!("tracon-workspace-{tag}"));
        let (locked, held) = tokio::sync::oneshot::channel();
        let fetch = tokio::spawn({
            let lock = lock.clone();
            let ws = ws.clone();
            async move {
                let _volume = lock.lock().await;
                let _ = locked.send(());
                let cmd = step_command(
                    "have",
                    &[],
                    Vec::new(),
                    &ws.to_string_lossy(),
                    &format!("tracon-fetch-{tag}"),
                );
                bounded(
                    &crate::runner::local::LocalRunner,
                    cmd,
                    Duration::from_secs(1),
                    "s1",
                )
                .await
            }
        });
        held.await.unwrap();
        let next = tokio::time::timeout(Duration::from_secs(30), lock.lock()).await;
        assert!(
            next.is_ok(),
            "the volume lock was held past the step's limit"
        );
        drop(next);
        let answer = fetch.await.unwrap().unwrap_err();
        assert!(
            answer.contains(
                "did not finish within 1s (starting the sandbox, sending the workspace, or Git)"
            ),
            "{answer}"
        );
        assert!(!answer.contains("FIFO"), "{answer}");

        // A FIFO opened for writing without blocking is refused while no one
        // has it open for reading: once Git is gone, it is. The kill is a
        // signal, so give the process a moment to exit first.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let open = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(ws.join(".git/packed-refs"));
        assert!(
            matches!(&open, Err(error) if error.raw_os_error() == Some(libc::ENXIO)),
            "git still reads the FIFO: {open:?}"
        );
    }

    /// An upstream with one commit on `main`, a working copy to advance it
    /// from, and a managed clone of it.
    fn managed_clone(tmp: &Path, tag: &str) -> std::path::PathBuf {
        sh(
            tmp,
            "git init -q --bare -b main upstream.git && git clone -q upstream.git work \
             && cd work && echo one > a.txt && git add a.txt \
             && git -c user.name=t -c user.email=t@t commit -qm one && git push -q origin main",
        );
        let owner = format!("{tag}-{}", uuid::Uuid::now_v7().simple());
        let clone = crate::forge::managed_root(&Config::state_dir())
            .join("github.com")
            .join(owner)
            .join("proj");
        std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
        sh(
            clone.parent().unwrap(),
            &format!("git clone -q {} proj", tmp.join("upstream.git").display()),
        );
        clone
    }
}
