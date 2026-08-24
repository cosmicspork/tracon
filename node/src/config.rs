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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Harness {
    /// Harness id. Only `omp` has an adapter.
    pub id: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionDefaults {
    pub budget_tokens: i64,
    pub permission_timeout_secs: u64,
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
            },
            session: SessionDefaults {
                budget_tokens: 2_000_000,
                permission_timeout_secs: 900,
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
