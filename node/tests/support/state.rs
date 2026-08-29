//! Keeping tests out of the operator's state directory.
//!
//! Integration tests link the library without `cfg(test)`, so the guard in
//! `Config::state_dir` does not cover them. Without this, a test that exercises
//! the credential store or a provider login writes the *real* one: that is how
//! a `cargo test` on a machine that also runs a node replaced its credential
//! store with one sealed under a test key, and deleted its provider logins.
//!
//! Call `isolate()` first in any test that reaches state. It is idempotent and
//! sets the same value from every thread, so racing on it is harmless.

/// Point `TRACON_STATE_DIR` at a throwaway directory for this test process.
#[allow(dead_code)]
pub fn isolate() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("TRACON_STATE_DIR").is_some() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("tracon-it-state-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("TRACON_STATE_DIR", &dir);
    });
}
