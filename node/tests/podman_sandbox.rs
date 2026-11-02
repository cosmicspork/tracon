//! The Podman sandbox provider and the protocol runner against real Podman:
//! commands and a native harness session run in a container whose only
//! process the node started is `tracon-sandbox`, with everything else over
//! the sandbox protocol.
//!
//! Runs only when asked for:
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 TRACON_SANDBOX_BUNDLE=<bundle>/<version>/amd64 \
//!   cargo test -p tracon --test podman_sandbox -- --test-threads 1
//! ```
//!
//! The bundle needs `bin/tracon-sandbox` and `bin/tracon-agent` built from
//! this tree (`scripts/harness-bundle.sh --sandbox`). The image is
//! `TRACON_SANDBOX_TEST_IMAGE` (default `docker.io/library/ubuntu:24.04`,
//! which must be in the local store). Everything the tests make is named
//! with `TRACON_SANDBOX_TEST_PREFIX` (default `tracon-test-sbx-`) and removed
//! again; nothing of a node's is used.

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tracon::config::{Config, HarnessListen};
use tracon::runner::podman::RunSpec;
use tracon::runner::{Mount, Runner, RunnerCommand};

use support::podman::{podman, requested};

struct Rig {
    prefix: String,
    cfg: Config,
    image: String,
    network: String,
    volumes: Vec<String>,
    _short: tempfile::TempDir,
}

impl Rig {
    /// A network of the test's own (internal when asked), a configuration
    /// running Podman over the sandbox protocol on it, and somewhere short
    /// for the sockets.
    fn new(tag: &str, internal: bool) -> Option<Rig> {
        if !requested() {
            return None;
        }
        support::state::isolate();
        let bundle = std::env::var("TRACON_SANDBOX_BUNDLE")
            .map(PathBuf::from)
            .expect("TRACON_PODMAN_TESTS=1 needs TRACON_SANDBOX_BUNDLE");
        assert!(bundle.join("bin/tracon-sandbox").is_file());
        let image = std::env::var("TRACON_SANDBOX_TEST_IMAGE")
            .unwrap_or_else(|_| "docker.io/library/ubuntu:24.04".into());
        assert!(podman(&["image", "exists", &image]).is_some(), "{image}");
        let prefix = format!(
            "{}{tag}-",
            std::env::var("TRACON_SANDBOX_TEST_PREFIX")
                .unwrap_or_else(|_| "tracon-test-sbx-".into())
        );
        let network = format!("{prefix}net");
        let _ = podman(&["network", "rm", "-f", &network]);
        let mut create = vec!["network", "create", "--disable-dns"];
        if internal {
            create.push("--internal");
        }
        create.push(&network);
        assert!(podman(&create).is_some(), "create {network}");
        let short = tempfile::Builder::new()
            .prefix("sbx")
            .tempdir_in("/tmp")
            .unwrap();
        let mut cfg = Config::default();
        cfg.boundary.network = network.clone();
        cfg.boundary.gateway_container = format!("{prefix}gw");
        cfg.boundary.harness_image = image.clone();
        cfg.gateway.harness_listen = HarnessListen::Unix(short.path().join("harness.sock"));
        cfg.runtime.podman.sandbox_protocol = true;
        cfg.sandbox.bundle = Some(bundle);
        Some(Rig {
            prefix,
            cfg,
            image,
            network,
            volumes: Vec::new(),
            _short: short,
        })
    }

    fn name(&self, what: &str) -> String {
        format!("{}{what}", self.prefix)
    }

    fn volume(&mut self, what: &str) -> String {
        let name = self.name(what);
        let _ = podman(&["volume", "rm", "-f", &name]);
        self.volumes.push(name.clone());
        name
    }

    fn runner(&self, mounts: Vec<Mount>) -> Arc<dyn Runner> {
        let mut spec = RunSpec::from_config(&self.cfg, true);
        spec.image = self.image.clone();
        spec.extra_mounts = mounts;
        tracon::boundary::podman::protocol_runner(&self.cfg, spec)
    }

    fn command(&self, what: &str, script: &str) -> RunnerCommand {
        RunnerCommand {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            name: self.name(what),
            ..Default::default()
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(ids) = podman(&["ps", "-aq", "--filter", &format!("name=^{}", self.prefix)]) {
            for id in ids.lines() {
                let _ = podman(&["rm", "-f", id]);
            }
        }
        for volume in &self.volumes {
            let _ = podman(&["volume", "rm", "-f", volume]);
        }
        let _ = podman(&["network", "rm", "-f", &self.network]);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test]
async fn a_command_runs_confined_with_the_bundle_read_only_and_the_supervisor_as_pid_1() {
    let Some(mut rig) = Rig::new("cmd", true) else {
        return;
    };
    let work = rig.volume("work");
    let runner = rig.runner(Vec::new());
    let mut cmd = rig.command(
        "probe",
        "tr '\\0' ' ' </proc/1/cmdline; echo; cat /proc/self/attr/current; echo; \
         id -u; pwd; echo \"$GREETING $LENT\"; \
         if touch /opt/tracon/x 2>/dev/null; then echo bundle-writable; else echo bundle-read-only; fi; \
         if ls /run/podman-init >/dev/null 2>&1; then echo has-init; fi; \
         echo kept > /work/kept; echo to-stderr >&2; exit 5",
    );
    cmd.env = vec![("GREETING".into(), "hello".into())];
    cmd.secret_env = tracon::runner::SecretEnv::new(vec![("LENT".into(), "lent-value".into())]);
    cmd.mounts = vec![Mount::volume(&work, "/work", false)];
    let out = runner.run_capture(cmd).await.unwrap();
    let stdout = text(&out.stdout);
    eprintln!("{stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines[0].starts_with("/opt/tracon/bin/tracon-sandbox"),
        "{stdout}"
    );
    assert!(lines[1].contains(":container_t:"), "confined: {stdout}");
    assert_eq!(lines[2], "0");
    assert_eq!(lines[3], "/work");
    assert_eq!(lines[4], "hello lent-value");
    assert_eq!(lines[5], "bundle-read-only");
    assert_eq!(text(&out.stderr), "to-stderr\n");
    assert_eq!(out.status.code(), Some(5));

    // The sandbox is gone, and with it its socket directory; the volume
    // kept what was written.
    let name = runner.capture_name(&rig.name("probe"));
    assert!(podman(&["container", "exists", &name]).is_none());
    let sockets = rig.cfg.sandbox_socket_dir();
    assert_eq!(std::fs::read_dir(&sockets).unwrap().count(), 0);
    let mut again = rig.command("again", "cat /work/kept");
    again.mounts = vec![Mount::volume(&work, "/work", true)];
    assert_eq!(
        text(&runner.run_capture(again).await.unwrap().stdout),
        "kept\n"
    );
}

#[tokio::test]
async fn under_init_the_supervisor_is_a_child_and_still_runs_the_command() {
    let Some(mut rig) = Rig::new("init", true) else {
        return;
    };
    rig.cfg.runtime.podman.sandbox_init = true;
    let out = rig
        .runner(Vec::new())
        .run_capture(rig.command(
            "init",
            "tr '\\0' ' ' </proc/1/cmdline; echo; ps -o pid=,args= -p $PPID 2>/dev/null || cat /proc/$PPID/cmdline",
        ))
        .await
        .unwrap();
    let stdout = text(&out.stdout);
    eprintln!("{stdout}");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let mut lines = stdout.lines();
    assert!(
        lines.next().unwrap().starts_with("/run/podman-init "),
        "{stdout}"
    );
    // The command's parent, the supervisor, is not PID 1.
    let parent = lines.next().unwrap().trim();
    assert!(parent.contains("tracon-sandbox"), "{stdout}");
    assert!(!parent.starts_with("1 "), "{stdout}");
}

#[tokio::test]
async fn a_harness_streams_stdio_with_backpressure_and_ends_when_killed() {
    let Some(rig) = Rig::new("stdio", true) else {
        return;
    };
    let runner = rig.runner(Vec::new());
    let spawned = runner.spawn(rig.command("cat", "exec cat")).await.unwrap();
    let mut stdin = spawned.stdin;
    let mut stdout = spawned.stdout;
    // Far more than any window: the writer can only finish if the reader
    // keeps taking.
    let sent: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let expected = sent.clone();
    let writer = tokio::spawn(async move {
        stdin.write_all(&sent).await.unwrap();
        stdin.shutdown().await.unwrap();
    });
    let mut got = Vec::new();
    stdout.read_to_end(&mut got).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got.len(), expected.len());
    assert!(got == expected, "the bytes came back changed");
    assert_eq!(spawned.done.await.unwrap(), 0);

    let long = runner
        .spawn(rig.command("sleeper", "exec sleep 600"))
        .await
        .unwrap();
    let name = rig.name("sleeper");
    assert!(podman(&["container", "exists", &name]).is_some());
    // A runner made afresh, as the session's kill path makes one.
    rig.runner(Vec::new()).kill(&name).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(30), long.done)
        .await
        .expect("the harness outlived its sandbox");
    assert!(ended.is_err() || ended.unwrap() != 0);
    assert!(podman(&["container", "exists", &name]).is_none());
}

/// A fake model on `path`, answering each request with the next of
/// `script` (and an empty body once it runs out).
async fn fake_model(
    path: &str,
    script: Vec<String>,
) -> (u16, Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
    use axum::routing::post;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let replies = Arc::new(std::sync::Mutex::new(script));
    let app = axum::Router::new().route(
        path,
        post({
            let requests = requests.clone();
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let requests = requests.clone();
                let replies = replies.clone();
                async move {
                    requests.lock().unwrap().push(body);
                    let next = {
                        let mut replies = replies.lock().unwrap();
                        match replies.is_empty() {
                            true => String::new(),
                            false => replies.remove(0),
                        }
                    };
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        next,
                    )
                }
            }
        }),
    );
    // On every address: the sandbox reaches the host through Podman's
    // `host.containers.internal`, which is not the host's loopback.
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, requests)
}

fn sse(chunks: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

/// Runs the bundle's copy of a harness in place of the image's.
struct FromBundle(&'static str, Arc<dyn Runner>);

#[async_trait::async_trait]
impl Runner for FromBundle {
    async fn spawn(
        &self,
        mut cmd: RunnerCommand,
    ) -> Result<tracon::runner::Spawned, tracon::runner::RunnerError> {
        cmd.argv[0] = format!("/opt/tracon/bin/{}", self.0);
        self.1.spawn(cmd).await
    }
    async fn run_capture(
        &self,
        mut cmd: RunnerCommand,
    ) -> Result<std::process::Output, tracon::runner::RunnerError> {
        cmd.argv[0] = format!("/opt/tracon/bin/{}", self.0);
        self.1.run_capture(cmd).await
    }
    async fn kill(&self, name: &str) -> Result<(), tracon::runner::RunnerError> {
        self.1.kill(name).await
    }
}

#[tokio::test]
async fn a_native_session_runs_a_turn_with_a_tool_call_over_the_protocol() {
    use serde_json::json;
    use tracon::adapter::{
        native::NativeAdapter, HarnessAdapter, HarnessEvent, LaunchSpec, PermissionReply,
    };

    // Not internal: the fake model is on the host, where the gateway would
    // be.
    let Some(mut rig) = Rig::new("native", false) else {
        return;
    };
    let (port, requests) = fake_model("/v1/chat/completions", vec![
        sse(&[
            json!({"id": "req-1", "choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1",
                "type": "function", "function": {"name": "shell",
                "arguments": json!({"command": "uname -s > /work/where; echo ran"}).to_string()}}]},
                "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10}}),
        ]),
        sse(&[
            json!({"choices": [{"delta": {"content": "done inside"}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 120, "completion_tokens": 3}}),
        ]),
    ])
    .await;

    // The agent's sealed configuration, as the node materialises it.
    let state = rig.volume("agent-state");
    let work = rig.volume("work");
    let providers = json!([{
        "name": "fake",
        "shape": "openai",
        "base_url": format!("http://host.containers.internal:{port}"),
        "key_env": "TRACON_PROVIDER_KEY_FAKE",
        "models": [{"id": "m", "context": 32000, "parallel_tools": true}],
    }]);
    let seed = rig.runner(Vec::new());
    let mut write = rig.command(
        "seed",
        &format!(
            "mkdir -p /s/config /s/run && printf '%s' '{providers}' > /s/config/providers.json"
        ),
    );
    write.mounts = vec![Mount::volume(&state, "/s", false)];
    let seeded = seed.run_capture(write).await.unwrap();
    assert!(seeded.status.success(), "{}", text(&seeded.stderr));

    let runner = FromBundle(
        "tracon-agent",
        rig.runner(vec![
            Mount::volume(&state, "/root/.tracon-agent", false),
            Mount::volume(&work, "/work", false),
        ]),
    );
    let spec = LaunchSpec {
        secret_env: tracon::runner::SecretEnv::new(vec![(
            "TRACON_PROVIDER_KEY_FAKE".into(),
            "not-a-real-key".into(),
        )]),
        cwd_in_runner: "/work".into(),
        model: "fake/m".into(),
        container_name: rig.name("session"),
        harness_home: "/root".into(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        // Straight to the fake: there is no gateway proxy on this network.
        env: vec![
            ("NO_PROXY".into(), "host.containers.internal".into()),
            ("no_proxy".into(), "host.containers.internal".into()),
        ],
        system_prompt_file: None,
        resume: None,
        labels: Default::default(),
    };
    let adapter = NativeAdapter::new(NativeAdapter::PINNED_VERSION);
    let (handle, mut rx) = adapter.launch(&runner, spec).await.unwrap();
    assert_eq!(handle.compat().agent, "tracon-agent");
    let turn = tokio::spawn(async move { handle.prompt("say where you are".into()).await });

    let permission = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match rx.recv().await.expect("events ended") {
                HarnessEvent::Permission { request, reply } => break (request, reply),
                other => eprintln!("event: {other:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "no permission request; the model was asked {} times",
            requests.lock().unwrap().len()
        )
    });
    let (request, reply) = permission;
    assert_eq!(
        request.command.as_deref(),
        Some("uname -s > /work/where; echo ran")
    );
    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(60), turn)
        .await
        .expect("the turn ended")
        .unwrap()
        .unwrap();
    assert_eq!(result.stop_reason, "end_turn");
    assert_eq!(requests.lock().unwrap().len(), 2);
    // The tool's result went back to the model.
    let second = requests.lock().unwrap()[1].to_string();
    assert!(second.contains("ran"), "{second}");

    // What the tool did, it did inside the sandbox, on the session's volume.
    let mut read = rig.command("read", "cat /work/where");
    read.mounts = vec![Mount::volume(&work, "/work", true)];
    assert_eq!(
        text(&seed.run_capture(read).await.unwrap().stdout),
        "Linux\n"
    );
    let _ = rig.runner(Vec::new()).kill(&rig.name("session")).await;
}

#[tokio::test]
async fn the_boundary_probe_checks_pass_for_a_sandbox() {
    let Some(mut rig) = Rig::new("checks", true) else {
        return;
    };
    // A gateway for the network check to find: any container at the
    // configured address will do.
    let subnet = podman(&[
        "network",
        "inspect",
        &rig.network,
        "--format",
        "{{range .Subnets}}{{.Subnet}}{{end}}",
    ])
    .unwrap();
    let base = subnet
        .split('/')
        .next()
        .unwrap()
        .rsplit_once('.')
        .unwrap()
        .0
        .to_string();
    rig.cfg.boundary.gateway_ip = format!("{base}.200");
    assert!(podman(&[
        "run",
        "-d",
        "--name",
        &rig.cfg.boundary.gateway_container,
        "--network",
        &rig.network,
        "--ip",
        &rig.cfg.boundary.gateway_ip,
        &rig.image,
        "sleep",
        "300",
    ])
    .is_some());
    let state = rig.volume("state");
    let mut spec = RunSpec::from_config(&rig.cfg, true);
    spec.image = rig.image.clone();
    spec.extra_mounts = vec![Mount::volume(&state, "/root/.claude", false)];
    let checks =
        tracon::boundary::podman::checks::probe_checks(&rig.cfg, &spec, &rig.name("probe")).await;
    for check in &checks {
        eprintln!("{:?}: {} {}", check.id, check.ok, check.detail);
    }
    assert_eq!(checks.len(), 3);
    assert!(checks.iter().all(|c| c.ok));
    assert!(podman(&["container", "exists", &rig.name("probe")]).is_none());
}

/// An Anthropic Messages stream answering with `text`.
fn anthropic_answer(text: &str) -> String {
    use serde_json::json;
    let events = [
        (
            "message_start",
            json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-test",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 50, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": text}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 5}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ];
    events
        .iter()
        .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
        .collect()
}

#[tokio::test]
async fn a_claude_code_session_runs_a_turn_over_the_protocol() {
    use tracon::adapter::{claude::ClaudeAdapter, HarnessAdapter, LaunchSpec};

    let Some(mut rig) = Rig::new("claude", false) else {
        return;
    };
    assert!(
        rig.cfg
            .sandbox
            .bundle
            .as_ref()
            .unwrap()
            .join("bin/claude")
            .is_file(),
        "the bundle has no Claude Code"
    );
    let (port, requests) = fake_model(
        "/v1/messages",
        (0..8)
            .map(|_| anthropic_answer("hello from the fake"))
            .collect(),
    )
    .await;
    let state = rig.volume("claude-state");
    let work = rig.volume("work");
    let seed = rig.runner(Vec::new());
    let mut write = rig.command(
        "seed",
        "mkdir -p /s/plugin/.claude-plugin && \
         printf '%s' '{\"name\":\"tracon\",\"version\":\"1.0.0\"}' > /s/plugin/.claude-plugin/plugin.json",
    );
    write.mounts = vec![Mount::volume(&state, "/s", false)];
    let seeded = seed.run_capture(write).await.unwrap();
    assert!(seeded.status.success(), "{}", text(&seeded.stderr));

    let runner = FromBundle(
        "claude",
        rig.runner(vec![
            Mount::volume(&state, "/root/.claude", false),
            Mount::volume(&work, "/work", false),
        ]),
    );
    let base = format!("http://host.containers.internal:{port}");
    let spec = LaunchSpec {
        secret_env: tracon::runner::SecretEnv::new(vec![(
            "ANTHROPIC_API_KEY".into(),
            "not-a-real-key".into(),
        )]),
        cwd_in_runner: "/work".into(),
        model: "anthropic/claude-test".into(),
        container_name: rig.name("session"),
        harness_home: "/root".into(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        env: vec![
            ("ANTHROPIC_BASE_URL".into(), base),
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
                "1".into(),
            ),
            ("NO_PROXY".into(), "host.containers.internal".into()),
            ("no_proxy".into(), "host.containers.internal".into()),
        ],
        system_prompt_file: None,
        resume: None,
        labels: Default::default(),
    };
    let adapter = ClaudeAdapter::new(ClaudeAdapter::PINNED_VERSION);
    let (handle, mut rx) = adapter.launch(&runner, spec).await.unwrap();
    let events = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            eprintln!("event: {event:?}");
        }
    });
    let result = tokio::time::timeout(Duration::from_secs(120), handle.prompt("say hello".into()))
        .await
        .expect("the turn ended")
        .unwrap();
    eprintln!("{result:?}");
    assert!(!requests.lock().unwrap().is_empty());
    assert_eq!(result.stop_reason, "end_turn");
    drop(handle);
    let _ = rig.runner(Vec::new()).kill(&rig.name("session")).await;
    events.abort();
}
