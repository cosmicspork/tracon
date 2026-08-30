//! Forges, for choosing where a session runs: list the operator's
//! repositories over REST and clone one into the node's managed root. The
//! tokens are the same `gh` and `glab` credentials publishing uses, read
//! through the broker on the node's side of the privilege boundary; a clone
//! or fetch hands git the token through the environment only, so nothing
//! lands in argv, in `.git/config`, or in the stored remote URL.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::broker::{BrokerError, SharedBroker};

/// A forge is named by the credential that reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forge {
    Github,
    Gitlab,
}

impl Forge {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "github" | "gh" => Some(Self::Github),
            "gitlab" | "glab" => Some(Self::Gitlab),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
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

    pub fn token<'a>(&self, env: &'a BTreeMap<String, String>) -> Option<&'a String> {
        match self {
            Self::Github => env.get("GH_TOKEN").or_else(|| env.get("GITHUB_TOKEN")),
            Self::Gitlab => env.get("GITLAB_TOKEN").or_else(|| env.get("GLAB_TOKEN")),
        }
    }

    /// The username the credential helper answers with. Both forges take any
    /// HTTPS basic auth with the token as the password; these are the values
    /// their own tooling uses.
    fn git_user(&self) -> &'static str {
        match self {
            Self::Github => "x-access-token",
            Self::Gitlab => "oauth2",
        }
    }

    /// The REST base and the clone host, from the credential's own env so an
    /// enterprise or self-hosted forge (`GH_HOST`/`GITHUB_API`, `GITLAB_HOST`)
    /// needs nothing new — and so a test can stand a forge on loopback.
    fn endpoints(&self, env: &BTreeMap<String, String>) -> (String, String) {
        match self {
            Self::Github => {
                let api = env
                    .get("GITHUB_API")
                    .map(|s| s.trim_end_matches('/').to_string())
                    .unwrap_or_else(|| "https://api.github.com".into());
                let host = env
                    .get("GH_HOST")
                    .map(|s| s.trim_end_matches('/').to_string())
                    .unwrap_or_else(|| "github.com".into());
                (api, host)
            }
            Self::Gitlab => {
                let raw = env
                    .get("GITLAB_HOST")
                    .map(|s| s.trim_end_matches('/').to_string())
                    .unwrap_or_else(|| "https://gitlab.com".into());
                let base = if raw.starts_with("http://") || raw.starts_with("https://") {
                    raw
                } else {
                    format!("https://{raw}")
                };
                let host = base
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .to_string();
                (format!("{base}/api/v4"), host)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Repo {
    pub host: String,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub private: bool,
    pub default_branch: Option<String>,
    /// The forge's own "last pushed" stamp, verbatim; the list arrives most
    /// recently pushed first.
    pub pushed_at: Option<String>,
}

/// One forge's answer: its repositories, or why it has none to show. A forge
/// whose credential does not exist at all is simply absent — not configured
/// is not an error.
#[derive(Debug, Clone, Serialize)]
pub struct ForgeRepos {
    pub forge: &'static str,
    pub repos: Vec<Repo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The listing for every forge whose credential the broker holds, one page
/// each (100 repositories, most recently active first).
pub async fn list_repos(
    http: &reqwest::Client,
    broker: &SharedBroker,
    channel: &str,
    node_id: &str,
) -> Vec<ForgeRepos> {
    let mut out = Vec::new();
    for forge in [Forge::Github, Forge::Gitlab] {
        let env = match broker
            .read()
            .unwrap()
            .env_for(forge.credential(), channel, node_id)
        {
            Ok(env) => env,
            // No such credential: the forge is not configured here.
            Err(BrokerError::Unknown(_)) => continue,
            // Exists but this channel or node may not use it: say so.
            Err(e) => {
                out.push(ForgeRepos {
                    forge: forge.name(),
                    repos: Vec::new(),
                    error: Some(e.to_string()),
                });
                continue;
            }
        };
        let entry = match fetch_repos(http, forge, &env).await {
            Ok(repos) => ForgeRepos {
                forge: forge.name(),
                repos,
                error: None,
            },
            Err(e) => ForgeRepos {
                forge: forge.name(),
                repos: Vec::new(),
                error: Some(e),
            },
        };
        out.push(entry);
    }
    out
}

async fn fetch_repos(
    http: &reqwest::Client,
    forge: Forge,
    env: &BTreeMap<String, String>,
) -> Result<Vec<Repo>, String> {
    let token = forge.token(env).ok_or_else(|| {
        format!(
            "credential {} has no token the forge accepts",
            forge.credential()
        )
    })?;
    let (api, host) = forge.endpoints(env);
    match forge {
        Forge::Github => {
            let v = get(
                http,
                &format!("{api}/user/repos?per_page=100&sort=pushed"),
                &[
                    ("authorization", &format!("Bearer {token}")),
                    ("user-agent", "tracon"),
                    ("accept", "application/vnd.github+json"),
                    ("x-github-api-version", "2022-11-28"),
                ],
            )
            .await?;
            let rows = v.as_array().cloned().unwrap_or_default();
            Ok(rows
                .iter()
                .filter_map(|r| {
                    let full = r["full_name"].as_str()?;
                    let (owner, name) = full.split_once('/')?;
                    Some(Repo {
                        host: host.clone(),
                        owner: owner.to_string(),
                        name: name.to_string(),
                        full_name: full.to_string(),
                        private: r["private"].as_bool().unwrap_or(false),
                        default_branch: r["default_branch"].as_str().map(String::from),
                        pushed_at: r["pushed_at"].as_str().map(String::from),
                    })
                })
                .collect())
        }
        Forge::Gitlab => {
            let v = get(
                http,
                &format!("{api}/projects?membership=true&order_by=last_activity_at&per_page=100"),
                &[("private-token", token.as_str())],
            )
            .await?;
            let rows = v.as_array().cloned().unwrap_or_default();
            Ok(rows
                .iter()
                .filter_map(|r| {
                    let full = r["path_with_namespace"].as_str()?;
                    let (owner, name) = full.rsplit_once('/')?;
                    Some(Repo {
                        host: host.clone(),
                        owner: owner.to_string(),
                        name: name.to_string(),
                        full_name: full.to_string(),
                        private: r["visibility"].as_str() != Some("public"),
                        default_branch: r["default_branch"].as_str().map(String::from),
                        pushed_at: r["last_activity_at"].as_str().map(String::from),
                    })
                })
                .collect())
        }
    }
}

async fn get(http: &reqwest::Client, url: &str, headers: &[(&str, &str)]) -> Result<Value, String> {
    let mut req = http.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = req.send().await.map_err(|e| format!("forge: {e}"))?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("forge answered {status}: {}", v["message"]));
    }
    Ok(v)
}

/// Where managed clones live, under the node's own state.
pub fn managed_root(state_dir: &Path) -> PathBuf {
    state_dir.join("repos")
}

/// `<root>/<host>/<owner>/<name>`, with every component checked: a path is
/// built from forge-supplied strings, so nothing may traverse or hide.
pub fn clone_dest(root: &Path, host: &str, owner: &str, name: &str) -> Result<PathBuf, String> {
    for part in [host, owner, name] {
        let ok = !part.is_empty()
            && !part.starts_with('.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !ok {
            return Err(format!("refusing path component {part:?}"));
        }
    }
    Ok(root.join(host).join(owner).join(name))
}

/// The environment that lets node-side git authenticate to a forge without
/// the token touching disk or argv: an inline credential helper that answers
/// from two variables git never records.
pub fn git_credential_env(forge: Forge, token: &str) -> Vec<(String, String)> {
    vec![
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
        (
            "GIT_CONFIG_VALUE_0".into(),
            r#"!f(){ printf 'username=%s\npassword=%s\n' "$TRACON_GIT_USER" "$TRACON_GIT_TOKEN"; }; f"#
                .into(),
        ),
        ("TRACON_GIT_USER".into(), forge.git_user().into()),
        ("TRACON_GIT_TOKEN".into(), token.into()),
    ]
}

/// The auth environment for node-side git against `repo_path`, or empty when
/// none applies: a repo outside the managed root is the operator's own
/// checkout with the operator's own auth, and a broker refusal degrades to
/// anonymous rather than failing a public repo.
pub fn git_env_for(
    broker: &SharedBroker,
    state_dir: &Path,
    channel: &str,
    repo_path: &Path,
    node_id: &str,
) -> Vec<(String, String)> {
    let root = managed_root(state_dir);
    let Ok(rest) = repo_path.strip_prefix(&root) else {
        return Vec::new();
    };
    let Some(host) = rest.components().next() else {
        return Vec::new();
    };
    let host = host.as_os_str().to_string_lossy();
    let forge = if host.contains("github") {
        Forge::Github
    } else {
        Forge::Gitlab
    };
    let Ok(env) = broker
        .read()
        .unwrap()
        .env_for(forge.credential(), channel, node_id)
    else {
        return Vec::new();
    };
    match forge.token(&env) {
        Some(t) => git_credential_env(forge, t),
        None => Vec::new(),
    }
}

/// Clone into the managed root. Idempotent: an existing clone is the answer,
/// not an error. The URL carries no credential; the helper env does.
pub async fn clone(
    http_env: Vec<(String, String)>,
    host: &str,
    owner: &str,
    name: &str,
    dest: &Path,
) -> Result<(), String> {
    if dest.join(".git").exists() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let url = format!("https://{host}/{owner}/{name}.git");
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone").arg(&url).arg(dest);
    for (k, v) in &http_env {
        cmd.env(k, v);
    }
    let out = cmd.output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        // A failed partial clone would poison the idempotency check.
        let _ = std::fs::remove_dir_all(dest);
        return Err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// The managed clones already on disk, for the picker.
pub fn managed_repos(state_dir: &Path) -> Vec<Repo> {
    let root = managed_root(state_dir);
    let mut out = Vec::new();
    let Ok(hosts) = std::fs::read_dir(&root) else {
        return out;
    };
    for host in hosts.flatten() {
        let Ok(owners) = std::fs::read_dir(host.path()) else {
            continue;
        };
        for owner in owners.flatten() {
            let Ok(names) = std::fs::read_dir(owner.path()) else {
                continue;
            };
            for name in names.flatten() {
                if !name.path().join(".git").exists() {
                    continue;
                }
                let (h, o, n) = (
                    host.file_name().to_string_lossy().to_string(),
                    owner.file_name().to_string_lossy().to_string(),
                    name.file_name().to_string_lossy().to_string(),
                );
                out.push(Repo {
                    full_name: format!("{o}/{n}"),
                    host: h,
                    owner: o,
                    name: n,
                    private: false,
                    default_branch: None,
                    pushed_at: None,
                });
            }
        }
    }
    out.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_names_and_credentials_pair_up() {
        assert_eq!(Forge::parse("github"), Some(Forge::Github));
        assert_eq!(Forge::parse("glab"), Some(Forge::Gitlab));
        assert_eq!(Forge::parse("bitbucket"), None);
        assert_eq!(Forge::Github.credential(), "gh");
        assert_eq!(Forge::Gitlab.credential(), "glab");
    }

    #[test]
    fn tokens_are_read_from_the_names_the_tools_use() {
        let mut env = BTreeMap::new();
        env.insert("GITHUB_TOKEN".into(), "fake-token-for-tests".into());
        assert_eq!(
            Forge::Github.token(&env).map(String::as_str),
            Some("fake-token-for-tests")
        );
        env.insert("GH_TOKEN".into(), "fake-token-wins".into());
        assert_eq!(
            Forge::Github.token(&env).map(String::as_str),
            Some("fake-token-wins")
        );
        assert!(Forge::Gitlab.token(&env).is_none());
    }

    #[test]
    fn endpoints_default_to_the_public_forges_and_bend_to_the_credential() {
        let mut env = BTreeMap::new();
        let (api, host) = Forge::Github.endpoints(&env);
        assert_eq!(api, "https://api.github.com");
        assert_eq!(host, "github.com");
        let (api, host) = Forge::Gitlab.endpoints(&env);
        assert_eq!(api, "https://gitlab.com/api/v4");
        assert_eq!(host, "gitlab.com");

        env.insert("GITLAB_HOST".into(), "git.example.com/".into());
        let (api, host) = Forge::Gitlab.endpoints(&env);
        assert_eq!(api, "https://git.example.com/api/v4");
        assert_eq!(host, "git.example.com");

        env.insert("GITHUB_API".into(), "http://127.0.0.1:9999/".into());
        let (api, _) = Forge::Github.endpoints(&env);
        assert_eq!(api, "http://127.0.0.1:9999");
    }

    #[test]
    fn hostile_path_components_are_refused() {
        let root = Path::new("/state/repos");
        assert!(clone_dest(root, "github.com", "me", "proj").is_ok());
        for bad in ["..", "", "a/b", ".hidden", "a b"] {
            assert!(
                clone_dest(root, "github.com", bad, "proj").is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_credential_env_carries_the_token_out_of_argv() {
        let env = git_credential_env(Forge::Github, "fake-token-for-tests");
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"GIT_CONFIG_KEY_0"));
        assert!(keys.contains(&"TRACON_GIT_TOKEN"));
        let helper = &env
            .iter()
            .find(|(k, _)| k == "GIT_CONFIG_VALUE_0")
            .unwrap()
            .1;
        // The helper reads the variables; the token itself is not in the
        // helper text, so `ps` and git traces never see it.
        assert!(!helper.contains("fake-token-for-tests"));
    }

    #[test]
    fn only_managed_repos_get_an_auth_env() {
        let broker = crate::broker::Broker::default().shared();
        let env = git_env_for(
            &broker,
            Path::new("/state"),
            "personal",
            Path::new("/home/op/src/project"),
            "n1",
        );
        assert!(env.is_empty());
        // Managed path but no credential in the broker: anonymous, not an error.
        let env = git_env_for(
            &broker,
            Path::new("/state"),
            "personal",
            Path::new("/state/repos/github.com/me/proj"),
            "n1",
        );
        assert!(env.is_empty());
    }
}
