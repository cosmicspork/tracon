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
//!
//! A converge that fails is tried again ([`upkeep`]): first a minute later,
//! then twice as long after each failure, at most [`RETRY_CAP`] apart, and
//! not again once [`RETRY_LIMIT`] in a row have failed. A success, a change
//! in the definitions, or a setup the operator runs through the node
//! ([`operator_setup`]) starts the count again. Only a setup in the node's
//! own process pins the sandbox base the node runs ([`hold_unpinned`]), so
//! `tracon setup` asks a running node to run it rather than running it
//! beside it.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::{Notify, RwLock, RwLockReadGuard};
use tokio::time::Instant;

use super::{Backend, BoundaryError, BoundaryReport, CheckId};
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

/// The wait after the first failure.
pub const RETRY_FIRST: Duration = Duration::from_secs(60);
/// The longest wait between two attempts.
pub const RETRY_CAP: Duration = Duration::from_secs(30 * 60);
/// Failures in a row after which the node stops trying by itself.
pub const RETRY_LIMIT: u32 = 8;

/// How long to wait after `failures` setups in a row have failed before the
/// next attempt; `None` when there is none to make (nothing failed, or
/// [`RETRY_LIMIT`] have).
pub fn retry_delay(failures: u32) -> Option<Duration> {
    if failures == 0 || failures >= RETRY_LIMIT {
        return None;
    }
    let doubled = RETRY_FIRST.saturating_mul(1u32 << (failures - 1).min(16));
    Some(doubled.min(RETRY_CAP))
}

/// Where the node's retries of a failed setup stand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Retry {
    /// Setups in a row that failed.
    pub failures: u32,
    /// When the next attempt is due, if one is.
    pub next: Option<Instant>,
    /// The same, as Unix milliseconds, for what the interface shows.
    pub next_ms: Option<i64>,
}

impl Retry {
    /// Failed as often as it may: the node no longer tries by itself.
    pub fn stopped(&self) -> bool {
        self.failures >= RETRY_LIMIT
    }
}

static RETRY: Mutex<Retry> = Mutex::new(Retry {
    failures: 0,
    next: None,
    next_ms: None,
});

/// Woken whenever [`RETRY`] changes, so [`upkeep`] never sleeps past an
/// attempt an operator's failed setup just scheduled.
static RETRY_CHANGED: Notify = Notify::const_new();

/// Whether [`upkeep`] runs in this process, to make the attempts [`RETRY`]
/// schedules.
static UPKEEP: AtomicBool = AtomicBool::new(false);

pub fn retry() -> Retry {
    RETRY.lock().map(|r| *r).unwrap_or_default()
}

fn set_retry(retry: Retry) {
    if let Ok(mut r) = RETRY.lock() {
        *r = retry;
    }
    RETRY_CHANGED.notify_one();
}

/// Start the count again: a change in definitions, or an operator's setup.
pub fn reset_retries() {
    set_retry(Retry::default());
}

/// Record a setup's outcome: its status, and when (if ever) to try again.
fn record<T>(result: &Result<T, BoundaryError>) {
    match result {
        Ok(_) => {
            set(Status::Idle);
            set_retry(Retry::default());
        }
        Err(error) => {
            set(Status::Failed(error.to_string()));
            let failures = retry().failures.saturating_add(1);
            let delay = retry_delay(failures).filter(|_| UPKEEP.load(Ordering::Relaxed));
            set_retry(Retry {
                failures,
                next: delay.map(|d| Instant::now() + d),
                next_ms: delay.map(|d| crate::store::now_ms() + d.as_millis() as i64),
            });
        }
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
    record(&result);
    result
}

/// A setup the operator asked the node for (`POST /api/boundary/setup`,
/// which `tracon setup` asks a running node): exclusive like the node's own,
/// and recorded the same way, as the first of a fresh count. One cut short
/// (its caller gave up waiting) counts as failed.
pub async fn operator_setup(
    backend: &dyn Backend,
    cfg: &Config,
    rebuild: bool,
) -> Result<(), BoundaryError> {
    /// Records a setup dropped before it finished.
    struct CutShort(bool);
    impl Drop for CutShort {
        fn drop(&mut self) {
            if self.0 {
                record::<()>(&Err(BoundaryError::Other(
                    "the setup was cut short before it finished".into(),
                )));
            }
        }
    }
    reset_retries();
    set(Status::Running);
    let mut guard = CutShort(true);
    let result = exclusive(backend.setup(cfg, rebuild)).await.and_then(|r| r);
    guard.0 = false;
    record(&result);
    result
}

/// Keep the boundary set up: `converge_and_check` now, again whenever
/// `definitions` changes (looked at every `poll`), and, after a failure,
/// again when [`retry`] says the next attempt is due. Each call is expected
/// to go through [`converge`], which records its outcome.
pub async fn upkeep<D, C, F>(poll: Duration, mut definitions: D, mut converge_and_check: C)
where
    D: FnMut() -> Option<String>,
    C: FnMut() -> F,
    F: Future<Output = ()>,
{
    UPKEEP.store(true, Ordering::Relaxed);
    let mut seen = definitions();
    converge_and_check().await;
    let mut next_look = Instant::now() + poll;
    loop {
        let wake = match retry().next {
            Some(due) => due.min(next_look),
            None => next_look,
        };
        tokio::select! {
            _ = tokio::time::sleep_until(wake) => {}
            // An operator's setup moved the schedule: look at it again.
            _ = RETRY_CHANGED.notified() => continue,
        }
        if Instant::now() >= next_look {
            next_look = Instant::now() + poll;
            let now = definitions();
            if now != seen {
                tracing::info!("the boundary's definitions changed; setting it up again");
                seen = now;
                reset_retries();
                converge_and_check().await;
                continue;
            }
        }
        let due = retry();
        if due.next.is_some_and(|at| at <= Instant::now()) {
            tracing::info!(
                attempt = due.failures + 1,
                of = RETRY_LIMIT,
                "setting up the boundary again after it failed"
            );
            converge_and_check().await;
        }
    }
}

/// What a failed check adds while the node's own converge is running, or
/// after it failed: the check is not the whole story.
pub fn note() -> Option<String> {
    match status() {
        Status::Idle => None,
        Status::Running => {
            Some("the node is setting up its boundary and checks again when it finishes".into())
        }
        Status::Failed(error) => Some(failed_note(&error, retry(), crate::store::now_ms())),
    }
}

fn failed_note(error: &str, retry: Retry, now_ms: i64) -> String {
    let again = "run `tracon setup` or Set up again under Settings › System to try now";
    match retry.next_ms {
        Some(at) => {
            let secs = ((at - now_ms).max(0) + 999) / 1000;
            let when = match secs {
                0..=89 => format!("{secs}s"),
                _ => format!("{} min", (secs + 30) / 60),
            };
            format!(
                "the node could not set up its boundary itself ({error}); it tries again in \
                 {when} (attempt {} of {RETRY_LIMIT}), or {again}",
                retry.failures + 1
            )
        }
        None if retry.stopped() => format!(
            "the node could not set up its boundary itself ({error}) and stopped trying after \
             {} attempts; {again}",
            retry.failures
        ),
        // Nothing retries here (`[boundary] auto_reconcile` is off).
        None => format!("the node could not set up its boundary ({error}); {again}"),
    }
}

/// Hold a passing runtime check while the sandbox base is not pinned in this
/// process: a node that sets its boundary up itself (`converges`) pins the
/// base in doing so, and sessions, snapshots and DigitalOcean templates are
/// keyed on that pin. A setup run in another process leaves the files right
/// and the node unpinned, which is exactly what this shows.
pub fn hold_unpinned(report: &mut BoundaryReport, converges: bool, cfg: &Config) {
    if !converges || crate::sandbox_images::active_base().is_some() {
        return;
    }
    let Some(runtime) = report
        .checks
        .iter_mut()
        .find(|check| check.id == CheckId::Runtime && check.ok)
    else {
        return;
    };
    let base = cfg.sandbox.base_image_ref();
    let held = format!(
        "{}; but the sandbox base {base} is not pinned yet, which the node does when it sets up \
         its boundary",
        runtime.detail
    );
    *runtime = match running() {
        true => super::CheckResult::pending(CheckId::Runtime, held),
        false => super::CheckResult::fail(CheckId::Runtime, held),
    };
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

    /// A minute, doubling, at most half an hour apart, and eight attempts in
    /// all: the first and seven retries.
    #[test]
    fn retries_back_off_and_stop() {
        let waits: Vec<_> = (0..=RETRY_LIMIT + 1)
            .map(|failures| retry_delay(failures).map(|d| d.as_secs()))
            .collect();
        assert_eq!(
            waits,
            [
                None,
                Some(60),
                Some(120),
                Some(240),
                Some(480),
                Some(960),
                Some(1800),
                Some(1800),
                None,
                None
            ]
        );
        assert!(Retry {
            failures: RETRY_LIMIT,
            ..Default::default()
        }
        .stopped());
    }

    /// While retrying, the check says the last error and when the next
    /// attempt is; once stopped, says so and how to try again.
    #[test]
    fn a_failed_setup_says_when_it_tries_again() {
        let retrying = Retry {
            failures: 2,
            next: None,
            next_ms: Some(1_000 + 120_000),
        };
        let note = failed_note("no network", retrying, 1_000);
        assert!(note.contains("(no network)"), "{note}");
        assert!(
            note.contains("tries again in 2 min (attempt 3 of 8)"),
            "{note}"
        );
        assert!(note.contains("`tracon setup`"), "{note}");
        let soon = Retry {
            next_ms: Some(1_000 + 60_000),
            ..retrying
        };
        assert!(failed_note("x", soon, 1_000).contains("in 60s"));
        let stopped = Retry {
            failures: RETRY_LIMIT,
            next: None,
            next_ms: None,
        };
        let note = failed_note("no network", stopped, 1_000);
        assert!(note.contains("stopped trying after 8 attempts"), "{note}");
        assert!(
            note.contains("Set up again under Settings › System"),
            "{note}"
        );
        // Where nothing retries, it promises none.
        let unretried = Retry {
            failures: 1,
            next: None,
            next_ms: None,
        };
        let note = failed_note("no network", unretried, 1_000);
        assert!(
            !note.contains("tries again") && !note.contains("stopped"),
            "{note}"
        );
        assert!(note.contains("`tracon setup`"), "{note}");
    }
}
