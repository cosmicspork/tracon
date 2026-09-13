//! Adapter behaviour against the fake ACP agent: launch, model selection,
//! streaming, permission round-trip, and the turn result the budget uses.

#[path = "support/mod.rs"]
mod support;
use support::events::{drain_until, next_permission};
use support::state;

use tracon::adapter::{
    omp::OmpAdapter, AdapterError, HarnessAdapter, HarnessEvent, LaunchSpec, PermissionReply,
};
use tracon::runner::{local::LocalRunner, Runner, RunnerCommand};

struct FakeRunner;

#[async_trait::async_trait]
impl Runner for FakeRunner {
    async fn spawn(
        &self,
        mut cmd: RunnerCommand,
    ) -> Result<tracon::runner::Spawned, tracon::runner::RunnerError> {
        // Cargo builds this bin for integration tests and hands us its path.
        cmd.argv = vec![env!("CARGO_BIN_EXE_fake_agent").to_string()];
        LocalRunner.spawn(cmd).await
    }

    async fn run_capture(
        &self,
        cmd: RunnerCommand,
    ) -> Result<std::process::Output, tracon::runner::RunnerError> {
        // `omp --version` shape, so the pin check has something to parse.
        let _ = cmd;
        Ok(std::process::Output {
            status: Default::default(),
            stdout: b"omp/18.0.4\n".to_vec(),
            stderr: Vec::new(),
        })
    }

    async fn kill(&self, _name: &str) -> Result<(), tracon::runner::RunnerError> {
        Ok(())
    }
}

#[tokio::test]
async fn version_is_parsed_from_the_runner() {
    state::isolate();
    let v = OmpAdapter::new("18.0.4")
        .version(&FakeRunner)
        .await
        .unwrap();
    assert_eq!(v.found, "18.0.4");
    assert!(v.matches());
    let v = OmpAdapter::new("18.0.3")
        .version(&FakeRunner)
        .await
        .unwrap();
    assert!(!v.matches());
}

#[tokio::test]
async fn probe_models_lists_what_the_harness_offers() {
    state::isolate();
    let models = OmpAdapter::new("18.0.4")
        .probe_models(&FakeRunner, &Default::default())
        .await
        .unwrap();
    assert_eq!(
        models.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
        ["m/a", "m/b"]
    );
}

#[tokio::test]
async fn unknown_model_is_refused_before_prompting() {
    state::isolate();
    let err = OmpAdapter::new("18.0.4")
        .launch(
            &FakeRunner,
            LaunchSpec {
                cwd_in_runner: "/work".into(),
                model: "m/nope".into(),
                container_name: "t".into(),
                harness_home: "/root".into(),
                mcp_servers: Vec::new(),
                tools: Vec::new(),
                env: Vec::new(),
                system_prompt_file: None,
            },
        )
        .await
        .err()
        .expect("unknown model must fail launch");
    assert!(matches!(err, AdapterError::UnknownModel(m) if m == "m/nope"));
}

#[tokio::test]
async fn launch_prompt_permission_and_turn_result() {
    state::isolate();
    let (handle, mut rx) = OmpAdapter::new("18.0.4")
        .launch(
            &FakeRunner,
            LaunchSpec {
                cwd_in_runner: "/work".into(),
                model: "m/b".into(),
                container_name: "t".into(),
                harness_home: "/root".into(),
                mcp_servers: Vec::new(),
                tools: Vec::new(),
                env: Vec::new(),
                system_prompt_file: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(handle.harness_session_id(), "fake-session");

    let mut seen = Vec::new();
    let prompt = tokio::spawn({
        // The turn does not resolve until the permission is answered, so the
        // prompt has to be in flight while we read events.
        let text = "fix the validation".to_string();
        async move { handle.prompt(text).await }
    });

    let perm = next_permission(&mut rx, &mut seen).await;
    let HarnessEvent::Permission { request, reply } = perm else {
        panic!("expected a permission request")
    };
    assert_eq!(request.title, "run just test");
    assert_eq!(request.kind.as_deref(), Some("execute"));
    assert!(request.options.iter().any(|o| o.option_id == "allow_once"));
    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();

    let turn = prompt.await.unwrap().unwrap();
    assert_eq!(turn.stop_reason, "end_turn");
    // The budget counts cumulative totalTokens, not visible input.
    assert_eq!(turn.usage.total_tokens, 15024);

    drain_until(&mut rx, &mut seen, "tool_update:completed").await;
    assert!(seen.contains(&"models:2".to_string()));
    assert!(seen.contains(&"chunk:working".to_string()));
    assert!(seen.contains(&"tool_call:run just test".to_string()));
    assert!(seen.contains(&"tool_update:completed".to_string()));
    assert!(seen.contains(&"usage".to_string()));
}

#[tokio::test]
async fn denying_a_permission_fails_the_tool_call() {
    state::isolate();
    let (handle, mut rx) = OmpAdapter::new("18.0.4")
        .launch(
            &FakeRunner,
            LaunchSpec {
                cwd_in_runner: "/work".into(),
                model: "m/a".into(),
                container_name: "t".into(),
                harness_home: "/root".into(),
                mcp_servers: Vec::new(),
                tools: Vec::new(),
                env: Vec::new(),
                system_prompt_file: None,
            },
        )
        .await
        .unwrap();
    let prompt = tokio::spawn(async move { handle.prompt("x".into()).await });
    let mut seen = Vec::new();
    let HarnessEvent::Permission { reply, .. } = next_permission(&mut rx, &mut seen).await else {
        panic!("expected permission")
    };
    reply
        .send(PermissionReply::Selected("reject_once".into()))
        .unwrap();
    prompt.await.unwrap().unwrap();
    drain_until(&mut rx, &mut seen, "tool_update:failed").await;
    assert!(seen.contains(&"tool_update:failed".to_string()));
}

/// A harness process that starts but never answers the handshake.
///
/// It holds the node's end of a pipe, so the test can try to answer after the
/// node has given up and see whether anything is still listening.
struct SilentRunner {
    /// The node's side of the harness's stdout, kept so the test can write a
    /// late `initialize` response into it.
    node_side: std::sync::Mutex<Option<tokio::io::DuplexStream>>,
    killed: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Runner for SilentRunner {
    async fn spawn(
        &self,
        _cmd: RunnerCommand,
    ) -> Result<tracon::runner::Spawned, tracon::runner::RunnerError> {
        let (node_side, harness_side) = tokio::io::duplex(64 * 1024);
        *self.node_side.lock().unwrap() = Some(node_side);
        Ok(tracon::runner::Spawned {
            stdin: Box::new(tokio::io::sink()),
            stdout: Box::new(harness_side),
            // A process that is still running: nothing ever reports an exit.
            done: Box::pin(std::future::pending()),
            endpoint: None,
        })
    }

    async fn run_capture(
        &self,
        _cmd: RunnerCommand,
    ) -> Result<std::process::Output, tracon::runner::RunnerError> {
        Ok(std::process::Output {
            status: Default::default(),
            stdout: b"omp/18.0.4\n".to_vec(),
            stderr: Vec::new(),
        })
    }

    async fn kill(&self, name: &str) -> Result<(), tracon::runner::RunnerError> {
        self.killed.lock().unwrap().push(name.to_string());
        Ok(())
    }
}

/// A harness that starts and then says nothing.
///
/// Startup is bounded rather than hanging the session forever, the runtime is
/// told to remove the harness, and — the part a timeout alone does not give —
/// nothing on the node is left listening for the handshake, so an
/// `initialize` answer that arrives after the node gave up has nowhere to
/// land. The clock is paused: the wait is the adapter's, not the test's.
#[tokio::test(start_paused = true)]
async fn a_harness_that_never_handshakes_times_out_and_leaves_nothing_listening() {
    state::isolate();
    let runner = SilentRunner {
        node_side: std::sync::Mutex::new(None),
        killed: std::sync::Mutex::new(Vec::new()),
    };
    let error = OmpAdapter::new("18.0.4")
        .launch(
            &runner,
            LaunchSpec {
                cwd_in_runner: "/work".into(),
                model: "m/a".into(),
                container_name: "tracon-h-silent".into(),
                harness_home: "/root".into(),
                mcp_servers: Vec::new(),
                tools: Vec::new(),
                env: Vec::new(),
                system_prompt_file: None,
            },
        )
        .await
        .err()
        .expect("a harness that never handshakes must not become a session");
    assert!(
        error.to_string().contains("startup timed out"),
        "the reason says what happened: {error}"
    );
    assert_eq!(
        runner.killed.lock().unwrap().as_slice(),
        ["tracon-h-silent".to_string()],
        "the harness the node started is removed rather than left running"
    );

    // The harness answers at last. Nothing is reading: the reader the
    // handshake started was aborted with it, so the late answer cannot be
    // taken for a session that started. (An abort is delivered at the task's
    // next poll, so give the runtime a turn first.)
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    let mut node_side = runner.node_side.lock().unwrap().take().expect("spawned");
    let late = br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#;
    let wrote = {
        use tokio::io::AsyncWriteExt;
        node_side
            .write_all(late)
            .await
            .and(node_side.write_all(b"\n").await)
    };
    assert!(
        wrote.is_err(),
        "a late handshake must find the node's side of the harness closed"
    );
}

/// The shapes this adapter decodes were read from one revision of ACP. An
/// agent that answers `initialize` with another one is refused by name, with
/// both sides in the reason, rather than driven until a field fails to decode
/// in the middle of a turn.
#[tokio::test]
async fn an_agent_speaking_an_unsupported_acp_protocol_is_refused() {
    state::isolate();
    let error = OmpAdapter::new("18.0.4")
        .launch(
            &FakeRunner,
            LaunchSpec {
                cwd_in_runner: "/work".into(),
                model: "m/a".into(),
                container_name: "t".into(),
                harness_home: "/root".into(),
                mcp_servers: Vec::new(),
                tools: Vec::new(),
                env: vec![
                    ("FAKE_ACP_PROTOCOL".into(), "2".into()),
                    ("FAKE_ACP_NAME".into(), "oh-my-pi".into()),
                ],
                system_prompt_file: None,
            },
        )
        .await
        .err()
        .expect("an unsupported protocol must not become a session");
    let reason = error.to_string();
    assert!(
        matches!(error, AdapterError::IncompatibleProtocol { .. }),
        "{reason}"
    );
    assert_eq!(
        reason, "harness oh-my-pi reports acp protocol 2; this node supports 1",
        "the reason names the harness, what it speaks, and what this node speaks"
    );
}

/// The `--version` check and the handshake check are two different moments and
/// can disagree — the image the probe ran against need not be the image the
/// session runs in. The handshake is the one that decides.
#[tokio::test]
async fn an_agent_reporting_a_version_other_than_the_pin_is_refused() {
    state::isolate();
    let error = OmpAdapter::new("18.0.4")
        .launch(
            &FakeRunner,
            LaunchSpec {
                cwd_in_runner: "/work".into(),
                model: "m/a".into(),
                container_name: "t".into(),
                harness_home: "/root".into(),
                mcp_servers: Vec::new(),
                tools: Vec::new(),
                env: vec![("FAKE_ACP_VERSION".into(), "18.0.5".into())],
                system_prompt_file: None,
            },
        )
        .await
        .err()
        .expect("a harness outside the pin must not become a session");
    assert!(
        matches!(&error, AdapterError::VersionMismatch { found, pinned }
            if found == "18.0.5" && pinned == "18.0.4"),
        "{error}"
    );
}

/// What a compatible handshake leaves behind: the agent's own name, the build
/// it reported, and the protocol revision the session negotiated.
#[tokio::test]
async fn a_compatible_handshake_reports_what_it_ran() {
    state::isolate();
    let (handle, _rx) = OmpAdapter::new("18.0.4")
        .launch(
            &FakeRunner,
            LaunchSpec {
                cwd_in_runner: "/work".into(),
                model: "m/a".into(),
                container_name: "t".into(),
                harness_home: "/root".into(),
                mcp_servers: Vec::new(),
                tools: Vec::new(),
                env: vec![("FAKE_ACP_NAME".into(), "oh-my-pi".into())],
                system_prompt_file: None,
            },
        )
        .await
        .expect("a matching harness starts");
    let compat = handle.compat();
    assert_eq!(compat.agent, "oh-my-pi");
    assert_eq!(compat.version, "18.0.4");
    assert_eq!(compat.protocol, "acp/1");
    handle.close().await.ok();
}
