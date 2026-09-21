//! The command QA target kind: deploying a candidate through an argv the node
//! runs itself, with a brokered credential.
//!
//! Everything here runs against a fake `cloud` that records what it was given.
//! That is the point: the claims are about *what the node passes* — the
//! credential as environment and never as an argument, the candidate's own SHA
//! and published branch substituted into the argv, the host's answer read
//! rather than assumed — and only a stub that writes down its own invocation
//! can witness them. A test against the real CLI would prove the CLI works.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};
use serde_json::{json, Value};
use tracon::{
    broker::{Broker, Credential},
    config::{
        Config, Qa, QaBrowser, QaDeployment, QaDiscover, QaStatusCommand, QaTarget, QA_KIND_COMMAND,
    },
    mcp::Tools,
    qa::{service::QaAccess, DeployRequest},
    session::Manager,
    store::{now_ms, AuthorityGrantRow, CandidateRow, PublicationBegin, Store},
    stream::Bus,
};

const CHANNEL: &str = "work";
const SESSION: &str = "s1";
const NODE: &str = "n1";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const BRANCH: &str = "feat/qa-thing";
const TOKEN: &str = "cloud-token-that-must-never-be-seen";
const BROWSER_IMAGE: &str = "example.invalid/browser@sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// A fake `cloud` that writes down every invocation and answers from files the
/// test can rewrite between polls.
struct FakeCli {
    dir: PathBuf,
    path: String,
}

impl FakeCli {
    fn new(name: &str) -> Self {
        let dir = state::scratch(name);
        let path = dir.join("cloud");
        let script = format!(
            r#"#!/bin/sh
rec='{dir}'
printf '%s\n' "$*" >> "$rec/argv.log"
env >> "$rec/env.log"
case "$1" in
  --version) echo "Fake Cloud CLI 9.9.9"; exit 0 ;;
  environment:list) cat "$rec/environments.json"; exit $(cat "$rec/environments.exit" 2>/dev/null || echo 0) ;;
  deployment:list) cat "$rec/deployments.json"; exit 0 ;;
  deploy) cat "$rec/deploy.out" 2>/dev/null; exit $(cat "$rec/deploy.exit" 2>/dev/null || echo 0) ;;
esac
echo "fake cloud: unknown command $1" >&2
exit 64
"#,
            dir = dir.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            path: path.to_string_lossy().into_owned(),
            dir,
        }
    }

    fn write(&self, file: &str, contents: &str) {
        std::fs::write(self.dir.join(file), contents).unwrap();
    }

    fn environments(&self, value: Value) {
        self.write("environments.json", &value.to_string());
    }

    fn deployments(&self, value: Value) {
        self.write("deployments.json", &value.to_string());
    }

    fn argv_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("argv.log")).unwrap_or_default()
    }

    fn env_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("env.log")).unwrap_or_default()
    }
}

/// A loopback origin standing in for the deployed application, answering the
/// identity endpoint with whatever build id the test wants it to claim.
async fn fake_origin(identity: Option<String>) -> String {
    let app = Router::new()
        .route(
            "/_tracon/identity",
            get(|State(identity): State<Option<String>>| async move {
                match identity {
                    Some(value) => {
                        (StatusCode::OK, [("x-tracon-deployment-id", value)]).into_response()
                    }
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        )
        .with_state(identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{}", addr.port())
}

fn discovery_target(cli: &FakeCli, with_deploy_command: bool) -> QaTarget {
    QaTarget {
        r#private: true,
        origin: String::new(),
        identity_url: String::new(),
        identity_path: "/_tracon/identity".into(),
        identity_header: "x-tracon-deployment-id".into(),
        allowed_origins: Vec::new(),
        deployment: QaDeployment {
            kind: QA_KIND_COMMAND.into(),
            command: match with_deploy_command {
                true => vec![
                    cli.path.clone(),
                    "deploy".into(),
                    "{app}".into(),
                    "{branch}".into(),
                    "-n".into(),
                ],
                false => Vec::new(),
            },
            env_credential: "laravel-cloud".into(),
            args: BTreeMap::from([("app".to_string(), "my-app".to_string())]),
            discover: Some(QaDiscover {
                command: vec![
                    cli.path.clone(),
                    "environment:list".into(),
                    "{app}".into(),
                    "--json".into(),
                    "-n".into(),
                ],
                branch_field: "branch".into(),
                id_field: "id".into(),
                url_field: "url".into(),
                state_field: "status".into(),
                prefer_field: "createdFromAutomation".into(),
                ready_states: vec!["running".into()],
                origin_suffix: "127.0.0.1".into(),
                ..QaDiscover::default()
            }),
            status: Some(QaStatusCommand {
                command: vec![
                    cli.path.clone(),
                    "deployment:list".into(),
                    "{env_id}".into(),
                    "--json".into(),
                    "-n".into(),
                ],
                id_field: "id".into(),
                state_field: "status".into(),
                commit_field: "commitHash".into(),
                ready_states: vec!["success".into()],
                failed_states: vec!["failed".into(), "cancelled".into()],
                ..QaStatusCommand::default()
            }),
            // Short, because these tests run in real time: a real subprocess
            // and tokio's paused clock do not mix, and pretending otherwise
            // would make the timeout test prove nothing.
            deploy_timeout_secs: 20,
            poll_interval_secs: 1,
            identity_matches_candidate: true,
            ..QaDeployment::default()
        },
        browser: QaBrowser {
            image: BROWSER_IMAGE.into(),
            test_credential: None,
            timeout_secs: 60,
        },
    }
}

struct Fixture {
    store: Arc<Store>,
    manager: Manager,
    cfg: Arc<Config>,
    broker: tracon::broker::SharedBroker,
    policy: parking_lot::RwLock<tracon::policy::Policy>,
    http: reqwest::Client,
    candidate_id: String,
}

impl Fixture {
    fn access(&self) -> QaAccess<'_> {
        QaAccess {
            store: &self.store,
            manager: &self.manager,
            cfg: &self.cfg,
            broker: &self.broker,
            http: &self.http,
            policy: &self.policy,
            node_id: NODE,
            requester_session_id: Some(SESSION),
            requester_channel: Some(CHANNEL),
        }
    }

    async fn deploy(&self) -> Result<tracon::store::QaDeploymentRow, String> {
        tracon::qa::service::deploy(
            &self.access(),
            DeployRequest {
                candidate_id: self.candidate_id.clone(),
                target: "cloud-qa".into(),
            },
        )
        .await
    }
}

/// A node with one candidate, one publication that pushed its exact SHA, one
/// brokered credential, and one active deploy grant.
fn fixture(target: QaTarget, published: bool, granted: bool) -> Fixture {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let candidate_id = format!("cand-{SHA}");
    store
        .insert_candidate(&CandidateRow {
            id: candidate_id.clone(),
            head_sha: SHA.into(),
            tree_sha: Some("t1".into()),
            channel: CHANNEL.into(),
            owner_session_id: SESSION.into(),
            source_kind: "worktree".into(),
            captured_ms: now_ms(),
            capture_json: "{}".into(),
        })
        .unwrap();
    if published {
        let row = store
            .publication_begin(&PublicationBegin {
                id: "pub-1",
                review_id: "rev-1",
                revision_id: None,
                candidate_id: &candidate_id,
                channel: CHANNEL,
                node_id: NODE,
                provider: "github",
                project: "example/my-app",
                base: "main",
                branch: BRANCH,
                head_sha: SHA,
                instance: "i1",
            })
            .unwrap();
        store.publication_pushed(&row.id, SHA).unwrap();
        store
            .publication_opened(&row.id, "https://github.invalid/pr/1")
            .unwrap();
    }
    if granted {
        store
            .authority_grant_insert(&AuthorityGrantRow {
                id: "g1".into(),
                action: "deploy".into(),
                verdict: "allow".into(),
                target: tracon::qa::deploy_authority_target("cloud-qa", &target),
                channel: CHANNEL.into(),
                session_id: Some(SESSION.into()),
                revision: Some(SHA.into()),
                expires_ms: None,
                revoked_ms: None,
                reason: "test".into(),
                created_ms: now_ms(),
            })
            .unwrap();
    }
    let cfg = Arc::new(Config {
        qa: Qa {
            targets: BTreeMap::from([("cloud-qa".to_string(), target)]),
            prototype: None,
        },
        ..Config::default()
    });
    let mut broker = Broker::default();
    broker.put(
        "laravel-cloud",
        Credential {
            env: BTreeMap::from([("CLOUD_API_TOKEN".to_string(), TOKEN.to_string())]),
            channels: vec![CHANNEL.into()],
            nodes: vec![NODE.into()],
            ..Credential::default()
        },
    );
    let broker = broker.shared();
    let tools = Arc::new(Tools {
        broker: broker.clone(),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg.clone(),
        NODE.into(),
        tools,
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    Fixture {
        store,
        manager,
        cfg,
        broker,
        policy: parking_lot::RwLock::new(tracon::policy::Policy::shipped()),
        http: reqwest::Client::new(),
        candidate_id,
    }
}

fn environments(url: &str, branch: &str, status: &str) -> Value {
    json!([
        { "id": "env-hand", "url": url, "branch": branch, "status": status, "createdFromAutomation": false },
        { "id": "env-auto", "url": url, "branch": branch, "status": status, "createdFromAutomation": true },
        { "id": "env-main", "url": "https://example.invalid", "branch": "main", "status": "running", "createdFromAutomation": true },
    ])
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

fn validating(target: QaTarget) -> Result<(), String> {
    Qa {
        targets: BTreeMap::from([("cloud-qa".to_string(), target)]),
        prototype: None,
    }
    .validate()
}

fn plain_command_target() -> QaTarget {
    QaTarget {
        r#private: true,
        origin: "https://qa.example.com".into(),
        identity_url: "https://qa.example.com/_tracon/identity".into(),
        identity_header: "x-tracon-deployment-id".into(),
        deployment: QaDeployment {
            kind: QA_KIND_COMMAND.into(),
            command: vec!["cloud".into(), "deploy".into(), "app".into(), "qa".into()],
            env_credential: "laravel-cloud".into(),
            status: Some(QaStatusCommand {
                command: vec!["cloud".into(), "deployment:list".into(), "--json".into()],
                commit_field: "commitHash".into(),
                ..QaStatusCommand::default()
            }),
            ..QaDeployment::default()
        },
        browser: QaBrowser {
            image: BROWSER_IMAGE.into(),
            timeout_secs: 60,
            test_credential: None,
        },
        ..QaTarget::default()
    }
}

#[test]
fn a_command_target_is_accepted_only_with_every_invariant_the_gitlab_kind_has() {
    state::isolate();
    validating(plain_command_target()).expect("the base target is valid");

    let mut not_private = plain_command_target();
    not_private.r#private = false;
    assert!(validating(not_private)
        .unwrap_err()
        .contains("explicitly marked private"));

    let mut identity_elsewhere = plain_command_target();
    identity_elsewhere.identity_url = "https://elsewhere.example.com/id".into();
    assert!(validating(identity_elsewhere)
        .unwrap_err()
        .contains("credential-free URL on qa.origin"));

    let mut identity_with_credential = plain_command_target();
    identity_with_credential.identity_url = "https://u:p@qa.example.com/id".into();
    assert!(validating(identity_with_credential).is_err());

    let mut production = plain_command_target();
    production.deployment.args = BTreeMap::from([("env".into(), "production".into())]);
    production.deployment.command = vec!["cloud".into(), "deploy".into(), "{env}".into()];
    assert!(validating(production)
        .unwrap_err()
        .contains("names production"));

    // A target named for production is refused before the target itself is
    // even read, as it always has been.
    let named = Qa {
        targets: BTreeMap::from([("production-qa".to_string(), plain_command_target())]),
        prototype: None,
    };
    assert!(named
        .validate()
        .unwrap_err()
        .contains("must not name a production environment"));
}

#[test]
fn a_command_target_refuses_a_shell_an_unlisted_binary_a_path_and_a_credential() {
    state::isolate();
    for (argv, expected) in [
        (
            vec!["sh".to_string(), "-c".into(), "cloud deploy".into()],
            "program rather than arguments",
        ),
        (
            vec!["bash".to_string(), "deploy.sh".into()],
            "program rather than arguments",
        ),
        (
            vec!["/usr/bin/env".to_string(), "cloud".into()],
            "program rather than arguments",
        ),
        (
            vec!["curl".to_string(), "https://example.com".into()],
            "neither an allowlisted deployment binary nor an absolute path",
        ),
        (
            vec!["cloud".to_string(), "deploy".into(), "/etc/hosts".into()],
            "names a path",
        ),
        (
            vec!["cloud".to_string(), "deploy".into(), "../../etc".into()],
            "names a path",
        ),
        (
            vec![
                "cloud".to_string(),
                "deploy".into(),
                "--token=abc123".into(),
            ],
            "looks like a credential",
        ),
        (
            vec!["cloud".to_string(), "deploy".into(), "--api-key".into()],
            "looks like a credential",
        ),
        (
            vec!["cloud".to_string(), "deploy".into(), "{nope}".into()],
            "unknown placeholder",
        ),
    ] {
        let mut target = plain_command_target();
        target.deployment.command = argv.clone();
        let error = validating(target).unwrap_err();
        assert!(error.contains(expected), "{argv:?} gave {error}");
    }

    // An absolute path is the escape hatch from the allowlist, and it works.
    let mut absolute = plain_command_target();
    absolute.deployment.command = vec!["/opt/deploy/ship".into(), "qa".into()];
    validating(absolute).expect("an absolute path names exactly which file runs");
}

#[test]
fn a_command_target_needs_a_credential_a_way_to_observe_and_no_gitlab_fields() {
    state::isolate();
    let mut no_credential = plain_command_target();
    no_credential.deployment.env_credential = String::new();
    assert!(validating(no_credential)
        .unwrap_err()
        .contains("env_credential"));

    let mut blind = plain_command_target();
    blind.deployment.status = None;
    blind.deployment.discover = None;
    assert!(validating(blind)
        .unwrap_err()
        .contains("nothing observes whether the deployment finished"));

    let mut mixed = plain_command_target();
    mixed.deployment.project = "group/project".into();
    assert!(validating(mixed)
        .unwrap_err()
        .contains("belong to the gitlab kind"));

    let mut unknown_kind = plain_command_target();
    unknown_kind.deployment.kind = "helmish".into();
    assert!(validating(unknown_kind)
        .unwrap_err()
        .contains("unknown deployment kind"));
}

#[test]
fn a_discovery_target_attests_a_suffix_instead_of_an_origin_and_is_held_to_it() {
    state::isolate();
    let cli = FakeCli::new("qa-command-suffix");
    let target = discovery_target(&cli, false);
    validating(target.clone()).expect("a discovery target is valid without an origin");

    // The origin is not known until an environment exists.
    assert!(target.discovers_origin());
    assert!(target.resolved(None).is_err());

    let mut with_origin = target.clone();
    with_origin.origin = "https://qa.example.com".into();
    assert!(validating(with_origin)
        .unwrap_err()
        .contains("identity_path rather than origin"));

    let mut no_suffix = target.clone();
    no_suffix
        .deployment
        .discover
        .as_mut()
        .unwrap()
        .origin_suffix = String::new();
    assert!(validating(no_suffix).unwrap_err().contains("origin_suffix"));

    // Resolution holds a discovered URL to the attested suffix, and a
    // lookalike host is not a subdomain of it.
    let mut laravel = target.clone();
    laravel.deployment.discover.as_mut().unwrap().origin_suffix = "laravel.cloud".into();
    let resolved = laravel
        .resolved(Some("https://qa-abc.my-app.laravel.cloud/"))
        .unwrap();
    assert_eq!(resolved.origin, "https://qa-abc.my-app.laravel.cloud");
    assert_eq!(
        resolved.identity_url,
        "https://qa-abc.my-app.laravel.cloud/_tracon/identity"
    );
    for hostile in [
        "https://evil-laravel.cloud/",
        "https://laravel.cloud.evil.example/",
        "http://qa-abc.laravel.cloud/",
        "https://user:pass@qa-abc.laravel.cloud/",
    ] {
        assert!(
            laravel.resolved(Some(hostile)).is_err(),
            "{hostile} must not resolve under laravel.cloud"
        );
    }
}

// ---------------------------------------------------------------------------
// Deploy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_candidate_deploys_through_the_command_and_the_credential_never_leaves_the_environment() {
    state::isolate();
    let cli = FakeCli::new("qa-command-happy");
    let origin = fake_origin(Some(SHA.into())).await;
    cli.environments(environments(&origin, BRANCH, "running"));
    cli.deployments(json!([{ "id": "dep-9", "status": "success", "commitHash": SHA }]));
    let f = fixture(discovery_target(&cli, true), true, true);

    let row = f.deploy().await.expect("the deploy runs");
    assert_eq!(row.outcome, "succeeded");
    assert_eq!(row.candidate_id, f.candidate_id);
    assert_eq!(row.origin, origin);
    assert_eq!(row.environment_identity.as_deref(), Some(SHA));
    assert_eq!(row.identity_state, "fresh");
    assert!(
        row.execution_image.starts_with("command:sha256:"),
        "a command kind's evidence identity is a digest of the tool that ran: {}",
        row.execution_image
    );
    assert!(
        row.build_id.contains("env-auto")
            && row.build_id.contains("dep-9")
            && row.build_id.contains(SHA),
        "the build id names the environment, the host deployment and the candidate: {}",
        row.build_id
    );

    // The candidate's own SHA and published branch were substituted in, and
    // the operator's `{app}` with them.
    let argv = cli.argv_log();
    assert!(
        argv.contains(&format!("deploy my-app {BRANCH} -n")),
        "{argv}"
    );
    assert!(argv.contains("environment:list my-app --json -n"), "{argv}");
    assert!(
        argv.contains("deployment:list env-auto --json -n"),
        "{argv}"
    );

    // The credential reached the command as environment, and nowhere else.
    let env = cli.env_log();
    assert!(
        env.contains(&format!("CLOUD_API_TOKEN={TOKEN}")),
        "the command needs the credential"
    );
    assert!(env.contains(&format!("TRACON_CANDIDATE_SHA={SHA}")));
    assert!(env.contains(&format!("TRACON_CANDIDATE_BRANCH={BRANCH}")));
    assert!(env.contains("TRACON_QA_TARGET=cloud-qa"));
    assert!(
        !argv.contains(TOKEN),
        "the credential must never be an argument: {argv}"
    );
    assert!(
        !row.detail_json.contains(TOKEN),
        "the credential must never reach durable evidence"
    );
    let detail: Value = serde_json::from_str(&row.detail_json).unwrap();
    assert_eq!(detail["transport"], "command");
    assert_eq!(detail["branch"], BRANCH);
    assert_eq!(detail["environment"]["id"], "env-auto");
    assert_eq!(detail["deployment_status"]["commit"], SHA);
    assert_eq!(detail["identity_attests_candidate"], json!(true));
    assert_eq!(detail["deploy_command"]["ok"], json!(true));
    assert_eq!(detail["deploy_command"]["exit_status"], json!(0));
    assert_eq!(detail["env_credential"], "laravel-cloud");
}

#[tokio::test]
async fn a_target_whose_host_builds_the_branch_itself_runs_no_deploy_command_at_all() {
    state::isolate();
    let cli = FakeCli::new("qa-command-discover-only");
    let origin = fake_origin(Some("build 0123456789ab".to_string())).await;
    cli.environments(environments(&origin, BRANCH, "running"));
    cli.deployments(json!([{ "id": "dep-1", "status": "success", "commitHash": SHA }]));
    let f = fixture(discovery_target(&cli, false), true, true);

    let row = f.deploy().await.expect("discovery alone is a deploy");
    assert_eq!(row.outcome, "succeeded", "{}", row.detail_json);
    let argv = cli.argv_log();
    assert!(
        !argv.contains("deploy my-app"),
        "nothing was triggered: {argv}"
    );
    assert!(argv.contains("environment:list"), "{argv}");
    let detail: Value = serde_json::from_str(&row.detail_json).unwrap();
    assert!(detail.get("deploy_command").is_none());
    // An abbreviated build id still identifies the candidate.
    assert_eq!(detail["identity_attests_candidate"], json!(true));
}

#[tokio::test]
async fn an_unpublished_candidate_is_refused_with_the_reason_rather_than_deploying_a_branch_head() {
    state::isolate();
    let cli = FakeCli::new("qa-command-unpublished");
    let f = fixture(discovery_target(&cli, true), false, true);
    let error = f.deploy().await.unwrap_err();
    assert!(
        error.contains("no published branch holding") && error.contains("never a bare commit"),
        "{error}"
    );
    assert!(
        cli.argv_log().is_empty(),
        "nothing may run when the precondition fails"
    );
    assert!(f
        .store
        .qa_deployments_for_candidate(&f.candidate_id)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn no_environment_for_the_branch_times_out_and_creates_nothing() {
    state::isolate();
    let cli = FakeCli::new("qa-command-timeout");
    let origin = fake_origin(Some(SHA.into())).await;
    // Every environment belongs to some other branch.
    cli.environments(environments(&origin, "other/branch", "running"));
    cli.deployments(json!([]));
    let mut target = discovery_target(&cli, false);
    target.deployment.deploy_timeout_secs = 4;
    let f = fixture(target, true, true);

    let row = f
        .deploy()
        .await
        .expect("a timeout is recorded, not thrown away");
    assert_eq!(row.outcome, "failed");
    assert_eq!(row.origin, "", "nothing was found, so no origin is claimed");
    let detail: Value = serde_json::from_str(&row.detail_json).unwrap();
    let observation = detail["observation_error"].as_str().unwrap_or_default();
    assert!(
        observation.contains("not ready in time") && observation.contains(BRANCH),
        "{observation}"
    );
    // It polled rather than giving up on the first answer, and it created
    // nothing: the only thing it ever ran was a read.
    let argv = cli.argv_log();
    assert!(
        argv.lines()
            .filter(|line| line.starts_with("environment:list"))
            .count()
            >= 3,
        "{argv}"
    );
    assert!(!argv.contains("environment:create") && !argv.contains("environment:delete"));

    // A browser run against a deployment with no origin is refused rather
    // than falling back to the target's configuration.
    let error = tracon::qa::service::browser_verify(
        &f.access(),
        serde_json::from_value(json!({
            "deployment_id": row.id,
            "scenario": { "start_path": "/", "steps": [], "assertions": [] },
        }))
        .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("did not attest this candidate SHA"),
        "{error}"
    );
}

#[tokio::test]
async fn a_host_that_deployed_another_commit_is_a_failure_however_healthy_it_looks() {
    state::isolate();
    let cli = FakeCli::new("qa-command-other-commit");
    let origin = fake_origin(Some(SHA.into())).await;
    cli.environments(environments(&origin, BRANCH, "running"));
    cli.deployments(json!([{ "id": "dep-2", "status": "success", "commitHash": "ffffffffffffffffffffffffffffffffffffffff" }]));
    let f = fixture(discovery_target(&cli, false), true, true);

    let row = f.deploy().await.unwrap();
    assert_eq!(row.outcome, "failed");
    let detail: Value = serde_json::from_str(&row.detail_json).unwrap();
    assert!(detail["commit_mismatch"]
        .as_str()
        .unwrap_or_default()
        .contains("ffffffff"));
}

#[tokio::test]
async fn a_failed_host_deployment_stops_the_poll_rather_than_waiting_out_the_timeout() {
    state::isolate();
    let cli = FakeCli::new("qa-command-host-failed");
    let origin = fake_origin(Some(SHA.into())).await;
    cli.environments(environments(&origin, BRANCH, "running"));
    cli.deployments(json!([{ "id": "dep-3", "status": "failed", "commitHash": SHA }]));
    let f = fixture(discovery_target(&cli, false), true, true);

    let row = f.deploy().await.unwrap();
    assert_eq!(row.outcome, "failed");
    let detail: Value = serde_json::from_str(&row.detail_json).unwrap();
    assert_eq!(detail["deployment_status"]["state"], "failed");
    assert!(
        detail.get("observation_error").is_none(),
        "it was observed, not timed out"
    );
}

#[tokio::test]
async fn an_identity_endpoint_that_does_not_attest_the_candidate_is_not_a_live_deployment() {
    state::isolate();
    let cli = FakeCli::new("qa-command-identity-fails");
    let origin = fake_origin(Some("some-other-build".into())).await;
    cli.environments(environments(&origin, BRANCH, "running"));
    cli.deployments(json!([{ "id": "dep-4", "status": "success", "commitHash": SHA }]));
    let f = fixture(discovery_target(&cli, false), true, true);

    let row = f.deploy().await.unwrap();
    assert_eq!(row.outcome, "failed");
    let detail: Value = serde_json::from_str(&row.detail_json).unwrap();
    assert_eq!(detail["identity_attests_candidate"], json!(false));
    assert_eq!(
        row.environment_identity.as_deref(),
        Some("some-other-build")
    );
}

#[tokio::test]
async fn a_failing_deploy_command_records_its_exit_status_and_redacted_tail_and_waits_for_nothing()
{
    state::isolate();
    let cli = FakeCli::new("qa-command-failing");
    let origin = fake_origin(Some(SHA.into())).await;
    cli.environments(environments(&origin, BRANCH, "running"));
    cli.deployments(json!([{ "id": "dep-5", "status": "success", "commitHash": SHA }]));
    // The CLI prints the credential back at us, as a real one printing an
    // environment's variables would.
    cli.write("deploy.out", &format!("refused: bad token {TOKEN}\n"));
    cli.write("deploy.exit", "3");
    let f = fixture(discovery_target(&cli, true), true, true);

    let row = f.deploy().await.unwrap();
    assert_eq!(row.outcome, "failed");
    let detail: Value = serde_json::from_str(&row.detail_json).unwrap();
    assert_eq!(detail["deploy_command"]["exit_status"], json!(3));
    assert_eq!(detail["deploy_command"]["ok"], json!(false));
    let tail = detail["deploy_command"]["tail"].as_str().unwrap();
    assert!(tail.contains("refused: bad token [redacted]"), "{tail}");
    assert!(!row.detail_json.contains(TOKEN));
    assert!(
        !cli.argv_log().contains("environment:list"),
        "a failed deploy command is not followed by waiting for it to work"
    );
}

#[tokio::test]
async fn a_credential_that_is_not_bound_to_this_channel_refuses_before_anything_runs() {
    state::isolate();
    let cli = FakeCli::new("qa-command-unbound");
    let mut target = discovery_target(&cli, true);
    target.deployment.env_credential = "not-configured".into();
    let f = fixture(target, true, true);
    let error = f.deploy().await.unwrap_err();
    assert!(
        error.contains("not-configured") && error.contains("unavailable"),
        "{error}"
    );
    assert!(cli.argv_log().is_empty());
}

#[tokio::test]
async fn without_the_deploy_grant_nothing_runs_and_the_refusal_names_the_target() {
    state::isolate();
    let cli = FakeCli::new("qa-command-ungranted");
    let f = fixture(discovery_target(&cli, true), true, false);
    let error = f.deploy().await.unwrap_err();
    assert!(
        error.contains("deploy is not authorized for qa:cloud-qa:command:"),
        "{error}"
    );
    assert!(cli.argv_log().is_empty(), "nothing runs without the grant");
}

/// The same refusal reaches a harness through the MCP tool, which is the path
/// an agent actually has. The policy ships deployments as ask/deny, so a
/// harness that asks for one without an active exact grant is refused rather
/// than queued behind a credential.
#[tokio::test]
async fn the_qa_deploy_tool_refuses_an_agent_without_the_deploy_authority_grant() {
    state::isolate();
    let cli = FakeCli::new("qa-command-mcp");
    let f = fixture(discovery_target(&cli, true), true, false);
    let tools = Arc::new(Tools {
        broker: f.broker.clone(),
        cfg: f.cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let access = tracon::mcp::SessionAccess {
        store: f.store.clone(),
        manager: f.manager.clone(),
    };
    let ctx = tracon::mcp::CallContext {
        session_id: SESSION.into(),
        channel: CHANNEL.into(),
        node_id: NODE.into(),
    };
    let error = tracon::mcp::qa::call(
        &tools,
        &access,
        &ctx,
        tracon::mcp::qa::DEPLOY,
        &json!({ "candidate_id": f.candidate_id, "target": "cloud-qa" }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("not authorized"), "{error}");
    assert!(cli.argv_log().is_empty());

    // The tool is declared, so the refusal is about authority rather than a
    // missing verb: an agent can ask, and is told no.
    let declared = tracon::mcp::qa::definitions();
    assert!(declared
        .iter()
        .any(|tool| tool["name"] == tracon::mcp::qa::DEPLOY));
}

// ---------------------------------------------------------------------------
// Browser verification against a command-kind target
// ---------------------------------------------------------------------------

#[tokio::test]
async fn browser_verification_binds_to_the_discovered_origin_and_refuses_a_changed_one() {
    state::isolate();
    let cli = FakeCli::new("qa-command-browser");
    let origin = fake_origin(Some(SHA.into())).await;
    cli.environments(environments(&origin, BRANCH, "running"));
    cli.deployments(json!([{ "id": "dep-6", "status": "success", "commitHash": SHA }]));
    let f = fixture(discovery_target(&cli, false), true, true);
    let row = f.deploy().await.unwrap();
    assert_eq!(row.outcome, "succeeded");

    // The plan a browser run would be given comes off the deployment's own
    // origin, not off configuration that never had one.
    let target = f.cfg.qa.targets["cloud-qa"]
        .resolved(Some(&row.origin))
        .unwrap();
    let plan = tracon::qa::browser_plan(
        &target,
        &serde_json::from_value(json!({
            "start_path": "/login",
            "steps": [],
            "assertions": [{ "kind": "url_path_is", "path": "/login" }],
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(plan.start_url, format!("{origin}/login"));
    assert_eq!(plan.allowed_origins, vec![origin.clone()]);

    // One grant covers the target's preview environments, because it names
    // the attested suffix rather than a per-pull-request hostname.
    assert_eq!(
        tracon::qa::browser_authority_target("cloud-qa", &f.cfg.qa.targets["cloud-qa"]).unwrap(),
        "qa:cloud-qa:origin:*.127.0.0.1"
    );

    // A run against a deployment whose origin is outside the attestation is
    // refused at resolution rather than reaching a browser.
    assert!(f.cfg.qa.targets["cloud-qa"]
        .resolved(Some("https://elsewhere.example.com"))
        .is_err());

    // Browser verification refuses a deployment that did not attest the
    // candidate, whatever its origin.
    let mut unattested = row.clone();
    unattested.id = "other".into();
    unattested.outcome = "failed".into();
    f.store.qa_insert_deployment(&unattested).unwrap();
    let error = tracon::qa::service::browser_verify(
        &f.access(),
        serde_json::from_value(json!({
            "deployment_id": "other",
            "scenario": { "start_path": "/", "steps": [], "assertions": [] },
        }))
        .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("did not attest this candidate SHA"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// The documented Laravel Cloud recipe
// ---------------------------------------------------------------------------

/// The recipe in the README is configuration, not code, so the thing that can
/// rot is the README. This parses the `[qa.targets.*]` block out of it and
/// validates it exactly as the node would, so a documented example that would
/// be refused at startup fails here instead.
#[test]
fn the_laravel_cloud_recipe_in_the_readme_is_a_target_this_node_accepts() {
    state::isolate();
    let readme = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("README.md"),
    )
    .unwrap();
    let block = readme
        .split("<!-- qa-target-example -->")
        .nth(1)
        .expect("README carries a marked QA target example")
        .split("```")
        .nth(1)
        .expect("the example is a fenced block");
    let toml = block.strip_prefix("toml\n").unwrap_or(block);
    #[derive(serde::Deserialize)]
    struct Wrapper {
        qa: Qa,
    }
    let parsed: Wrapper = toml::from_str(toml).expect("the documented example parses");
    parsed
        .qa
        .validate()
        .expect("the documented example is a target this node accepts");
    let target = parsed
        .qa
        .targets
        .values()
        .next()
        .expect("the example configures a target");
    assert_eq!(target.deployment.kind, QA_KIND_COMMAND);
    assert_eq!(target.deployment.env_credential, "laravel-cloud");
    assert!(
        target.discovers_origin(),
        "the documented Cloud default discovers the preview environment the automation made"
    );
}
