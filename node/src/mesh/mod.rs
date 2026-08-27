//! The node's side of the mesh: its identity, the hub client, the mirror of
//! peer state, and (later) command forwarding.

pub mod client;
pub mod enroll;
pub mod forward;
pub mod frames;
pub mod identity;
pub mod mirror;

use serde::Serialize;

/// Hub reachability and counters, as the interface shows them.
#[derive(Clone, Debug, Serialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct MeshState {
    pub hub: HubState,
    pub hub_url: Option<String>,
    pub node_id: String,
    pub fingerprint: Option<String>,
    pub last_ok_ms: Option<i64>,
    /// Frames waiting in the outbox.
    pub queued: usize,
    /// Frames delivered since the hub last became reachable.
    pub delivered_since_reconnect: usize,
    /// Frames that arrived but could not be opened (unknown channel or epoch).
    pub undecryptable: u64,
    pub last_error: Option<String>,
    pub last_refusal: Option<String>,
}

#[derive(Clone, Debug, Serialize, Default, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HubState {
    #[default]
    Disabled,
    Connected,
    Unreachable {
        since_ms: i64,
    },
}
