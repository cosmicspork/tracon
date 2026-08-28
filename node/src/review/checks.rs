//! Deterministic supervision: what a test suite can say, a test suite says.
//! At submit the node runs the project's checks in a throwaway harness
//! container with the worktree mounted and nothing else — no credentials,
//! no gateway token, no MCP — and feeds failures back to the agent as the
//! reason the submission was refused.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::boundary::Backend;
use crate::config::Config;
use crate::runner::{Mount, RunnerCommand};

/// A worktree may carry its own list: one command per line, `#` comments.
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

/// The commands to run for a worktree: its own file, else the node's list.
pub fn commands_for(cfg: &Config, worktree: &Path) -> Vec<String> {
    let own = std::fs::read_to_string(worktree.join(CHECKS_FILE))
        .ok()
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty());
    own.unwrap_or_else(|| cfg.supervision.checks.clone())
}

/// Run every check in order, stopping at the first failure. Each runs as
/// `sh -lc <command>` in `/work` inside a fresh container named for the
/// session, under the configured timeout.
pub async fn run(
    backend: &dyn Backend,
    cfg: &Config,
    worktree: &Path,
    session_slug: &str,
    commands: &[String],
) -> Vec<CheckResult> {
    // The worktree rides on the command, not the runner, so every backend
    // (including the local one tests use) sees it in the same place.
    let runner = backend.runner(Vec::new());
    let mount = Mount {
        source: worktree.to_string_lossy().into_owned(),
        target: "/work".into(),
        read_only: false,
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worktree_list_wins_and_comments_are_skipped() {
        let dir = std::env::temp_dir().join(format!("tracon-checks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".tracon")).unwrap();
        let cfg = Config::default();
        assert_eq!(commands_for(&cfg, &dir), vec!["just check".to_string()]);
        std::fs::write(
            dir.join(CHECKS_FILE),
            "# project checks\n\ncargo test\n  bun test  \n",
        )
        .unwrap();
        assert_eq!(
            commands_for(&cfg, &dir),
            vec!["cargo test".to_string(), "bun test".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
