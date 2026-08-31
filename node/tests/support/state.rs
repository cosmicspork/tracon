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

use std::path::PathBuf;

/// Point `TRACON_STATE_DIR` and `TRACON_CONFIG_DIR` at a throwaway directory
/// for this test process.
///
/// The directory is cleared first: a run that was killed leaves its state
/// behind, and a recycled pid would otherwise hand the next run a populated
/// credential store.
#[allow(dead_code)]
pub fn isolate() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("TRACON_STATE_DIR").is_some() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("tracon-it-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("TRACON_STATE_DIR", &dir);
        // node.toml is not under the state directory on every platform, and
        // the interface writes it now. Same throwaway, same reason.
        if std::env::var_os("TRACON_CONFIG_DIR").is_none() {
            std::env::set_var("TRACON_CONFIG_DIR", &dir);
        }
    });
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
