//! Publishing crosses two explicit boundaries. A candidate is read without a
//! credential from its runtime snapshot, then imported into a fresh
//! publisher-controlled bare repository. Only that publisher repository sees
//! broker-provided forge authentication.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

    pub fn noun(&self) -> &'static str {
        match self {
            Self::Github => "pull request",
            Self::Gitlab => "merge request",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub provider: String,
    pub project: String,
    pub base: String,
    pub branch: String,
    /// External worktree provenance only. Publication still imports its
    /// immutable candidate into a distinct publisher repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
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
    #[error("candidate identity changed while transferring to the publisher")]
    IdentityChanged,
    #[error("invalid publication target: {0}")]
    Target(String),
}

/// Publish an immutable candidate. `candidate` is a node-owned snapshot made
/// immediately before approval, never the mutable agent workspace. Git reads
/// it with an empty credential environment, writes a bundle, verifies commit
/// and tree identity after importing it, then pushes only from a new publisher
/// repository with broker authentication.
#[allow(clippy::too_many_arguments)]
pub async fn publish(
    broker: &SharedBroker,
    cfg: &Config,
    channel: &str,
    node_id: &str,
    candidate_id: &str,
    candidate: &str,
    target: &Target,
    head_sha: &str,
    title: &str,
    body: &str,
    before_push: Option<&(dyn Fn() -> Result<(), String> + Send + Sync)>,
) -> Result<String, PublishError> {
    let provider = Provider::parse(&target.provider).ok_or(PublishError::UnknownProvider {
        provider: target.provider.clone(),
    })?;
    let env = broker
        .read()
        .unwrap()
        .env_for(provider.credential(), channel, node_id)
        .map_err(|e| PublishError::Broker(e.to_string()))?;

    let candidate_path = Path::new(candidate);
    let now = source_git(&cfg.publish.git, candidate_path, ["rev-parse", "HEAD"]).await?;
    if now != head_sha {
        return Err(PublishError::BranchMoved {
            reviewed: head_sha.to_string(),
            now,
        });
    }
    let source_type = source_git(
        &cfg.publish.git,
        candidate_path,
        ["cat-file", "-t", head_sha],
    )
    .await?;
    if source_type != "commit" {
        return Err(PublishError::IdentityChanged);
    }
    let source_tree = source_git(
        &cfg.publish.git,
        candidate_path,
        ["rev-parse", &format!("{head_sha}^{{tree}}")],
    )
    .await?;
    if let Some(recheck) = before_push {
        recheck().map_err(PublishError::Broker)?;
    }

    let publisher = publisher_dir(candidate_id)?;
    let bundle = publisher.with_extension("bundle");
    let _ = std::fs::remove_dir_all(&publisher);
    let _ = std::fs::remove_file(&bundle);
    if let Some(parent) = publisher.parent() {
        std::fs::create_dir_all(parent).map_err(|source| PublishError::Spawn {
            cli: "mkdir".into(),
            source,
        })?;
    }
    init_publisher(&cfg.publish.git, &publisher).await?;
    // `git bundle create` needs the positive revision to resolve to a named
    // ref: a bare commit SHA has no ref to advertise in the bundle header, so
    // git refuses it as "empty" even though the objects are all there. Point
    // a throwaway ref at it for the bundle, then remove the ref again; the
    // candidate is a private snapshot, so nothing else observes it.
    let bundle_ref = "refs/tracon/publish-candidate";
    source_git(
        &cfg.publish.git,
        candidate_path,
        ["update-ref", bundle_ref, head_sha],
    )
    .await?;
    let bundled = source_git(
        &cfg.publish.git,
        candidate_path,
        [
            "bundle",
            "create",
            bundle.to_string_lossy().as_ref(),
            bundle_ref,
        ],
    )
    .await;
    let _ = source_git(
        &cfg.publish.git,
        candidate_path,
        ["update-ref", "-d", bundle_ref],
    )
    .await;
    bundled?;
    publisher_git(
        &cfg.publish.git,
        &publisher,
        [
            "fetch",
            "--no-tags",
            bundle.to_string_lossy().as_ref(),
            &format!("{head_sha}:refs/heads/candidate"),
        ],
    )
    .await?;
    let imported = publisher_git(
        &cfg.publish.git,
        &publisher,
        ["rev-parse", "refs/heads/candidate"],
    )
    .await?;
    let imported_tree = publisher_git(
        &cfg.publish.git,
        &publisher,
        ["rev-parse", "refs/heads/candidate^{tree}"],
    )
    .await?;
    if imported != head_sha || imported_tree != source_tree {
        return Err(PublishError::IdentityChanged);
    }

    let remote = remote_url(provider, &env, &target.project)?;
    publisher_git(
        &cfg.publish.git,
        &publisher,
        ["remote", "add", "origin", &remote],
    )
    .await?;
    let refspec = format!("{head_sha}:refs/heads/{}", target.branch);
    publisher_git_with_env(
        &cfg.publish.git,
        &publisher,
        &env,
        ["push", "origin", &refspec],
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
            "--no-squash-before-merge".into(),
            "--yes".into(),
        ],
    };
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Some(recheck) = before_push {
        recheck().map_err(PublishError::Broker)?;
    }
    run_cli(provider.command(cfg), &publisher, &env, &argv).await
}

fn publisher_dir(candidate_id: &str) -> Result<PathBuf, PublishError> {
    if candidate_id.is_empty()
        || !candidate_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(PublishError::Target(
            "candidate id must be an opaque safe id".into(),
        ));
    }
    Ok(Config::state_dir().join("publishers").join(candidate_id))
}

fn remote_url(
    provider: Provider,
    env: &BTreeMap<String, String>,
    project: &str,
) -> Result<String, PublishError> {
    if project.is_empty()
        || project.starts_with('/')
        || project.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    {
        return Err(PublishError::Target("project path is not canonical".into()));
    }
    let raw_host = match provider {
        Provider::Github => env
            .get("GITHUB_HOST")
            .map(String::as_str)
            .unwrap_or("github.com"),
        Provider::Gitlab => env
            .get("GITLAB_HOST")
            .map(String::as_str)
            .unwrap_or("gitlab.com"),
    };
    let host = raw_host
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':'))
    {
        return Err(PublishError::Target("forge host is not canonical".into()));
    }
    Ok(format!("https://{host}/{project}.git"))
}

const GIT_SAFE: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=",
    "-c",
    "core.useReplaceRefs=false",
    "-c",
    "credential.helper=",
];

async fn source_git<'a>(
    git: &str,
    dir: &Path,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, PublishError> {
    let mut command = Command::new(git);
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", Config::state_dir().join("publish-home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(dir)
        .args(GIT_SAFE)
        .args(args);
    output(git, command).await
}

async fn init_publisher(git: &str, dir: &Path) -> Result<String, PublishError> {
    let mut command = Command::new(git);
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", Config::state_dir().join("publish-home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(GIT_SAFE)
        .args(["init", "--bare"])
        .arg(dir);
    output(git, command).await
}

async fn publisher_git<'a>(
    git: &str,
    dir: &Path,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, PublishError> {
    publisher_git_inner(git, dir, None, args).await
}

async fn publisher_git_with_env<'a>(
    git: &str,
    dir: &Path,
    env: &BTreeMap<String, String>,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, PublishError> {
    publisher_git_inner(git, dir, Some(env), args).await
}

async fn publisher_git_inner<'a>(
    git: &str,
    dir: &Path,
    env: Option<&BTreeMap<String, String>>,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, PublishError> {
    let mut command = Command::new(git);
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", Config::state_dir().join("publish-home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("--git-dir")
        .arg(dir)
        .args(GIT_SAFE)
        .args(args);
    if let Some(env) = env {
        command.envs(env);
    }
    output(git, command).await
}

async fn run_cli(
    cli: String,
    dir: &Path,
    env: &BTreeMap<String, String>,
    args: &[&str],
) -> Result<String, PublishError> {
    let mut command = Command::new(&cli);
    command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", Config::state_dir().join("publish-home"))
        .envs(env);
    output(&cli, command).await
}

async fn output(cli: &str, mut command: Command) -> Result<String, PublishError> {
    let out = command
        .output()
        .await
        .map_err(|source| PublishError::Spawn {
            cli: cli.to_string(),
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
            cli: cli.to_string(),
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
        assert_eq!(Provider::Github.credential(), "gh");
    }

    #[tokio::test]
    async fn publishing_without_a_bound_credential_is_refused_before_anything_runs() {
        let broker = crate::broker::Broker::default().shared();
        let target = Target {
            provider: "gitlab".into(),
            project: "custom-development/integrations".into(),
            base: "main".into(),
            branch: "feat/x".into(),
            worktree: None,
        };
        let err = publish(
            &broker,
            &Config::default(),
            "work",
            "n1",
            "cand1",
            "/tmp",
            &target,
            "deadbeef",
            "t",
            "b",
            None,
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
            worktree: None,
        };
        let err = publish(
            &crate::broker::Broker::default().shared(),
            &Config::default(),
            "work",
            "n1",
            "cand1",
            "/tmp",
            &target,
            "deadbeef",
            "t",
            "b",
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PublishError::UnknownProvider { .. }));
    }

    #[test]
    fn publication_remote_is_reconstructed_not_read_from_candidate_config() {
        let env = BTreeMap::from([("GITHUB_HOST".into(), "github.example".into())]);
        assert_eq!(
            remote_url(Provider::Github, &env, "group/project").unwrap(),
            "https://github.example/group/project.git"
        );
        assert!(remote_url(Provider::Github, &env, "../project").is_err());
    }
}
