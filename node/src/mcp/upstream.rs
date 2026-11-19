//! MCP servers outside the node (`[[mcp_server]]`), offered to sessions
//! through the node's own MCP endpoint.
//!
//! The harness never reaches such a server. It sees each of the server's tools
//! as `<server>__<tool>` beside the node's own, and a call is decided exactly
//! as a brokered tool's is: the signed policy first, under the `tool` kind and
//! the namespaced name, then the operator's per-tool list in the server's own
//! entry, and an asked call becomes an approval holding the exact arguments.
//! Only once a call is allowed does the node speak to the server, with the
//! credential the broker holds for it.
//!
//! Streamable HTTP only. The node keeps one MCP session per server and channel
//! and starts another when the server forgets it, and holds each server's tool
//! list for a while so a `tools/list` does not wait on every server each time.
//!
//! A plugin's tools (`crate::plugin`) are called through the same sessions,
//! over the plugin's Unix socket, and decided the same way under its
//! `[[plugin]]` entry (`Entry`).

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use serde_json::{json, Value};

use crate::{
    broker::SharedBroker,
    config::{McpServer, PluginEntry},
    policy::{Decision, Policy, Verdict},
};

/// What separates a server's name from its tool's in the name a session
/// sees. A server name cannot hold it (`config::validate_mcp_servers`) and
/// no tool of the node's own does.
pub const SEPARATOR: &str = "__";

/// The variable of a server's credential that is sent to it.
pub const TOKEN_ENV: &str = "TOKEN";

const PROTOCOL_VERSION: &str = "2025-06-18";

/// How long a listing is used before it is fetched again.
const FRESH_FOR: Duration = Duration::from_secs(300);

/// How soon a server that could not be listed is tried again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// How long listing one server may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Function names the model APIs accept: `[A-Za-z0-9_-]{1,64}`.
const MAX_NAME: usize = 64;

const MAX_DESCRIPTION: usize = 2000;

/// The most of one response the node reads before refusing it.
const MAX_RESPONSE: usize = 8 << 20;

/// The most pages of `tools/list` followed for one server.
const MAX_PAGES: usize = 20;

/// One tool a server lists, under the name a session calls it by.
#[derive(Debug, Clone)]
pub struct RemoteTool {
    pub name: String,
    pub remote: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Default)]
struct Listing {
    /// The entry it was fetched under: a changed address or credential is a
    /// different server.
    fingerprint: String,
    tools: Vec<RemoteTool>,
    fetched: Option<Instant>,
    tried: Option<Instant>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct Connection {
    fingerprint: String,
    session_id: Option<String>,
    version: String,
}

/// Why a request to a server did not produce its answer.
#[derive(Debug)]
enum Failure {
    /// Nothing reached the server.
    Unreachable(String),
    /// The request may have reached it; what it did is not known.
    Unknown(String),
    /// The server no longer knows the MCP session the request named.
    Expired,
    /// It answered, and not with a result.
    Refused(String),
    /// The node could not make the request: nothing was sent.
    NotSent(String),
}

impl Failure {
    /// In words, `server` naming what was asked: `MCP server linear`,
    /// `plugin sql-review`.
    fn describe(&self, server: &str) -> String {
        match self {
            Failure::Unreachable(e) => format!("{server} is unreachable: {e}"),
            Failure::Unknown(e) => format!(
                "mutation-outcome-unknown: {server} did not answer, and what it \
                 did is not known: {e}"
            ),
            Failure::Expired => format!("{server} forgot the session it was given"),
            Failure::Refused(e) => format!("{server}: {e}"),
            Failure::NotSent(e) => format!("{server} was not asked: {e}"),
        }
    }
}

fn server_label(server: &McpServer) -> String {
    format!("MCP server {}", server.name)
}

/// The node's view of every `[[mcp_server]]`: what each one lists, and the
/// MCP session the node holds with it.
pub struct Registry {
    http: reqwest::Client,
    listings: parking_lot::RwLock<HashMap<String, Listing>>,
    connections: parking_lot::Mutex<HashMap<(String, String), Connection>>,
    /// One refresh at a time, so concurrent `tools/list` calls do not list
    /// the same server twice.
    refreshing: tokio::sync::Mutex<()>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            // No redirects: a credential written for one address is not
            // carried to another, whatever header it is sent in.
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("http client"),
            listings: Default::default(),
            connections: Default::default(),
            refreshing: Default::default(),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

/// The server a namespaced tool name belongs to, and the rest of the name.
pub fn server_of<'a, 'n>(
    servers: &'a [McpServer],
    name: &'n str,
) -> Option<(&'a McpServer, &'n str)> {
    let (prefix, rest) = name.split_once(SEPARATOR)?;
    let server = servers.iter().find(|s| s.name == prefix)?;
    (!rest.is_empty()).then_some((server, rest))
}

/// The operator's per-tool lists for one source of tools: an
/// `[[mcp_server]]`, or a `[[plugin]]` (which may have no entry at all).
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    /// The table it is written in, as a rule id names it.
    kind: &'static str,
    /// What it is called in words.
    what: &'static str,
    name: &'a str,
    allow: &'a [String],
    ask: &'a [String],
    deny: &'a [String],
}

impl<'a> Entry<'a> {
    pub fn server(server: &'a McpServer) -> Self {
        Self {
            kind: "mcp_server",
            what: "MCP server",
            name: &server.name,
            allow: &server.allow,
            ask: &server.ask,
            deny: &server.deny,
        }
    }

    /// Plugin `name`, under its `[[plugin]]` entry if the operator wrote one.
    pub fn plugin(name: &'a str, entry: Option<&'a PluginEntry>) -> Self {
        Self {
            kind: "plugin",
            what: "plugin",
            name,
            allow: entry.map(|e| e.allow.as_slice()).unwrap_or_default(),
            ask: entry.map(|e| e.ask.as_slice()).unwrap_or_default(),
            deny: entry.map(|e| e.deny.as_slice()).unwrap_or_default(),
        }
    }

    /// What the entry says about one of its tools, by the tool's own name:
    /// deny over ask over allow, `None` when no list names it.
    pub fn setting(&self, remote: &str) -> Option<Verdict> {
        let named = |list: &[String]| {
            list.iter()
                .any(|pattern| crate::policy::glob(pattern.trim(), remote))
        };
        if named(self.deny) {
            Some(Verdict::Deny)
        } else if named(self.ask) {
            Some(Verdict::Ask)
        } else if named(self.allow) {
            Some(Verdict::Allow)
        } else {
            None
        }
    }
}

/// One call's verdict, from the signed policy's (`policy_decision`, made for
/// the namespaced name) and the operator's entry. As with an authority grant,
/// a signed denial dominates everything, the entry's deny and ask dominate a
/// policy allow, and the entry's allow widens nothing while the bundle is
/// unavailable. A tool neither names is asked.
pub fn decide(
    on: &Entry<'_>,
    remote: &str,
    policy: &Policy,
    policy_decision: Decision,
) -> Decision {
    if policy_decision.verdict == Verdict::Deny {
        return policy_decision;
    }
    let entry = |verdict: Verdict, list: &str, reason: String| Decision {
        verdict,
        rule_id: Some(format!("{}.{}.{list}", on.kind, on.name)),
        reason: Some(reason),
    };
    match on.setting(remote) {
        Some(Verdict::Deny) => entry(
            Verdict::Deny,
            "deny",
            format!(
                "the operator does not allow {remote} on {} {}",
                on.what, on.name
            ),
        ),
        Some(Verdict::Ask) => entry(
            Verdict::Ask,
            "ask",
            format!(
                "the operator decides each call to {remote} on {} {}",
                on.what, on.name
            ),
        ),
        _ if !policy.trusted => Decision {
            verdict: Verdict::Ask,
            rule_id: None,
            reason: Some("the signed policy bundle is unavailable or invalid".into()),
        },
        _ if policy_decision.verdict == Verdict::Allow => policy_decision,
        Some(Verdict::Allow) => entry(
            Verdict::Allow,
            "allow",
            format!(
                "the operator allows {remote} on {} {} without asking",
                on.what, on.name
            ),
        ),
        None => Decision {
            verdict: Verdict::Ask,
            rule_id: None,
            reason: None,
        },
    }
}

fn fingerprint(server: &McpServer) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        server.url,
        server.credential.as_deref().unwrap_or_default(),
        server.header.as_deref().unwrap_or_default()
    )
}

/// The headers that carry a server's credential for a call on `channel`, as
/// the broker lends it: bound to that channel and this node, or not at all.
fn credential_headers(
    server: &McpServer,
    broker: &SharedBroker,
    channel: &str,
    node_id: &str,
) -> Result<Vec<(String, String)>, String> {
    let Some(name) = server.credential.as_deref() else {
        return Ok(Vec::new());
    };
    let env = broker
        .read()
        .unwrap()
        .env_for(name, channel, node_id)
        .map_err(|e| e.to_string())?;
    let token = env
        .get(TOKEN_ENV)
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("credential {name} has no {TOKEN_ENV}"))?;
    Ok(vec![match server.header.as_deref() {
        Some(header) => (header.to_string(), token.to_string()),
        None => ("authorization".to_string(), format!("Bearer {token}")),
    }])
}

/// `<server>__<tool>`, with any character a function name cannot carry
/// written as `_`; `None` when that does not fit in one.
pub fn namespaced(server: &str, remote: &str) -> Option<String> {
    let tool: String = remote
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{server}{SEPARATOR}{tool}");
    (!remote.is_empty() && name.len() <= MAX_NAME).then_some(name)
}

/// The longest name a model API accepts for a tool.
pub const MAX_TOOL_NAME: usize = MAX_NAME;

/// A server's input schema as a session is offered it: an object schema, and
/// a `required` that is a list or absent (Claude Code drops every tool of a
/// server with one that is not).
fn input_schema(schema: &Value) -> Value {
    let mut schema = match schema {
        Value::Object(_) => schema.clone(),
        _ => json!({ "type": "object", "properties": {} }),
    };
    let map = schema.as_object_mut().expect("an object");
    map.entry("type").or_insert_with(|| json!("object"));
    if map.get("required").is_some_and(|r| !r.is_array()) {
        map.remove("required");
    }
    schema
}

pub fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

impl Registry {
    /// The definitions of every tool the servers serving `channel` list, for
    /// `tools/list`. `credentials` is what the channel may use on this node:
    /// a server whose credential it may not use is not offered.
    pub fn definitions(
        &self,
        servers: &[McpServer],
        channel: &str,
        credentials: &[&str],
    ) -> Vec<Value> {
        let listings = self.listings.read();
        let mut out = Vec::new();
        for server in servers.iter().filter(|s| s.serves(channel)) {
            if server
                .credential
                .as_deref()
                .is_some_and(|c| !credentials.contains(&c))
            {
                continue;
            }
            let Some(listing) = listings.get(&server.name) else {
                continue;
            };
            if listing.fingerprint != fingerprint(server) {
                continue;
            }
            for tool in &listing.tools {
                let description = if tool.description.trim().is_empty() {
                    format!("{} on MCP server {}.", tool.remote, server.name)
                } else {
                    format!("[MCP server {}] {}", server.name, tool.description.trim())
                };
                out.push(json!({
                    "name": tool.name,
                    "description": bounded(&description, MAX_DESCRIPTION),
                    "inputSchema": tool.input_schema,
                }));
            }
        }
        out
    }

    /// The server's own name for a namespaced tool it last listed.
    pub fn remote_name(&self, server: &McpServer, name: &str) -> Option<String> {
        let listings = self.listings.read();
        let listing = listings.get(&server.name)?;
        (listing.fingerprint == fingerprint(server))
            .then(|| listing.tools.iter().find(|t| t.name == name))
            .flatten()
            .map(|t| t.remote.clone())
    }

    /// The input schema of a namespaced tool `server` last listed.
    pub fn input_schema(&self, server: &McpServer, name: &str) -> Option<Value> {
        let listings = self.listings.read();
        let listing = listings.get(&server.name)?;
        (listing.fingerprint == fingerprint(server))
            .then(|| listing.tools.iter().find(|t| t.name == name))
            .flatten()
            .map(|t| t.input_schema.clone())
    }

    /// Why a listing of `server` is unusable, when it is: never fetched, or
    /// its last attempt failed and nothing earlier is held.
    pub fn unavailable(&self, server: &McpServer) -> Option<String> {
        let listings = self.listings.read();
        match listings.get(&server.name) {
            Some(l) if l.fingerprint == fingerprint(server) && l.fetched.is_some() => None,
            Some(l) if l.fingerprint == fingerprint(server) => Some(
                l.error
                    .clone()
                    .unwrap_or_else(|| "it has not been listed yet".into()),
            ),
            _ => Some("it has not been listed yet".into()),
        }
    }

    fn stale(&self, server: &McpServer) -> bool {
        let listings = self.listings.read();
        let Some(listing) = listings.get(&server.name) else {
            return true;
        };
        if listing.fingerprint != fingerprint(server) {
            return true;
        }
        let recently =
            |at: Option<Instant>, within: Duration| at.is_some_and(|at| at.elapsed() < within);
        !recently(listing.fetched, FRESH_FOR) && !recently(listing.tried, RETRY_AFTER)
    }

    /// List again every server in `servers` whose listing is stale, or every
    /// one when `force`. A server that cannot be listed keeps the tools it
    /// last listed and records why.
    pub async fn refresh(
        &self,
        servers: &[McpServer],
        broker: &SharedBroker,
        node_id: &str,
        force: bool,
    ) {
        let _one = self.refreshing.lock().await;
        // Entries the configuration no longer has are dropped.
        self.listings
            .write()
            .retain(|name, _| servers.iter().any(|s| &s.name == name));
        self.connections.lock().retain(|(name, _), _| {
            name.starts_with(PLUGIN_KEY) || servers.iter().any(|s| &s.name == name)
        });
        for server in servers {
            if !force && !self.stale(server) {
                continue;
            }
            let outcome =
                match tokio::time::timeout(LIST_TIMEOUT, self.list_tools(server, broker, node_id))
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => Err(format!(
                        "MCP server {} did not list its tools within {}s",
                        server.name,
                        LIST_TIMEOUT.as_secs()
                    )),
                };
            let print = fingerprint(server);
            let mut listings = self.listings.write();
            let listing = listings.entry(server.name.clone()).or_default();
            if listing.fingerprint != print {
                *listing = Listing {
                    fingerprint: print,
                    ..Default::default()
                };
            }
            listing.tried = Some(Instant::now());
            match outcome {
                Ok(tools) => {
                    listing.tools = tools;
                    listing.fetched = Some(Instant::now());
                    listing.error = None;
                }
                Err(error) => {
                    tracing::warn!(server = %server.name, %error, "could not list an MCP server's tools");
                    listing.error = Some(error);
                }
            }
        }
    }

    async fn list_tools(
        &self,
        server: &McpServer,
        broker: &SharedBroker,
        node_id: &str,
    ) -> Result<Vec<RemoteTool>, String> {
        // Listed with the credential as one of the server's own channels may
        // use it: the listing is offered only to those.
        let mut headers = Err(format!(
            "MCP server {} serves no channel; name one in its channels",
            server.name
        ));
        let mut channel = "";
        for candidate in &server.channels {
            headers = credential_headers(server, broker, candidate, node_id);
            if headers.is_ok() {
                channel = candidate;
                break;
            }
        }
        let headers = headers?;
        let mut tools: Vec<RemoteTool> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let page = self
                .request(
                    server,
                    &headers,
                    channel,
                    "tools/list",
                    params,
                    LIST_TIMEOUT,
                )
                .await
                .map_err(|f| f.describe(&server_label(server)))?;
            for tool in page_tools(&page) {
                let Some(name) = namespaced(&server.name, &tool.remote) else {
                    tracing::warn!(server = %server.name, tool = %tool.remote, "an MCP tool's name does not fit; not offered");
                    continue;
                };
                if tools.iter().any(|t| t.name == name) {
                    tracing::warn!(server = %server.name, tool = %tool.remote, "two MCP tools share a name once namespaced; the second is not offered");
                    continue;
                }
                tools.push(RemoteTool { name, ..tool });
            }
            cursor = next_cursor(&page);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// Call `remote` on `server` for a caller on `channel`, with the
    /// arguments as they were decided. The result is the server's own
    /// content; a result the server marks as an error is returned as one.
    pub async fn call(
        &self,
        server: &McpServer,
        broker: &SharedBroker,
        channel: &str,
        node_id: &str,
        remote: &str,
        args: &Value,
    ) -> Result<Value, String> {
        let headers = credential_headers(server, broker, channel, node_id)?;
        let timeout = Duration::from_secs(server.timeout_secs());
        let label = server_label(server);
        let result = self
            .request(
                server,
                &headers,
                channel,
                "tools/call",
                json!({ "name": remote, "arguments": args }),
                timeout,
            )
            .await
            .map_err(|f| f.describe(&label))?;
        call_result(result, &label, remote)
    }

    /// Call `tool` on the plugin instance at `endpoint` for a caller on
    /// `channel`. `meta` makes the caller context each attempt sends as
    /// `_meta.tracon`: a fresh one for a retry, since a plugin takes each
    /// nonce once.
    #[allow(clippy::too_many_arguments)]
    pub async fn call_plugin(
        &self,
        endpoint: &crate::plugin::supervisor::Endpoint,
        plugin: &str,
        channel: &str,
        tool: &str,
        args: &Value,
        mut meta: impl FnMut() -> Result<Value, String>,
        timeout: Duration,
    ) -> Result<Value, String> {
        let http = unix_client(&endpoint.socket);
        let target = Target {
            http: &http,
            url: PLUGIN_URL,
        };
        let label = format!("plugin {plugin}");
        let instance = crate::plugin::context::instance_of(&endpoint.token);
        let result = self
            .request_at(
                target,
                (format!("{PLUGIN_KEY}{plugin}"), channel.to_string()),
                &instance,
                &[],
                "tools/call",
                || {
                    let meta = meta().map_err(Failure::NotSent)?;
                    Ok(json!({
                        "name": tool,
                        "arguments": args,
                        "_meta": { "tracon": meta },
                    }))
                },
                timeout,
            )
            .await
            .map_err(|f| f.describe(&label))?;
        call_result(result, &label, tool)
    }

    /// One request in the MCP session the node holds with `server` for
    /// `channel`, starting one first when there is none, and once more when
    /// the server has forgotten it.
    async fn request(
        &self,
        server: &McpServer,
        headers: &[(String, String)],
        channel: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, Failure> {
        self.request_at(
            self.target(server),
            (server.name.clone(), channel.to_string()),
            &fingerprint(server),
            headers,
            method,
            || Ok(params.clone()),
            timeout,
        )
        .await
    }

    /// [`Self::request`] at `target`, in the session held under `key` while
    /// it was made for `print`. `params` is called once for each attempt.
    #[allow(clippy::too_many_arguments)]
    async fn request_at(
        &self,
        target: Target<'_>,
        key: (String, String),
        print: &str,
        headers: &[(String, String)],
        method: &str,
        mut params: impl FnMut() -> Result<Value, Failure>,
        timeout: Duration,
    ) -> Result<Value, Failure> {
        for _ in 0..2 {
            let held = self
                .connections
                .lock()
                .get(&key)
                .filter(|c| c.fingerprint == print)
                .cloned();
            let connection = match held {
                Some(connection) => connection,
                None => {
                    let id = self.next_id();
                    let connection =
                        handshake(target, id, headers, LIST_TIMEOUT)
                            .await
                            .map(|c| Connection {
                                fingerprint: print.to_string(),
                                ..c
                            })?;
                    self.connections
                        .lock()
                        .insert(key.clone(), connection.clone());
                    connection
                }
            };
            match rpc(
                target,
                self.next_id(),
                headers,
                Some(&connection),
                method,
                params()?,
                timeout,
            )
            .await
            {
                Err(Failure::Expired) => {
                    self.connections.lock().remove(&key);
                }
                outcome => return outcome.map(|(result, _)| result),
            }
        }
        Err(Failure::Expired)
    }

    fn target<'a>(&'a self, server: &'a McpServer) -> Target<'a> {
        Target {
            http: &self.http,
            url: &server.url,
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Each server as the node sees it now, for the operator: where it is,
    /// who it serves, whether it could be listed, and each tool with what
    /// the server's entry says of it and what a call to it is answered now.
    pub fn status(&self, servers: &[McpServer], policy: &Policy) -> Vec<Value> {
        let listings = self.listings.read();
        servers
            .iter()
            .map(|server| {
                let listing = listings
                    .get(&server.name)
                    .filter(|l| l.fingerprint == fingerprint(server));
                let channel = server.channels.first().map(String::as_str).unwrap_or("");
                let tools: Vec<Value> = listing
                    .map(|l| l.tools.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .map(|tool| {
                        let policy_decision = policy.decide(&crate::policy::Request {
                            channel,
                            action: &tool.name,
                            kind: Some(super::TOOL_KIND),
                            resource: None,
                            command: None,
                            arguments: None,
                            doc_kinds: None,
                        });
                        let entry = Entry::server(server);
                        let decision = decide(&entry, &tool.remote, policy, policy_decision);
                        json!({
                            "name": tool.name,
                            "remote": tool.remote,
                            "description": bounded(&tool.description, 300),
                            "setting": entry.setting(&tool.remote).map(verdict_name),
                            "decision": verdict_name(decision.verdict),
                            "rule_id": decision.rule_id,
                        })
                    })
                    .collect();
                json!({
                    "name": server.name,
                    "url": server.url,
                    "channels": server.channels,
                    "credential": server.credential,
                    "listed": listing.is_some_and(|l| l.fetched.is_some()),
                    "listed_secs_ago": listing.and_then(|l| l.fetched).map(|at| at.elapsed().as_secs()),
                    "error": listing.and_then(|l| l.error.clone()),
                    "tools": tools,
                })
            })
            .collect()
    }
}

/// Where a request goes: the client that carries it and the URL it names.
/// An `[[mcp_server]]` is reached over HTTP at its `url`; a plugin over its
/// Unix socket (`unix_client`), at `PLUGIN_URL`.
#[derive(Clone, Copy)]
pub struct Target<'a> {
    pub http: &'a reqwest::Client,
    pub url: &'a str,
}

/// The URL a plugin is asked at: the host names nothing, the socket is the
/// connection.
pub const PLUGIN_URL: &str = "http://plugin/";

/// What a plugin's MCP sessions are held under, before its name: no
/// `[[mcp_server]]` name can hold a `:`.
const PLUGIN_KEY: &str = "plugin:";

/// A client for an MCP server on a Unix socket: a plugin
/// (`crate::plugin::supervisor`). No proxy and no redirects.
pub fn unix_client(socket: &std::path::Path) -> reqwest::Client {
    reqwest::Client::builder()
        .unix_socket(socket)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("http client")
}

/// Whether the MCP server `http` reaches (`unix_client`) answers
/// `initialize` within `timeout`: a plugin's readiness. No credential is
/// sent; the instance token travels with each call.
pub async fn initialize_unix(http: &reqwest::Client, timeout: Duration) -> Result<(), String> {
    let target = Target {
        http,
        url: PLUGIN_URL,
    };
    handshake(target, 1, &[], timeout)
        .await
        .map(|_| ())
        .map_err(|f| f.describe("plugin"))
}

/// Every tool the plugin `http` reaches lists, in an MCP session of its
/// own, each named as a session is offered it (`<plugin>__<tool>`, empty
/// when that does not fit) and by its own name.
pub async fn list_unix(
    http: &reqwest::Client,
    plugin: &str,
    timeout: Duration,
) -> Result<Vec<RemoteTool>, String> {
    let target = Target {
        http,
        url: PLUGIN_URL,
    };
    let label = format!("plugin {plugin}");
    let connection = handshake(target, 1, &[], timeout)
        .await
        .map_err(|f| f.describe(&label))?;
    let mut tools: Vec<RemoteTool> = Vec::new();
    let mut cursor: Option<String> = None;
    for page_no in 0..MAX_PAGES {
        let params = match &cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        let (page, _) = rpc(
            target,
            2 + page_no as u64,
            &[],
            Some(&connection),
            "tools/list",
            params,
            timeout,
        )
        .await
        .map_err(|f| f.describe(&label))?;
        tools.extend(page_tools(&page).into_iter().map(|tool| RemoteTool {
            name: namespaced(plugin, &tool.remote).unwrap_or_default(),
            ..tool
        }));
        cursor = next_cursor(&page);
        if cursor.is_none() {
            break;
        }
    }
    Ok(tools)
}

/// The tools one page of `tools/list` names, by their own names only.
fn page_tools(page: &Value) -> Vec<RemoteTool> {
    page["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            Some(RemoteTool {
                name: String::new(),
                remote: tool["name"].as_str()?.to_string(),
                description: tool["description"].as_str().unwrap_or_default().to_string(),
                input_schema: input_schema(&tool["inputSchema"]),
            })
        })
        .collect()
}

fn next_cursor(page: &Value) -> Option<String> {
    page["nextCursor"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

/// A `tools/call` result as a session is given it: the server's own
/// content, and its structured result; one it marks as an error is
/// returned as one, `server` naming what answered.
fn call_result(result: Value, server: &str, remote: &str) -> Result<Value, String> {
    let content = result
        .get("content")
        .filter(|c| c.is_array())
        .cloned()
        .unwrap_or_else(|| json!([]));
    if result["isError"].as_bool() == Some(true) {
        let text: Vec<&str> = content
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["text"].as_str())
            .collect();
        return Err(format!(
            "{server} reports an error from {remote}: {}",
            if text.is_empty() {
                "no detail".to_string()
            } else {
                text.join("\n")
            }
        ));
    }
    let mut out = json!({ "content": content });
    if let Some(structured) = result.get("structuredContent").filter(|s| s.is_object()) {
        out["structuredContent"] = structured.clone();
    }
    Ok(out)
}

/// `initialize`, then `notifications/initialized`: the MCP session it
/// starts, its fingerprint left for the caller to set.
async fn handshake(
    target: Target<'_>,
    id: u64,
    headers: &[(String, String)],
    timeout: Duration,
) -> Result<Connection, Failure> {
    let (result, session_id) = rpc(
        target,
        id,
        headers,
        None,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "mcp-client", "version": "1.0.0" },
        }),
        timeout,
    )
    .await
    .map_err(|failure| match failure {
        // Nothing was asked of the server yet.
        Failure::Unknown(e) => Failure::Unreachable(e),
        other => other,
    })?;
    let connection = Connection {
        fingerprint: String::new(),
        session_id,
        version: result["protocolVersion"]
            .as_str()
            .unwrap_or(PROTOCOL_VERSION)
            .to_string(),
    };
    let response = post(
        target,
        headers,
        Some(&connection),
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .timeout(timeout)
    .send()
    .await
    .map_err(|e| Failure::Unreachable(e.to_string()))?;
    if !response.status().is_success() {
        return Err(Failure::Refused(format!(
            "HTTP {} to notifications/initialized",
            response.status()
        )));
    }
    Ok(connection)
}

fn post(
    target: Target<'_>,
    headers: &[(String, String)],
    connection: Option<&Connection>,
    body: &Value,
) -> reqwest::RequestBuilder {
    let mut request = target
        .http
        .post(target.url)
        .header("accept", "application/json, text/event-stream")
        .json(body);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    if let Some(connection) = connection {
        request = request.header("mcp-protocol-version", &connection.version);
        if let Some(id) = &connection.session_id {
            request = request.header("mcp-session-id", id);
        }
    }
    request
}

/// One JSON-RPC request, answered as JSON or as an event stream, and the
/// session id the server set, if it set one.
async fn rpc(
    target: Target<'_>,
    id: u64,
    headers: &[(String, String)],
    connection: Option<&Connection>,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<(Value, Option<String>), Failure> {
    let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let request = post(target, headers, connection, &body);
    let exchange = async {
        let response = request.send().await.map_err(|e| {
            if e.is_connect() || e.is_builder() {
                Failure::Unreachable(e.to_string())
            } else {
                Failure::Unknown(e.to_string())
            }
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND
            && connection.is_some_and(|c| c.session_id.is_some())
        {
            return Err(Failure::Expired);
        }
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let streamed = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.starts_with("text/event-stream"));
        if !status.is_success() {
            let text = read_bounded(response).await.unwrap_or_default();
            return Err(Failure::Refused(match status.as_u16() {
                401 | 403 => format!("refused the credential (HTTP {status})"),
                _ => format!("HTTP {status}: {}", bounded(text.trim(), 300)),
            }));
        }
        let message = if streamed {
            read_event(response, id).await?
        } else {
            let text = read_bounded(response).await.map_err(Failure::Unknown)?;
            let value: Value = serde_json::from_str(&text)
                .map_err(|e| Failure::Refused(format!("unreadable answer: {e}")))?;
            match value {
                Value::Array(batch) => batch
                    .into_iter()
                    .find(|m| m["id"] == json!(id))
                    .ok_or_else(|| Failure::Refused("no answer to the request".into()))?,
                single => single,
            }
        };
        if let Some(error) = message.get("error") {
            return Err(Failure::Refused(
                error["message"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| error.to_string()),
            ));
        }
        Ok((message["result"].clone(), session_id))
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(outcome) => outcome,
        Err(_) => Err(Failure::Unknown(format!(
            "no answer to {method} within {}s",
            timeout.as_secs()
        ))),
    }
}

fn verdict_name(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Allow => "allow",
        Verdict::Ask => "ask",
        Verdict::Deny => "deny",
    }
}

async fn read_bounded(mut response: reqwest::Response) -> Result<String, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_RESPONSE {
            return Err(format!("answer larger than {MAX_RESPONSE} bytes"));
        }
    }
    String::from_utf8(body).map_err(|e| e.to_string())
}

/// Read an event stream until the message answering `id`. The server may
/// send requests or notifications of its own first; they are passed over.
async fn read_event(mut response: reqwest::Response, id: u64) -> Result<Value, Failure> {
    let mut buffer = String::new();
    let mut read = 0usize;
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|e| Failure::Unknown(e.to_string()))?;
        let Some(chunk) = chunk else {
            return Err(Failure::Unknown(
                "the event stream ended without an answer".into(),
            ));
        };
        read += chunk.len();
        if read > MAX_RESPONSE {
            return Err(Failure::Refused(format!(
                "answer larger than {MAX_RESPONSE} bytes"
            )));
        }
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        let normalized = buffer.replace("\r\n", "\n");
        let mut events: Vec<&str> = normalized.split("\n\n").collect();
        // The last piece is an event still arriving.
        let pending = events.pop().unwrap_or_default().to_string();
        for event in events {
            let data: Vec<&str> = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if data.is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&data.join("\n")) else {
                continue;
            };
            if message["id"] == json!(id)
                && (message.get("result").is_some() || message.get("error").is_some())
            {
                return Ok(message);
            }
        }
        buffer = pending;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> McpServer {
        McpServer {
            name: "linear".into(),
            url: "http://127.0.0.1:1/mcp".into(),
            channels: vec!["personal".into()],
            allow: vec!["list_*".into(), "get_issue".into()],
            ask: vec!["list_secrets".into()],
            deny: vec!["delete_*".into()],
            ..Default::default()
        }
    }

    fn ask() -> Decision {
        Decision {
            verdict: Verdict::Ask,
            rule_id: None,
            reason: None,
        }
    }

    #[test]
    fn a_tool_name_is_split_back_to_its_server() {
        let servers = vec![server()];
        let (found, rest) = server_of(&servers, "linear__create_issue").unwrap();
        assert_eq!(found.name, "linear");
        assert_eq!(rest, "create_issue");
        assert!(server_of(&servers, "other__create_issue").is_none());
        assert!(server_of(&servers, "linear__").is_none());
        assert!(server_of(&servers, "doc_read").is_none());
    }

    #[test]
    fn deny_wins_over_ask_and_ask_over_allow() {
        let s = server();
        assert_eq!(
            Entry::server(&s).setting("delete_issue"),
            Some(Verdict::Deny)
        );
        assert_eq!(
            Entry::server(&s).setting("list_secrets"),
            Some(Verdict::Ask)
        );
        assert_eq!(
            Entry::server(&s).setting("list_issues"),
            Some(Verdict::Allow)
        );
        assert_eq!(Entry::server(&s).setting("create_issue"), None);
    }

    #[test]
    fn a_signed_denial_dominates_and_an_unnamed_tool_is_asked() {
        let s = server();
        let policy = Policy::shipped();
        let denied = Decision {
            verdict: Verdict::Deny,
            rule_id: Some("r".into()),
            reason: Some("no".into()),
        };
        assert_eq!(
            decide(&Entry::server(&s), "list_issues", &policy, denied).verdict,
            Verdict::Deny
        );
        assert_eq!(
            decide(&Entry::server(&s), "list_issues", &policy, ask()).verdict,
            Verdict::Allow
        );
        assert_eq!(
            decide(&Entry::server(&s), "create_issue", &policy, ask()).verdict,
            Verdict::Ask
        );
        assert_eq!(
            decide(&Entry::server(&s), "delete_issue", &policy, ask()).verdict,
            Verdict::Deny
        );
        // The entry's ask holds even where a policy rule would allow.
        let allowed = Decision {
            verdict: Verdict::Allow,
            rule_id: Some("p".into()),
            reason: None,
        };
        assert_eq!(
            decide(&Entry::server(&s), "list_secrets", &policy, allowed.clone()).verdict,
            Verdict::Ask
        );
        assert_eq!(
            decide(&Entry::server(&s), "create_issue", &policy, allowed).verdict,
            Verdict::Allow
        );
        // An allow widens nothing without a trusted bundle.
        let untrusted = Policy::default();
        assert_eq!(
            decide(&Entry::server(&s), "list_issues", &untrusted, ask()).verdict,
            Verdict::Ask
        );
    }

    #[test]
    fn a_plugin_is_decided_as_a_server_is_under_its_own_entry() {
        let entry = PluginEntry {
            name: "sql-review".into(),
            allow: vec!["query".into(), "list_*".into()],
            ask: vec!["list_secrets".into()],
            deny: vec!["drop_*".into()],
            ..Default::default()
        };
        let on = Entry::plugin("sql-review", Some(&entry));
        let policy = Policy::shipped();
        let allowed = Decision {
            verdict: Verdict::Allow,
            rule_id: Some("p".into()),
            reason: None,
        };
        let denied = Decision {
            verdict: Verdict::Deny,
            rule_id: Some("r".into()),
            reason: Some("no".into()),
        };
        // (tool, policy's own decision, verdict, rule)
        let cases = [
            (
                "query",
                ask(),
                Verdict::Allow,
                Some("plugin.sql-review.allow"),
            ),
            ("query", denied.clone(), Verdict::Deny, Some("r")),
            (
                "drop_table",
                allowed.clone(),
                Verdict::Deny,
                Some("plugin.sql-review.deny"),
            ),
            (
                "list_secrets",
                allowed.clone(),
                Verdict::Ask,
                Some("plugin.sql-review.ask"),
            ),
            (
                "list_tables",
                ask(),
                Verdict::Allow,
                Some("plugin.sql-review.allow"),
            ),
            ("explain", ask(), Verdict::Ask, None),
            ("explain", allowed.clone(), Verdict::Allow, Some("p")),
        ];
        for (tool, policy_decision, verdict, rule) in cases {
            let decision = decide(&on, tool, &policy, policy_decision);
            assert_eq!(decision.verdict, verdict, "{tool}");
            assert_eq!(decision.rule_id.as_deref(), rule, "{tool}");
        }
        let reason = decide(&on, "drop_table", &policy, ask()).reason.unwrap();
        assert_eq!(
            reason,
            "the operator does not allow drop_table on plugin sql-review"
        );
        // No entry: only the signed policy allows, and an unnamed tool is
        // asked.
        let bare = Entry::plugin("open", None);
        assert_eq!(
            decide(&bare, "query", &policy, allowed).verdict,
            Verdict::Allow
        );
        assert_eq!(decide(&bare, "query", &policy, ask()).verdict, Verdict::Ask);
        // The entry's allow widens nothing without a trusted bundle.
        assert_eq!(
            decide(&on, "query", &Policy::default(), ask()).verdict,
            Verdict::Ask
        );
    }

    #[test]
    fn names_are_cut_to_what_a_function_name_carries() {
        assert_eq!(
            namespaced("linear", "issues.create").as_deref(),
            Some("linear__issues_create")
        );
        assert_eq!(namespaced("linear", &"x".repeat(60)), None);
        assert_eq!(namespaced("linear", ""), None);
    }

    #[test]
    fn an_offered_schema_is_an_object_with_a_list_of_required_fields() {
        assert_eq!(
            input_schema(&Value::Null),
            json!({ "type": "object", "properties": {} })
        );
        assert_eq!(
            input_schema(&json!({ "properties": {}, "required": null })),
            json!({ "type": "object", "properties": {} })
        );
    }
}
