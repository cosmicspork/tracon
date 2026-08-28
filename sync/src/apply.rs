//! Stamping local writes and applying remote ones. Every write to a replicated
//! table goes through [`write_change`]; every change that arrives goes
//! through [`apply_changes`]. Both run inside one SQLite transaction, so the
//! change log and the row never disagree.

use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::Value;

use crate::{columns_of, Change, ChangeOp, Hlc, Result, SyncError, CLEARED_ON_DELETE};

/// What became of one received change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// It won and the row now reads as the change says.
    Stored,
    /// It was older than what the row already held; logged, not applied.
    Lost,
    /// Seen before (same site and sequence).
    Duplicate,
    /// Its `site` was not the sender. The whole payload is dropped.
    Impersonation,
    /// Unknown table or a row that does not fit it.
    Malformed,
}

/// Stamp and apply a local write. `row` is the record as a JSON object of
/// the table's columns (`id` and the stamps are set here); null for a delete.
/// Returns the change to publish, after the transaction committed.
#[allow(clippy::too_many_arguments)]
pub fn write_change(
    conn: &mut Connection,
    site: &str,
    channel: &str,
    table: &str,
    op: ChangeOp,
    id: &str,
    row: Value,
    now_ms: i64,
) -> Result<Change> {
    columns_of(table)
        .ok_or_else(|| SyncError::Malformed(format!("no replicated table {table}")))?;
    let tx = conn.transaction()?;
    let mut hlc = Hlc::load(&tx)?;
    let (hlc_ms, hlc_ctr) = hlc.tick(now_ms);
    hlc.store(&tx)?;
    let site_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(site_seq), 0) + 1 FROM change_log WHERE site = ?1",
        [site],
        |r| r.get(0),
    )?;
    let change = Change {
        table: table.to_string(),
        op,
        id: id.to_string(),
        site: site.to_string(),
        site_seq,
        hlc_ms,
        hlc_ctr,
        row: if op == ChangeOp::Delete {
            Value::Null
        } else {
            row
        },
    };
    put_row(&tx, &change)?;
    log_change(&tx, channel, &change, true, now_ms)?;
    tx.commit()?;
    Ok(change)
}

/// Apply changes received from `sender`. A change whose `site` is not the
/// sender fails the whole batch as [`Applied::Impersonation`]: a node speaks
/// only for itself.
pub fn apply_changes(
    conn: &mut Connection,
    sender: &str,
    channel: &str,
    changes: &[Change],
    now_ms: i64,
) -> Result<Vec<Applied>> {
    if changes.iter().any(|c| c.site != sender) {
        return Ok(vec![Applied::Impersonation; changes.len()]);
    }
    let tx = conn.transaction()?;
    let mut hlc = Hlc::load(&tx)?;
    let mut out = Vec::with_capacity(changes.len());
    for c in changes {
        if columns_of(&c.table).is_none() || (c.op == ChangeOp::Upsert && !c.row.is_object()) {
            out.push(Applied::Malformed);
            continue;
        }
        let fresh = tx.execute(
            "INSERT OR IGNORE INTO change_log (site, site_seq, channel, tbl, op, record_id, hlc_ms, hlc_ctr, row_json, applied, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)",
            params![
                c.site,
                c.site_seq,
                channel,
                c.table,
                op_name(c.op),
                c.id,
                c.hlc_ms,
                c.hlc_ctr as i64,
                (!c.row.is_null()).then(|| c.row.to_string()),
                now_ms
            ],
        )?;
        if fresh == 0 {
            out.push(Applied::Duplicate);
            continue;
        }
        hlc.observe(now_ms, (c.hlc_ms, c.hlc_ctr));
        let local: Option<(i64, i64, String)> = tx
            .query_row(
                &format!(
                    "SELECT hlc_ms, hlc_ctr, site FROM {} WHERE id = ?1",
                    c.table
                ),
                [&c.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let wins = match local {
            None => true,
            Some((ms, ctr, site)) => {
                (c.hlc_ms, c.hlc_ctr as i64, c.site.as_str()) > (ms, ctr, site.as_str())
            }
        };
        if wins {
            put_row(&tx, c)?;
            tx.execute(
                "UPDATE change_log SET applied = 1 WHERE site = ?1 AND site_seq = ?2",
                params![c.site, c.site_seq],
            )?;
            out.push(Applied::Stored);
        } else {
            out.push(Applied::Lost);
        }
    }
    hlc.store(&tx)?;
    tx.commit()?;
    Ok(out)
}

fn op_name(op: ChangeOp) -> &'static str {
    match op {
        ChangeOp::Upsert => "upsert",
        ChangeOp::Delete => "delete",
    }
}

fn log_change(
    conn: &Connection,
    channel: &str,
    c: &Change,
    applied: bool,
    now_ms: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO change_log (site, site_seq, channel, tbl, op, record_id, hlc_ms, hlc_ctr, row_json, applied, created_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            c.site,
            c.site_seq,
            channel,
            c.table,
            op_name(c.op),
            c.id,
            c.hlc_ms,
            c.hlc_ctr as i64,
            (!c.row.is_null()).then(|| c.row.to_string()),
            applied as i64,
            now_ms
        ],
    )?;
    Ok(())
}

/// Write the row a change describes: the full record for an upsert, a
/// tombstone (stamps updated, content cleared) for a delete. A delete of a
/// record never seen inserts the tombstone so a later, older upsert loses.
fn put_row(conn: &Connection, c: &Change) -> Result<()> {
    let cols = columns_of(&c.table).expect("checked by caller");
    match c.op {
        ChangeOp::Upsert => {
            let obj = c
                .row
                .as_object()
                .ok_or_else(|| SyncError::Malformed("upsert without a row".into()))?;
            let mut names = vec!["id", "site", "site_seq", "hlc_ms", "hlc_ctr", "deleted"];
            let mut vals: Vec<rusqlite::types::Value> = vec![
                c.id.clone().into(),
                c.site.clone().into(),
                c.site_seq.into(),
                c.hlc_ms.into(),
                (c.hlc_ctr as i64).into(),
                0i64.into(),
            ];
            for col in cols {
                if let Some(v) = obj.get(*col) {
                    names.push(col);
                    vals.push(json_to_sql(v));
                }
            }
            let placeholders: Vec<String> = (1..=names.len()).map(|i| format!("?{i}")).collect();
            let updates: Vec<String> = names
                .iter()
                .skip(1)
                .map(|n| format!("{n} = excluded.{n}"))
                .collect();
            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT(id) DO UPDATE SET {}",
                c.table,
                names.join(", "),
                placeholders.join(", "),
                updates.join(", ")
            );
            conn.execute(&sql, params_from_iter(vals))?;
        }
        ChangeOp::Delete => {
            let clears: Vec<String> = cols
                .iter()
                .filter(|col| CLEARED_ON_DELETE.contains(col))
                .map(|col| format!("{col} = ''"))
                .collect();
            let set = if clears.is_empty() {
                String::new()
            } else {
                format!(", {}", clears.join(", "))
            };
            let updated = conn.execute(
                &format!(
                    "UPDATE {} SET site = ?1, site_seq = ?2, hlc_ms = ?3, hlc_ctr = ?4, deleted = 1{set} WHERE id = ?5",
                    c.table
                ),
                params![c.site, c.site_seq, c.hlc_ms, c.hlc_ctr as i64, c.id],
            )?;
            if updated == 0 {
                // Tombstone for a record this replica never held: the NOT NULL
                // columns get their defaults, `channel` and the like come from
                // nowhere, so they are filled from the change log later if a
                // row ever arrives — a tombstone only needs its stamps.
                let mut names = vec!["id", "site", "site_seq", "hlc_ms", "hlc_ctr", "deleted"];
                let mut vals: Vec<rusqlite::types::Value> = vec![
                    c.id.clone().into(),
                    c.site.clone().into(),
                    c.site_seq.into(),
                    c.hlc_ms.into(),
                    (c.hlc_ctr as i64).into(),
                    1i64.into(),
                ];
                for col in cols {
                    match *col {
                        "channel" | "slug" | "kind" => {
                            names.push(col);
                            vals.push(String::new().into());
                        }
                        "created_ms" | "updated_ms" => {
                            names.push(col);
                            vals.push(c.hlc_ms.into());
                        }
                        _ => {}
                    }
                }
                let placeholders: Vec<String> =
                    (1..=names.len()).map(|i| format!("?{i}")).collect();
                conn.execute(
                    &format!(
                        "INSERT INTO {} ({}) VALUES ({})",
                        c.table,
                        names.join(", "),
                        placeholders.join(", ")
                    ),
                    params_from_iter(vals),
                )?;
            }
        }
    }
    Ok(())
}

fn json_to_sql(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as S;
    match v {
        Value::Null => S::Null,
        Value::Bool(b) => S::Integer(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                S::Integer(i)
            } else {
                S::Real(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => S::Text(s.clone()),
        other => S::Text(other.to_string()),
    }
}

/// This site's own changes on a channel after a sequence number, for a
/// backfill request. Only `site == self` rows: a node speaks for itself.
pub fn changes_of_site_after(
    conn: &Connection,
    site: &str,
    channel: &str,
    after: i64,
    limit: usize,
) -> Result<Vec<Change>> {
    let mut stmt = conn.prepare(
        "SELECT tbl, op, record_id, site_seq, hlc_ms, hlc_ctr, row_json FROM change_log
         WHERE site = ?1 AND channel = ?2 AND site_seq > ?3 ORDER BY site_seq ASC LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![site, channel, after, limit as i64], |r| {
        let op: String = r.get(1)?;
        let row_json: Option<String> = r.get(6)?;
        Ok(Change {
            table: r.get(0)?,
            op: if op == "delete" {
                ChangeOp::Delete
            } else {
                ChangeOp::Upsert
            },
            id: r.get(2)?,
            site: site.to_string(),
            site_seq: r.get(3)?,
            hlc_ms: r.get(4)?,
            hlc_ctr: r.get::<_, i64>(5)? as u32,
            row: row_json
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or(Value::Null),
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The highest sequence seen from a site on a channel, for gap detection.
pub fn change_log_max(conn: &Connection, site: &str, channel: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(site_seq), 0) FROM change_log WHERE site = ?1 AND channel = ?2",
        params![site, channel],
        |r| r.get(0),
    )?)
}

/// Sites that have written on a channel, as this replica has seen them.
pub fn sites_on_channel(conn: &Connection, channel: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT site FROM change_log WHERE channel = ?1")?;
    let rows = stmt.query_map([channel], |r| r.get(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Drop tombstones (and their log rows) older than `older_than_ms`. Kept
/// long enough that a straggling older upsert still loses to them.
pub fn prune_tombstones(conn: &Connection, older_than_ms: i64) -> Result<usize> {
    let mut n = 0;
    for (table, _) in crate::TABLES {
        n += conn.execute(
            &format!("DELETE FROM {table} WHERE deleted = 1 AND hlc_ms < ?1"),
            [older_than_ms],
        )?;
    }
    conn.execute(
        "DELETE FROM change_log WHERE op = 'delete' AND hlc_ms < ?1",
        [older_than_ms],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::install(&c).unwrap();
        c
    }

    fn doc(slug: &str, body: &str) -> Value {
        json!({"channel": "personal", "slug": slug, "kind": "guide", "title": slug, "body": body, "hash": "h", "created_ms": 1, "updated_ms": 1})
    }

    fn dump(conn: &Connection) -> Vec<(String, String, i64)> {
        let mut stmt = conn
            .prepare("SELECT id, body, deleted FROM document ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn a_local_write_is_stamped_logged_and_readable() {
        let mut a = db();
        let c = write_change(
            &mut a,
            "A",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d1",
            doc("x", "one"),
            1000,
        )
        .unwrap();
        assert_eq!((c.site_seq, c.hlc_ms, c.hlc_ctr), (1, 1000, 0));
        let c2 = write_change(
            &mut a,
            "A",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d1",
            doc("x", "two"),
            1000,
        )
        .unwrap();
        assert_eq!((c2.site_seq, c2.hlc_ms, c2.hlc_ctr), (2, 1000, 1));
        assert_eq!(dump(&a), vec![("d1".into(), "two".into(), 0)]);
        assert_eq!(change_log_max(&a, "A", "personal").unwrap(), 2);
        assert_eq!(
            changes_of_site_after(&a, "A", "personal", 1, 10).unwrap(),
            vec![c2]
        );
        assert_eq!(
            sites_on_channel(&a, "personal").unwrap(),
            vec!["A".to_string()]
        );
    }

    #[test]
    fn changes_converge_in_every_order_and_the_later_hlc_wins() {
        let mut a = db();
        let mut b = db();
        let ca = write_change(
            &mut a,
            "A",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d1",
            doc("x", "from a"),
            1000,
        )
        .unwrap();
        let cb = write_change(
            &mut b,
            "B",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d1",
            doc("x", "from b"),
            1001,
        )
        .unwrap();
        let cb2 = write_change(
            &mut b,
            "B",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d2",
            doc("y", "only b"),
            1002,
        )
        .unwrap();
        let all = [ca.clone(), cb.clone(), cb2.clone()];
        let orders: Vec<Vec<usize>> = vec![
            vec![0, 1, 2],
            vec![2, 1, 0],
            vec![1, 0, 2],
            vec![1, 2, 0],
            vec![0, 2, 1],
            vec![2, 0, 1],
        ];
        let mut dumps = Vec::new();
        for order in orders {
            let mut c = db();
            for i in order {
                let ch = &all[i];
                apply_changes(&mut c, &ch.site, "personal", std::slice::from_ref(ch), 5000)
                    .unwrap();
            }
            dumps.push(dump(&c));
        }
        for d in &dumps {
            assert_eq!(d, &dumps[0]);
        }
        assert_eq!(
            dumps[0],
            vec![
                ("d1".into(), "from b".into(), 0),
                ("d2".into(), "only b".into(), 0)
            ]
        );
        // And the originals converge too.
        apply_changes(&mut a, "B", "personal", &[cb.clone(), cb2.clone()], 5000).unwrap();
        let r = apply_changes(&mut b, "A", "personal", std::slice::from_ref(&ca), 5000).unwrap();
        assert_eq!(r, vec![Applied::Lost]);
        assert_eq!(dump(&a), dump(&b));
    }

    #[test]
    fn duplicates_impersonation_and_malformed_are_named() {
        let mut a = db();
        let mut b = db();
        let ca = write_change(
            &mut a,
            "A",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d1",
            doc("x", "a"),
            1000,
        )
        .unwrap();
        assert_eq!(
            apply_changes(&mut b, "A", "personal", std::slice::from_ref(&ca), 2000).unwrap(),
            vec![Applied::Stored]
        );
        assert_eq!(
            apply_changes(&mut b, "A", "personal", std::slice::from_ref(&ca), 2000).unwrap(),
            vec![Applied::Duplicate]
        );
        assert_eq!(
            apply_changes(&mut b, "C", "personal", std::slice::from_ref(&ca), 2000).unwrap(),
            vec![Applied::Impersonation]
        );
        let mut bad = ca.clone();
        bad.table = "nope".into();
        bad.site_seq = 9;
        assert_eq!(
            apply_changes(&mut b, "A", "personal", &[bad], 2000).unwrap(),
            vec![Applied::Malformed]
        );
        // The receiver's clock moved past what it saw.
        let mut h = Hlc::load(&b).unwrap();
        assert!(h.tick(0) > (1000, 0));
    }

    #[test]
    fn a_tombstone_beats_an_older_upsert_and_clears_content() {
        let mut a = db();
        let mut b = db();
        let ca = write_change(
            &mut a,
            "A",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d1",
            doc("x", "secret"),
            1000,
        )
        .unwrap();
        apply_changes(&mut b, "A", "personal", std::slice::from_ref(&ca), 1500).unwrap();
        let del = write_change(
            &mut b,
            "B",
            "personal",
            "document",
            ChangeOp::Delete,
            "d1",
            Value::Null,
            2000,
        )
        .unwrap();
        assert_eq!(dump(&b), vec![("d1".into(), String::new(), 1)]);
        // A late, older upsert loses to the tombstone.
        let stale = Change {
            site_seq: 2,
            ..ca.clone()
        };
        // (same stamps as the original: older than the delete)
        let mut c = db();
        apply_changes(&mut c, "B", "personal", std::slice::from_ref(&del), 3000).unwrap();
        assert_eq!(
            apply_changes(&mut c, "A", "personal", &[stale], 3000).unwrap(),
            vec![Applied::Lost]
        );
        assert_eq!(dump(&c), vec![("d1".into(), String::new(), 1)]);
        // Pruning removes it once old enough.
        assert_eq!(prune_tombstones(&c, 2001).unwrap(), 1);
        assert!(dump(&c).is_empty());
    }

    #[test]
    fn memory_and_promotion_rows_travel_the_same_way() {
        let mut a = db();
        let mut b = db();
        let m = write_change(
            &mut a, "A", "personal", "memory", ChangeOp::Upsert, "m1",
            json!({"channel": "personal", "scope": "global", "kind": "directive", "body": "run just test", "confidence": 1.0, "state": "active", "created_ms": 1, "updated_ms": 1}),
            1000,
        ).unwrap();
        let p = write_change(
            &mut a,
            "A",
            "personal",
            "promotion",
            ChangeOp::Upsert,
            "p1",
            json!({"channel": "personal", "items_json": "[]", "state": "open", "created_ms": 1}),
            1001,
        )
        .unwrap();
        assert_eq!(
            apply_changes(&mut b, "A", "personal", &[m, p], 2000).unwrap(),
            vec![Applied::Stored, Applied::Stored]
        );
        let hits: i64 = b
            .query_row(
                "SELECT count(*) FROM memory_fts WHERE memory_fts MATCH 'test'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
        let state: String = b
            .query_row("SELECT state FROM promotion WHERE id = 'p1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "open");
    }
}
