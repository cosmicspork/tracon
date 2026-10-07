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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::mcp::{github, gitlab, CallContext, Tools};
use crate::session::state::event_kind as ek;
use crate::store::{now_ms, publication::PublicationRow, NewEvent};

/// How often each open request is read.
pub const EVERY: Duration = Duration::from_secs(120);

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

/// Follow every request this node opened, for as long as it runs.
pub async fn run(tools: Arc<Tools>) {
    let mut every = tokio::time::interval(EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        tick(&tools).await;
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
