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
use crate::store::{now_ms, EventRow, NewEvent};
use crate::stream::Frame;

/// What runs a command on the owning node. Implemented by the HTTP layer's
/// state, which has everything a local request would.
#[async_trait::async_trait]
pub trait CommandExecutor: Send + Sync {
    async fn execute(&self, sender: &str, command: Command) -> Result<Value, String>;
}

/// How far the owner's clock may run ahead of the sender's before a command
/// that is still inside its deadline reads as expired. The deadline exists to
/// stop a command running minutes or hours after its sender gave up; holding
/// it to the millisecond across two machines' clocks would refuse fresh ones.
pub const DEADLINE_SKEW_MS: i64 = 30_000;

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("the owner did not answer in time (unreachable, or on an older build)")]
    Timeout,
    /// No answer from a peer that last announced an older build than this
    /// one. A build that predates a command drops it unread, so this is the
    /// likeliest reason, though not a certain one.
    #[error(
        "the owner did not answer in time and runs an older build ({peer}) than this node \
         ({local}); it may not understand this command, so upgrade it"
    )]
    OlderPeer { peer: String, local: String },
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Local(String),
}

impl MeshClient {
    /// Queue a command for `node_id` without waiting for its answer. It
    /// carries no deadline: the operator asked for it to wait in the outbox
    /// until the owner returns.
    pub fn send_command(&self, node_id: &str, command: Command) -> Result<String, CommandError> {
        let cmd_id = uuid::Uuid::now_v7().to_string();
        self.enqueue_direct(
            MESH_CHANNEL,
            node_id,
            &Payload::Command {
                cmd_id: cmd_id.clone(),
                command,
                not_after_ms: None,
            },
        )
        .map_err(|e| CommandError::Local(e.to_string()))?;
        Ok(cmd_id)
    }

    /// Send a command and wait for the owner's ack. The owner refuses it once
    /// `timeout` has passed, so a command this node reported as failed never
    /// runs later from the outbox.
    pub async fn command(
        &self,
        node_id: &str,
        command: Command,
        timeout: Duration,
    ) -> Result<Value, CommandError> {
        let deadline = now_ms().saturating_add(timeout.as_millis().min(i64::MAX as u128) as i64);
        self.command_until(node_id, command, timeout, Some(deadline))
            .await
    }

    /// As [`command`](Self::command), with the deadline the owner holds it
    /// to named outright. `None` is what a sender on an older build sends.
    #[doc(hidden)]
    pub async fn command_until(
        &self,
        node_id: &str,
        command: Command,
        timeout: Duration,
        not_after_ms: Option<i64>,
    ) -> Result<Value, CommandError> {
        self.await_ack(node_id, timeout, |cmd_id| {
            serde_json::to_vec(&Payload::Command {
                cmd_id,
                command,
                not_after_ms,
            })
            .map_err(|e| e.to_string())
        })
        .await
    }

    /// Send a command this build's [`Command`] cannot express, as a newer
    /// build would, and wait for the answer.
    #[doc(hidden)]
    pub async fn command_json(
        &self,
        node_id: &str,
        command: Value,
        timeout: Duration,
    ) -> Result<Value, CommandError> {
        self.await_ack(node_id, timeout, |cmd_id| {
            serde_json::to_vec(&serde_json::json!({
                "kind": "command",
                "cmd_id": cmd_id,
                "command": command,
            }))
            .map_err(|e| e.to_string())
        })
        .await
    }

    async fn await_ack(
        &self,
        node_id: &str,
        timeout: Duration,
        payload: impl FnOnce(String) -> Result<Vec<u8>, String>,
    ) -> Result<Value, CommandError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cmd_id = uuid::Uuid::now_v7().to_string();
        self.pending.lock().unwrap().insert(cmd_id.clone(), tx);
        let queued = payload(cmd_id.clone()).and_then(|plaintext| {
            self.enqueue_direct_plaintext(MESH_CHANNEL, node_id, &plaintext)
                .map_err(|e| e.to_string())
        });
        if let Err(e) = queued {
            self.pending.lock().unwrap().remove(&cmd_id);
            return Err(CommandError::Local(e));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(m))) => Err(CommandError::Refused(m)),
            Ok(Err(_)) => Err(CommandError::Local("client shut down".into())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&cmd_id);
                Err(self.timed_out(node_id))
            }
        }
    }

    /// What to say when a peer never answered: plain silence, unless the
    /// peer last announced an older build, which drops a command it cannot
    /// read.
    fn timed_out(&self, node_id: &str) -> CommandError {
        let local = env!("CARGO_PKG_VERSION");
        match self
            .store
            .get_node(node_id)
            .ok()
            .flatten()
            .and_then(|row| row.app_version)
        {
            Some(peer) if older_build(&peer, local) => CommandError::OlderPeer {
                peer,
                local: local.to_string(),
            },
            _ => CommandError::Timeout,
        }
    }

    /// Execute a command a peer sent us and ack it. Runs off the pull path so a
    /// slow command does not stall ingestion. A command past its sender's
    /// deadline is refused unrun: the sender has already reported it failed.
    pub(super) fn run_command(
        &self,
        sender: String,
        cmd_id: String,
        command: Command,
        not_after_ms: Option<i64>,
    ) {
        let Some(this) = self.weak.upgrade() else {
            return;
        };
        tokio::spawn(async move {
            let late_ms = not_after_ms.map(|deadline| now_ms().saturating_sub(deadline));
            let result = match (this.executor.get(), late_ms) {
                (_, Some(late)) if late > DEADLINE_SKEW_MS => {
                    let refusal = format!(
                        "this command expired {}s ago, after its sender stopped waiting; it was not run",
                        late / 1000
                    );
                    tracing::warn!(from = %sender, cmd = %cmd_id, late_ms = late, "expired mesh command refused");
                    this.note_refusal(format!("command from {sender}: {refusal}"));
                    Err(refusal)
                }
                (Some(executor), _) => executor.execute(&sender, command).await,
                (None, _) => Err("this node is not ready to run commands".into()),
            };
            this.ack(&sender, cmd_id, result);
        });
    }

    /// Answer a command whose payload this build could not parse: a newer
    /// sender's op or field shape. Without this the frame is dropped and the
    /// sender waits out its timeout with no reason.
    pub(super) fn refuse_unreadable_command(&self, sender: &str, payload: &Value, error: &str) {
        let Some(cmd_id) = payload["cmd_id"].as_str() else {
            return;
        };
        let op = payload["command"]["op"].as_str().unwrap_or("unknown");
        let refusal = format!(
            "the owner runs an older build ({}) that cannot read this `{op}` command ({error}); \
             upgrade it",
            env!("CARGO_PKG_VERSION")
        );
        tracing::warn!(from = %sender, op, error, "unreadable mesh command refused");
        self.note_refusal(format!("command from {sender}: {refusal}"));
        self.ack(sender, cmd_id.to_string(), Err(refusal));
    }

    fn ack(&self, to: &str, cmd_id: String, result: Result<Value, String>) {
        let (ok, err) = match result {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        if let Err(e) = self.enqueue_direct(MESH_CHANNEL, to, &Payload::Ack { cmd_id, ok, err }) {
            tracing::warn!(to = %to, error = %e, "could not queue an ack");
        }
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

/// Whether `peer` names an earlier release than `local`. Only plain
/// `major.minor.patch` versions compare; anything else is not called older,
/// so an unreadable version never blames the peer's build.
fn older_build(peer: &str, local: &str) -> bool {
    fn parts(v: &str) -> Option<(u64, u64, u64)> {
        let core = v.trim().trim_start_matches('v');
        let core = core.split(['-', '+']).next()?;
        let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
        let parsed = (it.next()??, it.next()??, it.next()??);
        it.next().is_none().then_some(parsed)
    }
    matches!((parts(peer), parts(local)), (Some(p), Some(l)) if p < l)
}

#[cfg(test)]
mod tests {
    use super::older_build;

    #[test]
    fn only_a_readable_earlier_release_is_older() {
        assert!(older_build("0.31.4", "0.32.0"));
        assert!(older_build("v0.9.9", "0.10.0"));
        assert!(!older_build("0.32.0", "0.32.0"));
        assert!(!older_build("0.33.0", "0.32.0"));
        assert!(!older_build("dev", "0.32.0"));
        assert!(!older_build("", "0.32.0"));
        assert!(!older_build("0.32", "0.32.0"));
    }
}
