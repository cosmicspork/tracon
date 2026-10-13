//! A run's secret environment on Kubernetes, against a fake API.
//!
//! What anyone with `get pods` can read is the pod spec. A lent credential or
//! an egress grant's proxy URL must not be in it: it goes in a Secret of the
//! run's own, made before the pod, owned by the pod, and deleted once the pod
//! has started or the run is over, however it ended. A node that may not do
//! that refuses the run rather than falling back to a plaintext value.

#[path = "support/mod.rs"]
mod support;
use support::kube_api::{KubeApi, Script};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use tracon::boundary::kubernetes::{checks, KubeBackend};
use tracon::boundary::Backend;
use tracon::config::{Config, RuntimeKind};
use tracon::runner::kube::{
    secret_name, KubeRunner, KubeSpec, PodEnv, SecretRights, PID_LABEL, RUN_LABEL,
};
use tracon::runner::{Runner, RunnerCommand, SecretEnv};

const NAMESPACE: &str = "tracon-test";
const TOKEN: &str = "npm_lent0123456789";
const PROXY: &str = "http://prepare:grant-password-0123@tracon-gw:7424";

fn pod_env() -> PodEnv {
    PodEnv {
        namespace: NAMESPACE.into(),
        pod_ip: "10.244.0.9".into(),
        node_name: "general-1".into(),
    }
}

fn config() -> Config {
    let mut cfg = Config::default();
    cfg.runtime.kind = RuntimeKind::Kubernetes;
    cfg
}

fn runner_with(api: &KubeApi, state: &std::path::Path, rights: SecretRights) -> KubeRunner {
    let mut spec = KubeSpec::for_harness(&config(), "claude", pod_env());
    spec.state_mount = state.to_path_buf();
    KubeRunner::new(api.client(), spec).with_secret_rights(rights)
}

fn runner(api: &KubeApi, state: &std::path::Path) -> KubeRunner {
    runner_with(api, state, SecretRights::granted())
}

/// A warm-up as #466 runs it: a lent credential and its own egress grant.
fn lent(name: &str) -> RunnerCommand {
    RunnerCommand {
        argv: vec!["sh".into(), "-c".into(), "npm ci".into()],
        env: vec![("npm_config_cache".into(), "/cache/npm".into())],
        secret_env: SecretEnv::new(vec![
            ("NPM_TOKEN".into(), TOKEN.into()),
            ("HTTPS_PROXY".into(), PROXY.into()),
            ("https_proxy".into(), PROXY.into()),
        ]),
        name: name.into(),
        image: Some(format!("registry.example/app@sha256:{}", "ab".repeat(32))),
        ..Default::default()
    }
}

/// Every Secret is gone, and each was deleted by name rather than left to
/// the garbage collector.
fn all_removed(api: &KubeApi) {
    assert!(
        api.secrets_live().is_empty(),
        "left behind: {:?}",
        api.secrets_live()
            .iter()
            .map(|s| s.metadata.name.clone())
            .collect::<Vec<_>>()
    );
    let created: Vec<_> = api
        .secrets_created()
        .into_iter()
        .filter_map(|s| s.metadata.name)
        .collect();
    let deleted = api.secrets_deleted();
    for name in &created {
        assert!(deleted.contains(name), "{name} was never deleted");
    }
}

async fn settle(api: &KubeApi) {
    for _ in 0..100 {
        if api.secrets_live().is_empty() && api.live().is_empty() {
            return;
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_lent_value_is_never_in_the_pod_spec() {
    let api = KubeApi::new(NAMESPACE).default_script(
        Script::exits(0)
            .pending(Duration::from_secs(5))
            .running(Duration::from_secs(10))
            .logs("added 120 packages\n"),
    );
    let state = tempfile::tempdir().unwrap();
    let runner = runner(&api, state.path());

    let out = runner.run_capture(lent("tracon-w-app")).await.unwrap();
    assert!(out.status.success());

    let pod_name = runner.capture_name("tracon-w-app");
    let created = api.created();
    assert_eq!(created.len(), 1);
    let pod = &created[0];
    // Nowhere in the spec: not as a value, not in a label, not anywhere.
    let rendered = serde_json::to_string(pod).unwrap();
    for value in [TOKEN, PROXY, "grant-password-0123"] {
        assert!(!rendered.contains(value), "{value} is in the pod spec");
    }
    let env = pod.spec.as_ref().unwrap().containers[0]
        .env
        .clone()
        .unwrap();
    for name in ["NPM_TOKEN", "HTTPS_PROXY", "https_proxy"] {
        let vars: Vec<_> = env.iter().filter(|v| v.name == name).collect();
        assert_eq!(vars.len(), 1, "{name} is set once");
        assert!(vars[0].value.is_none());
        let selector = vars[0]
            .value_from
            .as_ref()
            .and_then(|f| f.secret_key_ref.as_ref())
            .unwrap_or_else(|| panic!("{name} has no secretKeyRef"));
        assert_eq!(selector.name, secret_name(&pod_name));
        assert_eq!(selector.key, name);
    }
    // Plain environment stays plain, and the harness proxy the runner would
    // otherwise set is not set beside the grant.
    assert!(env
        .iter()
        .any(|v| v.name == "npm_config_cache" && v.value.as_deref() == Some("/cache/npm")));
    assert!(!env
        .iter()
        .any(|v| v.name == "HTTPS_PROXY" && v.value.is_some()));

    // The Secret: made before its pod, immutable, labelled with its pod,
    // holding the values, owned by the pod's uid, deleted by name.
    assert_eq!(
        api.secrets_at_pod_create(),
        vec![(pod_name.clone(), vec![secret_name(&pod_name)])]
    );
    let secrets = api.secrets_created();
    assert_eq!(secrets.len(), 1);
    let secret = &secrets[0];
    assert_eq!(secret.immutable, Some(true));
    let labels = secret.metadata.labels.clone().unwrap();
    assert_eq!(labels.get(RUN_LABEL), Some(&pod_name));
    assert_eq!(labels.get(PID_LABEL), Some(&std::process::id().to_string()));
    let data = secret.data.clone().unwrap();
    assert_eq!(data["NPM_TOKEN"].0, TOKEN.as_bytes());
    assert_eq!(data["HTTPS_PROXY"].0, PROXY.as_bytes());

    let patches = api.secret_patches();
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].0, secret_name(&pod_name));
    let owner = &patches[0].1["metadata"]["ownerReferences"][0];
    assert_eq!(owner["kind"], "Pod");
    assert_eq!(owner["apiVersion"], "v1");
    assert_eq!(owner["name"], pod_name.as_str());
    assert_eq!(
        owner["uid"].as_str(),
        pod.metadata.uid.as_deref(),
        "owned by the pod that was created"
    );
    assert!(patches[0].1.get("data").is_none());

    all_removed(&api);
    assert!(api.live().is_empty());
}

/// The kubelet reads the Secret when it creates the container, and a pod
/// that never restarts never reads it again; it is not kept for the run.
#[tokio::test(start_paused = true)]
async fn the_secret_is_gone_once_the_pod_has_started() {
    let api = KubeApi::new(NAMESPACE)
        .default_script(Script::exits(0).running(Duration::from_secs(60 * 60)));
    let state = tempfile::tempdir().unwrap();
    let runner = Arc::new(runner(&api, state.path()));
    let capture = tokio::spawn({
        let runner = runner.clone();
        async move { runner.run_capture(lent("tracon-w-long")).await }
    });
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(api.live().len(), 1, "the run is still going");
    assert!(api.secrets_live().is_empty(), "{:?}", api.secrets_live());
    assert_eq!(api.secrets_deleted().len(), 1);
    capture.abort();
}

#[tokio::test(start_paused = true)]
async fn a_failing_run_removes_its_secret() {
    let api = KubeApi::new(NAMESPACE).default_script(
        Script::exits(1)
            .running(Duration::from_secs(3))
            .logs(&format!("npm ERR! 401 with {TOKEN}\n")),
    );
    let state = tempfile::tempdir().unwrap();
    let runner = runner(&api, state.path());
    let out = runner.run_capture(lent("tracon-w-fail")).await.unwrap();
    assert_eq!(out.status.code(), Some(1));
    all_removed(&api);
}

#[tokio::test(start_paused = true)]
async fn a_pod_that_never_starts_takes_its_secret_with_it() {
    let api = KubeApi::new(NAMESPACE).default_script(Script::never_starts("ImagePullBackOff"));
    let state = tempfile::tempdir().unwrap();
    let runner = runner(&api, state.path());
    let error = runner
        .run_capture(lent("tracon-w-pull"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("ImagePullBackOff"), "{error}");
    assert!(!error.contains(TOKEN));
    all_removed(&api);

    // The same for a session's harness: a spawn that fails to start.
    let error = match runner.spawn(lent("tracon-h-s1")).await {
        Ok(_) => panic!("a pod that cannot pull does not start"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("ImagePullBackOff"), "{error}");
    assert_eq!(api.secrets_created().len(), 2);
    all_removed(&api);
}

/// Killed while the image is still pulling: the Secret has not been read
/// yet and is still there, and the kill takes it with the pod.
#[tokio::test(start_paused = true)]
async fn a_killed_run_removes_its_secret() {
    let api = KubeApi::new(NAMESPACE).default_script(
        Script::exits(0)
            .pending(Duration::from_secs(10 * 60))
            .running(Duration::from_secs(60)),
    );
    let state = tempfile::tempdir().unwrap();
    let runner = Arc::new(runner(&api, state.path()));
    let capture = tokio::spawn({
        let runner = runner.clone();
        async move { runner.run_capture(lent("tracon-w-kill")).await }
    });
    tokio::time::sleep(Duration::from_secs(30)).await;
    let name = runner.capture_name("tracon-w-kill");
    assert_eq!(api.secrets_live().len(), 1, "unread while pending");

    runner.kill(&name).await.unwrap();
    assert!(api.live().is_empty());
    assert!(api.secrets_live().is_empty());
    assert!(capture.await.unwrap().is_err());
    all_removed(&api);
}

/// A caller that stops waiting (its own deadline, a cancellation) drops the
/// run before it has started; the Secret goes as the pod does.
#[tokio::test(start_paused = true)]
async fn a_dropped_run_removes_its_secret() {
    let api = KubeApi::new(NAMESPACE).default_script(
        Script::exits(0)
            .pending(Duration::from_secs(10 * 60))
            .running(Duration::from_secs(60)),
    );
    let state = tempfile::tempdir().unwrap();
    let runner = runner(&api, state.path());
    let timed_out = tokio::time::timeout(
        Duration::from_secs(30),
        runner.run_capture(lent("tracon-w-dropped")),
    )
    .await;
    assert!(timed_out.is_err());
    settle(&api).await;
    assert!(api.live().is_empty(), "left behind: {:?}", api.live());
    all_removed(&api);
}

/// A node whose rights were never confirmed, or found missing, refuses a run
/// carrying credentials before it creates anything, and says what to grant.
/// A run carrying none is unaffected.
#[tokio::test(start_paused = true)]
async fn without_the_rights_a_run_lent_credentials_is_refused() {
    let api = KubeApi::new(NAMESPACE)
        .default_script(Script::exits(0))
        .deny("secrets", "patch");
    let state = tempfile::tempdir().unwrap();

    // Never checked.
    let unchecked = runner_with(&api, state.path(), SecretRights::default());
    let error = unchecked
        .run_capture(lent("tracon-w-unchecked"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("refusing a run that carries credentials"),
        "{error}"
    );
    assert!(error.contains("role.yaml"), "{error}");
    assert!(!error.contains(TOKEN));

    // Checked, and one verb missing.
    let verdict = checks::secret_rights(api.client(), NAMESPACE).await;
    assert_eq!(
        verdict.clone().unwrap_err(),
        format!("the service account may not patch secrets in {NAMESPACE}")
    );
    let rights = SecretRights::default();
    rights.set(verdict);
    let refused = runner_with(&api, state.path(), rights);
    let error = refused
        .run_capture(lent("tracon-w-refused"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("may not patch secrets"), "{error}");

    assert!(api.created().is_empty(), "no pod was made");
    assert!(api.secrets_created().is_empty(), "no Secret was made");

    // Nothing lent, nothing refused.
    let out = refused
        .run_capture(RunnerCommand {
            argv: vec!["true".into()],
            name: "tracon-c-plain".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(out.status.success());

    // All granted.
    let open = KubeApi::new(NAMESPACE);
    assert_eq!(
        checks::secret_rights(open.client(), NAMESPACE).await,
        Ok(())
    );
}

/// Rights confirmed at the check and withdrawn since: the create is refused
/// by the API, the run with it, and the next run is refused before it asks.
#[tokio::test(start_paused = true)]
async fn rights_withdrawn_after_the_check_still_refuse() {
    let api = KubeApi::new(NAMESPACE)
        .default_script(Script::exits(0))
        .forbid_secrets();
    let state = tempfile::tempdir().unwrap();
    let rights = SecretRights::granted();
    let runner = runner_with(&api, state.path(), rights.clone());
    let error = runner
        .run_capture(lent("tracon-w-withdrawn"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("may not create secrets"), "{error}");
    assert!(api.created().is_empty(), "no pod was made");
    assert!(!rights.is_granted());
}

/// What a node that died left: a Secret owned by no pod yet (it died in
/// between) or by one that has started. A Secret of a pod still waiting to
/// start, one made a moment ago by a node sharing the namespace, and one
/// that is not a run's are left alone.
#[tokio::test(start_paused = true)]
async fn reconcile_sweeps_orphaned_run_secrets() {
    let api = KubeApi::new(NAMESPACE);
    let old = k8s_openapi::jiff::Timestamp::now()
        .checked_sub(k8s_openapi::jiff::SignedDuration::from_secs(10 * 60))
        .unwrap();
    let secret = |name: &str, run: Option<&str>, created: Option<Time>| Secret {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some(NAMESPACE.into()),
            labels: run.map(|pod| BTreeMap::from([(RUN_LABEL.to_string(), pod.to_string())])),
            creation_timestamp: created,
            ..Default::default()
        },
        immutable: Some(true),
        ..Default::default()
    };
    let pod = |name: &str| Pod {
        metadata: ObjectMeta {
            name: Some(name.into()),
            ..Default::default()
        },
        ..Default::default()
    };
    api.insert_secret(secret(
        "tracon-w-gone-1-env",
        Some("tracon-w-gone-1"),
        Some(Time(old)),
    ));
    api.insert_secret(secret(
        "tracon-w-new-1-env",
        Some("tracon-w-new-1"),
        Some(Time(k8s_openapi::jiff::Timestamp::now())),
    ));
    api.insert_pod(
        pod("tracon-w-pulling-1"),
        Script::never_starts("ContainerCreating"),
    );
    api.insert_secret(secret(
        "tracon-w-pulling-1-env",
        Some("tracon-w-pulling-1"),
        Some(Time(old)),
    ));
    api.insert_pod(
        pod("tracon-w-running-1"),
        Script::exits(0).running(Duration::from_secs(60 * 60)),
    );
    api.insert_secret(secret(
        "tracon-w-running-1-env",
        Some("tracon-w-running-1"),
        Some(Time(old)),
    ));
    api.insert_secret(secret("someone-elses", None, Some(Time(old))));

    let state = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.runtime.kubernetes.state_mount = state.path().to_path_buf();
    let backend = KubeBackend::with_client(&cfg, pod_env(), api.client());
    backend.secret_rights().set(Ok(()));
    backend.reconcile(&[]).await;

    let mut deleted = api.secrets_deleted();
    deleted.sort();
    assert_eq!(
        deleted,
        vec![
            "tracon-w-gone-1-env".to_string(),
            "tracon-w-running-1-env".to_string()
        ]
    );
    let live: Vec<_> = api
        .secrets_live()
        .into_iter()
        .filter_map(|s| s.metadata.name)
        .collect();
    assert_eq!(
        live,
        vec![
            "someone-elses".to_string(),
            "tracon-w-new-1-env".to_string(),
            "tracon-w-pulling-1-env".to_string()
        ]
    );

    // A session reconciled by name takes its Secret with its pod.
    api.insert_pod(
        pod("tracon-h-s9"),
        Script::never_starts("ContainerCreating"),
    );
    api.insert_secret(secret(
        "tracon-h-s9-env",
        Some("tracon-h-s9"),
        Some(Time(k8s_openapi::jiff::Timestamp::now())),
    ));
    backend.reconcile(&["tracon-h-s9".to_string()]).await;
    assert!(!api.live().contains(&"tracon-h-s9".to_string()));
    assert!(api
        .secrets_deleted()
        .contains(&"tracon-h-s9-env".to_string()));
}
