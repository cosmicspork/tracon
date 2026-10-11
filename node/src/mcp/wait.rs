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
/// It was 20 s, sized against the retired omp harness's 30 s client. Both
/// remaining harnesses give a tool call 60 s: OpenCode inherits the MCP TS
/// SDK's `DEFAULT_REQUEST_TIMEOUT_MSEC` unless told otherwise
/// (`docs/reference/opencode-v1.18.30/config-state.md` §5.4), and the node
/// does tell it otherwise — the launch config it writes sets
/// `experimental.mcp_timeout` explicitly, so 60 s is the floor rather than
/// the ceiling. 45 s keeps a quarter of the shortest budget in hand, which is
/// what a slow store read on a busy node needs, and more than doubles how
/// much of a human's attention one call can cover.
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

    #[test]
    fn the_wait_stays_under_any_mcp_client_timeout() {
        // The cap only earns its keep if it is inside the shortest client
        // timeout either supported harness gives a tool call: 60 s, the MCP TS
        // SDK default OpenCode inherits and the node then states explicitly
        // (`config-state.md` §5.4). A quarter of that stays in hand.
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
}
