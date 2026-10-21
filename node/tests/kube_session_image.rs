//! A Kubernetes session in its repository's image, against a fake API.
//!
//! `repo_image` layers the harness onto the repository's image with a
//! profile for pods (the harness uid, its home, an init) and the session runs
//! there: the pod names the layered image, runs it under that init (and, for
//! OpenCode, the harness entrypoint the layer copied in), as the harness uid
//! and never root,
//! with the session's cache at `/cache`. The repository's preparation runs on
//! the workspace first, in the repository's image, through an egress grant
//! of its own.

#[path = "support/mod.rs"]
mod support;
use support::kube_api::{KubeApi, Script};

use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use serde_json::json;
use tracon::adapter::HarnessAdapter;
use tracon::boundary::kubernetes::KubeBackend;
use tracon::boundary::Backend;
use tracon::config::{Config, KubeBuilder, RuntimeKind};
use tracon::environment::{PrepareOutcome, RepoEnvironment};
use tracon::runner::kube::{PodEnv, SESSION_INIT};
use tracon::runner::toolchain::IMAGE_ENTRYPOINT;
use tracon::runner::{Mount, RunnerCommand};

const NAMESPACE: &str = "tracon-test";
const SESSION_IMAGE: &str = "registry.digitalocean.com/cosmicspork/tracon-repo-app-0123456789-claude@sha256:1111111111111111111111111111111111111111111111111111111111111111";
const BASE_IMAGE: &str = "registry.digitalocean.com/cosmicspork/tracon-repo-app-0123456789@sha256:2222222222222222222222222222222222222222222222222222222222222222";

fn config(state: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    cfg.runtime.kind = RuntimeKind::Kubernetes;
    cfg.runtime.kubernetes.state_mount = state.to_path_buf();
    cfg
}

fn backend(api: &KubeApi, cfg: &Config) -> KubeBackend {
    let backend = KubeBackend::with_client(
        cfg,
        PodEnv {
            namespace: NAMESPACE.into(),
            pod_ip: "10.244.0.9".into(),
            node_name: "general-1".into(),
        },
        api.client(),
    );
    backend.secret_rights().set(Ok(()));
    backend.egress_port().set(Ok(()));
    backend
}

fn created(api: &KubeApi, name: &str) -> Pod {
    api.created()
        .into_iter()
        .find(|pod| pod.metadata.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no pod {name}: {:?}", api.live()))
}

/// Start a harness in `runner` as a session would, far enough for its pod to
/// be created: the fake API has no attach, so it goes no further.
async fn start(runner: &dyn tracon::runner::Runner, name: &str, env: Vec<(String, String)>) {
    let _ = runner
        .spawn(RunnerCommand {
            argv: vec!["claude".into(), "--print".into()],
            env,
            name: name.into(),
            ..Default::default()
        })
        .await;
}

/// `runner_in` puts a session in the image it is given: the pod names that
/// image and runs the init the layer copied in front of the harness, then
/// the harness's entrypoint where it has one: OpenCode's layer copies it to
/// the path its own image has it. It is the harness uid's, never root's, and
/// sized as a session.
#[tokio::test(start_paused = true)]
async fn a_session_runs_in_the_layered_image_under_its_init() {
    support::state::isolate();
    let api =
        KubeApi::new(NAMESPACE).default_script(Script::exits(0).running(Duration::from_secs(60)));
    let state = tempfile::tempdir().unwrap();
    let cfg = config(state.path());
    let backend = backend(&api, &cfg);

    for harness in ["claude", "opencode"] {
        let runner = backend.runner_in(harness, SESSION_IMAGE, Vec::new());
        start(runner.as_ref(), &format!("tracon-h-{harness}"), Vec::new()).await;
        let pod = created(&api, &format!("tracon-h-{harness}"));
        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        assert_eq!(container.image.as_deref(), Some(SESSION_IMAGE), "{harness}");
        let mut command = vec![SESSION_INIT, "--"];
        if harness == "opencode" {
            command.push(IMAGE_ENTRYPOINT);
        }
        assert_eq!(
            container.command.clone().unwrap(),
            command,
            "{harness}: the init, then the harness's own entrypoint"
        );
        assert_eq!(
            container.args.clone().unwrap(),
            ["claude", "--print"],
            "{harness}"
        );
        let pod_security = spec.security_context.unwrap();
        assert_eq!(pod_security.run_as_non_root, Some(true));
        assert_eq!(pod_security.run_as_user, Some(65532));
        assert_eq!(pod_security.run_as_group, Some(65532));
        let requests = container.resources.clone().unwrap().requests.unwrap();
        assert_eq!(
            requests["memory"].0,
            cfg.runtime.kubernetes.resources.session().memory_request,
            "{harness}: sized as a session"
        );
        let home = container
            .env
            .iter()
            .flatten()
            .find(|var| var.name == "HOME")
            .and_then(|var| var.value.clone());
        assert_eq!(home.as_deref(), Some("/home/harness"));

        // A command the session's runner runs in another image (the
        // repository's own, for a check) gets neither the init nor the
        // entrypoint: that image has neither file.
        let _ = runner
            .spawn(RunnerCommand {
                argv: vec!["true".into()],
                image: Some(BASE_IMAGE.into()),
                name: format!("tracon-c-{harness}"),
                ..Default::default()
            })
            .await;
        let pod = created(&api, &format!("tracon-c-{harness}"));
        let container = &pod.spec.unwrap().containers[0];
        assert_eq!(container.command.clone().unwrap(), ["true"], "{harness}");
        assert!(container.args.is_none(), "{harness}");
    }

    // The harness's own image keeps what it has: OpenCode's entrypoint, and
    // no init it does not carry.
    let own = Backend::runner_for(&backend, "opencode", Vec::new());
    start(own.as_ref(), "tracon-h-own", Vec::new()).await;
    let pod = created(&api, "tracon-h-own");
    let container = &pod.spec.unwrap().containers[0];
    assert_eq!(
        container.image.as_deref(),
        Some(cfg.kubernetes_harness_image("opencode").as_str())
    );
    let command = container.command.clone().unwrap();
    assert_eq!(command.len(), 1);
    assert_ne!(command[0], SESSION_INIT);

    // Asked for the harness image itself, `runner_in` is `runner_for`.
    let same = backend.runner_in(
        "opencode",
        &cfg.kubernetes_harness_image("opencode"),
        Vec::new(),
    );
    start(same.as_ref(), "tracon-h-same", Vec::new()).await;
    let pod = created(&api, "tracon-h-same");
    assert_eq!(
        pod.spec.unwrap().containers[0].command,
        Some(command),
        "the harness image keeps its entrypoint"
    );
}

/// An OpenCode session launched in its layered image, as the session manager
/// launches it, against the fake API: the pod runs the layered image under
/// the init and the harness entrypoint, starts the binary by the path the
/// layer put it at, exposes the server's port, and carries every secret the
/// session has (the gateway token, the per-launch server password) as a
/// `secretKeyRef`, never as a value.
#[tokio::test(start_paused = true)]
async fn an_opencode_session_pod_runs_the_layered_image_with_its_secrets_hidden() {
    const TOKEN: &str = "session-token-0123456789abcdef";
    const OPENCODE_IMAGE: &str = "registry.digitalocean.com/cosmicspork/tracon-repo-app-0123456789-opencode@sha256:3333333333333333333333333333333333333333333333333333333333333333";
    support::state::isolate();
    let api =
        KubeApi::new(NAMESPACE).default_script(Script::exits(0).running(Duration::from_secs(60)));
    let state = tempfile::tempdir().unwrap();
    let cfg = config(state.path());
    let backend = backend(&api, &cfg);
    let home = backend.harness_home();

    let mut wiring = tracon::gateway::model::harness_wiring(&cfg, "tracon-gw", TOKEN, |_, _| true);
    wiring.mcp_servers = vec![json!({
        "type": "http",
        "name": "tracon",
        "url": "http://tracon-gw:7421/mcp/s1",
        "headers": [{ "name": "Authorization", "value": format!("Bearer {TOKEN}") }],
    })];
    let (env, secret_env) = tracon::session::harness_environment(&wiring, None, &home);
    let (runner, cache_env) = tracon::session::session_runner(
        &backend,
        "opencode",
        Some(OPENCODE_IMAGE),
        Some("tracon-cache-app".into()),
        Vec::new(),
    );
    let mut env = env;
    env.extend(cache_env);
    let opencode = tracon::adapter::opencode::OpenCodeAdapter::new("1.18.30");
    // It goes no further than attaching (the fake API has no attach), but
    // its pod is created as it would be.
    let launched = opencode
        .launch(
            runner.as_ref(),
            tracon::adapter::LaunchSpec {
                cwd_in_runner: "/work".into(),
                model: "anthropic/claude-x".into(),
                container_name: "tracon-h-oc".into(),
                harness_home: home.clone(),
                mcp_servers: wiring.mcp_servers.clone(),
                tools: Vec::new(),
                env,
                secret_env,
                system_prompt_file: None,
                cursor: None,
                resume: None,
            },
        )
        .await;
    assert!(launched.is_err());

    let pod = created(&api, "tracon-h-oc");
    let rendered = serde_json::to_string(&pod).unwrap();
    assert!(!rendered.contains(TOKEN), "the token is in the pod spec");
    let spec = pod.spec.unwrap();
    let container = &spec.containers[0];
    assert_eq!(container.image.as_deref(), Some(OPENCODE_IMAGE));
    assert_eq!(
        container.command.clone().unwrap(),
        [SESSION_INIT, "--", IMAGE_ENTRYPOINT]
    );
    let args = container.args.clone().unwrap();
    assert_eq!(
        args[..2],
        [tracon::adapter::opencode::OpenCodeAdapter::BINARY, "serve"]
    );
    let port: i32 = args[args.iter().position(|a| a == "--port").unwrap() + 1]
        .parse()
        .unwrap();
    assert!(container
        .ports
        .iter()
        .flatten()
        .any(|p| p.container_port == port));
    assert_eq!(spec.security_context.unwrap().run_as_user, Some(65532));

    let var = |name: &str| {
        container
            .env
            .iter()
            .flatten()
            .find(|var| var.name == name)
            .cloned()
    };
    for name in [
        tracon::adapter::opencode::SERVER_PASSWORD_ENV,
        "TRACON_PROVIDER_KEY_ANTHROPIC",
    ] {
        let hidden = var(name).unwrap_or_else(|| panic!("no {name}"));
        assert!(hidden.value.is_none(), "{name} has a value in the pod spec");
        assert!(
            hidden
                .value_from
                .and_then(|from| from.secret_key_ref)
                .is_some(),
            "{name} is not from the Secret"
        );
    }
    // The launch's own environment, unchanged by the image it runs in: the
    // per-session home and XDG tree under the harness's home, and the cache.
    assert!(var("XDG_CACHE_HOME")
        .and_then(|v| v.value)
        .unwrap()
        .starts_with(&format!("{home}/.opencode/")));
    assert_eq!(
        var("OPENCODE_DISABLE_PROJECT_CONFIG").and_then(|v| v.value),
        Some("true".into())
    );
    assert!(container
        .volume_mounts
        .iter()
        .flatten()
        .any(|mount| mount.mount_path == "/cache"));
}

/// A session in its repository's image gets its session cache at `/cache`
/// and the variables that point package managers and builds into it; one in
/// the harness's own image gets neither.
#[tokio::test(start_paused = true)]
async fn a_kubernetes_session_in_its_repository_image_has_its_cache() {
    support::state::isolate();
    let api =
        KubeApi::new(NAMESPACE).default_script(Script::exits(0).running(Duration::from_secs(60)));
    let state = tempfile::tempdir().unwrap();
    let cfg = config(state.path());
    let backend = backend(&api, &cfg);
    let cache = "tracon-cache-s-app-personal".to_string();
    let workspace = Mount::volume("tracon-workspace-a", "/work", false);

    let (runner, env) = tracon::session::session_runner(
        &backend,
        "claude",
        Some(SESSION_IMAGE),
        Some(cache.clone()),
        vec![workspace.clone()],
    );
    for name in ["CARGO_TARGET_DIR", "CARGO_HOME", "npm_config_cache"] {
        assert!(
            env.iter()
                .any(|(set, value)| set == name && value.starts_with("/cache/")),
            "{name}: {env:?}"
        );
    }
    start(runner.as_ref(), "tracon-h-cached", env).await;
    let pod = created(&api, "tracon-h-cached");
    let container = &pod.spec.unwrap().containers[0];
    assert_eq!(container.image.as_deref(), Some(SESSION_IMAGE));
    let mounts = container.volume_mounts.clone().unwrap();
    let at = |target: &str| {
        mounts
            .iter()
            .find(|mount| mount.mount_path == target)
            .cloned()
    };
    let cached = at("/cache").expect("the session cache is mounted");
    assert_eq!(cached.sub_path.as_deref(), Some(cache.as_str()));
    assert_eq!(cached.read_only, Some(false));
    assert!(at("/work").is_some());
    let env = container.env.clone().unwrap();
    assert!(env
        .iter()
        .any(|var| var.name == "CARGO_TARGET_DIR" && var.value.is_some()));

    let (runner, env) =
        tracon::session::session_runner(&backend, "claude", None, Some(cache), vec![workspace]);
    assert!(env.is_empty());
    start(runner.as_ref(), "tracon-h-bare", env).await;
    let pod = created(&api, "tracon-h-bare");
    let container = &pod.spec.unwrap().containers[0];
    assert_eq!(
        container.image.as_deref(),
        Some(cfg.kubernetes_harness_image("claude").as_str())
    );
    assert!(!container
        .volume_mounts
        .iter()
        .flatten()
        .any(|mount| mount.mount_path == "/cache"));
}

/// The repository's preparation runs on a Kubernetes session's workspace:
/// in the repository's image, as the harness uid, with the workspace and
/// the session's cache writable and its own egress grant, which reaches the
/// pod only through the run's Secret.
#[tokio::test]
async fn a_kubernetes_sessions_workspace_is_prepared_through_its_grant() {
    support::state::isolate();
    let api = KubeApi::new(NAMESPACE).default_script(Script::exits(0));
    let state = tempfile::tempdir().unwrap();
    let cfg = config(state.path());
    let backend = backend(&api, &cfg);
    let environment = RepoEnvironment {
        image: BASE_IMAGE.into(),
        image_source: "repository Dockerfile",
        checks: Vec::new(),
        timeout_secs: 60,
        prepare: vec!["composer install".into()],
        egress: vec!["repo.packagist.org".into()],
        session_egress: Vec::new(),
        cache_volume: "tracon-cache-base-app".into(),
    };

    let done = tracon::session::prepare_session_workspace(
        &backend,
        &environment,
        ("tracon-workspace-a", Ok("tracon-cache-s-app-personal")),
        "tracon-s-app",
        &tracon::review::checks::RunToCompletion,
    )
    .await
    .expect("the entry prepares");
    assert_eq!(done.outcome, PrepareOutcome::Prepared, "{done:?}");

    let pods = api.created();
    assert_eq!(pods.len(), 1, "{:?}", api.live());
    let spec = pods[0].spec.clone().unwrap();
    let container = &spec.containers[0];
    assert_eq!(container.image.as_deref(), Some(BASE_IMAGE));
    assert_eq!(
        spec.security_context.unwrap().run_as_user,
        Some(cfg.runtime.kubernetes.uid)
    );
    let mounts = container.volume_mounts.clone().unwrap();
    for (target, volume) in [
        ("/work", "tracon-workspace-a"),
        ("/cache", "tracon-cache-s-app-personal"),
    ] {
        let mount = mounts
            .iter()
            .find(|mount| mount.mount_path == target)
            .unwrap_or_else(|| panic!("{target} is not mounted"));
        assert_eq!(mount.sub_path.as_deref(), Some(volume));
        assert_eq!(mount.read_only, Some(false));
    }
    let proxy = container
        .env
        .iter()
        .flatten()
        .find(|var| var.name == "HTTPS_PROXY")
        .expect("the grant is in the environment");
    assert!(proxy.value.is_none(), "the grant is in the pod spec");
    assert!(proxy
        .value_from
        .as_ref()
        .is_some_and(|from| from.secret_key_ref.is_some()));
    let secrets = api.secrets_created();
    assert_eq!(secrets.len(), 1);
    let url =
        String::from_utf8(secrets[0].data.as_ref().unwrap()["HTTPS_PROXY"].0.clone()).unwrap();
    assert!(url.starts_with("http://prepare:"), "{url}");
    assert!(url.ends_with("@tracon-gw:7424"), "{url}");
}

/// Whether a session is layered at all follows the builder: without
/// `[runtime.kubernetes.builder]` a session keeps its harness's image and is
/// told why; with one, it is layered.
#[tokio::test]
async fn only_a_node_with_a_builder_runs_sessions_in_repository_images() {
    support::state::isolate();
    let api = KubeApi::new(NAMESPACE);
    let state = tempfile::tempdir().unwrap();
    let mut cfg = config(state.path());
    assert!(!backend(&api, &cfg).runs_sessions_in_repository_images());
    cfg.runtime.kubernetes.builder = Some(KubeBuilder {
        registry: "registry.digitalocean.com/cosmicspork".into(),
        ..Default::default()
    });
    assert!(backend(&api, &cfg).runs_sessions_in_repository_images());
}
