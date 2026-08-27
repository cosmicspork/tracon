//! What this node says on the mesh: the bus frames it originates, converted
//! to payloads addressed to channels. Rows already carry their `node_id`, so
//! a peer can check that a node only ever speaks for itself.

use proto::frame::{Payload, MESH_CHANNEL};
use serde_json::{json, Value};

use crate::store::{PermissionRow, ReviewRow, Store};
use crate::stream::Frame;

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
        // Live chunks and tool progress are not forwarded in this phase: the
        // remote view is message-granular. Mesh state is local by definition.
        Frame::Chunk { .. } | Frame::ToolUpdate { .. } | Frame::Mesh(_) => Vec::new(),
        _ => Vec::new(),
    }
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
