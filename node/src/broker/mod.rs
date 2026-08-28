//! The credential broker. Credentials live here and nowhere the harness can
//! reach: not in its environment, not in its filesystem, not in a tool result.
//! The harness is given a capability (a tool it may call) rather than a secret.
//!
//! This is the difference between a gate and theatre. consulta today reads a
//! database password from a `.env` on the same UID as the agent that calls it;
//! an agent with a shell can read that file and open its own unguarded
//! connection. Here the agent never has the password at all.
//!
//! At rest the store is sealed under a key derived from the node's identity
//! seed (`credentials.sealed`), so a copied file is ciphertext on any other
//! machine and losing the seed loses the store — the same rule channel keys
//! already live by. A plaintext `credentials.toml` is still accepted as an
//! import and is sealed on first load.

pub mod guard;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use proto::envelope::{DataKey, Sealed};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The broker as the node holds it: readable by every tool call, writable by
/// a login, an import, or a handoff.
pub type SharedBroker = Arc<RwLock<Broker>>;

/// Associated data for the sealed store, so the bytes cannot be presented as
/// anything else sealed under the same key.
const STORE_AAD: &[u8] = b"tracon/credstore";
const SEALED_FILE: &str = "credentials.sealed";
const PLAIN_FILE: &str = "credentials.toml";

#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("no credential named {0}")]
    Unknown(String),
    #[error("credential {name} is not bound to channel {channel}")]
    NotBound { name: String, channel: String },
    #[error("credential {name} is not bound to this node")]
    NotOnThisNode { name: String },
    #[error("credential store: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential store is malformed: {0}")]
    Parse(String),
    #[error("credential store {path} is readable beyond its owner (mode {mode:o}); chmod 600 it")]
    TooOpen { path: String, mode: u32 },
    #[error("credential store could not be opened with this node's key")]
    Sealed,
}

/// The kinds a credential can be. `env` is an opaque environment for a
/// node-side subprocess or tool; the model kinds carry a provider so the
/// gateway knows whose header to write.
pub const KIND_ENV: &str = "env";
pub const KIND_API_KEY: &str = "api_key";
pub const KIND_OAUTH: &str = "oauth";

/// One credential and the channels allowed to use it. `--profile` in consulta
/// becomes a channel binding here: which channel may use which connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credential {
    /// Environment the sidecar needs, injected at spawn and never logged.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Channels permitted to use it. Empty means no channel, not every channel:
    /// an unbound credential is unusable rather than universal.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Nodes permitted to use it, by node id. Empty means the node holding
    /// this file, which is the only node that can read it anyway; a list pins
    /// it further, so a store copied to another machine brokers nothing there.
    /// "consulta on the work node only" is this field.
    #[serde(default)]
    pub nodes: Vec<String>,
    /// `env` (default), `api_key`, or `oauth`.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// For model kinds: the provider the gateway injects it for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// For `oauth`: when the access token expires, so a refresh can run ahead
    /// of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<i64>,
    /// For `oauth`: who the token belongs to, as the provider reports it.
    /// Shown, never used for anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

fn default_kind() -> String {
    KIND_ENV.to_string()
}

impl Default for Credential {
    fn default() -> Self {
        Self {
            env: BTreeMap::new(),
            channels: Vec::new(),
            nodes: Vec::new(),
            kind: default_kind(),
            provider: None,
            expires_ms: None,
            identity: None,
        }
    }
}

impl Credential {
    pub fn allows(&self, channel: &str) -> bool {
        self.channels.iter().any(|c| c == channel)
    }

    pub fn allows_node(&self, node_id: &str) -> bool {
        self.nodes.is_empty() || self.nodes.iter().any(|n| n == node_id)
    }

    /// A credential the model gateway injects, as opposed to one a tool
    /// consumes as environment.
    pub fn is_model(&self) -> bool {
        self.kind == KIND_API_KEY || self.kind == KIND_OAUTH
    }
}

/// Loaded at startup from a store only the node can open, and written back
/// whenever a login, an import, or a handoff changes it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Broker {
    #[serde(default)]
    credentials: BTreeMap<String, Credential>,
}

impl Broker {
    /// The sealed store.
    pub fn path() -> PathBuf {
        crate::config::Config::state_dir().join(SEALED_FILE)
    }

    /// The plaintext store the node accepted before Phase 4. Read once and
    /// sealed; kept only as an import path.
    pub fn plain_path() -> PathBuf {
        crate::config::Config::state_dir().join(PLAIN_FILE)
    }

    pub fn shared(self) -> SharedBroker {
        Arc::new(RwLock::new(self))
    }

    /// Missing store is not an error: a node with no credentials brokers no
    /// tools, which is the correct behaviour rather than a startup failure.
    /// A plaintext store found beside a missing sealed one is imported and
    /// sealed, and the plaintext renamed so it is not read twice.
    pub fn load(key: &DataKey) -> Result<Self, BrokerError> {
        Self::load_at(&Self::path(), &Self::plain_path(), key)
    }

    pub fn load_at(sealed: &Path, plain: &Path, key: &DataKey) -> Result<Self, BrokerError> {
        match std::fs::read(sealed) {
            Ok(bytes) => return Self::open(&bytes, key),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let text = match std::fs::read_to_string(plain) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let broker = Self::parse_plain(plain, &text)?;
        broker.save_at(sealed, key)?;
        let imported = plain.with_extension("toml.imported");
        std::fs::rename(plain, &imported)?;
        tracing::info!(
            from = %plain.display(),
            to = %sealed.display(),
            "plaintext credential store sealed; the original is kept as {}",
            imported.display()
        );
        Ok(broker)
    }

    /// Parse a plaintext store, refusing one readable beyond its owner.
    pub fn parse_plain(path: &Path, text: &str) -> Result<Self, BrokerError> {
        // The whole point of the broker is that nothing but the node's user can
        // read these secrets. A file group- or world-accessible is refused
        // rather than loaded: the documented guarantee is enforcement, not
        // advice. A refused store brokers nothing, which fails closed.
        Self::refuse_if_too_open(path)?;
        toml::from_str(text).map_err(|e| BrokerError::Parse(e.to_string()))
    }

    fn open(bytes: &[u8], key: &DataKey) -> Result<Self, BrokerError> {
        let sealed = Sealed::from_bytes(bytes).map_err(|_| BrokerError::Sealed)?;
        let plain = key
            .open(&sealed, STORE_AAD)
            .map_err(|_| BrokerError::Sealed)?;
        let text = String::from_utf8(plain).map_err(|e| BrokerError::Parse(e.to_string()))?;
        toml::from_str(&text).map_err(|e| BrokerError::Parse(e.to_string()))
    }

    /// Seal and write atomically, owner-only.
    pub fn save(&self, key: &DataKey) -> Result<(), BrokerError> {
        self.save_at(&Self::path(), key)
    }

    pub fn save_at(&self, path: &Path, key: &DataKey) -> Result<(), BrokerError> {
        let text = toml::to_string(self).map_err(|e| BrokerError::Parse(e.to_string()))?;
        let bytes = key.seal(text.as_bytes(), STORE_AAD).to_bytes();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("sealed.tmp");
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            use std::io::Write;
            opts.open(&tmp)?.write_all(&bytes)?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
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

    pub fn get(&self, name: &str) -> Option<&Credential> {
        self.credentials.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Credential)> {
        self.credentials.iter().map(|(n, c)| (n.as_str(), c))
    }

    /// Add or replace. Returns whether a credential of that name existed.
    pub fn put(&mut self, name: &str, cred: Credential) -> bool {
        self.credentials.insert(name.to_string(), cred).is_some()
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.credentials.remove(name).is_some()
    }

    /// Credentials pinned to `node_id`: what an enrollment or a share hands
    /// off. A credential with no `nodes` list is this node's alone and never
    /// travels.
    pub fn bound_to(&self, node_id: &str) -> Vec<(String, Credential)> {
        self.credentials
            .iter()
            .filter(|(_, c)| c.nodes.iter().any(|n| n == node_id))
            .map(|(n, c)| (n.clone(), c.clone()))
            .collect()
    }

    /// Apply a handoff. Each row is `{ "name", "credential" }`; a row not
    /// pinned to this node is dropped — the sender's bindings are a claim, the
    /// receiver's are the rule. Returns how many were stored.
    pub fn apply_handoff(&mut self, self_id: &str, rows: &[Value]) -> usize {
        let mut n = 0;
        for row in rows {
            let Some(name) = row["name"].as_str() else {
                continue;
            };
            let Ok(cred) = serde_json::from_value::<Credential>(row["credential"].clone()) else {
                tracing::warn!(name, "credential handoff row is malformed; dropped");
                continue;
            };
            if cred.nodes.is_empty() || !cred.allows_node(self_id) {
                tracing::warn!(name, "credential handoff not pinned to this node; dropped");
                continue;
            }
            self.credentials.insert(name.to_string(), cred);
            n += 1;
        }
        n
    }

    /// Rows for [`Broker::apply_handoff`] on the other side.
    pub fn handoff_rows(creds: &[(String, Credential)]) -> Vec<Value> {
        creds
            .iter()
            .map(|(n, c)| serde_json::json!({ "name": n, "credential": c }))
            .collect()
    }

    /// Whether any model credential can be injected on this node at all — the
    /// condition for probing the harness's model list.
    pub fn has_model_credential(&self, node_id: &str) -> bool {
        self.credentials
            .values()
            .any(|c| c.is_model() && c.allows_node(node_id))
    }

    /// The only way out of the broker for tools: environment for a process the
    /// node spawns on its own side of the privilege boundary. There is
    /// deliberately no accessor that returns a secret as a value a response
    /// could carry; the model gateway's injection is built inside this module
    /// for the same reason.
    pub fn env_for(
        &self,
        name: &str,
        channel: &str,
        node_id: &str,
    ) -> Result<BTreeMap<String, String>, BrokerError> {
        Ok(self.usable(name, channel, node_id)?.env.clone())
    }

    fn usable(&self, name: &str, channel: &str, node_id: &str) -> Result<&Credential, BrokerError> {
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
        if !cred.allows_node(node_id) {
            return Err(BrokerError::NotOnThisNode {
                name: name.to_string(),
            });
        }
        Ok(cred)
    }

    /// Whether a channel may use a credential on this node at all, for
    /// deciding which tools to offer a session before it asks.
    pub fn available_to(&self, channel: &str, node_id: &str) -> Vec<&str> {
        self.credentials
            .iter()
            .filter(|(_, c)| c.allows(channel) && c.allows_node(node_id))
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

            [credentials.pinned]
            channels = ["work"]
            nodes = ["node-b"]
            [credentials.pinned.env]
            TOKEN = "secret"

            [credentials.anthropic]
            kind = "api_key"
            provider = "anthropic"
            channels = ["personal"]
            [credentials.anthropic.env]
            API_KEY = "sk-x"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn a_bound_channel_gets_the_environment() {
        let env = broker().env_for("warehouse", "work", "node-a").unwrap();
        assert_eq!(env.get("DB_BACKEND").map(String::as_str), Some("sqlite"));
    }

    #[test]
    fn another_channel_is_refused() {
        let err = broker()
            .env_for("warehouse", "personal", "node-a")
            .unwrap_err();
        assert!(matches!(err, BrokerError::NotBound { .. }));
    }

    #[test]
    fn an_unbound_credential_is_unusable_not_universal() {
        // The dangerous default would be "no channels listed means any channel".
        for channel in ["work", "personal", ""] {
            assert!(broker().env_for("orphan", channel, "node-a").is_err());
        }
        assert!(broker()
            .available_to("work", "node-a")
            .contains(&"warehouse"));
        assert!(!broker().available_to("work", "node-a").contains(&"orphan"));
    }

    #[test]
    fn a_credential_pinned_to_a_node_is_unusable_elsewhere() {
        // The store may be copied to another machine; the binding travels
        // with it and refuses there. "consulta on the work node only".
        let b = broker();
        assert!(b.env_for("pinned", "work", "node-b").is_ok());
        assert!(matches!(
            b.env_for("pinned", "work", "node-a").unwrap_err(),
            BrokerError::NotOnThisNode { .. }
        ));
        assert!(b.available_to("work", "node-b").contains(&"pinned"));
        assert!(!b.available_to("work", "node-a").contains(&"pinned"));
    }

    #[test]
    fn an_unknown_credential_is_refused() {
        assert!(matches!(
            broker().env_for("nope", "work", "node-a").unwrap_err(),
            BrokerError::Unknown(_)
        ));
    }

    #[test]
    fn a_missing_store_brokers_nothing() {
        let b = Broker::default();
        assert!(b.is_empty());
        assert!(b.available_to("work", "node-a").is_empty());
    }

    #[test]
    fn kinds_default_to_env_and_model_kinds_are_recognised() {
        let b = broker();
        assert_eq!(b.get("warehouse").unwrap().kind, KIND_ENV);
        assert!(!b.get("warehouse").unwrap().is_model());
        assert!(b.get("anthropic").unwrap().is_model());
        assert!(b.has_model_credential("node-a"));
        assert!(!Broker::default().has_model_credential("node-a"));
    }

    #[test]
    fn the_store_round_trips_sealed_and_is_ciphertext_under_another_key() {
        let dir = std::env::temp_dir().join(format!("tracon-broker-seal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sealed = dir.join("credentials.sealed");
        let plain = dir.join("credentials.toml");
        let key = DataKey::from_bytes([7u8; 32]);
        broker().save_at(&sealed, &key).unwrap();
        let bytes = std::fs::read(&sealed).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("sk-x"));
        let back = Broker::load_at(&sealed, &plain, &key).unwrap();
        assert_eq!(back.get("anthropic"), broker().get("anthropic"));
        assert!(matches!(
            Broker::load_at(&sealed, &plain, &DataKey::from_bytes([8u8; 32])).unwrap_err(),
            BrokerError::Sealed
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&sealed).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plaintext_store_is_sealed_on_first_load_and_set_aside() {
        let dir = std::env::temp_dir().join(format!("tracon-broker-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sealed = dir.join("credentials.sealed");
        let plain = dir.join("credentials.toml");
        std::fs::write(
            &plain,
            "[credentials.x]\nchannels=[\"work\"]\n[credentials.x.env]\nT=\"1\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let key = DataKey::from_bytes([7u8; 32]);
        let b = Broker::load_at(&sealed, &plain, &key).unwrap();
        assert!(b.get("x").is_some());
        assert!(sealed.exists());
        assert!(!plain.exists());
        assert!(dir.join("credentials.toml.imported").exists());
        // Second load reads the sealed store.
        assert!(Broker::load_at(&sealed, &plain, &key)
            .unwrap()
            .get("x")
            .is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_handoff_keeps_only_rows_pinned_to_the_receiver() {
        let mut b = Broker::default();
        let pinned = Credential {
            channels: vec!["work".into()],
            nodes: vec!["node-b".into()],
            ..Default::default()
        };
        let loose = Credential {
            channels: vec!["work".into()],
            ..Default::default()
        };
        let rows = Broker::handoff_rows(&[("p".into(), pinned), ("l".into(), loose)]);
        assert_eq!(b.apply_handoff("node-b", &rows), 1);
        assert!(b.get("p").is_some() && b.get("l").is_none());
        assert_eq!(Broker::default().apply_handoff("node-a", &rows), 0);
        assert_eq!(b.bound_to("node-b").len(), 1);
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
