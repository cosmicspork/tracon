//! The replicated schema, installed behind its own version so the node's
//! migration list and the hub's fresh database both call one function.

use rusqlite::Connection;

/// Ordered, append-only. Each step runs once per database.
const STEPS: &[&str] = &[r#"
    CREATE TABLE IF NOT EXISTS hlc (
        id       INTEGER PRIMARY KEY CHECK (id = 1),
        last_ms  INTEGER NOT NULL,
        last_ctr INTEGER NOT NULL
    );
    INSERT OR IGNORE INTO hlc (id, last_ms, last_ctr) VALUES (1, 0, 0);

    CREATE TABLE IF NOT EXISTS change_log (
        site       TEXT    NOT NULL,
        site_seq   INTEGER NOT NULL,
        channel    TEXT    NOT NULL,
        tbl        TEXT    NOT NULL,
        op         TEXT    NOT NULL,
        record_id  TEXT    NOT NULL,
        hlc_ms     INTEGER NOT NULL,
        hlc_ctr    INTEGER NOT NULL,
        row_json   TEXT,
        applied    INTEGER NOT NULL,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY (site, site_seq)
    );
    CREATE INDEX IF NOT EXISTS change_log_channel ON change_log(channel, site, site_seq);

    CREATE TABLE IF NOT EXISTS document (
        id         TEXT PRIMARY KEY,
        channel    TEXT NOT NULL,
        slug       TEXT NOT NULL,
        kind       TEXT NOT NULL DEFAULT 'other',
        title      TEXT NOT NULL DEFAULT '',
        body       TEXT NOT NULL DEFAULT '',
        hash       TEXT NOT NULL DEFAULT '',
        site       TEXT NOT NULL,
        site_seq   INTEGER NOT NULL,
        hlc_ms     INTEGER NOT NULL,
        hlc_ctr    INTEGER NOT NULL DEFAULT 0,
        deleted    INTEGER NOT NULL DEFAULT 0,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS document_slug ON document(channel, slug);

    CREATE TABLE IF NOT EXISTS memory (
        id             TEXT PRIMARY KEY,
        channel        TEXT NOT NULL,
        scope          TEXT NOT NULL DEFAULT 'global',
        scope_ref      TEXT,
        kind           TEXT NOT NULL,
        body           TEXT NOT NULL DEFAULT '',
        source_session TEXT,
        source_node    TEXT,
        confidence     REAL NOT NULL DEFAULT 1.0,
        state          TEXT NOT NULL DEFAULT 'active',
        site           TEXT NOT NULL,
        site_seq       INTEGER NOT NULL,
        hlc_ms         INTEGER NOT NULL,
        hlc_ctr        INTEGER NOT NULL DEFAULT 0,
        deleted        INTEGER NOT NULL DEFAULT 0,
        created_ms     INTEGER NOT NULL,
        updated_ms     INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS memory_channel ON memory(channel, state, kind);

    CREATE TABLE IF NOT EXISTS promotion (
        id            TEXT PRIMARY KEY,
        channel       TEXT NOT NULL,
        items_json    TEXT NOT NULL DEFAULT '[]',
        state         TEXT NOT NULL DEFAULT 'open',
        verdicts_json TEXT,
        decided_by    TEXT,
        decided_ms    INTEGER,
        site          TEXT NOT NULL,
        site_seq      INTEGER NOT NULL,
        hlc_ms        INTEGER NOT NULL,
        hlc_ctr       INTEGER NOT NULL DEFAULT 0,
        deleted       INTEGER NOT NULL DEFAULT 0,
        created_ms    INTEGER NOT NULL
    );

    CREATE VIRTUAL TABLE IF NOT EXISTS document_fts USING fts5(
        title, body, content='document', content_rowid='rowid', tokenize='porter unicode61'
    );
    CREATE TRIGGER IF NOT EXISTS document_ai AFTER INSERT ON document BEGIN
        INSERT INTO document_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body);
    END;
    CREATE TRIGGER IF NOT EXISTS document_ad AFTER DELETE ON document BEGIN
        INSERT INTO document_fts(document_fts, rowid, title, body) VALUES ('delete', old.rowid, old.title, old.body);
    END;
    CREATE TRIGGER IF NOT EXISTS document_au AFTER UPDATE ON document BEGIN
        INSERT INTO document_fts(document_fts, rowid, title, body) VALUES ('delete', old.rowid, old.title, old.body);
        INSERT INTO document_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body);
    END;

    CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
        body, content='memory', content_rowid='rowid', tokenize='porter unicode61'
    );
    CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory BEGIN
        INSERT INTO memory_fts(rowid, body) VALUES (new.rowid, new.body);
    END;
    CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory BEGIN
        INSERT INTO memory_fts(memory_fts, rowid, body) VALUES ('delete', old.rowid, old.body);
    END;
    CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory BEGIN
        INSERT INTO memory_fts(memory_fts, rowid, body) VALUES ('delete', old.rowid, old.body);
        INSERT INTO memory_fts(rowid, body) VALUES (new.rowid, new.body);
    END;
    "#];

/// Install or upgrade the replicated schema. Safe to call on every open.
pub fn install(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_schema (id INTEGER PRIMARY KEY CHECK (id = 1), version INTEGER NOT NULL);
         INSERT OR IGNORE INTO sync_schema (id, version) VALUES (1, 0);",
    )?;
    let version: i64 = conn.query_row("SELECT version FROM sync_schema WHERE id = 1", [], |r| {
        r.get(0)
    })?;
    for (i, ddl) in STEPS.iter().enumerate() {
        let target = i as i64 + 1;
        if version < target {
            conn.execute_batch(ddl)?;
            conn.execute("UPDATE sync_schema SET version = ?1 WHERE id = 1", [target])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent_and_fts_answers() {
        let conn = Connection::open_in_memory().unwrap();
        install(&conn).unwrap();
        install(&conn).unwrap();
        conn.execute(
            "INSERT INTO document (id, channel, slug, title, body, site, site_seq, hlc_ms, created_ms, updated_ms)
             VALUES ('d', 'personal', 'guide-x', 'Test command', 'run just test', 's', 1, 1, 1, 1)",
            [],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM document_fts WHERE document_fts MATCH 'tests'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "porter stemming finds 'test' from 'tests'");
        conn.execute(
            "UPDATE document SET body = 'nothing here' WHERE id = 'd'",
            [],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM document_fts WHERE document_fts MATCH 'just'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0, "the update trigger replaced the indexed text");
    }
}
