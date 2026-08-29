//! Publishing the approved bytes. `gh` for GitHub, `glab` for GitLab; the
//! token comes from the broker and the CLI runs on the node's side of the
//! boundary. The harness has no push path except this one, and this one only
//! runs after a human has approved.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::{broker::SharedBroker, config::Config};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Github,
    Gitlab,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "github" | "gh" | "pr" => Some(Self::Github),
            "gitlab" | "glab" | "mr" => Some(Self::Gitlab),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
        }
    }

    /// The credential this provider publishes with. Naming them after the CLI
    /// keeps the credential store readable.
    pub fn credential(&self) -> &'static str {
        match self {
            Self::Github => "gh",
            Self::Gitlab => "glab",
        }
    }

    pub fn command(&self, cfg: &Config) -> String {
        match self {
            Self::Github => cfg.publish.gh.clone(),
            Self::Gitlab => cfg.publish.glab.clone(),
        }
    }

    /// What the operator is told will happen, before it happens.
    pub fn noun(&self) -> &'static str {
        match self {
            Self::Github => "pull request",
            Self::Gitlab => "merge request",
        }
    }
}

/// Where a review is destined. Captured at submit so the approval screen can
/// say what approving will do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub provider: String,
    /// `owner/name` on GitHub, the project path on GitLab.
    pub project: String,
    pub base: String,
    pub branch: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("{0}")]
    Broker(String),
    #[error("{provider} is not a provider this node publishes to")]
    UnknownProvider { provider: String },
    #[error("could not run {cli}: {source}")]
    Spawn { cli: String, source: std::io::Error },
    #[error("{cli} refused: {stderr}")]
    Refused { cli: String, stderr: String },
    #[error("the branch moved after approval (reviewed {reviewed:.8}, now {now:.8}); re-review before publishing")]
    BranchMoved { reviewed: String, now: String },
}

/// Push the branch and open the change, with the approved title and body.
/// Returns whatever the CLI printed, which is the URL for both of these.
///
/// `head_sha` is the commit that was reviewed. The push pins to it — a branch
/// that moved between approval and publish cannot ride out unreviewed, which is
/// the TOCTOU the approve-time staleness check alone would leave open.
#[allow(clippy::too_many_arguments)]
pub async fn publish(
    broker: &SharedBroker,
    cfg: &Config,
    channel: &str,
    node_id: &str,
    worktree: &str,
    target: &Target,
    head_sha: &str,
    title: &str,
    body: &str,
) -> Result<String, PublishError> {
    let provider = Provider::parse(&target.provider).ok_or(PublishError::UnknownProvider {
        provider: target.provider.clone(),
    })?;
    let env = broker
        .read()
        .unwrap()
        .env_for(provider.credential(), channel, node_id)
        .map_err(|e| PublishError::Broker(e.to_string()))?;

    // Re-assert the tip is still the reviewed commit. The staleness check ran at
    // approve time; a commit landed in the interval must stop the publish rather
    // than be carried out under an approval it never had.
    let now = head_of(&cfg.publish.git, worktree).await?;
    if now != head_sha {
        return Err(PublishError::BranchMoved {
            reviewed: head_sha.to_string(),
            now,
        });
    }

    // The push carries the credential too: a branch the forge cannot see is not
    // a change anyone can review. It pushes the reviewed commit by sha, not the
    // live branch tip, and runs with hooks and fsmonitor disabled. No force: a
    // new branch is created, a fast-forward updates, and a diverged remote
    // branch is rejected rather than clobbered.
    let refspec = format!("{head_sha}:refs/heads/{}", target.branch);
    run(
        cfg.publish.git.clone(),
        worktree,
        &env,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=",
            "push",
            "origin",
            &refspec,
        ],
    )
    .await?;

    let args: Vec<String> = match provider {
        Provider::Github => vec![
            "pr".into(),
            "create".into(),
            "--repo".into(),
            target.project.clone(),
            "--base".into(),
            target.base.clone(),
            "--head".into(),
            target.branch.clone(),
            "--title".into(),
            title.to_string(),
            "--body".into(),
            body.to_string(),
        ],
        Provider::Gitlab => vec![
            "mr".into(),
            "create".into(),
            "--repo".into(),
            target.project.clone(),
            "--target-branch".into(),
            target.base.clone(),
            "--source-branch".into(),
            target.branch.clone(),
            "--title".into(),
            title.to_string(),
            "--description".into(),
            body.to_string(),
            // Merging is the operator's, never the agent's.
            "--no-squash-before-merge".into(),
            "--yes".into(),
        ],
    };
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    run(provider.command(cfg), worktree, &env, &argv).await
}

/// The worktree's current HEAD, via the configured git, with hooks and
/// fsmonitor disabled. Used to confirm the tip is still the reviewed commit.
async fn head_of(git: &str, dir: &str) -> Result<String, PublishError> {
    let out = Command::new(git)
        .args([
            "-C",
            dir,
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=",
            "rev-parse",
            "HEAD",
        ])
        .output()
        .await
        .map_err(|source| PublishError::Spawn {
            cli: git.to_string(),
            source,
        })?;
    if !out.status.success() {
        return Err(PublishError::Refused {
            cli: git.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn run(
    cli: String,
    dir: &str,
    env: &BTreeMap<String, String>,
    args: &[&str],
) -> Result<String, PublishError> {
    let out = Command::new(&cli)
        .args(args)
        .current_dir(dir)
        // A clean environment so nothing else on the node leaks into the CLI,
        // and so the credential is the only one it can use.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .envs(env)
        .output()
        .await
        .map_err(|source| PublishError::Spawn {
            cli: cli.clone(),
            source,
        })?;
    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if stdout.is_empty() {
            String::from_utf8_lossy(&out.stderr).trim().to_string()
        } else {
            stdout
        })
    } else {
        Err(PublishError::Refused {
            cli,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_map_to_their_cli_and_credential() {
        assert_eq!(Provider::parse("gitlab"), Some(Provider::Gitlab));
        assert_eq!(Provider::parse("mr"), Some(Provider::Gitlab));
        assert_eq!(Provider::parse("github"), Some(Provider::Github));
        assert_eq!(Provider::parse("pr"), Some(Provider::Github));
        assert_eq!(Provider::parse("bitbucket"), None);

        assert_eq!(Provider::Gitlab.credential(), "glab");
        assert_eq!(Provider::Gitlab.command(&Config::default()), "glab");
        assert_eq!(Provider::Gitlab.noun(), "merge request");
        assert_eq!(Provider::Github.credential(), "gh");
        assert_eq!(Provider::Github.noun(), "pull request");
    }

    #[tokio::test]
    async fn publishing_without_a_bound_credential_is_refused_before_anything_runs() {
        let broker = crate::broker::Broker::default().shared();
        let target = Target {
            provider: "gitlab".into(),
            project: "custom-development/integrations".into(),
            base: "main".into(),
            branch: "feat/x".into(),
        };
        let err = publish(
            &broker,
            &Config::default(),
            "work",
            "n1",
            "/tmp",
            &target,
            "deadbeef",
            "t",
            "b",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PublishError::Broker(_)), "{err}");
    }

    #[tokio::test]
    async fn an_unknown_provider_is_refused() {
        let target = Target {
            provider: "bitbucket".into(),
            project: "x/y".into(),
            base: "main".into(),
            branch: "feat/x".into(),
        };
        let err = publish(
            &crate::broker::Broker::default().shared(),
            &Config::default(),
            "work",
            "n1",
            "/tmp",
            &target,
            "deadbeef",
            "t",
            "b",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PublishError::UnknownProvider { .. }));
    }
}
