//! `[[repo]] build` against real Podman: a repository's image is built from
//! root steps on the stock base, the steps reaching only the entry's egress
//! through the node's egress proxy; its `prepare` then runs as the base's
//! user and is snapshotted, and a session sandbox from the snapshot has what
//! both left.
//!
//! Runs only when asked for, and fetches from Ubuntu's apt sources:
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 \
//!   TRACON_SANDBOX_BASE=<the stock base image, in the local store> \
//!   TRACON_SANDBOX_BUNDLE=<a harness bundle's <version>/<arch> directory> \
//!   cargo test -p tracon --test build_steps -- --test-threads 1
//! ```
//!
//! Everything it makes is named from `TRACON_BUILD_STEPS_TEST_PREFIX`
//! (default `tracon-test-steps-`), which is also the process's resource
//! prefix, or carries the test repository's name, and is removed again.

#[path = "support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tracon::boundary::{ImageBuild, ImageBuilder as _};
use tracon::config::{Config, HarnessListen, Repo};
use tracon::runner::podman::RunSpec;
use tracon::runner::{Runner, RunnerCommand};

use support::podman::{podman, requested};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn git(repo: &Path, args: &[&str]) {
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
}

/// Names the process's runtime resources from the test prefix: the first
/// configuration a process loads as its own is what does that.
fn claim_resource_prefix(prefix: &str) {
    let path = Config::config_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    if !path.exists() {
        std::fs::write(
            &path,
            format!("[boundary]\nresource_prefix = \"{prefix}\"\n"),
        )
        .unwrap();
    }
    Config::load().unwrap();
    assert_eq!(tracon::config::resource_prefix(), prefix);
}

/// Removes, when dropped, everything named for the test.
struct Cleanup {
    prefix: String,
    repo_image: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(ids) = podman(&["ps", "-aq", "--filter", &format!("name=^{}", self.prefix)]) {
            for id in ids.lines() {
                let _ = podman(&["rm", "-f", id]);
            }
        }
        if let Some(volumes) = podman(&["volume", "ls", "-q"]) {
            for volume in volumes.lines().filter(|v| v.starts_with(&self.prefix)) {
                let _ = podman(&["volume", "rm", "-f", volume]);
            }
        }
        if let Some(images) = podman(&["images", "--format", "{{.Repository}}:{{.Tag}}"]) {
            let snapshots = format!("localhost/{}", self.prefix);
            for image in images
                .lines()
                .filter(|i| i.starts_with(&self.repo_image) || i.starts_with(&snapshots))
            {
                let _ = podman(&["image", "rm", "-f", image]);
            }
        }
    }
}

#[tokio::test]
async fn a_repository_built_from_steps_prepares_snapshots_and_starts() {
    if !requested() {
        return;
    }
    support::state::isolate();
    let base = std::env::var("TRACON_SANDBOX_BASE").expect("TRACON_SANDBOX_BASE");
    assert!(podman(&["image", "exists", &base]).is_some(), "{base}");
    let bundle =
        PathBuf::from(std::env::var("TRACON_SANDBOX_BUNDLE").expect("TRACON_SANDBOX_BUNDLE"));
    let prefix = std::env::var("TRACON_BUILD_STEPS_TEST_PREFIX")
        .unwrap_or_else(|_| "tracon-test-steps-".into());
    claim_resource_prefix(prefix.trim_end_matches('-'));

    let repo_dir = tempfile::tempdir().unwrap();
    let repo = repo_dir.path().join(format!("{prefix}app"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("data.json"), "{\"answer\": 42}\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);

    let _cleanup = Cleanup {
        prefix: prefix.clone(),
        repo_image: format!("localhost/{prefix}repo-{prefix}app-"),
    };
    let short = tempfile::Builder::new()
        .prefix("bst")
        .tempdir_in("/tmp")
        .unwrap();

    let mut cfg = Config::default();
    cfg.boundary.resource_prefix = tracon::config::resource_prefix().to_string();
    cfg.boundary.gateway_container = format!("{prefix}gw");
    cfg.gateway.harness_listen = HarnessListen::Unix(short.path().join("harness.sock"));
    cfg.sandbox.base_image = Some(base.clone());
    cfg.sandbox.bundle = Some(bundle);
    // `jq` is in the base too; `tree` is not, so its presence is the build's.
    cfg.repo = vec![Repo {
        path: repo.clone(),
        build: vec![
            "apt-get update".into(),
            "apt-get install -y --no-install-recommends jq tree".into(),
            "rm -rf /var/lib/apt/lists/*".into(),
        ],
        egress: vec!["apt".into()],
        prepare: vec![
            "jq -r .answer data.json > /cache/answer && id -u > /cache/prepared-by && tree --version > /cache/tree".into(),
        ],
        ..Default::default()
    }];
    tracon::config::validate_sandbox(&cfg).unwrap();
    tracon::config::validate_repos(&cfg.repo).unwrap();
    tracon::sandbox_images::converge(&cfg).await.unwrap();
    let pinned = tracon::sandbox_images::base_image(&cfg);
    assert!(pinned.contains("@sha256:"), "{pinned}");

    let backend = tracon::boundary::podman::PodmanBackend::detect(&cfg).await;
    let store = tracon::store::Store::open_in_memory().unwrap();

    // The build, then the preparation and its snapshot, as a session's
    // start makes them.
    let began = Instant::now();
    let ensured = tracon::repo_image::ensure_base(&backend, &cfg, &store, &repo, false).await;
    let built_in = began.elapsed();
    let image = match &ensured {
        tracon::repo_image::Ensured::Rebuilt(image) => image.clone(),
        other => {
            let row = store
                .latest_repo_image(&repo.to_string_lossy(), tracon::repo_image::BASE)
                .unwrap();
            panic!("not built: {other:?}\n{row:?}");
        }
    };
    let environment = tracon::environment::environment_for(&cfg, &store, Some(&repo));
    assert_eq!(environment.image, image);
    assert_eq!(
        environment.image_source,
        tracon::environment::REPOSITORY_BUILD
    );
    let snapshot =
        tracon::sandbox_images::snapshot_for(&backend, &cfg, &store, &repo, &environment, false)
            .await
            .expect("the build's warm-up made a snapshot");
    eprintln!("built, prepared and snapshotted in {built_in:?}: {image} -> {snapshot}");
    for kind in [tracon::repo_image::BASE, tracon::sandbox_images::SNAPSHOT] {
        let row = store
            .latest_repo_image(&repo.to_string_lossy(), kind)
            .unwrap()
            .unwrap();
        let took = row.finished_ms.unwrap_or(row.started_ms) - row.started_ms;
        eprintln!("{kind}: {} in {took} ms", row.status);
    }

    // One image on the base: no source image under it, nothing of the
    // grant left in it.
    let name = image.split('@').next().unwrap();
    let held = podman(&["images", "--format", "{{.Repository}}"]).unwrap();
    let ours: Vec<&str> = held.lines().filter(|r| r.starts_with(name)).collect();
    assert_eq!(ours, [name], "one image for the repository");
    let config: serde_json::Value = serde_json::from_str(
        &podman(&["image", "inspect", "--format", "{{json .Config}}", &image]).unwrap(),
    )
    .unwrap();
    assert_eq!(config["User"], "65532:65532");
    assert_eq!(config["WorkingDir"], "/work");
    let env = config["Env"].to_string();
    assert!(env.contains("HOME=/home/harness"), "{env}");
    assert!(!env.to_ascii_lowercase().contains("_proxy"), "{env}");
    let history = podman(&[
        "history",
        "--no-trunc",
        "--format",
        "{{.CreatedBy}}",
        &image,
    ])
    .unwrap();
    assert!(
        !history.contains("build:"),
        "the grant's credentials are in its history:\n{history}"
    );

    // A second asking builds nothing.
    let began = Instant::now();
    let again = tracon::repo_image::ensure_base(&backend, &cfg, &store, &repo, false).await;
    assert_eq!(again, tracon::repo_image::Ensured::Current(image.clone()));
    eprintln!("current again in {:?}", began.elapsed());

    // A session sandbox from the snapshot, as the base's user: the build's
    // tools and the preparation's cache are both there.
    let mut spec = RunSpec::for_harness(&cfg, "claude");
    spec.image = snapshot.clone();
    let runner: Arc<dyn Runner> = tracon::boundary::podman::protocol_runner(&cfg, spec);
    let began = Instant::now();
    let out = runner
        .run_capture(RunnerCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "id -u; jq --version; tree --version; cat /cache/answer /cache/prepared-by /cache/tree; claude --version".into(),
            ],
            name: format!("{prefix}session"),
            ..Default::default()
        })
        .await
        .unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{said}{}", text(&out.stderr));
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines[0], "65532", "{said}");
    assert!(lines[1].starts_with("jq-"), "{said}");
    assert!(lines[2].starts_with("tree v"), "{said}");
    assert_eq!(lines[3], "42", "{said}");
    assert_eq!(
        lines[4], "65532",
        "the preparation ran as the base's user: {said}"
    );
    assert!(lines[5].starts_with("tree v"), "{said}");
    assert!(said.contains("Claude Code"), "{said}");
    eprintln!(
        "session ran in {:?}: {}",
        began.elapsed(),
        said.replace('\n', " | ")
    );

    // A step that asks for a host the entry does not name is refused, and
    // nothing reaches the network but through the proxy.
    let stage = tempfile::tempdir().unwrap();
    let containerfile = stage.path().join("Containerfile");
    std::fs::write(
        &containerfile,
        format!(
            "FROM {pinned}\nUSER 0:0\n\
             RUN if curl -fsS --max-time 10 https://example.com >/dev/null; then echo reached-$((1+1)); fi\n\
             RUN if curl -fsS --noproxy '*' --max-time 10 https://1.1.1.1 >/dev/null; then echo direct-$((1+1)); fi\n\
             RUN curl -sS --max-time 10 -o /dev/null -w 'mirror %{{http_code}}\\n' https://archive.ubuntu.com/ubuntu/\n"
        ),
    )
    .unwrap();
    let empty = stage.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let tag = format!("localhost/{prefix}refused:1");
    let built = backend
        .build_with_egress(
            ImageBuild {
                tag: &tag,
                containerfile: &containerfile,
                context: &empty,
                labels: &[],
                timeout: std::time::Duration::from_secs(300),
            },
            &["archive.ubuntu.com".to_string()],
        )
        .await
        .unwrap();
    let log = built.log_tail;
    assert!(!log.contains("reached-2"), "{log}");
    assert!(!log.contains("direct-2"), "{log}");
    assert!(log.contains("returned error: 403"), "{log}");
    assert!(log.contains("mirror 200"), "{log}");
    let _ = podman(&["image", "rm", "-f", &tag]);
}
