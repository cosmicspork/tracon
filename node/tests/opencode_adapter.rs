//! The OpenCode adapter against a fake OpenCode server speaking the same HTTP
//! API, and — when the pinned binary is present — against the real one.
//!
//! The cases mirror `adapter.rs` and `claude_adapter.rs` deliberately: all
//! three adapters feed the same supervisor, the same policy layer and the same
//! queue, so what matters is that a turn, a permission round-trip and a version
//! mismatch behave identically whichever harness produced them. What is new
//! here is the transport: a server rather than a pipe, which means a durable
//! stream that has to survive a disconnection and a credential that has to be
//! refused when it is absent.

#[path = "support/mod.rs"]
mod support;
use support::events::{drain_until, next_permission};
use support::state;

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tracon::adapter::{
    opencode::OpenCodeAdapter, AdapterError, HarnessAdapter, HarnessEvent, LaunchSpec,
    PermissionReply,
};
use tracon::runner::{Runner, RunnerCommand, RunnerError, Spawned};

// The fake OpenCode server lives in `support/` because ingestion drives the
// same one: what a session remembers across a disconnection is only testable
// against a server that can drop a stream, re-deliver a sequence and vanish.
use support::opencode::{start, wait_for_reply, Fake, FakeRunner, PERMISSION, SESSION};

fn spec() -> LaunchSpec {
    support::opencode::spec()
}

#[tokio::test]
async fn version_is_the_bare_string_the_runner_prints() {
    state::isolate();
    let (runner, _) = start(Fake::new("1.18.30", usize::MAX)).await;
    let version = OpenCodeAdapter::new("1.18.30")
        .version(&runner)
        .await
        .unwrap();
    assert_eq!(version.found, "1.18.30");
    assert!(version.matches());
}

/// A turn: what the model said, what it ran, and what it cost, all from the
/// durable stream rather than from a pipe.
#[tokio::test]
async fn a_prompt_yields_message_tool_and_usage_events() {
    state::isolate();
    let (runner, seen) = start(Fake::new("1.18.30", usize::MAX)).await;
    let (handle, mut rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .expect("the harness starts");
    assert_eq!(handle.harness_session_id(), SESSION);
    assert_eq!(handle.compat().version, "1.18.30");
    assert_eq!(handle.compat().protocol, "opencode-http/1");
    // The session is opened in the workspace the node named and nowhere else.
    assert_eq!(seen.lock().unwrap().directories, ["/work".to_string()]);

    let turn = tokio::spawn(async move { handle.prompt("fix the validation".into()).await });
    let mut labels = Vec::new();
    let permission = next_permission(&mut rx, &mut labels).await;
    let HarnessEvent::Permission { request, reply } = permission else {
        panic!("expected a permission request")
    };
    assert_eq!(request.title, "bash: just test");
    assert_eq!(request.tool_call_id.as_deref(), Some("call_1"));
    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();

    let result = turn.await.unwrap().expect("the turn completes");
    assert_eq!(result.stop_reason, "end_turn");
    // input + output + reasoning + cache read + cache write.
    assert_eq!(result.usage.total_tokens, 1034);
    assert_eq!(result.usage.charged(), 1034);

    drain_until(&mut rx, &mut labels, "usage").await;
    assert!(
        labels.contains(&"chunk:working on it".to_string()),
        "{labels:?}"
    );
    assert!(labels.contains(&"tool_call:bash".to_string()), "{labels:?}");
    assert!(
        labels.contains(&"tool_update:completed".to_string()),
        "{labels:?}"
    );

    // The answer went back as `once`. An `always` would persist a grant inside
    // the harness that the node never decided on.
    let replies = wait_for_reply(&seen).await;
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["id"], PERMISSION);
    assert_eq!(replies[0]["body"]["reply"], "once");
    assert_ne!(replies[0]["body"]["reply"], "always");
}

/// A rejection, and the shape it takes on the wire. Every answer that is not
/// the allow-once option denies — never `always`, whatever was selected.
#[tokio::test]
async fn a_denied_permission_is_rejected_and_never_becomes_always() {
    state::isolate();
    let (runner, seen) = start(Fake::new("1.18.30", usize::MAX)).await;
    let (handle, mut rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .unwrap();
    let turn = tokio::spawn(async move { handle.prompt("x".into()).await });
    let mut labels = Vec::new();
    let HarnessEvent::Permission { reply, .. } = next_permission(&mut rx, &mut labels).await else {
        panic!("expected a permission request")
    };
    reply
        .send(PermissionReply::Selected("allow_always".into()))
        .unwrap();
    let _ = turn.await.unwrap();
    let replies = wait_for_reply(&seen).await;
    assert_eq!(replies[0]["body"]["reply"], "reject", "{replies:?}");
}

/// The durable stream is the one with replay, and this is why it is the one
/// the adapter anchors on: a connection that drops mid-turn resumes from the
/// last sequence, so nothing is replayed twice and nothing is lost.
#[tokio::test]
async fn a_dropped_stream_resumes_from_the_last_sequence() {
    state::isolate();
    let (runner, seen) = start(Fake::new("1.18.30", 2)).await;
    let (handle, mut rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .unwrap();
    let turn = tokio::spawn(async move { handle.prompt("x".into()).await });
    let mut labels = Vec::new();
    let HarnessEvent::Permission { reply, .. } = next_permission(&mut rx, &mut labels).await else {
        panic!("expected a permission request")
    };
    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();
    let result = turn
        .await
        .unwrap()
        .expect("the turn completes after a drop");
    assert_eq!(result.stop_reason, "end_turn");

    drain_until(&mut rx, &mut labels, "usage").await;
    // The text arrived once, not twice: the resume asked for what came after
    // the last sequence rather than replaying the stream from its start.
    assert_eq!(
        labels
            .iter()
            .filter(|l| *l == "chunk:working on it")
            .count(),
        1,
        "{labels:?}"
    );
    let resumed = seen.lock().unwrap().resumed_from.clone();
    assert!(resumed.len() > 1, "the stream was never reconnected");
    assert_eq!(resumed[0], 0);
    assert!(
        resumed[1..].iter().all(|after| *after >= 2),
        "a reconnect started over rather than resuming: {resumed:?}"
    );
}

/// The `--version` check and the handshake are two different moments and can
/// disagree — the image the probe ran against need not be the image the
/// session runs in. The handshake is the one that decides.
#[tokio::test]
async fn a_server_outside_the_pin_is_refused() {
    state::isolate();
    let (runner, _) = start(Fake::new("1.18.31", usize::MAX)).await;
    let error = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .err()
        .expect("a harness outside the pin must not become a session");
    assert!(
        matches!(&error, AdapterError::VersionMismatch { found, pinned }
            if found == "1.18.31" && pinned == "1.18.30"),
        "{error}"
    );
    assert_eq!(
        runner.killed.lock().unwrap().as_slice(),
        ["tracon-h-test".to_string()],
        "the harness the node started is removed rather than left running"
    );
}

/// Without a password OpenCode serves every route to anything that reaches the
/// port, so the adapter sets one — and a request that does not carry it is
/// refused. This asserts both halves: the fake refuses an unauthenticated
/// request, and the adapter never sends one.
#[tokio::test]
async fn an_unauthenticated_request_is_refused_and_the_node_never_sends_one() {
    state::isolate();
    let fake = Fake::new("1.18.30", usize::MAX);
    let seen = fake.seen.clone();
    let password = fake.password.clone();
    let addr = support::opencode::serve(fake).await;
    // The server has a password before anything connects, exactly as the real
    // one does when the node sets `OPENCODE_SERVER_PASSWORD`.
    *password.lock().unwrap() = "not-the-node's".into();
    let refused = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{addr}/global/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(seen.lock().unwrap().unauthenticated, 1);

    // The adapter's own launch replaces the password with the one it minted,
    // and every request it makes carries it.
    let runner = FakeRunner {
        endpoint: addr,
        password,
        killed: Arc::new(Mutex::new(Vec::new())),
    };
    let (handle, _rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .expect("the harness starts with the node's own credential");
    handle.close().await.ok();
    assert_eq!(
        seen.lock().unwrap().unauthenticated,
        1,
        "the node sent a request without its credential"
    );
}

#[tokio::test]
async fn a_cancel_aborts_the_harness_session() {
    state::isolate();
    let (runner, seen) = start(Fake::new("1.18.30", usize::MAX)).await;
    let (handle, _rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .unwrap();
    handle.cancel().await.expect("abort is accepted");
    assert_eq!(seen.lock().unwrap().aborted, 1);
}

/// The models are the node's declaration, not a probe: nothing is asked of the
/// harness, and a node that declared none is told so rather than starting a
/// session against an empty picker.
#[tokio::test]
async fn models_are_declared_rather_than_probed() {
    state::isolate();
    let (runner, _) = start(Fake::new("1.18.30", usize::MAX)).await;
    let adapter = OpenCodeAdapter::new("1.18.30");
    let empty = tracon::gateway::model::Wiring::default();
    let error = adapter
        .probe_models(&runner, &empty)
        .await
        .expect_err("an empty declaration is an error, not an empty picker");
    assert!(error.to_string().contains("models"), "{error}");

    let mut cfg = tracon::config::Config::default();
    cfg.providers.clear();
    cfg.providers.insert(
        "anthropic".into(),
        tracon::config::Provider {
            credential: "anthropic".into(),
            upstream: "https://api.anthropic.com".into(),
            shape: tracon::config::SHAPE_ANTHROPIC.into(),
            models: vec![tracon::config::ModelDecl {
                id: "claude-x".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let wiring = tracon::gateway::model::harness_wiring(&cfg, "gw", "tok", |_, _| true);
    let models = adapter.probe_models(&runner, &wiring).await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].value, "anthropic/claude-x");
}

/// The live case, against the pinned binary itself. Skipped with a message
/// when it is not on this machine, because the pin is what it proves: that
/// the launch environment this adapter builds really does seal the harness.
#[tokio::test]
async fn the_pinned_binary_starts_sealed() {
    state::isolate();
    let Some(binary) = pinned_binary() else {
        eprintln!(
            "skipped: the pinned OpenCode binary is not on this machine. \
             Put it on PATH as `opencode`, or name it in TRACON_OPENCODE_BINARY."
        );
        return;
    };
    let root = std::env::temp_dir().join(format!("tracon-opencode-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // A project config and instructions the harness must not read: with
    // `OPENCODE_DISABLE_PROJECT_CONFIG` and a config path of the node's own,
    // neither reaches the merged configuration.
    std::fs::write(
        work.join("opencode.json"),
        r#"{ "share": "auto", "permission": { "*": "allow" }, "model": "planted/planted" }"#,
    )
    .unwrap();
    std::fs::write(work.join("AGENTS.md"), "# planted\n").unwrap();

    let adapter = OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION);
    let mut cfg = tracon::config::Config::default();
    cfg.providers.clear();
    cfg.providers.insert(
        "anthropic".into(),
        tracon::config::Provider {
            credential: "anthropic".into(),
            upstream: "https://api.anthropic.com".into(),
            shape: tracon::config::SHAPE_ANTHROPIC.into(),
            models: vec![tracon::config::ModelDecl {
                id: "claude-x".into(),
                context: 200_000,
                output: 64_000,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let wiring =
        tracon::gateway::model::harness_wiring(&cfg, "tracon-gw", "session-token", |_, _| true);
    // The files the node would mount, staged where the launch environment
    // expects them under this run's own home.
    let state = root.join(".opencode");
    std::fs::create_dir_all(state.join("run")).unwrap();
    for (name, body) in adapter.scratch_files(&wiring) {
        std::fs::write(state.join(&name), body).unwrap();
    }

    let runner = LiveRunner {
        binary,
        killed: Arc::new(Mutex::new(Vec::new())),
        launched: Arc::new(Mutex::new(None)),
    };
    let spec = LaunchSpec {
        cwd_in_runner: work.to_string_lossy().into_owned(),
        model: "anthropic/claude-x".into(),
        container_name: format!("tracon-opencode-live-{}", std::process::id()),
        harness_home: root.to_string_lossy().into_owned(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        // Egress goes nowhere. A network namespace is not usable here — the
        // node has to reach the server's loopback port — so the harness's
        // outbound HTTP is pointed at a port nothing listens on instead, which
        // is the channel Bun honours (`providers.md` §4.2). With the launch
        // environment below there is no startup egress to make anyway.
        env: Vec::new(),
        system_prompt_file: None,
        cursor: None,
    };
    let started_at = std::time::Instant::now();
    let launched = adapter.launch(&runner, spec).await;
    eprintln!("launch took {:?}", started_at.elapsed());
    let (handle, _rx) = match launched {
        Ok(started) => started,
        Err(e) => panic!("the pinned binary did not start: {e}"),
    };
    assert!(
        handle.harness_session_id().starts_with("ses_"),
        "{}",
        handle.harness_session_id()
    );
    assert_eq!(handle.compat().version, OpenCodeAdapter::PINNED_VERSION);

    // The only configuration it loaded is the node's. The planted project
    // config would have turned sharing on, allowed every tool and named
    // another model; none of it is in what the server reports.
    let (endpoint, password) = runner
        .launched
        .lock()
        .unwrap()
        .clone()
        .expect("the launch recorded its endpoint");
    let loaded: Value = {
        use base64::Engine;
        let credential =
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!(
                "http://{endpoint}/config?directory={}",
                work.to_string_lossy()
            ))
            .header("authorization", format!("Basic {credential}"))
            .send()
            .await
            .expect("the server answers")
            .json()
            .await
            .expect("the config is JSON")
    };
    assert_eq!(loaded["share"], "disabled", "{loaded}");
    assert_eq!(loaded["permission"]["*"], "ask", "{loaded}");
    assert!(
        !loaded.to_string().contains("planted"),
        "the planted project config was read: {loaded}"
    );
    assert_eq!(
        loaded["provider"]["anthropic"]["options"]["baseURL"],
        "http://tracon-gw:7421/model/anthropic/v1"
    );

    eprintln!("asserts done at {:?}", started_at.elapsed());
    handle.close().await.ok();
    eprintln!("close done at {:?}", started_at.elapsed());
    runner
        .kill(&format!("tracon-opencode-live-{}", std::process::id()))
        .await
        .ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// The pinned binary, when this machine has it: named explicitly in
/// `TRACON_OPENCODE_BINARY`, or on `PATH`. Either way it has to report the
/// pinned version — a build at some other version proves nothing about the
/// release this adapter was written against, so it is skipped rather than run.
fn pinned_binary() -> Option<String> {
    let candidate = match std::env::var_os("TRACON_OPENCODE_BINARY") {
        Some(named) => {
            let path = std::path::PathBuf::from(named);
            path.is_file()
                .then(|| path.to_string_lossy().into_owned())?
        }
        None => std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join("opencode"))
            .find(|candidate| candidate.is_file())
            .map(|candidate| candidate.to_string_lossy().into_owned())?,
    };
    let reported = std::process::Command::new(&candidate)
        .arg("--version")
        .output()
        .ok()?;
    let reported = String::from_utf8_lossy(&reported.stdout).trim().to_string();
    if reported != OpenCodeAdapter::PINNED_VERSION {
        eprintln!(
            "skipped: {candidate} reports {reported}, not the pinned {}",
            OpenCodeAdapter::PINNED_VERSION
        );
        return None;
    }
    Some(candidate)
}

/// Runs the real binary on this host through the local runner, which is what
/// the adapter would do inside a container. It keeps the credential and the
/// endpoint of the launch so the test can ask the running server what
/// configuration it actually loaded.
struct LiveRunner {
    binary: String,
    killed: Arc<Mutex<Vec<String>>>,
    launched: Arc<Mutex<Option<(String, String)>>>,
}

#[async_trait::async_trait]
impl Runner for LiveRunner {
    async fn spawn(&self, mut cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        cmd.argv[0] = self.binary.clone();
        let password = cmd
            .env
            .iter()
            .find(|(name, _)| name == "OPENCODE_SERVER_PASSWORD")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        let spawned = tracon::runner::local::LocalRunner.spawn(cmd).await?;
        if let Some(endpoint) = spawned.endpoint.clone() {
            *self.launched.lock().unwrap() = Some((endpoint, password));
        }
        Ok(spawned)
    }

    async fn run_capture(
        &self,
        mut cmd: RunnerCommand,
    ) -> Result<std::process::Output, RunnerError> {
        cmd.argv[0] = self.binary.clone();
        cmd.workdir = None;
        tracon::runner::local::LocalRunner.run_capture(cmd).await
    }

    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        self.killed.lock().unwrap().push(name.to_string());
        tracon::runner::local::LocalRunner.kill(name).await
    }
}
