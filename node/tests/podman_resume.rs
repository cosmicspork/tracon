//! A sandbox outliving its connection, against real Podman: the node's side
//! of a session's sandbox is cut off while a long command runs, the
//! supervisor redials the node's socket with its resume secret, and the
//! command's output arrives whole and in order, with its exit status.
//!
//! Runs only when asked for:
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 TRACON_SANDBOX_BUNDLE=<bundle>/<version>/amd64 \
//!   cargo test -p tracon --test podman_resume -- --test-threads 1
//! ```
//!
//! The bundle needs `bin/tracon-sandbox` built from this tree. The image is
//! `TRACON_SANDBOX_TEST_IMAGE` (default `docker.io/library/ubuntu:24.04`,
//! which must be in the local store). The sandbox runs with no network
//! (`sandbox_tunnels`), named with `TRACON_SANDBOX_TEST_PREFIX` (default
//! `spike-b1-`), and is removed again.

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tracon::config::{Config, HarnessListen};
use tracon::provider::podman::PodmanProvider;
use tracon::provider::{Bootstrap, CreateSpec, Resources, SandboxProvider, Shape};
use tracon::runner::podman::RunSpec;
use tracon::runner::RunLabels;
use tracon_sandbox::node::{SandboxServer, ServerOptions};
use tracon_sandbox::protocol::SpawnSpec;
use tracon_sandbox::tls::NodeIdentity;

use support::podman::{podman, requested};

#[tokio::test]
async fn a_long_command_carries_on_through_a_dropped_connection() {
    if !requested() {
        return;
    }
    support::state::isolate();
    let bundle = std::env::var("TRACON_SANDBOX_BUNDLE")
        .map(PathBuf::from)
        .expect("TRACON_PODMAN_TESTS=1 needs TRACON_SANDBOX_BUNDLE");
    let image = std::env::var("TRACON_SANDBOX_TEST_IMAGE")
        .unwrap_or_else(|_| "docker.io/library/ubuntu:24.04".into());
    assert!(podman(&["image", "exists", &image]).is_some(), "{image}");
    let prefix = std::env::var("TRACON_SANDBOX_TEST_PREFIX").unwrap_or_else(|_| "spike-b1-".into());
    let name = format!("{prefix}resume");
    let _ = podman(&["rm", "-f", &name]);
    let short = tempfile::Builder::new()
        .prefix("b1")
        .tempdir_in("/tmp")
        .unwrap();
    let mut cfg = Config::default();
    cfg.boundary.harness_image = image.clone();
    cfg.boundary.gateway_container = format!("{prefix}gw");
    cfg.gateway.harness_listen = HarnessListen::Unix(short.path().join("harness.sock"));
    cfg.runtime.podman.sandbox_protocol = true;
    cfg.runtime.podman.sandbox_tunnels = true;
    cfg.sandbox.bundle = Some(bundle);
    let mut spec = RunSpec::from_config(&cfg, true);
    spec.image = image.clone();
    let provider = PodmanProvider::from_config(&cfg, spec);

    let server = SandboxServer::new(
        &NodeIdentity::generate().unwrap(),
        ServerOptions {
            resume_within: Duration::from_secs(30),
            ..ServerOptions::default()
        },
    )
    .unwrap();
    let run = format!("{name}:run");
    let bootstrap = Bootstrap {
        run: run.clone(),
        cert_pin: server.pin().to_string(),
        token: server.tokens().issue(run.clone()),
    };
    let handle = provider
        .create(&CreateSpec {
            name: Some(&name),
            image: &image,
            resources: &Resources::default(),
            mounts: &[],
            bootstrap: &bootstrap,
            labels: &RunLabels::default(),
            shape: Shape::Session,
        })
        .await
        .unwrap();
    let conn = provider
        .connect(&handle, &server, Duration::from_secs(60))
        .await
        .unwrap();
    assert!(conn.is_resumable());

    let mut spawn = SpawnSpec::new([
        "sh",
        "-c",
        "i=0; while [ $i -lt 60 ]; do echo tick $i; i=$((i+1)); sleep 0.05; done; exit 9",
    ]);
    spawn.stdin = false;
    spawn.stderr = false;
    let process = conn.spawn(spawn).await.unwrap();
    let mut stdout = process.stdout().await.unwrap();
    let mut first = [0u8; 7];
    stdout.read_exact(&mut first).await.unwrap();
    assert_eq!(&first, b"tick 0\n");

    // Cut the node's side off twice: the supervisor dials the socket again
    // each time, and keeps the command running and its output while away.
    for _ in 0..2 {
        conn.sever();
        assert!(!conn.is_closed());
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    let mut rest = String::new();
    tokio::time::timeout(Duration::from_secs(30), stdout.read_to_string(&mut rest))
        .await
        .expect("the output carried on")
        .unwrap();
    let expected: String = (1..60).map(|i| format!("tick {i}\n")).collect();
    assert_eq!(rest, expected);
    let status = process.wait().await.unwrap();
    assert_eq!(status.code, Some(9));
    assert!(conn.connections() >= 3, "{}", conn.connections());

    let _ = tokio::time::timeout(Duration::from_secs(5), conn.shutdown()).await;
    drop(conn);
    provider.destroy(&handle).await.unwrap();
    assert!(podman(&["container", "exists", &name]).is_none());
}
