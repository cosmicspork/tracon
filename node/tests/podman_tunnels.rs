//! Sandboxes whose only route is the node (`[runtime.podman]
//! sandbox_tunnels`), against real Podman: no network, the protocol's socket
//! the one way out, and the MCP and model gateway, the allowlist proxy and
//! the per-client egress proxy reached through tunnels over that connection
//! to the node's own listeners, served here as the node serves them.
//!
//! Runs only when asked for:
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 TRACON_SANDBOX_BUNDLE=<bundle>/<version>/amd64 \
//!   TRACON_SANDBOX_TEST_IMAGE=<an image with curl> \
//!   cargo test -p tracon --test podman_tunnels -- --ignored --test-threads 1
//! ```
//!
//! The bundle needs `bin/tracon-sandbox` and `bin/claude`. The image needs
//! `curl` and `getent` (Ubuntu 24.04 with curl installed will do). The deep
//! egress check asks `api.anthropic.com` through the node's allowlist proxy,
//! as the real check does, so that one needs the internet. Everything the
//! tests make is named with `TRACON_SANDBOX_TEST_PREFIX` (default
//! `tracon-test-tun-`) and removed again; nothing of a node's is used.

#[path = "support/mod.rs"]
mod support;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tracon::config::{Config, HarnessListen, Provider, SHAPE_ANTHROPIC};
use tracon::gateway::proxy::{Allowlist, GrantSpec, Granted, Grants, Refusal, Resolve};
use tracon::runner::podman::RunSpec;
use tracon::runner::{Mount, Runner, RunnerCommand, SecretEnv};

use support::fake::FakeAdapter;
use support::podman::{podman, requested};

/// What a fake upstream was sent: each request's headers and body.
type Heard = Arc<Mutex<Vec<(Vec<(String, String)>, String)>>>;

struct Rig {
    prefix: String,
    cfg: Config,
    image: String,
    volumes: Vec<String>,
    manager: tracon::session::Manager,
    grants: Grants,
    /// What the model provider behind the model gateway was sent.
    model: Heard,
    /// What `allowed.test` and `nova.test` were sent.
    origin: Heard,
    /// The model provider's answers, in order.
    script: Arc<Mutex<Vec<String>>>,
    _short: tempfile::TempDir,
}

/// Records each request and answers from `script` (an Anthropic stream),
/// or `fallback` once that runs out.
async fn recording_server(
    heard: Heard,
    script: Arc<Mutex<Vec<String>>>,
    fallback: &'static str,
) -> SocketAddr {
    use axum::body::Body;
    use axum::http::Request;
    let app = axum::Router::new().fallback(axum::routing::any(move |req: Request<Body>| {
        let heard = heard.clone();
        let script = script.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(body, 1 << 22).await.unwrap();
            let headers = parts
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            heard
                .lock()
                .unwrap()
                .push((headers, String::from_utf8_lossy(&body).into_owned()));
            let next = {
                let mut script = script.lock().unwrap();
                match script.is_empty() {
                    true => None,
                    false => Some(script.remove(0)),
                }
            };
            match next {
                Some(stream) => (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    stream,
                ),
                None => (
                    [(axum::http::header::CONTENT_TYPE, "text/plain")],
                    fallback.to_string(),
                ),
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Every name to `addr`, whatever port was asked: the proxies only dial
/// 443, which a test cannot bind.
fn resolving(names: &'static [&'static str], addr: SocketAddr) -> Resolve {
    Arc::new(move |host, _port| {
        let found: Vec<SocketAddr> = names
            .iter()
            .filter(|name| **name == host)
            .map(|_| addr)
            .collect();
        Box::pin(async move { Ok(found) })
    })
}

const BROKER: &str = r#"
    [credentials.stubcred]
    kind = "api_key"
    provider = "stub"
    channels = ["work"]
    [credentials.stubcred.env]
    API_KEY = "real-key"
"#;

impl Rig {
    /// A configuration with tunnels on, and the node's three listeners
    /// served where it says: the harness router (MCP and model gateway)
    /// with a stub provider behind it, the allowlist proxy, and the granted
    /// proxy.
    async fn new(tag: &str) -> Option<Rig> {
        if !requested() {
            return None;
        }
        support::state::isolate();
        let bundle = std::env::var("TRACON_SANDBOX_BUNDLE")
            .map(PathBuf::from)
            .expect("TRACON_PODMAN_TESTS=1 needs TRACON_SANDBOX_BUNDLE");
        assert!(bundle.join("bin/tracon-sandbox").is_file());
        let image = std::env::var("TRACON_SANDBOX_TEST_IMAGE")
            .expect("these tests need TRACON_SANDBOX_TEST_IMAGE, an image with curl");
        assert!(podman(&["image", "exists", &image]).is_some(), "{image}");
        let prefix = format!(
            "{}{tag}-",
            std::env::var("TRACON_SANDBOX_TEST_PREFIX")
                .unwrap_or_else(|_| "tracon-test-tun-".into())
        );
        let short = tempfile::Builder::new()
            .prefix("tun")
            .tempdir_in("/tmp")
            .unwrap();

        let model: Heard = Default::default();
        let script: Arc<Mutex<Vec<String>>> = Default::default();
        let model_addr = recording_server(model.clone(), script.clone(), "").await;
        let origin: Heard = Default::default();
        let origin_addr =
            recording_server(origin.clone(), Default::default(), "hello from the origin").await;

        let mut cfg = Config::default();
        cfg.sandbox.base_image = Some(image.clone());
        cfg.gateway.harness_listen = HarnessListen::Unix(short.path().join("harness.sock"));
        cfg.gateway.allow_hosts.push(r"^127\.0\.0\.1$".into());
        cfg.sandbox.bundle = Some(bundle);
        cfg.providers.clear();
        cfg.providers.insert(
            "stub".into(),
            Provider {
                credential: "stubcred".into(),
                upstream: format!("http://{model_addr}"),
                shape: SHAPE_ANTHROPIC.into(),
                login: None,
                price: None,
                models: Vec::new(),
            },
        );

        // The harness listener, as the node builds it.
        let shared = Arc::new(cfg.clone());
        let broker: tracon::broker::Broker = toml::from_str(BROKER).unwrap();
        let tools = Arc::new(tracon::mcp::Tools {
            broker: broker.shared(),
            cfg: shared.clone(),
            policy: tracon::policy::Policy::shipped_shared(),
            http: reqwest::Client::new(),
            session: Default::default(),
            upstream: Default::default(),
        });
        let store = Arc::new(tracon::store::Store::open_in_memory().unwrap());
        let manager = tracon::session::Manager::new(
            store.clone(),
            tracon::stream::Bus::new(),
            shared.clone(),
            "n1".into(),
            tools.clone(),
            Default::default(),
            Arc::new(tracon::runner::local::LocalBackend),
        );
        let _ = tools.session.set(tracon::mcp::SessionAccess {
            store,
            manager: manager.clone(),
        });
        let state = tracon::http::api::AppState {
            manager: manager.clone(),
            cfg: shared,
            adapter: Arc::new(FakeAdapter {
                tx: Arc::new(tokio::sync::Mutex::new(None)),
                tokens: Arc::new(tokio::sync::Mutex::new(0)),
            }),
            node_id: "n1".into(),
            tools,
            mesh: None,
            auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
            enroll: Default::default(),
        };
        let HarnessListen::Unix(harness) = &cfg.gateway.harness_listen else {
            unreachable!()
        };
        let listener = tokio::net::UnixListener::bind(harness).unwrap();
        let app = tracon::http::harness_router(state);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // The allowlist proxy, which the gateway's tinyproxy was.
        let listener = tokio::net::UnixListener::bind(cfg.allow_proxy_socket()).unwrap();
        tokio::spawn(tracon::gateway::proxy::run_unix_refusing(
            listener,
            Allowlist::new(&cfg.gateway.allow_hosts).unwrap(),
            Refusal::this_machine(),
        ));

        // The granted proxy, its test hosts on loopback.
        let grants = Grants::default();
        let HarnessListen::Unix(egress) = cfg.egress_listen() else {
            unreachable!()
        };
        let listener = tokio::net::UnixListener::bind(egress).unwrap();
        tokio::spawn(tracon::gateway::proxy::serve_granted_unix(
            listener,
            Granted::dialling_any_address(grants.clone())
                .resolving_with(resolving(&["allowed.test", "nova.test"], origin_addr))
                .upstreams_over_plain_http(),
        ));

        Some(Rig {
            prefix,
            cfg,
            image,
            volumes: Vec::new(),
            manager,
            grants,
            model,
            origin,
            script,
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

    fn spec(&self, mounts: Vec<Mount>) -> RunSpec {
        let mut spec = RunSpec::from_config(&self.cfg);
        spec.image = self.image.clone();
        spec.extra_mounts = mounts;
        spec
    }

    fn runner(&self, mounts: Vec<Mount>) -> Arc<dyn Runner> {
        tracon::boundary::podman::protocol_runner(&self.cfg, self.spec(mounts))
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
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// An Anthropic Messages stream of `blocks`, each `(start, deltas)`,
/// stopping for `stop`.
fn anthropic(blocks: &[(Value, Vec<Value>)], stop: &str) -> String {
    let mut events = vec![(
        "message_start",
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-test",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 50, "output_tokens": 1}}}),
    )];
    for (index, (start, deltas)) in blocks.iter().enumerate() {
        events.push((
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": start}),
        ));
        for delta in deltas {
            events.push((
                "content_block_delta",
                json!({"type": "content_block_delta", "index": index, "delta": delta}),
            ));
        }
        events.push((
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        ));
    }
    events.push((
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null},
            "usage": {"output_tokens": 5}}),
    ));
    events.push(("message_stop", json!({"type": "message_stop"})));
    events
        .iter()
        .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
        .collect()
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

/// From inside: loopback is the only interface and nothing resolves, yet
/// the node answers through the gateway tunnel.
#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn a_sandbox_has_no_network_but_loopback_and_reaches_the_node_through_its_tunnel() {
    let Some(rig) = Rig::new("route").await else {
        return;
    };
    let out = rig
        .runner(Vec::new())
        .run_capture(rig.command(
            "route",
            "ls /sys/class/net | tr '\\n' ' '; echo; \
             getent hosts example.com >/dev/null && echo resolved || echo no-name; \
             getent hosts tracon-gw; \
             curl -s -m 5 --noproxy '*' http://1.1.1.1/ >/dev/null; echo direct=$?; \
             curl -s -m 5 --noproxy '*' http://tracon-gw:7421/harness/ping; echo",
        ))
        .await
        .unwrap();
    let stdout = text(&out.stdout);
    eprintln!("{stdout}{}", text(&out.stderr));
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0].trim(), "lo", "{stdout}");
    assert_eq!(lines[1], "no-name", "{stdout}");
    assert!(lines[2].starts_with("127.0.0.1"), "{stdout}");
    // 7: could not connect. No route, so at once.
    assert_eq!(lines[3], "direct=7", "{stdout}");
    assert_eq!(lines[4], "pong", "{stdout}");
}

/// A Claude Code session: its model turns go through the model gateway's
/// tunnel, where the node swaps the placeholder for the credential, and it
/// connects to the MCP gateway and calls a tool there.
#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn a_claude_code_session_uses_the_model_and_mcp_gateways_through_tunnels() {
    use tracon::adapter::{claude::ClaudeAdapter, HarnessAdapter, HarnessEvent, LaunchSpec};

    let Some(mut rig) = Rig::new("claude").await else {
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
    let token = rig.manager.register_tool_token_for_test("s1", "work").await;
    *rig.script.lock().unwrap() = vec![
        anthropic(
            &[(
                json!({"type": "tool_use", "id": "toolu_1", "name": "mcp__tracon__retain", "input": {}}),
                vec![json!({"type": "input_json_delta", "partial_json": json!({
                    "kind": "fact", "scope": "global",
                    "body": "the sandbox reached the node through a tunnel",
                    "confidence": 0.9}).to_string()})],
            )],
            "tool_use",
        ),
        anthropic(
            &[(
                json!({"type": "text", "text": ""}),
                vec![json!({"type": "text_delta", "text": "remembered"})],
            )],
            "end_turn",
        ),
    ];
    let mcp = json!({
        "type": "http",
        "name": "tracon",
        "url": format!("http://tracon-gw:{}/mcp/s1", rig.cfg.gateway.forward_port),
        "headers": [{"name": "Authorization", "value": format!("Bearer {token}")}],
    });
    let state = rig.volume("claude-state");
    let work = rig.volume("work");
    let seed = rig.runner(Vec::new());
    let mcp_file = json!({"mcpServers": {"tracon": {
        "type": "http", "url": mcp["url"],
        "headers": {"Authorization": format!("Bearer {token}")}}}});
    let mut write = rig.command(
        "seed",
        &format!(
            "mkdir -p /s/plugin/.claude-plugin && \
             printf '%s' '{{\"name\":\"tracon\",\"version\":\"1.0.0\"}}' > /s/plugin/.claude-plugin/plugin.json && \
             printf '%s' '{mcp_file}' > /s/tracon-mcp.json"
        ),
    );
    write.mounts = vec![Mount::volume(&state, "/s", false)];
    let seeded = seed.run_capture(write).await.unwrap();
    assert!(seeded.status.success(), "{}", text(&seeded.stderr));

    // The MCP gateway through its tunnel, as any client inside calls it.
    let mut list = rig.command(
        "list",
        &format!(
            "curl -s -m 10 -H 'content-type: application/json' -H \"authorization: Bearer $T\" \
             -d '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}}' \
             http://tracon-gw:{}/mcp/s1",
            rig.cfg.gateway.forward_port
        ),
    );
    list.secret_env = SecretEnv::new(vec![("T".into(), token.clone())]);
    let listed = seed.run_capture(list).await.unwrap();
    assert!(
        text(&listed.stdout).contains("\"name\":\"retain\""),
        "{}",
        text(&listed.stdout)
    );

    let runner = FromBundle(
        "claude",
        rig.runner(vec![
            Mount::volume(&state, "/home/harness/.claude", false),
            Mount::volume(&work, "/work", false),
        ]),
    );
    let spec = LaunchSpec {
        // The placeholder: the session's gateway token, which the model
        // gateway exchanges for the provider credential.
        secret_env: SecretEnv::new(vec![("ANTHROPIC_API_KEY".into(), token.clone())]),
        cwd_in_runner: "/work".into(),
        model: "anthropic/claude-test".into(),
        container_name: rig.name("session"),
        harness_home: "/home/harness".into(),
        mcp_servers: vec![mcp],
        tools: Vec::new(),
        env: vec![
            (
                "ANTHROPIC_BASE_URL".into(),
                format!(
                    "http://tracon-gw:{}/model/stub",
                    rig.cfg.gateway.forward_port
                ),
            ),
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
                "1".into(),
            ),
        ],
        system_prompt_file: None,
        resume: None,
        labels: Default::default(),
    };
    let adapter = ClaudeAdapter::new(ClaudeAdapter::PINNED_VERSION);
    // The launch reads `system/init` and refuses one whose MCP server did
    // not connect: getting past it is the MCP gateway answering.
    let (handle, mut rx) = adapter.launch(&runner, spec).await.unwrap();
    let events = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                HarnessEvent::Permission { request, reply } => {
                    eprintln!("permission: {} {:?}", request.title, request.options);
                    let id = request.options[0].option_id.clone();
                    let _ = reply.send(tracon::adapter::PermissionReply::Selected(id));
                }
                other => eprintln!("event: {other:?}"),
            }
        }
    });
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        handle.prompt("remember where you are".into()),
    )
    .await
    .expect("the turn ended")
    .unwrap();
    eprintln!("{result:?}");
    assert_eq!(result.stop_reason, "end_turn");

    let model = rig.model.lock().unwrap().clone();
    assert!(model.len() >= 2, "{} requests", model.len());
    for (headers, body) in &model {
        assert_eq!(header(headers, "x-api-key"), Some("real-key"));
        assert!(
            !format!("{headers:?}{body}").contains(&token),
            "the placeholder reached the provider"
        );
    }
    // The tool ran at the node and its answer went back to the model.
    let last = &model.last().unwrap().1;
    assert!(last.contains("toolu_1"), "{last}");
    assert!(last.contains("recalled by later sessions"), "{last}");
    drop(handle);
    let _ = rig.runner(Vec::new()).kill(&rig.name("session")).await;
    events.abort();
}

/// The per-client egress proxy through its tunnel: what a grant names is
/// reached, what it does not is refused and remembered against it, and a
/// declared upstream is sent its credential by the node. The allowlist
/// proxy refuses what is not on the list.
#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn egress_grants_and_upstreams_hold_through_the_egress_tunnel() {
    let Some(rig) = Rig::new("egress").await else {
        return;
    };
    let token = rig
        .grants
        .issue(GrantSpec {
            client: "session".into(),
            session_id: Some("s1".into()),
            hosts: vec!["allowed.test".into()],
            refusal: "not granted to this session".into(),
            upstreams: vec![tracon::broker::Attachment::for_test(
                "nova.test",
                "authorization",
                "Bearer real-nova",
            )],
            ..Default::default()
        })
        .unwrap();
    let port = rig.cfg.gateway.qa_proxy_port;
    let mut cmd = rig.command(
        "grant",
        &format!(
            "P=http://session:$EGRESS_TOKEN@tracon-gw:{port}; \
             curl -s -m 10 -p -x $P http://allowed.test:443/x; echo; \
             curl -s -m 10 -o /dev/null -w 'other=%{{http_connect}}\\n' -p -x $P http://other.test:443/; \
             curl -s -m 10 -x $P -H 'Authorization: Bearer placeholder' http://nova.test/dist; echo; \
             curl -s -m 10 -o /dev/null -w 'anonymous=%{{http_connect}}\\n' -p -x http://tracon-gw:{port} http://allowed.test:443/; \
             curl -s -m 10 -o /dev/null -w 'unlisted=%{{http_connect}}\\n' https://example.com/"
        ),
    );
    cmd.secret_env = SecretEnv::new(vec![("EGRESS_TOKEN".into(), token.clone())]);
    let out = rig.runner(Vec::new()).run_capture(cmd).await.unwrap();
    let stdout = text(&out.stdout);
    eprintln!("{stdout}{}", text(&out.stderr));
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "hello from the origin", "{stdout}");
    assert_eq!(lines[1], "other=403", "{stdout}");
    assert_eq!(lines[2], "hello from the origin", "{stdout}");
    assert_eq!(lines[3], "anonymous=407", "{stdout}");
    // Through HTTPS_PROXY, the node's allowlist proxy.
    assert_eq!(lines[4], "unlisted=403", "{stdout}");

    assert_eq!(
        rig.grants.refused_by(&token),
        vec!["other.test".to_string()]
    );
    let origin = rig.origin.lock().unwrap().clone();
    assert_eq!(origin.len(), 2, "{origin:?}");
    let nova = &origin[1].0;
    assert_eq!(header(nova, "authorization"), Some("Bearer real-nova"));
    assert!(header(nova, "proxy-authorization").is_none());
    assert!(!format!("{origin:?}").contains(&token));
}

/// The checks with the setting on: the probe reads back a single route,
/// the deep check proves it from inside over the protocol, and the
/// capabilities say so.
#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn the_checks_verify_the_single_route() {
    let Some(mut rig) = Rig::new("checks").await else {
        return;
    };
    let state = rig.volume("state");
    let spec = rig.spec(vec![Mount::volume(&state, "/home/harness/.claude", false)]);
    let mut checks =
        tracon::boundary::podman::checks::probe_checks(&rig.cfg, &spec, &rig.name("probe")).await;
    // The deep check runs in the configured harness image.
    let egress =
        tracon::boundary::podman::checks::check_egress(&rig.cfg, &rig.name("egress")).await;
    checks.push(egress);
    for check in &checks {
        eprintln!("{:?}: {} {}", check.id, check.ok, check.detail);
    }
    assert_eq!(checks.len(), 4);
    assert!(checks.iter().all(|c| c.ok));
    let network = &checks[2];
    assert!(
        network.detail.starts_with("route: node-only, dns: none"),
        "{}",
        network.detail
    );
    assert!(podman(&["container", "exists", &rig.name("probe")]).is_none());

    let mut report = tracon::boundary::BoundaryReport {
        checks: vec![tracon::boundary::CheckResult::ok(
            tracon::boundary::CheckId::Runtime,
            "podman, rootless",
        )],
    };
    report.checks.extend(checks);
    let caps = tracon::provider::capabilities::from_checks(
        tracon::provider::ProviderKind::Podman,
        &report,
    );
    let value = |name: &str| {
        caps.lines
            .iter()
            .find(|l| l.capability == name)
            .and_then(|l| l.value)
    };
    assert_eq!(value("route"), Some("node-only"));
    assert_eq!(value("dns"), Some("none"));
    assert!(caps.verified);
}
