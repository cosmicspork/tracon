//! `web_fetch`: the node fetches a web page for a session, deny by default.
//!
//! A session's own network reaches only the hosts its grant opens, so the
//! harness's client-side fetch tool and `curl` meet the egress proxy's 403.
//! Reading a page goes through the node instead, which decides each URL:
//!
//! - the signed policy first, under the kind `fetch` with the URL as the
//!   resource: a denial wins over everything, and an allow rule that names
//!   the kind (`kinds = ["fetch"]`, `matches` the URL patterns) fetches
//!   unasked;
//! - then the channel's own fetch allow list: authority grants of the
//!   action `fetch` (`authority::FETCH`), whose targets are patterns. A
//!   denied pattern refuses; an allowed one fetches unasked, only while the
//!   signed bundle is intact, as every local grant;
//! - anything else is put to the operator on a card with three answers:
//!   fetch this URL once, save the pattern to the channel's allow list and
//!   fetch, or deny.
//!
//! What is fetched is bounded: a GET with no credentials, at most
//! [`MAX_BODY_BYTES`] of body and [`MAX_REDIRECTS`] redirects, each redirect
//! target decided by the same policy before it is followed (one the policy
//! would ask about stops the fetch and names the URL, so the agent asks for
//! it on its own). The addresses the node will not dial are the egress
//! proxy's (`Backend::egress_refusal`), checked on the addresses actually
//! dialled. Every fetch, and every one that failed, is recorded on the
//! session as evidence: the URL, the final URL, the status and a digest of
//! the body (`event_kind::WEB_FETCHED`).

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    gateway::proxy::Refusal,
    policy::{fetch_pattern_matches, Decision, Policy, Request, Verdict, FETCH_KIND},
    store::Store,
};

pub const WEB_FETCH: &str = crate::store::approvals::FETCH_TOOL;

/// The answer that saves the card's pattern to the channel's allow list.
pub const OPTION_SAVE: &str = crate::store::approvals::OPTION_ALLOW_CHANNEL;

/// The most body a fetch reads. A longer one is refused, not cut short: a
/// page read in part is not the page its digest would say it was.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// The most redirects one fetch follows.
pub const MAX_REDIRECTS: usize = 5;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_URL_CHARS: usize = 4096;
const MAX_PATTERN_CHARS: usize = 500;

pub fn definitions() -> Vec<Value> {
    vec![json!({
        "name": WEB_FETCH,
        "description": format!(
            "Fetch a web page (HTTP GET) through the node and return its status and body. \
             This session's own network does not reach the web: `curl`, `wget` and your \
             harness's own fetch tool get a 403 from the egress proxy, so read pages with \
             this instead. A URL the channel's fetch allow list covers is fetched at once; \
             any other is put to the operator, who fetches it once, saves its pattern to \
             the channel, or denies it: you get an approval_id, and approval_status waits \
             for the fetch's result. A denial is final: do without the page, and do not \
             look for another way to fetch it. At most {} MB of body and {MAX_REDIRECTS} \
             redirects; a redirect to a URL the allow list does not cover stops and names \
             it, for you to ask about on its own.",
            MAX_BODY_BYTES / (1024 * 1024)
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "An http or https URL, such as `https://docs.rs/serde/latest/serde/`.",
                },
                "pattern": {
                    "type": "string",
                    "description": "The pattern you would have the operator save to the channel if they save it, \
                                    such as `https://docs.rs/*` or `*.python.org`. It must cover the URL. \
                                    Left out, the URL's origin (`https://docs.rs/*`).",
                },
            },
            "required": ["url"],
        },
    })]
}

/// The URL as the node fetches it: http or https, with a host, without
/// credentials or a fragment.
pub fn normalize_url(raw: &str) -> Result<url::Url, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("name the URL to fetch".into());
    }
    if raw.chars().count() > MAX_URL_CHARS {
        return Err(format!("a URL is at most {MAX_URL_CHARS} characters"));
    }
    let mut url = url::Url::parse(raw).map_err(|e| format!("{raw:?} is not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "only http and https are fetched; {raw:?} is {}",
            url.scheme()
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(format!("{raw:?} names no host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("a URL with credentials in it is not fetched".into());
    }
    url.set_fragment(None);
    Ok(url)
}

/// The pattern a card offers to save when the agent proposed none: the URL's
/// origin, any path.
pub fn default_pattern(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{}://{host}:{port}/*", url.scheme()),
        None => format!("{}://{host}/*", url.scheme()),
    }
}

/// A pattern as the allow list holds it, or why it cannot be one. A pattern
/// that names no host of its own (`*`, `https://*`) would open the whole web
/// to every session on the channel: that is a policy rule the operator
/// writes and signs, never a card's answer.
pub fn validate_pattern(raw: &str) -> Result<String, String> {
    let pattern = raw.trim();
    if pattern.is_empty() {
        return Err("a fetch pattern cannot be empty".into());
    }
    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return Err(format!(
            "a fetch pattern is at most {MAX_PATTERN_CHARS} characters"
        ));
    }
    if pattern.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("a fetch pattern holds no spaces".into());
    }
    let host = match pattern.split_once("://") {
        Some((scheme, rest)) => {
            if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
                return Err(format!(
                    "a fetch pattern names http or https; {pattern:?} names {scheme}"
                ));
            }
            rest.split('/').next().unwrap_or_default()
        }
        None if pattern.contains('/') => {
            return Err(format!(
                "{pattern:?} is neither a host (`docs.rs`, `*.python.org`) nor a URL pattern \
                 (`https://docs.rs/*`)"
            ))
        }
        None => pattern,
    };
    let named = host
        .split(':')
        .next()
        .unwrap_or_default()
        .split('.')
        .filter(|label| !label.is_empty() && !label.contains('*'))
        .count();
    if named == 0 || (named == 1 && host.contains('*')) {
        return Err(format!(
            "{pattern:?} names no host of its own; a pattern that wide is a signed policy \
             rule, not a channel's allow list"
        ));
    }
    Ok(pattern.to_string())
}

/// Whether the channel may fetch `url` unasked, never, or only once the
/// operator says. See the module's documentation for the order.
pub fn decide(
    policy: &Policy,
    store: &Store,
    channel: &str,
    session_id: Option<&str>,
    url: &str,
) -> Decision {
    let signed = policy.decide(&Request {
        channel,
        action: WEB_FETCH,
        kind: Some(FETCH_KIND),
        resource: Some(url),
        command: None,
        arguments: None,
        doc_kinds: None,
    });
    if signed.verdict == Verdict::Deny {
        return signed;
    }
    let grants = store
        .authority_grants_of_action(
            channel,
            session_id,
            crate::authority::FETCH,
            crate::store::now_ms(),
        )
        .unwrap_or_default();
    let matching = |verdict: &str| {
        grants
            .iter()
            .find(|g| g.verdict == verdict && fetch_pattern_matches(&g.target, url))
    };
    if let Some(grant) = matching("deny") {
        return Decision {
            verdict: Verdict::Deny,
            rule_id: Some(grant.id.clone()),
            reason: Some(if grant.reason.trim().is_empty() {
                format!("the channel's fetch list denies {}", grant.target)
            } else {
                grant.reason.clone()
            }),
        };
    }
    if signed.verdict == Verdict::Allow {
        return signed;
    }
    let ask = Decision {
        verdict: Verdict::Ask,
        rule_id: None,
        reason: None,
    };
    if let Some(grant) = matching("ask") {
        return Decision {
            rule_id: Some(grant.id.clone()),
            reason: Some(grant.reason.clone()),
            ..ask
        };
    }
    // A local grant widens only what an intact bundle would let it.
    if !policy.trusted {
        return ask;
    }
    match matching("allow") {
        Some(grant) => Decision {
            verdict: Verdict::Allow,
            rule_id: Some(grant.id.clone()),
            reason: Some(grant.reason.clone()),
        },
        None => ask,
    }
}

/// What one fetch came to.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub url: String,
    pub final_url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub sha256: String,
    pub body: Vec<u8>,
    /// Every URL a redirect led to, in order; the last is `final_url`.
    pub redirects: Vec<String>,
}

/// Fetch `url`, following at most [`MAX_REDIRECTS`] redirects, each of
/// which `admit` must allow before it is followed. Never dials an address
/// `refusal` refuses, and dials only the addresses it checked.
pub async fn fetch(
    url: &url::Url,
    refusal: &Refusal,
    admit: impl Fn(&url::Url) -> Result<(), String>,
) -> Result<Fetched, (String, Vec<String>)> {
    let mut current = url.clone();
    let mut redirects: Vec<String> = Vec::new();
    loop {
        let fail = |why: String, redirects: &Vec<String>| Err((why, redirects.clone()));
        let addresses = match addresses(&current, refusal).await {
            Ok(addresses) => addresses,
            Err(why) => return fail(why, &redirects),
        };
        let mut client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // The addresses checked are the addresses dialled: a proxy
            // would resolve the name again, out of the node's sight.
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TIMEOUT)
            .user_agent(concat!("tracon-web-fetch/", env!("CARGO_PKG_VERSION")));
        if let Some(host) = current.domain() {
            client = client.resolve_to_addrs(host, &addresses);
        }
        let client = match client.build() {
            Ok(client) => client,
            Err(e) => return fail(format!("could not fetch: {e}"), &redirects),
        };
        let response = match client.get(current.clone()).send().await {
            Ok(response) => response,
            Err(e) => {
                return fail(
                    format!("could not fetch {current}: {}", without_url(&e)),
                    &redirects,
                )
            }
        };
        let status = response.status();
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if let (true, Some(location)) = (status.is_redirection(), location) {
            if redirects.len() >= MAX_REDIRECTS {
                return fail(
                    format!("more than {MAX_REDIRECTS} redirects; stopped at {current}"),
                    &redirects,
                );
            }
            let next = match current
                .join(&location)
                .map_err(|e| e.to_string())
                .and_then(|next| normalize_url(next.as_str()))
            {
                Ok(next) => next,
                Err(why) => {
                    return fail(
                        format!("{current} redirected somewhere that is not fetched: {why}"),
                        &redirects,
                    )
                }
            };
            if let Err(why) = admit(&next) {
                return fail(why, &redirects);
            }
            redirects.push(next.to_string());
            current = next;
            continue;
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BODY_BYTES as u64)
        {
            return fail(too_large(&current), &redirects);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut response = response;
        let mut body: Vec<u8> = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > MAX_BODY_BYTES {
                        return fail(too_large(&current), &redirects);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    return fail(
                        format!("reading {current} failed: {}", without_url(&e)),
                        &redirects,
                    )
                }
            }
        }
        return Ok(Fetched {
            url: url.to_string(),
            final_url: current.to_string(),
            status: status.as_u16(),
            content_type,
            sha256: hex::encode(Sha256::digest(&body)),
            body,
            redirects,
        });
    }
}

fn too_large(url: &url::Url) -> String {
    format!(
        "{url} answers with more than {} MB; nothing of it is returned",
        MAX_BODY_BYTES / (1024 * 1024)
    )
}

/// A transport error without the URL it already names, which the sentence
/// around it says.
fn without_url(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    if let Some(url) = error.url() {
        text = text.replace(&format!(" ({url})"), "");
    }
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    text
}

/// The addresses `url`'s host may be dialled at, or why none.
async fn addresses(url: &url::Url, refusal: &Refusal) -> Result<Vec<SocketAddr>, String> {
    let port = url.port_or_known_default().unwrap_or(443);
    let host = url.host_str().unwrap_or_default();
    let resolved: Vec<SocketAddr> = match url.host() {
        Some(url::Host::Ipv4(ip)) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Some(url::Host::Ipv6(ip)) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        _ => {
            if let Some(why) = refusal.name(host) {
                return Err(format!("{host} {why}"));
            }
            tokio::net::lookup_host((host, port))
                .await
                .map_err(|e| format!("{host} does not resolve: {e}"))?
                .collect()
        }
    };
    let mut refused = None;
    let admitted: Vec<SocketAddr> = resolved
        .into_iter()
        .filter(|addr| match refusal.address(&addr.ip()) {
            Some(why) => {
                refused = Some(why);
                false
            }
            None => true,
        })
        .collect();
    match (admitted.is_empty(), refused) {
        (true, Some(why)) => Err(format!("{host} {why}; the node does not fetch it")),
        (true, None) => Err(format!("{host} does not resolve")),
        _ => Ok(admitted),
    }
}

/// Whether a body is shown to the agent as text.
fn is_text(content_type: Option<&str>, body: &[u8]) -> bool {
    match content_type.map(|t| {
        t.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    }) {
        Some(t) if t.starts_with("text/") => true,
        Some(t)
            if t.ends_with("json")
                || t.ends_with("+xml")
                || t == "application/xml"
                || t == "application/javascript"
                || t == "application/x-www-form-urlencoded" =>
        {
            true
        }
        Some(t) if !t.is_empty() => false,
        _ => std::str::from_utf8(body).is_ok(),
    }
}

/// The arguments a fetch card holds: the URL and the pattern saving it would
/// add, validated to cover the URL.
pub fn asked_arguments(url: &url::Url, proposed: Option<&str>) -> Result<Value, String> {
    let pattern = match proposed.map(str::trim).filter(|p| !p.is_empty()) {
        Some(pattern) => pattern_for(url, pattern)?,
        None => default_pattern(url),
    };
    Ok(json!({ "url": url.as_str(), "pattern": pattern }))
}

/// `pattern`, validated to be one the allow list can hold and to cover `url`.
pub fn pattern_for(url: &url::Url, pattern: &str) -> Result<String, String> {
    let pattern = validate_pattern(pattern)?;
    if !fetch_pattern_matches(&pattern, url.as_str()) {
        return Err(format!("{pattern:?} does not cover {url}"));
    }
    Ok(pattern)
}

/// Who let a fetch through, for the evidence.
pub struct Allowed<'a> {
    pub decided_by: &'static str,
    pub rule: Option<&'a str>,
    pub approval_id: Option<&'a str>,
}

/// Run a `web_fetch` the policy or the operator allowed, record it on the
/// session, and say what came back. `approval` is the card the operator
/// allowed, when it was one: the URL it named needs no allow of its own,
/// but a signed or channel denial still wins, and every redirect is decided
/// afresh.
pub async fn run(
    access: &super::SessionAccess,
    policy: &parking_lot::RwLock<Policy>,
    ctx: &super::CallContext,
    args: &Value,
    approval: Option<&str>,
) -> Result<Value, String> {
    let url = normalize_url(args.get("url").and_then(Value::as_str).unwrap_or(""))?;
    let session_id = ctx.session_id();
    let decide_now =
        |url: &str| decide(&policy.read(), &access.store, &ctx.channel, session_id, url);
    let decision = decide_now(url.as_str());
    let allowed = match (decision.verdict, approval) {
        (Verdict::Deny, _) => return Err(refused(&url, &decision)),
        (Verdict::Allow, _) => Allowed {
            decided_by: if decision
                .rule_id
                .as_deref()
                .is_some_and(|id| access.store.authority_grant(id).ok().flatten().is_some())
            {
                "grant"
            } else {
                "policy"
            },
            rule: decision.rule_id.as_deref(),
            approval_id: None,
        },
        (Verdict::Ask, Some(id)) => Allowed {
            decided_by: "operator",
            rule: None,
            approval_id: Some(id),
        },
        (Verdict::Ask, None) => {
            return Err(format!(
                "{url} is not on this channel's fetch allow list; the operator decides it"
            ))
        }
    };
    let refusal = access.manager.backend().egress_refusal();
    let admit = |next: &url::Url| {
        let decision = decide_now(next.as_str());
        match decision.verdict {
            Verdict::Allow => Ok(()),
            Verdict::Deny => Err(format!(
                "{url} redirected to {next}, which is refused: {}",
                decision.reason.unwrap_or_default()
            )),
            Verdict::Ask => Err(format!(
                "{url} redirected to {next}, which this channel's fetch allow list does not \
                 cover; call {WEB_FETCH} with that URL to ask the operator for it"
            )),
        }
    };
    let outcome = fetch(&url, &refusal, admit).await;
    let mut evidence = json!({
        "url": url.as_str(),
        "decided_by": allowed.decided_by,
        "rule": allowed.rule,
        "approval_id": allowed.approval_id,
    });
    let result = match &outcome {
        Ok(fetched) => {
            evidence["final_url"] = json!(fetched.final_url);
            evidence["status"] = json!(fetched.status);
            evidence["sha256"] = json!(fetched.sha256);
            evidence["bytes"] = json!(fetched.body.len());
            evidence["redirects"] = json!(fetched.redirects);
            evidence["content_type"] = json!(fetched.content_type);
            let mut result = json!({
                "url": fetched.url,
                "final_url": fetched.final_url,
                "status": fetched.status,
                "content_type": fetched.content_type,
                "bytes": fetched.body.len(),
                "sha256": fetched.sha256,
                "redirects": fetched.redirects,
            });
            if is_text(fetched.content_type.as_deref(), &fetched.body) {
                result["body"] = json!(String::from_utf8_lossy(&fetched.body));
            } else {
                result["body_omitted"] =
                    json!("not text; its size and digest are above, its bytes are not shown");
            }
            Ok(result)
        }
        Err((why, redirects)) => {
            evidence["error"] = json!(why);
            evidence["redirects"] = json!(redirects);
            Err(why.clone())
        }
    };
    if let Some(session_id) = session_id {
        access.manager.record_event(
            session_id,
            crate::session::state::event_kind::WEB_FETCHED,
            evidence,
        );
    }
    result
}

/// What a refused fetch tells the agent: final, with the rule's reason.
pub fn refused(url: &url::Url, decision: &Decision) -> String {
    format!(
        "{url} is refused{}: {}. Do without it, and do not look for another way to fetch it",
        decision
            .rule_id
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default(),
        decision.reason.as_deref().unwrap_or("the policy denies it")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn a_url_is_fetched_only_over_http_with_a_host_and_no_credentials() {
        let url = normalize_url(" https://Docs.RS/serde#top ").unwrap();
        assert_eq!(url.as_str(), "https://docs.rs/serde");
        for bad in [
            "",
            "docs.rs",
            "file:///etc/passwd",
            "ftp://example.com/x",
            "https://user:pw@example.com/",
            "javascript:alert(1)",
        ] {
            assert!(normalize_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_pattern_is_a_url_glob_or_a_host_glob() {
        let m = |p: &str, u: &str| fetch_pattern_matches(p, u);
        assert!(m("https://docs.rs/*", "https://docs.rs/serde/latest"));
        assert!(m("HTTPS://Docs.rs/*", "https://docs.rs/serde"));
        assert!(!m("https://docs.rs/*", "http://docs.rs/serde"));
        assert!(!m("https://docs.rs/*", "https://docs.rs.evil.test/x"));
        assert!(m("docs.rs", "http://docs.rs:8080/x"));
        assert!(m("*.python.org", "https://docs.python.org/3/"));
        assert!(!m("*.python.org", "https://python.org.evil.test/"));
        assert!(!m("", "https://docs.rs/"));
        assert!(!m("docs.rs", "not a url"));
    }

    #[test]
    fn a_saved_pattern_names_a_host_and_covers_the_url() {
        let url = normalize_url("https://docs.rs/serde").unwrap();
        assert_eq!(default_pattern(&url), "https://docs.rs/*");
        assert_eq!(
            default_pattern(&normalize_url("http://127.0.0.1:8080/a").unwrap()),
            "http://127.0.0.1:8080/*"
        );
        assert_eq!(pattern_for(&url, "docs.rs").unwrap(), "docs.rs");
        for bad in [
            "*",
            "https://*",
            "https://*/x",
            "*.*",
            "*.com",
            "ftp://docs.rs/*",
            "docs.rs/serde",
            "docs .rs",
        ] {
            assert!(validate_pattern(bad).is_err(), "{bad}");
        }
        assert!(pattern_for(&url, "https://crates.io/*").is_err());
    }

    fn policy(rules: &str) -> Policy {
        let mut policy: Policy = toml::from_str(rules).unwrap();
        policy.trusted = true;
        policy
    }

    fn grant(store: &Store, verdict: &str, target: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        store
            .authority_grant_insert(&crate::store::AuthorityGrantRow {
                id: id.clone(),
                action: crate::authority::FETCH.into(),
                verdict: verdict.into(),
                target: target.into(),
                channel: "work".into(),
                session_id: None,
                revision: None,
                expires_ms: None,
                revoked_ms: None,
                reason: String::new(),
                created_ms: crate::store::now_ms(),
            })
            .unwrap();
        id
    }

    #[test]
    fn a_url_is_asked_unless_an_allow_list_covers_it_and_a_denial_always_wins() {
        let store = Store::open_in_memory().unwrap();
        let shipped = Policy::shipped();
        let decide = |p: &Policy, url: &str| decide(p, &store, "work", Some("s1"), url).verdict;
        // Deny by default: nothing in the shipped bundle fetches unasked.
        assert_eq!(decide(&shipped, "https://docs.rs/serde"), Verdict::Ask);

        let signed = policy(
            r#"
            [[rule]]
            id = "docs"
            verdict = "allow"
            reason = "documentation"
            kinds = ["fetch"]
            matches = ["https://docs.rs/*", "*.python.org"]
            [[rule]]
            id = "any-kind"
            verdict = "allow"
            reason = "names no kind"
            matches = ["example.com"]
            [[rule]]
            id = "no-paste"
            verdict = "deny"
            reason = "no paste sites"
            kinds = ["fetch"]
            matches = ["*.pastebin.test"]
            "#,
        );
        assert_eq!(decide(&signed, "https://docs.rs/serde"), Verdict::Allow);
        assert_eq!(
            decide(&signed, "https://docs.python.org/3/"),
            Verdict::Allow
        );
        // An allow that does not name the kind allows no fetch.
        assert_eq!(decide(&signed, "https://example.com/"), Verdict::Ask);
        assert_eq!(
            decide(&signed, "https://x.pastebin.test/raw"),
            Verdict::Deny
        );

        // The channel's list: saved patterns allow, denied ones refuse even
        // what the signed bundle allows, and another channel's do nothing.
        grant(&store, "allow", "https://crates.io/*");
        grant(&store, "deny", "https://docs.rs/private/*");
        assert_eq!(
            decide(&shipped, "https://crates.io/crates/x"),
            Verdict::Allow
        );
        assert_eq!(decide(&signed, "https://docs.rs/private/x"), Verdict::Deny);
        assert_eq!(
            super::decide(&shipped, &store, "other", None, "https://crates.io/").verdict,
            Verdict::Ask
        );
        // A saved pattern widens nothing while the bundle is not intact.
        let broken = Policy::default();
        assert_eq!(decide(&broken, "https://crates.io/crates/x"), Verdict::Ask);
        // And a signed denial wins over a saved allow.
        grant(&store, "allow", "*.pastebin.test");
        assert_eq!(
            decide(&signed, "https://x.pastebin.test/raw"),
            Verdict::Deny
        );
    }

    /// A tiny HTTP server: each request's path picks the answer.
    async fn server() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let response = answer_for(&path, addr);
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        addr
    }

    fn answer_for(path: &str, addr: std::net::SocketAddr) -> Vec<u8> {
        let redirect = |to: String| {
            format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .into_bytes()
        };
        let ok = |content_type: &str, body: &[u8]| {
            let mut out = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            out.extend_from_slice(body);
            out
        };
        if let Some(n) = path.strip_prefix("/hop/") {
            let n: usize = n.parse().unwrap();
            return if n == 0 {
                ok("text/plain", b"landed")
            } else {
                redirect(format!("/hop/{}", n - 1))
            };
        }
        match path {
            "/page" => ok("text/html; charset=utf-8", b"<h1>hello</h1>"),
            "/missing" => {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\ngone"
                    .to_vec()
            }
            "/elsewhere" => redirect(format!("http://localhost:{}/page", addr.port())),
            "/big" => {
                let mut out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    MAX_BODY_BYTES + 1
                )
                .into_bytes();
                out.extend(std::iter::repeat_n(b'x', MAX_BODY_BYTES + 1));
                out
            }
            "/big-unsized" => {
                let mut out =
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n"
                        .to_vec();
                out.extend(std::iter::repeat_n(b'y', MAX_BODY_BYTES + 1));
                out
            }
            _ => ok("text/plain", b"?"),
        }
    }

    fn at(addr: std::net::SocketAddr, path: &str) -> url::Url {
        normalize_url(&format!("http://{addr}{path}")).unwrap()
    }

    #[tokio::test]
    async fn a_fetch_returns_the_status_the_body_and_its_digest() {
        let addr = server().await;
        let got = fetch(&at(addr, "/page"), &Refusal::nothing(), |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.body, b"<h1>hello</h1>");
        assert_eq!(got.sha256, hex::encode(Sha256::digest(b"<h1>hello</h1>")));
        assert_eq!(got.final_url, got.url);
        assert!(is_text(got.content_type.as_deref(), &got.body));
        // A status that is not a success is still what the page answered.
        let gone = fetch(&at(addr, "/missing"), &Refusal::nothing(), |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(gone.status, 404);
    }

    #[tokio::test]
    async fn redirects_are_followed_at_most_five_times_each_one_admitted_first() {
        let addr = server().await;
        let asked = std::sync::Mutex::new(Vec::new());
        let got = fetch(&at(addr, "/hop/5"), &Refusal::nothing(), |next| {
            asked.lock().unwrap().push(next.to_string());
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(got.body, b"landed");
        assert_eq!(got.redirects.len(), 5);
        assert_eq!(got.final_url, at(addr, "/hop/0").to_string());
        assert_eq!(*asked.lock().unwrap(), got.redirects);

        let (why, followed) = fetch(&at(addr, "/hop/6"), &Refusal::nothing(), |_| Ok(()))
            .await
            .unwrap_err();
        assert!(why.contains("more than 5 redirects"), "{why}");
        assert_eq!(followed.len(), 5);

        // A redirect the policy would not admit is not followed.
        let (why, followed) = fetch(&at(addr, "/elsewhere"), &Refusal::nothing(), |next| {
            Err(format!("not {next}"))
        })
        .await
        .unwrap_err();
        assert!(why.contains("localhost"), "{why}");
        assert!(followed.is_empty());
    }

    #[tokio::test]
    async fn a_body_over_ten_megabytes_is_refused_whether_or_not_it_says_so() {
        let addr = server().await;
        for path in ["/big", "/big-unsized"] {
            let (why, _) = fetch(&at(addr, path), &Refusal::nothing(), |_| Ok(()))
                .await
                .unwrap_err();
            assert!(why.contains("more than 10 MB"), "{path}: {why}");
        }
    }

    #[tokio::test]
    async fn an_address_the_egress_proxy_refuses_is_never_dialled() {
        let addr = server().await;
        let (why, _) = fetch(&at(addr, "/page"), &Refusal::this_machine(), |_| Ok(()))
            .await
            .unwrap_err();
        assert!(why.contains("this machine"), "{why}");
        // By name too, after it resolves.
        let url = normalize_url(&format!("http://localhost:{}/page", addr.port())).unwrap();
        let (why, _) = fetch(&url, &Refusal::this_machine(), |_| Ok(()))
            .await
            .unwrap_err();
        assert!(why.contains("this machine"), "{why}");
    }
}
