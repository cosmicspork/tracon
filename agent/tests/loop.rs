//! The binary against a scripted fake of an OpenAI-compatible server.

mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};
use support::{answer, serve, tool_call, Reply};
use tracon_agent::hashline;

struct Run {
    code: i32,
    stdout: String,
    events: Vec<Value>,
}

async fn run(base: &str, cwd: &std::path::Path, extra: &[&str]) -> Run {
    let events = cwd.join(".events.jsonl");
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_tracon-agent"))
        .args([
            "--standalone",
            "--base-url",
            base,
            "--model",
            "fake",
            "--prompt",
            "fix it",
        ])
        .arg("--cwd")
        .arg(cwd)
        .arg("--events")
        .arg(&events)
        .args(extra)
        .output()
        .await
        .unwrap();
    let events = std::fs::read_to_string(&events)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    Run {
        code: out.status.code().unwrap(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        events,
    }
}

fn names(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn a_stale_anchor_is_refused_and_the_retry_lands() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("greet.py"), "print('hi')\n").unwrap();
    let good = hashline::anchor(1, "print('hi')");
    let stale = if good.ends_with(":0000") {
        "1:ffff"
    } else {
        "1:0000"
    };
    let (base, fake) = serve(vec![
        tool_call("c1", "read", json!({"path": "greet.py"})),
        tool_call(
            "c2",
            "edit",
            json!({"path": "greet.py", "edits": [{"op": "replace", "start": stale, "text": "print('hello')"}]}),
        ),
        tool_call(
            "c3",
            "edit",
            json!({"path": "greet.py", "edits": [{"op": "replace", "start": good, "text": "print('hello')"}]}),
        ),
        answer("done"),
    ])
    .await;

    let r = run(&base, dir.path(), &[]).await;

    assert_eq!(r.code, 0, "{:#?}", r.events);
    assert_eq!(r.stdout.trim(), "done");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("greet.py")).unwrap(),
        "print('hello')\n"
    );
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let read_result = requests[1]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(read_result["role"], "tool");
    assert_eq!(read_result["content"], format!("{good}|print('hi')\n"));
    let refused = requests[2]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(refused["tool_call_id"], "c2");
    let refused = refused["content"].as_str().unwrap();
    assert!(
        refused.starts_with("ERROR: greet.py: nothing was written."),
        "{refused}"
    );
    assert!(
        refused.contains("line 1 changed since it was read"),
        "{refused}"
    );
    // The request names every tool and keeps one call per turn.
    let tools: Vec<_> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["read", "edit", "write", "glob", "grep", "shell"]);
    assert_eq!(requests[0]["parallel_tool_calls"], false);

    assert_eq!(
        names(&r.events),
        [
            "start",
            "request",
            "response",
            "tool_call",
            "tool_result",
            "request",
            "response",
            "tool_call",
            "tool_result",
            "request",
            "response",
            "tool_call",
            "tool_result",
            "request",
            "response",
            "end"
        ]
    );
    let end = r.events.last().unwrap();
    assert_eq!(end["outcome"], "finished");
    assert_eq!(end["totals"]["turns"], 4);
    assert_eq!(end["totals"]["tool_calls"], 3);
    assert_eq!(end["totals"]["tool_errors"], 1);
    assert_eq!(end["totals"]["usage"]["input"], 420);
    assert_eq!(r.events[2]["request_id"], "req-c1");
}

#[tokio::test]
async fn the_turn_limit_and_context_overflow_have_their_own_exits() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _) = serve(vec![
        tool_call("c1", "glob", json!({"pattern": "*"})),
        tool_call("c2", "glob", json!({"pattern": "*"})),
    ])
    .await;
    let r = run(&base, dir.path(), &["--max-turns", "2"]).await;
    assert_eq!(r.code, 3);
    assert_eq!(r.events.last().unwrap()["outcome"], "max_turns");

    let overflow = json!({"error": {"code": 400, "type": "exceed_context_size_error",
        "message": "request (40000 tokens) exceeds the available context size (32768 tokens)"}});
    let (base, _) = serve(vec![Reply::error(
        StatusCode::BAD_REQUEST,
        overflow.to_string(),
    )])
    .await;
    let r = run(&base, dir.path(), &[]).await;
    assert_eq!(r.code, 4);
    assert_eq!(r.events.last().unwrap()["outcome"], "context_overflow");
}

#[tokio::test]
async fn a_prompt_on_the_command_line_needs_standalone() {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_tracon-agent"))
        .args([
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "m",
            "--prompt",
            "x",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--standalone"));
}
