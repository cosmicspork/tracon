//! How long a status tool blocks inside one call.

use serde_json::Value;

use super::CallContext;

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

/// The longest a status tool blocks for an external harness that declared
/// a longer client timeout. Ten minutes covers most of a person's attention
/// in one call while still bounding what a dropped connection holds.
pub const MAX_EXTERNAL_WAIT_SECS: u64 = 600;

/// What is kept in hand of a declared budget, for the same slow store read
/// [`MAX_WAIT_SECS`] leaves room for, and the round trip.
const BUDGET_HEADROOM_SECS: u64 = 60;

/// The most this call may block: [`MAX_WAIT_SECS`], or for an external
/// harness that declared its client's timeout (`X-Tracon-Wait-Budget`), that
/// budget less a minute, up to [`MAX_EXTERNAL_WAIT_SECS`]. A budget that
/// leaves less than the default keeps the default, which every client allows.
pub fn cap(ctx: &CallContext) -> u64 {
    ctx.wait_budget()
        .map(|budget| {
            budget
                .saturating_sub(BUDGET_HEADROOM_SECS)
                .clamp(MAX_WAIT_SECS, MAX_EXTERNAL_WAIT_SECS)
        })
        .unwrap_or(MAX_WAIT_SECS)
}

/// How long a status call should block, clamped to the call's [`cap`].
///
/// An oversized request is honoured up to the cap rather than rejected: the
/// caller wants to wait, and polling is the only thing that actually works.
pub fn wait_secs(ctx: &CallContext, args: &Value) -> u64 {
    let cap = cap(ctx);
    args.get("wait_secs")
        .and_then(Value::as_u64)
        .unwrap_or(cap)
        .min(cap)
}

/// What a `still_waiting` message says of the cap: that asking for longer
/// will not help, unless the caller declared a budget that already sets it.
pub fn capped(ctx: &CallContext) -> String {
    let cap = cap(ctx);
    match ctx.wait_budget() {
        Some(_) => {
            format!("The wait is capped at {cap} seconds, inside what your client declared.")
        }
        None => format!(
            "The wait is capped at {cap} seconds so this call returns before your MCP client \
             times out; asking for longer will not help."
        ),
    }
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
        "Blocks at most {MAX_WAIT_SECS} seconds a call, or longer when your client says it \
         allows more; call again to keep waiting (your orientation says when ending your turn \
         waits instead)."
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
        let ctx = CallContext::session("s1", "work", "n1");
        let wait_secs = |args: Value| wait_secs(&ctx, &args);
        assert_eq!(wait_secs(json!({})), MAX_WAIT_SECS);
        assert_eq!(wait_secs(json!({ "wait_secs": 600 })), MAX_WAIT_SECS);
        assert_eq!(wait_secs(json!({ "wait_secs": 300 })), MAX_WAIT_SECS);
        assert_eq!(wait_secs(json!({ "wait_secs": 5 })), 5);
        assert_eq!(wait_secs(json!({ "wait_secs": 0 })), 0);
        // Nonsense falls back to the default rather than blocking forever.
        assert_eq!(wait_secs(json!({ "wait_secs": -1 })), MAX_WAIT_SECS);
        assert_eq!(wait_secs(json!({ "wait_secs": "600" })), MAX_WAIT_SECS);
    }

    #[test]
    fn an_external_harness_that_declares_a_longer_client_timeout_waits_longer() {
        let external = |budget| CallContext::external(None, "work", "n1").with_wait_budget(budget);
        assert_eq!(cap(&external(None)), MAX_WAIT_SECS);
        // A minute of the budget is kept in hand.
        assert_eq!(cap(&external(Some(1140))), MAX_EXTERNAL_WAIT_SECS);
        assert_eq!(cap(&external(Some(300))), 240);
        assert_eq!(wait_secs(&external(Some(300)), &json!({})), 240);
        assert_eq!(
            wait_secs(&external(Some(300)), &json!({ "wait_secs": 600 })),
            240
        );
        assert_eq!(
            wait_secs(&external(Some(300)), &json!({ "wait_secs": 5 })),
            5
        );
        // Too short a budget keeps the default; a huge one is held to the
        // external cap.
        assert_eq!(cap(&external(Some(30))), MAX_WAIT_SECS);
        assert_eq!(cap(&external(Some(90))), MAX_WAIT_SECS);
        assert_eq!(cap(&external(Some(u64::MAX))), MAX_EXTERNAL_WAIT_SECS);
        // A session's calls never carry one.
        let session = CallContext::session("s1", "work", "n1").with_wait_budget(Some(1140));
        assert_eq!(cap(&session), MAX_WAIT_SECS);
        assert!(capped(&external(None)).contains("will not help"));
        assert!(!capped(&external(Some(1140))).contains("will not help"));
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
