//! Following a published request after it opens.
//!
//! Approval publishes a change; what happens to it next happens on the
//! forge: CI runs, the request is marked ready, reviewers approve or ask for
//! changes, comments arrive, it is merged or closed. The node polls each
//! request it opened, through the same brokered credential and the same
//! status calls the forge tools make, compares what it sees with what it saw
//! last, and records each change as an event on the session that submitted
//! the review, which carries the session's work item. A change is pushed to
//! the operator's phones named ("CI failed on `node`", "marked ready", "2 new
//! comments"); a session still attached reads the request itself with
//! `pr_status` or `mr_status`.
//!
//! A request is followed until it is merged or closed. Following only reads:
//! it never merges, approves, retries or comments. Polling only — webhooks
//! would need the node reachable from the forge — and only for reviews a
//! session submitted, since the events hang on that session.
//!
//! A pipeline an agent started (`pipeline_run`, `job_play`, `deploy`) is
//! followed on the same tick, for whoever started it: each job result is an
//! event on that session (or the channel's log, for a harness the operator
//! runs), and the pipeline finishing, failing or stopping at a manual job is
//! pushed. It is followed on its own, so it outlives the request whose merge
//! triggered it. Following a pipeline never retries, cancels or plays a job.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::mcp::{github, gitlab, CallContext, Tools};
use crate::session::state::event_kind as ek;
use crate::store::pipeline_follow::{NewPipelineFollow, PipelineFollowRow};
use crate::store::{now_ms, publication::PublicationRow, NewEvent};

/// How often each open request is read.
pub const EVERY: Duration = Duration::from_secs(120);

/// A followed pipeline that has not moved for this long is let go: no
/// runner picked it up, or a job hangs, and either is for a person to find.
pub const PIPELINE_STALE: Duration = Duration::from_secs(24 * 60 * 60);

/// What one look at a request saw, kept on its event to compare the next
/// look with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// `open`, `merged` or `closed`.
    pub state: String,
    pub draft: bool,
    /// Each check's name and `passed`, `failed` or `pending`.
    pub checks: BTreeMap<String, String>,
    /// `approved`, `changes_requested`, or none.
    pub decision: Option<String>,
    /// Comments and reviews, counted together.
    pub comments: u64,
}

impl Snapshot {
    pub fn closed(&self) -> bool {
        self.state == "merged" || self.state == "closed"
    }

    /// From `pr_status`.
    pub fn from_github(status: &Value) -> Self {
        let state = if status["merged"] == true {
            "merged"
        } else if status["state"] == "closed" {
            "closed"
        } else {
            "open"
        };
        let checks = status["checks"]["runs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|run| {
                let name = run["name"].as_str()?.to_string();
                let verdict = match (run["status"].as_str(), run["conclusion"].as_str()) {
                    (Some("completed"), Some("success" | "neutral" | "skipped")) => "passed",
                    (Some("completed"), _) => "failed",
                    _ => "pending",
                };
                Some((name, verdict.to_string()))
            })
            .collect();
        let decision = match status["review_decision"].as_str() {
            Some("APPROVED") | Some("approved") => Some("approved".to_string()),
            Some("CHANGES_REQUESTED") | Some("changes_requested") => {
                Some("changes_requested".to_string())
            }
            _ => None,
        };
        let reviews = status["reviews"].as_array().map_or(0, Vec::len) as u64;
        Self {
            state: state.into(),
            draft: status["draft"] == true,
            checks,
            decision,
            comments: status["comments"].as_u64().unwrap_or(0) + reviews,
        }
    }

    /// From `mr_status`.
    pub fn from_gitlab(status: &Value) -> Self {
        let state = match status["state"].as_str() {
            Some("merged") => "merged",
            Some("closed") => "closed",
            _ => "open",
        };
        let mut checks = BTreeMap::new();
        if let Some(pipeline) = status["pipeline"].as_str() {
            let verdict = match pipeline {
                "success" | "skipped" | "manual" => "passed",
                "failed" | "canceled" => "failed",
                _ => "pending",
            };
            checks.insert("pipeline".to_string(), verdict.to_string());
        }
        Self {
            state: state.into(),
            draft: status["draft"] == true,
            checks,
            decision: (status["approved"] == true).then(|| "approved".to_string()),
            comments: status["notes"].as_u64().unwrap_or(0),
        }
    }
}

/// What changed between two looks, each in the words a push carries.
pub fn changes(before: &Snapshot, now: &Snapshot) -> Vec<String> {
    let mut said = Vec::new();
    if before.state != now.state {
        said.push(match now.state.as_str() {
            "merged" => "merged".to_string(),
            "closed" => "closed without merging".to_string(),
            _ => "reopened".to_string(),
        });
    }
    if before.draft && !now.draft {
        said.push("marked ready".into());
    } else if !before.draft && now.draft {
        said.push("converted to draft".into());
    }
    for (name, verdict) in &now.checks {
        if verdict == "failed" && before.checks.get(name) != Some(verdict) {
            said.push(format!("CI failed on `{name}`"));
        }
    }
    let all_passed = |checks: &BTreeMap<String, String>| {
        !checks.is_empty() && checks.values().all(|verdict| verdict == "passed")
    };
    if all_passed(&now.checks) && !all_passed(&before.checks) {
        said.push("CI passed".into());
    }
    if before.decision != now.decision {
        match now.decision.as_deref() {
            Some("approved") => said.push("approved".into()),
            Some("changes_requested") => said.push("changes requested".into()),
            _ => {}
        }
    }
    if now.comments > before.comments {
        let n = now.comments - before.comments;
        said.push(match n {
            1 => "1 new comment".to_string(),
            n => format!("{n} new comments"),
        });
    }
    said
}

/// The request's number on its forge, from the URL publication recorded.
pub fn number_of(url: &str) -> Option<u64> {
    url.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

/// One look at one request, through the brokered credential.
async fn look(tools: &Tools, publication: &PublicationRow) -> Result<Snapshot, String> {
    let url = publication
        .result
        .as_deref()
        .ok_or("the publication recorded no request")?;
    let number = number_of(url).ok_or_else(|| format!("no request number in {url}"))?;
    let ctx = CallContext::external(None, &publication.channel, &publication.node_id);
    match publication.provider.as_str() {
        "github" => github::call(
            &tools.broker,
            &tools.http,
            &ctx,
            github::PR_STATUS,
            &json!({ "repo": publication.project, "number": number }),
            None,
        )
        .await
        .map(|status| Snapshot::from_github(&status)),
        "gitlab" => gitlab::call(
            &tools.broker,
            &tools.http,
            &ctx,
            gitlab::MR_STATUS,
            &json!({ "project": publication.project, "iid": number }),
            None,
        )
        .await
        .map(|status| Snapshot::from_gitlab(&status)),
        other => Err(format!("no forge named {other}")),
    }
}

/// What one pass found for one request.
#[derive(Debug, Clone, Serialize)]
pub struct Followed {
    pub publication_id: String,
    pub review_id: String,
    pub changes: Vec<String>,
}

/// Look at every request this node opened and has not seen close, record
/// what changed, and return it.
pub async fn tick(tools: &Tools) -> Vec<Followed> {
    let Some(access) = tools.session.get() else {
        return Vec::new();
    };
    let store = &access.store;
    let manager = &access.manager;
    let opened = store
        .publications_opened(manager.node_id())
        .unwrap_or_default();
    let mut followed = Vec::new();
    for publication in opened {
        let Ok(Some(review)) = store.get_review(&publication.review_id) else {
            continue;
        };
        let Some(session_id) = review.session_id.clone() else {
            continue;
        };
        let before: Option<Snapshot> = store
            .latest_event_payload(&session_id, ek::FORGE_FOLLOW, &publication.id)
            .ok()
            .flatten()
            .and_then(|payload| serde_json::from_value(payload["snapshot"].clone()).ok());
        if before.as_ref().is_some_and(Snapshot::closed) {
            continue;
        }
        let now = match look(tools, &publication).await {
            Ok(now) => now,
            Err(error) => {
                tracing::debug!(publication = %publication.id, %error, "request not read");
                continue;
            }
        };
        // The first look is the baseline: what the request already was
        // when the node began following it is not news.
        let said = match &before {
            Some(before) if before == &now => continue,
            Some(before) => changes(before, &now),
            None => Vec::new(),
        };
        manager.record_new_event(NewEvent {
            session_id: session_id.clone(),
            work_item_id: None,
            kind: ek::FORGE_FOLLOW.into(),
            ref_id: Some(publication.id.clone()),
            payload: json!({
                "review_id": review.id,
                "url": publication.result,
                "changes": said,
                "snapshot": now,
            }),
            at_ms: now_ms(),
            mono_ms: 0,
        });
        if !said.is_empty() {
            notify(store, manager.cfg(), &review, &said).await;
            followed.push(Followed {
                publication_id: publication.id.clone(),
                review_id: review.id.clone(),
                changes: said,
            });
        }
    }
    followed
}

/// Push what changed to the phones subscribed here, unless the review's
/// channel is quiet.
async fn notify(
    store: &Arc<crate::store::Store>,
    cfg: &crate::config::Config,
    review: &crate::store::ReviewRow,
    said: &[String],
) {
    if !crate::notify::channel_pushes(store, &review.channel) {
        return;
    }
    let notification = crate::notify::Notification::forge(
        &review.id,
        review
            .edited_title
            .clone()
            .unwrap_or_else(|| review.title.clone()),
        said.join(" · "),
    );
    let devices = store.push_subscriptions_live(now_ms()).unwrap_or_default();
    for device in devices {
        crate::notify::deliver(store, cfg, &device, &notification, now_ms()).await;
    }
}

/// What one look at a pipeline saw.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineSnapshot {
    pub status: String,
    /// Each job by id.
    pub jobs: BTreeMap<i64, Job>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub name: String,
    pub status: String,
}

impl PipelineSnapshot {
    /// From `pipeline_status`.
    pub fn from_gitlab(status: &Value) -> Self {
        Self {
            status: status["status"].as_str().unwrap_or_default().to_string(),
            jobs: status["jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|job| {
                    Some((
                        job["id"].as_i64()?,
                        Job {
                            name: job["name"].as_str().unwrap_or_default().to_string(),
                            status: job["status"].as_str().unwrap_or_default().to_string(),
                        },
                    ))
                })
                .collect(),
        }
    }

    pub fn finished(&self) -> bool {
        gitlab::finished(&self.status)
    }
}

/// What a pipeline did between two looks, and whether it is worth a push:
/// job results are recorded, the pipeline finishing, failing or stopping at
/// a manual job is pushed too.
pub fn pipeline_changes(
    before: Option<&PipelineSnapshot>,
    now: &PipelineSnapshot,
) -> (Vec<String>, bool) {
    let mut said = Vec::new();
    for (id, job) in &now.jobs {
        let word = match job.status.as_str() {
            "success" => "passed",
            "failed" => "failed",
            "canceled" => "canceled",
            // A skipped job is what follows a failure; the failure is news.
            _ => continue,
        };
        if before.and_then(|b| b.jobs.get(id)).map(|j| &j.status) != Some(&job.status) {
            said.push(format!("`{}` {word}", job.name));
        }
    }
    if before.map(|b| &b.status) == Some(&now.status) {
        return (said, false);
    }
    let pipeline = match now.status.as_str() {
        "success" => "pipeline passed".to_string(),
        "failed" => "pipeline failed".to_string(),
        "canceled" => "pipeline canceled".to_string(),
        "skipped" => "pipeline skipped".to_string(),
        "manual" => {
            let waiting: Vec<String> = now
                .jobs
                .values()
                .filter(|job| job.status == "manual")
                .map(|job| format!("`{}`", job.name))
                .collect();
            if waiting.is_empty() {
                "waiting on a manual job".to_string()
            } else {
                format!("waiting on {} to be played", waiting.join(", "))
            }
        }
        _ => return (said, false),
    };
    said.push(pipeline);
    (said, true)
}

/// Who started a followed pipeline, as the call that reads it.
fn caller_of(row: &PipelineFollowRow) -> CallContext {
    match &row.session_id {
        Some(session_id) => CallContext::session(session_id, &row.channel, &row.node_id),
        None => CallContext::external(row.lane.clone(), &row.channel, &row.node_id),
    }
}

async fn look_at_pipeline(
    tools: &Tools,
    ctx: &CallContext,
    project: &str,
    pipeline_id: i64,
) -> Result<Value, String> {
    gitlab::call(
        &tools.broker,
        &tools.http,
        ctx,
        gitlab::PIPELINE_STATUS,
        &json!({ "project": project, "pipeline_id": pipeline_id }),
        None,
    )
    .await
}

/// After `pipeline_run`, `job_play` or `deploy` succeeded: follow the
/// pipeline it started for its caller, from what it looks like now, and say
/// so in the result.
pub async fn subscribe(
    tools: &Tools,
    ctx: &CallContext,
    name: &str,
    args: &Value,
    mut started: Value,
) -> Value {
    let Some(access) = tools.session.get() else {
        return started;
    };
    let pipeline_id = match name {
        gitlab::PIPELINE_RUN => started["id"].as_i64(),
        gitlab::JOB_PLAY => started["pipeline_id"].as_i64(),
        _ => args["pipeline_id"].as_i64(),
    };
    let (Some(pipeline_id), Some(project)) = (pipeline_id, args["project"].as_str()) else {
        return started;
    };
    // The baseline: what was already there when following began is not news.
    let now = look_at_pipeline(tools, ctx, project, pipeline_id)
        .await
        .ok();
    let snapshot = now.as_ref().map(|status| {
        serde_json::to_value(PipelineSnapshot::from_gitlab(status)).unwrap_or_default()
    });
    let web_url = now
        .as_ref()
        .and_then(|status| status["web_url"].as_str().map(str::to_string));
    let followed = access.store.follow_pipeline(&NewPipelineFollow {
        node_id: &ctx.node_id,
        channel: &ctx.channel,
        session_id: ctx.session_id(),
        lane: ctx.lane(),
        provider: "gitlab",
        project,
        pipeline_id,
        started_by: name,
        web_url: web_url.as_deref(),
        snapshot: snapshot.as_ref(),
        now_ms: now_ms(),
    });
    let id = match followed {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(%error, pipeline_id, "pipeline not followed");
            return started;
        }
    };
    access.manager.record_for(
        ctx,
        ek::PIPELINE_FOLLOW,
        Some(&id),
        json!({
            "project": project,
            "pipeline_id": pipeline_id,
            "url": web_url,
            "started_by": name,
            "changes": ["following"],
        }),
    );
    if let Some(result) = started.as_object_mut() {
        result.insert(
            "following".into(),
            json!({
                "pipeline_id": pipeline_id,
                "note": "The node follows this pipeline: each job result is recorded and the \
                         operator is told when it finishes, fails or stops at a manual job. \
                         pipeline_wait waits for it here.",
            }),
        );
    }
    started
}

/// What one pass found for one pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct FollowedPipeline {
    pub id: String,
    pub pipeline_id: i64,
    pub changes: Vec<String>,
    pub pushed: bool,
}

/// Look at every pipeline this node follows, record what moved, push what
/// the operator should know, and return it.
pub async fn tick_pipelines(tools: &Tools) -> Vec<FollowedPipeline> {
    let Some(access) = tools.session.get() else {
        return Vec::new();
    };
    let store = &access.store;
    let manager = &access.manager;
    let followed = store
        .pipelines_followed(manager.node_id())
        .unwrap_or_default();
    let mut out = Vec::new();
    for row in followed {
        let ctx = caller_of(&row);
        if now_ms() - row.changed_ms > PIPELINE_STALE.as_millis() as i64 {
            let _ = store.pipeline_follow_drop(&row.id);
            manager.record_for(
                &ctx,
                ek::PIPELINE_FOLLOW,
                Some(&row.id),
                json!({
                    "project": row.project,
                    "pipeline_id": row.pipeline_id,
                    "url": row.web_url,
                    "changes": ["no longer followed: nothing moved in a day"],
                }),
            );
            continue;
        }
        let now = match look_at_pipeline(tools, &ctx, &row.project, row.pipeline_id).await {
            Ok(status) => PipelineSnapshot::from_gitlab(&status),
            Err(error) => {
                tracing::debug!(pipeline = %row.id, %error, "pipeline not read");
                continue;
            }
        };
        let before: Option<PipelineSnapshot> = row
            .snapshot
            .clone()
            .and_then(|snapshot| serde_json::from_value(snapshot).ok());
        let moved = before.as_ref() != Some(&now);
        let (said, push) = pipeline_changes(before.as_ref(), &now);
        let snapshot = serde_json::to_value(&now).unwrap_or_default();
        if let Err(error) =
            store.pipeline_follow_seen(&row.id, &snapshot, moved, now.finished(), now_ms())
        {
            tracing::warn!(pipeline = %row.id, %error, "pipeline look not kept");
            continue;
        }
        if said.is_empty() {
            continue;
        }
        manager.record_for(
            &ctx,
            ek::PIPELINE_FOLLOW,
            Some(&row.id),
            json!({
                "project": row.project,
                "pipeline_id": row.pipeline_id,
                "url": row.web_url,
                "status": now.status,
                "changes": said,
            }),
        );
        if push && crate::notify::channel_pushes(store, &row.channel) {
            let notification = crate::notify::Notification::pipeline(
                row.session_id.as_deref(),
                format!("{} pipeline {}", row.project, row.pipeline_id),
                said.join(" · "),
                &row.id,
            );
            let devices = store.push_subscriptions_live(now_ms()).unwrap_or_default();
            for device in devices {
                crate::notify::deliver(store, manager.cfg(), &device, &notification, now_ms())
                    .await;
            }
        }
        out.push(FollowedPipeline {
            id: row.id.clone(),
            pipeline_id: row.pipeline_id,
            changes: said,
            pushed: push,
        });
    }
    out
}

/// Follow every request this node opened and every pipeline an agent
/// started, for as long as it runs.
pub async fn run(tools: Arc<Tools>) {
    let mut every = tokio::time::interval(EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        tick(&tools).await;
        tick_pipelines(&tools).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(state: &str, draft: bool, checks: &[(&str, &str)], comments: u64) -> Snapshot {
        Snapshot {
            state: state.into(),
            draft,
            checks: checks
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            decision: None,
            comments,
        }
    }

    #[test]
    fn each_change_is_named() {
        let before = snap("open", true, &[("node", "pending"), ("spa", "passed")], 1);
        let now = snap("open", false, &[("node", "failed"), ("spa", "passed")], 3);
        assert_eq!(
            changes(&before, &now),
            ["marked ready", "CI failed on `node`", "2 new comments"]
        );
        let fixed = snap("open", false, &[("node", "passed"), ("spa", "passed")], 3);
        assert_eq!(changes(&now, &fixed), ["CI passed"]);
        // A failure already reported is not reported again.
        assert!(changes(&now, &now).is_empty());
        let mut approved = fixed.clone();
        approved.decision = Some("approved".into());
        approved.state = "merged".into();
        assert_eq!(changes(&fixed, &approved), ["merged", "approved"]);
        assert!(approved.closed());
    }

    #[test]
    fn a_forges_status_reads_as_one_snapshot() {
        let github = Snapshot::from_github(&json!({
            "state": "closed", "merged": true, "draft": false, "comments": 2,
            "review_decision": "APPROVED", "reviews": [{}, {}],
            "checks": { "runs": [
                { "name": "test", "status": "completed", "conclusion": "failure" },
                { "name": "lint", "status": "in_progress", "conclusion": null },
            ]},
        }));
        assert_eq!(github.state, "merged");
        assert_eq!(github.checks["test"], "failed");
        assert_eq!(github.checks["lint"], "pending");
        assert_eq!(github.decision.as_deref(), Some("approved"));
        assert_eq!(github.comments, 4);
        let gitlab = Snapshot::from_gitlab(&json!({
            "state": "opened", "draft": true, "pipeline": "failed", "notes": 5, "approved": false,
        }));
        assert_eq!(gitlab.state, "open");
        assert!(gitlab.draft);
        assert_eq!(gitlab.checks["pipeline"], "failed");
        assert_eq!(gitlab.decision, None);
    }

    fn pipeline(status: &str, jobs: &[(i64, &str, &str)]) -> PipelineSnapshot {
        PipelineSnapshot {
            status: status.into(),
            jobs: jobs
                .iter()
                .map(|(id, name, status)| {
                    (
                        *id,
                        Job {
                            name: name.to_string(),
                            status: status.to_string(),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn a_pipelines_job_results_are_recorded_and_its_end_is_pushed() {
        let started = pipeline(
            "running",
            &[(1, "build", "running"), (2, "test", "created")],
        );
        let built = pipeline(
            "running",
            &[(1, "build", "success"), (2, "test", "running")],
        );
        assert_eq!(
            pipeline_changes(Some(&started), &built),
            (vec!["`build` passed".to_string()], false)
        );
        let failed = pipeline(
            "failed",
            &[
                (1, "build", "success"),
                (2, "test", "failed"),
                (3, "deploy", "skipped"),
            ],
        );
        assert_eq!(
            pipeline_changes(Some(&built), &failed),
            (
                vec!["`test` failed".to_string(), "pipeline failed".to_string()],
                true
            )
        );
        assert!(failed.finished());
        let stopped = pipeline(
            "manual",
            &[(1, "build", "success"), (4, "staging", "manual")],
        );
        assert_eq!(
            pipeline_changes(Some(&built), &stopped),
            (vec!["waiting on `staging` to be played".to_string()], true)
        );
        // Nothing moved, nothing said.
        assert_eq!(pipeline_changes(Some(&built), &built), (vec![], false));
        // Without a baseline every result so far is news.
        assert_eq!(pipeline_changes(None, &built).0, ["`build` passed"]);
    }

    #[test]
    fn the_number_is_read_from_the_url() {
        assert_eq!(number_of("https://github.com/o/r/pull/12"), Some(12));
        assert_eq!(
            number_of("https://gitlab.com/g/p/-/merge_requests/7/"),
            Some(7)
        );
        assert_eq!(number_of("https://forge/x"), None);
    }
}
