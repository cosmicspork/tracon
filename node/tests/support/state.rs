//! Keeping tests out of the operator's state directory.
//!
//! Integration tests link the library without `cfg(test)`, so the guard in
//! `Config::state_dir` does not cover them. Without this, a test that exercises
//! the credential store or a provider login writes the *real* one: that is how
//! a `cargo test` on a machine that also runs a node replaced its credential
//! store with one sealed under a test key, and deleted its provider logins.
//!
//! Every integration test file includes this module and calls `isolate()`
//! first in any test that could reach state; `scripts/check-tests.sh` refuses
//! a test file that does not include it. It is idempotent and sets the same
//! value from every thread, so racing on it is harmless.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The prefix every isolated state directory shares, so a sweep can recognise
/// one left behind by a run that did not get to clean up.
const PREFIX: &str = "tracon-it-state-";

/// How stale a sibling has to be before the sweep takes it. Long enough that a
/// test binary running concurrently in the same `cargo test` is never a
/// candidate, short enough that a machine does not accumulate them.
const STALE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// What the exit hook removes. Set once, by `isolate()`, before the hook is
/// registered.
static OURS: OnceLock<PathBuf> = OnceLock::new();

/// Registered with `atexit`, so it runs when the test binary's harness exits
/// however the tests went — passed, failed, or panicked. A binary killed
/// outright still leaves its directory, which is what the sweep is for.
extern "C" fn remove_ours() {
    if let Some(dir) = OURS.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Point `TRACON_STATE_DIR` and `TRACON_CONFIG_DIR` at a throwaway directory
/// for this test process.
///
/// The directory is cleared first: a run that was killed leaves its state
/// behind, and a recycled pid would otherwise hand the next run a populated
/// credential store. It is removed again when this binary exits, and stale
/// siblings are swept on the way in — a workspace run is dozens of binaries,
/// and left to themselves they filled a tmpfs's inode table.
#[allow(dead_code)]
pub fn isolate() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("TRACON_STATE_DIR").is_some() {
            return;
        }
        let tmp = std::env::temp_dir();
        let dir = tmp.join(format!("{PREFIX}{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        sweep(&tmp, &dir);
        std::env::set_var("TRACON_STATE_DIR", &dir);
        // node.toml is not under the state directory on every platform, and
        // the interface writes it now. Same throwaway, same reason.
        if std::env::var_os("TRACON_CONFIG_DIR").is_none() {
            std::env::set_var("TRACON_CONFIG_DIR", &dir);
        }
        let _ = OURS.set(dir);
        // SAFETY: `remove_ours` reads a `OnceLock` set above and touches the
        // filesystem; it allocates nothing that has to outlive the exit.
        unsafe {
            libc::atexit(remove_ours);
        }
    });
}

/// Remove state directories left by earlier runs. Anything younger than
/// `STALE` is somebody else's — possibly a sibling binary in this very
/// `cargo test` — and is left alone.
fn sweep(tmp: &Path, ours: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == ours || !entry.file_name().to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= STALE);
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// A fresh, empty directory under the isolated state for one test, so
/// parallel tests that write files never share one. `name` should be the
/// test's own name.
#[allow(dead_code)]
pub fn scratch(name: &str) -> PathBuf {
    isolate();
    let dir = PathBuf::from(std::env::var_os("TRACON_STATE_DIR").unwrap())
        .join("scratch")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `node.toml` is one file per test binary (`Config::config_path`, guarded by
/// `TRACON_CONFIG_DIR` the same way `TRACON_STATE_DIR` is above), unlike
/// `Store::open_in_memory()`, which every test gets its own copy of. A test
/// that writes through a loopback endpoint and then asserts what actually
/// landed on disk races against any other test doing the same — `cargo
/// test`'s default threading runs every `#[tokio::test]` in a file
/// concurrently. Hold this for the span of such a test — an async
/// `tokio::sync::Mutex`, not `std::sync::Mutex`, because the span crosses
/// `.await` points.
#[allow(dead_code)]
pub async fn config_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}
