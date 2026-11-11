//! A session's `prepare` against its repository's prepared snapshot, on real
//! Podman with `sandbox_images` and `sandbox_files`: at the snapshot's commit
//! it is skipped and the session finds the installed tree under its own; at
//! another commit it runs again in one sandbox from the snapshot. Each start
//! is timed against the path every session took before (a sandbox per step,
//! and the whole prepared tree sent into the session).
//!
//! Runs only when asked for, in a process of its own, since it names every
//! runtime resource with its own prefix (`spike-b3-`):
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 \
//!   TRACON_SANDBOX_BASE=<the stock base image, in the local store> \
//!   TRACON_SANDBOX_BUNDLE=<an unpacked bundle: bin/tracon-sandbox from this tree> \
//!   cargo test -p tracon --test prepare_from_snapshot -- --nocapture
//! ```
//!
//! Everything it makes is removed again; the base image is only read.

#[path = "support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracon::config::{Config, HarnessListen};
use tracon::environment::{PrepareOutcome, RepoEnvironment};
use tracon::runner::{Mount, RunKind, RunLabels, RunnerCommand};
use tracon::session::FromSnapshot;

use support::podman::{podman, requested};

const PREFIX: &str = "spike-b3";

const LOCK: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": { "": { "name": "app", "version": "1.0.0" } }
}
"#;

/// A small install: npm's own, then a stand-in dependency tree (ten copies
/// of npm's package, about 23k files), marked with the commit's README so a
/// session can tell which preparation made it.
const PREPARE: [&str; 2] = [
    "npm ci --ignore-scripts --no-audit --no-fund --offline",
    "mkdir -p node_modules && for i in 0 1 2 3 4 5 6 7 8 9; do \
     cp -r /usr/local/lib/node_modules/npm node_modules/npm-copy-$i; done && \
     cp README.md node_modules/.prepared-from && touch /cache/prepared",
];

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
    text(&out.stdout).trim().to_string()
}

/// Removes what the test made, however it ends.
struct Cleanup {
    network: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let name = format!("name=^{PREFIX}-");
        if let Some(ids) = podman(&["ps", "-aq", "--filter", &name]) {
            for id in ids.lines() {
                let _ = podman(&["rm", "-f", id]);
            }
        }
        if let Some(volumes) = podman(&["volume", "ls", "-q", "--filter", &name]) {
            for volume in volumes.lines() {
                let _ = podman(&["volume", "rm", "-f", volume]);
            }
        }
        let snapshots = format!("reference=localhost/{PREFIX}-snapshot*");
        if let Some(images) = podman(&["images", "-q", "--filter", &snapshots]) {
            for image in images.lines() {
                let _ = podman(&["image", "rm", "-f", image]);
            }
        }
        let _ = podman(&["network", "rm", "-f", &self.network]);
    }
}

/// What one session's start cost, and what it found.
struct Started {
    outcome: PrepareOutcome,
    prepare: Duration,
    first_command: Duration,
    said: String,
}

#[allow(clippy::too_many_arguments)]
async fn start(
    backend: &tracon::boundary::podman::PodmanBackend,
    cfg: &Config,
    environment: &RepoEnvironment,
    repo: &Path,
    label: &str,
    commit: &str,
    from: Option<FromSnapshot<'_>>,
) -> Started {
    let checkout = tempfile::tempdir().unwrap();
    let dir = checkout.path().join("w");
    git(
        repo,
        &[
            "clone",
            "-q",
            &repo.to_string_lossy(),
            &dir.to_string_lossy(),
        ],
    );
    git(&dir, &["checkout", "-q", commit]);
    let files = tracon::runner::sandbox_files::FilesStore::for_node();
    let workspace = tracon::config::resource_name(&format!("workspace-{label}"));
    files.import(&workspace, &dir).unwrap();
    let cache = tracon::config::resource_name(&format!("cache-r-{label}"));

    let began = Instant::now();
    let done = tracon::session::prepare_session_workspace(
        backend,
        environment,
        (&workspace, Ok(&cache)),
        &tracon::config::resource_name(&format!("s-{label}")),
        &tracon::review::checks::RunToCompletion,
        from,
    )
    .await
    .unwrap();
    let prepare = began.elapsed();

    let mut spec = tracon::runner::podman::RunSpec::for_harness(cfg, "claude", true);
    spec.image = environment.image.clone();
    let runner = tracon::boundary::podman::protocol_runner(cfg, spec);
    let began = Instant::now();
    let out = runner
        .run_capture(RunnerCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "cat node_modules/.prepared-from; \
                 test -f node_modules/npm-copy-9/package.json && echo deps; \
                 test -f /cache/prepared && echo cache-warm; \
                 ls -A | grep -v '^node_modules$' | tr '\\n' ' '; echo; \
                 git -c safe.directory='*' status --porcelain | tr '\\n' ' '; echo status-done"
                    .into(),
            ],
            name: tracon::config::resource_name(&format!("h-{label}")),
            env: tracon::environment::cache_env(),
            workdir: Some("/work".into()),
            mounts: vec![
                Mount::volume(workspace.clone(), "/work", false),
                Mount::volume(cache.clone(), "/cache", false),
            ],
            labels: RunLabels::of(RunKind::Session),
            ..Default::default()
        })
        .await
        .unwrap();
    let first_command = began.elapsed();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{said}{}", text(&out.stderr));
    let _ = files.remove(&workspace);
    Started {
        outcome: done.outcome,
        prepare,
        first_command,
        said,
    }
}

#[tokio::test]
async fn a_session_at_its_snapshots_commit_skips_prepare_and_another_prepares_on_it() {
    if !requested() {
        return;
    }
    support::state::isolate();
    let config_dir = PathBuf::from(std::env::var("TRACON_CONFIG_DIR").unwrap());
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("node.toml"),
        format!("[boundary]\nresource_prefix = \"{PREFIX}\"\n"),
    )
    .unwrap();
    let mut cfg = Config::load().unwrap();
    assert_eq!(tracon::config::resource_prefix(), PREFIX);

    let base = std::env::var("TRACON_SANDBOX_BASE").expect("TRACON_SANDBOX_BASE");
    assert!(podman(&["image", "exists", &base]).is_some(), "{base}");
    let network = format!("{PREFIX}-net");
    let _cleanup = Cleanup {
        network: network.clone(),
    };
    let _ = podman(&["network", "rm", "-f", &network]);
    assert!(podman(&["network", "create", "--disable-dns", &network]).is_some());
    let short = tempfile::Builder::new()
        .prefix("b3")
        .tempdir_in("/tmp")
        .unwrap();
    cfg.boundary.network = network.clone();
    cfg.boundary.gateway_container = format!("{PREFIX}-gw");
    cfg.gateway.harness_listen = HarnessListen::Unix(short.path().join("harness.sock"));
    cfg.runtime.podman.sandbox_protocol = true;
    cfg.runtime.podman.sandbox_images = true;
    cfg.runtime.podman.sandbox_files = true;
    cfg.sandbox.base_image = Some(base.clone());
    cfg.sandbox.bundle = Some(PathBuf::from(
        std::env::var("TRACON_SANDBOX_BUNDLE").expect("TRACON_SANDBOX_BUNDLE"),
    ));
    tracon::config::validate_sandbox(&cfg).unwrap();
    tracon::sandbox_images::converge(&cfg).await.unwrap();
    let pinned = tracon::sandbox_images::base_image(&cfg);
    let backend = tracon::boundary::podman::PodmanBackend::detect(&cfg).await;
    assert!(tracon::boundary::Backend::workspace_over_image(&backend));

    let repo_dir = tempfile::tempdir().unwrap();
    let repo = repo_dir.path().join("app");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("package.json"),
        "{\"name\":\"app\",\"version\":\"1.0.0\"}\n",
    )
    .unwrap();
    std::fs::write(repo.join("package-lock.json"), LOCK).unwrap();
    std::fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();
    std::fs::write(repo.join("README.md"), "one\n").unwrap();
    std::fs::write(repo.join("old.txt"), "the default branch had me\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let c1 = git(&repo, &["rev-parse", "HEAD"]);

    let environment = RepoEnvironment {
        image: pinned.clone(),
        image_source: "stock base",
        checks: Vec::new(),
        timeout_secs: 300,
        prepare: PREPARE.iter().map(|s| s.to_string()).collect(),
        egress: Vec::new(),
        session_egress: Vec::new(),
        cache_volume: tracon::config::resource_name("cache-base-app"),
        repo: None,
    };
    let store = tracon::store::Store::open_in_memory().unwrap();
    let began = Instant::now();
    let snapshot = tracon::sandbox_images::warm(
        &backend,
        &cfg,
        &store,
        &repo,
        &environment,
        &Default::default(),
    )
    .await
    .unwrap()
    .expect("a snapshot");
    eprintln!("warm-up and snapshot: {:?} ({snapshot})", began.elapsed());
    assert!(tracon::sandbox_images::is_snapshot(&snapshot), "{snapshot}");

    // The snapshot records its commit, on the image and in the store, and its
    // `/work` holds what `prepare` installed and none of the tracked files.
    let label = format!(
        "{{{{index .Labels \"{}\"}}}}",
        tracon::sandbox_images::COMMIT_LABEL
    );
    assert_eq!(
        podman(&["image", "inspect", "--format", &label, &snapshot]).unwrap(),
        c1
    );
    assert_eq!(
        tracon::sandbox_images::prepared_from(&store, &repo, &snapshot).as_deref(),
        Some(c1.as_str())
    );
    let work = podman(&[
        "run",
        "--rm",
        "--name",
        &format!("{PREFIX}-look"),
        "--network",
        "none",
        "--entrypoint",
        "sh",
        &snapshot,
        "-c",
        "ls -A /work; ls /work/node_modules/npm-copy-9 | wc -l",
    ])
    .unwrap();
    let mut lines = work.lines();
    assert_eq!(lines.next(), Some("node_modules"), "{work}");
    assert_ne!(lines.next(), Some("0"), "{work}");

    let mut from_snapshot = environment.clone();
    from_snapshot.image = snapshot.clone();
    from_snapshot.image_source = tracon::sandbox_images::SNAPSHOT_SOURCE;

    // Before: every step in a sandbox of its own, and the prepared tree
    // taken back and sent whole into the session.
    let before = start(&backend, &cfg, &from_snapshot, &repo, "before", &c1, None).await;
    assert_eq!(before.outcome, PrepareOutcome::Prepared);
    assert!(before.said.contains("deps"), "{}", before.said);

    // At the snapshot's commit: nothing runs, and the session finds the
    // install under its own checkout, with nothing Git would report.
    let same = start(
        &backend,
        &cfg,
        &from_snapshot,
        &repo,
        "same",
        &c1,
        Some(FromSnapshot {
            snapshot: &snapshot,
            prepared_at: Some(&c1),
            base: Some(&c1),
        }),
    )
    .await;
    assert_eq!(
        same.outcome,
        PrepareOutcome::FromSnapshot { commit: c1.clone() }
    );
    let said: Vec<&str> = same.said.lines().collect();
    assert_eq!(said[0], "one", "{}", same.said);
    assert!(
        said.contains(&"deps") && said.contains(&"cache-warm"),
        "{}",
        same.said
    );
    assert!(same.said.contains("old.txt"), "{}", same.said);
    assert!(
        same.said.contains("\nstatus-done"),
        "a clean tree: {}",
        same.said
    );

    // Another commit, which deletes a file the default branch tracks: the
    // preparation runs again on the snapshot, and the deleted file does not
    // come back from it.
    std::fs::write(repo.join("README.md"), "two\n").unwrap();
    git(&repo, &["rm", "-q", "old.txt"]);
    std::fs::write(repo.join("new.txt"), "added\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "two"]);
    let c2 = git(&repo, &["rev-parse", "HEAD"]);
    let other = start(
        &backend,
        &cfg,
        &from_snapshot,
        &repo,
        "other",
        &c2,
        Some(FromSnapshot {
            snapshot: &snapshot,
            prepared_at: Some(&c1),
            base: Some(&c2),
        }),
    )
    .await;
    assert_eq!(other.outcome, PrepareOutcome::Prepared);
    let said: Vec<&str> = other.said.lines().collect();
    assert_eq!(said[0], "two", "prepared again: {}", other.said);
    assert!(
        said.contains(&"deps") && said.contains(&"cache-warm"),
        "{}",
        other.said
    );
    assert!(!other.said.contains("old.txt"), "{}", other.said);
    assert!(other.said.contains("new.txt"), "{}", other.said);
    assert!(
        other.said.contains("\nstatus-done"),
        "a clean tree: {}",
        other.said
    );

    for (what, run) in [
        ("before (a sandbox per step)", &before),
        ("same commit (skipped)", &same),
        ("another commit (one sandbox)", &other),
    ] {
        eprintln!(
            "{what}: prepare {:?}, first command in the session {:?}, start {:?}",
            run.prepare,
            run.first_command,
            run.prepare + run.first_command
        );
    }
}
