//! A node whose own boundary setup fails tries it again, a minute later and
//! then twice as long each time, and stops after eight attempts. A retry
//! that succeeds leaves the sandbox base pinned. One test: what it watches
//! (the setup's status and the pin) is process-wide.

#[path = "support/mod.rs"]
mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use tracon::boundary::reconcile::{self, RETRY_LIMIT};
use tracon::boundary::{Backend, BoundaryError, BoundaryReport};
use tracon::config::Config;
use tracon::runner::{local::LocalBackend, Mount, Runner};

const PINNED: &str = "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:\
    2222222222222222222222222222222222222222222222222222222222222222";

/// Fails its converge `failing` times, then pins the base.
struct Flaky {
    failing: AtomicU32,
    attempts: AtomicU32,
}

impl Flaky {
    fn failing(times: u32) -> Arc<Self> {
        Arc::new(Flaky {
            failing: AtomicU32::new(times),
            attempts: AtomicU32::new(0),
        })
    }
}

#[async_trait]
impl Backend for Flaky {
    fn kind(&self) -> &'static str {
        "local"
    }
    async fn setup(&self, cfg: &Config, _rebuild: bool) -> Result<(), BoundaryError> {
        self.converge(cfg).await.map(|_| ())
    }
    async fn converge(&self, _cfg: &Config) -> Result<bool, BoundaryError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let failed = self
            .failing
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if failed {
            return Err(BoundaryError::Other("no network".into()));
        }
        tracon::sandbox_images::pin("ghcr.io/cosmicspork/tracon-sandbox-base:9.9.9", PINNED);
        Ok(true)
    }
    fn converges(&self) -> bool {
        true
    }
    async fn check_all(&self, _cfg: &Config, _deep: bool) -> BoundaryReport {
        BoundaryReport { checks: Vec::new() }
    }
    fn runner(&self, extra: Vec<Mount>) -> Arc<dyn Runner> {
        LocalBackend.runner(extra)
    }
    fn runner_for(&self, harness_id: &str, extra: Vec<Mount>) -> Arc<dyn Runner> {
        LocalBackend.runner_for(harness_id, extra)
    }
    async fn import_volume(&self, v: &str, s: &std::path::Path) -> Result<(), BoundaryError> {
        LocalBackend.import_volume(v, s).await
    }
    async fn export_volume(&self, v: &str, d: &std::path::Path) -> Result<(), BoundaryError> {
        LocalBackend.export_volume(v, d).await
    }
    async fn clone_volume(&self, s: &str, d: &str) -> Result<(), BoundaryError> {
        LocalBackend.clone_volume(s, d).await
    }
    fn harness_host(&self) -> String {
        LocalBackend.harness_host()
    }
    fn harness_home(&self) -> String {
        LocalBackend.harness_home()
    }
    async fn reconcile(&self, names: &[String]) {
        LocalBackend.reconcile(names).await
    }
}

/// The node's upkeep over `backend`, with definitions that never change.
fn upkeep(backend: Arc<Flaky>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let cfg = Config::default();
        reconcile::upkeep(
            Duration::from_secs(300),
            || Some("unchanging".into()),
            || async {
                let _ = reconcile::converge(backend.as_ref(), &cfg).await;
            },
        )
        .await
    })
}

/// Let virtual time run until `done`, or fail after `limit` of it.
async fn until(limit: Duration, mut done: impl FnMut() -> bool) {
    let start = tokio::time::Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "not done after {limit:?}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_setup_is_retried_with_backoff_until_it_pins_or_gives_up() {
    support::state::isolate();

    // Two failures, then a retry that pins: one minute, then two, apart.
    let flaky = Flaky::failing(2);
    let start = tokio::time::Instant::now();
    let running = upkeep(flaky.clone());
    until(Duration::from_secs(3600), || {
        tracon::sandbox_images::active_base().is_some()
    })
    .await;
    assert_eq!(flaky.attempts.load(Ordering::SeqCst), 3);
    assert!(
        start.elapsed() >= Duration::from_secs(180),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        tracon::sandbox_images::active_base().as_deref(),
        Some(PINNED)
    );
    assert_eq!(reconcile::status(), reconcile::Status::Idle);
    assert_eq!(reconcile::retry(), reconcile::Retry::default());
    assert_eq!(reconcile::note(), None);
    // Nothing more to do: no more attempts.
    tokio::time::sleep(Duration::from_secs(4 * 3600)).await;
    assert_eq!(flaky.attempts.load(Ordering::SeqCst), 3);
    running.abort();

    // One that never succeeds stops after eight attempts, and says so.
    let broken = Flaky::failing(u32::MAX);
    let running = upkeep(broken.clone());
    until(Duration::from_secs(12 * 3600), || {
        reconcile::retry().stopped()
    })
    .await;
    assert_eq!(broken.attempts.load(Ordering::SeqCst), RETRY_LIMIT);
    let note = reconcile::note().unwrap();
    assert!(note.contains("no network"), "{note}");
    assert!(note.contains("stopped trying after 8 attempts"), "{note}");
    tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
    assert_eq!(broken.attempts.load(Ordering::SeqCst), RETRY_LIMIT);

    // An operator's setup starts the count again: failed, it is the first
    // of a fresh count with a retry a minute on.
    let cfg = Config::default();
    assert!(reconcile::operator_setup(broken.as_ref(), &cfg, false)
        .await
        .is_err());
    let retry = reconcile::retry();
    assert_eq!(retry.failures, 1);
    assert!(retry.next.is_some());
    assert!(reconcile::note().unwrap().contains("attempt 2 of 8"));
    until(Duration::from_secs(120), || {
        broken.attempts.load(Ordering::SeqCst) == RETRY_LIMIT + 2
    })
    .await;
    running.abort();
}
