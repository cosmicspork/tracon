//! Workspaces and harness state on a Podman sandbox's own disk, over the
//! sandbox protocol's files channel, against real Podman: the trees go in at
//! start onto the sandbox's own storage, a review's export reads the running
//! sandbox, what changed comes back at exit, and a tracon-agent or Claude
//! Code session resumes in a new sandbox after its first one has gone, its
//! model reached through the gateway's tunnel. Also times the files path on
//! a real repository.
//!
//! Runs only when asked for:
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 TRACON_SANDBOX_BUNDLE=<bundle>/<version>/amd64 \
//!   [TRACON_SANDBOX_TEST_IMAGE=<default: this version's tracon-sandbox-base>] \
//!   [TRACON_FILES_BENCH_REPO=/path/to/a/repo] \
//!   cargo test -p tracon --test podman_sandbox_files -- --ignored --test-threads 1 --nocapture
//! ```
//!
//! The bundle needs `bin/tracon-sandbox`, `bin/tracon-agent` and `bin/claude`
//! (`scripts/harness-bundle.sh --sandbox`). Everything the tests make in
//! Podman is named with `TRACON_SANDBOX_TEST_PREFIX` (default
//! `tracon-test-sbf-`) and removed again; the trees the node keeps are in
//! the test's own state directory.

#[path = "support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracon::boundary::Backend as _;
use tracon::config::{Config, HarnessListen};
use tracon::runner::podman::RunSpec;
use tracon::runner::sandbox_files::{self, FilesStore};
use tracon::runner::{Mount, RunKind, RunLabels, Runner, RunnerCommand};

use support::podman::{podman, requested};

struct Rig {
    prefix: String,
    cfg: Config,
    image: String,
    volumes: Vec<String>,
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
        assert!(bundle.join("bin/tracon-sandbox").is_file());
        let image = std::env::var("TRACON_SANDBOX_TEST_IMAGE").unwrap_or_else(|_| {
            format!(
                "ghcr.io/cosmicspork/tracon-sandbox-base:{}",
                env!("CARGO_PKG_VERSION")
            )
        });
        assert!(podman(&["image", "exists", &image]).is_some(), "{image}");
        let prefix = format!(
            "{}{tag}-",
            std::env::var("TRACON_SANDBOX_TEST_PREFIX")
                .unwrap_or_else(|_| "tracon-test-sbf-".into())
        );
        let short = tempfile::Builder::new()
            .prefix("sbf")
            .tempdir_in("/tmp")
            .unwrap();
        let mut cfg = Config::default();
        cfg.boundary.gateway_container = format!("{prefix}gw");
        cfg.sandbox.base_image = Some(image.clone());
        cfg.gateway.harness_listen = HarnessListen::Unix(short.path().join("harness.sock"));
        cfg.sandbox.bundle = Some(bundle);
        Some(Rig {
            prefix,
            cfg,
            image,
            volumes: Vec::new(),
            _short: short,
        })
    }

    fn name(&self, what: &str) -> String {
        format!("{}{what}", self.prefix)
    }

    /// A name the node keeps the tree of when files go over the protocol;
    /// it never becomes a Podman volume.
    fn kept(&self, kind: &str, what: &str) -> String {
        format!("tracon-{kind}-{}{what}", self.prefix)
    }

    fn runner(&self, cfg: &Config, mounts: Vec<Mount>) -> Arc<dyn Runner> {
        let mut spec = RunSpec::from_config(cfg);
        spec.image = self.image.clone();
        spec.extra_mounts = mounts;
        tracon::boundary::podman::protocol_runner(cfg, spec)
    }

    /// Where a harness in a sandbox reaches the node's harness listener:
    /// the gateway's name, on the sandbox's loopback, through its tunnel.
    fn gateway(&self) -> String {
        format!(
            "http://{}:{}",
            self.cfg.boundary.gateway_container, self.cfg.gateway.forward_port
        )
    }

    /// The node's harness listener, which a fake model stands in for.
    fn harness_socket(&self) -> PathBuf {
        match &self.cfg.gateway.harness_listen {
            HarnessListen::Unix(path) => path.clone(),
            HarnessListen::Tcp(_) => unreachable!("the rig listens on a socket"),
        }
    }

    async fn backend(&self, cfg: &Config) -> tracon::boundary::podman::PodmanBackend {
        tracon::boundary::podman::PodmanBackend::detect(cfg).await
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(ids) = podman(&["ps", "-aq", "--filter", &format!("name=^{}", self.prefix)]) {
            for id in ids.lines() {
                let _ = podman(&["rm", "-f", "-v", id]);
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

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
}

/// A small repository, or a clone of `TRACON_FILES_BENCH_REPO`: its tracked
/// files and its repository, as a session's workspace is seeded.
fn seed(dir: &Path) -> PathBuf {
    let seed = dir.join("seed");
    match std::env::var("TRACON_FILES_BENCH_REPO") {
        Ok(repo) => {
            let out = std::process::Command::new("git")
                .args(["clone", "--quiet", "--no-hardlinks", &repo])
                .arg(&seed)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", text(&out.stderr));
        }
        Err(_) => {
            std::fs::create_dir_all(seed.join("src")).unwrap();
            std::fs::write(seed.join("src/a.txt"), "one\n").unwrap();
            git(&seed, &["init", "-q", "-b", "main"]);
            git(&seed, &["add", "."]);
            git(&seed, &["commit", "-q", "-m", "seed"]);
        }
    }
    seed
}

fn sh(name: &str, kind: RunKind, script: &str, mounts: Vec<Mount>) -> RunnerCommand {
    RunnerCommand {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        name: name.into(),
        mounts,
        labels: RunLabels::of(kind),
        ..Default::default()
    }
}

fn workspace(id: &str, volume: &str) -> tracon::workspace::Workspace {
    tracon::workspace::Workspace {
        id: id.into(),
        volume: volume.into(),
        snapshot: tracon::workspace::snapshot_path(id),
    }
}

/// The files path end to end on Podman, timed.
#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn a_workspace_lives_on_the_sandboxs_own_disk() {
    let Some(rig) = Rig::new("disk") else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let seed = seed(tmp.path());
    let files = rig.backend(&rig.cfg).await;
    let tracked = String::from_utf8(
        std::process::Command::new("git")
            .args(["ls-files"])
            .current_dir(&seed)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .lines()
    .count();
    let first = String::from_utf8(
        std::process::Command::new("git")
            .args(["ls-files"])
            .current_dir(&seed)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .lines()
    .next()
    .unwrap()
    .to_string();

    let mut rows = Vec::new();
    for round in 0..3 {
        // The files path.
        let kept = rig.kept("workspace", &format!("ws{round}"));
        let at = Instant::now();
        files.import_volume(&kept, &seed).await.unwrap();
        let files_import = at.elapsed();
        assert!(
            podman(&["volume", "exists", &kept]).is_none(),
            "the node keeps it; Podman has no volume of that name"
        );
        let runner = rig.runner(&rig.cfg, Vec::new());
        let name = rig.name(&format!("session{round}"));
        let at = Instant::now();
        let spawned = runner
            .spawn(sh(
                &name,
                RunKind::Session,
                "test -d /work/.git && echo ready; read _; exit 0",
                vec![Mount::volume(&kept, "/work", false)],
            ))
            .await
            .unwrap();
        let mut stdin = spawned.stdin;
        let mut lines =
            tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(spawned.stdout));
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("ready"));
        let files_start = at.elapsed();
        if round == 0 {
            // /work is storage of the sandbox's own: an anonymous volume, not
            // the node's tree and not a named volume.
            let mounts = podman(&[
                "inspect",
                &name,
                "--format",
                "{{range .Mounts}}{{.Destination}}={{.Name}};{{end}}",
            ])
            .unwrap();
            let work = mounts
                .split(';')
                .find(|m| m.starts_with("/work="))
                .unwrap_or_else(|| panic!("{mounts}"));
            assert!(!work.contains(&kept), "{mounts}");
        }
        drop(lines);
        let at = Instant::now();
        let snapshot = tracon::workspace::export(&files, &workspace(&format!("f{round}"), &kept))
            .await
            .unwrap();
        let files_export_live = at.elapsed();
        assert!(snapshot.join(".git").is_dir());
        assert!(snapshot.join(&first).exists());
        tokio::io::AsyncWriteExt::write_all(&mut stdin, b"go\n")
            .await
            .unwrap();
        assert_eq!(spawned.done.await.unwrap(), 0);
        assert!(podman(&["container", "exists", &name]).is_none());
        assert!(!sandbox_files::is_live(&kept));
        let at = Instant::now();
        tracon::workspace::export(&files, &workspace(&format!("g{round}"), &kept))
            .await
            .unwrap();
        let files_export_kept = at.elapsed();
        rows.push([
            files_import,
            files_start,
            files_export_live,
            files_export_kept,
        ]);
        files.remove_volume(&kept).await.unwrap();
    }
    let median = |i: usize| {
        let mut v: Vec<Duration> = rows.iter().map(|r| r[i]).collect();
        v.sort();
        v[v.len() / 2]
    };
    eprintln!("tracked files: {tracked}");
    for (i, what) in [
        "files: import (into the node's store)",
        "files: start a sandbox and send it in (to first output)",
        "files: export the checkout from the running sandbox",
        "files: export the checkout from the node's copy",
    ]
    .iter()
    .enumerate()
    {
        eprintln!("{what}: {} ms (median of 3)", median(i).as_millis());
    }

    // What a preparation installed (`TRACON_FILES_BENCH_INSTALLED`, e.g.
    // `node_modules,vendor` of the bench repository), which goes in whole at
    // every start and comes back whole after a preparation.
    let Ok(installed) = std::env::var("TRACON_FILES_BENCH_INSTALLED") else {
        return;
    };
    let repo = PathBuf::from(std::env::var("TRACON_FILES_BENCH_REPO").unwrap());
    let kept = rig.kept("workspace", "prepared");
    files.import_volume(&kept, &seed).await.unwrap();
    let tree = FilesStore::for_node().tree(&kept).unwrap();
    for dir in installed.split(',') {
        // As a preparation's last sync would have left it.
        let out = std::process::Command::new("cp")
            .args(["-a", "--reflink=auto"])
            .arg(repo.join(dir))
            .arg(tree.join(dir))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
    let runner = rig.runner(&rig.cfg, Vec::new());
    let at = Instant::now();
    let out = runner
        .run_capture(sh(
            &rig.name("prepared-start"),
            RunKind::Probe,
            "find /work -type f | wc -l",
            vec![Mount::volume(&kept, "/work", false)],
        ))
        .await
        .unwrap();
    let start = at.elapsed();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let count = text(&out.stdout).trim().to_string();
    let at = Instant::now();
    let out = runner
        .run_capture(sh(
            &rig.name("prepared-sync"),
            RunKind::Prepare,
            "true",
            vec![Mount::volume(&kept, "/work", false)],
        ))
        .await
        .unwrap();
    let round_trip = at.elapsed();
    assert!(out.status.success(), "{}", text(&out.stderr));
    eprintln!(
        "prepared tree of {} files: a sandbox started and sent it in: {} ms; \
         sent in and taken back whole (a preparation): {} ms",
        count,
        start.as_millis(),
        round_trip.as_millis()
    );
    files.remove_volume(&kept).await.unwrap();
}

/// A harness writes into its workspace and its own state, the sandbox goes,
/// and both are there for the next one.
#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn what_a_session_changed_is_there_for_the_next_sandbox() {
    let Some(rig) = Rig::new("next") else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let seed = seed(tmp.path());
    let backend = rig.backend(&rig.cfg).await;
    let work = rig.kept("workspace", "w");
    let scratch = rig.kept("scratch", "s");
    backend.import_volume(&work, &seed).await.unwrap();
    let staged = tmp.path().join("scratch");
    std::fs::create_dir_all(staged.join("harness/plugin")).unwrap();
    std::fs::write(staged.join("harness/plugin/skill.md"), "skill\n").unwrap();
    std::fs::write(staged.join("orientation.md"), "# o\n").unwrap();
    backend.import_volume(&scratch, &staged).await.unwrap();
    let mounts = vec![
        Mount::volume(&work, "/work", false),
        Mount::at(&scratch, "state", "/home/harness/.claude", false),
        Mount::at(
            &scratch,
            "harness/plugin",
            "/home/harness/.claude/plugin",
            true,
        ),
        Mount::at(
            &scratch,
            "orientation.md",
            "/home/harness/.claude/orientation.md",
            true,
        ),
    ];
    let runner = rig.runner(&rig.cfg, Vec::new());
    let out = runner
        .spawn(sh(
            &rig.name("first"),
            RunKind::Session,
            "cat /home/harness/.claude/plugin/skill.md /home/harness/.claude/orientation.md >&2; \
             echo edited > /work/new.txt; mkdir -p /home/harness/.claude/projects/-work; \
             echo '{}' > /home/harness/.claude/projects/-work/c.jsonl; touch /home/harness/.claude/plugin/planted",
            mounts.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(out.done.await.unwrap(), 0);
    let out = runner
        .run_capture(sh(
            &rig.name("second"),
            RunKind::Probe,
            "cat /work/new.txt /home/harness/.claude/projects/-work/c.jsonl; \
             test ! -e /home/harness/.claude/plugin/planted && echo sealed",
            mounts,
        ))
        .await
        .unwrap();
    assert_eq!(
        text(&out.stdout),
        "edited\n{}\nsealed\n",
        "{}",
        text(&out.stderr)
    );
    backend.remove_volume(&work).await.unwrap();
    backend.remove_volume(&scratch).await.unwrap();
}

/// A preparation's virtualenv, whose interpreter is an absolute link to the
/// image's, comes back to the node whole and is there for the next sandbox.
/// Needs an image with Python's venv module (`ghcr.io/cosmicspork/
/// tracon-sandbox-base` has it, without `ensurepip`) or uv.
#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn a_prepared_virtualenv_is_there_for_the_next_sandbox() {
    let Some(rig) = Rig::new("venv") else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let seed = seed(tmp.path());
    let backend = rig.backend(&rig.cfg).await;
    let work = rig.kept("prep", "v");
    backend.import_volume(&work, &seed).await.unwrap();
    let mounts = vec![Mount::volume(&work, "/work", false)];
    let runner = rig.runner(&rig.cfg, Vec::new());
    let out = runner
        .run_capture(sh(
            &rig.name("prepare"),
            RunKind::Prepare,
            "cd /work && { python3 -m venv --without-pip .venv || uv venv .venv; }",
            mounts.clone(),
        ))
        .await
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let tree = FilesStore::for_node().tree(&work).unwrap();
    let outward = std::fs::read_dir(tree.join(".venv/bin"))
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.unwrap().path()).ok())
        .any(|target| target.is_absolute());
    assert!(outward, "the interpreter link to the image's came back");
    assert!(tree.join(".venv/pyvenv.cfg").is_file());

    let out = runner
        .run_capture(sh(
            &rig.name("use"),
            RunKind::Probe,
            "/work/.venv/bin/python -c 'import sys; print(sys.prefix)'",
            mounts,
        ))
        .await
        .unwrap();
    assert_eq!(text(&out.stdout), "/work/.venv\n", "{}", text(&out.stderr));
    backend.remove_volume(&work).await.unwrap();
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
        if cmd.argv[0] != "/bin/sh" {
            cmd.argv[0] = format!("/opt/tracon/bin/{}", self.0);
        }
        self.1.run_capture(cmd).await
    }
    async fn kill(&self, name: &str) -> Result<(), tracon::runner::RunnerError> {
        self.1.kill(name).await
    }
}

/// A fake model on `path`, answering each request with the next of
/// `script` (and an empty body once it runs out), served where the node's
/// harness listener would be: a sandbox reaches it through the gateway's
/// tunnel, as a harness reaches the node.
async fn fake_model(
    socket: &Path,
    path: &str,
    script: Vec<String>,
) -> Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
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
    let _ = std::fs::remove_file(socket);
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    requests
}

fn sse(chunks: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn openai_answer(text: &str) -> String {
    use serde_json::json;
    sse(&[
        json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 3}}),
    ])
}

/// tracon-agent journals its session in its state, which comes back at
/// exit; a new sandbox loads it with `session/load`, and the model sees the
/// first conversation again.
#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn a_tracon_agent_session_resumes_after_its_sandbox_ended() {
    use serde_json::json;
    use tracon::adapter::{native::NativeAdapter, HarnessAdapter, LaunchSpec, Resume};

    let Some(rig) = Rig::new("native") else {
        return;
    };
    let requests = fake_model(
        &rig.harness_socket(),
        "/v1/chat/completions",
        vec![
            openai_answer("first answer"),
            openai_answer("second answer"),
        ],
    )
    .await;
    let backend = rig.backend(&rig.cfg).await;
    let tmp = tempfile::tempdir().unwrap();
    let work = rig.kept("workspace", "w");
    let scratch = rig.kept("scratch", "s");
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    backend.import_volume(&work, &empty).await.unwrap();
    // The agent's sealed configuration and its run directory, as the node
    // stages them.
    let staged = tmp.path().join("scratch");
    std::fs::create_dir_all(staged.join("harness/config")).unwrap();
    std::fs::create_dir_all(staged.join("harness/run")).unwrap();
    std::fs::write(
        staged.join("harness/config/providers.json"),
        json!([{
            "name": "fake",
            "shape": "openai",
            "base_url": rig.gateway(),
            "key_env": "TRACON_PROVIDER_KEY_FAKE",
            "models": [{"id": "m", "context": 32000, "parallel_tools": true}],
        }])
        .to_string(),
    )
    .unwrap();
    backend.import_volume(&scratch, &staged).await.unwrap();
    let mut mounts = vec![
        Mount::volume(
            tracon::session::materialize::harness_state_volume(),
            "/home/harness/.tracon-agent",
            false,
        ),
        Mount::at(
            &scratch,
            "harness/run",
            "/home/harness/.tracon-agent/run",
            false,
        ),
        Mount::at(
            &scratch,
            "harness/config",
            "/home/harness/.tracon-agent/config",
            true,
        ),
        Mount::volume(&work, "/work", false),
    ];
    tracon::session::materialize::own_state(&mut mounts, &scratch);
    let runner = FromBundle("tracon-agent", rig.runner(&rig.cfg, mounts));
    let spec = |resume: Option<Resume>| LaunchSpec {
        secret_env: tracon::runner::SecretEnv::new(vec![(
            "TRACON_PROVIDER_KEY_FAKE".into(),
            "not-a-real-key".into(),
        )]),
        cwd_in_runner: "/work".into(),
        model: "fake/m".into(),
        container_name: rig.name("session"),
        harness_home: "/home/harness".into(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        env: vec![
            (
                "NO_PROXY".into(),
                rig.cfg.boundary.gateway_container.clone(),
            ),
            (
                "no_proxy".into(),
                rig.cfg.boundary.gateway_container.clone(),
            ),
        ],
        system_prompt_file: None,
        resume,
        labels: RunLabels::of(RunKind::Session),
    };
    let adapter = NativeAdapter::new(NativeAdapter::PINNED_VERSION);
    let (handle, rx) = adapter.launch(&runner, spec(None)).await.unwrap();
    drop(rx);
    let result = tokio::time::timeout(Duration::from_secs(60), handle.prompt("remember me".into()))
        .await
        .expect("the first turn ended")
        .unwrap();
    assert_eq!(result.stop_reason, "end_turn");
    let resume = handle.resume_point().expect("a journalled session resumes");
    sandbox_files::turn_ended(&rig.name("session")).await;
    drop(handle);
    runner.kill(&rig.name("session")).await.unwrap();
    assert!(podman(&["container", "exists", &rig.name("session")]).is_none());
    let journal = FilesStore::for_node()
        .tree(&scratch)
        .unwrap()
        .join("harness/run/sessions")
        .join(format!("{}.jsonl", resume.harness_session_id));
    assert!(journal.is_file(), "{}", journal.display());

    // A new sandbox, the same session.
    let (handle, rx) = adapter
        .launch(&runner, spec(Some(resume.clone())))
        .await
        .unwrap();
    drop(rx);
    assert_eq!(handle.harness_session_id(), resume.harness_session_id);
    let result = tokio::time::timeout(Duration::from_secs(60), handle.prompt("again".into()))
        .await
        .expect("the resumed turn ended")
        .unwrap();
    assert_eq!(result.stop_reason, "end_turn");
    let second = requests.lock().unwrap()[1].to_string();
    assert!(second.contains("remember me"), "{second}");
    assert!(second.contains("first answer"), "{second}");
    drop(handle);
    runner.kill(&rig.name("session")).await.unwrap();
    backend.remove_volume(&work).await.unwrap();
    backend.remove_volume(&scratch).await.unwrap();
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

/// Claude Code keeps the conversation in its state directory, which is the
/// session's own and comes back at the turn's end; after the sandbox has
/// gone, the conversation is found and `--resume` carries it on.
#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn a_claude_code_session_resumes_after_its_sandbox_ended() {
    use tracon::adapter::{claude::ClaudeAdapter, HarnessAdapter, LaunchSpec, Resume};

    let Some(rig) = Rig::new("claude") else {
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
    let requests = fake_model(
        &rig.harness_socket(),
        "/v1/messages",
        (0..16)
            .map(|i| anthropic_answer(&format!("answer {i}")))
            .collect(),
    )
    .await;
    let backend = rig.backend(&rig.cfg).await;
    let tmp = tempfile::tempdir().unwrap();
    let work = rig.kept("workspace", "w");
    let scratch = rig.kept("scratch", "s");
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    backend.import_volume(&work, &empty).await.unwrap();
    let staged = tmp.path().join("scratch");
    std::fs::create_dir_all(staged.join("harness/plugin/.claude-plugin")).unwrap();
    std::fs::write(
        staged.join("harness/plugin/.claude-plugin/plugin.json"),
        "{\"name\":\"tracon\",\"version\":\"1.0.0\"}",
    )
    .unwrap();
    backend.import_volume(&scratch, &staged).await.unwrap();
    let mut mounts = vec![
        Mount::volume(
            tracon::session::materialize::harness_state_volume(),
            "/home/harness/.claude",
            false,
        ),
        Mount::at(
            &scratch,
            "harness/plugin",
            "/home/harness/.claude/plugin",
            true,
        ),
        Mount::volume(&work, "/work", false),
    ];
    tracon::session::materialize::own_state(&mut mounts, &scratch);
    let runner = FromBundle("claude", rig.runner(&rig.cfg, mounts));
    let base = rig.gateway();
    let spec = |resume: Option<Resume>| LaunchSpec {
        secret_env: tracon::runner::SecretEnv::new(vec![(
            "ANTHROPIC_API_KEY".into(),
            "not-a-real-key".into(),
        )]),
        cwd_in_runner: "/work".into(),
        model: "anthropic/claude-test".into(),
        container_name: rig.name("session"),
        harness_home: "/home/harness".into(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        env: vec![
            ("ANTHROPIC_BASE_URL".into(), base.clone()),
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
                "1".into(),
            ),
            (
                "NO_PROXY".into(),
                rig.cfg.boundary.gateway_container.clone(),
            ),
            (
                "no_proxy".into(),
                rig.cfg.boundary.gateway_container.clone(),
            ),
        ],
        system_prompt_file: None,
        resume,
        labels: RunLabels::of(RunKind::Session),
    };
    let adapter = ClaudeAdapter::new(ClaudeAdapter::PINNED_VERSION);
    let (handle, mut rx) = adapter.launch(&runner, spec(None)).await.unwrap();
    let events = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        handle.prompt("remember the word kumquat".into()),
    )
    .await
    .expect("the first turn ended")
    .unwrap();
    assert_eq!(result.stop_reason, "end_turn");
    let id = handle.harness_session_id().to_string();
    // As the supervisor does at a turn's end.
    sandbox_files::turn_ended(&rig.name("session")).await;
    drop(handle);
    runner.kill(&rig.name("session")).await.unwrap();
    events.abort();
    assert!(podman(&["container", "exists", &rig.name("session")]).is_none());
    let transcript = FilesStore::for_node()
        .tree(&scratch)
        .unwrap()
        .join("state/projects/-work")
        .join(format!("{id}.jsonl"));
    assert!(transcript.is_file(), "{}", transcript.display());

    // After the sandbox ended: the conversation is found, in a sandbox of
    // its own, and carried on.
    let resume = Resume {
        harness_session_id: id.clone(),
        last_seq: 0,
    };
    let reopened = spec(Some(resume.clone()));
    assert!(adapter
        .has_conversation(&runner, &reopened, &id)
        .await
        .unwrap());
    let (handle, mut rx) = adapter.launch(&runner, reopened).await.unwrap();
    let events = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    assert_eq!(handle.harness_session_id(), id);
    let before = requests.lock().unwrap().len();
    let result = tokio::time::timeout(Duration::from_secs(120), handle.prompt("and now?".into()))
        .await
        .expect("the resumed turn ended")
        .unwrap();
    assert_eq!(result.stop_reason, "end_turn");
    let resumed = requests.lock().unwrap()[before..]
        .iter()
        .map(|r| r.to_string())
        .find(|r| r.contains("and now?"))
        .expect("the resumed turn reached the model");
    assert!(
        resumed.contains("kumquat"),
        "the resumed turn lost the conversation"
    );
    drop(handle);
    runner.kill(&rig.name("session")).await.unwrap();
    events.abort();
    backend.remove_volume(&work).await.unwrap();
    backend.remove_volume(&scratch).await.unwrap();
}
