//! The data contract, version 1: everything a node holds, as one versioned
//! JSONL file that reads with nothing running.
//!
//! An export is a header line, one line per record, and a signed trailer.
//! Every record is a row of one of the node's own tables, tagged with the
//! contract kind it belongs to, so a column added to a table appears in the
//! next export without a change here; a table added to the schema must be
//! named below, under a kind or as omitted, or the completeness test fails.
//! Credentials and private keys are never written: the tables and columns
//! that hold them are listed in `OMITTED_TABLES` and `OMITTED_COLUMNS`, and
//! the header repeats both lists so an export says what it leaves out.
//!
//! Export only. Nothing here reads an export back into a store, and neither
//! verifying an export's signature nor reading one grants anything: a
//! verified signature says the bytes are whole and which node wrote them,
//! and that is all it says. The written spec is
//! `docs/reference/data-contract.md`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::store::Store;

/// The contract's name, as the header carries it.
pub const CONTRACT: &str = "tracon/data/v1";
/// Moves only on a change a version 1 reader would misread: a new encoding,
/// a renamed kind, a changed line shape. A new column or table is additive.
pub const CONTRACT_VERSION: u32 = 1;
/// What the trailer's signature is over, ahead of the digest.
const SIGNING_DOMAIN: &[u8] = b"tracon/data-export/v1\0";

/// Said in the header, by the CLI and by the interface, every time.
pub const WARNING: &str = "Credentials and private keys are left out, but transcripts, \
    prompts, tool output, documents and memories are exported as they were written, and \
    can still carry anything that passed through a session: keys pasted into a prompt, \
    tokens printed by a command, private data read from a repository. Keep the file as \
    you would keep the node's own state.";

/// The contract's record kinds, in export order, and the tables whose rows
/// each one is. A table may belong to one kind only.
pub const KINDS: &[(&str, &[&str])] = &[
    (
        "session",
        &[
            "session",
            "turn_usage",
            "model_usage",
            "harness_request_usage",
            "sandbox_usage",
            "provider_limit_snapshot",
            "exhaustion",
            "operator_notification",
            "launch_manifest",
            "manifest_item",
            "channel",
            "node",
            "node_channel",
            "entity_name",
        ],
    ),
    ("event", &["event", "external_event"]),
    ("work_item", &["work_item", "project", "pipeline_follow"]),
    (
        "decision",
        &[
            "approval",
            "permission_request",
            "operator_question",
            "review_decision",
            "authority_grant",
            "authority_action",
            "repo_recipe_offer",
        ],
    ),
    (
        "draft",
        &["review_draft", "queued_prompt", "operator_issue"],
    ),
    (
        "document",
        &["document", "document_bundle_file", "document_bundle_chunk"],
    ),
    ("memory", &["memory", "promotion"]),
    (
        "workspace",
        &[
            "workspace_origin",
            "opencode_session",
            "opencode_object",
            "opencode_intent",
            "opencode_state",
            "opencode_part_claim",
            "repo_image",
        ],
    ),
    (
        "evidence",
        &[
            "candidate",
            "candidate_file",
            "check_run",
            "demonstration",
            "review",
            "review_revision",
            "review_revision_trigger",
            "review_session_revision",
            "publication",
            "shown_work",
            "qa_deployment",
            "qa_browser_run",
            "qa_asset",
            "prototype",
        ],
    ),
    (
        "provenance",
        &[
            "transfer",
            "transfer_event",
            "transfer_import",
            "config_audit",
            "policy_installation",
            "policy_rollout",
            "policy_rollout_target",
            "change_log",
        ],
    ),
];

/// The state directories whose entries are workspace records: named and
/// weighed, never read.
const WORKSPACE_DIRS: &[&str] = &["workspaces", "workspace-staging"];

/// Tables never exported, and why. Each is a credential, a key, a secret's
/// hash, transport bookkeeping, or derived data rebuilt from what is
/// exported.
pub const OMITTED_TABLES: &[(&str, &str)] = &[
    (
        "kv",
        "node settings, among them the hash of the operator token",
    ),
    (
        "auth_session",
        "signed-in clients, keyed by a credential's hash",
    ),
    ("ui_grant", "credentials minted for the interface's origin"),
    ("webauthn_credential", "passkeys: sign-in credentials"),
    ("webauthn_challenge", "sign-in ceremonies in flight"),
    (
        "device_approval",
        "device sign-in requests, keyed by a secret's hash",
    ),
    ("push_subscription", "push endpoints and their keys"),
    ("push_announced", "push delivery bookkeeping"),
    (
        "credential_revoke",
        "credential revocations, naming the credentials",
    ),
    (
        "credential_revoke_target",
        "credential revocations, by peer",
    ),
    (
        "repo_credential_hold",
        "which repositories lend credentials",
    ),
    (
        "mesh_admin_statement",
        "spent admin statements: replay protection",
    ),
    (
        "mesh_outbox",
        "mesh frames waiting to be sent: sealed transport",
    ),
    ("mesh_held", "mesh frames held for a peer: sealed transport"),
    ("mesh_cursor", "mesh delivery cursors"),
    ("mesh_seen", "mesh replay protection"),
    ("mesh_rollup_seq", "mesh delivery sequence numbers"),
    (
        "operator_notification_attempt",
        "notification delivery bookkeeping",
    ),
    ("operator_notification_rate", "notification rate limiting"),
    ("hlc", "the replication clock"),
    (
        "sync_schema",
        "the replicated schema's version, in the header instead",
    ),
    (
        "embedding",
        "the search index: derived from documents and memories",
    ),
    (
        "document_fts",
        "the full-text index: derived from documents",
    ),
    ("memory_fts", "the full-text index: derived from memories"),
    ("plugin", "installed plugin builds: programs, not records"),
];

/// Columns left out of tables that are otherwise exported.
pub const OMITTED_COLUMNS: &[(&str, &str, &str)] = &[
    (
        "channel",
        "keyring",
        "the channel's keys, wrapped to this node's key",
    ),
    (
        "qa_deployment",
        "test_credential",
        "a credential for the deployment under test",
    ),
];

#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not a tracon data export: {0}")]
    NotAnExport(String),
    #[error("this export is {0}; this reader knows {CONTRACT}, version {CONTRACT_VERSION}")]
    UnknownContract(String),
}

/// What an export wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Written {
    pub records: u64,
    pub sha256: String,
}

/// The kind a table's rows are exported as, if they are.
pub fn kind_of(table: &str) -> Option<&'static str> {
    KINDS
        .iter()
        .find(|(_, tables)| tables.contains(&table))
        .map(|(kind, _)| *kind)
}

/// Whether a table is accounted for: exported under a kind, omitted with a
/// reason, or internal to SQLite or to a virtual table.
fn accounted(table: &str) -> bool {
    kind_of(table).is_some() || OMITTED_TABLES.iter().any(|(t, _)| *t == table) || internal(table)
}

/// SQLite's own tables, and the shadow tables the search indexes (virtual
/// tables) keep, which are part of the index they shadow.
fn internal(table: &str) -> bool {
    table.starts_with("sqlite_")
        || ["embedding_", "document_fts_", "memory_fts_"]
            .iter()
            .any(|prefix| table.starts_with(prefix))
}

/// Every table of the node's schema no kind or omission names. Empty, or
/// the contract has fallen behind the schema.
pub fn unaccounted(store: &Store) -> Result<Vec<String>, ContractError> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("snapshot.db");
    store.snapshot_into(&path)?;
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(tables(&conn)?
        .into_iter()
        .filter(|t| !accounted(t))
        .collect())
}

fn tables(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect()
}

fn columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?
        .query_map([table], |r| r.get(0))?
        .collect()
}

/// A temporary directory for the snapshot, readable by the node's user
/// only, removed however the export ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(state_dir: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join(format!(".data-export-{}", uuid::Uuid::now_v7()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Everything written passes through the digest the trailer signs.
struct Hashing<'a> {
    out: &'a mut dyn Write,
    hasher: Sha256,
}

impl Hashing<'_> {
    fn line(&mut self, value: &Value) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.hasher.update(&bytes);
        self.out.write_all(&bytes)
    }
}

/// One SQLite value as the contract encodes it: text as a string, integers
/// and reals as numbers (a non-finite real as null), null as null, and a
/// blob as `{"base64": "..."}`. Text holding JSON stays text.
fn encode(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => json!(n),
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => {
            json!({ "base64": base64::engine::general_purpose::STANDARD.encode(b) })
        }
    }
}

/// Write the whole node to `out` as contract version 1.
///
/// The database is copied first (one statement under the store's lock), and
/// the copy, kept in a private directory under `state_dir` and removed
/// after, is what is read: the node keeps running meanwhile, and the export
/// is one point in time.
pub fn export(
    store: &Store,
    identity: &proto::keys::Identity,
    state_dir: &Path,
    out: &mut dyn Write,
) -> Result<Written, ContractError> {
    let scratch = Scratch::new(state_dir)?;
    let path = scratch.0.join("snapshot.db");
    store.snapshot_into(&path)?;
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let present = tables(&conn)?;
    let store_schema: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let sync_schema: Option<i64> = conn
        .query_row("SELECT version FROM sync_schema WHERE id = 1", [], |r| {
            r.get(0)
        })
        .ok();
    let node_id = identity.node_id();
    let mut out = Hashing {
        out,
        hasher: Sha256::new(),
    };
    out.line(&json!({
        "kind": "export",
        "contract": CONTRACT,
        "contract_version": CONTRACT_VERSION,
        "node": node_id,
        "tracon_version": env!("CARGO_PKG_VERSION"),
        "created_ms": crate::store::now_ms(),
        "store_schema": store_schema,
        "sync_schema": sync_schema,
        "warning": WARNING,
        "kinds": KINDS.iter().map(|(kind, tables)| json!({ "kind": kind, "tables": tables })).collect::<Vec<_>>(),
        "omitted_tables": OMITTED_TABLES.iter().map(|(table, reason)| json!({ "table": table, "reason": reason })).collect::<Vec<_>>(),
        "omitted_columns": OMITTED_COLUMNS.iter().map(|(table, column, reason)| json!({ "table": table, "column": column, "reason": reason })).collect::<Vec<_>>(),
    }))?;
    let mut records = 0u64;
    for (kind, kind_tables) in KINDS {
        for table in *kind_tables {
            if !present.iter().any(|t| t == table) {
                continue;
            }
            records += export_table(&conn, kind, table, &mut out)?;
        }
        if *kind == "workspace" {
            for dir in WORKSPACE_DIRS {
                records += export_dir(&state_dir.join(dir), dir, &mut out)?;
            }
        }
    }
    let digest: [u8; 32] = out.hasher.clone().finalize().into();
    let mut message = Vec::with_capacity(SIGNING_DOMAIN.len() + digest.len());
    message.extend_from_slice(SIGNING_DOMAIN);
    message.extend_from_slice(&digest);
    let sha256 = hex::encode(digest);
    out.line(&json!({
        "kind": "end",
        "records": records,
        "sha256": sha256,
        "node": node_id,
        "signature": hex::encode(identity.sign(&message).to_bytes()),
    }))?;
    out.out.flush()?;
    Ok(Written { records, sha256 })
}

fn export_table(
    conn: &Connection,
    kind: &str,
    table: &str,
    out: &mut Hashing<'_>,
) -> Result<u64, ContractError> {
    let all = columns(conn, table)?;
    let kept: Vec<&String> = all
        .iter()
        .filter(|c| {
            !OMITTED_COLUMNS
                .iter()
                .any(|(t, column, _)| *t == table && column == c)
        })
        .collect();
    if kept.is_empty() {
        return Ok(0);
    }
    // A tombstone is a delete the node remembers, not something it holds.
    let live = if all.iter().any(|c| c == "deleted") {
        " WHERE deleted = 0"
    } else {
        ""
    };
    let rowid: bool = conn
        .query_row(
            "SELECT wr FROM pragma_table_list WHERE name = ?1 AND schema = 'main'",
            [table],
            |r| r.get::<_, i64>(0),
        )
        .map(|wr| wr == 0)
        .unwrap_or(false);
    let order = if rowid { " ORDER BY rowid" } else { "" };
    let list = kept
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut statement = conn.prepare(&format!("SELECT {list} FROM \"{table}\"{live}{order}"))?;
    let mut rows = statement.query([])?;
    let mut n = 0;
    while let Some(row) = rows.next()? {
        let mut fields = Map::with_capacity(kept.len());
        for (i, column) in kept.iter().enumerate() {
            fields.insert((*column).clone(), encode(row.get_ref(i)?));
        }
        out.line(&json!({ "kind": kind, "table": table, "row": fields }))?;
        n += 1;
    }
    Ok(n)
}

/// One record per entry of a workspace directory: its name, how many files
/// it holds and what they weigh. The files themselves are a workspace's
/// working tree, which a session package carries when it is wanted.
fn export_dir(path: &Path, dir: &str, out: &mut Hashing<'_>) -> Result<u64, ContractError> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(0);
    };
    let mut names: Vec<_> = entries.flatten().collect();
    names.sort_by_key(|e| e.file_name());
    let mut n = 0;
    for entry in names {
        let (files, bytes) = weigh(&entry.path());
        out.line(&json!({
            "kind": "workspace",
            "directory": dir,
            "row": {
                "name": entry.file_name().to_string_lossy(),
                "files": files,
                "bytes": bytes,
            },
        }))?;
        n += 1;
    }
    Ok(n)
}

fn weigh(path: &Path) -> (u64, u64) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if meta.is_dir() {
        std::fs::read_dir(path)
            .map(|entries| {
                entries.flatten().fold((0, 0), |(f, b), e| {
                    let (ef, eb) = weigh(&e.path());
                    (f + ef, b + eb)
                })
            })
            .unwrap_or((0, 0))
    } else if meta.is_file() {
        (1, meta.len())
    } else {
        (0, 0)
    }
}

/// What reading an export found.
#[derive(Debug, Clone)]
pub struct Reading {
    /// The header line.
    pub header: Value,
    /// Records per kind, in the order the kinds first appeared.
    pub counts: Vec<(String, u64)>,
    pub records: u64,
    /// Session packages (the `package_json` of `transfer` rows): how many,
    /// and how many verify as the offline package reader verifies them.
    pub packages: u64,
    pub packages_verified: u64,
    /// Whether the trailer's digest and signature hold, and if not why.
    /// Holding says the file is whole and which node wrote it; it grants
    /// nothing.
    pub verified: Result<(), String>,
}

/// Read an export without a node, a store or a harness, handing each record
/// to `each` as it is read, and check the trailer.
///
/// Reading is all this does: nothing is imported, trusted or acted on,
/// whether or not the signature verifies. A record of a kind this reader
/// does not know is counted and handed on, as a later version 1 export may
/// carry one; an export of another contract version is refused.
pub fn read(input: impl BufRead, mut each: impl FnMut(&Value)) -> Result<Reading, ContractError> {
    let mut reading = Reading {
        header: Value::Null,
        counts: Vec::new(),
        records: 0,
        packages: 0,
        packages_verified: 0,
        verified: Err("the export has no trailer: it was cut short".into()),
    };
    let mut hasher = Sha256::new();
    let mut lines = input.split(b'\n');
    let Some(first) = lines.next() else {
        return Err(ContractError::NotAnExport("the file is empty".into()));
    };
    let first = first?;
    let header: Value = serde_json::from_slice(&first)
        .map_err(|e| ContractError::NotAnExport(format!("the first line: {e}")))?;
    if header["kind"] != "export" {
        return Err(ContractError::NotAnExport(
            "the first line is not an export header".into(),
        ));
    }
    let contract = header["contract"].as_str().unwrap_or_default();
    if contract != CONTRACT || header["contract_version"] != json!(CONTRACT_VERSION) {
        return Err(ContractError::UnknownContract(format!(
            "{contract}, version {}",
            header["contract_version"]
        )));
    }
    hasher.update(&first);
    hasher.update(b"\n");
    each(&header);
    reading.header = header;
    for line in lines {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let record: Value = serde_json::from_slice(&line)?;
        let kind = record["kind"].as_str().unwrap_or_default().to_string();
        if kind == "end" {
            reading.verified = check_trailer(&record, &reading, hasher.clone().finalize().into());
            each(&record);
            // Nothing after the trailer is covered by it.
            break;
        }
        hasher.update(&line);
        hasher.update(b"\n");
        if record["table"] == "transfer" {
            reading.packages += 1;
            if record["row"]["package_json"]
                .as_str()
                .and_then(|p| serde_json::from_str::<crate::transfers::SignedTransfer>(p).ok())
                .is_some_and(|t| t.verify().is_ok())
            {
                reading.packages_verified += 1;
            }
        }
        match reading.counts.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, n)) => *n += 1,
            None => reading.counts.push((kind, 1)),
        }
        reading.records += 1;
        each(&record);
    }
    Ok(reading)
}

fn check_trailer(trailer: &Value, reading: &Reading, digest: [u8; 32]) -> Result<(), String> {
    if trailer["records"].as_u64() != Some(reading.records) {
        return Err(format!(
            "the trailer counts {} records and the file holds {}",
            trailer["records"], reading.records
        ));
    }
    if trailer["sha256"].as_str() != Some(hex::encode(digest).as_str()) {
        return Err("the digest does not match: the file was changed".into());
    }
    let node = trailer["node"].as_str().unwrap_or_default();
    if reading.header["node"].as_str() != Some(node) {
        return Err("the trailer names another node than the header".into());
    }
    let key = proto::keys::key32(node)
        .and_then(|key| VerifyingKey::from_bytes(&key).ok())
        .ok_or("the node id is not a key")?;
    let signature: [u8; 64] = trailer["signature"]
        .as_str()
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or("the signature is malformed")?;
    let mut message = Vec::with_capacity(SIGNING_DOMAIN.len() + digest.len());
    message.extend_from_slice(SIGNING_DOMAIN);
    message.extend_from_slice(&digest);
    if !proto::keys::verify(&key, &message, &Signature::from_bytes(&signature)) {
        return Err("the signature does not verify".into());
    }
    Ok(())
}

/// The export's documents written out as document export writes them:
/// `<dir>/<channel>/<slug>.md`, archived ones under `archive/`. Markdown
/// only, as document export does; returns how many were written and how
/// many skipped (HTML, or a channel or slug that is not a safe file name).
pub fn write_documents(dir: &Path, documents: &[Value]) -> std::io::Result<(usize, usize)> {
    use std::collections::BTreeMap;
    let mut by_channel: BTreeMap<String, Vec<crate::corpus::export::ExportDoc>> = BTreeMap::new();
    let mut skipped = 0;
    for record in documents {
        let row = &record["row"];
        let (Some(channel), Some(slug), Some(body)) = (
            row["channel"].as_str(),
            row["slug"].as_str(),
            row["body"].as_str(),
        ) else {
            skipped += 1;
            continue;
        };
        let safe_channel = !channel.is_empty()
            && channel.len() <= 64
            && channel
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && channel != ARCHIVE;
        if row["format"].as_str().unwrap_or("markdown") == "html"
            || !safe_channel
            || !crate::mcp::docs::valid_slug(slug)
        {
            skipped += 1;
            continue;
        }
        by_channel
            .entry(channel.to_string())
            .or_default()
            .push(crate::corpus::export::ExportDoc {
                slug: slug.to_string(),
                body: body.to_string(),
                archived: row["archived"].as_i64().unwrap_or(0) != 0,
            });
    }
    let mut written = 0;
    for (channel, docs) in by_channel {
        let report = crate::corpus::export::sync_dir(&dir.join(&channel), &docs, &[])?;
        written += report.written + report.unchanged;
    }
    Ok((written, skipped))
}

const ARCHIVE: &str = "archive";

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> proto::keys::Identity {
        proto::keys::Identity::from_seed(&[7u8; 32])
    }

    #[test]
    fn every_table_is_exported_under_a_kind_or_omitted_with_a_reason() {
        let store = Store::open_in_memory().unwrap();
        let missing = unaccounted(&store).unwrap();
        assert!(
            missing.is_empty(),
            "name these tables in contract::KINDS or OMITTED_TABLES: {missing:?}"
        );
        // And nothing is named twice, or both exported and omitted.
        let mut seen = std::collections::HashSet::new();
        for (_, tables) in KINDS {
            for table in *tables {
                assert!(seen.insert(*table), "{table} is in two kinds");
                assert!(!OMITTED_TABLES.iter().any(|(t, _)| t == table), "{table}");
            }
        }
        for (table, column, _) in OMITTED_COLUMNS {
            assert!(kind_of(table).is_some(), "{table}.{column}");
        }
        let kinds: Vec<&str> = KINDS.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            [
                "session",
                "event",
                "work_item",
                "decision",
                "draft",
                "document",
                "memory",
                "workspace",
                "evidence",
                "provenance"
            ]
        );
    }

    #[test]
    fn every_named_table_is_one_the_schema_has() {
        let store = Store::open_in_memory().unwrap();
        let missing: Vec<&str> = KINDS
            .iter()
            .flat_map(|(_, tables)| tables.iter().copied())
            .chain(OMITTED_TABLES.iter().map(|(t, _)| *t))
            .filter(|t| !store.has_table(t))
            .collect();
        assert!(missing.is_empty(), "not in the schema: {missing:?}");
    }

    fn seeded() -> (Store, tempfile::TempDir) {
        let store = Store::open_in_memory().unwrap();
        store
            .kv_put("operator_token_sha256", "SECRET-HASH")
            .unwrap();
        {
            let conn = store.conn();
            conn.execute(
                "INSERT INTO channel (name, keyring, created_ms, updated_ms) \
                 VALUES ('personal', X'DEADBEEF', 1, 1)",
                [],
            )
            .unwrap();
            for (id, slug, body, deleted, archived) in [
                ("d1", "note-one", "# One\n", 0, 0),
                ("d2", "note-two", "# Two\n", 0, 1),
                ("d3", "note-gone", "GONE-BODY", 1, 0),
                ("d4", "../escape", "nope", 0, 0),
            ] {
                conn.execute(
                    "INSERT INTO document (id, channel, slug, body, site, site_seq, hlc_ms, \
                     deleted, archived, created_ms, updated_ms) \
                     VALUES (?1, 'personal', ?2, ?3, 's', 1, 1, ?4, ?5, 1, 1)",
                    rusqlite::params![id, slug, body, deleted, archived],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO memory (id, channel, kind, body, confidence, site, site_seq, \
                 hlc_ms, created_ms, updated_ms) \
                 VALUES ('m1', 'personal', 'fact', 'the build uses bun', 0.5, 's', 1, 1, 1, 1)",
                [],
            )
            .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("workspaces/w1/src")).unwrap();
        std::fs::write(dir.path().join("workspaces/w1/src/a"), [0u8; 100]).unwrap();
        (store, dir)
    }

    fn exported(store: &Store, dir: &Path) -> Vec<u8> {
        let mut out = Vec::new();
        let written = export(store, &identity(), dir, &mut out).unwrap();
        assert!(written.records > 0);
        out
    }

    #[test]
    fn an_export_reads_back_whole_signed_and_without_credentials() {
        let (store, dir) = seeded();
        let bytes = exported(&store, dir.path());
        let text = String::from_utf8(bytes.clone()).unwrap();
        // No credential, key or tombstone is in the file at all.
        assert!(!text.contains("SECRET-HASH"));
        assert!(!text.contains("3q2+7w=="), "the channel keyring");
        assert!(!text.contains("GONE-BODY"));
        assert!(text.contains(WARNING));
        // The snapshot it read is gone.
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".data-export-"))
            .collect();
        assert!(left.is_empty(), "{left:?}");

        let mut seen = Vec::new();
        let reading = read(bytes.as_slice(), |r| seen.push(r.clone())).unwrap();
        assert_eq!(reading.verified, Ok(()));
        assert_eq!(reading.header["contract"], CONTRACT);
        assert_eq!(reading.header["node"], identity().node_id());
        let count = |kind: &str| {
            reading
                .counts
                .iter()
                .find(|(k, _)| k == kind)
                .map_or(0, |(_, n)| *n)
        };
        assert_eq!(count("document"), 3);
        assert_eq!(count("memory"), 1);
        let memory = seen.iter().find(|r| r["table"] == "memory").unwrap();
        assert_eq!(memory["row"]["body"], "the build uses bun");
        assert_eq!(memory["row"]["confidence"], 0.5);
        let channel = seen.iter().find(|r| r["table"] == "channel").unwrap();
        assert_eq!(channel["kind"], "session");
        assert!(channel["row"].get("keyring").is_none());
        let workspace = seen
            .iter()
            .find(|r| r["directory"] == "workspaces")
            .unwrap();
        assert_eq!(workspace["row"]["name"], "w1");
        assert_eq!(workspace["row"]["bytes"], 100);
        assert_eq!(seen.last().unwrap()["kind"], "end");
        assert_eq!(reading.records, seen.len() as u64 - 2);

        // The documents write out as document export writes them, and a
        // slug that is not a file name is skipped, not followed.
        let docs: Vec<Value> = seen
            .iter()
            .filter(|r| r["table"] == "document")
            .cloned()
            .collect();
        let out = tempfile::tempdir().unwrap();
        let (written, skipped) = write_documents(out.path(), &docs).unwrap();
        assert_eq!((written, skipped), (2, 1));
        let base = out.path().join("personal");
        assert_eq!(
            std::fs::read_to_string(base.join("note-one.md")).unwrap(),
            "# One\n"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("archive/note-two.md")).unwrap(),
            "# Two\n"
        );
    }

    #[test]
    fn a_changed_or_cut_export_says_so_and_still_reads() {
        let (store, dir) = seeded();
        let bytes = exported(&store, dir.path());
        let text = String::from_utf8(bytes).unwrap();

        let changed = text.replace("the build uses bun", "the build uses npm");
        let reading = read(changed.as_bytes(), |_| {}).unwrap();
        assert!(reading.verified.unwrap_err().contains("digest"));

        let mut lines: Vec<&str> = text.lines().collect();
        lines.pop();
        let cut = lines.join("\n");
        let reading = read(cut.as_bytes(), |_| {}).unwrap();
        assert!(reading.verified.unwrap_err().contains("cut short"));
        assert!(reading.records > 0);

        // Signed by another key over the same digest: refused.
        let mut trailer: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        let digest = hex::decode(trailer["sha256"].as_str().unwrap()).unwrap();
        let mut message = SIGNING_DOMAIN.to_vec();
        message.extend_from_slice(&digest);
        let other = proto::keys::Identity::from_seed(&[9u8; 32]);
        trailer["signature"] = json!(hex::encode(other.sign(&message).to_bytes()));
        let mut forged: Vec<&str> = text.lines().collect();
        forged.pop();
        let forged = format!("{}\n{trailer}\n", forged.join("\n"));
        let reading = read(forged.as_bytes(), |_| {}).unwrap();
        assert!(reading.verified.unwrap_err().contains("signature"));

        // Another contract version is refused, not misread.
        let other = text.replacen("\"contract_version\":1", "\"contract_version\":2", 1);
        assert!(matches!(
            read(other.as_bytes(), |_| {}),
            Err(ContractError::UnknownContract(_))
        ));
        assert!(matches!(
            read("{\"kind\":\"package\"}\n".as_bytes(), |_| {}),
            Err(ContractError::NotAnExport(_))
        ));
    }
}
