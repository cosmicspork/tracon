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
            Payload::Hello { node, contract } => self.apply_node(sender, &node, Some(contract)),
            Payload::Node(node) => self.apply_node(sender, &node, None),
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
                if p == Applied::Malformed || r == Applied::Malformed {
                    return Applied::Malformed;
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
            Payload::PolicyReceipt { .. } => Applied::Unhandled("policy_receipt"),
            Payload::CredentialHandoff { .. } => Applied::Unhandled("credential_handoff"),
            // These are handled before the generic mirror path: a hub stores
            // rollups separately and a direct transfer only stages a package.
            Payload::Rollup { .. } => Applied::Unhandled("rollup"),
            Payload::CandidateTransfer { .. } => Applied::Unhandled("candidate_transfer"),
            Payload::Changes {
                channel: c,
                changes,
            } => {
                if c != channel {
                    return Applied::Malformed;
                }
                let results = match self.store.apply_changes(sender, channel, &changes) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(error = %e, "changes not applied");
                        return Applied::Malformed;
                    }
                };
                if results.contains(&tracon_sync::Applied::Impersonation) {
                    return Applied::Impersonation;
                }
                let won: Vec<_> = changes
                    .into_iter()
                    .zip(results.iter())
                    .filter(|(_, r)| **r == tracon_sync::Applied::Stored)
                    .map(|(c, _)| c)
                    .collect();
                if won.is_empty() {
                    return Applied::Duplicate;
                }
                let touched_batches = won
                    .iter()
                    .any(|c| c.table == "promotion" || c.table == "memory");
                self.bus.publish_untapped(Frame::Changes {
                    channel: channel.to_string(),
                    changes: won,
                });
                if touched_batches {
                    crate::corpus::promote::publish(&self.store, &self.bus);
                }
                Applied::Stored
            }
            Payload::ChangesRequest { .. } => Applied::Unhandled("changes_request"),
            Payload::ChangesBatch { .. } => Applied::Unhandled("changes_batch"),
        }
    }

    fn apply_node(&self, sender: &str, node: &Value, hello_contract: Option<u32>) -> Applied {
        let Some(mut row) = NodeRow::from_json(node) else {
            return Applied::Malformed;
        };
        if let Some(contract) = hello_contract {
            row.wire_contract = Some(contract);
        } else if row.wire_contract.is_none() {
            // `Hello.contract` predates node JSON metadata. A later legacy
            // Node frame must not erase that observed wire fact.
            row.wire_contract = self
                .store
                .get_node(sender)
                .ok()
                .flatten()
                .and_then(|known| known.wire_contract);
        }
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
                    match self.store.upsert_review_mirror(&r) {
                        Ok(true) => keep.push(r.id.clone()),
                        // The sender owns the payload row, but it tried to
                        // reuse an ID already bound to another owner/channel.
                        Ok(false) => return Applied::Impersonation,
                        Err(error) => {
                            tracing::warn!(%error, review = %r.id, "mirrored review not stored");
                            return Applied::Malformed;
                        }
                    }
                }
                Ok(_) => return Applied::Impersonation,
                Err(_) => return Applied::Malformed,
            }
        }
        if let Err(error) = self.store.gone_absent_reviews(sender, channel, &keep) {
            tracing::warn!(%error, "mirrored review absence reconciliation failed");
            return Applied::Malformed;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal_session() -> SessionRow {
        SessionRow {
            id: "report-session".into(),
            node_id: "owner".into(),
            channel: "personal".into(),
            work_item_id: None,
            repo_path: "/repo".into(),
            worktree_path: None,
            branch: "report".into(),
            harness_id: crate::session::external::HARNESS_ID.into(),
            harness_version: "external".into(),
            harness_agent: None,
            harness_found: None,
            harness_protocol: None,
            harness_session_id: None,
            container_name: None,
            model: "external".into(),
            project_id: None,
            phase: "done".into(),
            policy_version: None,
            manifest_digest: None,
            review_id: None,
            budget_tokens: 0,
            tokens_used: 0,
            cost_usd: None,
            context_used: None,
            context_size: None,
            state: "closed".into(),
            end_reason: Some("complete".into()),
            last_error: None,
            turn_active: 0,
            draft: None,
            draft_updated_ms: None,
            created_ms: now_ms(),
            started_mono_ms: Some(0),
            ended_mono_ms: Some(1),
            updated_ms: now_ms(),
            archived_ms: None,
            legacy_ms: None,
            parent_session: None,
            continued_from: None,
        }
    }

    fn acknowledged_report() -> ReviewRow {
        ReviewRow {
            id: "report".into(),
            session_id: "report-session".into(),
            node_id: "owner".into(),
            channel: "personal".into(),
            kind: crate::store::reports::KIND.into(),
            title: "Incident summary".into(),
            body: "The narrative body".into(),
            edited_title: None,
            edited_body: None,
            provider: "none".into(),
            target: r#"{"kind":"narrative_report"}"#.into(),
            diff: String::new(),
            files: "[]".into(),
            head_sha: "narrative-version".into(),
            base_ref: "none".into(),
            added: 0,
            removed: 0,
            state: crate::store::reports::ACKNOWLEDGED.into(),
            verdict_reason: Some("received".into()),
            publish_result: None,
            claimed_ms: None,
            created_ms: now_ms(),
            created_mono_ms: 0,
            resolved_mono_ms: Some(now_ms()),
            updated_ms: now_ms(),
            checks_json: None,
            review_session_id: None,
            ai_verdict_json: None,
            revision_patch: None,
        }
    }

    fn owner_with_acknowledged_report() -> Store {
        let owner = Store::open_in_memory().unwrap();
        owner.ensure_peer_node("owner").unwrap();
        owner.node_channel_add("owner", "personal").unwrap();
        owner.insert_session(&terminal_session()).unwrap();
        owner.insert_review(&acknowledged_report()).unwrap();
        owner
    }

    #[test]
    fn snapshot_keeps_acknowledged_report_and_terminal_session_on_peer() {
        let owner = owner_with_acknowledged_report();

        let payload = crate::mesh::frames::snapshots(&owner, "owner")
            .into_iter()
            .find_map(|(channel, payload)| (channel == "personal").then_some(payload))
            .unwrap();

        let peer = Arc::new(Store::open_in_memory().unwrap());
        let mirror = Mirror {
            store: peer.clone(),
            bus: Bus::new(),
            self_id: "peer".into(),
        };
        assert_eq!(mirror.apply("owner", "personal", payload), Applied::Stored);

        assert_eq!(
            peer.get_session("report-session").unwrap().unwrap().state,
            "closed"
        );
        let report = peer.get_review("report").unwrap().unwrap();
        assert_eq!(report.state, crate::store::reports::ACKNOWLEDGED);
        assert_eq!(report.verdict_reason.as_deref(), Some("received"));
        assert!(
            peer.open_reviews().unwrap().is_empty(),
            "terminal reports must not re-enter the operator queue"
        );
    }

    #[test]
    fn review_frame_mirrors_acknowledgement_without_reopening_the_queue() {
        let owner = owner_with_acknowledged_report();
        let peer = Arc::new(Store::open_in_memory().unwrap());
        let mirror = Mirror {
            store: peer.clone(),
            bus: Bus::new(),
            self_id: "peer".into(),
        };
        let session = owner.get_session("report-session").unwrap().unwrap();
        assert_eq!(
            mirror.apply(
                "owner",
                "personal",
                Payload::Session(serde_json::to_value(session).unwrap()),
            ),
            Applied::Stored
        );

        let payload = crate::mesh::frames::to_payloads(
            &Frame::Reviews {
                waiting: Vec::new(),
            },
            &owner,
            "owner",
        )
        .into_iter()
        .find_map(|(channel, payload)| (channel == "personal").then_some(payload))
        .unwrap();
        assert_eq!(mirror.apply("owner", "personal", payload), Applied::Stored);

        assert_eq!(
            peer.get_review("report").unwrap().unwrap().state,
            crate::store::reports::ACKNOWLEDGED
        );
        assert!(peer.open_reviews().unwrap().is_empty());
    }
}
