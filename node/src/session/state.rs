//! Session and permission states, as `DESIGN.md` §3 defines them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Running,
    /// The supervisor has cancelled any active turn and fenced the harness's
    /// prompt, tool, and model paths. Its workspace and evidence stay intact.
    Paused,
    WaitingOnYou,
    /// Defined here because the schema and the interface both name it; nothing
    /// in this slice runs deterministic checks between turns yet.
    WaitingOnCheck,
    /// Its work is published and it went idle, so it was put away: container
    /// stopped, egress grant revoked, workspace kept. Not ended — ending it is
    /// the operator's — and carried on as a new session on that workspace
    /// when a CI failure or a review comment needs it.
    Suspended,
    Closed,
    KilledBudget,
    Failed,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::WaitingOnYou => "waiting_on_you",
            Self::WaitingOnCheck => "waiting_on_check",
            Self::Suspended => "suspended",
            Self::Closed => "closed",
            Self::KilledBudget => "killed_budget",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::KilledBudget | Self::Failed)
    }

    /// The stored spellings of the states a session never leaves, for the
    /// guarded writes that refuse to resurrect one. Kept beside
    /// `is_terminal`, and tested against it, so a new terminal state cannot
    /// be added to one and forgotten in the other.
    pub const TERMINAL: &'static [&'static str] = &["closed", "killed_budget", "failed"];

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
    /// An externally attached harness went quiet for the idle timeout, or the
    /// operator detached it. Nothing failed; nothing is running.
    Detached,
    /// The harness was not the one this node is built to drive: a version
    /// outside the pin, or a protocol revision the adapter does not speak.
    /// Distinct from `Error` because it is a fact about the image rather than
    /// about this session, and every session on this node will end the same
    /// way until the image or the pin changes.
    Incompatible,
    /// The operator carried a suspended session's work on in a new session,
    /// which `continued_from` names.
    Continued,
    /// The node stopped or restarted while the session was live: a graceful
    /// shutdown ended it, or the next start found its row still open. Not the
    /// operator's stop and not the harness's failure, and the work it was
    /// doing can be carried forward from its workspace.
    NodeRestart,
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
            Self::Detached => "detached",
            Self::Incompatible => "incompatible",
            Self::Continued => "continued",
            Self::NodeRestart => "node_restart",
            Self::Error => "error",
        }
    }
}

/// Event kinds written to the log. The SPA groups on these.
pub mod event_kind {
    pub const SESSION_STARTED: &str = "session_started";
    pub const WORKTREE: &str = "worktree";
    /// The repository's image is being built before the harness can start in
    /// it (`status`, `repo`). Minutes, once per repository; the start event
    /// that follows says which image the session got.
    pub const REPO_IMAGE: &str = "repo_image";
    /// The session asked for a host its repository does not open to it
    /// (`host`). Once per host: the agent saw a 403 from a package manager,
    /// and this is where the operator sees what it wanted.
    pub const EGRESS_REFUSED: &str = "egress_refused";
    /// What the session was told at start, so the transcript shows it.
    pub const ORIENTATION: &str = "orientation";
    pub const STATE: &str = "state";
    /// An operator or a failure watchdog fenced this session before it could
    /// start another turn. `source` is telemetry only; the state is decisive.
    pub const SESSION_PAUSED: &str = "session_paused";
    pub const SESSION_RESUMED: &str = "session_resumed";
    /// The session's work was published (`url`, `review_id`): the change on
    /// the forge it is now answerable for.
    pub const PUBLISHED: &str = "published";
    /// A published session went idle and was put away (`idle_ms`).
    pub const SESSION_SUSPENDED: &str = "session_suspended";
    /// The host slept while this session was live (`asleep_ms`,
    /// `inhibitor_held`). An interruption, not a gap: the session's waiting
    /// cards had their deadlines moved on by the same time.
    pub const HOST_SUSPENDED: &str = "host_suspended";
    pub const USER_PROMPT: &str = "user_prompt";
    pub const MESSAGE: &str = "message";
    pub const THOUGHT: &str = "thought";
    pub const TOOL_CALL: &str = "tool_call";
    pub const TOOL_RESULT: &str = "tool_result";
    pub const PLAN: &str = "plan";
    pub const USAGE: &str = "usage";
    /// The gateway's on-the-wire count for a turn and the harness's own
    /// report disagreed beyond tolerance, or the harness reported nothing
    /// while the gateway counted something (`turn`, `gateway`, `harness`,
    /// `charged_tokens`). Both numbers are in the payload: a disagreement is
    /// a fact with two sides, and the operator is the one who can say which
    /// is wrong. The budget is charged the larger of the two, so a harness
    /// that under-reports cannot buy itself more room.
    pub const USAGE_MISMATCH: &str = "usage_mismatch";
    /// Model calls went through for this turn and the gateway could count
    /// none of them: the provider returned no usage and there is no
    /// estimator, so the honest answer is "unknown", not zero. Recorded so an
    /// unmetered turn is visible against a budget or a ceiling rather than
    /// silently free.
    pub const USAGE_UNMETERED: &str = "usage_unmetered";
    pub const TURN_END: &str = "turn_end";
    pub const PERMISSION_REQUEST: &str = "permission_request";
    pub const PERMISSION_ANSWER: &str = "permission_answer";
    pub const PERMISSION_EXPIRED: &str = "permission_expired";
    /// An approved call ran: `state` is how it ended.
    pub const APPROVAL_SETTLED: &str = "approval_settled";
    /// Answered by policy without interrupting the operator.
    pub const POLICY_ALLOWED: &str = "policy_allowed";
    pub const POLICY_DENIED: &str = "policy_denied";
    pub const ERROR: &str = "error";
    /// The session's work item was closed (by the agent or the operator);
    /// the session ends at the end of the turn.
    pub const WORK_CLOSED: &str = "work_closed";
    /// A plan session wrote its plan document; the item now carries its slug.
    pub const PLAN_ARTIFACT: &str = "plan_artifact";
    /// The agent showed the operator its work (`id`, `title`, `format`,
    /// `head_sha`, `review_id`, `channel`, `slug`, `files`).
    pub const WORK_SHOWN: &str = "work_shown";
    /// Deterministic checks began at submit (`commands`).
    pub const CHECK_STARTED: &str = "check_started";
    /// One check finished (`command`, `ok`, `exit`, `tail`, `ms`).
    pub const CHECK_RESULT: &str = "check_result";
    /// All nonempty operator-required checks passed (or exact immutable
    /// evidence was reused) for this candidate.
    pub const CANDIDATE_VERIFIED: &str = "candidate_verified";
    /// A submission was refused before a review existed: over the cap, or a
    /// check failed (`reason`).
    pub const REVIEW_REJECTED: &str = "review_rejected";
    /// A review session gave its verdict on the review it was spawned for.
    pub const REVIEW_VERDICT: &str = "review_verdict";
    /// An operator persisted a review decision. Authority-driven transitions
    /// intentionally do not emit this human-intervention metric.
    pub const REVIEW_DECISION: &str = "review_decision";
    /// The channel reached its daily token ceiling; the gateway refused a
    /// model call. Recorded once per session.
    pub const CEILING: &str = "ceiling";
    /// A provider answered a model call with an error and the harness is
    /// retrying inside the turn (`provider`, `status`, `message`, `attempt`).
    /// Informational: the session stays running, because the harness has not
    /// given up. Recorded by the gateway, which sees the upstream answer, and
    /// by the supervisor when the harness says so itself.
    pub const PROVIDER_ERROR: &str = "provider_error";
    /// The harness issued the same tool call, unchanged, several times in a
    /// row within one turn (`what`, `count`, `title`, `kind`). Recorded and
    /// surfaced, never acted on: repeating a command is also what a great
    /// deal of legitimate work looks like, so this is a signal for the
    /// operator to weigh, not a measure of progress. Only repeated *failure*
    /// pauses a session.
    pub const REPETITION: &str = "repetition";
    /// Something arrived for a session that had already ended (or been
    /// fenced) and was refused rather than applied: a harness event, a check
    /// result, a startup handoff, a state transition a writer still held.
    /// `what` names the writer and `state` the row it was refused against.
    /// Recorded, never silent: a refusal the operator cannot see is
    /// indistinguishable from a race that was never noticed.
    pub const LATE_REFUSED: &str = "late_refused";
    /// A deterministic check was stopped mid-execution because the session
    /// was paused or ended (`candidate_id`, `head_sha`, `reason`). Its
    /// evidence row is `cancelled`, which is never reusable and never a pass.
    pub const CHECK_CANCELLED: &str = "check_cancelled";
    /// A required check could not run in the check image at all: the shell
    /// could not find its command (`candidate_id`, `command`, `missing_tool`,
    /// `image`, `image_source`). Not a failing check — nothing about the
    /// candidate was established — and its evidence row is `not_runnable`,
    /// which is never reusable and never a pass.
    pub const CHECK_NOT_RUNNABLE: &str = "check_not_runnable";
    /// The candidate's dependencies were prepared for this run's checks
    /// (`ok`, `ms`): once, before the first check that actually executed.
    pub const CHECK_PREPARED: &str = "check_prepared";
    /// A mediated mutation's outcome could not be determined — a prompt whose
    /// dispatch never reported, a node that restarted with one in flight — or
    /// has since been established (`reason`, `intent`, `refusing`, or
    /// `cleared`). While it stands, operations that depend on the unknown
    /// answer are refused: a second prompt would duplicate a turn that may
    /// already be running. Never presented as success or failure.
    pub const UNCERTAIN: &str = "uncertain";
    /// The harness created a session of its own — a fork, a background
    /// subagent — naming one of this node's as its parent (`child`, `parent`,
    /// `untracked`). Recorded with its lineage and surfaced; never driven,
    /// because there is no route to register a child session yet and an
    /// untracked worker the node cannot supervise is worse than a visible
    /// refusal.
    pub const CHILD_SESSION: &str = "child_session";
    /// The gateway refused a model call before it reached the provider,
    /// because the method and path are not on the inference allowlist the
    /// credential is lent for (`provider`, `method`, `reason`, `attempt`).
    pub const GATEWAY_REFUSED: &str = "gateway_refused";
    /// The harness's own API changed the workspace tree behind tracon's back —
    /// a revert, an unrevert, a patch applied through the harness — with
    /// tracon's permission (`action`, `method`, `path`). It is recorded
    /// because a candidate review is bound to the tree that was captured
    /// (#188): a tree that moved under a verified candidate has to be visible
    /// to whatever decides whether that verification still holds.
    pub const WORKSPACE_CHANGED: &str = "workspace_changed";
    /// A terminal was opened under the `terminal` capability: the shell that
    /// was spawned and where (`phase` — `spawn` or `attach` — `pty_id`,
    /// `command`, `args`, `cwd`, `env_kept`, `env_dropped`, `grant`).
    ///
    /// A PTY is an interactive shell, not a per-command ledger. What the
    /// operator authorised is *a terminal in this workspace*, once; every
    /// command typed at that prompt afterwards runs without a further
    /// decision and leaves no tool-call record, because the harness raises
    /// none for a PTY (finding 7). The event says what was opened, not what
    /// was run in it.
    pub const PTY_OPENED: &str = "pty_opened";
    /// A terminal was closed, or a proxied terminal connection ended
    /// (`phase` — `removed` or `detached` — `pty_id`, `bytes_in`,
    /// `bytes_out`, `duration_ms`, `reason`, and `output` only when output
    /// capture was deliberately turned on).
    pub const PTY_CLOSED: &str = "pty_closed";
    /// The operator abandoned the work this session was the latest attempt
    /// at (`reason`, `summary`): its item closed, or its lineage put away.
    pub const ABANDONED: &str = "abandoned";
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
        assert!(!SessionState::Paused.accepts_prompt());
        for s in [
            SessionState::Closed,
            SessionState::KilledBudget,
            SessionState::Failed,
        ] {
            assert!(!s.accepts_prompt());
            assert!(s.is_terminal());
        }
    }

    /// The guarded writes exclude states by their stored spelling, so the
    /// list has to say exactly what `is_terminal` says.
    #[test]
    fn the_terminal_list_matches_the_predicate() {
        for state in [
            SessionState::Starting,
            SessionState::Running,
            SessionState::Paused,
            SessionState::WaitingOnYou,
            SessionState::WaitingOnCheck,
            SessionState::Closed,
            SessionState::KilledBudget,
            SessionState::Failed,
        ] {
            assert_eq!(
                SessionState::TERMINAL.contains(&state.as_str()),
                state.is_terminal(),
                "{} is listed differently from what is_terminal says",
                state.as_str()
            );
        }
    }
}
