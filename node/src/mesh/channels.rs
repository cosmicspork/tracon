//! Creating a channel: minting its key here, and telling the hub this node
//! holds it.
//!
//! One place, because the CLI, `mesh init`, and the interface all do it and a
//! channel that exists in the store but not in the hub's record cannot be
//! handed to another node by an invitation.

use crate::store::Store;

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("channel names are lowercase [a-z0-9@._-], at most 64 characters")]
    Name,
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}

/// What creating a channel did, so a caller can say so without asking again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Created {
    /// False when the channel was already here: creation is idempotent.
    pub minted: bool,
}

/// Mint a genesis keyring for `name` wrapped to this node, unless one exists,
/// and record that this node holds it. Idempotent.
pub fn create(
    store: &Store,
    id: &proto::keys::Identity,
    name: &str,
) -> Result<Created, ChannelError> {
    if !proto::frame::valid_channel(name) {
        return Err(ChannelError::Name);
    }
    let minted = if store.channel_get(name)?.is_some() {
        false
    } else {
        let ring = proto::keyring::Keyring::genesis(
            &id.x25519_public(),
            &proto::envelope::DataKey::generate(),
        );
        store.channel_put(name, &ring.to_bytes(), "{}")?;
        true
    };
    store.node_channel_add(&id.node_id(), name)?;
    Ok(Created { minted })
}

/// Create, then tell the hub this node holds the channel. The hub record is
/// what lets an invitation hand the key on, so a failure there is worth
/// reporting — but it is not a failure to create, and the next invite syncs
/// it anyway.
pub async fn create_and_sync(
    store: &Store,
    id: &proto::keys::Identity,
    name: &str,
    cfg: &crate::config::Config,
) -> Result<(Created, Option<String>), ChannelError> {
    let created = create(store, id, name)?;
    let Some(hub) = &cfg.mesh.hub_url else {
        return Ok((created, None));
    };
    let note = match crate::mesh::enroll::sync_own_channels(store, id, hub, &cfg.node_name).await {
        Ok(_) => None,
        Err(e) => Some(format!(
            "hub record not updated ({e}); it is synced on the next invite"
        )),
    };
    Ok((created, note))
}
