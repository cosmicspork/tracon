//! Session sandboxes a node leaves running when it stops, and takes back
//! when it starts again.
//!
//! A record names the sandbox (the session's container name), the run it
//! was started for, the provider's handle, the digest of the run's resume
//! secret and the harness process in it. It never holds the secret: the
//! sandbox does, and proves it when it redials the next node process, which
//! knows it again by the digest.
//!
//! One file per sandbox under `<state>/sandbox-runs/`, written at stop and
//! taken (read and removed) when the sandbox is adopted or given up.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Where the records are kept, under the node's state directory.
pub const DIR: &str = "sandbox-runs";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The sandbox's name: the session's container name.
    pub name: String,
    pub run: String,
    /// The provider's handle for it.
    pub handle: String,
    /// The resume secret's SHA-256, hex.
    pub digest: String,
    /// The harness process the sandbox ran for the session.
    pub harness: Option<u64>,
    /// The session was in a turn when the node stopped.
    #[serde(default)]
    pub in_turn: bool,
}

impl Record {
    pub fn digest(&self) -> Option<[u8; 32]> {
        hex::decode(&self.digest).ok()?.try_into().ok()
    }
}

fn dir() -> PathBuf {
    crate::config::Config::state_dir().join(DIR)
}

fn path(name: &str) -> PathBuf {
    let digest = hex::encode(Sha256::digest(name.as_bytes()));
    dir().join(format!("{}.json", &digest[..16]))
}

/// Keep `record` for the next node process.
pub fn keep(record: &Record) -> std::io::Result<()> {
    let dir = dir();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let path = path(&record.name);
    let staged = path.with_extension("tmp");
    let _ = std::fs::remove_file(&staged);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    file.write_all(&serde_json::to_vec(record).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(&staged, &path)
}

/// Every record kept.
pub fn all() -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect()
}

/// The record for `name`, removed: whoever takes it adopts the sandbox or
/// gives it up.
pub fn take(name: &str) -> Option<Record> {
    let path = path(name);
    let record = serde_json::from_slice::<Record>(&std::fs::read(&path).ok()?).ok();
    let _ = std::fs::remove_file(&path);
    record.filter(|r| r.name == name)
}

/// Whether a record for `name` is kept: the sandbox is still to be taken
/// back.
pub fn is_kept(name: &str) -> bool {
    path(name).exists()
}

/// Forget `name`'s record, if there is one.
pub fn forget(name: &str) {
    let _ = std::fs::remove_file(path(name));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_kept_private_and_taken_once() {
        use std::os::unix::fs::PermissionsExt as _;
        let record = Record {
            name: "tracon-h-keep".into(),
            run: "tracon-h-keep:1".into(),
            handle: "tracon-h-keep".into(),
            digest: hex::encode([7u8; 32]),
            harness: Some(1),
            in_turn: true,
        };
        keep(&record).unwrap();
        let mode = std::fs::metadata(path(&record.name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(all().contains(&record));
        assert!(is_kept(&record.name));
        assert_eq!(record.digest(), Some([7u8; 32]));
        assert_eq!(take(&record.name), Some(record.clone()));
        assert_eq!(take(&record.name), None);
        assert!(!is_kept(&record.name));
        assert!(!all().contains(&record));
    }
}
