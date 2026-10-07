//! What a session came to, said from what was recorded.
//!
//! An outcome record answers five questions about a session: what changed,
//! what was verified, what still needs a decision, what is uncertain, and what
//! it cost. Every answer is read from rows other paths wrote: reviews and their
//! revisions, the checks the node ran, shown work, open permissions and
//! questions, the turn ledger. The agent's own words appear only as claims, and
//! a claim is backed only by a check the node ran on the commit the claim was
//! made at. However the summary reads, prose never becomes verification.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;

use crate::session::state::event_kind as ek;
use crate::store::{
    reports, shown::ShownWorkView, CheckRunRow, OperatorQuestionRow, PermissionRow, PublicationRow,
    ReviewRow, SessionRow, Store, StoreError,
};

pub const PASSED: &str = "passed";
pub const FAILED: &str = "failed";
pub const RUNNING: &str = "running";
pub const REUSED: &str = "reused";

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub session_id: String,
    pub channel: String,
    pub state: String,
    pub end_reason: Option<String>,
    /// The commit the session's latest code review stands at. None when it
    /// submitted none: then nothing it changed is under review.
    pub head_sha: Option<String>,
    pub changed: Changed,
    pub verified: Vec<Verification>,
    pub claims: Vec<Claim>,
    pub needs_decision: Vec<Pending>,
    pub uncertain: Vec<String>,
    pub cost: Cost,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Changed {
    pub reviews: Vec<ChangedReview>,
    /// Distinct paths across the session's code reviews.
    pub files: Vec<String>,
    pub added: i64,
    pub removed: i64,
    /// How many times the node saw the workspace change under the session.
    pub workspace_changes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedReview {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub state: String,
    pub head_sha: String,
    pub added: i64,
    pub removed: i64,
    pub files: usize,
}

/// One check the node ran. `passed` is the node's verdict, reused or not.
#[derive(Debug, Clone, Serialize)]
pub struct Verification {
    pub check_id: String,
    pub command: Option<String>,
    pub outcome: String,
    pub source_outcome: Option<String>,
    pub head_sha: Option<String>,
    pub passed: bool,
    pub failed: bool,
    /// True when it ran on the commit the session's latest review stands at.
    pub current: bool,
    pub finished_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Claim {
    /// `review`, `report` or `shown_work`.
    pub source: String,
    pub id: String,
    pub title: String,
    pub text: String,
    pub head_sha: Option<String>,
    /// Passing checks on the commit the claim was made at. Empty means the
    /// claim stands on its author's word alone.
    pub backed_by: Vec<String>,
    pub backed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    /// `permission`, `question`, `review` or `report`.
    pub kind: String,
    pub id: String,
    pub title: String,
    pub since_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Cost {
    pub tokens_used: i64,
    pub budget_tokens: i64,
    pub cost_usd: Option<f64>,
    pub gateway_tokens: i64,
    pub charged_tokens: i64,
    pub unmetered_turns: i64,
    pub mismatched_turns: i64,
}

/// Everything the record is derived from, read once.
pub struct Facts {
    pub session: SessionRow,
    pub reviews: Vec<ReviewRow>,
    /// Each review's latest head, by review id; the review's own when it has
    /// no revision.
    pub heads: Vec<(String, String)>,
    pub checks: Vec<CheckRunRow>,
    pub shown: Vec<ShownWorkView>,
    pub publications: Vec<PublicationRow>,
    pub permissions: Vec<PermissionRow>,
    pub questions: Vec<OperatorQuestionRow>,
    pub workspace_changes: usize,
    /// The latest uncertainty event's payload, if any.
    pub uncertainty: Option<Value>,
    pub interrupted_turns: usize,
    pub usage: Value,
}

pub fn read(store: &Store, session: SessionRow) -> Result<Facts, StoreError> {
    let id = session.id.clone();
    let reviews = store.reviews_of_session(&id)?;
    let mut heads = Vec::new();
    let mut shown = Vec::new();
    let mut publications = Vec::new();
    let mut seen = BTreeSet::new();
    for review in &reviews {
        let head = store
            .latest_review_revision(&review.id)?
            .map(|revision| revision.head_sha)
            .unwrap_or_else(|| review.head_sha.clone());
        heads.push((review.id.clone(), head));
        if review.kind != reports::KIND {
            for view in store.shown_work_for_review(review)? {
                if seen.insert(view.row.id.clone()) {
                    shown.push(view);
                }
            }
        }
        publications.extend(store.publications_for_review(&review.id)?);
    }
    // Work the session showed with no review to read it against.
    for view in store.shown_work_for_session(&id)? {
        if seen.insert(view.row.id.clone()) {
            shown.push(view);
        }
    }
    let uncertainty = store
        .session_events_of_kind(&id, ek::UNCERTAIN)?
        .pop()
        .map(|event| event.payload);
    let interrupted_turns = store
        .session_events_of_kind(&id, ek::TURN_END)?
        .iter()
        .filter(|event| {
            matches!(
                event.payload.get("stop_reason").and_then(Value::as_str),
                Some("cancelled" | "interrupted")
            )
        })
        .count();
    Ok(Facts {
        reviews,
        heads,
        checks: store.check_runs_for_session(&id)?,
        shown,
        publications,
        permissions: store.permissions_for_session(&id)?,
        questions: store.session_operator_questions(&id)?,
        workspace_changes: store
            .session_events_of_kind(&id, ek::WORKSPACE_CHANGED)?
            .len(),
        uncertainty,
        interrupted_turns,
        usage: crate::metrics::session_usage(store, &id),
        session,
    })
}

pub fn outcome(store: &Store, session: SessionRow) -> Result<Outcome, StoreError> {
    Ok(derive(&read(store, session)?))
}

fn passed(check: &CheckRunRow) -> bool {
    check.outcome == PASSED
        || (check.outcome == REUSED && check.source_outcome.as_deref() == Some(PASSED))
}

fn failed(check: &CheckRunRow) -> bool {
    check.outcome == FAILED
        || (check.outcome == REUSED && check.source_outcome.as_deref() == Some(FAILED))
}

/// The commit a check ran on, from its candidate's identity.
fn check_head(check: &CheckRunRow) -> Option<&str> {
    check
        .candidate_id
        .as_deref()
        .map(|id| id.split_once(':').map_or(id, |(sha, _)| sha))
}

fn file_paths(files: &str) -> Vec<String> {
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(files) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry {
            Value::String(path) => Some(path.clone()),
            other => other
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
        .collect()
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The passing checks on `head`, by id.
fn backing(checks: &[CheckRunRow], head: &str) -> Vec<String> {
    checks
        .iter()
        .filter(|check| passed(check) && check_head(check) == Some(head))
        .map(|check| check.id.clone())
        .collect()
}

pub fn derive(facts: &Facts) -> Outcome {
    let session = &facts.session;
    let head_of = |id: &str| {
        facts
            .heads
            .iter()
            .find(|(review, _)| review == id)
            .map(|(_, head)| head.clone())
    };
    let code: Vec<&ReviewRow> = facts
        .reviews
        .iter()
        .filter(|review| review.kind != reports::KIND)
        .collect();
    let head_sha = code.last().and_then(|review| head_of(&review.id));

    let mut changed = Changed {
        workspace_changes: facts.workspace_changes,
        ..Changed::default()
    };
    let mut files = BTreeSet::new();
    for review in &code {
        let paths = file_paths(&review.files);
        changed.added += review.added;
        changed.removed += review.removed;
        changed.reviews.push(ChangedReview {
            id: review.id.clone(),
            kind: review.kind.clone(),
            title: review.title.clone(),
            state: review.state.clone(),
            head_sha: head_of(&review.id).unwrap_or_else(|| review.head_sha.clone()),
            added: review.added,
            removed: review.removed,
            files: paths.len(),
        });
        files.extend(paths);
    }
    changed.files = files.into_iter().collect();

    let verified: Vec<Verification> = facts
        .checks
        .iter()
        .map(|check| Verification {
            check_id: check.id.clone(),
            command: check.command.clone(),
            outcome: check.outcome.clone(),
            source_outcome: check.source_outcome.clone(),
            head_sha: check_head(check).map(str::to_string),
            passed: passed(check),
            failed: failed(check),
            current: head_sha.is_some() && check_head(check) == head_sha.as_deref(),
            finished_ms: check.finished_ms,
        })
        .collect();

    let mut claims = Vec::new();
    for review in &facts.reviews {
        let report = review.kind == reports::KIND;
        // A report's head is a hash of its text, not a commit: nothing the
        // node runs can back it.
        let head =
            (!report).then(|| head_of(&review.id).unwrap_or_else(|| review.head_sha.clone()));
        let backed_by = head
            .as_deref()
            .map(|head| backing(&facts.checks, head))
            .unwrap_or_default();
        claims.push(Claim {
            source: if report { "report" } else { "review" }.into(),
            id: review.id.clone(),
            title: review.title.clone(),
            text: review.body.clone(),
            head_sha: head,
            backed: !backed_by.is_empty(),
            backed_by,
        });
    }
    for view in &facts.shown {
        let backed_by = backing(&facts.checks, &view.row.head_sha);
        claims.push(Claim {
            source: "shown_work".into(),
            id: view.row.id.clone(),
            title: view.row.title.clone(),
            text: view.row.markdown.clone(),
            head_sha: Some(view.row.head_sha.clone()),
            backed: !backed_by.is_empty(),
            backed_by,
        });
    }

    let mut needs_decision = Vec::new();
    for permission in facts.permissions.iter().filter(|p| p.state == "new") {
        needs_decision.push(Pending {
            kind: "permission".into(),
            id: permission.id.clone(),
            title: permission.title.clone(),
            since_ms: permission.created_ms,
        });
    }
    for question in facts.questions.iter().filter(|q| q.state == "unanswered") {
        needs_decision.push(Pending {
            kind: "question".into(),
            id: question.id.clone(),
            title: question.prompt.clone(),
            since_ms: question.created_ms,
        });
    }
    for review in facts
        .reviews
        .iter()
        .filter(|review| matches!(review.state.as_str(), "new" | "claimed"))
    {
        needs_decision.push(Pending {
            kind: if review.kind == reports::KIND {
                "report"
            } else {
                "review"
            }
            .into(),
            id: review.id.clone(),
            title: review.title.clone(),
            since_ms: review.created_ms,
        });
    }
    needs_decision.sort_by_key(|pending| pending.since_ms);

    let mut uncertain = Vec::new();
    if let Some(payload) = &facts.uncertainty {
        if let Some(reason) = payload.get("reason").and_then(Value::as_str) {
            uncertain.push(format!(
                "Whether a dispatched prompt was sent is unknown: {reason}"
            ));
        }
    }
    let usage = &facts.usage;
    let count = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    let unmetered = count("unmetered_turns");
    let mismatched = count("mismatched_turns");
    if unmetered > 0 {
        uncertain.push(format!(
            "{unmetered} turn(s) were not metered by the gateway; their cost is the harness's own report"
        ));
    }
    if mismatched > 0 {
        uncertain.push(format!(
            "{mismatched} turn(s) where the harness and the gateway disagree on usage"
        ));
    }
    if facts.interrupted_turns > 0 {
        uncertain.push(format!(
            "{} turn(s) ended before the agent finished",
            facts.interrupted_turns
        ));
    }
    if let Some(head) = &head_sha {
        let current: Vec<&Verification> = verified.iter().filter(|v| v.current).collect();
        if current.is_empty() {
            if verified.is_empty() {
                uncertain.push(format!("No check has run on {}", short(head)));
            } else {
                uncertain.push(format!(
                    "Checks ran only on earlier commits, not on {}",
                    short(head)
                ));
            }
        }
        for v in current.iter().filter(|v| !v.passed && !v.failed) {
            uncertain.push(format!(
                "{} is {} on {}",
                v.command.as_deref().unwrap_or("A check"),
                v.outcome,
                short(head)
            ));
        }
    }
    for view in facts.shown.iter().filter(|view| view.stale) {
        uncertain.push(format!(
            "Shown work \"{}\" is stale: {}",
            view.row.title,
            view.stale_reason
                .as_deref()
                .unwrap_or("it no longer matches")
        ));
    }
    for publication in facts.publications.iter().filter(|p| p.state == "uncertain") {
        uncertain.push(format!(
            "Publishing to {} may or may not have reached the forge{}",
            publication.project,
            publication
                .note
                .as_deref()
                .map(|note| format!(": {note}"))
                .unwrap_or_default()
        ));
    }
    if let Some(error) = &session.last_error {
        uncertain.push(format!("The session's last error: {error}"));
    }

    let cost = Cost {
        tokens_used: session.tokens_used,
        budget_tokens: session.budget_tokens,
        cost_usd: session.cost_usd,
        gateway_tokens: count("gateway_tokens"),
        charged_tokens: count("charged_tokens"),
        unmetered_turns: unmetered,
        mismatched_turns: mismatched,
    };

    Outcome {
        session_id: session.id.clone(),
        channel: session.channel.clone(),
        state: session.state.clone(),
        end_reason: session.end_reason.clone(),
        head_sha,
        changed,
        verified,
        claims,
        needs_decision,
        uncertain,
        cost,
    }
}

/// Whether any check on the current head failed: what the operator must see
/// first, before any claim.
pub fn failing(outcome: &Outcome) -> bool {
    outcome.verified.iter().any(|v| v.current && v.failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionRow {
        serde_json::from_value(serde_json::json!({
            "id": "s1", "node_id": "n1", "channel": "personal", "repo_path": "/r",
            "branch": "b", "harness_id": "fake", "harness_version": "1", "model": "m",
            "phase": "execute", "budget_tokens": 1000, "tokens_used": 40,
            "state": "closed", "turn_active": 0, "created_ms": 0, "updated_ms": 0,
        }))
        .unwrap()
    }

    fn review(id: &str, kind: &str, head: &str, state: &str) -> ReviewRow {
        serde_json::from_value(serde_json::json!({
            "id": id, "session_id": "s1", "node_id": "n1", "channel": "personal",
            "kind": kind, "title": format!("{id} title"), "body": "all tests pass",
            "provider": "github", "target": "{}", "diff": "",
            "files": r#"[{"path":"a.rs","blob":"x"},{"path":"b.rs","blob":"y"}]"#,
            "head_sha": head, "base_ref": "main", "added": 3, "removed": 1,
            "state": state, "created_ms": 1, "created_mono_ms": 0, "updated_ms": 1,
        }))
        .unwrap()
    }

    fn check(id: &str, head: &str, outcome: &str) -> CheckRunRow {
        serde_json::from_value(serde_json::json!({
            "id": id, "candidate_id": format!("{head}:personal"), "session_id": "s1",
            "command": "just check", "definition_json": "{}", "outcome": outcome,
            "log": "", "started_ms": 0, "finished_ms": 1, "metadata_json": "{}",
        }))
        .unwrap()
    }

    fn facts(reviews: Vec<ReviewRow>, checks: Vec<CheckRunRow>) -> Facts {
        Facts {
            heads: reviews
                .iter()
                .map(|r| (r.id.clone(), r.head_sha.clone()))
                .collect(),
            reviews,
            checks,
            session: session(),
            shown: Vec::new(),
            publications: Vec::new(),
            permissions: Vec::new(),
            questions: Vec::new(),
            workspace_changes: 2,
            uncertainty: None,
            interrupted_turns: 0,
            usage: serde_json::json!({ "gateway_tokens": 40, "unmetered_turns": 1 }),
        }
    }

    #[test]
    fn a_claim_is_backed_only_by_a_passing_check_on_its_own_commit() {
        let out = derive(&facts(
            vec![
                review("rv1", "pr", "aaa", "approved"),
                review("rp", "report", "hash", "new"),
            ],
            vec![check("c1", "old", PASSED), check("c2", "aaa", FAILED)],
        ));
        assert_eq!(out.head_sha.as_deref(), Some("aaa"));
        let claim = out.claims.iter().find(|c| c.id == "rv1").unwrap();
        assert!(!claim.backed, "a pass on another commit does not back it");
        let report = out.claims.iter().find(|c| c.id == "rp").unwrap();
        assert!(
            !report.backed && report.head_sha.is_none(),
            "a report is never backed"
        );
        assert!(failing(&out));
        assert_eq!(out.changed.files, vec!["a.rs", "b.rs"]);
        assert_eq!(out.changed.workspace_changes, 2);
        assert_eq!(
            out.needs_decision
                .iter()
                .map(|p| p.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["report"]
        );

        let out = derive(&facts(
            vec![review("rv1", "pr", "aaa", "new")],
            vec![check("c3", "aaa", PASSED)],
        ));
        let claim = &out.claims[0];
        assert!(claim.backed);
        assert_eq!(claim.backed_by, vec!["c3"]);
        assert!(!failing(&out));
    }

    #[test]
    fn what_was_not_established_is_said() {
        let out = derive(&facts(
            vec![review("rv1", "pr", "bbb", "new")],
            vec![check("c1", "aaa", PASSED), check("c2", "bbb", RUNNING)],
        ));
        let text = out.uncertain.join("\n");
        assert!(text.contains("not metered"), "{text}");
        assert!(text.contains("just check is running on bbb"), "{text}");

        let out = derive(&facts(
            vec![review("rv1", "pr", "bbb", "new")],
            vec![check("c1", "aaa", PASSED)],
        ));
        assert!(out
            .uncertain
            .iter()
            .any(|u| u.contains("only on earlier commits")));

        let out = derive(&facts(vec![review("rv1", "pr", "bbb", "new")], Vec::new()));
        assert!(out.uncertain.iter().any(|u| u.contains("No check has run")));
        assert_eq!(out.cost.gateway_tokens, 40);
        assert_eq!(out.cost.tokens_used, 40);
    }
}
