//! The nightly promotion batch, as plans over the replicated tables. A plan
//! is pure: what to write is decided here, and written by whoever runs the
//! batch (a node for its channels, the hub for the ones shared with it) via
//! their own `write_change`, so every row travels like any other.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::Result;

/// One memory in a batch, as the operator sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub memory_id: String,
    pub kind: String,
    pub scope: String,
    pub scope_ref: Option<String>,
    pub body: String,
    pub confidence: f64,
    pub source_session: Option<String>,
    pub source_node: Option<String>,
    pub created_ms: i64,
}

/// A batch to write: the `promotion` row and the memories it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub promotion_id: String,
    pub channel: String,
    pub items: Vec<Item>,
}

impl Plan {
    /// The `promotion` row as a change row.
    pub fn promotion_row(&self, now_ms: i64) -> Value {
        json!({
            "channel": self.channel,
            "items_json": serde_json::to_string(&self.items).unwrap_or_else(|_| "[]".into()),
            "state": "open",
            "verdicts_json": Value::Null,
            "decided_by": Value::Null,
            "decided_ms": Value::Null,
            "created_ms": now_ms,
        })
    }
}

/// Candidates on a channel older than `min_age_ms`: what the agent retained
/// and was told would wait. `None` when there is nothing to batch.
pub fn plan_promotion(
    conn: &Connection,
    channel: &str,
    promotion_id: &str,
    now_ms: i64,
    min_age_ms: i64,
) -> Result<Option<Plan>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, scope, scope_ref, body, confidence, source_session, source_node, created_ms
         FROM memory WHERE channel = ?1 AND deleted = 0 AND state = 'candidate' AND created_ms <= ?2
         ORDER BY created_ms ASC LIMIT 200",
    )?;
    let items: Vec<Item> = stmt
        .query_map(params![channel, now_ms - min_age_ms], |r| {
            Ok(Item {
                memory_id: r.get(0)?,
                kind: r.get(1)?,
                scope: r.get(2)?,
                scope_ref: r.get(3)?,
                body: r.get(4)?,
                confidence: r.get(5)?,
                source_session: r.get(6)?,
                source_node: r.get(7)?,
                created_ms: r.get(8)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(Plan {
        promotion_id: promotion_id.to_string(),
        channel: channel.to_string(),
        items,
    }))
}

/// A memory row as it should read once it is part of a batch, or decided.
/// Reads the current row so the change carries the whole record.
pub fn memory_row_with_state(
    conn: &Connection,
    memory_id: &str,
    state: &str,
    now_ms: i64,
) -> Result<Option<Value>> {
    let row: Option<Value> = conn
        .query_row(
            "SELECT channel, scope, scope_ref, kind, body, source_session, source_node, confidence, created_ms
             FROM memory WHERE id = ?1 AND deleted = 0",
            [memory_id],
            |r| {
                Ok(json!({
                    "channel": r.get::<_, String>(0)?, "scope": r.get::<_, String>(1)?,
                    "scope_ref": r.get::<_, Option<String>>(2)?, "kind": r.get::<_, String>(3)?,
                    "body": r.get::<_, String>(4)?, "source_session": r.get::<_, Option<String>>(5)?,
                    "source_node": r.get::<_, Option<String>>(6)?, "confidence": r.get::<_, f64>(7)?,
                    "state": state, "created_ms": r.get::<_, i64>(8)?, "updated_ms": now_ms,
                }))
            },
        )
        .ok();
    Ok(row)
}

/// The decided `promotion` row and each memory's new state.
pub type VerdictPlan = (Value, Vec<(String, &'static str)>);

/// The verdicts on a batch: `memory_id → "promote" | "reject"`. Returns the
/// decided `promotion` row and each memory's new state, for the caller to
/// write. An item not named keeps waiting: the batch stays open until every
/// item is decided.
pub fn plan_verdict(
    conn: &Connection,
    promotion_id: &str,
    verdicts: &serde_json::Map<String, Value>,
    decided_by: &str,
    now_ms: i64,
) -> Result<Option<VerdictPlan>> {
    let (channel, items_json, state, existing): (String, String, String, Option<String>) = match conn
        .query_row(
            "SELECT channel, items_json, state, verdicts_json FROM promotion WHERE id = ?1 AND deleted = 0",
            [promotion_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if state != "open" {
        return Ok(None);
    }
    let items: Vec<Item> = serde_json::from_str(&items_json).unwrap_or_default();
    let mut all: serde_json::Map<String, Value> = existing
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut memories = Vec::new();
    for (id, v) in verdicts {
        if !items.iter().any(|i| &i.memory_id == id) {
            continue;
        }
        let state = match v.as_str() {
            Some("promote") => "promoted",
            Some("reject") => "rejected",
            _ => continue,
        };
        all.insert(id.clone(), v.clone());
        memories.push((id.clone(), state));
    }
    let complete = items.iter().all(|i| all.contains_key(&i.memory_id));
    let row = json!({
        "channel": channel,
        "items_json": items_json,
        "state": if complete { "decided" } else { "open" },
        "verdicts_json": serde_json::to_string(&all).unwrap_or_default(),
        "decided_by": if complete { Value::String(decided_by.to_string()) } else { Value::Null },
        "decided_ms": if complete { json!(now_ms) } else { Value::Null },
        "created_ms": conn.query_row("SELECT created_ms FROM promotion WHERE id = ?1", [promotion_id], |r| r.get::<_, i64>(0)).unwrap_or(now_ms),
    });
    Ok(Some((row, memories)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{write_change, ChangeOp};

    fn mem(state: &str, created: i64) -> Value {
        json!({"channel": "personal", "scope": "global", "scope_ref": null, "kind": "lesson", "body": "b",
               "source_session": "s", "source_node": "n", "confidence": 0.8, "state": state,
               "created_ms": created, "updated_ms": created})
    }

    #[test]
    fn candidates_old_enough_are_planned_and_verdicts_close_the_batch() {
        let mut c = Connection::open_in_memory().unwrap();
        crate::schema::install(&c).unwrap();
        write_change(
            &mut c,
            "n",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-old",
            mem("candidate", 1_000),
            5_000,
        )
        .unwrap();
        write_change(
            &mut c,
            "n",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-new",
            mem("candidate", 4_900),
            5_000,
        )
        .unwrap();
        write_change(
            &mut c,
            "n",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-active",
            mem("active", 1_000),
            5_000,
        )
        .unwrap();
        let plan = plan_promotion(&c, "personal", "p1", 5_000, 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.items
                .iter()
                .map(|i| i.memory_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m-old"]
        );
        assert!(plan_promotion(&c, "work", "p2", 5_000, 0)
            .unwrap()
            .is_none());

        write_change(
            &mut c,
            "n",
            "personal",
            "promotion",
            ChangeOp::Upsert,
            "p1",
            plan.promotion_row(5_000),
            5_000,
        )
        .unwrap();
        let row = memory_row_with_state(&c, "m-old", "proposed", 5_000)
            .unwrap()
            .unwrap();
        write_change(
            &mut c,
            "n",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-old",
            row,
            5_000,
        )
        .unwrap();

        let mut v = serde_json::Map::new();
        v.insert("m-old".into(), json!("promote"));
        v.insert("m-not-in-batch".into(), json!("promote"));
        let (prow, mems) = plan_verdict(&c, "p1", &v, "n", 6_000).unwrap().unwrap();
        assert_eq!(prow["state"], "decided");
        assert_eq!(prow["decided_by"], "n");
        assert_eq!(mems, vec![("m-old".to_string(), "promoted")]);
        write_change(
            &mut c,
            "n",
            "personal",
            "promotion",
            ChangeOp::Upsert,
            "p1",
            prow,
            6_000,
        )
        .unwrap();
        assert!(
            plan_verdict(&c, "p1", &v, "n", 6_001).unwrap().is_none(),
            "decided batches are closed"
        );
    }
}
