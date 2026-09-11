//! Deterministic supervision: what a test suite can say, a test suite says.
//! At submit the node runs the project's checks in a throwaway harness
//! container with the worktree mounted and nothing else — no credentials,
//! no gateway token, no MCP — and feeds failures back to the agent as the
//! reason the submission was refused.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::boundary::Backend;
use crate::config::Config;
use crate::workspace::Workspace;
use crate::runner::RunnerCommand;

/// Required checks are node policy, not a repository-controlled file. A
/// candidate may describe useful commands in its own docs, but it cannot
/// silently replace the operator's required-check set.
pub const CHECKS_FILE: &str = ".tracon/checks";
/// How much of the output is kept per check.
const TAIL_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckResult {
    pub command: String,
    pub ok: bool,
    pub exit: Option<i32>,
    /// The last few KiB of stdout+stderr.
    pub tail: String,
    pub ms: u64,
}

/// The configured required checks. Repository files are intentionally ignored:
/// the candidate cannot relax or redirect the gate it is about to satisfy.
pub fn commands_for(cfg: &Config) -> Vec<String> {
    cfg.supervision.checks.clone()
}

/// Run every check in order, stopping at the first failure. Each runs as
/// `sh -lc <command>` in `/work` inside a fresh container named for the
/// session, under the configured timeout.
pub async fn run(
    backend: &dyn Backend,
    cfg: &Config,
    workspace: &Workspace,
    session_slug: &str,
    commands: &[String],
) -> Vec<CheckResult> {
    let runner = backend.runner(Vec::new());
    let mount = workspace.mount("/work", true);
    let timeout = Duration::from_secs(cfg.supervision.timeout_secs.max(1));
    let mut out = Vec::new();
    for (i, command) in commands.iter().enumerate() {
        let started = std::time::Instant::now();
        let cmd = RunnerCommand {
            argv: vec!["sh".into(), "-lc".into(), command.clone()],
            env: Vec::new(),
            mounts: vec![mount.clone()],
            workdir: Some("/work".into()),
            name: format!("tracon-check-{session_slug}-{i}"),
            image: None,
        };
        let result = match tokio::time::timeout(timeout, runner.run_capture(cmd)).await {
            Ok(Ok(o)) => {
                let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&o.stderr));
                CheckResult {
                    command: command.clone(),
                    ok: o.status.success(),
                    exit: o.status.code(),
                    tail: tail(&text),
                    ms: started.elapsed().as_millis() as u64,
                }
            }
            Ok(Err(e)) => CheckResult {
                command: command.clone(),
                ok: false,
                exit: None,
                tail: format!("could not run: {e}"),
                ms: started.elapsed().as_millis() as u64,
            },
            Err(_) => {
                let _ = runner
                    .kill(&format!("tracon-check-{session_slug}-{i}"))
                    .await;
                CheckResult {
                    command: command.clone(),
                    ok: false,
                    exit: None,
                    tail: format!("timed out after {}s", timeout.as_secs()),
                    ms: started.elapsed().as_millis() as u64,
                }
            }
        };
        let ok = result.ok;
        out.push(result);
        if !ok {
            break;
        }
    }
    out
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

/// The legacy review path records the deterministic check results beside the
/// captured candidate. Before any publication, require that record to exist
/// and contain no failed check. Candidate evidence extends this same gate with
/// input/image freshness; callers use one entry point rather than choosing a
/// weaker approval path.
pub fn review_required_checks_current(
    store: &crate::store::Store,
    review_id: &str,
    _cfg: &Config,
) -> Result<(), String> {
    let review = store
        .get_review(review_id)
        .map_err(|e| e.to_string())?
        .ok_or("review is gone")?;
    let Some(recorded) = review.checks_json else {
        // External harness submissions do not run the node's isolated checks;
        // they remain eligible for explicit review, never automatic publish.
        return Err("automatic publication requires node-recorded check evidence".into());
    };
    let checks: Vec<CheckResult> = serde_json::from_str(&recorded)
        .map_err(|_| "recorded check evidence is malformed".to_string())?;
    if let Some(failed) = checks.iter().find(|check| !check.ok) {
        return Err(format!("required check failed: {}", failed.command));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_checks_cannot_replace_node_policy() {
        let dir = std::env::temp_dir().join(format!("tracon-checks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".tracon")).unwrap();
        std::fs::write(
            dir.join(CHECKS_FILE),
            "# project checks\n\ncargo test\n  bun test  \n",
        )
        .unwrap();
        let cfg = Config::default();
        assert_eq!(commands_for(&cfg), vec!["just check".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
