//! The workspace's `origin`: a read-only Git remote served by the gateway.
//!
//! A session's workspace names `origin` as
//! `http://<gateway>/git/<channel>/<owner>/<name>.git`. The gateway speaks
//! Git's smart HTTP to the forge for it, with the channel's brokered token, so
//! a plain `git fetch`, `git pull` or a rebase onto a `main` that moved since
//! launch works from inside the sandbox while the token stays on the node:
//! the harness presents its own session token, which names the session and
//! nothing else, and the node attaches the forge credential on the way out.
//!
//! Only reads pass. `git-upload-pack` (the ref advertisement and the fetch
//! itself) is forwarded; `git-receive-pack` is answered here, at the
//! protocol, with a message pointing at `submit_review`, so publication stays
//! review-only however Git is invoked. A refusal is a `403` with a
//! `text/plain` body, which Git prints as `remote: …` before it gives up, so
//! it never reads as a network error.
//!
//! The gateway forwards one path for a session: its own repository, resolved
//! from the session's row on every request. Any other owner, name or channel
//! is refused before a credential is looked up, so the token cannot be used
//! to read the operator's other private repositories.
//!
//! Each fetch is recorded on the session (`git_fetch`) with the refs and
//! commit ids it asked for. LFS and submodules from other repositories are
//! not served: their URLs are not this path.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use serde_json::json;

use crate::http::api::AppState;
use crate::session::state::event_kind as ek;

/// What a push is answered with.
pub const READ_ONLY: &str = "origin is read-only. Work leaves this workspace through review, \
     never a push: commit on your branch and call the submit_review tool to publish it. \
     git fetch and git pull work as usual.";

/// The largest request body the gateway reads: a fetch's negotiation (its
/// `want` and `have` lines), never a pack, since nothing is pushed.
const MAX_REQUEST: usize = 64 * 1024 * 1024;

/// The largest ref listing the gateway reads whole to record a fetch by.
const MAX_LISTING: usize = 32 * 1024 * 1024;

const CONNECT: Duration = Duration::from_secs(30);

/// The three tails Git's smart HTTP adds to a repository's URL.
const INFO_REFS: &str = "/info/refs";
const UPLOAD_PACK: &str = "/git-upload-pack";
const RECEIVE_PACK: &str = "/git-receive-pack";

/// One path segment, percent-encoded where it is not unreserved, so a channel
/// name reaches the gateway exactly as it was written into the URL.
fn segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The path the gateway serves a repository at for a channel:
/// `/git/<channel>/<owner>/<name>.git`. `identity` is the managed clone's
/// `host/owner/name`; the owner may be a GitLab group path.
fn repo_path(channel: &str, identity: &str) -> Option<String> {
    let (_, path) = identity.split_once('/')?;
    if path.is_empty() || !path.contains('/') {
        return None;
    }
    Some(format!("/git/{}/{path}.git", segment(channel)))
}

/// The URL a workspace's `origin` names: the gateway at `host:port`, for
/// `identity` (`host/owner/name`) on `channel`.
pub fn origin_url(host: &str, port: u16, channel: &str, identity: &str) -> Option<String> {
    Some(format!(
        "http://{host}:{port}{}",
        repo_path(channel, identity)?
    ))
}

/// The origin a session on `repo` is given, when `repo` is a managed clone of
/// a forge repository: the only kind the gateway can serve.
pub fn origin_for(
    state_dir: &Path,
    repo: &Path,
    host: &str,
    port: u16,
    channel: &str,
) -> Option<String> {
    crate::forge::managed_clone(repo).ok()?;
    let identity = crate::forge::managed_identity(state_dir, repo)?;
    crate::mesh::recipe::valid_identity(&identity).ok()?;
    origin_url(host, port, channel, &identity)
}

/// A value quoted for a gitconfig file.
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The harness's global gitconfig lines that send the session's own
/// `token` to `url`, the workspace's `origin`, and nowhere else: the header
/// the gateway knows the session by. The forge's credential is not here: the
/// node attaches it on the way out.
pub fn gitconfig(url: &str, token: &str) -> String {
    format!(
        "[http {}]\n\textraHeader = {}\n",
        quoted(url),
        quoted(&format!("Authorization: Bearer {token}")),
    )
}

/// The `origin` section of a workspace's own `.git/config`, fetching every
/// branch into `origin/<branch>`. It is the workspace's, not the harness's
/// global configuration, because Git answers `git remote get-url` and
/// `set-url` only for a remote the repository itself configures.
fn origin_section(url: &str) -> String {
    format!("[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n")
}

/// Whether `url` is a gateway origin as [`origin_url`] spells one: plain
/// HTTP, a `/git/<channel>/<owner>/<name>.git` path, and nothing a gitconfig
/// value or a transport could read as anything more.
pub fn is_gateway_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let Some((authority, path)) = rest.split_once('/') else {
        return false;
    };
    let plain = |s: &str, extra: &str| {
        !s.is_empty()
            && s.bytes().all(|b| {
                b.is_ascii_alphanumeric() || b"-._~".contains(&b) || extra.as_bytes().contains(&b)
            })
    };
    let segments: Vec<&str> = path.split('/').collect();
    plain(authority, ":[]")
        && !authority.contains('@')
        && segments.len() >= 4
        && segments[0] == "git"
        && segments[1..]
            .iter()
            .all(|s| plain(s, "%") && !s.starts_with('.'))
        && segments
            .last()
            .is_some_and(|name| name.len() > 4 && name.ends_with(".git"))
}

/// Name `url` as the `origin` of the workspace at `root`, in its own
/// `.git/config`, replacing any `origin` it had. Only a gateway URL is
/// written.
pub fn set_workspace_origin(root: &Path, url: &str) -> std::io::Result<()> {
    if !is_gateway_url(url) {
        return Err(std::io::Error::other("not a gateway origin"));
    }
    let config = root.join(".git").join("config");
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    let mut kept = without_origin(&text);
    if !kept.is_empty() && !kept.ends_with('\n') {
        kept.push('\n');
    }
    kept.push_str(&origin_section(url));
    std::fs::write(config, kept)
}

/// A gitconfig's text with its `[remote "origin"]` section left out.
fn without_origin(text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            skipping = trimmed.replace(' ', "") == "[remote\"origin\"]";
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The gateway origin a workspace's `.git/config` text names, if it names
/// one: what the node keeps when it rewrites that file (`workspace::
/// sanitize_git`), so the remote outlives the tree's trips through the node.
/// Anything else an `origin` section held is dropped with the rest.
pub fn kept_origin(text: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed.replace(' ', "") == "[remote\"origin\"]";
            continue;
        }
        if !inside {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("url") {
            let value = value.trim().trim_matches('"');
            return is_gateway_url(value).then(|| value.to_string());
        }
    }
    None
}

/// The gitconfig section that keeps a gateway `origin`, for a rewritten
/// workspace configuration.
pub fn kept_section(url: &str) -> String {
    origin_section(url)
}

/// A refusal Git prints: a `text/plain` body is shown line by line as
/// `remote: …` after a failed ref advertisement.
fn refuse(status: StatusCode, message: &str) -> Response {
    (
        status,
        [("content-type", "text/plain; charset=utf-8")],
        format!("{message}\n"),
    )
        .into_response()
}

/// What a request asks of the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    /// `GET info/refs?service=git-upload-pack`.
    Advertise,
    /// `POST git-upload-pack`.
    UploadPack,
    /// Either half of a push.
    ReceivePack,
}

/// What `method` on `tail` (the path after the repository) with `query` is,
/// or why it is not served.
fn call_of(method: &Method, tail: &str, query: Option<&str>) -> Result<Call, &'static str> {
    let service = query.and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("service=")));
    match (method.as_str(), tail, service) {
        ("GET", INFO_REFS, Some("git-upload-pack")) => Ok(Call::Advertise),
        ("POST", UPLOAD_PACK, _) => Ok(Call::UploadPack),
        ("GET", INFO_REFS, Some("git-receive-pack")) | ("POST", RECEIVE_PACK, _) => {
            Ok(Call::ReceivePack)
        }
        _ => Err("the gateway serves Git's smart HTTP fetch (git-upload-pack) and nothing else"),
    }
}

/// The repository part of a request path and its smart-HTTP tail.
fn split_tail(path: &str) -> Option<(&str, &str)> {
    [INFO_REFS, UPLOAD_PACK, RECEIVE_PACK]
        .into_iter()
        .find_map(|tail| path.strip_suffix(tail).map(|repo| (repo, tail)))
}

/// The pkt-lines of a Git protocol body, as payloads: `None` for a flush,
/// delimiter or response-end packet. Stops at the first malformed length,
/// which for a fetch response is where the pack begins.
fn pkt_lines(mut body: &[u8]) -> Vec<Option<&[u8]>> {
    let mut out = Vec::new();
    while body.len() >= 4 {
        let Some(len) = std::str::from_utf8(&body[..4])
            .ok()
            .and_then(|hex| usize::from_str_radix(hex, 16).ok())
        else {
            break;
        };
        if len < 4 {
            out.push(None);
            body = &body[4..];
            continue;
        }
        if len > body.len() {
            break;
        }
        out.push(Some(&body[4..len]));
        body = &body[len..];
    }
    out
}

fn text(line: &[u8]) -> &str {
    std::str::from_utf8(line)
        .unwrap_or("")
        .trim_end_matches('\n')
}

fn hex_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The refs a listing names, `(ref, sha)`: a protocol v0 advertisement
/// (`<sha> <ref>\0<capabilities>` after the service line) or a v2 `ls-refs`
/// answer (`<sha> <ref>[ <attribute>…]`). A v2 capability advertisement
/// names none.
fn listed_refs(body: &[u8]) -> Vec<(String, String)> {
    pkt_lines(body)
        .into_iter()
        .flatten()
        .filter_map(|line| {
            let line = text(line);
            let line = line.split('\0').next().unwrap_or(line);
            let mut words = line.split(' ');
            let sha = words.next()?;
            let name = words.next()?;
            (hex_id(sha) && !name.is_empty() && name != "capabilities^{}")
                .then(|| (name.to_string(), sha.to_string()))
        })
        .collect()
}

/// What an upload-pack request body asks for.
#[derive(Debug, Default, PartialEq, Eq)]
struct Asked {
    /// Protocol v2's `command=…`, or `None` for v0/v1, where every request
    /// is a fetch.
    command: Option<String>,
    wants: Vec<String>,
}

fn asked(body: &[u8]) -> Asked {
    let mut out = Asked::default();
    for line in pkt_lines(body).into_iter().flatten() {
        let line = text(line);
        if let Some(command) = line.strip_prefix("command=") {
            out.command = Some(command.to_string());
        } else if let Some(rest) = line.strip_prefix("want ") {
            let sha = rest.split([' ', '\0']).next().unwrap_or("");
            if hex_id(sha) && !out.wants.iter().any(|w| w == sha) {
                out.wants.push(sha.to_string());
            }
        }
    }
    out
}

/// A request body as Git sent it, decompressed when Git compressed it.
fn decoded(headers: &HeaderMap, body: &Bytes) -> Option<Vec<u8>> {
    let encoding = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match encoding.as_str() {
        "" | "identity" => Some(body.to_vec()),
        "gzip" | "x-gzip" => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&body[..])
                .take(MAX_REQUEST as u64)
                .read_to_end(&mut out)
                .ok()?;
            Some(out)
        }
        _ => None,
    }
}

/// What a session's last listings named, so a fetch is recorded by ref as
/// well as by commit, and the want-set it last recorded, so the rounds of
/// one negotiation are one fetch.
#[derive(Default)]
struct Seen {
    refs: Vec<(String, String)>,
    recorded: Option<(Vec<String>, Instant)>,
}

/// Sessions are few and their listings small; the map is cleared whole if it
/// ever grows past this, which costs only the ref names of a fetch then.
const MAX_SEEN: usize = 1024;

fn seen() -> &'static Mutex<HashMap<String, Seen>> {
    static SEEN: LazyLock<Mutex<HashMap<String, Seen>>> = LazyLock::new(Default::default);
    &SEEN
}

fn remember_refs(session: &str, refs: Vec<(String, String)>) {
    if refs.is_empty() {
        return;
    }
    let mut seen = seen().lock().unwrap();
    if seen.len() >= MAX_SEEN && !seen.contains_key(session) {
        seen.clear();
    }
    seen.entry(session.to_string()).or_default().refs = refs;
}

/// The refs a fetch of `wants` brings, by the session's last listing, or
/// `None` when it repeats the want-set just recorded (one negotiation's
/// later rounds).
fn fetch_record(session: &str, wants: &[String]) -> Option<Vec<serde_json::Value>> {
    let mut seen = seen().lock().unwrap();
    let entry = seen.entry(session.to_string()).or_default();
    let mut sorted = wants.to_vec();
    sorted.sort();
    if let Some((last, at)) = &entry.recorded {
        if *last == sorted && at.elapsed() < Duration::from_secs(120) {
            return None;
        }
    }
    entry.recorded = Some((sorted, Instant::now()));
    Some(
        wants
            .iter()
            .map(|sha| {
                let names: Vec<&str> = entry
                    .refs
                    .iter()
                    .filter(|(_, at)| at == sha)
                    .map(|(name, _)| name.as_str())
                    .collect();
                json!({ "sha": sha, "refs": names })
            })
            .collect(),
    )
}

/// The managed clone's own `origin`, where the gateway forwards to: the
/// forge URL the clone was made from, over HTTP(S), and only when it names
/// the repository the clone is filed under.
async fn upstream_of(repo: &Path, identity: &str) -> Result<String, String> {
    let mut command = crate::git_remote::git_bare(
        "git",
        &repo.join(".git"),
        crate::forge::FORGE_HOME,
        &crate::git_remote::Credential::Anonymous,
    )
    .map_err(|e| e.to_string())?;
    command.args(["config", "--get", "remote.origin.url"]);
    let out = command.output().await.map_err(|e| e.to_string())?;
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || url.is_empty() {
        return Err("the node's clone of this repository has no origin".into());
    }
    let web = url.starts_with("https://") || url.starts_with("http://");
    let same = crate::corpus::project::canonical_remote(&url)
        .is_some_and(|canonical| canonical.eq_ignore_ascii_case(identity));
    if !web || !same || url.contains('@') {
        return Err("the node's clone of this repository is not fetched over HTTPS".into());
    }
    Ok(url.trim_end_matches('/').to_string())
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // A redirect would carry the forge credential somewhere this
            // path was not resolved to.
            .redirect(reqwest::redirect::Policy::none())
            // Direct, as the node's own Git reaches the forge
            // (`git_remote`, which clears the environment a proxy is named in).
            .no_proxy()
            .connect_timeout(CONNECT)
            .build()
            .expect("git upstream client")
    })
}

/// The session a request's bearer token names.
async fn caller(s: &AppState, headers: &HeaderMap) -> Option<(String, String)> {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))?;
    s.manager.session_for_token(presented).await
}

/// `ANY /git/{*rest}`.
pub async fn handle(
    State(s): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some((session_id, channel)) = caller(&s, &headers).await else {
        return refuse(
            StatusCode::FORBIDDEN,
            "this remote answers only a running session of this node, with its own credential",
        );
    };
    let Some((asked_path, tail)) = split_tail(uri.path()) else {
        return refuse(
            StatusCode::NOT_FOUND,
            call_of(&method, "", None).unwrap_err(),
        );
    };
    let call = match call_of(&method, tail, uri.query()) {
        Ok(call) => call,
        Err(reason) => return refuse(StatusCode::FORBIDDEN, reason),
    };
    let store = s.manager.store();
    let repo = match crate::environment::origin_repo(store, Some(&session_id)) {
        Ok(Some(repo)) => repo,
        _ => {
            return refuse(
                StatusCode::NOT_FOUND,
                "this session has no forge repository to fetch from",
            )
        }
    };
    let state_dir = crate::config::Config::state_dir();
    let Some(identity) = crate::forge::managed_clone(&repo)
        .ok()
        .and_then(|_| crate::forge::managed_identity(&state_dir, &repo))
    else {
        return refuse(
            StatusCode::NOT_FOUND,
            "this session's repository is not a forge clone of this node's",
        );
    };
    let own = repo_path(&channel, &identity);
    let (_, own_name) = identity.split_once('/').unwrap_or(("", &identity));
    if own.as_deref() != Some(asked_path) {
        tracing::warn!(session = %session_id, path = asked_path, "git gateway refused another repository");
        return refuse(
            StatusCode::NOT_FOUND,
            &format!("this remote serves this session's own repository, {own_name}, and no other"),
        );
    }
    if call == Call::ReceivePack {
        tracing::info!(session = %session_id, "git gateway refused a push");
        return refuse(StatusCode::FORBIDDEN, READ_ONLY);
    }
    let upstream = match upstream_of(&repo, &identity).await {
        Ok(url) => url,
        Err(reason) => return refuse(StatusCode::BAD_GATEWAY, &reason),
    };
    let body = match axum::body::to_bytes(body, MAX_REQUEST).await {
        Ok(body) => body,
        Err(_) => return refuse(StatusCode::PAYLOAD_TOO_LARGE, "the request is too large"),
    };
    let brokered =
        crate::forge::git_token_for(s.manager.broker(), &state_dir, &channel, &repo, &s.node_id);
    forward(
        &s,
        Forwarded {
            session: &session_id,
            repository: own_name,
            call,
            upstream: &upstream,
            credential: brokered
                .as_ref()
                .map(|(forge, token)| (forge.git_user(), token.as_str())),
        },
        &headers,
        body,
    )
    .await
}

/// One request on its way to the forge.
struct Forwarded<'a> {
    session: &'a str,
    /// `owner/name`, as the session's events name it.
    repository: &'a str,
    call: Call,
    /// The forge repository's URL, without a trailing slash.
    upstream: &'a str,
    /// The brokered `(user, token)`, or `None` for an anonymous read.
    credential: Option<(&'a str, &'a str)>,
}

async fn forward(s: &AppState, f: Forwarded<'_>, headers: &HeaderMap, body: Bytes) -> Response {
    let target = match f.call {
        Call::Advertise => format!("{}{INFO_REFS}?service=git-upload-pack", f.upstream),
        _ => format!("{}{UPLOAD_PACK}", f.upstream),
    };
    let asked = match f.call {
        Call::UploadPack => decoded(headers, &body)
            .map(|b| asked(&b))
            .unwrap_or_default(),
        _ => Asked::default(),
    };
    let mut req = match f.call {
        Call::Advertise => client().get(&target),
        _ => client().post(&target).body(body),
    };
    for name in [
        "content-type",
        "content-encoding",
        "accept",
        "git-protocol",
        "user-agent",
    ] {
        if let Some(value) = headers.get(name) {
            req = req.header(name, value);
        }
    }
    req = req.header("accept-encoding", "identity");
    if let Some((user, token)) = f.credential {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
        req = req.header("authorization", format!("Basic {basic}"));
    }
    let upstream = match req.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(session = f.session, error = %error.without_url(), "git gateway could not reach the forge");
            return refuse(StatusCode::BAD_GATEWAY, "the forge could not be reached");
        }
    };
    let status = upstream.status();
    if !status.is_success() {
        tracing::warn!(session = f.session, %status, "the forge refused a fetch through the git gateway");
        // Never a 401: a challenge would send Git looking for a credential
        // in the sandbox, where there is none to find.
        return refuse(
            if status == reqwest::StatusCode::NOT_FOUND {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_GATEWAY
            },
            &format!(
                "the forge answered {} for {}; the channel's credential may not reach it",
                status.as_u16(),
                f.repository
            ),
        );
    }
    let mut out = Response::builder().status(status.as_u16());
    for (name, value) in upstream.headers() {
        if matches!(
            name.as_str(),
            "content-type" | "cache-control" | "expires" | "pragma"
        ) {
            out = out.header(name.as_str(), value.as_bytes());
        }
    }
    let listing = f.call == Call::Advertise || asked.command.as_deref() == Some("ls-refs");
    if listing {
        // A listing is read whole, to know which refs a later fetch names.
        let bytes = match read_capped(upstream, MAX_LISTING).await {
            Ok(bytes) => bytes,
            Err(error) => return refuse(StatusCode::BAD_GATEWAY, &error),
        };
        remember_refs(f.session, listed_refs(&bytes));
        return out
            .body(Body::from(bytes))
            .unwrap_or_else(|_| refuse(StatusCode::BAD_GATEWAY, "bad upstream response"));
    }
    let fetch = asked.command.is_none() || asked.command.as_deref() == Some("fetch");
    if fetch && !asked.wants.is_empty() {
        if let Some(refs) = fetch_record(f.session, &asked.wants) {
            s.manager.record_event(
                f.session,
                ek::GIT_FETCH,
                json!({
                    "repository": f.repository,
                    "fetched": refs,
                    "protocol": if asked.command.is_some() { "v2" } else { "v0" },
                }),
            );
            tracing::info!(
                session = f.session,
                wants = asked.wants.len(),
                "git fetch through the gateway"
            );
        }
    }
    out.body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|_| refuse(StatusCode::BAD_GATEWAY, "bad upstream response"))
}

async fn read_capped(mut response: reqwest::Response, cap: usize) -> Result<Bytes, String> {
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if out.len() + chunk.len() > cap {
            return Err("the forge's ref listing is too large to serve".into());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(line: &str) -> String {
        format!("{:04x}{line}", line.len() + 4)
    }

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn the_origin_names_the_channel_and_the_repository_and_nothing_else() {
        assert_eq!(
            origin_url("tracon-gw", 7421, "work", "github.com/o/r").as_deref(),
            Some("http://tracon-gw:7421/git/work/o/r.git")
        );
        // A GitLab group path is the owner, whole.
        assert_eq!(
            origin_url("gw", 1, "work", "gitlab.com/g/sub/r").as_deref(),
            Some("http://gw:1/git/work/g/sub/r.git")
        );
        // A channel is one segment, however it is spelled.
        assert_eq!(
            repo_path("a/b c", "github.com/o/r").as_deref(),
            Some("/git/a%2Fb%20c/o/r.git")
        );
        assert_eq!(repo_path("work", "github.com"), None);
        assert_eq!(repo_path("work", "github.com/r"), None);
    }

    #[test]
    fn the_gitconfig_carries_the_session_token_for_that_url_only() {
        let config = gitconfig("http://gw:1/git/work/o/r.git", "tok");
        assert!(!config.contains("remote"), "{config}");
        assert!(config.contains(
            "[http \"http://gw:1/git/work/o/r.git\"]\n\textraHeader = \"Authorization: Bearer tok\""
        ));
        // Git itself reads it so.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("gitconfig");
        std::fs::write(&file, &config).unwrap();
        let out = std::process::Command::new("git")
            .args(["config", "--file"])
            .arg(&file)
            .args(["--get", "http.http://gw:1/git/work/o/r.git.extraheader"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "Authorization: Bearer tok"
        );
    }

    #[test]
    fn the_workspace_keeps_a_gateway_origin_and_nothing_else() {
        let url = "http://tracon-gw:7421/git/work/o/r.git";
        assert!(is_gateway_url(url));
        assert!(is_gateway_url("http://gw:1/git/a%2Fb/g/sub/r.git"));
        for bad in [
            "https://gw/git/w/o/r.git",
            "http://u@gw/git/w/o/r.git",
            "http://gw/git/w/r.git",
            "http://gw/x/w/o/r.git",
            "http://gw/git/w/o/r",
            "http://gw/git/w/../r.git",
            "http://gw/git/w/o/r.git\"\n[core]",
            "ext::sh -c x",
        ] {
            assert!(!is_gateway_url(bad), "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        std::fs::write(
            dir.path().join(".git/config"),
            "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = /elsewhere\n",
        )
        .unwrap();
        set_workspace_origin(dir.path(), url).unwrap();
        let text = std::fs::read_to_string(dir.path().join(".git/config")).unwrap();
        assert!(!text.contains("/elsewhere"), "{text}");
        assert!(text.contains("bare = false"), "{text}");
        assert_eq!(kept_origin(&text).as_deref(), Some(url));
        let out = std::process::Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), url);
        // An origin that is not the gateway's is not kept.
        assert_eq!(
            kept_origin("[remote \"origin\"]\n\turl = https://github.com/o/r\n"),
            None
        );
        assert!(set_workspace_origin(dir.path(), "https://github.com/o/r").is_err());
    }

    #[test]
    fn only_upload_pack_is_a_call_and_a_push_is_known_as_one() {
        let get = Method::GET;
        let post = Method::POST;
        assert_eq!(
            call_of(&get, INFO_REFS, Some("service=git-upload-pack")),
            Ok(Call::Advertise)
        );
        assert_eq!(call_of(&post, UPLOAD_PACK, None), Ok(Call::UploadPack));
        assert_eq!(
            call_of(&get, INFO_REFS, Some("service=git-receive-pack")),
            Ok(Call::ReceivePack)
        );
        assert_eq!(call_of(&post, RECEIVE_PACK, None), Ok(Call::ReceivePack));
        // Dumb HTTP, a GET of the pack endpoint, and a ref listing that
        // names no service are not served.
        assert!(call_of(&get, INFO_REFS, None).is_err());
        assert!(call_of(&get, UPLOAD_PACK, None).is_err());
        assert!(call_of(&Method::PUT, UPLOAD_PACK, None).is_err());
        assert_eq!(
            split_tail("/git/w/o/r.git/info/refs"),
            Some(("/git/w/o/r.git", INFO_REFS))
        );
        assert_eq!(split_tail("/git/w/o/r.git/HEAD"), None);
    }

    #[test]
    fn a_listing_names_its_refs_in_either_protocol() {
        // v0: the service line, a flush, then refs, the first with
        // capabilities after a NUL.
        let v0 = format!(
            "{}0000{}{}0000",
            pkt("# service=git-upload-pack\n"),
            pkt(&format!("{A} HEAD\0multi_ack side-band-64k\n")),
            pkt(&format!("{B} refs/heads/feat\n")),
        );
        assert_eq!(
            listed_refs(v0.as_bytes()),
            vec![
                ("HEAD".to_string(), A.to_string()),
                ("refs/heads/feat".to_string(), B.to_string())
            ]
        );
        // v2 ls-refs, with attributes.
        let v2 = format!(
            "{}{}0000",
            pkt(&format!("{A} HEAD symref-target:refs/heads/main\n")),
            pkt(&format!("{A} refs/heads/main\n")),
        );
        assert_eq!(listed_refs(v2.as_bytes()).len(), 2);
        // A v2 capability advertisement names none.
        let caps = format!("{}{}0000", pkt("version 2\n"), pkt("ls-refs=unborn\n"));
        assert!(listed_refs(caps.as_bytes()).is_empty());
    }

    #[test]
    fn a_fetch_request_says_what_it_wants() {
        let v0 = format!(
            "{}{}0000{}{}",
            pkt(&format!("want {A} multi_ack_detailed side-band-64k\n")),
            pkt(&format!("want {B}\n")),
            pkt(&format!("have {B}\n")),
            pkt("done\n"),
        );
        assert_eq!(
            asked(v0.as_bytes()),
            Asked {
                command: None,
                wants: vec![A.to_string(), B.to_string()]
            }
        );
        let v2 = format!(
            "{}{}0001{}{}0000",
            pkt("command=fetch\n"),
            pkt("agent=git/2\n"),
            pkt("ofs-delta\n"),
            pkt(&format!("want {A}\n")),
        );
        assert_eq!(
            asked(v2.as_bytes()),
            Asked {
                command: Some("fetch".into()),
                wants: vec![A.to_string()]
            }
        );
        let ls = format!("{}0001{}0000", pkt("command=ls-refs\n"), pkt("peel\n"));
        assert_eq!(asked(ls.as_bytes()).command.as_deref(), Some("ls-refs"));
    }

    #[test]
    fn a_fetch_is_recorded_once_by_the_refs_its_listing_named() {
        let session = "test-git-gateway-record";
        remember_refs(
            session,
            vec![
                ("HEAD".into(), A.into()),
                ("refs/heads/main".into(), A.into()),
                ("refs/heads/feat".into(), B.into()),
            ],
        );
        let fetched = fetch_record(session, &[A.to_string()]).unwrap();
        assert_eq!(
            fetched,
            vec![json!({ "sha": A, "refs": ["HEAD", "refs/heads/main"] })]
        );
        // The next round of the same negotiation is the same fetch.
        assert!(fetch_record(session, &[A.to_string()]).is_none());
        assert!(fetch_record(session, &[B.to_string()]).is_some());
    }

    #[test]
    fn a_compressed_request_is_read_decompressed() {
        use std::io::Write as _;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"0032want 1111111111111111111111111111111111111111\n")
            .unwrap();
        let body = Bytes::from(gz.finish().unwrap());
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", "gzip".parse().unwrap());
        let plain = decoded(&headers, &body).unwrap();
        assert_eq!(asked(&plain).wants, vec![A.to_string()]);
        headers.insert("content-encoding", "br".parse().unwrap());
        assert!(decoded(&headers, &body).is_none());
    }
}
