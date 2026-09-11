//! What this node says on the mesh: the bus frames it originates, converted
//! to payloads addressed to channels. Rows already carry their `node_id`, so
//! a peer can check that a node only ever speaks for itself.

use proto::frame::{Payload, MESH_CHANNEL};
use serde_json::{json, Value};

use crate::store::{PermissionRow, ReviewRow, Store};
use crate::stream::Frame;

/// Changes per frame, well under the 4 MiB frame cap for ordinary rows.
pub const CHANGES_PER_FRAME: usize = 200;

/// The `(channel, payload)` pairs a locally published frame becomes. Frames
/// about other nodes' sessions (mirrored state) yield nothing; the bus only
/// taps what this node publishes, but the filter is repeated here so a bug
/// upstream cannot turn this node into a relay of a relay.
pub fn to_payloads(frame: &Frame, store: &Store, self_id: &str) -> Vec<(String, Payload)> {
    match frame {
        Frame::Event {
            seq,
            node_id,
            session_id,
            kind,
            ref_id,
            payload,
            at_ms,
        } if node_id == self_id => {
            let Some(channel) = channel_of(store, session_id) else {
                return Vec::new();
            };
            vec![(
                channel,
                Payload::Event {
                    origin_seq: *seq,
                    session_id: session_id.clone(),
                    event_kind: kind.clone(),
                    ref_id: ref_id.clone(),
                    payload: payload.clone(),
                    at_ms: *at_ms,
                },
            )]
        }
        Frame::Session(row) if row.node_id == self_id => {
            vec![(row.channel.clone(), Payload::Session(json!(row)))]
        }
        Frame::Queue { waiting } => {
            // One payload per member channel, possibly empty: an empty list is
            // how a peer learns a request it mirrored has been answered.
            grouped(store, self_id, waiting, |p: &PermissionRow| &p.session_id)
                .into_iter()
                .map(|(c, rows)| (c, Payload::Queue { waiting: rows }))
                .collect()
        }
        Frame::Reviews { waiting } => {
            let mine: Vec<&ReviewRow> = waiting.iter().filter(|r| r.node_id == self_id).collect();
            let mut out = Vec::new();
            for c in store.node_channels(self_id).unwrap_or_default() {
                let rows: Vec<Value> = mine
                    .iter()
                    .filter(|r| r.channel == c)
                    .map(|r| json!(r))
                    .collect();
                out.push((c, Payload::Reviews { waiting: rows }));
            }
            out
        }
        Frame::Node(v) => vec![(MESH_CHANNEL.to_string(), Payload::Node(v.clone()))],
        // Only this site's own changes leave here: a mirrored change that won
        // locally is republished untapped, and even if it were not, a change
        // stamped by another site must not be relayed as ours.
        Frame::Changes { channel, changes } if changes.iter().all(|c| c.site == self_id) => {
            let mut out = Vec::new();
            let mut ordinary = Vec::with_capacity(CHANGES_PER_FRAME);
            for change in changes {
                if change.table == "document_bundle_chunk" {
                    push_change_batch(&mut out, channel, &mut ordinary);
                    out.push((
                        channel.clone(),
                        Payload::Changes {
                            channel: channel.clone(),
                            changes: vec![change.clone()],
                        },
                    ));
                } else {
                    ordinary.push(change.clone());
                    if ordinary.len() == CHANGES_PER_FRAME {
                        push_change_batch(&mut out, channel, &mut ordinary);
                    }
                }
            }
            push_change_batch(&mut out, channel, &mut ordinary);
            out
        }
        // Live chunks and tool progress are not forwarded in this phase: the
        // remote view is message-granular. Mesh state is local by definition.
        Frame::Chunk { .. } | Frame::ToolUpdate { .. } | Frame::Mesh(_) => Vec::new(),
        _ => Vec::new(),
    }
}

fn push_change_batch(
    out: &mut Vec<(String, Payload)>,
    channel: &str,
    changes: &mut Vec<proto::frame::Change>,
) {
    if changes.is_empty() {
        return;
    }
    out.push((
        channel.to_string(),
        Payload::Changes {
            channel: channel.to_string(),
            changes: std::mem::take(changes),
        },
    ));
}

/// This node's full open state per member channel, for peers that connect
/// late or resync after falling behind retention.
pub fn snapshots(store: &Store, self_id: &str) -> Vec<(String, Payload)> {
    let sessions = store.sessions_of_node(self_id).unwrap_or_default();
    let waiting = store.open_permissions().unwrap_or_default();
    let reviews = store.open_reviews().unwrap_or_default();
    let mut out = Vec::new();
    for c in store.node_channels(self_id).unwrap_or_default() {
        let s: Vec<Value> = sessions
            .iter()
            .filter(|s| {
                s.channel == c
                    && !crate::session::state::SessionState::from_stored(&s.state).is_terminal()
            })
            .map(|s| json!(s))
            .collect();
        let session_ids: Vec<&str> = sessions
            .iter()
            .filter(|s| s.channel == c)
            .map(|s| s.id.as_str())
            .collect();
        let w: Vec<Value> = waiting
            .iter()
            .filter(|p| p.node_id == self_id && session_ids.contains(&p.session_id.as_str()))
            .map(|p| json!(p))
            .collect();
        let r: Vec<Value> = reviews
            .iter()
            .filter(|r| r.node_id == self_id && r.channel == c)
            .map(|r| json!(r))
            .collect();
        out.push((
            c,
            Payload::Snapshot {
                sessions: s,
                waiting: w,
                reviews: r,
            },
        ));
    }
    out
}

/// Summaries are opt-in twice: the channel binding records that its key was
/// handed to the hub, and the payload is still channel-sealed. A local node
/// never asks the hub to summarize an ordinary member channel.
pub fn rollups(store: &Store, self_id: &str) -> Vec<(String, Payload)> {
    store
        .channel_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|channel| {
            serde_json::from_str::<Value>(&channel.bindings_json)
                .ok()
                .and_then(|bindings| bindings["processing"].as_str().map(str::to_string))
                .as_deref()
                == Some("hub")
        })
        .filter_map(|channel| {
            store
                .next_rollup(self_id, &channel.name)
                .ok()
                .map(|rollup| {
                    (
                        channel.name.clone(),
                        Payload::Rollup {
                            channel: channel.name,
                            rollup,
                        },
                    )
                })
        })
        .collect()
}

fn channel_of(store: &Store, session_id: &str) -> Option<String> {
    store
        .get_session(session_id)
        .ok()
        .flatten()
        .map(|s| s.channel)
}

fn grouped<'a, T: serde::Serialize>(
    store: &Store,
    self_id: &str,
    rows: &'a [T],
    session_of: impl Fn(&'a T) -> &'a String,
) -> Vec<(String, Vec<Value>)> {
    let mut out: Vec<(String, Vec<Value>)> = store
        .node_channels(self_id)
        .unwrap_or_default()
        .into_iter()
        .map(|c| (c, Vec::new()))
        .collect();
    for row in rows {
        let Some(c) = channel_of(store, session_of(row)) else {
            continue;
        };
        if let Some(slot) = out.iter_mut().find(|(name, _)| *name == c) {
            slot.1.push(json!(row));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::frame::{Change, ChangeOp};

    fn change(table: &str, id: &str) -> Change {
        Change {
            table: table.to_string(),
            op: ChangeOp::Upsert,
            id: id.to_string(),
            site: "self".to_string(),
            site_seq: 1,
            hlc_ms: 1,
            hlc_ctr: 0,
            row: json!({}),
        }
    }

    #[test]
    fn bundle_chunks_are_isolated_without_reordering_changes() {
        let store = Store::open_in_memory().unwrap();
        let frame = Frame::Changes {
            channel: "personal".to_string(),
            changes: vec![
                change("document_bundle_file", "file"),
                change("document_bundle_chunk", "chunk"),
                change("document", "document"),
            ],
        };
        let payloads = to_payloads(&frame, &store, "self");
        let ids: Vec<Vec<&str>> = payloads
            .iter()
            .map(|(_, payload)| match payload {
                Payload::Changes { changes, .. } => {
                    changes.iter().map(|change| change.id.as_str()).collect()
                }
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids, vec![vec!["file"], vec!["chunk"], vec!["document"]]);
    }
    #[test]
    fn encrypted_bundle_chunk_payload_stays_below_frame_limit() {
        use base64::Engine;
        use proto::{
            envelope::DataKey,
            frame::{Envelope, MAX_FRAME_BYTES},
            keyring::Keyring,
            keys::Identity,
        };

        let store = Store::open_in_memory().unwrap();
        let identity = Identity::from_seed(&[7u8; 32]);
        let self_id = identity.node_id();
        let ring = Keyring::genesis(&identity.x25519_public(), &DataKey::generate());
        let mut chunk = change("document_bundle_chunk", "chunk");
        chunk.site = self_id.clone();
        chunk.row = json!({
            "channel": "personal",
            "bytes_b64": base64::engine::general_purpose::STANDARD
                .encode(vec![0xff; crate::corpus::html::CHUNK_BYTES]),
        });
        let frame = Frame::Changes {
            channel: "personal".into(),
            changes: vec![chunk],
        };
        let payloads = to_payloads(&frame, &store, &self_id);
        assert_eq!(payloads.len(), 1);
        for (channel, payload) in payloads {
            let envelope =
                Envelope::seal_channel(&identity, &channel, None, &ring, &payload, 1).unwrap();
            let size = serde_json::to_vec(&envelope).unwrap().len();
            assert!(size < MAX_FRAME_BYTES, "{size} >= {MAX_FRAME_BYTES}");
        }
    }
}
