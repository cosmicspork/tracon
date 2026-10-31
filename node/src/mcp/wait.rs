//! How long a status tool blocks inside one call.

use serde_json::Value;

/// The longest a status tool will block inside a single call.
///
/// A harness's MCP client gives up on a tool call long before a human gets to
/// a review, and that failure surfaces as a transport error the agent cannot
/// act on. Returning "still waiting" inside that budget turns the same wait
/// into something it can retry, so the cap sits below the shortest client
/// timeout with room to spare rather than near it.
///
/// It was 20 s, sized against the retired omp harness's 30 s client. The
/// shortest budget left is 60 s, the MCP TS SDK's `DEFAULT_REQUEST_TIMEOUT_MSEC`
/// that a harness run outside the boundary commonly inherits; Claude Code's
/// is lifted further by its launch (`adapter::claude`), and tracon-agent sets
/// no read timeout of its own. 45 s keeps a quarter of the shortest budget in
/// hand, which is what a slow store read on a busy node needs, and more than
/// doubles how much of a human's attention one call can cover.
pub const MAX_WAIT_SECS: u64 = 45;

/// How long a status call should block, clamped to [`MAX_WAIT_SECS`].
///
/// An oversized request is honoured up to the cap rather than rejected: the
/// caller wants to wait, and polling is the only thing that actually works.
pub fn wait_secs(args: &Value) -> u64 {
    args.get("wait_secs")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_WAIT_SECS)
        .min(MAX_WAIT_SECS)
}

/// What ends a `pipeline_wait` or `run_wait` call before its time is up, as
/// its `until` argument names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    /// The pipeline or any of its jobs changing state.
    Change,
    /// Only the pipeline finishing.
    Finished,
    /// A job failing, or the pipeline finishing.
    Failure,
}

/// The `until` a CI wait was asked for, `change` when absent. Both forges
/// parse it here so an unknown value is refused the same way on either.
pub fn until(args: &Value) -> Result<Until, String> {
    match args.get("until") {
        None | Some(Value::Null) => Ok(Until::Change),
        Some(v) => match v.as_str() {
            Some("change") => Ok(Until::Change),
            Some("finished") => Ok(Until::Finished),
            Some("failure") => Ok(Until::Failure),
            _ => Err(format!(
                "until must be \"change\", \"finished\" or \"failure\", not {v}"
            )),
        },
    }
}

/// The schema of a CI wait's `until` argument.
pub fn until_schema() -> Value {
    serde_json::json!({
        "type": "string",
        "enum": ["change", "finished", "failure"],
        "description": "What ends the wait early: `change` (the default) any move, \
            `finished` only finishing, `failure` a job failing or finishing.",
    })
}

/// Whether a CI wait returns on this read rather than holding on; running
/// out of time is the caller's to add.
///
/// `changed` is whether the state moved from the one the caller last saw,
/// and `first` whether it passed no `since` and this is its first look. A
/// failed job ends a `failure` wait only when that is news (`changed` or
/// `first`), so a caller already told of one is not answered again at once
/// on every call while later stages run.
pub fn returns(until: Until, changed: bool, first: bool, done: bool, failed: bool) -> bool {
    done || match until {
        Until::Change => changed,
        Until::Finished => false,
        Until::Failure => failed && (changed || first),
    }
}

/// What one read of a pipeline or run means to a CI wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    /// The token to compare the next read with and to hand back as `state`.
    pub state: String,
    /// Whether the state moved from the one the caller last saw.
    pub changed: bool,
    /// Whether the pipeline or run has finished.
    pub done: bool,
    /// Whether the wait returns on this read, time left aside.
    pub returns: bool,
}

/// The tools that wait: each blocks inside one call for at most
/// [`MAX_WAIT_SECS`]. How to keep waiting on one is said once, in the
/// orientation, and each description carries only [`pointer`].
pub const WAITING: &[&str] = &[
    super::operator::QUESTION_STATUS,
    super::approvals::STATUS,
    super::review::STATUS,
    super::review::REPORT_STATUS,
    super::operator::REPORT_STATUS,
    super::egress::REQUEST,
    super::setup::TRY,
    super::github::RUN_WAIT,
    super::gitlab::PIPELINE_WAIT,
];

/// The waits on the operator a session is held open for when its turn ends,
/// as `session::waits` reads them: ending the turn on one of these is a way
/// to wait, and on any other wait it is not.
pub const HELD: &[&str] = &[
    super::operator::QUESTION_STATUS,
    super::approvals::STATUS,
    super::review::STATUS,
    super::review::REPORT_STATUS,
];

/// The line a waiting tool's description ends with in place of the shared
/// paragraph on waiting. It says enough on its own for a harness the
/// operator runs, which has no orientation.
pub fn pointer() -> String {
    format!(
        "Blocks at most {MAX_WAIT_SECS} seconds a call; call again to keep waiting (your \
         orientation says when ending your turn waits instead)."
    )
}

/// Whether a call to one of the node's own tools is a poll: a wait its
/// description tells the agent to repeat, unchanged, until something moves.
/// Each of these blocks for at most [`MAX_WAIT_SECS`] and then answers that it
/// is still waiting, so a run of identical calls to one is how an agent waits
/// on a person or a pipeline, not a loop. `repo_setup_try` is a poll only with
/// the `trial_id` of a trial already running; without one it starts a trial.
pub fn is_poll(tool: &str, args: Option<&Value>) -> bool {
    match tool {
        super::review::STATUS
        | super::review::REPORT_STATUS
        | super::operator::QUESTION_STATUS
        | super::operator::REPORT_STATUS
        | super::approvals::STATUS
        | super::egress::REQUEST
        | super::github::RUN_WAIT
        | super::gitlab::PIPELINE_WAIT => true,
        super::setup::TRY => args.is_some_and(|a| a.get("trial_id").is_some()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Waiting is explained once, in the orientation: a waiting tool's own
    /// description names its states and ends on the pointer, and does not
    /// say again how ending a turn waits.
    #[test]
    fn every_waiting_tool_ends_on_the_pointer_rather_than_explaining_waiting() {
        use super::super::*;
        let all = [
            operator::definitions(),
            approvals::definitions(),
            review::definitions(),
            egress::definitions(),
            setup::definitions(),
            github::definitions(),
            gitlab::definitions(),
        ]
        .concat();
        for name in WAITING.iter().chain(HELD) {
            let description = all
                .iter()
                .find(|d| d["name"] == *name)
                .unwrap_or_else(|| panic!("{name} is not defined"))["description"]
                .as_str()
                .unwrap();
            assert!(description.ends_with(&pointer()), "{name}: {description}");
            assert!(
                !description.contains("end your turn"),
                "{name}: {description}"
            );
        }
    }

    #[test]
    fn the_wait_stays_under_any_mcp_client_timeout() {
        // The cap only earns its keep if it is inside the shortest client
        // timeout a harness gives a tool call: 60 s, the MCP TS SDK default.
        // A quarter of that stays in hand.
        const { assert!(MAX_WAIT_SECS <= 45) };
        assert_eq!(wait_secs(&json!({})), MAX_WAIT_SECS);
        assert_eq!(wait_secs(&json!({ "wait_secs": 600 })), MAX_WAIT_SECS);
        assert_eq!(wait_secs(&json!({ "wait_secs": 300 })), MAX_WAIT_SECS);
        assert_eq!(wait_secs(&json!({ "wait_secs": 5 })), 5);
        assert_eq!(wait_secs(&json!({ "wait_secs": 0 })), 0);
        // Nonsense falls back to the default rather than blocking forever.
        assert_eq!(wait_secs(&json!({ "wait_secs": -1 })), MAX_WAIT_SECS);
        assert_eq!(wait_secs(&json!({ "wait_secs": "600" })), MAX_WAIT_SECS);
    }

    #[test]
    fn only_documented_waits_are_polls() {
        for tool in [
            "review_status",
            "report_status",
            "question_status",
            "issue_report_status",
            "approval_status",
            "request_egress",
            "run_wait",
            "pipeline_wait",
        ] {
            assert!(is_poll(tool, None), "{tool}");
        }
        assert!(is_poll(
            "repo_setup_try",
            Some(&json!({ "trial_id": "t-1" }))
        ));
        assert!(!is_poll("repo_setup_try", Some(&json!({}))));
        assert!(!is_poll("repo_setup_try", None));
        for tool in [
            "submit_review",
            "pr_status",
            "run_status",
            "service_status",
            "doc_read",
        ] {
            assert!(!is_poll(tool, None), "{tool}");
        }
    }

    #[test]
    fn until_defaults_to_change_and_refuses_an_unknown_value() {
        assert_eq!(until(&json!({})), Ok(Until::Change));
        assert_eq!(until(&json!({ "until": "finished" })), Ok(Until::Finished));
        assert_eq!(until(&json!({ "until": "failure" })), Ok(Until::Failure));
        for bad in [json!("done"), json!(true), json!(1)] {
            let e = until(&json!({ "until": bad })).unwrap_err();
            assert!(e.contains("until must be"), "{e}");
        }
    }
}
