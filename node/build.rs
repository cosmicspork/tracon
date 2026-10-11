use std::{fs, path::Path, process::Command};

// rust-embed refuses to compile when its folder is missing. The SPA is built by
// `just spa`, never by cargo, so a fresh clone and CI's Rust job get a placeholder
// page that says so instead of a build failure.
fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../spa/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    if !dist.join("index.html").exists() {
        fs::create_dir_all(&dist).expect("create spa/dist");
        fs::write(
            dist.join("index.html"),
            "<!doctype html><title>tracon</title><p>SPA not built. Run <code>just spa</code>.</p>\n",
        )
        .expect("write placeholder index.html");
    }
    build_id();
}

/// `TRACON_BUILD`: the commit this binary was built from, unless that commit
/// is the release's own tag, so a build of unreleased code never reads as the
/// release whose version it carries. Empty for a release, and for a source
/// tree without git (the container builds exclude `.git`).
fn build_id() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // HEAD moves, and a tag makes it the release, without any source file
    // changing; watch the refs that say which commit this is.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .unwrap_or_else(|| dir.clone());
        println!("cargo:rerun-if-changed={dir}/HEAD");
        if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={common}/{head}");
        }
        println!("cargo:rerun-if-changed={common}/refs/tags");
        println!("cargo:rerun-if-changed={common}/packed-refs");
    }
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    let released =
        git(&["tag", "--points-at", "HEAD"]).is_some_and(|tags| tags.lines().any(|t| t == tag));
    let id = match git(&["rev-parse", "--short=12", "HEAD"]) {
        Some(commit) if !released => commit,
        _ => String::new(),
    };
    println!("cargo:rustc-env=TRACON_BUILD={id}");
}
