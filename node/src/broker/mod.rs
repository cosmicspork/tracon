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
    cmp::Ordering,
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
    /// The node that minted `grant_id`: where it was signed in, shared, or
    /// first renewed. It travels with every handoff, so each holder knows
    /// whose sign-in its copy is, and only that node's revoke drops it.
    /// Absent on a credential put here that was never shared, and on one
    /// from before origins were recorded: [`Self::origin`] reads those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_origin: Option<String>,
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
            .field("grant_origin", &self.grant_origin)
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
            grant_origin: None,
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

    /// Whether this copy is of a later sign-in than `grant_id`: what lets a
    /// credential past a tombstone or a revoke. The same sign-in refreshed
    /// is not newer, so a hand-back of a removed credential stays removed.
    /// Grants are UUIDv7s minted on different nodes, so between two nodes
    /// whose clocks disagree the skew decides which sign-in is later; a
    /// re-share or reconnect within that skew of a removal may be refused,
    /// and putting the credential here again always passes.
    pub fn newer_grant_than(&self, grant_id: Option<&str>) -> bool {
        self.grant_id.as_deref() > grant_id
    }

    /// Whose sign-in this copy is: [`Self::grant_origin`], or for a copy
    /// from before origins were recorded, the first node `nodes` lists,
    /// which is the node that signed in or shared it (a share lists the
    /// sharer first, and renewal without a claim already relies on it).
    pub fn origin(&self) -> Option<&str> {
        self.grant_origin
            .as_deref()
            .or_else(|| self.nodes.first().map(String::as_str))
    }

    /// Whether removing this credential at `node_id` is the origin removing
    /// its sign-in, which revokes every other holder's copy, rather than one
    /// holder dropping its own. Read through [`Self::origin`], so a copy
    /// from before origins were recorded is the first listed node's, as the
    /// share refusal reads it; a copy that names no origin and lists no
    /// nodes is `node_id`'s.
    pub fn originates_at(&self, node_id: &str) -> bool {
        self.origin().is_none_or(|o| o == node_id)
    }

    /// The origin of a copy held at `node_id`, when that is another node:
    /// [`Self::origin`], reading the first node `nodes` lists for a copy that
    /// records none. Such a copy is not `node_id`'s to share. Its origin's
    /// revoke reaches only the nodes the origin lists, and `node_id`'s is
    /// obeyed nowhere, so a copy shared on would outlive every revoke; and a
    /// share minting a sign-in for it would take it from the origin.
    pub fn shared_from(&self, node_id: &str) -> Option<&str> {
        self.origin().filter(|origin| *origin != node_id)
    }

    /// The nodes owed a revoke when `node_id` removes this credential: every
    /// other node it lists, when `node_id` is its origin, and none when it
    /// is not, since only the origin's revoke is obeyed.
    pub fn revoke_targets(&self, node_id: &str) -> Vec<String> {
        if !self.originates_at(node_id) {
            return Vec::new();
        }
        self.nodes
            .iter()
            .filter(|node| *node != node_id)
            .cloned()
            .collect()
    }

    /// Give a credential that names no sign-in one, minted at `node_id`,
    /// before it is shared, so a revoke and a tombstone can tell this share
    /// from a later one and every holder knows whose it is. OAuth too: a
    /// copy shared without one would get its identity at its first renewal,
    /// which a tombstone of the unnamed copy could not tell from a new
    /// sign-in. Returns whether it changed.
    pub fn ensure_grant(&mut self, node_id: &str) -> bool {
        if self.grant_id.is_some() {
            return false;
        }
        self.grant_id = Some(uuid::Uuid::now_v7().to_string());
        self.grant_origin = Some(node_id.to_string());
        true
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

    /// Refuse a `repos` entry that globs anywhere but a final `/**`, or
    /// whose prefix steps through `.` or `..`, so one that looks wider or
    /// narrower than it is never reaches the store.
    fn check_repos(&self) -> Result<(), String> {
        for bound in &self.repos {
            let prefix = bound.strip_suffix("/**").unwrap_or(bound);
            if prefix.contains('*') {
                return Err(format!(
                    "repos entry {bound:?}: `**` is allowed only as the last segment, as in \
                     `gitlab.com/group/**`, and no other wildcard is"
                ));
            }
            // Read textually: `Path::components` drops an inner `.`.
            if bound.ends_with("/**") && prefix.split('/').any(|part| part == "." || part == "..") {
                return Err(format!(
                    "repos entry {bound:?}: a `group/**` prefix is written without `.` or `..` \
                     segments"
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

/// Whether the credential `name` is one of provider `entry`'s sign-ins under
/// `base` ([`Broker::family`]). `base` is, whatever it holds, as it always
/// was; a further sign-in must say it is `entry`'s, so a `base@…` name an
/// operator gave something else is never injected for the provider.
pub fn in_family(name: &str, cred: &Credential, entry: &str, base: &str) -> bool {
    name == base
        || (is_sign_in_name(name, base)
            && cred.is_model()
            && cred.provider.as_deref() == Some(entry))
}

/// Whether `name` is written as a further sign-in under `base`, whatever it
/// holds: what a revoke, which carries only names, is matched by.
pub fn is_sign_in_name(name: &str, base: &str) -> bool {
    name.strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('@'))
        .is_some_and(|suffix| !suffix.is_empty())
}

/// The order sign-ins serving one channel are chosen in: one that
/// originates at `node_id` first, then the later sign-in, then the name, so
/// the choice never rests on the map's order alone.
fn precedence(a: &(&str, &Credential), b: &(&str, &Credential), node_id: &str) -> Ordering {
    let key = |(name, cred): &(&str, &Credential)| {
        (
            cred.originates_at(node_id),
            cred.grant_id.clone(),
            cred.grant_version,
            std::cmp::Reverse(name.to_string()),
        )
    };
    key(a).cmp(&key(b))
}

/// Whether `bound`, one entry in a credential's `repos`, names the
/// repository rooted at `repo`. Without a wildcard it matches as a
/// `[[repo]]` path does. Ending in `/**` it names every repository strictly
/// under the prefix, matched by whole segment as the same rule matches the
/// prefix: an absolute prefix must lead the root, a relative one must end
/// one of its ancestors. Any other glob matches nothing, and so does a root
/// that steps through `..`, which could climb out from under the prefix.
fn repos_entry_names(bound: &str, repo: &Path) -> bool {
    if repo
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        return false;
    }
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

/// What a removed credential leaves behind: the sign-in it was, and whose,
/// so a hand-back of that sign-in is refused. Tombstones never expire; only
/// a copy of a later sign-in, or the credential put here again, passes one.
/// A tombstone of a copy that named no sign-in also refuses every copy from
/// its origin: that origin's first renewal mints a sign-in for it, which
/// would otherwise pass as a later one. One the origin's own revoke left
/// names no origin, since nothing of the origin's is left to renew, and
/// refuses only copies that name no sign-in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub grant_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl Tombstone {
    fn of(credential: &Credential) -> Self {
        Self {
            grant_id: credential.grant_id.clone(),
            grant_version: credential.grant_version,
            origin: credential.origin().map(str::to_string),
        }
    }

    fn key(&self) -> (Option<&str>, u64) {
        (self.grant_id.as_deref(), self.grant_version)
    }

    /// Whether a handoff of `copy` is of the sign-in this removed.
    fn refuses(&self, copy: &Credential) -> bool {
        !copy.newer_grant_than(self.grant_id.as_deref())
            || (self.grant_id.is_none()
                && self.origin.is_some()
                && copy.origin() == self.origin.as_deref())
    }
}

/// What a revoke from a peer came to here. Each one is answered with it, so
/// the sender can tell a peer that dropped its copy from one that kept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoked {
    /// The copy of that peer's sign-in was dropped.
    Dropped,
    /// A credential of that name is held, and kept: it is another node's
    /// sign-in (this node's own among them), or a later one of the sender's.
    KeptOwn,
    /// Nothing of that name is held.
    NotHeld,
}

impl Revoked {
    /// The word a receipt carries.
    pub fn outcome(self) -> proto::frame::RevokeOutcome {
        use proto::frame::RevokeOutcome;
        match self {
            Self::Dropped => RevokeOutcome::Dropped,
            Self::KeptOwn => RevokeOutcome::Kept,
            Self::NotHeld => RevokeOutcome::NotHeld,
        }
    }
}

/// Loaded at startup from a store only the node can open, and written back
/// whenever a login, an import, or a handoff changes it.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Broker {
    #[serde(default)]
    credentials: BTreeMap<String, Credential>,
    /// Credentials removed here or revoked by the peer that handed them
    /// over, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    revoked: BTreeMap<String, Tombstone>,
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
    /// Putting one here is the operator bringing it back, so its tombstone
    /// goes.
    pub fn put(&mut self, name: &str, cred: Credential) -> bool {
        self.revoked.remove(name);
        self.credentials.insert(name.to_string(), cred).is_some()
    }

    /// Put a credential the operator imported at `node_id`. An import is
    /// the operator putting the credential here, so one that records no
    /// origin is `node_id`'s, whichever node its `nodes` lists first: that
    /// fallback is for copies that arrived by handoff, not for a list an
    /// operator wrote. An origin the file names is kept.
    ///
    /// When it replaces one this node shared, the nodes holding copies would
    /// otherwise drop out of `nodes`, or keep a sign-in later than the one a
    /// later `rm` names, and that `rm` would reach none of them. So those
    /// nodes are carried over, and the import gets a sign-in of its own,
    /// later than the copies', which a later revoke names. The copies are
    /// not re-sent: they hold the old value until it is shared again.
    /// Returns the nodes carried over, for the caller to warn about.
    pub fn import(&mut self, name: &str, mut cred: Credential, node_id: &str) -> Vec<String> {
        let named_elsewhere = cred.grant_origin.as_deref().is_some_and(|o| o != node_id);
        if cred.grant_origin.is_none() {
            cred.grant_origin = Some(node_id.to_string());
        }
        let holders = self
            .credentials
            .get(name)
            .filter(|_| !named_elsewhere)
            .map(|held| (held.nodes.clone(), held.revoke_targets(node_id)))
            .filter(|(_, targets)| !targets.is_empty());
        let Some((listed, carried)) = holders else {
            self.put(name, cred);
            return Vec::new();
        };
        // An empty list means "here only", which would lock this node out
        // once the holders are listed.
        if cred.nodes.is_empty() {
            cred.nodes.push(node_id.to_string());
        }
        for node in listed {
            if !cred.nodes.contains(&node) {
                cred.nodes.push(node);
            }
        }
        cred.grant_id = None;
        cred.grant_version = 0;
        cred.ensure_grant(node_id);
        self.put(name, cred);
        carried
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.credentials.remove(name).is_some()
    }

    /// Remove a credential for good: what `tracon credential rm` does. The
    /// sign-in it was is kept as a tombstone, so a peer handing it back
    /// after a refresh does not bring it back. Returns what was removed.
    pub fn revoke(&mut self, name: &str) -> Option<Credential> {
        let removed = self.credentials.remove(name)?;
        self.bury(name, Tombstone::of(&removed));
        Some(removed)
    }

    /// The tombstone a removed credential left, if any.
    pub fn tombstone(&self, name: &str) -> Option<&Tombstone> {
        self.revoked.get(name)
    }

    /// Record a tombstone, keeping whichever of it and one already here is
    /// the later grant.
    fn bury(&mut self, name: &str, tombstone: Tombstone) {
        match self.revoked.get(name) {
            Some(held) if held.key() >= tombstone.key() => {}
            _ => {
                self.revoked.insert(name.to_string(), tombstone);
            }
        }
    }

    /// Apply a revoke from `from`, a verified sender. Only the origin of the
    /// copy held here is obeyed ([`Credential::origin`]): a holder that
    /// removed its own copy drops nothing anywhere else, so neither the
    /// owner's credential nor another holder's goes with it. The copy stays,
    /// too, when it is of a later sign-in than the one revoked: a re-share
    /// since.
    ///
    /// With nothing held, nothing is recorded unless a tombstone of the
    /// sender's sign-in is already here: the sender cannot be shown to be
    /// any grant's origin otherwise, and a tombstone recorded on its word
    /// alone would let any member refuse a credential's every later handoff
    /// here, and grow the store by a name at a time.
    ///
    /// A revoke that names no sign-in leaves a tombstone without an origin.
    /// Refusing the origin's every later copy guards against its renewal of
    /// a copy it still holds, and its revoke says it holds none, so its next
    /// share, which names a sign-in, is taken.
    pub fn apply_revoke(
        &mut self,
        from: &str,
        name: &str,
        grant_id: Option<&str>,
        grant_version: u64,
    ) -> Revoked {
        let origin = grant_id.map(|_| from.to_string());
        let revoked = Tombstone {
            grant_id: grant_id.map(str::to_string),
            grant_version,
            origin: origin.clone(),
        };
        let Some(held) = self.credentials.get(name) else {
            if self
                .revoked
                .get(name)
                .is_some_and(|t| t.origin.as_deref() == Some(from))
            {
                self.bury(name, revoked);
            }
            return Revoked::NotHeld;
        };
        if held.origin() != Some(from) || held.newer_grant_than(grant_id) {
            return Revoked::KeptOwn;
        }
        let dropped = self.credentials.remove(name).expect("held above");
        let mut left = Tombstone::of(&dropped);
        if origin.is_none() {
            left.origin = None;
        }
        self.bury(name, left);
        self.bury(name, revoked);
        Revoked::Dropped
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

    /// What admitting `to` hands it, by the rules a share follows: the
    /// credentials pinned to it, less any copy of another node's sign-in,
    /// which only that origin may pass on, and each named as a sign-in
    /// minted at `self_id` where it named none, so `to`'s own `rm` of one
    /// leaves a tombstone of that sign-in rather than one refusing every
    /// later copy. Returns the rows and the names given a sign-in here, for
    /// the caller to save before anything is sent.
    pub fn admission_handoff(
        &mut self,
        self_id: &str,
        to: &str,
    ) -> (Vec<(String, Credential)>, Vec<String>) {
        let mut rows = Vec::new();
        let mut minted = Vec::new();
        for (name, cred) in self.credentials.iter_mut() {
            if !cred.nodes.iter().any(|n| n == to) {
                continue;
            }
            if let Some(origin) = cred.shared_from(self_id) {
                tracing::info!(
                    credential = %name,
                    origin,
                    "not handed over at admission: a copy of another node's sign-in"
                );
                continue;
            }
            if cred.ensure_grant(self_id) {
                minted.push(name.clone());
            }
            rows.push((name.clone(), cred.clone()));
        }
        (rows, minted)
    }

    /// Apply a handoff from `from`, the envelope's verified sender. Each row
    /// is `{ "name", "credential" }`; a row not pinned to this node is
    /// dropped — the sender's bindings are a claim, the receiver's are the
    /// rule. A tracked sign-in replaces what is here only when it supersedes
    /// it, so a delayed or replayed copy never undoes a refresh. A name with
    /// a tombstone takes only a later sign-in than the one removed, so a
    /// refresh handed back does not undo a removal and a re-share does.
    /// Returns the names stored.
    pub fn apply_handoff(&mut self, self_id: &str, from: &str, rows: &[Value]) -> Vec<String> {
        let mut stored = Vec::new();
        for row in rows {
            let Some(name) = row["name"].as_str() else {
                continue;
            };
            let Ok(mut cred) = serde_json::from_value::<Credential>(row["credential"].clone())
            else {
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
            // A copy from before origins were recorded is the sender's when
            // it names no sign-in, which only a share mints now; one that
            // does is its first node's, as [`Credential::origin`] reads it.
            if cred.grant_origin.is_none() {
                cred.grant_origin = match cred.grant_id {
                    Some(_) => cred.nodes.first().cloned(),
                    None => Some(from.to_string()),
                };
            }
            if let Some(tombstone) = self.revoked.get(name).filter(|t| t.refuses(&cred)) {
                tracing::warn!(
                    name,
                    from,
                    origin = ?tombstone.origin,
                    "credential handoff is of a sign-in removed here; refused (put the credential here again to take it back)"
                );
                continue;
            }
            self.revoked.remove(name);
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

    /// The sign-ins provider `entry` holds here under `base`, the credential
    /// its `[providers.<entry>]` names: `base` itself, and each `base@…`
    /// model credential for `entry`, one per set of channels it serves.
    pub fn family(
        &self,
        entry: &str,
        base: &str,
    ) -> impl Iterator<Item = (&str, &Credential)> + '_ {
        let (entry, base) = (entry.to_string(), base.to_string());
        self.credentials
            .iter()
            .filter(move |(name, cred)| in_family(name, cred, &entry, &base))
            .map(|(n, c)| (n.as_str(), c))
    }

    /// The sign-in of provider `entry` that serves `channel` on `node_id`:
    /// of those bound to it, one signed in here before a copy of another
    /// node's, then the later sign-in. A copy shared from a peer can serve a
    /// channel a sign-in here also does; the operator's own is the one used.
    pub fn model_credential(
        &self,
        entry: &str,
        base: &str,
        channel: &str,
        node_id: &str,
    ) -> Option<(&str, &Credential)> {
        self.family(entry, base)
            .filter(|(_, c)| c.allows(channel) && c.allows_node(node_id))
            .max_by(|a, b| precedence(a, b, node_id))
    }

    /// The sign-in a provider is described by where no channel is asked:
    /// `base` when it is held here, otherwise the first by
    /// [`Self::model_credential`]'s order.
    pub fn primary_sign_in(
        &self,
        entry: &str,
        base: &str,
        node_id: &str,
    ) -> Option<(&str, &Credential)> {
        if let Some((name, cred)) = self
            .credentials
            .get_key_value(base)
            .filter(|(_, c)| c.allows_node(node_id))
        {
            return Some((name.as_str(), cred));
        }
        self.family(entry, base)
            .filter(|(_, c)| c.allows_node(node_id))
            .max_by(|a, b| precedence(a, b, node_id))
    }

    /// [`Self::inject_for`] for a provider, through the sign-in that serves
    /// `channel`. With none, the refusal is the one `base` itself earns, so
    /// a provider holding one sign-in is refused as it always was.
    pub fn inject_for_provider(
        &self,
        entry: &str,
        base: &str,
        channel: &str,
        node_id: &str,
        shape: &str,
    ) -> Result<Injection, BrokerError> {
        match self.model_credential(entry, base, channel, node_id) {
            Some((_, cred)) => Self::injection(cred, shape),
            None => self.inject_for(base, channel, node_id, shape),
        }
    }

    /// [`Self::inject_for_probe`] for a provider: any of its sign-ins some
    /// channel on this node could use.
    pub fn inject_for_probe_provider(
        &self,
        entry: &str,
        base: &str,
        node_id: &str,
        shape: &str,
    ) -> Result<Injection, BrokerError> {
        let usable = self
            .family(entry, base)
            .filter(|(_, c)| !c.channels.is_empty() && c.allows_node(node_id))
            .max_by(|a, b| precedence(a, b, node_id));
        match usable {
            Some((_, cred)) => Self::injection(cred, shape),
            None => self.inject_for_probe(base, node_id, shape),
        }
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
        // A root that climbs back out of the group is not under it.
        assert!(!lent("composer", &format!("{clones}/../evil/x")));
        assert!(!lent("composer", "gitlab.com/unebraska/uitao/../evil/x"));
        // An absolute prefix must lead the root; it is not found further down.
        assert!(lent("local", "/src/uitao/portal"));
        assert!(lent("local", "/src/uitao/group/portal"));
        assert!(!lent("local", "/home/me/src/uitao/portal"));
        assert!(!lent("local", "/src/uitao2/portal"));
        assert!(!lent("local", "/src/uitao"));
        assert!(!lent("local", "/src/uitao/../evil/x"));
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
            "a/../**",
            "gitlab.com/uitao/../**",
            "gitlab.com/./uitao/**",
            "./gitlab.com/uitao/**",
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
        assert!(b.apply_handoff("node-a", "node-x", &rows).is_empty());
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

    /// A provider's sign-ins are its base credential and each `base@…` one
    /// written for it; a channel is served by one made here before a peer's
    /// copy, then by the later sign-in.
    #[test]
    fn a_channel_is_served_by_the_sign_in_made_here_then_the_later_one() {
        let sign_in = |channels: &[&str], origin: &str, key: &str| {
            let mut cred = Credential {
                kind: KIND_API_KEY.into(),
                provider: Some("anthropic".into()),
                channels: channels.iter().map(|c| c.to_string()).collect(),
                nodes: vec![origin.into(), "node-a".into()],
                grant_id: Some(uuid::Uuid::now_v7().to_string()),
                grant_origin: Some(origin.into()),
                ..Default::default()
            };
            cred.env.insert("API_KEY".into(), key.into());
            cred
        };
        let key = |b: &Broker, channel: &str| {
            b.inject_for_provider("anthropic", "anthropic", channel, "node-a", "anthropic")
                .ok()
                .and_then(|i| i.x_api_key)
        };
        let mut b = Broker::default();
        b.put(
            "anthropic",
            sign_in(&["personal", "work"], "node-b", "peer"),
        );
        assert_eq!(key(&b, "work").as_deref(), Some("peer"));
        b.put("anthropic@work", sign_in(&["work"], "node-a", "mine"));
        b.put("anthropic@ops", sign_in(&["ops"], "node-b", "later-peer"));
        // Named like a sign-in but written for something else.
        let mut other = sign_in(&["ops"], "node-a", "not-anthropic");
        other.provider = Some("openai".into());
        b.put("anthropic@other", other);
        b.put("anthropicx", sign_in(&["ops"], "node-a", "not-anthropic"));
        assert_eq!(
            b.family("anthropic", "anthropic")
                .map(|(n, _)| n)
                .collect::<Vec<_>>(),
            ["anthropic", "anthropic@ops", "anthropic@work"]
        );
        assert_eq!(key(&b, "work").as_deref(), Some("mine"));
        assert_eq!(key(&b, "personal").as_deref(), Some("peer"));
        assert_eq!(key(&b, "ops").as_deref(), Some("later-peer"));
        assert!(matches!(
            b.inject_for_provider("anthropic", "anthropic", "nowhere", "node-a", "anthropic")
                .unwrap_err(),
            BrokerError::NotBound { .. }
        ));
        b.put(
            "anthropic@ops-2",
            sign_in(&["ops"], "node-b", "latest-peer"),
        );
        assert_eq!(key(&b, "ops").as_deref(), Some("latest-peer"));
        assert_eq!(
            b.primary_sign_in("anthropic", "anthropic", "node-a")
                .map(|(n, _)| n),
            Some("anthropic")
        );
        b.remove("anthropic");
        assert_eq!(
            b.primary_sign_in("anthropic", "anthropic", "node-a")
                .map(|(n, _)| n),
            Some("anthropic@work"),
            "made here, before the peers' copies"
        );
        assert!(b
            .inject_for_probe_provider("anthropic", "anthropic", "node-a", "anthropic")
            .is_ok());
        assert!(is_sign_in_name("anthropic@work", "anthropic"));
        assert!(!is_sign_in_name("anthropic@", "anthropic"));
        assert!(!is_sign_in_name("anthropicx@work", "anthropic"));
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
        assert_eq!(b.apply_handoff("node-b", "node-a", &rows), ["p"]);
        assert!(b.get("p").is_some() && b.get("l").is_none());
        assert!(Broker::default()
            .apply_handoff("node-a", "node-x", &rows)
            .is_empty());
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
        assert_eq!(
            b.apply_handoff("node-b", "node-a", &copy("0190-a", 2, "v2"))
                .len(),
            1
        );
        assert!(b
            .apply_handoff("node-b", "node-a", &copy("0190-a", 1, "v1"))
            .is_empty());
        assert!(b
            .apply_handoff("node-b", "node-a", &copy("0190-a", 2, "v2-again"))
            .is_empty());
        assert_eq!(token(&b), "v2");
        assert_eq!(
            b.apply_handoff("node-b", "node-a", &copy("0190-a", 3, "v3"))
                .len(),
            1
        );
        // A new sign-in wins over any refresh of an older one, and an older
        // sign-in never comes back.
        assert_eq!(
            b.apply_handoff("node-b", "node-a", &copy("0191-b", 0, "new"))
                .len(),
            1
        );
        assert!(b
            .apply_handoff("node-b", "node-a", &copy("0190-a", 9, "old"))
            .is_empty());
        assert_eq!(token(&b), "new");
    }

    /// Node A's sign-in, shared with node B.
    fn shared(grant: &str, version: u64, token: &str) -> Credential {
        let mut c = Credential {
            kind: KIND_OAUTH.into(),
            nodes: vec!["node-a".into(), "node-b".into()],
            grant_id: Some(grant.into()),
            grant_version: version,
            grant_origin: Some("node-a".into()),
            ..Default::default()
        };
        c.env.insert("ACCESS_TOKEN".into(), token.into());
        c
    }

    /// The bug a tombstone closes: a peer refreshes a shared credential and
    /// hands the renewed copy back to the node that removed it.
    #[test]
    fn a_removed_shared_credential_is_refused_when_a_refresh_is_handed_back() {
        let dir = std::env::temp_dir().join(format!("tracon-broker-tomb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (sealed, plain) = (dir.join("credentials.sealed"), dir.join("credentials.toml"));
        let key = DataKey::generate();
        let mut b = Broker::default();
        b.put("anthropic", shared("0190-a", 2, "v2"));
        assert!(b.revoke("anthropic").is_some());
        b.save_at(&sealed, &key).unwrap();

        // The tombstone is sealed with the store and outlives a restart.
        let mut b = Broker::load_at(&sealed, &plain, &key).unwrap();
        let refreshed = Broker::handoff_rows(&[("anthropic".into(), shared("0190-a", 3, "v3"))]);
        assert!(b.apply_handoff("node-a", "node-b", &refreshed).is_empty());
        assert!(b.get("anthropic").is_none());

        // A later sign-in passes it, and the tombstone goes.
        let again = Broker::handoff_rows(&[("anthropic".into(), shared("0191-b", 0, "new"))]);
        assert_eq!(b.apply_handoff("node-a", "node-b", &again), ["anthropic"]);
        assert!(b.tombstone("anthropic").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn importing_a_removed_credential_again_clears_its_tombstone() {
        let mut b = Broker::default();
        b.put("anthropic", shared("0190-a", 2, "v2"));
        b.revoke("anthropic");
        b.put("anthropic", shared("0190-a", 2, "v2"));
        assert!(b.tombstone("anthropic").is_none());
        let refreshed = Broker::handoff_rows(&[("anthropic".into(), shared("0190-a", 3, "v3"))]);
        assert_eq!(b.apply_handoff("node-a", "node-b", &refreshed).len(), 1);
    }

    /// Review of #608, must-fix 1: B shares `glab` to A and C, and A's
    /// operator removes A's copy. That is A's alone to drop: nothing is
    /// owed, and a revoke from A, were one sent, drops neither B's own
    /// credential nor C's copy. B's revoke drops C's.
    #[test]
    fn a_recipient_removing_its_copy_leaves_the_owner_and_other_holders_theirs() {
        let mut glab = Credential {
            nodes: vec!["node-b".into(), "node-a".into(), "node-c".into()],
            ..Default::default()
        };
        assert!(glab.ensure_grant("node-b"));
        let grant = glab.grant_id.clone();
        let mut owner = Broker::default();
        owner.put("glab", glab.clone());
        let rows = Broker::handoff_rows(&[("glab".into(), glab)]);
        let (mut a, mut c) = (Broker::default(), Broker::default());
        assert_eq!(a.apply_handoff("node-a", "node-b", &rows), ["glab"]);
        assert_eq!(c.apply_handoff("node-c", "node-b", &rows), ["glab"]);

        let removed = a.revoke("glab").unwrap();
        assert!(!removed.originates_at("node-a"));
        assert!(removed.revoke_targets("node-a").is_empty());
        assert_eq!(removed.origin(), Some("node-b"));

        for holder in [&mut owner, &mut c] {
            assert_eq!(
                holder.apply_revoke("node-a", "glab", grant.as_deref(), 0),
                Revoked::KeptOwn
            );
            assert!(holder.get("glab").is_some() && holder.tombstone("glab").is_none());
        }
        // The origin removing it is what revokes the others.
        assert_eq!(
            owner.get("glab").unwrap().revoke_targets("node-b"),
            ["node-a", "node-c"]
        );
        assert_eq!(
            c.apply_revoke("node-b", "glab", grant.as_deref(), 0),
            Revoked::Dropped
        );
    }

    /// A peer that renewed the owner's OAuth sign-in hands the renewed copy
    /// back; it is still the owner's, so that peer's revoke leaves it.
    #[test]
    fn a_renewal_handed_back_stays_the_owners_sign_in() {
        let mut owner = Broker::default();
        owner.put("anthropic", shared("0190-a", 1, "v1"));
        let renewed = Broker::handoff_rows(&[("anthropic".into(), shared("0190-a", 2, "v2"))]);
        assert_eq!(owner.apply_handoff("node-a", "node-b", &renewed).len(), 1);
        assert_eq!(owner.get("anthropic").unwrap().origin(), Some("node-a"));
        assert!(owner.get("anthropic").unwrap().originates_at("node-a"));
        assert_eq!(
            owner.apply_revoke("node-b", "anthropic", Some("0190-a"), 2),
            Revoked::KeptOwn
        );
    }

    /// Review of #608, must-fix 2: A shared `glab` to D before shares named a
    /// sign-in, so D's copy names none. A's revoke of the sign-in a later
    /// share minted drops it all the same: D's copy lists A first. B's own
    /// credential of the name, which lists B first, stays.
    #[test]
    fn a_legacy_copy_without_a_sign_in_goes_with_its_origins_revoke() {
        let legacy = |nodes: &[&str]| Credential {
            nodes: nodes.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        };
        let mut d = Broker::default();
        d.put("glab", legacy(&["node-a", "node-d"]));
        assert_eq!(
            d.apply_revoke("node-a", "glab", Some("0190-x"), 0),
            Revoked::Dropped
        );
        assert!(d.tombstone("glab").is_some());

        let mut b = Broker::default();
        b.put("glab", legacy(&["node-b", "node-a"]));
        assert_eq!(
            b.apply_revoke("node-a", "glab", Some("0190-x"), 0),
            Revoked::KeptOwn
        );
        assert!(b.get("glab").is_some());
    }

    /// Review of #608, should-fix 3: a member sends a revoke for a name this
    /// node holds nothing under. Nothing is recorded, so a real share later
    /// is taken; only a tombstone of the sender's own sign-in is kept up to
    /// date by one.
    #[test]
    fn a_revoke_for_a_name_not_held_plants_no_tombstone() {
        let mut b = Broker::default();
        assert_eq!(
            b.apply_revoke("node-x", "anthropic", Some("ffffffff-ffff"), 9),
            Revoked::NotHeld
        );
        assert!(b.tombstone("anthropic").is_none());
        let rows = Broker::handoff_rows(&[("anthropic".into(), shared("0190-a", 0, "v0"))]);
        assert_eq!(b.apply_handoff("node-b", "node-a", &rows), ["anthropic"]);

        // Dropped by its origin, then that revoke again with a later count:
        // the tombstone follows the origin and no one else.
        b.apply_revoke("node-a", "anthropic", Some("0190-a"), 0);
        b.apply_revoke("node-a", "anthropic", Some("0190-a"), 3);
        assert_eq!(b.tombstone("anthropic").unwrap().grant_version, 3);
        b.apply_revoke("node-x", "anthropic", Some("ffffffff-ffff"), 9);
        assert_eq!(
            b.tombstone("anthropic").unwrap().grant_id.as_deref(),
            Some("0190-a")
        );
    }

    /// Review of #608, should-fix 4: D removes a copy of A's OAuth sign-in
    /// from before sign-ins were named. A's first renewal mints one, which
    /// would sort after the nameless tombstone; it is refused as A's. Another
    /// node's sign-in of the name is not A's and is taken.
    #[test]
    fn a_tombstone_of_a_nameless_copy_refuses_its_origins_first_renewal() {
        let mut d = Broker::default();
        d.put(
            "anthropic",
            Credential {
                kind: KIND_OAUTH.into(),
                nodes: vec!["node-a".into(), "node-d".into()],
                ..Default::default()
            },
        );
        d.revoke("anthropic");
        assert_eq!(
            d.tombstone("anthropic").unwrap().origin.as_deref(),
            Some("node-a")
        );
        let renewed = Broker::handoff_rows(&[("anthropic".into(), shared("0190-a", 1, "v1"))]);
        assert!(d.apply_handoff("node-d", "node-a", &renewed).is_empty());
        let mut theirs = shared("0191-c", 0, "c");
        theirs.grant_origin = Some("node-c".into());
        theirs.nodes = vec!["node-c".into(), "node-d".into()];
        let rows = Broker::handoff_rows(&[("anthropic".into(), theirs)]);
        assert_eq!(d.apply_handoff("node-d", "node-c", &rows), ["anthropic"]);
    }

    /// Second review of #608, should-fix 2: A shared env `glab` with D before
    /// shares named a sign-in, and removes it. Its revoke names none, and A
    /// holds nothing to renew after it, so the tombstone D keeps refuses only
    /// a copy that names no sign-in: A importing `glab` again and sharing it
    /// is taken.
    #[test]
    fn a_nameless_copy_the_origin_revoked_takes_the_origins_next_share() {
        let mut d = Broker::default();
        d.put(
            "glab",
            Credential {
                nodes: vec!["node-a".into(), "node-d".into()],
                ..Default::default()
            },
        );
        assert_eq!(d.apply_revoke("node-a", "glab", None, 0), Revoked::Dropped);
        assert_eq!(d.tombstone("glab").unwrap().origin, None);
        // The removed copy handed back is refused still.
        let stale = vec![serde_json::json!({ "name": "glab", "credential": {
            "nodes": ["node-a", "node-d"],
        }})];
        assert!(d.apply_handoff("node-d", "node-a", &stale).is_empty());

        let mut again = Credential {
            nodes: vec!["node-a".into(), "node-d".into()],
            ..Default::default()
        };
        assert!(again.ensure_grant("node-a"));
        let rows = Broker::handoff_rows(&[("glab".into(), again)]);
        assert_eq!(d.apply_handoff("node-d", "node-a", &rows), ["glab"]);
        assert!(d.tombstone("glab").is_none());
    }

    /// Second review of #608, should-fix 1: a copy is shared on only by its
    /// origin. One that records no origin reads its first node as it, so an
    /// old copy of B's is not A's to share either.
    #[test]
    fn a_copy_is_not_shared_from_anywhere_but_its_origin() {
        let mut held = Credential {
            nodes: vec!["node-b".into(), "node-a".into()],
            ..Default::default()
        };
        assert_eq!(held.shared_from("node-a"), Some("node-b"));
        assert_eq!(held.shared_from("node-b"), None);
        held.grant_origin = Some("node-c".into());
        assert_eq!(held.shared_from("node-a"), Some("node-c"));
        assert_eq!(held.shared_from("node-c"), None);
        assert_eq!(Credential::default().shared_from("node-a"), None);
    }

    /// Third review of #608, should-fix 2: D holds a copy A shared before
    /// origins were recorded, so it lists A first and names no sign-in or
    /// origin. It is A's sign-in to D's `rm` as it is to D's `share`: D's
    /// removal owes no one a revoke.
    #[test]
    fn a_legacy_copy_is_its_first_nodes_to_revoke_as_to_share() {
        let legacy = Credential {
            nodes: vec!["node-a".into(), "node-d".into()],
            ..Default::default()
        };
        assert!(!legacy.originates_at("node-d"));
        assert!(legacy.revoke_targets("node-d").is_empty());
        assert_eq!(legacy.shared_from("node-d"), Some("node-a"));
        assert!(legacy.originates_at("node-a"));
        assert_eq!(legacy.revoke_targets("node-a"), ["node-d"]);
        assert!(Credential::default().originates_at("node-d"));
    }

    /// Third review of #608, should-fix 3: A shares `glab` to D, then the
    /// operator imports a rotated token on A from a file that lists no
    /// nodes. D stays listed, the import is a later sign-in than D's copy,
    /// and A's `rm` then drops D's copy.
    #[test]
    fn importing_over_a_shared_credential_keeps_its_holders_within_a_revoke() {
        let token = |value: &str| {
            let mut c = Credential::default();
            c.env.insert("GLAB_TOKEN".into(), value.into());
            c
        };
        let mut a = Broker::default();
        let mut glab = token("v1");
        glab.nodes = vec!["node-a".into(), "node-d".into()];
        glab.ensure_grant("node-a");
        a.put("glab", glab.clone());
        let mut d = Broker::default();
        let rows = Broker::handoff_rows(&[("glab".into(), glab.clone())]);
        assert_eq!(d.apply_handoff("node-d", "node-a", &rows), ["glab"]);

        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(a.import("glab", token("v2"), "node-a"), ["node-d"]);
        let imported = a.get("glab").unwrap().clone();
        assert_eq!(imported.nodes, ["node-a", "node-d"]);
        assert_eq!(imported.grant_origin.as_deref(), Some("node-a"));
        assert!(imported.newer_grant_than(glab.grant_id.as_deref()));

        let removed = a.revoke("glab").unwrap();
        assert_eq!(removed.revoke_targets("node-a"), ["node-d"]);
        assert_eq!(
            d.apply_revoke("node-a", "glab", removed.grant_id.as_deref(), 0),
            Revoked::Dropped
        );
        assert!(d.get("glab").is_none());

        // A file that lists D already is the same import.
        let mut a = Broker::default();
        a.put("glab", glab.clone());
        let mut listed = token("v3");
        listed.nodes = vec!["node-a".into(), "node-d".into()];
        assert_eq!(a.import("glab", listed, "node-a"), ["node-d"]);
        assert!(a.get("glab").unwrap().grant_id.is_some());

        // A credential nobody else holds, or one that is another node's
        // copy, is put as it was imported.
        let mut a = Broker::default();
        a.put("mine", token("v1"));
        assert!(a.import("mine", token("v2"), "node-a").is_empty());
        assert!(a.get("mine").unwrap().grant_id.is_none());
        let mut copy = token("v1");
        copy.nodes = vec!["node-b".into(), "node-a".into()];
        a.put("theirs", copy);
        assert!(a.import("theirs", token("v2"), "node-a").is_empty());
        assert!(a.get("theirs").unwrap().nodes.is_empty());
    }

    /// Fourth review of #608, should-fix 2: an import is the operator putting
    /// the credential here, so one that names no origin is this node's,
    /// whichever node its `nodes` lists first. An origin the file names is
    /// kept, and is not taken for this node's share.
    #[test]
    fn an_import_is_stamped_with_this_node_unless_it_names_an_origin() {
        let listed = |origin: Option<&str>| Credential {
            nodes: vec!["laptop".into(), "node-s".into()],
            grant_origin: origin.map(str::to_string),
            ..Default::default()
        };
        let mut s = Broker::default();
        assert!(s.import("glab", listed(None), "node-s").is_empty());
        let glab = s.get("glab").unwrap();
        assert_eq!(glab.origin(), Some("node-s"));
        assert_eq!(glab.shared_from("node-s"), None);
        assert_eq!(glab.revoke_targets("node-s"), ["laptop"]);

        assert!(s
            .import("glab", listed(Some("laptop")), "node-s")
            .is_empty());
        let glab = s.get("glab").unwrap();
        assert_eq!(glab.origin(), Some("laptop"));
        assert_eq!(glab.shared_from("node-s"), Some("laptop"));
        assert!(glab.grant_id.is_none());
    }

    /// Third review of #608, nit 5: admitting D hands it what a share
    /// would: this node's credentials pinned to D, each named as a sign-in
    /// minted here, and never a copy of another node's.
    #[test]
    fn admission_hands_over_only_this_nodes_sign_ins_and_names_them() {
        let pinned = |nodes: &[&str]| Credential {
            nodes: nodes.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        };
        let mut a = Broker::default();
        a.put("own", pinned(&["node-a", "node-d"]));
        a.put("copy", pinned(&["node-b", "node-a", "node-d"]));
        a.put("elsewhere", pinned(&["node-a", "node-c"]));
        let mut named = pinned(&["node-a", "node-d"]);
        named.ensure_grant("node-a");
        a.put("named", named.clone());

        let (rows, minted) = a.admission_handoff("node-a", "node-d");
        let names: Vec<&str> = rows.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["named", "own"]);
        assert_eq!(minted, ["own"]);
        let own = a.get("own").unwrap();
        assert!(own.grant_id.is_some());
        assert_eq!(own.grant_origin.as_deref(), Some("node-a"));
        assert_eq!(rows[1].1.grant_id, own.grant_id);
        assert_eq!(a.get("named").unwrap().grant_id, named.grant_id);
        assert!(a.get("copy").unwrap().grant_id.is_none());
    }

    /// Redelivery, a re-share since, and a copy that came by way of another
    /// holder's refresh.
    #[test]
    fn a_revoke_from_the_origin_drops_its_sign_in_once_and_spares_a_later_one() {
        let mut b = Broker::default();
        let env = |grant: &str| Credential {
            nodes: vec!["node-a".into(), "node-b".into()],
            grant_id: Some(grant.into()),
            grant_origin: Some("node-a".into()),
            ..Default::default()
        };
        let rows = Broker::handoff_rows(&[("glab".into(), env("0190-a"))]);
        b.apply_handoff("node-b", "node-a", &rows);
        assert_eq!(
            b.apply_revoke("node-a", "glab", Some("0190-a"), 0),
            Revoked::Dropped
        );
        assert!(b.get("glab").is_none());
        // Redelivered: nothing left to drop, and the late copy stays out.
        assert_eq!(
            b.apply_revoke("node-a", "glab", Some("0190-a"), 0),
            Revoked::NotHeld
        );
        assert!(b.apply_handoff("node-b", "node-a", &rows).is_empty());

        // A re-share since the removal is a later grant, which a revoke
        // redelivered behind it leaves alone.
        let reshared = Broker::handoff_rows(&[("glab".into(), env("0193-a"))]);
        assert_eq!(b.apply_handoff("node-b", "node-a", &reshared), ["glab"]);
        assert_eq!(
            b.apply_revoke("node-a", "glab", Some("0190-a"), 0),
            Revoked::KeptOwn
        );

        // A copy of the revoked grant that came by way of another holder's
        // refresh is still the origin's, and goes.
        let mut c = Broker::default();
        let via = Broker::handoff_rows(&[("anthropic".into(), shared("0190-a", 4, "v4"))]);
        c.apply_handoff("node-b", "node-c", &via);
        assert_eq!(
            c.apply_revoke("node-a", "anthropic", Some("0190-a"), 2),
            Revoked::Dropped
        );
    }

    /// A handoff carries whose sign-in the copy is. One from before origins
    /// were recorded is read as its sender's when it names no sign-in, and
    /// as its first node's when it does.
    #[test]
    fn a_handoff_carries_the_origin_and_a_copy_without_one_gets_it() {
        let rows = Broker::handoff_rows(&[("anthropic".into(), shared("0190-a", 1, "v1"))]);
        assert_eq!(rows[0]["credential"]["grant_origin"], "node-a");

        let mut b = Broker::default();
        let old = |grant: Option<&str>| {
            vec![serde_json::json!({ "name": "c", "credential": {
                "nodes": ["node-a", "node-b"], "grant_id": grant,
            }})]
        };
        b.apply_handoff("node-b", "node-c", &old(None));
        assert_eq!(b.get("c").unwrap().grant_origin.as_deref(), Some("node-c"));
        let mut b = Broker::default();
        b.apply_handoff("node-b", "node-c", &old(Some("0190-a")));
        assert_eq!(b.get("c").unwrap().grant_origin.as_deref(), Some("node-a"));
    }

    #[test]
    fn a_credential_without_a_sign_in_gets_one_minted_here_to_be_shared() {
        let mut env = Credential::default();
        assert!(env.ensure_grant("node-a"));
        let first = env.grant_id.clone();
        assert!(!env.ensure_grant("node-b"));
        assert_eq!(env.grant_id, first);
        assert_eq!(env.grant_origin.as_deref(), Some("node-a"));
        let mut oauth = Credential {
            kind: KIND_OAUTH.into(),
            ..Default::default()
        };
        assert!(oauth.ensure_grant("node-a"));
        assert_eq!(oauth.origin(), Some("node-a"));
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
