//! What changed since the operator last decided on a review.
//!
//! A resubmission answers feedback, and the operator should be able to read
//! the answer rather than the whole change again. So beside the full diff
//! against the base, a review shows the diff from the last revision a verdict
//! was given on to the one on the screen, and, for each piece of feedback
//! given on the way, which files the next revision changed in response.
//!
//! The interdiff is read from Git in the review's worktree, between the two
//! revisions' commits. A revision whose commit the worktree no longer holds
//! (a branch rewritten and collected) says so rather than guessing; a branch
//! rebased between revisions shows what the base brought in as well, and the
//! response says so when the base moved.

use serde::Serialize;

use crate::store::{evidence::ReviewDecisionRow, ReviewRevisionRow, Store};

use super::{git, ReviewError};

/// One file changed between two revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: String,
    /// `None` for a binary file.
    pub added: Option<i64>,
    pub removed: Option<i64>,
}

/// The files changed from `from` to `to`.
pub async fn changed_files(
    worktree: &str,
    from: &str,
    to: &str,
) -> Result<Vec<FileChange>, ReviewError> {
    let numstat = git(
        worktree,
        "diff --numstat",
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            from,
            to,
        ],
    )
    .await?;
    Ok(numstat
        .lines()
        .filter_map(|line| {
            let mut cols = line.splitn(3, '\t');
            let added = cols.next()?.parse().ok();
            let removed = cols.next()?.parse().ok();
            let path = cols.next()?.to_string();
            Some(FileChange {
                path,
                added,
                removed,
            })
        })
        .collect())
}

/// The diff from `from` to `to`. Diff and textconv drivers are off, as they
/// are for the capture itself.
pub async fn interdiff(worktree: &str, from: &str, to: &str) -> Result<String, ReviewError> {
    git(
        worktree,
        "diff",
        &["diff", "--no-ext-diff", "--no-textconv", from, to],
    )
    .await
}

/// Feedback given on one revision, and what the next revision changed.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub revision_id: String,
    pub decision: String,
    pub source: String,
    pub reason: Option<String>,
    /// The operator sent an edit with the feedback.
    pub sent_edit: bool,
    pub decided_ms: i64,
    /// The revision that followed it, if one has.
    pub answered_by: Option<String>,
    /// What that revision changed, when the worktree can still say.
    pub files: Option<Vec<FileChange>>,
}

/// The last reviewed revision against the one on the screen.
#[derive(Debug, Clone, Serialize)]
pub struct SinceReviewed {
    pub revision_id: String,
    pub head_sha: String,
    pub created_ms: i64,
    /// The interdiff; `None` when the worktree cannot produce it.
    pub diff: Option<String>,
    pub files: Option<Vec<FileChange>>,
    /// Why there is no interdiff, in words.
    pub unavailable: Option<String>,
    /// Every piece of feedback given on this review, oldest first.
    pub responses: Vec<Response>,
}

/// What changed since the operator last decided, for the revision on the
/// screen. `None` for a first revision, or one nobody has decided on before.
pub async fn since_reviewed(
    store: &Store,
    worktree: Option<&str>,
    review_id: &str,
    current: &ReviewRevisionRow,
) -> Result<Option<SinceReviewed>, crate::store::StoreError> {
    let revisions = store.review_revisions(review_id)?;
    let decisions = store.review_decisions(review_id)?;
    let Some(position) = revisions.iter().position(|r| r.id == current.id) else {
        return Ok(None);
    };
    let decided = |revision: &ReviewRevisionRow| -> Vec<&ReviewDecisionRow> {
        decisions
            .iter()
            .filter(|d| {
                d.revision_id == revision.id
                    && !matches!(d.decision.as_str(), "approve" | "approved")
            })
            .collect()
    };
    let Some(last) = revisions[..position]
        .iter()
        .rev()
        .find(|revision| !decided(revision).is_empty())
    else {
        return Ok(None);
    };

    let mut responses = Vec::new();
    for (index, revision) in revisions[..position].iter().enumerate() {
        let next = revisions.get(index + 1);
        for decision in decided(revision) {
            let files = match (worktree, next) {
                (Some(worktree), Some(next)) => {
                    changed_files(worktree, &revision.head_sha, &next.head_sha)
                        .await
                        .ok()
                }
                _ => None,
            };
            responses.push(Response {
                revision_id: revision.id.clone(),
                decision: decision.decision.clone(),
                source: decision.source.clone(),
                reason: decision.reason.clone(),
                sent_edit: decision.patch.as_deref().is_some_and(|p| !p.is_empty()),
                decided_ms: decision.decided_ms,
                answered_by: next.map(|n| n.id.clone()),
                files,
            });
        }
    }

    let (diff, files, unavailable) = match worktree {
        None => (
            None,
            None,
            Some("the worktree this review was captured from is gone".to_string()),
        ),
        Some(_) if last.head_sha == current.head_sha => (
            Some(String::new()),
            Some(Vec::new()),
            Some("the commit is unchanged; only the prose was resubmitted".to_string()),
        ),
        Some(worktree) => {
            match (
                interdiff(worktree, &last.head_sha, &current.head_sha).await,
                changed_files(worktree, &last.head_sha, &current.head_sha).await,
            ) {
                (Ok(diff), Ok(files)) => (Some(diff), Some(files), None),
                _ => (
                    None,
                    None,
                    Some(format!(
                        "the worktree no longer holds {:.8}, the last reviewed commit; the \
                         branch was probably rewritten",
                        last.head_sha
                    )),
                ),
            }
        }
    };
    Ok(Some(SinceReviewed {
        revision_id: last.id.clone(),
        head_sha: last.head_sha.clone(),
        created_ms: last.created_ms,
        diff,
        files,
        unavailable,
        responses,
    }))
}
