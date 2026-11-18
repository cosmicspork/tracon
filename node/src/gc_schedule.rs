//! `tracon gc --apply` on a timer the operator sets (`[gc]`).
//!
//! Off unless `every_secs` is set. Each run first archives this node's own
//! ended sessions nobody has touched for `archive_ended_after_secs`, then
//! sweeps exactly as `tracon gc --apply` does (`crate::gc::sweep`), with no
//! deletion rule of its own: what an archived session left goes, and a
//! workspace another open session still uses stays. Only runtime storage is
//! ever removed; session rows, their logs and evidence are kept.
//!
//! A scheduled run and a sweep asked for by hand never overlap: both hold
//! [`lock`] while they remove. The last run is kept in memory, for Settings
//! and `tracon gc` to say when it was and what it did.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::config::Config;

/// Held by whatever removes runtime storage: the scheduled run, and a
/// sweep with `apply` asked for through `POST /api/maintenance/storage`.
pub fn lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &LOCK
}

/// The shortest interval the node runs the sweep at.
pub const FLOOR: Duration = Duration::from_secs(3600);

/// One item a run could not remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub kind: String,
    pub name: String,
    pub error: String,
}

/// What one scheduled run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub started_ms: i64,
    pub finished_ms: i64,
    /// Sessions it archived.
    pub archived: usize,
    /// Sessions old enough to archive that it left alone, because their
    /// review is still open or its publication is not settled.
    #[serde(default)]
    pub held: Vec<String>,
    /// Pieces of runtime storage it removed.
    pub removed: usize,
    /// What those freed, where it is known (images).
    pub bytes: u64,
    #[serde(default)]
    pub failures: Vec<Failure>,
    /// Why the run stopped short, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The schedule as it stands, and the last run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The interval in force, floor applied; unset while the sweep is off.
    #[serde(default)]
    pub every_secs: Option<u64>,
    /// When the next run is due, while the sweep is on.
    #[serde(default)]
    pub next_ms: Option<i64>,
    #[serde(default)]
    pub last: Option<Run>,
}

fn status_cell() -> &'static parking_lot::Mutex<Status> {
    static STATUS: std::sync::LazyLock<parking_lot::Mutex<Status>> =
        std::sync::LazyLock::new(Default::default);
    &STATUS
}

/// The schedule and the last run, for the API.
pub fn status() -> Status {
    status_cell().lock().clone()
}

/// The schedule as one line, as Settings shows it and `tracon gc` prints it.
pub fn describe(status: &Status, now_ms: i64) -> String {
    let next = match (status.every_secs, status.next_ms) {
        (None, _) => "off ([gc] every_secs is unset)".to_string(),
        (Some(every), Some(next)) => format!(
            "next {}, every {}",
            until(next - now_ms),
            span(every as i64 * 1000)
        ),
        (Some(every), None) => format!("every {}", span(every as i64 * 1000)),
    };
    let Some(last) = &status.last else {
        return format!("automatic sweep: none since the node started; {next}");
    };
    let mut did = format!("archived {}, removed {}", last.archived, last.removed);
    if last.bytes > 0 {
        did.push_str(&format!(" ({})", bytes(last.bytes)));
    }
    if !last.failures.is_empty() {
        did.push_str(&format!(", {} could not be removed", last.failures.len()));
    }
    if !last.held.is_empty() {
        did.push_str(&format!(
            ", {} held for an open review or publication",
            last.held.len()
        ));
    }
    if let Some(error) = &last.error {
        did.push_str(&format!(", stopped: {error}"));
    }
    format!(
        "last automatic sweep {} ago: {did}; {next}",
        span(now_ms - last.finished_ms)
    )
}

fn until(ms: i64) -> String {
    if ms <= 0 {
        "now".into()
    } else {
        format!("in {}", span(ms))
    }
}

/// A span of time as a person reads it, to its largest whole unit.
fn span(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3600 => format!("{}h", s / 3600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    match unit {
        0 => format!("{n} B"),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

/// When the timer runs: the node's own, or a shorter one for tests.
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    /// The shortest interval: a shorter `every_secs` is raised to it.
    floor: Duration,
    /// How long the timer waits between looks at the configuration: before
    /// the first run, and while the sweep is off.
    idle: Duration,
}

impl Default for Schedule {
    fn default() -> Self {
        Self::new()
    }
}

impl Schedule {
    /// The node's: at least an hour apart, the configuration looked at once
    /// a minute.
    pub fn new() -> Self {
        Self {
            floor: FLOOR,
            idle: Duration::from_secs(60),
        }
    }

    /// A schedule with its own floor and idle wait, for tests that cannot
    /// wait an hour.
    #[doc(hidden)]
    pub fn with(floor: Duration, idle: Duration) -> Self {
        Self { floor, idle }
    }

    /// The interval `[gc]` asks for now, floor applied; `None` while off.
    pub fn every(&self, gc: &crate::config::Gc) -> Option<Duration> {
        (gc.every_secs > 0).then(|| Duration::from_secs(gc.every_secs).max(self.floor))
    }

    /// Run the sweep on its timer, for as long as the node runs.
    pub async fn run(self, manager: crate::session::Manager, cfg: Arc<Config>) {
        self.tick(cfg.clone(), move || {
            let manager = manager.clone();
            let cfg = cfg.clone();
            async move { sweep_once(&manager, &cfg).await }
        })
        .await
    }

    /// The timer around `run_once`. `[gc]` is read again on every look, so
    /// an edit of it applies from the next run, and turning it off stops the
    /// next one. The first run comes one idle wait after the node starts.
    pub async fn tick<F, Fut>(self, cfg: Arc<Config>, mut run_once: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let started = Instant::now();
        let mut last: Option<Instant> = None;
        loop {
            let every = self.every(&cfg.live().gc);
            let Some(every) = every else {
                {
                    let mut status = status_cell().lock();
                    status.every_secs = None;
                    status.next_ms = None;
                }
                tokio::time::sleep(self.idle).await;
                continue;
            };
            let due = match last {
                Some(last) => last + every,
                None => started + self.idle,
            };
            let now = Instant::now();
            if now < due {
                let wait = due - now;
                {
                    let mut status = status_cell().lock();
                    status.every_secs = Some(every.as_secs());
                    status.next_ms = Some(crate::store::now_ms() + wait.as_millis() as i64);
                }
                // Looked at again within an idle wait, so an edit or turning
                // it off is not held up by a long interval.
                tokio::time::sleep(wait.min(self.idle)).await;
                continue;
            }
            last = Some(now);
            run_once().await;
        }
    }
}

/// One scheduled run: archive what is old enough, sweep, report. Holds the
/// sweep lock throughout, so a sweep asked for by hand meanwhile is told
/// one is running.
pub async fn sweep_once(manager: &crate::session::Manager, cfg: &Config) {
    let _held = lock().lock().await;
    let gc = cfg.live().gc.clone();
    let mut run = Run {
        started_ms: crate::store::now_ms(),
        ..Default::default()
    };
    let store = manager.store();
    let node_id = manager.node_id();
    if let Some(after) = gc.archive_ended_after_secs {
        let before = run.started_ms - (after as i64).saturating_mul(1000);
        match store.archive_aged_ended_sessions(node_id, before, run.started_ms) {
            Ok(done) => {
                run.archived = done.archived.len();
                for row in done.archived {
                    manager
                        .bus()
                        .publish_untapped(crate::stream::Frame::Session(Box::new(row)));
                }
                run.held = done.held;
            }
            Err(error) => run.error = Some(format!("could not archive ended sessions: {error}")),
        }
    }
    if run.error.is_none() {
        let swept = crate::gc::sweep(
            manager.backend().as_ref(),
            cfg,
            store,
            node_id,
            crate::gc::Sweep {
                apply: true,
                caches: gc.caches,
                legacy: false,
                adopt_unlabelled: false,
            },
        )
        .await;
        match swept {
            Ok(items) => {
                for item in items {
                    if item.removed {
                        run.removed += 1;
                        run.bytes += item.size.unwrap_or(0);
                    } else if let (true, Some(error)) = (item.remove, item.error) {
                        run.failures.push(Failure {
                            kind: item.kind.to_string(),
                            name: item.name,
                            error,
                        });
                    }
                }
            }
            Err(error) => run.error = Some(error),
        }
    }
    run.finished_ms = crate::store::now_ms();
    report(&run);
    status_cell().lock().last = Some(run);
}

fn report(run: &Run) {
    for failure in &run.failures {
        tracing::warn!(
            kind = %failure.kind,
            name = %failure.name,
            error = %failure.error,
            "scheduled storage sweep could not remove this"
        );
    }
    if let Some(error) = &run.error {
        tracing::warn!(%error, archived = run.archived, "scheduled storage sweep stopped");
        return;
    }
    if run.held.is_empty() {
        tracing::info!(
            archived = run.archived,
            removed = run.removed,
            bytes = run.bytes,
            failed = run.failures.len(),
            "scheduled storage sweep done"
        );
    } else {
        tracing::info!(
            archived = run.archived,
            held = run.held.len(),
            held_sessions = %run.held.join(","),
            removed = run.removed,
            bytes = run.bytes,
            failed = run.failures.len(),
            "scheduled storage sweep done; ended sessions whose review is open or whose \
             publication is not settled stay unarchived, as approving or publishing reads \
             their workspace again"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn config(every_secs: u64) -> Arc<Config> {
        Arc::new(Config {
            gc: crate::config::Gc {
                every_secs,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// Runs `schedule` for `for_` of (paused) time and counts the runs.
    async fn runs(schedule: Schedule, cfg: Arc<Config>, for_: Duration) -> usize {
        let count = Arc::new(AtomicUsize::new(0));
        let counted = count.clone();
        let timer = tokio::spawn(schedule.tick(cfg, move || {
            counted.fetch_add(1, Ordering::SeqCst);
            async {}
        }));
        tokio::time::sleep(for_).await;
        timer.abort();
        count.load(Ordering::SeqCst)
    }

    /// The first run comes one idle wait after start, then one each
    /// interval; an interval under the floor is raised to it.
    #[tokio::test(start_paused = true)]
    async fn the_timer_runs_on_its_interval_and_never_under_its_floor() {
        let quick = Schedule::with(Duration::ZERO, Duration::from_millis(10));
        // At 0.01s, 1.01s, 2.01s and 3.01s.
        assert_eq!(runs(quick, config(1), Duration::from_millis(3500)).await, 4);
        let floored = Schedule::with(Duration::from_secs(2), Duration::from_millis(10));
        // At 0.01s and 2.01s: one second is raised to two.
        assert_eq!(
            runs(floored, config(1), Duration::from_millis(3500)).await,
            2
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_timer_never_runs_while_off() {
        let quick = Schedule::with(Duration::ZERO, Duration::from_millis(10));
        assert_eq!(runs(quick, config(0), Duration::from_secs(5)).await, 0);
    }

    #[test]
    fn the_node_s_floor_is_an_hour() {
        let schedule = Schedule::new();
        let gc = |every_secs| crate::config::Gc {
            every_secs,
            ..Default::default()
        };
        assert_eq!(schedule.every(&gc(0)), None);
        assert_eq!(schedule.every(&gc(60)), Some(FLOOR));
        assert_eq!(
            schedule.every(&gc(86_400)),
            Some(Duration::from_secs(86_400))
        );
    }

    #[test]
    fn the_status_line_says_when_and_what() {
        let off = Status::default();
        assert_eq!(
            describe(&off, 0),
            "automatic sweep: none since the node started; off ([gc] every_secs is unset)"
        );
        let hour = 3_600_000;
        let status = Status {
            every_secs: Some(86_400),
            next_ms: Some(30 * hour),
            last: Some(Run {
                started_ms: 6 * hour - 5000,
                finished_ms: 6 * hour,
                archived: 2,
                held: vec!["s1".into()],
                removed: 14,
                bytes: 1_200_000_000,
                failures: vec![Failure {
                    kind: "volume".into(),
                    name: "tracon-workspace-x".into(),
                    error: "in use".into(),
                }],
                error: None,
            }),
        };
        assert_eq!(
            describe(&status, 9 * hour),
            "last automatic sweep 3h ago: archived 2, removed 14 (1.2 GB), 1 could not be \
             removed, 1 held for an open review or publication; next in 21h, every 1d"
        );
    }
}
