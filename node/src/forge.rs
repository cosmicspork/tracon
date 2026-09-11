//! Forges, for choosing where a session runs: list the operator's
//! repositories over REST and clone one into the node's managed root. The
//! tokens are the same `gh` and `glab` credentials publishing uses, read
//! through the broker on the node's side of the privilege boundary; a clone
//! or fetch hands git the token through the environment only, so nothing
//! lands in argv, in `.git/config`, or in the stored remote URL.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

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
    /// The next provider page, opaque to the picker. A page is deliberately
    /// small; the operator must explicitly ask for another one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// False means this list stopped early because it has another page or its
    /// provider supplied invalid pagination metadata.
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

const REPOS_PER_PAGE: u32 = 50;
const MAX_REPO_PAGE: u32 = 100;

/// Credential-bearing forge requests never follow redirects, so a provider
/// cannot send its authorization header to another host.
static FORGE_LISTING_HTTP: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("could not initialize forge listing client: {error}"))
});

fn forge_listing_http() -> Result<&'static reqwest::Client, String> {
    FORGE_LISTING_HTTP.as_ref().map_err(|error| error.clone())
}

struct RepoPage {
    repos: Vec<Repo>,
    next_cursor: Option<String>,
    complete: bool,
    error: Option<String>,
}

/// The listing for every selected forge whose credential the broker holds, one
/// bounded provider page at a time. `only` is used by the picker when it loads
/// another page from one forge; no request can exhaust a forge in one call.
pub async fn list_repos(
    broker: &SharedBroker,
    channel: &str,
    node_id: &str,
    only: Option<Forge>,
    cursor: Option<&str>,
) -> Vec<ForgeRepos> {
    let http = forge_listing_http();
    let mut out = Vec::new();
    for forge in [Forge::Github, Forge::Gitlab] {
        if only.is_some_and(|selected| selected != forge) {
            continue;
        }
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
                    next_cursor: None,
                    complete: false,
                    error: Some(e.to_string()),
                });
                continue;
            }
        };
        let entry = match &http {
            Ok(http) => fetch_repos(http, forge, &env, cursor).await,
            Err(error) => Err(error.clone()),
        };
        out.push(match entry {
            Ok(page) => ForgeRepos {
                forge: forge.name(),
                repos: page.repos,
                next_cursor: page.next_cursor,
                complete: page.complete,
                error: page.error,
            },
            Err(error) => ForgeRepos {
                forge: forge.name(),
                repos: Vec::new(),
                next_cursor: None,
                complete: false,
                error: Some(error),
            },
        });
    }
    out
}

async fn fetch_repos(
    http: &reqwest::Client,
    forge: Forge,
    env: &BTreeMap<String, String>,
    cursor: Option<&str>,
) -> Result<RepoPage, String> {
    let page = repo_page(cursor)?;
    let token = forge.token(env).ok_or_else(|| {
        format!(
            "credential {} has no token the forge accepts",
            forge.credential()
        )
    })?;
    let (api, host) = forge.endpoints(env);
    match forge {
        Forge::Github => {
            let endpoint =
                format!("{api}/user/repos?per_page={REPOS_PER_PAGE}&sort=pushed&page={page}");
            let endpoint_url =
                url::Url::parse(&endpoint).map_err(|_| "configured GitHub API URL is invalid")?;
            let (v, headers) = get(
                http,
                &endpoint,
                &[
                    ("authorization", &format!("Bearer {token}")),
                    ("user-agent", "tracon"),
                    ("accept", "application/vnd.github+json"),
                    ("x-github-api-version", "2022-11-28"),
                ],
            )
            .await?;
            let rows = github_repos(&v, &host);
            let pagination = github_next_page(&headers, &endpoint_url, page);
            Ok(page_result(rows, pagination))
        }
        Forge::Gitlab => {
            let endpoint = format!(
                "{api}/projects?membership=true&order_by=last_activity_at&per_page={REPOS_PER_PAGE}&page={page}"
            );
            let (v, headers) = get(http, &endpoint, &[("private-token", token.as_str())]).await?;
            let rows = gitlab_repos(&v, &host);
            let pagination = gitlab_next_page(&headers, page);
            Ok(page_result(rows, pagination))
        }
    }
}

fn repo_page(cursor: Option<&str>) -> Result<u32, String> {
    let page = match cursor {
        None => 1,
        Some(cursor) => cursor
            .parse()
            .map_err(|_| "repository page cursor is invalid".to_string())?,
    };
    (1..=MAX_REPO_PAGE)
        .contains(&page)
        .then_some(page)
        .ok_or_else(|| "repository listing is limited to 5,000 repositories".to_string())
}

fn page_result(repos: Vec<Repo>, pagination: Result<Option<u32>, String>) -> RepoPage {
    match pagination {
        Ok(next) => RepoPage {
            repos,
            next_cursor: next.map(|page| page.to_string()),
            complete: next.is_none(),
            error: None,
        },
        Err(error) => RepoPage {
            repos,
            next_cursor: None,
            complete: false,
            error: Some(error),
        },
    }
}

fn github_repos(v: &Value, host: &str) -> Vec<Repo> {
    v.as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            let full = r["full_name"].as_str()?;
            let (owner, name) = full.split_once('/')?;
            Some(Repo {
                host: host.to_string(),
                owner: owner.to_string(),
                name: name.to_string(),
                full_name: full.to_string(),
                private: r["private"].as_bool().unwrap_or(false),
                default_branch: r["default_branch"].as_str().map(String::from),
                pushed_at: r["pushed_at"].as_str().map(String::from),
            })
        })
        .collect()
}

fn gitlab_repos(v: &Value, host: &str) -> Vec<Repo> {
    v.as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            let full = r["path_with_namespace"].as_str()?;
            let (owner, name) = full.rsplit_once('/')?;
            Some(Repo {
                host: host.to_string(),
                owner: owner.to_string(),
                name: name.to_string(),
                full_name: full.to_string(),
                private: r["visibility"].as_str() != Some("public"),
                default_branch: r["default_branch"].as_str().map(String::from),
                pushed_at: r["last_activity_at"].as_str().map(String::from),
            })
        })
        .collect()
}

fn github_next_page(
    headers: &reqwest::header::HeaderMap,
    expected: &url::Url,
    current_page: u32,
) -> Result<Option<u32>, String> {
    let Some(value) = headers.get(reqwest::header::LINK) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| "forge sent an invalid pagination link".to_string())?;
    let Some(next) = value.split(',').find_map(|part| {
        let (url, parameters) = part.trim().split_once('>')?;
        let url = url.strip_prefix('<')?;
        parameters
            .split(';')
            .any(|parameter| matches!(parameter.trim(), r#"rel="next""# | "rel=next"))
            .then_some(url)
    }) else {
        return Ok(None);
    };
    let next =
        url::Url::parse(next).map_err(|_| "forge sent an invalid pagination link".to_string())?;
    if next.origin() != expected.origin()
        || !next.username().is_empty()
        || next.password().is_some()
        || next.path() != expected.path()
    {
        return Err("forge pagination link was not on the configured API host".to_string());
    }
    let mut pages = next
        .query_pairs()
        .filter(|(key, _)| key == "page")
        .map(|(_, value)| value.into_owned());
    let next_page = pages
        .next()
        .ok_or_else(|| "forge sent an invalid pagination link".to_string())?
        .parse()
        .map_err(|_| "forge sent an invalid pagination link".to_string())?;
    if pages.next().is_some() {
        return Err("forge sent an invalid pagination link".to_string());
    }
    checked_next_page(next_page, current_page)
}

fn gitlab_next_page(
    headers: &reqwest::header::HeaderMap,
    current_page: u32,
) -> Result<Option<u32>, String> {
    let Some(value) = headers.get("x-next-page") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| "forge sent an invalid pagination cursor".to_string())?;
    if value.is_empty() {
        return Ok(None);
    }
    let next_page = value
        .parse()
        .map_err(|_| "forge sent an invalid pagination cursor".to_string())?;
    checked_next_page(next_page, current_page)
}

fn checked_next_page(next_page: u32, current_page: u32) -> Result<Option<u32>, String> {
    if next_page <= current_page || next_page > MAX_REPO_PAGE {
        return Err("forge sent an invalid pagination cursor".to_string());
    }
    Ok(Some(next_page))
}

async fn get(
    http: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<(Value, reqwest::header::HeaderMap), String> {
    let mut req = http.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = req.send().await.map_err(|e| format!("forge: {e}"))?;
    let status = res.status();
    let headers = res.headers().clone();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(
            "forge answered 401 Unauthorized: the token was rejected; replace it under Settings → Credentials"
                .into(),
        );
    }
    if !status.is_success() {
        return Err(format!("forge answered {status}: {}", v["message"]));
    }
    Ok((v, headers))
}

/// Where managed clones live, under the node's own state.
pub fn managed_root(state_dir: &Path) -> PathBuf {
    state_dir.join("repos")
}

/// `<root>/<host>/<owner>/<name>`, with every component checked: a path is
/// built from forge-supplied strings, so nothing may traverse or hide. The
/// owner may be a GitLab group path (`group/sub/team`); each of its segments
/// is checked on its own.
pub fn clone_dest(root: &Path, host: &str, owner: &str, name: &str) -> Result<PathBuf, String> {
    let mut dest = root.join(host);
    check_component(host)?;
    for part in owner.split('/') {
        check_component(part)?;
        dest.push(part);
    }
    check_component(name)?;
    dest.push(name);
    Ok(dest)
}

fn check_component(part: &str) -> Result<(), String> {
    let ok = !part.is_empty()
        && !part.starts_with('.')
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!("refusing path component {part:?}"))
    }
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
/// Clone into the managed root. Idempotent: an existing clone is the answer,
/// not an error. The URL carries no credential; the helper env does. Git runs
/// in a clean process environment so ambient host helpers cannot become a
/// second authentication path.
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
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", crate::config::Config::state_dir().join("forge-home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=",
            "-c",
            "core.useReplaceRefs=false",
            "clone",
            "--no-local",
            "--no-hardlinks",
            &url,
        ])
        .arg(dest);
    for (k, v) in &http_env {
        cmd.env(k, v);
    }
    let out = cmd.output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(dest);
        return Err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Refresh a trusted managed clone with broker-provided Git authentication.
/// This is intentionally unavailable to agent workspaces: it only accepts a
/// node-owned repository path, and runs with no ambient credential helpers.
pub async fn fetch_managed(
    repo: &Path,
    env: &[(String, String)],
) -> Result<(), String> {
    let root = managed_root(&crate::config::Config::state_dir());
    if !repo.starts_with(&root) {
        return Err("refusing to fetch a repository outside managed storage".into());
    }
    let mut command = tokio::process::Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", crate::config::Config::state_dir().join("forge-home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=",
            "-c",
            "core.useReplaceRefs=false",
            "fetch",
            "--prune",
            "origin",
        ])
        .envs(env);
    let out = command.output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git fetch failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// How deep under a host a clone may sit: an owner, or a GitLab group path
/// of a few levels, then the repository. Nothing legitimate goes deeper, and
/// the walk must not follow a clone into its own tree.
const MAX_CLONE_DEPTH: usize = 8;

/// The managed clones already on disk, for the picker. A clone is any
/// directory under a host that holds a `.git`; the segments between the two
/// are its owner, which for a GitLab group is more than one.
pub fn managed_repos(state_dir: &Path) -> Vec<Repo> {
    let root = managed_root(state_dir);
    let mut out = Vec::new();
    let Ok(hosts) = std::fs::read_dir(&root) else {
        return out;
    };
    for host in hosts.flatten() {
        let h = host.file_name().to_string_lossy().to_string();
        let mut segments = Vec::new();
        walk_clones(&host.path(), &h, &mut segments, &mut out);
    }
    out.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    out
}

fn walk_clones(dir: &Path, host: &str, segments: &mut Vec<String>, out: &mut Vec<Repo>) {
    if segments.len() >= MAX_CLONE_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.join(".git").exists() {
            if segments.is_empty() {
                continue;
            }
            let owner = segments.join("/");
            out.push(Repo {
                full_name: format!("{owner}/{name}"),
                host: host.to_string(),
                owner,
                name,
                private: false,
                default_branch: None,
                pushed_at: None,
            });
            continue;
        }
        segments.push(name);
        walk_clones(&path, host, segments, out);
        segments.pop();
    }
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
    fn github_pagination_link_must_stay_on_the_authenticated_endpoint() {
        let expected =
            url::Url::parse("https://api.github.com/user/repos?per_page=50&sort=pushed&page=1")
                .unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            reqwest::header::HeaderValue::from_static(
                "<https://api.github.com/user/repos?per_page=50&sort=pushed&page=2>; rel=\"next\"",
            ),
        );
        assert_eq!(github_next_page(&headers, &expected, 1).unwrap(), Some(2));

        headers.insert(
            reqwest::header::LINK,
            reqwest::header::HeaderValue::from_static(
                "<https://attacker.example/user/repos?per_page=50&sort=pushed&page=2>; rel=\"next\"",
            ),
        );
        assert_eq!(
            github_next_page(&headers, &expected, 1).unwrap_err(),
            "forge pagination link was not on the configured API host"
        );

        headers.insert(
            reqwest::header::LINK,
            reqwest::header::HeaderValue::from_static(
                "<https://token@api.github.com/user/repos?per_page=50&sort=pushed&page=2>; rel=\"next\"",
            ),
        );
        assert_eq!(
            github_next_page(&headers, &expected, 1).unwrap_err(),
            "forge pagination link was not on the configured API host"
        );
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
        for bad in ["..", "", ".hidden", "a b", "a/../b", "a//b", "a/.x"] {
            assert!(
                clone_dest(root, "github.com", bad, "proj").is_err(),
                "{bad}"
            );
        }
        // The name is one segment; a slash there is not a group path.
        assert!(clone_dest(root, "github.com", "me", "a/b").is_err());
    }

    /// A GitLab project lives under a group path, each level its own directory.
    #[test]
    fn a_group_path_nests_under_the_host() {
        let root = Path::new("/state/repos");
        let dest = clone_dest(root, "gitlab.example", "group/sub/team", "proj").unwrap();
        assert_eq!(
            dest,
            Path::new("/state/repos/gitlab.example/group/sub/team/proj")
        );
    }

    #[test]
    fn managed_repos_finds_clones_at_any_group_depth() {
        let dir = std::env::temp_dir().join(format!("tracon-forge-managed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = managed_root(&dir);
        std::fs::create_dir_all(root.join("github.com/me/proj/.git")).unwrap();
        std::fs::create_dir_all(root.join("gitlab.example/group/sub/team/tool/.git")).unwrap();
        // A directory inside a clone is never a second clone.
        std::fs::create_dir_all(root.join("github.com/me/proj/vendor/.git")).unwrap();
        // A host with nothing cloned yet, and a stray file, are skipped.
        std::fs::create_dir_all(root.join("codeberg.org/empty")).unwrap();
        std::fs::write(root.join("README"), "").unwrap();
        let found: Vec<(String, String, String)> = managed_repos(&dir)
            .into_iter()
            .map(|r| (r.host, r.owner, r.full_name))
            .collect();
        assert_eq!(
            found,
            vec![
                (
                    "gitlab.example".into(),
                    "group/sub/team".into(),
                    "group/sub/team/tool".into()
                ),
                ("github.com".into(), "me".into(), "me/proj".into()),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
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
