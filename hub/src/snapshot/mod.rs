//! Encrypted snapshots of the hub's data directory to object storage, and
//! their restore. The hub can seal and never unseal: each snapshot's data key
//! is wrapped to an operator-held restore key whose public half is all the
//! hub keeps. The identity seed is inside the snapshot on purpose — the
//! replica's keyrings are wrapped to it, so a restore without it would be
//! ciphertext.

pub mod format;
pub mod objects;
pub mod s3;

use std::path::{Path, PathBuf};

use proto::keys::Identity;

pub use objects::{FsObjects, ObjectStore};

pub const RECIPIENT_FILE: &str = "snapshot-recipient.pub";

/// Generate the restore key: prints the seed once, keeps only the public
/// half beside the data.
pub fn create_restore_key(data_dir: &Path) -> std::io::Result<(String, PathBuf)> {
    let (seed, identity) = Identity::generate();
    let path = data_dir.join(RECIPIENT_FILE);
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(&path, identity.x25519_hex())?;
    Ok((hex::encode(seed), path))
}

/// The recipient public key the hub seals to, from the file or the
/// environment (`TRACON_HUB_SNAPSHOT_PUBKEY`).
pub fn recipient(data_dir: &Path) -> Option<x25519_dalek::PublicKey> {
    let hex_key = std::env::var("TRACON_HUB_SNAPSHOT_PUBKEY")
        .ok()
        .or_else(|| std::fs::read_to_string(data_dir.join(RECIPIENT_FILE)).ok())?;
    proto::keys::key32(hex_key.trim()).map(x25519_dalek::PublicKey::from)
}

/// Snapshot the data directory to `store` under `prefix`. Returns the object
/// key written.
pub fn take(
    data_dir: &Path,
    recipient: &x25519_dalek::PublicKey,
    store: &dyn ObjectStore,
    prefix: &str,
    now_ms: i64,
) -> std::io::Result<String> {
    let bytes = format::pack(data_dir, recipient)?;
    let key = format!("{}snapshot-{now_ms}.tracon-snap", prefix_slash(prefix));
    store.put(&key, &bytes)?;
    Ok(key)
}

/// Keep the newest `keep` snapshots under `prefix`; returns keys removed.
pub fn prune(store: &dyn ObjectStore, prefix: &str, keep: usize) -> std::io::Result<Vec<String>> {
    let mut keys: Vec<String> = store
        .list(&prefix_slash(prefix))?
        .into_iter()
        .filter(|k| k.ends_with(".tracon-snap"))
        .collect();
    keys.sort();
    let mut removed = Vec::new();
    while keys.len() > keep {
        let k = keys.remove(0);
        store.delete(&k)?;
        removed.push(k);
    }
    Ok(removed)
}

/// Restore a snapshot into an empty directory with the restore seed.
pub fn restore(
    store: &dyn ObjectStore,
    key: &str,
    seed_hex: &str,
    into: &Path,
) -> std::io::Result<Vec<String>> {
    let seed = proto::keys::key32(seed_hex.trim()).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "seed is not 64 hex characters",
        )
    })?;
    let identity = Identity::from_seed(&seed);
    let bytes = store.get(key)?;
    format::unpack(&bytes, &identity, into)
}

/// The newest snapshot key under `prefix`.
pub fn latest(store: &dyn ObjectStore, prefix: &str) -> std::io::Result<Option<String>> {
    let mut keys: Vec<String> = store
        .list(&prefix_slash(prefix))?
        .into_iter()
        .filter(|k| k.ends_with(".tracon-snap"))
        .collect();
    keys.sort();
    Ok(keys.pop())
}

fn prefix_slash(prefix: &str) -> String {
    let p = prefix.trim_matches('/');
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}/")
    }
}
