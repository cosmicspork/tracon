//! The Claude Code adapter against a fake harness speaking the same
//! stream-json and control protocol on real pipes.
//!
//! The cases mirror `adapter.rs` deliberately: both adapters feed the same
//! supervisor, the same policy layer and the same queue, so what matters is
//! that a permission round-trip, a turn result and a version mismatch behave
//! identically whichever harness produced them.

use tokio::sync::mpsc;

use tracon::adapter::{
    claude::ClaudeAdapter, AdapterError, HarnessAdapter, HarnessEvent, LaunchSpec, PermissionReply,
};
use tracon::runner::{local::LocalRunner, Runner, RunnerCommand};

struct FakeRunner;

#[async_trait::async_trait]
impl Runner for FakeRunner {
    async fn spawn(
        &self,
        mut cmd: RunnerCommand,
    ) -> Result<tracon::runner::Spawned, tracon::runner::RunnerError> {
        // Keep the flags: the argv the adapter built is part of what is under
        // test, and the fake reads --session-id and --model out of it.
        cmd.argv[0] = env!("CARGO_BIN_EXE_fake_claude").to_string();
        cmd.workdir = None;
        LocalRunner.spawn(cmd).await
    }

    async fn run_capture(
        &self,
        _cmd: RunnerCommand,
    ) -> Result<std::process::Output, tracon::runner::RunnerError> {
        Ok(std::process::Output {
            status: Default::default(),
            stdout: b"2.1.247 (Claude Code)\n".to_vec(),
            stderr: Vec::new(),
        })
    }

    async fn kill(&self, _name: &str) -> Result<(), tracon::runner::RunnerError> {
        Ok(())
    }
}

fn spec() -> LaunchSpec {
    spec_env(Vec::new())
}

/// Environment for the *child*, never the test process: these tests run in
/// parallel threads, and `set_var` in one would reach every launch in flight.
fn spec_env(env: Vec<(String, String)>) -> LaunchSpec {
    LaunchSpec {
        cwd_in_runner: "/work".into(),
        model: "opus".into(),
        container_name: "tracon-h-test".into(),
        mcp_servers: vec![serde_json::json!({
            "type": "http",
            "name": "tracon",
            "url": "http://gw:7421/mcp/s1",
            "headers": [{ "name": "Authorization", "value": "Bearer tok" }],
        })],
        tools: Vec::new(),
        env,
        system_prompt_file: None,
    }
}

/// Collect labels until a permission request arrives or the harness goes away.
async fn drain(
    rx: &mut mpsc::Receiver<HarnessEvent>,
    out: &mut Vec<String>,
) -> Option<HarnessEvent> {
    let quiet = std::time::Duration::from_millis(500);
    while let Ok(Some(ev)) = tokio::time::timeout(quiet, rx.recv()).await {
        match ev {
            HarnessEvent::MessageChunk { ref text, .. } => out.push(format!("chunk:{text}")),
            HarnessEvent::ThoughtChunk { ref text, .. } => out.push(format!("thought:{text}")),
            HarnessEvent::ToolCall(ref t) => out.push(format!("tool_call:{}", t.title)),
            HarnessEvent::ToolCallUpdate(ref t) => out.push(format!(
                "tool_update:{}",
                t.status.clone().unwrap_or_default()
            )),
            HarnessEvent::Usage { .. } => out.push("usage".into()),
            HarnessEvent::Permission { .. } => return Some(ev),
            HarnessEvent::Exited { .. } => return None,
            _ => {}
        }
    }
    None
}

#[tokio::test]
async fn version_is_parsed_from_the_runner() {
    let a = ClaudeAdapter::new("2.1.247");
    let v = a.version(&FakeRunner).await.unwrap();
    assert_eq!(v.found, "2.1.247");
    assert!(v.matches());
}

#[tokio::test]
async fn launch_prompt_permission_and_turn_result() {
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, mut rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    assert!(!handle.harness_session_id().is_empty());

    let turn = tokio::spawn(async move { handle.prompt("do the thing".into()).await });

    let mut seen = Vec::new();
    let ev = drain(&mut rx, &mut seen).await.expect("a permission ask");
    assert!(
        seen.contains(&"chunk:working on it".to_string()),
        "{seen:?}"
    );
    assert!(
        seen.contains(&"thought:considering it".to_string()),
        "{seen:?}"
    );
    assert!(seen.iter().any(|s| s.starts_with("tool_call:")), "{seen:?}");

    let HarnessEvent::Permission { request, reply } = ev else {
        unreachable!()
    };
    // The queue renders this, so it has to say what would actually happen.
    assert_eq!(request.title, "Bash: git status");
    assert_eq!(request.kind.as_deref(), Some("tool"));
    // Exactly the two option ids the supervisor and the policy layer assume.
    let ids: Vec<&str> = request
        .options
        .iter()
        .map(|o| o.option_id.as_str())
        .collect();
    assert_eq!(ids, vec!["allow_once", "reject_once"]);

    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("the turn ended")
        .unwrap()
        .unwrap();
    assert_eq!(result.stop_reason, "end_turn");
    // The CLI reports no total; the budget must still be charged.
    assert_eq!(result.usage.charged(), 15024);

    let mut after = Vec::new();
    drain(&mut rx, &mut after).await;
    assert!(
        after.contains(&"tool_update:completed".to_string()),
        "{after:?}"
    );
}

#[tokio::test]
async fn denying_a_permission_fails_the_tool_call() {
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, mut rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    let turn = tokio::spawn(async move { handle.prompt("do it".into()).await });

    let mut seen = Vec::new();
    let ev = drain(&mut rx, &mut seen).await.expect("a permission ask");
    let HarnessEvent::Permission { reply, .. } = ev else {
        unreachable!()
    };
    reply
        .send(PermissionReply::Selected("reject_once".into()))
        .unwrap();

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await;
    let mut after = Vec::new();
    drain(&mut rx, &mut after).await;
    assert!(
        after.contains(&"tool_update:failed".to_string()),
        "{after:?}"
    );
}

/// A permission that expires is a deny, never a silent allow: the supervisor
/// drops the reply channel, and the harness must still be told.
#[tokio::test]
async fn an_expired_permission_denies_rather_than_hanging() {
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, mut rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    let turn = tokio::spawn(async move { handle.prompt("do it".into()).await });

    let mut seen = Vec::new();
    let ev = drain(&mut rx, &mut seen).await.expect("a permission ask");
    let HarnessEvent::Permission { reply, .. } = ev else {
        unreachable!()
    };
    drop(reply);

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await;
    let mut after = Vec::new();
    drain(&mut rx, &mut after).await;
    assert!(
        after.contains(&"tool_update:failed".to_string()),
        "{after:?}"
    );
}

/// This layer is the one most likely to break silently, so it fails closed:
/// the pin is checked against the harness's own report, not only a
/// `--version` call that may have run against a different image.
#[tokio::test]
async fn a_version_the_node_did_not_pin_refuses_to_launch() {
    let a = ClaudeAdapter::new("2.1.247");
    let spec = spec_env(vec![("FAKE_CLAUDE_VERSION".into(), "2.0.1".into())]);
    let err = match a.launch(&FakeRunner, spec).await {
        Ok(_) => panic!("an unpinned version must not launch"),
        Err(e) => e,
    };
    match err {
        AdapterError::VersionMismatch { found, pinned } => {
            assert_eq!(found, "2.0.1");
            assert_eq!(pinned, "2.1.247");
        }
        other => panic!("expected a version mismatch, got {other}"),
    }
}

/// A session whose MCP server did not connect has no tools, and would fail in
/// a way that looks like the model being unhelpful rather than a broken node.
#[tokio::test]
async fn an_unreachable_mcp_server_refuses_to_launch() {
    let a = ClaudeAdapter::new("2.1.247");
    let spec = spec_env(vec![("FAKE_CLAUDE_MCP_STATUS".into(), "failed".into())]);
    let err = match a.launch(&FakeRunner, spec).await {
        Ok(_) => panic!("a session with no tools must not launch"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("MCP"), "{err}");
}

/// The session id the node wrote on the row and the one the harness uses have
/// to be the same string, or a resumed session cannot be found again.
#[tokio::test]
async fn the_node_chooses_the_session_id() {
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, _rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    let id = handle.harness_session_id();
    assert!(uuid::Uuid::parse_str(id).is_ok(), "{id} is not a uuid");
}

#[tokio::test]
async fn a_session_takes_more_than_one_turn() {
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, mut rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    let handle = std::sync::Arc::new(handle);

    for round in 0..2 {
        let h = handle.clone();
        let turn = tokio::spawn(async move { h.prompt(format!("round {round}")).await });
        let mut seen = Vec::new();
        let ev = drain(&mut rx, &mut seen)
            .await
            .unwrap_or_else(|| panic!("round {round}: the harness went away"));
        let HarnessEvent::Permission { reply, .. } = ev else {
            unreachable!()
        };
        reply
            .send(PermissionReply::Selected("allow_once".into()))
            .unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
            .await
            .unwrap_or_else(|_| panic!("round {round} never finished"))
            .unwrap()
            .unwrap();
        assert_eq!(r.stop_reason, "end_turn");
    }
}
