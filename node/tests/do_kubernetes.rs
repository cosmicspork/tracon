//! DigitalOcean from a Kubernetes node over the sandbox protocol, minus
//! DigitalOcean and the cluster: a stand-in for the API (`support::do_api`)
//! and a builder standing in for the node's BuildKit.
//!
//! The node builds each template's source on its builder and registers the
//! template from the digest the builder pushed; its sandboxes there reach the
//! node's gateways through the tunnels its own sandboxes get; a prepared
//! snapshot the store has ready keeps its template from the sweep, though
//! the runtime lists none (its snapshots are in a registry).

#![cfg(target_os = "linux")]

#[path = "support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracon::boundary::{BuildFailure, BuiltImage, ImageBuild, ImageBuilder};
use tracon::config::{Config, KubeBuilder, RuntimeKind};
use tracon::provider::digitalocean::listener::DoListener;
use tracon::provider::digitalocean::runtime::{self, DoRuntime};
use tracon::provider::digitalocean::templates::{Source, TemplateState, Templates};
use tracon::provider::digitalocean::{ClientSource, HubRelay, Owner, Settings};

use support::do_api::FakeDo;

const IMAGE: &str =
    "ghcr.io/example/tracon-do-sandbox@sha256:1111111111111111222222222222222222222222222222222222222222222222";
const SNAPSHOT: &str = "registry.digitalocean.com/acme/tracon-snapshot-app-0123@sha256:3333333333333333444444444444444444444444444444444444444444444444";
const REGISTRY: &str = "registry.digitalocean.com/acme";
const PUSHED: &str = "sha256:5555555555555555666666666666666666666666666666666666666666666666";
/// The stock base as the boundary pins it, which the sweep waits for.
const BASE: &str = "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:014c6d9e014c6d9eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

/// What the boundary's converge does once it has pinned the base: every
/// test here that pins it pins the same one.
fn pin_base() {
    tracon::sandbox_images::pin("ghcr.io/cosmicspork/tracon-sandbox-base:latest", BASE);
}
/// A repository's image as a Kubernetes node keeps it, in the registry; one
/// it has since replaced; and a prepared snapshot whose key has moved on.
const REPO: &str = "registry.digitalocean.com/acme/tracon-repo-tracon-72b24c1098@sha256:e9a1e9a1e9a1e9a1777777777777777777777777777777777777777777777777";
const REPO_OLD: &str = "registry.digitalocean.com/acme/tracon-repo-tracon-0000000000@sha256:8888888888888888999999999999999999999999999999999999999999999999";
const OLD_SNAPSHOT: &str = "registry.digitalocean.com/acme/tracon-snapshot-tracon-0123@sha256:aaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbccccccccccccccccdddddddddddddddd";

/// A BuildKit stand-in: what it was asked to build, and a push under the
/// tag's name at `PUSHED`.
#[derive(Default)]
struct FakeBuildKit {
    scratch: PathBuf,
    built: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl ImageBuilder for FakeBuildKit {
    async fn build(&self, build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
        let containerfile = std::fs::read_to_string(build.containerfile).unwrap();
        assert!(build.context.join("bundle/bin/tracon-sandbox").is_file());
        self.built
            .lock()
            .unwrap()
            .push((build.tag.to_string(), containerfile));
        Ok(BuiltImage {
            image: format!("{}@{PUSHED}", build.tag.rsplit_once(':').unwrap().0),
            log_tail: String::new(),
        })
    }
    async fn exists(&self, _image: &str) -> bool {
        false
    }
    async fn identity(&self, _image: &str) -> Option<String> {
        None
    }
    async fn remove(&self, _image: &str) {}
    fn scratch_dir(&self) -> Option<PathBuf> {
        Some(self.scratch.clone())
    }
    fn registry(&self) -> Option<String> {
        Some(REGISTRY.into())
    }
}

fn bundle(dir: &Path) -> PathBuf {
    let bundle = dir.join("bundle");
    std::fs::create_dir_all(bundle.join("bin")).unwrap();
    std::fs::write(bundle.join("bin/tracon-sandbox"), "x").unwrap();
    bundle
}

fn built(dir: &Path) -> Source {
    Source::Built {
        registry: REGISTRY.into(),
        prefix: "tracon".into(),
        bundle: bundle(dir),
        services: Vec::new(),
    }
}

fn kubernetes() -> Config {
    let mut cfg = Config::default();
    cfg.runtime.kind = RuntimeKind::Kubernetes;
    cfg.runtime.kubernetes.builder = Some(KubeBuilder {
        registry: REGISTRY.into(),
        ..Default::default()
    });
    cfg
}

async fn runtime(fake: &FakeDo, source: Source) -> Arc<DoRuntime> {
    let server = tracon::runner::protocol::node_server().unwrap();
    let listener = DoListener::bind("127.0.0.1:0".parse().unwrap(), server)
        .await
        .unwrap();
    let node = listener.local_addr().to_string();
    let settings = Settings {
        credential: "do".into(),
        api_url: fake.url.clone(),
        image: match &source {
            Source::Given(image) => image.clone(),
            _ => String::new(),
        },
        registry: match &source {
            Source::Built { registry, .. } => registry.clone(),
            _ => String::new(),
        },
        listen: node.clone(),
        node_addr: node,
        ..Settings::default()
    };
    let client: ClientSource = {
        let fake = fake.clone();
        Arc::new(move || Ok(fake.client()))
    };
    DoRuntime::with(
        client,
        settings,
        source,
        Owner::new("tracon", "abcd1234"),
        "tracon-gw".into(),
        listener,
        HubRelay::none(),
    )
    .unwrap()
}

/// The source is built and pushed by the node's builder, on its scratch,
/// with the base as the Containerfile's default; the template is registered
/// from the digest the builder pushed, once.
#[tokio::test]
async fn a_kubernetes_node_builds_its_templates_on_its_builder() {
    support::state::isolate();
    let fake = FakeDo::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let builder = Arc::new(FakeBuildKit {
        scratch: tmp.path().join("state/tmp"),
        ..Default::default()
    });
    let templates = Templates::new(built(tmp.path()), "coding-base", "tracon-tabcd1234")
        .unwrap()
        .with_builder(builder.clone());

    let client = fake.client();
    let state = templates.ensure(&client, "unused", SNAPSHOT).await.unwrap();
    assert!(matches!(state, TemplateState::Building { .. }), "{state:?}");
    let name = templates.name_for(SNAPSHOT).unwrap();
    let registered: Vec<serde_json::Value> = fake
        .api
        .lock()
        .unwrap()
        .templates
        .values()
        .cloned()
        .collect();
    assert_eq!(registered.len(), 1, "{registered:?}");
    assert_eq!(registered[0]["name"], name.as_str());
    assert_eq!(registered[0]["spec"]["image"]["digest"], PUSHED);
    let built = builder.built.lock().unwrap().clone();
    assert_eq!(built.len(), 1);
    assert_eq!(
        built[0].0,
        format!("{REGISTRY}/tracon-do-sandbox:3333333333333333")
    );
    assert!(built[0].1.contains(&format!("ARG BASE={SNAPSHOT}\n")));

    // Found from then on, never built again.
    let state = templates.ensure(&client, "unused", SNAPSHOT).await.unwrap();
    assert!(matches!(state, TemplateState::Ready { .. }), "{state:?}");
    assert_eq!(builder.built.lock().unwrap().len(), 1);
}

/// A Kubernetes node over the sandbox protocol can take sessions there once
/// the template is ready; off the protocol it cannot. Its sandboxes reach
/// the node's own listeners, as its pods' sandboxes do.
#[tokio::test]
async fn a_kubernetes_node_over_the_protocol_runs_sessions_there() {
    support::state::isolate();
    let fake = FakeDo::start().await;
    fake.api.lock().unwrap().template_builds = 1;
    let runtime = runtime(&fake, Source::Given(IMAGE.into())).await;
    while !matches!(
        runtime.ensure_template(IMAGE).await,
        TemplateState::Ready { .. }
    ) {}

    let cfg = kubernetes();
    assert_eq!(runtime.health(&cfg), Ok(()));
    let tunnels = runtime::do_tunnels(&cfg);
    assert_eq!(tunnels, tracon::runner::tunnels::kube_tunnels(&cfg));
    let proxy = tunnels.iter().find(|t| t.name == "proxy").unwrap();
    assert_eq!(
        proxy.endpoint,
        tracon_sandbox::node::Endpoint::Tcp(format!("127.0.0.1:{}", cfg.gateway.proxy_port))
    );
    let podman = Config::default();
    assert_eq!(
        runtime::do_tunnels(&podman),
        tracon::runner::tunnels::gateway_tunnels(&podman)
    );
}

/// A Kubernetes node keeps its snapshots in a registry, which its runtime
/// does not list: the store's ready snapshot keeps its template, and only a
/// template nothing wants is swept.
#[tokio::test]
async fn a_ready_snapshots_template_is_kept_from_the_sweep() {
    support::state::isolate();
    let fake = FakeDo::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let runtime = runtime(&fake, built(tmp.path())).await;
    let mut cfg = kubernetes();
    cfg.runtime.kubernetes.state_mount = tmp.path().join("state");
    let backend: Arc<dyn tracon::boundary::Backend> =
        Arc::new(tracon::boundary::kubernetes::KubeBackend::new(&cfg));
    let store = Arc::new(tracon::store::Store::open_in_memory().unwrap());
    let row = store
        .start_repo_image(&tracon::store::RepoImageStart {
            repo_path: "/src/app",
            kind: tracon::sandbox_images::SNAPSHOT,
            recipe_hash: "k1",
            source_ref: "main",
            source_commit: "abc",
            parent_image: IMAGE,
        })
        .unwrap();
    store
        .finish_repo_image(&row, Ok(SNAPSHOT), "", &[])
        .unwrap();
    runtime.set_node(backend, store);
    pin_base();

    let kept = runtime.provider().templates().name_for(SNAPSHOT).unwrap();
    let stale = format!(
        "{}-ffffffffffffffff",
        Owner::new("tracon", "abcd1234").template_stem()
    );
    {
        let mut api = fake.api.lock().unwrap();
        api.put_template(&kept, "TEMPLATE_STATUS_READY");
        api.put_template(&stale, "TEMPLATE_STATUS_READY");
    }
    let swept = runtime.sweep().await.unwrap();
    assert_eq!(swept.templates, vec![stale.clone()]);
    let left: Vec<String> = fake
        .api
        .lock()
        .unwrap()
        .templates
        .values()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(left, vec![kept]);
}

fn ready_row(store: &tracon::store::Store, repo: &str, kind: &str, key: &str, image: &str) {
    let row = store
        .start_repo_image(&tracon::store::RepoImageStart {
            repo_path: repo,
            kind,
            recipe_hash: key,
            source_ref: "main",
            source_commit: "abc",
            parent_image: IMAGE,
        })
        .unwrap();
    store.finish_repo_image(&row, Ok(image), "", &[]).unwrap();
}

fn template_names(fake: &FakeDo) -> Vec<String> {
    let mut names: Vec<String> = fake
        .api
        .lock()
        .unwrap()
        .templates
        .values()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

/// A repository with prepare steps whose ready snapshot is not the current
/// one (its key moved with a new repository image) has its sessions start
/// from its image: that image's template is kept from the sweep, as is the
/// snapshot's. The template of the image it replaced goes, and the node
/// forgets it, so its image has no template ready rather than one that
/// every create would be refused for.
#[tokio::test]
async fn a_repositorys_image_keeps_its_template_beside_a_stale_snapshot() {
    support::state::isolate();
    let fake = FakeDo::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let runtime = runtime(&fake, built(tmp.path())).await;
    let mut cfg = kubernetes();
    cfg.runtime.kubernetes.state_mount = tmp.path().join("state");
    let backend: Arc<dyn tracon::boundary::Backend> =
        Arc::new(tracon::boundary::kubernetes::KubeBackend::new(&cfg));
    let store = Arc::new(tracon::store::Store::open_in_memory().unwrap());
    ready_row(&store, "/src/tracon", tracon::repo_image::BASE, "r1", REPO);
    ready_row(
        &store,
        "/src/tracon",
        tracon::sandbox_images::SNAPSHOT,
        "k0",
        OLD_SNAPSHOT,
    );
    runtime.set_node(backend, store);
    pin_base();

    let templates = runtime.provider().templates();
    let repo = templates.name_for(REPO).unwrap();
    let snapshot = templates.name_for(OLD_SNAPSHOT).unwrap();
    let replaced = templates.name_for(REPO_OLD).unwrap();
    let base = templates.name_for(BASE).unwrap();
    {
        let mut api = fake.api.lock().unwrap();
        for name in [&repo, &snapshot, &replaced, &base] {
            api.put_template(name, "TEMPLATE_STATUS_READY");
        }
    }
    for image in [REPO, REPO_OLD] {
        assert!(matches!(
            runtime.ensure_template(image).await,
            TemplateState::Ready { .. }
        ));
    }
    assert_eq!(templates.ready(REPO_OLD).unwrap(), replaced);

    let swept = runtime.sweep().await.unwrap();
    assert_eq!(swept.templates, vec![replaced.clone()]);
    let mut kept = vec![repo.clone(), snapshot, base];
    kept.sort();
    assert_eq!(template_names(&fake), kept);
    assert_eq!(templates.ready(REPO).unwrap(), repo);
    assert_eq!(
        templates.ready(REPO_OLD).unwrap_err(),
        TemplateState::Unknown.describe(),
        "a swept template is not ready"
    );
}

/// A template the sweep removed, once forgotten, is registered again by the
/// next `ensure`, with the usual build, rather than named as ready.
#[tokio::test]
async fn a_swept_template_is_registered_again() {
    support::state::isolate();
    let fake = FakeDo::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let builder = Arc::new(FakeBuildKit {
        scratch: tmp.path().join("state/tmp"),
        ..Default::default()
    });
    let owner = Owner::new("tracon", "abcd1234");
    let templates = Templates::new(built(tmp.path()), "coding-base", &owner.template_stem())
        .unwrap()
        .with_builder(builder.clone());
    let client = fake.client();
    let name = templates.name_for(SNAPSHOT).unwrap();
    templates.ensure(&client, "unused", SNAPSHOT).await.unwrap();
    let state = templates.ensure(&client, "unused", SNAPSHOT).await.unwrap();
    assert!(matches!(state, TemplateState::Ready { .. }), "{state:?}");

    let swept = tracon::provider::digitalocean::upkeep::sweep(
        &client,
        &owner,
        &templates,
        Some(&Default::default()),
        tracon::store::now_ms(),
    )
    .await
    .unwrap();
    assert_eq!(swept.templates, vec![name.clone()]);
    templates.forget(&swept.templates);
    assert!(templates.ready(SNAPSHOT).is_err());

    let state = templates.ensure(&client, "unused", SNAPSHOT).await.unwrap();
    assert!(matches!(state, TemplateState::Building { .. }), "{state:?}");
    assert_eq!(builder.built.lock().unwrap().len(), 2, "built again");
    assert_eq!(template_names(&fake), vec![name.clone()]);
    let state = templates.ensure(&client, "unused", SNAPSHOT).await.unwrap();
    assert!(matches!(state, TemplateState::Ready { .. }), "{state:?}");
    assert_eq!(templates.ready(SNAPSHOT).unwrap(), name);
}

/// A create DigitalOcean refuses because the template is gone (409, "not
/// provisioned") is classified as such, and the template forgotten: the
/// image has none ready, and the next `ensure` registers it again.
#[tokio::test]
async fn a_create_refused_for_a_gone_template_forgets_it() {
    support::state::isolate();
    let fake = FakeDo::start().await;
    let runtime = runtime(&fake, Source::Given(IMAGE.into())).await;
    let templates = runtime.provider().templates();
    let name = templates.name_for(IMAGE).unwrap();
    fake.api
        .lock()
        .unwrap()
        .put_template(&name, "TEMPLATE_STATUS_READY");
    assert!(matches!(
        runtime.ensure_template(IMAGE).await,
        TemplateState::Ready { .. }
    ));
    // Removed behind the node's back.
    fake.api.lock().unwrap().templates.clear();

    let bootstrap = tracon::provider::Bootstrap {
        run: "run-gone".into(),
        cert_pin: "sha256:00".into(),
        token: "t".into(),
    };
    let error = tracon::provider::SandboxProvider::create(
        runtime.provider().as_ref(),
        &tracon::provider::CreateSpec {
            name: Some("tracon-h-gone"),
            image: IMAGE,
            resources: &tracon::provider::Resources::default(),
            mounts: &[],
            bootstrap: &bootstrap,
            labels: &tracon::runner::RunLabels::default(),
            shape: tracon::provider::Shape::Probe,
        },
    )
    .await
    .unwrap_err();
    let tracon::provider::ProviderError::Refused { reason, message } = error else {
        panic!("{error:?}");
    };
    assert_eq!(reason, tracon::provider::digitalocean::TEMPLATE_GONE);
    assert!(message.contains("not provisioned"), "{message}");
    assert!(fake.api.lock().unwrap().sessions.is_empty());
    assert_eq!(
        templates.ready(IMAGE).unwrap_err(),
        TemplateState::Unknown.describe()
    );

    assert!(matches!(
        runtime.ensure_template(IMAGE).await,
        TemplateState::Building { .. }
    ));
    assert_eq!(template_names(&fake), vec![name]);
}
