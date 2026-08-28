//! Inline DDL with `user_version` migrations, following review's style: no
//! migration framework, statements applied in order and idempotently.

use rusqlite::Connection;

/// Ordered, append-only. Each entry is applied once; never edit a shipped one.
const MIGRATIONS: &[&str] = &[
    // 1: the Phase 1 vertical slice.
    r#"
    CREATE TABLE node (
        id             TEXT PRIMARY KEY,
        name           TEXT NOT NULL,
        state          TEXT NOT NULL,
        failed_check   TEXT,
        failed_detail  TEXT,
        harness_id     TEXT NOT NULL,
        harness_pinned TEXT NOT NULL,
        harness_found  TEXT,
        models_json    TEXT,
        checked_at_ms  INTEGER
    );

    CREATE TABLE session (
        id                 TEXT PRIMARY KEY,
        node_id            TEXT NOT NULL REFERENCES node(id),
        channel            TEXT NOT NULL,
        work_item_id       TEXT,
        repo_path          TEXT NOT NULL,
        worktree_path      TEXT,
        branch             TEXT NOT NULL,
        harness_id         TEXT NOT NULL,
        harness_version    TEXT NOT NULL,
        harness_session_id TEXT,
        container_name     TEXT,
        model              TEXT NOT NULL,
        budget_tokens      INTEGER NOT NULL,
        tokens_used        INTEGER NOT NULL DEFAULT 0,
        cost_usd           REAL,
        context_used       INTEGER,
        context_size       INTEGER,
        state              TEXT NOT NULL,
        end_reason         TEXT,
        last_error         TEXT,
        turn_active        INTEGER NOT NULL DEFAULT 0,
        draft              TEXT,
        draft_updated_ms   INTEGER,
        created_ms         INTEGER NOT NULL,
        started_mono_ms    INTEGER,
        ended_mono_ms      INTEGER,
        updated_ms         INTEGER NOT NULL
    );
    CREATE INDEX session_state ON session(state, created_ms);

    CREATE TABLE event (
        seq          INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id   TEXT NOT NULL REFERENCES session(id),
        work_item_id TEXT,
        kind         TEXT NOT NULL,
        ref_id       TEXT,
        payload      TEXT NOT NULL,
        at_ms        INTEGER NOT NULL,
        mono_ms      INTEGER NOT NULL
    );
    CREATE INDEX event_session ON event(session_id, seq);

    CREATE TABLE permission_request (
        id               TEXT PRIMARY KEY,
        session_id       TEXT NOT NULL REFERENCES session(id),
        node_id          TEXT NOT NULL,
        rpc_id           INTEGER NOT NULL,
        tool_call_id     TEXT,
        title            TEXT NOT NULL,
        kind             TEXT,
        raw_input        TEXT,
        options          TEXT NOT NULL,
        state            TEXT NOT NULL,
        answer_option_id TEXT,
        created_ms       INTEGER NOT NULL,
        created_mono_ms  INTEGER NOT NULL,
        resolved_mono_ms INTEGER,
        expires_ms       INTEGER NOT NULL
    );
    CREATE INDEX perm_open ON permission_request(state, created_ms);
    "#,
    // 2: the review contract. Reviews are the second thing that can wait on the
    // operator, and they outlive the turn that submitted them.
    r#"
    CREATE TABLE review (
        id              TEXT PRIMARY KEY,
        session_id      TEXT NOT NULL REFERENCES session(id),
        node_id         TEXT NOT NULL,
        channel         TEXT NOT NULL,
        kind            TEXT NOT NULL,
        title           TEXT NOT NULL,
        body            TEXT NOT NULL,
        edited_title    TEXT,
        edited_body     TEXT,
        provider        TEXT NOT NULL,
        target          TEXT NOT NULL,
        diff            TEXT NOT NULL,
        files           TEXT NOT NULL,
        head_sha        TEXT NOT NULL,
        base_ref        TEXT NOT NULL,
        added           INTEGER NOT NULL DEFAULT 0,
        removed         INTEGER NOT NULL DEFAULT 0,
        state           TEXT NOT NULL,
        verdict_reason  TEXT,
        publish_result  TEXT,
        claimed_ms      INTEGER,
        created_ms      INTEGER NOT NULL,
        created_mono_ms INTEGER NOT NULL,
        resolved_mono_ms INTEGER,
        updated_ms      INTEGER NOT NULL
    );
    CREATE INDEX review_open ON review(state, created_ms);
    CREATE INDEX review_session ON review(session_id);
    "#,
    // 3: the mesh. Peers share these tables; rows are scoped by node_id. The
    // one existing node row is this node.
    r#"
    ALTER TABLE node ADD COLUMN is_self      INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE node ADD COLUMN x25519_pub   TEXT;
    ALTER TABLE node ADD COLUMN last_seen_ms INTEGER;
    ALTER TABLE node ADD COLUMN reachable    INTEGER NOT NULL DEFAULT 1;
    UPDATE node SET is_self = 1;

    ALTER TABLE event ADD COLUMN node_id    TEXT;
    ALTER TABLE event ADD COLUMN origin_seq INTEGER;
    UPDATE event SET node_id = (SELECT node_id FROM session WHERE session.id = event.session_id);
    CREATE UNIQUE INDEX event_origin ON event(node_id, origin_seq) WHERE origin_seq IS NOT NULL;

    CREATE TABLE channel (
        name          TEXT PRIMARY KEY,
        keyring       BLOB NOT NULL,
        bindings_json TEXT NOT NULL DEFAULT '{}',
        created_ms    INTEGER NOT NULL,
        updated_ms    INTEGER NOT NULL
    );
    CREATE TABLE node_channel (
        node_id TEXT NOT NULL,
        channel TEXT NOT NULL,
        PRIMARY KEY (node_id, channel)
    );
    CREATE TABLE mesh_cursor (channel TEXT PRIMARY KEY, seq INTEGER NOT NULL);
    CREATE TABLE mesh_outbox (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        channel    TEXT NOT NULL,
        envelope   TEXT NOT NULL,
        created_ms INTEGER NOT NULL
    );
    CREATE TABLE mesh_seen (frame_id TEXT PRIMARY KEY, at_ms INTEGER NOT NULL);
    CREATE INDEX mesh_seen_at ON mesh_seen(at_ms);
    "#,
    // 4: what the model gateway counted, per request.
    r#"
    CREATE TABLE model_usage (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        channel       TEXT NOT NULL,
        node_id       TEXT NOT NULL,
        session_id    TEXT,
        provider      TEXT NOT NULL,
        model         TEXT,
        at_ms         INTEGER NOT NULL,
        input_tokens  INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        requests      INTEGER NOT NULL DEFAULT 1
    );
    CREATE INDEX model_usage_channel ON model_usage(channel, at_ms);
    "#,
    // 5: bank identity. The replicated corpus itself is installed by the
    // `sync` crate after these run (see `migrate`).
    r#"
    CREATE TABLE project (
        id         TEXT PRIMARY KEY,
        channel    TEXT NOT NULL,
        name       TEXT NOT NULL,
        remote_url TEXT,
        created_ms INTEGER NOT NULL
    );
    ALTER TABLE session ADD COLUMN project_id TEXT;
    "#,
];

/// The first N migrations, for tests that build a database as an older build
/// left it and then migrate it forward.
#[cfg(test)]
pub(crate) fn migrate_to(conn: &Connection, version: usize) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", true)?;
    for ddl in &MIGRATIONS[..version] {
        conn.execute_batch(ddl)?;
    }
    conn.pragma_update(None, "user_version", version as i64)?;
    Ok(())
}

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, ddl) in MIGRATIONS.iter().enumerate() {
        let target = i as i64 + 1;
        if version < target {
            conn.execute_batch(ddl)?;
            conn.pragma_update(None, "user_version", target)?;
        }
    }
    // The replicated tables are one schema shared with the hub's replica.
    tracon_sync::schema::install(conn)?;
    Ok(())
}
