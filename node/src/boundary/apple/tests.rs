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
            ["image", "delete", _] => Ok(String::new()),
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
