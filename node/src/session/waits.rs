//! What a session is waiting on the operator for, and what it has not yet
//! been told about.
//!
//! An agent that asks the operator something gets `still_waiting` back
//! within one status call, and nothing makes it call again: it can end its
//! turn instead. A session whose turn ended used to be finished with — a plan
//! session closed on its plan — and whatever it was waiting for died with it.
//! So the node owns the wait rather than the agent: a session whose turn ends
//! with one of these still open is held, waiting on the operator, and each
//! decision is brought back to it as its next prompt. These are the waits a
//! session owns and a status tool reads:
//!
//! - an `ask_operator` question (`question_status`);
//! - a review it submitted, its required checks included (`review_status`),
//!   and a narrative report (`report_status`);
//! - a gated call it asked the operator to allow (`approval_status`).
//!
//! Not an egress ask: a refused connection raises one whether or not the
//! agent wants the host, and answering one changes nothing the agent has to
//! hear about. Not an issue draft either: drafting one says outright that it
//! does not pause the work.
//!
//! What has been told is kept in memory, per session: what a status call
//! returned decided, and what the node delivered. A session does not outlive
//! the process, so nothing here needs to.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use serde_json::{json, Value};

use crate::{
    mcp::checking::{Checking, Entry},
    store::{approvals::ApprovalRow, OperatorQuestionRow, ReviewRow, Store},
};

/// One thing a session waits on, open or decided.
#[derive(Debug, Clone)]
pub struct Wait {
    /// Names this wait's present outcome, so a review decided, resubmitted
    /// and decided again is two things to tell.
    pub key: String,
    /// The status tool that reads it.
    pub tool: &'static str,
    /// The id that tool takes.
    pub id: String,
    /// What it is about, in a line: the question, the review's title.
    pub about: String,
    /// What the status tool returns once it is decided; `None` while the
    /// operator has not decided it.
    pub decided: Option<Value>,
}

impl Wait {
    pub fn is_open(&self) -> bool {
        self.decided.is_none()
    }

    /// Whether it is still there once the node restarts. Checks run in the
    /// node's process and a restart interrupts them; the rest is in the store.
    pub fn survives_restart(&self) -> bool {
        !self.key.starts_with("checks:")
    }

    /// The handle a held session's log names it by.
    pub fn handle(&self) -> Value {
        json!({ "tool": self.tool, "id": self.id })
    }
}

/// What each session has been told, by wait key; and the reviewer's
/// verdict on each review as of its last change, so a settle pass does not
/// read it again for a review that has not changed.
#[derive(Default)]
pub struct Told(Mutex<HashSet<(String, String)>>, Verdicts);

/// A review's AI verdict on its latest revision, by review id, as of the
/// review's `updated_ms`.
#[derive(Default)]
struct Verdicts(Mutex<std::collections::HashMap<String, (i64, Option<Value>)>>);

impl Verdicts {
    fn of(&self, store: &Store, row: &ReviewRow) -> Option<Value> {
        if let Some((_, verdict)) = self
            .0
            .lock()
            .unwrap()
            .get(&row.id)
            .filter(|(updated, _)| *updated == row.updated_ms)
        {
            return verdict.clone();
        }
        // Read outside the lock: every session's settle pass shares it.
        let verdict = crate::mcp::review::current_verdict(store, row);
        self.0
            .lock()
            .unwrap()
            .insert(row.id.clone(), (row.updated_ms, verdict.clone()));
        verdict
    }
}

/// The waits of the sessions this node runs, and what each has been told.
#[derive(Clone)]
pub struct Waits {
    store: Arc<Store>,
    checking: Arc<Checking>,
    told: Arc<Told>,
}

pub fn question_key(row: &OperatorQuestionRow) -> String {
    format!("question:{}:{}", row.id, row.state)
}

pub fn review_key(row: &ReviewRow) -> String {
    format!("review:{}:{}:{}", row.id, row.state, row.updated_ms)
}

/// A reviewer's verdict on a review still waiting on the operator: told
/// once, by the verdict it is.
pub fn reviewer_key(review_id: &str, verdict: &Value) -> String {
    format!("reviewer:{review_id}:{}", verdict["at_ms"])
}

pub fn approval_key(row: &ApprovalRow) -> String {
    format!("approval:{}:{}", row.id, row.state)
}

pub fn checks_key(entry: &Entry) -> String {
    format!("checks:{}:{}", entry.handle, entry.started_ms)
}

/// What a status tool adds when it answers a session that it is still
/// waiting: that polling is not the only way to wait.
pub fn brought_back(in_session: bool) -> &'static str {
    if in_session {
        " Or end your turn: this session stays open while it waits, and the node brings \
         the decision back to you as your next prompt."
    } else {
        ""
    }
}

impl Waits {
    pub fn new(store: Arc<Store>, checking: Arc<Checking>, told: Arc<Told>) -> Self {
        Self {
            store,
            checking,
            told,
        }
    }

    /// Everything `session_id` waits on or has waited on, open or decided.
    pub fn of(&self, session_id: &str) -> Vec<Wait> {
        collect(&self.store, &self.checking, session_id, |row| {
            self.told.1.of(&self.store, row)
        })
    }

    /// What `session_id` still waits on the operator for.
    pub fn open(&self, session_id: &str) -> Vec<Wait> {
        open(&self.store, &self.checking, session_id)
    }

    /// Record that `session_id` has been told what `key` came to.
    pub fn tell(&self, session_id: &str, key: &str) {
        self.told
            .0
            .lock()
            .unwrap()
            .insert((session_id.to_string(), key.to_string()));
    }

    pub fn was_told(&self, session_id: &str, key: &str) -> bool {
        self.told
            .0
            .lock()
            .unwrap()
            .contains(&(session_id.to_string(), key.to_string()))
    }

    /// Let go of what was kept for reviews no longer open (`open` holds
    /// those that are): their reviewer verdicts are not told again, nor
    /// read for a settle pass, and an external caller's wait on one is over.
    pub fn retain_open(&self, open: &HashSet<&str>) {
        self.told.0.lock().unwrap().retain(|(session, key)| {
            if let Some(review) = session.strip_prefix("external:") {
                return open.contains(review);
            }
            match key.strip_prefix("reviewer:") {
                Some(rest) => open.contains(rest.rsplit_once(':').map_or(rest, |(id, _)| id)),
                None => true,
            }
        });
        self.told
            .1
             .0
            .lock()
            .unwrap()
            .retain(|review, _| open.contains(review.as_str()));
    }

    /// The session has ended; nothing more will be told to it.
    pub fn forget(&self, session_id: &str) {
        self.told
            .0
            .lock()
            .unwrap()
            .retain(|(session, _)| session != session_id);
        // Nor will its reviews' verdicts be read for it again. An external
        // caller is told as its one review (`external:{review_id}`).
        let reviews: Vec<String> = match session_id.strip_prefix("external:") {
            Some(review_id) => vec![review_id.to_string()],
            None => self
                .store
                .reviews_of_session(session_id)
                .unwrap_or_default()
                .into_iter()
                .map(|row| row.id)
                .collect(),
        };
        let mut memo = self.told.1 .0.lock().unwrap();
        for review in &reviews {
            memo.remove(review);
        }
    }
}

/// Everything `session_id` waits on or has waited on, open or decided, as
/// the store and the node's running checks say.
pub fn of(store: &Store, checking: &Checking, session_id: &str) -> Vec<Wait> {
    collect(store, checking, session_id, |row| {
        crate::mcp::review::current_verdict(store, row)
    })
}

/// [`of`], reading a review's AI verdict with `verdict`.
fn collect(
    store: &Store,
    checking: &Checking,
    session_id: &str,
    verdict: impl Fn(&ReviewRow) -> Option<Value>,
) -> Vec<Wait> {
    use crate::mcp::{approvals, operator, review};
    let mut waits = Vec::new();
    for row in store
        .operator_questions_of_session(session_id)
        .unwrap_or_default()
    {
        waits.push(Wait {
            key: question_key(&row),
            tool: operator::QUESTION_STATUS,
            id: row.id.clone(),
            about: row.prompt.clone(),
            decided: (row.state != "unanswered").then(|| operator::question_view(&row, true)),
        });
    }
    // Checks still running, or failed before their review was recorded.
    // Checks that passed became the review, which is read below.
    let mut running = HashSet::new();
    for entry in checking.of_session(session_id) {
        let decided = match entry.outcome() {
            None => None,
            Some(_) => match review::submit_failed_view(store, &entry) {
                Some(view) => Some(view),
                None => continue,
            },
        };
        if decided.is_none() {
            running.insert(entry.handle.clone());
        }
        waits.push(Wait {
            key: checks_key(&entry),
            tool: review::STATUS,
            id: entry.handle.clone(),
            about: format!("the required checks ({})", entry.commands.join(", ")),
            decided,
        });
    }
    for row in store.reviews_of_session(session_id).unwrap_or_default() {
        let report = row.kind == crate::store::reports::KIND;
        let decided = if report {
            review::decided_report_view(&row)
        } else {
            review::decided_review_view(&row)
        };
        // A revision whose checks are running is the checks' wait.
        if decided.is_some() && running.contains(&row.id) {
            continue;
        }
        // A reviewer's verdict on a review still waiting is told on its own:
        // the submitter can start on it before the operator decides.
        if let Some(verdict) = (decided.is_none() && !report && !running.contains(&row.id))
            .then(|| verdict(&row))
            .flatten()
        {
            waits.push(Wait {
                key: reviewer_key(&row.id, &verdict),
                tool: review::STATUS,
                id: row.id.clone(),
                about: format!("the AI reviewer's verdict on {}", row.title),
                decided: Some(json!({
                    "review_id": row.id,
                    "state": row.state,
                    "still_waiting": true,
                    "reviewer": {
                        "state": "done",
                        "model": verdict["model"],
                        "verdict": verdict["verdict"],
                        "summary": verdict["summary"],
                        "findings": verdict["findings"],
                    },
                    "message": review::reviewer_verdict_says(&verdict),
                })),
            });
        }
        waits.push(Wait {
            key: review_key(&row),
            tool: if report {
                review::REPORT_STATUS
            } else {
                review::STATUS
            },
            id: row.id.clone(),
            about: row.title.clone(),
            decided,
        });
    }
    for row in store.approvals_of_session(session_id).unwrap_or_default() {
        if row.tool == crate::store::approvals::EGRESS_TOOL {
            continue;
        }
        waits.push(Wait {
            key: approval_key(&row),
            tool: approvals::STATUS,
            id: row.id.clone(),
            about: row.title.clone(),
            decided: row.is_settled().then(|| approvals::view(&row)),
        });
    }
    waits
}

/// What `session_id` still waits on the operator for.
pub fn open(store: &Store, checking: &Checking, session_id: &str) -> Vec<Wait> {
    // A reviewer's verdict is never an open wait, so it is not read here.
    collect(store, checking, session_id, |_| None)
        .into_iter()
        .filter(Wait::is_open)
        .collect()
}

/// Each decided wait as its status tool returns it.
pub fn decisions(decided: &[Wait]) -> String {
    let mut text = String::new();
    for wait in decided {
        let view = wait.decided.as_ref().unwrap_or(&Value::Null);
        text.push_str(&format!(
            "\n{} `{}` — {}:\n\n```json\n{}\n```\n",
            wait.tool,
            wait.id,
            wait.about,
            serde_json::to_string_pretty(view).unwrap_or_default(),
        ));
    }
    text
}

/// The prompt that brings decisions back to a session: each one as its
/// status tool returns it, and what is still open.
pub fn prompt(decided: &[Wait], open: &[Wait]) -> String {
    let mut text = String::from(
        if decided.iter().all(|wait| wait.key.starts_with("reviewer:")) {
            "An AI reviewer has given its verdict on a review you are waiting on. It is \
             advisory: the operator has not decided yet. Each result below is what its status \
             tool returns now.\n"
        } else {
            "The operator has decided something you were waiting on. Each result below is \
             what its status tool returns now.\n"
        },
    );
    text.push_str(&decisions(decided));
    if open.is_empty() {
        text.push_str("\nNothing else is waiting on the operator. Carry on from here.");
    } else {
        text.push_str("\nStill waiting on the operator:\n\n");
        for wait in open {
            text.push_str(&format!("- {} `{}` — {}\n", wait.tool, wait.id, wait.about));
        }
        text.push_str(
            "\nCarry on from here. When those are decided they come back to you the same \
             way, so you may end your turn rather than poll.",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(id: &str, decided: Option<Value>) -> Wait {
        Wait {
            key: format!("question:{id}"),
            tool: "question_status",
            id: id.into(),
            about: "ship it?".into(),
            decided,
        }
    }

    #[test]
    fn the_prompt_carries_each_decision_and_names_what_is_still_open() {
        let text = prompt(
            &[wait(
                "q1",
                Some(json!({ "state": "answered", "answer": { "text": "yes" } })),
            )],
            &[wait("q2", None)],
        );
        assert!(text.contains("question_status `q1`"), "{text}");
        assert!(text.contains("\"text\": \"yes\""), "{text}");
        assert!(text.contains("Still waiting on the operator"), "{text}");
        assert!(text.contains("`q2`"), "{text}");

        let done = prompt(&[wait("q1", Some(json!({ "state": "cancelled" })))], &[]);
        assert!(done.contains("Nothing else is waiting"), "{done}");
    }

    #[test]
    fn only_a_session_is_told_it_may_end_its_turn() {
        assert!(brought_back(true).contains("end your turn"));
        assert!(brought_back(false).is_empty());
    }
}
