//! Code-review capture and standalone narrative report invariants.
//!
//! A code-review agent submits an intent; the node captures the diff itself
//! from the worktree it created, then publishes approved bytes. A narrative
//! report is a separate queue item with no Git, candidate, or publication
//! path. The agent never holds a forge token and never runs the publishing CLI,
//! so "review before publish" is a property of the code flow rather than an
//! instruction the agent may forget by hour two.

pub mod checks;
pub mod prose;
pub mod publish;
pub mod report;

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

/// The object a revision names in `dir`, by hash. For the node's own reads of
/// a repository it does not own: nothing in that repository's configuration
/// runs.
pub(crate) async fn resolve(dir: &str, revision: &str) -> Result<String, ReviewError> {
    git(
        dir,
        "rev-parse",
        &["rev-parse", "--verify", "--quiet", revision],
    )
    .await
}

/// A blob's bytes, addressed by hash or by `<commit>:<path>`.
pub(crate) async fn blob(dir: &str, object: &str) -> Result<Vec<u8>, ReviewError> {
    git_bytes(dir, "cat-file", &["cat-file", "blob", object]).await
}

/// Who wrote and who committed one commit of a review, as Git recorded it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CommitAuthor {
    pub sha: String,
    pub author: String,
    pub author_email: String,
    pub committer_email: String,
}

/// Every commit `head` adds over `base`, with its author and committer. Read
/// with the same hardened Git as the capture, so a `.mailmap` or a format
/// configured in the repository cannot change what is reported.
pub async fn authors(
    worktree: &str,
    base: &str,
    head: &str,
) -> Result<Vec<CommitAuthor>, ReviewError> {
    let range = format!("{base}..{head}");
    let out = git(
        worktree,
        "log",
        &[
            "log",
            "--no-show-signature",
            "--no-mailmap",
            "--format=%H%x1f%an%x1f%ae%x1f%ce",
            &range,
        ],
    )
    .await?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut cols = line.split('\x1f');
            Some(CommitAuthor {
                sha: cols.next()?.to_string(),
                author: cols.next()?.to_string(),
                author_email: cols.next()?.to_string(),
                committer_email: cols.next()?.to_string(),
            })
        })
        .collect())
}

/// The commits that would not read as `identity`'s on its forge: authored or
/// committed under another address. Rewriting them is the operator's call,
/// never something publication does on its own.
pub fn misattributed(
    commits: &[CommitAuthor],
    identity: &crate::forge::Identity,
) -> Vec<CommitAuthor> {
    commits
        .iter()
        .filter(|c| !identity.owns(&c.author_email) || !identity.owns(&c.committer_email))
        .cloned()
        .collect()
}

/// What a review says about who its commits are by: the identity of the
/// target forge account, and each commit that is not authored and committed
/// as it. `None` when that identity cannot be resolved (no credential bound,
/// or the forge did not answer), which is said rather than guessed at.
pub async fn authorship(
    broker: &crate::broker::SharedBroker,
    provider: &str,
    channel: &str,
    node_id: &str,
    worktree: &str,
    base: &str,
    head: &str,
) -> Option<serde_json::Value> {
    let forge = crate::forge::Forge::parse(&provider.to_ascii_lowercase())?;
    let identity = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::forge::identity_on(broker, forge, channel, node_id),
    )
    .await
    .ok()?
    .ok()?;
    let commits = authors(worktree, base, head).await.ok()?;
    let wrong = misattributed(&commits, &identity);
    let note = if wrong.is_empty() {
        format!(
            "Every commit is authored and committed as {}.",
            identity.email
        )
    } else {
        format!(
            "{} of {} commits are not authored and committed as {} <{}>, so the forge will not \
             attribute them to {}. Set the identity in this repository (`git config user.name` \
             and `git config user.email`), then re-author them, for example with \
             `git rebase --exec 'git commit --amend --no-edit --reset-author' {base}`. Nothing \
             is rewritten for you.",
            wrong.len(),
            commits.len(),
            identity.name,
            identity.email,
            identity.login,
        )
    };
    Some(serde_json::json!({
        "identity": identity,
        "misattributed": wrong,
        "note": note,
    }))
}

/// Whether `head` descends from `ancestor`, a commit this worktree holds. A
/// commit it does not hold is not an ancestor it can vouch for.
pub async fn descends_from(worktree: &str, head: &str, ancestor: &str) -> bool {
    hardened_git(worktree)
        .args(["merge-base", "--is-ancestor", ancestor, head])
        .output()
        .await
        .is_ok_and(|out| out.status.success())
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

/// Run `git cat-file` in batch mode over `objects`, one per line, and return
/// its whole output. The input is written from its own task so a large output
/// cannot fill the pipe while git is still waiting to read.
async fn cat_file(
    dir: &str,
    op: &'static str,
    mode: &str,
    objects: &[String],
) -> Result<Vec<u8>, ReviewError> {
    use tokio::io::AsyncWriteExt;
    if objects.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = hardened_git(dir)
        .args(["cat-file", mode])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = objects.join("\n") + "\n";
    let writer = tokio::spawn(async move {
        stdin.write_all(input.as_bytes()).await?;
        stdin.shutdown().await
    });
    let out = child.wait_with_output().await?;
    writer
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))??;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(ReviewError::Git {
            op,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// The commits `HEAD` has beyond `base_ref`, oldest first.
pub async fn commits(
    worktree: &str,
    base_ref: &str,
) -> Result<Vec<prose::CommitLine>, ReviewError> {
    let listed = git(
        worktree,
        "log",
        &[
            "log",
            "--reverse",
            "--no-decorate",
            "--format=%H%x1f%s",
            &format!("{base_ref}..HEAD"),
        ],
    )
    .await?;
    Ok(listed
        .lines()
        .filter_map(|line| {
            let (sha, subject) = line.split_once('\u{1f}')?;
            Some(prose::CommitLine {
                sha: sha.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect())
}

/// Where `head` leaves `base_ref`: the commit a squash of it sits on.
pub async fn merge_base(worktree: &str, base_ref: &str, head: &str) -> Result<String, ReviewError> {
    git(worktree, "merge-base", &["merge-base", base_ref, head]).await
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

    let names: Vec<String> = paths.iter().map(|path| format!("HEAD:{path}")).collect();
    let checked = cat_file(worktree, "cat-file --batch-check", "--batch-check", &names).await?;
    let mut files = Vec::new();
    for (path, line) in paths.iter().zip(String::from_utf8_lossy(&checked).lines()) {
        // A deleted file has no blob at HEAD; record it as absent rather than
        // failing the capture.
        let blob = match line.split(' ').collect::<Vec<_>>()[..] {
            [oid, "blob", _] => oid.to_string(),
            _ => "absent".into(),
        };
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

impl CandidateSnapshot {
    /// The same materialized source, rebuilt from the files the evidence store
    /// retained at capture time rather than from a Git repository. This is how
    /// anything later — a build, a demonstration — runs on the candidate's own
    /// immutable tree instead of the owner session's still-mutable workspace.
    pub fn materialize(
        root: PathBuf,
        tree_sha: String,
        files: Vec<crate::store::CandidateFile>,
    ) -> Result<Self, ReviewError> {
        // Constructed before the files are written: a partial tree is removed
        // by `Drop` rather than left behind for the next caller to find.
        let snapshot = Self {
            root,
            tree_sha,
            files,
        };
        materialize_candidate_files(&snapshot.root, &snapshot.files)?;
        Ok(snapshot)
    }
}

impl Drop for CandidateSnapshot {
    fn drop(&mut self) {
        make_tree_writable(&self.root);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A candidate's tree as read out of Git, before anything is written to disk.
pub struct CandidateTree {
    pub tree_sha: String,
    pub files: Vec<crate::store::CandidateFile>,
}

/// Copy exactly the Git tree named by `head_sha`, not the mutable worktree.
/// Tree traversal and blob reads address hashes after disabling replacement,
/// graft, attribute, hook, and executable configuration paths. `head_sha` is
/// any tree-ish: a commit for a candidate, `<commit>:<directory>` for one
/// directory of it.
pub async fn snapshot_candidate(
    worktree: &str,
    head_sha: &str,
    max_bytes: u64,
) -> Result<CandidateSnapshot, ReviewError> {
    let tree = read_candidate(worktree, head_sha, max_bytes).await?;
    let root = std::env::temp_dir()
        .join("tracon-candidates")
        .join(uuid::Uuid::now_v7().to_string());
    tokio::task::spawn_blocking(move || {
        CandidateSnapshot::materialize(root, tree.tree_sha, tree.files)
    })
    .await
    .map_err(|error| std::io::Error::other(error.to_string()))?
}

/// The files `snapshot_candidate` copies, read the same way but held in
/// memory rather than written to disk.
pub async fn read_candidate(
    worktree: &str,
    head_sha: &str,
    max_bytes: u64,
) -> Result<CandidateTree, ReviewError> {
    let tree = format!("{head_sha}^{{tree}}");
    let tree_sha = git(worktree, "rev-parse tree", &["rev-parse", &tree]).await?;
    let listing = git_bytes(
        worktree,
        "ls-tree",
        &["ls-tree", "-rz", "-r", "--full-tree", head_sha],
    )
    .await?;
    let mut snapshot = CandidateTree {
        tree_sha,
        files: Vec::new(),
    };
    let mut entries = Vec::new();
    for entry in listing.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        let (meta, path) = split_once_byte(entry, b'\t')
            .ok_or_else(|| ReviewError::Rejected("Git tree entry has no path separator".into()))?;
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
        entries.push((mode, object.to_string(), path.to_string()));
    }
    // Two git processes for the whole tree, not two per file: spawning one
    // per blob made a submission outlast the client on a throttled host.
    let objects: Vec<String> = entries
        .iter()
        .map(|(_, object, _)| object.clone())
        .collect();
    let checked = cat_file(
        worktree,
        "cat-file --batch-check",
        "--batch-check",
        &objects,
    )
    .await?;
    let mut sizes = Vec::with_capacity(entries.len());
    let mut total = 0u64;
    for ((_, object, _), line) in entries
        .iter()
        .zip(String::from_utf8_lossy(&checked).lines())
    {
        let size = match line.split(' ').collect::<Vec<_>>()[..] {
            [oid, "blob", size] if oid == object => size.parse::<u64>().ok(),
            _ => None,
        }
        .ok_or_else(|| {
            ReviewError::Rejected(format!("candidate blob {object} has invalid size"))
        })?;
        total = total
            .checked_add(size)
            .ok_or_else(|| ReviewError::Rejected("candidate snapshot is too large".into()))?;
        if total > max_bytes {
            return Err(ReviewError::Rejected(format!(
                "candidate snapshot is {total} bytes; the operator limit is {max_bytes}"
            )));
        }
        sizes.push(size);
    }
    if sizes.len() != entries.len() {
        return Err(ReviewError::Rejected(
            "git did not describe every candidate blob".into(),
        ));
    }
    let batch = cat_file(worktree, "cat-file --batch", "--batch", &objects).await?;
    let mut rest = &batch[..];
    for ((mode, object, path), size) in entries.into_iter().zip(sizes) {
        let changed = || {
            ReviewError::Rejected(format!(
                "candidate blob {object} changed while it was captured"
            ))
        };
        let (header, body) = split_once_byte(rest, b'\n').ok_or_else(changed)?;
        let len = usize::try_from(size).map_err(|_| changed())?;
        if header != format!("{object} blob {size}").as_bytes() || body.get(len) != Some(&b'\n') {
            return Err(changed());
        }
        snapshot.files.push(crate::store::CandidateFile {
            path,
            mode,
            content: body[..len].to_vec(),
        });
        rest = &body[len + 1..];
    }
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
                    std::fs::set_permissions(
                        &target,
                        std::fs::Permissions::from_mode(permissions),
                    )?;
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
            if std::fs::symlink_metadata(&child)
                .map(|metadata| metadata.file_type().is_dir())
                .unwrap_or(false)
            {
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
    let mut objects: Vec<String> = requested
        .iter()
        .map(|(path, _, _)| format!("{head_sha}:{path}"))
        .collect();
    objects.sort();
    objects.dedup();
    let blobs = match cat_file(worktree, "cat-file --batch", "--batch", &objects).await {
        Ok(batch) => parse_batch(&objects, &batch),
        Err(_) => return Vec::new(),
    };
    let mut contexts = Vec::new();
    for (path, start, count) in requested {
        let Some(Some(bytes)) = blobs.get(&format!("{head_sha}:{path}")) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
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

/// Split `git cat-file --batch` output back into each requested object, by
/// the size in its header rather than by line. An object that is missing or
/// is not a blob maps to `None`.
fn parse_batch(
    objects: &[String],
    batch: &[u8],
) -> std::collections::HashMap<String, Option<Vec<u8>>> {
    let mut found = std::collections::HashMap::new();
    let mut rest = batch;
    for object in objects {
        let Some((header, body)) = split_once_byte(rest, b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(header);
        if header.ends_with(" missing") || header.ends_with(" ambiguous") {
            found.insert(object.clone(), None);
            rest = body;
            continue;
        }
        let mut fields = header.rsplitn(3, ' ');
        let (Some(size), Some(kind)) = (
            fields.next().and_then(|size| size.parse::<usize>().ok()),
            fields.next(),
        ) else {
            break;
        };
        if body.get(size) != Some(&b'\n') {
            break;
        }
        found.insert(
            object.clone(),
            (kind == "blob").then(|| body[..size].to_vec()),
        );
        rest = &body[size + 1..];
    }
    found
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

    /// A commit made under another address is named; one made as the
    /// forge identity is not, and the base's own commits are not the
    /// review's to answer for.
    #[tokio::test]
    async fn a_commit_not_authored_as_the_forge_identity_is_named() {
        let dir = repo("authorship").await;
        sh(
            &dir,
            "git -c user.email=1+ada@users.noreply.github.com -c user.name=Ada \
             commit -q --allow-empty -m mine",
        )
        .await;
        let identity = crate::forge::Identity {
            name: "Ada".into(),
            email: "1+ada@users.noreply.github.com".into(),
            login: "ada".into(),
            forge: "github",
        };
        let dir = dir.to_string_lossy().into_owned();
        let commits = authors(&dir, "main", "HEAD").await.unwrap();
        assert_eq!(commits.len(), 2, "{commits:?}");
        let wrong = misattributed(&commits, &identity);
        assert_eq!(wrong.len(), 1, "{wrong:?}");
        assert_eq!(wrong[0].author_email, "t@e");
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

    /// How the context was read before it was batched: one `git show` per
    /// hunk.
    async fn context_one_hunk_at_a_time(
        worktree: &str,
        head_sha: &str,
        diff: &str,
    ) -> Vec<CodeContext> {
        let mut current = None::<String>;
        let mut requested = Vec::new();
        for line in diff.lines() {
            if line == "+++ /dev/null" {
                current = None;
            } else if let Some(path) = line.strip_prefix("+++ b/") {
                current = Some(path.to_string());
            } else if let (Some(path), Some((start, count))) = (&current, changed_hunk(line)) {
                requested.push((path.clone(), start, count));
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

    #[tokio::test]
    async fn batched_context_matches_one_read_per_hunk() {
        const FN: &str = "batched_context_matches_one_read_per_hunk";
        let dir = repo(FN).await;
        sh(
            &dir,
            "git checkout -q main && seq 1 60 > long.txt && printf 'x\\ny' > tail.txt \
             && echo gone > gone.txt && printf 'a\\nb\\n' > 'with space.txt' \
             && git add -A && git commit -qm more && git checkout -q feat/x && git rebase -q main \
             && sed -i.bak -e '5s/$/ edited/' -e '50s/$/ edited/' long.txt && rm long.txt.bak \
             && printf 'x\\ny\\nz' > tail.txt && git rm -q gone.txt \
             && printf 'a\\nb\\nc\\n' > 'with space.txt' && printf '\\377\\376\\n' > bin.dat \
             && git add -A && git commit -qm edits",
        )
        .await;
        let worktree = dir.to_str().unwrap();
        let head = git(worktree, "rev-parse", &["rev-parse", "HEAD"])
            .await
            .unwrap();
        let diff = git(
            worktree,
            "diff",
            &["diff", "--no-ext-diff", "--no-textconv", "main...HEAD"],
        )
        .await
        .unwrap();
        let batched = pinned_context(worktree, &head, &diff).await;
        let reference = context_one_hunk_at_a_time(worktree, &head, &diff).await;
        assert_eq!(
            serde_json::to_value(&batched).unwrap(),
            serde_json::to_value(&reference).unwrap()
        );
        let paths: Vec<&str> = batched.iter().map(|c| c.path.as_str()).collect();
        assert!(
            paths.iter().filter(|p| **p == "long.txt").count() >= 2,
            "{paths:?}"
        );
        assert!(paths.contains(&"tail.txt"), "{paths:?}");
        assert!(!paths.contains(&"bin.dat"), "{paths:?}");
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
    async fn a_deleted_file_is_captured_as_absent() {
        const FN: &str = "a_deleted_file_is_captured_as_absent";
        let dir = repo(FN).await;
        sh(&dir, "git rm -q a.txt && git commit -qm gone").await;
        let c = capture(dir.to_str().unwrap(), "main", "feat/x")
            .await
            .unwrap();
        let a = c.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!(a.blob, "absent");
        let b = c.files.iter().find(|f| f.path == "b.txt").unwrap();
        assert_ne!(b.blob, "absent");
    }

    #[tokio::test]
    async fn a_snapshot_copies_every_blob_exactly() {
        const FN: &str = "a_snapshot_copies_every_blob_exactly";
        let dir = repo(FN).await;
        // Content with embedded newlines and no trailing one, so a batch parse
        // that trusted line boundaries instead of sizes would misalign.
        sh(
            &dir,
            "printf 'x\\n\\ny' > nl.bin && printf '#!/bin/sh\\n' > run.sh && chmod +x run.sh \
             && mkdir -p d && : > d/empty && ln -s b.txt link && git add -A && git commit -qm more",
        )
        .await;
        let head = git(dir.to_str().unwrap(), "rev-parse", &["rev-parse", "HEAD"])
            .await
            .unwrap();
        let snapshot = snapshot_candidate(dir.to_str().unwrap(), &head, 1 << 20)
            .await
            .unwrap();
        let file = |path: &str| {
            snapshot
                .files
                .iter()
                .find(|f| f.path == path)
                .unwrap_or_else(|| panic!("{path} missing"))
        };
        assert_eq!(snapshot.files.len(), 6);
        assert_eq!(file("a.txt").content, b"one\ntwo\n");
        assert_eq!(file("nl.bin").content, b"x\n\ny");
        assert_eq!(file("d/empty").content, b"");
        assert_eq!(file("run.sh").mode, 0o100755);
        assert_eq!(file("link").mode, 0o120000);
        assert_eq!(file("link").content, b"b.txt");
        assert_eq!(
            std::fs::read(snapshot.root.join("nl.bin")).unwrap(),
            b"x\n\ny"
        );
    }

    #[tokio::test]
    async fn a_snapshot_over_the_byte_limit_is_refused() {
        const FN: &str = "a_snapshot_over_the_byte_limit_is_refused";
        let dir = repo(FN).await;
        let head = git(dir.to_str().unwrap(), "rev-parse", &["rev-parse", "HEAD"])
            .await
            .unwrap();
        let Err(ReviewError::Rejected(reason)) =
            snapshot_candidate(dir.to_str().unwrap(), &head, 4).await
        else {
            panic!("a snapshot over the limit was accepted");
        };
        assert!(reason.contains("the operator limit is 4"), "{reason}");
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
