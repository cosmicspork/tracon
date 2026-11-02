//! The protocol runner, and the Podman provider's connect, against
//! `tracon-sandbox`'s own loop with no container: everything the protocol
//! carries, end to end over a real socket and TLS.
//!
//! The loop is the sandbox's init in a real sandbox, and reaps every child
//! of its process with `waitpid(-1)`. So it never shares a process with
//! another test (one test function here, which nextest runs alone), each
//! sandbox runs on a thread and runtime of its own that end with it, and
//! one sandbox is finished before the next starts.

#![cfg(target_os = "linux")]

#[path = "support/mod.rs"]
mod support;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tracon::provider::capabilities::{
    Capabilities, Dns, ImageIdentityKind, Privilege, Route, RuntimeSocket, Tier,
};
use tracon::provider::{
    Capacity, CreateSpec, Handle, ImageIdentity, Network, ProviderError, ProviderKind,
    SandboxProvider, Status,
};
use tracon::runner::protocol::ProtocolRunner;
use tracon::runner::{Mount, Runner, RunnerCommand};
use tracon_sandbox::carrier::Dial;
use tracon_sandbox::inside;
use tracon_sandbox::node::{SandboxConn, SandboxServer};
use tracon_sandbox::tls::CertPin;

/// Run the sandbox loop for `config` on a thread of its own.
fn sandbox(config: inside::Config) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let _ = runtime.block_on(inside::run(config));
        runtime.shutdown_timeout(Duration::from_secs(1));
    })
}

/// A provider whose "sandbox" is the loop on a thread, dialling a socket in
/// a temporary directory through a relay `destroy` cuts, as removing a
/// container cuts its connection.
#[derive(Default)]
struct Loopback {
    dir: Option<tempfile::TempDir>,
    listeners: Mutex<HashMap<String, tokio::net::UnixListener>>,
    relays: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    created: Mutex<Vec<(String, String, Vec<Mount>)>>,
    destroyed: Mutex<Vec<String>>,
    /// Present a token other than the one issued.
    wrong_token: bool,
}

impl Loopback {
    fn new(wrong_token: bool) -> Arc<Self> {
        Arc::new(Loopback {
            dir: Some(tempfile::tempdir().unwrap()),
            wrong_token,
            ..Default::default()
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir
            .as_ref()
            .unwrap()
            .path()
            .join(format!("{name}.sock"))
    }

    /// Wait for every sandbox to have ended, so no reaper outlives its test.
    async fn settle(&self) {
        let threads: Vec<_> = std::mem::take(&mut *self.threads.lock().unwrap());
        for thread in threads {
            tokio::time::timeout(
                Duration::from_secs(30),
                tokio::task::spawn_blocking(move || thread.join()),
            )
            .await
            .expect("a sandbox did not end")
            .unwrap()
            .unwrap();
        }
    }
}

#[async_trait]
impl SandboxProvider for Loopback {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Podman
    }

    async fn create(&self, spec: &CreateSpec<'_>) -> Result<Handle, ProviderError> {
        let name = spec.name.unwrap().to_string();
        self.created.lock().unwrap().push((
            name.clone(),
            spec.image.to_string(),
            spec.mounts.to_vec(),
        ));
        let path = self.path(&name);
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let token = match self.wrong_token {
            true => "forged".to_string(),
            false => spec.bootstrap.token.clone(),
        };
        let config = inside::Config::new(
            Dial::Unix(path),
            CertPin::parse(&spec.bootstrap.cert_pin).unwrap(),
            token,
        );
        self.threads.lock().unwrap().push(sandbox(config));
        self.listeners
            .lock()
            .unwrap()
            .insert(name.clone(), listener);
        Ok(Handle(name))
    }

    async fn connect(
        &self,
        handle: &Handle,
        server: &SandboxServer,
        within: Duration,
    ) -> Result<SandboxConn, ProviderError> {
        let listener = self
            .listeners
            .lock()
            .unwrap()
            .remove(&handle.0)
            .ok_or_else(|| ProviderError::NotFound(handle.0.clone()))?;
        let (mut accepted, _) = tokio::time::timeout(within, listener.accept())
            .await
            .map_err(|_| ProviderError::Failed("timed out".into()))?
            .map_err(|e| ProviderError::Failed(e.to_string()))?;
        let (mut near, far) = tokio::io::duplex(64 * 1024);
        let relay = tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut accepted, &mut near).await;
        });
        self.relays.lock().unwrap().insert(handle.0.clone(), relay);
        server
            .accept(far)
            .await
            .map_err(|e| ProviderError::Failed(e.to_string()))
    }

    async fn destroy(&self, handle: &Handle) -> Result<(), ProviderError> {
        self.destroyed.lock().unwrap().push(handle.0.clone());
        self.listeners.lock().unwrap().remove(&handle.0);
        // The loop sees its connection lost and tears its processes down
        // itself.
        if let Some(relay) = self.relays.lock().unwrap().remove(&handle.0) {
            relay.abort();
        }
        Ok(())
    }

    async fn status(&self, _: &Handle) -> Result<Status, ProviderError> {
        Ok(Status::Running)
    }

    async fn image_identity(&self, _: &Handle) -> Result<ImageIdentity, ProviderError> {
        Err(self.unsupported("image identity"))
    }

    async fn network(&self, _: &Handle) -> Result<Network, ProviderError> {
        Err(self.unsupported("network"))
    }

    async fn verify_isolation(&self, _: &Handle) -> Result<Capabilities, ProviderError> {
        Ok(self.capabilities())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tier: Tier::Container,
            route: Route::NodeOnly,
            dns: Dns::None,
            privilege: Privilege::Unprivileged,
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Exact,
        }
    }

    async fn capacity(&self) -> Result<Capacity, ProviderError> {
        Ok(Capacity {
            running: 0,
            limit: None,
        })
    }
}

fn runner(provider: Arc<Loopback>) -> ProtocolRunner {
    ProtocolRunner::new(provider, "img@sha256:aa")
        .with_mounts(vec![Mount::volume("state", "/state", false)])
        .with_connect_within(Duration::from_secs(10))
}

fn sh(script: &str, name: &str) -> RunnerCommand {
    RunnerCommand {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        name: name.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_protocol_runner_over_a_loopback_sandbox() {
    support::state::isolate();
    a_capture_runs_with_its_env_cwd_and_exit_code().await;
    a_spawned_harness_streams_stdio_and_reports_its_exit().await;
    kill_removes_the_sandbox_and_the_harness_ends().await;
    a_dropped_capture_still_removes_its_sandbox().await;
    a_sandbox_with_the_wrong_token_is_refused_and_removed().await;
    the_podman_provider_takes_the_supervisors_connection_past_a_bad_one().await;
}

async fn a_capture_runs_with_its_env_cwd_and_exit_code() {
    let provider = Loopback::new(false);
    let r = runner(provider.clone());
    let work = tempfile::tempdir().unwrap();
    let workdir = work.path().to_string_lossy().into_owned();
    let mut cmd = sh(
        "echo \"$PLAIN $HIDDEN $(pwd)\"; echo oops >&2; exit 7",
        "tracon-x-capture",
    );
    cmd.workdir = Some(workdir.clone());
    cmd.env = vec![("PLAIN".into(), "seen".into())];
    cmd.secret_env = tracon::runner::SecretEnv::new(vec![("HIDDEN".into(), "lent".into())]);
    cmd.image = Some("repo@sha256:bb".into());
    cmd.mounts = vec![Mount::volume("ws", &workdir, false)];
    let out = r.run_capture(cmd).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("seen lent {workdir}\n")
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "oops\n");
    assert_eq!(out.status.code(), Some(7));

    let name = r.capture_name("tracon-x-capture");
    let created = provider.created.lock().unwrap().clone();
    assert_eq!(created[0].0, name);
    assert_eq!(created[0].1, "repo@sha256:bb");
    assert_eq!(
        created[0].2,
        vec![
            Mount::volume("state", "/state", false),
            Mount::volume("ws", &workdir, false)
        ]
    );
    assert_eq!(*provider.destroyed.lock().unwrap(), vec![name]);
    provider.settle().await;
}

async fn a_spawned_harness_streams_stdio_and_reports_its_exit() {
    let provider = Loopback::new(false);
    let r = runner(provider.clone());
    let mut spawned = r
        .spawn(sh(
            "while read line; do echo \"got $line\"; done; exit 3",
            "tracon-h-stdio",
        ))
        .await
        .unwrap();
    let mut lines = BufReader::new(spawned.stdout).lines();
    for n in 0..50 {
        spawned
            .stdin
            .write_all(format!("{n}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(
            lines.next_line().await.unwrap().unwrap(),
            format!("got {n}")
        );
    }
    spawned.stdin.shutdown().await.unwrap();
    assert_eq!(lines.next_line().await.unwrap(), None);
    assert_eq!(spawned.done.await.unwrap(), 3);
    assert_eq!(
        *provider.destroyed.lock().unwrap(),
        vec!["tracon-h-stdio".to_string()]
    );
    provider.settle().await;
}

async fn kill_removes_the_sandbox_and_the_harness_ends() {
    let provider = Loopback::new(false);
    let r = runner(provider.clone());
    let spawned = r.spawn(sh("sleep 15", "tracon-h-kill")).await.unwrap();
    // A runner made afresh, as every caller makes one.
    runner(provider.clone())
        .kill("tracon-h-kill")
        .await
        .unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(10), spawned.done)
        .await
        .expect("the harness outlived its sandbox");
    assert!(ended.is_err() || ended.unwrap() != 0);
    assert!(provider
        .destroyed
        .lock()
        .unwrap()
        .contains(&"tracon-h-kill".to_string()));
    provider.settle().await;
}

async fn a_dropped_capture_still_removes_its_sandbox() {
    let provider = Loopback::new(false);
    let r = runner(provider.clone());
    let capture = r.run_capture(sh("sleep 15", "tracon-x-drop"));
    assert!(tokio::time::timeout(Duration::from_millis(500), capture)
        .await
        .is_err());
    let name = r.capture_name("tracon-x-drop");
    for _ in 0..250 {
        if provider.destroyed.lock().unwrap().contains(&name) {
            provider.settle().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the abandoned sandbox was never removed");
}

async fn a_sandbox_with_the_wrong_token_is_refused_and_removed() {
    let provider = Loopback::new(true);
    let error = runner(provider.clone())
        .run_capture(sh("true", "tracon-x-forged"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("token"), "{error}");
    assert_eq!(provider.destroyed.lock().unwrap().len(), 1);
    provider.settle().await;
}

/// The Podman provider's listener, made by `create`, is the one the
/// supervisor reaches; a connection that is not the supervisor's is passed
/// over. Podman itself is a script here.
async fn the_podman_provider_takes_the_supervisors_connection_past_a_bad_one() {
    use tokio::io::AsyncWriteExt as _;
    use tracon::provider::podman::{CliError, PodmanCli, PodmanOptions, PodmanProvider};
    use tracon::provider::{Bootstrap, Resources};

    struct Script;
    #[async_trait]
    impl PodmanCli for Script {
        async fn run(&self, args: &[String], _: &[(String, String)]) -> Result<String, CliError> {
            Ok(match args[0].as_str() {
                "image" => "null".into(),
                "container" => serde_json::json!([{"State": {"Status": "running"}}]).to_string(),
                _ => String::new(),
            })
        }
    }

    let dir = tempfile::Builder::new()
        .prefix("pc")
        .tempdir_in("/tmp")
        .unwrap();
    let bundle = dir.path().join("bundle");
    std::fs::create_dir_all(bundle.join("bin")).unwrap();
    std::fs::write(bundle.join("bin/tracon-sandbox"), b"").unwrap();
    let provider = PodmanProvider::with_cli(
        PodmanOptions {
            spec: tracon::runner::podman::RunSpec::from_config(
                &tracon::config::Config::default(),
                false,
            ),
            bundle,
            socket_dir: dir.path().join("s"),
            init: false,
            limit: None,
            connect_timeout: Duration::from_secs(10),
        },
        Box::new(Script),
    );
    let server = tracon::runner::protocol::node_server().unwrap();
    let token = server.tokens().issue("run-p");
    let bootstrap = Bootstrap {
        run: "run-p".into(),
        cert_pin: server.pin().to_string(),
        token: token.clone(),
    };
    let handle = provider
        .create(&CreateSpec {
            name: Some("tracon-h-p"),
            image: "img",
            resources: &Resources::default(),
            mounts: &[],
            bootstrap: &bootstrap,
        })
        .await
        .unwrap();
    let socket = provider.socket_dir(&handle.0).join("node.sock");

    // Something that is not the supervisor connects first.
    let mut stray = tokio::net::UnixStream::connect(&socket).await.unwrap();
    stray.write_all(b"not tls at all").await.unwrap();
    drop(stray);
    let thread = sandbox(inside::Config::new(Dial::Unix(socket), server.pin(), token));
    let conn = provider
        .connect(&handle, &server, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(conn.run(), "run-p");
    let out = conn
        .spawn(tracon_sandbox::protocol::SpawnSpec::new([
            "sh", "-c", "echo hi",
        ]))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"hi\n");
    conn.shutdown().await.unwrap();
    tokio::task::spawn_blocking(move || thread.join())
        .await
        .unwrap()
        .unwrap();
    // Taken once.
    assert!(provider
        .connect(&handle, &server, Duration::from_secs(1))
        .await
        .is_err());
    provider.destroy(&handle).await.unwrap();
    assert!(!provider.socket_dir(&handle.0).exists());
}
