//! The native adapter against a fake `tracon-agent` speaking ACP on real
//! pipes. The cases mirror `claude_adapter.rs`: every adapter feeds the same
//! supervisor, policy layer and queue.

#[path = "support/mod.rs"]
mod support;
use support::events::{drain_until, next_permission};
use support::state;

use tracon::adapter::{
    native::NativeAdapter, AdapterError, HarnessAdapter, HarnessEvent, LaunchSpec, PermissionReply,
    Resume,
};
use tracon::runner::{local::LocalRunner, Runner, RunnerCommand};

/// Runs the fake in place of the image's binary, with the adapter's argv and
/// `env` extra variables for the fake itself.
struct FakeRunner {
    env: Vec<(String, String)>,
}

#[async_trait::async_trait]
impl Runner for FakeRunner {
    async fn spawn(
        &self,
        mut cmd: RunnerCommand,
    ) -> Result<tracon::runner::Spawned, tracon::runner::RunnerError> {
        assert_eq!(cmd.argv[0], NativeAdapter::BINARY);
        cmd.argv[0] = env!("CARGO_BIN_EXE_fake_native").to_string();
        cmd.workdir = None;
        cmd.env.extend(self.env.clone());
        LocalRunner.spawn(cmd).await
    }

    async fn run_capture(
        &self,
        _cmd: RunnerCommand,
    ) -> Result<std::process::Output, tracon::runner::RunnerError> {
        Ok(std::process::Output {
            status: Default::default(),
            stdout: format!("tracon-agent {}\n", NativeAdapter::PINNED_VERSION).into_bytes(),
            stderr: Vec::new(),
        })
    }

    async fn kill(&self, _name: &str) -> Result<(), tracon::runner::RunnerError> {
        Ok(())
    }
}

fn runner() -> FakeRunner {
    FakeRunner { env: Vec::new() }
}

fn spec() -> LaunchSpec {
    LaunchSpec {
        secret_env: Default::default(),
        cwd_in_runner: "/work".into(),
        model: "local/qwen3-8b".into(),
        container_name: "tracon-h-native-test".into(),
        harness_home: "/root".into(),
        mcp_servers: vec![serde_json::json!({
            "type": "http",
            "name": "tracon",
            "url": "http://gw:7421/mcp/s1",
            "headers": [{ "name": "Authorization", "value": "Bearer tok" }],
        })],
        tools: Vec::new(),
        env: Vec::new(),
        system_prompt_file: None,
        cursor: None,
        resume: None,
    }
}

fn adapter() -> NativeAdapter {
    NativeAdapter::new(NativeAdapter::PINNED_VERSION)
}

#[tokio::test]
async fn version_is_the_nodes_own() {
    state::isolate();
    let v = adapter().version(&runner()).await.unwrap();
    assert_eq!(v.found, NativeAdapter::PINNED_VERSION);
    assert!(v.matches());
}

#[tokio::test]
async fn launch_prompt_permission_and_turn_result() {
    state::isolate();
    let (handle, mut rx) = adapter().launch(&runner(), spec()).await.unwrap();
    assert_eq!(handle.harness_session_id(), "native-1");
    assert_eq!(handle.compat().protocol, "acp/1");
    assert_eq!(handle.compat().agent, "tracon-agent");

    let turn = tokio::spawn(async move { handle.prompt("do the thing".into()).await });

    let mut seen = Vec::new();
    let ev = next_permission(&mut rx, &mut seen).await;
    assert!(
        seen.contains(&"thought:considering it".to_string()),
        "{seen:?}"
    );
    assert!(
        seen.contains(&"chunk:working on it".to_string()),
        "{seen:?}"
    );
    assert!(
        seen.contains(&"tool_call:git status".to_string()),
        "{seen:?}"
    );

    let HarnessEvent::Permission { request, reply } = ev else {
        unreachable!()
    };
    // Policy reads what the node derived from the tool's name and input.
    assert_eq!(request.action, "bash");
    assert_eq!(request.kind.as_deref(), Some("execute"));
    assert_eq!(request.command.as_deref(), Some("git status"));
    assert_eq!(request.resource, None);
    assert_eq!(request.tool_call_id.as_deref(), Some("c1"));
    let ids: Vec<&str> = request
        .options
        .iter()
        .map(|o| o.option_id.as_str())
        .collect();
    assert_eq!(ids, ["allow_once", "reject_once"]);

    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("the turn ended")
        .unwrap()
        .unwrap();
    assert_eq!(result.stop_reason, "end_turn");
    // Fresh input and output; the cache read is recorded, never charged.
    assert_eq!(result.usage.charged(), 1050);
    assert_eq!(result.usage.cached_read_tokens, 400);

    let mut after = Vec::new();
    drain_until(&mut rx, &mut after, "tool_update:completed").await;
    drain_until(&mut rx, &mut after, "usage").await;
}

#[tokio::test]
async fn a_policy_refusal_reaches_the_harness_as_a_rejection() {
    state::isolate();
    let (handle, mut rx) = adapter().launch(&runner(), spec()).await.unwrap();
    let turn = tokio::spawn(async move { handle.prompt("do it".into()).await });
    let mut seen = Vec::new();
    let HarnessEvent::Permission { reply, .. } = next_permission(&mut rx, &mut seen).await else {
        unreachable!()
    };
    reply
        .send(PermissionReply::Refused {
            rule: Some("no-shell".into()),
            reason: Some("not on this channel".into()),
        })
        .unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await;
    let mut after = Vec::new();
    drain_until(&mut rx, &mut after, "tool_update:failed").await;
}

/// A permission that expires is a deny, never a silent allow.
#[tokio::test]
async fn an_expired_permission_denies_rather_than_hanging() {
    state::isolate();
    let (handle, mut rx) = adapter().launch(&runner(), spec()).await.unwrap();
    let turn = tokio::spawn(async move { handle.prompt("do it".into()).await });
    let mut seen = Vec::new();
    let HarnessEvent::Permission { reply, .. } = next_permission(&mut rx, &mut seen).await else {
        unreachable!()
    };
    drop(reply);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await;
    let mut after = Vec::new();
    drain_until(&mut rx, &mut after, "tool_update:failed").await;
}

#[tokio::test]
async fn a_version_other_than_the_pin_is_refused_at_the_handshake() {
    state::isolate();
    let r = FakeRunner {
        env: vec![("FAKE_NATIVE_VERSION".into(), "0.0.1".into())],
    };
    match adapter().launch(&r, spec()).await {
        Err(AdapterError::VersionMismatch { found, pinned }) => {
            assert_eq!(found, "0.0.1");
            assert_eq!(pinned, NativeAdapter::PINNED_VERSION);
        }
        Err(other) => panic!("expected a version mismatch, got {other}"),
        Ok(_) => panic!("expected a version mismatch, got a session"),
    }
}

#[tokio::test]
async fn a_protocol_outside_the_range_is_refused_before_a_session() {
    state::isolate();
    let r = FakeRunner {
        env: vec![("FAKE_NATIVE_PROTOCOL".into(), "2".into())],
    };
    match adapter().launch(&r, spec()).await {
        Err(AdapterError::IncompatibleProtocol { found, .. }) => assert_eq!(found, "2"),
        Err(other) => panic!("expected an incompatible protocol, got {other}"),
        Ok(_) => panic!("expected an incompatible protocol, got a session"),
    }
}

#[tokio::test]
async fn a_relaunch_loads_the_session_and_takes_nothing_twice() {
    state::isolate();
    let adapter = adapter();
    assert!(adapter.restorable());
    let spec = LaunchSpec {
        resume: Some(Resume {
            harness_session_id: "0199-abc".into(),
            last_seq: 7,
        }),
        ..spec()
    };
    let (handle, mut rx) = adapter.launch(&runner(), spec).await.unwrap();
    assert_eq!(handle.harness_session_id(), "0199-abc");
    let turn = tokio::spawn({
        let mut seen = Vec::new();
        async move {
            let ev = next_permission(&mut rx, &mut seen).await;
            let HarnessEvent::Permission { reply, .. } = ev else {
                unreachable!()
            };
            reply
                .send(PermissionReply::Selected("allow_once".into()))
                .unwrap();
            drain_until(&mut rx, &mut seen, "request:req-1").await;
            seen
        }
    });
    let prompt = handle.prompt("carry on".into()).await.unwrap();
    assert_eq!(prompt.stop_reason, "end_turn");
    let seen = turn.await.unwrap();
    // Seq 7 was the node's already: dropped. Seq 8 closes the cut call.
    assert!(
        !seen.contains(&"chunk:already taken".to_string()),
        "{seen:?}"
    );
    assert_eq!(seen[0], "tool_update:failed", "{seen:?}");
    // Each request's own usage reaches the supervisor.
    assert_eq!(seen.last().unwrap(), "request:req-1");
    let resume = handle.resume_point().unwrap();
    assert_eq!(resume.harness_session_id, "0199-abc");
    assert_eq!(resume.last_seq, 15);
}
