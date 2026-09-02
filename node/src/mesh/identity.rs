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
    write_seed(path, &seed)?;
    Ok((identity, true))
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

/// Written through a temporary file in the same directory and renamed into
/// place: a reader racing the write sees either no seed or a whole one, never
/// the empty file a truncating write leaves behind for an instant.
fn write_seed(path: &Path, seed: &[u8; SEED_LEN]) -> Result<(), IdentityError> {
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
    fs::rename(&tmp, path).map_err(|source| {
        let _ = fs::remove_file(&tmp);
        io(path)(source)
    })?;
    Ok(())
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
    /// behind for an instant.
    #[test]
    fn a_racing_reader_never_sees_half_a_seed() {
        let dir = std::env::temp_dir().join(format!("tracon-id-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SEED_FILE);
        for _ in 0..300 {
            let _ = fs::remove_file(&path);
            std::thread::scope(|s| {
                for _ in 0..8 {
                    s.spawn(|| {
                        load_or_generate_at(&path).unwrap();
                    });
                }
            });
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
