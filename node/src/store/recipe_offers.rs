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
    /// `pending`, `accepting` (an accept holds it), `accepted`, `declined`,
    /// `superseded`, `expired`, or `withdrawn` (its sender no longer shares
    /// the channel it came on).
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
    /// The sender has offered as many as it may in the last hour.
    TooOften,
}

/// How many offers one sender may make a node in an hour, superseded ones
/// included.
pub const MAX_OFFERS_PER_HOUR: i64 = 10;

/// How long a decided, superseded, lapsed or withdrawn offer is kept for the
/// operator to look back on.
pub const KEEP_DECIDED_MS: i64 = 30 * 24 * 3600 * 1000;

/// How long an accept may hold an offer before it is taken to have died
/// part way (a crash between the claim and the write) and is pending again.
const CLAIM_MS: i64 = 5 * 60 * 1000;

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

fn sweep(conn: &rusqlite::Connection, now_ms: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE repo_recipe_offer SET state = 'pending', decided_ms = NULL
         WHERE state = 'accepting' AND decided_ms < ?1",
        [now_ms - CLAIM_MS],
    )?;
    conn.execute(
        "DELETE FROM repo_recipe_offer
         WHERE state NOT IN ('pending', 'accepting') AND received_ms < ?1",
        [now_ms - KEEP_DECIDED_MS],
    )?;
    conn.execute(
        "UPDATE repo_recipe_offer SET state = 'expired'
         WHERE state = 'pending' AND expires_ms < ?1",
        [now_ms],
    )
}

impl Store {
    /// Hold `row` as pending. A pending offer from the same sender for the
    /// same repository is superseded by it, so the operator decides only the
    /// newest; past `max_pending` undecided offers from one sender, or
    /// [`MAX_OFFERS_PER_HOUR`] in the last hour, nothing is staged.
    pub fn recipe_offer_stage(&self, row: &RecipeOfferRow, max_pending: i64) -> Result<Staged> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        sweep(&tx, row.received_ms)?;
        let recent: i64 = tx.query_row(
            "SELECT COUNT(*) FROM repo_recipe_offer WHERE sender_node = ?1 AND received_ms > ?2",
            params![row.sender_node, row.received_ms - 3_600_000],
            |r| r.get(0),
        )?;
        if recent >= MAX_OFFERS_PER_HOUR {
            return Ok(Staged::TooOften);
        }
        let superseded = tx.execute(
            "UPDATE repo_recipe_offer SET state = 'superseded', decided_ms = ?3
             WHERE state = 'pending' AND sender_node = ?1 AND repo = ?2",
            params![row.sender_node, row.repo, row.received_ms],
        )?;
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM repo_recipe_offer
             WHERE state IN ('pending', 'accepting') AND sender_node = ?1",
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

    /// Expire every pending offer past its time, return to pending one an
    /// accept stopped holding part way, and forget decided offers older than
    /// [`KEEP_DECIDED_MS`].
    pub fn recipe_offers_expire(&self, now_ms: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(sweep(&conn, now_ms)?)
    }

    /// Up to `limit` offers: undecided ones first, then the newest.
    pub fn recipe_offers(&self, limit: i64) -> Result<Vec<RecipeOfferRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM repo_recipe_offer
             ORDER BY state = 'pending' DESC, received_ms DESC, id DESC LIMIT ?1"
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

    /// Settle a pending, unexpired offer as `state` (`declined`,
    /// `withdrawn`). False when it was not pending any more, or had lapsed.
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

    /// Take a pending, unexpired offer for one accept, so two cannot both
    /// apply it. False when it is not pending, has lapsed, or another accept
    /// holds it.
    pub fn recipe_offer_claim(&self, id: &str, now_ms: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE repo_recipe_offer SET state = 'accepting', decided_ms = ?2
             WHERE id = ?1 AND state = 'pending' AND expires_ms >= ?2",
            params![id, now_ms],
        )? == 1)
    }

    /// The claimed offer was written to `path`.
    pub fn recipe_offer_accepted(&self, id: &str, path: &str, now_ms: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE repo_recipe_offer SET state = 'accepted', decided_ms = ?2, decided_path = ?3
             WHERE id = ?1 AND state = 'accepting'",
            params![id, now_ms, path],
        )? == 1)
    }

    /// The claimed offer was not written: it waits for a decision again.
    pub fn recipe_offer_release(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE repo_recipe_offer SET state = 'pending', decided_ms = NULL
             WHERE id = ?1 AND state = 'accepting'",
            [id],
        )?;
        Ok(())
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

    #[test]
    fn a_sender_offers_a_bounded_number_an_hour() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..MAX_OFFERS_PER_HOUR {
            assert!(matches!(
                store
                    .recipe_offer_stage(
                        &offer(&format!("o{i}"), "n1", "github.com/o/a", 1000 + i),
                        99
                    )
                    .unwrap(),
                Staged::Held { .. }
            ));
        }
        assert_eq!(
            store
                .recipe_offer_stage(&offer("late", "n1", "github.com/o/a", 2000), 99)
                .unwrap(),
            Staged::TooOften
        );
        // Another sender is not held to the first one's count, and an hour
        // on the first may offer again.
        assert!(matches!(
            store
                .recipe_offer_stage(&offer("n2", "n2", "github.com/o/a", 2000), 99)
                .unwrap(),
            Staged::Held { .. }
        ));
        let mut later = offer("later", "n1", "github.com/o/a", 3_700_000);
        later.expires_ms = 9_000_000;
        assert!(matches!(
            store.recipe_offer_stage(&later, 99).unwrap(),
            Staged::Held { .. }
        ));
    }

    /// One accept holds an offer at a time; one that dies part way gives it
    /// back, and old decided offers are forgotten.
    #[test]
    fn an_accept_claims_the_offer_and_old_decisions_are_pruned() {
        let store = Store::open_in_memory().unwrap();
        let mut row = offer("1", "n1", "github.com/o/a", 10);
        row.expires_ms = i64::MAX / 2;
        store.recipe_offer_stage(&row, 8).unwrap();
        assert!(store.recipe_offer_claim("1", 20).unwrap());
        assert!(!store.recipe_offer_claim("1", 21).unwrap());
        assert!(!store
            .recipe_offer_decide("1", "declined", None, 22)
            .unwrap());
        store.recipe_offer_release("1").unwrap();
        assert!(store.recipe_offer_claim("1", 30).unwrap());
        // Held past the claim window, it is pending again.
        store.recipe_offers_expire(30 + CLAIM_MS + 1).unwrap();
        assert_eq!(store.recipe_offer("1").unwrap().unwrap().state, "pending");
        assert!(store.recipe_offer_claim("1", 40).unwrap());
        assert!(store
            .recipe_offer_accepted("1", "github.com/o/a", 41)
            .unwrap());
        let done = store.recipe_offer("1").unwrap().unwrap();
        assert_eq!(done.state, "accepted");
        assert_eq!(done.decided_path.as_deref(), Some("github.com/o/a"));
        store
            .recipe_offers_expire(10 + KEEP_DECIDED_MS + 1)
            .unwrap();
        assert!(store.recipe_offer("1").unwrap().is_none());
    }

    #[test]
    fn undecided_offers_are_listed_first() {
        let store = Store::open_in_memory().unwrap();
        store
            .recipe_offer_stage(&offer("old", "n1", "github.com/o/a", 10), 8)
            .unwrap();
        store
            .recipe_offer_decide("old", "declined", None, 11)
            .unwrap();
        let mut newer = offer("new", "n1", "github.com/o/b", 5);
        newer.expires_ms = 1_000_000;
        store.recipe_offer_stage(&newer, 8).unwrap();
        let mut decided = offer("newest", "n1", "github.com/o/c", 20);
        decided.expires_ms = 1_000_000;
        store.recipe_offer_stage(&decided, 8).unwrap();
        store
            .recipe_offer_decide("newest", "declined", None, 21)
            .unwrap();
        let ids: Vec<String> = store
            .recipe_offers(10)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, ["new", "newest", "old"]);
    }
}
