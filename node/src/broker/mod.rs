//! The credential broker. Credentials live here and nowhere the harness can
//! reach: not in its environment, not in its filesystem, not in a tool result.
//! The harness is given a capability (a tool it may call) rather than a secret.
//!
//! This is the difference between a gate and theatre. consulta today reads a
//! database password from a `.env` on the same UID as the agent that calls it;
//! an agent with a shell can read that file and open its own unguarded
//! connection. Here the agent never has the password at all.

pub mod guard;

use std::{collections::BTreeMap, path::PathBuf};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("no credential named {0}")]
    Unknown(String),
    #[error("credential {name} is not bound to channel {channel}")]
    NotBound { name: String, channel: String },
    #[error("credential store: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential store is malformed: {0}")]
    Parse(String),
    #[error("credential store {path} is readable beyond its owner (mode {mode:o}); chmod 600 it")]
    TooOpen { path: String, mode: u32 },
}

/// One credential and the channels allowed to use it. `--profile` in consulta
/// becomes a channel binding here: which channel may use which connection.
#[derive(Debug, Clone, Deserialize)]
pub struct Credential {
    /// Environment the sidecar needs, injected at spawn and never logged.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Channels permitted to use it. Empty means no channel, not every channel:
    /// an unbound credential is unusable rather than universal.
    #[serde(default)]
    pub channels: Vec<String>,
}

impl Credential {
    pub fn allows(&self, channel: &str) -> bool {
        self.channels.iter().any(|c| c == channel)
    }
}

/// Loaded once at startup from a file only the node's user can read.
#[derive(Debug, Default, Deserialize)]
pub struct Broker {
    #[serde(default)]
    credentials: BTreeMap<String, Credential>,
}

impl Broker {
    pub fn path() -> PathBuf {
        crate::config::Config::state_dir().join("credentials.toml")
    }

    /// Missing store is not an error: a node with no credentials brokers no
    /// tools, which is the correct behaviour rather than a startup failure.
    pub fn load() -> Result<Self, BrokerError> {
        let path = Self::path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        // The whole point of the broker is that nothing but the node's user can
        // read these secrets. A file group- or world-accessible is refused
        // rather than loaded: the documented guarantee is enforcement, not
        // advice. A refused store brokers nothing, which fails closed.
        Self::refuse_if_too_open(&path)?;
        toml::from_str(&text).map_err(|e| BrokerError::Parse(e.to_string()))
    }

    #[cfg(unix)]
    fn refuse_if_too_open(path: &std::path::Path) -> Result<(), BrokerError> {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(BrokerError::TooOpen {
                    path: path.display().to_string(),
                    mode,
                });
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn refuse_if_too_open(_path: &std::path::Path) -> Result<(), BrokerError> {
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.credentials.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.credentials.keys().map(String::as_str).collect()
    }

    /// The only way out of the broker: environment for a process the node
    /// spawns on its own side of the privilege boundary. There is deliberately
    /// no accessor that returns a secret as a value a response could carry.
    pub fn env_for(
        &self,
        name: &str,
        channel: &str,
    ) -> Result<BTreeMap<String, String>, BrokerError> {
        let cred = self
            .credentials
            .get(name)
            .ok_or_else(|| BrokerError::Unknown(name.to_string()))?;
        if !cred.allows(channel) {
            return Err(BrokerError::NotBound {
                name: name.to_string(),
                channel: channel.to_string(),
            });
        }
        Ok(cred.env.clone())
    }

    /// Whether a channel may use a credential at all, for deciding which tools
    /// to offer a session before it asks.
    pub fn available_to(&self, channel: &str) -> Vec<&str> {
        self.credentials
            .iter()
            .filter(|(_, c)| c.allows(channel))
            .map(|(n, _)| n.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker() -> Broker {
        toml::from_str(
            r#"
            [credentials.warehouse]
            channels = ["work"]
            [credentials.warehouse.env]
            DB_BACKEND = "sqlite"
            DB_DATABASE = "/tmp/x.db"

            [credentials.orphan]
            [credentials.orphan.env]
            TOKEN = "secret"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn a_bound_channel_gets_the_environment() {
        let env = broker().env_for("warehouse", "work").unwrap();
        assert_eq!(env.get("DB_BACKEND").map(String::as_str), Some("sqlite"));
    }

    #[test]
    fn another_channel_is_refused() {
        let err = broker().env_for("warehouse", "personal").unwrap_err();
        assert!(matches!(err, BrokerError::NotBound { .. }));
    }

    #[test]
    fn an_unbound_credential_is_unusable_not_universal() {
        // The dangerous default would be "no channels listed means any channel".
        for channel in ["work", "personal", ""] {
            assert!(broker().env_for("orphan", channel).is_err());
        }
        assert!(broker().available_to("work").contains(&"warehouse"));
        assert!(!broker().available_to("work").contains(&"orphan"));
    }

    #[test]
    fn an_unknown_credential_is_refused() {
        assert!(matches!(
            broker().env_for("nope", "work").unwrap_err(),
            BrokerError::Unknown(_)
        ));
    }

    #[test]
    fn a_missing_store_brokers_nothing() {
        let b = Broker::default();
        assert!(b.is_empty());
        assert!(b.available_to("work").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_group_readable_store_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tracon-broker-perms-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("credentials.toml");
        std::fs::write(&path, "[credentials.x]\n").unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Broker::refuse_if_too_open(&path),
            Err(BrokerError::TooOpen { .. })
        ));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Broker::refuse_if_too_open(&path).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
