//! Workspaces and harness state over the sandbox protocol's files channel
//! (`runner::sandbox_files`), against `tracon-sandbox`'s own loop with no
//! container: the trees a run is sent, what comes back at a turn's end,
//! during a long turn, at exit and before a kill, what a lost sandbox costs,
//! a live export as review takes one, and a hostile sandbox's tree.
//!
//! The loop reaps every child of its process with `waitpid(-1)`, so this is
//! one test function in a file of its own (see `protocol_runner.rs`), and
//! its "sandbox" is a thread whose paths are a temporary directory's.

#![cfg(target_os = "linux")]

#[path = "support/mod.rs"]
mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tracon::provider::capabilities::{
    Capabilities, Dns, ImageIdentityKind, Privilege, Route, RuntimeSocket, Tier,
};
use tracon::provider::{
    Capacity, CreateSpec, Handle, ImageIdentity, Network, ProviderError, ProviderKind,
    SandboxProvider, Status,
};
use tracon::runner::protocol::ProtocolRunner;
use tracon::runner::sandbox_files::{self, FilesStore};
use tracon::runner::{Mount, RunKind, RunLabels, Runner, RunnerCommand};
use tracon_sandbox::carrier::Dial;
use tracon_sandbox::inside;
use tracon_sandbox::node::{SandboxConn, SandboxServer};
use tracon_sandbox::tls::CertPin;

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

/// A provider whose sandbox is the loop on a thread, reached through a relay
/// that `destroy` cuts as removing a container cuts its connection.
#[derive(Default)]
struct Loopback {
    dir: Option<tempfile::TempDir>,
    listeners: Mutex<HashMap<String, tokio::net::UnixListener>>,
    relays: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    created: Mutex<Vec<Vec<Mount>>>,
}

impl Loopback {
    fn new() -> Arc<Self> {
        Arc::new(Loopback {
            dir: Some(tempfile::tempdir().unwrap()),
            ..Default::default()
        })
    }

    /// Cut a sandbox's connection without asking anything of the runner, as
    /// a sandbox that dies does.
    fn lose(&self, name: &str) {
        if let Some(relay) = self.relays.lock().unwrap().remove(name) {
            relay.abort();
        }
    }

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
        self.created.lock().unwrap().push(spec.mounts.to_vec());
        let path = self
            .dir
            .as_ref()
            .unwrap()
            .path()
            .join(format!("{name}.sock"));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let config = inside::Config::new(
            Dial::Unix(path),
            CertPin::parse(&spec.bootstrap.cert_pin).unwrap(),
            spec.bootstrap.token.clone(),
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
        self.listeners.lock().unwrap().remove(&handle.0);
        self.lose(&handle.0);
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

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path.as_ref())
        .unwrap_or_else(|e| panic!("{}: {e}", path.as_ref().display()))
}

/// Where "inside" is, and the volumes a session's run gets.
struct Layout {
    _tmp: tempfile::TempDir,
    store: FilesStore,
    /// The sandbox's own disk: emptied before each sandbox starts, as a new
    /// container's is.
    inside: PathBuf,
    work: PathBuf,
    state: PathBuf,
    mounts: Vec<Mount>,
}

const WORKSPACE: &str = "tracon-workspace-p3loop";
const SCRATCH: &str = "tracon-scratch-p3loop";

fn layout() -> Layout {
    let tmp = tempfile::tempdir().unwrap();
    let store = FilesStore::for_node().sync_every(Duration::from_millis(300));
    let _ = store.remove(WORKSPACE);
    let _ = store.remove(SCRATCH);

    // A checkout, and what a preparation installed beside it.
    let seed = tmp.path().join("seed");
    std::fs::create_dir_all(seed.join("node_modules/dep")).unwrap();
    std::fs::write(seed.join(".gitignore"), "node_modules/\n").unwrap();
    std::fs::write(seed.join("a.txt"), "one\n").unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    std::fs::write(seed.join("node_modules/dep/index.js"), "installed\n").unwrap();
    store.import(WORKSPACE, &seed).unwrap();

    // The session's configuration, as `materialize` stages it.
    let scratch = tmp.path().join("scratch");
    std::fs::create_dir_all(scratch.join("harness/plugin")).unwrap();
    std::fs::write(scratch.join("harness/plugin/skill.md"), "skill\n").unwrap();
    std::fs::write(scratch.join("orientation.md"), "# orientation\n").unwrap();
    std::fs::write(scratch.join("gitconfig"), "[user]\n\tname = t\n").unwrap();
    store.import(SCRATCH, &scratch).unwrap();

    let inside = tmp.path().join("inside");
    let work = inside.join("work");
    let state = inside.join("home/.agent");
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let mounts = vec![
        Mount::volume(WORKSPACE, s(&work), false),
        Mount::at(SCRATCH, "state", s(&state), false),
        Mount::at(SCRATCH, "harness/plugin", s(&state.join("plugin")), true),
        Mount::at(
            SCRATCH,
            "orientation.md",
            s(&state.join("orientation.md")),
            true,
        ),
        Mount::at(
            SCRATCH,
            "gitconfig",
            s(&inside.join("home/.gitconfig")),
            true,
        ),
    ];
    Layout {
        _tmp: tmp,
        store,
        inside,
        work,
        state,
        mounts,
    }
}

/// A "harness" that runs each line it is sent and says when it is done.
struct Harness {
    stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    lines: Lines<BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>>,
    done: futures_core::future::BoxFuture<'static, Result<i32, tracon::runner::RunnerError>>,
}

impl Layout {
    /// A new sandbox's disk, with nothing a previous one left.
    fn fresh(&self) {
        let _ = std::fs::remove_dir_all(&self.inside);
    }
}

impl Harness {
    async fn start(runner: &ProtocolRunner, name: &str, l: &Layout) -> Harness {
        l.fresh();
        let mounts = &l.mounts;
        let spawned = runner
            .spawn(RunnerCommand {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "while read -r line; do sh -c \"$line\"; echo \"done $?\"; done".into(),
                ],
                name: name.into(),
                mounts: mounts.to_vec(),
                labels: RunLabels::of(RunKind::Session),
                ..Default::default()
            })
            .await
            .unwrap();
        Harness {
            stdin: spawned.stdin,
            lines: BufReader::new(spawned.stdout).lines(),
            done: spawned.done,
        }
    }

    /// Run `line` inside; what it printed, up to its `done`.
    async fn run(&mut self, line: &str) -> String {
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let mut out = String::new();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(20), self.lines.next_line())
                .await
                .expect("the harness answered")
                .unwrap()
                .expect("the harness is running");
            if let Some(code) = next.strip_prefix("done ") {
                assert_eq!(code, "0", "{line}: {out}");
                return out;
            }
            out.push_str(&next);
            out.push('\n');
        }
    }

    async fn exit(mut self) {
        self.stdin.shutdown().await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(30), self.done)
            .await
            .expect("the harness ended");
    }
}

#[tokio::test]
async fn files_over_the_protocol_with_a_loopback_sandbox() {
    support::state::isolate();
    a_sessions_trees_go_in_come_back_and_resume().await;
    a_preparation_comes_back_whole_and_a_check_not_at_all().await;
}

async fn a_sessions_trees_go_in_come_back_and_resume() {
    let l = layout();
    let provider = Loopback::new();
    let runner = || {
        ProtocolRunner::new(provider.clone(), "img")
            .with_files(l.store.clone())
            .with_connect_within(Duration::from_secs(10))
    };
    let tree = || l.store.tree(WORKSPACE).unwrap();
    let state = || l.store.tree(SCRATCH).unwrap().join("state");
    let name = "tracon-h-p3loop";
    let w = l.work.display().to_string();
    let st = l.state.display().to_string();

    // In: the provider mounts none of it; two directories of the sandbox's
    // own carry everything (the home, which the gitconfig is sent into,
    // holds the state), and every tree arrived.
    let mut harness = Harness::start(&runner(), name, &l).await;
    let home = l.state.parent().unwrap().display().to_string();
    assert_eq!(
        provider.created.lock().unwrap()[0],
        vec![Mount::local(&w), Mount::local(&home)]
    );
    assert_eq!(
        harness
            .run(&format!(
                "cat {w}/a.txt {w}/node_modules/dep/index.js {st}/plugin/skill.md \
                 {st}/orientation.md; test -d {w}/.git"
            ))
            .await,
        "one\ninstalled\nskill\n# orientation\n"
    );

    // A review's snapshot while it runs reads the sandbox's tree: the
    // checkout, as `snapshot_workspace` exports it for submit_review,
    // show_work and the UI's diff.
    harness
        .run(&format!("echo two > {w}/a.txt; echo new > {w}/b.txt"))
        .await;
    let mut cfg = tracon::config::Config::default();
    cfg.runtime.podman.sandbox_protocol = true;
    cfg.runtime.podman.sandbox_files = true;
    let backend = tracon::boundary::podman::PodmanBackend::detect(&cfg).await;
    let workspace = tracon::workspace::Workspace {
        id: "p3loop".into(),
        volume: WORKSPACE.into(),
        snapshot: tracon::workspace::snapshot_path("p3loop"),
    };
    let snapshot = tracon::workspace::export(&backend, &workspace)
        .await
        .unwrap();
    assert_eq!(read(snapshot.join("a.txt")), "two\n");
    assert_eq!(read(snapshot.join("b.txt")), "new\n");
    assert!(snapshot.join(".git").is_dir());
    assert!(!snapshot.join("node_modules").exists());
    // ...and it is not a sync: the node's copy is as it was.
    assert_eq!(read(tree().join("a.txt")), "one\n");
    // Nothing imports over a tree a sandbox is working in.
    {
        use tracon::boundary::Backend as _;
        assert!(backend.import_volume(WORKSPACE, &snapshot).await.is_err());
    }

    // During a turn, every period.
    sandbox_files::turn_began(name);
    harness
        .run(&format!(
            "echo three > {w}/a.txt; mkdir -p {st}/projects; echo '{{}}' > {st}/projects/t.jsonl; \
             echo planted > {st}/plugin/planted"
        ))
        .await;
    // The workspace and the harness state are separate trees, each replaced
    // whole when it lands, so one can arrive a moment after the other.
    wait_for(
        || {
            read(tree().join("a.txt")) == "three\n"
                && std::fs::read_to_string(state().join("projects/t.jsonl"))
                    .is_ok_and(|text| text == "{}\n")
        },
        "a sync during the turn",
    )
    .await;
    // Its own state came back; what the node sealed and sends did not.
    assert!(!state().join("plugin").exists());
    assert!(!state().join("orientation.md").exists());
    let scratch = l.store.tree(SCRATCH).unwrap();
    assert!(!scratch.join("harness/plugin/planted").exists());
    // What a preparation installed is still the node's.
    assert_eq!(
        read(tree().join("node_modules/dep/index.js")),
        "installed\n"
    );

    // At its end, at once.
    harness.run(&format!("echo four > {w}/a.txt")).await;
    sandbox_files::turn_ended(name).await;
    assert_eq!(read(tree().join("a.txt")), "four\n");
    // Between turns nothing is taken.
    harness.run(&format!("echo five > {w}/a.txt")).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(read(tree().join("a.txt")), "four\n");

    // At exit.
    harness.run(&format!("echo six > {w}/a.txt")).await;
    harness.exit().await;
    assert_eq!(read(tree().join("a.txt")), "six\n");
    assert!(!sandbox_files::is_live(WORKSPACE));
    // With the sandbox gone, an export reads the node's copy.
    let snapshot = tracon::workspace::export(&backend, &workspace)
        .await
        .unwrap();
    assert_eq!(read(snapshot.join("a.txt")), "six\n");

    // A new sandbox resumes from what came back.
    let mut harness = Harness::start(&runner(), name, &l).await;
    assert_eq!(
        harness
            .run(&format!(
                "cat {w}/a.txt {st}/projects/t.jsonl {w}/node_modules/dep/index.js; \
                 test ! -e {st}/plugin/planted"
            ))
            .await,
        "six\n{}\ninstalled\n"
    );

    // Before a kill.
    harness.run(&format!("echo seven > {w}/a.txt")).await;
    runner().kill(name).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(30), harness.done).await;
    assert_eq!(read(tree().join("a.txt")), "seven\n");

    // A lost sandbox costs what changed since its last sync, no more.
    let mut harness = Harness::start(&runner(), name, &l).await;
    sandbox_files::turn_began(name);
    harness.run(&format!("echo eight > {w}/a.txt")).await;
    sandbox_files::turn_ended(name).await;
    harness.run(&format!("echo nine > {w}/a.txt")).await;
    provider.lose(name);
    let _ = tokio::time::timeout(Duration::from_secs(30), harness.done).await;
    assert_eq!(read(tree().join("a.txt")), "eight\n");

    // A hostile sandbox's tree is refused whole, and the node keeps its copy.
    let mut harness = Harness::start(&runner(), name, &l).await;
    harness
        .run(&format!(
            "echo ten > {w}/a.txt; ln -s /etc {w}/escape; ln -s ../../../../etc {st}/up"
        ))
        .await;
    sandbox_files::turn_ended(name).await;
    assert_eq!(read(tree().join("a.txt")), "eight\n");
    assert!(std::fs::symlink_metadata(tree().join("escape")).is_err());
    assert!(std::fs::symlink_metadata(state().join("up")).is_err());
    assert!(tracon::workspace::export(&backend, &workspace)
        .await
        .is_err());
    harness.exit().await;
    assert_eq!(read(tree().join("a.txt")), "eight\n");

    l.store.remove(WORKSPACE).unwrap();
    l.store.remove(SCRATCH).unwrap();
    provider.settle().await;
}

async fn a_preparation_comes_back_whole_and_a_check_not_at_all() {
    let l = layout();
    let provider = Loopback::new();
    let runner = ProtocolRunner::new(provider.clone(), "img")
        .with_files(l.store.clone())
        .with_connect_within(Duration::from_secs(10));
    let w = l.work.display().to_string();
    let capture = |script: String, kind: RunKind, mounts: Vec<Mount>| RunnerCommand {
        argv: vec!["sh".into(), "-c".into(), script],
        name: "tracon-p-p3loop".into(),
        mounts,
        labels: RunLabels::of(kind),
        ..Default::default()
    };
    let workspace = vec![Mount::volume(WORKSPACE, &w, false)];

    // A preparation's install is the point of it: it comes back whole.
    l.fresh();
    let out = runner
        .run_capture(capture(
            format!("mkdir -p {w}/node_modules/new && echo fresh > {w}/node_modules/new/i.js"),
            RunKind::Prepare,
            workspace.clone(),
        ))
        .await
        .unwrap();
    assert!(out.status.success());
    let tree = l.store.tree(WORKSPACE).unwrap();
    assert_eq!(read(tree.join("node_modules/new/i.js")), "fresh\n");

    // A check's copy is the tree itself, sent in; what the check does to it
    // stays in its sandbox.
    l.store
        .clone_volume(WORKSPACE, "tracon-check-p3loop")
        .await
        .unwrap();
    l.fresh();
    let out = runner
        .run_capture(capture(
            format!("cat {w}/a.txt && echo broken > {w}/a.txt && rm -r {w}/node_modules"),
            RunKind::Check,
            vec![Mount::volume("tracon-check-p3loop", &w, false)],
        ))
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "one\n");
    assert_eq!(read(tree.join("a.txt")), "one\n");
    assert!(tree.join("node_modules/new/i.js").exists());

    // So does a probe's.
    l.fresh();
    let out = runner
        .run_capture(capture(
            format!("echo probed > {w}/a.txt"),
            RunKind::Probe,
            workspace,
        ))
        .await
        .unwrap();
    assert!(out.status.success());
    assert_eq!(read(tree.join("a.txt")), "one\n");

    l.store.remove("tracon-check-p3loop").unwrap();
    l.store.remove(WORKSPACE).unwrap();
    l.store.remove(SCRATCH).unwrap();
    provider.settle().await;
}

async fn wait_for(mut done: impl FnMut() -> bool, what: &str) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never saw {what}");
}
