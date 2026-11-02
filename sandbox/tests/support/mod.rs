//! A node-side harness around the real `tracon-sandbox` binary: it issues a
//! token, starts the binary with its bootstrap environment pointing at a
//! loopback listener, and accepts the connection.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tracon_sandbox::carrier::{bind_unix_labelled, Listener};
use tracon_sandbox::node::{SandboxConn, SandboxError, SandboxServer, ServerOptions};
use tracon_sandbox::tls::{CertPin, NodeIdentity};

pub const BIN: &str = env!("CARGO_BIN_EXE_tracon-sandbox");

pub struct Node {
    pub identity: NodeIdentity,
    pub server: SandboxServer,
    pub listener: Listener,
    pub dir: tempfile::TempDir,
}

pub enum Carrier {
    Tcp,
    Unix,
}

impl Node {
    pub async fn new(carrier: Carrier, options: ServerOptions) -> Node {
        let identity = NodeIdentity::generate().unwrap();
        let server = SandboxServer::new(&identity, options).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let listener = match carrier {
            Carrier::Tcp => {
                Listener::Tcp(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
            }
            Carrier::Unix => {
                Listener::Unix(bind_unix_labelled(&dir.path().join("node.sock"), None).unwrap())
            }
        };
        Node {
            identity,
            server,
            listener,
            dir,
        }
    }

    /// The sandbox binary, configured to dial this node.
    pub fn command(&self, pin: CertPin, token: &str) -> Command {
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TRACON_SANDBOX_PIN", pin.to_string())
            .env("TRACON_SANDBOX_TOKEN", token)
            .env("TRACON_SANDBOX_LOG", "warn")
            .env("TRACON_SANDBOX_CONNECT_TIMEOUT_MS", "5000")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        match self.listener.dial().unwrap() {
            tracon_sandbox::carrier::Dial::Tcp(addr) => command.env("TRACON_SANDBOX_ADDR", addr),
            tracon_sandbox::carrier::Dial::Unix(path) => command.env("TRACON_SANDBOX_SOCKET", path),
            tracon_sandbox::carrier::Dial::Listen(_) => unreachable!("a listener dials"),
        };
        command
    }

    pub async fn accept(&self) -> Result<SandboxConn, SandboxError> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.server.accept_from(&self.listener),
        )
        .await
        .expect("the sandbox dialled in time")
    }
}

pub struct Sandbox {
    pub node: Node,
    pub conn: SandboxConn,
    pub child: Child,
}

pub async fn start() -> Sandbox {
    start_with(Carrier::Tcp, ServerOptions::default(), |_| {}).await
}

pub async fn start_with(
    carrier: Carrier,
    options: ServerOptions,
    configure: impl FnOnce(&mut Command),
) -> Sandbox {
    let node = Node::new(carrier, options).await;
    let token = node.server.tokens().issue("run");
    let mut command = node.command(node.identity.pin(), &token);
    configure(&mut command);
    let child = command.spawn().unwrap();
    let conn = node.accept().await.unwrap();
    Sandbox { node, conn, child }
}

impl Sandbox {
    /// Ask the sandbox to stop and wait for it to exit.
    pub async fn finish(mut self) -> std::process::ExitStatus {
        self.conn.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(15), self.child.wait())
            .await
            .expect("the sandbox exited in time")
            .unwrap()
    }
}

pub fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

pub fn path(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    dir.path().join(name)
}
