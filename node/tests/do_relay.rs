//! The DigitalOcean provider reaching its sandboxes through the hub's own
//! URL, minus DigitalOcean: `[digitalocean] relay` with no `node_addr`.
//! A stand-in for its API (`support::do_api`) whose `exec` starts
//! `tracon-sandbox`'s own loop with the bootstrap the node delivered; that
//! loop dials the hub's `/v0/relay/connect` as a WebSocket, the hub routes
//! it by SNI to this node's relay bridge, and the bridge hands it to the
//! provider's listener. The egress DigitalOcean is asked for names the hub
//! alone, and the route reads as unverified.
//!
//! The process stands in for the runtime's environment, as its init gives
//! it to the supervisor: the runtime's proxy and a `NO_PROXY` that does not
//! name the gateway. The harness must get the node's proxy at the gateway
//! tunnel instead, while the supervisor still dials through the runtime's
//! proxy, from the bootstrap's copy of it (here a stand-in that records each
//! tunnel).
//!
//! The loop reaps every child of its process with `waitpid(-1)`, so this is
//! one test function in a file of its own (see `protocol_runner.rs`).

#![cfg(target_os = "linux")]

#[path = "support/mod.rs"]
mod support;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hub::sandbox_relay::{RelayConfig, SandboxRelay as HubSide};
use hub::store::{Member, MemberStore, MemoryFrames, MemoryMembers};
use tracon::config::{Config, HarnessListen, SandboxRelay as NodeRelayConfig};
use tracon::mesh::sandbox_relay::SandboxRelay;
use tracon::provider::capabilities::Route;
use tracon::provider::digitalocean::bootstrap::{Delivery, Dial as BootstrapDial};
use tracon::provider::digitalocean::listener::DoListener;
use tracon::provider::digitalocean::runtime::DoRuntime;
use tracon::provider::digitalocean::templates::{Source, TemplateState};
use tracon::provider::digitalocean::{ClientSource, HubRelay, Owner, Settings};
use tracon::provider::{Requirements, SandboxProvider as _};
use tracon::runner::sandbox_files::FilesStore;
use tracon::runner::RunnerCommand;
use tracon_sandbox::carrier::{Dial, RelayUrl};
use tracon_sandbox::inside;
use tracon_sandbox::tls::CertPin;

use support::do_api::FakeDo;
use support::mesh::{identity, WAIT};

const IMAGE: &str =
    "ghcr.io/example/tracon-do-sandbox@sha256:3333333333333333444444444444444444444444444444444444444444444444";

type Sandbox = std::thread::JoinHandle<Result<(), String>>;

fn sandbox(config: inside::Config) -> Sandbox {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let ended = runtime.block_on(inside::run(config));
        runtime.shutdown_timeout(Duration::from_secs(1));
        ended.map(|_| ()).map_err(|e| e.to_string())
    })
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A hub with the sandbox relay on and no relay listener: the WebSocket
/// route on its own address is the only way in.
async fn start_hub(node: &proto::keys::Identity) -> SocketAddr {
    let members = Arc::new(MemoryMembers::new());
    members
        .put(&Member {
            node_id: node.node_id(),
            x25519_pub: node.x25519_hex(),
            binding_sig: proto::enroll::sign_binding(node),
            name: "n".into(),
            channels: vec![proto::frame::MESH_CHANNEL.into()],
            admitted_ms: 0,
            admitted_by: "test".into(),
            role: Default::default(),
        })
        .unwrap();
    let relay = HubSide::new(RelayConfig::new("relay.test"), members.clone());
    let mut state = hub::state_for(
        Arc::new(MemoryFrames::new()),
        members,
        hub::HubConfig::default(),
        Arc::new(hub::pokes::PokeHub::new()),
        None,
    );
    state.relay = Some(relay);
    let app = hub::app_with_state(state);
    let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = api.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(api, app).await.unwrap() });
    addr
}

/// A forward proxy that answers `CONNECT host:port` and records each target.
async fn connect_proxy() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let record = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let record = record.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let Some(target) = head
                    .lines()
                    .next()
                    .and_then(|l| l.strip_prefix("CONNECT "))
                    .and_then(|l| l.split(' ').next())
                    .map(str::to_string)
                else {
                    return;
                };
                record.lock().unwrap().push(target.clone());
                let Ok(mut upstream) = tokio::net::TcpStream::connect(&target).await else {
                    return;
                };
                if client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (addr, seen)
}

/// The runtime's proxy, as its init hands it to the supervisor.
const DO_PROXY: &str = "http://169.254.169.254:3128";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_digitalocean_sandbox_reaches_the_node_over_the_hubs_url() {
    support::state::isolate();
    // The runtime's environment, which a sandbox's processes inherit unless
    // the node gives them their own. Its NO_PROXY spares the node's own
    // loopback calls here, as it spares the runtime's metadata address.
    for name in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
        std::env::set_var(name, DO_PROXY);
    }
    for name in ["NO_PROXY", "no_proxy"] {
        std::env::set_var(
            name,
            "169.254.169.254,localhost,127.0.0.1,.vpc-endpoint.internal.digitalocean.com",
        );
    }
    let (do_proxy, tunnelled) = connect_proxy().await;
    let fake = FakeDo::start().await;

    let server = tracon::runner::protocol::node_server().unwrap();
    let listener = DoListener::bind("127.0.0.1:0".parse().unwrap(), server.clone())
        .await
        .unwrap();

    // The node: no address a sandbox could dial, a relay bridge to its
    // listener, and the hub named by a host name, as a real one is.
    let node = identity(61);
    let hub = start_hub(&node).await;
    let hub_url = format!("http://localhost:{}", hub.port());
    let bridge = SandboxRelay::new(
        Arc::new(node),
        &hub_url,
        &NodeRelayConfig {
            enabled: true,
            target: listener.local_addr().to_string(),
            max_streams: 4,
        },
    );
    bridge.spawn();
    bridge.publish();
    let mut held = bridge.host();
    let relay_host = tokio::time::timeout(WAIT, held.wait_for(|h| h.is_some()))
        .await
        .expect("the node registered with the hub's relay")
        .unwrap()
        .clone()
        .unwrap();

    let mut cfg = Config::default();
    cfg.runtime.podman.sandbox_protocol = true;
    cfg.runtime.podman.sandbox_files = true;
    cfg.runtime.podman.sandbox_tunnels = true;
    cfg.gateway.forward_port = free_port();
    cfg.gateway.proxy_port = free_port();
    cfg.gateway.qa_proxy_port = 0;
    cfg.gateway.harness_listen = HarnessListen::Tcp(([127, 0, 0, 1], free_port()).into());
    let settings = Settings {
        credential: "do".into(),
        api_url: fake.url.clone(),
        image: IMAGE.into(),
        listen: listener.local_addr().to_string(),
        relay: true,
        limit: 2,
        ..Settings::default()
    };
    let client: ClientSource = {
        let fake = fake.clone();
        Arc::new(move || Ok(fake.client()))
    };
    let runtime = DoRuntime::with(
        client,
        settings,
        Source::Given(IMAGE.into()),
        Owner::new("tracon", "abcd1234"),
        "tracon-gw".into(),
        listener.clone(),
        HubRelay {
            host: {
                let host = relay_host.clone();
                Arc::new(move || Some(host.clone()))
            },
            hub_url: Some(hub_url.clone()),
        },
    )
    .unwrap();
    let provider = runtime.provider().clone();
    // The stand-in's supervisor is this test's own loop, as its user.
    let me = tracon_sandbox::inside::user::current().uid;
    provider.expect_uid(Some(me));
    assert_eq!(provider.capabilities().route, Route::Unverified);
    // This hub serves the relay's WebSocket, so nothing is held against it.
    runtime.clone().check_hub(hub_url.clone()).await;

    let threads: Arc<Mutex<Vec<Sandbox>>> = Arc::default();
    fake.on_exec({
        let threads = threads.clone();
        Arc::new(move |_session: &str, argv: &[String]| {
            let Some(d) = Delivery::parse(argv) else {
                return 2;
            };
            let BootstrapDial::RelayUrl(url) = d.dial else {
                return 3;
            };
            // The supervisor's environment after the bootstrap: the
            // runtime's proxy copied under its own names, ahead of the
            // runtime's, which the hub (on loopback here) would bypass.
            let env = |name: &str| match name {
                "TRACON_SANDBOX_PROXY" => Some(format!("http://{do_proxy}")),
                "TRACON_SANDBOX_NO_PROXY" => Some("169.254.169.254".to_string()),
                other => std::env::var(other).ok(),
            };
            let mut config = inside::Config::new(
                Dial::WebSocket(RelayUrl::from_env(url, env).unwrap()),
                CertPin::parse(d.pin).unwrap(),
                d.token,
            );
            config.server_name = d.server_name.to_string();
            threads.lock().unwrap().push(sandbox(config));
            0
        })
    });

    while !matches!(
        runtime.ensure_template(IMAGE).await,
        TemplateState::Ready { .. }
    ) {}

    let tmp = tempfile::tempdir().unwrap();
    let runner = runtime.runner_with(
        &cfg,
        Requirements::default(),
        Vec::new(),
        IMAGE.into(),
        tracon::adapter::claude::ClaudeAdapter::ID,
        FilesStore::new(tmp.path().join("store")),
    );
    let out = runner
        .run_capture(RunnerCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                r#"echo over the hub; echo "$HTTPS_PROXY|$HTTP_PROXY|$https_proxy|$http_proxy"; echo "$NO_PROXY|$no_proxy"; echo "$HOME|$CLAUDE_CONFIG_DIR""#.into(),
            ],
            name: "tracon-h-ws1".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    // The harness gets the node's proxy at the gateway tunnel, and reaches
    // the gateway's name directly, not through the runtime's proxy.
    let proxy = format!("http://tracon-gw:{}", cfg.gateway.proxy_port);
    let direct = "tracon-gw,localhost,127.0.0.1,::1";
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "over the hub\n{proxy}|{proxy}|{proxy}|{proxy}\n{direct}|{direct}\n\
             /home/harness|/home/harness/.claude\n"
        )
    );
    // While the supervisor dialled the hub through the runtime's proxy.
    assert!(
        tunnelled
            .lock()
            .unwrap()
            .contains(&format!("localhost:{}", hub.port())),
        "{:?}",
        tunnelled.lock().unwrap()
    );

    let (manifest, exec) = {
        let api = fake.api.lock().unwrap();
        (api.manifests[0].clone(), api.execs[0].clone())
    };
    // DigitalOcean is asked to admit the hub's name and nothing else.
    assert_eq!(
        manifest["egress"],
        serde_json::json!({"allow_hosts": ["localhost"]})
    );
    let delivered = Delivery::parse(&exec.1).unwrap();
    assert_eq!(
        delivered.dial,
        BootstrapDial::RelayUrl(&format!(
            "ws://localhost:{}{}",
            hub.port(),
            proto::relay::CONNECT_PATH
        ))
    );
    assert_eq!(delivered.server_name, relay_host);
    assert!(!manifest.to_string().contains(delivered.token));

    // Work that asks for a node-only route is not placed in such a sandbox.
    let strict = runtime.runner_with(
        &cfg,
        Requirements {
            route: Some(vec![Route::NodeOnly]),
            ..Requirements::default()
        },
        Vec::new(),
        IMAGE.into(),
        tracon::adapter::claude::ClaudeAdapter::ID,
        FilesStore::new(tmp.path().join("store")),
    );
    let refused = strict
        .run_capture(RunnerCommand {
            argv: vec!["true".into()],
            name: "tracon-h-ws2".into(),
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("route is unverified"), "{refused}");

    // A hub from before the relay's WebSocket: the provider says so, rather
    // than leaving each sandbox's dial to fail unexplained.
    runtime.health(&cfg).unwrap();
    let old = axum::Router::new().route(
        "/v0/info",
        axum::routing::get(|| async {
            axum::Json(serde_json::json!({"sandbox_relay": {"domain": "relay.test"}}))
        }),
    );
    let old_hub = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old_url = format!("http://{}", old_hub.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(old_hub, old).await.unwrap() });
    runtime.clone().check_hub(old_url).await;
    let why = runtime.health(&cfg).unwrap_err();
    assert!(
        why.contains(tracon::provider::digitalocean::HUB_WITH_CONNECT),
        "{why}"
    );
    // Settings shows the hub's URL as the way in, the route as unverified,
    // and why no session goes there now.
    let report = serde_json::to_value(runtime.report(&cfg).await).unwrap();
    assert_eq!(report["verified"], false);
    let route = report["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["capability"] == "route")
        .unwrap()
        .clone();
    assert_eq!(route["value"], "unverified");
    assert!(
        route["detail"].as_str().unwrap().contains("localhost"),
        "{route}"
    );
    let elsewhere = &report["elsewhere"];
    assert_eq!(elsewhere["reach"], "hub url");
    assert_eq!(
        elsewhere["dial"],
        format!(
            "ws://localhost:{}{}",
            hub.port(),
            proto::relay::CONNECT_PATH
        )
    );
    assert_eq!(elsewhere["egress"], serde_json::json!(["localhost"]));
    assert_eq!(elsewhere["healthy"], false);
    assert!(
        elsewhere["why"]
            .as_str()
            .unwrap()
            .contains(tracon::provider::digitalocean::HUB_WITH_CONNECT),
        "{elsewhere}"
    );

    let ended = std::mem::take(&mut *threads.lock().unwrap());
    for thread in ended {
        let _ = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::task::spawn_blocking(move || thread.join().unwrap()),
        )
        .await
        .expect("a sandbox did not end");
    }
    assert!(fake.api.lock().unwrap().sessions.is_empty());
}
