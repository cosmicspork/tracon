//! Tests that build and run real containers.
//!
//! They are `#[ignore]`d, so a run that did not make them reports them as
//! ignored rather than passed, and they run only when asked for, with
//! `TRACON_PODMAN_TESTS=1` beside `--ignored`: `--ignored` alone also names
//! every other test that needs something a machine may not have. A machine
//! with Podman is often one running a node, and a test left to find Podman
//! for itself shares that node's Podman machine while its sessions are
//! working. Asked for, a test borrows nothing of the node's: it names its
//! image, and every name it gives a runtime resource is its own. Each suite
//! says at its top what it needs and how to run it.

use std::process::Command;

/// What asks for these tests.
pub const OPT_IN: &str = "TRACON_PODMAN_TESTS";

/// Whether these tests were asked for. When they were not, says so once per
/// test and the test returns; when they were, a missing prerequisite fails it
/// rather than skipping, since a run asked for and not made reads as a pass.
pub fn requested() -> bool {
    let asked = std::env::var(OPT_IN).is_ok_and(|v| v == "1");
    if !asked {
        eprintln!("skipped: real-container tests run only with {OPT_IN}=1");
    }
    asked
}

/// `podman` with `args`, its trimmed stdout when it succeeded.
pub fn podman(args: &[&str]) -> Option<String> {
    let out = Command::new("podman").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The image named in `var`, which has to be set and present: the node's own
/// images are never picked up in its place.
pub fn named_image(var: &str) -> String {
    let image = std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| panic!("{OPT_IN}=1 needs the image under test named in {var}"));
    assert!(
        podman(&["image", "exists", &image]).is_some(),
        "{var} names {image}, which is not in the local store"
    );
    image
}
