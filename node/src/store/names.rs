//! The operator's names for records: the replicated `entity_name` table.
//!
//! A record with no row here reads by its generated name, so the table holds
//! only overrides. A row's id is `<kind>:<entity id>`; a revert is a
//! tombstone, so it travels the mesh like a rename does. Rules on what a name
//! may be live in `crate::names`; this is storage, the clash check and the
//! records each kind resolves against.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::json;
use tracon_sync::{Change, ChangeOp};

use super::{corpus::sync_err, now_ms, Result, Store};

/// The replicated table.
pub const TABLE: &str = "entity_name";

/// The row id of `kind`'s record `id`.
pub fn row_id(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

/// An operator name as it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntityName {
    pub channel: String,
    pub kind: String,
    /// The named record's id.
    pub id: String,
    pub name: String,
    pub updated_ms: i64,
}

/// A record a name can resolve to: what each kind's generated names and id
/// prefixes are matched against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NamedRecord {
    pub kind: String,
    pub id: String,
    pub channel: String,
    pub created_ms: i64,
}

/// What a rename or a revert came to.
#[derive(Debug)]
pub enum NameWrite {
    /// Stored; the change to publish.
    Written(Change),
    /// Another record of the kind on the channel already has the name (in
    /// any case). Nothing was written.
    Clash(EntityName),
    /// Already so: nothing to write.
    Unchanged,
}

fn entity_name(r: &rusqlite::Row) -> rusqlite::Result<EntityName> {
    Ok(EntityName {
        channel: r.get("channel")?,
        kind: r.get("kind")?,
        id: r.get("entity_id")?,
        name: r.get("name")?,
        updated_ms: r.get("updated_ms")?,
    })
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

impl Store {
    /// Every live operator name, on one channel or on all.
    pub fn names_list(&self, channel: Option<&str>) -> Result<Vec<EntityName>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT channel, kind, entity_id, name, updated_ms FROM entity_name
             WHERE deleted = 0 AND name != '' AND (?1 IS NULL OR channel = ?1)
             ORDER BY channel, kind, entity_id",
        )?;
        let rows = stmt.query_map(params![channel], entity_name)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The operator name of `kind`'s record `id`, if it has one.
    pub fn name_get(&self, kind: &str, id: &str) -> Result<Option<EntityName>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT channel, kind, entity_id, name, updated_ms FROM entity_name
                 WHERE id = ?1 AND deleted = 0 AND name != ''",
                [row_id(kind, id)],
                entity_name,
            )
            .optional()?)
    }

    /// The records of `kind` whose operator name is `name`, ignoring case,
    /// optionally on one channel. More than one only when two sites named
    /// two records alike at once.
    pub fn names_matching(
        &self,
        kind: &str,
        name: &str,
        channel: Option<&str>,
    ) -> Result<Vec<EntityName>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT channel, kind, entity_id, name, updated_ms FROM entity_name
             WHERE kind = ?1 AND deleted = 0 AND name != '' AND (?2 IS NULL OR channel = ?2)",
        )?;
        let rows = stmt.query_map(params![kind, channel], entity_name)?;
        let all: Vec<EntityName> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(all
            .into_iter()
            .filter(|n| same_name(&n.name, name))
            .collect())
    }

    /// Name `kind`'s record `id` on `channel`, or with `None` revert it to
    /// its generated name. The clash check and the replicated write are one
    /// transaction, so two renames on this node cannot both take a name.
    /// `name` is already validated (`crate::names::validate`).
    pub fn name_write(
        &self,
        site: &str,
        channel: &str,
        kind: &str,
        id: &str,
        name: Option<&str>,
    ) -> Result<NameWrite> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let key = row_id(kind, id);
        let current: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT name, deleted, created_ms FROM entity_name WHERE id = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let live = current
            .as_ref()
            .filter(|(n, deleted, _)| *deleted == 0 && !n.is_empty());
        let now = now_ms();
        let change = match name {
            None => {
                if live.is_none() {
                    return Ok(NameWrite::Unchanged);
                }
                tracon_sync::apply::write_change_in_tx(
                    &tx,
                    site,
                    channel,
                    TABLE,
                    ChangeOp::Delete,
                    &key,
                    serde_json::Value::Null,
                    now,
                )
                .map_err(sync_err)?
            }
            Some(name) => {
                if live.is_some_and(|(n, _, _)| n == name) {
                    return Ok(NameWrite::Unchanged);
                }
                let holder = {
                    let mut stmt = tx.prepare(
                        "SELECT channel, kind, entity_id, name, updated_ms FROM entity_name
                         WHERE channel = ?1 AND kind = ?2 AND entity_id != ?3
                           AND deleted = 0 AND name != ''",
                    )?;
                    let rows = stmt.query_map(params![channel, kind, id], entity_name)?;
                    let rows: Vec<EntityName> = rows.collect::<rusqlite::Result<_>>()?;
                    rows.into_iter().find(|n| same_name(&n.name, name))
                };
                if let Some(holder) = holder {
                    return Ok(NameWrite::Clash(holder));
                }
                let created = live.map(|(_, _, c)| *c).unwrap_or(now);
                tracon_sync::apply::write_change_in_tx(
                    &tx,
                    site,
                    channel,
                    TABLE,
                    ChangeOp::Upsert,
                    &key,
                    json!({
                        "channel": channel,
                        "kind": kind,
                        "entity_id": id,
                        "name": name,
                        "created_ms": created,
                        "updated_ms": now,
                    }),
                    now,
                )
                .map_err(sync_err)?
            }
        };
        tx.commit()?;
        Ok(NameWrite::Written(change))
    }

    /// Every record of `kind` this node holds, its own and mirrored: what a
    /// generated name or an id prefix is resolved against. A kind with no
    /// table here has none.
    pub fn named_records(&self, kind: &str) -> Result<Vec<NamedRecord>> {
        let sql = match kind {
            "session" => "SELECT id, channel, created_ms FROM session",
            "review" => "SELECT id, channel, created_ms FROM review WHERE kind != 'report'",
            "report" => "SELECT id, channel, created_ms FROM review WHERE kind = 'report'",
            "approval" => "SELECT id, channel, created_ms FROM approval",
            "manifest" => {
                "SELECT digest, channel, MIN(created_ms) FROM launch_manifest GROUP BY digest, channel"
            }
            "task" => "SELECT id, channel, created_ms FROM work_item WHERE deleted = 0",
            "transfer" => "SELECT id, channel, created_ms FROM transfer",
            "check" => {
                "SELECT c.id, COALESCE(s.channel, ''), c.started_ms FROM check_run c
                 LEFT JOIN session s ON s.id = c.session_id"
            }
            _ => return Ok(Vec::new()),
        };
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(NamedRecord {
                kind: kind.to_string(),
                id: r.get(0)?,
                channel: r.get(1)?,
                created_ms: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Which named kind the record `id` is, if it is a session, review,
    /// report, approval or task this node holds. Ids are global, so one id
    /// is at most one of them.
    pub fn kind_of_record(&self, id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT 'session' FROM session WHERE id = ?1
                 UNION ALL SELECT CASE WHEN kind = 'report' THEN 'report' ELSE 'review' END
                   FROM review WHERE id = ?1
                 UNION ALL SELECT 'approval' FROM approval WHERE id = ?1
                 UNION ALL SELECT 'task' FROM work_item WHERE id = ?1 AND deleted = 0
                 LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// `kind`'s record `id`, if this node holds it. A review and a report
    /// share a table, so either kind finds either row, and the answer
    /// carries the row's own kind.
    pub fn named_record(&self, kind: &str, id: &str) -> Result<Option<NamedRecord>> {
        if matches!(kind, "review" | "report") {
            let conn = self.conn.lock().unwrap();
            return Ok(conn
                .query_row(
                    "SELECT kind, channel, created_ms FROM review WHERE id = ?1",
                    [id],
                    |r| {
                        let k: String = r.get(0)?;
                        Ok(NamedRecord {
                            kind: if k == "report" { "report" } else { "review" }.into(),
                            id: id.to_string(),
                            channel: r.get(1)?,
                            created_ms: r.get(2)?,
                        })
                    },
                )
                .optional()?);
        }
        Ok(self.named_records(kind)?.into_iter().find(|r| r.id == id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: &str = "site-a";

    #[test]
    fn a_name_is_unique_per_channel_and_kind_ignoring_case_and_a_revert_frees_it() {
        let store = Store::open_in_memory().unwrap();
        let w = |ch: &str, kind: &str, id: &str, name: Option<&str>| {
            store.name_write(SITE, ch, kind, id, name).unwrap()
        };
        assert!(matches!(
            w("personal", "session", "s1", Some("Login flow")),
            NameWrite::Written(_)
        ));
        assert!(matches!(
            w("personal", "session", "s1", Some("Login flow")),
            NameWrite::Unchanged
        ));
        match w("personal", "session", "s2", Some("LOGIN FLOW")) {
            NameWrite::Clash(holder) => assert_eq!(holder.id, "s1"),
            other => panic!("expected a clash, got {other:?}"),
        }
        // Another kind, or another channel, may use it.
        assert!(matches!(
            w("personal", "review", "r1", Some("Login flow")),
            NameWrite::Written(_)
        ));
        assert!(matches!(
            w("work", "session", "s3", Some("login flow")),
            NameWrite::Written(_)
        ));
        assert_eq!(
            store.name_get("session", "s1").unwrap().unwrap().name,
            "Login flow"
        );
        assert_eq!(
            store
                .names_matching("session", "login FLOW", None)
                .unwrap()
                .len(),
            2
        );

        // A revert is a tombstone: the name is gone and free again.
        let NameWrite::Written(change) = w("personal", "session", "s1", None) else {
            panic!("a revert writes");
        };
        assert_eq!(change.op, ChangeOp::Delete);
        assert_eq!(change.id, "session:s1");
        assert!(store.name_get("session", "s1").unwrap().is_none());
        let deleted: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT deleted FROM entity_name WHERE id = 'session:s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deleted, 1);
        assert!(matches!(
            w("personal", "session", "s1", None),
            NameWrite::Unchanged
        ));
        assert!(matches!(
            w("personal", "session", "s2", Some("Login flow")),
            NameWrite::Written(_)
        ));
        let listed = store.names_list(Some("personal")).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|n| (n.kind.as_str(), n.id.as_str()))
                .collect::<Vec<_>>(),
            vec![("review", "r1"), ("session", "s2")]
        );
    }
}
