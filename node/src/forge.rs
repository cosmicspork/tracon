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
    pub fn git_user(&self) -> &'static str {
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

/// A repository as the forge tools name it: `owner/name` on GitHub, the
/// project path on GitLab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeRepo {
    pub forge: Forge,
    pub path: String,
}

impl ForgeRepo {
    /// From a canonical remote (`host/owner/name`, as
    /// [`crate::corpus::project::canonical_remote`] spells it). `None` for a
    /// host that is neither forge, and for a path the tools could not take:
    /// GitHub's is exactly two segments, GitLab's at least two.
    pub fn from_remote(canonical: &str) -> Option<Self> {
        let forge = crate::readiness::forge_of(canonical)?;
        let (_, path) = canonical.split_once('/')?;
        let segments = path.split('/').collect::<Vec<_>>();
        let fits = match forge {
            Forge::Github => segments.len() == 2,
            Forge::Gitlab => segments.len() >= 2,
        };
        (fits && segments.iter().all(|s| !s.is_empty())).then(|| Self {
            forge,
            path: path.to_string(),
        })
    }

    /// The argument the forge's tools take the repository as.
    pub fn argument(&self) -> &'static str {
        match self.forge {
            Forge::Github => "repo",
            Forge::Gitlab => "project",
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
                    ("user-agent", USER_AGENT),
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

/// What the node sends as its user agent to a forge's API. GitHub refuses a
/// request without one; the value names the HTTP library, not tracon, so
/// nothing the operator publishes is labelled as coming from it.
pub const USER_AGENT: &str = "reqwest";

/// Who the operator is on a forge, as Git should record them: the name and
/// address commits are authored and committed with, so the work reads as
/// theirs and is attributed to their account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub name: String,
    pub email: String,
    /// The account's login, to say whose identity this is.
    pub login: String,
    pub forge: &'static str,
}

impl Identity {
    /// A `[user]` section for a gitconfig. Values are quoted, with the two
    /// characters a quoted gitconfig value treats specially escaped, so a
    /// display name cannot add a key of its own.
    pub fn gitconfig(&self) -> String {
        format!(
            "[user]\n\tname = {}\n\temail = {}\n",
            gitconfig_quote(&self.name),
            gitconfig_quote(&self.email)
        )
    }

    /// Whether `email` is one this identity commits as. The forge account
    /// may own other addresses; only this one is known without asking for
    /// more access than the token was given.
    pub fn owns(&self, email: &str) -> bool {
        email.eq_ignore_ascii_case(&self.email)
    }
}

fn gitconfig_quote(value: &str) -> String {
    let clean: String = value.chars().filter(|c| !c.is_control()).collect();
    format!("\"{}\"", clean.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Identities already resolved, keyed by forge and a hash of the token: the
/// same binding answers from here, and a replaced token is resolved afresh
/// because its hash differs.
static IDENTITIES: LazyLock<std::sync::Mutex<BTreeMap<String, Identity>>> =
    LazyLock::new(Default::default);

fn identity_key(forge: Forge, token: &str) -> String {
    format!("{}:{}", forge.name(), crate::corpus::hash_body(token))
}

/// The identity of the account a bound credential belongs to, from the
/// forge's own `/user`, resolved once per token. GitHub: the profile name
/// (the login when it has none) and the account's noreply address,
/// `<id>+<login>@users.noreply.github.com`. GitLab: the name and the
/// account's commit address, or its public one.
pub async fn identity(forge: Forge, env: &BTreeMap<String, String>) -> Result<Identity, String> {
    let token = forge.token(env).ok_or_else(|| {
        format!(
            "credential {} has no token the forge accepts",
            forge.credential()
        )
    })?;
    let key = identity_key(forge, token);
    if let Some(known) = IDENTITIES.lock().unwrap().get(&key) {
        return Ok(known.clone());
    }
    let http = forge_listing_http()?;
    let (api, host) = forge.endpoints(env);
    let resolved = match forge {
        Forge::Github => {
            let (v, _) = get(
                http,
                &format!("{api}/user"),
                &[
                    ("authorization", &format!("Bearer {token}")),
                    ("user-agent", USER_AGENT),
                    ("accept", "application/vnd.github+json"),
                    ("x-github-api-version", "2022-11-28"),
                ],
            )
            .await?;
            github_identity(&v, &host)?
        }
        Forge::Gitlab => {
            let (v, _) = get(
                http,
                &format!("{api}/user"),
                &[("private-token", token.as_str())],
            )
            .await?;
            gitlab_identity(&v, &host)?
        }
    };
    IDENTITIES.lock().unwrap().insert(key, resolved.clone());
    Ok(resolved)
}

fn github_identity(v: &Value, host: &str) -> Result<Identity, String> {
    let login = v["login"]
        .as_str()
        .filter(|l| !l.is_empty())
        .ok_or("GitHub's /user named no login")?;
    let id = v["id"].as_u64().ok_or("GitHub's /user named no id")?;
    let name = v["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(login);
    // Enterprise Server hosts its noreply addresses under its own name.
    let noreply = if host == "github.com" {
        "users.noreply.github.com".to_string()
    } else {
        format!("users.noreply.{host}")
    };
    Ok(Identity {
        name: name.to_string(),
        email: format!("{id}+{login}@{noreply}"),
        login: login.to_string(),
        forge: "github",
    })
}

fn gitlab_identity(v: &Value, host: &str) -> Result<Identity, String> {
    let login = v["username"]
        .as_str()
        .filter(|l| !l.is_empty())
        .ok_or("GitLab's /user named no username")?;
    let name = v["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(login);
    let email = ["commit_email", "public_email"]
        .iter()
        .filter_map(|k| v[*k].as_str())
        .find(|e| e.contains('@'))
        .map(str::to_string)
        .or_else(|| {
            v["id"]
                .as_u64()
                .map(|id| format!("{id}-{login}@users.noreply.{host}"))
        })
        .ok_or("GitLab's /user named no address to commit as")?;
    Ok(Identity {
        name: name.to_string(),
        email,
        login: login.to_string(),
        forge: "gitlab",
    })
}

/// The identity to author a channel's work as on this node: the forge the
/// work is bound for when it is known, else the first forge with a bound
/// credential. `None` when no forge credential is bound or none answers;
/// the caller decides what that means, never a placeholder from here.
pub async fn identity_for(
    broker: &SharedBroker,
    channel: &str,
    node_id: &str,
    prefer: Option<Forge>,
) -> Option<Identity> {
    let order = match prefer {
        Some(Forge::Gitlab) => [Forge::Gitlab, Forge::Github],
        _ => [Forge::Github, Forge::Gitlab],
    };
    for forge in order {
        let env = broker
            .read()
            .unwrap()
            .env_for(forge.credential(), channel, node_id)
            .ok();
        let Some(env) = env else { continue };
        match identity(forge, &env).await {
            Ok(identity) => return Some(identity),
            Err(error) => {
                tracing::warn!(forge = forge.name(), %error, "could not resolve the forge identity")
            }
        }
    }
    None
}

/// The identity behind the credential bound for exactly `forge` on this
/// channel and node: the account a publication there pushes as.
pub async fn identity_on(
    broker: &SharedBroker,
    forge: Forge,
    channel: &str,
    node_id: &str,
) -> Result<Identity, String> {
    let env = broker
        .read()
        .unwrap()
        .env_for(forge.credential(), channel, node_id)
        .map_err(|e| e.to_string())?;
    identity(forge, &env).await
}

/// The host's own global Git identity, for when no forge identity is bound
/// or reachable: the operator's, as every Git outside the boundary already
/// commits with — never a name for tracon. `None` when the host has none.
pub async fn host_identity(git: &str) -> Option<Identity> {
    let git = crate::config::path_guard::program("publish.git", git)
        .inspect_err(|error| tracing::warn!(%error, "the host's git identity was not read"))
        .ok()?;
    let git = &git;
    let read = |key: &'static str| async move {
        let out = tokio::process::Command::new(git)
            .env("PATH", crate::config::path_guard::child_path())
            .args(["config", "--global", "--get", key])
            .output()
            .await
            .ok()?;
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !value.is_empty()).then_some(value)
    };
    let name = read("user.name").await?;
    let email = read("user.email").await?;
    Some(Identity {
        login: name.clone(),
        name,
        email,
        forge: "host",
    })
}

/// Where managed clones live, under the node's own state.
pub fn managed_root(state_dir: &Path) -> PathBuf {
    state_dir.join("repos")
}

/// `repo` as it really sits on disk, when that is a clone inside managed
/// storage, else why not. Both sides are canonicalised: a lexical prefix
/// check passes `<root>/../../elsewhere` and a symlink out of the root, and
/// what is done to a managed clone (`checkout --force`, `clean -ffdx`) must
/// never reach an operator's checkout that way. The root itself may sit
/// behind a symlink (`/home` to `/var/home`), so it is resolved too.
pub fn managed_clone(repo: &Path) -> Result<PathBuf, String> {
    let refuse = || {
        format!(
            "refusing {}: it is not a clone inside managed storage",
            repo.display()
        )
    };
    let root = managed_root(&crate::config::Config::state_dir())
        .canonicalize()
        .map_err(|_| refuse())?;
    let real = repo.canonicalize().map_err(|_| refuse())?;
    if real == root || !real.starts_with(&root) {
        return Err(refuse());
    }
    // A directory under the root that is not itself a repository's top level
    // (`<root>/github.com/<owner>`) would have Git search upward and reset
    // whatever checkout encloses the state directory. A `.git` that is a
    // gitfile or a symlink would send the reset to another repository's Git
    // directory. Only a clone's own `.git` directory will do.
    let own_git_dir = std::fs::symlink_metadata(real.join(".git"))
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false);
    if !own_git_dir {
        return Err(refuse());
    }
    Ok(real)
}

/// A Git command against a managed clone that `managed_clone` resolved,
/// pinned to that clone's own Git directory and working tree, so Git never
/// searches upward for an enclosing repository.
fn managed_git(
    repo: &Path,
    credential: &crate::git_remote::Credential<'_>,
) -> std::io::Result<tokio::process::Command> {
    let mut command = crate::git_remote::git_in("git", repo, FORGE_HOME, credential)?;
    command
        .arg("--git-dir")
        .arg(repo.join(".git"))
        .arg("--work-tree")
        .arg(repo);
    Ok(command)
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

/// The brokered token for node-side git against `repo_path`, or none when no
/// credential applies: a repo outside the managed root is not the node's to
/// authenticate for, and a broker refusal degrades to anonymous rather than
/// failing a public repo. What git does with the token is
/// `crate::git_remote`'s business, so there is exactly one way to pass it.
pub fn git_token_for(
    broker: &SharedBroker,
    state_dir: &Path,
    channel: &str,
    repo_path: &Path,
    node_id: &str,
) -> Option<(Forge, String)> {
    let host = managed_host(state_dir, repo_path)?;
    let broker = broker.read().unwrap();
    let env_of = |forge: Forge| broker.env_for(forge.credential(), channel, node_id).ok();
    let forge = resolve_forge(&host, env_of);
    let env = env_of(forge)?;
    forge.token(&env).map(|token| (forge, token.clone()))
}

/// The host a managed clone sits under. `None` for a path outside the
/// managed root.
fn managed_host(state_dir: &Path, repo_path: &Path) -> Option<String> {
    let root = managed_root(state_dir);
    // A clone named by its real path, when the state directory is reached
    // through a symlink, is under the real root instead.
    let real = root.canonicalize().ok();
    let rest = repo_path
        .strip_prefix(&root)
        .ok()
        .or_else(|| repo_path.strip_prefix(real.as_deref()?).ok())?;
    let host = rest.components().next()?;
    Some(host.as_os_str().to_string_lossy().into_owned())
}

/// The forge `host` is, as the node clones and authenticates for it: the
/// forge whose credential, as `env_of` hands it over, names `host` as its
/// clone host (`GH_HOST`, `GITLAB_HOST`, or the public forge when neither is
/// set), so an enterprise GitHub on a host of its own is still GitHub. Only
/// when no credential claims the host is it guessed from its name.
pub fn resolve_forge(
    host: &str,
    env_of: impl Fn(Forge) -> Option<BTreeMap<String, String>>,
) -> Forge {
    [Forge::Github, Forge::Gitlab]
        .into_iter()
        .find(|forge| {
            env_of(*forge).is_some_and(|env| forge.endpoints(&env).1.eq_ignore_ascii_case(host))
        })
        .unwrap_or_else(|| forge_for_host(host))
}

/// The forge a host is by its name alone, the last resort when no
/// credential says: a host that names GitHub is GitHub, and any other is a
/// GitLab.
pub fn forge_for_host(host: &str) -> Forge {
    match host.contains("github") {
        true => Forge::Github,
        false => Forge::Gitlab,
    }
}

/// Why a session named a repository the node will not start from.
pub const HOST_CHECKOUT_REFUSED: &str = "host checkouts are not a session source; name the \
     forge repository (github.com/owner/name), upload files, or run your own harness and use \
     submit_review";

/// The managed clone a new session starts from, for the repository it
/// names: a forge identity (`host/owner/name`), or an absolute path that
/// canonicalises under managed storage (old rows, and a client that still
/// sends the path it was given). Either way the answer is the absolute path
/// `<root>/<identity>`, so repository images, caches and `[[repo]]` matching
/// key on what they always did. The clone need not exist yet: the start
/// makes it. Anything else, an operator's own checkout above all, is
/// refused.
pub fn session_repo(state_dir: &Path, repo: &str) -> Result<PathBuf, String> {
    let given = Path::new(repo);
    let identity = if given.is_absolute() {
        managed_identity(state_dir, given).ok_or_else(|| HOST_CHECKOUT_REFUSED.to_string())?
    } else {
        lowercase_host(repo)
    };
    crate::mesh::recipe::valid_identity(&identity)?;
    let root = managed_root(state_dir);
    let dest = root.join(&identity);
    clone_conflict(&root, &dest)?;
    Ok(dest)
}

/// An identity with its host in lower case, as hosts are spelled in managed
/// storage (`GitHub.com/o/r` is `github.com/o/r`). The owner and name are
/// left as they were given: the forge's own spelling of those is only known
/// from asking it.
fn lowercase_host(identity: &str) -> String {
    match identity.split_once('/') {
        Some((host, rest)) => format!("{}/{rest}", host.to_ascii_lowercase()),
        None => identity.to_ascii_lowercase(),
    }
}

/// Why a clone may not be made at `dest`, under `root`, or may not be taken
/// for one: a directory there already that is not a clone (a GitLab group
/// holding clones, say) would be refused by `git clone`, and its contents
/// are someone's; and a clone inside another clone's tree
/// (`github.com/o/r/src`) is that clone's files, not a repository. An
/// existing clone, or an empty directory, is no conflict.
fn clone_conflict(root: &Path, dest: &Path) -> Result<(), String> {
    let identity = |path: &Path| {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    if let Some(clone) = dest
        .ancestors()
        .skip(1)
        .take_while(|dir| *dir != root && dir.starts_with(root))
        .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
    {
        return Err(format!(
            "{} is inside the clone of {} on this node, not a repository of its own",
            identity(dest),
            identity(clone)
        ));
    }
    let occupied = std::fs::symlink_metadata(dest).is_ok()
        && std::fs::symlink_metadata(dest.join(".git")).is_err()
        && std::fs::read_dir(dest).map_or(true, |mut entries| entries.next().is_some());
    if occupied {
        return Err(format!(
            "{} is a directory in managed storage that is not a clone (it may hold other \
             clones), so nothing is cloned there",
            identity(dest)
        ));
    }
    Ok(())
}

/// The forge identity of a path inside managed storage, `host/owner/name`.
/// A path that exists is judged by where it really leads, so a symlink out
/// of the root, or `..` back out of it, is not inside; one that does not
/// exist yet (a clone still to be made) is read as it is spelled, and any
/// `..` in it is refused. `None` for anything outside, and for the root.
pub fn managed_identity(state_dir: &Path, repo: &Path) -> Option<String> {
    let root = managed_root(state_dir);
    let rest = if repo.exists() {
        let real = repo.canonicalize().ok()?;
        real.strip_prefix(root.canonicalize().ok()?)
            .ok()?
            .to_path_buf()
    } else {
        let real_root = root.canonicalize().ok();
        repo.strip_prefix(&root)
            .ok()
            .or_else(|| repo.strip_prefix(real_root.as_deref()?).ok())?
            .to_path_buf()
    };
    let parts: Vec<&str> = rest
        .components()
        .map(|part| match part {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// What a new `[[repo]]` entry for `repo` is written as: a managed clone by
/// its forge identity, which matches the clone's path as a suffix and says
/// which repository it is to whoever reads node.toml; anything else by its
/// path, as it was given.
pub fn entry_path(state_dir: &Path, repo: &Path) -> PathBuf {
    managed_identity(state_dir, repo)
        .filter(|identity| crate::mesh::recipe::valid_identity(identity).is_ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.to_path_buf())
}

/// Why a clone could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneError {
    /// The repository is named in a way the node will not make a path from.
    Name(String),
    /// The channel has no forge credential it may use here.
    Credential(String),
    TimedOut,
    Failed(String),
}

impl std::fmt::Display for CloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(why) | Self::Credential(why) | Self::Failed(why) => f.write_str(why),
            Self::TimedOut => f.write_str("the clone ran out of time"),
        }
    }
}

/// The managed clone of `host/owner/name`, made now with the channel's forge
/// credential when it is not on disk yet: the picker's clone of a repository
/// its forge listed, so a channel with no credential it may use for that
/// forge is refused. A credential without a token clones anonymously;
/// nothing else on the host may answer for it.
pub async fn ensure_clone(
    broker: &SharedBroker,
    channel: &str,
    node_id: &str,
    forge: Forge,
    parts: (&str, &str, &str),
) -> Result<PathBuf, CloneError> {
    let token = {
        let broker = broker.read().unwrap();
        let env = broker
            .env_for(forge.credential(), channel, node_id)
            .map_err(|e| CloneError::Credential(e.to_string()))?;
        forge.token(&env).cloned()
    };
    make_clone(forge, token.as_deref(), parts).await
}

/// The managed clone a session starts from, for a repository named by its
/// forge identity, made now when it is not on disk yet. The forge is the one
/// the channel's credentials say the host is (`resolve_forge`). A channel
/// with no credential it may use here clones anonymously, as a sync of the
/// clone then fetches (`git_token_for`), so a public repository needs none.
pub async fn ensure_clone_of(
    broker: &SharedBroker,
    channel: &str,
    node_id: &str,
    identity: &str,
) -> Result<PathBuf, CloneError> {
    crate::mesh::recipe::valid_identity(identity).map_err(CloneError::Name)?;
    let (host, rest) = identity
        .split_once('/')
        .ok_or_else(|| CloneError::Name(format!("{identity:?} names no repository")))?;
    let (owner, name) = rest
        .rsplit_once('/')
        .ok_or_else(|| CloneError::Name(format!("{identity:?} names no owner")))?;
    let (forge, token) = {
        let broker = broker.read().unwrap();
        let env_of = |forge: Forge| broker.env_for(forge.credential(), channel, node_id).ok();
        let forge = resolve_forge(host, env_of);
        (
            forge,
            env_of(forge).and_then(|env| forge.token(&env).cloned()),
        )
    };
    make_clone(forge, token.as_deref(), (host, owner, name)).await
}

/// Make the managed clone of `host/owner/name` unless it is there already.
/// Made under the clone's lock, so two sessions asking for one repository at
/// once make one clone, and neither seeds from it halfway. A directory there
/// that is not a clone, or a clone enclosing the destination, is refused
/// before anything runs.
async fn make_clone(
    forge: Forge,
    token: Option<&str>,
    (host, owner, name): (&str, &str, &str),
) -> Result<PathBuf, CloneError> {
    let root = managed_root(&crate::config::Config::state_dir());
    let dest = clone_dest(&root, host, owner, name).map_err(CloneError::Name)?;
    clone_conflict(&root, &dest).map_err(CloneError::Name)?;
    let credential = crate::git_remote::brokered(forge.git_user(), token);
    // Keyed by where the clone will really be, which is the key a sync takes
    // once it exists, even when the root sits behind a symlink.
    std::fs::create_dir_all(&root).map_err(|e| CloneError::Failed(e.to_string()))?;
    let key = root
        .canonicalize()
        .ok()
        .and_then(|real| Some(real.join(dest.strip_prefix(&root).ok()?)))
        .unwrap_or_else(|| dest.clone());
    let lock = clone_lock(&key);
    let _guard = lock.lock().await;
    // Asked again under the lock: another clone may have been made around
    // this one while it waited.
    clone_conflict(&root, &dest).map_err(CloneError::Name)?;
    tokio::time::timeout(
        Duration::from_secs(600),
        clone(&credential, host, owner, name, &dest),
    )
    .await
    .map_err(|_| CloneError::TimedOut)?
    .map_err(CloneError::Failed)?;
    Ok(dest)
}

/// Fetch a managed clone before an image is built from its default branch,
/// so a build the operator asks for builds what the forge holds now rather
/// than what the last session happened to fetch. A build has no channel, so
/// the forge credential is the node's (`Broker::env_for_node`); without one
/// the fetch is anonymous. A host checkout is the operator's own and is left
/// as it is: `Ok(false)`.
pub async fn refresh_for_build(
    broker: &SharedBroker,
    repo_path: &Path,
    node_id: &str,
) -> Result<bool, String> {
    let Some(host) = managed_host(&crate::config::Config::state_dir(), repo_path) else {
        return Ok(false);
    };
    let (forge, token) = {
        let broker = broker.read().unwrap();
        let env_of = |forge: Forge| broker.env_for_node(forge.credential(), node_id).ok();
        let forge = resolve_forge(&host, env_of);
        (
            forge,
            env_of(forge).and_then(|env| forge.token(&env).cloned()),
        )
    };
    let credential = crate::git_remote::brokered(forge.git_user(), token.as_deref());
    // Named under the root but resolving outside it is refused, not taken
    // for a host checkout: nothing legitimate is spelled that way.
    let repo_path = &managed_clone(repo_path)?;
    let lock = clone_lock(repo_path);
    let _guard = lock.lock().await;
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        fetch_managed(repo_path, &credential),
    )
    .await
    .map_err(|_| "git fetch ran out of time".to_string())??;
    Ok(true)
}

/// One writer at a time per managed clone: a session seeding from it while
/// another syncs it, or a build fetching it, would otherwise read a tree
/// halfway between two commits. Keyed by where the clone really is, so two
/// spellings of one path share a lock.
pub fn clone_lock(repo: &Path) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    type Locks = std::sync::Mutex<
        std::collections::HashMap<PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>,
    >;
    static LOCKS: LazyLock<Locks> = LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap()
        .entry(repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf()))
        .or_default()
        .clone()
}

/// What a managed clone was brought to before a session seeded from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synced {
    /// The forge's default branch, as `origin/HEAD` names it.
    pub default: String,
    /// The forge branch the session starts from: the default, or the branch
    /// the session named when the forge has it.
    pub source: String,
    /// The clone's remote-tracking ref for `source` when that is not the
    /// default (`refs/remotes/origin/<branch>`), for the seed to start from.
    /// The clone's own working tree stays on the default branch either way.
    pub source_ref: Option<String>,
    /// The commit the session starts from: `source`'s tip as last fetched.
    pub commit: String,
    /// When the clone last fetched from its forge successfully, as the node
    /// recorded it (`FETCHED_AT`); `None` before the node has.
    pub fetched_ms: Option<i64>,
    /// Why the fetch failed, when it did: the clone was brought to the
    /// `origin/<default>` it fetched last time instead.
    pub stale: Option<String>,
}

/// Bring a managed clone's default branch and working tree to what its forge
/// holds now: fetch, follow the forge's default branch, reset the local
/// branch onto it and remove anything the tree gathered. The clone is the
/// node's own, so nothing in it is anyone's work to keep. A fetch that fails
/// does not fail the session: the clone goes to the `origin/<default>` it
/// last fetched, and `stale` says why. A path that is not a clone inside
/// managed storage is refused before any Git command runs, and that refusal
/// fails the start: it is never a stale fetch. Callers hold `clone_lock`.
///
/// `branch`, the branch a session names, is what the session starts from
/// instead when the forge has a branch of that name (continuing an open
/// change's branch): `source_ref` names it for the seed, and the clone's
/// own tree and branches stay as the default left them, so whatever reads
/// the clone as the repository (a preview, image detection) still reads
/// the default, and no local branch is made that a later forge branch could
/// clash with. A name the forge has no branch for is a new branch, and
/// starts from the default. Only a branch name will do: a revision such as
/// `main~3` is not one, so no caller chooses an arbitrary commit this way.
pub async fn sync_managed(
    repo: &Path,
    credential: &crate::git_remote::Credential<'_>,
    branch: Option<&str>,
) -> Result<Synced, String> {
    let repo = &managed_clone(repo)?;
    let fetched = tokio::time::timeout(Duration::from_secs(300), fetch_managed(repo, credential))
        .await
        .unwrap_or_else(|_| Err("git fetch ran out of time".into()));
    let stale = fetched.err();
    if stale.is_none() {
        // The forge may have renamed its default branch since the clone was
        // made. Best effort: a failure leaves the `origin/HEAD` it had.
        // It asks the forge, so it carries the fetch's credential.
        if let Ok(mut command) = managed_git(repo, credential) {
            command
                .args(["remote", "set-head", "origin", "--auto"])
                .kill_on_drop(true);
            let _ = tokio::time::timeout(Duration::from_secs(60), command.output()).await;
        }
    }
    let head = local_git(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .await
    .map_err(|e| format!("the clone has no origin/HEAD to start from: {e}"))?;
    let default = head
        .strip_prefix("origin/")
        .filter(|name| !name.is_empty() && !name.starts_with('-'))
        .ok_or_else(|| format!("origin/HEAD names {head:?}, not a branch of origin"))?
        .to_string();
    let remote = format!("refs/remotes/origin/{default}");
    local_git(repo, &["checkout", "--force", "-B", &default, &remote]).await?;
    local_git(repo, &["clean", "-ffdx"]).await?;
    let mut source = default.clone();
    let mut source_ref = None;
    if let Some(named) = branch.filter(|named| *named != default && !named.starts_with('-')) {
        let local = format!("refs/heads/{named}");
        let remote = format!("refs/remotes/origin/{named}");
        let on_forge = local_git(repo, &["check-ref-format", &local]).await.is_ok()
            && local_git(repo, &["show-ref", "--verify", "--quiet", &remote])
                .await
                .is_ok();
        if on_forge {
            source = named.to_string();
            source_ref = Some(remote);
        }
    }
    let commit = match &source_ref {
        Some(remote) => {
            local_git(
                repo,
                &["rev-parse", "--verify", &format!("{remote}^{{commit}}")],
            )
            .await?
        }
        None => local_git(repo, &["rev-parse", "HEAD"]).await?,
    };
    let fetched_ms = std::fs::read_to_string(repo.join(".git").join(FETCHED_AT))
        .ok()
        .and_then(|at| at.trim().parse().ok());
    Ok(Synced {
        default,
        source,
        source_ref,
        commit,
        fetched_ms,
        stale,
    })
}

/// A Git command against a managed clone that never leaves the disk, so it
/// needs no credential, with its trimmed output.
async fn local_git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let anonymous = crate::git_remote::Credential::Anonymous;
    let mut command = managed_git(repo, &anonymous).map_err(|e| e.to_string())?;
    command.args(args).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .map_err(|_| format!("git {} ran out of time", args[0]))?
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The home `$HOME` points at for forge-side Git commands.
pub const FORGE_HOME: &str = "forge-home";

/// Clone into the managed root. Idempotent: an existing clone is the answer,
/// not an error. The URL carries no credential; the brokered helper does, and
/// `git_remote` is what decides that no other helper can answer instead.
///
/// The clone is made in a hidden sibling of `dest` and renamed into place
/// only once Git has finished, so `dest` never holds a clone halfway made: a
/// `.git` there is always a whole one. Git and its helpers die with the
/// future, and the sibling is removed whichever way the clone ends, a
/// timeout or a cancelled start included. Nothing already at `dest` is removed: a
/// directory there that is not a clone is refused.
pub async fn clone(
    credential: &crate::git_remote::Credential<'_>,
    host: &str,
    owner: &str,
    name: &str,
    dest: &Path,
) -> Result<(), String> {
    if dest.join(".git").exists() {
        return Ok(());
    }
    let occupied = std::fs::read_dir(dest).is_ok_and(|mut entries| entries.next().is_some());
    if occupied || (std::fs::symlink_metadata(dest).is_ok() && !dest.is_dir()) {
        return Err(format!(
            "refusing to clone into {}: something that is not a clone is there",
            dest.display()
        ));
    }
    let parent = dest
        .parent()
        .ok_or_else(|| format!("refusing to clone into {}", dest.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let leaf = dest
        .file_name()
        .map(|leaf| leaf.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Dot-named, so the picker's walk (`managed_repos`) never lists it.
    let partial =
        Partial(parent.join(format!(".{leaf}.cloning-{}", uuid::Uuid::now_v7().simple())));
    let url = format!("https://{host}/{owner}/{name}.git");
    let mut cmd =
        crate::git_remote::git("git", FORGE_HOME, credential).map_err(|e| e.to_string())?;
    cmd.args(["clone", "--no-local", "--no-hardlinks", &url])
        .arg(&partial.0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Its own group, so a clone given up takes its transport helper
    // (`git-remote-https`, which holds the connection) down with it.
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut group = Group(child.id());
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    group.0 = None;
    if !out.status.success() {
        return Err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // An empty directory at `dest` is replaced; anything else was refused.
    std::fs::rename(&partial.0, dest).map_err(|e| e.to_string())?;
    Ok(())
}

/// A clone being made, removed when it is dropped: by then it was either
/// renamed into place, and there is nothing left to remove, or it never
/// finished.
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The process group of a clone still running, killed when it is dropped:
/// the clone's future was given up before Git finished. `None` once it has.
struct Group(Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.0.and_then(|pid| i32::try_from(pid).ok()) {
            // SAFETY: `kill` takes plain integers and touches no memory of
            // ours. A negative pid addresses the group the clone leads.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
}

/// A fetch that succeeds records when, in this file under the clone's
/// `.git`, for a stale start to say how old its base is. `FETCH_HEAD`'s
/// mtime will not do: a fetch that fails partway can still write it.
pub const FETCHED_AT: &str = "tracon-fetched-ms";

/// Refresh a trusted managed clone with broker-provided Git authentication.
/// This is intentionally unavailable to agent workspaces: it only accepts a
/// node-owned repository path, and runs with no ambient credential helpers.
/// The fetch dies with its future, so a caller's timeout leaves nothing
/// rewriting refs after the clone's lock is released.
pub async fn fetch_managed(
    repo: &Path,
    credential: &crate::git_remote::Credential<'_>,
) -> Result<(), String> {
    let repo = &managed_clone(repo)?;
    let mut command = managed_git(repo, credential).map_err(|e| e.to_string())?;
    command
        .args(["fetch", "--prune", "origin"])
        .kill_on_drop(true);
    let out = command.output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        let _ = std::fs::write(
            repo.join(".git").join(FETCHED_AT),
            crate::store::now_ms().to_string(),
        );
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
    fn a_github_account_commits_as_its_noreply_address() {
        let v = serde_json::json!({ "login": "ada", "id": 42, "name": "Ada Lovelace" });
        let identity = github_identity(&v, "github.com").unwrap();
        assert_eq!(identity.name, "Ada Lovelace");
        assert_eq!(identity.email, "42+ada@users.noreply.github.com");
        // No profile name: the login, never a placeholder.
        let v = serde_json::json!({ "login": "ada", "id": 42, "name": null });
        assert_eq!(github_identity(&v, "github.com").unwrap().name, "ada");
        let v = serde_json::json!({ "login": "ada", "id": 42 });
        assert_eq!(
            github_identity(&v, "ghe.example").unwrap().email,
            "42+ada@users.noreply.ghe.example"
        );
    }

    #[test]
    fn a_gitlab_account_commits_as_its_commit_address() {
        let v = serde_json::json!({
            "username": "ada", "id": 7, "name": "Ada",
            "commit_email": "ada@example.com", "public_email": "other@example.com"
        });
        assert_eq!(
            gitlab_identity(&v, "gitlab.com").unwrap().email,
            "ada@example.com"
        );
        let v =
            serde_json::json!({ "username": "ada", "id": 7, "name": "Ada", "public_email": "" });
        assert_eq!(
            gitlab_identity(&v, "gitlab.com").unwrap().email,
            "7-ada@users.noreply.gitlab.com"
        );
    }

    #[test]
    fn a_display_name_cannot_add_a_gitconfig_key() {
        let identity = Identity {
            name: "Ada\n\temail = evil@example.com \"quoted\" \\".into(),
            email: "a@b".into(),
            login: "ada".into(),
            forge: "github",
        };
        assert_eq!(
            identity.gitconfig(),
            "[user]\n\tname = \"Adaemail = evil@example.com \\\"quoted\\\" \\\\\"\n\temail = \"a@b\"\n"
        );
    }

    #[test]
    fn a_canonical_remote_names_the_repository_the_forge_tools_take() {
        let github = ForgeRepo::from_remote("github.com/cosmicspork/tracon").unwrap();
        assert_eq!(
            (github.forge, github.path.as_str(), github.argument()),
            (Forge::Github, "cosmicspork/tracon", "repo")
        );
        let gitlab = ForgeRepo::from_remote("gitlab.example.com/group/sub/app").unwrap();
        assert_eq!(
            (gitlab.forge, gitlab.path.as_str(), gitlab.argument()),
            (Forge::Gitlab, "group/sub/app", "project")
        );
        // An unknown host, the directory-name fallback, and a path the tool
        // would refuse name nothing.
        for remote in [
            "git.example.com/o/r",
            "local/tracon",
            "github.com/o",
            "github.com/o/r/extra",
            "gitlab.com/solo",
        ] {
            assert_eq!(ForgeRepo::from_remote(remote), None, "{remote}");
        }
    }

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

    /// `tracon repo build` builds what the forge's default branch holds now:
    /// a managed clone is fetched first, so a Dockerfile change merged since
    /// the last session is what `default_commit` reads.
    #[tokio::test]
    async fn a_managed_clone_is_fetched_before_a_build_and_a_host_checkout_is_not() {
        let sh = |dir: &Path, script: &str| {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let upstream = tempfile::tempdir().unwrap();
        sh(
            upstream.path(),
            "git init -q -b main && git -c user.name=t -c user.email=t@t commit -q \
             --allow-empty -m one",
        );
        let owner = format!("refresh-{}", uuid::Uuid::now_v7().simple());
        let clone = managed_root(&crate::config::Config::state_dir())
            .join("github.com")
            .join(&owner)
            .join("proj");
        std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
        sh(
            clone.parent().unwrap(),
            &format!("git clone -q {} proj", upstream.path().display()),
        );
        let merged = sh(
            upstream.path(),
            "git -c user.name=t -c user.email=t@t commit -q --allow-empty -m two \
             && git rev-parse HEAD",
        );
        let broker = crate::broker::Broker::default().shared();

        assert_eq!(refresh_for_build(&broker, &clone, "n1").await, Ok(true));
        assert_eq!(sh(&clone, "git rev-parse origin/main"), merged);

        let host = tempfile::tempdir().unwrap();
        assert_eq!(
            refresh_for_build(&broker, host.path(), "n1").await,
            Ok(false)
        );
        let _ = std::fs::remove_dir_all(clone.parent().unwrap());
    }

    fn sh(dir: &Path, script: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare upstream with one commit on `main`, a managed clone of it, and
    /// a working copy to advance the upstream from.
    fn managed_fixture(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        sh(
            tmp.path(),
            "git init -q --bare -b main upstream.git && git clone -q upstream.git work \
             && cd work && echo one > a.txt && git add a.txt \
             && git -c user.name=t -c user.email=t@t commit -qm one && git push -q origin main",
        );
        let owner = format!("{tag}-{}", uuid::Uuid::now_v7().simple());
        let clone = managed_root(&crate::config::Config::state_dir())
            .join("github.com")
            .join(owner)
            .join("proj");
        std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
        sh(
            clone.parent().unwrap(),
            &format!(
                "git clone -q {} proj",
                tmp.path().join("upstream.git").display()
            ),
        );
        (tmp, clone)
    }

    fn advance(tmp: &Path, text: &str) -> String {
        sh(
            &tmp.join("work"),
            &format!(
                "echo {text} > a.txt && git -c user.name=t -c user.email=t@t commit -qam {text} \
                 && git push -q origin main && git rev-parse HEAD"
            ),
        )
    }

    /// A session on a managed clone starts from what the forge's default
    /// branch holds now, not from wherever the clone's own `main` was left
    /// when it was made: the workspace's HEAD, its `origin/main` and its files
    /// are all the upstream tip.
    #[tokio::test]
    async fn a_session_seeded_from_a_managed_clone_starts_at_the_upstream_tip() {
        let (tmp, clone) = managed_fixture("sync");
        let tip = advance(tmp.path(), "two");
        // An edit left in the clone's tree is the node's, and the sync
        // puts it back.
        std::fs::write(clone.join("a.txt"), "edited in place").unwrap();

        let lock = clone_lock(&clone);
        let _guard = lock.lock().await;
        let synced = sync_managed(&clone, &crate::git_remote::Credential::Anonymous, None)
            .await
            .unwrap();
        assert_eq!(synced.default, "main");
        assert_eq!(synced.commit, tip);
        assert_eq!(synced.stale, None);
        assert!(synced.fetched_ms.is_some());
        // Written after the sync, so only the seed can keep it out: a
        // managed clone's untracked files are not overlaid.
        std::fs::write(clone.join("stray.txt"), "left behind").unwrap();
        let id = format!("seed-sync-{}", uuid::Uuid::now_v7());
        let workspace = crate::workspace::seed_from_clone("git", &clone, "feat/x", None, &id)
            .await
            .unwrap();

        let stage = crate::workspace::staging_path(&workspace.id);
        assert_eq!(sh(&stage, "git rev-parse HEAD"), tip);
        assert_eq!(sh(&stage, "git rev-parse origin/main"), tip);
        assert_eq!(
            std::fs::read_to_string(stage.join("a.txt")).unwrap(),
            "two\n"
        );
        assert!(!stage.join("stray.txt").exists());
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::remove_dir_all(clone.parent().unwrap());
    }

    /// A forge that cannot be reached does not fail the session: the clone
    /// goes to the `origin/main` it fetched last, which is newer than its own
    /// `main`, and says the start is stale.
    #[tokio::test]
    async fn a_failed_fetch_starts_from_the_last_fetched_default_branch_and_says_so() {
        let (tmp, clone) = managed_fixture("stale");
        let fetched = advance(tmp.path(), "two");
        fetch_managed(&clone, &crate::git_remote::Credential::Anonymous)
            .await
            .unwrap();
        let at = std::fs::read_to_string(clone.join(".git").join(FETCHED_AT)).unwrap();
        sh(
            &clone,
            "git remote set-url origin /nonexistent/upstream.git",
        );

        let synced = sync_managed(&clone, &crate::git_remote::Credential::Anonymous, None)
            .await
            .unwrap();
        assert_eq!(synced.commit, fetched);
        assert!(synced.stale.is_some());
        // When the last fetch that succeeded ran, not when this one failed.
        assert_eq!(synced.fetched_ms, at.trim().parse().ok());
        assert_eq!(sh(&clone, "git rev-parse main"), fetched);
        let _ = std::fs::remove_dir_all(clone.parent().unwrap());
    }

    /// A forge that renamed its default branch since the clone was made: the
    /// sync follows `origin/HEAD` to the new name rather than resetting the
    /// old branch.
    #[tokio::test]
    async fn a_renamed_default_branch_is_followed_to_its_new_name() {
        let (tmp, clone) = managed_fixture("renamed");
        let tip = advance(tmp.path(), "two");
        sh(
            &tmp.path().join("upstream.git"),
            "git branch -m main trunk && git symbolic-ref HEAD refs/heads/trunk",
        );

        let synced = sync_managed(&clone, &crate::git_remote::Credential::Anonymous, None)
            .await
            .unwrap();
        assert_eq!(synced.default, "trunk");
        assert_eq!(synced.commit, tip);
        assert_eq!(synced.stale, None);
        assert_eq!(sh(&clone, "git branch --show-current"), "trunk");
        let _ = std::fs::remove_dir_all(clone.parent().unwrap());
    }

    /// A session that names a branch the forge has (continuing an open
    /// change) starts from the forge's tip of that branch; the default branch
    /// is still brought up to date beside it. A name the forge has no branch
    /// for, or a revision rather than a name, starts from the default. The
    /// clone's own tree stays on the default branch throughout, with no
    /// local branch made for the named one, so a local branch that would
    /// clash with the forge's (`fix` beside `fix/thing`) is no obstacle.
    #[tokio::test]
    async fn a_named_forge_branch_is_started_from_and_the_clone_stays_on_the_default() {
        let (tmp, clone) = managed_fixture("branch");
        let pr = sh(
            &tmp.path().join("work"),
            "git checkout -q -b fix/thing && echo fix > a.txt \
             && git -c user.name=t -c user.email=t@t commit -qam fix \
             && git push -q origin fix/thing && git rev-parse HEAD && git checkout -q main",
        );
        let tip = advance(tmp.path(), "two");
        sh(&clone, "git branch fix");
        let anonymous = crate::git_remote::Credential::Anonymous;

        let synced = sync_managed(&clone, &anonymous, Some("fix/thing"))
            .await
            .unwrap();
        assert_eq!(synced.source, "fix/thing");
        assert_eq!(
            synced.source_ref.as_deref(),
            Some("refs/remotes/origin/fix/thing")
        );
        assert_eq!(synced.commit, pr);
        assert_eq!(sh(&clone, "git rev-parse main"), tip);
        assert_eq!(sh(&clone, "git rev-parse HEAD"), tip);
        assert_eq!(sh(&clone, "git branch --show-current"), "main");
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "two\n"
        );
        assert_eq!(
            sh(
                &clone,
                "git for-each-ref '--format=%(refname:short)' refs/heads"
            ),
            "fix\nmain"
        );

        let id = format!("seed-named-{}", uuid::Uuid::now_v7());
        let workspace = crate::workspace::seed_from_clone(
            "git",
            &clone,
            "fix/thing",
            synced.source_ref.as_deref(),
            &id,
        )
        .await
        .unwrap();
        let stage = crate::workspace::staging_path(&workspace.id);
        assert_eq!(sh(&stage, "git rev-parse HEAD"), pr);
        assert_eq!(sh(&stage, "git branch --show-current"), "fix/thing");
        assert_eq!(
            std::fs::read_to_string(stage.join("a.txt")).unwrap(),
            "fix\n"
        );
        let _ = std::fs::remove_dir_all(&stage);

        for other in ["feat/new-work", "main~1", "fix/thing~1"] {
            let synced = sync_managed(&clone, &anonymous, Some(other)).await.unwrap();
            assert_eq!(synced.source, "main", "{other}");
            assert_eq!(synced.source_ref, None, "{other}");
            assert_eq!(synced.commit, tip, "{other}");
        }
        let _ = std::fs::remove_dir_all(clone.parent().unwrap());
    }

    /// A session names its repository by its forge identity, or by a path
    /// already inside managed storage; either way the row gets the clone's
    /// absolute path, made or not. A host checkout and a path that climbs
    /// out of the root are refused with the same words; an identity that is
    /// malformed is refused with what is wrong with it.
    #[test]
    fn a_session_repository_is_a_forge_identity_or_a_managed_path() {
        let state = tempfile::tempdir().unwrap();
        let root = managed_root(state.path());
        std::fs::create_dir_all(root.join("github.com/me/made/.git")).unwrap();
        let host = tempfile::tempdir().unwrap();
        assert_eq!(
            session_repo(state.path(), "github.com/me/proj"),
            Ok(root.join("github.com/me/proj"))
        );
        assert_eq!(
            session_repo(state.path(), "gitlab.example/group/sub/tool"),
            Ok(root.join("gitlab.example/group/sub/tool"))
        );
        for path in [
            root.join("github.com/me/made"),
            root.join("github.com/me/not-yet"),
        ] {
            assert_eq!(
                session_repo(state.path(), &path.to_string_lossy()),
                Ok(path.clone())
            );
        }
        assert_eq!(
            session_repo(state.path(), "GitHub.com/Me/proj"),
            Ok(root.join("github.com/Me/proj")),
            "the host is lowercased; the owner and name are as given"
        );
        for malformed in [
            ".",
            "src/proj",
            "github.com/me",
            "https://github.com/me/proj",
            "github.com/me/proj/",
        ] {
            let error = session_repo(state.path(), malformed).unwrap_err();
            assert_eq!(
                Err(error.clone()),
                crate::mesh::recipe::valid_identity(malformed),
                "{malformed}"
            );
            assert!(error.contains("forge identity"), "{error}");
        }
        for refused in [
            host.path().to_string_lossy().into_owned(),
            root.to_string_lossy().into_owned(),
            root.join("github.com/me/../../../../etc")
                .to_string_lossy()
                .into_owned(),
            root.join("github.com/../escape/x")
                .to_string_lossy()
                .into_owned(),
        ] {
            assert_eq!(
                session_repo(state.path(), &refused),
                Err(HOST_CHECKOUT_REFUSED.to_string()),
                "{refused}"
            );
        }
        #[cfg(unix)]
        {
            let link = root.join("github.com/me/link");
            std::os::unix::fs::symlink(host.path(), &link).unwrap();
            assert_eq!(
                session_repo(state.path(), &link.to_string_lossy()),
                Err(HOST_CHECKOUT_REFUSED.to_string())
            );
        }
    }

    /// A checkout outside managed storage with work in it that is not
    /// committed: an edit, an untracked file and an ignored one.
    fn host_checkout(tmp: &Path) -> PathBuf {
        let host = tmp.join("host");
        sh(
            tmp,
            "git init -q -b main host && cd host && echo one > a.txt \
             && echo ignored.txt > .gitignore && git add a.txt .gitignore \
             && git -c user.name=t -c user.email=t@t commit -qm one \
             && echo mine > a.txt && echo untracked > new.txt && echo kept > ignored.txt",
        );
        host
    }

    fn assert_untouched(host: &Path) {
        assert_eq!(
            std::fs::read_to_string(host.join("a.txt")).unwrap(),
            "mine\n"
        );
        assert!(host.join("new.txt").exists());
        assert!(host.join("ignored.txt").exists());
    }

    /// A path spelled under the managed root that climbs out of it with `..`
    /// passes a lexical prefix check. It is refused before any Git command
    /// runs, as a refusal and not as a stale fetch, and the checkout it
    /// reached keeps its uncommitted and ignored files.
    #[tokio::test]
    async fn a_path_that_climbs_out_of_managed_storage_is_refused_and_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host_checkout(tmp.path());
        let root = managed_root(&crate::config::Config::state_dir());
        std::fs::create_dir_all(&root).unwrap();
        // Enough `..` to reach `/` however deep the root really sits; more
        // stay at `/`.
        let mut climbing = root.clone();
        for _ in 0..root.canonicalize().unwrap().components().count() {
            climbing.push("..");
        }
        climbing.push(host.strip_prefix("/").unwrap());
        assert!(
            climbing.starts_with(&root),
            "the lexical check would pass it"
        );
        assert_eq!(
            climbing.canonicalize().unwrap(),
            host.canonicalize().unwrap()
        );

        let error = sync_managed(&climbing, &crate::git_remote::Credential::Anonymous, None)
            .await
            .unwrap_err();
        assert!(
            error.contains("not a clone inside managed storage"),
            "{error}"
        );
        let error = fetch_managed(&climbing, &crate::git_remote::Credential::Anonymous)
            .await
            .unwrap_err();
        assert!(
            error.contains("not a clone inside managed storage"),
            "{error}"
        );
        assert_untouched(&host);
    }

    /// The same through a symlink: a link inside managed storage to a
    /// checkout outside it is where the link leads, not where it sits.
    #[tokio::test]
    async fn a_symlink_out_of_managed_storage_is_refused_and_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host_checkout(tmp.path());
        let owner = format!("link-{}", uuid::Uuid::now_v7().simple());
        let link = managed_root(&crate::config::Config::state_dir())
            .join("github.com")
            .join(owner)
            .join("proj");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&host, &link).unwrap();

        let error = sync_managed(&link, &crate::git_remote::Credential::Anonymous, None)
            .await
            .unwrap_err();
        assert!(
            error.contains("not a clone inside managed storage"),
            "{error}"
        );
        assert_untouched(&host);
        let _ = std::fs::remove_dir_all(link.parent().unwrap());
    }

    /// Point a checkout's `origin/HEAD` at its own `main`, as a clone's is,
    /// so a sync that reached it would have something to reset it to.
    fn give_origin_head(checkout: &Path) {
        sh(
            checkout,
            "git update-ref refs/remotes/origin/main HEAD \
             && git symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main",
        );
    }

    /// What a repository's state is, to show nothing moved it: its branch,
    /// its commit and what its status reports.
    fn repo_state(checkout: &Path) -> String {
        sh(
            checkout,
            "git symbolic-ref HEAD && git rev-parse HEAD && git status --porcelain --ignored",
        )
    }

    /// A directory inside managed storage that is not a repository's top
    /// level, with a checkout enclosing it as one encloses a state directory
    /// kept under a worktree. Git would search upward from it and reset the
    /// enclosing checkout, so it is refused before any Git command runs.
    #[tokio::test]
    async fn a_plain_directory_inside_managed_storage_is_refused_and_its_enclosing_checkout_left_alone(
    ) {
        let parent = managed_root(&crate::config::Config::state_dir())
            .join("github.com")
            .join(format!("enclosed-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&parent).unwrap();
        let enclosing = host_checkout(&parent);
        give_origin_head(&enclosing);
        let owner = enclosing.join("owner");
        std::fs::create_dir_all(&owner).unwrap();
        std::fs::write(owner.join("work.txt"), "mine").unwrap();
        let before = repo_state(&enclosing);

        let error = sync_managed(&owner, &crate::git_remote::Credential::Anonymous, None)
            .await
            .unwrap_err();
        assert!(
            error.contains("not a clone inside managed storage"),
            "{error}"
        );
        let error = fetch_managed(&owner, &crate::git_remote::Credential::Anonymous)
            .await
            .unwrap_err();
        assert!(
            error.contains("not a clone inside managed storage"),
            "{error}"
        );
        // No fetch ran, so none was recorded, and nothing was reset or
        // cleaned.
        assert!(!enclosing.join(".git").join(FETCHED_AT).exists());
        assert_eq!(repo_state(&enclosing), before);
        assert_untouched(&enclosing);
        assert_eq!(
            std::fs::read_to_string(owner.join("work.txt")).unwrap(),
            "mine"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A directory under the root whose `.git` is a gitfile pointing at a
    /// repository outside managed storage: a reset there would move that
    /// repository's branch and index, so it is refused too.
    #[tokio::test]
    async fn a_gitfile_pointing_out_of_managed_storage_is_refused_and_its_repository_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host_checkout(tmp.path());
        give_origin_head(&host);
        let before = repo_state(&host);
        let clone = managed_root(&crate::config::Config::state_dir())
            .join("github.com")
            .join(format!("gitfile-{}", uuid::Uuid::now_v7().simple()))
            .join("proj");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(
            clone.join(".git"),
            format!("gitdir: {}\n", host.join(".git").display()),
        )
        .unwrap();

        let error = sync_managed(&clone, &crate::git_remote::Credential::Anonymous, None)
            .await
            .unwrap_err();
        assert!(
            error.contains("not a clone inside managed storage"),
            "{error}"
        );
        let error = fetch_managed(&clone, &crate::git_remote::Credential::Anonymous)
            .await
            .unwrap_err();
        assert!(
            error.contains("not a clone inside managed storage"),
            "{error}"
        );
        assert!(!host.join(".git").join(FETCHED_AT).exists());
        assert_eq!(repo_state(&host), before);
        assert_untouched(&host);
        let _ = std::fs::remove_dir_all(clone.parent().unwrap());
    }

    /// Two spellings of one clone are one clone, so they share its lock.
    #[test]
    fn two_spellings_of_one_clone_share_a_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("a");
        std::fs::create_dir_all(&clone).unwrap();
        let other = tmp.path().join("a").join("..").join("a");
        assert!(std::sync::Arc::ptr_eq(
            &clone_lock(&clone),
            &clone_lock(&other)
        ));
    }

    #[test]
    fn only_managed_repos_get_a_brokered_token() {
        let broker = crate::broker::Broker::default().shared();
        // Outside the managed root there is nothing for the node to
        // authenticate: that checkout is the operator's own.
        assert!(git_token_for(
            &broker,
            Path::new("/state"),
            "personal",
            Path::new("/elsewhere/src/project"),
            "n1",
        )
        .is_none());
        // Managed path but no credential in the broker: anonymous, not an error.
        assert!(git_token_for(
            &broker,
            Path::new("/state"),
            "personal",
            Path::new("/state/repos/github.com/me/proj"),
            "n1",
        )
        .is_none());
    }

    /// A clone is never made over what managed storage holds that is not a
    /// clone, and never inside another clone's tree: `git clone` would refuse
    /// either, and what is there is someone's. A GitLab group that holds a
    /// clone, and a subdirectory of a clone, are refused before anything
    /// runs, and both are left exactly as they were.
    #[tokio::test]
    async fn a_clone_over_a_group_of_clones_or_inside_a_clone_is_refused_and_nothing_is_removed() {
        let broker = crate::broker::Broker::default().shared();
        let state = crate::config::Config::state_dir();
        let root = managed_root(&state);
        let tag = uuid::Uuid::now_v7().simple().to_string();
        let group = root.join(format!("gitlab.example/g{tag}/sub"));
        std::fs::create_dir_all(group.join("p/.git")).unwrap();
        std::fs::write(group.join("p/a.txt"), "mine").unwrap();
        let clone = root.join(format!("github.com/o{tag}/r"));
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        std::fs::create_dir_all(clone.join("src")).unwrap();
        std::fs::write(clone.join("src/main.rs"), "mine").unwrap();

        for identity in [
            format!("gitlab.example/g{tag}/sub"),
            format!("github.com/o{tag}/r/src"),
            format!("github.com/o{tag}/r/src/deeper"),
        ] {
            let error = ensure_clone_of(&broker, "personal", "n1", &identity)
                .await
                .unwrap_err();
            assert!(matches!(error, CloneError::Name(_)), "{identity}: {error}");
            assert_eq!(
                session_repo(&state, &identity),
                Err(error.to_string()),
                "{identity}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(group.join("p/a.txt")).unwrap(),
            "mine"
        );
        assert!(group.join("p/.git").is_dir());
        assert_eq!(
            std::fs::read_to_string(clone.join("src/main.rs")).unwrap(),
            "mine"
        );
        assert!(clone.join(".git").is_dir());
        // The clones themselves are still what a session may name.
        for existing in [group.join("p"), clone.clone()] {
            let identity = managed_identity(&state, &existing).unwrap();
            assert_eq!(session_repo(&state, &identity), Ok(existing));
        }
        let _ = std::fs::remove_dir_all(root.join(format!("gitlab.example/g{tag}")));
        let _ = std::fs::remove_dir_all(root.join(format!("github.com/o{tag}")));
    }

    /// A clone cut short, by its timeout or by a start that is given up,
    /// leaves nothing to be taken for a finished clone later: its `git`
    /// dies with it, and neither the destination nor the half-made clone
    /// beside it stays on disk. A clone that fails leaves nothing either.
    #[tokio::test]
    async fn a_clone_cut_short_or_failed_leaves_nothing_behind_and_no_git_running() {
        let root = managed_root(&crate::config::Config::state_dir());
        let owner = root.join(format!("github.com/cut{}", uuid::Uuid::now_v7().simple()));
        let dest = owner.join("proj");
        // A host that takes the connection and never answers it: the clone
        // hangs in its TLS handshake until it is given up.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let (closed, hung_up) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            while matches!(std::io::Read::read(&mut conn, &mut buf), Ok(n) if n > 0) {}
            let _ = closed.send(());
        });
        let anonymous = crate::git_remote::Credential::Anonymous;

        let cut = tokio::time::timeout(
            Duration::from_secs(2),
            clone(&anonymous, &host, "owner", "proj", &dest),
        )
        .await;
        assert!(cut.is_err(), "the clone should still have been waiting");
        hung_up
            .recv_timeout(Duration::from_secs(10))
            .expect("git was still connected after its clone was given up");
        assert!(!dest.exists());
        let left: Vec<_> = std::fs::read_dir(&owner).unwrap().flatten().collect();
        assert!(left.is_empty(), "left behind: {left:?}");

        clone(&anonymous, "127.0.0.1:1", "owner", "proj", &dest)
            .await
            .unwrap_err();
        assert!(!dest.exists());
        let left: Vec<_> = std::fs::read_dir(&owner).unwrap().flatten().collect();
        assert!(left.is_empty(), "left behind: {left:?}");
        let _ = std::fs::remove_dir_all(&owner);
    }

    /// A host is the forge whose credential names it as its clone host, so
    /// an enterprise GitHub on a host of its own is GitHub and a GitLab on
    /// a host named after GitHub is GitLab. Only a host no credential
    /// claims is guessed from its name.
    #[test]
    fn a_host_is_the_forge_whose_credential_names_it_and_only_otherwise_guessed() {
        let only = |which: Forge, env: &[(&str, &str)]| {
            let env: BTreeMap<String, String> = env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |forge: Forge| (forge == which).then(|| env.clone())
        };
        let enterprise = only(Forge::Github, &[("GH_HOST", "code.acme.com")]);
        assert_eq!(resolve_forge("code.acme.com", &enterprise), Forge::Github);
        assert_eq!(resolve_forge("code.acme.com", |_| None), Forge::Gitlab);
        let mirror = only(
            Forge::Gitlab,
            &[("GITLAB_HOST", "https://github-mirror.acme.com")],
        );
        assert_eq!(
            resolve_forge("github-mirror.acme.com", &mirror),
            Forge::Gitlab
        );
        // A public GitHub credential claims github.com, and no other host.
        let public = only(Forge::Github, &[]);
        assert_eq!(resolve_forge("github.com", &public), Forge::Github);
        assert_eq!(resolve_forge("git.acme.com", &public), Forge::Gitlab);
    }
}
