//! The OpenCode adapter. Drives `opencode serve` over its own HTTP API.
//!
//! OpenCode is not a stdio agent: it is a server that the node starts inside
//! the runner, reaches on a loopback publish (Podman), the pod's own address
//! (Kubernetes) or plain loopback (the local runner), and drives with HTTP
//! requests plus one durable per-session event stream. Everything this adapter
//! sends and decodes was read from the pinned release's inventory in
//! `docs/reference/opencode-v1.18.30/` and checked against the pinned binary.
//!
//! Three properties are load-bearing and are asserted rather than assumed:
//!
//! * The launch environment is the whole of the harness's configuration. The
//!   config file the node writes is the only one it reads, its home and all
//!   four XDG directories are this session's own, and every ambient discovery
//!   path is switched off (`config-state.md` §9.2).
//! * Every tool class is `ask`, because OpenCode emits no event at all for an
//!   action its own ruleset allows (finding 1). A permission answered here is
//!   answered `once` and never `always`: an `always` would widen OpenCode's
//!   own ruleset behind the node's back (finding 2).
//! * The runner's auth store holds no `oauth` and no `wellknown` record. The
//!   first is what keeps every bundled subscription plugin inert — including
//!   the Codex one, which would otherwise rewrite requests past the gateway
//!   (finding 9) — and the second is what stops a remote config fetch from
//!   minting a credential by running a command a server named.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use super::{
    AdapterError, HarnessAdapter, HarnessCompat, HarnessEvent, HarnessHandle, HarnessVersion,
    LaunchSpec, Layout, ModelOption, PermissionReply, PermissionRequest, ProtocolSupport,
    TurnResult,
};
use crate::adapter::types::{self, PermissionOption, ToolCall, ToolCallUpdate, Usage};
use crate::gateway::model::Wiring;
use crate::runner::{free_loopback_port, Runner, RunnerCommand};

/// How long the server has to answer `GET /global/health` after the runner
/// reports it started. It covers an image pull only in the sense that the
/// runner has already waited for that; this is process start plus the
/// server's own boot.
const START_TIMEOUT: Duration = Duration::from_secs(60);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
/// One request that is not a stream. Long enough for a busy server, short
/// enough that a wedged one surfaces as an error rather than a hang.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait before reconnecting the durable stream after it drops.
const RECONNECT_DELAY: Duration = Duration::from_millis(250);
/// What the node tells OpenCode to wait for one of its MCP tool calls. The
/// effective default is the SDK's 60 s rather than the documented 5 s
/// (`config-state.md` §5.4), so it is stated rather than inherited.
const MCP_TIMEOUT_MS: u64 = 120_000;

/// Context window and output cap written for a model the operator declared
/// without them. They are only what the picker and the harness display; the
/// gateway's counting is unaffected.
const DEFAULT_CONTEXT: u64 = 200_000;
const DEFAULT_OUTPUT: u64 = 32_000;

/// Providers OpenCode knows about that this node never wires. `amazon-bedrock`
/// is the one with teeth: its SDK reads AWS environment and, failing that, the
/// instance metadata service (`providers.md` §5.2).
const NEVER_OFFERED: [&str; 3] = ["amazon-bedrock", "opencode", "github-copilot"];

pub struct OpenCodeAdapter {
    pinned: String,
}

impl OpenCodeAdapter {
    pub const ID: &'static str = "opencode";

    /// The version this node's harness image installs.
    /// `containers/harness-opencode/Containerfile` fetches exactly this
    /// release at exactly one digest; `the_image_installs_the_pinned_opencode`
    /// keeps the two from drifting apart.
    pub const PINNED_VERSION: &'static str = "1.18.30";

    /// OpenCode's server API carries no protocol version of its own, so the
    /// pinned release's major.minor API *is* the protocol identity and 1 is
    /// the name given to that revision here. What the server reports at
    /// `/global/health` is recorded as the version the session ran against,
    /// and a build outside the pin is refused before a session exists.
    pub const PROTOCOL: ProtocolSupport = ProtocolSupport {
        name: "opencode-http",
        min: 1,
        max: 1,
    };

    /// Plugin packages this node's harness image bakes into OpenCode's
    /// offline package cache, as `<package>@<version>`.
    ///
    /// Read from the image's own toolchain profile rather than written twice.
    /// A plugin is loaded with a raw `await import()` into the server's
    /// process, holding the server's credentials and `Bun.$`
    /// (`config-state.md` §4.6), and resolution is a bare existence check in
    /// that cache with no integrity verification (§4.4) — so the only bounded
    /// question is which packages the *image* made resolvable at all, and the
    /// profile the image was built from is the honest answer to it. A
    /// manifest naming anything else is refused at build, with the cache path
    /// it would have needed.
    pub fn baked_plugins() -> Vec<String> {
        let seed = &crate::runner::toolchain::profile().plugin;
        vec![format!("{}@{}", seed.package, seed.version)]
    }

    pub fn new(pinned: impl Into<String>) -> Self {
        Self {
            pinned: pinned.into(),
        }
    }

    /// OpenCode has no single state-directory variable, and the two that come
    /// closest are worse than nothing: `OPENCODE_CONFIG` names a file rather
    /// than a directory, and `OPENCODE_CONFIG_DIR` makes the directory a
    /// *discovered* one — which writes a `.gitignore` into it and forks an
    /// npm install from it (`config-state.md` §1.2). So the layout names no
    /// variable at all: where this harness keeps things is decided entirely by
    /// the `HOME` and XDG values in `launch_env`, and the mount below is the
    /// read-only manifest plus this session's own writable tree.
    pub const fn layout() -> Layout {
        Layout {
            dir: ".opencode",
            env: "",
        }
    }

    /// The read-only manifest, relative to the state directory.
    const CONFIG_FILE: &'static str = "opencode.json";
    /// The pinned catalogue, relative to the state directory.
    const MODELS_FILE: &'static str = "models.json";
    /// The auth store the runner is given, relative to the state directory.
    const AUTH_FILE: &'static str = "auth.json";
    /// This session's writable tree, relative to the state directory.
    const RUN_DIR: &'static str = "run";
    fn state_dir(home: &str) -> String {
        format!("{home}/{}", Self::layout().dir)
    }

    /// The per-session tree: a home with nothing in it and the four XDG
    /// directories, none of which is the state volume every session shares.
    fn run_dir(home: &str) -> String {
        format!("{}/{}", Self::state_dir(home), Self::RUN_DIR)
    }

    /// The launch environment, settled by `config-state.md` §9.2. Every entry
    /// is here for a reason recorded in that document; an unset one is an
    /// ambient discovery path left open, not a default.
    fn launch_env(home: &str, password: &str) -> Vec<(String, String)> {
        let state = Self::state_dir(home);
        let run = Self::run_dir(home);
        [
            // Per-session, and none of them the shared state volume.
            ("HOME", format!("{run}/home")),
            ("XDG_CONFIG_HOME", format!("{run}/config")),
            ("XDG_DATA_HOME", format!("{run}/data")),
            ("XDG_CACHE_HOME", format!("{run}/cache")),
            ("XDG_STATE_HOME", format!("{run}/state")),
            ("OPENCODE_DB", format!("{run}/state/opencode.db")),
            // The one file it may read, mounted read-only outside the worktree.
            ("OPENCODE_CONFIG", format!("{state}/{}", Self::CONFIG_FILE)),
            // Where the launch manifest's skills are mounted. The config file
            // is written before the node knows this path, so it names the
            // variable instead and `{env:…}` substitution resolves it when
            // the file is loaded (`config-state.md` §1.5). Set even when the
            // manifest has no skills: the directory then does not exist and
            // OpenCode logs a missing skill path, which is the honest state
            // and not an error.
            (
                crate::manifest::SKILL_ROOT_ENV,
                format!("{state}/{}", crate::manifest::SKILL_DIR),
            ),
            // The catalogue is declared, not fetched. Both are needed: the
            // path alone leaves an hourly refresh running, the flag alone
            // falls back to a build-time snapshot this node did not choose.
            (
                "OPENCODE_MODELS_PATH",
                format!("{state}/{}", Self::MODELS_FILE),
            ),
            ("OPENCODE_DISABLE_MODELS_FETCH", "true".into()),
            // Ambient discovery, plugins, skills and updates, all off.
            ("OPENCODE_DISABLE_PROJECT_CONFIG", "true".into()),
            ("OPENCODE_PURE", "1".into()),
            ("OPENCODE_DISABLE_DEFAULT_PLUGINS", "true".into()),
            ("OPENCODE_DISABLE_EXTERNAL_SKILLS", "true".into()),
            ("OPENCODE_DISABLE_CLAUDE_CODE", "true".into()),
            ("OPENCODE_DISABLE_LSP_DOWNLOAD", "true".into()),
            ("OPENCODE_DISABLE_AUTOUPDATE", "true".into()),
            ("OPENCODE_DISABLE_SHARE", "true".into()),
            // The store is supplied inline and holds nothing; the file next to
            // it says the same thing for the paths that read a file instead.
            ("OPENCODE_AUTH_CONTENT", empty_auth_store()),
            // Without a password the server is entirely unauthenticated
            // (finding 4). The username is stated rather than defaulted so the
            // credential this node sends is the credential it set.
            ("OPENCODE_SERVER_USERNAME", SERVER_USERNAME.into()),
            ("OPENCODE_SERVER_PASSWORD", password.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// `opencode serve` on loopback inside the runner. mDNS is off by
    /// omission and refused on loopback anyway; `--pure` is the plugin
    /// off-switch; the logs go to the runner's stdout, which is where a failed
    /// launch is read from.
    fn serve_cmd(spec: &LaunchSpec, port: u16, password: &str) -> RunnerCommand {
        let mut env = spec.env.clone();
        env.extend(Self::launch_env(&spec.harness_home, password));
        RunnerCommand {
            argv: vec![
                "opencode".into(),
                "serve".into(),
                "--pure".into(),
                "--hostname".into(),
                "127.0.0.1".into(),
                "--port".into(),
                port.to_string(),
                "--print-logs".into(),
                "--log-level".into(),
                "INFO".into(),
            ],
            env,
            workdir: Some(spec.cwd_in_runner.clone()),
            name: spec.container_name.clone(),
            expose: Some(port),
            ..Default::default()
        }
    }
}

/// The username OpenCode's Basic auth defaults to and this node sets.
const SERVER_USERNAME: &str = "opencode";

/// An auth store with nothing in it. Asserted rather than assumed: an `oauth`
/// record would arm a subscription plugin inside the runner and a `wellknown`
/// record would fetch a remote config and run a command to mint a credential.
fn empty_auth_store() -> String {
    "{}".to_string()
}

/// The AI SDK package for a provider shape. The Codex-shaped provider is
/// wired as an ordinary OpenAI provider on purpose: it must never be named
/// `openai`, and with no OAuth record in the store the Codex plugin never
/// installs its URL-rewriting fetch (finding 9).
fn npm_for(shape: &str) -> &'static str {
    match shape {
        crate::config::SHAPE_ANTHROPIC => "@ai-sdk/anthropic",
        crate::config::SHAPE_OPENAI | crate::config::SHAPE_OPENAI_CODEX => "@ai-sdk/openai",
        _ => "@ai-sdk/openai-compatible",
    }
}

/// Where the harness sends this provider's requests. The gateway's allowlist
/// is written in terms of the paths that follow, so the `/v1` belongs here
/// rather than in the SDK's idea of a default.
fn provider_base_url(base: &str) -> String {
    format!("{base}/v1")
}

/// The provider table, the permission ruleset, the MCP entry and the rest of
/// the configuration OpenCode is allowed to read.
fn config_document(wiring: &Wiring, mcp_servers: &[Value]) -> Value {
    let mut providers = serde_json::Map::new();
    for provider in &wiring.providers {
        let mut models = serde_json::Map::new();
        for model in &provider.models {
            models.insert(model.id.clone(), config_model(model));
        }
        providers.insert(
            provider.name.clone(),
            json!({
                "npm": npm_for(&provider.shape),
                "name": format!("{} (tracon)", provider.name),
                // Both spellings of the same fact, because two code paths in
                // this binary read different ones: the ai-sdk session builds
                // its client from `options.baseURL`, and the newer session
                // runner resolves a base URL from `api` by way of the
                // catalogue. A provider that carries only `options.baseURL`
                // is served by the second path from the *provider's own*
                // default host — `api.anthropic.com` — with the gateway
                // bypassed and no error anywhere. Observed against the pinned
                // binary; `node/tests/opencode_providers.rs` holds it.
                "api": provider_base_url(&provider.base_url),
                "options": {
                    "baseURL": provider_base_url(&provider.base_url),
                    // A placeholder: the real credential is attached by the
                    // gateway and never reaches the runner.
                    "apiKey": wiring.token,
                },
                "models": Value::Object(models),
            }),
        );
    }
    let mut disabled: Vec<String> = NEVER_OFFERED.iter().map(|p| (*p).to_string()).collect();
    disabled.retain(|name| !providers.contains_key(name));

    let mut document = json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": Value::Object(providers.clone()),
        // `disabled_providers` stops a provider's loader running at all;
        // `enabled_providers` is the allowlist on top of it (providers.md §1.3).
        "disabled_providers": disabled,
        "enabled_providers": providers.keys().cloned().collect::<Vec<_>>(),
        "permission": all_ask(),
        "share": "disabled",
        "autoshare": false,
        "autoupdate": false,
        "experimental": { "mcp_timeout": MCP_TIMEOUT_MS },
    });
    if let Some(mcp) = mcp_document(mcp_servers) {
        document["mcp"] = mcp;
    }
    for (key, value) in manifest_document(&wiring.manifest) {
        document[key] = value;
    }
    document
}

/// The operator's half of the same document: the launch manifest rendered
/// into the keys OpenCode reads.
///
/// Four decisions are visible here and each is a refusal of an upstream
/// default:
///
/// * `skills.paths` names one directory and `skills.urls` is never written.
///   Upstream discovers skills from six places and fetches the URL list over
///   plain HTTP before any interaction, with nothing that disables it
///   (`config-state.md` §3.2, §3.6). The three `OPENCODE_DISABLE_*` variables
///   in `launch_env` close the other five paths; not writing `urls` closes the
///   sixth, and it is a config control rather than a capability one, which is
///   why the runner also has no egress.
/// * The path is `{env:…}`, not a literal. A relative `skills.paths` entry is
///   resolved against the *worktree* (§3.2 #5), which is the one directory a
///   session can write; the variable resolves to an absolute path outside it
///   that the launch environment supplies.
/// * `plugin` carries only packages the image baked. Resolution is a bare
///   existence check in the offline cache (§4.4), so a name the image does
///   not have is not a warning — it is a plugin that silently is not there,
///   or a registry fetch in a runner with no network. The manifest refuses it
///   before the launch instead.
/// * `lsp` and `formatter` are not written here at all. The image's toolchain
///   profile owns them — it names absolute paths in an image this file knows
///   nothing about — and `scratch_files` merges that fragment in afterwards.
///   The manifest still carries the profile's names, so the digest on a
///   session says which toolchain it ran with, but rendering them twice would
///   be two sources for one fact.
fn manifest_document(manifest: &crate::manifest::LaunchManifest) -> Vec<(&'static str, Value)> {
    let mut keys: Vec<(&'static str, Value)> = Vec::new();
    if !manifest.skills.is_empty() {
        keys.push((
            "skills",
            json!({ "paths": [format!("{{env:{}}}", crate::manifest::SKILL_ROOT_ENV)] }),
        ));
    }
    if !manifest.plugins.is_empty() {
        keys.push(("plugin", json!(manifest.plugins)));
    }
    keys
}

/// One declared model, in the shape the config merges over the catalogue.
fn config_model(model: &crate::config::ModelDecl) -> Value {
    json!({
        "id": model.id,
        "name": model.label(),
        "reasoning": model.reasoning,
        "attachment": model.attachment,
        "tool_call": true,
        "limit": {
            "context": if model.context > 0 { model.context } else { DEFAULT_CONTEXT },
            "output": if model.output > 0 { model.output } else { DEFAULT_OUTPUT },
        },
    })
}

/// Every tool class set to `ask`, including the wildcard that covers the ones
/// this build has never heard of. An `allow` anywhere here would execute with
/// no event at all, and the node would never learn the action happened.
fn all_ask() -> Value {
    let mut rules = serde_json::Map::new();
    rules.insert("*".into(), json!("ask"));
    for tool in [
        "read",
        "edit",
        "glob",
        "grep",
        "list",
        "bash",
        "task",
        "external_directory",
        "todowrite",
        "question",
        "webfetch",
        "websearch",
        "lsp",
        "doom_loop",
        "skill",
    ] {
        rules.insert(tool.into(), json!("ask"));
    }
    Value::Object(rules)
}

/// The node's own MCP server, in the shape OpenCode's remote transport wants.
/// `oauth: false` is required: without it a 401 attaches an OAuth provider and
/// tries to open a browser (`config-state.md` §5.2).
fn mcp_document(servers: &[Value]) -> Option<Value> {
    if servers.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::new();
    for server in servers {
        let name = server["name"].as_str().unwrap_or("tracon").to_string();
        let mut headers = serde_json::Map::new();
        for header in server["headers"].as_array().into_iter().flatten() {
            if let (Some(k), Some(v)) = (header["name"].as_str(), header["value"].as_str()) {
                headers.insert(k.to_string(), json!(v));
            }
        }
        map.insert(
            name,
            json!({
                "type": "remote",
                "url": server["url"],
                "headers": Value::Object(headers),
                "oauth": false,
                "enabled": true,
                "timeout": MCP_TIMEOUT_MS,
            }),
        );
    }
    Some(Value::Object(map))
}

/// The pinned catalogue, in the shape `OPENCODE_MODELS_PATH` is read with: a
/// map of provider id to a provider whose `models` are the ones this node
/// declared. Nothing is fetched, so what the picker offers and what the
/// gateway will serve are the same list by construction (finding 11).
fn catalogue_document(wiring: &Wiring) -> Value {
    let mut catalogue = serde_json::Map::new();
    for provider in &wiring.providers {
        let mut models = serde_json::Map::new();
        for model in &provider.models {
            let mut entry = config_model(model);
            entry["release_date"] = json!("");
            entry["temperature"] = json!(false);
            entry["cost"] = json!({ "input": 0, "output": 0 });
            entry["modalities"] = json!({ "input": ["text"], "output": ["text"] });
            entry["status"] = json!("active");
            // Per model as well as per provider: this is the field the
            // session runner reads a base URL from, and a model without one
            // falls back to the provider package's own default host.
            entry["provider"] = json!({
                "npm": npm_for(&provider.shape),
                "api": provider_base_url(&provider.base_url),
            });
            models.insert(model.id.clone(), entry);
        }
        catalogue.insert(
            provider.name.clone(),
            json!({
                "id": provider.name,
                "name": format!("{} (tracon)", provider.name),
                "npm": npm_for(&provider.shape),
                // The gateway, not the provider. The catalogue is what the
                // session runner resolves a model's endpoint from, so leaving
                // this out is not "unset" — it is the provider's own host.
                "api": provider_base_url(&provider.base_url),
                // No environment variable supplies a key: the placeholder is
                // in the config and the real credential is the gateway's.
                "env": Vec::<String>::new(),
                "models": Value::Object(models),
            }),
        );
    }
    Value::Object(catalogue)
}

/// The models this node declares, as the picker lists them.
fn declared_models(wiring: &Wiring) -> Vec<ModelOption> {
    wiring
        .providers
        .iter()
        .flat_map(|provider| {
            provider.models.iter().map(move |model| ModelOption {
                value: format!("{}/{}", provider.name, model.id),
                name: format!("{} ({})", model.label(), provider.name),
            })
        })
        .collect()
}

fn parse_version(out: &str) -> String {
    // `opencode --version` prints the bare version and nothing else.
    out.trim()
        .lines()
        .next_back()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The two options tracon's queue is built around. OpenCode's reply vocabulary
/// is `once | always | reject`, and `always` is deliberately not offered: it
/// would widen OpenCode's own ruleset for the rest of the instance's life.
fn options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: types::OPTION_ALLOW_ONCE.into(),
            name: "Allow".into(),
            kind: "allow_once".into(),
        },
        PermissionOption {
            option_id: types::OPTION_REJECT_ONCE.into(),
            name: "Reject".into(),
            kind: "reject_once".into(),
        },
    ]
}

/// What a permission answer becomes on the wire. Only the allow-once option
/// allows, and it allows exactly once: `always` is never sent, whatever was
/// selected (finding 2).
fn reply_body(decision: PermissionReply) -> Value {
    match decision {
        PermissionReply::Selected(option) if option == types::OPTION_ALLOW_ONCE => {
            json!({ "reply": "once" })
        }
        PermissionReply::Selected(_) | PermissionReply::Edited { .. } => json!({
            "reply": "reject",
            "message": "the operator declined this",
        }),
        PermissionReply::Cancelled => json!({
            "reply": "reject",
            "message": "no answer was given before this expired",
        }),
    }
}

/// What the operator reads in the queue: the action and the first resource it
/// names, which for a shell is the command.
fn summarize(action: &str, resources: &[String]) -> String {
    match resources.first() {
        Some(first) if !first.is_empty() => {
            let one = first.replace('\n', " ");
            let short: String = one.chars().take(160).collect();
            format!("{action}: {short}")
        }
        _ => action.to_string(),
    }
}

/// The HTTP client for one harness. Everything it sends carries this session's
/// Basic credential, and nothing it sends goes through a proxy: the endpoint
/// is a loopback publish or a pod address, and an ambient `HTTPS_PROXY` in the
/// node's own environment must not be allowed to redirect it.
#[derive(Clone)]
struct Client {
    http: reqwest::Client,
    base: String,
    authorization: String,
    directory: String,
}

impl Client {
    fn new(endpoint: &str, password: &str, directory: &str) -> Result<Self, AdapterError> {
        use base64::Engine;
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|e| AdapterError::Protocol(format!("harness client: {e}")))?;
        let credential = base64::engine::general_purpose::STANDARD
            .encode(format!("{SERVER_USERNAME}:{password}"));
        Ok(Self {
            http,
            base: format!("http://{endpoint}"),
            authorization: format!("Basic {credential}"),
            directory: directory.to_string(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .header(reqwest::header::AUTHORIZATION, &self.authorization)
    }

    async fn get_json(&self, path: &str) -> Result<Value, AdapterError> {
        let response = self
            .request(reqwest::Method::GET, path)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| AdapterError::Protocol(format!("GET {path}: {e}")))?;
        json_of(path, response).await
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value, AdapterError> {
        let response = self
            .request(reqwest::Method::POST, path)
            .timeout(REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| AdapterError::Protocol(format!("POST {path}: {e}")))?;
        json_of(path, response).await
    }

    /// The v1 routes the node still uses carry the directory explicitly. It is
    /// this session's workspace and nothing else: OpenCode treats a directory
    /// on a request as authorization to instantiate that path (finding 5).
    fn scoped(&self, path: &str) -> String {
        let separator = if path.contains('?') { '&' } else { '?' };
        format!("{path}{separator}directory={}", urlencode(&self.directory))
    }
}

/// The snapshot and reply routes, lent to the ingestion layer. Nothing about
/// the credential or the address leaves the adapter with it: the node's
/// reconciliation says which path it wants, and this is what reaches the
/// harness.
#[async_trait]
impl super::HarnessSnapshots for Client {
    async fn get(&self, path: &str) -> Result<Value, AdapterError> {
        self.get_json(path).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, AdapterError> {
        self.post_json(path, body).await
    }
}

async fn json_of(path: &str, response: reqwest::Response) -> Result<Value, AdapterError> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AdapterError::Protocol(format!(
            "{path}: harness answered {status}: {}",
            body.chars().take(400).collect::<String>()
        )));
    }
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&body)
        .map_err(|e| AdapterError::Protocol(format!("{path}: harness answered non-JSON: {e}")))
}

/// Percent-encode a path for a query parameter. Deliberately small: the only
/// value that reaches it is a container path the node itself chose.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[async_trait]
impl HarnessAdapter for OpenCodeAdapter {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn pinned_version(&self) -> &str {
        &self.pinned
    }

    fn protocol(&self) -> ProtocolSupport {
        Self::PROTOCOL
    }

    fn layout(&self) -> Layout {
        Self::layout()
    }

    /// Everything OpenCode is allowed to read, written per session and mounted
    /// read-only: the provider table with the gateway's base URLs in it
    /// (OpenCode reads no base-URL environment variable — finding 8), the
    /// pinned catalogue, and an auth store with nothing in it.
    fn scratch_files(&self, wiring: &Wiring) -> Vec<(String, String)> {
        let mut config = config_document(wiring, &[]);
        // The `lsp` and `formatter` halves come from the image's toolchain
        // profile, which is the runner's to own: it names absolute paths in
        // an image this file knows nothing about. The manifest records the
        // same profile's names in its digest, so what a session ran with is
        // readable from its row, but it does not render them a second time.
        crate::runner::toolchain::merge_into_config(&mut config);
        let mut files = vec![
            (
                Self::CONFIG_FILE.into(),
                serde_json::to_string_pretty(&config).unwrap_or_else(|_| "{}".into()),
            ),
            (
                Self::MODELS_FILE.into(),
                serde_json::to_string_pretty(&catalogue_document(wiring))
                    .unwrap_or_else(|_| "{}".into()),
            ),
            (Self::AUTH_FILE.into(), empty_auth_store()),
        ];
        // The manifest's skill packages, staged beside the config and mounted
        // read-only like it — one directory per skill under a root the
        // worktree cannot reach and OpenCode does not discover on its own.
        files.extend(wiring.manifest.skill_files());
        files
    }

    fn scratch_dirs(&self) -> Vec<String> {
        vec![Self::RUN_DIR.to_string()]
    }

    async fn version(&self, runner: &dyn Runner) -> Result<HarnessVersion, AdapterError> {
        let out = runner
            .run_capture(RunnerCommand {
                argv: vec!["opencode".into(), "--version".into()],
                name: "opencode-version".into(),
                ..Default::default()
            })
            .await?;
        Ok(HarnessVersion {
            found: parse_version(&String::from_utf8_lossy(&out.stdout)),
            pinned: self.pinned.clone(),
        })
    }

    /// No probe. OpenCode is told its catalogue rather than asked for one, so
    /// the models this node can run are exactly the ones it declared and
    /// wrote into the harness's config (finding 11).
    async fn probe_models(
        &self,
        _runner: &dyn Runner,
        wiring: &Wiring,
    ) -> Result<Vec<ModelOption>, AdapterError> {
        let models = declared_models(wiring);
        if models.is_empty() {
            return Err(AdapterError::Protocol(
                "no models are declared for this harness; add `models` under a \
                 `[providers.<name>]` entry in the node configuration"
                    .into(),
            ));
        }
        Ok(models)
    }

    async fn launch(
        &self,
        runner: &dyn Runner,
        spec: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError> {
        let container = spec.container_name.clone();
        let password = uuid::Uuid::now_v7().simple().to_string();
        let port = free_loopback_port()?;
        let spawned = runner
            .spawn(Self::serve_cmd(&spec, port, &password))
            .await?;
        let endpoint = match spawned.endpoint.clone() {
            Some(endpoint) => endpoint,
            None => {
                let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
                return Err(AdapterError::Protocol(
                    "the runner exposed no endpoint for the harness server".into(),
                ));
            }
        };
        let client = Client::new(&endpoint, &password, &spec.cwd_in_runner)?;

        // Hold the process's stdout and its exit: dropping the exit future
        // kills the harness (the runner spawns with `kill_on_drop`), and
        // stdout has to keep being read or the server blocks on a full pipe.
        // The log lines are also what a failed start is diagnosed from.
        let logs = Arc::new(Mutex::new(String::new()));
        tokio::spawn(drain_logs(spawned.stdout, logs.clone()));
        let done = spawned.done;

        // What the handshake was waiting for when it was cut off. Two very
        // different failures share this timeout — a server that never came up
        // and a catalogue that never settled — and the second is invisible
        // without this.
        let stage = Arc::new(Mutex::new(String::from("waiting for the server to answer")));
        let started = match tokio::time::timeout(
            START_TIMEOUT,
            handshake(&client, &self.pinned, &spec, &stage),
        )
        .await
        {
            Ok(Ok(started)) => started,
            Ok(Err(e)) => {
                let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
                return Err(e);
            }
            Err(_) => {
                let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
                return Err(AdapterError::Protocol(format!(
                    "OpenCode harness startup timed out after {}s while {}; it last said: {}",
                    START_TIMEOUT.as_secs(),
                    stage.lock().unwrap(),
                    last_log(&logs)
                )));
            }
        };

        // The node's cursor learns which upstream session this is, and gets
        // the client to reconcile against, before anything is streamed.
        if let Some(cursor) = spec.cursor.clone() {
            cursor
                .bind(&started.session_id, Arc::new(client.clone()))
                .await;
        }

        let (tx, rx) = mpsc::channel(256);
        let turn = Arc::new(Mutex::new(None::<oneshot::Sender<TurnResult>>));
        let pump = Pump {
            client: client.clone(),
            session_id: started.session_id.clone(),
            cursor: spec.cursor.clone(),
            turn: turn.clone(),
            // The context window as the harness itself reports this node's
            // declaration, so a turn can say how much of it was used.
            context_size: context_window(&client, &spec.model).await,
            open: Arc::new(Mutex::new(HashMap::new())),
        };
        tokio::spawn(pump.clone().durable_stream(tx.clone()));
        tokio::spawn(pump.permission_stream(tx.clone()));
        tokio::spawn(async move {
            let code = done.await.ok();
            let _ = tx.send(HarnessEvent::Exited { code }).await;
        });

        let handle = OpenCodeHandle {
            client,
            session_id: started.session_id,
            turn,
            compat: HarnessCompat {
                agent: Self::ID.into(),
                version: started.version,
                protocol: Self::PROTOCOL.tag(Self::PROTOCOL.min),
            },
        };
        Ok((Box::new(handle), rx))
    }
}

async fn drain_logs(
    stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    logs: Arc<Mutex<String>>,
) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut sink = logs.lock().unwrap();
        sink.push_str(&line);
        sink.push('\n');
        if sink.len() > 64 * 1024 {
            let cut = sink.len() - 32 * 1024;
            sink.drain(..cut);
        }
    }
}

fn last_log(logs: &Arc<Mutex<String>>) -> String {
    logs.lock()
        .unwrap()
        .lines()
        .next_back()
        .unwrap_or("nothing")
        .to_string()
}

struct Started {
    session_id: String,
    version: String,
}

/// Wait for the server, check the pin against what it reports, then create the
/// session in this session's workspace. The directory is pinned here and comes
/// from nowhere else: it is what OpenCode treats as authorization to open a
/// path as a project.
async fn handshake(
    client: &Client,
    pinned: &str,
    spec: &LaunchSpec,
    stage: &Arc<Mutex<String>>,
) -> Result<Started, AdapterError> {
    loop {
        match client.get_json("/global/health").await {
            Ok(health) => {
                let found = health["version"].as_str().unwrap_or("unknown");
                if found != pinned {
                    return Err(AdapterError::VersionMismatch {
                        found: found.to_string(),
                        pinned: pinned.to_string(),
                    });
                }
                let (provider, model) = split_model(&spec.model)?;
                // The catalogue is not ready when health is. See
                // `catalogue_settled`: a session created before it settles
                // accepts a prompt and then never runs it.
                catalogue_settled(client, &provider, &model, stage).await?;
                let created = client
                    .post_json(
                        "/api/session",
                        json!({
                            "model": { "providerID": provider, "id": model },
                            "location": { "directory": spec.cwd_in_runner },
                        }),
                    )
                    .await?;
                let session_id = created["data"]["id"]
                    .as_str()
                    .filter(|id| id.starts_with("ses_"))
                    .ok_or_else(|| {
                        AdapterError::Protocol(format!("the harness created no session: {created}"))
                    })?
                    .to_string();
                // The session must be pinned to the workspace the node gave
                // it. Anything else means a request opened another path.
                let directory = created["data"]["location"]["directory"]
                    .as_str()
                    .unwrap_or_default();
                if !same_directory(directory, &spec.cwd_in_runner) {
                    return Err(AdapterError::Protocol(format!(
                        "the harness opened {directory:?} rather than the workspace \
                         {:?}",
                        spec.cwd_in_runner
                    )));
                }
                return Ok(Started {
                    session_id,
                    version: found.to_string(),
                });
            }
            Err(AdapterError::VersionMismatch { found, pinned }) => {
                return Err(AdapterError::VersionMismatch { found, pinned })
            }
            // A server that answered at all and refused the credential is not
            // a server that is still starting: retrying would only wait out
            // the timeout on a launch that can never succeed.
            Err(e) if e.to_string().contains("401") => {
                return Err(AdapterError::Protocol(format!(
                    "the harness refused this node's credential: {e}"
                )))
            }
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// How often the catalogue is re-asked while it settles.
const CATALOGUE_POLL: Duration = Duration::from_millis(100);

/// Wait until the model this session will run is actually in the harness's
/// catalogue.
///
/// `serve` answers `/global/health` before its provider catalogue has loaded,
/// and the two are not ordered. A prompt admitted in that window is accepted
/// and then never run: resolving the model fails inside the drain, nothing
/// retries it, and the session sits waiting on a turn that will not happen —
/// with no error anywhere, which is the part that makes it expensive. It was
/// observed intermittently against the pinned binary while the provider
/// proofs were being written, where the test harness had to wait for the
/// catalogue itself because the node's handshake did not.
///
/// So the handshake waits here instead, before the session exists. The caller
/// runs the whole handshake under `START_TIMEOUT`; a catalogue that never
/// settles therefore fails the launch visibly rather than producing a session
/// whose first prompt disappears. `/config/providers` is the route the
/// adapter already reads a context window from, so this asks nothing new of
/// the server.
async fn catalogue_settled(
    client: &Client,
    provider: &str,
    model: &str,
    stage: &Arc<Mutex<String>>,
) -> Result<(), AdapterError> {
    let mut last;
    loop {
        match client.get_json(&client.scoped("/config/providers")).await {
            Ok(listed) => {
                let entries = listed["providers"].as_array().cloned().unwrap_or_default();
                if entries
                    .iter()
                    .filter(|entry| entry["id"].as_str() == Some(provider))
                    .any(|entry| entry["models"].get(model).is_some())
                {
                    return Ok(());
                }
                let offered: Vec<String> = entries
                    .iter()
                    .filter_map(|entry| entry["id"].as_str().map(str::to_string))
                    .collect();
                last = if offered.is_empty() {
                    "it listed no providers at all".to_string()
                } else {
                    format!("it listed {}", offered.join(", "))
                };
            }
            Err(e) => last = e.to_string(),
        }
        *stage.lock().unwrap() =
            format!("waiting for {provider}/{model} to appear in the catalogue ({last})");
        // The caller's timeout is the bound. Failing here on a slow catalogue
        // would trade a silent hang for a flaky launch; the stage note above
        // is what makes that timeout say which of the two it was.
        tokio::time::sleep(CATALOGUE_POLL).await;
    }
}

/// `/var/home/x` and `/home/x` are the same directory on an ostree host, and
/// OpenCode answers with whichever one it resolved. Compare on the tail rather
/// than refusing a launch over a symlink.
fn same_directory(reported: &str, asked: &str) -> bool {
    reported == asked || reported.ends_with(asked) || asked.ends_with(reported)
}

fn split_model(model: &str) -> Result<(String, String), AdapterError> {
    model
        .split_once('/')
        .map(|(provider, id)| (provider.to_string(), id.to_string()))
        .ok_or_else(|| AdapterError::UnknownModel(model.to_string()))
}

/// The context window the harness reports for the session's model, which is
/// this node's own declaration read back from the harness that loaded it. A
/// harness that reports none leaves the session showing a token count without
/// a denominator, which is the honest answer when a model omits it.
async fn context_window(client: &Client, model: &str) -> Option<u64> {
    let (provider, id) = model.split_once('/')?;
    let providers = client
        .get_json(&client.scoped("/config/providers"))
        .await
        .ok()?;
    providers["providers"]
        .as_array()?
        .iter()
        .find(|entry| entry["id"].as_str() == Some(provider))?["models"][id]["limit"]["context"]
        .as_u64()
        .filter(|context| *context > 0)
}

/// Reads the harness's two streams and turns them into the events the
/// supervisor already knows how to persist.
#[derive(Clone)]
struct Pump {
    client: Client,
    session_id: String,
    /// The node's own record of what has been ingested. It owns the sequence
    /// the stream resumes from, so a restart resumes from the store rather
    /// than from zero, and it is what decides whether an event that arrives
    /// twice is translated twice.
    cursor: Option<Arc<dyn crate::adapter::DurableCursor>>,
    turn: Arc<Mutex<Option<oneshot::Sender<TurnResult>>>>,
    /// The context window this node declared for the session's model, so a
    /// turn can report how much of it was used rather than only a token count.
    context_size: Option<u64>,
    /// Permission requests still waiting on the operator, by OpenCode's own
    /// request id.
    open: Arc<Mutex<HashMap<String, ()>>>,
}

impl Pump {
    /// The durable per-session stream, which is the only one with replay: it
    /// is anchored on the aggregate sequence and reconnects from the last one
    /// seen, so a dropped connection loses nothing (finding 6).
    async fn durable_stream(self, tx: mpsc::Sender<HarnessEvent>) {
        let mut after: u64 = match &self.cursor {
            Some(cursor) => cursor.resume_from().await,
            None => 0,
        };
        let mut turn = TurnState::default();
        loop {
            // The node's record wins over this loop's memory: reconciliation
            // may have ingested from a snapshot while the stream was down, and
            // asking again for what it already recorded would double it.
            if let Some(cursor) = &self.cursor {
                after = after.max(cursor.resume_from().await);
            }
            let path = format!("/api/session/{}/event?after={after}", self.session_id);
            let response = self
                .client
                .request(reqwest::Method::GET, &path)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    let mut frames = sse(response);
                    while let Some(event) = frames.next().await {
                        if let Some(seq) = event["durable"]["seq"].as_u64() {
                            after = after.max(seq);
                        }
                        // An event the node has already ingested is dropped
                        // here rather than translated again: the resumed
                        // stream and a reconciled snapshot overlap by design.
                        // The tap the native UI's live channel is built from
                        // sees it first either way — a replay is a duplicate
                        // for the record and still news to a browser that just
                        // reconnected (finding 20).
                        if let Some(cursor) = &self.cursor {
                            cursor.observe(&event);
                            if !cursor.admit(&event).await {
                                continue;
                            }
                        }
                        if !self.on_durable(&event, &tx, &mut turn).await {
                            return;
                        }
                    }
                }
                Ok(response) => {
                    let status = response.status();
                    let _ = tx
                        .send(HarnessEvent::Other(json!({
                            "method": "opencode.stream.refused",
                            "params": { "status": status.as_u16() },
                        })))
                        .await;
                    if status == reqwest::StatusCode::UNAUTHORIZED
                        || status == reqwest::StatusCode::NOT_FOUND
                    {
                        return;
                    }
                }
                Err(_) => {}
            }
            if tx.is_closed() {
                return;
            }
            // Whatever the stream missed comes back when it resumes; what it
            // cannot carry — a permission raised while it was down, a session
            // that is gone — is the node's to reconcile.
            if let Some(cursor) = &self.cursor {
                cursor.reconnected().await;
            }
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }

    /// One durable event. Returns false when the pump should stop.
    async fn on_durable(
        &self,
        event: &Value,
        tx: &mpsc::Sender<HarnessEvent>,
        turn: &mut TurnState,
    ) -> bool {
        let kind = event["type"].as_str().unwrap_or_default();
        let data = &event["data"];
        let message_id = data["assistantMessageID"].as_str().map(str::to_string);
        let send = |event: HarnessEvent| tx.send(event);
        match kind {
            "session.next.text.ended" => {
                let text = data["text"].as_str().unwrap_or_default().to_string();
                if !text.is_empty()
                    && send(HarnessEvent::MessageChunk { message_id, text })
                        .await
                        .is_err()
                {
                    return false;
                }
            }
            "session.next.reasoning.ended" => {
                let text = data["text"].as_str().unwrap_or_default().to_string();
                if !text.is_empty()
                    && send(HarnessEvent::ThoughtChunk { message_id, text })
                        .await
                        .is_err()
                {
                    return false;
                }
            }
            "session.next.tool.called" => {
                let tool = data["tool"].as_str().unwrap_or("a tool").to_string();
                if send(HarnessEvent::ToolCall(ToolCall {
                    tool_call_id: data["callID"].as_str().unwrap_or_default().to_string(),
                    title: tool,
                    kind: Some("other".into()),
                    status: Some("in_progress".into()),
                    raw_input: Some(data["input"].clone()),
                    content: Vec::new(),
                    locations: Vec::new(),
                }))
                .await
                .is_err()
                {
                    return false;
                }
            }
            "session.next.tool.success" | "session.next.tool.failed" => {
                let failed = kind.ends_with("failed");
                if send(HarnessEvent::ToolCallUpdate(ToolCallUpdate {
                    tool_call_id: data["callID"].as_str().unwrap_or_default().to_string(),
                    status: Some(if failed {
                        "failed".into()
                    } else {
                        "completed".into()
                    }),
                    kind: None,
                    title: None,
                    content: vec![if failed {
                        data["error"].clone()
                    } else {
                        data["content"].clone()
                    }],
                    raw_output: Some(if failed {
                        data["error"].clone()
                    } else {
                        data["structured"].clone()
                    }),
                }))
                .await
                .is_err()
                {
                    return false;
                }
            }
            "session.next.step.ended" => {
                let usage = usage_of(&data["tokens"]);
                turn.add(&usage);
                if send(HarnessEvent::Usage {
                    size: self.context_size,
                    used: Some(usage.charged()),
                    // v2 publishes a hardcoded zero cost (providers.md §6.2),
                    // so nothing is claimed here: the gateway's own count is
                    // what a budget is spent against.
                    cost_usd: data["cost"].as_f64().filter(|cost| *cost > 0.0),
                })
                .await
                .is_err()
                {
                    return false;
                }
                let finish = data["finish"].as_str().unwrap_or_default();
                if finish != "tool-calls" {
                    self.finish_turn(turn.take(stop_reason(finish)));
                }
            }
            "session.next.step.failed" => {
                if send(HarnessEvent::Other(json!({
                    "method": kind,
                    "params": data.clone(),
                    "provider": data["error"]["type"],
                    "message": data["error"]["message"],
                })))
                .await
                .is_err()
                {
                    return false;
                }
                self.finish_turn(turn.take("refusal"));
            }
            // The harness's own "the provider refused, I am retrying" notice,
            // shaped so the supervisor's recogniser can be taught this
            // spelling without reaching back into the adapter.
            "session.next.retried" => {
                let notice = HarnessEvent::Other(json!({
                    "method": kind,
                    "params": data.clone(),
                    "attempt": data["attempt"],
                    "message": data["error"]["message"],
                }));
                if send(notice).await.is_err() {
                    return false;
                }
            }
            _ => {}
        }
        true
    }

    fn finish_turn(&self, result: TurnResult) {
        if let Some(done) = self.turn.lock().unwrap().take() {
            let _ = done.send(result);
        }
    }

    /// Permission and question asks. They are not on the durable stream —
    /// only the server-wide one carries them — so this filters that stream to
    /// this session and answers from the snapshot after every reconnect, so a
    /// request raised while the stream was down is still presented.
    async fn permission_stream(self, tx: mpsc::Sender<HarnessEvent>) {
        loop {
            self.sweep_pending(&tx).await;
            let response = self
                .client
                .request(reqwest::Method::GET, "/api/event")
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .send()
                .await;
            if let Ok(response) = response {
                if response.status().is_success() {
                    let mut frames = sse(response);
                    while let Some(event) = frames.next().await {
                        // This stream, not the durable one, is where the asks
                        // and the v1 session events the native app renders
                        // live. It is the node's one reader of it, so the
                        // UI's synthesised channel is fed from here rather
                        // than from a second connection of its own.
                        if let Some(cursor) = &self.cursor {
                            cursor.observe(&event);
                        }
                        if !self.on_ask(&event, &tx).await {
                            return;
                        }
                    }
                }
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }

    /// Every permission this session has pending, for the gap a dropped stream
    /// leaves: the server-wide stream has no replay at all (finding 6).
    async fn sweep_pending(&self, tx: &mpsc::Sender<HarnessEvent>) {
        let path = format!("/api/session/{}/permission", self.session_id);
        let Ok(pending) = self.client.get_json(&path).await else {
            return;
        };
        for request in pending["data"].as_array().into_iter().flatten() {
            self.ask(request, tx).await;
        }
    }

    async fn on_ask(&self, event: &Value, tx: &mpsc::Sender<HarnessEvent>) -> bool {
        if tx.is_closed() {
            return false;
        }
        let kind = event["type"].as_str().unwrap_or_default();
        let data = &event["data"];
        if data["sessionID"].as_str() != Some(self.session_id.as_str()) {
            return true;
        }
        match kind {
            "permission.v2.asked" | "permission.asked" => self.ask(data, tx).await,
            // A question is an ask with words rather than a tool, and the
            // reply route is shaped the same. Presenting it as a permission
            // keeps one queue rather than two.
            "question.v2.asked" | "question.asked" => self.ask_question(data, tx).await,
            _ => {}
        }
        true
    }

    async fn ask(&self, request: &Value, tx: &mpsc::Sender<HarnessEvent>) {
        let Some(id) = request["id"].as_str().map(str::to_string) else {
            return;
        };
        if self.open.lock().unwrap().insert(id.clone(), ()).is_some() {
            return;
        }
        let action = request["action"].as_str().unwrap_or("unknown").to_string();
        let resources: Vec<String> = request["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(str::to_string))
            .collect();
        let target = (!resources.is_empty()).then(|| resources.join("\n"));
        let (resource, command) = if action.eq_ignore_ascii_case("bash") {
            (None, target)
        } else {
            (target, None)
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let sent = tx
            .send(HarnessEvent::Permission {
                request: PermissionRequest::managed(
                    // OpenCode's call id is the provider's own string: it is
                    // mapped, never adopted as a tracon identity.
                    request["source"]["callID"].as_str().map(str::to_string),
                    summarize(&action, &resources),
                    &action,
                    resource,
                    command,
                    Some(json!({
                        "action": action,
                        "resources": resources,
                        "metadata": request["metadata"].clone(),
                    })),
                    options(),
                ),
                reply: reply_tx,
            })
            .await;
        if sent.is_err() {
            self.open.lock().unwrap().remove(&id);
            return;
        }
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        let open = self.open.clone();
        tokio::spawn(async move {
            let decision = reply_rx.await.unwrap_or(PermissionReply::Cancelled);
            open.lock().unwrap().remove(&id);
            let path = format!("/api/session/{session_id}/permission/{id}/reply");
            if let Err(e) = client.post_json(&path, reply_body(decision)).await {
                tracing::warn!(error = %e, "answering an OpenCode permission failed");
            }
        });
    }

    /// A question with its own choices. The reply route differs, so this one
    /// is answered on the question path rather than the permission path.
    async fn ask_question(&self, request: &Value, tx: &mpsc::Sender<HarnessEvent>) {
        let Some(id) = request["id"].as_str().map(str::to_string) else {
            return;
        };
        let text = request["question"]
            .as_str()
            .or_else(|| request["text"].as_str())
            .unwrap_or("the harness asked a question")
            .to_string();
        let (reply_tx, reply_rx) = oneshot::channel();
        let sent = tx
            .send(HarnessEvent::Permission {
                request: PermissionRequest {
                    tool_call_id: None,
                    title: summarize("question", std::slice::from_ref(&text)),
                    action: "question".into(),
                    kind: Some("question".into()),
                    resource: Some(text),
                    command: None,
                    raw_input: Some(request.clone()),
                    options: options(),
                },
                reply: reply_tx,
            })
            .await;
        if sent.is_err() {
            return;
        }
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            let decision = reply_rx.await.unwrap_or(PermissionReply::Cancelled);
            let allowed = matches!(&decision, PermissionReply::Selected(option)
                if option == types::OPTION_ALLOW_ONCE);
            let path = if allowed {
                format!("/api/session/{session_id}/question/{id}/reply")
            } else {
                format!("/api/session/{session_id}/question/{id}/reject")
            };
            if let Err(e) = client.post_json(&path, json!({})).await {
                tracing::warn!(error = %e, "answering an OpenCode question failed");
            }
        });
    }
}

/// What the turn has accumulated so far. OpenCode reports per-step tokens; a
/// turn is every step it took, so they are summed rather than last-step-wins,
/// which would undercount a turn that ran tools.
#[derive(Default)]
struct TurnState {
    usage: Usage,
}

impl TurnState {
    fn add(&mut self, usage: &Usage) {
        self.usage.input_tokens += usage.input_tokens;
        self.usage.output_tokens += usage.output_tokens;
        self.usage.total_tokens += usage.total_tokens;
        self.usage.cached_read_tokens += usage.cached_read_tokens;
    }

    fn take(&mut self, stop_reason: &str) -> TurnResult {
        let usage = std::mem::take(&mut self.usage);
        TurnResult {
            stop_reason: stop_reason.to_string(),
            usage,
        }
    }
}

/// The v1 usage fields (`providers.md` §6.1), which are non-overlapping: the
/// prompt is `input + cache.read + cache.write` and the output is
/// `output + reasoning`.
fn usage_of(tokens: &Value) -> Usage {
    let number = |v: &Value| v.as_f64().unwrap_or(0.0).max(0.0) as u64;
    let input = number(&tokens["input"]);
    let output = number(&tokens["output"]);
    let reasoning = number(&tokens["reasoning"]);
    let cache_read = number(&tokens["cache"]["read"]);
    let cache_write = number(&tokens["cache"]["write"]);
    Usage {
        input_tokens: input,
        output_tokens: output + reasoning,
        total_tokens: input + output + reasoning + cache_read + cache_write,
        cached_read_tokens: cache_read,
    }
}

/// The AI SDK's finish reason, as a tracon stop reason.
fn stop_reason(finish: &str) -> &'static str {
    match finish {
        "stop" | "" => "end_turn",
        "length" => "max_tokens",
        "content-filter" => "refusal",
        "error" => "refusal",
        _ => "end_turn",
    }
}

/// Server-sent events as JSON values. OpenCode writes `data:` lines and no
/// `id:` at all — the sequence to reconnect from is inside the payload, not in
/// the SSE frame.
fn sse(response: reqwest::Response) -> impl futures_util::Stream<Item = Value> + Unpin {
    let mut buffer = Vec::new();
    let mut pending = String::new();
    Box::pin(response.bytes_stream().flat_map(move |chunk| {
        let mut out = Vec::new();
        if let Ok(chunk) = chunk {
            buffer.extend_from_slice(&chunk);
            while let Some(at) = buffer.iter().position(|b| *b == b'\n') {
                let line = buffer.drain(..=at).collect::<Vec<_>>();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\r', '\n']);
                if let Some(data) = line.strip_prefix("data:") {
                    pending.push_str(data.trim_start());
                } else if line.is_empty() && !pending.is_empty() {
                    if let Ok(value) = serde_json::from_str::<Value>(&pending) {
                        out.push(value);
                    }
                    pending.clear();
                }
            }
        }
        futures_util::stream::iter(out)
    }))
}

pub struct OpenCodeHandle {
    client: Client,
    session_id: String,
    turn: Arc<Mutex<Option<oneshot::Sender<TurnResult>>>>,
    compat: HarnessCompat,
}

#[async_trait]
impl HarnessHandle for OpenCodeHandle {
    fn harness_session_id(&self) -> &str {
        &self.session_id
    }

    /// Where the policy-aware API gateway reaches this session's server, and
    /// with what. Nothing here is ever handed to a client: the gateway resolves
    /// it per request and injects the credential itself, so the operator's
    /// browser holds a tracon cookie and never the server password (finding 4).
    fn native_api(&self) -> Option<crate::adapter::NativeApi> {
        Some(crate::adapter::NativeApi {
            base: self.client.base.clone(),
            authorization: self.client.authorization.clone(),
            directory: self.client.directory.clone(),
            session_id: self.session_id.clone(),
        })
    }

    fn compat(&self) -> HarnessCompat {
        self.compat.clone()
    }

    async fn prompt(&self, text: String) -> Result<TurnResult, AdapterError> {
        let (tx, rx) = oneshot::channel();
        *self.turn.lock().unwrap() = Some(tx);
        let path = format!("/api/session/{}/prompt", self.session_id);
        // The model is the session's, bound when it was created; the prompt
        // route carries no model of its own.
        if let Err(e) = self
            .client
            .post_json(&path, json!({ "prompt": { "text": text } }))
            .await
        {
            self.turn.lock().unwrap().take();
            return Err(e);
        }
        rx.await
            .map_err(|_| AdapterError::Protocol("the harness ended mid-turn".into()))
    }

    async fn cancel(&self) -> Result<(), AdapterError> {
        let path = self
            .client
            .scoped(&format!("/session/{}/abort", self.session_id));
        self.client.post_json(&path, json!({})).await.map(|_| ())
    }

    async fn close(&self) -> Result<(), AdapterError> {
        // Release the instance rather than deleting the session: the
        // transcript OpenCode holds is what a direct-harness recovery reads.
        let path = self.client.scoped("/instance/dispose");
        let disposed = self.client.post_json(&path, json!({})).await;
        if let Err(e) = &disposed {
            tracing::debug!(error = %e, "disposing the OpenCode instance failed");
        }
        // `serve` installs no signal handlers, so nothing it started would be
        // reaped by a polite stop. The supervisor removes the container by
        // name immediately after this returns, which is what actually ends
        // the server and anything it spawned (`config-state.md` §9 row 5c).
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelDecl, Provider};

    fn wiring() -> Wiring {
        let mut cfg = crate::config::Config::default();
        cfg.providers.insert(
            "anthropic".into(),
            Provider {
                credential: "anthropic".into(),
                upstream: "https://api.anthropic.com".into(),
                shape: crate::config::SHAPE_ANTHROPIC.into(),
                models: vec![ModelDecl {
                    id: "claude-opus-5".into(),
                    name: "Opus 5".into(),
                    context: 200_000,
                    output: 64_000,
                    reasoning: true,
                    attachment: true,
                }],
                ..Default::default()
            },
        );
        cfg.providers.insert(
            "openai-codex".into(),
            Provider {
                credential: "openai-codex".into(),
                upstream: "https://chatgpt.com/backend-api".into(),
                shape: crate::config::SHAPE_OPENAI_CODEX.into(),
                models: vec![ModelDecl {
                    id: "gpt-5.5".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        cfg.providers.remove("openai");
        crate::gateway::model::harness_wiring(&cfg, "tracon-gw", "session-token", |_, _| true)
    }

    fn config() -> Value {
        let files = OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION).scratch_files(&wiring());
        let (_, body) = files
            .iter()
            .find(|(path, _)| path == OpenCodeAdapter::CONFIG_FILE)
            .expect("the config is written");
        serde_json::from_str(body).expect("the config is JSON")
    }

    /// OpenCode reads no base-URL environment variable, so the gateway's URL
    /// has to be in the file or the harness talks to the provider directly.
    #[test]
    fn every_provider_is_wired_to_the_gateway_in_the_config() {
        let config = config();
        assert_eq!(
            config["provider"]["anthropic"]["options"]["baseURL"],
            "http://tracon-gw:7421/model/anthropic/v1"
        );
        assert_eq!(
            config["provider"]["anthropic"]["options"]["apiKey"],
            "session-token"
        );
        assert_eq!(config["provider"]["anthropic"]["npm"], "@ai-sdk/anthropic");
        assert_eq!(
            config["provider"]["openai-codex"]["options"]["baseURL"],
            "http://tracon-gw:7421/model/openai-codex/v1"
        );
        // The Codex provider is never named `openai`: that name plus an OAuth
        // record is what would rewrite the request past the gateway.
        assert!(config["provider"]["openai"].is_null());
        assert_eq!(
            config["provider"]["anthropic"]["models"]["claude-opus-5"]["limit"]["context"],
            200_000
        );
    }

    /// An action OpenCode's own ruleset allows produces no event at all, so
    /// anything other than `ask` is an action the node never sees.
    #[test]
    fn every_tool_class_asks() {
        let config = config();
        let rules = config["permission"]
            .as_object()
            .expect("a permission ruleset");
        assert_eq!(rules["*"], "ask");
        for tool in ["bash", "edit", "read", "webfetch", "task", "skill"] {
            assert_eq!(rules[tool], "ask", "{tool} does not ask");
        }
        assert!(
            rules.values().all(|rule| rule == "ask"),
            "a rule other than ask: {rules:?}"
        );
    }

    #[test]
    fn the_node_is_the_only_mcp_server_and_it_never_starts_an_oauth_flow() {
        let servers = vec![json!({
            "type": "http",
            "name": "tracon",
            "url": "http://tracon-gw:7421/mcp/s1",
            "headers": [{ "name": "Authorization", "value": "Bearer tok" }],
        })];
        let config = config_document(&wiring(), &servers);
        let entry = &config["mcp"]["tracon"];
        assert_eq!(entry["type"], "remote");
        assert_eq!(entry["url"], "http://tracon-gw:7421/mcp/s1");
        assert_eq!(entry["headers"]["Authorization"], "Bearer tok");
        assert_eq!(entry["oauth"], false);
        assert!(entry["timeout"].is_number());
        assert_eq!(config["experimental"]["mcp_timeout"], MCP_TIMEOUT_MS);
        assert_eq!(config["share"], "disabled");
    }

    /// An `oauth` record is what arms every bundled subscription plugin, and a
    /// `wellknown` record is what fetches a remote config and runs a command
    /// to mint a credential. The store this node writes has neither.
    #[test]
    fn the_auth_store_holds_no_oauth_and_no_wellknown_record() {
        let files = OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION).scratch_files(&wiring());
        let (_, store) = files
            .iter()
            .find(|(path, _)| path == OpenCodeAdapter::AUTH_FILE)
            .expect("an auth store is written");
        let parsed: Value = serde_json::from_str(store).expect("the store is JSON");
        assert_eq!(parsed, json!({}));
        for file in &files {
            assert!(!file.1.contains("\"oauth\": true"), "{}", file.0);
            assert!(!file.1.contains("wellknown"), "{}", file.0);
        }
        let env = OpenCodeAdapter::launch_env("/root", "pw");
        let inline = env
            .iter()
            .find(|(k, _)| k == "OPENCODE_AUTH_CONTENT")
            .map(|(_, v)| v.clone())
            .expect("the store is supplied inline too");
        assert_eq!(
            serde_json::from_str::<Value>(&inline).expect("valid JSON, or it fails open"),
            json!({})
        );
    }

    /// The whole of §9.2. A missing entry is an ambient discovery path left
    /// open, so absence is the failure this test exists to catch.
    #[test]
    fn the_launch_environment_seals_the_harness() {
        let env: std::collections::HashMap<String, String> =
            OpenCodeAdapter::launch_env("/root", "secret-password")
                .into_iter()
                .collect();
        assert_eq!(env["HOME"], "/root/.opencode/run/home");
        assert_eq!(env["XDG_CONFIG_HOME"], "/root/.opencode/run/config");
        assert_eq!(env["XDG_DATA_HOME"], "/root/.opencode/run/data");
        assert_eq!(env["XDG_CACHE_HOME"], "/root/.opencode/run/cache");
        assert_eq!(env["XDG_STATE_HOME"], "/root/.opencode/run/state");
        assert_eq!(env["OPENCODE_DB"], "/root/.opencode/run/state/opencode.db");
        assert_eq!(env["OPENCODE_CONFIG"], "/root/.opencode/opencode.json");
        assert_eq!(env["OPENCODE_MODELS_PATH"], "/root/.opencode/models.json");
        for flag in [
            "OPENCODE_DISABLE_PROJECT_CONFIG",
            "OPENCODE_DISABLE_DEFAULT_PLUGINS",
            "OPENCODE_DISABLE_EXTERNAL_SKILLS",
            "OPENCODE_DISABLE_CLAUDE_CODE",
            "OPENCODE_DISABLE_LSP_DOWNLOAD",
            "OPENCODE_DISABLE_MODELS_FETCH",
            "OPENCODE_DISABLE_AUTOUPDATE",
            "OPENCODE_DISABLE_SHARE",
        ] {
            assert_eq!(env[flag], "true", "{flag} is not set");
        }
        assert_eq!(env["OPENCODE_PURE"], "1");
        assert_eq!(env["OPENCODE_SERVER_PASSWORD"], "secret-password");
        assert_eq!(env["OPENCODE_SERVER_USERNAME"], "opencode");
        // Per-session state is never the volume every session shares.
        assert!(!env["HOME"].ends_with("/.opencode"));
        // The launch manifest's skill root: absolute, beside the read-only
        // config rather than inside the worktree, and outside every directory
        // OpenCode discovers on its own. This is the value `{env:…}` in the
        // config resolves to — a relative `skills.paths` entry would be
        // resolved against the worktree, which is the one place a session can
        // write.
        assert_eq!(
            env[crate::manifest::SKILL_ROOT_ENV],
            "/root/.opencode/skills"
        );
        assert!(!env[crate::manifest::SKILL_ROOT_ENV].starts_with(&env["HOME"]));
    }

    /// The manifest renders into the keys OpenCode reads, and the ones it
    /// leaves out are as load-bearing as the ones it writes: a `skills.urls`
    /// entry would be fetched over plain HTTP before any interaction, with
    /// nothing that disables it.
    #[test]
    fn the_manifest_renders_skills_and_only_plugins_the_image_baked() {
        let files = vec![crate::manifest::ManifestFile {
            path: "SKILL.md".into(),
            text: "---\nname: notes\ndescription: d\n---\n\nbody\n".into(),
        }];
        let baked = OpenCodeAdapter::baked_plugins();
        let manifest = crate::manifest::build(crate::manifest::Inputs {
            channel: "work",
            skills: vec![crate::manifest::SkillEntry {
                name: "notes".into(),
                description: "d".into(),
                source: "dir:/srv/notes".into(),
                digest: crate::manifest::skill::digest_of(&files),
                warnings: Vec::new(),
                files,
            }],
            instructions: Vec::new(),
            agents: Vec::new(),
            plugins: &baked,
            baked: &baked,
            lsp: crate::manifest::toolchain_lsp(OpenCodeAdapter::ID),
            formatters: crate::manifest::toolchain_formatters(OpenCodeAdapter::ID),
            providers: vec!["anthropic".into()],
            policy_revision: "1".into(),
        })
        .expect("the image's own plugin builds");

        let mut wired = wiring();
        wired.manifest = manifest;
        let document = config_document(&wired, &[]);
        assert_eq!(
            document["skills"]["paths"],
            json!(["{env:TRACON_SKILL_ROOT}"])
        );
        assert!(document["skills"]["urls"].is_null(), "{document}");
        assert_eq!(document["plugin"], json!(baked));
        // `lsp` and `formatter` are the toolchain profile's, merged in by
        // `scratch_files`; the manifest renders neither, so one fact has one
        // source.
        assert!(document["lsp"].is_null(), "{document}");
        assert!(document["formatter"].is_null(), "{document}");

        // And an empty manifest writes neither a skill root nor a plugin list.
        let bare = config_document(&wiring(), &[]);
        assert!(bare["skills"].is_null(), "{bare}");
        assert!(bare["plugin"].is_null(), "{bare}");

        // What a session actually gets does carry the toolchain, because
        // `scratch_files` merges the profile in on the way out.
        let mut wired = wiring();
        wired.manifest = crate::manifest::LaunchManifest::default();
        let staged = OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION).scratch_files(&wired);
        let written: Value = serde_json::from_str(
            &staged
                .iter()
                .find(|(name, _)| name == OpenCodeAdapter::CONFIG_FILE)
                .expect("a config file is written")
                .1,
        )
        .expect("the config is JSON");
        assert!(written["lsp"].is_object(), "{written}");
        assert!(written["formatter"].is_object(), "{written}");
    }

    /// The server is unauthenticated without a password, so a launch that
    /// forgot one would serve every route to anything that reached the port.
    #[test]
    fn the_server_is_launched_with_a_password_and_on_loopback() {
        let spec = LaunchSpec {
            cwd_in_runner: "/work".into(),
            model: "anthropic/claude-opus-5".into(),
            container_name: "tracon-h-1".into(),
            harness_home: "/root".into(),
            mcp_servers: Vec::new(),
            tools: Vec::new(),
            env: Vec::new(),
            system_prompt_file: None,
            cursor: None,
        };
        let cmd = OpenCodeAdapter::serve_cmd(&spec, 41234, "pw");
        assert_eq!(
            cmd.argv,
            [
                "opencode",
                "serve",
                "--pure",
                "--hostname",
                "127.0.0.1",
                "--port",
                "41234",
                "--print-logs",
                "--log-level",
                "INFO"
            ]
        );
        assert_eq!(cmd.expose, Some(41234));
        assert!(!cmd.argv.iter().any(|a| a == "--mdns"));
        let env: std::collections::HashMap<_, _> = cmd.env.into_iter().collect();
        assert_eq!(env["OPENCODE_SERVER_PASSWORD"], "pw");
    }

    #[test]
    fn a_permission_is_answered_once_and_never_always() {
        let allow = reply_body(PermissionReply::Selected(types::OPTION_ALLOW_ONCE.into()));
        assert_eq!(allow["reply"], "once");
        for decision in [
            PermissionReply::Selected(types::OPTION_REJECT_ONCE.into()),
            PermissionReply::Selected("allow_always".into()),
            PermissionReply::Cancelled,
        ] {
            let body = reply_body(decision);
            assert_eq!(body["reply"], "reject");
            assert_ne!(body["reply"], "always");
        }
    }

    #[test]
    fn declared_models_are_what_the_picker_offers() {
        let models = declared_models(&wiring());
        let values: Vec<_> = models.iter().map(|m| m.value.as_str()).collect();
        assert_eq!(values, ["anthropic/claude-opus-5", "openai-codex/gpt-5.5"]);
        assert_eq!(models[0].name, "Opus 5 (anthropic)");
        // A model declared without a window still gets one, or the harness
        // would show a token count against nothing.
        let catalogue = catalogue_document(&wiring());
        assert_eq!(
            catalogue["openai-codex"]["models"]["gpt-5.5"]["limit"]["context"],
            DEFAULT_CONTEXT
        );
    }

    /// The catalogue is a file this node writes, in the shape the pinned
    /// release reads it: nothing is fetched and nothing is subtracted.
    #[test]
    fn the_catalogue_is_the_declared_models_and_nothing_else() {
        let catalogue = catalogue_document(&wiring());
        assert_eq!(catalogue["anthropic"]["id"], "anthropic");
        assert!(catalogue["anthropic"]["env"].as_array().unwrap().is_empty());
        let models = catalogue["anthropic"]["models"].as_object().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models["claude-opus-5"]["limit"]["output"], 64_000);
        assert_eq!(models["claude-opus-5"]["status"], "active");
    }

    #[test]
    fn a_version_is_the_bare_string_the_cli_prints() {
        assert_eq!(parse_version("1.18.30\n"), "1.18.30");
        assert_eq!(parse_version(" 1.18.30 "), "1.18.30");
    }

    #[test]
    fn usage_sums_the_non_overlapping_fields() {
        let usage = usage_of(&json!({
            "input": 100, "output": 20, "reasoning": 5,
            "cache": { "read": 7, "write": 3 },
        }));
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 25);
        assert_eq!(usage.cached_read_tokens, 7);
        assert_eq!(usage.charged(), 135);
    }

    #[test]
    fn the_state_directory_is_this_harnesss_own() {
        let layout = OpenCodeAdapter::layout();
        assert_eq!(layout.dir, ".opencode");
        // No variable: HOME and XDG decide, and both are in the launch env.
        assert_eq!(layout.env, "");
        assert_eq!(
            OpenCodeAdapter::new("1.18.30").scratch_dirs(),
            ["run".to_string()]
        );
    }
}
