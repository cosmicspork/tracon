//! The continuation view: one piece of work, whatever shape it took.
//!
//! A work item's attempts are the sessions that held it; a plain session's
//! are its lineage — what it continued and what continued it. Either way the
//! operator coming back to the work needs the same things in one place: what
//! it was for, what was decided, what was tried and how each try ended, what
//! is in the way, what to do next, where the work lives, and what it showed.
//! Everything here is read from recorded state. The next action is derived
//! from those records by fixed rules, never written by a model, so it cannot
//! claim more than the records say.

use serde::Serialize;
use tracon_sync::work::{Blocker, Readiness, WorkItem};

use crate::session::state::{event_kind as ek, EndReason, SessionState};
use crate::store::{continuation::Decided, SessionRow, Store, StoreError};

type Result<T> = std::result::Result<T, StoreError>;

/// What the view is of.
#[derive(Debug, Clone)]
pub enum Subject {
    Item(String),
    Session(String),
}

#[derive(Debug, Serialize)]
pub struct Continuation {
    /// `item` or `session`; `id` is the item's, or the lineage's first session.
    pub kind: &'static str,
    pub id: String,
    pub channel: String,
    pub intent: Intent,
    pub attempts: Vec<Attempt>,
    pub blockers: Vec<String>,
    pub next: NextAction,
    pub workspace: Option<Workspace>,
    pub decisions: Decisions,
    pub evidence: Evidence,
    /// What the operator may do from here.
    pub actions: Actions,
}

#[derive(Debug, Serialize)]
pub struct Intent {
    pub title: String,
    pub body: String,
    /// `item` (its title and body) or `prompt` (the first thing the operator
    /// asked the first session), or `none` when neither was recorded.
    pub source: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Attempt {
    pub id: String,
    pub phase: String,
    pub model: String,
    pub harness: String,
    pub state: String,
    pub end_reason: Option<String>,
    pub last_error: Option<String>,
    pub tokens_used: i64,
    pub created_ms: i64,
    /// Lineage: the attempt this one carries on, and the one it was made from.
    pub continued_from: Option<String>,
    pub parent_session: Option<String>,
    pub archived: bool,
}

#[derive(Debug, Serialize)]
pub struct Workspace {
    /// The retained workspace the latest attempt worked in.
    pub id: String,
    pub branch: String,
    pub session_id: String,
}

#[derive(Debug, Serialize)]
pub struct Decisions {
    /// The item's plan document, when one was written.
    pub plan: Option<String>,
    pub brief: Option<String>,
    pub answered: Vec<Decided>,
}

#[derive(Debug, Serialize)]
pub struct Evidence {
    pub reviews: Vec<ReviewSummary>,
    pub shown: Vec<ShownSummary>,
}

#[derive(Debug, Serialize)]
pub struct ReviewSummary {
    pub id: String,
    pub session_id: Option<String>,
    pub title: String,
    pub state: String,
    pub verdict_reason: Option<String>,
    pub publish_result: Option<String>,
    pub head_sha: String,
    pub created_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct ShownSummary {
    pub id: String,
    pub session_id: Option<String>,
    pub title: String,
    pub head_sha: String,
    pub stale: bool,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NextAction {
    /// `done`, `start`, `watch`, `answer`, `resume`, `unblock`, `execute`,
    /// `continue` or `change_approach`.
    pub kind: &'static str,
    pub text: String,
    /// The attempt the action is about, when it is about one.
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Actions {
    /// The ended attempt a continuation or a change of approach carries on
    /// from; `None` while an attempt is live or nothing has run.
    pub continue_from: Option<String>,
    pub abandon: bool,
}

/// What the next action is derived from: the subject's own state and the
/// latest attempt's.
#[derive(Debug, Clone, Default)]
pub struct Situation<'a> {
    pub item_closed: bool,
    pub item_blockers: Vec<String>,
    pub item_planned: bool,
    pub is_item: bool,
    pub latest: Option<Latest<'a>>,
    /// Whether the latest attempt has already been continued.
    pub continued: bool,
    pub open_permissions: i64,
    pub open_questions: i64,
    pub abandoned: bool,
}

/// The latest attempt, as far as the next action reads it.
#[derive(Debug, Clone, Copy)]
pub struct Latest<'a> {
    pub id: &'a str,
    pub state: &'a str,
    pub end_reason: Option<&'a str>,
    pub phase: &'a str,
    pub last_error: Option<&'a str>,
}

impl<'a> From<&'a SessionRow> for Latest<'a> {
    fn from(r: &'a SessionRow) -> Self {
        Self {
            id: &r.id,
            state: &r.state,
            end_reason: r.end_reason.as_deref(),
            phase: &r.phase,
            last_error: r.last_error.as_deref(),
        }
    }
}

fn short(id: &str) -> &str {
    &id[..8.min(id.len())]
}

/// The one thing to do next, by fixed rules over recorded state.
pub fn next_action(s: &Situation<'_>) -> NextAction {
    let at = |kind, text: String, row: Option<Latest<'_>>| NextAction {
        kind,
        text,
        session_id: row.map(|r| r.id.to_string()),
    };
    if s.item_closed {
        return at(
            "done",
            "The item is closed; nothing is left to do.".into(),
            None,
        );
    }
    if s.abandoned {
        return at("done", "Abandoned; nothing is left to do.".into(), None);
    }
    let Some(latest) = s.latest else {
        if !s.item_blockers.is_empty() {
            return at(
                "unblock",
                format!("Waits on {}.", s.item_blockers.join(", ")),
                None,
            );
        }
        return at(
            "start",
            if s.is_item && !s.item_planned {
                "Nothing has run yet: plan it, or execute it if it needs no plan.".into()
            } else {
                "Nothing has run yet: start a session on it.".into()
            },
            None,
        );
    };
    let state = SessionState::from_stored(latest.state);
    let who = short(latest.id);
    if !state.is_terminal() {
        if s.open_permissions + s.open_questions > 0 {
            let n = s.open_permissions + s.open_questions;
            return at(
                "answer",
                format!(
                    "Session {who} is waiting on you: {n} request{} to answer.",
                    if n == 1 { "" } else { "s" }
                ),
                Some(latest),
            );
        }
        if state == SessionState::Paused {
            return at(
                "resume",
                match latest.last_error {
                    Some(why) => format!("Session {who} is paused: {why} Resume it or stop it."),
                    None => format!("Session {who} is paused. Resume it or stop it."),
                },
                Some(latest),
            );
        }
        return at(
            "watch",
            format!("Session {who} is working on it."),
            Some(latest),
        );
    }
    if s.continued {
        // Only reachable for a lineage whose newest row is not its tip, which
        // ordering prevents; said plainly rather than guessed at.
        return at(
            "watch",
            format!("Session {who} has already been carried on."),
            Some(latest),
        );
    }
    if !s.item_blockers.is_empty() {
        return at(
            "unblock",
            format!("Waits on {}.", s.item_blockers.join(", ")),
            Some(latest),
        );
    }
    let reason = latest.end_reason.unwrap_or_default();
    let is = |r: EndReason| reason == r.as_str();
    if is(EndReason::PhaseDone) && latest.phase == "plan" {
        return at(
            "execute",
            "The plan is written: execute it.".into(),
            Some(latest),
        );
    }
    if is(EndReason::NodeRestart) || is(EndReason::Detached) || is(EndReason::KilledUser) {
        return at(
            "continue",
            format!(
                "Session {who} {}; continue from its workspace.",
                match reason {
                    "node_restart" => "was cut off by a node restart",
                    "detached" => "was detached",
                    _ => "was stopped",
                }
            ),
            Some(latest),
        );
    }
    let why = match reason {
        "budget" => "spent its budget".to_string(),
        "incompatible" => "ran on a harness the node could not drive".to_string(),
        "harness_exit" => "lost its harness".to_string(),
        _ => match latest.last_error {
            Some(e) => format!("failed: {e}"),
            None => "ended without finishing".to_string(),
        },
    };
    at(
        "change_approach",
        format!("Session {who} {why}. Continue with a different approach, or abandon it."),
        Some(latest),
    )
}

/// Assemble the view. `None` when the subject does not exist here.
pub fn view(store: &Store, subject: &Subject) -> Result<Option<Continuation>> {
    let (kind, id, channel, item, attempts) = match subject {
        Subject::Item(id) => {
            let Some(item) = store.work_get(id)? else {
                return Ok(None);
            };
            let attempts = store.sessions_of_work_item(id)?;
            (
                "item",
                id.clone(),
                item.channel.clone(),
                Some(item),
                attempts,
            )
        }
        Subject::Session(id) => {
            let attempts = store.session_lineage(id)?;
            let Some(first) = attempts.first() else {
                return Ok(None);
            };
            // A lineage that belongs to an item is that item's work.
            if let Some(item_id) = attempts.iter().find_map(|a| a.work_item_id.clone()) {
                return view(store, &Subject::Item(item_id));
            }
            (
                "session",
                first.id.clone(),
                first.channel.clone(),
                None,
                attempts,
            )
        }
    };
    let ids: Vec<String> = attempts.iter().map(|a| a.id.clone()).collect();
    let latest = attempts.last();

    let intent = match &item {
        Some(item) => Intent {
            title: item.title.clone(),
            body: item.body.clone(),
            source: "item",
        },
        None => match attempts
            .first()
            .and_then(|a| {
                store
                    .first_event_payload(&a.id, ek::USER_PROMPT)
                    .ok()
                    .flatten()
            })
            .and_then(|p| p["text"].as_str().map(str::to_string))
        {
            Some(text) => Intent {
                title: text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(120)
                    .collect(),
                body: text,
                source: "prompt",
            },
            None => Intent {
                title: String::new(),
                body: String::new(),
                source: "none",
            },
        },
    };

    let item_blockers = match &item {
        Some(item) => blockers_of(store, item)?,
        None => Vec::new(),
    };
    let open_permissions = store.open_permissions_of_sessions(&ids)?;
    let open_questions = store.open_questions_of_sessions(&ids)?;
    let mut blockers = item_blockers.clone();
    if let Some(l) = latest.filter(|l| !SessionState::from_stored(&l.state).is_terminal()) {
        if open_permissions + open_questions > 0 {
            blockers.push(format!(
                "session {} waits on {} answer{}",
                short(&l.id),
                open_permissions + open_questions,
                if open_permissions + open_questions == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if l.state == SessionState::Paused.as_str() {
            blockers.push(format!("session {} is paused", short(&l.id)));
        }
    }

    // Abandoned is what the operator said, not what archiving implies: a
    // lineage put away with everything else that ended is not abandoned.
    let abandoned = item.is_none()
        && match latest {
            Some(l) => store.first_event_payload(&l.id, ek::ABANDONED)?.is_some(),
            None => false,
        };
    let item_closed = item
        .as_ref()
        .is_some_and(|i| i.state == tracon_sync::work::CLOSED);
    let continued = latest.is_some_and(|l| {
        attempts
            .iter()
            .any(|a| a.continued_from.as_deref() == Some(l.id.as_str()))
    });
    let next = next_action(&Situation {
        item_closed,
        item_blockers,
        item_planned: item.as_ref().is_some_and(|i| i.phase_plan_slug.is_some()),
        is_item: item.is_some(),
        latest: latest.map(Latest::from),
        continued,
        open_permissions,
        open_questions,
        abandoned,
    });

    let workspace = latest.map(|l| Workspace {
        id: l
            .repo_path
            .strip_prefix("workspace://")
            .unwrap_or(&l.id)
            .to_string(),
        branch: l.branch.clone(),
        session_id: l.id.clone(),
    });

    let reviews = store
        .reviews_of_sessions(&ids)?
        .into_iter()
        .map(|r| ReviewSummary {
            title: r.approved_title().to_string(),
            id: r.id,
            session_id: r.session_id,
            state: r.state,
            verdict_reason: r.verdict_reason,
            publish_result: r.publish_result,
            head_sha: r.head_sha,
            created_ms: r.created_ms,
        })
        .collect();
    let mut shown = Vec::new();
    for id in &ids {
        for w in store.shown_work_for_session(id)? {
            shown.push(ShownSummary {
                id: w.row.id,
                session_id: w.row.session_id,
                title: w.row.title,
                head_sha: w.row.head_sha,
                stale: w.stale,
                created_ms: w.row.created_ms,
            });
        }
    }

    let terminal = latest.is_some_and(|l| SessionState::from_stored(&l.state).is_terminal());
    let continuable = latest.filter(|l| {
        terminal
            && !continued
            && !item_closed
            && !abandoned
            && l.phase != "review"
            && l.harness_id != crate::session::external::HARNESS_ID
    });
    let actions = Actions {
        continue_from: continuable.map(|l| l.id.clone()),
        abandon: !item_closed && !abandoned && (item.is_some() || !attempts.is_empty()),
    };

    Ok(Some(Continuation {
        kind,
        id,
        channel,
        intent,
        attempts: attempts
            .iter()
            .map(|a| Attempt {
                id: a.id.clone(),
                phase: a.phase.clone(),
                model: a.model.clone(),
                harness: a.harness_id.clone(),
                state: a.state.clone(),
                end_reason: a.end_reason.clone(),
                last_error: a.last_error.clone(),
                tokens_used: a.tokens_used,
                created_ms: a.created_ms,
                continued_from: a.continued_from.clone(),
                parent_session: a.parent_session.clone(),
                archived: a.archived_ms.is_some(),
            })
            .collect(),
        blockers,
        next,
        workspace,
        decisions: Decisions {
            plan: item.as_ref().and_then(|i| i.phase_plan_slug.clone()),
            brief: item.as_ref().and_then(|i| i.brief_slug.clone()),
            answered: store.decisions_of_sessions(&ids)?,
        },
        evidence: Evidence { reviews, shown },
        actions,
    }))
}

/// The item's blockers by title, as the operator knows them.
fn blockers_of(store: &Store, item: &WorkItem) -> Result<Vec<String>> {
    let view = store
        .work_status(&item.channel, None)?
        .into_iter()
        .find(|v| v.item.id == item.id);
    let Some(view) = view else {
        return Ok(Vec::new());
    };
    let Readiness::Blocked { by } = view.readiness else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for b in by {
        out.push(match b {
            Blocker::Open { id } => match store.work_get(&id)? {
                Some(dep) => format!("“{}” ({})", dep.title, short(&id)),
                None => format!("item {}", short(&id)),
            },
            Blocker::Unknown { id } => format!("item {} not seen on this node", short(&id)),
            Blocker::Cycle => "a dependency cycle".into(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(
        state: &'a str,
        end: Option<&'a str>,
        phase: &'a str,
        err: Option<&'a str>,
    ) -> Latest<'a> {
        Latest {
            id: "0123456789ab",
            state,
            end_reason: end,
            phase,
            last_error: err,
        }
    }

    fn next(s: Situation<'_>) -> NextAction {
        next_action(&s)
    }

    #[test]
    fn the_next_action_follows_the_records() {
        assert_eq!(
            next(Situation {
                is_item: true,
                ..Default::default()
            })
            .kind,
            "start"
        );
        assert_eq!(
            next(Situation {
                item_closed: true,
                ..Default::default()
            })
            .kind,
            "done"
        );
        assert_eq!(
            next(Situation {
                item_blockers: vec!["“x” (1)".into()],
                ..Default::default()
            })
            .text,
            "Waits on “x” (1)."
        );
        let running = row("running", None, "execute", None);
        assert_eq!(
            next(Situation {
                latest: Some(running),
                ..Default::default()
            })
            .kind,
            "watch"
        );
        let n = next(Situation {
            latest: Some(running),
            open_permissions: 2,
            ..Default::default()
        });
        assert_eq!(n.kind, "answer");
        assert!(n.text.contains("2 requests"), "{}", n.text);
        let paused = row("paused", None, "execute", Some("limit reached."));
        assert!(next(Situation {
            latest: Some(paused),
            ..Default::default()
        })
        .text
        .contains("limit reached."));
        let planned = row("closed", Some("phase_done"), "plan", None);
        assert_eq!(
            next(Situation {
                latest: Some(planned),
                ..Default::default()
            })
            .kind,
            "execute"
        );
        let restarted = row("closed", Some("node_restart"), "execute", None);
        assert_eq!(
            next(Situation {
                latest: Some(restarted),
                ..Default::default()
            })
            .kind,
            "continue"
        );
        let failed = row(
            "failed",
            Some("error"),
            "execute",
            Some("tests never passed"),
        );
        let n = next(Situation {
            latest: Some(failed),
            ..Default::default()
        });
        assert_eq!(n.kind, "change_approach");
        assert!(n.text.contains("failed: tests never passed"), "{}", n.text);
        assert_eq!(n.session_id.as_deref(), Some("0123456789ab"));
        assert_eq!(
            next(Situation {
                latest: Some(failed),
                abandoned: true,
                ..Default::default()
            })
            .kind,
            "done"
        );
    }
}

/// What an operator asks for when they carry the work on.
#[derive(Debug, Default, serde::Deserialize)]
pub struct CarryOn {
    /// The ended attempt to carry on from.
    pub session_id: String,
    /// A changed approach, in the operator's words: handed to the new
    /// attempt as the first thing it reads after the handoff. Absent: a
    /// plain continuation.
    #[serde(default)]
    pub approach: Option<String>,
    /// Another model or harness for the new attempt. Absent: the old
    /// attempt's model, and its harness when the model is unchanged.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
}

/// Continue the work from an ended attempt, or continue it with a changed
/// approach: a new session on the same workspace, branch and item, with the
/// lineage on its row and a handoff that says what it carries on and what it
/// does not inherit. Once per attempt, like every continuation.
pub async fn carry_on(
    manager: &crate::session::Manager,
    ask: CarryOn,
) -> std::result::Result<SessionRow, crate::session::SessionError> {
    use crate::session::{external, NewSession, Phase, SessionError};
    let store = manager.store();
    let old = store
        .get_session(&ask.session_id)?
        .ok_or(SessionError::NotFound)?;
    if !SessionState::from_stored(&old.state).is_terminal() {
        return Err(SessionError::Rejected(
            "this attempt is still live; resume or stop it first".into(),
        ));
    }
    if old.harness_id == external::HARNESS_ID {
        return Err(SessionError::Rejected(
            "an external agent's work runs outside this node; start it again where it runs".into(),
        ));
    }
    if old.phase == Phase::Review.as_str() {
        return Err(SessionError::Rejected(
            "a review session is started by its review, not continued; resubmit the review".into(),
        ));
    }
    if let Some(next) = store
        .session_lineage(&old.id)?
        .into_iter()
        .find(|s| s.continued_from.as_deref() == Some(old.id.as_str()))
    {
        return Err(SessionError::Rejected(format!(
            "session {} already continues this one",
            next.id
        )));
    }
    let approach = ask
        .approach
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty());
    let model = ask
        .model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    let harness = ask
        .harness
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| {
            model
                .is_none()
                .then(|| old.harness_id.clone())
                .filter(|h| crate::adapter::KNOWN.contains(&h.as_str()))
        });
    let workspace_id = old
        .repo_path
        .strip_prefix("workspace://")
        .unwrap_or(&old.id)
        .to_string();
    let spec = NewSession {
        channel: old.channel.clone(),
        repo_path: old.repo_path.clone(),
        branch: Some(old.branch.clone()),
        work_item_id: old.work_item_id.clone(),
        model: model.unwrap_or_else(|| old.model.clone()),
        budget_tokens: Some(old.budget_tokens),
        initial_prompt: Some(handoff(&old, approach.as_deref())),
        node_id: Some(old.node_id.clone()),
        phase: if old.phase == "plan" {
            Phase::Plan
        } else {
            Phase::Execute
        },
        review_id: None,
        base_sha: None,
        workspace_id: Some(workspace_id),
        parent_session: Some(old.id.clone()),
        continued_from: Some(old.id.clone()),
        harness,
        // The policy the old attempt ran under carries over with the work.
        on_exhaustion: store.exhaustion(&old.id)?.map(|row| row.choice()),
    };
    manager.create(spec).await
}

/// The new attempt's first prompt: what it continues, how that ended, that
/// it inherits the workspace and nothing else, and the operator's changed
/// approach when there is one.
fn handoff(old: &SessionRow, approach: Option<&str>) -> String {
    let ended = match old.end_reason.as_deref() {
        Some("node_restart") => "was cut off when its node restarted".to_string(),
        Some("provider_exhausted") => "stopped when its provider was exhausted".to_string(),
        Some("phase_done") => "finished its phase".to_string(),
        Some("budget") => "spent its budget".to_string(),
        Some("killed_user") => "was stopped by the operator".to_string(),
        Some(other) => match old.last_error.as_deref() {
            Some(e) => format!("ended ({other}): {e}"),
            None => format!("ended ({other})"),
        },
        None => "ended".to_string(),
    };
    let mut note = format!(
        "This session continues session {}, which {ended}. You inherit none of its \
         context: not its conversation, not its plan, not what it had already tried.\n\n\
         What you do have is its workspace, exactly as it left it, on branch `{}`.\n\n\
         Start by reading the workspace — the diff against the branch point, and any \
         notes left in it — and say what you find before changing anything. If the \
         earlier session's transcript matters, it is readable in the interface under \
         that id; ask for what you need from it rather than guessing.",
        old.id, old.branch,
    );
    if let Some(item) = old.work_item_id.as_deref() {
        note.push_str(&format!(
            "\n\nIt was working on item {item}, which is still open."
        ));
    }
    if let Some(approach) = approach {
        note.push_str(&format!(
            "\n\nThe operator has changed the approach. Work this way instead of \
             repeating what the earlier session did:\n\n{approach}"
        ));
    }
    note
}

/// Abandon the work: close the item (ending the attempt that holds it), or,
/// for a plain lineage, stop any live attempt and put every attempt away.
/// The reason is recorded on the latest attempt's log. Nothing is deleted:
/// an abandoned item can be reopened and an archived session restored.
pub async fn abandon(
    manager: &crate::session::Manager,
    subject: &Subject,
    reason: &str,
) -> std::result::Result<(), crate::session::SessionError> {
    use crate::session::SessionError;
    let store = manager.store();
    let reason = reason.trim();
    let item_id = match subject {
        Subject::Item(id) => Some(id.clone()),
        Subject::Session(id) => store
            .session_lineage(id)?
            .into_iter()
            .find_map(|a| a.work_item_id),
    };
    let attempts = match (&item_id, subject) {
        (Some(item), _) => store.sessions_of_work_item(item)?,
        (None, Subject::Session(id)) => store.session_lineage(id)?,
        (None, Subject::Item(_)) => Vec::new(),
    };
    let summary = if reason.is_empty() {
        "abandoned by the operator".to_string()
    } else {
        format!("abandoned by the operator: {reason}")
    };
    if let Some(item_id) = item_id {
        let item = store.work_get(&item_id)?.ok_or(SessionError::NotFound)?;
        if item.state == tracon_sync::work::CLOSED {
            return Err(SessionError::Rejected("the item is already closed".into()));
        }
        let holder = store.session_holding(&item_id)?;
        crate::corpus::work::close(
            store,
            manager.bus(),
            manager.node_id(),
            &item_id,
            holder.as_ref().map(|h| h.id.as_str()),
        )
        .map_err(|e| SessionError::Rejected(e.to_string()))?;
        if let Some(h) = holder {
            manager.item_closed(&h.id, &summary).await;
        }
    } else {
        if attempts.is_empty() {
            return Err(SessionError::NotFound);
        }
        for a in &attempts {
            if !SessionState::from_stored(&a.state).is_terminal() {
                manager.stop(&a.id).await?;
            }
        }
        let now = crate::store::now_ms();
        for a in &attempts {
            if let Some(row) = store.set_session_archived(&a.id, Some(now))? {
                manager
                    .bus()
                    .publish_untapped(crate::stream::Frame::Session(Box::new(row)));
            }
        }
    }
    if let Some(latest) = attempts.last() {
        manager.record_event(
            &latest.id,
            ek::ABANDONED,
            serde_json::json!({ "reason": reason, "summary": summary }),
        );
    }
    Ok(())
}
