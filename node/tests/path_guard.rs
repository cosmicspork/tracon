//! The path guard as a node process meets it: made from `node.toml` the
//! first time anything is spawned, with nothing installed beforehand, and
//! refusing at the spawn. The guard's rules are tested beside it
//! (`config::path_guard`); this is the wiring a unit test binary leaves off.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::os::unix::fs::PermissionsExt;

use tracon::config::Config;
use tracon::git_remote::{self, Credential};

#[test]
fn a_file_that_names_a_planted_program_still_loads_and_the_spawn_is_refused() {
    // Not `isolate`: that turns the guard off for the stand-in programs
    // other files run.
    state::isolate_dirs();
    let config_dir = std::path::PathBuf::from(std::env::var_os("TRACON_CONFIG_DIR").unwrap());
    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    let planted = work.join("tracon-1/git");
    std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
    std::fs::write(&planted, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        config_dir.join("node.toml"),
        format!(
            "[session]\nworktree_root = {:?}\n\n[publish]\ngit = {:?}\n",
            work.to_string_lossy(),
            planted.to_string_lossy()
        ),
    )
    .unwrap();

    // Written by hand, not through an edit: it loads, so the node starts.
    let cfg = Config::try_load().unwrap();
    assert_eq!(cfg.publish.git, planted.to_string_lossy());

    let error = git_remote::git(&cfg.publish.git, "publish-home", &Credential::Anonymous)
        .expect_err("the planted git was allowed to run");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        error.to_string().contains("which an agent can write"),
        "{error}"
    );

    // A git the operator installed is still run.
    if std::path::Path::new("/usr/bin/git").exists() {
        git_remote::git("git", "publish-home", &Credential::Anonymous).unwrap();
    }
}
