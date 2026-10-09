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

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde_json::Value;

/// What a submission came to: the response `submit_review` would have given
/// had it waited, or the refusal it would have returned.
pub type Outcome = Result<Value, String>;

/// How long a settled entry is kept for `review_status` to report. A handle
/// that is polled after this reads as the review it became, or as unknown.
const KEEP_SETTLED_MS: i64 = 60 * 60 * 1000;

/// The node's running and recently settled check submissions, by handle.
#[derive(Default)]
pub struct Checking {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
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

    /// Wait for the outcome until `deadline`; `None` when the checks are
    /// still running then.
    pub async fn wait(&self, deadline: tokio::time::Instant) -> Option<Outcome> {
        let mut settled = self.settled.subscribe();
        let waited = tokio::time::timeout_at(deadline, settled.wait_for(Option::is_some)).await;
        match waited {
            Ok(Ok(settled)) => settled.as_ref().map(|settled| settled.outcome.clone()),
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
}
