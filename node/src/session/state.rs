//! Session and permission states, as `DESIGN.md` §3 defines them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Running,
    WaitingOnYou,
    /// Defined here because the schema and the interface both name it; nothing
    /// in this slice runs deterministic checks between turns yet.
    WaitingOnCheck,
    Closed,
    KilledBudget,
    Failed,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::WaitingOnYou => "waiting_on_you",
            Self::WaitingOnCheck => "waiting_on_check",
            Self::Closed => "closed",
            Self::KilledBudget => "killed_budget",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::KilledBudget | Self::Failed)
    }

    /// Prompts are only accepted while the harness is idle and waiting on the
    /// operator for input rather than for a decision.
    pub fn accepts_prompt(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    KilledUser,
    Budget,
    HarnessExit,
    ItemClose,
    /// The phase's artifact landed (a plan was written, a review verdict
    /// given); the session has nothing more to do.
    PhaseDone,
    Error,
}

impl EndReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::KilledUser => "killed_user",
            Self::Budget => "budget",
            Self::HarnessExit => "harness_exit",
            Self::ItemClose => "item_close",
            Self::PhaseDone => "phase_done",
            Self::Error => "error",
        }
    }
}

/// Event kinds written to the log. The SPA groups on these.
pub mod event_kind {
    pub const SESSION_STARTED: &str = "session_started";
    pub const WORKTREE: &str = "worktree";
    /// What the session was told at start, so the transcript shows it.
    pub const ORIENTATION: &str = "orientation";
    pub const STATE: &str = "state";
    pub const USER_PROMPT: &str = "user_prompt";
    pub const MESSAGE: &str = "message";
    pub const THOUGHT: &str = "thought";
    pub const TOOL_CALL: &str = "tool_call";
    pub const TOOL_RESULT: &str = "tool_result";
    pub const PLAN: &str = "plan";
    pub const USAGE: &str = "usage";
    pub const TURN_END: &str = "turn_end";
    pub const PERMISSION_REQUEST: &str = "permission_request";
    pub const PERMISSION_ANSWER: &str = "permission_answer";
    pub const PERMISSION_EXPIRED: &str = "permission_expired";
    /// Answered by policy without interrupting the operator.
    pub const POLICY_ALLOWED: &str = "policy_allowed";
    pub const POLICY_DENIED: &str = "policy_denied";
    pub const ERROR: &str = "error";
    /// The session's work item was closed (by the agent or the operator);
    /// the session ends at the end of the turn.
    pub const WORK_CLOSED: &str = "work_closed";
    /// A plan session wrote its plan document; the item now carries its slug.
    pub const PLAN_ARTIFACT: &str = "plan_artifact";
    /// Deterministic checks began at submit (`commands`).
    pub const CHECK_STARTED: &str = "check_started";
    /// One check finished (`command`, `ok`, `exit`, `tail`, `ms`).
    pub const CHECK_RESULT: &str = "check_result";
    /// A submission was refused before a review existed: over the cap, or a
    /// check failed (`reason`).
    pub const REVIEW_REJECTED: &str = "review_rejected";
    /// A review session gave its verdict on the review it was spawned for.
    pub const REVIEW_VERDICT: &str = "review_verdict";
    /// The channel reached its daily token ceiling; the gateway refused a
    /// model call. Recorded once per session.
    pub const CEILING: &str = "ceiling";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_running_accepts_a_prompt() {
        assert!(SessionState::Running.accepts_prompt());
        // A session waiting on a decision takes the decision, not a prompt.
        assert!(!SessionState::WaitingOnYou.accepts_prompt());
        assert!(!SessionState::Starting.accepts_prompt());
        for s in [
            SessionState::Closed,
            SessionState::KilledBudget,
            SessionState::Failed,
        ] {
            assert!(!s.accepts_prompt());
            assert!(s.is_terminal());
        }
    }
}
