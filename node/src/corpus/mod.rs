//! The node's corpus: documents and memories written through the sync layer
//! and published to the mesh, project identity, and the orientation a session
//! starts with. Reads are always local; see `store::corpus`.

pub mod import;
pub mod orientation;
pub mod project;

use serde_json::Value;
use tracon_sync::{Change, ChangeOp};

use crate::store::{Store, StoreError};
use crate::stream::{Bus, Frame};

/// A local write: stamped and logged in the store, then published so the
/// mesh client seals it onto the record's channel. The publish is deliberate
/// after the commit, so the outbox never carries a change the store lacks.
#[allow(clippy::too_many_arguments)]
pub fn write(
    store: &Store,
    bus: &Bus,
    site: &str,
    channel: &str,
    table: &str,
    op: ChangeOp,
    id: &str,
    row: Value,
) -> Result<Change, StoreError> {
    let change = store.write_change(site, channel, table, op, id, row)?;
    bus.publish(Frame::Changes {
        channel: channel.to_string(),
        changes: vec![change.clone()],
    });
    Ok(change)
}

/// A fresh record id: time-ordered so a table scan reads in creation order.
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// SHA-256 of a body, hex: the `If-Match` value for document edits.
pub fn hash_body(body: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(body.as_bytes()))
}
