//! A fake `tracon-agent` for the native adapter's tests: the ACP side of the
//! protocol over real pipes, one scripted turn that asks before its one tool
//! call. Knobs, read from its own environment:
//!
//! - `FAKE_NATIVE_VERSION`: the version it reports (default: this build's);
//! - `FAKE_NATIVE_PROTOCOL`: the protocol version it answers (default 1).
//!
//! A session it is asked to load (`session/load`) answers as `tracon-agent`
//! does: first an update the node already has (seq 7, to be dropped), then the
//! interrupted call closed as failed (seq 8), and its turns number from 10 and
//! report their usage per request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use acp::codec::RpcError;
use acp::rpc::{Incoming, Peer};
use acp::types::{
    self, methods, Chunk, ContentBlock, PermissionOption, PermissionOutcome, PromptResult,
    RequestPermissionParams, RequestPermissionResult, RequestUsage, SessionUpdate,
    SessionUpdateParams, ToolCall, ToolCallUpdate, Usage, UsageUpdate,
};
use serde_json::{json, Value};

fn version() -> String {
    std::env::var("FAKE_NATIVE_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").into())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("tracon-agent {}", version());
        return;
    }
    eprintln!("ARGV={}", std::env::args().collect::<Vec<_>>().join(" "));
    let protocol: u32 = std::env::var("FAKE_NATIVE_PROTOCOL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let (peer, mut incoming, read) =
        Peer::new(Box::new(tokio::io::stdout()), Box::new(tokio::io::stdin()));
    tokio::spawn(read);
    let cancelled = Arc::new(AtomicBool::new(false));
    let loaded = Arc::new(AtomicBool::new(false));
    while let Some(message) = incoming.recv().await {
        match message {
            Incoming::Request {
                method,
                params,
                reply,
                ..
            } => match method.as_str() {
                methods::INITIALIZE => {
                    let _ = reply.send(Ok(json!({
                        "protocolVersion": protocol,
                        "agentInfo": {"name": "tracon-agent", "version": version()},
                        "agentCapabilities": {"loadSession": true},
                    })));
                }
                methods::SESSION_NEW => {
                    eprintln!(
                        "CWD={} MCP={}",
                        params["cwd"].as_str().unwrap_or_default(),
                        params["mcpServers"].as_array().map_or(0, Vec::len)
                    );
                    let _ = reply.send(Ok(json!({"sessionId": "native-1"})));
                }
                methods::SESSION_LOAD => {
                    eprintln!("LOAD={}", params["sessionId"].as_str().unwrap_or_default());
                    loaded.store(true, Ordering::SeqCst);
                    let _ = reply.send(Ok(json!({})));
                    update(
                        &peer,
                        7,
                        SessionUpdate::AgentMessageChunk(chunk("already taken")),
                    )
                    .await;
                    update(
                        &peer,
                        8,
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                            tool_call_id: "c1".into(),
                            status: Some("failed".into()),
                            raw_output: None,
                            content: Vec::new(),
                            title: None,
                            kind: None,
                        }),
                    )
                    .await;
                }
                methods::SESSION_PROMPT => {
                    let peer = peer.clone();
                    let cancelled = cancelled.clone();
                    let from = if loaded.load(Ordering::SeqCst) { 10 } else { 0 };
                    tokio::spawn(async move {
                        let _ = reply.send(Ok(turn(&peer, &cancelled, from).await));
                    });
                }
                methods::SESSION_CLOSE => {
                    let _ = reply.send(Ok(Value::Null));
                }
                other => {
                    let _ = reply.send(Err(RpcError::method_not_found(other)));
                }
            },
            Incoming::Notification { method, .. } => {
                if method == methods::SESSION_CANCEL {
                    cancelled.store(true, Ordering::SeqCst);
                }
            }
        }
    }
}

async fn update(peer: &Peer, seq: u64, update: SessionUpdate) {
    let _ = peer
        .notify(
            methods::SESSION_UPDATE,
            &SessionUpdateParams {
                session_id: "native-1".into(),
                update,
                seq: Some(seq),
            },
        )
        .await;
}

fn chunk(text: &str) -> Chunk {
    Chunk {
        content: ContentBlock::Text { text: text.into() },
        message_id: None,
    }
}

async fn turn(peer: &Peer, cancelled: &AtomicBool, from: u64) -> Value {
    update(
        peer,
        from + 1,
        SessionUpdate::AgentThoughtChunk(chunk("considering it")),
    )
    .await;
    update(
        peer,
        from + 2,
        SessionUpdate::AgentMessageChunk(chunk("working on it")),
    )
    .await;
    let call = ToolCall {
        tool_call_id: "c1".into(),
        title: "git status".into(),
        kind: Some("execute".into()),
        status: Some("pending".into()),
        raw_input: Some(json!({"command": "git status"})),
        content: Vec::new(),
        locations: Vec::new(),
        parent_tool_call_id: None,
    };
    update(peer, from + 3, SessionUpdate::ToolCall(call.clone())).await;
    let answer: Result<RequestPermissionResult, _> = peer
        .request(
            methods::SESSION_REQUEST_PERMISSION,
            &RequestPermissionParams {
                session_id: "native-1".into(),
                tool_call: call,
                options: vec![
                    PermissionOption {
                        option_id: types::OPTION_ALLOW_ONCE.into(),
                        name: "Allow".into(),
                        kind: "allow_once".into(),
                    },
                    PermissionOption {
                        option_id: types::OPTION_REJECT_ONCE.into(),
                        name: "Reject".into(),
                        kind: "reject_once".into(),
                    },
                ],
                tool: Some("shell".into()),
            },
        )
        .await;
    let allowed = matches!(
        answer,
        Ok(RequestPermissionResult { outcome: PermissionOutcome::Selected { ref option_id }, .. })
            if option_id == types::OPTION_ALLOW_ONCE
    );
    update(
        peer,
        from + 4,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate {
            tool_call_id: "c1".into(),
            status: Some(if allowed { "completed" } else { "failed" }.into()),
            raw_output: None,
            content: Vec::new(),
            title: None,
            kind: None,
        }),
    )
    .await;
    update(
        peer,
        from + 5,
        SessionUpdate::UsageUpdate(UsageUpdate {
            size: Some(32768),
            used: Some(1450),
            cost: None,
            request: (from > 0).then(|| RequestUsage {
                request_id: Some("req-1".into()),
                input_tokens: 600,
                output_tokens: 50,
                cached_read_tokens: 400,
            }),
        }),
    )
    .await;
    json!(PromptResult {
        stop_reason: if cancelled.load(Ordering::SeqCst) {
            "cancelled"
        } else {
            "end_turn"
        }
        .into(),
        usage: Some(Usage {
            input_tokens: 1000,
            output_tokens: 50,
            total_tokens: 1450,
            cached_read_tokens: 400,
            cache_write_tokens: 0,
        }),
        extra: Default::default(),
    })
}
