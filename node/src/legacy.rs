//! What the retired harness left on disk, and getting rid of it deliberately.
//!
//! Removing an adapter does not remove the state its harness wrote. omp kept
//! a credential database in the node-owned harness-state directory, and after
//! the OpenCode cutover nothing reads it, nothing refreshes it, and nothing
//! would ever notice it was still there — which is the whole problem. A
//! credential that no longer has a client is a credential nobody is watching.
//!
//! So `tracon setup` retires it, by name, and says what it removed. Two things
//! it deliberately does not touch:
//!
//! * **The broker.** `credentials.sealed` holds the tokens the node lifted out
//!   of that database and has used ever since; they are provider credentials,
//!   not harness state, and the harness they were obtained through is
//!   irrelevant to them. Deleting them would sign the operator out of
//!   providers that still work.
//! * **Workspaces and session state.** Archiving a legacy session keeps them
//!   on purpose (`Store::archive_legacy_sessions`), and a reopened session is
//!   given its workspace back.
//!
//! The provider login volumes need no step here: `Providers::connect` clears a
//! provider's login volume before every sign-in, so whatever omp's login wrote
//! into one is gone the first time the operator signs in again.

use std::path::PathBuf;

use crate::config::Config;

/// One thing the retired harness left behind, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub path: PathBuf,
    /// What it was, for the operator reading the list.
    pub what: &'static str,
    /// False when it was not there, which is the ordinary case on a node that
    /// never ran omp or has already been migrated.
    pub removed: bool,
}

/// Everything the retired harness wrote into node-owned state, as paths
/// relative to the harness-state directory.
///
/// omp's state directory was mounted at the harness's `~/.omp`, and this is
/// the host side of that mount. Only omp's own subtree is named: the same
/// directory is now OpenCode's and Claude Code's mount, and removing it whole
/// would take state a supported harness is using.
const ARTIFACTS: &[(&str, &str)] = &[(
    "agent",
    "omp's login credential database (agent.db) and the provider document it persisted (models.yml)",
)];

/// Remove them, and report what was there. Idempotent: a second run finds
/// nothing and says so.
pub fn retire_credentials() -> Vec<Artifact> {
    retire_credentials_in(&Config::harness_state_dir())
}

fn retire_credentials_in(harness_state: &std::path::Path) -> Vec<Artifact> {
    ARTIFACTS
        .iter()
        .map(|(relative, what)| {
            let path = harness_state.join(relative);
            // `exists()` first, so "removed" means this run removed it rather
            // than that the removal did not error on an absent path.
            let removed = path.exists() && std::fs::remove_dir_all(&path).is_ok();
            Artifact {
                path,
                what,
                removed,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state a supported harness is using shares the directory, so the
    /// removal has to be by name. Taking the directory whole would delete
    /// whatever OpenCode or Claude Code keeps there.
    #[test]
    fn only_the_retired_harnesss_own_subtree_is_removed() {
        let dir = std::env::temp_dir().join(format!("tracon-legacy-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("agent")).unwrap();
        std::fs::write(dir.join("agent/agent.db"), b"credentials").unwrap();
        std::fs::create_dir_all(dir.join("statsig")).unwrap();
        std::fs::write(dir.join("statsig/keep"), b"not omp's").unwrap();

        let report = retire_credentials_in(&dir);
        assert_eq!(report.len(), 1);
        assert!(report[0].removed);
        assert!(report[0].path.ends_with("agent"));
        assert!(!dir.join("agent").exists());
        assert!(dir.join("statsig/keep").exists());

        // Idempotent: nothing left to remove, and it says so rather than
        // reporting a second removal that did not happen.
        let again = retire_credentials_in(&dir);
        assert!(!again[0].removed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
