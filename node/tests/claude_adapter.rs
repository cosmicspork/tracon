//! The Claude Code adapter against a fake harness speaking the same
//! stream-json and control protocol on real pipes.
//!
//! The cases mirror `adapter.rs` deliberately: both adapters feed the same
//! supervisor, the same policy layer and the same queue, so what matters is
//! that a permission round-trip, a turn result and a version mismatch behave
//! identically whichever harness produced them.

#[path = "support/mod.rs"]
mod support;
use support::events::{drain_until, next_permission};
use support::state;

use tokio::io::AsyncWriteExt;
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
        harness_home: "/root".into(),
        mcp_servers: vec![serde_json::json!({
            "type": "http",
            "name": "tracon",
            "url": "http://gw:7421/mcp/s1",
            "headers": [{ "name": "Authorization", "value": "Bearer tok" }],
        })],
        tools: Vec::new(),
        env,
        system_prompt_file: None,
        cursor: None,
    }
}

#[tokio::test]
async fn version_is_parsed_from_the_runner() {
    state::isolate();
    let a = ClaudeAdapter::new("2.1.247");
    let v = a.version(&FakeRunner).await.unwrap();
    assert_eq!(v.found, "2.1.247");
    assert!(v.matches());
}

#[tokio::test]
async fn launch_prompt_permission_and_turn_result() {
    state::isolate();
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, mut rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    assert!(!handle.harness_session_id().is_empty());

    let turn = tokio::spawn(async move { handle.prompt("do the thing".into()).await });

    let mut seen = Vec::new();
    let ev = next_permission(&mut rx, &mut seen).await;
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
    drain_until(&mut rx, &mut after, "tool_update:completed").await;
    assert!(
        after.contains(&"tool_update:completed".to_string()),
        "{after:?}"
    );
}

#[tokio::test]
async fn denying_a_permission_fails_the_tool_call() {
    state::isolate();
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, mut rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    let turn = tokio::spawn(async move { handle.prompt("do it".into()).await });

    let mut seen = Vec::new();
    let ev = next_permission(&mut rx, &mut seen).await;
    let HarnessEvent::Permission { reply, .. } = ev else {
        unreachable!()
    };
    reply
        .send(PermissionReply::Selected("reject_once".into()))
        .unwrap();

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await;
    let mut after = Vec::new();
    drain_until(&mut rx, &mut after, "tool_update:failed").await;
    assert!(
        after.contains(&"tool_update:failed".to_string()),
        "{after:?}"
    );
}

/// A permission that expires is a deny, never a silent allow: the supervisor
/// drops the reply channel, and the harness must still be told.
#[tokio::test]
async fn an_expired_permission_denies_rather_than_hanging() {
    state::isolate();
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, mut rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    let turn = tokio::spawn(async move { handle.prompt("do it".into()).await });

    let mut seen = Vec::new();
    let ev = next_permission(&mut rx, &mut seen).await;
    let HarnessEvent::Permission { reply, .. } = ev else {
        unreachable!()
    };
    drop(reply);

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await;
    let mut after = Vec::new();
    drain_until(&mut rx, &mut after, "tool_update:failed").await;
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
    state::isolate();
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
    state::isolate();
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
    state::isolate();
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, _rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    let id = handle.harness_session_id();
    assert!(uuid::Uuid::parse_str(id).is_ok(), "{id} is not a uuid");
}

#[tokio::test]
async fn a_session_takes_more_than_one_turn() {
    state::isolate();
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, mut rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    let handle = std::sync::Arc::new(handle);

    for round in 0..2 {
        let h = handle.clone();
        let turn = tokio::spawn(async move { h.prompt(format!("round {round}")).await });
        let mut seen = Vec::new();
        let ev = next_permission(&mut rx, &mut seen).await;
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

/// Claude Code's stream-json carries no version field today, so absence means
/// the revision these shapes were read from. A CLI that starts naming another
/// one is refused with both sides in the reason rather than decoded on the
/// chance that nothing moved.
#[tokio::test]
async fn a_stream_json_revision_the_node_does_not_speak_refuses_to_launch() {
    state::isolate();
    let a = ClaudeAdapter::new("2.1.247");
    let spec = spec_env(vec![("FAKE_CLAUDE_PROTOCOL".into(), "4".into())]);
    let err = match a.launch(&FakeRunner, spec).await {
        Ok(_) => panic!("an unsupported protocol must not launch"),
        Err(e) => e,
    };
    assert!(
        matches!(err, AdapterError::IncompatibleProtocol { .. }),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        "harness claude reports claude-stream-json protocol 4; this node supports 1"
    );
}

/// What a compatible handshake leaves behind for the session row.
#[tokio::test]
async fn a_compatible_handshake_reports_what_it_ran() {
    state::isolate();
    let a = ClaudeAdapter::new("2.1.247");
    let (handle, _rx) = a.launch(&FakeRunner, spec()).await.unwrap();
    let compat = handle.compat();
    assert_eq!(compat.agent, "claude");
    assert_eq!(compat.version, "2.1.247");
    assert_eq!(compat.protocol, "claude-stream-json/1");
}

/// The login runner. `claude setup-token` is spawned inside `script(1)` for a
/// pty, which no test can rely on being spelled the same way on every host, so
/// the fake stands in for the whole wrapper — and the command the adapter
/// actually built is kept, because what is in it is half of what is under test.
#[derive(Default)]
struct LoginRunner {
    spawned: std::sync::Mutex<Option<RunnerCommand>>,
}

#[async_trait::async_trait]
impl Runner for LoginRunner {
    async fn spawn(
        &self,
        cmd: RunnerCommand,
    ) -> Result<tracon::runner::Spawned, tracon::runner::RunnerError> {
        *self.spawned.lock().unwrap() = Some(cmd.clone());
        LocalRunner
            .spawn(RunnerCommand {
                argv: vec![env!("CARGO_BIN_EXE_fake_setup_token").to_string()],
                env: cmd.env,
                name: cmd.name,
                ..Default::default()
            })
            .await
    }

    async fn run_capture(
        &self,
        _cmd: RunnerCommand,
    ) -> Result<std::process::Output, tracon::runner::RunnerError> {
        Err(tracon::runner::RunnerError::Other("not used".into()))
    }

    async fn kill(&self, _name: &str) -> Result<(), tracon::runner::RunnerError> {
        Ok(())
    }
}

impl LoginRunner {
    fn command(&self) -> RunnerCommand {
        self.spawned.lock().unwrap().clone().expect("a login ran")
    }
}

fn env_of(cmd: &RunnerCommand, key: &str) -> String {
    cmd.env
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| panic!("{key} is set on the login command"))
}

/// The whole subscription login: a URL for the operator, a code pasted back,
/// and a token the broker can be given. What `lift` returns is what the CLI
/// printed — there is no store behind it.
#[tokio::test]
async fn setup_token_mints_a_subscription_token_from_a_pasted_code() {
    state::isolate();
    let adapter = ClaudeAdapter::new("2.1.247");
    let runner = LoginRunner::default();
    let mut flow = adapter
        .login(
            &runner,
            "anthropic",
            "tracon-login-anthropic-1",
            "/root/.claude",
        )
        .await
        .unwrap();
    assert!(
        flow.url
            .starts_with("https://claude.com/cai/oauth/authorize?code=true"),
        "{}",
        flow.url
    );
    // The redirect is a hosted callback, not localhost: there is nothing for a
    // local listener to catch and no device code to show, so `connect` falls
    // back to the paste-back — which is this login's only completion.
    assert!(flow.device_code.is_none());
    assert!(
        tracon::providers::callback::CallbackTarget::parse(&flow.url).is_err(),
        "{} looked like a loopback callback",
        flow.url
    );

    // Exactly what `Providers::submit` writes, newline and all.
    flow.stdin.write_all(b"the-code\n").await.unwrap();
    flow.stdin.flush().await.unwrap();
    let exit = tokio::time::timeout(std::time::Duration::from_secs(30), flow.done)
        .await
        .expect("the login ended")
        .unwrap();
    assert_eq!(exit, 0);

    let before = tracon::store::now_ms();
    let token = adapter
        .lift(std::path::Path::new("/nonexistent"), "anthropic")
        .await
        .unwrap();
    assert_eq!(token.access, "sk-ant-oat01-the-code");
    assert!(token.refresh.is_none());
    let expires = token.expires_ms.expect("a lifetime was recorded");
    assert!(expires > before, "{expires} is not in the future");
    // 364 days, as the CLI said — not the assumed year.
    assert!(expires < before + 365 * 24 * 60 * 60 * 1000, "{expires}");

    // Printed once and stored nowhere: a second lift has nothing to give.
    assert!(adapter
        .lift(std::path::Path::new("/nonexistent"), "anthropic")
        .await
        .is_err());
}

/// The login helper is a throwaway. It must never be pointed at the state a
/// session mounts: that directory is shared by every harness this node runs,
/// and a half-finished sign-in has no business in it.
#[tokio::test]
async fn the_login_helper_keeps_its_state_out_of_any_session_directory() {
    state::isolate();
    let adapter = ClaudeAdapter::new("2.1.247").with_login_image(Some("localhost/claude".into()));
    let runner = LoginRunner::default();
    let flow = adapter
        .login(
            &runner,
            "anthropic",
            "tracon-login-anthropic-1",
            "/root/.claude",
        )
        .await
        .unwrap();
    drop(flow);

    let cmd = runner.command();
    let home = env_of(&cmd, "HOME");
    let config = env_of(&cmd, "CLAUDE_CONFIG_DIR");
    assert!(home.starts_with("/tmp/tracon-setup-token-"), "{home}");
    assert!(config.starts_with(&home), "{config} is not under {home}");
    for harness_home in ["/root", "/home/harness"] {
        let session =
            tracon::session::materialize::state_target(harness_home, ClaudeAdapter::layout());
        assert_ne!(config, session);
        assert_ne!(home, session);
    }

    // A pty, wide enough that the URL is printed on one line, and the image
    // that carries the CLI when the node's own harness is something else.
    let argv = cmd.argv.join(" ");
    assert!(argv.starts_with("script -q -e -c "), "{argv}");
    assert!(argv.contains("stty cols"), "{argv}");
    assert!(argv.contains("claude setup-token"), "{argv}");
    assert_eq!(cmd.image.as_deref(), Some("localhost/claude"));
}

/// A `setup-token` token has no refresh token behind it. Saying so as its own
/// error is what stops the refresh loop asking forever.
#[tokio::test]
async fn a_setup_token_credential_can_only_be_replaced_by_signing_in_again() {
    state::isolate();
    let adapter = ClaudeAdapter::new("2.1.247");
    let err = adapter
        .refresh(
            &LoginRunner::default(),
            "anthropic",
            "tracon-refresh-anthropic",
        )
        .await
        .expect_err("there is nothing to refresh");
    assert!(matches!(err, AdapterError::ReconnectRequired(_)), "{err:?}");
    assert!(
        err.to_string().contains("connect the provider again"),
        "{err}"
    );
}

/// Claude Code mints an Anthropic subscription token and nothing else; asking
/// it for another provider's login must fail rather than run the command and
/// store whatever came back under the wrong name.
#[tokio::test]
async fn the_claude_login_refuses_a_provider_it_cannot_sign_in_to() {
    state::isolate();
    let adapter = ClaudeAdapter::new("2.1.247");
    let err = match adapter
        .login(
            &LoginRunner::default(),
            "openai-codex",
            "tracon-login-x",
            "/root/.claude",
        )
        .await
    {
        Ok(_) => panic!("Claude Code signs in to anthropic and nothing else"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("openai-codex"), "{err}");
}
