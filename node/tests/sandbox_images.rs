//! `[runtime.podman] sandbox_images` against real Podman: the node fetches
//! its harness bundle from a release and runs harnesses from it in the stock
//! base image, and a repository is prepared once in a sandbox that is then
//! snapshotted, its sessions forking from the snapshot with the warm cache.
//!
//! Runs only when asked for:
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 \
//!   TRACON_SANDBOX_RELEASE=<dir holding tracon-bundle-<version>-amd64.tar.gz and its .sha256> \
//!   TRACON_SANDBOX_BASE=<the stock base image, in the local store> \
//!   cargo test -p tracon --test sandbox_images -- --test-threads 1
//! ```
//!
//! The release's bundle must be this tree's version, with `tracon-sandbox`
//! built from it (`scripts/harness-bundle.sh --sandbox`). The release is
//! served from a local server; nothing is fetched from the internet.
//! Everything the tests make is named with `TRACON_SANDBOX_TEST_PREFIX`
//! (default `tracon-test-img-`) and removed again.

#[path = "support/mod.rs"]
mod support;

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracon::config::{Config, HarnessListen};
use tracon::provider::podman::PodmanProvider;
use tracon::provider::{Bootstrap, ImageIdentity, Resources, SandboxProvider as _, SnapshotId};
use tracon::runner::podman::RunSpec;
use tracon::runner::{Mount, Runner, RunnerCommand};
use tracon::sandbox_images::{Held, Lockfile, SnapshotInputs};

use support::podman::{podman, requested};

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Rig {
    prefix: String,
    cfg: Config,
    base: String,
    network: String,
    volumes: Vec<String>,
    images: Vec<String>,
    _short: tempfile::TempDir,
}

impl Rig {
    fn new(tag: &str) -> Option<Rig> {
        if !requested() {
            return None;
        }
        support::state::isolate();
        let base = std::env::var("TRACON_SANDBOX_BASE").expect("TRACON_SANDBOX_BASE");
        assert!(podman(&["image", "exists", &base]).is_some(), "{base}");
        let prefix = format!(
            "{}{tag}-",
            std::env::var("TRACON_SANDBOX_TEST_PREFIX")
                .unwrap_or_else(|_| "tracon-test-img-".into())
        );
        let network = format!("{prefix}net");
        let _ = podman(&["network", "rm", "-f", &network]);
        assert!(podman(&["network", "create", "--disable-dns", &network]).is_some());
        let short = tempfile::Builder::new()
            .prefix("sbi")
            .tempdir_in("/tmp")
            .unwrap();
        let mut cfg = Config::default();
        // Each switch this suite runs is set below; the rest stay off.
        cfg.runtime.podman = tracon::config::PodmanRuntime::legacy();
        cfg.boundary.network = network.clone();
        cfg.boundary.gateway_container = format!("{prefix}gw");
        cfg.gateway.harness_listen = HarnessListen::Unix(short.path().join("harness.sock"));
        cfg.runtime.podman.sandbox_protocol = true;
        cfg.runtime.podman.sandbox_images = true;
        cfg.sandbox.base_image = Some(base.clone());
        tracon::config::validate_sandbox(&cfg).unwrap();
        Some(Rig {
            prefix,
            cfg,
            base,
            network,
            volumes: Vec::new(),
            images: Vec::new(),
            _short: short,
        })
    }

    fn name(&self, what: &str) -> String {
        format!("{}{what}", self.prefix)
    }

    fn volume(&mut self, what: &str) -> String {
        let name = self.name(what);
        let _ = podman(&["volume", "rm", "-f", &name]);
        self.volumes.push(name.clone());
        name
    }

    fn command(&self, what: &str, script: &str) -> RunnerCommand {
        RunnerCommand {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            name: self.name(what),
            env: tracon::environment::cache_env(),
            ..Default::default()
        }
    }

    /// The runner a session or check gets, in `image`.
    fn runner(&self, image: &str, mounts: Vec<Mount>) -> Arc<dyn Runner> {
        let mut spec = RunSpec::for_harness(&self.cfg, "claude", true);
        spec.image = image.to_string();
        spec.extra_mounts = mounts;
        tracon::boundary::podman::protocol_runner(&self.cfg, spec)
    }

    fn provider(&self) -> Arc<PodmanProvider> {
        Arc::new(PodmanProvider::from_config(
            &self.cfg,
            RunSpec::from_config(&self.cfg, true),
        ))
    }

    fn preparer(&self) -> tracon::sandbox_images::Preparer {
        tracon::sandbox_images::Preparer {
            provider: self.provider(),
            files: None,
            tunnels: Vec::new(),
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(ids) = podman(&["ps", "-aq", "--filter", &format!("name=^{}", self.prefix)]) {
            for id in ids.lines() {
                let _ = podman(&["rm", "-f", id]);
            }
        }
        for volume in &self.volumes {
            let _ = podman(&["volume", "rm", "-f", volume]);
        }
        for image in &self.images {
            let _ = podman(&["image", "rm", "-f", image]);
        }
        let _ = podman(&["network", "rm", "-f", &self.network]);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A release directory as GitHub lays it out: `v<version>/<asset>`.
fn release_with(dir: &Path, version: &str, tarball: &Path) {
    let into = dir.join(format!("v{version}"));
    std::fs::create_dir_all(&into).unwrap();
    let name = format!("tracon-bundle-{version}-amd64.tar.gz");
    std::fs::copy(tarball, into.join(&name)).unwrap();
    let digest = sha256(&into.join(&name));
    std::fs::write(
        into.join(format!("{name}.sha256")),
        format!("{digest}  {name}\n"),
    )
    .unwrap();
}

fn sha256(path: &Path) -> String {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    hash.update(std::fs::read(path).unwrap());
    hex::encode(hash.finalize())
}

/// The same bundle as release `to`: every entry moved under `to/`, and the
/// manifest saying so, as the next release's bundle would be laid out.
fn repack(from: &Path, version: &str, to: &str, out: &Path) {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(
        std::fs::File::open(from).unwrap(),
    ));
    let gz = flate2::write::GzEncoder::new(
        std::fs::File::create(out).unwrap(),
        flate2::Compression::fast(),
    );
    let mut builder = tar::Builder::new(gz);
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let kind = entry.header().entry_type();
        if kind.is_pax_local_extensions() || kind.is_pax_global_extensions() {
            continue;
        }
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let path = path.replacen(version, to, 1);
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        if path.ends_with("manifest.json") {
            let mut manifest: serde_json::Value = serde_json::from_slice(&data).unwrap();
            manifest["version"] = to.into();
            data = serde_json::to_vec_pretty(&manifest).unwrap();
        }
        let mut header = entry.header().clone();
        header.set_size(data.len() as u64);
        builder.append_data(&mut header, path, &data[..]).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();
}

async fn serve(dir: PathBuf) -> String {
    use axum::extract::Path as AxPath;
    let app = axum::Router::new().route(
        "/{tag}/{file}",
        axum::routing::get(move |AxPath((tag, file)): AxPath<(String, String)>| {
            let dir = dir.clone();
            async move {
                match std::fs::read(dir.join(&tag).join(&file)) {
                    Ok(bytes) => (axum::http::StatusCode::OK, bytes),
                    Err(_) => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn the_node_fetches_its_bundle_and_runs_harnesses_from_it_in_the_base_image() {
    let Some(mut rig) = Rig::new("fetch") else {
        return;
    };
    let release =
        PathBuf::from(std::env::var("TRACON_SANDBOX_RELEASE").expect("TRACON_SANDBOX_RELEASE"));
    let tarball = release.join(format!("tracon-bundle-{VERSION}-amd64.tar.gz"));
    let served = tempfile::tempdir().unwrap();
    release_with(served.path(), VERSION, &tarball);
    let next = format!("{VERSION}-next");
    let repacked = served.path().join("next.tar.gz");
    repack(&tarball, VERSION, &next, &repacked);
    release_with(served.path(), &next, &repacked);
    let url = serve(served.path().to_path_buf()).await;
    rig.cfg.sandbox.release_url = Some(url.clone());

    // The node's own converge: nothing configured but the setting.
    let began = std::time::Instant::now();
    assert!(tracon::sandbox_images::converge(&rig.cfg).await.unwrap());
    let fetched = began.elapsed();
    let bundle = tracon::bundle::effective(&rig.cfg).unwrap();
    assert!(
        bundle.starts_with(Config::state_dir()),
        "{}",
        bundle.display()
    );
    tracon::bundle::verify_dir(&bundle, VERSION, "amd64").unwrap();
    // A restart of the same release uses what it has.
    assert!(!tracon::sandbox_images::converge(&rig.cfg).await.unwrap());
    let pinned = tracon::sandbox_images::base_image(&rig.cfg);
    assert!(pinned.contains("@sha256:"), "{pinned}");
    eprintln!("fetched and checked the bundle in {fetched:?}; base pinned as {pinned}");

    // No harness image: the harness an adapter names is the bundle's, in the
    // base image, as its own user.
    assert_eq!(rig.cfg.podman_harness_image("claude"), pinned);
    assert_eq!(rig.cfg.podman_harness_image("native"), pinned);
    let work = rig.volume("work");
    let runner = rig.runner(&pinned, vec![Mount::volume(work.clone(), "/work", false)]);
    let mut cmd = rig.command(
        "claude",
        "id -u; echo $HOME; readlink -f /proc/1/exe; touch /work/written && echo wrote",
    );
    cmd.name = rig.name("probe");
    let out = runner.run_capture(cmd).await.unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{said}{}", text(&out.stderr));
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines[0], "65532", "{said}");
    assert_eq!(lines[1], "/home/harness", "{said}");
    assert_eq!(lines[2], "/opt/tracon/bin/tracon-sandbox", "{said}");
    assert_eq!(lines[3], "wrote", "{said}");
    for (harness, says) in [
        ("/usr/local/bin/claude", "Claude Code"),
        ("/usr/local/bin/tracon-agent", "tracon-agent"),
    ] {
        let out = runner
            .run_capture(RunnerCommand {
                argv: vec![harness.into(), "--version".into()],
                name: rig.name("version"),
                ..Default::default()
            })
            .await
            .unwrap();
        let said = text(&out.stdout);
        assert!(
            out.status.success() && said.contains(says),
            "{harness}: {said}{}",
            text(&out.stderr)
        );
        eprintln!("{harness} --version in {pinned}: {}", said.trim());
    }

    // The next release: its bundle is fetched as this one's was, and this
    // one's is then superseded.
    let client = reqwest::Client::new();
    let state = Config::state_dir();
    let updated = tracon::bundle::ensure_at(&client, &url, &state, &next, "amd64")
        .await
        .unwrap();
    assert!(matches!(updated, tracon::bundle::Ensured::Fetched(_)));
    assert_eq!(
        tracon::bundle::superseded(&state, &next),
        vec![state.join("bundles").join(VERSION)]
    );

    // A tampered release is refused and leaves nothing mounted.
    let tampered = tempfile::tempdir().unwrap();
    release_with(tampered.path(), "9.9.9", &repacked);
    let sums = tampered
        .path()
        .join("v9.9.9/tracon-bundle-9.9.9-amd64.tar.gz.sha256");
    std::fs::write(
        &sums,
        format!("{}  tracon-bundle-9.9.9-amd64.tar.gz\n", "0".repeat(64)),
    )
    .unwrap();
    let bad = serve(tampered.path().to_path_buf()).await;
    let refused = tracon::bundle::ensure_at(&client, &bad, &state, "9.9.9", "amd64")
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("does not match"), "{refused}");
    assert!(!state.join("bundles/9.9.9").exists());
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

const LOCK: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": { "": { "name": "app", "version": "1.0.0" } }
}
"#;

/// The default branch's tree, imported into a fresh volume as the node's
/// warm-up imports it.
fn import(rig: &mut Rig, what: &str, repo: &Path) -> String {
    let volume = rig.volume(what);
    let xfer = rig.name(&format!("{what}-xfer"));
    let mount = format!("type=volume,src={volume},dst=/data");
    assert!(podman(&[
        "create", "--name", &xfer, "--user", "0:0", "--mount", &mount, &rig.base, "true"
    ])
    .is_some());
    let tree = tempfile::tempdir().unwrap();
    git(
        repo,
        &[
            "archive",
            "-o",
            &tree.path().join("t.tar").to_string_lossy(),
            "HEAD",
        ],
    );
    std::process::Command::new("tar")
        .args(["-xf", "t.tar"])
        .current_dir(tree.path())
        .status()
        .unwrap();
    std::fs::remove_file(tree.path().join("t.tar")).unwrap();
    let copied = podman(&[
        "cp",
        &format!("{}/.", tree.path().display()),
        &format!("{xfer}:/data"),
    ]);
    let _ = podman(&["rm", "-f", &xfer]);
    assert!(copied.is_some(), "copy into {volume}");
    volume
}

const PREPARE: [&str; 2] = [
    "npm ci --ignore-scripts --no-audit --no-fund --offline",
    "test -d /cache/npm/_logs && sha256sum package-lock.json > /cache/prepared-for && touch /cache/npm/step-two-saw-step-one",
];

/// The default branch prepared in one sandbox from the base, and that
/// sandbox snapshotted.
async fn prepare(rig: &mut Rig, repo: &Path, round: &str) -> (String, String, std::time::Duration) {
    let commit = git(repo, &["rev-parse", "HEAD"]);
    let base = tracon::sandbox_images::base_image(&rig.cfg);
    let inputs = SnapshotInputs {
        base: base.clone(),
        image: base.clone(),
        commit: commit.clone(),
        lockfiles: tracon::sandbox_images::lockfiles(repo, &commit).await,
        prepare: PREPARE.iter().map(|s| s.to_string()).collect(),
        egress: Vec::new(),
    };
    assert_eq!(
        inputs
            .lockfiles
            .iter()
            .map(|l| l.path.as_str())
            .collect::<Vec<_>>(),
        ["package-lock.json"]
    );
    let key = inputs.key();
    let work = import(rig, &format!("warm-{round}"), repo);
    let reference = format!("localhost/{}snap:{}", rig.prefix, &key[..12]);
    rig.images.push(reference.clone());
    let began = std::time::Instant::now();
    let held = Held::for_preparation(
        &rig.preparer(),
        &rig.name(&format!("prep-{round}")),
        &base,
        &work,
    )
    .await
    .unwrap();
    let mut prepared = Ok(());
    for (step, command) in PREPARE.iter().enumerate() {
        let out = held
            .run_capture(RunnerCommand {
                argv: vec!["sh".into(), "-c".into(), command.to_string()],
                env: tracon::environment::cache_env(),
                workdir: Some("/work".into()),
                name: rig.name(&format!("prep-{round}-p{step}")),
                ..Default::default()
            })
            .await
            .unwrap();
        if !out.status.success() {
            prepared = Err(format!(
                "{command}: {}{}",
                text(&out.stdout),
                text(&out.stderr)
            ));
            break;
        }
    }
    let snapshot = held
        .conclude(
            prepared,
            &reference,
            &[
                (tracon::sandbox_images::FROM_LABEL.to_string(), base.clone()),
                (tracon::sandbox_images::KEY_LABEL.to_string(), key.clone()),
            ],
        )
        .await
        .unwrap();
    (snapshot, key, began.elapsed())
}

#[tokio::test]
async fn a_repository_prepares_once_and_its_sessions_fork_from_the_snapshot() {
    let Some(mut rig) = Rig::new("snap") else {
        return;
    };
    rig.cfg.sandbox.bundle = Some(PathBuf::from(
        std::env::var("TRACON_SANDBOX_BUNDLE").expect("TRACON_SANDBOX_BUNDLE"),
    ));
    // Pin the base as the node's converge would.
    tracon::sandbox_images::converge(&rig.cfg).await.unwrap();
    let base = tracon::sandbox_images::base_image(&rig.cfg);

    let repo_dir = tempfile::tempdir().unwrap();
    let repo = repo_dir.path().join("app");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("package.json"),
        "{\"name\":\"app\",\"version\":\"1.0.0\"}\n",
    )
    .unwrap();
    std::fs::write(repo.join("package-lock.json"), LOCK).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);

    let (snapshot, key, took) = prepare(&mut rig, &repo, "1").await;
    eprintln!("prepared and snapshotted in {took:?}: {snapshot}");
    assert!(
        podman(&[
            "ps",
            "-aq",
            "--filter",
            &format!("name=^{}prep-1", rig.prefix)
        ])
        .unwrap()
        .is_empty(),
        "the preparation sandbox is gone"
    );
    // Configured as the base was, with no token of the run that made it.
    let config: serde_json::Value = serde_json::from_str(
        &podman(&[
            "image",
            "inspect",
            "--format",
            "{{json .Config}}",
            &snapshot,
        ])
        .unwrap(),
    )
    .unwrap();
    let base_config: serde_json::Value = serde_json::from_str(
        &podman(&["image", "inspect", "--format", "{{json .Config}}", &base]).unwrap(),
    )
    .unwrap();
    assert_eq!(config["User"], base_config["User"]);
    assert_eq!(config["Entrypoint"], base_config["Entrypoint"]);
    assert_eq!(config["WorkingDir"], base_config["WorkingDir"]);
    for entry in config["Env"].as_array().unwrap() {
        let entry = entry.as_str().unwrap();
        if let Some((name, value)) = entry.split_once('=') {
            if name.starts_with("TRACON_SANDBOX_") || name.ends_with("_PROXY") {
                assert_eq!(value, "", "the snapshot keeps {entry}");
            }
        }
    }
    assert_eq!(
        config["Labels"][tracon::sandbox_images::KEY_LABEL],
        key.as_str()
    );

    // The repository's snapshot, as the store would hold it.
    let store = tracon::store::Store::open_in_memory().unwrap();
    let row = store
        .start_repo_image(&tracon::store::RepoImageStart {
            repo_path: &repo.to_string_lossy(),
            kind: tracon::sandbox_images::SNAPSHOT,
            recipe_hash: &key,
            source_ref: "",
            source_commit: "",
            parent_image: &base,
        })
        .unwrap();
    store
        .finish_repo_image(&row, Ok(&snapshot), "", &[])
        .unwrap();
    let backend = tracon::boundary::podman::PodmanBackend::detect(&rig.cfg).await;
    assert_eq!(
        tracon::sandbox_images::current(&backend, &store, &repo, &key)
            .await
            .as_deref(),
        Some(snapshot.as_str())
    );

    // Two sessions fork from it, each with a cache volume of its own seeded
    // from the snapshot's, and each prepares its own workspace in it.
    let base_cache = rig.name("cache-base");
    tracon::sandbox_images::record_seed(&base_cache, &snapshot);
    for session in ["s1", "s2"] {
        let cache = rig.volume(&format!("cache-{session}"));
        let began = std::time::Instant::now();
        tracon::boundary::Backend::clone_volume(&backend, &base_cache, &cache)
            .await
            .unwrap();
        let seeded = began.elapsed();
        let work = import(&mut rig, &format!("work-{session}"), &repo);
        let runner = rig.runner(
            &snapshot,
            vec![
                Mount::volume(work.clone(), "/work", false),
                Mount::volume(cache.clone(), "/cache", false),
            ],
        );
        let began = std::time::Instant::now();
        let out = runner
            .run_capture(rig.command(
                session,
                "test -e /cache/npm/step-two-saw-step-one && echo cache-warm; \
                 ls /cache/npm/_logs | wc -l; cat /cache/prepared-for; \
                 npm ci --ignore-scripts --no-audit --no-fund --offline >/dev/null && echo installed; \
                 claude --version",
            ))
            .await
            .unwrap();
        let said = text(&out.stdout);
        assert!(out.status.success(), "{said}{}", text(&out.stderr));
        assert!(said.starts_with("cache-warm\n"), "{said}");
        assert!(said.contains("package-lock.json"), "{said}");
        assert!(said.contains("installed"), "{said}");
        assert!(said.contains("Claude Code"), "{said}");
        eprintln!(
            "{session}: cache seeded in {seeded:?}, ran in {:?}: {}",
            began.elapsed(),
            said.replace('\n', " | ")
        );
    }

    // A fork reads as derived from the image it was made from.
    let provider = rig.provider();
    let server = tracon::runner::protocol::node_server().unwrap();
    let bootstrap = Bootstrap {
        run: "fork".into(),
        cert_pin: server.pin().to_string(),
        token: server.tokens().issue("fork"),
    };
    let handle = provider
        .fork(
            &SnapshotId(snapshot.clone()),
            &Resources::default(),
            &bootstrap,
        )
        .await
        .unwrap();
    let identity = provider.image_identity(&handle).await;
    provider.destroy(&handle).await.unwrap();
    server.tokens().revoke("fork");
    match identity.unwrap() {
        ImageIdentity::Derived { from, actual } => {
            assert_eq!(from, base);
            let digest =
                podman(&["image", "inspect", "--format", "{{.Digest}}", &snapshot]).unwrap();
            assert_eq!(actual, digest, "{snapshot}");
        }
        exact => panic!("a fork read as {exact:?}"),
    }

    // A lockfile change on the default branch: a new key, so the snapshot no
    // longer applies, and the next preparation makes another.
    std::fs::write(
        repo.join("package-lock.json"),
        LOCK.replace("\"requires\": true", "\"requires\": false"),
    )
    .unwrap();
    git(&repo, &["commit", "-qam", "two"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let moved: Vec<Lockfile> = tracon::sandbox_images::lockfiles(&repo, &commit).await;
    assert_eq!(moved.len(), 1);
    let (again, key2, _) = prepare(&mut rig, &repo, "2").await;
    assert_ne!(key2, key, "a lockfile change is a new key");
    assert_eq!(
        tracon::sandbox_images::current(&backend, &store, &repo, &key2).await,
        None
    );
    assert_ne!(again, snapshot);
    let out = rig
        .runner(&again, vec![])
        .run_capture(rig.command("s3", "cat /cache/prepared-for"))
        .await
        .unwrap();
    let digest = sha256(&repo.join("package-lock.json"));
    assert!(
        text(&out.stdout).starts_with(&digest),
        "{}",
        text(&out.stdout)
    );
}

/// A session on the stock base with `sandbox_files`: its harness state and
/// its gitconfig are sent in as a session's are, the gitconfig into a home
/// the sandbox's user owns whatever the image made of it (a local mount
/// holding what the image had there), and `$HOME` is that home.
#[tokio::test]
async fn a_session_on_the_stock_base_has_its_home_and_its_gitconfig() {
    let Some(mut rig) = Rig::new("home") else {
        return;
    };
    rig.cfg.runtime.podman.sandbox_files = true;
    rig.cfg.sandbox.bundle = Some(PathBuf::from(
        std::env::var("TRACON_SANDBOX_BUNDLE").expect("TRACON_SANDBOX_BUNDLE"),
    ));
    tracon::sandbox_images::converge(&rig.cfg).await.unwrap();
    let base = tracon::sandbox_images::base_image(&rig.cfg);

    let seed = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(seed.path().join("state")).unwrap();
    std::fs::write(
        seed.path().join("gitconfig"),
        "[user]\n\tname = Spike A1\n[safe]\n\tdirectory = /work\n",
    )
    .unwrap();
    let files = tracon::runner::sandbox_files::FilesStore::for_node();
    let scratch = format!("tracon-scratch-{}s", rig.prefix);
    files.import(&scratch, seed.path()).unwrap();
    let home = tracon::sandbox_images::HOME;
    let mut cmd = rig.command(
        "session",
        "id -u; echo \"$HOME\"; git config --global user.name; \
         test -w \"$HOME/.claude\" && echo state-writable; \
         stat -c %u \"$HOME\"; test -f \"$HOME/.profile\" && echo kept-profile",
    );
    cmd.labels = tracon::runner::RunLabels::of(tracon::runner::RunKind::Session);
    cmd.mounts = vec![
        Mount::at(&scratch, "state", format!("{home}/.claude"), false),
        Mount::at(&scratch, "gitconfig", format!("{home}/.gitconfig"), true),
    ];
    let out = rig.runner(&base, Vec::new()).run_capture(cmd).await;
    let _ = files.remove(&scratch);
    let out = out.unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{said}{}", text(&out.stderr));
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(
        lines,
        [
            "65532",
            home,
            "Spike A1",
            "state-writable",
            "65532",
            "kept-profile"
        ],
        "{said}"
    );
}
