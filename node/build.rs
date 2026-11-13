use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, SystemTime},
};

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
        // Cargo reruns this script when `dist` is newer than the script's last
        // run, and a page written during that run always is: the build after
        // the first one in a fresh checkout compiled the node a second time.
        // A placeholder as old as the tree's first commit is never the change.
        backdate(&dist.join("index.html"));
        backdate(&dist);
    }
    build_id();
    browser_version();
}

/// Best effort: a placeholder left with today's date costs a rebuild, not a
/// wrong build.
fn backdate(path: &Path) {
    let long_ago = SystemTime::UNIX_EPOCH + Duration::from_secs(946_684_800);
    if let Ok(file) = fs::File::open(path) {
        let _ = file.set_modified(long_ago);
    }
}

/// `TRACON_BROWSER_VERSION`: the Chrome version of the `browser` service
/// bundle a release attaches, read from the headless-shell tag pinned in
/// `containers/browser/Containerfile`, which `scripts/browser-bundle.sh`
/// builds that bundle from. Empty when the file is not in the source tree.
fn browser_version() {
    let containerfile =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../containers/browser/Containerfile");
    println!("cargo:rerun-if-changed={}", containerfile.display());
    let version = fs::read_to_string(&containerfile)
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("FROM docker.io/chromedp/headless-shell:")?;
                let tag = rest.split('@').next()?.trim();
                let numeric = tag.split('.').count() == 4
                    && tag
                        .split('.')
                        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
                numeric.then(|| tag.to_string())
            })
        })
        .unwrap_or_default();
    if version.is_empty() {
        println!(
            "cargo:warning=no pinned headless-shell tag in {}; this build fetches no browser bundle",
            containerfile.display()
        );
    }
    println!("cargo:rustc-env=TRACON_BROWSER_VERSION={version}");
}

/// `TRACON_BUILD`: the commit this binary was built from, unless that commit
/// is the release's own tag, so a build of unreleased code never reads as the
/// release whose version it carries. Empty for a release, and for a source
/// tree without git (the container builds exclude `.git`).
///
/// A debug build says `debug` and nothing else. The commit is compiled into
/// the library, so naming it meant every commit and every branch switch
/// recompiled the node and linked every test binary again, to tell a build
/// run from the tree in front of it which tree that was. `just node-dev` and
/// the releases build the release profile, and still say.
fn build_id() {
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        println!("cargo:rustc-env=TRACON_BUILD=debug");
        return;
    }
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
