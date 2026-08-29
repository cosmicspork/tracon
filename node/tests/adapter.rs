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
        .probe_models(&FakeRunner, Vec::new())
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
