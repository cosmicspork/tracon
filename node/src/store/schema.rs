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
];

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
    Ok(())
}
