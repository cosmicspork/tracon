//! Node configuration: `~/.config/tracon/node.toml`, overridden by flags.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

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
    /// Model providers the gateway fronts, by name.
    pub providers: std::collections::BTreeMap<String, Provider>,
    pub memory: Memory,
    pub supervision: Supervision,
    pub review: ReviewLimits,
    pub notify: Notify,
    pub embed: Embed,
    pub external: External,
    pub docs: Docs,
    /// What an operator may customize a session's launch with, bounded by
    /// what the harness image bakes.
    pub launch: Launch,
    /// Interfaces this node serves beside the operator's own.
    pub ui: Ui,
}

/// The harness's native interface, served by tracon from a pinned bundle on an
/// origin of its own.
///
/// A separate origin rather than a path under the operator interface, for three
/// reasons that are all upstream's: the bundle's asset, font and manifest
/// references are root-absolute, so it cannot live under a subpath
/// (`api-ui.md` §8 #16); its `site.webmanifest` claims `scope: "/"` and would
/// collide with tracon's own (#17); and its server URL is `location.origin`,
/// so whatever origin serves the page is the origin its API calls go to
/// (§6, "Server URL discovery"). Giving it an origin is what lets tracon
/// answer those calls with the mediated gateway instead of the harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Ui {
    /// Dedicated bind address for the native OpenCode interface. A different
    /// port from the operator listener, and never the same router: no operator
    /// route is reachable here and no operator cookie is read here.
    pub opencode_listen: SocketAddr,
    /// Public HTTP(S) origin for that listener, when it is published through
    /// an ingress. A *separate hostname* from the operator interface's — the
    /// cookie that authorises this origin is host-only, and a browser does not
    /// scope cookies by port, so two loopback ports share a cookie jar even
    /// though they are different origins to everything else. The separation
    /// that actually holds is in the code (each guard reads only its own
    /// cookie name, `http::ui::UI_COOKIE` and `http::auth::COOKIE`); a
    /// distinct hostname is what makes the browser agree.
    pub opencode_url: Option<String>,
    /// Where the vendored bundle was installed. Unset is the node's state
    /// directory, which is where `containers/opencode-ui/build.sh` puts it.
    pub opencode_bundle_dir: Option<PathBuf>,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            opencode_listen: "127.0.0.1:7423".parse().expect("valid UI address"),
            opencode_url: None,
            opencode_bundle_dir: None,
        }
    }
}

impl Ui {
    /// The origin the UI is reached at: what its CSP, its cookie audience and
    /// its `Origin` check are all written against.
    pub fn opencode_origin(&self) -> Result<String, String> {
        let Some(configured) = self.opencode_url.as_deref() else {
            return Ok(format!("http://127.0.0.1:{}", self.opencode_listen.port()));
        };
        let url = url::Url::parse(configured)
            .map_err(|error| format!("ui.opencode_url is not a valid URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "ui.opencode_url must be an HTTP(S) origin without credentials, path, query, or fragment"
                    .into(),
            );
        }
        Ok(url.origin().ascii_serialization())
    }

    /// The host the UI listener answers to, for the `Host` check that is the
    /// DNS-rebinding defence on this origin — the same question
    /// `http::host_is_local` asks of the operator listener.
    pub fn opencode_host(&self) -> Result<String, String> {
        let origin = self.opencode_origin()?;
        Ok(crate::http::hostname(&origin).to_string())
    }

    /// Whether the origin is loopback, which is what decides `Secure` on the
    /// cookie: a `Secure` cookie over plain HTTP is dropped by the browser,
    /// and loopback is the one place tracon serves plain HTTP on purpose.
    pub fn opencode_is_loopback(&self) -> bool {
        let Ok(host) = self.opencode_host() else {
            return false;
        };
        matches!(host.as_str(), "localhost" | "::1")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }

    /// Where the vendored bundle is read from.
    ///
    /// `TRACON_OPENCODE_UI_DIR` wins over both, because the one deployment
    /// that needs it has no operator to write a config file: the node image
    /// carries the tree on its own filesystem (`Dockerfile.node`) and mounts
    /// the state directory as a volume, which would shadow a copy under it.
    /// The digest check does not move — whatever this names is verified over
    /// the bytes about to be served, or nothing is served.
    pub fn opencode_bundle_path(&self) -> PathBuf {
        if let Some(named) = std::env::var_os("TRACON_OPENCODE_UI_DIR") {
            if !named.is_empty() {
                return PathBuf::from(named);
            }
        }
        self.opencode_bundle_dir
            .clone()
            .unwrap_or_else(|| Config::state_dir().join("opencode-ui"))
    }
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

pub fn safe_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.starts_with('/')
        && !value.contains('\\')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\0'))
}

/// The operator's approvals for what a session's harness may load.
///
/// A skill is bytes the node copied and can stage anywhere. A plugin is not:
/// OpenCode resolves one by a bare existence check at
/// `$XDG_CACHE_HOME/opencode/packages/<pkg>@<ver>/node_modules/<pkg>`, with no
/// version check and no registry contact (`config-state.md` §4.4), and it is
/// then loaded into the server's own process with its credentials and a shell
/// (§4.6). So what a plugin *is* comes entirely from what the harness image
/// baked, and this list can only ever narrow that — a name the image's
/// toolchain profile does not seed is refused when the manifest is built,
/// with the cache path it would have needed.
///
/// Language servers and formatters are not here: the image's toolchain
/// profile decides those, because the absolute paths they name only exist in
/// the image.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Launch {
    /// Plugin packages the operator approves, each as
    /// `<package>@<exact-version>`.
    pub plugins: Vec<String>,
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
}

impl Default for Docs {
    fn default() -> Self {
        Self {
            export_dir: None,
            export_channel: String::new(),
            export_every_secs: 1800,
            preview_listen: "127.0.0.1:7422".parse().expect("valid preview address"),
            preview_url: None,
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
}

impl Default for External {
    fn default() -> Self {
        Self {
            enabled: false,
            idle_timeout_secs: 3600,
            repo_roots: vec![PathBuf::from("~/src")],
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
    /// Added plus removed lines.
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
    /// The defaults before they were raised. `Config::save` writes every key,
    /// so a node that never touched the cap has these in its `node.toml` and
    /// would otherwise keep them forever.
    const RETIRED_DEFAULTS: (i64, usize) = (800, 40);

    fn lift_retired_defaults(&mut self) {
        if (self.max_diff_lines, self.max_files) == Self::RETIRED_DEFAULTS {
            *self = Self::default();
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
    /// Immutable project images the operator explicitly accepts in addition to
    /// digest-addressed images from safe devcontainer metadata.
    pub approved_images: Vec<String>,
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
    /// What preparation may reach: a preset (`crates`, `npm`, `pypi`,
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
];

/// The preset names an `egress` entry may use, and the hosts each opens, for
/// an interface that offers them.
pub fn egress_presets() -> Vec<(&'static str, &'static [&'static str])> {
    EGRESS_PRESETS.to_vec()
}

impl Repo {
    /// Whether this entry is the one for `repo`.
    pub fn matches(&self, repo: &Path) -> bool {
        let configured = expand_home(&self.path);
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
                "ghcr.io/cosmicspork/tracon-harness-opencode:{}",
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
    /// Harness id: `opencode` or `claude`. An unknown id refuses to start. A
    /// `node.toml` that still names the retired `omp` is migrated to
    /// `opencode` as it loads (`crate::legacy::migrate_config`); a config
    /// that says `omp` anyway is refused with the migration path rather than
    /// a bare refusal (`crate::adapter::RETIRED_MESSAGE`).
    pub id: String,
    /// The tools a session may use at all, by the harness's own names. Empty
    /// means the harness's default set, which is the default here.
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
    /// means the version this node's harness image installs — never "whatever
    /// the image happens to contain", since the same string is what the image
    /// build fetches and what both checks compare against.
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
    /// macOS: start the podman machine when the boundary finds it stopped.
    /// Nothing else starts it at login, and a node run by the service is up
    /// before any terminal is.
    pub start_machine: bool,
    /// Seconds between the SIGTERM a stopped harness container's init receives
    /// and the SIGKILL that follows. It bounds how long a stop can take, not
    /// how thorough it is: the container's PID namespace goes either way, and
    /// with it every LSP and formatter process the harness started
    /// (`docs/reference/opencode-v1.18.30/config-state.md` §6.7). The same
    /// number is the pod's `terminationGracePeriodSeconds`.
    pub stop_timeout_secs: u16,
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
    /// is told its catalogue rather than asked for one (OpenCode; see
    /// `docs/reference/opencode-v1.18.30/providers.md` §7.4). A harness that
    /// probes its own catalogue ignores this. A built-in provider written
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
/// it is connected. A declaring harness (OpenCode) serves exactly this list
/// and the gateway lends the credential for exactly these ids; a probing
/// harness ignores it. The limits are what the harness is told, not what is
/// enforced.
///
/// Three tiers, tried in order:
///
/// 1. An exact match on the cached OpenCode model catalogue's provider key
///    (`models_catalogue::provider_models`). Generic on the provider name —
///    a custom provider like `openrouter` gets real defaults the moment the
///    catalogue knows that key, not only the three built-ins below.
/// 2. The hardcoded list below for `anthropic`/`openai`/`openai-codex`, kept
///    as the safety net for a locked-down node whose `allow_hosts` refuses
///    the catalogue host, or one that has not fetched yet.
/// 3. An empty list.
pub fn default_models(provider: &str) -> Vec<ModelDecl> {
    default_models_tiered(provider, crate::models_catalogue::provider_models(provider))
}

/// The tiered lookup itself, taking the catalogue hit (or its absence) as a
/// parameter so it is testable without touching the process-wide catalogue
/// cache `models_catalogue` holds.
fn default_models_tiered(provider: &str, catalogue_hit: Option<Vec<ModelDecl>>) -> Vec<ModelDecl> {
    if let Some(models) = catalogue_hit {
        return models;
    }
    hardcoded_default_models(provider)
}

/// Tier 2: the fixed list this node falls back to when the catalogue has no
/// entry for `provider` — either because it names none of these three, or
/// because the catalogue itself is unavailable.
fn hardcoded_default_models(provider: &str) -> Vec<ModelDecl> {
    let model = |id: &str, name: &str, context: u64, output: u64, attachment: bool| ModelDecl {
        id: id.into(),
        name: name.into(),
        context,
        output,
        reasoning: true,
        attachment,
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
    /// Where worktrees are created. Outside any repo, so nothing is gitignored.
    pub worktree_root: PathBuf,
    /// How many terminal frames the gateway will hold for a browser that has
    /// stopped reading, in each direction. Past this the connection is closed
    /// with a reason rather than buffered: a stalled tab must not be able to
    /// grow the node's memory without bound.
    pub pty_buffer_frames: usize,
    /// Whether a proxied terminal's output tail is written to the session log
    /// when the connection ends. Off, because a terminal transcript is not a
    /// tool ledger and reads like one: the harness raises no permission and
    /// no tool call for anything typed at a PTY prompt, so a captured
    /// transcript records what the operator happened to run, not what tracon
    /// decided. Turn it on deliberately, knowing that.
    pub pty_capture_output: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node_name: hostname(),
            mesh: Mesh::default(),
            runtime: Runtime::default(),
            repo: Vec::new(),
            live_repo: LiveRepos::default(),
            providers: default_providers(),
            memory: Memory::default(),
            ui: Ui::default(),
            supervision: Supervision::default(),
            review: ReviewLimits::default(),
            notify: Notify::default(),
            embed: Embed::default(),
            external: External::default(),
            docs: Docs::default(),
            launch: Launch::default(),
            harness: Harness {
                id: crate::adapter::opencode::OpenCodeAdapter::ID.into(),
                version: crate::adapter::opencode::OpenCodeAdapter::PINNED_VERSION.into(),
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
                harness_image: "localhost/tracon-harness-opencode".into(),
                selinux_label_disable: None,
                start_machine: true,
                stop_timeout_secs: 10,
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
                worktree_root: default_worktree_root(),
                pty_buffer_frames: 256,
                pty_capture_output: false,
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
    /// The Podman image a harness runs in. `[boundary] harness_image` names
    /// the image for the configured `[harness] id`, as it always has; every
    /// other supported harness runs from the image `tracon setup` builds for
    /// it under the conventional name, so a node can hold both without the
    /// operator naming both.
    pub fn podman_harness_image(&self, harness_id: &str) -> String {
        if harness_id == self.harness.id {
            return self.boundary.harness_image.clone();
        }
        format!("localhost/tracon-harness-{harness_id}")
    }

    /// The same for a pod-hosted node: the configured image for the configured
    /// harness, the release's own image for the other.
    pub fn kubernetes_harness_image(&self, harness_id: &str) -> String {
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

    /// Where `tracon setup` writes the embedded container definitions.
    pub fn containers_dir() -> PathBuf {
        Self::state_dir().join("containers")
    }

    pub fn allow_file() -> PathBuf {
        Self::state_dir().join("gateway/allow.txt")
    }

    /// The `[[repo]]` table as it stands now: what the operator last saved
    /// through the running node, else what the file held at load.
    pub fn repos(&self) -> std::sync::Arc<Vec<Repo>> {
        match &*self.live_repo.0.read() {
            Some(live) => live.clone(),
            None => std::sync::Arc::new(self.repo.clone()),
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
                let mut config: Self = toml::from_str(&text)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                for (name, provider) in default_providers() {
                    config.providers.entry(name).or_insert(provider);
                }
                config.review.lift_retired_defaults();
                if let Some(section) = retired_qa_section(&text) {
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
                    .ui
                    .opencode_origin()
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                validate_repos(&config.repo)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                Ok(config)
            }
            Err(_) => Ok(Self::default()),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::config_path())
    }

    /// Held from `try_load` to `save` by every edit of `node.toml` this
    /// process makes. `save_to` writes the difference between the file as it
    /// is now and `self`, so an edit loaded before another one was saved
    /// writes that other edit's keys back to what it loaded: two settings
    /// saved together, or an enrolment finishing while the repository table
    /// is saved, would otherwise lose one of them. A `std` guard, so a
    /// handler cannot hold it across an `.await`.
    pub fn edit_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Write what changed, and only that.
    ///
    /// The file keeps every key it already had, and gains or changes only the
    /// values that differ from what loading it gives. Serializing the whole
    /// struct wrote every default into the file, where it then stayed fixed:
    /// a node whose settings were saved once kept that release's harness image
    /// (`runtime.kubernetes.harness_image`) through every upgrade after it,
    /// because a written value is never a default again.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let (mut doc, before) = match std::fs::read_to_string(path) {
            // A file that does not parse has nothing to keep: it is replaced,
            // as it always was. (`put_config` refuses before reaching here.)
            Ok(text) => match (
                toml::from_str::<toml::Table>(&text),
                Self::try_load_from(path),
            ) {
                (Ok(doc), Ok(before)) => (doc, before),
                _ => (toml::Table::new(), Self::default()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (toml::Table::new(), Self::default())
            }
            Err(e) => return Err(e),
        };
        let before = toml::Table::try_from(&before).map_err(std::io::Error::other)?;
        let after = toml::Table::try_from(self).map_err(std::io::Error::other)?;
        write_changes(&mut doc, &before, &after, &[]);
        let text = toml::to_string_pretty(&doc).map_err(std::io::Error::other)?;
        write_replacing(path, text.as_bytes())
    }
}

/// Replace `path` by renaming a finished file over it, so a reader never sees
/// it truncated or half-written: `try_load` does not hold `edit_lock`, and an
/// empty file parses as the defaults. A symlinked `node.toml` keeps its link,
/// and the file keeps its mode.
fn write_replacing(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let tmp = target.with_extension(format!("toml.{}.tmp", uuid::Uuid::now_v7()));
    let written = std::fs::write(&tmp, bytes).and_then(|()| {
        if let Ok(meta) = std::fs::metadata(&target) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &target)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Carry into `doc` every value that differs between `before` (what the file
/// loads as) and `after` (what is being saved), leaving everything else as the
/// file had it — present or absent.
fn write_changes(doc: &mut toml::Table, before: &toml::Table, after: &toml::Table, at: &[&str]) {
    for key in before.keys() {
        if !after.contains_key(key) {
            doc.remove(key);
        }
    }
    for (key, now) in after {
        let was = before.get(key);
        if was == Some(now) {
            continue;
        }
        // A provider the file does not name comes from the defaults on load,
        // entry by entry: a partial entry written here would shadow the whole
        // default one and lose its credential and upstream. Write it whole.
        let whole_entry = at == ["providers"] && !doc.contains_key(key);
        match (was, now) {
            (Some(toml::Value::Table(was)), toml::Value::Table(now)) if !whole_entry => {
                let entry = doc
                    .entry(key.clone())
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if !entry.is_table() {
                    *entry = toml::Value::Table(toml::Table::new());
                }
                if let toml::Value::Table(entry) = entry {
                    let mut path = at.to_vec();
                    path.push(key.as_str());
                    write_changes(entry, was, now, &path);
                }
            }
            _ => {
                doc.insert(key.clone(), now.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {

    /// Saving writes the change, not the release's defaults: a default that is
    /// written stops being one, which is how a node's harness image stayed at
    /// the release its settings were first saved under.
    #[test]
    fn a_save_writes_what_changed_and_no_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let mut cfg = Config::default();
        cfg.harness.id = "claude".into();
        cfg.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("claude"), "{text}");
        assert!(
            !text.contains("harness_image"),
            "a default was written: {text}"
        );
        assert!(
            !text.contains("[providers"),
            "a default was written: {text}"
        );
        let back = Config::try_load_from(&path).unwrap();
        assert_eq!(back.harness.id, "claude");
        assert_eq!(
            back.runtime.kubernetes.harness_image,
            Config::default().runtime.kubernetes.harness_image
        );
    }

    #[test]
    fn a_save_keeps_what_the_file_already_says() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        std::fs::write(
            &path,
            "node_name = \"kept\"\n\n[session]\nbudget_tokens = 5\n",
        )
        .unwrap();
        let mut cfg = Config::try_load_from(&path).unwrap();
        cfg.harness.id = "claude".into();
        cfg.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("node_name = \"kept\""), "{text}");
        assert!(text.contains("budget_tokens = 5"), "{text}");
        assert!(!text.contains("harness_image"), "{text}");
        assert_eq!(Config::try_load_from(&path).unwrap().harness.id, "claude");
    }

    /// The save renames a new file into place, which must not turn a
    /// symlinked `node.toml` into a copy or reset its mode.
    #[cfg(unix)]
    #[test]
    fn a_save_keeps_the_files_link_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let link = dir.path().join("node.toml");
        std::fs::write(&real, "node_name = \"n\"\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut cfg = Config::try_load_from(&link).unwrap();
        cfg.harness.id = "claude".into();
        cfg.save_to(&link).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(&real).unwrap().contains("claude"));
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    /// A provider the file never named is a default entry; editing it must not
    /// leave a partial one that shadows its credential, or drop the others.
    #[test]
    fn a_provider_edit_on_a_file_without_providers_keeps_them_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        std::fs::write(&path, "node_name = \"n\"\n").unwrap();
        let mut cfg = Config::try_load_from(&path).unwrap();
        let codex = cfg.providers.get_mut("openai-codex").unwrap();
        codex.models.truncate(1);
        let want = codex.models.clone();
        cfg.save_to(&path).unwrap();
        let back = Config::try_load_from(&path).unwrap();
        let codex = &back.providers["openai-codex"];
        assert_eq!(codex.models, want);
        assert_eq!(codex.credential, "openai-codex");
        assert!(!codex.upstream.is_empty());
        assert!(back.providers.contains_key("anthropic"));
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

    /// The README's `node.toml` reference is checked, not trusted: every key it
    /// names must exist, and the values it shows as defaults must be them.
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
        assert_eq!(parsed.boundary.subnet, d.boundary.subnet);
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

    /// The table a running node resolves against is the file's until the
    /// operator saves one through the node, and theirs from then on — for
    /// everything sharing that `Config`, without a restart. A copy taken
    /// earlier keeps what it saw.
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
        config.save_to(&path).unwrap();
        let back = Config::try_load_from(&path).unwrap();
        assert_eq!(back.repo.len(), 2);
        assert_eq!(back.repo[1].checks.as_deref(), Some(&[][..]));
        assert!(back.repo[1].image.is_none() && back.repo[1].timeout_secs.is_none());

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
        assert_eq!(codex.login.as_deref(), Some("openai"));
        let _ = std::fs::remove_dir_all(dir);
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

    /// Tier 1: any provider name the catalogue knows gets its exact models,
    /// not only the three built-ins — the pure lookup, exercised directly so
    /// it needs no process-wide catalogue state.
    #[test]
    fn default_models_tier1_matches_any_catalogue_provider_key() {
        let catalogue = vec![ModelDecl {
            id: "some/model".into(),
            name: "Some Model".into(),
            context: 128_000,
            output: 8_000,
            reasoning: false,
            attachment: false,
        }];
        assert_eq!(
            default_models_tiered("openrouter", Some(catalogue.clone())),
            catalogue
        );
    }

    /// Tier 2: no catalogue hit falls back to the hardcoded three-entry
    /// match, so a locked-down node (or one that has not fetched yet) never
    /// regresses to zero models for a built-in provider.
    #[test]
    fn default_models_tier2_falls_back_to_hardcoded_entries() {
        assert_eq!(
            default_models_tiered("anthropic", None),
            hardcoded_default_models("anthropic")
        );
        assert_eq!(default_models_tiered("anthropic", None).len(), 3);
        assert_eq!(default_models_tiered("openai", None).len(), 2);
        let codex: Vec<_> = default_models_tiered("openai-codex", None)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            codex,
            ["gpt-5.5"],
            "a ChatGPT sign-in refuses the -codex models"
        );
    }

    /// Tier 3: neither the catalogue nor the hardcoded match knows this
    /// name, so it declares nothing rather than guessing.
    #[test]
    fn default_models_tier3_is_empty_for_an_unknown_provider() {
        assert!(default_models_tiered("totally-unknown", None).is_empty());
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

    #[test]
    fn the_opencode_ui_origin_is_its_own_and_origin_only() {
        let mut ui = Ui::default();
        // Not the operator listener's port, and not the preview listener's.
        assert_eq!(ui.opencode_origin().unwrap(), "http://127.0.0.1:7423");
        assert_ne!(
            ui.opencode_listen.port(),
            Docs::default().preview_listen.port()
        );
        assert!(ui.opencode_is_loopback());
        assert_eq!(ui.opencode_host().unwrap(), "127.0.0.1");

        ui.opencode_url = Some("https://opencode.example:8443".into());
        assert_eq!(
            ui.opencode_origin().unwrap(),
            "https://opencode.example:8443"
        );
        assert_eq!(ui.opencode_host().unwrap(), "opencode.example");
        // Published through an ingress: the cookie must carry `Secure`.
        assert!(!ui.opencode_is_loopback());

        for invalid in [
            "ftp://opencode.example",
            "https://opencode.example/path",
            "https://user@opencode.example",
            "https://opencode.example?x=1",
            "https://opencode.example#f",
        ] {
            ui.opencode_url = Some(invalid.into());
            assert!(ui.opencode_origin().is_err(), "{invalid}");
        }
    }
}
