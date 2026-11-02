//! The node keeping its own boundary in step with the binary it runs.
//!
//! A release, or a newly pinned harness, changes what the harness and gateway
//! images are built from; a changed `node.toml` changes the allowlist and the
//! gateway's arguments. Rather than refusing until someone runs `tracon
//! setup`, the node converges them itself (`Backend::converge`) when it starts
//! and whenever the definitions change under it, then checks the boundary
//! again. A converge that leaves nothing to fix shows nothing; one that fails
//! shows as the check that still fails, with why.
//!
//! What it must never overlap:
//!
//! - **Itself, or `tracon setup`.** Both hold an exclusive `flock` on
//!   `<state>/boundary.lock`, so a setup run from a shell in another process
//!   waits for the node's, and the other way round.
//! - **A container starting from a harness image.** A converge holds
//!   [`IMAGES`] exclusively; everything that starts a container from one of
//!   the boundary's images (a session, a check, a preparation, a volume
//!   transfer) holds it shared while podman is started (for a transfer, for
//!   the whole copy), not while a session runs. A repository image's build is
//!   not held: it pins the harness image by digest and is rebuilt when that
//!   digest moves. A start that arrives during a converge waits for it and then runs
//!   in the image it produced, rather than racing a rebuild of its tag or
//!   starting in an image the next check will call stale. A container that
//!   is already running keeps its image whatever is rebuilt: a build only
//!   moves the tag, and the runtime keeps an image a container uses.

use std::future::Future;
use std::sync::Mutex;

use tokio::sync::{RwLock, RwLockReadGuard};

use super::{Backend, BoundaryError};
use crate::config::Config;

/// Shared by every container start from a boundary image, exclusive to a
/// converge. Tokio's lock is fair: once a converge is waiting, new starts
/// queue behind it rather than holding it off indefinitely.
static IMAGES: RwLock<()> = RwLock::const_new(());

static STATUS: Mutex<Status> = Mutex::new(Status::Idle);

/// Where the node's own converge stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// None running, and the last one (if any) succeeded.
    Idle,
    /// One is running, or is about to: set before the startup check so that
    /// check already says the node is on it.
    Running,
    /// The last one failed, and why.
    Failed(String),
}

pub fn status() -> Status {
    STATUS.lock().map(|s| s.clone()).unwrap_or(Status::Idle)
}

pub fn running() -> bool {
    status() == Status::Running
}

fn set(status: Status) {
    if let Ok(mut s) = STATUS.lock() {
        *s = status;
    }
}

/// The startup check runs before the converge does; this lets it say that
/// what it finds wrong is about to be put right.
pub fn mark_pending() {
    set(Status::Running);
}

/// Held for as long as a container is being started from a boundary image.
pub async fn images_in_use() -> RwLockReadGuard<'static, ()> {
    IMAGES.read().await
}

/// Run `work` with no container starting from a boundary image and no other
/// setup running, in this process or another.
pub async fn exclusive<T>(work: impl Future<Output = T>) -> Result<T, BoundaryError> {
    let _images = IMAGES.write().await;
    let _file = lock_file().await?;
    Ok(work.await)
}

async fn lock_file() -> Result<std::fs::File, BoundaryError> {
    let path = Config::state_dir().join("boundary.lock");
    tokio::task::spawn_blocking(move || {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                tracing::info!(lock = %path.display(), "waiting for another setup of this boundary to finish");
                file.lock()?;
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        Ok::<_, std::io::Error>(file)
    })
    .await
    .map_err(|e| BoundaryError::Other(format!("boundary lock: {e}")))?
    .map_err(BoundaryError::Io)
}

/// The node's own converge: exclusive, and recorded in [`status`]. `Ok(true)`
/// when it changed something, so the boundary is worth checking again.
pub async fn converge(backend: &dyn Backend, cfg: &Config) -> Result<bool, BoundaryError> {
    set(Status::Running);
    let result = exclusive(backend.converge(cfg)).await.and_then(|r| r);
    set(match &result {
        Ok(_) => Status::Idle,
        Err(error) => Status::Failed(error.to_string()),
    });
    result
}

/// What a failed check adds while the node's own converge is running, or
/// after it failed: the check is not the whole story.
pub fn note() -> Option<String> {
    match status() {
        Status::Idle => None,
        Status::Running => {
            Some("the node is setting up its boundary and checks again when it finishes".into())
        }
        Status::Failed(error) => Some(format!(
            "the node could not set up its boundary itself ({error}); `tracon setup` tries again"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, because the status is process-wide.
    #[tokio::test]
    async fn a_converge_waits_for_starts_and_starts_wait_for_a_converge() {
        let start = images_in_use().await;
        let converging = tokio::spawn(exclusive(async { "converged" }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!converging.is_finished(), "a converge ran under a start");
        // A start that arrives while the converge waits queues behind it.
        let late = tokio::spawn(async {
            let _held = images_in_use().await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!late.is_finished(), "a start overtook a waiting converge");
        drop(start);
        assert_eq!(converging.await.unwrap().unwrap(), "converged");
        late.await.unwrap();

        mark_pending();
        assert!(running());
        assert!(note().unwrap().contains("checks again"));
        set(Status::Failed("podman build exited 1".into()));
        assert!(note().unwrap().contains("podman build exited 1"));
        set(Status::Idle);
        assert_eq!(note(), None);
    }
}
