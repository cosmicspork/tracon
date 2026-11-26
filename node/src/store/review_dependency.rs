//! What a review publishes after: other reviews on its channel whose changes
//! must land first (`submit_review`'s `after`).
//!
//! The submitter names them on the node that owns the review, and only there
//! do they change; the list travels with the review's row to every peer that
//! mirrors it (`mesh::review_events::ReviewMeta::after`), as its threads are
//! read through the owner. Whether a dependency is satisfied is not kept: it
//! is read from the dependency's review as it stands (`review::dependencies`).

use std::collections::{BTreeSet, HashSet};

use rusqlite::params;

use super::{now_ms, Result, Store};

/// The column a review query selects to fill [`super::ReviewRow::after`]:
/// the ids it publishes after, space separated.
pub const AFTER_IDS: &str = "(SELECT group_concat(d.after_id, ' ') FROM review_dependency d \
     WHERE d.review_id = review.id) AS after_ids";

/// [`AFTER_IDS`] read back, in a stable order.
pub fn split_ids(ids: &str) -> Vec<String> {
    let set: BTreeSet<&str> = ids.split(' ').filter(|id| !id.is_empty()).collect();
    set.into_iter().map(str::to_string).collect()
}

impl Store {
    /// Make `after` the reviews `review_id` publishes after, replacing what
    /// it named before. Whether the list changed.
    pub fn set_review_dependencies(&self, review_id: &str, after: &[String]) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let held: BTreeSet<String> = {
            let mut stmt =
                tx.prepare("SELECT after_id FROM review_dependency WHERE review_id=?1")?;
            let rows = stmt
                .query_map([review_id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<_, _>>()?;
            rows
        };
        let wanted: BTreeSet<String> = after.iter().cloned().collect();
        if held == wanted {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM review_dependency WHERE review_id=?1",
            [review_id],
        )?;
        let now = now_ms();
        for id in &wanted {
            tx.execute(
                "INSERT INTO review_dependency (review_id, after_id, created_ms) VALUES (?1,?2,?3)",
                params![review_id, id, now],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Keep why an approved review was published before what it publishes
    /// after was satisfied, as its verdict's reason. Whether it was kept.
    pub fn record_approved_anyway(&self, review_id: &str, reason: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE review SET verdict_reason=?2, updated_ms=?3 WHERE id=?1 AND state='approved'",
            params![review_id, reason, now_ms()],
        )?;
        Ok(n == 1)
    }

    /// The reviews `review_id` publishes after, by id.
    pub fn review_dependencies(&self, review_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT after_id FROM review_dependency WHERE review_id=?1 ORDER BY after_id",
        )?;
        let rows = stmt
            .query_map([review_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// The reviews that publish after `review_id`, by id.
    pub fn review_dependents(&self, review_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT review_id FROM review_dependency WHERE after_id=?1 ORDER BY review_id",
        )?;
        let rows = stmt
            .query_map([review_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Whether `from` publishes after `to`, directly or through others: what
    /// naming `to` as publishing after `from` would close into a cycle.
    pub fn review_depends_on(&self, from: &str, to: &str) -> Result<bool> {
        let mut seen = HashSet::new();
        let mut next = vec![from.to_string()];
        while let Some(id) = next.pop() {
            if id == to {
                return Ok(true);
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            next.extend(self.review_dependencies(&id)?);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(store: &Store, id: &str) {
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO review (id, session_id, node_id, channel, kind, title, body, provider,
                target, diff, files, head_sha, base_ref, added, removed, state, created_ms,
                created_mono_ms, updated_ms)
             VALUES (?1, NULL, 'n1', 'work', 'pr', 't', 'b', 'github', '{}', '', '[]', 'h',
                'main', 0, 0, 'new', 1, 0, 1)",
            [id],
        )
        .unwrap();
    }

    #[test]
    fn dependencies_are_replaced_read_both_ways_and_listed_on_the_row() {
        let store = Store::open_in_memory().unwrap();
        for id in ["a", "b", "c"] {
            review(&store, id);
        }
        assert!(store
            .set_review_dependencies("c", &["b".into(), "a".into()])
            .unwrap());
        assert!(!store
            .set_review_dependencies("c", &["a".into(), "b".into()])
            .unwrap());
        assert_eq!(store.review_dependencies("c").unwrap(), ["a", "b"]);
        assert_eq!(store.review_dependents("a").unwrap(), ["c"]);
        assert_eq!(store.get_review("c").unwrap().unwrap().after, ["a", "b"]);
        assert!(store.get_review("a").unwrap().unwrap().after.is_empty());
        let open = store.open_reviews().unwrap();
        assert_eq!(open.iter().find(|r| r.id == "c").unwrap().after, ["a", "b"]);
        assert_eq!(
            store
                .mirrored_reviews()
                .unwrap()
                .iter()
                .find(|r| r.id == "c")
                .unwrap()
                .after,
            ["a", "b"]
        );

        // Through others, and never back.
        store.set_review_dependencies("b", &["a".into()]).unwrap();
        assert!(store.review_depends_on("c", "a").unwrap());
        assert!(!store.review_depends_on("a", "c").unwrap());

        // A peer's copy keeps the list its owner listed, and only the owner's
        // listing changes it.
        let mut meta = crate::mesh::review_events::ReviewMeta::of(
            &store.get_review("c").unwrap().unwrap(),
            vec![],
        );
        meta.id = "d".into();
        meta.node_id = "peer".into();
        assert!(store.upsert_review_meta(&meta).unwrap());
        assert_eq!(store.get_review("d").unwrap().unwrap().after, ["a", "b"]);
        meta.after = vec!["a".into()];
        assert!(store.upsert_review_meta(&meta).unwrap());
        assert_eq!(store.review_dependencies("d").unwrap(), ["a"]);
        meta.node_id = "impostor".into();
        meta.after = vec![];
        assert!(!store.upsert_review_meta(&meta).unwrap());
        assert_eq!(store.review_dependencies("d").unwrap(), ["a"]);

        assert!(store.set_review_dependencies("c", &["b".into()]).unwrap());
        assert_eq!(store.review_dependents("a").unwrap(), ["b", "d"]);
        assert!(store.set_review_dependencies("c", &[]).unwrap());
        assert!(store.get_review("c").unwrap().unwrap().after.is_empty());
    }
}
