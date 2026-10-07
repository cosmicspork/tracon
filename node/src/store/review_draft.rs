//! What the operator has written on a review and not yet sent.
//!
//! The reason for a request for changes and the edited publication prose are
//! held by the node, not the tab, so they survive navigation, a reconnect and
//! a change of device. Two devices write against a version: a save that was
//! not made on top of the current one is refused with what is there now, and
//! the operator chooses, rather than the last writer silently winning. The
//! draft is cleared when a verdict is recorded. Desktop diff edits are not
//! here: they stay in the desktop's own storage, keyed by revision.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use super::{now_ms, Result, Store};

/// The largest draft the node keeps, in bytes of JSON.
pub const MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewDraft {
    pub review_id: String,
    /// The revision on the screen it was written against.
    pub revision_id: Option<String>,
    pub draft: serde_json::Value,
    pub version: i64,
    pub updated_ms: i64,
}

/// What a save found.
#[derive(Debug, Clone, PartialEq)]
pub enum Saved {
    Saved {
        version: i64,
        updated_ms: i64,
    },
    /// Another device saved since `base`; nothing was written.
    Conflict(Option<ReviewDraft>),
}

impl Store {
    pub fn review_draft(&self, review_id: &str) -> Result<Option<ReviewDraft>> {
        let conn = self.conn.lock().unwrap();
        read(&conn, review_id)
    }

    /// Save `draft` if `base` is the version this device last saw (0 for
    /// none). Atomic: two devices saving on the same base, one wins.
    pub fn save_review_draft(
        &self,
        review_id: &str,
        revision_id: Option<&str>,
        base: i64,
        draft: &serde_json::Value,
    ) -> Result<Saved> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let current = read(&tx, review_id)?;
        if current.as_ref().map_or(0, |d| d.version) != base {
            return Ok(Saved::Conflict(current));
        }
        let version = base + 1;
        let updated_ms = now_ms();
        tx.execute(
            "INSERT INTO review_draft (review_id, revision_id, body_json, version, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(review_id) DO UPDATE SET revision_id=?2, body_json=?3, version=?4,
             updated_ms=?5",
            params![
                review_id,
                revision_id,
                draft.to_string(),
                version,
                updated_ms
            ],
        )?;
        tx.commit()?;
        Ok(Saved::Saved {
            version,
            updated_ms,
        })
    }

    /// The verdict was sent; what led to it is not a draft any more.
    pub fn clear_review_draft(&self, review_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM review_draft WHERE review_id=?1", [review_id])?;
        Ok(())
    }
}

fn read(conn: &rusqlite::Connection, review_id: &str) -> Result<Option<ReviewDraft>> {
    conn.query_row(
        "SELECT revision_id, body_json, version, updated_ms FROM review_draft WHERE review_id=?1",
        [review_id],
        |r| {
            let body: String = r.get(1)?;
            Ok(ReviewDraft {
                review_id: review_id.to_string(),
                revision_id: r.get(0)?,
                draft: serde_json::from_str(&body).unwrap_or_default(),
                version: r.get(2)?,
                updated_ms: r.get(3)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_save_on_a_stale_version_is_refused_with_what_is_there() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.review_draft("r1").unwrap().is_none());
        let first = store
            .save_review_draft("r1", Some("v1"), 0, &json!({ "reason": "phone" }))
            .unwrap();
        assert!(matches!(first, Saved::Saved { version: 1, .. }));
        // The desktop, which also started from nothing, loses.
        let Saved::Conflict(Some(theirs)) = store
            .save_review_draft("r1", Some("v1"), 0, &json!({ "reason": "desktop" }))
            .unwrap()
        else {
            panic!("a stale save must conflict");
        };
        assert_eq!(theirs.draft["reason"], "phone");
        assert!(matches!(
            store
                .save_review_draft("r1", Some("v1"), 1, &json!({ "reason": "desktop" }))
                .unwrap(),
            Saved::Saved { version: 2, .. }
        ));
        assert_eq!(
            store.review_draft("r1").unwrap().unwrap().draft["reason"],
            "desktop"
        );
        store.clear_review_draft("r1").unwrap();
        assert!(store.review_draft("r1").unwrap().is_none());
    }
}
