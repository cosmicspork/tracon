//! Applying what peers say to the local store. Rows land in the same tables
//! as local ones, scoped by `node_id`, and are published to the bus untapped
//! so they reach the interface but never echo back onto the mesh.
//!
//! The one rule enforced everywhere: **a node speaks only for itself.** A
//! payload whose rows name another `node_id` than the verified sender is
//! dropped, so a compromised peer cannot rewrite a third node's state.

use std::sync::Arc;

use proto::frame::Payload;
use serde_json::Value;

use crate::store::{now_ms, NewEvent, NodeRow, PermissionRow, ReviewRow, SessionRow, Store};
use crate::stream::{Bus, Frame};

pub struct Mirror {
    pub store: Arc<Store>,
    pub bus: Bus,
    pub self_id: String,
}

/// What applying a payload did, for counters and logs.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    Stored,
    Duplicate,
    /// Rows named a node other than the sender.
    Impersonation,
    /// A payload kind this module does not handle (commands, handoffs).
    Unhandled(&'static str),
    Malformed,
}

impl Mirror {
    pub fn apply(&self, sender: &str, channel: &str, payload: Payload) -> Applied {
        match payload {
            Payload::Hello { node, .. } | Payload::Node(node) => self.apply_node(sender, &node),
            Payload::Session(v) => match serde_json::from_value::<SessionRow>(v) {
                Ok(row) if row.node_id == sender => {
                    let _ = self.store.ensure_peer_node(sender);
                    if let Err(e) = self.store.upsert_session_mirror(&row) {
                        tracing::warn!(error = %e, "mirrored session not stored");
                        return Applied::Malformed;
                    }
                    self.bus.publish_untapped(Frame::Session(Box::new(row)));
                    Applied::Stored
                }
                Ok(_) => Applied::Impersonation,
                Err(_) => Applied::Malformed,
            },
            Payload::Event {
                origin_seq,
                session_id,
                event_kind,
                ref_id,
                payload,
                at_ms,
            } => {
                // The session must be the sender's; an event for a session we
                // have never seen is kept (its row may arrive out of order) but
                // only if nothing says it belongs to someone else.
                if let Ok(Some(s)) = self.store.get_session(&session_id) {
                    if s.node_id != sender {
                        return Applied::Impersonation;
                    }
                } else {
                    return Applied::Malformed;
                }
                let e = NewEvent {
                    session_id: session_id.clone(),
                    work_item_id: None,
                    kind: event_kind.clone(),
                    ref_id: ref_id.clone(),
                    payload: payload.clone(),
                    at_ms,
                    mono_ms: 0,
                };
                match self.store.append_mirrored_event(sender, origin_seq, &e) {
                    Err(e) => {
                        tracing::warn!(error = %e, "mirrored event not stored");
                        Applied::Malformed
                    }
                    Ok(Some(seq)) => {
                        self.bus.publish_untapped(Frame::Event {
                            seq,
                            node_id: sender.to_string(),
                            session_id,
                            kind: event_kind,
                            ref_id,
                            payload,
                            at_ms,
                        });
                        Applied::Stored
                    }
                    Ok(None) => Applied::Duplicate,
                }
            }
            Payload::Queue { waiting } => {
                let r = self.apply_permissions(sender, channel, &waiting);
                if r == Applied::Stored {
                    self.publish_queue();
                }
                r
            }
            Payload::Reviews { waiting } => {
                let r = self.apply_reviews(sender, channel, &waiting);
                if r == Applied::Stored {
                    self.publish_queue();
                }
                r
            }
            Payload::Snapshot {
                sessions,
                waiting,
                reviews,
            } => {
                let mut keep = Vec::new();
                let _ = self.store.ensure_peer_node(sender);
                for v in &sessions {
                    match serde_json::from_value::<SessionRow>(v.clone()) {
                        Ok(row) if row.node_id == sender => {
                            keep.push(row.id.clone());
                            let _ = self.store.upsert_session_mirror(&row);
                            self.bus.publish_untapped(Frame::Session(Box::new(row)));
                        }
                        Ok(_) => return Applied::Impersonation,
                        Err(_) => return Applied::Malformed,
                    }
                }
                // A snapshot is the owner's whole open state on this channel;
                // anything we still hold open for it that is missing was lost
                // on the owner.
                for id in self
                    .store
                    .close_absent_sessions(sender, channel, &keep)
                    .unwrap_or_default()
                {
                    if let Ok(Some(row)) = self.store.get_session(&id) {
                        self.bus.publish_untapped(Frame::Session(Box::new(row)));
                    }
                }
                let p = self.apply_permissions(sender, channel, &waiting);
                let r = self.apply_reviews(sender, channel, &reviews);
                if p == Applied::Impersonation || r == Applied::Impersonation {
                    return Applied::Impersonation;
                }
                self.publish_queue();
                Applied::Stored
            }
            Payload::Command { .. } => Applied::Unhandled("command"),
            Payload::Ack { .. } => Applied::Unhandled("ack"),
            Payload::EventsRequest { .. } => Applied::Unhandled("events_request"),
            Payload::EventsBatch { .. } => Applied::Unhandled("events_batch"),
            Payload::KeyHandoff { .. } => Applied::Unhandled("key_handoff"),
            Payload::PolicyBundle { .. } => Applied::Unhandled("policy_bundle"),
            Payload::CredentialHandoff { .. } => Applied::Unhandled("credential_handoff"),
        }
    }

    fn apply_node(&self, sender: &str, node: &Value) -> Applied {
        let Some(mut row) = NodeRow::from_json(node) else {
            return Applied::Malformed;
        };
        if row.id != sender {
            return Applied::Impersonation;
        }
        if row.id == self.self_id {
            // Our own presence echoed back; nothing to learn.
            return Applied::Duplicate;
        }
        row.is_self = 0;
        row.reachable = 1;
        row.last_seen_ms = Some(now_ms());
        if self.store.put_node(&row).is_err() {
            return Applied::Malformed;
        }
        self.bus.publish_untapped(Frame::Node(row.to_json()));
        Applied::Stored
    }

    fn apply_permissions(&self, sender: &str, channel: &str, rows: &[Value]) -> Applied {
        let mut keep = Vec::new();
        for v in rows {
            match serde_json::from_value::<PermissionRow>(v.clone()) {
                Ok(p) if p.node_id == sender => {
                    keep.push(p.id.clone());
                    let _ = self.store.upsert_permission_mirror(&p);
                }
                Ok(_) => return Applied::Impersonation,
                Err(_) => return Applied::Malformed,
            }
        }
        let _ = self.store.expire_absent_permissions(sender, channel, &keep);
        Applied::Stored
    }

    fn apply_reviews(&self, sender: &str, channel: &str, rows: &[Value]) -> Applied {
        let mut keep = Vec::new();
        for v in rows {
            match serde_json::from_value::<ReviewRow>(v.clone()) {
                Ok(r) if r.node_id == sender && r.channel == channel => {
                    keep.push(r.id.clone());
                    let _ = self.store.upsert_review_mirror(&r);
                }
                Ok(_) => return Applied::Impersonation,
                Err(_) => return Applied::Malformed,
            }
        }
        let _ = self.store.gone_absent_reviews(sender, channel, &keep);
        Applied::Stored
    }

    fn publish_queue(&self) {
        if let Ok(waiting) = self.store.open_permissions() {
            self.bus.publish_untapped(Frame::Queue { waiting });
        }
        if let Ok(reviews) = self.store.open_reviews() {
            self.bus
                .publish_untapped(Frame::Reviews { waiting: reviews });
        }
    }
}
