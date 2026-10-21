//! The binary against a scripted fake of an OpenAI-compatible server.

mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};
use support::{
    answer, events, measured_call, responses_answer, responses_call, serve, tool_call, Reply,
};
use tracon_agent::compact::{SUMMARISER, SUMMARY_HEADER};
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

/// A read too big for the window: before the next request the conversation is
/// summarised, and the work carries on from the summary.
#[tokio::test]
async fn a_conversation_near_the_window_is_summarised_and_the_work_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let big: String = (0..600)
        .map(|i| format!("line {i:04} {}\n", "x".repeat(90)))
        .collect();
    std::fs::write(dir.path().join("big.txt"), big).unwrap();
    let (base, fake) = serve(vec![
        measured_call("c1", "read", json!({"path": "big.txt"})),
        answer("big.txt is 600 numbered lines of x."),
        answer("done"),
    ])
    .await;

    let r = run(&base, dir.path(), &["--context-window", "24000"]).await;

    assert_eq!(r.code, 0, "{:#?}", r.events);
    assert_eq!(r.stdout.trim(), "done");
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    // The summariser: the transcript, no tools, a bounded answer.
    let summariser = &requests[1];
    assert_eq!(summariser["messages"][0]["content"], SUMMARISER);
    assert!(summariser.get("tools").is_none());
    assert_eq!(summariser["max_tokens"], 4096);
    let transcript = summariser["messages"][1]["content"].as_str().unwrap();
    assert!(transcript.contains("[operator]\nfix it"), "{transcript}");
    assert!(transcript.contains("[end of transcript]"), "{transcript}");
    assert!(transcript.contains("[result of read]"), "{transcript}");
    assert!(transcript.contains("not shown to you"), "{transcript}");
    assert!(
        transcript.contains("\n1\tline 0000 "),
        "anchors stripped: {transcript}"
    );
    // The next request carries the summary and the task, not the read.
    let messages = requests[2]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{messages:#?}");
    let summary = messages[1]["content"].as_str().unwrap();
    assert!(summary.starts_with(SUMMARY_HEADER), "{summary}");
    assert!(summary.contains("big.txt is 600 numbered lines of x."));
    assert!(
        summary.contains("--- operator message 1 ---\nfix it"),
        "{summary}"
    );
    assert!(
        summary.contains("- read {\"path\":\"big.txt\"} → ok"),
        "{summary}"
    );
    assert!(requests[2].to_string().len() < 8_000);

    let compacted = r.events.iter().find(|e| e["event"] == "compacted").unwrap();
    assert_eq!(compacted["trigger"], "threshold");
    assert_eq!(compacted["summarised_messages"], 3);
    assert!(
        compacted["tokens_before"].as_u64().unwrap() >= 12_000,
        "{compacted}"
    );
    assert!(
        compacted["tokens_after"].as_u64().unwrap() < 2_000,
        "{compacted}"
    );
    // The summariser's tokens are the session's too.
    let end = r.events.last().unwrap();
    assert_eq!(end["outcome"], "finished");
    assert!(end["totals"]["usage"]["input"].as_u64().unwrap() > 120 + 120);
}

/// With no window declared, the upstream's refusal is the signal: the
/// conversation is compacted and the request made once more.
#[tokio::test]
async fn a_request_refused_as_too_long_is_compacted_and_asked_again() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(dir.path().join("big.txt"), "x\n".repeat(20_000)).unwrap();
    let overflow = json!({"error": {"code": 400, "type": "exceed_context_size_error",
        "message": "request (70000 tokens) exceeds the available context size (65536 tokens)"}});
    let (base, fake) = serve(vec![
        measured_call("c1", "read", json!({"path": "big.txt"})),
        measured_call("c2", "read", json!({"path": "a.txt"})),
        Reply::error(StatusCode::BAD_REQUEST, overflow.to_string()),
        answer("listed and read a.txt"),
        answer("done"),
    ])
    .await;

    let r = run(&base, dir.path(), &[]).await;

    assert_eq!(r.code, 0, "{:#?}", r.events);
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[3]["messages"][0]["content"], SUMMARISER);
    // Asked again with the summary in place of the first call, and the
    // second call and its result kept as they were.
    let messages = requests[4]["messages"].as_array().unwrap();
    assert!(messages[1]["content"]
        .as_str()
        .unwrap()
        .starts_with(SUMMARY_HEADER));
    assert_eq!(messages[2]["tool_calls"][0]["id"], "c2");
    assert_eq!(messages[3]["tool_call_id"], "c2");
    let names = names(&r.events);
    let at = names.iter().position(|e| *e == "overflow").unwrap();
    assert_eq!(
        names[at + 1..at + 4],
        ["summarising", "summary", "compacted"]
    );
    let compacted = &r.events[at + 3];
    assert_eq!(compacted["trigger"], "overflow");
    assert_eq!(compacted["summarised_messages"], 3);
}

/// The Codex backend's wire: the system prompt in `instructions`, nothing
/// stored upstream, and each reasoning item sent back in place, less its id.
#[tokio::test]
async fn the_responses_wire_runs_calls_and_carries_reasoning_back() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("greet.py"), "print('hi')\n").unwrap();
    let (base, fake) = serve(vec![
        responses_call("c1", "read", json!({"path": "greet.py"}), "E1"),
        responses_answer("done"),
    ])
    .await;

    let r = run(
        &base,
        dir.path(),
        &["--wire", "responses", "--reasoning-effort", "low"],
    )
    .await;

    assert_eq!(r.code, 0, "{:#?}", r.events);
    assert_eq!(r.stdout.trim(), "done");
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let first = &requests[0];
    assert!(first["instructions"]
        .as_str()
        .unwrap()
        .starts_with("You are a coding agent"));
    assert_eq!(first["store"], false);
    assert_eq!(first["reasoning"]["effort"], "low");
    assert_eq!(
        first["input"],
        json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "fix it"}]}])
    );
    let tools: Vec<_> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["read", "edit", "write", "glob", "grep", "shell"]);
    let input = requests[1]["input"].as_array().unwrap();
    assert_eq!(
        input[1],
        json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}],
            "encrypted_content": "E1"})
    );
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "c1");
    assert!(input[2].get("id").is_none());
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(
        input[3]["output"],
        format!("{}|print('hi')\n", hashline::anchor(1, "print('hi')"))
    );
    assert_eq!(r.events[2]["request_id"], "resp_c1");
    let end = r.events.last().unwrap();
    assert_eq!(end["totals"]["usage"]["input"], 200);
    assert_eq!(end["totals"]["usage"]["cache_read"], 120);
}

/// The Codex backend says a prompt is too long its own way; compaction
/// still hears it.
#[tokio::test]
async fn a_responses_overflow_is_compacted_and_asked_again() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(dir.path().join("big.txt"), "x\n".repeat(20_000)).unwrap();
    let overflow = json!({"type": "response.failed", "response": {"id": "resp_x", "status": "failed",
        "error": {"code": "context_length_exceeded",
            "message": "Your input exceeds the context window of this model."}}});
    let (base, fake) = serve(vec![
        responses_call("c1", "read", json!({"path": "big.txt"}), "E1"),
        responses_call("c2", "read", json!({"path": "a.txt"}), "E2"),
        events(&[overflow]),
        responses_answer("read big.txt and a.txt"),
        responses_answer("done"),
    ])
    .await;

    let r = run(&base, dir.path(), &["--wire", "responses"]).await;

    assert_eq!(r.code, 0, "{:#?}", r.events);
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[3]["instructions"], SUMMARISER);
    assert!(requests[3].get("tools").is_none());
    let input = requests[4]["input"].as_array().unwrap();
    let summary = input[0]["content"][0]["text"].as_str().unwrap();
    assert!(summary.starts_with(SUMMARY_HEADER), "{summary}");
    // The second call is kept with its reasoning, in place.
    let kinds: Vec<_> = input.iter().map(|i| i["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "message",
            "reasoning",
            "function_call",
            "function_call_output"
        ]
    );
    assert_eq!(input[1]["encrypted_content"], "E2");
    let compacted = r.events.iter().find(|e| e["event"] == "compacted").unwrap();
    assert_eq!(compacted["trigger"], "overflow");
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
