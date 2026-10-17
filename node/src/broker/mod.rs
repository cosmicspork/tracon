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
    #[error(
        "credential {name} does not name repository {repo} in its `repos`; add the \
         repository's path, or its group's as `group/**`"
    )]
    NotForRepo { name: String, repo: String },
    #[error("credential {name} is a model credential, not environment")]
    NotEnv { name: String },
    #[error("credentials {first} and {second} both set {key}")]
    Overlap {
        first: String,
        second: String,
        key: String,
    },
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
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
    /// Repositories, written as a `[[repo]]` entry's `path`, whose
    /// default-branch preparation may be lent this credential's `env` when
    /// their entry names it in `prepare_credentials`. An entry ending in
    /// `/**` names every repository under that group instead. Empty means
    /// none: the entry naming a credential does not release it on its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
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
    /// For `oauth`: the sign-in these tokens descend from, a UUIDv7, so a
    /// later sign-in sorts after an earlier one. Absent on a credential
    /// written before sign-ins were tracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    /// For `oauth`: how many refreshes this sign-in has been through. Every
    /// holder of a shared credential keeps the highest it has seen.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub grant_version: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("channels", &self.channels)
            .field("nodes", &self.nodes)
            .field("repos", &self.repos)
            .field("kind", &self.kind)
            .field("provider", &self.provider)
            .field("expires_ms", &self.expires_ms)
            .field("identity", &self.identity)
            .field("grant_id", &self.grant_id)
            .field("grant_version", &self.grant_version)
            .finish()
    }
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
            repos: Vec::new(),
            kind: default_kind(),
            provider: None,
            expires_ms: None,
            identity: None,
            grant_id: None,
            grant_version: 0,
        }
    }
}

impl Credential {
    pub fn allows(&self, channel: &str) -> bool {
        self.channels.iter().any(|c| c == channel)
    }

    /// Whether this copy of a shared OAuth credential is newer than `other`:
    /// a later sign-in, or the same sign-in refreshed more times. A copy that
    /// names no sign-in orders before one that does.
    pub fn supersedes(&self, other: &Credential) -> bool {
        (self.grant_id.as_deref(), self.grant_version)
            > (other.grant_id.as_deref(), other.grant_version)
    }

    pub fn allows_node(&self, node_id: &str) -> bool {
        self.nodes.is_empty() || self.nodes.iter().any(|n| n == node_id)
    }

    /// A credential the model gateway injects, as opposed to one a tool
    /// consumes as environment.
    pub fn is_model(&self) -> bool {
        self.kind == KIND_API_KEY || self.kind == KIND_OAUTH
    }

    /// Whether some entry in `repos` names the repository rooted at `repo`.
    pub fn names_repo(&self, repo: &Path) -> bool {
        self.repos
            .iter()
            .any(|bound| repos_entry_names(bound, repo))
    }

    /// The `[[repo]]` entries, by `path`, whose repositories `repos` names
    /// now, for showing what a group prefix reaches. An entry is resolved to
    /// the `clones` it matches, or read as its own root when it matches none.
    pub fn covers(&self, entries: &[crate::config::Repo], clones: &[PathBuf]) -> Vec<String> {
        entries
            .iter()
            .filter(|entry| {
                let mut roots = clones
                    .iter()
                    .filter(|clone| entry.matches(clone))
                    .peekable();
                match roots.peek() {
                    Some(_) => roots.any(|root| self.names_repo(root)),
                    None => self.names_repo(&crate::config::expand_home(&entry.path)),
                }
            })
            .map(|entry| entry.path.display().to_string())
            .collect()
    }

    /// Refuse a `repos` entry that globs anywhere but a final `/**`, so one
    /// that looks wider or narrower than it is never reaches the store.
    fn check_repos(&self) -> Result<(), String> {
        for bound in &self.repos {
            let prefix = bound.strip_suffix("/**").unwrap_or(bound);
            if prefix.contains('*') {
                return Err(format!(
                    "repos entry {bound:?}: `**` is allowed only as the last segment, as in \
                     `gitlab.com/group/**`, and no other wildcard is"
                ));
            }
            let named = Path::new(prefix)
                .components()
                .any(|part| matches!(part, std::path::Component::Normal(_)));
            if !named {
                return Err(format!(
                    "repos entry {bound:?} names no repository or group; a prefix is written \
                     as `gitlab.com/group/**`"
                ));
            }
        }
        Ok(())
    }
}

/// Whether `bound`, one entry in a credential's `repos`, names the
/// repository rooted at `repo`. Without a wildcard it matches as a
/// `[[repo]]` path does. Ending in `/**` it names every repository strictly
/// under the prefix, matched by whole segment as the same rule matches the
/// prefix: an absolute prefix must lead the root, a relative one must end
/// one of its ancestors. Any other glob matches nothing.
fn repos_entry_names(bound: &str, repo: &Path) -> bool {
    let Some(prefix) = bound.strip_suffix("/**") else {
        return !bound.contains('*') && crate::config::repo_path_matches(Path::new(bound), repo);
    };
    if prefix.contains('*') {
        return false;
    }
    let prefix = crate::config::expand_home(Path::new(prefix));
    let want: Vec<_> = prefix.components().collect();
    let have: Vec<_> = repo.components().collect();
    if !want
        .iter()
        .any(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return false;
    }
    // Component-wise, never textual: `group/**` must not match `group2/x`.
    if prefix.is_absolute() {
        return have.len() > want.len() && have[..want.len()] == want[..];
    }
    (want.len()..have.len()).any(|end| have[end - want.len()..end] == want[..])
}

/// Headers the model gateway writes in place of the harness's placeholder.
/// Built inside the broker so the secret leaves it only as a header bound for
/// the provider, never as a value a response could carry.
#[derive(Clone, Default)]
pub struct Injection {
    pub authorization: Option<String>,
    pub x_api_key: Option<String>,
    /// An Anthropic subscription token: the gateway merges the OAuth beta flag.
    pub oauth_beta: bool,
    pub chatgpt_account_id: Option<String>,
}

impl std::fmt::Debug for Injection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Injection")
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "<redacted>"),
            )
            .field("x_api_key", &self.x_api_key.as_ref().map(|_| "<redacted>"))
            .field("oauth_beta", &self.oauth_beta)
            .field(
                "chatgpt_account_id",
                &self.chatgpt_account_id.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Loaded at startup from a store only the node can open, and written back
/// whenever a login, an import, or a handoff changes it.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Broker {
    #[serde(default)]
    credentials: BTreeMap<String, Credential>,
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Broker")
            .field("credentials", &self.credentials.keys().collect::<Vec<_>>())
            .finish()
    }
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
        Self::parse_text(text)
    }

    /// Parse a plaintext store that never was a file. The mode check exists
    /// to stop a secret sitting readable on disk; text handed straight to the
    /// node has no mode to check, and is never written down.
    pub fn parse_text(text: &str) -> Result<Self, BrokerError> {
        let broker: Self = toml::from_str(text).map_err(|e| BrokerError::Parse(e.to_string()))?;
        for (name, cred) in &broker.credentials {
            cred.check_repos()
                .map_err(|e| BrokerError::Parse(format!("credential {name}: {e}")))?;
        }
        Ok(broker)
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
        let tmp = path.with_extension(format!("sealed.{}.tmp", uuid::Uuid::now_v7()));
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
        if let Err(error) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
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

    /// What the interface may see of each credential: names, kinds, and
    /// bindings, plus the env *key names* alone — never a value, keeping the
    /// module's rule that no accessor returns a secret a response could carry.
    pub fn summaries(&self) -> Vec<Value> {
        self.credentials
            .iter()
            .map(|(name, c)| {
                serde_json::json!({
                    "name": name,
                    "kind": c.kind,
                    "provider": c.provider,
                    "channels": c.channels,
                    "nodes": c.nodes,
                    "repos": c.repos,
                    "identity": c.identity,
                    "expires_ms": c.expires_ms,
                    "env_keys": c.env.keys().collect::<Vec<_>>(),
                })
            })
            .collect()
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
    /// receiver's are the rule. A tracked sign-in replaces what is here only
    /// when it supersedes it, so a delayed or replayed copy never undoes a
    /// refresh. Returns the names stored.
    pub fn apply_handoff(&mut self, self_id: &str, rows: &[Value]) -> Vec<String> {
        let mut stored = Vec::new();
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
            if let Err(error) = cred.check_repos() {
                tracing::warn!(name, %error, "credential handoff names repositories wrongly; dropped");
                continue;
            }
            if cred.grant_id.is_some()
                && self
                    .credentials
                    .get(name)
                    .is_some_and(|held| !cred.supersedes(held))
            {
                tracing::info!(
                    name,
                    "credential handoff is not newer than the copy held; dropped"
                );
                continue;
            }
            self.credentials.insert(name.to_string(), cred);
            stored.push(name.to_string());
        }
        stored
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

    /// Environment for the preparation that warms `repo`'s base cache from its
    /// default branch, the one preparation that runs no code an agent wrote.
    /// Bound by the repository rather than by a channel: what it fetches is
    /// copied into every channel's sessions on that repository, and no
    /// session ever runs with the credential. Only an `env` credential is
    /// lent, and only to a repository it names itself.
    pub fn preparation_env(
        &self,
        name: &str,
        repo: &Path,
        node_id: &str,
    ) -> Result<BTreeMap<String, String>, BrokerError> {
        let cred = self
            .credentials
            .get(name)
            .ok_or_else(|| BrokerError::Unknown(name.to_string()))?;
        if !cred.allows_node(node_id) {
            return Err(BrokerError::NotOnThisNode {
                name: name.to_string(),
            });
        }
        if cred.kind != KIND_ENV {
            return Err(BrokerError::NotEnv {
                name: name.to_string(),
            });
        }
        if !cred.names_repo(repo) {
            return Err(BrokerError::NotForRepo {
                name: name.to_string(),
                repo: repo.display().to_string(),
            });
        }
        Ok(cred.env.clone())
    }

    /// The gateway's way out: headers for one provider request on behalf of a
    /// session in `channel`. Same bindings as `env_for`.
    pub fn inject_for(
        &self,
        name: &str,
        channel: &str,
        node_id: &str,
        shape: &str,
    ) -> Result<Injection, BrokerError> {
        let cred = self.usable(name, channel, node_id)?;
        Self::injection(cred, shape)
    }

    /// For the node's own model probe, which has no channel: the credential
    /// must at least be usable by some channel on this node.
    pub fn inject_for_probe(
        &self,
        name: &str,
        node_id: &str,
        shape: &str,
    ) -> Result<Injection, BrokerError> {
        Self::injection(self.usable_by_node(name, node_id)?, shape)
    }

    /// Environment for the node's own Git fetch of a clone it manages, which
    /// has no channel: refreshing what is already on disk before building
    /// from it. Same binding as the model probe: the credential must at least
    /// be usable by some channel on this node. The process it is given to
    /// runs no code from the repository.
    pub fn env_for_node(
        &self,
        name: &str,
        node_id: &str,
    ) -> Result<BTreeMap<String, String>, BrokerError> {
        Ok(self.usable_by_node(name, node_id)?.env.clone())
    }

    fn usable_by_node(&self, name: &str, node_id: &str) -> Result<&Credential, BrokerError> {
        let cred = self
            .credentials
            .get(name)
            .ok_or_else(|| BrokerError::Unknown(name.to_string()))?;
        if cred.channels.is_empty() {
            return Err(BrokerError::NotBound {
                name: name.to_string(),
                channel: "(any)".into(),
            });
        }
        if !cred.allows_node(node_id) {
            return Err(BrokerError::NotOnThisNode {
                name: name.to_string(),
            });
        }
        Ok(cred)
    }

    fn injection(cred: &Credential, shape: &str) -> Result<Injection, BrokerError> {
        let missing = |k: &str| BrokerError::Parse(format!("credential has no {k}"));
        match cred.kind.as_str() {
            KIND_API_KEY => {
                let key = cred.env.get("API_KEY").ok_or_else(|| missing("API_KEY"))?;
                Ok(if shape == "anthropic" {
                    Injection {
                        x_api_key: Some(key.clone()),
                        ..Default::default()
                    }
                } else {
                    Injection {
                        authorization: Some(format!("Bearer {key}")),
                        ..Default::default()
                    }
                })
            }
            KIND_OAUTH => {
                let token = cred
                    .env
                    .get("ACCESS_TOKEN")
                    .ok_or_else(|| missing("ACCESS_TOKEN"))?;
                let chatgpt_account_id = if shape == "openai-codex" {
                    let account = cred
                        .env
                        .get("CHATGPT_ACCOUNT_ID")
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| missing("CHATGPT_ACCOUNT_ID"))?;
                    axum::http::HeaderValue::from_str(account).map_err(|_| {
                        BrokerError::Parse("credential has an invalid CHATGPT_ACCOUNT_ID".into())
                    })?;
                    Some(account.clone())
                } else {
                    None
                };
                Ok(Injection {
                    authorization: Some(format!("Bearer {token}")),
                    oauth_beta: shape == "anthropic",
                    chatgpt_account_id,
                    ..Default::default()
                })
            }
            other => Err(BrokerError::Parse(format!(
                "credential kind {other} cannot be injected into a model request"
            ))),
        }
    }

    /// The first model credential for `provider` usable on this node, for
    /// reporting which providers are connected.
    pub fn model_credential_for(
        &self,
        provider: &str,
        node_id: &str,
    ) -> Option<(&str, &Credential)> {
        self.credentials
            .iter()
            .find(|(_, c)| {
                c.is_model() && c.provider.as_deref() == Some(provider) && c.allows_node(node_id)
            })
            .map(|(n, c)| (n.as_str(), c))
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

    /// Every credential `names` lends `repo`'s default-branch preparation, as
    /// one environment: all of them or none, and refused when two set the
    /// same variable, since which one won would be an accident of order.
    pub fn preparation_env_all(
        &self,
        names: &[String],
        repo: &Path,
        node_id: &str,
    ) -> Result<Vec<(String, String)>, BrokerError> {
        let mut env: BTreeMap<String, (String, &str)> = BTreeMap::new();
        for name in names {
            for (key, value) in self.preparation_env(name, repo, node_id)? {
                if let Some((_, first)) = env.get(&key) {
                    return Err(BrokerError::Overlap {
                        first: first.to_string(),
                        second: name.clone(),
                        key,
                    });
                }
                env.insert(key, (value, name));
            }
        }
        Ok(env
            .into_iter()
            .map(|(key, (value, _))| (key, value))
            .collect())
    }
}

/// The node's broker and id, for the one reader no request carries them to:
/// the warm-up of a repository's base cache, which an image build starts.
/// Installed once by `serve`; a process that never serves lends nothing.
static LENT_TO_PREPARATION: std::sync::OnceLock<(SharedBroker, String)> =
    std::sync::OnceLock::new();

/// Make this the broker preparation borrows from. The first wins.
pub fn lend_to_preparation(broker: SharedBroker, node_id: &str) {
    let _ = LENT_TO_PREPARATION.set((broker, node_id.to_string()));
}

/// The environment `names` lend `repo`'s default-branch preparation, read from
/// the broker as it is now, so a credential imported in Settings applies to
/// the next warm-up without a restart.
pub fn preparation_env_for(repo: &Path, names: &[String]) -> Result<Vec<(String, String)>, String> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let (broker, node_id) = LENT_TO_PREPARATION
        .get()
        .ok_or("the node's credential broker is not loaded in this process")?;
    let broker = broker
        .read()
        .map_err(|_| "the credential broker is unavailable".to_string())?;
    broker
        .preparation_env_all(names, repo, node_id)
        .map_err(|error| error.to_string())
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

    /// The node's own fetch of a managed clone has no channel: any channel's
    /// binding on this node will do, but an unbound or pinned-elsewhere
    /// credential is still refused.
    #[test]
    fn the_nodes_own_fetch_takes_a_credential_some_channel_may_use_here() {
        let b = broker();
        assert!(b.env_for_node("warehouse", "node-a").is_ok());
        assert!(matches!(
            b.env_for_node("orphan", "node-a").unwrap_err(),
            BrokerError::NotBound { .. }
        ));
        assert!(matches!(
            b.env_for_node("pinned", "node-a").unwrap_err(),
            BrokerError::NotOnThisNode { .. }
        ));
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

    /// A registry credential reaches a repository's default-branch
    /// preparation only when the credential itself names that repository:
    /// a `[[repo]]` entry naming it is not enough, a channel binding is not a
    /// repository binding, and a model credential is never lent as
    /// environment.
    #[test]
    fn preparation_is_lent_only_what_names_its_repository() {
        let b: Broker = toml::from_str(
            r#"
            [credentials.nova]
            repos = ["github.com/acme/shop"]
            [credentials.nova.env]
            COMPOSER_AUTH = '{"http-basic":{"nova.laravel.com":{"username":"a@b.c","password":"nova-licence-key"}}}'

            [credentials.npm]
            repos = ["/src/shop"]
            [credentials.npm.env]
            NPM_TOKEN = "npm_abc"

            [credentials.clash]
            repos = ["github.com/acme/shop"]
            [credentials.clash.env]
            COMPOSER_AUTH = "{}"

            [credentials.channel_only]
            channels = ["work"]
            [credentials.channel_only.env]
            TOKEN = "t"

            [credentials.model]
            kind = "api_key"
            provider = "anthropic"
            repos = ["/src/shop"]
            [credentials.model.env]
            API_KEY = "sk-x"
            "#,
        )
        .unwrap();
        let clone = Path::new("/var/lib/tracon/repos/github.com/acme/shop");
        let env = b.preparation_env("nova", clone, "node-a").unwrap();
        assert!(env["COMPOSER_AUTH"].contains("nova.laravel.com"));
        assert!(b
            .preparation_env("npm", Path::new("/src/shop"), "node-a")
            .is_ok());
        // Matched as a `[[repo]]` path is: whole components, never characters.
        assert!(matches!(
            b.preparation_env("nova", Path::new("/r/github.com/acme/shop2"), "node-a"),
            Err(BrokerError::NotForRepo { .. })
        ));
        assert!(matches!(
            b.preparation_env("channel_only", clone, "node-a"),
            Err(BrokerError::NotForRepo { .. })
        ));
        assert!(matches!(
            b.preparation_env("model", Path::new("/src/shop"), "node-a"),
            Err(BrokerError::NotEnv { .. })
        ));
        assert!(matches!(
            b.preparation_env("absent", clone, "node-a"),
            Err(BrokerError::Unknown(_))
        ));

        // Several are one environment, all or nothing.
        let names = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let both = Path::new("/src/shop");
        let mut b = b;
        let mut nova = b.get("nova").unwrap().clone();
        nova.repos.push("/src/shop".into());
        b.put("nova", nova);
        let env = b
            .preparation_env_all(&names(&["nova", "npm"]), both, "node-a")
            .unwrap();
        let keys: Vec<&str> = env.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["COMPOSER_AUTH", "NPM_TOKEN"]);
        assert!(matches!(
            b.preparation_env_all(&names(&["nova", "channel_only"]), both, "node-a"),
            Err(BrokerError::NotForRepo { .. })
        ));
        assert!(matches!(
            b.preparation_env_all(&names(&["nova", "clash"]), clone, "node-a"),
            Err(BrokerError::Overlap { .. })
        ));

        // The binding is shown with the rest, and a value never is.
        let summaries = serde_json::to_string(&b.summaries()).unwrap();
        assert!(summaries.contains("github.com/acme/shop"));
        assert!(!summaries.contains("npm_abc") && !summaries.contains("nova-licence-key"));
        assert!(!format!("{b:?}").contains("npm_abc"));
    }

    /// One credential lent to a forge group: `group/**` names every
    /// repository under the group, by whole segment, with the prefix matched
    /// as a `[[repo]]` path is (absolute leads the root, relative ends an
    /// ancestor), and never the group itself.
    #[test]
    fn a_group_prefix_names_every_repository_under_it_by_whole_segment() {
        let b = Broker::parse_text(
            r#"
            [credentials.composer]
            repos = ["gitlab.com/unebraska/uitao/**"]
            [credentials.composer.env]
            COMPOSER_AUTH = "{}"

            [credentials.local]
            repos = ["/src/uitao/**"]
            [credentials.local.env]
            NPM_TOKEN = "t"
            "#,
        )
        .unwrap();
        let lent =
            |name: &str, repo: &str| b.preparation_env(name, Path::new(repo), "node-a").is_ok();
        let clones = "/var/lib/tracon/repos/gitlab.com/unebraska/uitao";
        assert!(lent(
            "composer",
            &format!("{clones}/custom-development/integrations")
        ));
        assert!(lent("composer", &format!("{clones}/portal")));
        assert!(lent("composer", "gitlab.com/unebraska/uitao/portal"));
        // Segments, never characters, and the group is not one of its repositories.
        assert!(!lent(
            "composer",
            "/var/lib/tracon/repos/gitlab.com/unebraska/uitao2/portal"
        ));
        assert!(!lent(
            "composer",
            "/var/lib/tracon/repos/gitlab.com/unebraska/xuitao/portal"
        ));
        assert!(!lent("composer", clones));
        assert!(!lent(
            "composer",
            "/var/lib/tracon/repos/gitlab.com/unebraska/portal"
        ));
        // An absolute prefix must lead the root; it is not found further down.
        assert!(lent("local", "/src/uitao/portal"));
        assert!(lent("local", "/src/uitao/group/portal"));
        assert!(!lent("local", "/home/me/src/uitao/portal"));
        assert!(!lent("local", "/src/uitao2/portal"));
        assert!(!lent("local", "/src/uitao"));
    }

    /// `**` is the last segment or nothing: anywhere else, any other
    /// wildcard, or a prefix naming no group is refused when the store is
    /// read in, never stored to match something other than it looks.
    #[test]
    fn a_repos_wildcard_anywhere_but_the_last_segment_is_refused_at_import() {
        let store = |bound: &str| {
            Broker::parse_text(&format!(
                "[credentials.c]\nrepos = [{bound:?}]\n[credentials.c.env]\nK = \"v\"\n"
            ))
        };
        for bound in [
            "gitlab.com/**/uitao",
            "**/uitao",
            "gitlab.com/uit*/**",
            "gitlab.com/uitao/*",
            "gitlab.com/uitao**",
            "**",
            "/**",
        ] {
            let error = store(bound).unwrap_err().to_string();
            assert!(
                error.contains("credential c") && error.contains("**"),
                "{bound}: {error}"
            );
        }
        for bound in [
            "gitlab.com/uitao/**",
            "/src/uitao/**",
            "github.com/acme/shop",
        ] {
            assert!(store(bound).is_ok(), "{bound}");
        }
        // A copy handed over by a peer is held to the same rule.
        let mut b = Broker::default();
        let rows = vec![serde_json::json!({
            "name": "c",
            "credential": { "nodes": ["node-a"], "repos": ["a/**/b"], "env": { "K": "v" } },
        })];
        assert!(b.apply_handoff("node-a", &rows).is_empty());
    }

    /// A repository no entry names is refused in words that point at both
    /// fixes: its own path, or its group's as a prefix.
    #[test]
    fn a_repository_no_entry_names_is_refused_naming_the_group_prefix_form() {
        let b = Broker::parse_text(
            "[credentials.c]\nrepos = [\"gitlab.com/other/**\"]\n[credentials.c.env]\nK = \"v\"\n",
        )
        .unwrap();
        let error = b
            .preparation_env("c", Path::new("/r/gitlab.com/uitao/portal"), "node-a")
            .unwrap_err();
        assert!(matches!(error, BrokerError::NotForRepo { .. }));
        let message = error.to_string();
        assert!(message.contains("/r/gitlab.com/uitao/portal"), "{message}");
        assert!(message.contains("`group/**`"), "{message}");
    }

    /// Settings shows what a prefix reaches: the configured entries whose
    /// repositories it names, resolved through the clones they match.
    #[test]
    fn a_credential_covers_the_configured_repositories_its_repos_name() {
        let cred = Credential {
            repos: vec!["gitlab.com/uitao/**".into(), "/src/shop".into()],
            ..Default::default()
        };
        let entry = |path: &str| crate::config::Repo {
            path: path.into(),
            ..Default::default()
        };
        let entries = [
            entry("uitao/portal"),
            entry("gitlab.com/uitao/api"),
            entry("/src/shop"),
            entry("gitlab.com/elsewhere/api"),
        ];
        let clones = [PathBuf::from(
            "/var/lib/tracon/repos/gitlab.com/uitao/portal",
        )];
        assert_eq!(
            cred.covers(&entries, &clones),
            ["uitao/portal", "gitlab.com/uitao/api", "/src/shop"]
        );
    }

    #[test]
    fn a_missing_store_brokers_nothing() {
        let b = Broker::default();
        assert!(b.is_empty());
        assert!(b.available_to("work", "node-a").is_empty());
    }

    #[test]
    fn injection_is_shaped_by_kind_and_provider_and_never_for_env() {
        let b = broker();
        let i = b
            .inject_for("anthropic", "personal", "node-a", "anthropic")
            .unwrap();
        assert_eq!(i.x_api_key.as_deref(), Some("sk-x"));
        assert!(i.authorization.is_none() && !i.oauth_beta);
        let i = b
            .inject_for("anthropic", "personal", "node-a", "openai")
            .unwrap();
        assert_eq!(i.authorization.as_deref(), Some("Bearer sk-x"));
        assert!(matches!(
            b.inject_for("anthropic", "work", "node-a", "anthropic")
                .unwrap_err(),
            BrokerError::NotBound { .. }
        ));
        assert!(b
            .inject_for("warehouse", "work", "node-a", "openai")
            .is_err());
        let mut oauth = Credential {
            kind: KIND_OAUTH.into(),
            provider: Some("anthropic".into()),
            channels: vec!["work".into()],
            ..Default::default()
        };
        oauth.env.insert("ACCESS_TOKEN".into(), "at".into());
        oauth
            .env
            .insert("CHATGPT_ACCOUNT_ID".into(), "acct-123".into());
        let mut b = Broker::default();
        b.put("max", oauth);
        let i = b.inject_for("max", "work", "node-a", "anthropic").unwrap();
        assert_eq!(i.authorization.as_deref(), Some("Bearer at"));
        assert!(i.oauth_beta);
        let codex = b
            .inject_for("max", "work", "node-a", "openai-codex")
            .unwrap();
        assert_eq!(codex.authorization.as_deref(), Some("Bearer at"));
        assert_eq!(codex.chatgpt_account_id.as_deref(), Some("acct-123"));
        assert!(b.inject_for_probe("max", "node-a", "anthropic").is_ok());
        assert!(b.model_credential_for("anthropic", "node-a").is_some());
        assert!(b.model_credential_for("openai", "node-a").is_none());
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
        assert_eq!(b.apply_handoff("node-b", &rows), ["p"]);
        assert!(b.get("p").is_some() && b.get("l").is_none());
        assert!(Broker::default().apply_handoff("node-a", &rows).is_empty());
        assert_eq!(b.bound_to("node-b").len(), 1);
    }

    /// A refreshed copy arrives after, or is replayed behind, a newer one: only
    /// a later sign-in or more refreshes of the same one replaces what is held.
    #[test]
    fn a_handoff_never_replaces_a_newer_sign_in_or_refresh() {
        let copy = |grant: &str, version: u64, token: &str| {
            let mut c = Credential {
                kind: KIND_OAUTH.into(),
                nodes: vec!["node-a".into(), "node-b".into()],
                grant_id: Some(grant.into()),
                grant_version: version,
                ..Default::default()
            };
            c.env.insert("ACCESS_TOKEN".into(), token.into());
            Broker::handoff_rows(&[("anthropic".into(), c)])
        };
        let token = |b: &Broker| b.get("anthropic").unwrap().env["ACCESS_TOKEN"].clone();
        let mut b = Broker::default();
        assert_eq!(b.apply_handoff("node-b", &copy("0190-a", 2, "v2")).len(), 1);
        assert!(b
            .apply_handoff("node-b", &copy("0190-a", 1, "v1"))
            .is_empty());
        assert!(b
            .apply_handoff("node-b", &copy("0190-a", 2, "v2-again"))
            .is_empty());
        assert_eq!(token(&b), "v2");
        assert_eq!(b.apply_handoff("node-b", &copy("0190-a", 3, "v3")).len(), 1);
        // A new sign-in wins over any refresh of an older one, and an older
        // sign-in never comes back.
        assert_eq!(
            b.apply_handoff("node-b", &copy("0191-b", 0, "new")).len(),
            1
        );
        assert!(b
            .apply_handoff("node-b", &copy("0190-a", 9, "old"))
            .is_empty());
        assert_eq!(token(&b), "new");
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

    #[test]
    fn secret_bearing_debug_output_is_redacted() {
        let mut credential = Credential::default();
        credential
            .env
            .insert("TOKEN".into(), "credential-secret".into());
        let injection = Injection {
            authorization: Some("Bearer authorization-secret".into()),
            x_api_key: Some("api-secret".into()),
            chatgpt_account_id: Some("account-secret".into()),
            ..Default::default()
        };
        let mut broker = Broker::default();
        broker.put("forge", credential.clone());
        let debug = format!("{credential:?} {injection:?} {broker:?}");
        for secret in [
            "credential-secret",
            "authorization-secret",
            "api-secret",
            "account-secret",
        ] {
            assert!(!debug.contains(secret), "{debug}");
        }
        assert!(debug.contains("TOKEN"));
        assert!(debug.contains("forge"));
    }
}
