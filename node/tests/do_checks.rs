//! A review's checks on DigitalOcean, minus DigitalOcean: the stand-in for
//! its API (`support::do_api`) whose `exec` plays each sandbox's template
//! service by starting `tracon-sandbox`'s own loop, dialling the node's
//! listener with the bootstrap it was delivered.
//!
//! Prepared once in one sandbox with the preparation's egress grant, the
//! grant revoked before anything is forked, then one child forked per
//! check, four to a fork, each with a run and token of its own and no
//! grant; evidence recorded as derived, and reused from there; a template
//! waited for, or not ready in time; checks whose image has no template go
//! back to the node.
//!
//! The stand-in's sandboxes share this host's filesystem, so a child "holds"
//! what its parent prepared because the directory is still there, not
//! because anything was forked: what this proves is the node's side.
//!
//! The loop reaps every child of its process with `waitpid(-1)`, and the
//! children of one prepared sandbox are alive at once, so each stand-in
//! sandbox is a process of its own: this test binary run again as
//! [`sandbox_process`], told its bootstrap in its environment.

#![cfg(target_os = "linux")]

#[path = "support/mod.rs"]
mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tracon::boundary::{Backend, BoundaryError, BoundaryReport, EgressGrant};
use tracon::config::{Config, HarnessListen};
use tracon::environment::RepoEnvironment;
use tracon::gateway::proxy::GrantSpec;
use tracon::provider::capabilities::Route;
use tracon::provider::digitalocean::bootstrap::Delivery;
use tracon::provider::digitalocean::listener::DoListener;
use tracon::provider::digitalocean::runtime::DoRuntime;
use tracon::provider::digitalocean::templates::Source;
use tracon::provider::digitalocean::{ClientSource, HubRelay, Owner, Settings};
use tracon::provider::{ImageIdentity, ProviderKind, Requirements, SandboxProvider};
use tracon::review::checks::RunToCompletion;
use tracon::review::forked::{run_forked, Forked, Forks};
use tracon::runner::sandbox_files::FilesStore;
use tracon::runner::{Mount, RunKind, RunLabels, Runner, RunnerCommand, RunnerError, Spawned};
use tracon::sandbox_images::Making;
use tracon::store::{CandidateRow, Store};
use tracon_sandbox::carrier::Dial;
use tracon_sandbox::inside;
use tracon_sandbox::tls::CertPin;

use support::do_api::FakeDo;

const DIGEST: &str = "sha256:3333333333333333444444444444444444444444444444444444444444444444";

fn image() -> String {
    format!("ghcr.io/example/tracon-snapshot-app@{DIGEST}")
}

/// A stand-in sandbox: this test binary, run as [`sandbox_process`].
type Sandbox = std::process::Child;

const DIAL: &str = "TRACON_TEST_DO_SANDBOX_DIAL";
const PIN: &str = "TRACON_TEST_DO_SANDBOX_PIN";
const TOKEN: &str = "TRACON_TEST_DO_SANDBOX_TOKEN";
const SERVER_NAME: &str = "TRACON_TEST_DO_SANDBOX_SERVER_NAME";

fn sandbox(dial: &str, pin: &str, token: &str, server_name: &str) -> Sandbox {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["sandbox_process", "--exact", "--nocapture"])
        .env(DIAL, dial)
        .env(PIN, pin)
        .env(TOKEN, token)
        .env(SERVER_NAME, server_name)
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

/// One stand-in sandbox's loop, when this binary was started as one;
/// nothing otherwise.
#[test]
fn sandbox_process() {
    let Ok(dial) = std::env::var(DIAL) else {
        return;
    };
    let mut config = inside::Config::new(
        Dial::Tcp(dial),
        CertPin::parse(&std::env::var(PIN).unwrap()).unwrap(),
        std::env::var(TOKEN).unwrap(),
    );
    config.server_name = std::env::var(SERVER_NAME).unwrap();
    config.connect_timeout = Duration::from_secs(5);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ended = runtime.block_on(inside::run(config));
    runtime.shutdown_timeout(Duration::from_secs(1));
    if let Err(error) = ended {
        panic!("{error}");
    }
}

/// Wait for a stand-in sandbox to end; whether it ended well.
async fn join(mut child: Sandbox) -> bool {
    tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || child.wait().unwrap().success()),
    )
    .await
    .expect("a sandbox did not end")
    .unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct NoRunner;

#[async_trait]
impl Runner for NoRunner {
    async fn spawn(&self, _cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        Err(RunnerError::Other("nothing runs on the node here".into()))
    }
    async fn run_capture(&self, _cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        Err(RunnerError::Other("nothing runs on the node here".into()))
    }
    async fn kill(&self, _name: &str) -> Result<(), RunnerError> {
        Ok(())
    }
}

/// Each egress grant opened: the hosts it was for, and how many forks had
/// been made when it was revoked.
type Grants = Arc<Mutex<Vec<(Vec<String>, Option<usize>)>>>;

/// The node's own runtime, as far as checks placed elsewhere touch it: the
/// egress grant their preparation is given, which says when it is revoked
/// and whether anything had been forked by then.
struct Node {
    fake: FakeDo,
    grants: Grants,
}

#[async_trait]
impl Backend for Node {
    fn kind(&self) -> &'static str {
        "podman"
    }
    async fn setup(&self, _cfg: &Config, _rebuild: bool) -> Result<(), BoundaryError> {
        Ok(())
    }
    async fn check_all(&self, _cfg: &Config, _deep: bool) -> BoundaryReport {
        BoundaryReport { checks: Vec::new() }
    }
    fn runner(&self, _extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        Arc::new(NoRunner)
    }
    fn runner_for(&self, _harness_id: &str, _extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        Arc::new(NoRunner)
    }
    fn harness_host(&self) -> String {
        "tracon-gw".into()
    }
    async fn import_volume(&self, _volume: &str, _source: &Path) -> Result<(), BoundaryError> {
        Ok(())
    }
    async fn export_volume(&self, _volume: &str, _destination: &Path) -> Result<(), BoundaryError> {
        Ok(())
    }
    fn harness_home(&self) -> String {
        "/home/harness".into()
    }
    async fn reconcile(&self, _names: &[String]) {}
    fn egress_grant(&self, spec: GrantSpec) -> Result<EgressGrant, BoundaryError> {
        let at = {
            let mut grants = self.grants.lock().unwrap();
            grants.push((spec.hosts.clone(), None));
            grants.len() - 1
        };
        let (grants, api) = (self.grants.clone(), self.fake.api.clone());
        Ok(EgressGrant::new(
            "http://tracon-gw:8890".into(),
            spec.client,
            "grant-secret".into(),
            move || {
                let forks = api.lock().unwrap().forks.len();
                grants.lock().unwrap()[at].1 = Some(forks);
            },
        ))
    }
}

fn candidate(store: &Store, head: &str) -> CandidateRow {
    let row = CandidateRow {
        id: format!("cand-{head}"),
        head_sha: head.into(),
        tree_sha: Some(format!("tree-{head}")),
        channel: "personal".into(),
        owner_session_id: None,
        source_kind: "git".into(),
        captured_ms: 1,
        capture_json: "{}".into(),
    };
    store.insert_candidate(&row).unwrap();
    row
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_are_prepared_once_and_forked_per_check() {
    support::state::isolate();
    let fake = FakeDo::start().await;
    let server = tracon::runner::protocol::node_server().unwrap();
    let listener = DoListener::bind("127.0.0.1:0".parse().unwrap(), server.clone())
        .await
        .unwrap();
    let node = listener.local_addr();
    let mut cfg = Config::default();
    cfg.gateway.forward_port = free_port();
    cfg.gateway.proxy_port = free_port();
    cfg.gateway.qa_proxy_port = 0;
    cfg.gateway.harness_listen = HarnessListen::Tcp("127.0.0.1:9".parse().unwrap());
    let settings = Settings {
        credential: "do".into(),
        api_url: fake.url.clone(),
        image: image(),
        listen: node.to_string(),
        node_addr: node.to_string(),
        limit: 8,
        ..Settings::default()
    };
    let client: ClientSource = {
        let fake = fake.clone();
        Arc::new(move || Ok(fake.client()))
    };
    let runtime = DoRuntime::with(
        client,
        settings,
        Source::Given(image()),
        Owner::new("tracon", "abcd1234"),
        "tracon-gw".into(),
        listener.clone(),
        HubRelay::none(),
    )
    .unwrap();
    let provider = runtime.provider().clone();
    provider.expect_uid(Some(tracon_sandbox::inside::user::current().uid));

    // Each bootstrap: the session it was delivered to, and its token.
    let delivered: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let threads: Arc<Mutex<Vec<Sandbox>>> = Arc::default();
    fake.on_exec({
        let (delivered, threads) = (delivered.clone(), threads.clone());
        Arc::new(move |session: &str, argv: &[String]| {
            let Some(d) = Delivery::parse(argv) else {
                return 2;
            };
            delivered
                .lock()
                .unwrap()
                .push((session.to_string(), d.token.to_string()));
            threads
                .lock()
                .unwrap()
                .push(sandbox(d.dial.value(), d.pin, d.token, d.server_name));
            0
        })
    });

    // The template is waited for while it builds, and past the wait the
    // checks are not placed; an image this node makes none for is refused
    // at once.
    fake.api.lock().unwrap().template_builds = 1_000;
    let late = runtime
        .check_template(
            &image(),
            Duration::from_millis(50),
            Duration::from_millis(10),
            &RunToCompletion,
        )
        .await
        .unwrap_err();
    assert!(late.contains("was not ready within"), "{late}");
    fake.api.lock().unwrap().template_builds = 1;
    let template = runtime
        .check_template(
            &image(),
            Duration::from_secs(30),
            Duration::from_millis(10),
            &RunToCompletion,
        )
        .await
        .unwrap();
    let other = format!("ghcr.io/example/other@sha256:{}", "5".repeat(64));
    let refused = runtime
        .check_template(
            &other,
            Duration::from_secs(30),
            Duration::from_millis(10),
            &RunToCompletion,
        )
        .await
        .unwrap_err();
    assert!(refused.contains("makes no template"), "{refused}");

    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("inside/work");
    let tree = tmp.path().join("candidate");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("README"), "the candidate\n").unwrap();
    let store = Store::open_in_memory().unwrap();
    let backend = Node {
        fake: fake.clone(),
        grants: Arc::default(),
    };
    let forks = Forks {
        kind: ProviderKind::DigitalOcean,
        provider: provider.clone() as Arc<dyn SandboxProvider>,
        server: server.clone(),
        files: FilesStore::new(tmp.path().join("store")),
        tunnels: Vec::new(),
        requirements: Requirements {
            route: Some(vec![Route::NodeOnly]),
            ..Requirements::default()
        },
        making: Making {
            kind: RunKind::Check,
            labels: RunLabels::of(RunKind::Check),
            resources: Default::default(),
            token_window: Some(Duration::from_secs(300)),
        },
        workdir: work.to_string_lossy().into_owned(),
    };
    // Four that pass, reading what preparation left and their own proxy,
    // and one that fails: five children, in a fork of four and one of one.
    let mut checks: Vec<String> = (0..4)
        .map(|n| format!("cat marker README && echo check{n} proxy=${{HTTPS_PROXY:-none}}"))
        .collect();
    checks.push("echo it broke; exit 3".into());
    let environment = RepoEnvironment {
        image: image(),
        image_source: "prepared snapshot",
        checks,
        warm: None,
        timeout_secs: 60,
        prepare: vec!["echo prepared > marker; printf '%s' \"$HTTPS_PROXY\" > proxy-seen".into()],
        egress: vec!["registry.example".into()],
        session_egress: Vec::new(),
        cache_volume: "tracon-cache-base-app".into(),
        repo: Some("app".into()),
    };
    let first = candidate(&store, "sha1");
    let forked = run_forked(
        &backend,
        &cfg,
        &store,
        &first,
        Some("s1"),
        &environment,
        &tree,
        false,
        &RunToCompletion,
        &forks,
        None,
    )
    .await;
    let Forked::Ran(report) = forked else {
        panic!("not placed on DigitalOcean")
    };
    let report = (*report).unwrap();
    let ended = std::mem::take(&mut *threads.lock().unwrap());
    for thread in ended {
        join(thread).await;
    }

    let outcomes: Vec<&str> = report.results.iter().map(|r| r.outcome.as_str()).collect();
    assert_eq!(
        outcomes,
        ["passed", "passed", "passed", "passed", "failed"],
        "{report:?}"
    );
    assert!(!report.all_required_passed);
    assert!(report.prepared.as_ref().unwrap().ok, "{report:?}");
    for (n, result) in report.results[..4].iter().enumerate() {
        // What preparation left, the candidate's tree, and no proxy at all.
        assert!(result.tail.contains("prepared"), "{}", result.tail);
        assert!(result.tail.contains("the candidate"), "{}", result.tail);
        assert!(
            result.tail.contains(&format!("check{n} proxy=none")),
            "{}",
            result.tail
        );
    }
    // Derived from the image, as the provider confirmed it.
    let elsewhere = report.elsewhere.clone().unwrap();
    assert_eq!(elsewhere.provider, ProviderKind::DigitalOcean);
    assert_eq!(elsewhere.forks, 5);
    assert_eq!(
        elsewhere.identity,
        ImageIdentity::Derived {
            from: DIGEST.into(),
            actual: format!("digitalocean-template:{template}"),
        }
    );

    // Preparation had its grant, and only it: revoked before any fork.
    assert_eq!(
        std::fs::read_to_string(work.join("proxy-seen")).unwrap(),
        "http://prepare:grant-secret@tracon-gw:8890"
    );
    assert_eq!(
        *backend.grants.lock().unwrap(),
        [(vec!["registry.example".to_string()], Some(0))]
    );

    // One sandbox made from the template, forked from its checkpoint in
    // batches of at most four.
    let (manifests, forks_made, deleted_checkpoints, sessions_left) = {
        let api = fake.api.lock().unwrap();
        (
            api.manifests.clone(),
            api.forks.clone(),
            api.deleted_checkpoints.clone(),
            api.sessions.len(),
        )
    };
    assert_eq!(manifests.len(), 1, "{manifests:?}");
    assert_eq!(manifests[0]["template"], template);
    let parent = forks_made[0].0.clone();
    let counts: Vec<u64> = forks_made
        .iter()
        .map(|(session, body)| {
            assert_eq!(*session, parent);
            assert!(body["from_checkpoint_id"].is_string(), "{body}");
            body["count"].as_u64().unwrap()
        })
        .collect();
    assert_eq!(counts, [4, 1]);
    // Every sandbox gone, and the checkpoint with them.
    assert_eq!(sessions_left, 0);
    assert_eq!(deleted_checkpoints.len(), 1);

    // Every child its own bootstrap: a token of its own, never its parent's
    // or a sibling's.
    let delivered = delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 6, "{delivered:?}");
    assert_eq!(delivered[0].0, parent);
    let tokens: std::collections::HashSet<&str> =
        delivered.iter().map(|(_, token)| token.as_str()).collect();
    assert_eq!(tokens.len(), 6, "a token was delivered twice");
    let children: std::collections::HashSet<&str> =
        delivered[1..].iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(children.len(), 5);
    assert!(!children.contains(parent.as_str()));
    // A child that came back with its parent's token gets nowhere: it was
    // spent, and the parent's run ended before anything was forked.
    let replayed = sandbox(
        &node.to_string(),
        &server.pin().to_string(),
        &delivered[0].1,
        "tracon-node",
    );
    assert!(!join(replayed).await, "the parent's token was taken again");

    // Each check's record: derived, by DigitalOcean.
    let runs = store.check_runs_for_candidate(&first.id).unwrap();
    assert_eq!(runs.len(), 5);
    for run in &runs {
        let metadata: serde_json::Value = serde_json::from_str(&run.metadata_json).unwrap();
        assert_eq!(metadata["execution_backend"], "digitalocean", "{metadata}");
        assert_eq!(metadata["image_identity"]["kind"], "derived", "{metadata}");
        assert_eq!(metadata["image_identity"]["from"], DIGEST, "{metadata}");
        assert!(metadata.get("cache_volume").is_none(), "{metadata}");
        assert_eq!(run.execution_image.as_deref(), Some(image().as_str()));
    }

    // Submitted again: every result reused from there, nothing made.
    let again = run_forked(
        &backend,
        &cfg,
        &store,
        &first,
        Some("s1"),
        &environment,
        &tree,
        false,
        &RunToCompletion,
        &forks,
        None,
    )
    .await;
    let Forked::Ran(again) = again else {
        panic!("not placed")
    };
    let again = (*again).unwrap();
    assert!(again.reused);
    assert!(
        again.results.iter().all(|r| r.outcome == "reused"),
        "{again:?}"
    );
    assert_eq!(fake.api.lock().unwrap().manifests.len(), 1);

    // What a harness asks to run (`check_run`) runs there too, prepared the
    // same way, but neither reuses what is recorded for the candidate nor is
    // recorded on it: its rows name no candidate.
    let recorded = store.check_runs_for_candidate(&first.id).unwrap().len();
    let asked = vec!["cat marker README".to_string()];
    let ad_hoc = run_forked(
        &backend,
        &cfg,
        &store,
        &first,
        None,
        &environment,
        &tree,
        false,
        &RunToCompletion,
        &forks,
        Some(&asked),
    )
    .await;
    let Forked::Ran(ad_hoc) = ad_hoc else {
        panic!("not placed")
    };
    let ad_hoc = (*ad_hoc).unwrap();
    let ended = std::mem::take(&mut *threads.lock().unwrap());
    for thread in ended {
        join(thread).await;
    }
    assert!(!ad_hoc.reused, "{ad_hoc:?}");
    assert_eq!(ad_hoc.results.len(), 1, "{ad_hoc:?}");
    assert_eq!(ad_hoc.results[0].outcome, "passed", "{ad_hoc:?}");
    assert!(ad_hoc.results[0].tail.contains("prepared"), "{ad_hoc:?}");
    assert_eq!(fake.api.lock().unwrap().manifests.len(), 2);
    assert_eq!(
        store.check_runs_for_candidate(&first.id).unwrap().len(),
        recorded
    );

    // An image this provider has no template for: back to the node, with
    // nothing recorded.
    let second = candidate(&store, "sha2");
    let elsewhere = RepoEnvironment {
        image: other.clone(),
        ..environment.clone()
    };
    let unplaced = run_forked(
        &backend,
        &cfg,
        &store,
        &second,
        Some("s1"),
        &elsewhere,
        &tree,
        false,
        &RunToCompletion,
        &forks,
        None,
    )
    .await;
    let Forked::Unplaced(why) = unplaced else {
        panic!("placed with no template")
    };
    assert!(why.contains("cannot start a sandbox"), "{why}");
    assert!(store
        .check_runs_for_candidate(&second.id)
        .unwrap()
        .is_empty());
    let ended = std::mem::take(&mut *threads.lock().unwrap());
    for thread in ended {
        join(thread).await;
    }
}
