//! Commands across nodes. A command for a session another node owns is sealed
//! to that node, queued through the outbox, and answered with an ack the same
//! way. The owner executes exactly what it would for a local request, so a
//! remote verdict or answer is gated by the owner's own policy and store.
//!
//! Event backfill rides the same path: a node that starts mirroring a session
//! part-way asks the owner for the events before its first mirrored one.

use std::time::Duration;

use proto::frame::{Change, Command, Payload, MESH_CHANNEL};
use serde_json::Value;

use super::client::MeshClient;
use crate::store::{EventRow, NewEvent};
use crate::stream::Frame;

/// What runs a command on the owning node. Implemented by the HTTP layer's
/// state, which has everything a local request would.
#[async_trait::async_trait]
pub trait CommandExecutor: Send + Sync {
    async fn execute(&self, sender: &str, command: Command) -> Result<Value, String>;
}

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("the owner did not answer in time (unreachable, or on an older build)")]
    Timeout,
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Local(String),
}

impl MeshClient {
    /// Queue a command for `node_id` without waiting for its answer.
    pub fn send_command(&self, node_id: &str, command: Command) -> Result<String, CommandError> {
        let cmd_id = uuid::Uuid::now_v7().to_string();
        self.enqueue_direct(
            MESH_CHANNEL,
            node_id,
            &Payload::Command {
                cmd_id: cmd_id.clone(),
                command,
            },
        )
        .map_err(|e| CommandError::Local(e.to_string()))?;
        Ok(cmd_id)
    }

    /// Send a command and wait for the owner's ack.
    pub async fn command(
        &self,
        node_id: &str,
        command: Command,
        timeout: Duration,
    ) -> Result<Value, CommandError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cmd_id = uuid::Uuid::now_v7().to_string();
        self.pending.lock().unwrap().insert(cmd_id.clone(), tx);
        if let Err(e) = self.enqueue_direct(
            MESH_CHANNEL,
            node_id,
            &Payload::Command {
                cmd_id: cmd_id.clone(),
                command,
            },
        ) {
            self.pending.lock().unwrap().remove(&cmd_id);
            return Err(CommandError::Local(e.to_string()));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(m))) => Err(CommandError::Refused(m)),
            Ok(Err(_)) => Err(CommandError::Local("client shut down".into())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&cmd_id);
                Err(CommandError::Timeout)
            }
        }
    }

    /// Execute a command a peer sent us and ack it. Runs off the pull path so a
    /// slow command does not stall ingestion.
    pub(super) fn run_command(&self, sender: String, cmd_id: String, command: Command) {
        let Some(this) = self.weak.upgrade() else {
            return;
        };
        tokio::spawn(async move {
            let result = match this.executor.get() {
                Some(executor) => executor.execute(&sender, command).await,
                None => Err("this node is not ready to run commands".into()),
            };
            let (ok, err) = match result {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(e)),
            };
            if let Err(e) =
                this.enqueue_direct(MESH_CHANNEL, &sender, &Payload::Ack { cmd_id, ok, err })
            {
                tracing::warn!(to = %sender, error = %e, "could not queue an ack");
            }
        });
    }

    /// Direct-seal credentials to a member, through the outbox — the same
    /// payload enrollment hands over, so the receiver's pin rule applies.
    pub fn send_credential_handoff(
        &self,
        node_id: &str,
        credentials: Vec<Value>,
    ) -> Result<(), String> {
        self.enqueue_direct(
            MESH_CHANNEL,
            node_id,
            &Payload::CredentialHandoff { credentials },
        )
        .map_err(|e| e.to_string())
    }

    /// Ask a session's owner for the events this node has not mirrored.
    pub fn request_backfill(&self, node_id: &str, session_id: &str) {
        let after = self
            .store
            .mirrored_origin_max(session_id)
            .ok()
            .flatten()
            .unwrap_or(0);
        if let Err(e) = self.enqueue_direct(
            MESH_CHANNEL,
            node_id,
            &Payload::EventsRequest {
                session_id: session_id.to_string(),
                after_origin_seq: after,
            },
        ) {
            tracing::debug!(error = %e, "backfill not requested");
        }
    }

    /// Ask a site for the record changes this node has not seen from it on a
    /// channel. Used on a sequence gap, after a retention resync, and when a
    /// channel key first arrives.
    pub fn request_changes_backfill(&self, site: &str, channel: &str) {
        if site == self.node_id() {
            return;
        }
        let after = self.store.change_log_max(site, channel).unwrap_or(0);
        if let Err(e) = self.enqueue_direct(
            MESH_CHANNEL,
            site,
            &Payload::ChangesRequest {
                channel: channel.to_string(),
                after_site_seq: after,
            },
        ) {
            tracing::debug!(error = %e, site, channel, "changes backfill not requested");
        }
    }

    /// Every site known on a channel, asked for what this node lacks.
    pub fn request_changes_backfill_all(&self, channel: &str) {
        let mut sites = self.store.nodes_in_channel(channel).unwrap_or_default();
        for s in self.store.sites_on_channel(channel).unwrap_or_default() {
            if !sites.contains(&s) {
                sites.push(s);
            }
        }
        for site in sites {
            self.request_changes_backfill(&site, channel);
        }
    }

    /// Only this site's own changes; a peer asking for another site's gets
    /// nothing, exactly as with events.
    pub(super) fn answer_changes_request(&self, sender: &str, channel: &str, after: i64) {
        let me = self.node_id();
        let mut after = after;
        loop {
            let changes = self
                .store
                .changes_of_site_after(&me, channel, after, 500)
                .unwrap_or_default();
            let done = changes.len() < 500;
            if changes.is_empty() && after > 0 {
                break;
            }
            after = changes.last().map(|c| c.site_seq).unwrap_or(after);
            let empty = changes.is_empty();
            let _ = self.enqueue_direct(
                MESH_CHANNEL,
                sender,
                &Payload::ChangesBatch {
                    channel: channel.to_string(),
                    changes,
                    done,
                },
            );
            if done || empty {
                break;
            }
        }
    }

    pub(super) fn apply_changes_batch(
        &self,
        sender: &str,
        channel: &str,
        changes: &[Change],
    ) -> usize {
        // A batch is a peer's own history on a channel this node reads; the
        // apply enforces that every change is the sender's.
        if self.store.channel_get(channel).ok().flatten().is_none() {
            return 0;
        }
        match self.store.apply_changes(sender, channel, changes) {
            Ok(results) => {
                let won: Vec<Change> = changes
                    .iter()
                    .zip(results.iter())
                    .filter(|(_, r)| **r == tracon_sync::Applied::Stored)
                    .map(|(c, _)| c.clone())
                    .collect();
                let n = won.len();
                if n > 0 {
                    self.bus.publish_untapped(Frame::Changes {
                        channel: channel.to_string(),
                        changes: won,
                    });
                }
                n
            }
            Err(e) => {
                tracing::warn!(error = %e, "changes batch not applied");
                0
            }
        }
    }

    pub(super) fn answer_events_request(&self, sender: &str, session_id: &str, after: i64) {
        // Only our own sessions; a peer asking about someone else's gets nothing.
        let Ok(Some(row)) = self.store.get_session(session_id) else {
            return;
        };
        if row.node_id != self.node_id() {
            return;
        }
        let mut after = after;
        loop {
            let rows: Vec<EventRow> = self
                .store
                .events_after(session_id, after, 500)
                .unwrap_or_default();
            let done = rows.len() < 500;
            if rows.is_empty() && after > 0 {
                break;
            }
            after = rows.last().map(|r| r.seq).unwrap_or(after);
            let events: Vec<Value> = rows.iter().map(|r| serde_json::json!(r)).collect();
            let empty = events.is_empty();
            let _ = self.enqueue_direct(
                MESH_CHANNEL,
                sender,
                &Payload::EventsBatch {
                    session_id: session_id.to_string(),
                    events,
                    done,
                },
            );
            if done || empty {
                break;
            }
        }
    }

    pub(super) fn apply_events_batch(
        &self,
        sender: &str,
        session_id: &str,
        events: &[Value],
    ) -> usize {
        let Ok(Some(row)) = self.store.get_session(session_id) else {
            return 0;
        };
        if row.node_id != sender {
            return 0;
        }
        let mut n = 0;
        for v in events {
            let Ok(e) = serde_json::from_value::<EventRow>(v.clone()) else {
                continue;
            };
            let ne = NewEvent {
                session_id: session_id.to_string(),
                work_item_id: e.work_item_id,
                kind: e.kind.clone(),
                ref_id: e.ref_id.clone(),
                payload: e.payload.clone(),
                at_ms: e.at_ms,
                mono_ms: e.mono_ms,
            };
            if let Ok(Some(seq)) = self.store.append_mirrored_event(sender, e.seq, &ne) {
                n += 1;
                self.bus.publish_untapped(Frame::Event {
                    seq,
                    node_id: sender.to_string(),
                    session_id: session_id.to_string(),
                    kind: e.kind,
                    ref_id: e.ref_id,
                    payload: e.payload,
                    at_ms: e.at_ms,
                });
            }
        }
        n
    }
}
