//! Tests that drive the built interface in a real browser.
//!
//! They run only when asked for, with `TRACON_BROWSER_TESTS=1`, as the
//! real-container tests do with theirs (`podman.rs`). Left to find a browser
//! and a built interface for themselves they passed without running wherever
//! one was missing, which was every CI run, and failed twenty seconds into a
//! timeout wherever `spa/dist` was a build of some earlier tree. Asked for, a
//! missing prerequisite fails the test rather than skipping it, since a run
//! asked for and not made reads as a pass.
//!
//! ```sh
//! just spa
//! TRACON_BROWSER_TESTS=1 cargo test -p tracon --test passkey_pwa -- --ignored
//! ```
//!
//! `TRACON_CHROMIUM` names the browser where Playwright's own is not installed.

use std::path::PathBuf;

/// What asks for these tests.
pub const OPT_IN: &str = "TRACON_BROWSER_TESTS";

/// Whether these tests were asked for. When they were not, says so and the
/// test returns.
pub fn requested() -> bool {
    let asked = std::env::var(OPT_IN).is_ok_and(|v| v == "1");
    if !asked {
        eprintln!("skipped: browser tests run only with {OPT_IN}=1");
    }
    asked
}

/// `bun`, and the SPA's directory with its dev dependencies installed and the
/// interface built. Each is required: these tests were asked for.
pub fn bun_and_spa() -> (PathBuf, PathBuf) {
    let spa = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("spa");
    assert!(
        spa.join("node_modules/playwright-core").is_dir(),
        "{OPT_IN}=1 needs the SPA's dev dependencies; run `bun install` in spa/"
    );
    let index = std::fs::read_to_string(spa.join("dist/index.html")).unwrap_or_default();
    assert!(
        index.contains("/assets/"),
        "{OPT_IN}=1 needs the built interface, and spa/dist is the placeholder page; run `just spa`"
    );
    let bun = std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("bun"))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| panic!("{OPT_IN}=1 needs bun on PATH"));
    (bun, spa)
}

/// What a driver under `spa/tests` saw: its one `RESULT` line. A driver with
/// no browser to drive says `SKIP`, which fails here for the same reason a
/// missing build does.
pub fn result(out: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if let Some(why) = stdout.lines().find_map(|l| l.strip_prefix("SKIP ")) {
        panic!("{OPT_IN}=1 and the driver could not run: {why}");
    }
    let Some(line) = stdout.lines().find_map(|l| l.strip_prefix("RESULT ")) else {
        panic!("the driver printed no result\nstdout:\n{stdout}\nstderr:\n{stderr}");
    };
    serde_json::from_str(line).unwrap()
}
