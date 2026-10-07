//! How much of each table the node holds, for the data pane in Settings.

use super::{Result, Store};

/// Rows and their approximate weight in one table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Weight {
    pub rows: i64,
    /// The summed length of every column's value: what the rows carry, not
    /// the pages SQLite keeps them in.
    pub bytes: i64,
}

impl Store {
    /// The rows of `table` (a name from the node's own schema, never input)
    /// and what they carry. Replicated tables count only live rows: a
    /// tombstone is a delete the node remembers, not something it holds. A
    /// table this database does not have weighs nothing.
    pub fn table_weight(&self, table: &str) -> Result<Weight> {
        let conn = self.conn.lock().unwrap();
        let columns: Vec<String> = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        if columns.is_empty() {
            return Ok(Weight::default());
        }
        let bytes = columns
            .iter()
            .map(|c| format!("COALESCE(LENGTH(\"{c}\"), 0)"))
            .collect::<Vec<_>>()
            .join(" + ");
        let live = if columns.iter().any(|c| c == "deleted") {
            " WHERE deleted = 0"
        } else {
            ""
        };
        let (rows, bytes) = conn.query_row(
            &format!("SELECT COUNT(*), COALESCE(SUM({bytes}), 0) FROM \"{table}\"{live}"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Weight { rows, bytes })
    }

    pub fn has_table(&self, table: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n > 0)
    }

    /// The database file's size: every page SQLite holds, free ones included.
    pub fn database_bytes(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tables_weight_counts_live_rows_and_what_they_carry() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.table_weight("work_item").unwrap().rows, 0);
        assert_eq!(
            store.table_weight("no_such_table").unwrap(),
            Weight::default()
        );
        store.ensure_peer_node("n1").unwrap();
        let node = store.table_weight("node").unwrap();
        assert_eq!(node.rows, 1);
        assert!(node.bytes >= 2);
        assert!(store.database_bytes().unwrap() > 0);
    }
}
