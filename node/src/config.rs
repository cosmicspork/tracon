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
}

/// Where this node sends a push when something starts waiting on the operator,
/// and what a notification links back to. Which channels are pushed at all is a
/// channel binding, not config: the sink follows the work, not the machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Notify {
    /// The pager bridge's capture endpoint. The bridge holds every key and
    /// seals to the paired devices; the node hands it cleartext over loopback
    /// (or the pod network) and holds no notification secret of its own.
    pub pager_url: String,
    /// The origin a notification's link should point at — the address the
    /// operator reaches this node on. Without one, pushes carry no link.
    pub link_origin: Option<String>,
}

impl Default for Notify {
    fn default() -> Self {
        Self {
            pager_url: "http://127.0.0.1:4500/capture".into(),
            link_origin: None,
        }
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

/// One model provider the gateway fronts. The harness reaches it at
/// `/model/<name>/…`; the node injects `credential` and forwards to
/// `upstream`, which must also pass the egress allowlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Provider {
    /// The broker credential injected for it (kind `api_key` or `oauth`).
    pub credential: String,
    pub upstream: String,
    /// `anthropic` or `openai`: which headers the credential becomes.
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
            harness: Harness {
                id: "omp".into(),
                version: "18.0.4".into(),
                // Empty: see the field's note. The surface a session actually
                // runs against is bounded by the boundary and by policy, both
                // of which hold whatever the harness offers.
                tools: Vec::new(),
            },
            boundary: Boundary {
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
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tracon/node.toml")
    }

    /// State the node owns: database, gateway allowlist, per-session scratch.
    pub fn state_dir() -> PathBuf {
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
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
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
    use super::*;

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
}
