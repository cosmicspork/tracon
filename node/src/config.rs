//! Node configuration: `~/.config/tracon/node.toml`, overridden by flags.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    pub mesh: Mesh,
    pub runtime: Runtime,
    /// Model providers the gateway fronts, by name.
    pub providers: std::collections::BTreeMap<String, Provider>,
    pub memory: Memory,
    pub supervision: Supervision,
    pub review: ReviewLimits,
    pub notify: Notify,
    pub embed: Embed,
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
    /// one. A path rather than the token itself: `node.toml` is a plain file
    /// and a read-only ConfigMap in a pod, so the secret stays somewhere that
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

impl Notify {
    pub fn subject(&self) -> &str {
        self.contact.as_deref().unwrap_or("mailto:tracon@localhost")
    }
}

/// Deterministic checks the node runs at submit, in a throwaway harness
/// container with the worktree mounted and nothing else. A worktree may
/// carry its own list in `.tracon/checks` (one command per line).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Supervision {
    pub checks: Vec<String>,
    pub timeout_secs: u64,
}

impl Default for Supervision {
    fn default() -> Self {
        Self {
            checks: vec!["just check".into()],
            timeout_secs: 900,
        }
    }
}

/// What a submission may be at most. Complexity accretes because nothing
/// says no at submission time; this does.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewLimits {
    /// Added plus removed lines.
    pub max_diff_lines: i64,
    pub max_files: usize,
}

impl Default for ReviewLimits {
    fn default() -> Self {
        Self {
            max_diff_lines: 800,
            max_files: 40,
        }
    }
}

/// Which boundary this node establishes. `podman` is a laptop or Linux host
/// with rootless Podman; `kubernetes` is a node running as a pod that owns
/// harness pods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    #[default]
    Podman,
    Kubernetes,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Runtime {
    pub kind: RuntimeKind,
    pub kubernetes: Kubernetes,
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
}

impl Default for Kubernetes {
    fn default() -> Self {
        Self {
            namespace: String::new(),
            harness_image: format!(
                "ghcr.io/cosmicspork/tracon-harness:{}",
                env!("CARGO_PKG_VERSION")
            ),
            state_claim: "tracon-state".into(),
            state_mount: PathBuf::from("/state"),
            harness_home: "/home/harness".into(),
            uid: 65532,
            gateway_host: "tracon-gw".into(),
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
}
impl Default for Mesh {
    fn default() -> Self {
        Self {
            hub_url: None,
            heartbeat_secs: 60,
            poll_secs: 30,
            command_timeout_secs: 15,
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
    /// Harness id. Only `omp` has an adapter.
    pub id: String,
    /// The tools a session may use at all, by the harness's own names. Empty
    /// means the harness's default set, which is the default here.
    ///
    /// Restricting is available but not on by default, and the reason is worth
    /// knowing: omp's `--tools` is a whitelist, and its shell is not one of the
    /// names it accepts. Any list at all therefore removes the shell, which
    /// removes the agent's ability to commit — and without commits there is
    /// nothing to review, so the whole publish path stops. An agent that loses
    /// its shell does not report that it is stuck; it starts reading `.git`
    /// by hand to work around it.
    ///
    /// Reduce the surface deliberately, per node, once you know which tools a
    /// given channel actually needs.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Exact version this node runs. Checked twice: `omp --version` in the
    /// runner, and `initialize.agentInfo.version` at session start.
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Boundary {
    /// The podman binary. Empty: resolved from PATH, then well-known install
    /// locations — a node launched from Finder inherits launchd's minimal
    /// PATH, which has no Homebrew.
    pub podman: String,
    pub network: String,
    pub subnet: String,
    pub gateway_ip: String,
    pub gateway_container: String,
    pub gateway_image: String,
    pub harness_image: String,
    /// Podman needs `label=disable` for bind mounts on SELinux hosts.
    pub selinux_label_disable: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Gateway {
    /// Hosts the harness may CONNECT to, as anchored regexes for tinyproxy's
    /// filter. Everything else is denied.
    pub allow_hosts: Vec<String>,
    pub proxy_port: u16,
    /// Port the gateway forwards from the internal network to the node.
    pub forward_port: u16,
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
    /// The harness's own provider id for a subscription login
    /// (`omp auth-broker login <id>`); none means API key only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// What a token costs through this provider, when the credential is
    /// metered. Absent means a subscription: tokens are counted, dollars are
    /// not derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
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
        }
    }
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
            },
        ),
        (
            "openai-codex",
            Provider {
                credential: "openai-codex".into(),
                upstream: "https://chatgpt.com/backend-api".into(),
                shape: SHAPE_OPENAI_CODEX.into(),
                login: Some("openai-codex".into()),
                price: None,
            },
        ),
    ]
    .into_iter()
    .map(|(n, p)| (n.to_string(), p))
    .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionDefaults {
    pub budget_tokens: i64,
    pub permission_timeout_secs: u64,
    /// How long a claim survives a client that stopped talking. A dropped socket
    /// should not zero the attention count; a closed laptop should.
    pub claim_grace_secs: u64,
    /// Where worktrees are created. Outside any repo, so nothing is gitignored.
    pub worktree_root: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node_name: hostname(),
            mesh: Mesh::default(),
            runtime: Runtime::default(),
            providers: default_providers(),
            memory: Memory::default(),
            supervision: Supervision::default(),
            review: ReviewLimits::default(),
            notify: Notify::default(),
            embed: Embed::default(),
            harness: Harness {
                id: "omp".into(),
                version: "18.0.4".into(),
                // Empty: see the field's note. The surface a session actually
                // runs against is bounded by the boundary and by policy, both
                // of which hold whatever the harness offers.
                tools: Vec::new(),
            },
            boundary: Boundary {
                podman: String::new(),
                network: "tracon-int".into(),
                subnet: "10.89.0.0/24".into(),
                gateway_ip: "10.89.0.2".into(),
                gateway_container: "tracon-gw".into(),
                gateway_image: "localhost/tracon-gateway".into(),
                harness_image: "localhost/tracon-harness".into(),
                selinux_label_disable: None,
            },
            gateway: Gateway {
                allow_hosts: vec![
                    r"^api\.anthropic\.com$".into(),
                    r"^api\.openai\.com$".into(),
                    r"^chatgpt\.com$".into(),
                    r"^auth\.openai\.com$".into(),
                ],
                proxy_port: 8888,
                forward_port: 7421,
                harness_listen: HarnessListen::default(),
            },
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
            },
            session: SessionDefaults {
                budget_tokens: 2_000_000,
                permission_timeout_secs: 900,
                claim_grace_secs: 60,
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

    /// Where `tracon setup` writes the embedded container definitions.
    pub fn containers_dir() -> PathBuf {
        Self::state_dir().join("containers")
    }

    pub fn allow_file() -> PathBuf {
        Self::state_dir().join("gateway/allow.txt")
    }

    /// The harness's own state directory, node-owned. Only the harness's
    /// credential database is mounted into it; nothing else from `~/.omp`
    /// leaks in (its `AGENTS.md` is a symlink to the operator's workspace).
    pub fn harness_state_dir() -> PathBuf {
        Self::state_dir().join("harness-state")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    /// Lenient load for one-shot commands: a bad file logs and yields defaults.
    pub fn load_from(path: &Path) -> Self {
        Self::try_load_from(path).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "bad node.toml, using defaults");
            Self::default()
        })
    }

    /// Strict load for `serve`: a file that exists but does not parse is an
    /// error, so a typo cannot silently drop the whole configuration.
    pub fn try_load() -> Result<Self, String> {
        Self::try_load_from(&Self::config_path())
    }

    pub fn try_load_from(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut config: Self = toml::from_str(&text)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                for (name, provider) in default_providers() {
                    config.providers.entry(name).or_insert(provider);
                }
                Ok(config)
            }
            Err(_) => Ok(Self::default()),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::config_path())
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {

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

    /// The README's `node.toml` reference is checked, not trusted: every key it
    /// names must exist, and the values it shows as defaults must be them.
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
        assert_eq!(parsed.session.budget_tokens, d.session.budget_tokens);
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
        assert_eq!(parsed.boundary.subnet, d.boundary.subnet);
        assert_eq!(
            parsed.providers["anthropic"].upstream,
            d.providers["anthropic"].upstream
        );
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
        let mut c = Config::default();
        c.mesh.hub_url = Some("https://hub.example".into());
        c.save_to(&path).unwrap();
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
        assert_eq!(codex.login.as_deref(), Some("openai-codex"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
