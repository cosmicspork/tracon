//! A minimal ACP agent for adapter tests. Speaks just enough of `omp acp`:
//! initialize, session/new (with a model config option), session/set_config_option,
//! session/prompt (emitting chunks, a tool call, one permission request, usage),
//! and session/close. NDJSON on stdio.
//!
//! The requested model is echoed back on stderr as `MODEL=<value>` so the test
//! can assert what `set_config_option` received.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

fn send(out: &mut impl Write, v: &Value) {
    writeln!(out, "{v}").unwrap();
    out.flush().unwrap();
}

fn main() {
    // One locked reader for the whole process: nesting `stdin().lock()` inside the
    // read loop deadlocks, and the permission answer has to be read mid-turn.
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut next_agent_id = 0i64;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_str(line.trim()).unwrap();
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

        match method {
            "initialize" => send(
                &mut stdout,
                &json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":1,
                    "agentInfo":{"name":"fake","version":"18.0.4"}
                }}),
            ),
            "session/new" => send(
                &mut stdout,
                &json!({"jsonrpc":"2.0","id":id,"result":{
                    "sessionId":"fake-session",
                    "configOptions":[{"id":"model","category":"model","currentValue":"m/a",
                        "options":[{"value":"m/a","name":"A"},{"value":"m/b","name":"B"}]}]
                }}),
            ),
            "session/set_config_option" => {
                let value = msg["params"]["value"].as_str().unwrap_or("").to_string();
                eprintln!("MODEL={value}");
                send(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":id,"result":{
                        "configOptions":[{"id":"model","category":"model","currentValue":value,
                            "options":[{"value":"m/a","name":"A"},{"value":"m/b","name":"B"}]}]
                    }}),
                );
            }
            "session/prompt" => {
                let sid = msg["params"]["sessionId"].clone();
                // Streamed message.
                send(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":sid,"update":{"sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":"working"},"messageId":"m1"}}}),
                );
                // A tool call that needs permission.
                send(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":sid,"update":{"sessionUpdate":"tool_call","toolCallId":"call|fc",
                    "title":"run just test","kind":"execute","status":"pending",
                    "rawInput":{"command":"just test"}}}}),
                );
                // Ask permission (agent->client request).
                next_agent_id += 1;
                let perm_id = next_agent_id;
                send(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":perm_id,"method":"session/request_permission","params":{
                    "sessionId":sid,
                    "toolCall":{"toolCallId":"call|fc","title":"run just test","kind":"execute",
                        "status":"pending","rawInput":{"command":"just test"}},
                    "options":[{"optionId":"allow_once","name":"Allow once","kind":"allow_once"},
                        {"optionId":"reject_once","name":"Reject","kind":"reject_once"}]}}),
                );
                // Wait for the permission answer before finishing the turn.
                let answer = read_response(&mut reader, perm_id);
                let opt = answer["result"]["outcome"]["optionId"]
                    .as_str()
                    .unwrap_or("");
                let status = if opt == "allow_once" {
                    "completed"
                } else {
                    "failed"
                };
                send(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":sid,"update":{"sessionUpdate":"tool_call_update","toolCallId":"call|fc",
                    "status":status,"rawOutput":{"content":[{"type":"text","text":"ok"}]}}}}),
                );
                send(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":sid,"update":{"sessionUpdate":"usage_update","size":1000000,
                    "used":15024,"cost":{"amount":0.12,"currency":"USD"}}}}),
                );
                send(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":id,"result":{
                    "stopReason":"end_turn",
                    "usage":{"inputTokens":15019,"outputTokens":5,"totalTokens":15024,"cachedReadTokens":0}}}),
                );
            }
            "session/close" => {
                send(&mut stdout, &json!({"jsonrpc":"2.0","id":id,"result":{}}));
                break;
            }
            _ => {
                if id.is_some() {
                    send(
                        &mut stdout,
                        &json!({"jsonrpc":"2.0","id":id,
                        "error":{"code":-32601,"message":"unknown"}}),
                    );
                }
            }
        }
    }
}

/// Block until the response with `id` arrives (the permission answer). Other
/// client lines are ignored in this fake.
fn read_response(reader: &mut impl BufRead, id: i64) -> Value {
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        if !line.trim().is_empty() {
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            if v.get("id").and_then(|x| x.as_i64()) == Some(id) && v.get("method").is_none() {
                return v;
            }
        }
        line.clear();
    }
    json!({})
}
