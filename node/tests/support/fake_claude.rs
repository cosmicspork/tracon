//! A stand-in for `claude --print --input-format stream-json`, speaking the
//! same stream-json and control protocol on stdio.
//!
//! It exists so the adapter is exercised against a real process over real
//! pipes rather than against a mock of itself. The frames it emits are the
//! shapes read out of the shipped 2.1.247 binary and confirmed against a live
//! run — see `docs/reference/phase-7-notes.md`.

use std::io::{BufRead, Write};

fn emit(v: serde_json::Value) {
    let mut out = std::io::stdout();
    writeln!(out, "{v}").ok();
    out.flush().ok();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("2.1.247 (Claude Code)");
        return;
    }
    let session_id = args
        .windows(2)
        .find(|w| w[0] == "--session-id")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "fake-session".into());
    let model = args
        .windows(2)
        .find(|w| w[0] == "--model")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "sonnet".into());
    // The version the adapter checks the pin against. `FAKE_CLAUDE_VERSION`
    // lets a test drive the mismatch path.
    let version = std::env::var("FAKE_CLAUDE_VERSION").unwrap_or_else(|_| "2.1.247".to_string());
    let mcp_status =
        std::env::var("FAKE_CLAUDE_MCP_STATUS").unwrap_or_else(|_| "connected".to_string());
    let has_mcp = args.iter().any(|a| a == "--mcp-config");

    emit(serde_json::json!({
        "type": "system",
        "subtype": "init",
        "session_id": session_id,
        "claude_code_version": version,
        "model": model,
        "permissionMode": "default",
        "tools": ["Bash", "Read", "Edit"],
        "mcp_servers": if has_mcp {
            serde_json::json!([{ "name": "tracon", "status": mcp_status }])
        } else {
            serde_json::json!([])
        },
        "capabilities": ["interrupt_receipt_v1"],
    }));

    // One turn per user message on stdin, and the process stays alive between
    // them: that is what makes a session multi-turn rather than one shot.
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match v["type"].as_str().unwrap_or_default() {
            "user" => turn(&session_id),
            // An interrupt is acknowledged and ends the turn.
            "control_request" => emit(serde_json::json!({
                "type": "control_response",
                "response": { "subtype": "success", "request_id": v["request_id"] },
            })),
            "control_response" => {
                // The answer to our permission ask: allow runs the tool, deny
                // reports the failure back the way the real one does.
                let allowed = v["response"]["response"]["behavior"] == "allow";
                emit(serde_json::json!({
                    "type": "user",
                    "message": { "role": "user", "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_1",
                        "is_error": !allowed,
                        "content": if allowed { "ok" } else { "permission denied" },
                    }]},
                }));
                emit(serde_json::json!({
                    "type": "result",
                    "subtype": "success",
                    "session_id": session_id,
                    "total_cost_usd": 0.0123,
                    "usage": {
                        "input_tokens": 15019,
                        "output_tokens": 5,
                        "cache_read_input_tokens": 0,
                    },
                }));
            }
            _ => {}
        }
    }
}

/// A turn: some text, a tool the harness wants to run, and then the wait for
/// the operator's answer. The result frame is emitted from the control
/// response above, so the turn genuinely blocks on the decision.
fn turn(session_id: &str) {
    emit(serde_json::json!({
        "type": "assistant",
        "message": {
            "id": "msg_1",
            "role": "assistant",
            "content": [
                { "type": "thinking", "thinking": "considering it" },
                { "type": "text", "text": "working on it" },
                { "type": "tool_use", "id": "toolu_1", "name": "Bash",
                  "input": { "command": "git status" } },
            ],
        },
    }));
    emit(serde_json::json!({
        "type": "control_request",
        "request_id": "req_1",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "tool_use_id": "toolu_1",
            "input": { "command": "git status" },
        },
    }));
    let _ = session_id;
}
