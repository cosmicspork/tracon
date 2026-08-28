//! The hub's own identity, for the replica half: a seed file under the data
//! directory, exactly as a node keeps its own. Without a data directory the
//! hub has no identity and stays a relay.

use std::fs;
use std::path::Path;

use proto::keys::{Identity, SEED_LEN};

pub const SEED_FILE: &str = "hub-identity.seed";

pub fn load_or_generate(dir: &Path) -> std::io::Result<(Identity, bool)> {
    let path = dir.join(SEED_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let seed = hex::decode(text.trim())
                .ok()
                .and_then(|b| <[u8; SEED_LEN]>::try_from(b).ok())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{} is not 64 hex characters", path.display()),
                    )
                })?;
            Ok((Identity::from_seed(&seed), false))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(dir)?;
            let (seed, identity) = Identity::generate();
            fs::write(&path, hex::encode(seed))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
            Ok((identity, true))
        }
        Err(e) => Err(e),
    }
}
