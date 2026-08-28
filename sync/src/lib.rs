//! What the node and the hub share about records: the tables that replicate
//! (`document`, `memory`, `promotion`), the change log that makes a change
//! idempotent, the hybrid logical clock that orders writes across sites, and
//! the last-writer-wins rule that applies them. One crate so the replica in
//! the hub is the same code as the replica in a node, and neither drags the
//! other's dependencies in.
//!
//! The shape stays close to `crsql_changes` (table, key, site, sequence,
//! clock) so the deferred cr-sqlite path remains open.

pub mod apply;
pub mod batch;
pub mod hlc;
pub mod schema;

pub use apply::{apply_changes, write_change, Applied};
pub use hlc::Hlc;
pub use proto::frame::{Change, ChangeOp};

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Malformed(String),
}

pub type Result<T> = std::result::Result<T, SyncError>;

/// The replicated tables and their columns, in the order the row JSON is
/// read. `id` and the sync stamps are implicit on every table.
pub const TABLES: &[(&str, &[&str])] = &[
    (
        "document",
        &[
            "channel",
            "slug",
            "kind",
            "title",
            "body",
            "hash",
            "created_ms",
            "updated_ms",
        ],
    ),
    (
        "memory",
        &[
            "channel",
            "scope",
            "scope_ref",
            "kind",
            "body",
            "source_session",
            "source_node",
            "confidence",
            "state",
            "created_ms",
            "updated_ms",
        ],
    ),
    (
        "promotion",
        &[
            "channel",
            "items_json",
            "state",
            "verdicts_json",
            "decided_by",
            "decided_ms",
            "created_ms",
        ],
    ),
];

/// Columns emptied on a tombstone so a deleted record carries no content.
pub const CLEARED_ON_DELETE: &[&str] = &["title", "body", "items_json", "verdicts_json"];

pub fn columns_of(table: &str) -> Option<&'static [&'static str]> {
    TABLES.iter().find(|(t, _)| *t == table).map(|(_, c)| *c)
}
