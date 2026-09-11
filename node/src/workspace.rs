//! Managed workspaces live in runtime-owned storage.  The node stages bytes
//! through bounded copies; a harness never receives a host path or a forge
//! credential.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use serde::Serialize;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

use crate::runner::Mount;

pub const MAX_IMPORT_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_IMPORT_FILES: usize = 20_000;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("workspace source {0} does not exist")]
    Missing(PathBuf),
    #[error("workspace source contains a symbolic link or unsupported file: {0}")]
    Unsafe(PathBuf),
    #[error("workspace import exceeds the {MAX_IMPORT_BYTES}-byte limit")]
    TooLarge,
    #[error("workspace import contains more than {MAX_IMPORT_FILES} files")]
    TooManyFiles,
    #[error("git {op} refused: {message}")]
    Git { op: &'static str, message: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub id: String,
    pub volume: String,
    pub snapshot: PathBuf,
}

impl Workspace {
    pub fn mount(&self, target: impl Into<String>, read_only: bool) -> Mount {
        Mount::volume(self.volume.clone(), target, read_only)
    }
}

pub fn volume_name(id: &str) -> String {
    format!("tracon-workspace-{}", safe_id(id))
}

pub fn scratch_volume_name(id: &str) -> String {
    format!("tracon-scratch-{}", safe_id(id))
}

pub fn snapshot_path(id: &str) -> PathBuf {
    crate::config::Config::state_dir()
        .join("workspaces")
        .join(safe_id(id))
}

pub fn staging_path(id: &str) -> PathBuf {
    crate::config::Config::state_dir()
        .join("workspace-staging")
        .join(safe_id(id))
}

fn safe_id(id: &str) -> String {
    id.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                b as char
            } else {
                '-'
            }
        })
        .collect()
}

/// One lock per runtime volume, serializing import/export against it. Two
/// operations racing the same volume (a session's own start-up export racing
/// a client's `/download`, or two concurrent approvals snapshotting the same
/// workspace) would otherwise interleave a remove-and-rename with a reader,
/// handing back a torn directory or content that was never the tree
/// `validate_tree` just approved.
fn volume_lock(volume: &str) -> Arc<AsyncMutex<()>> {
    static LOCKS: std::sync::LazyLock<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
        std::sync::LazyLock::new(|| StdMutex::new(HashMap::new()));
    LOCKS
        .lock()
        .unwrap()
        .entry(volume.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

/// Open a file without following a symlink at its leaf. Combined with
/// checking the *opened* descriptor's metadata (`std::fs::File::metadata`
/// stats the fd, not the path), this collapses the classic stat-then-open
/// TOCTOU into one resolution: a symlink swapped in after `read_dir` but
/// before this call cannot make an open elsewhere succeed silently.
#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Copy selected bytes without following symlinks.  `skip_git` is used only
/// when overlaying an operator checkout onto the separately cloned seed: host
/// Git configuration and executable metadata never cross that boundary.
pub fn copy_tree(source: &Path, destination: &Path, skip_git: bool) -> Result<(), WorkspaceError> {
    let metadata =
        std::fs::symlink_metadata(source).map_err(|_| WorkspaceError::Missing(source.into()))?;

    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkspaceError::Unsafe(source.into()));
    }
    let mut budget = CopyBudget::default();
    copy_dir(source, destination, skip_git, &mut budget)
}
/// Create or replace the runtime-owned bytes for a workspace from a validated
/// node staging directory. This is the only bridge from a candidate snapshot
/// or browser artifact into a harness-visible filesystem.
pub async fn import(
    backend: &dyn crate::boundary::Backend,
    workspace: &Workspace,
    source: &Path,
) -> Result<(), WorkspaceError> {
    let lock = volume_lock(&workspace.volume);
    let _guard = lock.lock().await;
    validate_tree(source)?;
    backend
        .import_volume(&workspace.volume, source)
        .await
        .map_err(|e| WorkspaceError::Git {
            op: "runtime import",
            message: e.to_string(),
        })
}

/// Export a validated immutable snapshot of a workspace. The returned path is
/// under node state, never the selected host source.
pub async fn export(
    backend: &dyn crate::boundary::Backend,
    workspace: &Workspace,
) -> Result<PathBuf, WorkspaceError> {
    let lock = volume_lock(&workspace.volume);
    let _guard = lock.lock().await;
    backend
        .export_volume(&workspace.volume, &workspace.snapshot)
        .await
        .map_err(|e| WorkspaceError::Git {
            op: "runtime export",
            message: e.to_string(),
        })?;
    if let Err(error) = validate_tree(&workspace.snapshot) {
        // A rejected snapshot never sits around at a predictable node-local
        // path for something else to stumble into later.
        let _ = std::fs::remove_dir_all(&workspace.snapshot);
        return Err(error);
    }
    Ok(workspace.snapshot.clone())
}

/// Build a named runtime workspace from an already identified candidate or
/// artifact directory. It does not initialize or otherwise reinterpret Git.
pub async fn from_snapshot(
    backend: &dyn crate::boundary::Backend,
    id: &str,
    source: &Path,
) -> Result<Workspace, WorkspaceError> {
    let workspace = Workspace {
        id: id.to_string(),
        volume: volume_name(id),
        snapshot: snapshot_path(id),
    };
    import(backend, &workspace, source).await?;
    Ok(workspace)
}

#[derive(Default)]
struct CopyBudget {
    bytes: u64,
    files: usize,
}

fn copy_dir(
    source: &Path,
    destination: &Path,
    skip_git: bool,
    budget: &mut CopyBudget,
) -> Result<(), WorkspaceError> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip_git && name.to_string_lossy().eq_ignore_ascii_case(".git") {
            continue;
        }
        let source_path = entry.path();
        let destination_path = destination.join(&name);
        let metadata = std::fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            return Err(WorkspaceError::Unsafe(source_path));
        }
        if metadata.is_dir() {
            copy_dir(&source_path, &destination_path, skip_git, budget)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(WorkspaceError::Unsafe(source_path));
        }
        // The lstat above and a copy-by-path would resolve `source_path`
        // twice; a symlink swapped in between would make the second
        // resolution copy an arbitrary file's bytes into the snapshot.
        // Opening without following a symlink and re-checking the *opened*
        // descriptor collapses that into one resolution.
        let mut src = open_no_follow(&source_path)
            .map_err(|_| WorkspaceError::Unsafe(source_path.clone()))?;
        let opened = src.metadata()?;
        if !opened.is_file() {
            return Err(WorkspaceError::Unsafe(source_path));
        }
        budget.files += 1;
        budget.bytes = budget.bytes.saturating_add(opened.len());
        if budget.files > MAX_IMPORT_FILES {
            return Err(WorkspaceError::TooManyFiles);
        }
        if budget.bytes > MAX_IMPORT_BYTES {
            return Err(WorkspaceError::TooLarge);
        }
        if let Some(parent) = destination_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut dst = std::fs::File::create(&destination_path)?;
        std::io::copy(&mut src, &mut dst)?;
    }
    Ok(())
}

/// Check a tree received from a runtime before node-side Git, export, or
/// publication reads it.  It is deliberately the same strict shape as import.
pub fn validate_tree(root: &Path) -> Result<(), WorkspaceError> {
    let parent = root
        .parent()
        .ok_or_else(|| WorkspaceError::Unsafe(root.into()))?;
    let inspection = parent.join(format!(".tracon-inspect-{}", uuid::Uuid::now_v7()));
    let result = copy_tree(root, &inspection, false);

    let _ = std::fs::remove_dir_all(inspection);
    result
}
/// Attach signed transfer context as inert JSON. Tracon never executes or uses
/// this file as configuration; it is solely visible provenance for the next
/// session. The staging root must already be a checked directory.
pub fn write_context(selected: &Path, context: &serde_json::Value) -> Result<(), WorkspaceError> {
    let metadata = std::fs::symlink_metadata(selected)
        .map_err(|_| WorkspaceError::Missing(selected.into()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(WorkspaceError::Unsafe(selected.into()));
    }
    let context_dir = selected.join(".tracon");
    if context_dir.exists()
        && std::fs::symlink_metadata(&context_dir)?
            .file_type()
            .is_symlink()
    {
        return Err(WorkspaceError::Unsafe(context_dir));
    }
    std::fs::create_dir_all(&context_dir)?;
    let path = context_dir.join("transfer-context.json");
    let encoded = serde_json::to_vec_pretty(context).map_err(|e| WorkspaceError::Git {
        op: "transfer context",
        message: e.to_string(),
    })?;
    std::fs::write(path, encoded)?;
    Ok(())
}

/// Make an independent checkout that carries committed and selected
/// uncommitted files but none of the operator's Git configuration.  `source`
/// is read only; `git clone --no-local` keeps Git from hard-linking or writing
/// back into it.
pub async fn seed_from_checkout(
    git: &str,
    source: &Path,
    branch: &str,
    base_sha: Option<&str>,
    id: &str,
) -> Result<Workspace, WorkspaceError> {
    if !source.is_dir() {
        return Err(WorkspaceError::Missing(source.into()));
    }
    // `branch`/`base_sha` are client-supplied (`NewSession::branch`/`base_sha`)
    // and reach `git` as bare positional arguments below. A value starting
    // with `-` would be parsed as an option instead of a revision — with
    // `clone --no-local` forcing a remote-style transport, an option like
    // `--upload-pack=...` on a revision is a known command-injection vector.
    if branch.starts_with('-') || base_sha.is_some_and(|s| s.starts_with('-')) {
        return Err(WorkspaceError::Git {
            op: "checkout",
            message: "branch or base commit looks like a command-line option".into(),
        });
    }
    let stage = staging_path(id);
    let _ = std::fs::remove_dir_all(&stage);
    if let Some(parent) = stage.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let source_arg = source.to_string_lossy().into_owned();
    let stage_arg = stage.to_string_lossy().into_owned();
    run_git(
        git,
        None,
        "clone",
        [
            "clone",
            "--no-local",
            "--no-hardlinks",
            "--no-checkout",
            "--",
            source_arg.as_str(),
            stage_arg.as_str(),
        ],
    )
    .await?;
    let revision = base_sha.unwrap_or("HEAD");
    run_git(
        git,
        Some(&stage),
        "checkout",
        ["checkout", "--detach", revision],
    )
    .await?;
    run_git(
        git,
        Some(&stage),
        "branch",
        ["checkout", "-B", branch, revision],
    )
    .await?;
    // Overlay files after checkout so selected, uncommitted work arrives as
    // bytes only. .git stays the fresh, node-created checkout.
    copy_tree(source, &stage, true)?;
    sanitize_git(&stage)?;
    Ok(Workspace {
        id: id.to_string(),
        volume: volume_name(id),
        snapshot: snapshot_path(id),
    })
}

/// Seed an empty, commit-capable managed workspace from explicit uploaded
/// files.  The caller has already written the upload into `selected` using
/// checked relative paths.
pub async fn seed_from_files(
    git: &str,
    selected: &Path,
    id: &str,
) -> Result<Workspace, WorkspaceError> {
    let stage = staging_path(id);
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    copy_tree(selected, &stage, true)?;
    run_git(git, Some(&stage), "init", ["init", "-b", "main"]).await?;
    sanitize_git(&stage)?;
    Ok(Workspace {
        id: id.to_string(),
        volume: volume_name(id),
        snapshot: snapshot_path(id),
    })
}

/// Ensure an exported workspace cannot supply a credential helper, hook,
/// alternates file, replacement ref, graft, or `config.worktree` to a
/// node-side Git command.  Objects and ordinary refs remain so the commit and
/// blob identity can be verified without trusting metadata behavior.
pub fn sanitize_git(root: &Path) -> Result<(), WorkspaceError> {
    let git = root.join(".git");
    let metadata =
        std::fs::symlink_metadata(&git).map_err(|_| WorkspaceError::Unsafe(git.clone()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkspaceError::Unsafe(git));
    }
    for path in [
        git.join("config"),
        git.join("config.worktree"),
        git.join("hooks"),
        git.join("info/grafts"),
        git.join("objects/info/alternates"),
        git.join("refs/replace"),
    ] {
        if path.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    std::fs::create_dir_all(git.join("info"))?;
    std::fs::write(
        git.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\thooksPath = /dev/null\n\tfsmonitor =\n[user]\n\tname = tracon\n\temail = tracon@localhost\n",
    )?;
    Ok(())
}

/// Checked relative paths from browser folder uploads.  No path may escape,
/// become absolute, or name an empty component.
pub fn selected_path(root: &Path, relative: &str) -> Result<PathBuf, WorkspaceError> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
        || path.components().any(
            |component| matches!(component, Component::Normal(name) if name.to_string_lossy().eq_ignore_ascii_case(".git")),
        )
    {
        return Err(WorkspaceError::Unsafe(path.into()));
    }
    Ok(root.join(path))
}

async fn run_git<'a>(
    git: &str,
    dir: Option<&Path>,
    op: &'static str,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<(), WorkspaceError> {
    let mut command = Command::new(git);
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", crate::config::Config::state_dir().join("git-home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=",
            "-c",
            "core.useReplaceRefs=false",
        ])
        .args(args);
    let out = command.output().await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(WorkspaceError::Git {
            op,
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}
