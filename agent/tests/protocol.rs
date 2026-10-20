//! The binary in protocol mode, driven as the node drives it: JSON-RPC over
//! its stdio, answering its permission requests.

mod support;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use acp::rpc::{Incoming, Peer};
use acp::types::{
    methods, ContentBlock, NewSessionParams, NewSessionResult, PermissionOutcome, PromptParams,
    PromptResult, RequestPermissionParams, RequestPermissionResult, SessionIdParams,
};
use serde_json::{json, Value};
use support::{answer, serve, tool_call};
use tracon_agent::hashline;

/// What the node saw, in the order it arrived.
#[derive(Debug, Clone)]
enum Seen {
    Update(Value),
    Permission(Box<RequestPermissionParams>),
}

struct Node {
    peer: Peer,
    seen: Arc<Mutex<Vec<Seen>>>,
    _child: tokio::process::Child,
}

/// Starts the agent and answers its permission requests: edits are declined,
/// everything else allowed.
async fn start(base: &str, events: &std::path::Path) -> Node {
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
        _child: child,
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
