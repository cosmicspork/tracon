use std::{fs, path::Path};

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
}
