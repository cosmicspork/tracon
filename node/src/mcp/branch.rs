//! `fetch_branch`: bring a branch of the repository's origin up to date in
//! the session's own workspace, at any time.
//!
//! A managed workspace is a clone of the node's managed clone with no remote
//! of its own, by design: its container has nothing it could fetch from. So
//! the fetch runs on the node. The managed clone is fetched from the forge
//! with the repository's credential, as for a session's start, and what the
//! workspace lacks of the branch is carried to it as a bundle and fetched by
//! a short-lived container that mounts only the workspace and the bundle, as
//! the workspace's owner and with no credential. It runs on the harness
//! network with the gateway's proxy environment, as a preparation does, but
//! fetches nothing over it: the bundle is all it reads. Only
//! `refs/remotes/origin/<branch>` moves: the working tree, the index and the
//! local branches are the agent's, which rebases, merges or checks out itself.
//!
//! It reads the forge and writes nothing there, so like `show_work` it is
//! dispatched before the policy is consulted and is never asked.

use std::time::Duration;

use serde_json::{json, Value};

use crate::{
    config::Config,
    mcp::{CallContext, SessionAccess},
    runner::{Mount, Runner, RunnerCommand},
    workspace::Workspace,
};

pub const FETCH: &str = "fetch_branch";

/// Where the bundle is mounted in the container that fetches it.
const BUNDLE_DIR: &str = "/fetch";
const BUNDLE_FILE: &str = "branch.bundle";

/// How long one step in the workspace may run. Each holds the workspace's
/// volume lock, and Git there reads a `.git` the agent controls: a FIFO for
/// `packed-refs`, or a config that sends a fetch to a command that never
/// returns, would otherwise hold every export of the workspace until the node
/// restarts.
const STEP_LIMIT: Duration = Duration::from_secs(120);

/// What the container runs, in the workspace. `$1` is the branch, `$2` its
/// tip in the managed clone and `$3` the bundle to fetch it from, or empty
/// when the workspace already holds the tip and only the ref moves. It prints
/// the commit ids the workspace's refs name (`have`), or how far its HEAD is
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

/// The command for one step of [`SCRIPT`] in the workspace, named `name`.
fn step_command(step: &str, rest: &[&str], mounts: Vec<Mount>, name: &str) -> RunnerCommand {
    let mut argv = vec!["sh".into(), "-c".into(), SCRIPT.into(), step.into()];
    argv.extend(rest.iter().map(|arg| arg.to_string()));
    RunnerCommand {
        argv,
        env: vec![
            ("HOME".into(), "/tmp".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ],
        secret_env: Default::default(),
        mounts,
        workdir: Some("/work".into()),
        image: None,
        name: name.to_string(),
    }
}

/// Run a step for at most `limit`. One that runs out of time is killed by
/// the name the runtime gave it, since not every runner stops a capture whose
/// future is dropped, and the caller is answered with an error so the volume
/// lock it holds is let go.
async fn bounded(
    runner: &dyn Runner,
    cmd: RunnerCommand,
    limit: Duration,
) -> Result<std::process::Output, String> {
    let running = runner.capture_name(&cmd.name);
    match tokio::time::timeout(limit, runner.run_capture(cmd)).await {
        Ok(finished) => finished.map_err(|e| e.to_string()),
        Err(_) => {
            if let Err(error) = runner.kill(&running).await {
                tracing::warn!(%error, name = %running, "timed-out fetch step not killed");
            }
            Err(format!(
                "Git in the workspace did not finish in {}s and was stopped; \
                 its .git may hold a FIFO, or a config naming a command that does not return",
                limit.as_secs()
            ))
        }
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
    let workspace_id = session
        .repo_path
        .strip_prefix("workspace://")
        .unwrap_or(&session.id);
    let workspace = Workspace {
        id: workspace_id.to_string(),
        volume: crate::workspace::volume_name(workspace_id),
        snapshot: crate::workspace::snapshot_path(workspace_id),
    };
    let runner = manager
        .backend()
        .runner_for(&session.harness_id, Vec::new());
    let name = workspace
        .volume
        .replacen("tracon-workspace-", "tracon-fetch-", 1);
    // One writer at a time on the volume: an export for a submission reads a
    // whole tree, and two fetches share the bundle's volume.
    let lock = crate::workspace::volume_lock(&workspace.volume);
    let _volume = lock.lock().await;
    let run = |step: &str, rest: &[&str], bundle: Option<&str>| {
        let mut mounts = vec![workspace.mount("/work", false)];
        if let Some(volume) = bundle {
            mounts.push(Mount::volume(volume, BUNDLE_DIR, true));
        }
        bounded(
            runner.as_ref(),
            step_command(step, rest, mounts, &name),
            STEP_LIMIT,
        )
    };
    let failed = |what: &str, out: &std::process::Output| {
        format!(
            "{what} in the workspace failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    };

    let listed = run("have", &[], None).await?;
    if !listed.status.success() {
        return Err(failed("reading the refs", &listed));
    }
    let have: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .collect();

    let stage = crate::workspace::staging_path(&name);
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    let bundled = {
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
    };
    let bundled = match bundled {
        Ok(bundled) => bundled,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(error);
        }
    };

    let fetched = if bundled.bundled {
        let imported = manager.backend().import_volume(&name, &stage).await;
        let _ = std::fs::remove_dir_all(&stage);
        if let Err(error) = imported {
            // The volume is made before the copy into it, so a failed copy
            // leaves one behind.
            if let Err(error) = manager.backend().remove_volume(&name).await {
                tracing::warn!(%error, volume = %name, "unstaged bundle's volume not removed");
            }
            return Err(format!("the bundle could not be staged: {error}"));
        }
        let path = format!("{BUNDLE_DIR}/{BUNDLE_FILE}");
        let fetched = run("fetch", &[branch, &bundled.sha, &path], Some(&name)).await;
        if let Err(error) = manager.backend().remove_volume(&name).await {
            tracing::warn!(%error, volume = %name, "fetched bundle's volume not removed");
        }
        fetched
    } else {
        let _ = std::fs::remove_dir_all(&stage);
        run("fetch", &[branch, &bundled.sha, ""], None).await
    }?;
    if !fetched.status.success() {
        return Err(failed(&format!("fetching origin/{branch}"), &fetched));
    }
    let counts = String::from_utf8_lossy(&fetched.stdout);
    let (ahead, behind) = match counts.split_whitespace().collect::<Vec<_>>()[..] {
        [ahead, behind] => (ahead.parse::<u64>().ok(), behind.parse::<u64>().ok()),
        _ => (None, None),
    };
    Ok(json!({
        "branch": branch,
        "ref": format!("origin/{branch}"),
        "head": bundled.sha,
        "ahead": ahead,
        "behind": behind,
        "message": format!(
            "origin/{branch} is at {:.12} in your workspace. Your working tree, index and \
             branches are as you left them: rebase, merge or check out from origin/{branch} \
             yourself.",
            bundled.sha
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

    /// A step Git cannot finish, here `for-each-ref` reading a FIFO the
    /// agent put at `.git/packed-refs`, is stopped at its limit: the
    /// workspace's volume lock is let go and nothing is left reading.
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
                let mut cmd = step_command("have", &[], Vec::new(), &format!("tracon-fetch-{tag}"));
                cmd.workdir = Some(ws.to_string_lossy().into_owned());
                bounded(
                    &crate::runner::local::LocalRunner,
                    cmd,
                    Duration::from_secs(1),
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
        assert!(answer.contains("did not finish in 1s"), "{answer}");

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
