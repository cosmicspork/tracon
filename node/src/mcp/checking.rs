//! Submissions whose required checks are running on the node.
//!
//! A required check is minutes of subprocess, and an MCP client gives up on a
//! tool call that runs long: Claude Code 2.1.247 dropped it at about five
//! minutes whatever timeout its config named, and no release waits past that
//! timeout (twenty minutes from this node). Dropping the call used to drop the
//! check with it. So `submit_review` hands the checks to
//! a task the node owns and returns a handle, and `review_status` waits on that
//! handle the way it waits on a human. Nothing here outlives the process: a restart
//! interrupts every `running` check row and closes the sessions that asked
//! for them (`Store::reconcile_interrupted_runs`,
//! `session::reconcile_after_restart`).
//!
//! Checks the machine had no room for (`review::checks::no_room`: the pod
//! could not be scheduled, was refused, or was evicted) are no verdict on
//! the candidate, so they are not refused: the submission joins a line,
//! and while anything is in it the node's checks run one at a time, the
//! next only once none is running. A submission whose checks stop short of
//! room [`MAX_SHORT`] times is held instead of tried again: out of the
//! line, still waiting, until a resubmission of its commit puts it back.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde_json::Value;

/// What a submission came to: the response `submit_review` would have given
/// had it waited, or the refusal it would have returned.
pub type Outcome = Result<Value, String>;

/// How long a settled entry is kept for `review_status` to report. A handle
/// that is polled after this reads as the review it became, or as unknown.
const KEEP_SETTLED_MS: i64 = 60 * 60 * 1000;

/// How many times a submission's checks may stop short of room before the
/// node stops trying them on its own and holds them. Run one at a time, a
/// check that is still put out is one this machine cannot fit, and trying
/// it again would only loop.
pub const MAX_SHORT: u32 = 3;

/// How long a check that stopped short of room waits before it is tried
/// again: time for the machine to finish putting things out, and for a
/// pressure taint to lift.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// How often a queued submission looks again whether it is its turn, beside
/// being woken when a run ends: and whether it is still wanted.
const TURN_POLL: Duration = Duration::from_millis(500);

/// The node's running and recently settled check submissions, by handle,
/// and the line of those waiting for room.
pub struct Checking {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    line: Mutex<Line>,
    moved: tokio::sync::Notify,
    retry_after_ms: AtomicU64,
}

impl Default for Checking {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            line: Mutex::default(),
            moved: tokio::sync::Notify::new(),
            retry_after_ms: AtomicU64::new(RETRY_AFTER.as_millis() as u64),
        }
    }
}

/// Submissions waiting for room, first first, and the runs under way.
#[derive(Default)]
struct Line {
    waiting: VecDeque<Arc<Entry>>,
    /// Check runs under way, however they started.
    running: usize,
    /// Of those, the runs taken from the line. While any is going, or
    /// anything waits, a new submission queues rather than starts.
    serial: usize,
}

/// Where a submission stands while its checks wait for room.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    pub since_ms: i64,
    /// What the machine said the last time its checks stopped short of
    /// room; `None` while it only waits behind others.
    pub why: Option<String>,
    /// How many times its checks have stopped short of room.
    pub short: u32,
    /// Set once the node has stopped trying them on its own: what it said
    /// last. Out of the line until a resubmission releases it.
    pub held: Option<String>,
    not_before: Option<tokio::time::Instant>,
}

/// Why a queued submission stopped waiting for its turn.
#[derive(Debug, PartialEq)]
pub enum Stop {
    /// Whoever asked for the checks withdrew them.
    Cancelled(String),
    /// The submission settled while it waited: replaced by a newer one.
    Settled,
}

/// A check run's place among those under way: given up when dropped, which
/// lets the next in line look again.
pub struct Slot {
    checking: Arc<Checking>,
    serial: bool,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut line = self.checking.line.lock().unwrap();
        line.running = line.running.saturating_sub(1);
        if self.serial {
            line.serial = line.serial.saturating_sub(1);
        }
        drop(line);
        self.checking.moved.notify_waiters();
    }
}

/// One submission's checks.
pub struct Entry {
    /// The id `review_status` takes: the review this submission revises, or
    /// the id the review it opens will have.
    pub handle: String,
    /// The session that submitted, the only caller that may read it.
    pub session_id: String,
    /// The candidate and its check definitions: what a retried submission of
    /// the same commit attaches to instead of starting another run.
    pub key: String,
    pub candidate_id: String,
    pub commands: Vec<String>,
    pub started_ms: i64,
    settled: tokio::sync::watch::Sender<Option<Settled>>,
    /// Set while it waits for room, in the line or held.
    queued: tokio::sync::watch::Sender<Option<Queued>>,
    /// How many times its checks have stopped short of room, kept while
    /// they run again.
    short: std::sync::atomic::AtomicU32,
}

#[derive(Clone)]
struct Settled {
    outcome: Outcome,
    /// The review was opened or revised, whatever came after it.
    recorded: bool,
    at_ms: i64,
}

/// The attach key: the submitting session, the candidate, and every check
/// definition it answers to, in order.
pub fn key(session_id: &str, candidate_id: &str, commands: &[String]) -> String {
    let mut key = format!("{session_id}\n{candidate_id}");
    for command in commands {
        key.push('\n');
        key.push_str(command);
    }
    key
}

impl Checking {
    /// Record checks that have started under `handle`, replacing whatever
    /// settled entry the handle held before.
    pub fn begin(
        &self,
        handle: &str,
        session_id: &str,
        candidate_id: &str,
        commands: Vec<String>,
    ) -> Arc<Entry> {
        let entry = Arc::new(Entry {
            handle: handle.to_string(),
            session_id: session_id.to_string(),
            key: key(session_id, candidate_id, &commands),
            candidate_id: candidate_id.to_string(),
            commands,
            started_ms: crate::store::now_ms(),
            settled: tokio::sync::watch::Sender::new(None),
            queued: tokio::sync::watch::Sender::new(None),
            short: Default::default(),
        });
        let mut entries = self.entries.lock().unwrap();
        let now = crate::store::now_ms();
        entries.retain(|_, entry| {
            entry
                .settled_ms()
                .is_none_or(|at| now - at < KEEP_SETTLED_MS)
        });
        entries.insert(handle.to_string(), entry.clone());
        entry
    }

    /// The entry under `handle`, if the node has one.
    pub fn get(&self, handle: &str) -> Option<Arc<Entry>> {
        self.entries.lock().unwrap().get(handle).cloned()
    }

    /// The entries `session_id` submitted, running or recently settled.
    pub fn of_session(&self, session_id: &str) -> Vec<Arc<Entry>> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .filter(|entry| entry.session_id == session_id)
            .cloned()
            .collect()
    }

    /// Submissions whose checks have not settled: each may start another
    /// check after the one running now finishes. One held for want of room
    /// starts nothing until it is submitted again, so a restart does not
    /// wait on it.
    pub fn in_flight(&self) -> usize {
        self.entries
            .lock()
            .unwrap()
            .values()
            .filter(|entry| entry.outcome().is_none() && entry.held().is_none())
            .count()
    }

    /// How long a check that stopped short of room waits before its next
    /// try.
    pub fn set_retry_after(&self, after: Duration) {
        self.retry_after_ms
            .store(after.as_millis() as u64, Ordering::Relaxed);
    }

    /// Start `entry`'s checks now, when nothing waits for room and no run
    /// taken from the line is under way: its slot. Otherwise it joins the
    /// end of the line, and [`Self::turn`] waits for its slot.
    pub fn start(self: &Arc<Self>, entry: &Arc<Entry>) -> Option<Slot> {
        let mut line = self.line.lock().unwrap();
        if line.waiting.is_empty() && line.serial == 0 {
            line.running += 1;
            return Some(Slot {
                checking: self.clone(),
                serial: false,
            });
        }
        line.waiting.push_back(entry.clone());
        entry.queued.send_replace(Some(Queued {
            since_ms: crate::store::now_ms(),
            why: None,
            short: 0,
            held: None,
            not_before: None,
        }));
        None
    }

    /// Wait for `entry`'s turn: first in line, no run under way, and past
    /// its wait after stopping short. A held entry waits until it is
    /// released. `cancelled` is asked as it waits, and says why the checks
    /// are no longer wanted.
    pub async fn turn(
        self: &Arc<Self>,
        entry: &Arc<Entry>,
        cancelled: impl Fn() -> Option<String>,
    ) -> Result<Slot, Stop> {
        loop {
            // Registered before the line is read, so a run ending in between
            // still wakes this.
            let moved = self.moved.notified();
            tokio::pin!(moved);
            moved.as_mut().enable();
            if entry.outcome().is_some() {
                self.leave(entry);
                return Err(Stop::Settled);
            }
            if let Some(reason) = cancelled() {
                self.leave(entry);
                return Err(Stop::Cancelled(reason));
            }
            {
                let mut line = self.line.lock().unwrap();
                let first = line
                    .waiting
                    .front()
                    .is_some_and(|first| Arc::ptr_eq(first, entry));
                let due = entry.queued.borrow().as_ref().is_none_or(|queued| {
                    queued
                        .not_before
                        .is_none_or(|at| tokio::time::Instant::now() >= at)
                });
                if first && due && line.running == 0 {
                    line.waiting.pop_front();
                    line.running += 1;
                    line.serial += 1;
                    entry.queued.send_replace(None);
                    return Ok(Slot {
                        checking: self.clone(),
                        serial: true,
                    });
                }
            }
            tokio::select! {
                _ = moved => {}
                _ = tokio::time::sleep(TURN_POLL) => {}
            }
        }
    }

    /// `entry`'s checks stopped short of room, as `why` says: back to the
    /// end of the line to be tried again after a wait, or, the
    /// [`MAX_SHORT`]th time, held. Where it stands now.
    pub fn stopped_short(&self, entry: &Arc<Entry>, why: String) -> Queued {
        let short = entry.short.fetch_add(1, Ordering::Relaxed) + 1;
        let held = (short >= MAX_SHORT).then(|| why.clone());
        let after = Duration::from_millis(self.retry_after_ms.load(Ordering::Relaxed));
        let queued = Queued {
            since_ms: crate::store::now_ms(),
            why: Some(why),
            short,
            held: held.clone(),
            not_before: Some(tokio::time::Instant::now() + after),
        };
        let mut line = self.line.lock().unwrap();
        line.waiting.retain(|waiting| !Arc::ptr_eq(waiting, entry));
        if held.is_none() {
            line.waiting.push_back(entry.clone());
        }
        entry.queued.send_replace(Some(queued.clone()));
        drop(line);
        self.moved.notify_waiters();
        queued
    }

    /// Put a held entry back at the end of the line, to be tried at once
    /// when its turn comes, as if it had never stopped short. Whether it
    /// was held.
    pub fn release(&self, entry: &Arc<Entry>) -> bool {
        let mut line = self.line.lock().unwrap();
        if entry.held().is_none() || entry.outcome().is_some() {
            return false;
        }
        line.waiting.push_back(entry.clone());
        entry.short.store(0, Ordering::Relaxed);
        entry.queued.send_replace(Some(Queued {
            since_ms: crate::store::now_ms(),
            why: None,
            short: 0,
            held: None,
            not_before: None,
        }));
        drop(line);
        self.moved.notify_waiters();
        true
    }

    /// Take `entry` out of the line.
    fn leave(&self, entry: &Arc<Entry>) {
        let mut line = self.line.lock().unwrap();
        line.waiting.retain(|waiting| !Arc::ptr_eq(waiting, entry));
        entry.queued.send_replace(None);
        drop(line);
        self.moved.notify_waiters();
    }

    /// `entry`'s place in the line, first being 1; `None` when it is not in
    /// it.
    pub fn position(&self, entry: &Entry) -> Option<usize> {
        self.line
            .lock()
            .unwrap()
            .waiting
            .iter()
            .position(|waiting| std::ptr::eq(waiting.as_ref(), entry))
            .map(|at| at + 1)
    }

    /// Checks still running under `key`.
    pub fn running(&self, key: &str) -> Option<Arc<Entry>> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .find(|entry| entry.key == key && entry.outcome().is_none())
            .cloned()
    }
}

impl Entry {
    /// What the submission came to, once it has.
    pub fn outcome(&self) -> Option<Outcome> {
        self.settled
            .borrow()
            .as_ref()
            .map(|settled| settled.outcome.clone())
    }

    /// Whether the submission got as far as opening or revising its review.
    /// An outcome that is an error after that (an automatic publication that
    /// failed, say) is the review's to report, not a failed submission.
    pub fn recorded(&self) -> bool {
        self.settled
            .borrow()
            .as_ref()
            .is_some_and(|settled| settled.recorded)
    }

    fn settled_ms(&self) -> Option<i64> {
        self.settled.borrow().as_ref().map(|settled| settled.at_ms)
    }

    /// Whole seconds since the checks started.
    pub fn running_secs(&self) -> i64 {
        (crate::store::now_ms() - self.started_ms).max(0) / 1000
    }

    /// Where it stands while it waits for room; `None` once it is running,
    /// or never waited.
    pub fn queued(&self) -> Option<Queued> {
        self.queued.borrow().clone()
    }

    /// Why the node stopped trying its checks, when it has.
    pub fn held(&self) -> Option<String> {
        self.queued.borrow().as_ref().and_then(|q| q.held.clone())
    }

    /// Wait for the outcome until `deadline`; `None` when the checks are
    /// still running (or queued) then, or when the node holds them during
    /// the wait: that is news a waiter is told at once.
    pub async fn wait(&self, deadline: tokio::time::Instant) -> Option<Outcome> {
        let mut settled = self.settled.subscribe();
        let mut queued = self.queued.subscribe();
        queued.mark_unchanged();
        let held = async {
            loop {
                if queued.changed().await.is_err() {
                    return std::future::pending::<()>().await;
                }
                if queued
                    .borrow_and_update()
                    .as_ref()
                    .is_some_and(|q| q.held.is_some())
                {
                    return;
                }
            }
        };
        let waited = tokio::time::timeout_at(deadline, async {
            tokio::select! {
                settled = settled.wait_for(Option::is_some) => {
                    settled.ok().and_then(|settled| settled.as_ref().map(|s| s.outcome.clone()))
                }
                _ = held => None,
            }
        })
        .await;
        match waited {
            Ok(Some(outcome)) => Some(outcome),
            _ => self.outcome(),
        }
    }

    /// Settle the submission. The first outcome stands.
    pub fn finish(&self, outcome: Outcome, recorded: bool) {
        self.settled.send_if_modified(|held| {
            if held.is_some() {
                return false;
            }
            *held = Some(Settled {
                outcome,
                recorded,
                at_ms: crate::store::now_ms(),
            });
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_waiter_sees_the_outcome_and_a_retry_finds_the_running_entry() {
        let checking = Checking::default();
        let entry = checking.begin("r1", "s1", "c1", vec!["just check".into()]);
        let key = key("s1", "c1", &["just check".into()]);
        assert!(checking.running(&key).is_some());
        assert!(checking
            .running(&super::key("s1", "c1", &["other".into()]))
            .is_none());
        assert!(checking
            .running(&super::key("s2", "c1", &["just check".into()]))
            .is_none());

        let soon = tokio::time::Instant::now() + std::time::Duration::from_millis(20);
        assert!(entry.wait(soon).await.is_none(), "still running");

        let waiting = {
            let entry = entry.clone();
            tokio::spawn(async move {
                entry
                    .wait(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
                    .await
            })
        };
        entry.finish(Ok(json!({ "review_id": "r1" })), true);
        entry.finish(Err("too late".into()), false);
        assert_eq!(
            waiting.await.unwrap(),
            Some(Ok(json!({ "review_id": "r1" })))
        );
        assert!(checking.running(&key).is_none(), "settled is not running");
        assert!(checking.get("r1").unwrap().outcome().unwrap().is_ok());
        assert!(entry.recorded());
    }

    fn never() -> Option<String> {
        None
    }

    /// With nothing short of room, checks start at once, side by side. Once
    /// one stops short, it and every submission after it wait in a line
    /// that runs one at a time, the next only once nothing is running, until
    /// the line is empty again.
    #[tokio::test]
    async fn checks_short_of_room_run_one_at_a_time() {
        let checking = Arc::new(Checking::default());
        checking.set_retry_after(Duration::ZERO);
        let a = checking.begin("a", "s1", "c1", vec!["just check".into()]);
        let b = checking.begin("b", "s2", "c2", vec!["just check".into()]);
        let first = checking.start(&a).expect("nothing waits: a starts");
        let beside = checking.start(&b).expect("and b beside it");
        drop(beside);

        // a's machine had no room: it queues, and so does what comes next.
        drop(first);
        let now = checking.stopped_short(&a, "evicted: low on memory".into());
        assert_eq!(now.short, 1);
        assert!(now.held.is_none());
        let c = checking.begin("c", "s3", "c3", vec!["just check".into()]);
        assert!(checking.start(&c).is_none(), "c queues behind a");
        assert_eq!(checking.position(&a), Some(1));
        assert_eq!(checking.position(&c), Some(2));
        assert!(c.queued().unwrap().why.is_none());

        // a's turn comes; c waits until a's run is over.
        let a_runs = checking.turn(&a, never).await.unwrap();
        assert!(a.queued().is_none(), "running, not queued");
        assert_eq!(checking.position(&c), Some(1));
        let waiting_c = tokio::spawn({
            let checking = checking.clone();
            let c = c.clone();
            async move {
                checking
                    .turn(&c, never)
                    .await
                    .map(|slot| (slot, crate::store::now_ms()))
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting_c.is_finished(), "one at a time");
        // A new submission meanwhile queues too: the line is serial still.
        let d = checking.begin("d", "s4", "c4", vec!["just check".into()]);
        assert!(checking.start(&d).is_none());
        let a_done = crate::store::now_ms();
        drop(a_runs);
        let (c_runs, c_began) = waiting_c.await.unwrap().unwrap();
        assert!(c_began >= a_done, "c began after a ended");
        drop(c_runs);
        drop(checking.turn(&d, never).await.unwrap());
        // The line is empty and nothing runs from it: back to side by side.
        let e = checking.begin("e", "s5", "c5", vec!["just check".into()]);
        assert!(checking.start(&e).is_some());
    }

    /// Stopping short of room again and again holds the submission instead
    /// of trying it for ever: out of the line, still unsettled, not counted
    /// as checks a restart waits on, and the waiter told at once. A
    /// resubmission puts it back, its count started again.
    #[tokio::test]
    async fn checks_that_never_fit_are_held_until_submitted_again() {
        let checking = Arc::new(Checking::default());
        checking.set_retry_after(Duration::ZERO);
        let a = checking.begin("a", "s1", "c1", vec!["just check".into()]);
        drop(checking.start(&a).unwrap());
        for tried in 1..MAX_SHORT {
            let now = checking.stopped_short(&a, format!("evicted {tried}"));
            assert!(now.held.is_none(), "{now:?}");
            drop(checking.turn(&a, never).await.unwrap());
        }
        assert_eq!(checking.in_flight(), 1);
        let waiter = tokio::spawn({
            let a = a.clone();
            async move {
                a.wait(tokio::time::Instant::now() + Duration::from_secs(10))
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = checking.stopped_short(&a, "evicted again".into());
        assert_eq!(now.held.as_deref(), Some("evicted again"));
        assert_eq!(now.short, MAX_SHORT);
        let told = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("a waiter hears of the hold at once")
            .unwrap();
        assert!(told.is_none(), "held is no outcome");
        assert_eq!(a.held().as_deref(), Some("evicted again"));
        assert_eq!(checking.position(&a), None, "out of the line");
        assert_eq!(checking.in_flight(), 0, "a restart does not wait on it");
        assert!(a.outcome().is_none());
        // Its own turn does not come while it is held.
        let turn = tokio::spawn({
            let checking = checking.clone();
            let a = a.clone();
            async move { checking.turn(&a, never).await.is_ok() }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!turn.is_finished());
        assert!(checking.release(&a));
        assert!(!checking.release(&a), "only a held entry is released");
        assert!(turn.await.unwrap(), "released, its turn comes");
        assert_eq!(a.queued(), None);
    }

    /// Withdrawn while queued, or replaced by a newer submission, a
    /// submission leaves the line and its turn never comes.
    #[tokio::test]
    async fn a_queued_submission_withdrawn_leaves_the_line() {
        let checking = Arc::new(Checking::default());
        let a = checking.begin("a", "s1", "c1", vec![]);
        let running = checking.start(&a).unwrap();
        checking.stopped_short(&a, "no room".into());
        let b = checking.begin("b", "s2", "c2", vec![]);
        assert!(checking.start(&b).is_none());
        let stop = checking
            .turn(&b, || Some("the session was paused".into()))
            .await;
        assert_eq!(
            stop.err(),
            Some(Stop::Cancelled("the session was paused".into()))
        );
        assert_eq!(checking.position(&b), None);
        a.finish(Err("superseded".into()), false);
        drop(running);
        assert_eq!(checking.turn(&a, never).await.err(), Some(Stop::Settled));
        assert_eq!(checking.position(&a), None);
    }
}
