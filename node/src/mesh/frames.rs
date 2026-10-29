//! What this node says on the mesh: the bus frames it originates, converted
//! to payloads addressed to channels. Rows already carry their `node_id`, so
//! a peer can check that a node only ever speaks for itself.

use proto::frame::{Payload, MESH_CHANNEL};
use serde_json::{json, Value};

use crate::store::{OperatorQuestionRow, PermissionView, ReviewRow, Store};
use crate::stream::Frame;

/// Changes per frame, well under the 4 MiB frame cap for ordinary rows.
pub const CHANGES_PER_FRAME: usize = 200;

/// How far back a listing names what became of this node's decided code
/// reviews: long enough for a peer that slept through a decision, or was
/// away when it could have asked, to learn it from the next listing.
const DECIDED_LISTED_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// At most this many decided outcomes per channel in one listing, the most
/// recent, so a busy week cannot push a listing toward the frame cap.
const DECIDED_LISTED_MAX: usize = 200;

/// What became of this node's code reviews on `channel` decided lately, for
/// a listing.
fn decided(store: &Store, self_id: &str, channel: &str) -> Vec<Value> {
    let since = crate::store::now_ms() - DECIDED_LISTED_MS;
    store
        .decided_review_outcomes(self_id, channel, since, DECIDED_LISTED_MAX)
        .unwrap_or_default()
        .into_iter()
        .map(|o| json!(o))
        .collect()
}

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
            // how a peer learns a request it mirrored has been answered. A
            // peer's request mirrored here is never relayed as ours.
            let mine: Vec<&PermissionView> =
                waiting.iter().filter(|p| p.node_id == self_id).collect();
            grouped(store, self_id, &mine, |p: &&PermissionView| {
                match p.request.session_id.as_deref() {
                    Some(id) => channel_of(store, id),
                    // A session-less caller's card carries its channel.
                    None => p.intent.channel.clone(),
                }
            })
            .into_iter()
            .map(|(c, rows)| (c, Payload::Queue { waiting: rows }))
            .collect()
        }
        Frame::Questions { waiting } => {
            // As the queue: one list per member channel, possibly empty. A
            // question with no session is answered where it was asked, as a
            // session-less permission card is, and a peer's question mirrored
            // here is never relayed as ours.
            let mine: Vec<&OperatorQuestionRow> = waiting
                .iter()
                .filter(|q| q.node_id == self_id && q.session_id.is_some())
                .collect();
            grouped(store, self_id, &mine, |q| Some(q.channel.clone()))
                .into_iter()
                .map(|(c, rows)| (c, Payload::Questions { waiting: rows }))
                .collect()
        }
        Frame::Reviews { .. } => {
            // The UI frame only lists waiting reviews. The mesh additionally
            // carries acknowledged narrative reports, so a peer learns durable
            // receipt without showing terminal reports in its operator queue.
            let Ok(replicated) = store.mirrored_reviews() else {
                return Vec::new();
            };
            let mine: Vec<&ReviewRow> =
                replicated.iter().filter(|r| r.node_id == self_id).collect();
            let mut out = Vec::new();
            for c in store.node_channels_keyed(self_id).unwrap_or_default() {
                let rows: Vec<Value> = mine
                    .iter()
                    .filter(|r| r.channel == c)
                    .map(|r| json!(r))
                    .collect();
                let decided = decided(store, self_id, &c);
                out.push((
                    c,
                    Payload::Reviews {
                        waiting: rows,
                        decided,
                    },
                ));
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
        Frame::Chunk { .. }
        | Frame::ToolUpdate { .. }
        | Frame::Mesh(_)
        | Frame::Awake(_)
        | Frame::Restart(_) => Vec::new(),
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

/// This node's full mirrored review state and open session state (with its
/// open questions) per member channel, for peers that connect late or resync after falling behind
/// retention. A terminal session accompanies any mirrored review that still
/// references it, preserving the review row's foreign-key prerequisite without
/// reopening that session.
pub fn snapshots(store: &Store, self_id: &str) -> Vec<(String, Payload)> {
    let sessions = store.sessions_of_node(self_id).unwrap_or_default();
    let waiting = store.open_permission_views().unwrap_or_default();
    let reviews = store.mirrored_reviews().unwrap_or_default();
    let questions = store.open_operator_questions().unwrap_or_default();
    let mut out = Vec::new();
    for c in store.node_channels_keyed(self_id).unwrap_or_default() {
        let r: Vec<&ReviewRow> = reviews
            .iter()
            .filter(|r| r.node_id == self_id && r.channel == c)
            .collect();
        let review_session_ids: Vec<&str> = r
            .iter()
            .filter_map(|review| review.session_id.as_deref())
            .collect();
        let s: Vec<Value> = sessions
            .iter()
            .filter(|s| {
                s.channel == c
                    && (!crate::session::state::SessionState::from_stored(&s.state).is_terminal()
                        || review_session_ids.contains(&s.id.as_str()))
            })
            .map(|s| json!(s))
            .collect();
        // Every card the queue lists, approvals and a session-less caller's
        // among them: a peer expires what a snapshot leaves out.
        let w: Vec<Value> = waiting
            .iter()
            .filter(|p| p.node_id == self_id && p.intent.channel.as_deref() == Some(c.as_str()))
            .map(|p| json!(p))
            .collect();
        let q: Vec<Value> = questions
            .iter()
            .filter(|q| q.node_id == self_id && q.channel == c && q.session_id.is_some())
            .map(|q| json!(q))
            .collect();
        let d = decided(store, self_id, &c);
        out.push((
            c,
            Payload::Snapshot {
                sessions: s,
                waiting: w,
                reviews: r.into_iter().map(|review| json!(review)).collect(),
                questions: q,
                decided: d,
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

fn grouped<T: serde::Serialize>(
    store: &Store,
    self_id: &str,
    rows: &[T],
    channel_of_row: impl Fn(&T) -> Option<String>,
) -> Vec<(String, Vec<Value>)> {
    let mut out: Vec<(String, Vec<Value>)> = store
        .node_channels_keyed(self_id)
        .unwrap_or_default()
        .into_iter()
        .map(|c| (c, Vec::new()))
        .collect();
    for row in rows {
        let Some(c) = channel_of_row(row) else {
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
