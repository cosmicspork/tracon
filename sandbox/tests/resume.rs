//! A run outliving its connection: the real binary, cut off from its node
//! and taken back, with what its processes did meanwhile neither lost nor
//! repeated.

mod support;

use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tracon_sandbox::carrier::Listener;
use tracon_sandbox::node::{Accepted, SandboxConn, SandboxError, SandboxServer, ServerOptions};
use tracon_sandbox::protocol::{ListenAddr, SpawnSpec, OUTPUT_AHEAD};

use support::{start, start_with, Carrier};

fn sh(script: &str) -> SpawnSpec {
    let mut spec = SpawnSpec::new(["sh", "-c", script]);
    spec.stdin = false;
    spec.stderr = false;
    spec
}

/// The sandbox's next connection on `listener`, which must be it coming
/// back.
async fn taken_back(server: &SandboxServer, listener: &Listener) -> SandboxConn {
    let io = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("the sandbox redialled in time")
        .unwrap();
    match server.accept_any(io).await.unwrap() {
        Accepted::Resumed(conn) => conn,
        Accepted::New(_) => panic!("a redial is not a new run"),
    }
}

#[tokio::test]
async fn output_written_while_away_arrives_once_and_in_order() {
    let sandbox = start().await;
    let process = sandbox
        .conn
        .spawn(sh(
            "i=0; while [ $i -lt 300 ]; do echo line $i; i=$((i+1)); sleep 0.005; done",
        ))
        .await
        .unwrap();
    let mut stdout = process.stdout().await.unwrap();
    let reader = tokio::spawn(async move {
        let mut all = Vec::new();
        stdout.read_to_end(&mut all).await.map(|_| all)
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    sandbox.conn.sever();
    assert!(!sandbox.conn.is_connected());
    assert!(!sandbox.conn.is_closed());
    // The process keeps writing while nobody is there to read.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let back = taken_back(&sandbox.node.server, &sandbox.node.listener).await;
    assert_eq!(back.run(), "run");
    assert_eq!(sandbox.conn.connections(), 2);
    let all = reader.await.unwrap().unwrap();
    let expected: String = (0..300).map(|i| format!("line {i}\n")).collect();
    assert_eq!(String::from_utf8(all).unwrap(), expected);
    assert!(process.wait().await.unwrap().success());
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn stdin_carries_on_over_a_new_connection_without_repeats() {
    // Podman's carrier: the node's socket, which it keeps accepting on.
    let support::Sandbox {
        node,
        conn,
        mut child,
    } = start_with(Carrier::Unix, ServerOptions::default(), |_| {}).await;
    let support::Node {
        server,
        listener,
        dir: _dir,
        ..
    } = node;
    let _back = server.keep_accepting(listener, &conn).unwrap();
    let mut spec = SpawnSpec::new(["cat"]);
    spec.stderr = false;
    let process = conn.spawn(spec).await.unwrap();
    let mut stdin = process.stdin().await.unwrap();
    let mut stdout = process.stdout().await.unwrap();
    let data: Vec<u8> = (0..6 * 1024 * 1024u32)
        .map(|i| (i * 13 % 251) as u8)
        .collect();
    let expected = Sha256::digest(&data);
    let (half, go) = (
        tokio::sync::oneshot::channel(),
        tokio::sync::oneshot::channel(),
    );
    let (half_tx, half_rx) = half;
    let (go_tx, go_rx) = go;
    let writer = tokio::spawn(async move {
        let (first, second) = data.split_at(data.len() / 2);
        stdin.write_all(first).await.unwrap();
        half_tx.send(()).unwrap();
        go_rx.await.unwrap();
        stdin.write_all(second).await.unwrap();
        stdin.shutdown().await.unwrap();
    });
    let reader = tokio::spawn(async move {
        let mut echoed = Vec::new();
        stdout.read_to_end(&mut echoed).await.map(|_| echoed)
    });
    half_rx.await.unwrap();
    conn.sever();
    go_tx.send(()).unwrap();
    writer.await.unwrap();
    let echoed = reader.await.unwrap().unwrap();
    assert_eq!(echoed.len(), 6 * 1024 * 1024);
    assert_eq!(Sha256::digest(&echoed), expected);
    assert!(process.wait().await.unwrap().success());
    assert_eq!(conn.connections(), 2);
    conn.shutdown().await.unwrap();
    assert!(child.wait().await.unwrap().success());
}

#[tokio::test]
async fn a_process_blocks_rather_than_lose_output_while_the_node_is_away() {
    let sandbox = start().await;
    let dir = support::scratch();
    let marker = dir.path().join("done");
    const TOTAL: usize = 16 * 1024 * 1024;
    const { assert!(TOTAL > 2 * OUTPUT_AHEAD) };
    let process = sandbox
        .conn
        .spawn(sh(&format!(
            "head -c {TOTAL} /dev/zero; touch {}",
            marker.display()
        )))
        .await
        .unwrap();
    let mut stdout = process.stdout().await.unwrap();
    let mut first = vec![0u8; 1024 * 1024];
    stdout.read_exact(&mut first).await.unwrap();
    sandbox.conn.sever();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !marker.exists(),
        "the process ran on past what the sandbox may hold"
    );
    let _back = taken_back(&sandbox.node.server, &sandbox.node.listener).await;
    let mut rest = Vec::new();
    stdout.read_to_end(&mut rest).await.unwrap();
    assert_eq!(first.len() + rest.len(), TOTAL);
    assert!(rest.iter().all(|b| *b == 0));
    assert!(process.wait().await.unwrap().success());
    assert!(marker.exists());
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn an_exit_while_the_node_is_away_is_told_when_it_is_back() {
    let sandbox = start().await;
    let process = sandbox
        .conn
        .spawn(sh("sleep 0.3; echo finished; exit 7"))
        .await
        .unwrap();
    let mut stdout = process.stdout().await.unwrap();
    sandbox.conn.sever();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let waiting = {
        let conn = sandbox.conn.clone();
        tokio::spawn(async move {
            // Asked while away: it waits for the sandbox to come back.
            let _ = conn.services().status().await;
        })
    };
    let _back = taken_back(&sandbox.node.server, &sandbox.node.listener).await;
    waiting.await.unwrap();
    let mut out = String::new();
    stdout.read_to_string(&mut out).await.unwrap();
    assert_eq!(out, "finished\n");
    let status = process.wait().await.unwrap();
    assert_eq!(status.code, Some(7));
    // Asked again, the sandbox still knows.
    assert_eq!(process.wait().await.unwrap().code, Some(7));
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn a_tunnel_listens_through_a_dropped_connection() {
    let sandbox = start().await;
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = echo.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = stream.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    let tunnel = sandbox
        .conn
        .open_tunnel(
            "echo",
            ListenAddr::Tcp("127.0.0.1:0".into()),
            tracon_sandbox::node::Endpoint::Tcp(echo_addr),
        )
        .await
        .unwrap();
    let ListenAddr::Tcp(bound) = tunnel.bound().clone() else {
        panic!("a tcp tunnel")
    };
    sandbox.conn.sever();
    let _back = taken_back(&sandbox.node.server, &sandbox.node.listener).await;
    let mut client = tokio::net::TcpStream::connect(&bound).await.unwrap();
    client.write_all(b"still there").await.unwrap();
    let mut got = [0u8; 11];
    tokio::time::timeout(Duration::from_secs(10), client.read_exact(&mut got))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&got, b"still there");
    drop(tunnel);
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn a_listening_sandbox_listens_again_and_the_node_connects_in_again() {
    let node = support::Node::new(Carrier::Tcp, ServerOptions::default()).await;
    let token = node.server.tokens().issue("run");
    let socket = node.dir.path().join("sb.sock");
    let mut command = node.command(node.identity.pin(), &token);
    command
        .env_remove("TRACON_SANDBOX_ADDR")
        .env("TRACON_SANDBOX_LISTEN", &socket);
    let mut child = command.spawn().unwrap();
    let conn = node
        .server
        .connect_to(&socket, Duration::from_secs(10))
        .await
        .unwrap();
    let _again = node.server.keep_connecting(socket.clone(), &conn).unwrap();
    let process = conn.spawn(sh("sleep 0.5; echo after")).await.unwrap();
    let mut stdout = process.stdout().await.unwrap();
    conn.sever();
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(15), stdout.read_to_string(&mut out))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out, "after\n");
    assert_eq!(conn.connections(), 2);
    assert!(!socket.exists(), "it stops listening once taken back");
    conn.shutdown().await.unwrap();
    assert!(child.wait().await.unwrap().success());
}

#[tokio::test]
async fn a_node_with_another_certificate_is_refused_and_the_sandbox_ends() {
    let sandbox = start().await;
    let support::Sandbox {
        node,
        conn,
        mut child,
    } = sandbox;
    conn.sever();
    // What a node whose identity was rotated looks like to a sandbox: the
    // same address, another certificate. It fails closed at once rather
    // than wait out its window.
    let rotated = tracon_sandbox::tls::NodeIdentity::generate().unwrap();
    let impostor = SandboxServer::new(&rotated, ServerOptions::default()).unwrap();
    let io = node.listener.accept().await.unwrap();
    assert!(matches!(
        impostor.accept_any(io).await,
        Err(SandboxError::Tls(_))
    ));
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("the sandbox gave up at once")
        .unwrap();
    assert_eq!(status.code(), Some(3));
    drop(conn);
}

#[tokio::test]
async fn a_secret_is_good_only_on_its_own_runs_carrier() {
    let sandbox = start().await;
    sandbox.conn.sever();
    let io = sandbox.node.listener.accept().await.unwrap();
    assert!(matches!(
        sandbox.node.server.accept_for(io, "another-run").await,
        Err(SandboxError::BadToken)
    ));
    // Refused there, the sandbox gives up: a carrier bound to a run is only
    // ever reached by that run's sandbox, so this is not a case to wait out.
    let support::Sandbox { mut child, .. } = sandbox;
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(3));
}

#[tokio::test]
async fn a_run_the_node_ends_while_away_stops_waiting_at_once() {
    let sandbox = start().await;
    let process = sandbox.conn.spawn(sh("sleep 30")).await.unwrap();
    sandbox.conn.sever();
    let waiting = {
        let conn = sandbox.conn.clone();
        tokio::spawn(async move { conn.services().status().await })
    };
    // Its sandbox is known to be gone: nothing waits out the window.
    sandbox.node.server.end_run("run");
    assert!(sandbox.conn.is_closed());
    let asked = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(asked, Err(SandboxError::Lost)), "{asked:?}");
    assert!(matches!(process.wait().await, Err(SandboxError::Lost)));
    // And its secret went with it.
    let io = sandbox.node.listener.accept().await.unwrap();
    assert!(matches!(
        sandbox.node.server.accept_any(io).await,
        Err(SandboxError::BadToken)
    ));
    let support::Sandbox { mut child, .. } = sandbox;
    assert_eq!(child.wait().await.unwrap().code(), Some(3));
}
