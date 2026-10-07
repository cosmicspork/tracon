//! What the node holds, kind by kind, for the data pane in Settings.
//!
//! Retention is decided: the node keeps everything and the operator deletes
//! by hand. This is the account that makes that workable: each kind of data,
//! how many there are and what they weigh, where it is deleted if it can be,
//! and what a delete does beyond this node. Read-only; the deletes stay where
//! they already are.

use std::path::Path;

use serde::Serialize;

use crate::config::Config;
use crate::store::Store;

/// Where a kind is deleted, in the interface.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Delete {
    /// The route that deletes one at a time.
    pub path: &'static str,
    pub label: &'static str,
}

/// One kind of data the node holds.
#[derive(Debug, Clone, Serialize)]
pub struct Holding {
    pub kind: &'static str,
    pub label: &'static str,
    /// What one of them is: the count is of these.
    pub unit: &'static str,
    pub count: i64,
    /// Rows: what the values carry. Directories: what the files take on disk.
    pub bytes: i64,
    /// Where it is deleted; none when nothing deletes it yet.
    pub delete: Option<Delete>,
    /// What a delete does beyond this node, or why there is none.
    pub propagation: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    /// The database file, free pages included.
    pub database_bytes: i64,
    /// The database and every directory counted below.
    pub total_bytes: i64,
    pub holdings: Vec<Holding>,
}

/// Where something lives: the tables whose rows it is (the first is what
/// is counted), and the state directories whose files it is.
struct Kind {
    kind: &'static str,
    label: &'static str,
    unit: &'static str,
    tables: &'static [&'static str],
    dirs: &'static [&'static str],
    delete: Option<Delete>,
    propagation: &'static str,
}

const RUNTIME_STORAGE: Delete = Delete {
    path: "/settings#maintenance",
    label: "Maintenance, under Runtime storage, once the session is archived",
};

const KINDS: &[Kind] = &[
    Kind {
        kind: "sessions",
        label: "Sessions",
        unit: "session",
        tables: &[
            "session",
            "permission_request",
            "approval",
            "turn_usage",
            "model_usage",
            "context_receipt",
            "operator_question",
            "operator_issue",
            "operator_notification",
        ],
        dirs: &[],
        delete: None,
        propagation: "Nothing deletes a session yet: its record is what its reviews, approvals \
                      and usage point at. Archiving one ends it and frees its workspace.",
    },
    Kind {
        kind: "events",
        label: "Session logs",
        unit: "event",
        tables: &["event", "external_event"],
        dirs: &[],
        delete: None,
        propagation: "Nothing deletes a log entry yet; a log is kept whole or not at all.",
    },
    Kind {
        kind: "evidence",
        label: "Candidates and evidence",
        unit: "candidate",
        tables: &[
            "candidate",
            "candidate_file",
            "check_run",
            "demonstration",
            "review",
            "review_revision",
            "review_decision",
            "publication",
            "shown_work",
            "criterion_judgement",
            "qa_deployment",
            "qa_browser_run",
            "qa_asset",
            "prototype",
        ],
        dirs: &[],
        delete: None,
        propagation: "Nothing deletes evidence yet: a verdict is only as good as what it was \
                      given, and a review may already be published.",
    },
    Kind {
        kind: "documents",
        label: "Documents",
        unit: "document",
        tables: &["document", "document_bundle_file", "document_bundle_chunk"],
        dirs: &[],
        delete: Some(Delete {
            path: "/docs",
            label: "Documents, one at a time",
        }),
        propagation: "A delete replicates to every node on the document's channel and to the \
                      hub. Each keeps a tombstone, the row with its content cleared, so the \
                      delete wins over an older copy arriving later.",
    },
    Kind {
        kind: "memories",
        label: "Memories",
        unit: "memory",
        tables: &["memory"],
        dirs: &[],
        delete: Some(Delete {
            path: "/memories",
            label: "Memories, one at a time",
        }),
        propagation: "A delete replicates to every node on the memory's channel and to the \
                      hub, each keeping a tombstone, and drops it from recall.",
    },
    Kind {
        kind: "work",
        label: "Work items",
        unit: "work item",
        tables: &["work_item"],
        dirs: &[],
        delete: Some(Delete {
            path: "/work",
            label: "Work, one item at a time",
        }),
        propagation: "A delete replicates to every node on the item's channel and to the hub, \
                      each keeping a tombstone. Sessions that worked on it keep their record.",
    },
    Kind {
        kind: "index",
        label: "Search index",
        unit: "vector",
        tables: &["embedding"],
        dirs: &[],
        delete: None,
        propagation: "Derived from documents and memories on this node: deleting one of them \
                      drops its vectors. Never replicated.",
    },
    Kind {
        kind: "workspaces",
        label: "Workspaces",
        unit: "workspace",
        tables: &[],
        dirs: &["workspaces", "workspace-staging"],
        delete: Some(RUNTIME_STORAGE),
        propagation: "On this node only; nothing replicates a workspace. Removing one gives up \
                      resuming, exporting and restoring its session.",
    },
    Kind {
        kind: "harness",
        label: "Harness state",
        unit: "session",
        tables: &[
            "opencode_session",
            "opencode_object",
            "opencode_intent",
            "opencode_state",
            "opencode_part_claim",
        ],
        dirs: &["sessions", "harness-state"],
        delete: Some(RUNTIME_STORAGE),
        propagation: "On this node only. Removing a session's directory gives up resuming it; \
                      the rows the node mirrored from the harness stay with the session.",
    },
];

/// What the node holds now. Walks the state directories, so call it off
/// the async runtime.
pub fn inventory(store: &Store) -> Result<Inventory, crate::store::StoreError> {
    inventory_in(store, &Config::state_dir())
}

pub fn inventory_in(
    store: &Store,
    state_dir: &Path,
) -> Result<Inventory, crate::store::StoreError> {
    let mut holdings = Vec::new();
    let mut on_disk = 0;
    for kind in KINDS {
        let mut count = 0;
        let mut bytes = 0;
        for (i, table) in kind.tables.iter().enumerate() {
            let weight = store.table_weight(table)?;
            if i == 0 {
                count = weight.rows;
            }
            bytes += weight.bytes;
        }
        for dir in kind.dirs {
            let (entries, size) = directory(&state_dir.join(dir));
            if kind.tables.is_empty() {
                count += entries;
            }
            bytes += size;
            on_disk += size;
        }
        holdings.push(Holding {
            kind: kind.kind,
            label: kind.label,
            unit: kind.unit,
            count,
            bytes,
            delete: kind.delete.clone(),
            propagation: kind.propagation,
        });
    }
    let database_bytes = store.database_bytes()?;
    Ok(Inventory {
        database_bytes,
        total_bytes: database_bytes + on_disk,
        holdings,
    })
}

/// How many entries a directory has, and what its files take, without
/// following links out of it.
fn directory(path: &Path) -> (i64, i64) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return (0, 0);
    };
    let mut count = 0;
    let mut bytes = 0;
    for entry in entries.flatten() {
        count += 1;
        bytes += size(&entry.path());
    }
    (count, bytes)
}

fn size(path: &Path) -> i64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        std::fs::read_dir(path)
            .map(|entries| entries.flatten().map(|e| size(&e.path())).sum())
            .unwrap_or(0)
    } else if meta.is_file() {
        meta.len() as i64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_is_accounted_for_and_only_real_deletes_are_offered() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("workspaces/w1/src")).unwrap();
        std::fs::write(dir.path().join("workspaces/w1/src/a"), [0u8; 100]).unwrap();
        std::fs::create_dir_all(dir.path().join("workspace-staging/w2")).unwrap();
        let inventory = inventory_in(&store, dir.path()).unwrap();
        let kinds: Vec<&str> = inventory.holdings.iter().map(|h| h.kind).collect();
        assert_eq!(
            kinds,
            [
                "sessions",
                "events",
                "evidence",
                "documents",
                "memories",
                "work",
                "index",
                "workspaces",
                "harness"
            ]
        );
        let workspaces = &inventory.holdings[7];
        assert_eq!((workspaces.count, workspaces.bytes), (2, 100));
        assert_eq!(
            inventory.total_bytes,
            inventory.database_bytes + 100,
            "the total is the database and the directories"
        );
        // Each table a kind names is one the schema has: a renamed table
        // would otherwise weigh nothing, silently.
        let missing: Vec<&str> = KINDS
            .iter()
            .flat_map(|k| k.tables.iter().copied())
            .filter(|t| !store.has_table(t))
            .collect();
        assert!(missing.is_empty(), "missing: {missing:?}");
        for holding in &inventory.holdings {
            let deletable = holding.delete.is_some();
            let named =
                ["documents", "memories", "work", "workspaces", "harness"].contains(&holding.kind);
            assert_eq!(deletable, named, "{}", holding.kind);
            assert!(!holding.propagation.is_empty());
        }
    }
}
