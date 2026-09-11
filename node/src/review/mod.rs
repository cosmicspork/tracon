//! Review capture and publication.
//!
//! The agent submits an intent; the node captures the diff itself from the
//! worktree it created, and the node publishes the approved bytes. The agent
//! never holds a forge token and never runs the publishing CLI, so "review
//! before publish" is a property of the system rather than an instruction the
//! agent may forget by hour two.

pub mod checks;
pub mod publish;

use std::path::{Component, Path, PathBuf};

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
    #[error("{repo} is not under any of [external] repo_roots")]
    OutsideRoots { repo: String },
}

/// A worktree the operator made, found for a harness they run themselves.
#[derive(Debug, Clone)]
pub struct Located {
    pub worktree: String,
    pub branch: String,
    pub repo: String,
}

/// Accept a worktree only at its top level, on a branch, and for a repository
/// under one of `roots`. The repository is what is checked, not the worktree's
/// own path, so a linked worktree in a scratch directory is accepted for a
/// repository that lives under a root.
pub async fn locate_worktree(
    path: &str,
    roots: &[std::path::PathBuf],
) -> Result<Located, ReviewError> {
    let given = std::fs::canonicalize(path)
        .map_err(|_| ReviewError::Rejected(format!("{path} does not exist")))?;
    let dir = given.to_string_lossy().into_owned();
    let top = git(&dir, "rev-parse", &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|_| ReviewError::Rejected(format!("{path} is not a git worktree")))?;
    if std::fs::canonicalize(&top).ok().as_ref() != Some(&given) {
        return Err(ReviewError::Rejected(format!(
            "{path} is inside a worktree; pass its top level, {top}"
        )));
    }
    let common = git(
        &dir,
        "rev-parse",
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let common = std::fs::canonicalize(&common)?;
    // The common directory is the repository's `.git`, or the repository
    // itself when it is bare.
    let repo = match common.file_name() {
        Some(name) if name == ".git" => common.parent().unwrap_or(&common).to_path_buf(),
        _ => common.clone(),
    };
    let inside = roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|r| repo.starts_with(&r));
    if !inside {
        return Err(ReviewError::OutsideRoots {
            repo: repo.display().to_string(),
        });
    }
    let branch = git(&dir, "rev-parse", &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    if branch == "HEAD" {
        return Err(ReviewError::Rejected(
            "the worktree is on a detached HEAD; check out the branch to publish".into(),
        ));
    }
    Ok(Located {
        worktree: dir,
        branch,
        repo: repo.display().to_string(),
    })
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
    /// Source excerpts around diff hunks, pinned at `head_sha`.
    pub contexts: Vec<CodeContext>,
    /// Uncommitted work is not in the diff. The operator is told rather than
    /// left to wonder why the review looks short.
    pub uncommitted: Vec<String>,
}

/// A small immutable source excerpt around a changed hunk. It is stored with
/// the review revision, not read back from a mutable worktree while reviewing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeContext {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

/// Global git options that disable every config-driven way a command can run
/// another program: hooks and the fsmonitor daemon. The node runs git host-side
/// against a worktree whose `.git` the harness can partly write, so even though
/// `config`/`hooks`/`info` are mounted read-only (see `materialize`), these
/// overrides are the second, independent line. Diff commands additionally pass
/// `--no-ext-diff --no-textconv` at the call site.
const GIT_SAFE: &[&str] = &[
    "--no-replace-objects",
    "-c",
    "core.useReplaceRefs=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=",
    "-c",
    "credential.helper=",
    "-c",
    "core.attributesfile=/dev/null",
];

/// Git is pointed at an agent-owned repository, so no local, global, or
/// system Git configuration may select a program or replacement object. The
/// remaining commands address objects by hash and never invoke a shell.
fn hardened_git(dir: &str) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(GIT_SAFE)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_DIFF_PATH_COUNTER")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    command
}

async fn git(dir: &str, op: &'static str, args: &[&str]) -> Result<String, ReviewError> {
    let out = hardened_git(dir).args(args).output().await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(ReviewError::Git {
            op,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

async fn git_bytes(dir: &str, op: &'static str, args: &[&str]) -> Result<Vec<u8>, ReviewError> {
    let out = hardened_git(dir).args(args).output().await?;
    if out.status.success() {
        Ok(out.stdout)
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

    let contexts = pinned_context(worktree, &head_sha, &diff).await;
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
        contexts,
        uncommitted,
    })
}

/// Materialized, read-only source for an isolated check. Its backing directory
/// is node-owned and removed after the check attempt; `files` is retained in
/// the evidence store for candidate transfer and later independent execution.
pub struct CandidateSnapshot {
    pub root: PathBuf,
    pub tree_sha: String,
    pub files: Vec<crate::store::CandidateFile>,
}

impl Drop for CandidateSnapshot {
    fn drop(&mut self) {
        make_tree_writable(&self.root);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Copy exactly the Git tree named by `head_sha`, not the mutable worktree.
/// Tree traversal and blob reads address hashes after disabling replacement,
/// graft, attribute, hook, and executable configuration paths.
pub async fn snapshot_candidate(
    worktree: &str,
    head_sha: &str,
    max_bytes: u64,
) -> Result<CandidateSnapshot, ReviewError> {
    let tree = format!("{head_sha}^{{tree}}");
    let tree_sha = git(worktree, "rev-parse tree", &["rev-parse", &tree]).await?;
    let listing = git_bytes(
        worktree,
        "ls-tree",
        &["ls-tree", "-rz", "-r", "--full-tree", head_sha],
    )
    .await?;
    let root = std::env::temp_dir()
        .join("tracon-candidates")
        .join(uuid::Uuid::now_v7().to_string());
    std::fs::create_dir_all(&root)?;
    let mut snapshot = CandidateSnapshot {
        root,
        tree_sha,
        files: Vec::new(),
    };
    let mut total = 0u64;
    for entry in listing.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        let (meta, path) = split_once_byte(entry, b'\t').ok_or_else(|| {
            ReviewError::Rejected("Git tree entry has no path separator".into())
        })?;
        let meta = std::str::from_utf8(meta)
            .map_err(|_| ReviewError::Rejected("Git tree metadata is not UTF-8".into()))?;
        let mut fields = meta.split_whitespace();
        let mode = fields
            .next()
            .and_then(|mode| u32::from_str_radix(mode, 8).ok())
            .ok_or_else(|| ReviewError::Rejected("Git tree entry has an invalid mode".into()))?;
        let kind = fields.next().unwrap_or_default();
        let object = fields.next().unwrap_or_default();
        if kind != "blob" || fields.next().is_some() || object.is_empty() {
            return Err(ReviewError::Rejected(
                "candidate tree contains a non-file Git entry".into(),
            ));
        }
        let path = std::str::from_utf8(path)
            .map_err(|_| ReviewError::Rejected("candidate path is not UTF-8".into()))?;
        safe_candidate_path(path)?;
        if !matches!(mode, 0o100644 | 0o100755 | 0o120000) {
            return Err(ReviewError::Rejected(format!(
                "candidate path {path} has unsupported mode {mode:o}"
            )));
        }
        let size = git(worktree, "cat-file size", &["cat-file", "-s", object])
            .await?
            .parse::<u64>()
            .map_err(|_| ReviewError::Rejected(format!("candidate blob {object} has invalid size")))?;
        total = total
            .checked_add(size)
            .ok_or_else(|| ReviewError::Rejected("candidate snapshot is too large".into()))?;
        if total > max_bytes {
            return Err(ReviewError::Rejected(format!(
                "candidate snapshot is {total} bytes; the operator limit is {max_bytes}"
            )));
        }
        let content = git_bytes(worktree, "cat-file", &["cat-file", "blob", object]).await?;
        if u64::try_from(content.len()).ok() != Some(size) {
            return Err(ReviewError::Rejected(format!(
                "candidate blob {object} changed while it was captured"
            )));
        }
        snapshot.files.push(crate::store::CandidateFile {
            path: path.to_string(),
            mode,
            content,
        });
    }
    materialize_candidate_files(&snapshot.root, &snapshot.files)?;
    Ok(snapshot)
}

/// Rebuild an isolated candidate from the file payload retained in the
/// evidence store. This is for trusted future execution/transfer code; it has
/// no worktree or Git configuration input.
pub fn materialize_candidate_files(
    root: &Path,
    files: &[crate::store::CandidateFile],
) -> Result<(), ReviewError> {
    std::fs::create_dir_all(root)?;
    for file in files {
        safe_candidate_path(&file.path)?;
        let target = root.join(&file.path);
        let parent = target
            .parent()
            .ok_or_else(|| ReviewError::Rejected("candidate file has no parent".into()))?;
        std::fs::create_dir_all(parent)?;
        match file.mode {
            0o100644 | 0o100755 => {
                std::fs::write(&target, &file.content)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let permissions = if file.mode == 0o100755 { 0o555 } else { 0o444 };
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(permissions))?;
                }
            }
            0o120000 => {
                let destination = std::str::from_utf8(&file.content).map_err(|_| {
                    ReviewError::Rejected(format!("candidate symlink {} is not UTF-8", file.path))
                })?;
                safe_symlink_target(&file.path, destination)?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(destination, &target)?;
                #[cfg(not(unix))]
                return Err(ReviewError::Rejected(
                    "candidate symbolic links are unsupported on this platform".into(),
                ));
            }
            _ => {
                return Err(ReviewError::Rejected(format!(
                    "candidate path {} has unsupported mode {:o}",
                    file.path, file.mode
                )))
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut directories = vec![root.to_path_buf()];
        for file in files {
            let mut path = root.join(&file.path);
            while let Some(parent) = path.parent() {
                if parent.starts_with(root) {
                    directories.push(parent.to_path_buf());
                }
                if parent == root {
                    break;
                }
                path = parent.to_path_buf();
            }
        }
        directories.sort();
        directories.dedup();
        for directory in directories {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o555))?;
        }
    }
    Ok(())
}

/// `[u8]::split_once` is not yet stable; this is the byte-slice equivalent
/// of `str::split_once` for a single-byte separator.
fn split_once_byte(bytes: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    let at = bytes.iter().position(|byte| *byte == separator)?;
    Some((&bytes[..at], &bytes[at + 1..]))
}

fn make_tree_writable(path: &Path) {
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                make_tree_writable(&child);
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
}
fn safe_candidate_path(path: &str) -> Result<(), ReviewError> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ReviewError::Rejected(format!(
            "candidate path {path:?} escapes its snapshot"
        )));
    }
    Ok(())
}

fn safe_symlink_target(path: &str, target: &str) -> Result<(), ReviewError> {
    if target.is_empty() || Path::new(target).is_absolute() {
        return Err(ReviewError::Rejected(format!(
            "candidate symlink {path} escapes its snapshot"
        )));
    }
    let mut depth = Path::new(path)
        .parent()
        .map(|parent| parent.components().count())
        .unwrap_or_default();
    for component in Path::new(target).components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ReviewError::Rejected(format!(
                    "candidate symlink {path} escapes its snapshot"
                )))
            }
        }
    }
    Ok(())
}

async fn pinned_context(worktree: &str, head_sha: &str, diff: &str) -> Vec<CodeContext> {
    let mut current = None::<String>;
    let mut requested = Vec::<(String, usize, usize)>::new();
    for line in diff.lines() {
        if line == "+++ /dev/null" {
            current = None;
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ b/") {
            current = Some(path.to_string());
            continue;
        }
        let Some(path) = current.as_deref() else {
            continue;
        };
        if let Some((start, count)) = changed_hunk(line) {
            requested.push((path.to_string(), start, count));
        }
    }
    requested.truncate(80);
    let mut contexts = Vec::new();
    for (path, start, count) in requested {
        let object = format!("{head_sha}:{path}");
        let Ok(bytes) = git_bytes(worktree, "show context", &["show", &object]).await else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            continue;
        }
        let first = start.saturating_sub(3).max(1);
        let last = start
            .saturating_add(count.max(1))
            .saturating_add(2)
            .min(lines.len());
        if first > last {
            continue;
        }
        let mut excerpt = lines[first - 1..last].join("\n");
        if last == lines.len() && text.ends_with('\n') {
            excerpt.push('\n');
        }
        contexts.push(CodeContext {
            path,
            start_line: first,
            end_line: last,
            text: excerpt,
        });
    }
    contexts
}

fn changed_hunk(line: &str) -> Option<(usize, usize)> {
    let changed = line
        .split_whitespace()
        .find(|part| part.starts_with('+') && part.len() > 1)?;
    let changed = changed.strip_prefix('+')?;
    let (start, count) = changed.split_once(',').unwrap_or((changed, "1"));
    Some((start.parse().ok()?, count.parse().ok()?))
}

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
    let out = hardened_git(worktree)
        .args(["cat-file", "blob", &f.blob])
        .output()
        .await?;
    if !out.status.success() {
        return Err(ReviewError::Git {
            op: "cat-file",
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
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
