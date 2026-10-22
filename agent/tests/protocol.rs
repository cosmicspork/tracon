//! The binary in protocol mode, driven as the node drives it: JSON-RPC over
//! its stdio, answering its permission requests.

mod support;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use acp::rpc::{Incoming, Peer};
use acp::types::{
    methods, ContentBlock, LoadSessionParams, NewSessionParams, NewSessionResult,
    PermissionOutcome, PromptParams, PromptResult, RequestPermissionParams,
    RequestPermissionResult, SessionIdParams,
};
use axum::http::StatusCode;
use serde_json::{json, Value};
use support::{answer, measured_call, serve, sse, tool_call, Reply};
use tracon_agent::compact::SUMMARY_HEADER;
use tracon_agent::hashline;
use tracon_agent::journal::OUTCOME_UNKNOWN;

/// What the node saw, in the order it arrived.
#[derive(Debug, Clone)]
enum Seen {
    Update(Value),
    Permission(Box<RequestPermissionParams>),
}

struct Node {
    peer: Peer,
    seen: Arc<Mutex<Vec<Seen>>>,
    child: tokio::process::Child,
    init: Value,
}

/// Starts the agent and answers its permission requests: edits are declined,
/// everything else allowed.
async fn start(base: &str, events: &std::path::Path) -> Node {
    start_with(base, events, &[]).await
}

async fn start_with(base: &str, events: &std::path::Path, extra: &[&str]) -> Node {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_tracon-agent"))
        .args([
            "--base-url",
            base,
            "--model",
            "fake",
            "--api-key-env",
            "FAKE_KEY",
        ])
        .arg("--events")
        .arg(events)
        .args(extra)
        .env("FAKE_KEY", "sekrit")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (peer, mut incoming, read) = Peer::new(
        Box::new(child.stdin.take().unwrap()),
        Box::new(child.stdout.take().unwrap()),
    );
    tokio::spawn(read);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Some(m) = incoming.recv().await {
            match m {
                Incoming::Notification { method, params } => {
                    assert_eq!(method, methods::SESSION_UPDATE);
                    log.lock().unwrap().push(Seen::Update(params));
                }
                Incoming::Request {
                    method,
                    params,
                    reply,
                    ..
                } => {
                    assert_eq!(method, methods::SESSION_REQUEST_PERMISSION);
                    let p: RequestPermissionParams = serde_json::from_value(params).unwrap();
                    let option = if p.tool_call.kind.as_deref() == Some("edit") {
                        "reject_once"
                    } else {
                        "allow_once"
                    };
                    log.lock().unwrap().push(Seen::Permission(Box::new(p)));
                    // A refusal says why, as the node's policy does.
                    let message = (option == "reject_once")
                        .then(|| "the node's policy refused this (rule `no-edits`)".to_string());
                    let _ = reply.send(Ok(json!(RequestPermissionResult {
                        outcome: PermissionOutcome::Selected {
                            option_id: option.into()
                        },
                        message,
                    })));
                }
            }
        }
    });
    let init: Value = peer
        .request(methods::INITIALIZE, &acp::types::InitializeParams::node())
        .await
        .unwrap();
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["agentInfo"]["name"], "tracon-agent");
    Node {
        peer,
        seen,
        child,
        init,
    }
}

async fn new_session(node: &Node, cwd: &std::path::Path) -> String {
    let r: NewSessionResult = node
        .peer
        .request(
            methods::SESSION_NEW,
            &NewSessionParams {
                cwd: cwd.display().to_string(),
                mcp_servers: Vec::new(),
            },
        )
        .await
        .unwrap();
    r.session_id
}

fn prompt(session: &str, text: &str) -> PromptParams {
    PromptParams {
        session_id: session.into(),
        prompt: vec![ContentBlock::Text { text: text.into() }],
    }
}

#[tokio::test]
async fn every_tool_call_is_asked_and_a_declined_edit_never_lands() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("greet.py"), "print('hi')\n").unwrap();
    let edit = json!({"path": "greet.py", "edits": [
        {"op": "replace", "start": hashline::anchor(1, "print('hi')"), "text": "print('hello')"}
    ]});
    let (base, fake) = serve(vec![
        tool_call("c1", "read", json!({"path": "greet.py"})),
        tool_call("c2", "edit", edit),
        answer("I could not edit it."),
    ])
    .await;
    let events = dir.path().join(".events.jsonl");
    let node = start(&base, &events).await;
    let session = new_session(&node, dir.path()).await;

    let result: PromptResult = node
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "fix greet.py"))
        .await
        .unwrap();

    assert_eq!(result.stop_reason, "end_turn");
    let usage = result.usage.unwrap();
    // 320 prompt tokens, 100 of them read from the cache.
    assert_eq!(usage.input_tokens, 220);
    assert_eq!(usage.cached_read_tokens, 100);
    assert_eq!(usage.output_tokens, 23);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("greet.py")).unwrap(),
        "print('hi')\n"
    );
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let declined = requests[2]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(declined["tool_call_id"], "c2");
    assert_eq!(
        declined["content"],
        "ERROR: the node's policy refused this (rule `no-edits`)"
    );

    let seen = node.seen.lock().unwrap().clone();
    // Each tool call is announced before it is asked about, and asked about
    // before anything says it ran.
    let position = |pred: &dyn Fn(&Seen) -> bool| seen.iter().position(pred).unwrap();
    for (id, final_status) in [("c1", "completed"), ("c2", "failed")] {
        let announced = position(&|s| {
            matches!(s, Seen::Update(u)
            if u["update"]["sessionUpdate"] == "tool_call" && u["update"]["toolCallId"] == id)
        });
        let asked =
            position(&|s| matches!(s, Seen::Permission(p) if p.tool_call.tool_call_id == id));
        let ended = position(&|s| {
            matches!(s, Seen::Update(u)
            if u["update"]["sessionUpdate"] == "tool_call_update"
                && u["update"]["toolCallId"] == id
                && u["update"]["status"] == final_status)
        });
        assert!(announced < asked && asked < ended, "{id}: {seen:#?}");
    }
    let asked: Vec<_> = seen
        .iter()
        .filter_map(|s| match s {
            Seen::Permission(p) => {
                Some((p.tool_call.kind.clone().unwrap(), p.tool_call.title.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        asked,
        [
            ("read".to_string(), "read greet.py".to_string()),
            ("edit".into(), "edit greet.py".into())
        ]
    );
    let seqs: Vec<u64> = seen
        .iter()
        .filter_map(|s| match s {
            Seen::Update(u) => u["seq"].as_u64(),
            _ => None,
        })
        .collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert!(seen.iter().any(|s| matches!(s, Seen::Update(u)
        if u["update"]["sessionUpdate"] == "agent_message_chunk"
            && u["update"]["content"]["text"] == "I could not edit it.")));

    // The events file mirrors every update with its seq.
    let mirrored: Vec<u64> = std::fs::read_to_string(&events)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["event"] == "update")
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(mirrored, seqs);
}

#[tokio::test]
async fn cancel_ends_the_turn_and_the_session_takes_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let (base, fake) = serve(vec![
        answer("too late").after(Duration::from_secs(30)),
        answer("second"),
    ])
    .await;
    let node = start(&base, &dir.path().join(".events.jsonl")).await;
    let session = new_session(&node, dir.path()).await;

    let peer = node.peer.clone();
    let first_prompt = prompt(&session, "think hard");
    let first = tokio::spawn(async move {
        peer.request::<_, PromptResult>(methods::SESSION_PROMPT, &first_prompt)
            .await
    });
    let started = Instant::now();
    while fake.requests.lock().unwrap().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no request reached the model"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    node.peer
        .notify(
            methods::SESSION_CANCEL,
            &SessionIdParams {
                session_id: session.clone(),
            },
        )
        .await
        .unwrap();
    let first = first.await.unwrap().unwrap();
    assert_eq!(first.stop_reason, "cancelled");
    assert!(started.elapsed() < Duration::from_secs(10));

    let second: PromptResult = node
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "again"))
        .await
        .unwrap();
    assert_eq!(second.stop_reason, "end_turn");
    let requests = fake.requests.lock().unwrap();
    // The cancelled prompt stays in the conversation the model sees next.
    let users: Vec<_> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(users, ["think hard", "again"]);
}

#[tokio::test]
async fn under_the_node_a_turn_is_not_cut_at_the_standalone_limit() {
    // Twenty requests ended every turn of the first live session; under the
    // node a turn runs until the model answers.
    let dir = tempfile::tempdir().unwrap();
    let mut script: Vec<_> = (0..24)
        .map(|i| tool_call(&format!("c{i}"), "glob", json!({"pattern": "*"})))
        .collect();
    script.push(answer("done"));
    let (base, fake) = serve(script).await;
    let node = start(&base, &dir.path().join(".events.jsonl")).await;
    let session = new_session(&node, dir.path()).await;

    let result: PromptResult = node
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "look around"))
        .await
        .unwrap();

    assert_eq!(result.stop_reason, "end_turn");
    assert_eq!(fake.requests.lock().unwrap().len(), 25);
}

fn updates(seen: &[Seen], kind: &str) -> Vec<Value> {
    seen.iter()
        .filter_map(|s| match s {
            Seen::Update(u) if u["update"]["sessionUpdate"] == kind => Some(u.clone()),
            _ => None,
        })
        .collect()
}

fn asked(seen: &[Seen]) -> usize {
    seen.iter()
        .filter(|s| matches!(s, Seen::Permission(_)))
        .count()
}

async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "waited for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_busy_upstream_is_asked_again_and_the_call_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let (base, fake) = serve(vec![
        Reply::error(StatusCode::SERVICE_UNAVAILABLE, "busy").header("retry-after", "1"),
        // Broken off before any answer text: nothing was shown, so it is asked again.
        Reply::cut_after(&[json!({"choices": [{"delta": {"reasoning_content": "hmm"}}]})]),
        tool_call("c1", "glob", json!({"pattern": "*"})),
        answer("done"),
    ])
    .await;
    let node = start(&base, &dir.path().join(".events.jsonl")).await;
    let session = new_session(&node, dir.path()).await;
    let started = Instant::now();

    let result: PromptResult = node
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "look"))
        .await
        .unwrap();

    assert_eq!(result.stop_reason, "end_turn");
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "Retry-After was honoured"
    );
    assert_eq!(fake.requests.lock().unwrap().len(), 4);
    let seen = node.seen.lock().unwrap().clone();
    assert_eq!(asked(&seen), 1, "the call ran once");
    let retries = updates(&seen, "agent_retry");
    assert_eq!(retries.len(), 2, "{retries:#?}");
    assert_eq!(retries[0]["update"]["subtype"], "api_retry");
    assert_eq!(retries[0]["update"]["status"], 503);
    // Each model request reports its own usage, by the id the gateway sees.
    let requests: Vec<_> = updates(&seen, "usage_update")
        .iter()
        .map(|u| u["update"]["request"].clone())
        .collect();
    assert_eq!(requests[0]["requestId"], "req-c1");
    assert_eq!(requests[0]["inputTokens"], 100);
    assert_eq!(requests[1]["inputTokens"], 20);
    assert_eq!(requests[1]["cachedReadTokens"], 100);
}

#[tokio::test]
async fn an_answer_broken_off_after_text_is_kept_and_not_asked_twice() {
    let dir = tempfile::tempdir().unwrap();
    let (base, fake) = serve(vec![
        Reply::cut_after(&[json!({"choices": [{"delta": {"content": "Half an ans"}}]})]),
        answer("whole"),
    ])
    .await;
    let node = start(&base, &dir.path().join(".events.jsonl")).await;
    let session = new_session(&node, dir.path()).await;

    let failed = node
        .peer
        .request::<_, PromptResult>(methods::SESSION_PROMPT, &prompt(&session, "explain"))
        .await;
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(
        fake.requests.lock().unwrap().len(),
        1,
        "text was shown: no retry"
    );

    let next: PromptResult = node
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "go on"))
        .await
        .unwrap();
    assert_eq!(next.stop_reason, "end_turn");
    let requests = fake.requests.lock().unwrap();
    let assistant: Vec<_> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| m["content"].clone())
        .collect();
    assert_eq!(assistant, [json!("Half an ans")]);
}

#[tokio::test]
async fn malformed_calls_are_answered_without_asking_and_a_second_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let raw = |id: &str| {
        sse(&[
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": id,
            "type": "function", "function": {"name": "read", "arguments": "{\"path\": "}}]},
            "finish_reason": "tool_calls"}]}),
        ])
    };
    let (base, fake) = serve(vec![
        tool_call("c1", "nope", json!({})),
        tool_call("c2", "glob", json!({"pattern": "*"})),
        raw("c3"),
        raw("c4"),
    ])
    .await;
    let node = start(&base, &dir.path().join(".events.jsonl")).await;
    let session = new_session(&node, dir.path()).await;

    let result = node
        .peer
        .request::<_, PromptResult>(methods::SESSION_PROMPT, &prompt(&session, "read it"))
        .await;

    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("two malformed responses in a row"),
        "{error}"
    );
    assert!(error.contains("not valid JSON"), "{error}");
    let seen = node.seen.lock().unwrap().clone();
    assert_eq!(asked(&seen), 1, "only the well-formed glob was asked about");
    let failed: Vec<_> = updates(&seen, "tool_call_update")
        .iter()
        .filter(|u| u["update"]["status"] == "failed")
        .map(|u| u["update"]["toolCallId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(failed, ["c1", "c3", "c4"]);
    let requests = fake.requests.lock().unwrap();
    let corrected = requests[1]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(corrected["tool_call_id"], "c1");
    assert!(corrected["content"]
        .as_str()
        .unwrap()
        .contains("there is no tool `nope`"));
}

#[tokio::test]
async fn a_tool_result_the_model_wrote_itself_stops_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _fake) = serve(vec![answer(
        "Done.\n<tool_response>{\"exit\": 0}</tool_response>",
    )])
    .await;
    let node = start(&base, &dir.path().join(".events.jsonl")).await;
    let session = new_session(&node, dir.path()).await;

    let error = node
        .peer
        .request::<_, PromptResult>(methods::SESSION_PROMPT, &prompt(&session, "run tests"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("wrote a tool's result itself"), "{error}");
}

#[tokio::test]
async fn a_replaced_process_carries_the_session_on_from_its_journal() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join(".events.jsonl");
    let (base, fake) = serve(vec![
        tool_call("c1", "shell", json!({"command": "sleep 30"})),
        answer("carried on"),
    ])
    .await;
    let mut first = start(&base, &events).await;
    assert_eq!(first.init["agentCapabilities"]["loadSession"], true);
    let session = new_session(&first, dir.path()).await;
    let peer = first.peer.clone();
    let p = prompt(&session, "run the slow thing");
    tokio::spawn(async move {
        let _ = peer
            .request::<_, PromptResult>(methods::SESSION_PROMPT, &p)
            .await;
    });
    // The call is allowed and running when the process dies.
    let seen = first.seen.clone();
    wait_for("the call to start", || {
        updates(&seen.lock().unwrap(), "tool_call_update")
            .iter()
            .any(|u| u["update"]["status"] == "in_progress")
    })
    .await;
    let last_seq = updates(&first.seen.lock().unwrap(), "tool_call_update")
        .last()
        .unwrap()["seq"]
        .as_u64()
        .unwrap();
    first.child.kill().await.unwrap();

    let second = start(&base, &events).await;
    let _: Value = second
        .peer
        .request(
            methods::SESSION_LOAD,
            &LoadSessionParams {
                session_id: session.clone(),
                cwd: dir.path().display().to_string(),
                mcp_servers: Vec::new(),
            },
        )
        .await
        .unwrap();
    let result: PromptResult = second
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "carry on"))
        .await
        .unwrap();
    assert_eq!(result.stop_reason, "end_turn");

    // The node hears how the call ended, numbered after what it already has.
    let seen = second.seen.lock().unwrap().clone();
    let closed = &updates(&seen, "tool_call_update")[0];
    assert_eq!(closed["update"]["toolCallId"], "c1");
    assert_eq!(closed["update"]["status"], "failed");
    assert_eq!(closed["seq"].as_u64().unwrap(), last_seq + 1);
    assert!(
        updates(&seen, "tool_call").is_empty(),
        "c1 was already shown"
    );
    // The model is told the call's outcome is unknown, and is not run again.
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    let roles: Vec<_> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool", "user"]);
    assert_eq!(messages[3]["content"], format!("ERROR: {OUTCOME_UNKNOWN}"));
    assert_eq!(messages[4]["content"], "carry on");
}

#[tokio::test]
async fn a_process_that_died_waiting_on_the_model_loses_nothing_said() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join(".events.jsonl");
    let (base, fake) = serve(vec![
        tool_call("c1", "glob", json!({"pattern": "*"})),
        tool_call("c2", "glob", json!({"pattern": "*.py"})).after(Duration::from_secs(30)),
        answer("here"),
    ])
    .await;
    let mut first = start(&base, &events).await;
    let session = new_session(&first, dir.path()).await;
    let peer = first.peer.clone();
    let p = prompt(&session, "survey");
    tokio::spawn(async move {
        let _ = peer
            .request::<_, PromptResult>(methods::SESSION_PROMPT, &p)
            .await;
    });
    let requests = fake.requests.clone();
    wait_for("the second request", || requests.lock().unwrap().len() == 2).await;
    first.child.kill().await.unwrap();

    let second = start(&base, &events).await;
    let _: Value = second
        .peer
        .request(
            methods::SESSION_LOAD,
            &LoadSessionParams {
                session_id: session.clone(),
                cwd: dir.path().display().to_string(),
                mcp_servers: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert!(
        updates(&second.seen.lock().unwrap(), "tool_call_update").is_empty(),
        "nothing was left open"
    );
    let _: PromptResult = second
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "again"))
        .await
        .unwrap();
    let requests = fake.requests.lock().unwrap();
    let roles: Vec<_> = requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool", "user"]);
}

#[tokio::test]
async fn a_call_written_as_text_is_pointed_out_and_twice_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let written = r#"<tool_call>{"name": "glob", "arguments": {"pattern": "*"}}</tool_call>"#;
    let (base, fake) = serve(vec![answer(written), answer(written)]).await;
    let node = start(&base, &dir.path().join(".events.jsonl")).await;
    let session = new_session(&node, dir.path()).await;

    let error = node
        .peer
        .request::<_, PromptResult>(methods::SESSION_PROMPT, &prompt(&session, "list"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("written into the text"), "{error}");
    let requests = fake.requests.lock().unwrap();
    let reminder = requests[1]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(reminder["role"], "user");
    assert!(reminder["content"]
        .as_str()
        .unwrap()
        .starts_with("[harness]"));
}

#[tokio::test]
async fn an_upstream_that_asks_for_a_long_wait_ends_the_turn_instead() {
    let dir = tempfile::tempdir().unwrap();
    let (base, fake) = serve(vec![
        Reply::error(StatusCode::TOO_MANY_REQUESTS, "slow down").header("retry-after", "600"),
        answer("never"),
    ])
    .await;
    let node = start(&base, &dir.path().join(".events.jsonl")).await;
    let session = new_session(&node, dir.path()).await;

    let error = node
        .peer
        .request::<_, PromptResult>(methods::SESSION_PROMPT, &prompt(&session, "go"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("429"), "{error}");
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
}

/// The node is told of a compaction, and a process that replaces this one
/// carries on the compacted conversation, not the one before it.
#[tokio::test]
async fn a_compaction_is_reported_and_survives_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join(".events.jsonl");
    let big: String = (0..600)
        .map(|i| format!("line {i:04} {}\n", "x".repeat(90)))
        .collect();
    std::fs::write(dir.path().join("big.txt"), big).unwrap();
    let (base, fake) = serve(vec![
        measured_call("c1", "read", json!({"path": "big.txt"})),
        answer("big.txt is 600 numbered lines."),
        answer("read it"),
        answer("ok"),
    ])
    .await;
    let window = ["--context-window", "24000"];
    let mut first = start_with(&base, &events, &window).await;
    let session = new_session(&first, dir.path()).await;
    let result: PromptResult = first
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "read big.txt"))
        .await
        .unwrap();
    assert_eq!(result.stop_reason, "end_turn");
    let seen = first.seen.lock().unwrap().clone();
    let compacted = updates(&seen, "context_compacted");
    assert_eq!(compacted.len(), 1, "{seen:#?}");
    let c = &compacted[0]["update"];
    assert_eq!(c["trigger"], "threshold");
    assert_eq!(c["summarisedMessages"], 3);
    assert_eq!(c["summary"], "big.txt is 600 numbered lines.");
    assert_eq!(c["contextWindow"], 24000);
    assert!(
        c["tokensBefore"].as_u64() > c["tokensAfter"].as_u64(),
        "{c}"
    );
    // The summariser's request is reported like any other, by its id.
    let usage = updates(&seen, "usage_update");
    assert_eq!(usage.len(), 3);
    first.child.kill().await.unwrap();

    let second = start_with(&base, &events, &window).await;
    let _: Value = second
        .peer
        .request(
            methods::SESSION_LOAD,
            &LoadSessionParams {
                session_id: session.clone(),
                cwd: dir.path().display().to_string(),
                mcp_servers: Vec::new(),
            },
        )
        .await
        .unwrap();
    let _: PromptResult = second
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "next"))
        .await
        .unwrap();
    let requests = fake.requests.lock().unwrap();
    let messages = requests[3]["messages"].as_array().unwrap();
    let roles: Vec<_> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert!(messages[1]["content"]
        .as_str()
        .unwrap()
        .starts_with(SUMMARY_HEADER));
    assert_eq!(messages[2]["content"], "read it");
    assert_eq!(messages[3]["content"], "next");
}

/// A long command's output reaches the node while it runs, as the call's
/// running updates, before its result.
#[tokio::test]
async fn a_running_commands_output_reaches_the_node_before_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _) = serve(vec![
        tool_call(
            "c1",
            "shell",
            json!({"command": "echo building; sleep 1.5; echo testing; sleep 1.5"}),
        ),
        answer("done"),
    ])
    .await;
    let node = start(&base, &dir.path().join(".events.jsonl")).await;
    let session = new_session(&node, dir.path()).await;
    let _: PromptResult = node
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "build"))
        .await
        .unwrap();
    let seen = node.seen.lock().unwrap().clone();
    let updates: Vec<_> = updates(&seen, "tool_call_update")
        .into_iter()
        .map(|u| u["update"].clone())
        .collect();
    let running: Vec<_> = updates
        .iter()
        .filter_map(|u| u["rawOutput"].as_str())
        .collect();
    assert!(running.contains(&"building\n"), "{updates:#?}");
    assert!(running.contains(&"building\ntesting\n"), "{updates:#?}");
    let last = updates.last().unwrap();
    assert_eq!(last["status"], "completed");
}

/// A subagent's calls reach the node under the spawning call, with ids of
/// their own, and are asked about like the parent's; its requests count.
#[tokio::test]
async fn a_subagents_calls_reach_the_node_under_its_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _) = serve(vec![
        tool_call(
            "c1",
            "spawn",
            json!({"task": "list the files\nand say how many"}),
        ),
        tool_call("c2", "glob", json!({"pattern": "*"})),
        tool_call("c3", "finish", json!({"summary": "no files"})),
        answer("it found none"),
    ])
    .await;
    let node = start(&base, &dir.path().join(".events.jsonl")).await;
    let session = new_session(&node, dir.path()).await;
    let result: PromptResult = node
        .peer
        .request(methods::SESSION_PROMPT, &prompt(&session, "delegate"))
        .await
        .unwrap();
    assert_eq!(result.stop_reason, "end_turn");
    // The turn's usage holds the child's request too.
    assert_eq!(result.usage.unwrap().total_tokens, 110 * 3 + 123);

    let seen = node.seen.lock().unwrap().clone();
    let calls: Vec<_> = updates(&seen, "tool_call")
        .into_iter()
        .map(|u| u["update"].clone())
        .collect();
    assert_eq!(calls[0]["toolCallId"], "c1");
    assert_eq!(calls[0]["kind"], "think");
    assert_eq!(calls[0]["title"], "subagent: list the files");
    assert!(calls[0].get("parentToolCallId").is_none());
    assert_eq!(calls[1]["toolCallId"], "c1/c2");
    assert_eq!(calls[1]["parentToolCallId"], "c1");
    assert_eq!(calls[1]["title"], "[list the files] glob *");
    // Both asked, the child's under its parent.
    let asks: Vec<_> = seen
        .iter()
        .filter_map(|s| match s {
            Seen::Permission(p) => Some(p.tool_call.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(asks.len(), 2);
    assert_eq!(asks[1].tool_call_id, "c1/c2");
    assert_eq!(asks[1].parent_tool_call_id.as_deref(), Some("c1"));
    let results: Vec<_> = updates(&seen, "tool_call_update")
        .into_iter()
        .map(|u| u["update"].clone())
        .filter(|u| u["status"] == "completed")
        .collect();
    assert_eq!(results[0]["toolCallId"], "c1/c2");
    assert_eq!(results[1]["toolCallId"], "c1");
    assert_eq!(
        results[1]["rawOutput"]["output"],
        "{\"summary\":\"no files\"}"
    );
    // Each request reported by its id; the child's say nothing of the
    // parent's context.
    let usage: Vec<_> = updates(&seen, "usage_update")
        .into_iter()
        .map(|u| u["update"].clone())
        .collect();
    assert_eq!(usage.len(), 4);
    let child: Vec<_> = usage.iter().filter(|u| u["used"].is_null()).collect();
    assert_eq!(child.len(), 2);
    assert_eq!(child[0]["request"]["requestId"], "req-c2");
}
