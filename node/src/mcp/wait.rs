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
}
