//! The vector index, beside FTS5 rather than instead of it.
//!
//! FTS is the floor: the highest-value lookups in this corpus are exact ("what
//! is the test command"), and pure vector search is bad at exactly those. What
//! vectors add is the query whose words are not the document's words. So both
//! run, and `corpus::recall` fuses them.
//!
//! Everything here is node-local. A vector is not a safe form of encrypted
//! content — embedding inversion recovers much of the source text from the
//! vector alone — so replicating one would give the hub a readable index of a
//! channel it cannot open. Each node embeds its own replica instead.

use rusqlite::{params, Connection, Result};

use super::{now_ms, Store};

/// One chunk's vector, with everything needed to know whether it is stale.
#[derive(Debug, Clone, PartialEq)]
pub struct Vector {
    pub source_table: String,
    pub source_id: String,
    pub chunk_ix: i64,
    pub channel: String,
    pub model: String,
    pub text_hash: String,
    /// Where this chunk sits in the source body, so a hit can render the text
    /// it actually matched rather than the whole document.
    pub offset: i64,
    pub len: i64,
}

/// A neighbour, as the index found it. `distance` is L2 over normalized
/// vectors, so smaller is nearer.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbour {
    pub source_table: String,
    pub source_id: String,
    pub chunk_ix: i64,
    pub offset: i64,
    pub len: i64,
    pub distance: f64,
}

/// The vector table's name. It is created on demand rather than in a migration
/// because `vec0` fixes the dimension at creation and the dimension is the
/// embedding model's, which is configuration.
const VEC_TABLE: &str = "embedding_vec";

fn as_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Unit-normalize, so L2 distance ranks the same way cosine similarity would
/// and a long chunk does not beat a short one on magnitude alone.
fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

fn index_dim(conn: &Connection) -> Option<usize> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![VEC_TABLE],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|sql| {
        let start = sql.find("float[")? + "float[".len();
        let end = sql[start..].find(']')? + start;
        sql[start..end].parse().ok()
    })
}

impl Store {
    /// Make sure the index exists at `dim`. A changed dimension means a changed
    /// embedding model, and vectors from two models are not comparable — so the
    /// index is rebuilt from empty rather than silently mixed, and the metadata
    /// rows go with it so the backfill re-embeds everything.
    pub fn vec_ensure(&self, dim: usize) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        match index_dim(&conn) {
            Some(d) if d == dim => return Ok(()),
            Some(_) => {
                conn.execute_batch(&format!("DROP TABLE {VEC_TABLE}; DELETE FROM embedding;"))?;
            }
            None => {}
        }
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {VEC_TABLE} USING vec0(embedding float[{dim}]);"
        ))
    }

    /// Store one chunk's vector, replacing whatever was there for that chunk.
    pub fn vec_put(&self, v: &Vector, embedding: &[f32]) -> Result<()> {
        let normalized = normalize(embedding);
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // Clear the old vector *by its rowid* before the metadata row goes.
        // SQLite hands a reused rowid to the next insert, and `vec0` has no
        // upsert — so leaving the old vector behind is not a stale row that
        // the join hides, it is a primary-key collision on the next write.
        let stale: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM embedding
                 WHERE source_table = ?1 AND source_id = ?2 AND chunk_ix = ?3",
            )?;
            let rows = stmt.query_map(params![v.source_table, v.source_id, v.chunk_ix], |r| {
                r.get(0)
            })?;
            rows.collect::<Result<_>>()?
        };
        for id in stale {
            tx.execute(
                &format!("DELETE FROM {VEC_TABLE} WHERE rowid = ?1"),
                params![id],
            )?;
        }
        tx.execute(
            "DELETE FROM embedding WHERE source_table = ?1 AND source_id = ?2 AND chunk_ix = ?3",
            params![v.source_table, v.source_id, v.chunk_ix],
        )?;
        tx.execute(
            "INSERT INTO embedding (source_table, source_id, chunk_ix, channel, model, dim,
                text_hash, offset, len, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                v.source_table,
                v.source_id,
                v.chunk_ix,
                v.channel,
                v.model,
                normalized.len() as i64,
                v.text_hash,
                v.offset,
                v.len,
                now_ms()
            ],
        )?;
        let id = tx.last_insert_rowid();
        // The vector row is keyed to the metadata rowid, so the two cannot
        // drift: one transaction writes both or neither.
        tx.execute(
            &format!("INSERT INTO {VEC_TABLE} (rowid, embedding) VALUES (?1, ?2)"),
            params![id, as_blob(&normalized)],
        )?;
        tx.commit()
    }

    /// Forget every chunk of one record — an edit before re-embedding, or a
    /// delete.
    pub fn vec_forget(&self, source_table: &str, source_id: &str) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let ids: Vec<i64> = {
            let mut stmt =
                tx.prepare("SELECT id FROM embedding WHERE source_table = ?1 AND source_id = ?2")?;
            let rows = stmt.query_map(params![source_table, source_id], |r| r.get(0))?;
            rows.collect::<Result<_>>()?
        };
        for id in &ids {
            tx.execute(
                &format!("DELETE FROM {VEC_TABLE} WHERE rowid = ?1"),
                params![id],
            )?;
        }
        tx.execute(
            "DELETE FROM embedding WHERE source_table = ?1 AND source_id = ?2",
            params![source_table, source_id],
        )?;
        tx.commit()?;
        Ok(ids.len())
    }

    /// What this node has already embedded for a record, so a caller can tell
    /// a stale chunk from a current one without re-embedding to find out.
    pub fn vec_hashes(&self, source_table: &str, source_id: &str) -> Result<Vec<(i64, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT chunk_ix, text_hash FROM embedding
             WHERE source_table = ?1 AND source_id = ?2 ORDER BY chunk_ix",
        )?;
        let rows = stmt.query_map(params![source_table, source_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect()
    }

    /// The nearest chunks to `query`, within one channel. Returns nothing when
    /// the index does not exist yet, which is the honest answer before the
    /// first embedding has been written.
    pub fn vec_search(
        &self,
        channel: Option<&str>,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<Neighbour>> {
        let conn = self.conn.lock().unwrap();
        if index_dim(&conn).is_none() {
            return Ok(Vec::new());
        }
        // `k` is asked of the index before the channel filter is applied, so
        // over-fetch: otherwise a channel with few documents returns nothing
        // while another channel's neighbours fill the k slots.
        let k = (limit * 8).max(limit) as i64;
        let mut stmt = conn.prepare(&format!(
            "SELECT e.source_table, e.source_id, e.chunk_ix, e.offset, e.len, v.distance
             FROM {VEC_TABLE} v JOIN embedding e ON e.id = v.rowid
             WHERE v.embedding MATCH ?1 AND k = ?2 AND (?3 IS NULL OR e.channel = ?3)
             ORDER BY v.distance LIMIT ?4"
        ))?;
        let normalized = normalize(query);
        let rows = stmt.query_map(
            params![as_blob(&normalized), k, channel, limit as i64],
            |r| {
                Ok(Neighbour {
                    source_table: r.get(0)?,
                    source_id: r.get(1)?,
                    chunk_ix: r.get(2)?,
                    offset: r.get(3)?,
                    len: r.get(4)?,
                    distance: r.get(5)?,
                })
            },
        )?;
        rows.collect()
    }

    /// How many chunks are indexed, for the Nodes screen and for tests.
    pub fn vec_count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM embedding", [], |r| r.get(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(id: &str, channel: &str, hash: &str) -> Vector {
        Vector {
            source_table: "document".into(),
            source_id: id.into(),
            chunk_ix: 0,
            channel: channel.into(),
            model: "test-embed".into(),
            text_hash: hash.into(),
            offset: 0,
            len: 10,
        }
    }

    #[test]
    fn the_extension_is_linked_in_and_answers() {
        let s = Store::open_in_memory().unwrap();
        let conn = s.conn.lock().unwrap();
        let v: String = conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))
            .expect("sqlite-vec is compiled in");
        assert!(v.starts_with('v'), "{v}");
    }

    #[test]
    fn the_nearest_neighbour_is_the_one_pointing_the_same_way() {
        let s = Store::open_in_memory().unwrap();
        s.vec_ensure(4).unwrap();
        s.vec_put(&vector("a", "personal", "h1"), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        s.vec_put(&vector("b", "personal", "h2"), &[0.0, 1.0, 0.0, 0.0])
            .unwrap();
        let hits = s
            .vec_search(Some("personal"), &[0.9, 0.1, 0.0, 0.0], 2)
            .unwrap();
        assert_eq!(hits[0].source_id, "a");
        assert!(hits[0].distance < hits[1].distance);
    }

    /// Magnitude is not relevance: normalizing is what stops a long chunk
    /// outranking a short one that means the same thing.
    #[test]
    fn magnitude_does_not_decide_the_ranking() {
        let s = Store::open_in_memory().unwrap();
        s.vec_ensure(4).unwrap();
        s.vec_put(&vector("small", "personal", "h"), &[0.1, 0.0, 0.0, 0.0])
            .unwrap();
        s.vec_put(&vector("big", "personal", "h"), &[0.0, 9.0, 0.0, 0.0])
            .unwrap();
        let hits = s.vec_search(None, &[5.0, 0.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(hits[0].source_id, "small");
    }

    #[test]
    fn a_channel_never_sees_another_channels_vectors() {
        let s = Store::open_in_memory().unwrap();
        s.vec_ensure(4).unwrap();
        s.vec_put(&vector("work-doc", "work", "h"), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        let hits = s
            .vec_search(Some("personal"), &[1.0, 0.0, 0.0, 0.0], 5)
            .unwrap();
        assert!(hits.is_empty());
        let hits = s
            .vec_search(Some("work"), &[1.0, 0.0, 0.0, 0.0], 5)
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn re_embedding_a_chunk_replaces_it_rather_than_duplicating_it() {
        let s = Store::open_in_memory().unwrap();
        s.vec_ensure(4).unwrap();
        s.vec_put(&vector("a", "personal", "old"), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        s.vec_put(&vector("a", "personal", "new"), &[0.0, 1.0, 0.0, 0.0])
            .unwrap();
        assert_eq!(s.vec_count().unwrap(), 1);
        assert_eq!(
            s.vec_hashes("document", "a").unwrap(),
            vec![(0, "new".to_string())]
        );
        // And the vector moved with the metadata, rather than the index still
        // answering with the old direction.
        let hits = s.vec_search(None, &[0.0, 1.0, 0.0, 0.0], 1).unwrap();
        assert!(hits[0].distance < 0.01, "{:?}", hits[0]);
    }

    #[test]
    fn forgetting_a_record_clears_the_index_too() {
        let s = Store::open_in_memory().unwrap();
        s.vec_ensure(4).unwrap();
        s.vec_put(&vector("a", "personal", "h"), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        assert_eq!(s.vec_forget("document", "a").unwrap(), 1);
        assert_eq!(s.vec_count().unwrap(), 0);
        assert!(s
            .vec_search(None, &[1.0, 0.0, 0.0, 0.0], 5)
            .unwrap()
            .is_empty());
    }

    /// Vectors from two different models are not comparable, so a changed
    /// dimension has to empty the index rather than mix them.
    #[test]
    fn changing_the_dimension_rebuilds_the_index_from_empty() {
        let s = Store::open_in_memory().unwrap();
        s.vec_ensure(4).unwrap();
        s.vec_put(&vector("a", "personal", "h"), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        s.vec_ensure(8).unwrap();
        assert_eq!(s.vec_count().unwrap(), 0);
        s.vec_put(
            &vector("a", "personal", "h"),
            &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        )
        .unwrap();
        assert_eq!(s.vec_count().unwrap(), 1);
    }

    /// Searching before anything is indexed is a normal state, not an error:
    /// it is what every node looks like until the first backfill finishes.
    #[test]
    fn searching_an_index_that_does_not_exist_yet_is_empty_not_an_error() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.vec_search(None, &[1.0, 0.0], 5).unwrap().is_empty());
    }
}
