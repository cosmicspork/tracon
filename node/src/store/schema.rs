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
    // 6: phases and provenance. A session is one phase of one item; the
    // policy version it ran under is what provenance answers with.
    r#"
    ALTER TABLE session ADD COLUMN phase TEXT NOT NULL DEFAULT 'execute';
    ALTER TABLE session ADD COLUMN policy_version INTEGER;
    "#,
    // 7: supervision and review sessions. A review records the checks that
    // passed at submit, the fresh session that read it, and that session's
    // verdict; a review session records which review it is for.
    r#"
    ALTER TABLE review ADD COLUMN checks_json TEXT;
    ALTER TABLE review ADD COLUMN review_session_id TEXT;
    ALTER TABLE review ADD COLUMN ai_verdict_json TEXT;
    ALTER TABLE session ADD COLUMN review_id TEXT;
    "#,
    // 8: operator authentication. `kv` holds the hash of the operator token
    // (never the token); `auth_session` holds one row per logged-in client,
    // keyed by the hash of its cookie, so rotating the token or logging out
    // invalidates immediately rather than waiting for a signature to expire.
    r#"
    CREATE TABLE kv (
        k          TEXT PRIMARY KEY,
        v          TEXT NOT NULL,
        updated_ms INTEGER NOT NULL
    );
    CREATE TABLE auth_session (
        token_hash   TEXT PRIMARY KEY,
        created_ms   INTEGER NOT NULL,
        last_seen_ms INTEGER NOT NULL,
        expires_ms   INTEGER NOT NULL,
        user_agent   TEXT
    );
    "#,
    // 9: an edited diff. When the operator edits rather than describes, the
    // patch rides along with the notes; the agent applies it and resubmits,
    // so the agent is still the only writer to the worktree.
    r#"
    ALTER TABLE review ADD COLUMN revision_patch TEXT;
    "#,
    // 10: the vector index, beside FTS5 rather than instead of it.
    //
    // This is node-local on purpose, and the omission from
    // `tracon_sync::TABLES` is the enforcement: a vector is not a safe form of
    // encrypted content, because embedding inversion recovers a good deal of
    // the source text from the vector alone. Replicating one would hand the
    // hub a readable index of a channel whose contents it cannot open. So each
    // node embeds its own replica, and `apply_changes` refuses the table by
    // name if anything ever tries to send one.
    //
    // `model` and `dim` are per row, not per database: without them a stale
    // vector is undetectable and changing the embedding model means throwing
    // the whole index away rather than migrating it. `text_hash` is what makes
    // an edit re-embed exactly the chunks that changed.
    r#"
    CREATE TABLE embedding (
        id           INTEGER PRIMARY KEY,
        source_table TEXT NOT NULL,
        source_id    TEXT NOT NULL,
        chunk_ix     INTEGER NOT NULL,
        channel      TEXT NOT NULL,
        model        TEXT NOT NULL,
        dim          INTEGER NOT NULL,
        text_hash    TEXT NOT NULL,
        offset       INTEGER NOT NULL,
        len          INTEGER NOT NULL,
        updated_ms   INTEGER NOT NULL,
        UNIQUE (source_table, source_id, chunk_ix)
    );
    CREATE INDEX embedding_source ON embedding(source_table, source_id);
    CREATE INDEX embedding_channel ON embedding(channel);
    "#,
    // 11: the phones this node pushes to. A subscription is the public half
    // of a device key pair plus the push service's URL for it; the node can
    // seal to it but never read a push back. Tied to the browser session that
    // registered it, so revoking a client also silences its devices; a NULL
    // session is a browser on this machine, which never logs in.
    r#"
    CREATE TABLE push_subscription (
        id           TEXT PRIMARY KEY,
        session_hash TEXT,
        endpoint     TEXT NOT NULL UNIQUE,
        p256dh       TEXT NOT NULL,
        auth         TEXT NOT NULL,
        user_agent   TEXT,
        created_ms   INTEGER NOT NULL,
        last_ok_ms   INTEGER,
        fail_count   INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX push_subscription_session ON push_subscription(session_hash);
    "#,
    // 12: each node's provider summary, carried in the hello like its models,
    // so a peer's interface can render and drive them (contract 3). NULL on
    // rows from older builds; sender-owned, mirrored as received.
    r#"
    ALTER TABLE node ADD COLUMN providers_json TEXT;
    "#,
    // 13: when a session was put away. Ended sessions accumulate forever and
    // the home is not their museum; archiving hides one from what landed
    // recently without touching its state, which metrics and the phase
    // machinery both key on. NULL is "still on the home".
    r#"
    ALTER TABLE session ADD COLUMN archived_ms INTEGER;
    CREATE INDEX session_archived ON session(archived_ms, created_ms);
    "#,
    // 14: operator interventions are durable but node-local. Questions are
    // not permissions, and issue drafts never leave this database until a
    // separate, atomic operator approval claims their fixed bytes.
    r#"
    CREATE TABLE operator_question (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        channel TEXT NOT NULL,
        node_id TEXT NOT NULL,
        prompt TEXT NOT NULL,
        choices_json TEXT NOT NULL,
        state TEXT NOT NULL,
        answer_json TEXT,
        created_ms INTEGER NOT NULL,
        answered_ms INTEGER
    );
    CREATE INDEX operator_question_open ON operator_question(state, created_ms);
    CREATE INDEX operator_question_session ON operator_question(session_id, state, created_ms);
    CREATE TABLE operator_issue (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        channel TEXT NOT NULL,
        title TEXT NOT NULL,
        body TEXT NOT NULL,
        attachments_json TEXT NOT NULL,
        state TEXT NOT NULL,
        published_url TEXT,
        publish_error TEXT,
        created_ms INTEGER NOT NULL,
        approved_ms INTEGER
    );
    CREATE INDEX operator_issue_state ON operator_issue(state, created_ms);
    CREATE TABLE operator_notification (
        dedup_key TEXT PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        expires_ms INTEGER NOT NULL
    );
    CREATE TABLE operator_notification_attempt (
        id TEXT PRIMARY KEY,
        notification_id TEXT NOT NULL,
        device_id TEXT NOT NULL,
        outcome TEXT NOT NULL,
        attempted_ms INTEGER NOT NULL
    );
    CREATE INDEX operator_notification_attempt_notification ON operator_notification_attempt(notification_id, attempted_ms);
    "#,
    // 15: unique operator notices are rate-limited independently of their
    // dedup key, per sending node and channel.
    r#"
    CREATE TABLE operator_notification_rate (
        channel TEXT NOT NULL,
        origin TEXT NOT NULL,
        window_started_ms INTEGER NOT NULL,
        count INTEGER NOT NULL,
        PRIMARY KEY(channel, origin)
    );
    "#,
    // 16: a harness retry carries a caller-stable key. One question survives
    // reconnect/restart; a different body may never reuse its answer.
    r#"
    ALTER TABLE operator_question ADD COLUMN request_key TEXT;
    CREATE UNIQUE INDEX operator_question_request_key ON operator_question(session_id, request_key)
        WHERE request_key IS NOT NULL;
    "#,
    // 17: narrowly scoped, local authority decisions. Policy itself remains a
    // separately signed bundle: grants cannot alter trust roots or rules.
    r#"
    CREATE TABLE authority_grant (
        id          TEXT PRIMARY KEY,
        action      TEXT NOT NULL,
        verdict     TEXT NOT NULL CHECK (verdict IN ('allow','ask','deny')),
        target      TEXT NOT NULL,
        channel     TEXT NOT NULL,
        session_id  TEXT,
        revision    TEXT,
        expires_ms  INTEGER,
        revoked_ms  INTEGER,
        reason      TEXT NOT NULL,
        created_ms  INTEGER NOT NULL
    );
    CREATE INDEX authority_grant_live ON authority_grant(action, target, channel, revoked_ms, expires_ms);

    CREATE TABLE authority_action (
        id          TEXT PRIMARY KEY,
        grant_id    TEXT,
        action      TEXT NOT NULL,
        target      TEXT NOT NULL,
        channel     TEXT NOT NULL,
        session_id  TEXT NOT NULL,
        revision    TEXT,
        evidence    TEXT NOT NULL,
        state       TEXT NOT NULL CHECK (state IN ('pending','succeeded','failed','uncertain')),
        outcome     TEXT,
        created_ms  INTEGER NOT NULL,
        updated_ms  INTEGER NOT NULL
    );
    CREATE INDEX authority_action_pending ON authority_action(state, created_ms);
    "#,
    // 15: a remote result can be lost after the provider performs the action.
    // Keep that scope blocked rather than guessing an idempotent retry.
    r#"
    CREATE UNIQUE INDEX authority_action_unresolved_scope
        ON authority_action(action, target, channel, session_id, COALESCE(revision, ''))
        WHERE state IN ('pending', 'uncertain');
    "#,
    // 16: distinguish two honest requests to the same target by their final
    // canonical arguments while still blocking replay of an uncertain one.
    r#"
    DROP INDEX authority_action_unresolved_scope;
    ALTER TABLE authority_action ADD COLUMN request_hash TEXT NOT NULL DEFAULT '';
    CREATE UNIQUE INDEX authority_action_unresolved_scope
        ON authority_action(action, target, channel, session_id, COALESCE(revision, ''), request_hash)
        WHERE state IN ('pending', 'uncertain');
    "#,
    // 17: an acknowledged success can lose its MCP response too. The caller's
    // operation id is part of request_hash, so a replay returns its record
    // rather than performing the remote side effect again.
    r#"
    DROP INDEX authority_action_unresolved_scope;
    CREATE UNIQUE INDEX authority_action_operation
        ON authority_action(action, target, channel, session_id, COALESCE(revision, ''), request_hash)
        WHERE state IN ('pending', 'uncertain', 'succeeded');
    "#,
    // 18: operation replay protection survives a session restart.
    r#"
    DROP INDEX authority_action_operation;
    CREATE UNIQUE INDEX authority_action_operation
        ON authority_action(action, target, channel, COALESCE(revision, ''), request_hash)
        WHERE state IN ('pending', 'uncertain', 'succeeded');
    "#,
    // 19: client operation ids are the stable replay key. Payload hashes stay
    // recorded so an id cannot be reused for a changed request.
    r#"
    ALTER TABLE authority_action ADD COLUMN operation_id TEXT NOT NULL DEFAULT '';
    DROP INDEX authority_action_operation;
    CREATE UNIQUE INDEX authority_action_operation
        ON authority_action(action, target, channel, COALESCE(revision, ''), operation_id)
        WHERE operation_id <> '' AND state IN ('pending', 'uncertain', 'succeeded');
    "#,
    // 23: immutable candidates and the evidence attached to them. Legacy
    // rows retain only facts the old schema recorded: in particular, no
    // migration invents an execution image or dependency input identity.
    r#"
    CREATE TABLE candidate (
        id               TEXT PRIMARY KEY,
        head_sha         TEXT NOT NULL,
        tree_sha         TEXT,
        channel          TEXT NOT NULL,
        owner_session_id TEXT NOT NULL,
        source_kind      TEXT NOT NULL,
        captured_ms      INTEGER NOT NULL,
        capture_json     TEXT NOT NULL
    );
    CREATE TABLE candidate_file (
        candidate_id TEXT NOT NULL REFERENCES candidate(id),
        path         TEXT NOT NULL,
        mode         INTEGER NOT NULL,
        content      BLOB NOT NULL,
        size_bytes   INTEGER NOT NULL,
        PRIMARY KEY(candidate_id, path)
    );
    CREATE INDEX candidate_file_candidate ON candidate_file(candidate_id, path);
    CREATE UNIQUE INDEX candidate_commit_channel ON candidate(head_sha, channel);

    CREATE TABLE check_run (
        id               TEXT PRIMARY KEY,
        candidate_id     TEXT REFERENCES candidate(id),
        session_id       TEXT NOT NULL,
        definition_json  TEXT NOT NULL,
        definition_hash  TEXT,
        execution_image  TEXT,
        inputs_json      TEXT,
        reuse_key        TEXT,
        outcome          TEXT NOT NULL,
        source_outcome   TEXT,
        exit_code        INTEGER,
        log              TEXT NOT NULL,
        duration_ms      INTEGER,
        started_ms       INTEGER NOT NULL,
        finished_ms      INTEGER,
        rerun_of         TEXT REFERENCES check_run(id),
        reused_from_id   TEXT REFERENCES check_run(id),
        metadata_json    TEXT NOT NULL
    );
    CREATE INDEX check_run_candidate_started ON check_run(candidate_id, started_ms);
    CREATE INDEX check_run_reuse ON check_run(candidate_id, reuse_key, finished_ms);

    CREATE TABLE review_revision (
        id                        TEXT PRIMARY KEY,
        review_id                 TEXT NOT NULL REFERENCES review(id),
        candidate_id              TEXT NOT NULL REFERENCES candidate(id),
        title                     TEXT NOT NULL,
        body                      TEXT NOT NULL,
        diff                      TEXT NOT NULL,
        files                     TEXT NOT NULL,
        head_sha                  TEXT NOT NULL,
        context_json              TEXT NOT NULL,
        -- What the review screen must show as "Requirements": the linked work
        -- item's title/body as they stood at submit time, plus a hash of them.
        -- NULL when no work item was linked, or for a legacy backfilled row.
        -- Never re-read from the (mutable) work item at display time.
        requirements_work_item_id TEXT,
        requirements_title        TEXT,
        requirements_body         TEXT,
        requirements_hash         TEXT,
        created_ms                INTEGER NOT NULL
    );
    CREATE INDEX review_revision_review ON review_revision(review_id, created_ms);
    CREATE INDEX review_revision_candidate ON review_revision(candidate_id, created_ms);

    CREATE TABLE review_decision (
        id          TEXT PRIMARY KEY,
        review_id   TEXT NOT NULL REFERENCES review(id),
        revision_id TEXT NOT NULL REFERENCES review_revision(id),
        source      TEXT NOT NULL,
        decision    TEXT NOT NULL,
        reason      TEXT,
        title       TEXT,
        body        TEXT,
        patch       TEXT,
        decided_ms  INTEGER NOT NULL
    );
    CREATE INDEX review_decision_review ON review_decision(review_id, decided_ms);

    CREATE TABLE demonstration (
        id            TEXT PRIMARY KEY,
        candidate_id  TEXT NOT NULL REFERENCES candidate(id),
        channel       TEXT NOT NULL,
        document_id   TEXT NOT NULL,
        document_slug TEXT NOT NULL,
        document_hash TEXT NOT NULL,
        label         TEXT NOT NULL,
        created_ms    INTEGER NOT NULL
    );
    CREATE INDEX demonstration_candidate ON demonstration(candidate_id, created_ms);

    INSERT OR IGNORE INTO candidate
        (id, head_sha, tree_sha, channel, owner_session_id, source_kind, captured_ms, capture_json)
    SELECT head_sha || ':' || channel, head_sha, NULL, channel, session_id, 'legacy',
           created_ms, json_object('legacy_review_id', id, 'provenance', 'not_recorded')
    FROM review WHERE head_sha <> '';

    INSERT OR IGNORE INTO review_revision
        (id, review_id, candidate_id, title, body, diff, files, head_sha, context_json, created_ms)
    SELECT 'legacy:' || id, id, head_sha || ':' || channel, title, body, diff, files, head_sha,
           '[]', created_ms
    FROM review WHERE head_sha <> '';

    INSERT OR IGNORE INTO review_decision
        (id, review_id, revision_id, decision, source, reason, title, body, patch, decided_ms)
    SELECT 'legacy:' || id, id, 'legacy:' || id,
           CASE WHEN state = 'revising' THEN 'revise' ELSE state END,
           'legacy_unknown', verdict_reason,
           edited_title, edited_body, revision_patch, updated_ms
    FROM review WHERE state IN ('approved', 'rejected', 'revising') AND head_sha <> '';

    INSERT OR IGNORE INTO check_run
        (id, candidate_id, session_id, definition_json, definition_hash, execution_image, inputs_json,
         reuse_key, outcome, source_outcome, exit_code, log, duration_ms, started_ms, finished_ms,
         rerun_of, reused_from_id, metadata_json)
    SELECT 'legacy:review:' || r.id || ':' || j.key, r.head_sha || ':' || r.channel, r.session_id,
           json_object('command', json_extract(j.value, '$.command'), 'legacy', 1),
           NULL, NULL, NULL, NULL,
           CASE WHEN json_extract(j.value, '$.ok') THEN 'passed' ELSE 'failed' END,
           NULL, json_extract(j.value, '$.exit'), COALESCE(json_extract(j.value, '$.tail'), ''),
           json_extract(j.value, '$.ms'), r.created_ms, r.created_ms, NULL, NULL,
           json_object('legacy_source', 'review.checks_json', 'provenance', 'not_recorded')
    FROM review r, json_each(r.checks_json) j
    WHERE r.head_sha <> '' AND json_valid(r.checks_json) AND json_type(r.checks_json) = 'array';

    INSERT OR IGNORE INTO check_run
        (id, candidate_id, session_id, definition_json, definition_hash, execution_image, inputs_json,
         reuse_key, outcome, source_outcome, exit_code, log, duration_ms, started_ms, finished_ms,
         rerun_of, reused_from_id, metadata_json)
    SELECT 'legacy:event:' || seq, NULL, session_id,
           json_object('command', json_extract(payload, '$.command'), 'legacy', 1),
           NULL, NULL, NULL, NULL,
           CASE WHEN json_extract(payload, '$.ok') THEN 'passed' ELSE 'failed' END,
           NULL, json_extract(payload, '$.exit'), COALESCE(json_extract(payload, '$.tail'), ''),
           json_extract(payload, '$.ms'), at_ms, at_ms, NULL, NULL,
           json_object('legacy_source', 'event.check_result', 'candidate_provenance', 'unknown')
    FROM event
    WHERE kind = 'check_result' AND json_valid(payload);
    // 24: append-only QA observations and repository-derived prototypes. These
    // are node-owned execution evidence, not replicated demonstrations: an
    // offline peer cannot truthfully inherit a runtime observation.
    r#"
    CREATE TABLE qa_deployment (
        id                   TEXT PRIMARY KEY,
        candidate_id         TEXT NOT NULL,
        channel              TEXT NOT NULL,
        target_id            TEXT NOT NULL,
        build_id             TEXT NOT NULL,
        execution_image      TEXT NOT NULL,
        origin               TEXT NOT NULL,
        environment_identity TEXT,
        identity_state       TEXT NOT NULL CHECK(identity_state IN ('fresh','unknown','failed')),
        observed_ms          INTEGER NOT NULL,
        started_ms           INTEGER NOT NULL,
        finished_ms          INTEGER NOT NULL,
        outcome              TEXT NOT NULL CHECK(outcome IN ('succeeded','failed','unknown')),
        detail_json          TEXT NOT NULL
    );
    CREATE INDEX qa_deployment_candidate ON qa_deployment(candidate_id, observed_ms DESC);
    CREATE INDEX qa_deployment_target ON qa_deployment(channel, target_id, observed_ms DESC);
    CREATE INDEX qa_deployment_target_global ON qa_deployment(target_id, observed_ms DESC);

    CREATE TABLE qa_browser_run (
        id                       TEXT PRIMARY KEY,
        deployment_id            TEXT NOT NULL REFERENCES qa_deployment(id),
        candidate_id             TEXT NOT NULL,
        channel                  TEXT NOT NULL,
        target_id                TEXT NOT NULL,
        authorized_origins_json  TEXT NOT NULL,
        test_credential          TEXT,
        assertions_json          TEXT NOT NULL,
        outcome                  TEXT NOT NULL CHECK(outcome IN ('passed','failed','unknown')),
        environment_before       TEXT,
        environment_after        TEXT,
        evidence_state           TEXT NOT NULL CHECK(evidence_state IN ('fresh','stale','unknown')),
        log_tail                 TEXT NOT NULL,
        started_ms               INTEGER NOT NULL,
        finished_ms               INTEGER NOT NULL
    );
    CREATE INDEX qa_browser_run_candidate ON qa_browser_run(candidate_id, started_ms DESC);
    CREATE INDEX qa_browser_run_deployment ON qa_browser_run(deployment_id);

    CREATE TABLE qa_asset (
        id            TEXT PRIMARY KEY,
        browser_run_id TEXT NOT NULL REFERENCES qa_browser_run(id),
        candidate_id  TEXT NOT NULL,
        channel       TEXT NOT NULL,
        kind          TEXT NOT NULL CHECK(kind IN ('screenshots','browser-log','demonstration')),
        document_id   TEXT NOT NULL,
        document_hash TEXT NOT NULL,
        slug          TEXT NOT NULL,
        created_ms    INTEGER NOT NULL
    );
    CREATE INDEX qa_asset_run ON qa_asset(browser_run_id, created_ms);

    CREATE TABLE prototype (
        id                   TEXT PRIMARY KEY,
        candidate_id         TEXT NOT NULL,
        channel              TEXT NOT NULL,
        source_revision      TEXT NOT NULL,
        source_identity_json TEXT NOT NULL,
        build_image          TEXT NOT NULL,
        build_inputs_json    TEXT NOT NULL,
        document_id          TEXT,
        document_hash        TEXT,
        slug                 TEXT NOT NULL,
        entry_path           TEXT NOT NULL,
        outcome              TEXT NOT NULL CHECK(outcome IN ('succeeded','failed','unknown')),
        detail               TEXT NOT NULL,
        created_ms           INTEGER NOT NULL,
        finished_ms          INTEGER NOT NULL
    );
    CREATE INDEX prototype_candidate ON prototype(candidate_id, created_ms DESC);
    "#,
    // 24: the monotonic sequence for bounded channel rollups sent only to a
    // hub replica that has explicitly been handed that channel's key.
    r#"
    CREATE TABLE mesh_rollup_seq (
        channel TEXT PRIMARY KEY,
        seq     INTEGER NOT NULL
    );
    "#,
    // 25: signed candidate/context transfer packages and an append-only
    // receiver-side outcome history. Package bytes never mutate in place.
    r#"
    CREATE TABLE transfer (
        id             TEXT PRIMARY KEY,
        candidate_id   TEXT NOT NULL,
        channel        TEXT NOT NULL,
        origin_node    TEXT NOT NULL,
        target_node    TEXT,
        payload_sha256 TEXT NOT NULL,
        package_json   TEXT NOT NULL,
        created_ms     INTEGER NOT NULL
    );
    CREATE INDEX transfer_channel ON transfer(channel, created_ms DESC);
    CREATE TABLE transfer_event (
        seq         INTEGER PRIMARY KEY AUTOINCREMENT,
        transfer_id TEXT NOT NULL REFERENCES transfer(id),
        kind        TEXT NOT NULL,
        detail      TEXT,
        session_id  TEXT,
        at_ms       INTEGER NOT NULL
    );
    CREATE INDEX transfer_event_transfer ON transfer_event(transfer_id, seq);
    "#,
    // 26: only one operator-confirmed session may emerge from a package.
    // `preparing` survives a crash as an explicit uncertain outcome rather
    // than silently materializing a second workspace on retry.
    r#"
    CREATE TABLE transfer_import (
        transfer_id  TEXT PRIMARY KEY REFERENCES transfer(id),
        state        TEXT NOT NULL CHECK(state IN ('preparing', 'imported', 'failed')),
        workspace_id TEXT,
        session_id   TEXT,
        detail       TEXT,
        updated_ms   INTEGER NOT NULL
    );
    "#,
    // 27: the inbox can render a package's operator-visible manifest without
    // loading its (potentially 64 MiB) immutable JSON body.
    r#"
    ALTER TABLE transfer ADD COLUMN file_count INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE transfer ADD COLUMN document_count INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE transfer ADD COLUMN memory_count INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE transfer ADD COLUMN handoff_note TEXT NOT NULL DEFAULT '';
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

/// A process can die after recording a run but before its runner returns.
/// Leave the evidence honest on the next open rather than making a stale
/// `running` row look like a pass or a cancellable live process.
///
/// Deliberately not part of `migrate`: every `Store::open` runs migrations
/// (CLI subcommands included), and a CLI opening the same database while the
/// serving node has a check genuinely running must not interrupt it. Call
/// this once, explicitly, from the serving node's own startup path.
pub fn reconcile_interrupted_runs(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE check_run
         SET outcome='interrupted', finished_ms=CAST(strftime('%s','now') AS INTEGER) * 1000,
             metadata_json=json_set(metadata_json, '$.interrupted_by_restart', true)
         WHERE outcome='running'",
        [],
    )?;
    Ok(())
}
