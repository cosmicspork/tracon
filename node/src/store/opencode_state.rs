//! What an OpenCode session's state *is*, written down so a copy of it can be
//! restored into a runtime that can actually read it.
//!
//! The harness keeps everything it knows in one SQLite database, and that
//! database carries no version of its own. Upstream's `migration` table is an
//! applied-id ledger with no `user_version`, no maximum-known check and no
//! fingerprint (`config-state.md` §7.2), so an older binary opening a newer
//! database proceeds silently and can still write to it — a data-loss shape,
//! not an error shape (§9 row 7). Nothing in the file says which build made it.
//!
//! So the node says it. On every OpenCode launch a row is written here naming
//! four things, and each one gates something later:
//!
//! * **the build identity** — what the harness reported at the handshake
//!   (#187's `harness_found`) next to what this node pinned. A restore whose
//!   backup was made by a newer build than the restoring node's pin is refused
//!   on this alone.
//! * **the state schema generation** — the ordered list of migration ids the
//!   database actually holds, read from its own `migration` table after first
//!   start, plus a digest of that list. This is the node's substitute for the
//!   `user_version` upstream does not keep, and it is what makes "never restore
//!   a newer generation into an older runtime" checkable rather than hoped for.
//! * **the manifest digest** — which launch manifest the session ran against.
//!   Nullable: the manifest side is built separately, and a placeholder that
//!   says "not recorded" is honest where a fabricated digest would not be.
//! * **where the state lives** — the runtime volume and the volume-relative
//!   path of the tree holding the database, so a backup does not have to
//!   re-derive the layout from the adapter it was not taken with.
//!
//! One row per tracon session: the fence in `session::materialize` already
//! guarantees one writer per session state, and this is that state's identity.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{now_ms, Result, Store};

/// The identity recorded for one session's OpenCode state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenCodeStateRow {
    pub session_id: String,
    /// What the harness said it was at the handshake.
    pub build_version: String,
    /// What this node expected it to be.
    pub build_pinned: String,
    /// Applied migration ids, in order. Empty when the database could not be
    /// read yet — a backup fills it in from the copy it takes.
    pub generation: Vec<String>,
    pub generation_digest: String,
    /// The launch manifest this session ran against, when one is recorded.
    pub manifest_digest: Option<String>,
    /// The runtime volume holding the state.
    pub state_volume: String,
    /// Volume-relative directory of the harness state tree.
    pub state_path: String,
    pub recorded_ms: i64,
    pub updated_ms: i64,
}

/// A stable name for an ordered list of migration ids. Ids are upstream's own
/// timestamped filenames, so the list is compared as a set for coverage and
/// digested as a sequence for identity.
pub fn generation_digest(ids: &[String]) -> String {
    if ids.is_empty() {
        return String::new();
    }
    let mut hash = Sha256::new();
    for id in ids {
        hash.update(id.as_bytes());
        hash.update(b"\n");
    }
    hex::encode(hash.finalize())
}

impl OpenCodeStateRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let generation: String = r.get("generation_json")?;
        Ok(Self {
            session_id: r.get("session_id")?,
            build_version: r.get("build_version")?,
            build_pinned: r.get("build_pinned")?,
            generation: serde_json::from_str(&generation).unwrap_or_default(),
            generation_digest: r.get("generation_digest")?,
            manifest_digest: r.get("manifest_digest")?,
            state_volume: r.get("state_volume")?,
            state_path: r.get("state_path")?,
            recorded_ms: r.get("recorded_ms")?,
            updated_ms: r.get("updated_ms")?,
        })
    }

    /// Whether this generation contains everything `other` does. A restore is
    /// only safe in this direction: the runtime must already know every
    /// migration the backup carries, or it will query a schema it has never
    /// heard of and write to it anyway.
    pub fn covers(&self, other: &[String]) -> bool {
        other.iter().all(|id| self.generation.contains(id))
    }
}

impl Store {
    /// Record what a session's OpenCode state is. Idempotent per session: a
    /// relaunch overwrites the identity, because the state is whatever the
    /// build that just opened it made of it.
    ///
    /// An empty `generation` never overwrites a recorded one — the generation
    /// is read from the database itself and a launch that could not read it
    /// must not erase what an earlier read established.
    pub fn opencode_state_put(&self, row: &OpenCodeStateRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let generation = serde_json::to_string(&row.generation)?;
        let digest = if row.generation.is_empty() {
            String::new()
        } else {
            generation_digest(&row.generation)
        };
        let now = now_ms();
        conn.execute(
            "INSERT INTO opencode_state
                (session_id, build_version, build_pinned, generation_json, generation_digest,
                 manifest_digest, state_volume, state_path, recorded_ms, updated_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)
             ON CONFLICT(session_id) DO UPDATE SET
                build_version=?2, build_pinned=?3,
                generation_json=CASE WHEN ?4='[]' THEN generation_json ELSE ?4 END,
                generation_digest=CASE WHEN ?4='[]' THEN generation_digest ELSE ?5 END,
                manifest_digest=COALESCE(?6, manifest_digest),
                state_volume=?7, state_path=?8, updated_ms=?9",
            params![
                row.session_id,
                row.build_version,
                row.build_pinned,
                generation,
                digest,
                row.manifest_digest,
                row.state_volume,
                row.state_path,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn opencode_state_get(&self, session_id: &str) -> Result<Option<OpenCodeStateRow>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT * FROM opencode_state WHERE session_id=?1",
                params![session_id],
                OpenCodeStateRow::from_row,
            )
            .optional()?)
    }

    /// Replace the recorded generation for a session. Called when a backup or
    /// an upgrade has read the database itself and knows better than the
    /// launch did.
    pub fn opencode_state_set_generation(&self, session_id: &str, ids: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE opencode_state
                SET generation_json=?2, generation_digest=?3, updated_ms=?4
              WHERE session_id=?1",
            params![
                session_id,
                serde_json::to_string(ids)?,
                generation_digest(ids),
                now_ms(),
            ],
        )?;
        Ok(())
    }

    /// The generation this node should expect of a given OpenCode build: the
    /// widest one any session on that build actually produced. Widest rather
    /// than latest, because a session that never migrated fully is a floor,
    /// not a ceiling, and a restore gate wants the ceiling the runtime knows.
    pub fn opencode_state_expected(&self, build_version: &str) -> Result<Option<OpenCodeStateRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM opencode_state
              WHERE build_version=?1 AND generation_digest<>''
              ORDER BY updated_ms DESC",
        )?;
        let rows = stmt
            .query_map(params![build_version], OpenCodeStateRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().max_by_key(|r| r.generation.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::store::{NodeRow, SessionRow};

    fn store_with(sessions: &[&str]) -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .put_node(&NodeRow {
                id: "n1".into(),
                name: "t".into(),
                state: "ready".into(),
                failed_check: None,
                failed_detail: None,
                harness_id: "opencode".into(),
                harness_pinned: "1.18.30".into(),
                harness_found: None,
                models_json: None,
                checked_at_ms: None,
                is_self: 1,
                x25519_pub: None,
                last_seen_ms: None,
                reachable: 1,
                providers_json: None,
                app_version: None,
                wire_contract: None,
                policy_identity: None,
                policy_sha256: None,
                policy_receipt_v1: None,
            })
            .unwrap();
        for id in sessions {
            store
                .insert_session(&SessionRow {
                    id: (*id).into(),
                    node_id: "n1".into(),
                    channel: "personal".into(),
                    work_item_id: None,
                    repo_path: "/r".into(),
                    worktree_path: None,
                    branch: "b".into(),
                    harness_id: "opencode".into(),
                    harness_version: "1.18.30".into(),
                    harness_agent: None,
                    harness_found: None,
                    harness_protocol: None,
                    harness_session_id: None,
                    container_name: None,
                    model: "m".into(),
                    project_id: None,
                    phase: "execute".into(),
                    policy_version: None,
                    review_id: None,
                    budget_tokens: 0,
                    tokens_used: 0,
                    cost_usd: None,
                    context_used: None,
                    context_size: None,
                    state: "running".into(),
                    end_reason: None,
                    last_error: None,
                    turn_active: 0,
                    draft: None,
                    draft_updated_ms: None,
                    created_ms: now_ms(),
                    started_mono_ms: None,
                    ended_mono_ms: None,
                    updated_ms: now_ms(),
                    archived_ms: None,
                    manifest_digest: None,
                })
                .unwrap();
        }
        store
    }

    fn row(session: &str, build: &str, generation: &[&str]) -> OpenCodeStateRow {
        OpenCodeStateRow {
            session_id: session.into(),
            build_version: build.into(),
            build_pinned: build.into(),
            generation: generation.iter().map(|s| s.to_string()).collect(),
            generation_digest: String::new(),
            manifest_digest: None,
            state_volume: "tracon-scratch-x".into(),
            state_path: "harness/run".into(),
            recorded_ms: 0,
            updated_ms: 0,
        }
    }

    #[test]
    fn a_generation_is_a_digest_of_the_ordered_ids() {
        let a = generation_digest(&["one".into(), "two".into()]);
        let b = generation_digest(&["two".into(), "one".into()]);
        assert_ne!(a, b, "order is part of the identity");
        assert_eq!(a, generation_digest(&["one".into(), "two".into()]));
        assert_eq!(generation_digest(&[]), "", "nothing read is not a digest");
    }

    /// The identity is per session and a relaunch replaces it — except that a
    /// launch which could not read the database must not erase a generation an
    /// earlier read established.
    #[test]
    fn a_relaunch_replaces_the_identity_but_an_unread_generation_does_not_erase_one() {
        let store = store_with(&["s1"]);
        store
            .opencode_state_put(&row("s1", "1.18.30", &["m1", "m2"]))
            .unwrap();
        let recorded = store.opencode_state_get("s1").unwrap().unwrap();
        assert_eq!(recorded.generation, ["m1", "m2"]);
        assert_eq!(
            recorded.generation_digest,
            generation_digest(&["m1".into(), "m2".into()])
        );

        store
            .opencode_state_put(&row("s1", "1.18.30", &[]))
            .unwrap();
        let kept = store.opencode_state_get("s1").unwrap().unwrap();
        assert_eq!(
            kept.generation,
            ["m1", "m2"],
            "an unread launch erases nothing"
        );

        store
            .opencode_state_set_generation("s1", &["m1".into(), "m2".into(), "m3".into()])
            .unwrap();
        assert_eq!(
            store.opencode_state_get("s1").unwrap().unwrap().generation,
            ["m1", "m2", "m3"]
        );
    }

    /// Coverage is a set question, not a count: a runtime whose generation is
    /// missing even one of the backup's ids cannot read that backup.
    #[test]
    fn coverage_is_every_id_the_backup_carries() {
        let wide = row("s", "1.18.30", &["m1", "m2", "m3"]);
        assert!(wide.covers(&["m1".into(), "m2".into()]));
        assert!(wide.covers(&[]));
        assert!(!wide.covers(&["m1".into(), "m9".into()]));
    }

    /// The expected generation for a build is the widest one that build made,
    /// so a session that stopped early cannot lower the gate.
    #[test]
    fn the_expected_generation_is_the_widest_that_build_produced() {
        let store = store_with(&["s1", "s2"]);
        store
            .opencode_state_put(&row("s1", "1.18.30", &["m1", "m2", "m3"]))
            .unwrap();
        store
            .opencode_state_put(&row("s2", "1.18.30", &["m1"]))
            .unwrap();
        let expected = store.opencode_state_expected("1.18.30").unwrap().unwrap();
        assert_eq!(expected.generation, ["m1", "m2", "m3"]);
        assert!(store.opencode_state_expected("1.19.0").unwrap().is_none());
    }
}
