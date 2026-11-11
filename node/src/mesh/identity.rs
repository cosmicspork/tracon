//! The node's identity seed: 32 random bytes, hex, `0600`, under the state
//! directory. Generated on first use. Disposable by design — losing it means
//! re-enrolling, and the enrollment mechanism is the recovery mechanism.

use std::fs;
use std::path::{Path, PathBuf};

use proto::keys::{Identity, SEED_LEN};

use crate::config::Config;

const SEED_FILE: &str = "node-identity.seed";

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0} is not 64 hex characters")]
    Malformed(PathBuf),
}

pub fn seed_path() -> PathBuf {
    Config::state_dir().join(SEED_FILE)
}

/// Load the identity from the state directory, generating and persisting one
/// if none exists. Returns whether it was freshly generated.
pub fn load_or_generate() -> Result<(Identity, bool), IdentityError> {
    load_or_generate_at(&seed_path())
}

pub fn load_or_generate_at(path: &Path) -> Result<(Identity, bool), IdentityError> {
    if let Some(seed) = read_seed(path)? {
        return Ok((Identity::from_seed(&seed), false));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|source| IdentityError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    let (seed, identity) = Identity::generate();
    if write_seed(path, &seed)? {
        return Ok((identity, true));
    }
    // Another first use wrote its seed between our read and our write; that
    // one is the identity now, and ours was never anyone's.
    let seed = read_seed(path)?.ok_or_else(|| IdentityError::Malformed(path.to_path_buf()))?;
    Ok((Identity::from_seed(&seed), false))
}

/// The identity if a seed exists; `None` if the node has never been started.
pub fn load() -> Result<Option<Identity>, IdentityError> {
    Ok(read_seed(&seed_path())?.map(|s| Identity::from_seed(&s)))
}

fn read_seed(path: &Path) -> Result<Option<[u8; SEED_LEN]>, IdentityError> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(IdentityError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let seed = hex::decode(contents.trim())
        .ok()
        .and_then(|b| <[u8; SEED_LEN]>::try_from(b).ok())
        .ok_or_else(|| IdentityError::Malformed(path.to_path_buf()))?;
    Ok(Some(seed))
}

/// Written through a temporary file in the same directory and linked into
/// place: a reader racing the write sees either no seed or a whole one, never
/// the empty file a truncating write leaves behind for an instant. The link,
/// unlike a rename, never replaces a seed already there, so two first uses at
/// once (a node starting while the CLI seals a credential, or tests sharing a
/// state directory) cannot each go on with an identity the file does not hold.
/// Returns false, writing nothing, when a seed was already in place.
fn write_seed(path: &Path, seed: &[u8; SEED_LEN]) -> Result<bool, IdentityError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| IdentityError::Io { path, source }
    };
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, hex::encode(seed)).map_err(io(&tmp))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    let placed = match fs::hard_link(&tmp, path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        // A filesystem without hard links: renamed, as before, which leaves
        // the race but still never a half-written seed.
        Err(_) => fs::rename(&tmp, path).map(|()| true).map_err(io(path)),
    };
    let _ = fs::remove_file(&tmp);
    placed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_once_then_resumes() {
        let dir = std::env::temp_dir().join(format!("tracon-id-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join(SEED_FILE);
        let (a, fresh) = load_or_generate_at(&path).unwrap();
        assert!(fresh);
        let (b, fresh) = load_or_generate_at(&path).unwrap();
        assert!(!fresh);
        assert_eq!(a.node_id(), b.node_id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::write(&path, "zz").unwrap();
        assert!(matches!(
            load_or_generate_at(&path),
            Err(IdentityError::Malformed(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }
    /// One state directory is shared by every test in a binary, and they run at
    /// once, so two can reach the seed together. Whoever loses the race must
    /// still read a whole seed, not the empty file a truncating write leaves
    /// behind for an instant, and it must be the seed that stays: a key sealed
    /// under an identity the file no longer holds opens nothing afterwards.
    #[test]
    fn racing_first_uses_agree_on_one_whole_seed() {
        let dir = std::env::temp_dir().join(format!("tracon-id-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SEED_FILE);
        for _ in 0..300 {
            let _ = fs::remove_file(&path);
            let ids: Vec<_> = std::thread::scope(|s| {
                let racers: Vec<_> = (0..8)
                    .map(|_| s.spawn(|| load_or_generate_at(&path).unwrap()))
                    .collect();
                racers.into_iter().map(|r| r.join().unwrap()).collect()
            });
            assert_eq!(
                ids.iter().filter(|(_, fresh)| *fresh).count(),
                1,
                "exactly one racer generated the seed"
            );
            let kept = load_or_generate_at(&path).unwrap().0.node_id();
            for (identity, _) in &ids {
                assert_eq!(identity.node_id(), kept);
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
