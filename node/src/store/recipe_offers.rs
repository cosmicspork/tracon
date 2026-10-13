//! Repository recipes peers offered this node, held until its operator
//! accepts or declines each (`crate::mesh::recipe`). Node-local and never
//! replicated: an offer is addressed to this node alone.

use rusqlite::{params, OptionalExtension, Row};
use serde::Serialize;

use super::{Result, Store};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecipeOfferRow {
    pub id: String,
    pub sender_node: String,
    pub channel: String,
    /// The forge identity the recipe names.
    pub repo: String,
    pub recipe: serde_json::Value,
    pub note: Option<String>,
    pub received_ms: i64,
    pub expires_ms: i64,
    /// `pending`, `accepted`, `declined`, `superseded` or `expired`.
    pub state: String,
    pub decided_ms: Option<i64>,
    /// The `[[repo]]` path an accepted offer was written to.
    pub decided_path: Option<String>,
}

/// What staging an offer did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staged {
    /// Held for the operator; this many older pending offers from the same
    /// sender for the same repository were superseded by it.
    Held { superseded: usize },
    /// The sender already has as many undecided offers here as it may.
    TooMany,
}

const COLUMNS: &str =
    "id, sender_node, channel, repo, recipe_json, note, received_ms, expires_ms, \
                       state, decided_ms, decided_path";

fn from_row(r: &Row) -> rusqlite::Result<RecipeOfferRow> {
    let recipe: String = r.get(4)?;
    Ok(RecipeOfferRow {
        id: r.get(0)?,
        sender_node: r.get(1)?,
        channel: r.get(2)?,
        repo: r.get(3)?,
        recipe: serde_json::from_str(&recipe).unwrap_or_default(),
        note: r.get(5)?,
        received_ms: r.get(6)?,
        expires_ms: r.get(7)?,
        state: r.get(8)?,
        decided_ms: r.get(9)?,
        decided_path: r.get(10)?,
    })
}

impl Store {
    /// Hold `row` as pending. A pending offer from the same sender for the
    /// same repository is superseded by it, so the operator decides only the
    /// newest; past `max_pending` undecided offers from one sender, nothing
    /// is staged.
    pub fn recipe_offer_stage(&self, row: &RecipeOfferRow, max_pending: i64) -> Result<Staged> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE repo_recipe_offer SET state = 'expired'
             WHERE state = 'pending' AND expires_ms < ?1",
            [row.received_ms],
        )?;
        let superseded = tx.execute(
            "UPDATE repo_recipe_offer SET state = 'superseded', decided_ms = ?3
             WHERE state = 'pending' AND sender_node = ?1 AND repo = ?2",
            params![row.sender_node, row.repo, row.received_ms],
        )?;
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM repo_recipe_offer WHERE state = 'pending' AND sender_node = ?1",
            [&row.sender_node],
            |r| r.get(0),
        )?;
        if pending >= max_pending {
            // Dropping the transaction rolls the supersede back with it.
            return Ok(Staged::TooMany);
        }
        tx.execute(
            &format!("INSERT INTO repo_recipe_offer ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', NULL, NULL)"),
            params![
                row.id,
                row.sender_node,
                row.channel,
                row.repo,
                row.recipe.to_string(),
                row.note,
                row.received_ms,
                row.expires_ms,
            ],
        )?;
        tx.commit()?;
        Ok(Staged::Held { superseded })
    }

    /// Mark every pending offer past its time as expired.
    pub fn recipe_offers_expire(&self, now_ms: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE repo_recipe_offer SET state = 'expired'
             WHERE state = 'pending' AND expires_ms < ?1",
            [now_ms],
        )?)
    }

    /// The newest `limit` offers, newest first.
    pub fn recipe_offers(&self, limit: i64) -> Result<Vec<RecipeOfferRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM repo_recipe_offer ORDER BY received_ms DESC, id DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map([limit], from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn recipe_offer(&self, id: &str) -> Result<Option<RecipeOfferRow>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM repo_recipe_offer WHERE id = ?1"),
                [id],
                from_row,
            )
            .optional()?)
    }

    /// Settle a pending, unexpired offer as `state`. False when it was not
    /// pending any more, or had lapsed.
    pub fn recipe_offer_decide(
        &self,
        id: &str,
        state: &str,
        path: Option<&str>,
        now_ms: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE repo_recipe_offer SET state = ?2, decided_ms = ?3, decided_path = ?4
             WHERE id = ?1 AND state = 'pending' AND expires_ms >= ?3",
            params![id, state, now_ms, path],
        )? == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(id: &str, sender: &str, repo: &str, at: i64) -> RecipeOfferRow {
        RecipeOfferRow {
            id: id.into(),
            sender_node: sender.into(),
            channel: "personal".into(),
            repo: repo.into(),
            recipe: serde_json::json!({ "repo": repo }),
            note: None,
            received_ms: at,
            expires_ms: at + 1000,
            state: "pending".into(),
            decided_ms: None,
            decided_path: None,
        }
    }

    #[test]
    fn a_newer_offer_supersedes_the_older_and_offers_lapse() {
        let store = Store::open_in_memory().unwrap();
        let a = "github.com/o/a";
        assert_eq!(
            store
                .recipe_offer_stage(&offer("1", "n1", a, 10), 8)
                .unwrap(),
            Staged::Held { superseded: 0 }
        );
        // Another sender's offer for the same repository stands beside it.
        store
            .recipe_offer_stage(&offer("2", "n2", a, 20), 8)
            .unwrap();
        assert_eq!(
            store
                .recipe_offer_stage(&offer("3", "n1", a, 30), 8)
                .unwrap(),
            Staged::Held { superseded: 1 }
        );
        let state = |id: &str| store.recipe_offer(id).unwrap().unwrap().state;
        assert_eq!(state("1"), "superseded");
        assert_eq!(state("2"), "pending");
        assert_eq!(state("3"), "pending");
        assert_eq!(store.recipe_offers(10).unwrap()[0].id, "3");

        // Past its time an offer cannot be decided, and the sweep says so.
        assert!(!store
            .recipe_offer_decide("2", "accepted", None, 5000)
            .unwrap());
        assert_eq!(store.recipe_offers_expire(5000).unwrap(), 2);
        assert_eq!(state("2"), "expired");
        assert!(!store
            .recipe_offer_decide("1", "accepted", None, 31)
            .unwrap());
    }

    #[test]
    fn a_sender_holds_a_bounded_number_of_undecided_offers() {
        let store = Store::open_in_memory().unwrap();
        store
            .recipe_offer_stage(&offer("1", "n1", "github.com/o/a", 10), 2)
            .unwrap();
        store
            .recipe_offer_stage(&offer("2", "n1", "github.com/o/b", 10), 2)
            .unwrap();
        assert_eq!(
            store
                .recipe_offer_stage(&offer("3", "n1", "github.com/o/c", 10), 2)
                .unwrap(),
            Staged::TooMany
        );
        // Replacing one it already has does not need room.
        assert_eq!(
            store
                .recipe_offer_stage(&offer("4", "n1", "github.com/o/b", 11), 2)
                .unwrap(),
            Staged::Held { superseded: 1 }
        );
        assert!(store.recipe_offer("3").unwrap().is_none());
        assert!(store
            .recipe_offer_decide("4", "declined", None, 12)
            .unwrap());
        assert!(!store
            .recipe_offer_decide("4", "accepted", None, 13)
            .unwrap());
    }
}
