//! The replica half of the hub: for channels whose keys a node has handed it,
//! the hub opens frames on receipt and keeps the same replicated tables a
//! node does, so a memory written from a sleeping laptop is indexed at once
//! and the nightly batch has somewhere always-on to run. A channel nobody
//! shares with the hub stays ciphertext: there is no key to open it with, and
//! the count of frames it could not open is the only trace.
//!
//! The loop reads from the same frame store the relay appends to, from its
//! own cursor, so a crash between append and apply loses nothing and startup
//! catch-up is the ordinary path.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use proto::frame::{Change, ChangeOp, Envelope, Payload, MESH_CHANNEL};
use proto::keyring::Keyring;
use proto::keys::{key32, Identity};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use tokio::sync::Notify;

use crate::auth::now_ms;
use crate::pokes::PokeHub;
use crate::store::{FrameStore, MemberStore};

pub const DB_FILE: &str = "hub.db";

/// A random 128-bit id for hub-authored records: the hub has no uuid crate
/// and needs no ordering guarantee.
fn rand_id() -> u128 {
    use rand_core::RngCore;
    let mut b = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut b);
    u128::from_be_bytes(b)
}
const PAGE: usize = 200;

pub struct Replica {
    identity: Identity,
    db: Mutex<Connection>,
    frames: Arc<dyn FrameStore>,
    members: Arc<dyn MemberStore>,
    pokes: Arc<PokeHub>,
    pub wake: Notify,
    undecryptable: AtomicU64,
    applied: AtomicU64,
}

impl Replica {
    pub fn open(
        data_dir: &Path,
        identity: Identity,
        frames: Arc<dyn FrameStore>,
        members: Arc<dyn MemberStore>,
        pokes: Arc<PokeHub>,
    ) -> rusqlite::Result<Arc<Self>> {
        let conn = Connection::open(data_dir.join(DB_FILE))?;
        Self::with_conn(conn, identity, frames, members, pokes)
    }

    pub fn in_memory(
        identity: Identity,
        frames: Arc<dyn FrameStore>,
        members: Arc<dyn MemberStore>,
        pokes: Arc<PokeHub>,
    ) -> rusqlite::Result<Arc<Self>> {
        Self::with_conn(
            Connection::open_in_memory()?,
            identity,
            frames,
            members,
            pokes,
        )
    }

    fn with_conn(
        conn: Connection,
        identity: Identity,
        frames: Arc<dyn FrameStore>,
        members: Arc<dyn MemberStore>,
        pokes: Arc<PokeHub>,
    ) -> rusqlite::Result<Arc<Self>> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        tracon_sync::schema::install(&conn)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS replica_channel (
                 name TEXT PRIMARY KEY, keyring BLOB NOT NULL, bindings_json TEXT NOT NULL DEFAULT '{}');
             CREATE TABLE IF NOT EXISTS replica_cursor (channel TEXT PRIMARY KEY, seq INTEGER NOT NULL);",
        )?;
        Ok(Arc::new(Self {
            identity,
            db: Mutex::new(conn),
            frames,
            members,
            pokes,
            wake: Notify::new(),
            undecryptable: AtomicU64::new(0),
            applied: AtomicU64::new(0),
        }))
    }

    pub fn node_id(&self) -> String {
        self.identity.node_id()
    }

    pub fn x25519_hex(&self) -> String {
        self.identity.x25519_hex()
    }

    pub fn undecryptable(&self) -> u64 {
        self.undecryptable.load(Ordering::Relaxed)
    }

    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::Relaxed)
    }

    /// Channels the hub holds a keyring for: the ones a node chose to share.
    pub fn readable_channels(&self) -> Vec<String> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM replica_channel ORDER BY name")
            .expect("prepare");
        stmt.query_map([], |r| r.get(0))
            .expect("query")
            .filter_map(|r| r.ok())
            .collect()
    }

    pub fn bindings_of(&self, channel: &str) -> Value {
        let conn = self.db.lock().unwrap();
        conn.query_row(
            "SELECT bindings_json FROM replica_channel WHERE name = ?1",
            [channel],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
    }

    fn keyring(&self, channel: &str) -> Option<Keyring> {
        let conn = self.db.lock().unwrap();
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT keyring FROM replica_channel WHERE name = ?1",
                [channel],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        bytes.and_then(|b| Keyring::from_bytes(&b).ok())
    }

    fn cursor(&self, channel: &str) -> u64 {
        let conn = self.db.lock().unwrap();
        conn.query_row(
            "SELECT seq FROM replica_cursor WHERE channel = ?1",
            [channel],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or(0) as u64
    }

    fn set_cursor(&self, channel: &str, seq: u64) {
        let conn = self.db.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO replica_cursor (channel, seq) VALUES (?1, ?2)
             ON CONFLICT(channel) DO UPDATE SET seq = excluded.seq",
            params![channel, seq as i64],
        );
    }

    /// Read every channel the hub is a member of from its cursor and apply
    /// what it can open. Returns frames applied.
    pub fn ingest_pending(&self) -> usize {
        let me = self.node_id();
        let channels = self
            .members
            .get(&me)
            .ok()
            .flatten()
            .map(|m| m.channels)
            .unwrap_or_default();
        let mut applied = 0;
        for channel in channels {
            loop {
                let after = self.cursor(&channel);
                let page = match self.frames.read(&channel, after, PAGE) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(channel, error = %e, "replica read failed");
                        break;
                    }
                };
                // Behind retention on our own store: nothing to do but move on.
                if page.oldest > 0 && after + 1 < page.oldest {
                    self.set_cursor(&channel, page.oldest - 1);
                    continue;
                }
                if page.frames.is_empty() {
                    break;
                }
                let n = page.frames.len();
                for (seq, json) in page.frames {
                    if self.ingest_one(&channel, &json) {
                        applied += 1;
                    }
                    self.set_cursor(&channel, seq);
                }
                if n < PAGE {
                    break;
                }
            }
        }
        self.applied.fetch_add(applied as u64, Ordering::Relaxed);
        applied
    }

    fn ingest_one(&self, channel: &str, json: &str) -> bool {
        let Ok(env) = serde_json::from_str::<Envelope>(json) else {
            return false;
        };
        let Ok(sender) = env.verify() else {
            return false;
        };
        let sender = hex::encode(sender);
        if sender == self.node_id() {
            return false;
        }
        if env.is_direct() {
            if env.recipient.as_deref() != Some(self.node_id().as_str()) {
                return false;
            }
            let Ok(payload) = env.open_direct(&self.identity) else {
                return false;
            };
            return self.handle_direct(&sender, payload);
        }
        if channel == MESH_CHANNEL {
            // Presence and node rows: not records. Nothing to keep.
            return false;
        }
        let Some(ring) = self.keyring(channel) else {
            self.undecryptable.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let payload = match env.open_channel(&ring, &self.identity) {
            Ok(p) => p,
            Err(_) => {
                self.undecryptable.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        match payload {
            Payload::Changes {
                channel: c,
                changes,
            } if c == channel => {
                let mut conn = self.db.lock().unwrap();
                match tracon_sync::apply_changes(&mut conn, &sender, channel, &changes, now_ms()) {
                    Ok(r) => r.contains(&tracon_sync::Applied::Stored),
                    Err(e) => {
                        tracing::warn!(channel, error = %e, "replica apply failed");
                        false
                    }
                }
            }
            _ => false,
        }
    }

    fn handle_direct(&self, sender: &str, payload: Payload) -> bool {
        match payload {
            Payload::KeyHandoff { channels } => {
                let conn = self.db.lock().unwrap();
                let mut n = 0;
                for h in channels {
                    let merged = conn
                        .query_row(
                            "SELECT keyring FROM replica_channel WHERE name = ?1",
                            [&h.name],
                            |r| r.get::<_, Vec<u8>>(0),
                        )
                        .optional()
                        .ok()
                        .flatten()
                        .and_then(|b| Keyring::from_bytes(&b).ok())
                        .map(|mine| Keyring::merge(&mine, &h.keyring))
                        .unwrap_or_else(|| h.keyring.clone());
                    if conn
                        .execute(
                            "INSERT INTO replica_channel (name, keyring, bindings_json) VALUES (?1, ?2, ?3)
                             ON CONFLICT(name) DO UPDATE SET keyring = excluded.keyring, bindings_json = excluded.bindings_json",
                            params![h.name, merged.to_bytes(), h.bindings_json],
                        )
                        .is_ok()
                    {
                        n += 1;
                    }
                }
                drop(conn);
                tracing::info!(from = %sender, channels = n, "channel keys received by the replica");
                // Frames on those channels may already sit behind the cursor
                // — but the cursor is per channel and moved past them while
                // they were unopenable. Rewind so they are read again.
                for name in self.readable_channels() {
                    self.set_cursor(&name, 0);
                }
                n > 0
            }
            Payload::ChangesRequest {
                channel,
                after_site_seq,
            } => {
                self.answer_changes_request(sender, &channel, after_site_seq);
                true
            }
            _ => false,
        }
    }

    /// Only the hub's own changes, as any site answers.
    fn answer_changes_request(&self, sender: &str, channel: &str, after: i64) {
        let Some(member) = self.members.get(sender).ok().flatten() else {
            return;
        };
        let Some(pk) = key32(&member.x25519_pub).map(x25519_dalek::PublicKey::from) else {
            return;
        };
        let me = self.node_id();
        let mut after = after;
        loop {
            let changes = {
                let conn = self.db.lock().unwrap();
                tracon_sync::apply::changes_of_site_after(&conn, &me, channel, after, 500)
                    .unwrap_or_default()
            };
            let done = changes.len() < 500;
            if changes.is_empty() && after > 0 {
                break;
            }
            after = changes.last().map(|c| c.site_seq).unwrap_or(after);
            let empty = changes.is_empty();
            let payload = Payload::ChangesBatch {
                channel: channel.to_string(),
                changes,
                done,
            };
            if let Ok(env) = Envelope::seal_direct(
                &self.identity,
                MESH_CHANNEL,
                sender,
                &pk,
                &payload,
                now_ms(),
            ) {
                self.post(MESH_CHANNEL, &env);
            }
            if done || empty {
                break;
            }
        }
    }

    /// A hub-authored record: stamped in the replica and sealed onto the
    /// channel under its newest epoch, exactly as a node would.
    #[allow(clippy::too_many_arguments)]
    pub fn write_change(
        &self,
        channel: &str,
        table: &str,
        op: ChangeOp,
        id: &str,
        row: Value,
    ) -> Result<Change, String> {
        let ring = self
            .keyring(channel)
            .ok_or_else(|| format!("the hub holds no key for {channel}"))?;
        let change = {
            let mut conn = self.db.lock().unwrap();
            tracon_sync::write_change(
                &mut conn,
                &self.node_id(),
                channel,
                table,
                op,
                id,
                row,
                now_ms(),
            )
            .map_err(|e| e.to_string())?
        };
        let payload = Payload::Changes {
            channel: channel.to_string(),
            changes: vec![change.clone()],
        };
        let env = Envelope::seal_channel(&self.identity, channel, None, &ring, &payload, now_ms())
            .map_err(|e| e.to_string())?;
        self.post(channel, &env);
        Ok(change)
    }

    fn post(&self, channel: &str, env: &Envelope) {
        let Ok(json) = serde_json::to_string(env) else {
            return;
        };
        if let Err(e) = self.frames.append(channel, &json, now_ms()) {
            tracing::warn!(channel, error = %e, "replica could not append its frame");
            return;
        }
        for m in self.members.members_of(channel).unwrap_or_default() {
            if let Some(k) = key32(&m.node_id) {
                self.pokes.poke(&k);
            }
        }
    }

    /// The replica's own reads, for tests and the batch.
    pub fn with_db<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        let conn = self.db.lock().unwrap();
        f(&conn)
    }

    /// Build tonight's batch for every channel whose binding says the hub
    /// processes it. Returns promotion ids created.
    pub fn batch_now(&self, min_age_ms: i64) -> Vec<String> {
        let mut out = Vec::new();
        for channel in self.readable_channels() {
            if self.bindings_of(&channel)["processing"].as_str() != Some("hub") {
                continue;
            }
            let id = format!("{:032x}", rand_id());
            let plan = {
                let conn = self.db.lock().unwrap();
                tracon_sync::batch::plan_promotion(&conn, &channel, &id, now_ms(), min_age_ms)
            };
            let Ok(Some(plan)) = plan else {
                continue;
            };
            if self
                .write_change(
                    &channel,
                    "promotion",
                    ChangeOp::Upsert,
                    &id,
                    plan.promotion_row(now_ms()),
                )
                .is_err()
            {
                continue;
            }
            for item in &plan.items {
                let row = {
                    let conn = self.db.lock().unwrap();
                    tracon_sync::batch::memory_row_with_state(
                        &conn,
                        &item.memory_id,
                        "proposed",
                        now_ms(),
                    )
                };
                if let Ok(Some(row)) = row {
                    let _ = self.write_change(
                        &channel,
                        "memory",
                        ChangeOp::Upsert,
                        &item.memory_id,
                        row,
                    );
                }
            }
            out.push(id);
        }
        out
    }

    /// The nightly loop: at `HH:MM` UTC, build the batches.
    pub async fn nightly(self: Arc<Self>, at: String) {
        loop {
            let (h, m) = at
                .split_once(':')
                .and_then(|(h, m)| Some((h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)))
                .unwrap_or((3, 0));
            let day = (now_ms() / 1000).rem_euclid(86_400);
            let mut wait = h * 3600 + m * 60 - day;
            if wait <= 0 {
                wait += 86_400;
            }
            tokio::time::sleep(std::time::Duration::from_secs(wait as u64)).await;
            let made = self.batch_now(6 * 3600 * 1000);
            if !made.is_empty() {
                tracing::info!(batches = made.len(), "nightly promotion batches built");
            }
        }
    }

    /// Wake on every append, look anyway every thirty seconds.
    pub async fn run(self: Arc<Self>) {
        loop {
            let n = self.ingest_pending();
            if n > 0 {
                tracing::debug!(applied = n, "replica applied changes");
            }
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            }
        }
    }
}
