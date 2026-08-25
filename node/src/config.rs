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
    pub harness_listen: std::net::SocketAddr,
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
                harness_listen: "127.0.0.1:7421".parse().expect("valid default address"),
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
                worktree_root: PathBuf::from("/private/tmp"),
            },
        }
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

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "bad node.toml, using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }
}
