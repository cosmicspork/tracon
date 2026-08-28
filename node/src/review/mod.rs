//! Review capture and publication.
//!
//! The agent submits an intent; the node captures the diff itself from the
//! worktree it created, and the node publishes the approved bytes. The agent
//! never holds a forge token and never runs the publishing CLI, so "review
//! before publish" is a property of the system rather than an instruction the
//! agent may forget by hour two.

pub mod checks;
pub mod publish;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("git {op} failed: {stderr}")]
    Git { op: &'static str, stderr: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("nothing to review: no commits on {branch} beyond {base}")]
    Empty { branch: String, base: String },
    #[error("{0}")]
    Rejected(String),
}

/// One file as it stood when the review was submitted. The blob is git's own
/// content hash, so a file that changed after submit is detectable without
/// keeping a copy of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAtSubmit {
    pub path: String,
    pub blob: String,
}

#[derive(Debug, Clone)]
pub struct Capture {
    pub diff: String,
    pub files: Vec<FileAtSubmit>,
    pub head_sha: String,
    pub base_ref: String,
    pub added: i64,
    pub removed: i64,
    /// Uncommitted work is not in the diff. The operator is told rather than
    /// left to wonder why the review looks short.
    pub uncommitted: Vec<String>,
}

/// Global git options that disable every config-driven way a command can run
/// another program: hooks and the fsmonitor daemon. The node runs git host-side
/// against a worktree whose `.git` the harness can partly write, so even though
/// `config`/`hooks`/`info` are mounted read-only (see `materialize`), these
/// overrides are the second, independent line. Diff commands additionally pass
/// `--no-ext-diff --no-textconv` at the call site.
const GIT_SAFE: &[&str] = &["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor="];

async fn git(dir: &str, op: &'static str, args: &[&str]) -> Result<String, ReviewError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(GIT_SAFE)
        .args(args)
        .output()
        .await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(ReviewError::Git {
            op,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// The default branch the worktree was cut from, read from `origin/HEAD`. The
/// worktree shares the repo's refs, so this is resolvable there. Returns the
/// plain branch name (e.g. `main`), which is the branch a change merges into.
pub async fn default_base(worktree: &str) -> Result<String, ReviewError> {
    let head = git(
        worktree,
        "symbolic-ref",
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .await?;
    // `origin/main` → `main`. Strip only the `origin/` remote prefix so a
    // multi-segment branch (`release/2026.1`) survives intact.
    Ok(head.strip_prefix("origin/").unwrap_or(&head).to_string())
}

/// Capture what the branch contains beyond its base. Three-dot: the changes the
/// branch introduces, not everything that happened on the base since. `base_ref`
/// is a ref the worktree can resolve — a remote-tracking ref like `origin/main`,
/// so the diff is against what the change will actually merge into rather than a
/// possibly-stale local branch.
pub async fn capture(worktree: &str, base_ref: &str, branch: &str) -> Result<Capture, ReviewError> {
    let range = format!("{base_ref}...HEAD");
    let head_sha = git(worktree, "rev-parse", &["rev-parse", "HEAD"]).await?;

    // `--no-ext-diff --no-textconv`: an external diff or textconv driver named
    // by `.gitattributes` would run its configured command; disable both so the
    // capture cannot be turned into a node-side exec.
    let diff = git(
        worktree,
        "diff",
        &["diff", "--no-ext-diff", "--no-textconv", &range],
    )
    .await?;
    if diff.trim().is_empty() {
        return Err(ReviewError::Empty {
            branch: branch.to_string(),
            base: base_ref.to_string(),
        });
    }

    let numstat = git(
        worktree,
        "diff --numstat",
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            &range,
        ],
    )
    .await?;
    let (mut added, mut removed) = (0i64, 0i64);
    let mut paths = Vec::new();
    for line in numstat.lines() {
        let mut cols = line.split('\t');
        let a = cols.next().unwrap_or("0");
        let r = cols.next().unwrap_or("0");
        let path = cols.next().unwrap_or("").to_string();
        // Binary files report "-"; they count as changed but not as lines.
        added += a.parse::<i64>().unwrap_or(0);
        removed += r.parse::<i64>().unwrap_or(0);
        if !path.is_empty() {
            paths.push(path);
        }
    }

    let mut files = Vec::new();
    for path in &paths {
        // A deleted file has no blob at HEAD; record it as absent rather than
        // failing the capture.
        let blob = git(
            worktree,
            "rev-parse",
            &["rev-parse", &format!("HEAD:{path}")],
        )
        .await
        .unwrap_or_else(|_| "absent".into());
        files.push(FileAtSubmit {
            path: path.clone(),
            blob,
        });
    }

    let uncommitted = git(worktree, "status", &["status", "--porcelain"])
        .await
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.get(3..).map(str::to_string))
        .collect();

    Ok(Capture {
        diff,
        files,
        head_sha,
        base_ref: base_ref.to_string(),
        added,
        removed,
        uncommitted,
    })
}

/// What changed in the worktree since the review was submitted. Empty means the
/// diff still describes the branch.
/// One reviewed file's contents as they were submitted, read by blob hash so
/// what comes back is what the diff was taken against — not whatever the
/// worktree holds now. A file the diff created has no blob at the base, and a
/// binary one has nothing worth editing; both answer `None`.
pub async fn file_at_submit(
    worktree: &str,
    files: &[FileAtSubmit],
    path: &str,
) -> Result<Option<String>, ReviewError> {
    let Some(f) = files.iter().find(|f| f.path == path) else {
        return Ok(None);
    };
    if f.blob == "absent" {
        return Ok(None);
    }
    // Not `git`: that trims, and a file's trailing newline is part of the
    // file. Losing it here would make the editor build a patch that quietly
    // strips it.
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(GIT_SAFE)
        .args(["cat-file", "blob", &f.blob])
        .output()
        .await?;
    if !out.status.success() {
        return Err(ReviewError::Git {
            op: "cat-file",
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    // A file the operator can edit is text; anything else is not for this.
    let Ok(text) = String::from_utf8(out.stdout) else {
        return Ok(None);
    };
    Ok(Some(text))
}

pub async fn staleness(worktree: &str, head_sha: &str, files: &[FileAtSubmit]) -> Vec<String> {
    let now = match git(worktree, "rev-parse", &["rev-parse", "HEAD"]).await {
        Ok(sha) => sha,
        // A worktree that has gone away is the strongest possible staleness.
        Err(_) => return vec!["the worktree is no longer readable".into()],
    };
    if now == head_sha {
        return Vec::new();
    }
    let mut moved = Vec::new();
    for f in files {
        let blob = git(
            worktree,
            "rev-parse",
            &["rev-parse", &format!("HEAD:{}", f.path)],
        )
        .await
        .unwrap_or_else(|_| "absent".into());
        if blob != f.blob {
            moved.push(f.path.clone());
        }
    }
    if moved.is_empty() {
        // New commits that touched none of the reviewed files still mean the
        // branch is not what was approved.
        moved.push(format!("the branch moved to {}", &now[..now.len().min(8)]));
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sh(dir: &std::path::Path, script: &str) {
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Per test: these run in parallel, so a shared directory means one test
    /// commits into another's repo and both read the wrong thing.
    async fn repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tracon-review-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(
            &dir,
            "git init -q -b main . && git config user.email t@e && git config user.name t \
             && echo one > a.txt && git add -A && git commit -qm base \
             && git checkout -qb feat/x && echo two >> a.txt && echo new > b.txt \
             && git add -A && git commit -qm work",
        )
        .await;
        dir
    }

    #[tokio::test]
    async fn default_base_reads_origin_head_and_keeps_multi_segment_names() {
        const FN: &str = "default_base_reads_origin_head";
        let dir = std::env::temp_dir().join(format!("tracon-review-{}-{FN}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A default branch that is not `main` and has a slash in it: the old
        // `rsplit('/')` would have returned `2026.1`, and the old hardcoded
        // default returned `main` regardless.
        sh(
            &dir,
            "git init -q --bare -b release/2026.1 origin.git \
             && git clone -q origin.git wt && cd wt \
             && git config user.email t@e && git config user.name t \
             && echo hi > a.txt && git add -A && git commit -qm base \
             && git push -q origin release/2026.1 \
             && git remote set-head origin -a",
        )
        .await;
        let wt = dir.join("wt");
        assert_eq!(
            default_base(wt.to_str().unwrap()).await.unwrap(),
            "release/2026.1"
        );
    }

    #[tokio::test]
    async fn capture_describes_what_the_branch_adds() {
        const FN: &str = "capture_describes_what_the_branch_adds";
        let dir = repo(FN).await;
        let c = capture(dir.to_str().unwrap(), "main", "feat/x")
            .await
            .unwrap();
        assert!(c.diff.contains("b.txt"));
        assert_eq!(c.added, 2);
        assert_eq!(c.removed, 0);
        let paths: Vec<&str> = c.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.txt"]);
        assert!(c.uncommitted.is_empty());
    }

    #[tokio::test]
    async fn a_branch_with_no_changes_is_refused() {
        const FN: &str = "a_branch_with_no_changes_is_refused";
        let dir = repo(FN).await;
        sh(&dir, "git checkout -q main").await;
        let err = capture(dir.to_str().unwrap(), "main", "main")
            .await
            .unwrap_err();
        assert!(matches!(err, ReviewError::Empty { .. }));
    }

    #[tokio::test]
    async fn uncommitted_work_is_reported_not_included() {
        const FN: &str = "uncommitted_work_is_reported_not_included";
        let dir = repo(FN).await;
        std::fs::write(dir.join("c.txt"), "not committed").unwrap();
        let c = capture(dir.to_str().unwrap(), "main", "feat/x")
            .await
            .unwrap();
        assert!(!c.diff.contains("c.txt"));
        assert!(c.uncommitted.iter().any(|u| u.contains("c.txt")));
    }

    #[tokio::test]
    async fn a_fresh_capture_is_not_stale() {
        const FN: &str = "a_fresh_capture_is_not_stale";
        let dir = repo(FN).await;
        let c = capture(dir.to_str().unwrap(), "main", "feat/x")
            .await
            .unwrap();
        assert!(staleness(dir.to_str().unwrap(), &c.head_sha, &c.files)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_file_changed_after_submit_is_named() {
        const FN: &str = "a_file_changed_after_submit_is_named";
        let dir = repo(FN).await;
        let c = capture(dir.to_str().unwrap(), "main", "feat/x")
            .await
            .unwrap();
        sh(
            &dir,
            "echo three >> a.txt && git add -A && git commit -qm later",
        )
        .await;
        let moved = staleness(dir.to_str().unwrap(), &c.head_sha, &c.files).await;
        assert_eq!(moved, ["a.txt"], "the changed file should be named");
    }

    #[tokio::test]
    async fn a_new_commit_touching_nothing_reviewed_still_reads_as_stale() {
        const FN: &str = "a_new_commit_touching_nothing_reviewed_still_reads_as_stale";
        let dir = repo(FN).await;
        let c = capture(dir.to_str().unwrap(), "main", "feat/x")
            .await
            .unwrap();
        sh(
            &dir,
            "echo x > untouched.txt && git add -A && git commit -qm other",
        )
        .await;
        let moved = staleness(dir.to_str().unwrap(), &c.head_sha, &c.files).await;
        assert!(!moved.is_empty());
        assert!(moved[0].contains("branch moved"), "{moved:?}");
    }
}
