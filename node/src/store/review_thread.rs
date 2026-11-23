//! The operator's review threads: feedback on a review that can be answered
//! point by point instead of one note.
//!
//! A thread is general, on a file, or on a line or range of lines, and is
//! bound to the revision it was written on and, unless general, to a path
//! and a side of the diff. It stays bound there: a resubmission that changes
//! the lines leaves the thread where it was, reading outdated on the new
//! revision (`review::threads`), and never moves it onto other code. Threads
//! outlive resubmission, and each carries the operator's and the agent's
//! comments in order. They are held by the node that owns the review; a peer
//! reads them in the review's detail and changes them through the owner.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{now_ms, Result, Store};

/// Open, and waiting on the agent.
pub const UNRESOLVED: &str = "unresolved";
/// The agent said a revision addresses it; the operator resolves or reopens.
pub const ADDRESSED: &str = "addressed";
/// Settled by the operator.
pub const RESOLVED: &str = "resolved";

/// Who wrote a comment.
pub const OPERATOR: &str = "operator";
pub const AGENT: &str = "agent";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewThreadRow {
    pub id: String,
    pub review_id: String,
    /// The revision it was written on, and stays anchored to.
    pub revision_id: String,
    /// `general`, `file` or `line`.
    pub kind: String,
    pub path: Option<String>,
    /// `old` or `new`: which side of the diff its lines are numbered on.
    pub side: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    /// A line thread's lines as that revision's diff showed them; a file
    /// thread's hash of that file's diff.
    pub anchor: Option<String>,
    pub state: String,
    /// The revision the agent said addresses it.
    pub addressed_revision_id: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewThreadComment {
    pub id: String,
    pub thread_id: String,
    /// [`OPERATOR`] or [`AGENT`].
    pub author: String,
    pub body: String,
    /// The revision the review was on when it was written.
    pub revision_id: Option<String>,
    pub created_ms: i64,
}

/// What the agent said of one thread with a resubmission.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadAnswer {
    pub thread_id: String,
    pub reply: Option<String>,
    pub addressed: bool,
}

impl ReviewThreadRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            review_id: r.get("review_id")?,
            revision_id: r.get("revision_id")?,
            kind: r.get("kind")?,
            path: r.get("path")?,
            side: r.get("side")?,
            start_line: r.get("start_line")?,
            end_line: r.get("end_line")?,
            anchor: r.get("anchor")?,
            state: r.get("state")?,
            addressed_revision_id: r.get("addressed_revision_id")?,
            created_ms: r.get("created_ms")?,
            updated_ms: r.get("updated_ms")?,
        })
    }
}

impl ReviewThreadComment {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            thread_id: r.get("thread_id")?,
            author: r.get("author")?,
            body: r.get("body")?,
            revision_id: r.get("revision_id")?,
            created_ms: r.get("created_ms")?,
        })
    }
}

fn comment_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

impl Store {
    /// Open `thread` with the operator's first comment, `body`.
    pub fn insert_review_thread(&self, thread: &ReviewThreadRow, body: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO review_thread (id, review_id, revision_id, kind, path, side, start_line,
                end_line, anchor, state, addressed_revision_id, created_ms, updated_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                thread.id,
                thread.review_id,
                thread.revision_id,
                thread.kind,
                thread.path,
                thread.side,
                thread.start_line,
                thread.end_line,
                thread.anchor,
                thread.state,
                thread.addressed_revision_id,
                thread.created_ms,
                thread.updated_ms,
            ],
        )?;
        tx.execute(
            "INSERT INTO review_thread_comment (id, thread_id, author, body, revision_id, created_ms)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                comment_id(),
                thread.id,
                OPERATOR,
                body,
                thread.revision_id,
                thread.created_ms
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn review_thread(&self, id: &str) -> Result<Option<ReviewThreadRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM review_thread WHERE id=?1",
            [id],
            ReviewThreadRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// A review's threads, oldest first, each with its comments in order.
    pub fn review_threads(
        &self,
        review_id: &str,
    ) -> Result<Vec<(ReviewThreadRow, Vec<ReviewThreadComment>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM review_thread WHERE review_id=?1 ORDER BY created_ms ASC, id ASC",
        )?;
        let threads: Vec<ReviewThreadRow> = stmt
            .query_map([review_id], ReviewThreadRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        let mut stmt = conn.prepare(
            "SELECT c.* FROM review_thread_comment c JOIN review_thread t ON t.id = c.thread_id
             WHERE t.review_id=?1 ORDER BY c.created_ms ASC, c.id ASC",
        )?;
        let mut comments: Vec<ReviewThreadComment> = stmt
            .query_map([review_id], ReviewThreadComment::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(threads
            .into_iter()
            .map(|thread| {
                let (own, rest) = comments
                    .drain(..)
                    .partition(|c: &ReviewThreadComment| c.thread_id == thread.id);
                comments = rest;
                (thread, own)
            })
            .collect())
    }

    /// Whether any of a review's threads is unresolved: a point the agent
    /// has still to answer.
    pub fn review_has_open_threads(&self, review_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM review_thread WHERE review_id=?1 AND state=?2",
            params![review_id, UNRESOLVED],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Add a comment to a thread.
    pub fn reply_to_review_thread(
        &self,
        thread_id: &str,
        author: &str,
        body: &str,
        revision_id: Option<&str>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO review_thread_comment (id, thread_id, author, body, revision_id, created_ms)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![comment_id(), thread_id, author, body, revision_id, now],
        )?;
        tx.execute(
            "UPDATE review_thread SET updated_ms=?2 WHERE id=?1",
            params![thread_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Move a thread to `to` from any of `from`. False when it was in none of
    /// them, changing nothing.
    pub fn set_review_thread_state(
        &self,
        thread_id: &str,
        from: &[&str],
        to: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let from = serde_json::to_string(from).unwrap_or_else(|_| "[]".into());
        let n = conn.execute(
            "UPDATE review_thread SET state=?2, updated_ms=?3,
                addressed_revision_id=CASE WHEN ?2=?5 THEN addressed_revision_id ELSE NULL END
             WHERE id=?1 AND state IN (SELECT value FROM json_each(?4))",
            params![thread_id, to, now_ms(), from, ADDRESSED],
        )?;
        Ok(n == 1)
    }

    /// What the agent said of a review's threads with the revision
    /// `revision_id`: each reply is added, and each thread it says that
    /// revision addresses is marked so, unless the operator resolved it.
    /// All or nothing.
    pub fn answer_review_threads(
        &self,
        review_id: &str,
        revision_id: &str,
        answers: &[ThreadAnswer],
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let now = now_ms();
        for answer in answers {
            let on_review: Option<String> = tx
                .query_row(
                    "SELECT id FROM review_thread WHERE id=?1 AND review_id=?2",
                    params![answer.thread_id, review_id],
                    |r| r.get(0),
                )
                .optional()?;
            if on_review.is_none() {
                continue;
            }
            if let Some(reply) = answer.reply.as_deref().filter(|r| !r.trim().is_empty()) {
                tx.execute(
                    "INSERT INTO review_thread_comment
                        (id, thread_id, author, body, revision_id, created_ms)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                    params![
                        comment_id(),
                        answer.thread_id,
                        AGENT,
                        reply,
                        revision_id,
                        now
                    ],
                )?;
            }
            if answer.addressed {
                tx.execute(
                    "UPDATE review_thread SET state=?2, addressed_revision_id=?3, updated_ms=?4
                     WHERE id=?1 AND state<>?5",
                    params![answer.thread_id, ADDRESSED, revision_id, now, RESOLVED],
                )?;
            } else {
                tx.execute(
                    "UPDATE review_thread SET updated_ms=?2 WHERE id=?1",
                    params![answer.thread_id, now],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str, review: &str, revision: &str) -> ReviewThreadRow {
        ReviewThreadRow {
            id: id.into(),
            review_id: review.into(),
            revision_id: revision.into(),
            kind: "line".into(),
            path: Some("src/a.rs".into()),
            side: Some("new".into()),
            start_line: Some(3),
            end_line: Some(4),
            anchor: Some("x\ny".into()),
            state: UNRESOLVED.into(),
            addressed_revision_id: None,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    fn store_with_review() -> Store {
        let store = Store::open_in_memory().unwrap();
        // The rows threads reference are not what is under test here.
        let conn = store.conn.lock().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        drop(conn);
        store
    }

    #[test]
    fn a_thread_keeps_its_comments_in_order_and_moves_between_states() {
        let store = store_with_review();
        store
            .insert_review_thread(&thread("t1", "r1", "rev1"), "rename this")
            .unwrap();
        store
            .insert_review_thread(&thread("t2", "r1", "rev1"), "and this")
            .unwrap();
        store
            .insert_review_thread(&thread("other", "r2", "rev9"), "elsewhere")
            .unwrap();
        assert!(store.review_has_open_threads("r1").unwrap());

        store
            .answer_review_threads(
                "r1",
                "rev2",
                &[
                    ThreadAnswer {
                        thread_id: "t1".into(),
                        reply: Some("renamed".into()),
                        addressed: true,
                    },
                    // Another review's thread is not this one's to answer.
                    ThreadAnswer {
                        thread_id: "other".into(),
                        reply: Some("no".into()),
                        addressed: true,
                    },
                ],
            )
            .unwrap();
        let threads = store.review_threads("r1").unwrap();
        assert_eq!(threads.len(), 2);
        let (t1, comments) = &threads[0];
        assert_eq!(t1.state, ADDRESSED);
        assert_eq!(t1.addressed_revision_id.as_deref(), Some("rev2"));
        assert_eq!(
            comments
                .iter()
                .map(|c| (c.author.as_str(), c.body.as_str()))
                .collect::<Vec<_>>(),
            [(OPERATOR, "rename this"), (AGENT, "renamed")]
        );
        assert_eq!(threads[1].0.state, UNRESOLVED);
        let other = store.review_threads("r2").unwrap();
        assert_eq!(other[0].0.state, UNRESOLVED);
        assert_eq!(other[0].1.len(), 1);

        // Resolved, it cannot be addressed again; reopened, it is open.
        assert!(store
            .set_review_thread_state("t1", &[UNRESOLVED, ADDRESSED], RESOLVED)
            .unwrap());
        assert!(!store
            .set_review_thread_state("t1", &[UNRESOLVED, ADDRESSED], RESOLVED)
            .unwrap());
        let resolved = store.review_thread("t1").unwrap().unwrap();
        assert_eq!(resolved.addressed_revision_id, None);
        store
            .answer_review_threads(
                "r1",
                "rev3",
                &[ThreadAnswer {
                    thread_id: "t1".into(),
                    reply: None,
                    addressed: true,
                }],
            )
            .unwrap();
        assert_eq!(store.review_thread("t1").unwrap().unwrap().state, RESOLVED);
        assert!(store
            .set_review_thread_state("t1", &[RESOLVED, ADDRESSED], UNRESOLVED)
            .unwrap());
        store
            .reply_to_review_thread("t1", OPERATOR, "not quite", Some("rev3"))
            .unwrap();
        let threads = store.review_threads("r1").unwrap();
        assert_eq!(threads[0].0.state, UNRESOLVED);
        assert_eq!(threads[0].1.last().unwrap().body, "not quite");
    }
}
