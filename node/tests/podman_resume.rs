//! A sandbox outliving its connection, and the node process it was started
//! by, against real Podman: a long command keeps running while the node's
//! side is cut off, or dropped and made again as a restarted node makes it,
//! and its output arrives whole and in order, with its exit status.
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
use tracon::provider::{Bootstrap, CreateSpec, Handle, Resources, SandboxProvider, Shape};
use tracon::runner::podman::RunSpec;
use tracon::runner::RunLabels;
use tracon_sandbox::node::{SandboxConn, SandboxServer, ServerOptions};
use tracon_sandbox::protocol::SpawnSpec;
use tracon_sandbox::tls::NodeIdentity;

use support::podman::{podman, requested};

/// Sixty lines over three seconds, then exit 9.
const TICKS: &str =
    "i=0; while [ $i -lt 60 ]; do echo tick $i; i=$((i+1)); sleep 0.05; done; exit 9";

struct Rig {
    cfg: Config,
    image: String,
    name: String,
    _short: tempfile::TempDir,
}

impl Rig {
    fn new(tag: &str) -> Option<Rig> {
        if !requested() {
            return None;
        }
        support::state::isolate();
        let bundle = std::env::var("TRACON_SANDBOX_BUNDLE")
            .map(PathBuf::from)
            .expect("TRACON_PODMAN_TESTS=1 needs TRACON_SANDBOX_BUNDLE");
        let image = std::env::var("TRACON_SANDBOX_TEST_IMAGE")
            .unwrap_or_else(|_| "docker.io/library/ubuntu:24.04".into());
        assert!(podman(&["image", "exists", &image]).is_some(), "{image}");
        let prefix =
            std::env::var("TRACON_SANDBOX_TEST_PREFIX").unwrap_or_else(|_| "spike-b1-".into());
        let name = format!("{prefix}{tag}");
        let _ = podman(&["rm", "-f", &name]);
        let short = tempfile::Builder::new()
            .prefix("b1")
            .tempdir_in("/tmp")
            .unwrap();
        let mut cfg = Config::default();
        cfg.sandbox.base_image = Some(image.clone());
        cfg.boundary.gateway_container = format!("{prefix}gw");
        cfg.gateway.harness_listen = HarnessListen::Unix(short.path().join("harness.sock"));
        cfg.sandbox.bundle = Some(bundle);
        Some(Rig {
            cfg,
            image,
            name,
            _short: short,
        })
    }

    /// A provider as a node process makes one.
    fn provider(&self) -> PodmanProvider {
        let mut spec = RunSpec::from_config(&self.cfg, true);
        spec.image = self.image.clone();
        PodmanProvider::from_config(&self.cfg, spec)
    }

    /// A sandbox made and connected for `run`.
    async fn sandbox(
        &self,
        provider: &PodmanProvider,
        server: &SandboxServer,
        run: &str,
    ) -> (Handle, SandboxConn) {
        let bootstrap = Bootstrap {
            run: run.to_string(),
            cert_pin: server.pin().to_string(),
            token: server.tokens().issue(run),
        };
        let handle = provider
            .create(&CreateSpec {
                name: Some(&self.name),
                image: &self.image,
                resources: &Resources::default(),
                mounts: &[],
                bootstrap: &bootstrap,
                labels: &RunLabels::default(),
                shape: Shape::Session,
            })
            .await
            .unwrap();
        let conn = provider
            .connect(&handle, server, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(conn.is_resumable());
        (handle, conn)
    }

    async fn finish(&self, provider: &PodmanProvider, handle: &Handle, conn: SandboxConn) {
        let _ = tokio::time::timeout(Duration::from_secs(5), conn.shutdown()).await;
        drop(conn);
        provider.destroy(handle).await.unwrap();
        assert!(podman(&["container", "exists", &self.name]).is_none());
    }
}

fn server(identity: &NodeIdentity) -> SandboxServer {
    SandboxServer::new(
        identity,
        ServerOptions {
            resume_within: Duration::from_secs(30),
            ..ServerOptions::default()
        },
    )
    .unwrap()
}

fn ticks() -> SpawnSpec {
    let mut spawn = SpawnSpec::new(["sh", "-c", TICKS]);
    spawn.stdin = false;
    spawn.stderr = false;
    spawn
}

fn ticks_from(first: u32) -> String {
    (first..60).map(|i| format!("tick {i}\n")).collect()
}

#[tokio::test]
async fn a_long_command_carries_on_through_a_dropped_connection() {
    let Some(rig) = Rig::new("resume") else {
        return;
    };
    let provider = rig.provider();
    let server = server(&NodeIdentity::generate().unwrap());
    let (handle, conn) = rig.sandbox(&provider, &server, "resume:run").await;

    let process = conn.spawn(ticks()).await.unwrap();
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
    assert_eq!(rest, ticks_from(1));
    let status = process.wait().await.unwrap();
    assert_eq!(status.code, Some(9));
    assert!(conn.connections() >= 3, "{}", conn.connections());
    rig.finish(&provider, &handle, conn).await;
}

/// What a node restart is to a sandbox: the node process that started it
/// goes, every handle with it, and another with the same identity knows the
/// run again by its secret's digest, sets the carrier up again and finds
/// the command by its id, reading on from where the first had got to.
#[tokio::test]
async fn a_long_command_carries_on_when_the_node_side_is_made_again() {
    let Some(rig) = Rig::new("restart") else {
        return;
    };
    let identity = NodeIdentity::generate().unwrap();
    let run = "restart:run";
    let (handle, id, offset, digest) = {
        let provider = rig.provider();
        let server = server(&identity);
        let (handle, conn) = rig.sandbox(&provider, &server, run).await;
        let process = conn.spawn(ticks()).await.unwrap();
        let mut stdout = process.stdout().await.unwrap();
        let mut first = [0u8; 14];
        stdout.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"tick 0\ntick 1\n");
        let digest = server.tokens().resume_digest(run).unwrap();
        (handle, process.id(), stdout.offset(), digest)
        // The provider, the server, the run and its reader all go here.
    };
    // The command runs on while no node process is there.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let provider = rig.provider();
    let server = server(&identity);
    let conn = server.restore(run, digest);
    provider.reattach(&handle, &server, &conn).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(30), conn.connected())
            .await
            .unwrap(),
        "the sandbox came back"
    );
    let process = conn.process(id);
    let mut stdout = process.stdout_from(offset).await.unwrap();
    let mut rest = String::new();
    tokio::time::timeout(Duration::from_secs(30), stdout.read_to_string(&mut rest))
        .await
        .expect("the output carried on")
        .unwrap();
    assert_eq!(rest, ticks_from(2));
    assert_eq!(process.wait().await.unwrap().code, Some(9));
    rig.finish(&provider, &handle, conn).await;
}
