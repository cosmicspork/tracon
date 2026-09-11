//! Runtime-owned execution of configured QA and prototype operations.
//!
//! This is deliberately a consumer of the workspace/environment boundary. It
//! receives only candidate identities and configuration, imports node-created
//! transfer files into runtime volumes, and exports validated assets again. No
//! endpoint accepts a host path, bind mount, command line, browser image, or
//! credential value.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{json, Value};

use crate::{
    authority::{self, AuthorityQuery},
    config::{safe_relative_path, Config, PrototypeBuild, QaTarget},
    corpus::html::HtmlFile,
    mcp::{self, CallContext},
    policy::{Policy, Verdict},
    qa::{
        bounded_redacted_log, browser_authority_target, browser_plan, browser_report,
        deploy_authority_target, evidence_state, observe_environment, redact_secrets,
        requested_credential_keys, test_account_authority_target, BrowserAssertionResult,
        BrowserPlan, BrowserReport, BrowserRequest, DeployRequest, BROWSER_RUNNER,
    },
    runner::RunnerCommand,
    session::Manager,
    store::{
        ActionBegin, BrowserRunRow, DemonstrationRow, PrototypeRow, QaAssetRow, QaDeploymentRow,
        Store,
    },
    stream::Bus,
};

const DEPLOY_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const REPORT_BYTES: u64 = 512 * 1024;
const HTML_FILES: usize = 128;
const HTML_BYTES: u64 = 24 * 1024 * 1024;

pub struct QaAccess<'a> {
    pub store: &'a Store,
    pub manager: &'a Manager,
    pub cfg: &'a Config,
    pub broker: &'a crate::broker::SharedBroker,
    pub http: &'a reqwest::Client,
    pub policy: &'a std::sync::RwLock<Policy>,
    pub node_id: &'a str,
    /// Present for a harness-facing MCP call. Operator HTTP actions omit it,
    /// while a harness may operate only on its own candidate in its channel.
    pub requester_session_id: Option<&'a str>,
    pub requester_channel: Option<&'a str>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrowserRunResult {
    pub run: BrowserRunRow,
    pub assets: Vec<QaAssetRow>,
    pub artifact_error: Option<String>,
}

/// Start the configured GitLab QA pipeline at an exact candidate SHA, then
/// persist the resulting deployment identity observation whether it succeeds,
/// fails, or cannot be observed.
pub async fn deploy(
    access: &QaAccess<'_>,
    request: DeployRequest,
) -> Result<QaDeploymentRow, String> {
    let candidate = candidate(access.store, &request.candidate_id)?;
    let owner_session = candidate_owner(&candidate)?;
    let target = configured_target(access.cfg, &request.target)?;
    let browser_binding = browser_target_binding(&target)?;
    if candidate.channel.trim().is_empty() {
        return Err("candidate has no channel".into());
    }
    ensure_requester_scope(access, &candidate, &owner_session)?;

    let authority_target = deploy_authority_target(&request.target, &target);
    let action_id = authorize(
        access,
        &candidate.channel,
        &owner_session,
        authority::DEPLOY,
        &authority_target,
        Some(&candidate.head_sha),
        &json!({ "candidate_id": candidate.id, "target": request.target, "head_sha": candidate.head_sha }),
    )?;
    let started_ms = crate::store::now_ms();
    let mut build_id = format!(
        "gitlab:{}:unstarted:{}",
        target.deployment.project, candidate.head_sha
    );
    let mut detail = json!({
        "transport": "gitlab_pipeline",
        "target": request.target,
        "candidate_id": candidate.id,
        "head_sha": candidate.head_sha,
        "execution_image": target.deployment.execution_image,
        "browser_target_binding": browser_binding.clone(),
    });
    let pipeline = launch_pipeline(
        access,
        &candidate.channel,
        &owner_session,
        &candidate.head_sha,
        &request.target,
        &target,
        &action_id,
        &authority_target,
    )
    .await;
    let outcome = match pipeline {
        Ok(pipeline) => match pipeline["id"].as_i64() {
            Some(pipeline_id) => {
                let job_id = pipeline["job_id"].as_i64();
                let terminal = wait_for_pipeline(
                    access,
                    &candidate.channel,
                    &owner_session,
                    &target.deployment.project,
                    pipeline_id,
                )
                .await;
                match terminal {
                    Ok(status) => {
                        let sha = status["sha"].as_str().unwrap_or_default();
                        build_id = format!(
                            "gitlab:{}:{pipeline_id}:{}:{sha}",
                            target.deployment.project,
                            job_id.map(|id| id.to_string()).unwrap_or_default(),
                        );
                        detail = json!({
                            "transport": "gitlab_pipeline",
                            "pipeline": status,
                            "deploy_job_id": job_id,
                            "target": request.target,
                            "candidate_id": candidate.id,
                            "head_sha": candidate.head_sha,
                            "execution_image": target.deployment.execution_image,
                            "browser_target_binding": browser_binding,
                        });
                        if sha.eq_ignore_ascii_case(&candidate.head_sha)
                            && detail["pipeline"]["status"].as_str() == Some("success")
                        {
                            "succeeded"
                        } else {
                            "failed"
                        }
                    }
                    Err(error) => {
                        detail["pipeline_error"] = json!(error);
                        "failed"
                    }
                }
            }
            None => {
                detail["pipeline_error"] = json!("GitLab did not return a pipeline id");
                "failed"
            }
        },
        Err(error) => {
            detail["pipeline_error"] = json!(error);
            "failed"
        }
    };
    let observation = observe_environment(&target).await;
    detail["environment_observation"] = json!({
        "state": observation.state,
        "detail": observation.detail,
        "at_ms": observation.observed_ms,
    });
    let row = QaDeploymentRow {
        id: uuid::Uuid::now_v7().to_string(),
        candidate_id: candidate.id.clone(),
        channel: candidate.channel.clone(),
        target_id: request.target,
        build_id,
        execution_image: target.deployment.execution_image.clone(),
        origin: crate::config::qa_origin(&target.origin)?,
        environment_identity: observation.identity,
        identity_state: observation.state.into(),
        observed_ms: observation.observed_ms,
        started_ms,
        finished_ms: crate::store::now_ms(),
        outcome: outcome.into(),
        detail_json: detail.to_string(),
    };
    if let Err(error) = access.store.qa_insert_deployment(&row) {
        let detail = store_error(error);
        finish_action(access.store, &action_id, "failed", &detail);
        return Err(detail);
    }
    let action_outcome = if row.outcome == "succeeded" {
        "pipeline completed"
    } else {
        "pipeline failed or did not attest the candidate SHA"
    };
    finish_action(
        access.store,
        &action_id,
        if row.outcome == "succeeded" {
            "succeeded"
        } else {
            "failed"
        },
        action_outcome,
    );
    Ok(row)
}

/// Run a real Playwright browser in the configured image. It receives only the
/// explicitly selected values from the configured dedicated test credential,
/// and only while the process is inside the runtime boundary.
pub async fn browser_verify(
    access: &QaAccess<'_>,
    request: BrowserRequest,
) -> Result<BrowserRunResult, String> {
    let deployment = access
        .store
        .qa_deployment(&request.deployment_id)
        .map_err(store_error)?
        .ok_or("QA deployment was not found")?;
    let candidate = candidate(access.store, &deployment.candidate_id)?;
    let owner_session = candidate_owner(&candidate)?;
    ensure_requester_scope(access, &candidate, &owner_session)?;
    if candidate.channel != deployment.channel || candidate.id != deployment.candidate_id {
        return Err("QA deployment no longer matches its candidate identity".into());
    }
    if deployment.outcome != "succeeded" {
        return Err("QA deployment did not attest this candidate SHA; browser evidence would be untrustworthy".into());
    }
    let target = configured_target(access.cfg, &deployment.target_id)?;
    if crate::config::qa_origin(&target.origin)? != deployment.origin {
        return Err(
            "QA target origin changed since deployment; create a new deployment observation".into(),
        );
    }
    ensure_browser_target_binding(&deployment, &target)?;
    let plan = browser_plan(&target, &request.scenario)?;
    let browser_authority = browser_authority_target(&deployment.target_id, &target)?;
    let browser_action = authorize(
        access,
        &candidate.channel,
        &owner_session,
        authority::BROWSER_VERIFY,
        &browser_authority,
        Some(&candidate.head_sha),
        &json!({ "candidate_id": candidate.id, "deployment_id": deployment.id, "origin": deployment.origin }),
    )?;

    let keys = requested_credential_keys(&request.scenario);
    let (credential_name, credential_env, secret_values, test_account_action) = if keys.is_empty() {
        (None, Vec::new(), Vec::new(), None)
    } else {
        let name = match target
            .browser
            .test_credential
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        {
            Some(name) => name,
            None => return fail_browser_action(
                access,
                &browser_action,
                "scenario requests a credential but this QA target has no dedicated test account",
            ),
        };
        let authority_target = test_account_authority_target(&deployment.target_id, name);
        let action = match authorize(
            access,
            &candidate.channel,
            &owner_session,
            authority::BROWSER_TEST_ACCOUNT,
            &authority_target,
            Some(&candidate.head_sha),
            &json!({ "candidate_id": candidate.id, "deployment_id": deployment.id, "credential": name }),
        ) {
            Ok(action) => action,
            Err(error) => return fail_browser_action(access, &browser_action, &error),
        };
        let values = {
            let broker = match access.broker.read() {
                Ok(broker) => broker,
                Err(_) => {
                    finish_action(
                        access.store,
                        &action,
                        "failed",
                        "credential broker lock is unavailable",
                    );
                    return fail_browser_action(
                        access,
                        &browser_action,
                        "credential broker lock is unavailable",
                    );
                }
            };
            match broker.env_for(name, &candidate.channel, access.node_id) {
                Ok(values) => values,
                Err(error) => {
                    let detail = format!("dedicated test account is unavailable: {error}");
                    finish_action(access.store, &action, "failed", &detail);
                    return fail_browser_action(access, &browser_action, &detail);
                }
            }
        };
        let mut selected = Vec::with_capacity(keys.len());
        let mut secrets = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(value) = values.get(&key) else {
                let detail = format!("dedicated test account does not contain {key}");
                finish_action(access.store, &action, "failed", &detail);
                return fail_browser_action(access, &browser_action, &detail);
            };
            selected.push((key, value.clone()));
            secrets.push(value.clone());
        }
        (Some(name.to_string()), selected, secrets, Some(action))
    };

    let before = observe_environment(&target).await;
    let started_ms = crate::store::now_ms();
    let runtime = run_browser_runtime(access, &target, &plan, &credential_env).await;
    let after = observe_environment(&target).await;
    let finished_ms = crate::store::now_ms();
    let (report, exported, raw_output, runtime_error) = match runtime {
        Ok(result) => (result.report, Some(result.exported), result.output, None),
        Err(error) => (None, None, Vec::new(), Some(error)),
    };
    let log = {
        let mut raw = raw_output;
        if let Some(report) = &report {
            raw.extend_from_slice(report.log.as_bytes());
        }
        if let Some(error) = &runtime_error {
            raw.extend_from_slice(error.as_bytes());
        }
        bounded_redacted_log(&raw, secret_values.clone())
    };
    let assertions: Vec<BrowserAssertionResult> = report
        .as_ref()
        .map(|report| report.assertions.clone())
        .unwrap_or_else(|| {
            vec![BrowserAssertionResult {
                kind: "runner".into(),
                ok: false,
                detail: "browser runtime did not produce an accepted report".into(),
            }]
        });
    let assertions_json = redact_secrets(
        &serde_json::to_string(&assertions).expect("assertions serialize"),
        secret_values,
    );
    let outcome = if report.as_ref().is_some_and(|report| report.ok) && runtime_error.is_none() {
        "passed"
    } else if report.is_some() {
        "failed"
    } else {
        "unknown"
    };
    let evidence_state = run_identity_state(&deployment, &before, &after);
    let preliminary = BrowserRunRow {
        id: uuid::Uuid::now_v7().to_string(),
        deployment_id: deployment.id.clone(),
        candidate_id: candidate.id.clone(),
        channel: candidate.channel.clone(),
        target_id: deployment.target_id.clone(),
        authorized_origins_json: serde_json::to_string(&plan.allowed_origins)
            .expect("origins serialize"),
        test_credential: credential_name,
        assertions_json,
        outcome: outcome.into(),
        environment_before: before.identity,
        environment_after: after.identity,
        evidence_state: evidence_state.into(),
        log_tail: log,
        started_ms,
        finished_ms,
    };
    if let Err(error) = access.store.qa_insert_browser_run(&preliminary) {
        let detail = store_error(error);
        finish_action(access.store, &browser_action, "failed", &detail);
        if let Some(action) = test_account_action.as_deref() {
            finish_action(access.store, action, "failed", &detail);
        }
        return Err(detail);
    }
    finish_action(
        access.store,
        &browser_action,
        if preliminary.outcome == "passed" {
            "succeeded"
        } else {
            "failed"
        },
        if preliminary.outcome == "passed" {
            "browser run completed"
        } else {
            "browser run failed or was not observable"
        },
    );
    if let Some(action) = test_account_action {
        finish_action(
            access.store,
            &action,
            if preliminary.outcome == "passed" {
                "succeeded"
            } else {
                "failed"
            },
            "dedicated test account was injected only into the browser runtime",
        );
    }

    let mut assets = Vec::new();
    let artifact_error = match (report.as_ref(), exported.as_deref()) {
        (Some(report), Some(root)) => {
            match attach_browser_bundle(access, &candidate, &preliminary, report, root) {
                Ok(rows) => {
                    assets = rows;
                    None
                }
                Err(error) => Some(error),
            }
        }
        _ => runtime_error,
    };
    Ok(BrowserRunResult {
        run: preliminary,
        assets,
        artifact_error,
    })
}

/// Build configured repository-derived output from the candidate owner's
/// prepared snapshot, then import the result using the same HTML bundle store
/// and isolated preview path as every other artifact.
pub async fn build_prototype(
    access: &QaAccess<'_>,
    candidate_id: &str,
) -> Result<PrototypeRow, String> {
    let candidate = candidate(access.store, candidate_id)?;
    let owner_session = candidate_owner(&candidate)?;
    ensure_requester_scope(access, &candidate, &owner_session)?;
    let recipe = access
        .cfg
        .qa
        .prototype
        .clone()
        .ok_or("repository-derived prototype builds are not configured")?;
    let id = uuid::Uuid::now_v7().to_string();
    let started_ms = crate::store::now_ms();
    let source_snapshot = match access.manager.snapshot_workspace(&owner_session).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return insert_failed_prototype(
                access,
                &candidate,
                &recipe,
                &id,
                started_ms,
                Value::Null,
                format!("candidate has no prepared source snapshot: {error}"),
            )
        }
    };
    let backend = access.manager.backend();
    let workspace = match crate::workspace::from_snapshot(
        backend.as_ref(),
        &format!("prototype-{id}"),
        &source_snapshot,
    )
    .await
    {
        Ok(workspace) => workspace,
        Err(error) => {
            return insert_failed_prototype(
                access,
                &candidate,
                &recipe,
                &id,
                started_ms,
                Value::Null,
                format!("could not create prototype workspace: {error}"),
            )
        }
    };
    let plan = match crate::environment::inspect(&source_snapshot) {
        Ok(plan) => plan,
        Err(error) => {
            return insert_failed_prototype(
                access,
                &candidate,
                &recipe,
                &id,
                started_ms,
                Value::Null,
                format!("could not inspect prepared source: {error}"),
            )
        }
    };
    let plan_json = match serde_json::to_value(&plan) {
        Ok(plan) => plan,
        Err(error) => {
            return insert_failed_prototype(
                access,
                &candidate,
                &recipe,
                &id,
                started_ms,
                Value::Null,
                format!("could not record build preparation: {error}"),
            )
        }
    };
    let prepared =
        match crate::environment::prepare(backend.as_ref(), access.cfg, &workspace, &plan).await {
            Ok(prepared) => prepared,
            Err(error) => {
                return insert_failed_prototype(
                    access,
                    &candidate,
                    &recipe,
                    &id,
                    started_ms,
                    plan_json,
                    format!("could not prepare build environment: {error}"),
                )
            }
        };
    // Verification executes only the operator's locked checks in its fresh
    // runtime. The candidate recipe runs exactly once below, in its own
    // configured build image.
    let verification = crate::environment::verify(
        backend.as_ref(),
        &workspace,
        &prepared,
        &access.cfg.supervision.checks,
    )
    .await
    .map_err(|error| format!("prepared environment verification failed: {error}"));
    let verification = match verification {
        Ok(result) => result,
        Err(error) => {
            return insert_failed_prototype(
                access, &candidate, &recipe, &id, started_ms, plan_json, error,
            )
        }
    };
    let command = RunnerCommand {
        argv: recipe.command.clone(),
        mounts: vec![workspace.mount("/work", false)],
        workdir: Some("/work".into()),
        name: format!("tracon-prototype-{id}"),
        image: Some(recipe.image.clone()),
        ..Default::default()
    };
    let output = tokio::time::timeout(
        Duration::from_secs(recipe.timeout_secs),
        backend.runner(Vec::new()).run_capture(command),
    )
    .await
    .map_err(|_| "prototype build timed out".to_string())
    .and_then(|output| output.map_err(|error| format!("prototype runtime failed: {error}")));
    let detail = match &output {
        Ok(output) => bounded_redacted_log(
            &[output.stdout.as_slice(), output.stderr.as_slice()].concat(),
            Vec::new(),
        ),
        Err(error) => error.clone(),
    };
    if !output.as_ref().is_ok_and(|output| output.status.success()) {
        return insert_failed_prototype(
            access, &candidate, &recipe, &id, started_ms, plan_json, detail,
        );
    }
    let exported = match crate::workspace::export(backend.as_ref(), &workspace).await {
        Ok(exported) => exported,
        Err(error) => {
            return insert_unknown_prototype(
                access,
                &candidate,
                &recipe,
                &id,
                started_ms,
                plan_json,
                format!("could not export prototype bundle: {error}"),
            )
        }
    };
    let files = match collect_html_files(&exported, &recipe) {
        Ok(files) => files,
        Err(error) => {
            return insert_failed_prototype(
                access, &candidate, &recipe, &id, started_ms, plan_json, error,
            )
        }
    };
    let entry_path = format!("{}/{}", recipe.output_dir, recipe.entry_path);
    if !files.iter().any(|file| file.path == entry_path) {
        return insert_failed_prototype(
            access,
            &candidate,
            &recipe,
            &id,
            started_ms,
            plan_json,
            "prototype build did not produce its configured entry path".into(),
        );
    }
    let slug = format!("ref-prototype-{id}");
    let (document, changes) = match access.store.write_html_document_change(
        access.node_id,
        &candidate.channel,
        &slug,
        &format!("prototype-{id}.zip"),
        &entry_path,
        files,
        None,
        true,
    ) {
        Ok(result) => result,
        Err(error) => {
            return insert_unknown_prototype(
                access,
                &candidate,
                &recipe,
                &id,
                started_ms,
                plan_json,
                format!("could not import prototype HTML bundle: {error}"),
            )
        }
    };
    publish_changes(access.manager.bus(), &candidate.channel, changes);
    let build_inputs = json!({
        "candidate_id": candidate.id,
        "head_sha": candidate.head_sha,
        "recipe": { "command": recipe.command, "output_dir": recipe.output_dir, "entry_path": recipe.entry_path },
        "preparation": plan_json,
        "verification": verification,
    });
    let row = PrototypeRow {
        id,
        candidate_id: candidate.id.clone(),
        channel: candidate.channel.clone(),
        source_revision: candidate.head_sha.clone(),
        source_identity_json: candidate.capture_json.clone(),
        build_image: recipe.image,
        build_inputs_json: build_inputs.to_string(),
        document_id: Some(document.id),
        document_hash: Some(document.hash),
        slug,
        entry_path,
        outcome: "succeeded".into(),
        detail,
        created_ms: started_ms,
        finished_ms: crate::store::now_ms(),
    };
    access.store.prototype_insert(&row).map_err(store_error)?;
    Ok(row)
}

fn insert_failed_prototype(
    access: &QaAccess<'_>,
    candidate: &crate::store::CandidateRow,
    recipe: &PrototypeBuild,
    id: &str,
    started_ms: i64,
    preparation: Value,
    detail: String,
) -> Result<PrototypeRow, String> {
    insert_prototype_outcome(
        access,
        candidate,
        recipe,
        id,
        started_ms,
        preparation,
        "failed",
        detail,
    )
}

fn insert_unknown_prototype(
    access: &QaAccess<'_>,
    candidate: &crate::store::CandidateRow,
    recipe: &PrototypeBuild,
    id: &str,
    started_ms: i64,
    preparation: Value,
    detail: String,
) -> Result<PrototypeRow, String> {
    insert_prototype_outcome(
        access,
        candidate,
        recipe,
        id,
        started_ms,
        preparation,
        "unknown",
        detail,
    )
}

#[allow(clippy::too_many_arguments)] // mirrors insert_failed_prototype/insert_unknown_prototype's shape plus outcome
fn insert_prototype_outcome(
    access: &QaAccess<'_>,
    candidate: &crate::store::CandidateRow,
    recipe: &PrototypeBuild,
    id: &str,
    started_ms: i64,
    preparation: Value,
    outcome: &str,
    detail: String,
) -> Result<PrototypeRow, String> {
    let row = PrototypeRow {
        id: id.into(),
        candidate_id: candidate.id.clone(),
        channel: candidate.channel.clone(),
        source_revision: candidate.head_sha.clone(),
        source_identity_json: candidate.capture_json.clone(),
        build_image: recipe.image.clone(),
        build_inputs_json: json!({ "recipe": recipe.command, "preparation": preparation })
            .to_string(),
        document_id: None,
        document_hash: None,
        slug: format!("ref-prototype-{id}"),
        entry_path: recipe.entry_path.clone(),
        outcome: outcome.into(),
        detail: detail
            .chars()
            .rev()
            .take(64 * 1024)
            .collect::<String>()
            .chars()
            .rev()
            .collect(),
        created_ms: started_ms,
        finished_ms: crate::store::now_ms(),
    };
    access.store.prototype_insert(&row).map_err(store_error)?;
    Ok(row)
}

/// GitLab's pipeline-creation API resolves `ref` only against an existing
/// branch or tag — never a bare commit SHA — so this never creates a
/// pipeline. It finds an existing pipeline GitLab already ran at the exact
/// candidate SHA and plays the operator-configured manual deploy job inside
/// it. If no such pipeline, or none with that job ready to play, exists,
/// the deploy refuses: it never falls back to a moving ref.
#[allow(clippy::too_many_arguments)] // channel/session/sha/target identity plus the authority record to recheck before the one mutation
async fn launch_pipeline(
    access: &QaAccess<'_>,
    channel: &str,
    session_id: &str,
    sha: &str,
    target_id: &str,
    target: &QaTarget,
    action_id: &str,
    authority_target: &str,
) -> Result<Value, String> {
    let ctx = CallContext {
        session_id: session_id.into(),
        channel: channel.into(),
        node_id: access.node_id.into(),
    };
    let candidates = mcp::gitlab::call(
        access.broker,
        access.http,
        &ctx,
        mcp::gitlab::PIPELINE_LIST_BY_SHA,
        &json!({ "project": target.deployment.project, "sha": sha }),
        None,
    )
    .await?;
    let candidate_ids: Vec<i64> = candidates
        .as_array()
        .map(|pipelines| pipelines.iter().filter_map(|p| p["id"].as_i64()).collect())
        .unwrap_or_default();
    if candidate_ids.is_empty() {
        return Err(format!(
            "no GitLab pipeline exists for candidate sha {sha}; a QA deploy never creates one on a mutable ref"
        ));
    }
    let mut statuses = Vec::with_capacity(candidate_ids.len());
    for pipeline_id in candidate_ids {
        let status = mcp::gitlab::call(
            access.broker,
            access.http,
            &ctx,
            mcp::gitlab::PIPELINE_STATUS,
            &json!({ "project": target.deployment.project, "pipeline_id": pipeline_id }),
            None,
        )
        .await?;
        statuses.push(status);
    }
    let (pipeline_id, job_id) = select_deploy_job(&statuses, sha, &target.deployment.deploy_job)?;
    let mut variables: BTreeMap<String, Value> = target
        .deployment
        .variables
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    variables.insert("TRACON_CANDIDATE_SHA".into(), Value::String(sha.into()));
    variables.insert("TRACON_QA_TARGET".into(), Value::String(target_id.into()));
    variables.insert(
        "TRACON_EXECUTION_IMAGE".into(),
        Value::String(target.deployment.execution_image.clone()),
    );
    // A final authority recheck, immediately before the one remote mutation
    // this flow performs: authority state (a revoked grant, a changed
    // policy) can move during the pipeline lookups above, and playing the
    // job is not reversible.
    let recheck = || -> Result<(), String> {
        let policy = access
            .policy
            .read()
            .map_err(|_| "policy lock is unavailable")?;
        let decision = authority::decide(
            access.store,
            &policy,
            &AuthorityQuery {
                channel,
                session_id,
                action: authority::DEPLOY,
                target: authority_target,
                revision: Some(sha),
                args: &json!({ "target": target_id, "head_sha": sha }),
            },
        )
        .map_err(|error| format!("could not recheck deploy authority: {error}"))?;
        if decision.verdict != Verdict::Allow {
            return Err(format!(
                "deploy authorization for {authority_target} is no longer valid: {}",
                decision.reason.unwrap_or_default()
            ));
        }
        access
            .store
            .authority_action_set_grant(action_id, decision.rule_id.as_deref())
            .map_err(store_error)
    };
    let played = mcp::gitlab::call(
        access.broker,
        access.http,
        &ctx,
        mcp::gitlab::JOB_PLAY,
        &json!({ "project": target.deployment.project, "job_id": job_id, "variables": variables }),
        Some(&recheck),
    )
    .await?;
    Ok(json!({
        "id": pipeline_id,
        "job_id": job_id,
        "job_status": played["status"],
    }))
}

/// Pick the first candidate pipeline (already fetched by
/// `PIPELINE_LIST_BY_SHA` and re-fetched in full by `PIPELINE_STATUS`) whose
/// `sha` still matches exactly and that has `deploy_job` sitting `manual`.
/// The SHA is re-verified here, not just trusted from the list call, so an
/// id is never played against without confirming what it actually is.
fn select_deploy_job(
    statuses: &[Value],
    sha: &str,
    deploy_job: &str,
) -> Result<(i64, i64), String> {
    for status in statuses {
        let Some(pipeline_id) = status["id"].as_i64() else {
            continue;
        };
        if !status["sha"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(sha))
        {
            continue;
        }
        let job_id = status["jobs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|job| {
                job["name"].as_str() == Some(deploy_job) && job["status"].as_str() == Some("manual")
            })
            .and_then(|job| job["id"].as_i64());
        if let Some(job_id) = job_id {
            return Ok((pipeline_id, job_id));
        }
    }
    Err(format!(
        "no pipeline at sha {sha} has a manual {deploy_job:?} job ready to play"
    ))
}

async fn wait_for_pipeline(
    access: &QaAccess<'_>,
    channel: &str,
    session_id: &str,
    project: &str,
    pipeline_id: i64,
) -> Result<Value, String> {
    let started = tokio::time::Instant::now();
    loop {
        let status = mcp::gitlab::call(
            access.broker,
            access.http,
            &CallContext {
                session_id: session_id.into(),
                channel: channel.into(),
                node_id: access.node_id.into(),
            },
            mcp::gitlab::PIPELINE_STATUS,
            &json!({ "project": project, "pipeline_id": pipeline_id }),
            None,
        )
        .await?;
        match status["status"].as_str() {
            Some("success" | "failed" | "canceled" | "skipped" | "manual") => return Ok(status),
            _ if started.elapsed() >= DEPLOY_TIMEOUT => {
                return Err("QA deployment pipeline timed out".into())
            }
            _ => tokio::time::sleep(POLL_INTERVAL).await,
        }
    }
}

struct BrowserRuntime {
    report: Option<BrowserReport>,
    exported: PathBuf,
    output: Vec<u8>,
}

async fn run_browser_runtime(
    access: &QaAccess<'_>,
    target: &QaTarget,
    plan: &BrowserPlan,
    credential_env: &[(String, String)],
) -> Result<BrowserRuntime, String> {
    let backend = access.manager.backend();
    // Scoped for the life of this one run: the container's egress gateway
    // narrows to exactly the configured target origin(s), never the
    // harness's LLM-provider-only allowlist and never open egress. Held
    // across the whole container run below; dropping it restores deny-all.
    let allowed_hosts = origin_hosts(&plan.allowed_origins)?;
    let egress = backend
        .scope_qa_egress(&allowed_hosts)
        .await
        .map_err(|error| format!("could not scope QA browser egress: {error}"))?;
    let proxy_url = backend.qa_proxy_url();
    let transfer =
        tempfile::tempdir().map_err(|error| format!("could not stage browser runner: {error}"))?;
    fs::write(transfer.path().join("browser-runner.cjs"), BROWSER_RUNNER)
        .map_err(|error| format!("could not stage browser runner: {error}"))?;
    let spec = browser_runtime_spec(target, plan, proxy_url.as_deref())?;
    fs::write(
        transfer.path().join("browser.json"),
        serde_json::to_vec(&spec).expect("browser spec serializes"),
    )
    .map_err(|error| format!("could not stage browser scenario: {error}"))?;
    let id = format!("qa-browser-{}", uuid::Uuid::now_v7());
    let workspace = crate::workspace::from_snapshot(backend.as_ref(), &id, transfer.path())
        .await
        .map_err(|error| format!("could not create browser workspace: {error}"))?;
    let output = backend
        .runner(Vec::new())
        .run_capture(RunnerCommand {
            argv: vec![
                "node".into(),
                "/work/browser-runner.cjs".into(),
                "/work/browser.json".into(),
            ],
            env: credential_env.to_vec(),
            mounts: vec![workspace.mount("/work", false)],
            workdir: Some("/work".into()),
            name: id,
            image: Some(target.browser.image.clone()),
        })
        .await;
    drop(egress);
    let exported = crate::workspace::export(backend.as_ref(), &workspace)
        .await
        .map_err(|error| format!("could not export browser evidence: {error}"))?;
    let bytes = limited_file(&exported, "output/report.json", REPORT_BYTES)?;
    let report = browser_report(
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("browser report is not JSON: {error}"))?,
    )?;
    let output = output.map_err(|error| format!("browser runtime failed: {error}"))?;
    let mut raw = output.stdout;
    raw.extend_from_slice(&output.stderr);
    Ok(BrowserRuntime {
        report: Some(report),
        exported,
        output: raw,
    })
}

/// The exact hosts of a plan's approved origins, for the QA egress gateway
/// (which filters by host, not by scheme or path).
fn origin_hosts(origins: &[String]) -> Result<Vec<String>, String> {
    origins
        .iter()
        .map(|origin| {
            url::Url::parse(origin)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
                .ok_or_else(|| format!("QA target origin {origin:?} has no host"))
        })
        .collect()
}

fn browser_runtime_spec(
    target: &QaTarget,
    plan: &BrowserPlan,
    proxy_url: Option<&str>,
) -> Result<Value, String> {
    let origin = crate::config::qa_origin(&target.origin)?;
    let steps: Result<Vec<Value>, String> = plan
        .steps
        .iter()
        .map(|step| match step {
            crate::qa::BrowserStep::Navigate { path } => Ok(json!({
                "kind": "navigate", "url": crate::qa::browser_url(&origin, path)?,
            })),
            crate::qa::BrowserStep::Click { selector } => Ok(json!({ "kind": "click", "selector": selector })),
            crate::qa::BrowserStep::Fill { selector, value, credential_env } => Ok(json!({
                "kind": "fill", "selector": selector, "value": value, "credential_env": credential_env,
            })),
            crate::qa::BrowserStep::WaitFor { selector } => Ok(json!({ "kind": "wait_for", "selector": selector })),
        })
        .collect();
    Ok(json!({
        "start_url": plan.start_url,
        "allowed_origins": plan.allowed_origins,
        "steps": steps?,
        "assertions": plan.assertions,
        "timeout_ms": target.browser.timeout_secs * 1000,
        "proxy_url": proxy_url,
    }))
}

fn attach_browser_bundle(
    access: &QaAccess<'_>,
    candidate: &crate::store::CandidateRow,
    run: &BrowserRunRow,
    report: &BrowserReport,
    exported: &Path,
) -> Result<Vec<QaAssetRow>, String> {
    let slug = format!("ref-qa-browser-{}", run.id);
    let mut files = Vec::with_capacity(report.screenshots.len() + 1);
    let mut body = String::from("<!doctype html><html><head><meta charset=\"utf-8\"><title>QA browser evidence</title></head><body><h1>QA browser evidence</h1>");
    body.push_str("<h2>Assertions</h2><pre>");
    body.push_str(&escape_html(&run.assertions_json));
    body.push_str("</pre><h2>Bounded browser log</h2><pre>");
    body.push_str(&escape_html(&run.log_tail));
    body.push_str("</pre>");
    for shot in &report.screenshots {
        let bytes = limited_file(exported, &shot.path, 6 * 1024 * 1024)?;
        let path = format!("screenshots/{}.png", shot.label);
        body.push_str(&format!(
            "<figure><figcaption>{}</figcaption><img src=\"{}\" alt=\"{}\"></figure>",
            shot.label, path, shot.label
        ));
        files.push(HtmlFile { path, bytes });
    }
    body.push_str("</body></html>");
    files.push(HtmlFile {
        path: "index.html".into(),
        bytes: body.into_bytes(),
    });
    let (document, changes) = access
        .store
        .write_html_document_change(
            access.node_id,
            &candidate.channel,
            &slug,
            &format!("qa-browser-{}.zip", run.id),
            "index.html",
            files,
            None,
            true,
        )
        .map_err(|error| format!("could not import browser evidence bundle: {error}"))?;
    publish_changes(access.manager.bus(), &candidate.channel, changes);
    access
        .store
        .attach_demonstration(&DemonstrationRow {
            id: uuid::Uuid::now_v7().to_string(),
            candidate_id: candidate.id.clone(),
            channel: candidate.channel.clone(),
            document_id: document.id.clone(),
            document_slug: document.slug.clone(),
            document_hash: document.hash.clone(),
            label: format!("QA browser run {}", run.id),
            created_ms: crate::store::now_ms(),
        })
        .map_err(store_error)?;
    let now = crate::store::now_ms();
    let mut assets = vec![QaAssetRow {
        id: uuid::Uuid::now_v7().to_string(),
        browser_run_id: run.id.clone(),
        candidate_id: candidate.id.clone(),
        channel: candidate.channel.clone(),
        kind: "browser-log".into(),
        document_id: document.id.clone(),
        document_hash: document.hash.clone(),
        slug: document.slug.clone(),
        created_ms: now,
    }];
    if !report.screenshots.is_empty() {
        assets.push(QaAssetRow {
            id: uuid::Uuid::now_v7().to_string(),
            browser_run_id: run.id.clone(),
            candidate_id: candidate.id.clone(),
            channel: candidate.channel.clone(),
            kind: "screenshots".into(),
            document_id: document.id.clone(),
            document_hash: document.hash.clone(),
            slug: document.slug,
            created_ms: now,
        });
    }
    for asset in &assets {
        access.store.qa_insert_asset(asset).map_err(store_error)?;
    }
    Ok(assets)
}

fn configured_target(config: &Config, id: &str) -> Result<QaTarget, String> {
    config
        .qa
        .targets
        .get(id)
        .cloned()
        .ok_or_else(|| format!("QA target {id:?} is not configured"))
}

fn browser_target_binding(target: &QaTarget) -> Result<Value, String> {
    Ok(json!({
        "origin": crate::config::qa_origin(&target.origin)?,
        "identity_url": target.identity_url,
        "identity_header": target.identity_header,
        "allowed_origins": target.canonical_origins()?,
        "browser_image": target.browser.image,
        "timeout_secs": target.browser.timeout_secs,
    }))
}

fn ensure_browser_target_binding(
    deployment: &QaDeploymentRow,
    target: &QaTarget,
) -> Result<(), String> {
    let stored: Value = serde_json::from_str(&deployment.detail_json)
        .map_err(|_| "deployment record has no valid immutable target binding")?;
    let binding = browser_target_binding(target)?;
    if stored.get("browser_target_binding") != Some(&binding) {
        return Err(
            "QA target browser scope changed since this deployment; create a new deployment observation"
                .into(),
        );
    }
    Ok(())
}

fn candidate(store: &Store, id: &str) -> Result<crate::store::CandidateRow, String> {
    if id.len() > 256 || id.contains(['\0', '\r', '\n']) {
        return Err("candidate id is malformed".into());
    }
    store
        .candidate(id)
        .map_err(store_error)?
        .ok_or_else(|| format!("candidate {id:?} was not found"))
}

fn candidate_owner(candidate: &crate::store::CandidateRow) -> Result<String, String> {
    let session = candidate.owner_session_id.trim();
    if session.is_empty() {
        Err("candidate has no owner session; consequential QA cannot be authorized".into())
    } else {
        Ok(session.into())
    }
}

fn ensure_requester_scope(
    access: &QaAccess<'_>,
    candidate: &crate::store::CandidateRow,
    owner_session: &str,
) -> Result<(), String> {
    if let Some(channel) = access.requester_channel {
        if channel != candidate.channel {
            return Err("candidate is outside the calling session channel".into());
        }
    }
    if let Some(session_id) = access.requester_session_id {
        if session_id != owner_session {
            return Err("a harness can operate only on its own candidate".into());
        }
    }
    Ok(())
}

fn authorize(
    access: &QaAccess<'_>,
    channel: &str,
    session_id: &str,
    action: &str,
    target: &str,
    revision: Option<&str>,
    evidence: &Value,
) -> Result<String, String> {
    let policy = access
        .policy
        .read()
        .map_err(|_| "policy lock is unavailable")?;
    let decision = authority::decide(
        access.store,
        &policy,
        &AuthorityQuery {
            channel,
            session_id,
            action,
            target,
            revision,
            args: evidence,
        },
    )
    .map_err(|error| format!("could not decide authority: {error}"))?;
    if decision.verdict != Verdict::Allow {
        return Err(format!(
            "{} is not authorized for {}: {}",
            action,
            target,
            decision
                .reason
                .unwrap_or_else(|| "an active exact grant is required".into())
        ));
    }
    let id = uuid::Uuid::now_v7().to_string();
    let evidence_json = evidence.to_string();
    access
        .store
        .authority_action_begin(&ActionBegin {
            id: &id,
            grant_id: decision.rule_id.as_deref(),
            action,
            target,
            channel,
            session_id,
            revision,
            operation_id: None,
            evidence: &evidence_json,
        })
        .map_err(store_error)?;
    Ok(id)
}
fn fail_browser_action<T>(
    access: &QaAccess<'_>,
    action_id: &str,
    detail: impl Into<String>,
) -> Result<T, String> {
    let detail = detail.into();
    finish_action(access.store, action_id, "failed", &detail);
    Err(detail)
}

fn finish_action(store: &Store, id: &str, state: &str, outcome: &str) {
    if let Err(error) = store.authority_action_finish(id, state, outcome) {
        tracing::error!(%error, action_id = %id, "could not finish authority action record");
    }
}

fn run_identity_state(
    deployment: &QaDeploymentRow,
    before: &crate::qa::EnvironmentObservation,
    after: &crate::qa::EnvironmentObservation,
) -> &'static str {
    let provisional = BrowserRunRow {
        id: String::new(),
        deployment_id: deployment.id.clone(),
        candidate_id: deployment.candidate_id.clone(),
        channel: deployment.channel.clone(),
        target_id: deployment.target_id.clone(),
        authorized_origins_json: "[]".into(),
        test_credential: None,
        assertions_json: "[]".into(),
        outcome: "unknown".into(),
        environment_before: before.identity.clone(),
        environment_after: after.identity.clone(),
        evidence_state: "unknown".into(),
        log_tail: String::new(),
        started_ms: 0,
        finished_ms: 0,
    };
    if before.state == "fresh" && after.state == "fresh" {
        evidence_state(deployment, &provisional, Some(deployment))
    } else {
        "unknown"
    }
}

fn publish_changes(bus: &Bus, channel: &str, changes: Vec<tracon_sync::Change>) {
    for change in changes {
        bus.publish(crate::stream::Frame::Changes {
            channel: channel.into(),
            changes: vec![change],
        });
    }
}

fn limited_file(root: &Path, relative: &str, max: u64) -> Result<Vec<u8>, String> {
    if !safe_relative_path(relative) {
        return Err("runtime export named an unsafe path".into());
    }
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("exported evidence is missing: {error}"))?;
    if !metadata.file_type().is_file() || metadata.len() > max {
        return Err("exported evidence is not a bounded regular file".into());
    }
    fs::read(path).map_err(|error| format!("could not read exported evidence: {error}"))
}

fn collect_html_files(root: &Path, recipe: &PrototypeBuild) -> Result<Vec<HtmlFile>, String> {
    let output = root.join(&recipe.output_dir);
    let mut pending = vec![output.clone()];
    let mut files = Vec::new();
    let mut bytes = 0u64;
    while let Some(directory) = pending.pop() {
        for item in fs::read_dir(&directory)
            .map_err(|error| format!("prototype output is missing: {error}"))?
        {
            let item =
                item.map_err(|error| format!("could not enumerate prototype output: {error}"))?;
            let metadata = fs::symlink_metadata(item.path())
                .map_err(|error| format!("could not inspect prototype output: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err("prototype output contains a symlink".into());
            }
            if metadata.is_dir() {
                pending.push(item.path());
                continue;
            }
            if !metadata.is_file()
                || metadata.len() > HTML_BYTES
                || bytes.saturating_add(metadata.len()) > HTML_BYTES
            {
                return Err("prototype output contains an invalid or oversized file".into());
            }
            let relative = item
                .path()
                .strip_prefix(root)
                .map_err(|_| "prototype output escaped its export root")?
                .to_string_lossy()
                .replace('\\', "/");
            if !safe_relative_path(&relative) {
                return Err("prototype output has an unsafe path".into());
            }
            files.push(HtmlFile {
                path: relative,
                bytes: fs::read(item.path())
                    .map_err(|error| format!("could not read prototype output: {error}"))?,
            });
            bytes += metadata.len();
            if files.len() > HTML_FILES {
                return Err("prototype output has too many files".into());
            }
        }
    }
    if files.is_empty() {
        return Err("prototype output is empty".into());
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn store_error(error: crate::store::StoreError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(id: i64, sha: &str, jobs: Value) -> Value {
        json!({ "id": id, "sha": sha, "jobs": jobs })
    }

    #[test]
    fn no_candidate_pipelines_is_refused() {
        let error = select_deploy_job(&[], "deadbeef", "deploy").unwrap_err();
        assert!(error.contains("deadbeef"), "{error}");
    }

    #[test]
    fn a_pipeline_whose_sha_no_longer_matches_is_never_played() {
        // The list call filtered on this SHA, but the id is re-verified here:
        // a pipeline fetched by id is never trusted from the list alone.
        let statuses = vec![status(
            1,
            "other-sha",
            json!([{ "id": 9, "name": "deploy", "status": "manual" }]),
        )];
        let error = select_deploy_job(&statuses, "deadbeef", "deploy").unwrap_err();
        assert!(error.contains("deadbeef"), "{error}");
    }

    #[test]
    fn a_pipeline_with_no_matching_job_name_is_refused() {
        let statuses = vec![status(
            1,
            "deadbeef",
            json!([{ "id": 9, "name": "build", "status": "manual" }]),
        )];
        let error = select_deploy_job(&statuses, "deadbeef", "deploy").unwrap_err();
        assert!(error.contains("deploy"), "{error}");
    }

    #[test]
    fn a_matching_job_not_currently_manual_is_refused() {
        let statuses = vec![status(
            1,
            "deadbeef",
            json!([{ "id": 9, "name": "deploy", "status": "success" }]),
        )];
        assert!(select_deploy_job(&statuses, "deadbeef", "deploy").is_err());
    }

    #[test]
    fn an_exact_sha_match_with_a_manual_job_is_selected() {
        let statuses = vec![
            status(
                1,
                "other-sha",
                json!([{ "id": 5, "name": "deploy", "status": "manual" }]),
            ),
            status(
                2,
                "deadbeef",
                json!([{ "id": 9, "name": "deploy", "status": "manual" }]),
            ),
        ];
        let (pipeline_id, job_id) = select_deploy_job(&statuses, "deadbeef", "deploy").unwrap();
        assert_eq!(pipeline_id, 2);
        assert_eq!(job_id, 9);
    }

    #[test]
    fn sha_matching_is_case_insensitive() {
        let statuses = vec![status(
            1,
            "DEADBEEF",
            json!([{ "id": 9, "name": "deploy", "status": "manual" }]),
        )];
        let (pipeline_id, job_id) = select_deploy_job(&statuses, "deadbeef", "deploy").unwrap();
        assert_eq!(pipeline_id, 1);
        assert_eq!(job_id, 9);
    }
}
