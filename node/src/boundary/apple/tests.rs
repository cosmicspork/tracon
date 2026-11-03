use std::sync::Mutex;

use super::*;

/// `container` from a script: images it holds by reference, a build that
/// fails or tags, and every call recorded.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    images: Mutex<Vec<String>>,
    fail_build: bool,
}

const DIGEST: &str = "sha256:36a445445dd5983c51e01199a464e5f92aca2e70a4904e0e21cbdfc7a7342568";

#[async_trait]
impl ContainerCli for Fake {
    async fn run(&self, args: &[String]) -> Result<String, CliError> {
        self.calls.lock().unwrap().push(args.to_vec());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            ["system", "status"] => Ok("apiserver is running".into()),
            ["build", .., "-t", tag, _context] => match self.fail_build {
                true => Err(CliError::Failed(
                    "#5 ERROR: process did not complete".into(),
                )),
                false => {
                    self.images.lock().unwrap().push(tag.to_string());
                    Ok("#6 DONE 0.2s\n".into())
                }
            },
            ["image", "inspect", image] => {
                match self.images.lock().unwrap().contains(&image.to_string()) {
                    true => Ok(serde_json::json!([{
                        "configuration": {
                            "descriptor": {"digest": DIGEST, "size": 374},
                            "name": format!("docker.io/library/{image}")
                        }
                    }])
                    .to_string()),
                    false => Err(CliError::Failed(format!("Error: image not found: {image}"))),
                }
            }
            ["image", "tag", _, to] => {
                self.images.lock().unwrap().push(to.to_string());
                Ok(String::new())
            }
            ["image", "delete", image] => {
                self.images.lock().unwrap().retain(|held| held != image);
                Ok(String::new())
            }
            ["image", "list", "--format", "json"] => Ok(serde_json::Value::Array(
                self.images
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|name| {
                        serde_json::json!({"configuration": {
                            "name": name, "descriptor": {"digest": DIGEST}
                        }})
                    })
                    .collect(),
            )
            .to_string()),
            other => panic!("unexpected container {other:?}"),
        }
    }
}

fn backend(fake: Fake) -> (AppleBackend, Arc<Fake>) {
    let mut cfg = Config::default();
    cfg.runtime.kind = crate::config::RuntimeKind::Apple;
    cfg.gateway.qa_proxy_port = 7423;
    let fake = Arc::new(fake);
    (AppleBackend::with_cli(&cfg, fake.clone()), fake)
}

#[tokio::test]
async fn a_build_is_tagged_and_reported_by_digest() {
    let (b, fake) = backend(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let containerfile = dir.path().join("Containerfile");
    let labels = vec![("io.tracon.recipe".to_string(), "abc".to_string())];
    let built = b
        .build(ImageBuild {
            tag: "localhost:5000/tracon-repo-x:abc",
            containerfile: &containerfile,
            context: dir.path(),
            labels: &labels,
            timeout: std::time::Duration::from_secs(60),
        })
        .await
        .unwrap();
    assert_eq!(
        built.image,
        format!("localhost:5000/tracon-repo-x@{DIGEST}")
    );
    assert!(built.log_tail.contains("DONE"));
    let calls = fake.calls.lock().unwrap().clone();
    let containerfile = containerfile.to_string_lossy().into_owned();
    let context = dir.path().to_string_lossy().into_owned();
    assert_eq!(
        calls[0],
        [
            "build",
            "--progress",
            "plain",
            "--label",
            "io.tracon.recipe=abc",
            "-f",
            containerfile.as_str(),
            "-t",
            "localhost:5000/tracon-repo-x:abc",
            context.as_str(),
        ]
    );
    // The digest reference is a tag of its own, which is what makes the
    // image runnable by it.
    let pinned = format!("localhost:5000/tracon-repo-x@{DIGEST}");
    assert!(calls.contains(&vec![
        "image".to_string(),
        "tag".into(),
        "localhost:5000/tracon-repo-x:abc".into(),
        pinned.clone(),
    ]));
    assert!(b.exists(&pinned).await);
    assert!(b.exists("localhost:5000/tracon-repo-x:abc").await);
    assert!(!b.exists("tracon-repo-y:abc").await);
    // `container` runs only on macOS; anywhere else nothing can say.
    let expected = match cfg!(target_os = "macos") {
        true => Presence::Absent,
        false => Presence::Unknown,
    };
    assert_eq!(b.presence("tracon-repo-y:abc").await, expected);
}

#[tokio::test]
async fn a_failed_build_keeps_its_log_and_blames_the_recipe() {
    let (b, _) = backend(Fake {
        fail_build: true,
        ..Fake::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let failure = b
        .build(ImageBuild {
            tag: "tracon-repo-x:abc",
            containerfile: &dir.path().join("Containerfile"),
            context: dir.path(),
            labels: &[],
            timeout: std::time::Duration::from_secs(60),
        })
        .await
        .unwrap_err();
    assert!(failure.log_tail.contains("did not complete"), "{failure:?}");
    assert!(!failure.transient);
}

#[test]
fn a_reference_loses_its_tag_but_not_its_port() {
    assert_eq!(image_name("ubuntu:24.04"), "ubuntu");
    assert_eq!(image_name("localhost:5000/x"), "localhost:5000/x");
    assert_eq!(image_name("ghcr.io/a/b:1@sha256:00"), "ghcr.io/a/b:1");
}

/// The harness reaches the node at the sandbox's loopback: its proxies, its
/// state directory and the grant's proxy all say so.
#[test]
fn every_route_to_the_node_is_the_sandboxs_loopback() {
    let (b, _) = backend(Fake::default());
    assert_eq!(b.harness_host(), "127.0.0.1");
    assert_eq!(b.harness_home(), "/home/harness");
    let env = b.sandbox_env(crate::adapter::claude::ClaudeAdapter::ID);
    let get = |name: &str| {
        env.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(
        get("HTTPS_PROXY"),
        format!("http://127.0.0.1:{}", b.cfg.gateway.proxy_port)
    );
    assert!(get("NO_PROXY").contains("127.0.0.1"));
    let layout = crate::adapter::layout(crate::adapter::claude::ClaudeAdapter::ID);
    if !layout.env.is_empty() {
        assert_eq!(get(layout.env), format!("/home/harness/{}", layout.dir));
    }
    let grant = b
        .egress_grant(crate::gateway::proxy::GrantSpec {
            client: "s-1".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(grant.server, "http://127.0.0.1:7423");
    let names: Vec<_> = crate::runner::tunnels::gateway_tunnels(&b.cfg)
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["gateway", "proxy", "egress"]);
}

/// Every volume is a tree the node keeps: imported, read back, copied and
/// removed without any runtime.
#[tokio::test]
async fn volumes_are_trees_in_the_files_store() {
    let (b, fake) = backend(Fake::default());
    let volume =
        crate::config::resource_name(&format!("ws-apple-{}", uuid::Uuid::now_v7().simple()));
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("a.txt"), "one").unwrap();
    b.import_volume(&volume, source.path()).await.unwrap();
    assert!(b.volume_exists(&volume).await);
    assert!(b
        .list_volumes()
        .await
        .unwrap()
        .iter()
        .any(|v| v.name == volume));

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("copy");
    b.export_volume(&volume, &dest).await.unwrap();
    assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "one");

    let copy = format!("{volume}-copy");
    b.clone_volume(&volume, &copy).await.unwrap();
    assert!(b.volume_exists(&copy).await);
    for v in [&volume, &copy] {
        b.remove_volume(v).await.unwrap();
        assert!(!b.volume_exists(v).await);
    }
    assert!(fake.calls.lock().unwrap().is_empty(), "no runtime involved");
}

/// `tracon gc` finds this node's snapshots by name, not a commit still being
/// configured or another node's; removing one removes its tag too.
#[tokio::test]
async fn snapshots_are_listed_by_this_nodes_name_and_removed_whole() {
    let ours = crate::sandbox_images::snapshot_repo();
    let tagged = format!("{ours}-app-1:k1");
    let pinned = format!("{ours}-app-1@{DIGEST}");
    let fake = Fake::default();
    *fake.images.lock().unwrap() = vec![
        tagged.clone(),
        pinned.clone(),
        format!("{ours}-app-1-commit:k1"),
        "localhost/other-snapshot-app-1:k1".into(),
        "ubuntu:24.04".into(),
    ];
    let (b, fake) = backend(fake);
    assert_eq!(b.prepared_snapshots().await, std::slice::from_ref(&pinned));
    b.remove(&pinned).await;
    let left = fake.images.lock().unwrap().clone();
    assert!(
        !left.contains(&tagged) && !left.contains(&pinned),
        "{left:?}"
    );
    assert_eq!(left.len(), 3, "{left:?}");
}

/// Against a real `container`, the arm64 bundle at `TRACON_E2E_APPLE_BUNDLE`
/// and the base at `TRACON_E2E_APPLE_BASE`, with `TRACON_STATE_DIR` set to a
/// scratch directory: a repository's `prepare` runs once, its sandbox is
/// snapshotted, and a sandbox from the snapshot finds what it left.
#[tokio::test]
#[ignore = "needs Apple's container, a bundle and a base image"]
async fn a_repository_is_prepared_once_and_its_sessions_start_from_the_snapshot() {
    let (Ok(bundle), Ok(base)) = (
        std::env::var("TRACON_E2E_APPLE_BUNDLE"),
        std::env::var("TRACON_E2E_APPLE_BASE"),
    ) else {
        eprintln!("skipped: no bundle or base named");
        return;
    };
    let mut cfg = Config::default();
    cfg.runtime.kind = crate::config::RuntimeKind::Apple;
    cfg.sandbox.bundle = Some(bundle.into());
    cfg.sandbox.base_image = Some(base);
    let b = AppleBackend::new(&cfg);
    b.converge_images().await.unwrap();

    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=e2e",
                "-c",
                "user.email=e2e@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(repo.path().join("tracked"), "from the checkout").unwrap();
    git(&["add", "tracked"]);
    git(&["commit", "-q", "-m", "one"]);

    let store = crate::store::Store::open_in_memory().unwrap();
    let environment = crate::environment::RepoEnvironment {
        image: crate::sandbox_images::base_image(&cfg),
        image_source: "base",
        checks: Vec::new(),
        timeout_secs: 600,
        prepare: vec![
            "mkdir -p /cache/e2e && echo warm > /cache/e2e/made && cat tracked > \"$HOME/prepared\""
                .into(),
        ],
        egress: Vec::new(),
        session_egress: Vec::new(),
        cache_volume: crate::config::resource_name("cache-e2e"),
        repo: Some("e2e".into()),
    };
    let secrets = crate::environment::PrepareSecrets::default();
    let warm =
        || crate::sandbox_images::warm(&b, &cfg, &store, repo.path(), &environment, &secrets);
    let snapshot = warm().await.unwrap().unwrap();
    assert!(snapshot.contains("@sha256:"), "{snapshot}");
    assert_eq!(warm().await.unwrap().unwrap(), snapshot, "made once");

    let cmd = crate::runner::RunnerCommand {
        argv: vec![
            "sh".into(),
            "-c".into(),
            "cat /cache/e2e/made \"$HOME/prepared\"; echo; id -u".into(),
        ],
        image: Some(snapshot.clone()),
        name: crate::config::resource_name("e2e-from-snapshot"),
        labels: crate::runner::RunLabels::of(crate::runner::RunKind::Probe),
        ..Default::default()
    };
    let out = crate::boundary::Backend::runner(&b, Vec::new())
        .run_capture(cmd)
        .await;
    let snapshots = b.prepared_snapshots().await;
    let inspected = b
        .container(&["image", "inspect", &snapshot])
        .await
        .unwrap_or_default();
    b.remove(&snapshot).await;
    // The preparation's own token is not kept in the image.
    assert!(
        inspected.contains("\"TRACON_SANDBOX_TOKEN=\""),
        "{inspected}"
    );
    assert!(
        inspected.contains(crate::sandbox_images::KEY_LABEL),
        "{inspected}"
    );
    let out = out.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.starts_with("warm\nfrom the checkout\n65532\n"),
        "{stdout}"
    );
    assert!(snapshots.contains(&snapshot), "{snapshots:?}");
    assert!(!b.exists(&snapshot).await);
    // Only this repository's: the store may hold a running node's own.
    let ours = image_name(&snapshot).to_string();
    let left: Vec<_> = b
        .references()
        .await
        .into_iter()
        .filter(|(name, _)| image_name(name) == ours || name.starts_with(&format!("{ours}-commit")))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// Against a real `container`, as above, and Ubuntu's apt sources: a
/// repository's `build` steps run as root on the base and reach only their
/// egress; its `prepare` then runs as the base's user and is snapshotted, and
/// a sandbox from the snapshot has what both left.
#[tokio::test]
#[ignore = "needs Apple's container, a bundle, a base image and the network"]
async fn build_steps_reach_only_their_egress_and_end_as_the_bases_user() {
    let (Ok(bundle), Ok(base)) = (
        std::env::var("TRACON_E2E_APPLE_BUNDLE"),
        std::env::var("TRACON_E2E_APPLE_BASE"),
    ) else {
        eprintln!("skipped: no bundle or base named");
        return;
    };
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = repo_dir.path().join("e2e-steps");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=e2e",
                "-c",
                "user.email=e2e@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("data.json"), "{\"answer\": 42}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "one"]);

    let mut cfg = Config::default();
    cfg.runtime.kind = crate::config::RuntimeKind::Apple;
    cfg.sandbox.bundle = Some(bundle.into());
    cfg.sandbox.base_image = Some(base);
    // `jq` is in the base; `tree` is not, so its presence is the build's.
    cfg.repo = vec![crate::config::Repo {
        path: repo.clone(),
        build: vec![
            "apt-get update".into(),
            "apt-get install -y --no-install-recommends tree".into(),
            "rm -rf /var/lib/apt/lists/*".into(),
            "if curl -fsS --max-time 10 https://example.com >/dev/null; then echo reached-$((1+1)); fi".into(),
            "if curl -fsS --noproxy '*' --max-time 10 https://1.1.1.1 >/dev/null; then echo direct-$((1+1)); fi".into(),
            "curl -sS --max-time 10 -o /dev/null -w 'mirror %{http_code}\\n' https://archive.ubuntu.com/ubuntu/".into(),
            "id -u > /built-by".into(),
        ],
        egress: vec!["apt".into()],
        prepare: vec![
            "jq -r .answer data.json > /cache/answer && id -u > /cache/prepared-by".into(),
        ],
        ..Default::default()
    }];
    crate::config::validate_repos(&cfg.repo).unwrap();
    crate::config::validate_repo_builds(&cfg).unwrap();
    let b = AppleBackend::new(&cfg);
    b.converge_images().await.unwrap();
    let store = crate::store::Store::open_in_memory().unwrap();

    let began = std::time::Instant::now();
    let ensured = crate::repo_image::ensure_base(&b, &cfg, &store, &repo, false).await;
    let built_in = began.elapsed();
    let row = store
        .latest_repo_image(&repo.to_string_lossy(), crate::repo_image::BASE)
        .unwrap();
    let image = match &ensured {
        crate::repo_image::Ensured::Rebuilt(image) => image.clone(),
        other => panic!("not built: {other:?}\n{row:?}"),
    };
    let log = row.map(|row| row.log_tail).unwrap_or_default();
    let environment = crate::environment::environment_for(&cfg, &store, Some(&repo));
    let snapshot =
        crate::sandbox_images::snapshot_for(&b, &cfg, &store, &repo, &environment, false).await;
    eprintln!("built in {built_in:?}: {image} -> {snapshot:?}\n{log}");
    let inspected = b
        .container(&["image", "inspect", &image])
        .await
        .unwrap_or_default();

    let run = |image: String| {
        let cmd = crate::runner::RunnerCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "id -u; cat /built-by; tree --version; cat /cache/answer /cache/prepared-by; claude --version".into(),
            ],
            image: Some(image),
            name: crate::config::resource_name("e2e-steps-session"),
            labels: crate::runner::RunLabels::of(crate::runner::RunKind::Probe),
            ..Default::default()
        };
        let runner = crate::boundary::Backend::runner(&b, Vec::new());
        async move { runner.run_capture(cmd).await }
    };
    let out = match &snapshot {
        Some(snapshot) => Some(run(snapshot.clone()).await),
        None => None,
    };
    if let Some(snapshot) = &snapshot {
        b.remove(snapshot).await;
    }
    b.remove(&image).await;

    assert!(!log.contains("reached-2"), "{log}");
    assert!(!log.contains("direct-2"), "{log}");
    assert!(log.contains("returned error: 403"), "{log}");
    assert!(log.contains("mirror 200"), "{log}");
    let parsed: serde_json::Value = serde_json::from_str(&inspected).unwrap();
    let config = parsed[0]["variants"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|v| v["platform"]["architecture"] == "arm64")
        .map(|v| v["config"]["config"].clone())
        .unwrap_or_default();
    assert_eq!(config["User"], "65532:65532", "{config}");
    assert_eq!(config["WorkingDir"], "/work", "{config}");
    assert!(config["Labels"]["io.tracon.recipe"].is_string(), "{config}");
    let env = config["Env"].to_string();
    assert!(env.contains("HOME=/home/harness"), "{env}");
    assert!(!env.to_ascii_lowercase().contains("_proxy"), "{env}");
    assert!(
        !inspected.contains("build:"),
        "the grant is in the image: {inspected}"
    );
    let out = out.expect("a snapshot").unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines[0], "65532", "{said}");
    assert_eq!(lines[1], "0", "the steps ran as root: {said}");
    assert!(lines[2].starts_with("tree v"), "{said}");
    assert_eq!(lines[3], "42", "{said}");
    assert_eq!(lines[4], "65532", "{said}");
    assert!(said.contains("Claude Code"), "{said}");
}

/// A session from a snapshot finds its cache in the snapshot, and one with
/// no base cache held has nothing to copy: neither looks for a cache volume.
#[tokio::test]
async fn a_session_cache_is_not_copied_from_a_base_cache_the_snapshot_holds() {
    let (b, fake) = backend(Fake::default());
    let base = crate::environment::base_cache_volume(
        Some(std::path::Path::new("/src/no-such-app")),
        "localhost/tracon-repo-x@sha256:0",
    );
    assert!(!b.seeds_session_cache(&base, true).await);
    assert!(!b.seeds_session_cache(&base, false).await);
    assert!(fake.calls.lock().unwrap().is_empty(), "no runtime involved");
}
