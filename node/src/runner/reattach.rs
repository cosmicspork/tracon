//! Session sandboxes a node leaves running when it stops, and takes back
//! when it starts again.
//!
//! A record names the sandbox (the session's container name), the run it
//! was started for, the provider's handle, the digest of the run's resume
//! secret, the harness process in it and the image the session ran in. It never holds the secret: the
//! sandbox does, and proves it when it redials the next node process, which
//! knows it again by the digest.
//!
//! One file per sandbox under `<state>/sandbox-runs/`, written at stop and
//! taken (read and removed) when the sandbox is adopted or given up.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::repo_image::SessionImage;

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
    /// The image the session ran in, as its start chose it: the one the
    /// sandbox still runs, which a session carried on in it reports and runs
    /// anything else in. `None` in a record from a node before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<SessionImage>,
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

/// The image the session in `name` ran in, from its record, which stays
/// kept for the runner to take: a session carried on in the sandbox runs in
/// what the sandbox runs, never in an image chosen again. `None` with no
/// record, or one that did not keep it.
pub fn kept_image(name: &str) -> Option<SessionImage> {
    serde_json::from_slice::<Record>(&std::fs::read(path(name)).ok()?)
        .ok()
        .filter(|r| r.name == name)?
        .image
}

/// The image each session's sandbox this process starts runs, by name, for
/// the record left if the node stops while it runs.
fn images() -> &'static Mutex<HashMap<String, SessionImage>> {
    static IMAGES: OnceLock<Mutex<HashMap<String, SessionImage>>> = OnceLock::new();
    IMAGES.get_or_init(Default::default)
}

/// The session about to run in `name` runs in `image`.
pub fn note_image(name: &str, image: &SessionImage) {
    images()
        .lock()
        .unwrap()
        .insert(name.to_string(), image.clone());
}

/// The image noted for `name`.
pub fn noted_image(name: &str) -> Option<SessionImage> {
    images().lock().unwrap().get(name).cloned()
}

/// `name`'s sandbox is gone.
pub fn unnote_image(name: &str) {
    images().lock().unwrap().remove(name);
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
            image: Some(SessionImage {
                image: Some("localhost/tracon-snapshot-x@sha256:5bd1".into()),
                toolchain_image: Some("localhost/tracon-repo-x@sha256:0486".into()),
                note: None,
            }),
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
        assert_eq!(kept_image(&record.name), record.image);
        assert!(is_kept(&record.name), "reading its image leaves it kept");
        assert_eq!(take(&record.name), Some(record.clone()));
        assert_eq!(take(&record.name), None);
        assert!(!is_kept(&record.name));
        assert!(!all().contains(&record));
    }

    /// A record a node before the image was kept wrote still loads, with no
    /// image, so its session is carried on as before.
    #[test]
    fn a_record_without_an_image_still_loads() {
        let older = r#"{"name":"tracon-h-older","run":"tracon-h-older:1","handle":"tracon-h-older","digest":"ab","harness":3,"in_turn":true}"#;
        let record: Record = serde_json::from_str(older).unwrap();
        assert_eq!(record.image, None);
        assert!(record.in_turn);
        std::fs::DirBuilder::new()
            .recursive(true)
            .create(dir())
            .unwrap();
        std::fs::write(path(&record.name), older).unwrap();
        assert!(is_kept(&record.name));
        assert_eq!(kept_image(&record.name), None);
        assert_eq!(take(&record.name), Some(record));
    }

    #[test]
    fn a_noted_image_is_kept_until_its_sandbox_goes() {
        let image = SessionImage {
            image: Some("img".into()),
            ..Default::default()
        };
        note_image("tracon-h-noted", &image);
        assert_eq!(noted_image("tracon-h-noted"), Some(image));
        unnote_image("tracon-h-noted");
        assert_eq!(noted_image("tracon-h-noted"), None);
    }
}
