//! The Kubernetes runner's captured runs, against a fake API.
//!
//! What a check or a preparation is told about a command it ran in a pod: the
//! container's own exit code, a start that is bounded and a run that is not,
//! the image identity an approval keys evidence on, and which image gets the
//! harness's entrypoint. Every test runs on tokio's paused clock, so a pod
//! that runs for twenty minutes takes none.

#[path = "support/mod.rs"]
mod support;
use support::kube_api::{KubeApi, Script};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use tracon::config::{Config, RuntimeKind};
use tracon::runner::kube::{KubeRunner, KubeSpec, PodEnv};
use tracon::runner::{Runner, RunnerCommand};

const NAMESPACE: &str = "tracon-test";

fn pinned(name: &str) -> String {
    format!("registry.example/{name}@sha256:{}", "ab".repeat(32))
}

fn runner(api: &KubeApi, harness: &str, state: &std::path::Path) -> KubeRunner {
    let mut cfg = Config::default();
    cfg.runtime.kind = RuntimeKind::Kubernetes;
    let mut spec = KubeSpec::for_harness(
        &cfg,
        harness,
        PodEnv {
            namespace: NAMESPACE.into(),
            pod_ip: "10.244.0.9".into(),
            node_name: "general-1".into(),
        },
    );
    spec.state_mount = state.to_path_buf();
    KubeRunner::new(api.client(), spec)
}

fn check(name: &str, image: Option<String>) -> RunnerCommand {
    RunnerCommand {
        argv: vec!["sh".into(), "-c".into(), "just check".into()],
        name: name.into(),
        image,
        ..Default::default()
    }
}

/// `just check` failing with 2 is a failing check, and a missing tool (127)
/// is the environment's fault rather than the candidate's. Neither is
/// visible when every failure reads as 1.
#[tokio::test(start_paused = true)]
async fn the_containers_exit_code_is_the_runs() {
    let api = KubeApi::new(NAMESPACE)
        .script(
            "tracon-c-lint",
            Script::exits(2)
                .pending(Duration::from_secs(3))
                .running(Duration::from_secs(5))
                .logs("lint: 3 problems\n"),
        )
        .script(
            "tracon-c-missing",
            Script::exits(127).running(Duration::from_secs(1)),
        );
    let state = tempfile::tempdir().unwrap();
    let runner = runner(&api, "claude", state.path());

    let out = runner
        .run_capture(check("tracon-c-lint", Some(pinned("app"))))
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "lint: 3 problems\n");
    assert!(out.stderr.is_empty());

    let out = runner
        .run_capture(check("tracon-c-missing", Some(pinned("app"))))
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(127));

    let ok = runner
        .run_capture(check("tracon-c-ok", None))
        .await
        .unwrap();
    assert!(ok.status.success());

    // Every capture pod is removed once its result is read.
    assert!(api.live().is_empty(), "left behind: {:?}", api.live());
    assert_eq!(api.deleted().len(), 3);
}

/// The old runner gave up on any capture at 180 s. A check suite can take
/// twenty minutes, and the image pull before it several more; only the start
/// counts against the start deadline, and the run is the caller's to bound.
#[tokio::test(start_paused = true)]
async fn a_run_past_three_minutes_completes() {
    let api = KubeApi::new(NAMESPACE).default_script(
        Script::exits(0)
            .pending(Duration::from_secs(4 * 60))
            .running(Duration::from_secs(20 * 60))
            .logs("all green\n"),
    );
    let state = tempfile::tempdir().unwrap();
    let runner = runner(&api, "claude", state.path());
    let started = tokio::time::Instant::now();

    let out = runner
        .run_capture(check("tracon-c-slow", Some(pinned("app"))))
        .await
        .unwrap();

    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "all green\n");
    assert!(started.elapsed() >= Duration::from_secs(24 * 60));
    assert!(api.live().is_empty());
}

/// A pod that never starts is given up at the configured deadline, with what
/// it was waiting for, and removed.
#[tokio::test(start_paused = true)]
async fn a_pod_that_never_starts_is_given_up_with_its_reason() {
    let api = KubeApi::new(NAMESPACE).default_script(Script::never_starts("ContainerCreating"));
    let state = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.runtime.kind = RuntimeKind::Kubernetes;
    cfg.runtime.kubernetes.start_timeout_secs = 90;
    let mut spec = KubeSpec::for_harness(
        &cfg,
        "claude",
        PodEnv {
            namespace: NAMESPACE.into(),
            pod_ip: "10.244.0.9".into(),
            node_name: "general-1".into(),
        },
    );
    spec.state_mount = state.path().to_path_buf();
    let runner = KubeRunner::new(api.client(), spec);
    let started = tokio::time::Instant::now();

    let error = runner
        .run_capture(check("tracon-c-pull", Some(pinned("big"))))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("did not start in 90s"), "{error}");
    assert!(error.contains("ContainerCreating"), "{error}");
    assert!(error.contains("pulling"), "{error}");
    assert!(error.contains("start_timeout_secs"), "{error}");
    let waited = started.elapsed();
    assert!(waited >= Duration::from_secs(90) && waited < Duration::from_secs(92));
    assert!(api.live().is_empty());
}

/// A pull that fails outright is not waited out.
#[tokio::test(start_paused = true)]
async fn a_failed_pull_is_reported_at_once() {
    let api = KubeApi::new(NAMESPACE).default_script(Script::never_starts("ImagePullBackOff"));
    let state = tempfile::tempdir().unwrap();
    let runner = runner(&api, "claude", state.path());
    let started = tokio::time::Instant::now();
    let error = runner
        .run_capture(check("tracon-c-gone", Some(pinned("gone"))))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("ImagePullBackOff"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(api.live().is_empty());
}

/// The caller owns the deadline: it kills by `capture_name`, which removes
/// the pod, and the capture ends saying so rather than reporting an exit.
#[tokio::test(start_paused = true)]
async fn a_killed_capture_ends_without_an_exit_code() {
    let api = KubeApi::new(NAMESPACE)
        .default_script(Script::exits(0).running(Duration::from_secs(60 * 60)));
    let state = tempfile::tempdir().unwrap();
    let runner = Arc::new(runner(&api, "claude", state.path()));
    let capture = tokio::spawn({
        let runner = runner.clone();
        async move { runner.run_capture(check("tracon-c-long", None)).await }
    });
    tokio::time::sleep(Duration::from_secs(30)).await;
    let name = runner.capture_name("tracon-c-long");
    assert_eq!(api.live(), vec![name.clone()]);

    runner.kill(&name).await.unwrap();

    let error = capture.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("removed before it finished"), "{error}");
    assert!(api.live().is_empty());
}

/// A caller that stops waiting without a kill (a dropped future) does not
/// leave the command running in the cluster either.
#[tokio::test(start_paused = true)]
async fn a_dropped_capture_removes_its_pod() {
    let api = KubeApi::new(NAMESPACE)
        .default_script(Script::exits(0).running(Duration::from_secs(60 * 60)));
    let state = tempfile::tempdir().unwrap();
    let runner = runner(&api, "claude", state.path());

    let timed_out = tokio::time::timeout(
        Duration::from_secs(30),
        runner.run_capture(check("tracon-c-dropped", None)),
    )
    .await;
    assert!(timed_out.is_err());

    for _ in 0..100 {
        if api.live().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(api.live().is_empty(), "left behind: {:?}", api.live());
}

/// A pod evicted before its container reported an exit has no code to give;
/// that is an error, never a made-up 1.
#[tokio::test(start_paused = true)]
async fn an_evicted_pod_reports_no_exit_code() {
    let api = KubeApi::new(NAMESPACE)
        .default_script(Script::exits(0).running(Duration::from_secs(5)).evicted());
    let state = tempfile::tempdir().unwrap();
    let runner = runner(&api, "claude", state.path());
    let error = runner
        .run_capture(check("tracon-c-evicted", None))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("without an exit code"), "{error}");
    assert!(error.contains("Evicted"), "{error}");
}

/// Approval asks a runner made for the purpose, which has run nothing. A
/// digest-pinned image is its own identity, so the approval of a check run in
/// one is not refused for want of a previous run.
#[tokio::test(start_paused = true)]
async fn a_fresh_runner_resolves_a_pinned_image() {
    let api = KubeApi::new(NAMESPACE).default_script(
        Script::exits(0).image_id(&format!("docker-pullable://{}", pinned("tagged"))),
    );
    let state = tempfile::tempdir().unwrap();
    let fresh = runner(&api, "claude", state.path());

    let image = pinned("app");
    assert_eq!(
        fresh.resolved_image(Some(&image)).await,
        Some(image.clone())
    );
    let tagged_and_pinned = format!("registry.example/app:1.2@sha256:{}", "cd".repeat(32));
    assert_eq!(
        fresh.resolved_image(Some(&tagged_and_pinned)).await,
        Some(tagged_and_pinned.clone())
    );
    // A tag is not an identity until a run says what it resolved to.
    assert_eq!(
        fresh.resolved_image(Some("registry.example/app:1.2")).await,
        None
    );
    assert!(api.created().is_empty(), "resolving ran nothing");

    // A spec image pinned by digest answers for a command with no override.
    let mut cfg = Config::default();
    cfg.runtime.kind = RuntimeKind::Kubernetes;
    cfg.runtime.kubernetes.harness_image = pinned("harness");
    let mut spec = KubeSpec::for_harness(
        &cfg,
        &cfg.harness.id,
        PodEnv {
            namespace: NAMESPACE.into(),
            pod_ip: "10.244.0.9".into(),
            node_name: "general-1".into(),
        },
    );
    spec.state_mount = state.path().to_path_buf();
    let harness = KubeRunner::new(api.client(), spec);
    assert_eq!(harness.resolved_image(None).await, Some(pinned("harness")));

    // After a run, a tag is answered with what the kubelet ran for it, and
    // only for that tag.
    let tag = "registry.example/tagged:latest";
    fresh
        .run_capture(check("tracon-c-tagged", Some(tag.into())))
        .await
        .unwrap();
    assert_eq!(
        fresh.resolved_image(Some(tag)).await,
        Some(pinned("tagged"))
    );
    assert_eq!(
        fresh
            .resolved_image(Some("registry.example/other:latest"))
            .await,
        None
    );
}

/// What reaches the API server for a check: the command's requests and
/// memory limit (a namespace's LimitRange would otherwise fill in its own and
/// refuse a request above its default limit), the configured pull secrets,
/// a memory-backed `/dev/shm` for a browser, and a writable `TMPDIR`.
#[tokio::test(start_paused = true)]
async fn a_captured_pod_is_created_sized_with_shared_memory_and_pull_secrets() {
    let api = KubeApi::new(NAMESPACE).default_script(Script::exits(0));
    let state = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.runtime.kind = RuntimeKind::Kubernetes;
    cfg.runtime.kubernetes.image_pull_secrets = vec!["registry-tracon".into()];
    cfg.runtime.kubernetes.resources.command.memory_limit = Some("5Gi".into());
    cfg.runtime.kubernetes.validate().unwrap();
    let mut spec = KubeSpec::for_harness(
        &cfg,
        "claude",
        PodEnv {
            namespace: NAMESPACE.into(),
            pod_ip: "10.244.0.9".into(),
            node_name: "general-1".into(),
        },
    );
    spec.state_mount = state.path().to_path_buf();
    let runner = KubeRunner::new(api.client(), spec);

    runner
        .run_capture(check("tracon-c-browser", Some(pinned("app"))))
        .await
        .unwrap();

    let created = api.created();
    assert_eq!(created.len(), 1);
    let spec = created[0].spec.clone().unwrap();
    let pull: Vec<_> = spec
        .image_pull_secrets
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(pull, ["registry-tracon"]);

    let container = &spec.containers[0];
    let resources = container.resources.clone().unwrap();
    let get = |map: &Option<BTreeMap<String, Quantity>>, key: &str| {
        map.as_ref().and_then(|m| m.get(key)).map(|q| q.0.clone())
    };
    assert_eq!(get(&resources.requests, "cpu").as_deref(), Some("500m"));
    assert_eq!(get(&resources.requests, "memory").as_deref(), Some("2Gi"));
    assert_eq!(get(&resources.limits, "memory").as_deref(), Some("5Gi"));
    assert_eq!(get(&resources.limits, "cpu"), None);

    let volumes = spec.volumes.unwrap();
    let shm = volumes
        .iter()
        .find(|v| {
            v.empty_dir
                .as_ref()
                .is_some_and(|e| e.medium.as_deref() == Some("Memory"))
        })
        .expect("a memory-backed emptyDir");
    assert_eq!(
        shm.empty_dir
            .as_ref()
            .unwrap()
            .size_limit
            .as_ref()
            .map(|q| q.0.as_str()),
        Some("1Gi")
    );
    let mounts = container.volume_mounts.clone().unwrap();
    assert!(mounts
        .iter()
        .any(|m| m.name == shm.name && m.mount_path == "/dev/shm"));
    let tmpdir = container
        .env
        .iter()
        .flatten()
        .find(|e| e.name == "TMPDIR")
        .and_then(|e| e.value.clone())
        .expect("TMPDIR");
    let scratch = mounts
        .iter()
        .find(|m| m.mount_path == tmpdir)
        .expect("TMPDIR is a mount of its own");
    assert!(volumes
        .iter()
        .any(|v| v.name == scratch.name && v.empty_dir.is_some()));
}
