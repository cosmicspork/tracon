//! A review that publishes after others (`submit_review`'s `after`): what
//! each of them has come to, and whether approving it now keeps the order.
//!
//! A dependency is satisfied once it was approved, its publication landed,
//! and, when that publication opened or updated a change, the change merged
//! on the forge, as following it last saw (`follow::forge_state`). Approved
//! and published but not merged reads `waiting on merge`. Until every one is
//! satisfied the operator's approval is refused unless it says why it goes
//! ahead anyway, which is kept as the verdict's reason, and an authority
//! grant does not publish it unattended.

use serde::Serialize;
use serde_json::{json, Value};

use crate::store::{ReviewRow, Store, StoreError};

/// The most reviews one review publishes after.
pub const MAX_AFTER: usize = 16;

/// What one dependency has come to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dependency {
    pub review_id: String,
    /// Its title as it publishes, or `None` when this node holds no review
    /// of that id (a peer's not mirrored here yet).
    pub title: Option<String>,
    /// The review's own state, or `unknown`.
    pub state: String,
    /// In a few words: `merged`, `published`, `waiting on merge`, `closed
    /// without merging`, `waiting on review`, `publishing`, `rejected` or
    /// `unknown`.
    pub says: String,
    pub satisfied: bool,
}

impl Dependency {
    /// `waits on <name> · <state>`, as an approval is refused with.
    pub fn waits_on(&self) -> String {
        format!("waits on {} · {}", self.name(), self.says)
    }

    fn name(&self) -> String {
        match &self.title {
            Some(title) => format!("{title} ({})", short(&self.review_id)),
            None => format!("review {}", short(&self.review_id)),
        }
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// What `review` has come to as a dependency, from what this node holds.
/// `forge` is the state its change stands in on the forge, when known.
fn of(review: &ReviewRow, forge: Option<&str>) -> Dependency {
    let target = serde_json::from_str::<crate::review::publish::Target>(&review.target).ok();
    // A first push or a direct push opens and updates no change: landing is
    // all there is to wait for.
    let opens_no_change = target.as_ref().is_some_and(|t| t.initial || t.direct);
    let published = review
        .publish_result
        .as_deref()
        .is_some_and(|result| !result.trim().is_empty());
    let (says, satisfied) = match review.state.as_str() {
        "approved" if published && opens_no_change => ("published", true),
        "approved" if published => match forge {
            Some("merged") => ("merged", true),
            Some("closed") => ("closed without merging", false),
            _ => ("waiting on merge", false),
        },
        "approved" | "publishing" => ("publishing", false),
        "rejected" => ("rejected", false),
        "new" | "claimed" | "revising" => ("waiting on review", false),
        _ => ("unknown", false),
    };
    Dependency {
        review_id: review.id.clone(),
        title: Some(review.approved_title().to_string()),
        state: review.state.clone(),
        says: says.into(),
        satisfied,
    }
}

/// What the dependency `id` has come to, as this node holds it.
pub fn status(store: &Store, id: &str) -> Result<Dependency, StoreError> {
    let Some(review) = store.get_review(id)? else {
        return Ok(Dependency {
            review_id: id.to_string(),
            title: None,
            state: "unknown".into(),
            says: "unknown".into(),
            satisfied: false,
        });
    };
    let forge = crate::follow::forge_state(store, &review)?;
    Ok(of(
        &review,
        forge.as_ref().and_then(|state| state["state"].as_str()),
    ))
}

/// `deps` with each one waiting on a merge that following never reads (a
/// review no session submitted, held here) asked of the forge now.
pub async fn looked(
    tools: &crate::mcp::Tools,
    store: &Store,
    node_id: &str,
    deps: Vec<Dependency>,
) -> Vec<Dependency> {
    let mut out = Vec::with_capacity(deps.len());
    for dep in deps {
        let review = (dep.says == "waiting on merge")
            .then(|| store.get_review(&dep.review_id).ok().flatten())
            .flatten()
            .filter(|r| r.session_id.is_none() && r.node_id == node_id);
        let Some(review) = review else {
            out.push(dep);
            continue;
        };
        match crate::follow::look_now(tools, store, &review).await {
            Some(state) => out.push(of(&review, Some(&state))),
            None => out.push(dep),
        }
    }
    out
}

/// What each review `review` publishes after has come to.
pub fn after(store: &Store, review: &ReviewRow) -> Result<Vec<Dependency>, StoreError> {
    store
        .review_dependencies(&review.id)?
        .iter()
        .map(|id| status(store, id))
        .collect()
}

/// The dependencies of `review` not satisfied yet.
pub fn unmet(store: &Store, review: &ReviewRow) -> Result<Vec<Dependency>, StoreError> {
    Ok(after(store, review)?
        .into_iter()
        .filter(|d| !d.satisfied)
        .collect())
}

/// `waits on <name> · <state>` for each of `unmet`, joined.
pub fn waits_on(unmet: &[Dependency]) -> String {
    unmet
        .iter()
        .map(Dependency::waits_on)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The review page's account: what it publishes after (`after`, each with
/// where it stands), and what publishes after it.
pub fn view(store: &Store, review: &ReviewRow, after: &[Dependency]) -> Result<Value, StoreError> {
    let before: Vec<Value> = store
        .review_dependents(&review.id)?
        .into_iter()
        .map(|id| {
            let row = store.get_review(&id)?;
            Ok(json!({
                "review_id": id,
                "title": row.as_ref().map(|r| r.approved_title().to_string()),
                "state": row.as_ref().map(|r| r.state.clone()),
            }))
        })
        .collect::<Result<_, StoreError>>()?;
    Ok(json!({ "after": after, "before": before }))
}

/// `after` as `submit_review` was given it: `None` when left out, which
/// keeps what a resubmitted review named.
pub fn after_arg(args: &Value) -> Result<Option<Vec<String>>, String> {
    let list = match args.get("after") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(list)) => list,
        Some(_) => return Err("after is a list of review ids".into()),
    };
    let mut ids: Vec<String> = Vec::with_capacity(list.len());
    for value in list {
        let id = value
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or("after is a list of review ids")?;
        if !ids.iter().any(|held| held == id) {
            ids.push(id.to_string());
        }
    }
    if ids.len() > MAX_AFTER {
        return Err(format!(
            "after names {} reviews; a review publishes after at most {MAX_AFTER}",
            ids.len()
        ));
    }
    Ok(Some(ids))
}

/// Refuse an `after` that names anything but a code review on `channel` that
/// may still land, or that would close a cycle back to `review_id` (the
/// review being resubmitted; none for a new one).
pub fn check(
    store: &Store,
    channel: &str,
    review_id: Option<&str>,
    after: &[String],
) -> Result<(), String> {
    for id in after {
        if Some(id.as_str()) == review_id {
            return Err(format!("after names review {id} itself"));
        }
        let review = store
            .get_review(id)
            .map_err(|e| e.to_string())?
            .filter(|r| r.channel == channel && r.kind != crate::store::reports::KIND)
            .ok_or_else(|| format!("after names {id}, which is no review on channel {channel}"))?;
        if review.state == "rejected" {
            let reason = review
                .verdict_reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(|reason| format!(" (\"{reason}\")"))
                .unwrap_or_default();
            return Err(format!(
                "after names review {id}, which was rejected{reason} and will never land; \
                 leave it out, or name the review that replaces it"
            ));
        }
        if let Some(me) = review_id {
            if store.review_depends_on(id, me).map_err(|e| e.to_string())? {
                return Err(format!(
                    "after names review {id}, which already publishes after this one; that \
                     would be a cycle"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: &str, target: Value, publish_result: Option<&str>) -> ReviewRow {
        serde_json::from_value(json!({
            "id": "0192aaaa-bbbb", "session_id": null, "node_id": "n1", "channel": "work",
            "kind": "pr", "title": "feat: base", "body": "", "edited_title": null,
            "edited_body": null, "provider": "github", "target": target.to_string(),
            "diff": "", "files": "[]", "head_sha": "h", "base_ref": "main", "added": 0,
            "removed": 0, "state": state, "verdict_reason": null,
            "publish_result": publish_result, "claimed_ms": null, "created_ms": 1,
            "created_mono_ms": 0, "resolved_mono_ms": null, "updated_ms": 1,
        }))
        .unwrap()
    }

    #[test]
    fn a_dependency_is_satisfied_once_its_change_merged_or_it_opened_none() {
        let pr =
            json!({ "provider": "github", "project": "o/n", "base": "main", "branch": "feat/x" });
        let direct = json!({ "provider": "github", "project": "o/n", "base": "main", "branch": "main", "direct": true });
        let url = Some("https://github.test/pull/1");
        for (review, forge, says, satisfied) in [
            (
                row("new", pr.clone(), None),
                None,
                "waiting on review",
                false,
            ),
            (
                row("revising", pr.clone(), None),
                None,
                "waiting on review",
                false,
            ),
            (
                row("publishing", pr.clone(), None),
                None,
                "publishing",
                false,
            ),
            (row("rejected", pr.clone(), None), None, "rejected", false),
            (
                row("approved", pr.clone(), url),
                None,
                "waiting on merge",
                false,
            ),
            (
                row("approved", pr.clone(), url),
                Some("open"),
                "waiting on merge",
                false,
            ),
            (
                row("approved", pr.clone(), url),
                Some("closed"),
                "closed without merging",
                false,
            ),
            (
                row("approved", pr.clone(), url),
                Some("merged"),
                "merged",
                true,
            ),
            (row("approved", direct, url), None, "published", true),
        ] {
            let d = of(&review, forge);
            assert_eq!(
                (d.says.as_str(), d.satisfied),
                (says, satisfied),
                "{review:?} {forge:?}"
            );
        }
        let d = of(&row("new", pr, None), None);
        assert_eq!(
            d.waits_on(),
            "waits on feat: base (0192aaaa) · waiting on review"
        );
    }

    #[test]
    fn after_is_a_list_of_distinct_ids() {
        assert_eq!(after_arg(&json!({})).unwrap(), None);
        assert_eq!(
            after_arg(&json!({ "after": ["a", " a ", "b"] })).unwrap(),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(after_arg(&json!({ "after": [] })).unwrap(), Some(vec![]));
        assert!(after_arg(&json!({ "after": "a" })).is_err());
        assert!(after_arg(&json!({ "after": [""] })).is_err());
        let many: Vec<String> = (0..=MAX_AFTER).map(|i| i.to_string()).collect();
        assert!(after_arg(&json!({ "after": many })).is_err());
    }
}
