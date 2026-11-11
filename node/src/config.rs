//! Node configuration: `~/.config/tracon/node.toml`, overridden by flags.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub mod edit;
pub mod last_good;
pub mod live;
pub mod path_guard;
pub mod upstreams;
pub use edit::{edit, snapshot, update, EditError, EditOutcome, KeyPath, Op, Snapshot};
pub use live::{ConfigHandle, Live, PendingRestart};
pub use upstreams::{Attach, PreparationLending, Upstream, Upstreams};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub node_name: String,
    pub harness: Harness,
    pub boundary: Boundary,
    pub gateway: Gateway,
    pub session: SessionDefaults,
    pub consulta: Consulta,
    pub publish: Publish,
    /// What the GitHub tools may start on their own (`run_dispatch`).
    pub github: Github,
    pub mesh: Mesh,
    pub runtime: Runtime,
    /// Per-repository environment: image, checks, preparation and its egress.
    /// As the file had it when this was loaded; `repos()` is what a running
    /// node resolves against.
    pub repo: Vec<Repo>,
    /// The table as the operator last saved it through the node, when they
    /// have. Not part of the file: it is what lets an edit made in Settings
    /// or with `tracon repo set` apply to the next session and the next check
    /// without a restart, which would end every session on the node to change
    /// one repository's environment.
    #[serde(skip)]
    pub live_repo: LiveRepos,
    /// The newest file the running node has taken in, for the keys that
    /// apply without a restart (`Config::live`). Not part of the file.
    #[serde(skip)]
    pub live: ConfigHandle,
    /// Services a session may ask for by name (`service_start`): each a
    /// digest-pinned image run beside the session in its network namespace.
    pub service: Vec<Service>,
    /// MCP servers outside the node whose tools sessions reach through the
    /// node's own MCP endpoint, each tool decided allow, ask or deny.
    pub mcp_server: Vec<McpServer>,
    /// Model providers the gateway fronts, by name.
    pub providers: std::collections::BTreeMap<String, Provider>,
    /// The providers `node.toml` itself names, as opposed to the ones
    /// `default_providers` fills in. Not part of the file: it is what lets
    /// Connections leave out a built-in API-key provider nobody asked for,
    /// and lets `POST /api/providers` take that name over.
    #[serde(skip)]
    pub declared_providers: std::collections::BTreeSet<String>,
    pub memory: Memory,
    pub supervision: Supervision,
    pub review: ReviewLimits,
    pub notify: Notify,
    /// The node's public name, for clients that are not on this machine.
    pub auth: Auth,
    pub embed: Embed,
    pub external: External,
    pub docs: Docs,
    pub work: Work,
    /// Hosts the node attaches a stored credential to, on behalf of a client
    /// that never holds it.
    pub upstreams: Upstreams,
    /// Where sessions may run and what each channel and repository requires
    /// of the sandbox provider (`crate::provider`).
    pub placement: crate::provider::requirements::Placement,
    pub metrics: Metrics,
    /// What every sandbox is given: the harness bundle.
    pub sandbox: Sandbox,
    /// How this node reaches DigitalOcean's Harness Runtime, for sessions
    /// `[placement] digitalocean` sends there.
    pub digitalocean: crate::provider::digitalocean::Settings,
    /// What an older node's gateway and harness images were named, which an
    /// upgraded node removes. Not part of the file's current shape.
    #[serde(skip)]
    pub retired: RetiredNames,
}

/// `[sandbox]`: what a sandbox provider mounts into each sandbox.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sandbox {
    /// The harness bundle for this host's architecture: the directory
    /// holding `bin/tracon-sandbox` (one `<version>/<arch>` of
    /// `scripts/harness-bundle.sh`'s output). Mounted read-only at
    /// `/opt/tracon` in every sandbox, and relabelled for containers on an
    /// SELinux host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<PathBuf>,
    /// The stock image a sandbox runs when its repository names none: on
    /// Podman, Apple, and Kubernetes with `sandbox_protocol`. Pinned by
    /// digest once pulled.
    /// Unset, the release's own `tracon-sandbox-base` at this node's version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_image: Option<String>,
    /// Where the node fetches its harness bundle from, with no `bundle` set:
    /// `<release_url>/v<version>/tracon-bundle-<version>-<arch>.tar.gz` and
    /// its `.sha256`; and a `[[service]]` with `bundle = "release"`, its
    /// `tracon-browser-<chrome>-<arch>.tar.gz` beside them. Unset, this
    /// project's GitHub releases.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
    /// How long a run over the sandbox protocol whose connection dropped
    /// waits for its sandbox to redial before the run is lost; the sandbox
    /// keeps redialling for twice as long. 0 turns resuming off, so a
    /// dropped connection ends the run. Unset, 120.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_within_secs: Option<u64>,
}

/// Where releases are downloaded from, unless `[sandbox] release_url` says.
pub const RELEASE_URL: &str = "https://github.com/cosmicspork/tracon/releases/download";

impl Sandbox {
    /// `base_image`, or the release's own base at this node's version.
    pub fn base_image_ref(&self) -> String {
        match self.base_image.as_deref().map(str::trim) {
            Some(image) if !image.is_empty() => image.to_string(),
            _ => format!(
                "ghcr.io/cosmicspork/tracon-sandbox-base:{}",
                env!("CARGO_PKG_VERSION")
            ),
        }
    }

    /// `release_url` without a trailing slash, or [`RELEASE_URL`].
    pub fn release_url(&self) -> String {
        match self.release_url.as_deref().map(str::trim) {
            Some(url) if !url.is_empty() => url.trim_end_matches('/').to_string(),
            _ => RELEASE_URL.to_string(),
        }
    }

    /// `resume_within_secs`, or the protocol's default.
    pub fn resume_within(&self) -> std::time::Duration {
        self.resume_within_secs
            .map(std::time::Duration::from_secs)
            .unwrap_or(tracon_sandbox::node::DEFAULT_RESUME_WITHIN)
    }
}

/// Refuse a sandbox setting that cannot work rather than run sessions in
/// sandboxes that are not there.
pub fn validate_sandbox(config: &Config) -> Result<(), String> {
    let memory = &config.runtime.apple.memory;
    let digits = memory.trim_end_matches(['K', 'M', 'G', 'T']);
    if !memory.is_empty()
        && (digits.is_empty()
            || !digits.bytes().all(|b| b.is_ascii_digit())
            || memory.len() - digits.len() > 1)
    {
        return Err(format!(
            "runtime.apple.memory: {memory:?} is not a size such as 4G or 2048M"
        ));
    }
    if config.runtime.apple.cpus == Some(0) || config.runtime.apple.limit == Some(0) {
        return Err("runtime.apple.cpus and runtime.apple.limit must be at least 1".into());
    }
    if let Some(url) = config.sandbox.release_url.as_deref() {
        let parsed =
            url::Url::parse(url.trim()).map_err(|error| format!("sandbox.release_url: {error}"))?;
        if !matches!(parsed.scheme(), "https" | "http") {
            return Err("sandbox.release_url: must be an http(s) URL".into());
        }
    }
    Ok(())
}

/// The Prometheus scrape target (`GET /metrics`, `telemetry`): unset, nothing
/// listens. Unauthenticated and read-only, on a listener of its own, so it
/// belongs on loopback on a host, and on all interfaces only in a pod behind
/// a NetworkPolicy that admits the scraper alone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Metrics {
    pub listen: Option<SocketAddr>,
}

/// The operator listener's default port (`tracon serve --listen`), which
/// `[metrics] listen` may not take.
pub const OPERATOR_PORT: u16 = 7420;

/// Refuse a `[metrics] listen` port another of the node's listeners already
/// has: it would fail to bind at start, or, worse, take a port a harness can
/// reach.
pub fn validate_metrics(config: &Config) -> Result<(), String> {
    let Some(listen) = config.metrics.listen else {
        return Ok(());
    };
    let gateway = &config.gateway;
    let mut taken = vec![
        ("the operator listener", OPERATOR_PORT),
        ("gateway.forward_port", gateway.forward_port),
        ("gateway.proxy_port", gateway.proxy_port),
        ("gateway.qa_proxy_port", gateway.qa_proxy_port),
        ("gateway.egress_port", gateway.egress_port),
        ("docs.preview_listen", config.docs.preview_listen.port()),
    ];
    if let HarnessListen::Tcp(addr) = &gateway.harness_listen {
        taken.push(("gateway.harness_listen", addr.port()));
    }
    let kube = &config.runtime.kubernetes;
    if config.runtime.kind == RuntimeKind::Kubernetes && kube.sandbox_protocol {
        taken.push(("runtime.kubernetes.sandbox_port", kube.sandbox_port));
    }
    match taken.iter().find(|(_, port)| *port == listen.port()) {
        Some((what, port)) => Err(format!(
            "metrics.listen: port {port} is {what}'s; give the metrics listener a port of its own (7425 is free)"
        )),
        None => Ok(()),
    }
}

/// The tables an older `node.toml` may still carry for OpenCode, which is
/// gone: its native interface (`[ui]`) and its plugin approvals
/// (`[launch]`). Ignored either way, and worth one line in the log rather
/// than silence.
fn retired_opencode_sections(text: &str) -> Vec<&'static str> {
    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    ["ui", "launch"]
        .into_iter()
        .filter(|key| {
            value
                .get(*key)
                .and_then(toml::Value::as_table)
                .is_some_and(|t| !t.is_empty())
        })
        .collect()
}

/// What a `[qa]` table in an older `node.toml` still configures, if anything.
/// The feature is gone and the table is ignored, but a configured target is
/// worth one line in the log rather than silence.
fn retired_qa_section(text: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(text).ok()?;
    let qa = value.get("qa")?.as_table()?;
    let targets = qa
        .get("targets")
        .and_then(toml::Value::as_table)
        .map_or(0, |targets| targets.len());
    let prototype = qa.contains_key("prototype");
    (targets > 0 || prototype).then(|| {
        format!(
            "{targets} target(s){}",
            if prototype {
                " and a prototype recipe"
            } else {
                ""
            }
        )
    })
}

pub fn immutable_image(value: &str) -> Result<(), String> {
    let Some((name, digest)) = value.rsplit_once("@sha256:") else {
        return Err("must be pinned as image@sha256:<64 lowercase hex>".into());
    };
    if name.is_empty()
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("must be pinned as image@sha256:<64 lowercase hex>".into());
    }
    Ok(())
}

/// One entry of the `[[service]]` catalogue: something a session may start
/// beside itself by name, never by image or command. The operator chooses
/// the image and how it runs; the session chooses only whether it wants it,
/// and the policy bundle decides whether it may have it (`service_start` with
/// `name`), so a browser can run unattended while a database is asked.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Service {
    /// What a session asks for: `browser`, `postgres`.
    pub name: String,
    /// Pinned by digest, as a repository image is: what ran is what was named.
    /// Started as a container beside the session. May be left out when
    /// `bundle` is set and every session runs in a sandbox.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub image: String,
    /// The image's arguments, when its own entrypoint needs them; with
    /// `bundle`, the arguments after `run`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// A directory of binaries for this host's architecture (one
    /// `<version>/<arch>` of `scripts/browser-bundle.sh`'s output, say), or
    /// `release` on `browser`: the browser bundle of the node's own release,
    /// which the node fetches and checks (`crate::service_bundle`).
    /// On Podman and Apple (and Kubernetes over the protocol), it is mounted
    /// read-only at `/opt/tracon-services/<name>` in every sandbox, and the
    /// service runs there under the sandbox's supervisor instead of in
    /// `image`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<PathBuf>,
    /// With `bundle`: what the supervisor starts, inside the sandbox.
    /// Default `/opt/tracon-services/<name>/run`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub run: Vec<String>,
    /// With `bundle`: environment added to the sandbox's own.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// With `bundle`: whether the supervisor starts it again when it exits.
    #[serde(skip_serializing_if = "ServiceRestart::is_default")]
    pub restart: ServiceRestart,
    /// The port the service listens on, on the session's loopback.
    pub port: u16,
    /// An HTTP path that answers 2xx once the service is ready (`/json/version`
    /// for a CDP browser). Absent: ready once the port accepts a connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready: Option<String>,
    /// How long the service may take to become ready before it is reported
    /// as failed. Zero takes the default.
    #[serde(skip_serializing_if = "is_zero")]
    pub timeout_secs: u64,
}

/// When a bundled service is started again after it exits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceRestart {
    Never,
    #[default]
    OnFailure,
    Always,
}

impl ServiceRestart {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Where a bundled service's directory is mounted inside a sandbox, under
/// its name.
pub const SERVICE_BUNDLES_TARGET: &str = "/opt/tracon-services";

impl Service {
    pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

    /// Where its bundle is inside a sandbox.
    pub fn bundle_target(&self) -> String {
        format!("{SERVICE_BUNDLES_TARGET}/{}", self.name)
    }

    /// What the supervisor runs: `run` (or the bundle's `run`), then
    /// `command`.
    pub fn sandbox_argv(&self) -> Vec<String> {
        let mut argv = match self.run.is_empty() {
            true => vec![format!("{}/run", self.bundle_target())],
            false => self.run.clone(),
        };
        argv.extend(self.command.iter().cloned());
        argv
    }

    pub fn timeout_secs(&self) -> u64 {
        match self.timeout_secs {
            0 => Self::DEFAULT_TIMEOUT_SECS,
            n => n,
        }
    }
}

/// A service name is what a container name and a policy argument are built
/// from, so it is held to what both can carry.
pub fn validate_services(services: &[Service]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for service in services {
        let name = &service.name;
        if name.is_empty()
            || name.len() > 32
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || name.starts_with('-')
        {
            return Err(format!(
                "service {name:?}: name must be 1-32 lowercase letters, digits or '-'"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("service {name}: named twice"));
        }
        match (&service.bundle, service.image.is_empty()) {
            (None, true) => {
                return Err(format!("service {name}: needs an image or a bundle"));
            }
            (Some(bundle), _) if bundle.as_os_str() == crate::service_bundle::RELEASE => {
                if name != crate::service_bundle::BROWSER {
                    return Err(format!(
                        "service {name}: bundle = \"release\" is the release's browser bundle, \
                         and only a service named browser runs it"
                    ));
                }
            }
            (Some(bundle), _) if !bundle.is_absolute() => {
                return Err(format!(
                    "service {name}: bundle must be an absolute path, or \"release\""
                ));
            }
            _ => {}
        }
        if !service.image.is_empty() {
            immutable_image(&service.image).map_err(|e| format!("service {name}: image {e}"))?;
        }
        if service.bundle.is_none()
            && (!service.run.is_empty()
                || !service.env.is_empty()
                || service.restart != ServiceRestart::default())
        {
            return Err(format!(
                "service {name}: run, env and restart apply to a bundle, and it has none"
            ));
        }
        if service.run.iter().any(String::is_empty) {
            return Err(format!("service {name}: run has an empty argument"));
        }
        if let Some(key) = service
            .env
            .keys()
            .find(|k| k.is_empty() || k.contains('=') || k.contains('\0'))
        {
            return Err(format!("service {name}: env name {key:?} is not one"));
        }
        if service.port == 0 {
            return Err(format!("service {name}: port is required"));
        }
        if let Some(path) = &service.ready {
            if !path.starts_with('/') || path.chars().any(|c| c.is_whitespace() || c == '\'') {
                return Err(format!(
                    "service {name}: ready must be an HTTP path starting with '/'"
                ));
            }
        }
    }
    Ok(())
}

/// One `[[mcp_server]]`: an MCP server outside the node, spoken to over
/// streamable HTTP by the node alone. A session never learns its address or
/// holds its credential; it sees the server's tools on the node's own MCP
/// endpoint as `<name>__<tool>`, and each call is decided before the node
/// makes it.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct McpServer {
    /// The namespace its tools are offered under: `linear` offers
    /// `linear__create_issue`.
    pub name: String,
    /// The server's streamable HTTP endpoint.
    pub url: String,
    /// Channels whose sessions are offered its tools. Empty means none, as
    /// an unbound credential is unusable rather than universal.
    pub channels: Vec<String>,
    /// A broker credential whose `TOKEN` is sent with every request, as
    /// `Authorization: Bearer <TOKEN>` unless `header` names another header.
    /// It must be bound to the calling channel too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The header that carries the credential's `TOKEN` as it is, for a
    /// server that does not take a bearer token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// Tools, by the server's own names, that run without asking unless a
    /// signed policy rule denies them. `*` matches any run of characters.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Tools always put to the operator, even when a policy rule allows
    /// them. A tool no list names is asked unless a policy rule allows it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    /// Tools refused outright. Deny wins over ask, and ask over allow.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// How long one call may take. Zero takes the default.
    #[serde(skip_serializing_if = "is_zero")]
    pub timeout_secs: u64,
}

impl McpServer {
    pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

    pub fn timeout_secs(&self) -> u64 {
        match self.timeout_secs {
            0 => Self::DEFAULT_TIMEOUT_SECS,
            n => n,
        }
    }

    /// Whether a session on `channel` is offered this server at all.
    pub fn serves(&self, channel: &str) -> bool {
        self.channels.iter().any(|c| c == channel)
    }
}

/// Headers the node writes itself on every MCP request, which a credential
/// may not be sent in place of.
const MCP_RESERVED_HEADERS: &[&str] = &[
    "accept",
    "content-type",
    "content-length",
    "host",
    "connection",
    "transfer-encoding",
    "mcp-session-id",
    "mcp-protocol-version",
];

/// A server's name is the prefix its tools are offered under and split back
/// off by, so it is held to characters that cannot run into `__`.
pub fn validate_mcp_servers(servers: &[McpServer]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for server in servers {
        let name = &server.name;
        if name.is_empty()
            || name.len() > 24
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || name.starts_with('-')
        {
            return Err(format!(
                "mcp_server {name:?}: name must be 1-24 lowercase letters, digits or '-'"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("mcp_server {name}: named twice"));
        }
        match url::Url::parse(&server.url) {
            Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => {
                if !url.username().is_empty() || url.password().is_some() {
                    return Err(format!(
                        "mcp_server {name}: url must not carry a user or password; name a \
                         credential instead"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "mcp_server {name}: url must be an http or https address"
                ))
            }
        }
        if server
            .credential
            .as_deref()
            .is_some_and(|c| c.trim().is_empty())
        {
            return Err(format!("mcp_server {name}: credential is empty"));
        }
        if let Some(header) = &server.header {
            if server.credential.is_none() {
                return Err(format!(
                    "mcp_server {name}: header names where a credential goes, and no \
                     credential is named"
                ));
            }
            let lower = header.to_ascii_lowercase();
            if reqwest::header::HeaderName::from_bytes(header.as_bytes()).is_err()
                || MCP_RESERVED_HEADERS.contains(&lower.as_str())
            {
                return Err(format!(
                    "mcp_server {name}: header {header:?} is not one a credential can be sent in"
                ));
            }
        }
        for (list, tools) in [
            ("allow", &server.allow),
            ("ask", &server.ask),
            ("deny", &server.deny),
        ] {
            if tools.iter().any(|t| t.trim().is_empty()) {
                return Err(format!("mcp_server {name}: {list} has an empty entry"));
            }
        }
    }
    Ok(())
}

pub fn safe_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.starts_with('/')
        && !value.contains('\\')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\0'))
}

/// The corpus written back out as files, on a timer, so a directory kept under
/// version control, or read by other tools, follows the documents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Docs {
    /// Where to write; unset is off. A leading `~` is the operator's home.
    pub export_dir: Option<PathBuf>,
    /// Whose documents. Empty: `[session] default_channel`.
    pub export_channel: String,
    /// How often, in seconds (at least 60). The first export is at startup.
    pub export_every_secs: u64,
    /// Dedicated bind address for untrusted HTML preview resources.
    pub preview_listen: SocketAddr,
    /// Public HTTP(S) origin for the dedicated preview listener.
    pub preview_url: Option<String>,
    /// The document kinds a slug's prefix files a document under
    /// (`[[docs.kind]]`), beside the reserved ones the node's own behaviour
    /// hangs on (`mcp::docs::RESERVED`). Written out, the list replaces the
    /// default one whole. Read at the moment a document is classified, so a
    /// kind added, renamed or removed applies to every document already
    /// there: a removed kind's documents read as `other`.
    pub kind: Vec<DocKind>,
}

/// One operator-configured document kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocKind {
    /// The slug prefix: `runbook` files `runbook-deploy`, and `runbook`
    /// itself, under this kind. Lowercase letters and digits.
    pub prefix: String,
    /// What the Documents screen calls it.
    #[serde(default)]
    pub label: String,
    /// What it is for, in the words `doc_write` offers an agent.
    #[serde(default)]
    pub description: String,
    /// A working note: what a policy rule's `slug_working_note = true`
    /// matches, so the shipped bundle writes these unattended.
    #[serde(default)]
    pub working_note: bool,
}

impl DocKind {
    fn new(prefix: &str, label: &str, description: &str, working_note: bool) -> Self {
        Self {
            prefix: prefix.into(),
            label: label.into(),
            description: description.into(),
            working_note,
        }
    }
}

/// The kinds a node has before its operator configures any: the list it
/// always had, less the reserved ones.
pub fn default_doc_kinds() -> Vec<DocKind> {
    vec![
        DocKind::new(
            "guide",
            "Guide",
            "how something is done here, for whoever does it next",
            false,
        ),
        DocKind::new("ref", "Reference", "facts to look up", false),
        DocKind::new(
            "architecture",
            "Architecture",
            "how a system is put together, and why",
            false,
        ),
        DocKind::new(
            "proposal",
            "Proposal",
            "a change put to the operator to decide",
            false,
        ),
        DocKind::new(
            "repo",
            "Repository",
            "working notes on one repository",
            true,
        ),
        DocKind::new("note", "Note", "working notes", true),
        DocKind::new("meeting", "Meeting", "notes from a meeting", true),
        DocKind::new("inbox", "Inbox", "something to look at later", true),
    ]
}

/// Refuse a kind the node could not classify by: a prefix that is not one
/// word of lowercase letters and digits, that is a reserved kind or `other`,
/// or that another entry already has.
pub fn validate_doc_kinds(kinds: &[DocKind]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for kind in kinds {
        let prefix = kind.prefix.as_str();
        if prefix.is_empty()
            || !prefix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return Err(format!(
                "docs.kind: prefix {prefix:?} is not lowercase letters and digits"
            ));
        }
        if crate::mcp::docs::reserved(prefix).is_some() || prefix == crate::mcp::docs::OTHER {
            return Err(format!(
                "docs.kind: {prefix:?} is a kind the node reserves; it cannot be configured"
            ));
        }
        if !seen.insert(prefix) {
            return Err(format!("docs.kind: prefix {prefix:?} is given twice"));
        }
    }
    Ok(())
}

impl Default for Docs {
    fn default() -> Self {
        Self {
            export_dir: None,
            export_channel: String::new(),
            export_every_secs: 1800,
            preview_listen: "127.0.0.1:7422".parse().expect("valid preview address"),
            preview_url: None,
            kind: default_doc_kinds(),
        }
    }
}

impl Docs {
    pub fn preview_origin(&self) -> Result<String, String> {
        let Some(configured) = self.preview_url.as_deref() else {
            return Ok(format!("http://127.0.0.1:{}", self.preview_listen.port()));
        };
        let url = url::Url::parse(configured)
            .map_err(|error| format!("docs.preview_url is not a valid URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "docs.preview_url must be an HTTP(S) origin without credentials, path, query, or fragment"
                    .into(),
            );
        }
        Ok(url.origin().ascii_serialization())
    }
}

/// A harness the operator runs themselves, outside the boundary, reaching the
/// node's tools through the operator door (`POST /mcp/external/{channel}`).
///
/// Off by default. Turning it on is an explicit statement that a process on
/// the operator's own machine may ask this node to act on a channel's
/// credentials. What it gets is the boundary's tool surface, review included
/// for a worktree whose repository is under `repo_roots`, decided by the same
/// policy and logged on a session of its own. What it does not get is the
/// boundary's guarantee: a harness sharing the operator's UID could read what
/// the node holds, so the claim here is "never needs to", not "cannot".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct External {
    pub enabled: bool,
    /// Ignored. External harnesses no longer attach as sessions, so nothing
    /// times out; accepted so a node.toml that sets it still loads.
    pub idle_timeout_secs: u64,
    /// Where a worktree submitted for review may come from: its repository
    /// must live under one of these. A leading `~/` is the operator's home.
    pub repo_roots: Vec<PathBuf>,
    /// Run the repository's required checks on the node's own copy of a
    /// submitted candidate, in the repository's image, as evidence for its
    /// reviewer. They run behind the submission and never gate it: the
    /// harness's own checks ran where its toolchain is, and these are what
    /// the node can say about the same bytes. Off by default, because it runs
    /// code the node did not watch being written. On, it also makes `auto`
    /// the reviewer trigger for external submissions on a channel that sets
    /// none (`phases.review.trigger.external`): the reviewer has the checks
    /// to go on, and starts once they are done.
    pub run_checks: bool,
}

impl Default for External {
    fn default() -> Self {
        Self {
            enabled: false,
            idle_timeout_secs: 3600,
            repo_roots: vec![PathBuf::from("~/src")],
            run_checks: false,
        }
    }
}

/// A leading `~` is the operator's home. Nothing else in a configured path is
/// expanded: there is no shell here to do it.
pub fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

/// The embedding endpoint this node uses to build its own vector index.
///
/// It is an OpenAI-shaped `/v1/embeddings` service named here rather than a
/// model linked into the binary, because that is what lets a work channel be
/// embedded by something on this machine while a personal one may go to a
/// provider: `ARCHITECTURE.md` requires work-channel embeddings to stay local,
/// and inversion means a vector is about as sensitive as the text it came
/// from. Point `base_url` at a local `llama-server --embedding` and nothing
/// leaves the host.
///
/// Off by default. Retrieval is FTS5-only until a node is told otherwise, and
/// that remains a complete, working configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Embed {
    pub enabled: bool,
    /// Base URL of an OpenAI-shaped embeddings service; `/v1/embeddings` is
    /// appended. Ignored when `provider` is set.
    pub base_url: String,
    /// The embedding model's name, recorded on every vector so a change to it
    /// is detectable rather than a silent mixing of incomparable vectors.
    pub model: String,
    /// Its dimension. A change rebuilds the index from empty.
    pub dim: usize,
    /// A file holding the bearer token for `base_url`, when the endpoint wants
    /// one. A path rather than the token itself: `node.toml` is a plain file,
    /// seeded from a ConfigMap in a pod, so the secret stays somewhere that
    /// can be a Secret, and rotating it does not mean editing config.
    ///
    /// Not the broker. The broker holds what a *harness* may be given; this is
    /// the node talking to a service on its own machine, and putting it behind
    /// the gateway would mean adding loopback to the egress allowlist — which
    /// the harness's CONNECT proxy shares, so it would hand every session the
    /// run of this host's local ports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<PathBuf>,
    /// A `[providers]` name instead of `base_url`, when the endpoint needs a
    /// brokered credential. The call then goes through the model gateway, so
    /// the channel's provider binding and its daily ceiling still apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// How many chunks to embed in one request.
    pub batch: usize,
    pub timeout_secs: u64,
    /// The cosine similarity below which a neighbour is not a match. The index
    /// always has a nearest chunk, however unrelated, so without a floor every
    /// query returns something. Model-dependent: under Qwen3-Embedding-0.6B,
    /// chunks of unrelated documents score about 0.3 against each other. -1
    /// turns the floor off.
    pub min_similarity: f64,
}

impl Default for Embed {
    fn default() -> Self {
        Self {
            enabled: false,
            // llama.cpp's server default. Nothing is contacted unless
            // `enabled` is set.
            base_url: "http://127.0.0.1:8080".into(),
            model: "bge-m3".into(),
            dim: 1024,
            api_key_file: None,
            provider: None,
            batch: 16,
            timeout_secs: 60,
            min_similarity: 0.45,
        }
    }
}

/// Pushing what waits on the operator to the phones subscribed at this node.
/// Which channels notify at all is a channel binding (`notify.enabled`), not
/// config: it follows the work, not the machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Notify {
    /// Who a push service may contact about this sender: a `mailto:` or
    /// `https:` URL, sent as the VAPID subject. Apple checks its shape.
    pub contact: Option<String>,
}

/// Why this node will not push, in words for Settings and the log.
pub const NO_PUSH_CONTACT: &str = "no push contact: set [notify] contact in node.toml to a mailto: address or https: URL; Apple refuses pushes without a real one";

impl Notify {
    /// The VAPID subject this node signs with: `contact` when it is set, else
    /// the node's public HTTPS origin when one is known. Never an address made
    /// up on the spot: Apple answers `mailto:…@localhost` with a 403, so a
    /// node with neither sends nothing and says why.
    pub fn subject(&self, public_origin: Option<&str>) -> Result<String, &'static str> {
        if let Some(contact) = self.contact.as_deref() {
            return Ok(contact.to_string());
        }
        public_origin
            .filter(|origin| validate_contact(origin).is_ok())
            .map(|origin| origin.trim_end_matches('/').to_string())
            .ok_or(NO_PUSH_CONTACT)
    }
}

/// A VAPID subject a push service will take: `mailto:` an address on a real
/// domain, or an `https:` URL on a host other than this machine. Shape only;
/// nothing is sent to check that anyone answers.
pub fn validate_contact(contact: &str) -> Result<(), String> {
    let local = |host: &str| {
        let host = host
            .trim_matches(|c| c == '[' || c == ']')
            .to_ascii_lowercase();
        host == "localhost"
            || host.ends_with(".localhost")
            || host.ends_with(".local")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
    };
    if let Some(address) = contact.strip_prefix("mailto:") {
        let (user, domain) = address
            .split_once('@')
            .ok_or_else(|| format!("notify.contact {contact:?} has no address after mailto:"))?;
        let plain = |s: &str| !s.is_empty() && !s.contains(['@', ' ', '?', '/', ':']);
        if !plain(user) || !plain(domain) || !domain.contains('.') || local(domain) {
            return Err(format!(
                "notify.contact {contact:?} is not an address a push service can write to"
            ));
        }
        return Ok(());
    }
    let url = url::Url::parse(contact).map_err(|_| {
        format!("notify.contact {contact:?} must be a mailto: address or an https: URL")
    })?;
    match url.host_str() {
        Some(host) if url.scheme() == "https" && !local(host) => Ok(()),
        _ => Err(format!(
            "notify.contact {contact:?} must be a mailto: address or an https: URL on a public host"
        )),
    }
}

/// How clients that are not on this machine reach the node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Auth {
    /// The origin a remote browser opens the node at, behind whatever
    /// terminates TLS: `https://tracon-node.example.org`. It names the node
    /// to the outside world: the push subject falls back to it when
    /// `[notify] contact` is unset, and it is what a passkey is bound to.
    /// Unset, the node knows no public name for itself.
    pub public_origin: Option<String>,
    /// The proxies in front of the node whose `X-Forwarded-For` it believes,
    /// as addresses or CIDR blocks (an ingress controller's pod network, say).
    /// A loopback peer is always one. The node uses what they report only to
    /// tell callers apart (rate limits, and the address a device-approval
    /// page shows); it never makes a request local or trusted.
    pub trusted_proxies: Vec<String>,
}

impl Auth {
    /// The configured public origin, normalised to `scheme://host[:port]`
    /// (lower-case host, default port dropped), or why it is not one.
    ///
    /// HTTPS only, because the operator cookie is `Secure` off this machine
    /// and a passkey is refused on an insecure origin; plain HTTP is accepted
    /// only for `localhost`, which browsers treat as secure. The host is a
    /// domain name, never an IP address, because a passkey's relying-party ID
    /// must be one. An origin and nothing else: no credentials, path, query or
    /// fragment.
    /// `trusted_proxies`, parsed, or the first entry that is not an address
    /// or a block.
    pub fn trusted_proxies(&self) -> Result<Vec<crate::gateway::proxy::Cidr>, String> {
        self.trusted_proxies
            .iter()
            .map(|block| {
                block
                    .parse::<crate::gateway::proxy::Cidr>()
                    .map_err(|error| format!("auth.trusted_proxies: {error}"))
            })
            .collect()
    }

    pub fn public_origin(&self) -> Result<Option<String>, String> {
        let Some(configured) = self.public_origin.as_deref() else {
            return Ok(None);
        };
        let url = url::Url::parse(configured.trim())
            .map_err(|error| format!("auth.public_origin is not a valid URL: {error}"))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "auth.public_origin must be an origin without credentials, path, query, or fragment"
                    .into(),
            );
        }
        // A passkey is bound to a relying-party ID, which must be a domain
        // name: browsers refuse WebAuthn on an IP-literal origin, loopback
        // included. `localhost` is the one name that needs no certificate,
        // which is what local passkey testing runs against.
        let localhost =
            match url.host() {
                Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
                Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => return Err(
                    "auth.public_origin must name the node by a domain name, not an IP address: \
                     a passkey is bound to a domain, and browsers refuse one on an IP address \
                     (use http://localhost:<port> for local testing)"
                        .into(),
                ),
                None => return Err("auth.public_origin has no host".into()),
            };
        match url.scheme() {
            "https" => {}
            "http" if localhost => {}
            _ => return Err(
                "auth.public_origin must be an https: origin; http: is allowed only for localhost"
                    .into(),
            ),
        }
        Ok(Some(url.origin().ascii_serialization()))
    }
}

/// Trusted required checks the node runs for a candidate. These never come
/// from the candidate tree: an agent cannot make a required check disappear by
/// committing `.tracon/checks`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Supervision {
    /// One canonical shell command per required check, in operator-configured
    /// order. An empty list is explicit and does not count as verification.
    pub checks: Vec<String>,
    pub timeout_secs: u64,
    /// Operator-declared identities of dependency inputs that influence a
    /// check. Altering one deliberately invalidates reuse.
    pub dependency_inputs: std::collections::BTreeMap<String, String>,
    /// The largest immutable Git snapshot imported for one check run.
    pub max_snapshot_bytes: u64,
}

impl Default for Supervision {
    fn default() -> Self {
        Self {
            checks: vec!["just check".into()],
            timeout_secs: 900,
            dependency_inputs: Default::default(),
            max_snapshot_bytes: 512 * 1024 * 1024,
        }
    }
}

/// What a submission may be at most. Complexity accretes because nothing
/// says no at submission time; this does.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewLimits {
    /// Added lines. Removed ones are not counted: taking a feature out is
    /// mostly deletions, and those are not what a reviewer reads line by line.
    pub max_diff_lines: i64,
    pub max_files: usize,
}

impl Default for ReviewLimits {
    fn default() -> Self {
        Self {
            max_diff_lines: 10_000,
            max_files: 200,
        }
    }
}

impl ReviewLimits {
    /// How many times `max_diff_lines` the whole diff, added and removed
    /// together, may be. Every line of it is stored with the review and drawn
    /// on its screen, so deletions are bounded too, just far more loosely.
    pub const TOTAL_LINES_FACTOR: i64 = 10;

    /// Why a diff of this size is refused, or `None` when it is within the
    /// cap.
    pub fn refusal(&self, added: i64, removed: i64, files: usize) -> Option<String> {
        let total = added + removed;
        let max_total = self.max_diff_lines.saturating_mul(Self::TOTAL_LINES_FACTOR);
        if added > self.max_diff_lines || files > self.max_files {
            Some(format!(
                "the diff adds {added} lines (removing {removed}) across {files} files; the cap \
                 is {} added lines and {} files, and removed lines do not count toward it. \
                 Split the change into smaller submissions.",
                self.max_diff_lines, self.max_files
            ))
        } else if total > max_total {
            Some(format!(
                "the diff is {total} lines ({added} added, {removed} removed) across {files} \
                 files; added and removed together are capped at {max_total} lines. Split the \
                 change into smaller submissions."
            ))
        } else {
            None
        }
    }

    /// The defaults before they were raised. Saves once wrote every key, so a
    /// node that never touched the cap has these in its `node.toml` and would
    /// otherwise keep them forever.
    const RETIRED_DEFAULTS: (i64, usize) = (800, 40);

    fn lift_retired_defaults(&mut self) {
        if (self.max_diff_lines, self.max_files) == Self::RETIRED_DEFAULTS {
            *self = Self::default();
        }
    }
}

/// Which boundary this node establishes. `podman` is a Linux host with
/// rootless Podman; `kubernetes` is a node running as a pod that owns harness
/// pods; `apple` is a Mac running each sandbox in a lightweight VM of its own
/// with Apple's `container`, and the only kind a Mac runs.
///
/// The type's own default is `podman`, which is what a file that names no
/// kind gets everywhere but a Mac: a load settles that by the host
/// ([`RuntimeKind::for_host`]), so the defaults a test builds stay the same
/// on every platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    #[default]
    Podman,
    Kubernetes,
    Apple,
}

impl RuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Kubernetes => "kubernetes",
            Self::Apple => "apple",
        }
    }

    /// The kind a node runs when its `node.toml` names none: Apple
    /// Containerization on a Mac, Podman elsewhere.
    pub fn for_host() -> Self {
        Self::for_host_on(HOST_IS_MAC)
    }

    fn for_host_on(mac: bool) -> Self {
        match mac {
            true => Self::Apple,
            false => Self::Podman,
        }
    }
}

/// Whether this host is a Mac, for [`HostRules::here`].
const HOST_IS_MAC: bool = cfg!(target_os = "macos");

/// What the host a `node.toml` is read for decides about it, beyond what
/// the file says: the kind a file that names none runs, and whether
/// `kind = "podman"` is refused (a Mac's). Only a node's own load and its own
/// edits apply this host's rules; another node's file, read here to check an
/// edit before it goes there, is that node's to judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostRules {
    /// The kind a file that names none runs; `None` leaves the type's
    /// default.
    pub default_kind: Option<RuntimeKind>,
    /// Refuse `kind = "podman"`.
    pub mac: bool,
}

impl HostRules {
    /// No host's: what the file says, and nothing a host would add.
    pub const NEUTRAL: Self = Self {
        default_kind: None,
        mac: false,
    };

    /// This host's, for this node's own `node.toml`.
    pub fn here() -> Self {
        Self::of_host(HOST_IS_MAC)
    }

    fn of_host(mac: bool) -> Self {
        Self {
            default_kind: Some(RuntimeKind::for_host_on(mac)),
            mac,
        }
    }

    /// Another node's `node.toml`, read here: the kind that node said it
    /// runs where its file names none, and nothing refused that only its own
    /// host would refuse, since it checks the edit again itself.
    pub fn peer(runs: Option<RuntimeKind>) -> Self {
        Self {
            default_kind: runs,
            mac: false,
        }
    }
}

/// What a Mac is told when its `node.toml` says `kind = "podman"`.
pub const PODMAN_ON_MAC: &str = "runtime.kind = \"podman\" is not supported on macOS: a Mac \
    runs its sandboxes with Apple Containerization. Set `kind = \"apple\"` under [runtime] \
    (or delete the line), install Apple's container package, and run \
    `container system start --enable-kernel-install` once (README, \"On a Mac with Apple \
    Containerization\")";

/// Refuse a runtime the host cannot run: Podman on a Mac, whose sandboxes
/// are Apple Containerization VMs.
fn validate_runtime_kind(kind: RuntimeKind, mac: bool) -> Result<(), String> {
    match (kind, mac) {
        (RuntimeKind::Podman, true) => Err(PODMAN_ON_MAC.into()),
        _ => Ok(()),
    }
}

/// Whether `text` names `[runtime] kind` itself.
fn names_runtime_kind(text: &str) -> bool {
    toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|value| value.get("runtime")?.get("kind").cloned())
        .is_some()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Runtime {
    pub kind: RuntimeKind,
    pub podman: PodmanRuntime,
    pub kubernetes: Kubernetes,
    pub apple: AppleRuntime,
    /// Immutable project images the operator explicitly accepts in addition to
    /// digest-addressed images from safe devcontainer metadata.
    pub approved_images: Vec<String>,
}

/// `[runtime.podman]`: how the Podman boundary runs what it runs.
///
/// Every harness, check and preparation runs under `tracon-sandbox` from the
/// harness bundle, over the sandbox protocol, on the stock base or a
/// repository's image built on it: with no network of its own, its trees on
/// its own disk and its routes to the node as tunnels. The `sandbox_*`
/// switches that once chose this over the gateway container, harness images
/// and mounted volumes are gone (`retired_podman_switches`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PodmanRuntime {
    /// Start `tracon-sandbox` under Podman's `--init` rather than as the
    /// container's PID 1.
    pub sandbox_init: bool,
}

#[cfg(test)]
impl Config {
    /// A pod-hosted node off the sandbox protocol: the one runtime that
    /// still runs repositories' own images with a harness layered on, which
    /// the tests written against that path cover.
    pub fn pod_hosted() -> Self {
        let mut config = Self::default();
        config.runtime.kind = RuntimeKind::Kubernetes;
        config
    }
}

/// The `[runtime.podman]` switches that chose the sandbox protocol and its
/// parts over the paths it replaced, which are gone.
const RETIRED_PODMAN_SWITCHES: [&str; 5] = [
    "sandbox_protocol",
    "sandbox_files",
    "sandbox_tunnels",
    "sandbox_images",
    "sandbox_services",
];

/// The retired `[runtime.podman]` switches `text` sets, by name, with the
/// value each is set to.
fn retired_podman_switches(text: &str) -> Vec<(&'static str, bool)> {
    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    let Some(podman) = value
        .get("runtime")
        .and_then(|runtime| runtime.get("podman"))
    else {
        return Vec::new();
    };
    RETIRED_PODMAN_SWITCHES
        .into_iter()
        .filter_map(|key| Some((key, podman.get(key)?.as_bool()?)))
        .collect()
}

/// The retired switches `text` sets on a Podman node, which ask for what
/// always happens there now: worth a line in the log. Another kind's node
/// does not read `[runtime.podman]` at all.
fn ignored_podman_switches(text: &str, kind: RuntimeKind) -> Vec<&'static str> {
    match kind {
        RuntimeKind::Podman => retired_podman_switches(text)
            .into_iter()
            .map(|(key, _)| key)
            .collect(),
        RuntimeKind::Kubernetes | RuntimeKind::Apple => Vec::new(),
    }
}

/// Refuse a Podman node whose `text` turns a retired switch off: the path it
/// kept is gone, and running the default instead would change what the
/// operator asked for without saying so. One set `true` asks for what
/// always happens, and is only worth a line in the log.
fn refuse_retired_podman_switches(text: &str, kind: RuntimeKind) -> Result<(), String> {
    if kind != RuntimeKind::Podman {
        return Ok(());
    }
    let off: Vec<&str> = retired_podman_switches(text)
        .into_iter()
        .filter(|(_, on)| !on)
        .map(|(key, _)| key)
        .collect();
    if off.is_empty() {
        return Ok(());
    }
    Err(format!(
        "[runtime.podman] {} = false is no longer supported: Podman runs every sandbox over \
         the sandbox protocol, on the stock base with the harnesses from the bundle, with no \
         network but tunnels to the node, and the gateway container, harness images and \
         mounted volumes those switches kept are gone. Delete the line{} and run `tracon setup` \
         (it removes the old gateway); a [[repo]] entry with `image =`, or a Dockerfile not on \
         `FROM ${{TRACON_SANDBOX_BASE}}`, moves to `build` steps (README, \"Starting work\"); \
         `tracon gc --legacy` removes the old harness images",
        off.join(" = false, "),
        if off.len() > 1 { "s" } else { "" },
    ))
}

/// The `[boundary]` and `[harness]` keys only the gateway container and the
/// harness images read, which went with them. Ignored, and worth one line in
/// the log rather than silence.
const RETIRED_BOUNDARY_KEYS: [(&str, &str); 7] = [
    ("boundary", "network"),
    ("boundary", "subnet"),
    ("boundary", "gateway_ip"),
    ("boundary", "gateway_image"),
    ("boundary", "harness_image"),
    ("boundary", "start_machine"),
    ("harness", "native_binary"),
];

/// The retired keys `text` still sets, as `table.key`.
fn retired_boundary_keys(text: &str) -> Vec<String> {
    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    RETIRED_BOUNDARY_KEYS
        .into_iter()
        .filter(|(table, key)| value.get(*table).and_then(|t| t.get(*key)).is_some())
        .map(|(table, key)| format!("{table}.{key}"))
        .collect()
}

/// What an older node's gateway and harness images were named, as its
/// `node.toml` still says, so an upgraded node can find and remove them
/// (`boundary::podman::setup::retire_gateway`, `tracon gc --legacy`). Not
/// part of the file's current shape: read from the retired keys, else the
/// names every node used by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredNames {
    /// `[boundary] network`: the internal network the gateway served.
    pub network: String,
    /// `[boundary] gateway_ip`: where the gateway answered on it, which a
    /// sandbox must not reach.
    pub gateway_ip: String,
    /// `[boundary] gateway_image`.
    pub gateway_image: String,
    /// `[boundary] harness_image`: the configured harness's image, which the
    /// other harnesses' were named after.
    pub harness_image: String,
}

/// The names an older node's gateway and harness images took by default.
pub const DEFAULT_GATEWAY_NETWORK: &str = "tracon-int";
pub const DEFAULT_GATEWAY_IP: &str = "10.89.0.2";
pub const DEFAULT_GATEWAY_IMAGE: &str = "localhost/tracon-gateway";
pub const DEFAULT_HARNESS_IMAGE: &str = "localhost/tracon-harness-claude";

impl Default for RetiredNames {
    fn default() -> Self {
        Self {
            network: DEFAULT_GATEWAY_NETWORK.into(),
            gateway_ip: DEFAULT_GATEWAY_IP.into(),
            gateway_image: DEFAULT_GATEWAY_IMAGE.into(),
            harness_image: DEFAULT_HARNESS_IMAGE.into(),
        }
    }
}

impl RetiredNames {
    fn from_text(text: &str) -> Self {
        let value = toml::from_str::<toml::Value>(text).ok();
        let named = |key: &str| {
            value
                .as_ref()?
                .get("boundary")?
                .get(key)?
                .as_str()
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        };
        let defaults = Self::default();
        Self {
            network: named("network").unwrap_or(defaults.network),
            gateway_ip: named("gateway_ip").unwrap_or(defaults.gateway_ip),
            gateway_image: named("gateway_image").unwrap_or(defaults.gateway_image),
            harness_image: named("harness_image").unwrap_or(defaults.harness_image),
        }
    }
}

/// `[runtime.apple]`: a Mac node whose sandboxes are Apple Containerization
/// VMs (`kind = "apple"`). Every sandbox runs over the sandbox protocol with
/// no network of its own: its trees travel over the files channel, its
/// routes to the node are tunnels, and its harness comes from the bundle on
/// the stock base image (`[sandbox] base_image`) or the repository's image,
/// which `container build` makes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppleRuntime {
    /// The `container` CLI. Empty: `/usr/local/bin/container`, where Apple's
    /// installer puts it.
    pub container: String,
    /// Most sandboxes at once; none for no limit of the provider's own.
    pub limit: Option<u32>,
    /// CPUs and memory (`4G`, `2048M`) each sandbox's VM gets; empty for
    /// `container`'s defaults.
    pub cpus: Option<u32>,
    pub memory: String,
    /// How long one `container build` of a repository's image may take.
    pub build_timeout_secs: u64,
}

impl Default for AppleRuntime {
    fn default() -> Self {
        Self {
            container: String::new(),
            limit: None,
            cpus: None,
            memory: String::new(),
            build_timeout_secs: 30 * 60,
        }
    }
}

/// The `[[repo]]` table a running node resolves against, once the operator
/// has saved one through it. Shared by everything holding the same `Config`;
/// a clone carries the table it saw and is on its own from then on.
#[derive(Debug, Default)]
pub struct LiveRepos(parking_lot::RwLock<Option<std::sync::Arc<Vec<Repo>>>>);

impl Clone for LiveRepos {
    fn clone(&self) -> Self {
        Self(parking_lot::RwLock::new(self.0.read().clone()))
    }
}

impl LiveRepos {
    pub fn set(&self, repos: Vec<Repo>) {
        *self.0.write() = Some(std::sync::Arc::new(repos));
    }
}

/// What the node does for one repository: the image its required checks and
/// its preparation run in, the checks themselves, and how its dependencies get
/// there before they run. Everything but `path` is optional, and what an entry
/// leaves out stays on the node-wide answer (`[supervision]`, the harness
/// image, no preparation).
///
/// The table is the operator's and lives in the node's configuration, never in
/// the repository: a candidate that could edit it could pick its own checks.
///
/// `path` is matched against a session's *repository*, not its worktree, so
/// every worktree of a checkout resolves to the same entry. An absolute path
/// must equal the repository root; a relative one matches a path suffix
/// (`github.com/owner/name` names a managed clone wherever the clone root is).
/// The first matching entry wins, so a more specific path belongs above a more
/// general one.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Repo {
    pub path: PathBuf,
    /// The toolchain image, digest-pinned. Evidence keyed on a mutable tag is
    /// not evidence: the same `repo:tag` can be two different toolchains on
    /// either side of a pull, and a check's reuse key would not know. A locally
    /// built image has a digest too (`podman image inspect` reports
    /// `RepoDigests`), so this costs a local build nothing.
    ///
    /// Refused where every repository runs on the stock base
    /// (`repo_image::stock_base_only`), as is a `dockerfile` that does not
    /// begin `FROM ${TRACON_SANDBOX_BASE}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The repository's own dev environment, as a path inside it (commonly
    /// `.devcontainer/Dockerfile`), in place of `image`. The node builds it
    /// from the repository's default branch — never from a candidate, which
    /// could otherwise choose the image its own checks run in — and pins what
    /// it built, rebuilding when the file or its context changes. Only the
    /// Dockerfile is honoured: a devcontainer's hooks, features and compose
    /// files are not, and `prepare` is where their work belongs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    /// The build context for `dockerfile`, as a path inside the repository.
    /// Left out, it is the directory the Dockerfile is in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Shell steps run as root, at image build time, on the pinned stock
    /// base (`[sandbox] base_image`), in place of `image` or `dockerfile`:
    /// `apt-get install -y php8.5-cli`, or installing uv or rustup into
    /// `/usr/local`. Each step is one `RUN` of `sh -ec`; the image ends as the
    /// base's own user (65532) in `/work`, and `prepare` runs in it after.
    /// The steps reach only `egress`. Only where sandboxes run on the stock
    /// base: Podman, Apple, or Kubernetes with `sandbox_protocol`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub build: Vec<String>,
    /// This repository's required checks, in place of `[supervision] checks`.
    /// An empty list is explicit, as it is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<String>>,
    /// In place of `[supervision] timeout_secs`, for a repository whose
    /// preparation and checks take longer than the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Commands run once before a run's required checks, in the same image,
    /// on a copy of the candidate every check is then copied from, with a
    /// dependency cache writable and `egress` reachable. This is where
    /// `bun install`, `composer install` and `cargo fetch` belong: the checks
    /// that follow have neither.
    pub prepare: Vec<String>,
    /// What preparation and `build` may reach: a preset (`apt`, `crates`, `npm`, `pypi`,
    /// `packagist`, `github`) or a literal host name. Empty means preparation
    /// runs with no egress at all.
    pub egress: Vec<String>,
    /// Open `egress` to this repository's sessions too, so an agent can add a
    /// dependency and run what it installed. Off unless asked for: a session
    /// is long-lived and runs what a model decides, and a host that accepts
    /// uploads (`github` and `packagist` both carry `api.github.com`) accepts
    /// them from it for as long as it runs.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_egress: bool,
    /// Credentials in the node's broker whose environment the node lends to
    /// the one preparation it runs on the default branch, which fills the
    /// base cache every candidate and session starts from: a private
    /// registry's token, read by Composer from `COMPOSER_AUTH` or by npm
    /// through `.npmrc`. Never given to a candidate's preparation, a
    /// session's, or any check, all of which run code an agent can change.
    /// Each must also name this repository in its own `repos`, so naming a
    /// credential here does not by itself release it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prepare_credentials: Vec<String>,
    /// How this repository's commits reach the forge, in place of the
    /// channel's and `[publish] commits`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits: Option<Commits>,
    /// This repository's commit-subject and branch rules, in place of the
    /// channel's and `[publish] style`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<crate::review::prose::Style>,
}

/// The hosts each egress preset stands for. A preset is the registry and the
/// hosts it redirects downloads to, and nothing else. `packagist` carries
/// GitHub's archive hosts because that is where Composer's `dist` downloads
/// actually come from.
const EGRESS_PRESETS: &[(&str, &[&str])] = &[
    (
        "crates",
        &["crates.io", "index.crates.io", "static.crates.io"],
    ),
    ("npm", &["registry.npmjs.org"]),
    ("pypi", &["pypi.org", "files.pythonhosted.org"]),
    (
        "packagist",
        &[
            "packagist.org",
            "repo.packagist.org",
            "api.github.com",
            "codeload.github.com",
        ],
    ),
    (
        "github",
        &["github.com", "api.github.com", "codeload.github.com"],
    ),
    // The stock base's own apt sources (Ubuntu's snapshot service and the
    // PHP PPA), and Ubuntu's mirrors for a step that adds them back, arm64's
    // included. Only over HTTPS: the proxy carries nothing else.
    (
        "apt",
        &[
            "snapshot.ubuntu.com",
            "ppa.launchpadcontent.net",
            "archive.ubuntu.com",
            "security.ubuntu.com",
            "ports.ubuntu.com",
        ],
    ),
];

/// The preset names an `egress` entry may use, and the hosts each opens, for
/// an interface that offers them.
pub fn egress_presets() -> Vec<(&'static str, &'static [&'static str])> {
    EGRESS_PRESETS.to_vec()
}

impl Repo {
    /// Whether this entry is the one for `repo`.
    pub fn matches(&self, repo: &Path) -> bool {
        repo_path_matches(&self.path, repo)
    }

    /// The literal hosts `egress` names, presets expanded, in the order
    /// written and without repeats.
    pub fn egress_hosts(&self) -> Result<Vec<String>, String> {
        let mut hosts: Vec<String> = Vec::new();
        for entry in &self.egress {
            let entry = entry.trim();
            let expanded: Vec<&str> = match EGRESS_PRESETS.iter().find(|(name, _)| *name == entry) {
                Some((_, preset)) => preset.to_vec(),
                None if literal_host(entry) => vec![entry],
                None => {
                    return Err(format!(
                        "egress {entry:?} is neither a preset ({}) nor a host name",
                        EGRESS_PRESETS
                            .iter()
                            .map(|(name, _)| *name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            };
            for host in expanded {
                if !hosts.iter().any(|seen| seen == host) {
                    hosts.push(host.to_string());
                }
            }
        }
        Ok(hosts)
    }
}

/// Whether `configured`, written as a `[[repo]]` entry's `path`, names `repo`: an
/// absolute path must equal the repository root, a relative one matches a
/// path suffix.
pub fn repo_path_matches(configured: &Path, repo: &Path) -> bool {
    let configured = expand_home(configured);
    let want: Vec<_> = configured.components().collect();
    if want.is_empty() {
        return false;
    }
    let have: Vec<_> = repo.components().collect();
    // Component-wise, never textual: `…/name` must not match `…/name2`.
    if configured.is_absolute() {
        return have == want;
    }
    have.len() >= want.len() && have[have.len() - want.len()..] == want[..]
}

/// A plain DNS name: no scheme, port, path, wildcard or pattern. The scoped
/// gateway matches these exactly, so anything else would be a host that can
/// never match rather than a wider rule.
fn literal_host(value: &str) -> bool {
    value.contains('.')
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

/// Refuse a repository table the node could not honour. An image that can
/// never pin, an egress entry that is not a host, or a repository named twice
/// is worse silently ignored at check time than refused at load: the operator
/// would read a `not runnable` (or a preparation that reaches nothing) and
/// have nothing pointing at the line that caused it.
pub fn validate_repos(repos: &[Repo]) -> Result<(), String> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for entry in repos {
        let named = entry.path.display();
        if entry.path.as_os_str().is_empty() {
            return Err("repo entry has no path: name the repository it is for".into());
        }
        if let Some(image) = &entry.image {
            immutable_image(image).map_err(|e| format!("repo {named}: image {e}"))?;
        }
        if entry.image.is_some() && entry.dockerfile.is_some() {
            return Err(format!(
                "repo {named}: names both image and dockerfile; the node either runs the image \
                 it is given or builds one, so keep the one that is meant"
            ));
        }
        if entry.context.is_some() && entry.dockerfile.is_none() {
            return Err(format!("repo {named}: context has no dockerfile to build"));
        }
        validate_build_steps(entry)?;
        for (field, value) in [
            ("dockerfile", &entry.dockerfile),
            ("context", &entry.context),
        ] {
            // `.` is the repository root, which is a context and never a file.
            let root = field == "context" && value.as_deref() == Some(".");
            if value
                .as_deref()
                .is_some_and(|value| !root && !safe_relative_path(value))
            {
                return Err(format!(
                    "repo {named}: {field} must be a path inside the repository"
                ));
            }
        }
        entry
            .egress_hosts()
            .map_err(|e| format!("repo {named}: {e}"))?;
        if entry
            .prepare
            .iter()
            .any(|command| command.trim().is_empty())
        {
            return Err(format!("repo {named}: prepare has an empty command"));
        }
        if entry.session_egress && entry.egress.is_empty() {
            return Err(format!(
                "repo {named}: session_egress opens `egress` to sessions, and egress is empty"
            ));
        }
        if entry.timeout_secs == Some(0) {
            return Err(format!("repo {named}: timeout_secs must be at least 1"));
        }
        if !entry.prepare_credentials.is_empty() {
            if entry
                .prepare_credentials
                .iter()
                .any(|name| name.trim().is_empty() || name.trim() != name)
            {
                return Err(format!(
                    "repo {named}: prepare_credentials names a credential by an empty or padded name"
                ));
            }
            // The only preparation that is lent them is the one that warms
            // the base cache from the default branch, and the node runs that
            // only after building the image an entry's Dockerfile names.
            if entry.dockerfile.is_none() && entry.build.is_empty() {
                return Err(format!(
                    "repo {named}: prepare_credentials are lent only to the default branch's \
                     preparation, which runs when the node builds this repository's image; \
                     name a dockerfile or build steps"
                ));
            }
            if entry.prepare.is_empty() {
                return Err(format!(
                    "repo {named}: prepare_credentials has no prepare command to lend them to"
                ));
            }
        }
        let path = expand_home(&entry.path);
        if seen.contains(&path) {
            return Err(format!(
                "repo {named} is named twice: the first match wins, so the second entry could \
                 never apply"
            ));
        }
        seen.push(path);
    }
    Ok(())
}

/// An entry's `build` steps, as written: on the stock base alone, so never
/// beside an image of its own, and no step empty.
fn validate_build_steps(entry: &Repo) -> Result<(), String> {
    if entry.build.is_empty() {
        return Ok(());
    }
    let named = entry.path.display();
    if entry.image.is_some() || entry.dockerfile.is_some() {
        return Err(format!(
            "repo {named}: build steps run on the stock base, so the entry names no image or \
             dockerfile beside them; keep build and drop the other"
        ));
    }
    if entry.build.iter().any(|step| step.trim().is_empty()) {
        return Err(format!("repo {named}: build has an empty step"));
    }
    Ok(())
}

/// Whether this node's sandboxes run on the stock base, which is what an
/// entry's `build` steps are layered on.
pub fn builds_on_base(config: &Config) -> bool {
    match config.runtime.kind {
        RuntimeKind::Podman | RuntimeKind::Apple => true,
        RuntimeKind::Kubernetes => config.runtime.kubernetes.sandbox_protocol,
    }
}

/// Refuse `build` steps on a node whose sandboxes do not run the stock base:
/// the harness image they would have to stand on is retired with them.
pub fn validate_repo_builds(config: &Config) -> Result<(), String> {
    if builds_on_base(config) {
        return Ok(());
    }
    match config.repo.iter().find(|entry| !entry.build.is_empty()) {
        Some(entry) => Err(format!(
            "repo {}: build steps stand on the stock sandbox base, which this node runs only \
             with runtime.kubernetes.sandbox_protocol = true",
            entry.path.display()
        )),
        None => Ok(()),
    }
}

/// The pod-hosted boundary: one harness Pod per session, created by the node
/// through the API, isolated by the NetworkPolicies the deployment carries
/// (`deploy/kubernetes/base`), sharing one RWO volume with the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Kubernetes {
    /// Namespace for harness pods. Empty: the pod's own.
    pub namespace: String,
    pub harness_image: String,
    /// The PersistentVolumeClaim both the node and every harness mount.
    pub state_claim: String,
    /// Where that claim is mounted, in the node and in every harness pod —
    /// identical, because linked-worktree `.git` pointers are absolute paths.
    pub state_mount: PathBuf,
    /// The harness user's home inside its pod; the state directory and
    /// gitconfig are mounted under it.
    pub harness_home: String,
    /// The uid the harness runs as. Non-root, and the same as the node so the
    /// files each writes on the shared volume are readable by the other.
    pub uid: i64,
    /// The name the harness pod resolves to the node's pod IP.
    pub gateway_host: String,
    /// What builds a repository's image here, `[runtime.kubernetes.builder]`.
    /// Left out, the node builds nothing and a `[[repo]]` entry needs a
    /// pinned `image`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builder: Option<KubeBuilder>,
    /// How long a harness pod may take to start (scheduling, pulling its
    /// image, mounting) before the run is given up with the reason it was
    /// still waiting. Only the start is bounded here: a started command runs
    /// until it ends or its caller stops it, as it does under Podman.
    pub start_timeout_secs: u64,
    /// Requests and limits for harness pods, `[runtime.kubernetes.resources]`.
    pub resources: KubeResources,
    /// Secrets a harness pod pulls its image with. Empty leaves the pod
    /// without any, so the namespace's `default` service account supplies
    /// its own (a registry integration that patches that account works
    /// unchanged). Naming any replaces that account's list for these pods.
    pub image_pull_secrets: Vec<String>,
    /// Size of the memory-backed `/dev/shm` every harness pod gets. Chromium
    /// keeps its renderers' shared memory there, and the 64 MiB a container
    /// runtime gives by default crashes a browser test. It counts against
    /// the pod's memory limit, so it must fit within both limits.
    pub shm_size: String,
    /// Address blocks the per-client egress proxy refuses besides the
    /// private ones (RFC 1918, 100.64.0.0/10, fc00::/7), which it always
    /// refuses here: a cluster whose pod or service range, or whose nodes'
    /// network, is publicly routable names it, so a grant cannot reach it.
    /// So does one whose network translates IPv6 to IPv4 (NAT64,
    /// `64:ff9b::/96`; 6to4, `2002::/16`): an IPv4 address carried inside
    /// such an IPv6 one is not decoded, so the block itself must be named.
    pub internal_cidrs: Vec<String>,
    /// Run every harness pod as a sandbox over the sandbox protocol
    /// (`provider::kubernetes`): `tracon-sandbox` from the harness bundle,
    /// copied in by an init container, dials the node pod on `sandbox_port`
    /// over TLS, and the harness's process, stdio, files and its routes to the
    /// MCP gateway, the model gateway and the proxies all travel over that one
    /// connection. Its workspace and state are on the pod's own emptyDirs, not
    /// the state claim; no Secret is made for a run; the pod is placed beside
    /// the node pod by affinity. Needs the `tracon-sandbox` NetworkPolicy
    /// (`deploy/kubernetes/sandbox-protocol`). The bundle is the node's own
    /// (`[sandbox] bundle`, else its release's, fetched and checked as
    /// a Podman node does), which the node hands each pod's init
    /// container on the same port.
    pub sandbox_protocol: bool,
    /// The port the node listens on, on its pod address, for its sandboxes'
    /// connections.
    pub sandbox_port: u16,
}

/// The port a pod-hosted node takes its sandboxes' connections on by
/// default.
pub const KUBE_SANDBOX_PORT: u16 = 7426;

/// What a harness pod asks the scheduler for. Three kinds: a session (the
/// harness and whatever it runs while the operator works), a command run to
/// completion (a check, a preparation), which is where a test suite with a
/// bundler and a browser lands, and a probe (a version, a `command -v`, a
/// reachability check), which runs nothing a repository brings.
///
/// Every pod renders explicit requests and a memory limit. A namespace's
/// LimitRange otherwise fills in its own, and a request above its default
/// limit is refused at admission rather than defaulted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct KubeResources {
    pub session: PodResources,
    pub command: PodResources,
    pub probe: PodResources,
}

/// One kind of harness pod's requests and limits, as Kubernetes quantities
/// (`500m`, `2Gi`). An unset key takes the default for its kind
/// (`KubeResources::session`, `KubeResources::command`,
/// `KubeResources::probe`), so a table that changes one value keeps the rest.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PodResources {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_request: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_request: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<String>,
    /// Unset by default: a CPU limit throttles a build or a test run on a
    /// node that has idle cores, and buys nothing a request does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<String>,
}

/// A pod's resources with every default filled in: what is rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodSizing {
    pub cpu_request: String,
    pub memory_request: String,
    pub memory_limit: String,
    pub cpu_limit: Option<String>,
}

impl PodResources {
    fn over(&self, base: PodSizing) -> PodSizing {
        PodSizing {
            cpu_request: self.cpu_request.clone().unwrap_or(base.cpu_request),
            memory_request: self.memory_request.clone().unwrap_or(base.memory_request),
            memory_limit: self.memory_limit.clone().unwrap_or(base.memory_limit),
            cpu_limit: self.cpu_limit.clone().or(base.cpu_limit),
        }
    }
}

impl KubeResources {
    /// A session's pod. The defaults suit one shared 4 vCPU / 8 GiB node: a
    /// harness idles near a few hundred MiB, but a session also runs the
    /// project's tests, so the limit leaves room for a browser.
    pub fn session(&self) -> PodSizing {
        self.session.over(PodSizing {
            cpu_request: "250m".into(),
            memory_request: "1Gi".into(),
            memory_limit: "4Gi".into(),
            cpu_limit: None,
        })
    }

    /// A check's or a preparation's pod. A Laravel suite with Vite and
    /// Playwright peaks around 2 to 3 GiB, so that much is requested rather
    /// than hoped for.
    pub fn command(&self) -> PodSizing {
        self.command.over(PodSizing {
            cpu_request: "500m".into(),
            memory_request: "2Gi".into(),
            memory_limit: "4Gi".into(),
            cpu_limit: None,
        })
    }

    /// A probe's pod. Captured pods are pinned to the node's own Kubernetes
    /// node, so one the node cannot fit is refused outright rather than left
    /// pending; a `--version` sized as a test suite was refused on a node
    /// that still had room for a session.
    pub fn probe(&self) -> PodSizing {
        self.probe.over(PodSizing {
            cpu_request: "100m".into(),
            memory_request: "128Mi".into(),
            memory_limit: "512Mi".into(),
            cpu_limit: None,
        })
    }
}

/// A Kubernetes resource quantity (`250m`, `1.5`, `2Gi`, `1e3`) in base
/// units, or `None` for anything the API server would refuse. Negative values
/// are refused too: no request or limit can be one.
pub fn kube_quantity(value: &str) -> Option<f64> {
    let unsigned = value.strip_prefix('+').unwrap_or(value);
    let split = unsigned
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(unsigned.len());
    let (number, suffix) = unsigned.split_at(split);
    // Digits and dots only, so `inf` and `nan` never reach the parse.
    let number: f64 = number.parse().ok()?;
    let scale = match suffix {
        "" => 1.0,
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        "Ki" => 1024f64,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        exponent => {
            let digits = exponent
                .strip_prefix('e')
                .or_else(|| exponent.strip_prefix('E'))?;
            let unsigned = digits.strip_prefix(['+', '-']).unwrap_or(digits);
            if unsigned.is_empty() || !unsigned.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            10f64.powi(digits.parse().ok()?)
        }
    };
    Some(number * scale)
}

/// A Secret's name: a DNS subdomain, as the API server checks it.
fn dns_subdomain(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

impl Kubernetes {
    /// Refuse sizing the API server would reject at every pod create, or that
    /// could never hold: a limit below its own request is refused by the
    /// API, and found at load it points at the line rather than at a
    /// session that will not start.
    pub fn validate(&self) -> Result<(), String> {
        for (kind, sizing, browser) in [
            ("session", self.resources.session(), true),
            ("command", self.resources.command(), true),
            ("probe", self.resources.probe(), false),
        ] {
            let key = |name: &str| format!("runtime.kubernetes.resources.{kind}.{name}");
            let parse = |name: &str, value: &str| {
                kube_quantity(value)
                    .ok_or_else(|| format!("{}: {value:?} is not a quantity", key(name)))
            };
            let cpu_request = parse("cpu_request", &sizing.cpu_request)?;
            let memory_request = parse("memory_request", &sizing.memory_request)?;
            let memory_limit = parse("memory_limit", &sizing.memory_limit)?;
            if memory_limit < memory_request {
                return Err(format!(
                    "{} ({}) is below memory_request ({})",
                    key("memory_limit"),
                    sizing.memory_limit,
                    sizing.memory_request
                ));
            }
            if let Some(limit) = &sizing.cpu_limit {
                if parse("cpu_limit", limit)? < cpu_request {
                    return Err(format!(
                        "{} ({limit}) is below cpu_request ({})",
                        key("cpu_limit"),
                        sizing.cpu_request
                    ));
                }
            }
            let shm = kube_quantity(&self.shm_size)
                .filter(|size| *size > 0.0)
                .ok_or_else(|| {
                    format!(
                        "runtime.kubernetes.shm_size: {:?} is not a positive quantity",
                        self.shm_size
                    )
                })?;
            // A probe's pod has no shared memory to hold.
            if browser && shm > memory_limit {
                return Err(format!(
                    "runtime.kubernetes.shm_size ({}) is larger than {} ({}): shared memory \
                     counts against the pod's memory limit",
                    self.shm_size,
                    key("memory_limit"),
                    sizing.memory_limit
                ));
            }
        }
        if let Some(name) = self
            .image_pull_secrets
            .iter()
            .find(|name| !dns_subdomain(name))
        {
            return Err(format!(
                "runtime.kubernetes.image_pull_secrets: {name:?} is not a Secret name"
            ));
        }
        // What the egress proxy refuses is part of the boundary, so a block
        // that does not parse is refused here, at load, rather than found at
        // the next boundary check with per-client egress already off.
        for block in &self.internal_cidrs {
            block
                .parse::<crate::gateway::proxy::Cidr>()
                .map_err(|error| format!("runtime.kubernetes.internal_cidrs: {error}"))?;
        }
        // Both go into the layer the node builds over a repository's image
        // (`repo_image::RunAs`), in a shell command and a passwd entry, and
        // the pod refuses root.
        if self.uid <= 0 {
            return Err(format!(
                "runtime.kubernetes.uid: {} is not a non-root uid",
                self.uid
            ));
        }
        if !safe_home(&self.harness_home) {
            return Err(format!(
                "runtime.kubernetes.harness_home: {:?} must be an absolute path of letters, \
                 digits, '.', '_', '-' and '/', other than /",
                self.harness_home
            ));
        }
        Ok(())
    }
}

/// An absolute directory that can go unquoted into a shell command, a
/// Dockerfile `ENV` and a passwd entry: no spaces, quotes, colons or `..`.
fn safe_home(home: &str) -> bool {
    home.starts_with('/')
        && home.len() > 1
        && !home.ends_with('/')
        && home
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
        && !home.split('/').any(|part| part == "." || part == "..")
        && !home.contains("//")
}

impl Default for Kubernetes {
    fn default() -> Self {
        Self {
            namespace: String::new(),
            harness_image: format!(
                "ghcr.io/cosmicspork/tracon-harness-claude:{}",
                env!("CARGO_PKG_VERSION")
            ),
            state_claim: "tracon-state".into(),
            state_mount: PathBuf::from("/state"),
            harness_home: "/home/harness".into(),
            uid: 65532,
            gateway_host: "tracon-gw".into(),
            builder: None,
            // A first pull of a repository image can be gigabytes.
            start_timeout_secs: 15 * 60,
            resources: KubeResources::default(),
            image_pull_secrets: Vec::new(),
            shm_size: "1Gi".into(),
            internal_cidrs: Vec::new(),
            sandbox_protocol: false,
            sandbox_port: KUBE_SANDBOX_PORT,
        }
    }
}

/// A BuildKit daemon the node drives with `buildctl` over mutual TLS, and the
/// registry it pushes what it builds to
/// (`boundary::kubernetes::build::BuildKitBuilder`). The push credential is
/// mounted in the node's pod, not the daemon's, but BuildKit's session auth
/// hands it to the daemon on request during a build, so it should be scoped
/// to the registry alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct KubeBuilder {
    /// `tcp://buildkitd.tracon-build.svc:1234`.
    pub addr: String,
    /// The client certificate: `ca.crt`, `tls.crt` and `tls.key`, as a
    /// cert-manager Secret mounts them.
    pub tls_dir: PathBuf,
    /// Where built images are pushed, as `host/namespace`:
    /// `registry.digitalocean.com/<registry>`.
    pub registry: String,
    /// A directory holding a Docker `config.json` with a credential that may
    /// push to `registry`: what `DOCKER_CONFIG` names for `buildctl`, and what
    /// the node reads to look images up and remove them.
    pub docker_config: PathBuf,
    /// The longest one build may take. Left out, the node's own limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// A file holding a DigitalOcean API token, for removing an image when
    /// the registry refuses a delete through the distribution API. Left out,
    /// the registry credential's password is tried, which for a credential
    /// DigitalOcean issued is an API token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub do_token_file: Option<PathBuf>,
    /// The `buildctl` to run.
    pub buildctl: String,
    /// Build without BuildKit's cache (`--no-cache`). A daemon shared with
    /// anything else reuses a `RUN` it ran before on the same base for the
    /// same command, however that build reached the network, so a
    /// repository's `build` steps are held to their `egress` on a cache hit
    /// only when this node is the daemon's sole client, or with this on.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_cache: bool,
}

impl Default for KubeBuilder {
    fn default() -> Self {
        Self {
            addr: "tcp://buildkitd.tracon-build.svc:1234".into(),
            tls_dir: PathBuf::from("/etc/tracon/buildkit"),
            registry: String::new(),
            docker_config: PathBuf::from("/etc/tracon/registry"),
            timeout_secs: None,
            do_token_file: None,
            buildctl: "buildctl".into(),
            no_cache: false,
        }
    }
}

/// The hub this node dials. Written by `tracon enroll`; absent until then,
/// which leaves the node standalone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Mesh {
    pub hub_url: Option<String>,
    pub heartbeat_secs: u64,
    pub poll_secs: u64,
    pub command_timeout_secs: u64,
    /// How long an owner stream may take to answer its open before the
    /// serving node gives up. A stream that times out is closed, never
    /// re-sent.
    pub stream_open_timeout_secs: u64,
    /// How long a stream may go without a frame before either end closes it.
    pub stream_idle_secs: u64,
    /// The largest request body a stream may carry to an owner.
    pub stream_max_body_bytes: u64,
    /// The largest response an owner stream may carry back. An SSE stream is
    /// bounded by this too: a session's event stream is long, not infinite.
    pub stream_max_response_bytes: u64,
    /// How many owner streams this node may have open at once.
    pub stream_max_concurrent: usize,
    /// Renew shared subscription credentials first. Set it on a node that is
    /// always on; every other holder steps in only if this one has not.
    pub renew_credentials: bool,
    /// The peers (node ids) this node lets administer it over the mesh:
    /// read and edit its `node.toml`, and sign its providers in and out,
    /// each confirmed by the operator's passkey on that peer. Empty, the
    /// default, means none. An admin key itself: it is changed at this node,
    /// or remotely with this node's own passkey, never over the mesh.
    pub admin_peers: Vec<String>,
    /// Serve the hub's sandbox relay, so a sandbox that can only dial a
    /// public address reaches this node through the hub.
    pub sandbox_relay: SandboxRelay,
}

/// `[mesh.sandbox_relay]`: while on, the node holds a listen stream to the
/// hub and bridges each connection the hub relays for it to `target`. The
/// TLS inside is the sandbox's to this node; the hub only reads its SNI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SandboxRelay {
    pub enabled: bool,
    /// The node's local sandbox listener, `host:port`.
    pub target: String,
    /// Relayed connections bridged at once. An offer past this is left to
    /// time out at the hub, which closes it unanswered.
    pub max_streams: usize,
}

impl Default for SandboxRelay {
    fn default() -> Self {
        Self {
            enabled: false,
            target: String::new(),
            max_streams: 16,
        }
    }
}

impl Mesh {
    /// Whether `node_id` is one of [`Mesh::admin_peers`].
    pub fn is_admin_peer(&self, node_id: &str) -> bool {
        self.admin_peers.iter().any(|peer| peer == node_id)
    }
}

/// `[mesh] admin_peers` names nodes by id: 64 lowercase hex digits, the
/// form every node id takes, so a comparison is never fooled by case or a
/// prefix.
pub fn validate_admin_peers(peers: &[String]) -> Result<(), String> {
    for peer in peers {
        let ok = peer.len() == 64
            && peer
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !ok {
            return Err(format!(
                "[mesh] admin_peers: {peer:?} is not a node id (64 lowercase hex digits, as `tracon mesh id` prints it)"
            ));
        }
    }
    Ok(())
}
impl Default for Mesh {
    fn default() -> Self {
        Self {
            hub_url: None,
            heartbeat_secs: 60,
            poll_secs: 30,
            command_timeout_secs: 15,
            stream_open_timeout_secs: 30,
            stream_idle_secs: 120,
            stream_max_body_bytes: 8 * 1024 * 1024,
            stream_max_response_bytes: 256 * 1024 * 1024,
            stream_max_concurrent: 16,
            renew_credentials: false,
            admin_peers: Vec::new(),
            sandbox_relay: SandboxRelay::default(),
        }
    }
}

/// The harness listener: TCP or a Unix socket, written in TOML as either
/// `"127.0.0.1:7421"` or `"/run/user/1000/tracon/harness.sock"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HarnessListen {
    Tcp(std::net::SocketAddr),
    Unix(PathBuf),
}

impl Default for HarnessListen {
    fn default() -> Self {
        if cfg!(target_os = "linux") {
            HarnessListen::Unix(Config::runtime_dir().join("harness.sock"))
        } else {
            HarnessListen::Tcp("127.0.0.1:7421".parse().expect("valid default address"))
        }
    }
}

impl std::fmt::Display for HarnessListen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HarnessListen::Tcp(a) => write!(f, "{a}"),
            HarnessListen::Unix(p) => write!(f, "{}", p.display()),
        }
    }
}

/// The publishing CLIs. Names by default, absolute paths where a host keeps
/// them somewhere unusual.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Publish {
    pub gh: String,
    pub glab: String,
    pub git: String,
    /// How long one `gh` or `glab` call may run. Git itself is not bounded:
    /// a large push may take as long as it takes.
    pub forge_timeout_secs: u64,
    /// How a candidate's commits reach the forge, unless the channel's
    /// `publish.commits` binding or the repository's entry says otherwise.
    pub commits: Commits,
    /// The commit-subject and branch rules a submission is held to, unless
    /// the channel's `publish.style` binding or the repository's entry says
    /// otherwise. Every rule is off by default.
    pub style: crate::review::prose::Style,
}

/// How a candidate's commits reach the forge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Commits {
    /// One commit holding exactly the reviewed tree, carrying the approved
    /// message. The tree is the candidate, so nothing reviewed changes.
    #[default]
    Squash,
    /// The agent's commits as it wrote them.
    Keep,
}

/// What the GitHub tools may start. Only the operator's file says this: the
/// danger in starting a GitHub workflow is the workflow, not the ref, since
/// a release or a publish runs on `main` like any other branch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Github {
    /// The workflow files `run_dispatch` may start, by repository
    /// (`owner/name`, matched without regard to case) and file name as it
    /// sits in `.github/workflows/`. A workflow not listed is refused, so a
    /// repository not named here can start nothing.
    pub dispatch: std::collections::BTreeMap<String, Vec<String>>,
}

impl Github {
    /// Whether `run_dispatch` may start `workflow` in `repo`.
    pub fn may_dispatch(&self, repo: &str, workflow: &str) -> bool {
        self.dispatch
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(repo))
            .flat_map(|(_, files)| files)
            .any(|file| file == workflow)
    }
}

/// How the node runs the consulta sidecar. It stays a Python process because
/// Oracle's client is a glibc blob and the node is a static musl binary; the
/// pure-Python driver is what makes a read-only Oracle path possible at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Consulta {
    pub command: String,
    pub args: Vec<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Harness {
    /// The default harness: `claude` or `native`. A session's harness follows
    /// its model's credential, so this only breaks a tie. An unknown id
    /// refuses to start. A `node.toml` that still names a retired harness
    /// (`omp`, `opencode`) is migrated to `claude` as it loads
    /// (`crate::legacy::migrate_config`); a config that names one anyway is
    /// refused with the migration path rather than a bare refusal
    /// (`crate::adapter::retired_message`).
    pub id: String,
    /// The tools a session may use at all, by the harness's own names. Empty
    /// means the adapter's default set, which is the default here: for Claude
    /// Code that is the built-ins a node session uses, not all of them.
    ///
    /// Restricting is available but not on by default, and the reason is worth
    /// knowing: a tool list is a whitelist, and a harness's shell is easy to
    /// leave off it. Dropping the shell removes the agent's ability to commit —
    /// and without commits there is nothing to review, so the whole publish
    /// path stops. An agent that loses its shell does not report that it is
    /// stuck; it starts reading `.git` by hand to work around it.
    ///
    /// Reduce the surface deliberately, per node, once you know which tools a
    /// given channel actually needs.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Exact version this node runs. Checked twice: the harness's own
    /// `--version` in the runner, and what it reports at session start. Empty
    /// means the version this node pins — never "whatever the image happens
    /// to contain", since the same string is what the bundle carries and what
    /// both checks compare against.
    pub version: String,
    /// Harnesses outside the ones every node runs that this node also runs
    /// (`adapter::EXPERIMENTAL`; none today). Off by default.
    #[serde(default)]
    pub experimental: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Boundary {
    /// The podman binary. Empty: resolved from PATH, then well-known install
    /// locations — a node launched from a desktop launcher inherits a minimal
    /// PATH.
    pub podman: String,
    /// The name a harness reaches the node's gateways by: in a Podman
    /// sandbox, a hosts entry for its own loopback, where the tunnels listen.
    /// Once also the gateway container's name, which an upgraded node
    /// removes under it (`boundary::podman::setup::retire_gateway`).
    pub gateway_container: String,
    /// Whether an image-only service's sidecar container, which shares the
    /// session sandbox's network namespace, runs with `label=disable` on an
    /// SELinux host. Unset: whenever SELinux is enforcing.
    pub selinux_label_disable: Option<bool>,
    /// Seconds between the SIGTERM a stopped harness container's init receives
    /// and the SIGKILL that follows. It bounds how long a stop can take, not
    /// how thorough it is: the container's PID namespace goes either way, and
    /// with it every process the harness started. The same
    /// number is the pod's `terminationGracePeriodSeconds`.
    pub stop_timeout_secs: u16,
    /// Bring the harness bundle and the stock base in step with this binary
    /// by itself: when the node starts, and when what it fetches changes
    /// under it. Off, they change only when `tracon setup` runs, and the
    /// boundary check refuses until it has.
    pub auto_reconcile: bool,
    /// What every runtime volume and container this node names begins with,
    /// before a `-`: the shared harness state, the boundary probes, each
    /// session's, check's and preparation's own. A second node on the same
    /// container host sets its own, so it neither mounts the first's harness
    /// state nor counts, probes or sweeps the first's resources. Read once as
    /// the node starts; changing it later strands what the old one named.
    pub resource_prefix: String,
}

/// The default `[boundary] resource_prefix`, and every name a node used
/// before there was one.
pub const DEFAULT_RESOURCE_PREFIX: &str = "tracon";

static RESOURCE_PREFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// This process's `[boundary] resource_prefix`: the one its node
/// configuration was first loaded with. Names are made far from any `Config`
/// (a workspace's volume, a check's container), and a process runs one
/// node, so it is held once rather than passed everywhere.
pub fn resource_prefix() -> &'static str {
    RESOURCE_PREFIX
        .get()
        .map(String::as_str)
        .unwrap_or(DEFAULT_RESOURCE_PREFIX)
}

/// A runtime resource name of this node's: `<prefix>-<rest>`.
pub fn resource_name(rest: &str) -> String {
    format!("{}-{rest}", resource_prefix())
}

/// A prefix is a name's first segment: lowercase letters, digits and inner
/// `-`, as every container and volume name accepts. Another node's must not
/// begin with the default's `tracon-`, or the default node would read the
/// other's names as its own.
pub fn validate_resource_prefix(prefix: &str) -> Result<(), String> {
    let well_formed = !prefix.is_empty()
        && prefix.len() <= 32
        && prefix.starts_with(|c: char| c.is_ascii_lowercase())
        && !prefix.ends_with('-')
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !well_formed {
        return Err(format!(
            "[boundary] resource_prefix {prefix:?} must be 1-32 lowercase letters, digits and inner '-', starting with a letter"
        ));
    }
    if prefix != DEFAULT_RESOURCE_PREFIX
        && prefix.starts_with(&format!("{DEFAULT_RESOURCE_PREFIX}-"))
    {
        return Err(format!(
            "[boundary] resource_prefix {prefix:?} must not begin with \"{DEFAULT_RESOURCE_PREFIX}-\": a node with the default would take its resources for its own"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Gateway {
    /// Hosts the harness may CONNECT to, as anchored regexes for tinyproxy's
    /// filter. Everything else is denied.
    pub allow_hosts: Vec<String>,
    pub proxy_port: u16,
    /// The port, on the gateway's internal address, where a container
    /// reaches the node's per-client egress proxy (`Backend::egress_grant`):
    /// a dependency preparation, or a session whose repository opens
    /// registries to it. Each presents its own credentials and is filtered by
    /// its own grant, so nothing one client was granted is reachable from
    /// another. The name is from when only the retired QA browser used it.
    pub qa_proxy_port: u16,
    /// Port the gateway forwards from the internal network to the node.
    pub forward_port: u16,
    /// Where the node serves that egress proxy when the harness listener is a
    /// TCP address (a Podman machine): the same loopback address, this port.
    /// On a Linux host it is a socket beside the harness socket instead.
    pub egress_port: u16,
    /// Where the node listens for the harness. Loopback: the gateway reaches it
    /// through the Podman machine's host route, and nothing else can.
    /// Where the node listens for the gateway's forward. A socket address on
    /// a Podman machine (the VM reaches the host's loopback); an absolute path
    /// to a Unix socket on a Linux host, where `host.containers.internal` is
    /// not loopback and a TCP listener would have to face the LAN.
    pub harness_listen: HarnessListen,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    /// When the nightly promotion batch is built, `HH:MM` (UTC, or offset by
    /// `TRACON_TZ_OFFSET_MINUTES`), for channels this node processes.
    pub promote_at: String,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            promote_at: "02:00".into(),
        }
    }
}

/// How the work ledger reads an item's priority. The ledger stores an
/// integer and runs higher first; these name the values people pick from.
/// Written out, the list replaces the default whole; an empty list names
/// nothing and priorities are plain numbers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Work {
    /// `[[work.priority]]`: the named levels.
    pub priority: Vec<PriorityLevel>,
    /// What a new item gets when nobody says.
    pub default_priority: i64,
}

/// One named priority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityLevel {
    pub name: String,
    pub value: i64,
}

impl Default for Work {
    fn default() -> Self {
        let level = |name: &str, value| PriorityLevel {
            name: name.into(),
            value,
        };
        Self {
            priority: vec![
                level("Urgent", 4),
                level("Priority", 3),
                level("Planned", 2),
                level("Backlog", 1),
            ],
            default_priority: 2,
        }
    }
}

impl Work {
    /// The levels, highest first.
    pub fn levels(&self) -> Vec<PriorityLevel> {
        let mut levels = self.priority.clone();
        levels.sort_by_key(|l| std::cmp::Reverse(l.value));
        levels
    }

    /// The name of a priority, when a level has exactly that value.
    pub fn priority_name(&self, value: i64) -> Option<&str> {
        self.priority
            .iter()
            .find(|l| l.value == value)
            .map(|l| l.name.as_str())
    }

    /// A priority as someone wrote it: a level's name (any case) or a number.
    pub fn parse_priority(&self, text: &str) -> Result<i64, String> {
        let text = text.trim();
        if let Some(level) = self
            .priority
            .iter()
            .find(|l| l.name.eq_ignore_ascii_case(text))
        {
            return Ok(level.value);
        }
        text.parse().map_err(|_| {
            let levels = self.levels();
            let names: Vec<&str> = levels.iter().map(|l| l.name.as_str()).collect();
            if names.is_empty() {
                format!("priority {text:?} is not a number")
            } else {
                format!(
                    "priority {text:?} is not a number or one of {}",
                    names.join(", ")
                )
            }
        })
    }
}

/// Refuse levels a picker could not tell apart: an empty name, or a name or
/// value given twice.
pub fn validate_priorities(levels: &[PriorityLevel]) -> Result<(), String> {
    let mut names = std::collections::BTreeSet::new();
    let mut values = std::collections::BTreeSet::new();
    for level in levels {
        let name = level.name.trim();
        if name.is_empty() {
            return Err("work.priority: a level has no name".into());
        }
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(format!("work.priority: {name:?} is given twice"));
        }
        if !values.insert(level.value) {
            return Err(format!(
                "work.priority: value {} is given to two levels",
                level.value
            ));
        }
    }
    Ok(())
}

/// Request shapes the model gateway knows how to inject a credential into.
pub const SHAPE_ANTHROPIC: &str = "anthropic";
pub const SHAPE_OPENAI: &str = "openai";
pub const SHAPE_OPENAI_CODEX: &str = "openai-codex";

/// One model provider the gateway fronts. The harness reaches it at
/// `/model/<name>/…`; the node injects `credential` and forwards to
/// `upstream`, which must also pass the egress allowlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Provider {
    /// The broker credential injected for it (kind `api_key` or `oauth`).
    pub credential: String,
    pub upstream: String,
    /// `anthropic`, `openai`, or `openai-codex`: which headers and paths the credential becomes.
    pub shape: String,
    /// The subscription sign-in the node runs for it (`anthropic`, or
    /// `openai` for ChatGPT/Codex; see `oauth::Flow`); none means API key
    /// only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// What a token costs through this provider, when the credential is
    /// metered. Absent means a subscription: tokens are counted, dollars are
    /// not derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    /// The models this node declares under this provider, for a harness that
    /// is told its catalogue rather than asked for one (tracon-agent; Claude
    /// Code's picker too, for the `anthropic`-shaped provider). A built-in provider written
    /// without any gets `default_models` for its name on load, so a node that
    /// names no models still has a catalogue; write `models = []` to declare
    /// none on purpose.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDecl>,
}

/// One model this node declares under a provider. The picker offers exactly
/// what is written here and the gateway serves exactly that, which is the
/// whole point of declaring rather than probing: there is no catalogue to
/// subtract a denylist from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelDecl {
    /// The provider's own model id, as the request carries it.
    pub id: String,
    /// What a person reads in the picker. Empty: the id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Context window, in tokens. Zero: the harness's own default.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context: u64,
    /// Maximum output, in tokens. Zero: the harness's own default.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub output: u64,
    /// Whether the model reasons, so a harness shows thinking rather than
    /// discarding it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reasoning: bool,
    /// Whether it accepts attachments.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub attachment: bool,
    /// Whether the model may make several tool calls in one response, which
    /// is what lets subagents run together. Read by the native harness; on
    /// unless turned off, and off, it is asked for one call at a time.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub parallel_tools: bool,
    /// A reasoning model's effort (`minimal`, `low`, `medium`, `high`,
    /// `xhigh`), sent by the native harness on the Responses wire. Absent:
    /// the provider's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// The reasoning efforts a model declaration may name.
pub const REASONING_EFFORTS: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

impl Default for ModelDecl {
    fn default() -> Self {
        ModelDecl {
            id: String::new(),
            name: String::new(),
            context: 0,
            output: 0,
            reasoning: false,
            attachment: false,
            parallel_tools: true,
            reasoning_effort: None,
        }
    }
}

fn on() -> bool {
    true
}

fn is_on(v: &bool) -> bool {
    *v
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

impl ModelDecl {
    /// What the picker shows for this model.
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

/// Dollars per million tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

impl Price {
    pub fn cost(&self, input_tokens: i64, output_tokens: i64) -> f64 {
        (input_tokens as f64 * self.input_per_mtok + output_tokens as f64 * self.output_per_mtok)
            / 1_000_000.0
    }
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            credential: String::new(),
            upstream: String::new(),
            shape: SHAPE_OPENAI.into(),
            login: None,
            price: None,
            models: Vec::new(),
        }
    }
}

/// The provider names a `node.toml` text writes under `[providers]`.
fn declared_providers(text: &str) -> std::collections::BTreeSet<String> {
    toml::from_str::<toml::Table>(text)
        .ok()
        .and_then(|mut doc| match doc.remove("providers") {
            Some(toml::Value::Table(providers)) => {
                Some(providers.into_iter().map(|(name, _)| name).collect())
            }
            _ => None,
        })
        .unwrap_or_default()
}

pub fn default_providers() -> std::collections::BTreeMap<String, Provider> {
    [
        (
            "anthropic",
            Provider {
                credential: "anthropic".into(),
                upstream: "https://api.anthropic.com".into(),
                shape: SHAPE_ANTHROPIC.into(),
                login: Some("anthropic".into()),
                price: None,
                models: default_models("anthropic"),
            },
        ),
        (
            "openai",
            Provider {
                credential: "openai".into(),
                upstream: "https://api.openai.com".into(),
                shape: SHAPE_OPENAI.into(),
                login: None,
                price: None,
                models: default_models("openai"),
            },
        ),
        (
            "openai-codex",
            Provider {
                credential: "openai-codex".into(),
                upstream: "https://chatgpt.com/backend-api".into(),
                shape: SHAPE_OPENAI_CODEX.into(),
                login: Some("openai".into()),
                price: None,
                models: default_models("openai-codex"),
            },
        ),
    ]
    .into_iter()
    .map(|(n, p)| (n.to_string(), p))
    .collect()
}

/// The models a provider declares when the operator names none, so a fresh
/// node — or a provider the operator just added — has a catalogue the moment
/// it is connected: the current generation for `anthropic`, `openai` and
/// `openai-codex`, and nothing for any other provider, whose models the
/// operator declares. A declaring harness serves exactly this list. The
/// limits are what the harness is told, not what is enforced.
pub fn default_models(provider: &str) -> Vec<ModelDecl> {
    hardcoded_default_models(provider)
}

/// A model's reasoning effort is one the providers know.
fn validate_models(providers: &std::collections::BTreeMap<String, Provider>) -> Result<(), String> {
    for (name, provider) in providers {
        for m in &provider.models {
            if let Some(e) = &m.reasoning_effort {
                if !REASONING_EFFORTS.contains(&e.as_str()) {
                    return Err(format!(
                        "providers.{name}: model `{}` has reasoning_effort `{e}`; it is one of {}",
                        m.id,
                        REASONING_EFFORTS.join(", ")
                    ));
                }
            }
        }
    }
    Ok(())
}

fn hardcoded_default_models(provider: &str) -> Vec<ModelDecl> {
    let model = |id: &str, name: &str, context: u64, output: u64, attachment: bool| ModelDecl {
        id: id.into(),
        name: name.into(),
        context,
        output,
        reasoning: true,
        attachment,
        ..Default::default()
    };
    match provider {
        "anthropic" => vec![
            model("claude-opus-5", "Claude Opus 5", 200_000, 64_000, true),
            model("claude-sonnet-5", "Claude Sonnet 5", 200_000, 64_000, true),
            model(
                "claude-haiku-4-5",
                "Claude Haiku 4.5",
                200_000,
                64_000,
                true,
            ),
        ],
        "openai" => vec![
            model("gpt-5.5", "GPT-5.5", 400_000, 128_000, false),
            model("gpt-5.5-codex", "GPT-5.5 Codex", 400_000, 128_000, false),
        ],
        // A ChatGPT sign-in is refused the `-codex` models ("not supported
        // when using Codex with a ChatGPT account"), so the subscription's
        // fallback offers only what it will actually serve.
        "openai-codex" => vec![model("gpt-5.5", "GPT-5.5", 400_000, 128_000, false)],
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionDefaults {
    /// Per-session cap; zero means no token cap.
    pub budget_tokens: i64,
    pub permission_timeout_secs: u64,
    /// How long a brokered call held for the operator waits before it
    /// expires unanswered. Nothing is blocked while it waits, so this can be
    /// long.
    pub approval_expiry_secs: u64,
    /// How long a call the API gateway forwards to a session's harness may
    /// take before its outcome is recorded as unknown. A mediated mutation
    /// that outlives this is neither sent nor not-sent: the intent is on the
    /// record and reconciliation asks the harness what actually happened.
    pub harness_api_timeout_secs: u64,
    /// The channel the composer starts on when the client has not chosen one
    /// itself. Empty means no preference.
    pub default_channel: String,
    /// How long a claim survives a client that stopped talking. A dropped socket
    /// should not zero the attention count; a closed laptop should.
    pub claim_grace_secs: u64,
    /// How long a session whose work is published may sit idle before it is
    /// suspended: harness and container stopped, egress grant revoked,
    /// workspace kept. 0 never suspends one.
    pub suspend_published_after_secs: u64,
    /// Where worktrees are created. Outside any repo, so nothing is gitignored.
    pub worktree_root: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node_name: hostname(),
            mesh: Mesh::default(),
            runtime: Runtime::default(),
            repo: Vec::new(),
            live_repo: LiveRepos::default(),
            live: ConfigHandle::default(),
            service: Vec::new(),
            mcp_server: Vec::new(),
            providers: default_providers(),
            declared_providers: Default::default(),
            memory: Memory::default(),
            supervision: Supervision::default(),
            review: ReviewLimits::default(),
            notify: Notify::default(),
            auth: Auth::default(),
            embed: Embed::default(),
            external: External::default(),
            docs: Docs::default(),
            work: Work::default(),
            upstreams: Upstreams::default(),
            placement: Default::default(),
            metrics: Metrics::default(),
            sandbox: Sandbox::default(),
            digitalocean: Default::default(),
            retired: RetiredNames::default(),
            harness: Harness {
                id: crate::adapter::claude::ClaudeAdapter::ID.into(),
                version: crate::adapter::claude::ClaudeAdapter::PINNED_VERSION.into(),
                // Empty: see the field's note. The surface a session actually
                // runs against is bounded by the boundary and by policy, both
                // of which hold whatever the harness offers.
                tools: Vec::new(),
                experimental: Vec::new(),
            },
            boundary: Boundary {
                podman: String::new(),
                gateway_container: "tracon-gw".into(),
                selinux_label_disable: None,
                stop_timeout_secs: 10,
                auto_reconcile: true,
                resource_prefix: DEFAULT_RESOURCE_PREFIX.into(),
            },
            gateway: Gateway {
                allow_hosts: vec![
                    r"^api\.anthropic\.com$".into(),
                    r"^api\.openai\.com$".into(),
                    r"^chatgpt\.com$".into(),
                    r"^auth\.openai\.com$".into(),
                ],
                proxy_port: 8888,
                qa_proxy_port: 8890,
                forward_port: 7421,
                egress_port: 7424,
                harness_listen: HarnessListen::default(),
            },
            github: Github::default(),
            consulta: Consulta {
                command: "uv".into(),
                args: vec![
                    "run".into(),
                    "--project".into(),
                    dirs::home_dir()
                        .unwrap_or_default()
                        .join("src/consulta")
                        .to_string_lossy()
                        .into_owned(),
                    "consulta".into(),
                ],
                timeout_secs: 60,
            },
            publish: Publish {
                gh: "gh".into(),
                glab: "glab".into(),
                git: "git".into(),
                forge_timeout_secs: 15,
                commits: Commits::Squash,
                style: Default::default(),
            },
            session: SessionDefaults {
                budget_tokens: 0,
                permission_timeout_secs: 900,
                approval_expiry_secs: 86_400,
                harness_api_timeout_secs: 30,
                default_channel: String::new(),
                claim_grace_secs: 60,
                suspend_published_after_secs: 1800,
                worktree_root: default_worktree_root(),
            },
        }
    }
}

/// Where worktrees go by default. On macOS `/private/tmp` is the real temp dir
/// and survives across sessions; on a Linux node it usually does not exist for a
/// non-root user, so fall back to the platform temp dir there.
fn default_worktree_root() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/private/tmp")
    } else {
        std::env::temp_dir()
    }
}

impl Default for Harness {
    fn default() -> Self {
        Config::default().harness
    }
}
impl Default for Boundary {
    fn default() -> Self {
        Config::default().boundary
    }
}
impl Default for Gateway {
    fn default() -> Self {
        Config::default().gateway
    }
}
impl Default for SessionDefaults {
    fn default() -> Self {
        Config::default().session
    }
}
impl Default for Publish {
    fn default() -> Self {
        Config::default().publish
    }
}
impl Default for Consulta {
    fn default() -> Self {
        Config::default().consulta
    }
}

#[cfg(unix)]
fn uid() -> u32 {
    // No libc dependency: the runtime dir fallback only needs a stable per-user
    // suffix, and the environment carries it on every session manager.
    std::env::var("UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

/// One throwaway directory per test process, so the unit tests share a state
/// directory with each other and with nothing else.
#[cfg(test)]
fn test_state_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tracon-test-state-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Never called: `state_dir` only reaches for it under `cfg(test)`, and this
/// exists so the non-test build still compiles the branch away cleanly.
#[cfg(not(test))]
fn test_state_dir() -> PathBuf {
    unreachable!()
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "node".into())
}

impl Config {
    /// The image a harness runs in on a pod-hosted node: the configured image
    /// for the configured harness, the release's own image for the other.
    /// Over the sandbox protocol there are no harness images: every harness
    /// runs from the bundle on the stock base, as on Podman and Apple.
    pub fn kubernetes_harness_image(&self, harness_id: &str) -> String {
        if crate::boundary::kubernetes::uses_sandbox_protocol(self) {
            return crate::sandbox_images::base_image(self);
        }
        if harness_id == self.harness.id {
            return self.runtime.kubernetes.harness_image.clone();
        }
        format!(
            "ghcr.io/cosmicspork/tracon-harness-{harness_id}:{}",
            env!("CARGO_PKG_VERSION")
        )
    }

    /// Where `node.toml` lives.
    ///
    /// Guarded the way `state_dir` is, and for the same reason: the interface
    /// writes this file now, so a test that exercises that code path would
    /// rewrite the operator's real configuration. `TRACON_CONFIG_DIR`
    /// overrides it — integration tests use it, since they link the library
    /// without `cfg(test)`.
    pub fn config_path() -> PathBuf {
        Self::config_dir_from(std::env::var_os("TRACON_CONFIG_DIR")).join("node.toml")
    }

    /// `config_path`'s directory with the override handed in, so both
    /// branches are testable without touching the process environment.
    fn config_dir_from(override_dir: Option<std::ffi::OsString>) -> PathBuf {
        if let Some(dir) = override_dir {
            return PathBuf::from(dir);
        }
        if cfg!(test) {
            return test_state_dir();
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tracon")
    }

    /// State the node owns: database, gateway allowlist, per-session scratch,
    /// and — the reason this is guarded — the sealed credential store.
    ///
    /// Under `cargo test` this is a throwaway directory, always. It is not a
    /// convenience: the credential store, the provider login stores and the
    /// policy bundle all live here at a path derived from the environment, so
    /// a test that exercises the code that writes them wrote them *for real*.
    /// Running the suite on a machine that also runs a node replaced that
    /// node's credential store with one sealed under a test key, and deleted
    /// its provider logins. Nothing in a test can reach the operator's state
    /// now, whatever it calls.
    ///
    /// `TRACON_STATE_DIR` overrides it outright, which integration tests use
    /// (they link the library without `cfg(test)`) and which also makes a
    /// scratch node a one-liner.
    pub fn state_dir() -> PathBuf {
        Self::state_dir_from(std::env::var_os("TRACON_STATE_DIR"))
    }

    /// `state_dir` with the override handed in, so a test can assert both
    /// branches without touching the process-global environment that every
    /// other test in the binary is reading.
    fn state_dir_from(override_dir: Option<std::ffi::OsString>) -> PathBuf {
        if let Some(dir) = override_dir {
            return PathBuf::from(dir);
        }
        if cfg!(test) {
            return test_state_dir();
        }
        dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tracon")
    }

    pub fn db_path() -> PathBuf {
        Self::state_dir().join("node.db")
    }

    /// Short-lived runtime state: the harness socket. Under `$XDG_RUNTIME_DIR`
    /// to stay below the Unix socket path limit and vanish at logout.
    pub fn runtime_dir() -> PathBuf {
        dirs::runtime_dir()
            .unwrap_or_else(|| std::env::temp_dir().join(format!("tracon-{}", uid())))
            .join("tracon")
    }

    /// Where the Podman sandbox provider makes each sandbox's socket
    /// directory: beside the harness socket when that is a unix socket (so a
    /// scratch node with its own keeps its own), else under the runtime
    /// directory. Short either way, for the socket path limit.
    pub fn sandbox_socket_dir(&self) -> PathBuf {
        match &self.gateway.harness_listen {
            HarnessListen::Unix(path) => path
                .parent()
                .map(|dir| dir.join("sandbox"))
                .unwrap_or_else(|| Self::runtime_dir().join("sandbox")),
            HarnessListen::Tcp(_) => Self::runtime_dir().join("sandbox"),
        }
    }

    /// The `[[repo]]` table as it stands now: what the operator last saved
    /// through the running node, else what the file held at load.
    pub fn repos(&self) -> std::sync::Arc<Vec<Repo>> {
        match &*self.live_repo.0.read() {
            Some(live) => live.clone(),
            None => std::sync::Arc::new(self.repo.clone()),
        }
    }

    /// Where the node serves the allowlist proxy (`[gateway] allow_hosts`)
    /// for sandboxes that reach it through a tunnel rather than through the
    /// gateway container's tinyproxy: a socket beside the harness socket, so
    /// a scratch node keeps its own.
    pub fn allow_proxy_socket(&self) -> PathBuf {
        match &self.gateway.harness_listen {
            HarnessListen::Unix(path) => path.with_file_name("allow.sock"),
            HarnessListen::Tcp(_) => Self::runtime_dir().join("allow.sock"),
        }
    }

    /// Where the node serves the per-client egress proxy for the gateway to
    /// forward to: beside the harness listener, in whichever form that takes.
    pub fn egress_listen(&self) -> HarnessListen {
        match &self.gateway.harness_listen {
            HarnessListen::Tcp(addr) => HarnessListen::Tcp(std::net::SocketAddr::new(
                addr.ip(),
                self.gateway.egress_port,
            )),
            HarnessListen::Unix(path) => HarnessListen::Unix(path.with_file_name("egress.sock")),
        }
    }

    /// The harness's own state directory, node-owned. Only the harness's
    /// credential store is mounted into it; nothing from the operator's own
    /// copy of that harness on this host ever leaks in.
    pub fn harness_state_dir() -> PathBuf {
        Self::state_dir().join("harness-state")
    }

    /// The origin clients reach this node at from elsewhere, normalised:
    /// `[auth] public_origin`. The push subject falls back to it. `None` when
    /// unset; a value that does not validate stops `try_load`, so a running
    /// node never holds one.
    pub fn public_origin(&self) -> Option<String> {
        self.auth.public_origin().ok().flatten()
    }

    /// This node's configuration, which also fixes the process's
    /// [`resource_prefix`]. A missing file is the defaults; one that is there
    /// and does not load is an error with the reason, never the defaults in
    /// its place: a one-shot command would otherwise act as a node other
    /// than this one (another name, no hub, another runtime).
    pub fn load() -> Result<Self, String> {
        Self::try_load()
    }

    /// The first configuration this process loads as its own names its
    /// runtime resources from then on.
    fn claim_resource_prefix(&self) {
        let _ = RESOURCE_PREFIX.set(self.boundary.resource_prefix.clone());
    }

    /// The defaults a node on this host runs with no `node.toml`: those of
    /// [`Config::default`], on this host's runtime kind.
    pub fn for_host() -> Self {
        let mut config = Self::default();
        config.runtime.kind = RuntimeKind::for_host();
        config
    }

    /// Strict load for `serve`, `setup` and `check-boundary`: a file that
    /// exists but does not parse is an error, so a typo cannot silently drop
    /// the whole configuration, or have the boundary set up on defaults that
    /// differ from what the file says (`setup` on default tunnels retires a
    /// gateway the file keeps).
    pub fn try_load() -> Result<Self, String> {
        let config = Self::try_load_from(&Self::config_path())?;
        config.claim_resource_prefix();
        Ok(config)
    }

    pub fn try_load_from(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                // Before parsing into the struct: the retired harness is a
                // value the struct accepts and the node then refuses, and
                // every load — `serve`'s and `setup`'s alike — should see the
                // file the node can run.
                let text = match crate::legacy::migrate_config(&text) {
                    Some(migration) => {
                        crate::legacy::apply_config_migration(path, &migration);
                        migration.text
                    }
                    None => text,
                };
                Self::from_toml_with(&text, path, HostRules::here())
            }
            // No file is a node on its defaults; one that is there and
            // cannot be read is as wrong as one that does not parse.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::for_host()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// A `node.toml` text with every check a load makes, and no host's
    /// rules ([`HostRules::NEUTRAL`]); `path` is only for the message.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self, String> {
        Self::from_toml_with(text, path, HostRules::NEUTRAL)
    }

    /// A `node.toml` text as a node on a host with `host`'s rules loads it,
    /// with every check a load makes; `path` is only for the message. What
    /// `config::edit` holds a result to before it writes it.
    pub fn from_toml_with(text: &str, path: &Path, host: HostRules) -> Result<Self, String> {
        let mut config: Self =
            toml::from_str(text).map_err(|error| format!("{}: {error}", path.display()))?;
        // From the text, not the struct: a file with no
        // `[providers]` at all deserializes to the defaults whole.
        config.declared_providers = declared_providers(text);
        if let Some(kind) = host.default_kind.filter(|_| !names_runtime_kind(text)) {
            config.runtime.kind = kind;
        }
        validate_runtime_kind(config.runtime.kind, host.mac)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        for (name, provider) in default_providers() {
            config.providers.entry(name).or_insert(provider);
        }
        config.review.lift_retired_defaults();
        config.retired = RetiredNames::from_text(text);
        refuse_retired_podman_switches(text, config.runtime.kind)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        for key in ignored_podman_switches(text, config.runtime.kind) {
            tracing::warn!(
                "{}: [runtime.podman] {key} is ignored; Podman always runs that way now, so \
                 the line can go",
                path.display()
            );
        }
        for key in retired_boundary_keys(text) {
            tracing::warn!(
                "{}: {key} configured the gateway container or the harness images, which \
                 were removed; it only names what an upgraded node removes, and can go once \
                 `tracon setup` has run",
                path.display()
            );
        }
        for section in retired_opencode_sections(text) {
            tracing::warn!(
                "{}: [{section}] is ignored; it configured the OpenCode harness, which was removed",
                path.display()
            );
        }
        if let Some(section) = retired_qa_section(text) {
            tracing::warn!(
                "{}: [qa] is ignored; candidate QA deploys, browser runs and prototype builds were removed ({section})",
                path.display()
            );
        }
        // A built-in provider the operator wrote without models is
        // not one with none: the section is usually there to set a
        // credential name or an upstream, and an empty catalogue
        // behind it starts no session. `models = []` is preserved as
        // written only where the operator's file says so.
        for (name, provider) in config.providers.iter_mut() {
            if provider.models.is_empty() {
                provider.models = default_models(name);
            }
        }
        config
            .docs
            .preview_origin()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        config
            .auth
            .public_origin()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        config
            .auth
            .trusted_proxies()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        validate_repos(&config.repo).map_err(|error| format!("{}: {error}", path.display()))?;
        crate::provider::requirements::validate(&config.placement)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        crate::provider::digitalocean::validate(
            &config.digitalocean,
            &config.mesh,
            &crate::provider::digitalocean::SourceBuilder::of(&config),
        )
        .map_err(|error| format!("{}: {error}", path.display()))?;
        validate_services(&config.service)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        validate_mcp_servers(&config.mcp_server)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        validate_doc_kinds(&config.docs.kind)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        validate_metrics(&config).map_err(|error| format!("{}: {error}", path.display()))?;
        validate_sandbox(&config).map_err(|error| format!("{}: {error}", path.display()))?;
        validate_repo_builds(&config).map_err(|error| format!("{}: {error}", path.display()))?;
        validate_priorities(&config.work.priority)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        upstreams::validate_upstreams(&config.upstreams)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        validate_admin_peers(&config.mesh.admin_peers)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        validate_resource_prefix(&config.boundary.resource_prefix)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        validate_models(&config.providers)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        config
            .runtime
            .kubernetes
            .validate()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if let Some(contact) = config.notify.contact.as_deref() {
            validate_contact(contact).map_err(|error| format!("{}: {error}", path.display()))?;
        }
        // What `serve` refuses to start with, refused here too, so no edit
        // writes a harness the node has no adapter for.
        crate::adapter::adapter_for(&config)
            .map_err(|error| format!("{}: [harness] id: {error}", path.display()))?;
        Ok(config)
    }
}

/// The table `value` serializes as, less every key whose value is what
/// `T::default()` has there: an entry written this way says what the operator
/// chose, and loads back the same.
pub fn without_defaults<T: Serialize + Default>(value: &T) -> Result<toml::Table, String> {
    let mut table = toml::Table::try_from(value).map_err(|e| e.to_string())?;
    let defaults = toml::Table::try_from(T::default()).map_err(|e| e.to_string())?;
    table.retain(|key, value| defaults.get(key) != Some(value));
    Ok(table)
}

/// Replace `path` by renaming a finished file over it, so a reader never sees
/// it truncated or half-written: `try_load` does not hold the edit lock, and an
/// empty file parses as the defaults. A symlinked `node.toml` keeps its link,
/// and the file keeps its mode.
fn write_replacing(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let tmp = target.with_extension(format!("toml.{}.tmp", uuid::Uuid::now_v7()));
    let written = (|| {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        if let Ok(meta) = std::fs::metadata(&target) {
            file.set_permissions(meta.permissions())?;
        }
        // On disk before it is named, so a crash leaves the old file or the
        // whole new one, never an empty one.
        file.sync_all()?;
        std::fs::rename(&tmp, &target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return written;
    }
    // And the rename itself, so the new name survives a crash too. Past the
    // rename the file has changed, so a failure here is not a failed write:
    // it is said, and the write stands.
    #[cfg(unix)]
    if let Some(dir) = target.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        if let Err(error) = std::fs::File::open(dir).and_then(|dir| dir.sync_all()) {
            tracing::warn!(%error, dir = %dir.display(), "node.toml was replaced, but its directory could not be synced");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_resource_prefix_names_a_second_node_s_resources_apart() {
        assert_eq!(Config::default().boundary.resource_prefix, "tracon");
        for good in ["tracon", "spike-s", "dev2", "tracon2"] {
            assert!(validate_resource_prefix(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "Spike",
            "spike-",
            "-spike",
            "2dev",
            "spike_s",
            "spike/s",
            "tracon-dev",
            "tracon-",
        ] {
            assert!(validate_resource_prefix(bad).is_err(), "{bad:?}");
        }
        let cfg = Config::from_toml(
            "[boundary]\nresource_prefix = \"spike-s\"\n",
            Path::new("node.toml"),
        )
        .unwrap();
        assert_eq!(cfg.boundary.resource_prefix, "spike-s");
        let error = Config::from_toml(
            "[boundary]\nresource_prefix = \"tracon-dev\"\n",
            Path::new("node.toml"),
        )
        .unwrap_err();
        assert!(error.contains("resource_prefix"), "{error}");
    }

    #[test]
    fn admin_peers_name_nodes_by_their_full_id() {
        let id = "ab".repeat(32);
        assert!(validate_admin_peers(std::slice::from_ref(&id)).is_ok());
        for bad in [
            "AB".repeat(32),
            "ab".repeat(8),
            format!("{id} "),
            "zz".repeat(32),
        ] {
            assert!(
                validate_admin_peers(std::slice::from_ref(&bad)).is_err(),
                "{bad}"
            );
        }
        let text = format!("[mesh]\nadmin_peers = [\"{}\"]\n", "AB".repeat(32));
        assert!(Config::from_toml(&text, Path::new("node.toml"))
            .unwrap_err()
            .contains("admin_peers"));
        let text = format!("[mesh]\nadmin_peers = [\"{id}\"]\n");
        let cfg = Config::from_toml(&text, Path::new("node.toml")).unwrap();
        assert!(cfg.mesh.is_admin_peer(&id));
        assert!(!cfg.mesh.is_admin_peer(&"cd".repeat(32)));
    }

    /// The guard that stands between `cargo test` and the operator's sealed
    /// credential store, and the override integration tests use because they
    /// link the library without `cfg(test)`.
    ///
    #[test]
    fn tests_never_resolve_the_operators_state_directory() {
        let real = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_default()
            .join("tracon");

        // Never `remove_var`/`set_var` here: the other tests in this binary
        // resolve the state dir concurrently and would read the probe value.
        let dir = Config::state_dir_from(None);
        assert_ne!(dir, real, "a test would write the operator's state");
        assert!(
            dir.to_string_lossy().contains("tracon-test-state"),
            "unexpected test state dir: {}",
            dir.display()
        );
        // Everything dangerous is derived from it, so nothing reaches out.
        for p in [
            crate::broker::Broker::path(),
            crate::broker::Broker::plain_path(),
            crate::policy::bundle::Paths::bundle(),
            crate::policy::bundle::Paths::signing_key(),
            Config::db_path(),
        ] {
            assert!(
                p.starts_with(&dir),
                "{} escapes the test state dir",
                p.display()
            );
        }

        let overridden = Config::state_dir_from(Some("/elsewhere/tracon-override-probe".into()));
        assert_eq!(
            overridden,
            std::path::PathBuf::from("/elsewhere/tracon-override-probe")
        );
    }
    use super::*;

    /// The scrape listener binds nothing unless asked, and never a port one
    /// of the node's other listeners has.
    #[test]
    fn a_metrics_listener_is_off_unless_given_a_port_of_its_own() {
        let load = |text: &str| Config::from_toml(text, Path::new("node.toml"));
        assert!(load("").unwrap().metrics.listen.is_none());
        let cfg = load("[metrics]\nlisten = \"0.0.0.0:7425\"\n").unwrap();
        assert_eq!(cfg.metrics.listen, Some("0.0.0.0:7425".parse().unwrap()));
        for (port, owner) in [
            (7420, "operator"),
            (7421, "forward_port"),
            (7422, "preview_listen"),
            (7424, "egress_port"),
            (8888, "proxy_port"),
            (8890, "qa_proxy_port"),
        ] {
            let error = load(&format!("[metrics]\nlisten = \"127.0.0.1:{port}\"\n")).unwrap_err();
            assert!(error.contains("metrics.listen"), "{error}");
            assert!(error.contains(owner), "{port}: {error}");
        }
        let error = load(
            "[gateway]\nharness_listen = \"127.0.0.1:7499\"\n[metrics]\nlisten = \"127.0.0.1:7499\"\n",
        )
        .unwrap_err();
        assert!(error.contains("harness_listen"), "{error}");
    }

    #[test]
    fn a_service_is_named_safely_and_pinned() {
        let pinned = "img@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let ok = Service {
            name: "browser".into(),
            image: pinned.into(),
            port: 9222,
            ready: Some("/json/version".into()),
            ..Service::default()
        };
        assert!(validate_services(std::slice::from_ref(&ok)).is_ok());
        assert_eq!(ok.timeout_secs(), Service::DEFAULT_TIMEOUT_SECS);
        for (bad, why) in [
            (
                Service {
                    name: "Browser".into(),
                    ..ok.clone()
                },
                "name",
            ),
            (
                Service {
                    name: "a b".into(),
                    ..ok.clone()
                },
                "name",
            ),
            (
                Service {
                    image: "chrome:latest".into(),
                    ..ok.clone()
                },
                "pinned",
            ),
            (
                Service {
                    port: 0,
                    ..ok.clone()
                },
                "port",
            ),
            (
                Service {
                    ready: Some("json".into()),
                    ..ok.clone()
                },
                "ready",
            ),
        ] {
            let err = validate_services(&[bad]).unwrap_err();
            assert!(err.contains(why), "{err}");
        }
        assert!(validate_services(&[ok.clone(), ok.clone()])
            .unwrap_err()
            .contains("twice"));

        // A bundle in place of an image, run in the sandbox.
        let bundled = Service {
            image: String::new(),
            bundle: Some("/srv/browser/157/amd64".into()),
            ..ok.clone()
        };
        assert!(validate_services(std::slice::from_ref(&bundled)).is_ok());
        assert_eq!(bundled.sandbox_argv(), ["/opt/tracon-services/browser/run"]);
        let custom = Service {
            run: vec!["/opt/tracon-services/browser/bin/chrome".into()],
            command: vec!["--headless".into()],
            ..bundled.clone()
        };
        assert_eq!(
            custom.sandbox_argv(),
            ["/opt/tracon-services/browser/bin/chrome", "--headless"]
        );
        for (bad, why) in [
            (
                Service {
                    bundle: None,
                    ..bundled.clone()
                },
                "an image or a bundle",
            ),
            (
                Service {
                    bundle: Some("srv/browser".into()),
                    ..bundled.clone()
                },
                "absolute",
            ),
            (
                Service {
                    name: "postgres".into(),
                    bundle: Some("release".into()),
                    ..bundled.clone()
                },
                "only a service named browser",
            ),
            (
                Service {
                    restart: ServiceRestart::Always,
                    ..ok.clone()
                },
                "apply to a bundle",
            ),
            (
                Service {
                    env: [("A=B".to_string(), "c".to_string())].into(),
                    ..bundled.clone()
                },
                "env name",
            ),
        ] {
            let err = validate_services(&[bad]).unwrap_err();
            assert!(err.contains(why), "{err}");
        }
        // The release's own browser, with no path and no image.
        let release: Service =
            toml::from_str("name = \"browser\"\nport = 9222\nbundle = \"release\"\n").unwrap();
        assert!(validate_services(std::slice::from_ref(&release)).is_ok());
        assert!(crate::service_bundle::is_release(&release));
        assert_eq!(release.sandbox_argv(), ["/opt/tracon-services/browser/run"]);
        let toml_in =
            "name = \"browser\"\nport = 9222\nbundle = \"/srv/b\"\nrestart = \"always\"\n";
        let parsed: Service = toml::from_str(toml_in).unwrap();
        assert_eq!(parsed.restart, ServiceRestart::Always);
        // Saved without the defaults it did not set.
        let saved = toml::to_string(&Service {
            restart: ServiceRestart::OnFailure,
            ..parsed
        })
        .unwrap();
        assert!(
            !saved.contains("restart") && !saved.contains("image"),
            "{saved}"
        );
    }

    /// The README's `node.toml` reference is checked, not trusted: every key it
    /// names must exist, and the values it shows as defaults must be them.
    #[test]
    fn a_mostly_deletion_diff_over_the_line_cap_is_within_it() {
        let limits = ReviewLimits::default();
        assert_eq!(limits.refusal(213, 11_925, 75), None);
    }

    #[test]
    fn a_large_addition_is_still_over_the_line_cap() {
        let limits = ReviewLimits::default();
        let reason = limits.refusal(10_001, 0, 3).unwrap();
        assert!(reason.contains("adds 10001 lines"), "{reason}");
        assert!(reason.contains("10000 added lines"), "{reason}");
        assert!(reason.contains("removed lines do not count"), "{reason}");
    }

    #[test]
    fn deletions_past_the_total_bound_are_refused_and_say_what_was_counted() {
        let limits = ReviewLimits::default();
        assert_eq!(limits.refusal(0, 100_000, 1), None);
        let reason = limits.refusal(1, 100_000, 1).unwrap();
        assert!(
            reason.contains("100001 lines (1 added, 100000 removed)"),
            "{reason}"
        );
        assert!(reason.contains("capped at 100000 lines"), "{reason}");
    }

    #[test]
    fn a_node_toml_carrying_the_retired_review_defaults_gets_the_new_ones() {
        let dir = std::env::temp_dir().join(format!("tracon-review-lift-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node.toml");
        std::fs::write(&path, "[review]\nmax_diff_lines = 800\nmax_files = 40\n").unwrap();
        let lifted = Config::try_load_from(&path).unwrap();
        assert_eq!(lifted.review.max_diff_lines, 10_000);
        assert_eq!(lifted.review.max_files, 200);
        // One the operator chose is kept, even when half of it matches.
        std::fs::write(&path, "[review]\nmax_diff_lines = 800\nmax_files = 60\n").unwrap();
        let kept = Config::try_load_from(&path).unwrap();
        assert_eq!(kept.review.max_diff_lines, 800);
        assert_eq!(kept.review.max_files, 60);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_retired_qa_section_still_loads_and_is_named() {
        let text = "[qa.targets.cloud-qa]\nprivate = true\n\n[qa.prototype]\nimage = \"x\"\n";
        toml::from_str::<Config>(text).expect("an old [qa] table must not stop the node");
        assert_eq!(
            retired_qa_section(text).as_deref(),
            Some("1 target(s) and a prototype recipe")
        );
        assert_eq!(retired_qa_section("[qa]\n"), None);
        assert_eq!(retired_qa_section("[review]\nmax_files = 1\n"), None);
    }

    /// A Mac runs Apple Containerization and nothing else: a file that names
    /// no kind runs it there, and one that names Podman is refused with the
    /// way over. Elsewhere Podman stays the default.
    #[test]
    fn a_mac_runs_apple_and_refuses_podman() {
        assert_eq!(RuntimeKind::for_host_on(true), RuntimeKind::Apple);
        assert_eq!(RuntimeKind::for_host_on(false), RuntimeKind::Podman);
        let refused = validate_runtime_kind(RuntimeKind::Podman, true).unwrap_err();
        assert!(refused.contains("kind = \"apple\""), "{refused}");
        assert!(refused.contains("container system start"), "{refused}");
        assert!(validate_runtime_kind(RuntimeKind::Apple, true).is_ok());
        assert!(validate_runtime_kind(RuntimeKind::Kubernetes, true).is_ok());
        assert!(validate_runtime_kind(RuntimeKind::Podman, false).is_ok());

        assert!(names_runtime_kind("[runtime]\nkind = \"podman\"\n"));
        assert!(!names_runtime_kind(
            "[runtime.podman]\nsandbox_init = true\n"
        ));
        assert!(!names_runtime_kind(""));

        let path = Path::new("node.toml");
        let podman = "[runtime]\nkind = \"podman\"\n";
        // A Mac's own file: no kind is Apple, Podman is refused.
        let mac = HostRules::of_host(true);
        assert_eq!(
            Config::from_toml_with("", path, mac).unwrap().runtime.kind,
            RuntimeKind::Apple
        );
        let refused = Config::from_toml_with(podman, path, mac).unwrap_err();
        assert!(refused.contains("not supported on macOS"), "{refused}");
        // Anywhere else's own file: no kind is Podman.
        let linux = HostRules::of_host(false);
        assert_eq!(
            Config::from_toml_with("", path, linux)
                .unwrap()
                .runtime
                .kind,
            RuntimeKind::Podman
        );
        assert!(Config::from_toml_with(podman, path, linux).is_ok());
        // A file read without a host's rules says only what it says.
        assert_eq!(
            Config::from_toml("", path).unwrap().runtime.kind,
            RuntimeKind::default()
        );
        assert!(Config::from_toml(podman, path).is_ok());
        // Another node's: the kind it runs where its file names none, and
        // nothing refused for this host's sake.
        let peer_mac = HostRules::peer(Some(RuntimeKind::Apple));
        assert_eq!(
            Config::from_toml_with("", path, peer_mac)
                .unwrap()
                .runtime
                .kind,
            RuntimeKind::Apple
        );
        assert_eq!(
            Config::from_toml_with(podman, path, HostRules::peer(None))
                .unwrap()
                .runtime
                .kind,
            RuntimeKind::Podman
        );

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("node.toml");
        let missing = Config::try_load_from(&file).unwrap();
        assert_eq!(missing.runtime.kind, RuntimeKind::for_host());
        // This node's own file is held to this host's rules.
        std::fs::write(&file, podman).unwrap();
        match HOST_IS_MAC {
            true => {
                let refused = Config::try_load_from(&file).unwrap_err();
                assert!(refused.contains("not supported on macOS"), "{refused}");
            }
            false => assert_eq!(
                Config::try_load_from(&file).unwrap().runtime.kind,
                RuntimeKind::Podman
            ),
        }
    }

    /// A strict load takes a missing file as the defaults, and refuses one
    /// that is there but does not parse or cannot be read, naming why.
    #[test]
    fn a_strict_load_refuses_a_file_it_cannot_parse_or_read() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("node.toml");
        assert!(Config::try_load_from(&missing).is_ok());
        std::fs::write(&missing, "[runtime.podman\nsandbox_tunnels = false\n").unwrap();
        let error = Config::try_load_from(&missing).unwrap_err();
        assert!(error.contains("node.toml"), "{error}");
        assert!(error.contains("runtime.podman"), "{error}");
        let unreadable = Config::try_load_from(dir.path()).unwrap_err();
        assert!(
            unreadable.contains(&dir.path().display().to_string()),
            "{unreadable}"
        );
    }

    /// A retired `[runtime.podman]` switch turned off is refused on a Podman
    /// node, naming every one and the way over; turned on it only asks for
    /// what always happens. Another kind's node is not refused for a table it
    /// does not read.
    #[test]
    fn a_retired_podman_switch_turned_off_is_refused_with_the_way_over() {
        let load = |text: &str| Config::from_toml(text, Path::new("node.toml"));
        assert!(load("").is_ok());
        let on = "[runtime.podman]\nsandbox_protocol = true\nsandbox_images = true\nsandbox_init = true\n";
        assert!(load(on).unwrap().runtime.podman.sandbox_init);

        let error = load("[runtime.podman]\nsandbox_tunnels = false\nsandbox_images = false\n")
            .unwrap_err();
        assert!(error.contains("node.toml"), "{error}");
        assert!(
            error
                .contains("sandbox_tunnels = false, sandbox_images = false is no longer supported"),
            "{error}"
        );
        assert!(error.contains("Delete the lines"), "{error}");
        assert!(error.contains("tracon gc --legacy"), "{error}");
        let one = load("[runtime.podman]\nsandbox_protocol = false\n").unwrap_err();
        assert!(one.contains("Delete the line and"), "{one}");

        let kube = "[runtime]\nkind = \"kubernetes\"\n[runtime.podman]\nsandbox_protocol = false\n";
        assert!(refuse_retired_podman_switches(kube, RuntimeKind::Kubernetes).is_ok());
        assert_eq!(
            retired_podman_switches(on),
            [("sandbox_protocol", true), ("sandbox_images", true)]
        );
        assert!(retired_podman_switches("[runtime.podman]\nsandbox_init = true\n").is_empty());
        assert_eq!(
            ignored_podman_switches(on, RuntimeKind::Podman),
            ["sandbox_protocol", "sandbox_images"]
        );
        assert!(ignored_podman_switches(on, RuntimeKind::Kubernetes).is_empty());
        assert!(ignored_podman_switches(on, RuntimeKind::Apple).is_empty());
    }

    /// The keys only the gateway container and harness images read still
    /// load, are named, and are what an upgraded node finds the old gateway
    /// and images by.
    #[test]
    fn retired_boundary_keys_load_are_named_and_name_what_to_remove() {
        let text = "[boundary]\nnetwork = \"scratch-int\"\nsubnet = \"10.89.7.0/24\"\n\
                    harness_image = \"localhost/scratch-harness-claude\"\nstart_machine = true\n\
                    gateway_container = \"scratch-gw\"\n[harness]\nnative_binary = \"/x\"\n";
        let cfg = Config::from_toml(text, Path::new("node.toml")).unwrap();
        assert_eq!(
            retired_boundary_keys(text),
            [
                "boundary.network",
                "boundary.subnet",
                "boundary.harness_image",
                "boundary.start_machine",
                "harness.native_binary"
            ]
        );
        assert_eq!(
            cfg.retired,
            RetiredNames {
                network: "scratch-int".into(),
                gateway_ip: DEFAULT_GATEWAY_IP.into(),
                gateway_image: DEFAULT_GATEWAY_IMAGE.into(),
                harness_image: "localhost/scratch-harness-claude".into(),
            }
        );
        assert_eq!(cfg.boundary.gateway_container, "scratch-gw");
        let fresh = Config::from_toml("", Path::new("node.toml")).unwrap();
        assert_eq!(fresh.retired, RetiredNames::default());
        assert!(retired_boundary_keys("").is_empty());
    }

    /// The OpenCode harness's own tables still load, and are named rather
    /// than silently dropped.
    #[test]
    fn retired_opencode_sections_still_load_and_are_named() {
        let text = "[ui]\nopencode_listen = \"127.0.0.1:7423\"\n\n[launch]\nplugins = [\"a@1\"]\n";
        toml::from_str::<Config>(text).expect("an old [ui] or [launch] must not stop the node");
        assert_eq!(retired_opencode_sections(text), ["ui", "launch"]);
        assert!(retired_opencode_sections("[ui]\n").is_empty());
        assert!(retired_opencode_sections("[review]\nmax_files = 1\n").is_empty());
    }

    /// A server name that could run into the separator, an address that is
    /// not one, or a credential header the node writes itself is refused at
    /// load, with the entry named.
    #[test]
    fn an_mcp_server_entry_the_node_could_not_honour_is_refused_at_load() {
        let ok = McpServer {
            name: "linear".into(),
            url: "https://mcp.linear.app/mcp".into(),
            channels: vec!["personal".into()],
            credential: Some("linear".into()),
            ..Default::default()
        };
        validate_mcp_servers(std::slice::from_ref(&ok)).unwrap();
        for (bad, says) in [
            (
                McpServer {
                    name: "my_server".into(),
                    ..ok.clone()
                },
                "name must be",
            ),
            (
                McpServer {
                    url: "ftp://x".into(),
                    ..ok.clone()
                },
                "http or https",
            ),
            (
                McpServer {
                    url: "https://u:p@x/mcp".into(),
                    ..ok.clone()
                },
                "user or password",
            ),
            (
                McpServer {
                    header: Some("mcp-session-id".into()),
                    ..ok.clone()
                },
                "not one a credential",
            ),
            (
                McpServer {
                    credential: None,
                    header: Some("x-api-key".into()),
                    ..ok.clone()
                },
                "no credential",
            ),
            (
                McpServer {
                    deny: vec![" ".into()],
                    ..ok.clone()
                },
                "empty entry",
            ),
        ] {
            let error = validate_mcp_servers(&[bad]).unwrap_err();
            assert!(error.contains(says), "{error}");
        }
        assert!(validate_mcp_servers(&[ok.clone(), ok])
            .unwrap_err()
            .contains("twice"));
        let text = "[[mcp_server]]\nname = \"linear\"\nurl = \"https://x/mcp\"\nchannels = [\"personal\"]\nallow = [\"list_*\"]\n";
        let parsed: Config = toml::from_str(text).unwrap();
        assert_eq!(parsed.mcp_server[0].allow, vec!["list_*".to_string()]);
        assert_eq!(parsed.mcp_server[0].timeout_secs(), 60);
    }

    #[test]
    fn the_readme_configuration_block_is_a_valid_node_toml() {
        let readme = include_str!("../../README.md");
        let block = readme
            .split("```toml\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("README has a toml block");
        let parsed: Config =
            toml::from_str(block).unwrap_or_else(|e| panic!("README node.toml: {e}"));
        let d = Config::default();
        assert_eq!(parsed.harness.id, d.harness.id);
        assert_eq!(parsed.harness.version, d.harness.version);
        assert_eq!(parsed.gateway.allow_hosts, d.gateway.allow_hosts);
        assert_eq!(parsed.gateway.proxy_port, d.gateway.proxy_port);
        assert_eq!(
            parsed.session.permission_timeout_secs,
            d.session.permission_timeout_secs
        );
        assert_eq!(parsed.mesh.heartbeat_secs, d.mesh.heartbeat_secs);
        assert_eq!(parsed.memory.promote_at, d.memory.promote_at);
        assert_eq!(parsed.supervision.checks, d.supervision.checks);
        assert_eq!(parsed.review.max_diff_lines, d.review.max_diff_lines);
        assert_eq!(parsed.embed.dim, d.embed.dim);
        assert_eq!(parsed.embed.batch, d.embed.batch);
        assert_eq!(
            parsed.boundary.gateway_container,
            d.boundary.gateway_container
        );
        assert_eq!(
            parsed.providers["anthropic"].upstream,
            d.providers["anthropic"].upstream
        );
    }

    /// A repository entry is matched against the repository, component by
    /// component: an absolute path is that repository's root exactly, and a
    /// relative one is a suffix, which is how a managed clone is named without
    /// writing the node's clone root into the config.
    #[test]
    fn a_repo_entry_matches_the_repository_it_names() {
        let at = |path: &str| Repo {
            path: PathBuf::from(path),
            ..Default::default()
        };
        let absolute = at("/srv/code/tracon");
        assert!(absolute.matches(Path::new("/srv/code/tracon")));
        assert!(!absolute.matches(Path::new("/srv/code/tracon-hub")));
        // A worktree under the repository is not the repository.
        assert!(!absolute.matches(Path::new("/srv/code/tracon/.tracon/worktrees/x")));

        let managed = at("github.com/owner/name");
        assert!(managed.matches(Path::new("/var/lib/tracon/repos/github.com/owner/name")));
        // A suffix is whole path components, never characters.
        assert!(!managed.matches(Path::new("/var/lib/tracon/repos/github.com/owner/name2")));
        assert!(!managed.matches(Path::new("/var/lib/tracon/repos/github.com/other/name")));

        let empty = Repo::default();
        assert!(!empty.matches(Path::new("/srv/code/tracon")));
    }

    /// An image that could never pin, a repository named twice, and an egress
    /// entry that is not a host are refused at load with the entry in the
    /// message. Each would otherwise only show up as a check that will not run.
    #[test]
    fn a_repo_entry_the_node_could_not_honour_is_refused_at_load() {
        let pinned =
            "localhost/tc@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let repo = "/srv/code/tracon";
        let entry = |path: &str, image: &str| Repo {
            path: PathBuf::from(path),
            image: Some(image.into()),
            ..Default::default()
        };

        validate_repos(&[entry(repo, pinned)]).unwrap();
        // An entry may name no image at all: checks and preparation alone.
        validate_repos(&[Repo {
            path: PathBuf::from(repo),
            checks: Some(vec!["just check".into()]),
            ..Default::default()
        }])
        .unwrap();

        // A mutable tag, and a digest that is not 64 hex characters.
        let error = validate_repos(&[entry(repo, "localhost/tc:latest")]).unwrap_err();
        assert!(error.contains(repo), "{error}");
        assert!(error.contains("pinned"), "{error}");
        assert!(validate_repos(&[entry(repo, &pinned[..40])]).is_err());

        assert!(validate_repos(&[entry("", pinned)])
            .unwrap_err()
            .contains("no path"));
        assert!(validate_repos(&[entry(repo, pinned), entry(repo, pinned)])
            .unwrap_err()
            .contains("twice"));

        let reaching = |egress: &str| Repo {
            path: PathBuf::from(repo),
            egress: vec![egress.into()],
            ..Default::default()
        };
        for refused in [
            "https://crates.io",
            "crates.io:443",
            ".*",
            "*.npmjs.org",
            "nmp",
        ] {
            let error = validate_repos(&[reaching(refused)]).unwrap_err();
            assert!(error.contains("neither a preset"), "{refused}: {error}");
        }
        assert!(validate_repos(&[Repo {
            path: PathBuf::from(repo),
            prepare: vec!["  ".into()],
            ..Default::default()
        }])
        .is_err());

        // Opening nothing to sessions is a mistake worth saying at load.
        let opened = |egress: &[&str]| Repo {
            path: PathBuf::from(repo),
            egress: egress.iter().map(|host| host.to_string()).collect(),
            session_egress: true,
            ..Default::default()
        };
        validate_repos(&[opened(&["crates"])]).unwrap();
        assert!(validate_repos(&[opened(&[])])
            .unwrap_err()
            .contains("egress is empty"));

        // A Dockerfile the node builds stands where an image would, never
        // beside one, and both it and its context stay inside the repository.
        let building = |dockerfile: &str, context: Option<&str>| Repo {
            path: PathBuf::from(repo),
            dockerfile: Some(dockerfile.into()),
            context: context.map(str::to_string),
            ..Default::default()
        };
        validate_repos(&[building(".devcontainer/Dockerfile", None)]).unwrap();
        validate_repos(&[building(".devcontainer/Dockerfile", Some("."))]).unwrap();
        let both = Repo {
            image: Some(pinned.into()),
            ..building("Dockerfile", None)
        };
        assert!(validate_repos(&[both]).unwrap_err().contains("both"));
        for outside in ["/etc/Dockerfile", "../Dockerfile", "a/../../b"] {
            assert!(validate_repos(&[building(outside, None)]).is_err());
            assert!(validate_repos(&[building("Dockerfile", Some(outside))]).is_err());
        }
        assert!(validate_repos(&[Repo {
            path: PathBuf::from(repo),
            context: Some("docker".into()),
            ..Default::default()
        }])
        .unwrap_err()
        .contains("no dockerfile"));
    }

    /// Build steps stand on the stock base, never beside an image of the
    /// entry's own, and only on a node whose sandboxes run that base.
    #[test]
    fn build_steps_stand_on_the_stock_base_alone() {
        let stepped = || Repo {
            path: PathBuf::from("/srv/code/app"),
            build: vec!["apt-get update".into()],
            egress: vec!["apt".into()],
            ..Default::default()
        };
        validate_repos(&[stepped()]).unwrap();
        let pinned = format!("localhost/tc@sha256:{}", "a".repeat(64));
        for beside in [
            Repo {
                image: Some(pinned),
                ..stepped()
            },
            Repo {
                dockerfile: Some("Dockerfile".into()),
                ..stepped()
            },
        ] {
            let error = validate_repos(&[beside]).unwrap_err();
            assert!(error.contains("keep build and drop the other"), "{error}");
        }
        let empty = Repo {
            build: vec!["apt-get update".into(), " ".into()],
            ..stepped()
        };
        assert!(validate_repos(&[empty]).unwrap_err().contains("empty step"));
        // Credentials can be lent to the preparation that follows the build.
        validate_repos(&[Repo {
            prepare: vec!["composer install".into()],
            prepare_credentials: vec!["nova".into()],
            ..stepped()
        }])
        .unwrap();
        assert!(EGRESS_PRESETS
            .iter()
            .any(|(name, hosts)| *name == "apt" && hosts.contains(&"snapshot.ubuntu.com")));

        let toml = |runtime: &str| {
            format!("{runtime}\n[[repo]]\npath = \"/srv/code/app\"\nbuild = [\"apt-get update\"]\n")
        };
        Config::from_toml(&toml(""), Path::new("node.toml")).unwrap();
        Config::from_toml(&toml("[runtime]\nkind = \"apple\""), Path::new("node.toml")).unwrap();
        let kube = Config::from_toml(
            &toml("[runtime]\nkind = \"kubernetes\""),
            Path::new("node.toml"),
        )
        .unwrap_err();
        assert!(
            kube.contains("runtime.kubernetes.sandbox_protocol = true"),
            "{kube}"
        );
    }

    /// Credentials are lent only to the default branch's preparation, which
    /// runs after the node builds the entry's Dockerfile. An entry that could
    /// never lend them says so at load rather than leaving a registry to
    /// refuse an install later.
    #[test]
    fn prepare_credentials_need_a_built_image_and_a_preparation() {
        let lending = |dockerfile: Option<&str>, prepare: &[&str], names: &[&str]| Repo {
            path: PathBuf::from("/srv/code/app"),
            dockerfile: dockerfile.map(str::to_string),
            prepare: prepare.iter().map(|c| c.to_string()).collect(),
            prepare_credentials: names.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        };
        let install = ["composer install --no-interaction"];
        validate_repos(&[lending(Some("Dockerfile"), &install, &["nova"])]).unwrap();
        assert!(validate_repos(&[lending(None, &install, &["nova"])])
            .unwrap_err()
            .contains("name a dockerfile"));
        assert!(
            validate_repos(&[lending(Some("Dockerfile"), &[], &["nova"])])
                .unwrap_err()
                .contains("no prepare command")
        );
        for bad in ["", " nova"] {
            assert!(
                validate_repos(&[lending(Some("Dockerfile"), &install, &[bad])])
                    .unwrap_err()
                    .contains("empty or padded")
            );
        }
        // Left out of the file when there are none, so an entry that never
        // named any reads as it did.
        let plain = toml::to_string(&lending(Some("Dockerfile"), &install, &[])).unwrap();
        assert!(!plain.contains("prepare_credentials"), "{plain}");
        let parsed: Repo = toml::from_str(
            "path = \"/srv/code/app\"\ndockerfile = \"Dockerfile\"\nprepare = [\"x\"]\n\
             prepare_credentials = [\"nova\"]\n",
        )
        .unwrap();
        assert_eq!(parsed.prepare_credentials, ["nova"]);
    }

    /// The table a running node resolves against is the file's until the
    /// operator saves one through the node, and theirs from then on — for
    /// everything sharing that `Config`, without a restart. A copy taken
    /// earlier keeps what it saw.
    #[test]
    fn a_workflow_is_dispatched_only_where_the_file_lists_it() {
        let cfg: Config = toml::from_str(
            r#"
            [github.dispatch]
            "cosmicspork/homelab" = ["renovate.yaml"]
            "#,
        )
        .unwrap();
        assert!(cfg
            .github
            .may_dispatch("cosmicspork/homelab", "renovate.yaml"));
        assert!(cfg
            .github
            .may_dispatch("CosmicSpork/Homelab", "renovate.yaml"));
        assert!(!cfg
            .github
            .may_dispatch("cosmicspork/homelab", "release.yml"));
        assert!(!cfg
            .github
            .may_dispatch("cosmicspork/homelab", "Renovate.yaml"));
        assert!(!cfg
            .github
            .may_dispatch("cosmicspork/tracon", "renovate.yaml"));
        assert!(!Config::default().github.may_dispatch("o/n", "ci.yml"));
    }

    #[test]
    fn a_table_saved_through_the_node_replaces_the_loaded_one_live() {
        let entry = |path: &str| Repo {
            path: PathBuf::from(path),
            ..Default::default()
        };
        let cfg = std::sync::Arc::new(Config {
            repo: vec![entry("/src/loaded")],
            ..Default::default()
        });
        let shared = cfg.clone();
        let copied = (*cfg).clone();
        assert_eq!(shared.repos()[0].path, Path::new("/src/loaded"));

        cfg.live_repo
            .set(vec![entry("/src/saved"), entry("/src/second")]);
        assert_eq!(shared.repos().len(), 2);
        assert_eq!(shared.repos()[0].path, Path::new("/src/saved"));
        assert_eq!(copied.repos()[0].path, Path::new("/src/loaded"));
        // It is the node's state, not the file's: saving never writes it.
        assert!(!toml::to_string(&*cfg).unwrap().contains("live_repo"));
    }

    /// A preset is the hosts a package manager actually talks to; a literal
    /// host is itself; and naming a host twice (a preset and its own member)
    /// allows it once.
    #[test]
    fn egress_presets_expand_to_literal_hosts() {
        let repo = Repo {
            egress: vec![
                "crates".into(),
                "npm".into(),
                "static.crates.io".into(),
                "ghcr.io".into(),
            ],
            ..Default::default()
        };
        assert_eq!(
            repo.egress_hosts().unwrap(),
            [
                "crates.io",
                "index.crates.io",
                "static.crates.io",
                "registry.npmjs.org",
                "ghcr.io"
            ]
        );
        assert!(Repo::default().egress_hosts().unwrap().is_empty());
    }

    /// The table is written as `[[repo]]`, and an entry round-trips through
    /// the file without growing keys it never had.
    #[test]
    fn a_repo_table_loads_from_the_operators_file() {
        let dir = std::env::temp_dir().join(format!("tracon-cfg-repo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node.toml");
        std::fs::write(
            &path,
            r#"
[[repo]]
path = "github.com/owner/name"
image = "localhost/tc@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
checks = ["just check", "just test"]
prepare = ["composer install --no-interaction"]
egress = ["packagist"]
timeout_secs = 1800

[[repo]]
path = "/srv/code/notes"
checks = []
"#,
        )
        .unwrap();
        let config = Config::try_load_from(&path).unwrap();
        assert_eq!(config.repo.len(), 2);
        assert_eq!(config.repo[0].timeout_secs, Some(1800));
        assert_eq!(config.repo[0].prepare.len(), 1);
        assert_eq!(config.repo[1].checks.as_deref(), Some(&[][..]));
        assert!(config.repo[1].image.is_none());
        // Writing the table back as it loaded touches nothing.
        assert!(Op::replace_repos(&config.repo, &config.repo)
            .unwrap()
            .is_empty());
        let mut grown = config.repo.clone();
        grown[1].timeout_secs = Some(60);
        let ops = Op::replace_repos(&config.repo, &grown).unwrap();
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        edit::edit_at(&path, &store, &ops, None, "test").unwrap();
        let back = Config::try_load_from(&path).unwrap();
        assert_eq!(back.repo.len(), 2);
        assert_eq!(back.repo[1].checks.as_deref(), Some(&[][..]));
        assert!(back.repo[1].image.is_none());
        assert_eq!(back.repo[1].timeout_secs, Some(60));
        assert_eq!(back.repo[0].timeout_secs, Some(1800));

        std::fs::write(&path, "[[repo]]\npath = \"/srv/x\"\negress = [\"nope\"]\n").unwrap();
        assert!(Config::try_load_from(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strict_load_rejects_a_typo_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("tracon-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node.toml");
        std::fs::write(
            &path,
            "[mesh]\nhub_url = \"https://hub\"\nheartbeat_secs = \"x\"\n",
        )
        .unwrap();
        assert!(Config::try_load_from(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        let hub = Op::set(KeyPath::keys(&["mesh", "hub_url"]), "https://hub.example");
        edit::edit_at(&path, &store, &[hub], None, "test").unwrap();
        let back = Config::try_load_from(&path).unwrap();
        assert_eq!(back.mesh.hub_url.as_deref(), Some("https://hub.example"));
        assert_eq!(back.mesh.heartbeat_secs, 60);
        assert!(Config::try_load_from(&dir.join("missing.toml")).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_an_old_provider_table_adds_codex_without_overwriting_it() {
        let dir = std::env::temp_dir().join(format!("tracon-cfg-providers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node.toml");
        std::fs::write(
            &path,
            r#"
[providers.anthropic]
credential = "custom-anthropic"
upstream = "https://anthropic.internal"
shape = "anthropic"
login = "anthropic"

[providers.openai]
credential = "openai"
upstream = "https://openai.internal"
shape = "openai"
"#,
        )
        .unwrap();
        let config = Config::try_load_from(&path).unwrap();
        assert_eq!(config.providers["anthropic"].credential, "custom-anthropic");
        assert_eq!(
            config.providers["openai"].upstream,
            "https://openai.internal"
        );
        let codex = &config.providers["openai-codex"];
        assert_eq!(codex.credential, "openai-codex");
        assert_eq!(codex.upstream, "https://chatgpt.com/backend-api");
        assert_eq!(codex.shape, SHAPE_OPENAI_CODEX);
        assert_eq!(codex.login.as_deref(), Some("openai"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Only what the file writes under `[providers]` is the operator's; the
    /// built-ins merged in beside it, or standing in for a file with no
    /// providers at all, are not.
    #[test]
    fn declared_providers_are_the_ones_the_file_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        std::fs::write(&path, "node_name = \"n\"\n").unwrap();
        let config = Config::try_load_from(&path).unwrap();
        assert!(config.providers.contains_key("openai"));
        assert!(config.declared_providers.is_empty());

        std::fs::write(
            &path,
            r#"
[providers.openrouter]
credential = "openrouter"
upstream = "https://openrouter.ai/api/v1"
shape = "openai"

[providers.openai]
credential = "openai"
"#,
        )
        .unwrap();
        let config = Config::try_load_from(&path).unwrap();
        assert!(config.providers.contains_key("anthropic"));
        assert_eq!(
            config.declared_providers.iter().collect::<Vec<_>>(),
            ["openai", "openrouter"]
        );
        assert!(Config::default().declared_providers.is_empty());
    }

    /// A fresh node, or one whose provider sections name no models, still
    /// has a catalogue: the built-in providers declare the current
    /// generation. Models the operator writes are the whole list.
    #[test]
    fn built_in_providers_declare_models_unless_the_operator_does() {
        let dir = std::env::temp_dir().join(format!("tracon-cfg-models-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node.toml");
        std::fs::write(
            &path,
            r#"
[providers.anthropic]
credential = "custom-anthropic"

[providers.openai-codex]
models = [{ id = "gpt-6" }]
"#,
        )
        .unwrap();
        let config = Config::try_load_from(&path).unwrap();
        let ids = |name: &str| -> Vec<String> {
            config.providers[name]
                .models
                .iter()
                .map(|m| m.id.clone())
                .collect()
        };
        assert_eq!(config.providers["anthropic"].credential, "custom-anthropic");
        assert_eq!(
            ids("anthropic"),
            ["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"]
        );
        assert_eq!(ids("openai-codex"), ["gpt-6"]);
        assert_eq!(ids("openai"), ["gpt-5.5", "gpt-5.5-codex"]);
        assert_eq!(
            Config::default().providers["anthropic"].models,
            default_models("anthropic")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A built-in provider declares the current generation unasked, and any
    /// other declares nothing rather than guessing.
    #[test]
    fn default_models_are_the_built_ins_and_nothing_else() {
        assert_eq!(default_models("anthropic").len(), 3);
        assert_eq!(default_models("openai").len(), 2);
        let codex: Vec<_> = default_models("openai-codex")
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            codex,
            ["gpt-5.5"],
            "a ChatGPT sign-in refuses the -codex models"
        );
        assert!(default_models("totally-unknown").is_empty());
    }

    #[test]
    fn priorities_are_named_by_the_operator_or_left_as_numbers() {
        let load = |text: &str| Config::from_toml(text, Path::new("node.toml"));
        let cfg = load("").unwrap();
        let names: Vec<&str> = cfg.work.priority.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Urgent", "Priority", "Planned", "Backlog"]);
        assert_eq!(cfg.work.default_priority, 2);
        assert_eq!(cfg.work.priority_name(4), Some("Urgent"));
        assert_eq!(
            cfg.work.priority_name(0),
            None,
            "an unnamed value is a number"
        );
        assert_eq!(cfg.work.parse_priority("urgent"), Ok(4));
        assert_eq!(cfg.work.parse_priority(" 7 "), Ok(7));
        assert!(cfg
            .work
            .parse_priority("soon")
            .unwrap_err()
            .contains("Urgent"));

        // Written out, the list replaces the default, in any order.
        let cfg = load(
            "[work]\ndefault_priority = 0\n[[work.priority]]\nname = \"Low\"\nvalue = -1\n\
             [[work.priority]]\nname = \"High\"\nvalue = 10\n",
        )
        .unwrap();
        let levels: Vec<i64> = cfg.work.levels().iter().map(|l| l.value).collect();
        assert_eq!(levels, [10, -1], "highest first");
        assert_eq!(cfg.work.default_priority, 0);

        // An empty list names nothing: priorities are plain numbers.
        let cfg = load("[work]\npriority = []\n").unwrap();
        assert!(cfg.work.levels().is_empty());
        assert_eq!(cfg.work.parse_priority("3"), Ok(3));
        assert!(cfg.work.parse_priority("Urgent").is_err());

        for refused in [
            "[[work.priority]]\nname = \"\"\nvalue = 1\n",
            "[[work.priority]]\nname = \"A\"\nvalue = 1\n[[work.priority]]\nname = \"a\"\nvalue = 2\n",
            "[[work.priority]]\nname = \"A\"\nvalue = 1\n[[work.priority]]\nname = \"B\"\nvalue = 1\n",
        ] {
            assert!(load(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_kind_the_node_reserves_or_already_has_is_refused() {
        let load = |kinds: &str| Config::from_toml(kinds, Path::new("node.toml"));
        let cfg = load("").unwrap();
        assert_eq!(cfg.docs.kind, default_doc_kinds());
        let cfg =
            load("[[docs.kind]]\nprefix = \"runbook\"\nlabel = \"Runbook\"\nworking_note = true\n")
                .unwrap();
        assert_eq!(
            cfg.docs.kind.len(),
            1,
            "written out, the list replaces the default"
        );
        assert!(cfg.docs.kind[0].working_note);
        for refused in ["shown", "plan", "other", "run-book", "Runbook", ""] {
            let text = format!("[[docs.kind]]\nprefix = {refused:?}\n");
            assert!(load(&text).is_err(), "{refused:?}");
        }
        let twice = "[[docs.kind]]\nprefix = \"note\"\n[[docs.kind]]\nprefix = \"note\"\n";
        assert!(load(twice).unwrap_err().contains("twice"));
    }

    #[test]
    fn preview_origin_is_separate_and_origin_only() {
        let mut docs = Docs::default();
        assert_eq!(docs.preview_origin().unwrap(), "http://127.0.0.1:7422");
        docs.preview_url = Some("https://preview.example:8443".into());
        assert_eq!(
            docs.preview_origin().unwrap(),
            "https://preview.example:8443"
        );
        for invalid in [
            "ftp://preview.example",
            "https://preview.example/path",
            "https://user@preview.example",
            "https://preview.example?x=1",
        ] {
            docs.preview_url = Some(invalid.into());
            assert!(docs.preview_origin().is_err(), "{invalid}");
        }
    }

    /// The subject a push is signed with is the contact, then a public origin,
    /// and otherwise nothing: the old `mailto:tracon@localhost` default is
    /// what Apple answered with 403.
    #[test]
    fn the_push_subject_is_never_made_up() {
        let mut notify = Notify::default();
        assert_eq!(notify.subject(None), Err(NO_PUSH_CONTACT));
        assert_eq!(
            notify.subject(Some("https://tracon.example.org/")).unwrap(),
            "https://tracon.example.org"
        );
        for unusable in ["http://tracon.example.org", "https://localhost:7420"] {
            assert_eq!(
                notify.subject(Some(unusable)),
                Err(NO_PUSH_CONTACT),
                "{unusable}"
            );
        }
        notify.contact = Some("mailto:ops@example.org".into());
        assert_eq!(
            notify.subject(Some("https://tracon.example.org")).unwrap(),
            "mailto:ops@example.org",
            "a configured contact wins"
        );
    }

    #[test]
    fn a_push_contact_is_checked_when_the_config_is_loaded() {
        for good in [
            "mailto:ops@example.org",
            "https://tracon.example.org",
            "https://example.org/contact",
        ] {
            assert!(validate_contact(good).is_ok(), "{good}");
        }
        for bad in [
            "mailto:tracon@localhost",
            "mailto:ops@box.local",
            "mailto:example.org",
            "mailto:@example.org",
            "mailto:ops@nodot",
            "ops@example.org",
            "http://example.org",
            "https://127.0.0.1",
            "https://[::1]:7420",
            "",
        ] {
            assert!(validate_contact(bad).is_err(), "{bad:?}");
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        std::fs::write(&path, "[notify]\ncontact = \"mailto:tracon@localhost\"\n").unwrap();
        let error = Config::try_load_from(&path).unwrap_err();
        assert!(error.contains("notify.contact"), "{error}");
        std::fs::write(&path, "[notify]\ncontact = \"mailto:ops@example.org\"\n").unwrap();
        assert_eq!(
            Config::try_load_from(&path)
                .unwrap()
                .notify
                .contact
                .as_deref(),
            Some("mailto:ops@example.org")
        );
    }

    /// Every harness pod renders explicit requests and a memory limit, so a
    /// namespace's LimitRange never fills in a default the node did not
    /// choose; a check gets more than a session because a test suite with a
    /// browser needs it.
    #[test]
    fn an_internal_block_that_does_not_parse_is_refused_at_load() {
        let mut k = Config::default().runtime.kubernetes;
        k.internal_cidrs = vec!["203.0.113.0/24".into(), "64:ff9b::/96".into()];
        k.validate().unwrap();
        k.internal_cidrs.push("10.0.0.0/33".into());
        let why = k.validate().unwrap_err();
        assert!(
            why.starts_with("runtime.kubernetes.internal_cidrs:"),
            "{why}"
        );
        assert!(why.contains("10.0.0.0/33"), "{why}");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        std::fs::write(
            &path,
            "[runtime.kubernetes]\ninternal_cidrs = [\"public-pods\"]\n",
        )
        .unwrap();
        let why = Config::try_load_from(&path).unwrap_err();
        assert!(why.contains("internal_cidrs"), "{why}");
    }

    #[test]
    fn kubernetes_pods_are_sized_by_default() {
        let k = Config::default().runtime.kubernetes;
        k.validate().unwrap();
        let session = k.resources.session();
        assert_eq!(session.cpu_request, "250m");
        assert_eq!(session.memory_request, "1Gi");
        assert_eq!(session.memory_limit, "4Gi");
        assert_eq!(session.cpu_limit, None);
        let command = k.resources.command();
        assert_eq!(command.cpu_request, "500m");
        assert_eq!(command.memory_request, "2Gi");
        assert_eq!(command.memory_limit, "4Gi");
        assert_eq!(command.cpu_limit, None);
        let probe = k.resources.probe();
        assert_eq!(probe.cpu_request, "100m");
        assert_eq!(probe.memory_request, "128Mi");
        assert_eq!(probe.memory_limit, "512Mi");
        assert_eq!(probe.cpu_limit, None);
        assert!(k.image_pull_secrets.is_empty());
        assert_eq!(k.shm_size, "1Gi");
    }

    /// A table that changes one value keeps its own kind's defaults for the
    /// rest, not the other kind's.
    #[test]
    fn a_partial_resources_table_keeps_its_kinds_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [runtime.kubernetes]
            image_pull_secrets = ["registry-tracon"]
            shm_size = "512Mi"
            [runtime.kubernetes.resources.command]
            memory_limit = "6Gi"
            cpu_limit = "3"
            "#,
        )
        .unwrap();
        let k = &cfg.runtime.kubernetes;
        k.validate().unwrap();
        let command = k.resources.command();
        assert_eq!(command.memory_limit, "6Gi");
        assert_eq!(command.memory_request, "2Gi");
        assert_eq!(command.cpu_limit.as_deref(), Some("3"));
        assert_eq!(
            k.resources.session(),
            Config::default().runtime.kubernetes.resources.session()
        );
        assert_eq!(k.image_pull_secrets, ["registry-tracon"]);
        assert_eq!(k.shm_size, "512Mi");
    }

    #[test]
    fn kubernetes_sizing_the_api_would_refuse_is_refused_at_load() {
        let refused = |toml_text: &str, needle: &str| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("node.toml");
            std::fs::write(&path, toml_text).unwrap();
            let error = Config::try_load_from(&path).unwrap_err();
            assert!(error.contains(needle), "{needle}: {error}");
        };
        refused(
            "[runtime.kubernetes.resources.session]\nmemory_request = \"6Gi\"\n",
            "runtime.kubernetes.resources.session.memory_limit (4Gi) is below memory_request (6Gi)",
        );
        refused(
            "[runtime.kubernetes.resources.command]\ncpu_request = \"2\"\ncpu_limit = \"1500m\"\n",
            "resources.command.cpu_limit (1500m) is below cpu_request (2)",
        );
        refused(
            "[runtime.kubernetes.resources.command]\nmemory_limit = \"4 GB\"\n",
            "resources.command.memory_limit: \"4 GB\" is not a quantity",
        );
        refused(
            "[runtime.kubernetes]\nshm_size = \"8Gi\"\n",
            "shm_size (8Gi) is larger than",
        );
        refused(
            "[runtime.kubernetes]\nshm_size = \"0\"\n",
            "not a positive quantity",
        );
        refused(
            "[runtime.kubernetes]\nimage_pull_secrets = [\"Registry_Creds\"]\n",
            "\"Registry_Creds\" is not a Secret name",
        );
        refused(
            "[runtime.kubernetes]\nuid = 0\n",
            "uid: 0 is not a non-root uid",
        );
        refused("[runtime.kubernetes]\nuid = -1\n", "not a non-root uid");
        for home in [
            "home/harness",
            "/",
            "/home/harness/",
            "/home/my harness",
            "/home/$(id)",
            "/home/../root",
            "/home:x",
        ] {
            refused(
                &format!("[runtime.kubernetes]\nharness_home = {home:?}\n"),
                "runtime.kubernetes.harness_home",
            );
        }
        let mut k = Config::default().runtime.kubernetes;
        k.harness_home = "/var/lib/agent_home-2.d".into();
        k.uid = 1000;
        k.validate().unwrap();
        // Equal is allowed: a Guaranteed pod is a limit that meets its request.
        let mut k = Config::default().runtime.kubernetes;
        k.resources.command.memory_limit = Some("2048Mi".into());
        k.resources.command.cpu_limit = Some("0.5".into());
        k.validate().unwrap();
    }

    #[test]
    fn kubernetes_quantities_parse_as_the_api_writes_them() {
        assert_eq!(kube_quantity("250m"), Some(0.25));
        assert_eq!(kube_quantity("2"), Some(2.0));
        assert_eq!(kube_quantity("+1.5"), Some(1.5));
        assert_eq!(kube_quantity("1Gi"), Some(1024f64.powi(3)));
        assert_eq!(kube_quantity("2048Mi"), kube_quantity("2Gi"));
        assert_eq!(kube_quantity("1G"), Some(1e9));
        assert_eq!(kube_quantity("1e3"), Some(1000.0));
        assert_eq!(kube_quantity("5E-1"), Some(0.5));
        for bad in [
            "", "Gi", "-1", "1.2.3Gi", "1gb", "1 Gi", "inf", "1e", "1Gix",
        ] {
            assert_eq!(kube_quantity(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_public_origin_is_an_https_origin_and_nothing_more() {
        let mut auth = Auth::default();
        assert_eq!(auth.public_origin().unwrap(), None);

        for (written, normalised) in [
            (
                "https://tracon-node.0x69.xyz",
                "https://tracon-node.0x69.xyz",
            ),
            (
                "https://tracon-node.0x69.xyz/",
                "https://tracon-node.0x69.xyz",
            ),
            (
                "  HTTPS://Tracon.Example.org:443/ ",
                "https://tracon.example.org",
            ),
            (
                "https://tracon.example.org:8443",
                "https://tracon.example.org:8443",
            ),
            // Plain HTTP only where a browser already treats it as secure,
            // and where a passkey can still be made: local testing.
            ("http://localhost:7420", "http://localhost:7420"),
            ("http://LocalHost:7420/", "http://localhost:7420"),
            ("https://localhost:8443", "https://localhost:8443"),
        ] {
            auth.public_origin = Some(written.into());
            assert_eq!(
                auth.public_origin().unwrap().as_deref(),
                Some(normalised),
                "{written}"
            );
        }

        for invalid in [
            "tracon.example.org",
            "http://tracon.example.org",
            "http://192.168.1.10:7420",
            "http://localhost.example.org",
            "ftp://tracon.example.org",
            "https://tracon.example.org/tracon",
            "https://user@tracon.example.org",
            "https://user:pw@tracon.example.org",
            "https://tracon.example.org?x=1",
            "https://tracon.example.org#f",
            "",
        ] {
            auth.public_origin = Some(invalid.into());
            assert!(auth.public_origin().is_err(), "{invalid:?}");
        }

        // An IP address is never a relying-party ID, loopback or public, and
        // the refusal says why and what to use instead.
        for ip in [
            "https://203.0.113.5",
            "https://100.101.102.103:8443",
            "https://[2001:db8::1]",
            "http://127.0.0.1:7420",
            "https://127.0.0.1:7420",
            "http://[::1]:7420",
        ] {
            auth.public_origin = Some(ip.into());
            let error = auth.public_origin().unwrap_err();
            assert!(error.contains("domain name"), "{ip}: {error}");
            assert!(error.contains("localhost"), "{ip}: {error}");
        }
    }

    #[test]
    fn a_public_origin_is_checked_at_load_and_names_the_push_subject() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");

        std::fs::write(
            &path,
            "[auth]\npublic_origin = \"http://tracon.example.org\"\n",
        )
        .unwrap();
        let error = Config::try_load_from(&path).unwrap_err();
        assert!(error.contains("auth.public_origin"), "{error}");

        // Unset: no public name, and so no push subject without a contact.
        std::fs::write(&path, "").unwrap();
        let cfg = Config::try_load_from(&path).unwrap();
        assert_eq!(cfg.public_origin(), None);
        assert_eq!(
            cfg.notify.subject(cfg.public_origin().as_deref()),
            Err(NO_PUSH_CONTACT)
        );

        // Set, with no contact: the origin is the subject, as written once.
        std::fs::write(
            &path,
            "[auth]\npublic_origin = \"https://Tracon-Node.0x69.xyz/\"\n",
        )
        .unwrap();
        let cfg = Config::try_load_from(&path).unwrap();
        assert_eq!(
            cfg.public_origin().as_deref(),
            Some("https://tracon-node.0x69.xyz")
        );
        assert_eq!(
            cfg.notify.subject(cfg.public_origin().as_deref()).unwrap(),
            "https://tracon-node.0x69.xyz"
        );

        // A contact still wins.
        std::fs::write(
            &path,
            "[auth]\npublic_origin = \"https://tracon-node.0x69.xyz\"\n\n[notify]\ncontact = \"mailto:ops@example.org\"\n",
        )
        .unwrap();
        let cfg = Config::try_load_from(&path).unwrap();
        assert_eq!(
            cfg.notify.subject(cfg.public_origin().as_deref()).unwrap(),
            "mailto:ops@example.org"
        );

        // A loopback origin is valid for the node but is no push subject:
        // Apple will not write to this machine.
        std::fs::write(&path, "[auth]\npublic_origin = \"http://localhost:7420\"\n").unwrap();
        let cfg = Config::try_load_from(&path).unwrap();
        assert_eq!(
            cfg.public_origin().as_deref(),
            Some("http://localhost:7420")
        );
        assert_eq!(
            cfg.notify.subject(cfg.public_origin().as_deref()),
            Err(NO_PUSH_CONTACT)
        );
    }
}
