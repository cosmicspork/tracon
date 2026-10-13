//! Per-client egress on Kubernetes, against a fake API.
//!
//! A harness pod reaches the node's own egress proxy at the node's pod, and
//! presents credentials issued to that one run. The credentials are a
//! password in a proxy URL, so they travel as secret environment and from
//! there in a Secret of the run's own; a node that may not make one issues no
//! grant, and refuses a session, rather than putting them in a pod spec. The
//! same goes for every other secret a session carries: its gateway token,
//! Claude Code's MCP header and OpenCode's server password.

#[path = "support/mod.rs"]
mod support;
use support::{
    harness::{harness_on, Harness},
    kube_api::{KubeApi, Script},
    rows::{node_row, session_row},
};

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;
use tracon::adapter::HarnessAdapter;
use tracon::boundary::kubernetes::{EgressPort, KubeBackend};
use tracon::boundary::{Backend, EgressGrant};
use tracon::config::{Config, HarnessListen, RuntimeKind};
use tracon::gateway::proxy::{serve_granted, GrantSpec, Granted};
use tracon::runner::kube::PodEnv;
use tracon::runner::RunnerCommand;

const NAMESPACE: &str = "tracon-test";
/// Never resolves (RFC 2606): an admitted CONNECT answers 502.
const HOST: &str = "registry.example.test";
const TOKEN: &str = "session-token-0123456789abcdef";

fn config(state: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    cfg.runtime.kind = RuntimeKind::Kubernetes;
    cfg.runtime.kubernetes.state_mount = state.to_path_buf();
    cfg
}

fn backend(api: &KubeApi, cfg: &Config) -> KubeBackend {
    KubeBackend::with_client(
        cfg,
        PodEnv {
            namespace: NAMESPACE.into(),
            pod_ip: "10.244.0.9".into(),
            node_name: "general-1".into(),
        },
        api.client(),
    )
}

/// A backend whose checks found both halves: Secrets granted and the egress
/// port open in the policies.
fn ready(api: &KubeApi, cfg: &Config) -> KubeBackend {
    let backend = backend(api, cfg);
    backend.secret_rights().set(Ok(()));
    backend.egress_port().set(Ok(()));
    backend
}

/// The backend's grants served on a loopback port, as the node serves them
/// on `egress_port`, dialling nothing a cluster holds.
async fn serve(backend: &KubeBackend) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let granted = Granted::refusing(
        backend
            .egress_grants()
            .expect("a pod-hosted node serves grants")
            .clone(),
        backend.egress_refusal(),
    );
    tokio::spawn(serve_granted(l, granted));
    addr
}

/// The status line a CONNECT to `host` gets, with `grant`'s credentials.
async fn connect(addr: std::net::SocketAddr, host: &str, user: &str, password: &str) -> String {
    use base64::Engine;
    let credentials =
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\
             Proxy-Authorization: Basic {credentials}\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = vec![0u8; 4096];
    let n = s.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..n])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The proxy URL a grant's environment carries.
fn proxy_url(env: &tracon::runner::SecretEnv) -> String {
    env.iter()
        .find(|(name, _)| name == "HTTPS_PROXY")
        .map(|(_, value)| value.clone())
        .expect("the grant sets HTTPS_PROXY")
}

/// A preparation's grant names the node's pod at the egress port, opens its
/// hosts and nothing else, and reaches the pod that uses it only through the
/// run's own Secret. Dropping it closes it.
#[tokio::test]
async fn a_preparation_grant_reaches_the_node_pod_and_never_a_pod_spec() {
    support::state::isolate();
    let api = KubeApi::new(NAMESPACE).default_script(Script::exits(0));
    let state = tempfile::tempdir().unwrap();
    let cfg = config(state.path());
    let backend = ready(&api, &cfg);
    let addr = serve(&backend).await;

    let scoped = tracon::environment::scoped_egress(&backend, &[HOST.to_string()]).unwrap();
    let url = proxy_url(&scoped.secret_env);
    let (credentials, server) = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once('@'))
        .expect("credentials in the URL");
    assert_eq!(
        server, "tracon-gw:7424",
        "the node's pod, at the egress port"
    );
    let (user, password) = credentials.split_once(':').unwrap();
    assert_eq!(user, "prepare");

    assert!(connect(addr, HOST, user, password)
        .await
        .starts_with("HTTP/1.1 502"));
    let other = connect(addr, "pypi.org", user, password).await;
    assert!(other.starts_with("HTTP/1.1 403"), "{other}");

    let out = backend
        .runner(Vec::new())
        .run_capture(RunnerCommand {
            argv: vec!["sh".into(), "-c".into(), "npm ci".into()],
            secret_env: scoped.secret_env.clone(),
            name: "tracon-w-app".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(out.status.success());
    let created = api.created();
    let rendered = serde_json::to_string(&created).unwrap();
    assert!(!rendered.contains(password), "the grant is in the pod spec");
    // A captured run's pod (`capture_pod`) names the grant by reference and
    // is still sized as a command.
    let container = &created[0].spec.as_ref().unwrap().containers[0];
    for name in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        let var = container
            .env
            .iter()
            .flatten()
            .find(|var| var.name == name)
            .unwrap_or_else(|| panic!("{name} is not set"));
        assert!(var.value.is_none(), "{name} is rendered");
        assert!(
            var.value_from
                .as_ref()
                .is_some_and(|from| from.secret_key_ref.is_some()),
            "{name} is not from the Secret"
        );
    }
    let requests = container.resources.clone().unwrap().requests.unwrap();
    assert_eq!(
        requests["memory"].0,
        cfg.runtime.kubernetes.resources.command().memory_request
    );
    let secrets = api.secrets_created();
    assert_eq!(secrets.len(), 1);
    assert_eq!(
        secrets[0].data.as_ref().unwrap()["HTTPS_PROXY"].0,
        url.as_bytes()
    );

    drop(scoped);
    let revoked = connect(addr, HOST, user, password).await;
    assert!(revoked.starts_with("HTTP/1.1 407"), "{revoked}");
}

/// No grant is issued until both halves are known: the node may keep it in
/// a Secret, and the policies let a harness pod reach the port. Each refusal
/// says what to change. A session is refused outright without the Secrets.
#[tokio::test]
async fn no_grant_without_the_secret_rights_or_the_port() {
    support::state::isolate();
    let api = KubeApi::new(NAMESPACE);
    let state = tempfile::tempdir().unwrap();
    let cfg = config(state.path());
    let spec = || GrantSpec {
        client: "prepare".into(),
        hosts: vec![HOST.into()],
        ..Default::default()
    };
    let refused = |backend: &KubeBackend| match backend.egress_grant(spec()) {
        Ok(_) => panic!("a grant was issued"),
        Err(error) => error.to_string(),
    };

    // Never checked.
    let unchecked = backend(&api, &cfg);
    let why = refused(&unchecked);
    assert!(why.contains("may not manage Secrets"), "{why}");
    assert!(why.contains("role.yaml"), "{why}");
    let session = unchecked
        .secret_env_refusal()
        .expect("a session is refused");
    assert!(
        session.contains("refusing to start a session on Kubernetes"),
        "{session}"
    );
    assert!(session.contains("role.yaml"), "{session}");

    // Checked, and a verb missing: the check's words are repeated.
    let denied = backend(&api, &cfg);
    denied.secret_rights().set(Err(format!(
        "the service account may not patch secrets in {NAMESPACE}"
    )));
    denied.egress_port().set(Ok(()));
    let why = refused(&denied);
    assert!(why.contains("may not patch secrets"), "{why}");
    assert!(denied
        .secret_env_refusal()
        .unwrap()
        .contains("may not patch secrets"));
    let prepare = match tracon::environment::scoped_egress(&denied, &[HOST.to_string()]) {
        Ok(_) => panic!("preparation egress was opened"),
        Err(error) => error,
    };
    assert!(
        prepare.starts_with("could not open preparation egress: per-client egress is off"),
        "{prepare}"
    );
    assert!(prepare.contains("role.yaml"), "{prepare}");

    // Secrets granted, the port not yet confirmed, then found shut.
    let shut = backend(&api, &cfg);
    shut.secret_rights().set(Ok(()));
    assert!(shut.secret_env_refusal().is_none());
    assert!(refused(&shut).contains("egress port"), "{}", refused(&shut));
    shut.egress_port().set(Err(
        "tracon-harness does not allow egress to the node on port 7424".into(),
    ));
    assert!(refused(&shut).contains("port 7424"), "{}", refused(&shut));

    // Both: a grant.
    shut.egress_port().set(Ok(()));
    assert!(shut.egress_grant(spec()).is_ok());
    assert!(EgressPort::open().is_open());

    // An `internal_cidrs` entry that is not a block turns grants off and
    // still refuses what a cluster always does.
    let mut typo = cfg.clone();
    typo.runtime.kubernetes.internal_cidrs = vec!["10.0.0.0/33".into()];
    let typo = ready(&api, &typo);
    assert!(
        refused(&typo).contains("internal_cidrs"),
        "{}",
        refused(&typo)
    );
    assert!(typo
        .egress_refusal()
        .address(&"10.1.2.3".parse().unwrap())
        .is_some());
}

/// The secrets a session carries never reach its pod spec: the gateway
/// token (in the wiring's environment and in Claude Code's MCP header), the
/// egress grant, and OpenCode's per-launch server password. Each is a
/// `secretKeyRef` to the session's own Secret.
#[tokio::test(start_paused = true)]
async fn a_sessions_secrets_never_reach_its_pod_spec() {
    support::state::isolate();
    let api =
        KubeApi::new(NAMESPACE).default_script(Script::exits(0).running(Duration::from_secs(60)));
    let state = tempfile::tempdir().unwrap();
    let cfg = config(state.path());
    let backend = ready(&api, &cfg);
    let home = backend.harness_home();

    let mut wiring = tracon::gateway::model::harness_wiring(&cfg, "tracon-gw", TOKEN, |_, _| true);
    wiring.mcp_servers = vec![json!({
        "type": "http",
        "name": "tracon",
        "url": "http://tracon-gw:7421/mcp/s1",
        "headers": [{ "name": "Authorization", "value": format!("Bearer {TOKEN}") }],
    })];
    let grant: EgressGrant = backend
        .egress_grant(GrantSpec {
            client: "session".into(),
            session_id: Some("s1".into()),
            hosts: vec![HOST.into()],
            ..Default::default()
        })
        .unwrap();
    let (env, secret_env) = tracon::session::harness_environment(&wiring, Some(&grant), &home);
    let launch = |name: &str| tracon::adapter::LaunchSpec {
        cwd_in_runner: "/work".into(),
        model: "anthropic/claude-x".into(),
        container_name: name.into(),
        harness_home: home.clone(),
        mcp_servers: wiring.mcp_servers.clone(),
        tools: Vec::new(),
        env: env.clone(),
        secret_env: secret_env.clone(),
        system_prompt_file: None,
        cursor: None,
    };

    // Neither launch gets past attaching (the fake API has no attach), but
    // each pod is created as it would be.
    let runner = Backend::runner_for(&backend, "claude", Vec::new());
    let claude = tracon::adapter::claude::ClaudeAdapter::new("2.1.295");
    assert!(claude
        .launch(runner.as_ref(), launch("tracon-h-claude"))
        .await
        .is_err());
    let runner = Backend::runner_for(&backend, "opencode", Vec::new());
    let opencode = tracon::adapter::opencode::OpenCodeAdapter::new("1.18.30");
    assert!(opencode
        .launch(runner.as_ref(), launch("tracon-h-opencode"))
        .await
        .is_err());

    let created = api.created();
    assert_eq!(created.len(), 2, "{:?}", api.live());
    for pod in &created {
        let name = pod.metadata.name.clone().unwrap();
        let rendered = serde_json::to_string(pod).unwrap();
        assert!(
            !rendered.contains(TOKEN),
            "{name}: the token is in the pod spec"
        );
        assert!(
            !rendered.contains(&grant.password),
            "{name}: the grant is in the pod spec"
        );
        let container = &pod.spec.as_ref().unwrap().containers[0];
        let hidden: Vec<&str> = container
            .env
            .iter()
            .flatten()
            .filter(|var| {
                var.value_from
                    .as_ref()
                    .is_some_and(|from| from.secret_key_ref.is_some())
            })
            .map(|var| var.name.as_str())
            .collect();
        for name in [
            "ANTHROPIC_API_KEY",
            "TRACON_PROVIDER_KEY_ANTHROPIC",
            "HTTPS_PROXY",
        ] {
            assert!(
                hidden.contains(&name),
                "{name} is not from the Secret: {hidden:?}"
            );
        }
        if name.contains("opencode") {
            assert!(hidden.contains(&"OPENCODE_SERVER_PASSWORD"), "{hidden:?}");
        }
        // Base URLs are not secret and stay readable.
        assert!(container
            .env
            .iter()
            .flatten()
            .any(|var| { var.name == "ANTHROPIC_BASE_URL" && var.value.is_some() }));
    }
    let claude_pod = created
        .iter()
        .find(|pod| pod.metadata.name.as_deref() == Some("tracon-h-claude"))
        .unwrap();
    let argv = claude_pod.spec.as_ref().unwrap().containers[0]
        .command
        .clone()
        .unwrap()
        .join(" ");
    assert!(
        argv.contains(&format!("--mcp-config {home}/.claude/tracon-mcp.json")),
        "{argv}"
    );

    // What the pods read instead.
    let secrets = api.secrets_created();
    assert_eq!(secrets.len(), 2);
    for secret in &secrets {
        let data = secret.data.clone().unwrap();
        assert_eq!(data["ANTHROPIC_API_KEY"].0, TOKEN.as_bytes());
        assert_eq!(data["HTTPS_PROXY"].0, grant.url().as_bytes());
    }
    assert!(secrets.iter().any(|secret| secret
        .data
        .as_ref()
        .unwrap()
        .contains_key("OPENCODE_SERVER_PASSWORD")));
}

async fn answer(h: &Harness, id: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/permissions/{id}/answer"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = h.operator.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The egress ask needs nothing of Podman: on a node whose Kubernetes
/// backend issues the session its grant, a refused host is put to the
/// operator, the answer widens that grant, and `request_egress` sees it
/// open. A cluster name is refused without asking anyone.
#[tokio::test]
async fn the_egress_ask_works_on_kubernetes() {
    support::state::isolate();
    let api = KubeApi::new(NAMESPACE);
    let state = tempfile::tempdir().unwrap();
    let cfg = config(state.path());
    let backend = Arc::new(ready(&api, &cfg));
    let h = harness_on(cfg, backend.clone()).await;
    h.store
        .insert_session(&session_row("s1", "n1", "personal"))
        .unwrap();
    h.manager.record_egress_refusals();
    let addr = serve(&backend).await;
    let grant = backend
        .egress_grant(GrantSpec {
            client: "session".into(),
            session_id: Some("s1".into()),
            refusal: tracon::environment::SESSION_REFUSAL.into(),
            ..Default::default()
        })
        .unwrap();

    let line = connect(addr, HOST, "session", &grant.password).await;
    assert!(line.starts_with("HTTP/1.1 403"), "{line}");
    assert!(line.contains("the operator has been asked"), "{line}");
    let at = line.find("(approval ").unwrap() + "(approval ".len();
    let id = line[at..].split(')').next().unwrap().to_string();

    let (status, body) = answer(&h, &id, json!({ "option_id": "allow_session" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let opened = connect(addr, HOST, "session", &grant.password).await;
    assert!(opened.starts_with("HTTP/1.1 502"), "{opened}");

    let ctx = tracon::mcp::CallContext::session("s1", "personal", "n1");
    let reachable = h
        .tools
        .call(
            &ctx,
            "request_egress",
            &json!({ "host": HOST, "wait_secs": 0 }),
        )
        .await
        .unwrap();
    assert_eq!(reachable["state"], "reachable", "{reachable}");
    let inside = h
        .tools
        .call(
            &ctx,
            "request_egress",
            &json!({ "host": "vault.vault.svc", "wait_secs": 0 }),
        )
        .await
        .unwrap_err();
    assert!(
        inside.contains("names a host inside the cluster"),
        "{inside}"
    );
    assert_eq!(h.store.open_permission_views().unwrap().len(), 0);
}

/// A Kubernetes node that may not manage Secrets fails a session before it
/// stages anything, with the rule to add as the session's error.
#[tokio::test]
async fn a_session_is_refused_without_the_secret_rights() {
    support::state::isolate();
    let api = KubeApi::new(NAMESPACE);
    let state = tempfile::tempdir().unwrap();
    let cfg = config(state.path());
    let backend = Arc::new(backend(&api, &cfg));
    let h = harness_on(cfg, backend).await;
    h.store.put_node(&node_row("n1", "test")).unwrap();
    let row = h
        .manager
        .create_with(
            tracon::session::NewSession {
                channel: "personal".into(),
                repo_path: "/nonexistent/repo".into(),
                branch: None,
                work_item_id: None,
                model: "m/a".into(),
                budget_tokens: None,
                initial_prompt: None,
                node_id: None,
                phase: tracon::session::Phase::Execute,
                review_id: None,
                base_sha: None,
                workspace_id: None,
                parent_session: None,
                continued_from: None,
                harness: None,
                on_exhaustion: None,
            },
            Arc::new(support::fake::FakeAdapter {
                tx: Arc::new(tokio::sync::Mutex::new(None)),
                tokens: Arc::new(tokio::sync::Mutex::new(0)),
            }),
        )
        .await
        .unwrap();
    let mut failed = None;
    for _ in 0..300 {
        let session = h.store.get_session(&row.id).unwrap().unwrap();
        if session.state == "failed" {
            failed = Some(session);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let failed = failed.expect("the session failed");
    let error = failed.last_error.unwrap_or_default();
    assert!(
        error.contains("refusing to start a session on Kubernetes"),
        "{error}"
    );
    assert!(error.contains("role.yaml"), "{error}");
    assert!(api.created().is_empty(), "no pod was made");
}

/// The shipped harness policy's shape, egress to the node on `ports`.
fn harness_policy(ports: &[i32]) -> NetworkPolicy {
    serde_json::from_value(json!({
        "metadata": { "name": "tracon-harness", "namespace": NAMESPACE },
        "spec": {
            "podSelector": { "matchLabels": { "tracon.dev/role": "harness" } },
            "policyTypes": ["Ingress", "Egress"],
            "ingress": [{ "from": [{ "podSelector": { "matchLabels": { "tracon.dev/role": "node" } } }] }],
            "egress": [{
                "to": [{ "podSelector": { "matchLabels": { "tracon.dev/role": "node" } } }],
                "ports": ports.iter().map(|port| json!({ "protocol": "TCP", "port": port })).collect::<Vec<_>>(),
            }],
        },
    }))
    .unwrap()
}

/// The shipped node policy's shape, admitting harness pods on `ports`.
fn node_policy(ports: &[i32]) -> NetworkPolicy {
    serde_json::from_value(json!({
        "metadata": { "name": "tracon-node", "namespace": NAMESPACE },
        "spec": {
            "podSelector": { "matchLabels": { "tracon.dev/role": "node" } },
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{ "podSelector": { "matchLabels": { "tracon.dev/role": "harness" } } }],
                "ports": ports.iter().map(|port| json!({ "protocol": "TCP", "port": port })).collect::<Vec<_>>(),
            }],
        },
    }))
    .unwrap()
}

/// A pod-hosted node's configuration: the harness listener on the pod
/// network, as the shipped ConfigMap has it.
fn pod_config(state: &std::path::Path) -> Config {
    let mut cfg = config(state);
    cfg.gateway.harness_listen = HarnessListen::Tcp("0.0.0.0:7421".parse().unwrap());
    cfg
}

/// What the node policy looks like in one case.
enum NodePolicy {
    Absent,
    Admits(&'static [i32]),
    Unreadable,
}

/// The network check decides whether grants are on, and never fails the
/// node over it: a harness policy with or without the egress port, against
/// a node policy that is absent (unrestricted), narrow, open, or unreadable.
#[tokio::test]
async fn the_network_check_turns_grants_on_only_when_both_policies_open_the_port() {
    support::state::isolate();
    for (harness, node, open, said) in [
        (
            &[7421, 8888][..],
            NodePolicy::Absent,
            false,
            "tracon-harness does not allow",
        ),
        (&[7421, 8888, 7424][..], NodePolicy::Absent, true, ""),
        (
            &[7421, 8888, 7424][..],
            NodePolicy::Admits(&[7421, 8888]),
            false,
            "tracon-node does not admit",
        ),
        (
            &[7421, 8888, 7424][..],
            NodePolicy::Admits(&[7421, 8888, 7424]),
            true,
            "",
        ),
        (
            &[7421, 8888, 7424][..],
            NodePolicy::Unreadable,
            false,
            "could not read tracon-node",
        ),
        (
            &[7421, 8888][..],
            NodePolicy::Admits(&[7421, 8888, 7424]),
            false,
            "tracon-harness does not allow",
        ),
    ] {
        let api = match node {
            NodePolicy::Unreadable => KubeApi::new(NAMESPACE).forbid_policy("tracon-node"),
            _ => KubeApi::new(NAMESPACE),
        };
        api.insert_policy(harness_policy(harness));
        if let NodePolicy::Admits(ports) = node {
            api.insert_policy(node_policy(ports));
        }
        let state = tempfile::tempdir().unwrap();
        let cfg = pod_config(state.path());
        let backend = backend(&api, &cfg);
        let report = backend.check_all(&cfg, false).await;
        let case = format!("{harness:?}, open {open}");
        assert!(report.passed(), "{case}: {:?}", report.first_failure());
        let network = report
            .checks
            .iter()
            .find(|check| check.id == tracon::boundary::CheckId::NetworkIsolated)
            .unwrap();
        assert_eq!(
            backend.egress_port().is_open(),
            open,
            "{case}: {}",
            network.detail
        );
        match backend.egress_port().verdict().unwrap() {
            Ok(()) => assert!(network.detail.contains("egress port 7424"), "{case}"),
            Err(why) => {
                assert!(why.contains(said), "{case}: {why}");
                assert!(network.detail.contains(said), "{case}: {}", network.detail);
            }
        }
        assert_eq!(
            backend.egress_grant(GrantSpec::default()).is_ok(),
            open,
            "{case}"
        );
    }
}

/// A re-check runs while sessions start and preparations open grants: an
/// open port stays open while it runs, and is replaced only by what it
/// finds. A check that stops before the network check closes it.
#[tokio::test]
async fn a_recheck_never_closes_an_open_port_while_it_runs() {
    support::state::isolate();
    let api = KubeApi::new(NAMESPACE);
    api.insert_policy(harness_policy(&[7421, 8888, 7424]));
    let state = tempfile::tempdir().unwrap();
    let cfg = pod_config(state.path());
    let backend = Arc::new(backend(&api, &cfg));
    assert!(backend.check_all(&cfg, false).await.passed());
    assert!(backend.egress_port().is_open());

    let gate = api.hold_policy_reads();
    let reads = api.policy_reads();
    let checking = {
        let (backend, cfg) = (backend.clone(), cfg.clone());
        tokio::spawn(async move { backend.check_all(&cfg, false).await })
    };
    while api.policy_reads() == reads {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Mid-check, held at the policy read: still open, and grants still issue.
    assert!(backend.egress_port().is_open());
    assert!(backend.egress_grant(GrantSpec::default()).is_ok());
    gate.add_permits(16);
    assert!(checking.await.unwrap().passed());
    assert!(backend.egress_port().is_open());

    // A re-check that cannot even reach the runtime closes it.
    let mut loopback = cfg.clone();
    loopback.gateway.harness_listen = HarnessListen::Tcp("127.0.0.1:7421".parse().unwrap());
    let stopped = backend.check_all(&loopback, false).await;
    assert!(!stopped.passed());
    let why = backend.egress_port().verdict().unwrap().unwrap_err();
    assert!(why.contains("stopped before"), "{why}");
}
