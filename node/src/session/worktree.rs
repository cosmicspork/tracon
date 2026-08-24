//! Worktree creation. Sessions never run in a repo's main checkout, because
//! several of them run against one repo at a time.

use std::path::{Path, PathBuf};

use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("repo {0} does not exist")]
    NoRepo(PathBuf),
    #[error("{0} is not a git repository")]
    NotGit(PathBuf),
    #[error("git {op} failed: {stderr}")]
    Git { op: &'static str, stderr: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    /// True when the repo's main checkout has uncommitted changes. The session
    /// leaves it alone and reports it rather than touching parked work.
    pub main_checkout_dirty: bool,
}

async fn git(repo: &Path, op: &'static str, args: &[&str]) -> Result<String, WorktreeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(WorktreeError::Git {
            op,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// `origin/HEAD` names the default branch; fall back to `main` when the remote
/// head is not set locally.
pub async fn default_branch(repo: &Path) -> Result<String, WorktreeError> {
    match git(
        repo,
        "symbolic-ref",
        &["symbolic-ref", "refs/remotes/origin/HEAD"],
    )
    .await
    {
        Ok(s) => Ok(s.rsplit('/').next().unwrap_or("main").to_string()),
        Err(_) => Ok("main".to_string()),
    }
}

pub async fn is_dirty(repo: &Path) -> bool {
    git(repo, "status", &["status", "--porcelain"])
        .await
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// A worktree path that does not collide with an existing one.
fn worktree_path(root: &Path, repo: &Path, slug: &str) -> PathBuf {
    let name = repo
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    let mut path = root.join(format!("{name}-{slug}"));
    let mut n = 2;
    while path.exists() {
        path = root.join(format!("{name}-{slug}-{n}"));
        n += 1;
    }
    path
}

/// Fetch, then create a worktree on `branch` based on `origin/<default>`.
pub async fn create(
    repo: &Path,
    root: &Path,
    branch: &str,
    slug: &str,
) -> Result<Worktree, WorktreeError> {
    if !repo.exists() {
        return Err(WorktreeError::NoRepo(repo.to_path_buf()));
    }
    if !repo.join(".git").exists() {
        return Err(WorktreeError::NotGit(repo.to_path_buf()));
    }
    git(repo, "fetch", &["fetch", "origin"]).await?;
    let default = default_branch(repo).await?;
    let base = format!("origin/{default}");
    let dirty = is_dirty(repo).await;

    std::fs::create_dir_all(root)?;
    let path = worktree_path(root, repo, slug);
    let path_str = path.to_string_lossy().to_string();
    git(
        repo,
        "worktree add",
        &["worktree", "add", &path_str, "-b", branch, &base],
    )
    .await?;

    Ok(Worktree {
        path,
        branch: branch.to_string(),
        base,
        main_checkout_dirty: dirty,
    })
}

/// Remove a worktree. Only called when a session never produced commits; a
/// worktree with work in it outlives the session that made it.
pub async fn remove(repo: &Path, path: &Path) -> Result<(), WorktreeError> {
    let p = path.to_string_lossy().to_string();
    git(
        repo,
        "worktree remove",
        &["worktree", "remove", "--force", &p],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sh(dir: &Path, script: &str) {
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

    /// A repo with an `origin` remote, so the worktree has a real base to
    /// branch from.
    async fn fixture() -> (tempdir::TempDir, PathBuf) {
        let tmp = tempdir::TempDir::new();
        let origin = tmp.path().join("origin.git");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&origin).unwrap();
        sh(tmp.path(), "git init --bare -b main origin.git").await;
        sh(
            tmp.path(),
            "git clone -q origin.git repo && cd repo && \
             git config user.email t@e && git config user.name t && \
             echo hello > README.md && git add -A && git commit -qm init && git push -q origin main",
        )
        .await;
        (tmp, repo)
    }

    #[tokio::test]
    async fn creates_a_branch_from_origin_default() {
        let (tmp, repo) = fixture().await;
        let root = tmp.path().join("work");
        let wt = create(&repo, &root, "feat/thing", "thing").await.unwrap();
        assert!(wt.path.join("README.md").exists());
        assert_eq!(wt.branch, "feat/thing");
        assert_eq!(wt.base, "origin/main");
        assert!(!wt.main_checkout_dirty);
        // The main checkout is left on its own branch, untouched.
        let head = git(&repo, "b", &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();
        assert_eq!(head, "main");
    }

    #[tokio::test]
    async fn two_sessions_on_one_repo_get_distinct_worktrees() {
        let (tmp, repo) = fixture().await;
        let root = tmp.path().join("work");
        let a = create(&repo, &root, "feat/a", "same").await.unwrap();
        let b = create(&repo, &root, "feat/b", "same").await.unwrap();
        assert_ne!(a.path, b.path);
        assert!(b.path.to_string_lossy().ends_with("-same-2"));
    }

    #[tokio::test]
    async fn a_dirty_main_checkout_is_reported_not_touched() {
        let (tmp, repo) = fixture().await;
        std::fs::write(repo.join("scratch.txt"), "parked work").unwrap();
        let root = tmp.path().join("work");
        let wt = create(&repo, &root, "feat/thing", "thing").await.unwrap();
        assert!(wt.main_checkout_dirty);
        assert!(repo.join("scratch.txt").exists());
    }

    #[tokio::test]
    async fn a_missing_repo_is_refused() {
        let tmp = tempdir::TempDir::new();
        let err = create(
            &tmp.path().join("nope"),
            &tmp.path().join("work"),
            "feat/x",
            "x",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WorktreeError::NoRepo(_)));
    }

    /// Minimal scoped temp dir; avoids a dependency for four tests.
    pub mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct TempDir(PathBuf);

        impl TempDir {
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                let base = std::env::temp_dir().join(format!(
                    "tracon-test-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&base);
                std::fs::create_dir_all(&base).unwrap();
                Self(base)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
