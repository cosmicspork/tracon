//! The one builder for Git commands the node runs: clone, fetch, ls-remote,
//! push, and the local object reads publication makes on the way there.
//!
//! Authentication is brokered or absent. There is deliberately no third
//! option and no setting that adds one: a host credential helper
//! (`osxkeychain`, `gh auth git-credential`, `~/.git-credentials`), an
//! askpass program, or an `ssh-agent` identity must never become a second way
//! for the node to reach a forge, because none of them is bound to the
//! channel, the node, and the credential the broker checked.
//!
//! Every command is built here so that stays true by construction rather than
//! by each caller remembering the same eight environment variables.

use std::path::Path;

use tokio::process::Command;

use crate::config::Config;

/// Where the credential for one Git command comes from.
pub enum Credential<'a> {
    /// None: a public remote, or a purely local read. Git is still built the
    /// same way, so "anonymous" cannot quietly become "whatever the host had".
    Anonymous,
    /// One the broker handed the node for exactly this operation. Both forges
    /// take any username with the token as the password.
    Brokered { user: &'a str, token: &'a str },
}

/// Config the node pins on every Git command it runs. Hooks and the fsmonitor
/// are config-driven exec paths a repository could otherwise use to run a
/// program; replacement objects and grafts rewrite which bytes a commit
/// resolves to, and publication's whole claim is that the bytes pushed are the
/// bytes reviewed.
const SAFE: &[&str] = &[
    "--no-replace-objects",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=",
    "-c",
    "core.useReplaceRefs=false",
];

/// Where the node keeps the empty home a Git command is given, so `$HOME`
/// resolves inside node-owned state rather than at the operator's own Git
/// configuration.
pub fn home(name: &str) -> std::path::PathBuf {
    Config::state_dir().join(name)
}

/// A path that does not exist, pinned into `GIT_ASKPASS`/`SSH_ASKPASS` so no
/// ambient asker can answer for a credential the broker did not hand over.
fn askpass_refused() -> String {
    home("askpass-refused").to_string_lossy().into_owned()
}

/// The credential configuration, as environment config rather than `-c`.
///
/// Git applies `GIT_CONFIG_KEY_n`/`VALUE_n` in order, after every config file
/// and before any `-c`, and an empty `credential.helper` discards every helper
/// named before it. So the reset has to live here, ahead of the brokered
/// helper: a `-c credential.helper=` reset would arrive *after* this pair and
/// discard the brokered helper too, leaving the push with no credential at
/// all. The token itself never reaches argv — the helper prints what two
/// variables hold, and git records neither.
fn credential_env(credential: &Credential<'_>) -> Vec<(String, String)> {
    let mut env = vec![
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
        ("GIT_CONFIG_VALUE_0".into(), String::new()),
    ];
    if let Credential::Brokered { user, token } = credential {
        env[0].1 = "2".into();
        env.extend([
            ("GIT_CONFIG_KEY_1".into(), "credential.helper".into()),
            (
                "GIT_CONFIG_VALUE_1".into(),
                r#"!f(){ printf 'username=%s\npassword=%s\n' "$TRACON_GIT_USER" "$TRACON_GIT_TOKEN"; }; f"#
                    .into(),
            ),
            ("TRACON_GIT_USER".into(), (*user).to_string()),
            ("TRACON_GIT_TOKEN".into(), (*token).to_string()),
        ]);
    }
    env
}

/// The isolation every Git command the node runs starts from. `home_name` is
/// the directory under node state `$HOME` points at; it exists so a
/// publication and a managed clone cannot read each other's Git state.
fn base(git: &str, home_name: &str, credential: &Credential<'_>) -> Command {
    let mut command = Command::new(git);
    command
        // Nothing the node inherited reaches git: not the operator's `GIT_*`,
        // not `SSH_AUTH_SOCK`, not an askpass the desktop session set.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home(home_name))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", askpass_refused())
        .env("SSH_ASKPASS", askpass_refused())
        // The node speaks HTTPS to forges and nothing else: an `ssh://` or
        // `git@host:` remote must fail rather than reach an agent's keys.
        .env("GIT_SSH_COMMAND", "false")
        .envs(credential_env(credential));
    command
}

/// A Git command with no repository of its own (`clone`, `init`).
pub fn git(git_bin: &str, home_name: &str, credential: &Credential<'_>) -> Command {
    let mut command = base(git_bin, home_name, credential);
    command.args(SAFE);
    command
}

/// A Git command run inside a working tree (`-C`).
pub fn git_in(git_bin: &str, dir: &Path, home_name: &str, credential: &Credential<'_>) -> Command {
    let mut command = base(git_bin, home_name, credential);
    command.arg("-C").arg(dir).args(SAFE);
    command
}

/// A Git command run against a bare repository (`--git-dir`).
pub fn git_bare(
    git_bin: &str,
    dir: &Path,
    home_name: &str,
    credential: &Credential<'_>,
) -> Command {
    let mut command = base(git_bin, home_name, credential);
    command.arg("--git-dir").arg(dir).args(SAFE);
    command
}

/// The brokered credential from a token the broker did or did not hand over.
/// Absent is anonymous, not an error: a public remote needs no credential,
/// and a credential cannot be invented either way.
pub fn brokered<'a>(user: &'a str, token: Option<&'a str>) -> Credential<'a> {
    match token {
        Some(token) => Credential::Brokered { user, token },
        None => Credential::Anonymous,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn env_of(command: &Command) -> BTreeMap<String, String> {
        command
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect()
    }

    #[test]
    fn anonymous_still_discards_every_helper_a_config_file_could_name() {
        let env = env_of(&git("git", "publish-home", &Credential::Anonymous));
        assert_eq!(env.get("GIT_CONFIG_COUNT").unwrap(), "1");
        assert_eq!(env.get("GIT_CONFIG_KEY_0").unwrap(), "credential.helper");
        assert_eq!(env.get("GIT_CONFIG_VALUE_0").unwrap(), "");
        assert_eq!(env.get("GIT_CONFIG_GLOBAL").unwrap(), "/dev/null");
        assert_eq!(env.get("GIT_CONFIG_NOSYSTEM").unwrap(), "1");
        assert_eq!(env.get("GIT_TERMINAL_PROMPT").unwrap(), "0");
        assert_eq!(env.get("GIT_SSH_COMMAND").unwrap(), "false");
        assert!(env.contains_key("GIT_ASKPASS"));
        assert!(!env.contains_key("TRACON_GIT_TOKEN"));
    }

    #[test]
    fn the_brokered_helper_comes_after_the_reset_and_keeps_the_token_out_of_argv() {
        let command = git(
            "git",
            "publish-home",
            &Credential::Brokered {
                user: "x-access-token",
                token: "fake-token-for-tests",
            },
        );
        let env = env_of(&command);
        assert_eq!(env.get("GIT_CONFIG_COUNT").unwrap(), "2");
        // Index 0 is the reset, 1 is ours: git applies them in that order, so
        // the reset cannot discard the helper it precedes.
        assert_eq!(env.get("GIT_CONFIG_VALUE_0").unwrap(), "");
        assert!(env.get("GIT_CONFIG_VALUE_1").unwrap().contains("printf"));
        assert_eq!(env.get("TRACON_GIT_TOKEN").unwrap(), "fake-token-for-tests");
        let argv: Vec<String> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !argv.iter().any(|a| a.contains("fake-token-for-tests")),
            "{argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("credential.helper")),
            "a command-line helper would be applied after the env reset: {argv:?}"
        );
    }

    #[test]
    fn a_missing_token_is_anonymous_rather_than_an_invented_credential() {
        assert!(matches!(brokered("git", None), Credential::Anonymous));
        assert!(matches!(
            brokered("git", Some("t")),
            Credential::Brokered { .. }
        ));
    }
}
