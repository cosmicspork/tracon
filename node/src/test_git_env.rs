//! Git, as the tests run it, reads no configuration but a repository's own.
//!
//! A test's `git` is a child of the test process, so it read the operator's
//! `~/.gitconfig` and the machine's `/etc/gitconfig`. On a machine that signs
//! its commits that meant every commit a fixture made was signed with the
//! operator's key, one at a time through one agent: two thirds of the suite's
//! run time, and a suite whose result depended on whose machine ran it.
//!
//! Set before `main`, which is before the process has a second thread and
//! before whichever test comes first can spawn anything. The node's own
//! remote commands clear their environment and set the same two variables
//! themselves (`git_remote::base`), so this changes nothing they prove.
//!
//! Compiled into the library's unit tests from `lib.rs` and into every
//! integration test binary from `tests/support/mod.rs`.

#[used]
#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    link_section = ".init_array"
)]
#[cfg_attr(target_vendor = "apple", link_section = "__DATA,__mod_init_func")]
static BEFORE_MAIN: unsafe extern "C" fn() = set;

unsafe extern "C" fn set() {
    for (name, value) in [
        (c"GIT_CONFIG_GLOBAL", c"/dev/null"),
        (c"GIT_CONFIG_NOSYSTEM", c"1"),
    ] {
        libc::setenv(name.as_ptr(), value.as_ptr(), 1);
    }
}

/// The scopes other than a repository's own that `git` reads configuration
/// from here: empty when the two variables above reached it.
pub fn ambient_scopes() -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["config", "--list", "--show-scope"])
        .current_dir(std::env::temp_dir())
        .output()
        .expect("git runs");
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.starts_with("global") || line.starts_with("system"))
        .map(str::to_string)
        .collect()
}
