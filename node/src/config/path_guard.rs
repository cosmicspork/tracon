//! Where a `node.toml` key may point when it names a program the node runs on
//! its host, or a file or directory it reads, sends on or writes.
//!
//! Remote configuration makes every key writable by whoever holds an admin
//! session, and an agent can write files: its workspace, its scratch, the
//! state volume its pod shares with the node. A key that could name one of
//! those files as a program would let an agent's bytes run as the node; a
//! key whose file the node reads and sends on (`embed.api_key_file` goes to
//! `embed.base_url` as a bearer token) could carry the node's identity seed
//! or the operator's GitHub token to a host the writer chose; a directory
//! the node writes or replaces could be pointed at one that matters.
//!
//! So each such key is held to rules for what it names
//! (`config_schema::PathKind`), at three points:
//!
//! - **An edit** (`config::edit`) refuses a change that makes any guarded key
//!   break a rule it did not already break. A key that was already wrong is
//!   reported, not a reason to refuse an unrelated edit.
//! - **Start** (`serve`) checks the whole file and warns about each key that
//!   breaks a rule. It does not refuse to start: a node that cannot start
//!   cannot be reconfigured remotely either.
//! - **Use**: a program is checked again just before every spawn, and refused
//!   there; the embedding key file before every read; the export directory
//!   before every export; the UI bundle directory before it is replaced. A
//!   file swapped after the edit that allowed its path is caught here.
//!
//! What "an agent can write" means here: the state directory (workspaces,
//! sessions and caches are staged there), the state volume on Kubernetes
//! (`runtime.kubernetes.state_mount`, which harness pods mount), the session
//! worktree root, Podman's container storage (where named volumes and
//! container layers live), and the node's runtime directory. Every path is
//! resolved through its symbolic links first, and each link's own directory
//! counts, so a link cannot launder a path in or out.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::Metadata;
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
    ("embed.api_key_file", PathKind::SecretFile),
    ("docs.export_dir", PathKind::WriteDir),
    ("ui.opencode_bundle_dir", PathKind::ManagedDir),
    ("session.worktree_root", PathKind::Root),
    ("external.repo_roots", PathKind::Root),
];

/// The keys whose reader expands a leading `~/` to the operator's home.
const EXPANDS_HOME: &[&str] = &["docs.export_dir", "external.repo_roots"];

/// What each program key may be called. A key that runs `gh` cannot be
/// pointed at a shell or an interpreter that happens to be installed
/// somewhere trusted: the node would hand it arguments, and run it in a
/// directory, that an agent chose. A name may carry a suffix after a dash
/// (`podman-remote`).
fn program_names(key: &str) -> &'static [&'static str] {
    match key {
        "boundary.podman" => &["podman"],
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

/// Where an operator's own credentials usually live, under their home. A
/// key whose file the node sends on is kept out of these as well as out of
/// the node's own secrets. Best effort: it names the usual places, not every
/// place a credential could be.
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
    ".config/gh",
    ".config/glab-cli",
    ".config/gcloud",
    "Library/Keychains",
];

const SYSTEM_CREDENTIAL_STORES: &[&str] = &[
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/sudoers",
    "/etc/ssh",
    "/private/etc/shadow",
    "/private/etc/sudoers",
    "/private/etc/ssh",
];

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
/// packages come from, a script instead of a command.
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
const UV_RUN_PATHS: &[&str] = &["--project", "--directory"];

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

/// A directory the rules name, as written and as it resolves, so a path
/// reached through either spelling (`/tmp` and `/private/tmp`, `/home` and
/// `/var/home`) is recognised.
#[derive(Debug, Clone)]
struct Place {
    label: String,
    lexical: PathBuf,
    canonical: Option<PathBuf>,
}

impl Place {
    fn new(label: impl Into<String>, path: PathBuf) -> Self {
        Self {
            label: label.into(),
            canonical: std::fs::canonicalize(&path).ok(),
            lexical: path,
        }
    }

    /// `path` is this place or inside it.
    fn holds(&self, path: &Path) -> bool {
        path.starts_with(&self.lexical)
            || self
                .canonical
                .as_deref()
                .is_some_and(|c| path.starts_with(c))
    }

    /// This place is `path` or inside it.
    fn within(&self, path: &Path) -> bool {
        self.lexical.starts_with(path)
            || self
                .canonical
                .as_deref()
                .is_some_and(|c| c.starts_with(path))
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
    pub home: Option<PathBuf>,
    /// The `PATH` a bare program name is looked up on.
    pub path_var: Option<OsString>,
    /// Places an agent can write besides those `node.toml` names.
    pub agent_roots: Vec<(String, PathBuf)>,
    /// Package managers' prefixes, where an operator's tools are installed
    /// and owned by the operator's own user (Homebrew on macOS and Linux,
    /// `~/.local/bin`, `~/.cargo/bin`). A program there may be writable by
    /// the node's user; anywhere else it may not.
    pub trusted_prefixes: Vec<PathBuf>,
}

impl Layout {
    pub fn of_this_process() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute());
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
        if let Some(home) = &home {
            trusted_prefixes.push(home.join(".local/bin"));
            trusted_prefixes.push(home.join(".cargo/bin"));
        }
        let config_path = Config::config_path();
        Self {
            state_dir: Config::state_dir(),
            config_dir: config_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or(config_path),
            home,
            path_var: std::env::var_os("PATH"),
            agent_roots,
            trusted_prefixes,
        }
    }
}

/// The rules, for one node: its layout, and the places one or more
/// `node.toml` versions name. An edit is checked against both the file it
/// replaces and the result, so a key that moves a root cannot unmake the
/// root another key is checked against in the same edit.
#[derive(Debug, Clone)]
pub struct Guard {
    off: bool,
    euid: u32,
    egid: u32,
    state: Place,
    config: Place,
    home: Option<Place>,
    agent_roots: Vec<Place>,
    worktree_roots: Vec<Place>,
    trusted: Vec<Place>,
    secrets: Vec<PathBuf>,
    credential_stores: Vec<PathBuf>,
    path_var: Option<OsString>,
}

impl Guard {
    pub fn new(layout: Layout, configs: &[&Config]) -> Self {
        let mut agent_roots = vec![Place::new("the state directory", layout.state_dir.clone())];
        for (label, path) in &layout.agent_roots {
            agent_roots.push(Place::new(label.clone(), path.clone()));
        }
        let mut worktree_roots = Vec::new();
        for cfg in configs {
            let mount = &cfg.runtime.kubernetes.state_mount;
            if mount.is_absolute() {
                agent_roots.push(Place::new("the state volume", mount.clone()));
            }
            let root = &cfg.session.worktree_root;
            if root.is_absolute() {
                let place = Place::new("the session worktree root", root.clone());
                agent_roots.push(place.clone());
                worktree_roots.push(place);
            }
        }
        let mut credential_stores: Vec<PathBuf> =
            SYSTEM_CREDENTIAL_STORES.iter().map(PathBuf::from).collect();
        if let Some(home) = &layout.home {
            credential_stores.extend(HOME_CREDENTIAL_STORES.iter().map(|s| home.join(s)));
        }
        Self {
            off: false,
            // SAFETY: geteuid has no preconditions and cannot fail.
            euid: unsafe { libc::geteuid() },
            // SAFETY: as geteuid.
            egid: unsafe { libc::getegid() },
            secrets: NODE_SECRETS
                .iter()
                .map(|name| layout.state_dir.join(name))
                .collect(),
            state: Place::new("the state directory", layout.state_dir),
            config: Place::new("the configuration directory", layout.config_dir),
            home: layout
                .home
                .map(|home| Place::new("the home directory", home)),
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

    /// Checks nothing. For a test binary whose stand-in programs live in a
    /// temporary directory; nothing derives it from configuration.
    pub fn unchecked() -> Self {
        Self {
            off: true,
            ..Self::new(
                Layout {
                    state_dir: PathBuf::new(),
                    config_dir: PathBuf::new(),
                    home: None,
                    path_var: None,
                    agent_roots: Vec::new(),
                    trusted_prefixes: Vec::new(),
                },
                &[],
            )
        }
    }

    /// Every guarded key in `cfg` that breaks a rule.
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
            Some(_) => self.path(key, value),
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
        if !names.is_empty() && !named(name, names) {
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
        let on_path = self
            .path_var
            .as_deref()
            .map(|path| std::env::split_paths(path).collect::<Vec<_>>())
            .unwrap_or_default();
        let candidates = on_path
            .iter()
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

    /// The `consulta` program, with its arguments held to the shape the
    /// node runs it in.
    pub fn consulta(&self, consulta: &Consulta) -> Result<PathBuf, Refusal> {
        let program = self.program("consulta.command", &consulta.command);
        if self.off {
            return program;
        }
        self.consulta_args(consulta)?;
        program
    }

    /// The arguments alone, by the program the command names, whether or
    /// not that program is installed yet.
    fn consulta_args(&self, consulta: &Consulta) -> Result<(), Refusal> {
        let refused = |problem: Problem| Refusal {
            key: "consulta.args".into(),
            value: consulta.args.join(" "),
            reason: problem.reason,
            missing: false,
        };
        let name = Path::new(&consulta.command)
            .file_name()
            .unwrap_or_else(|| OsStr::new(""));
        if named(name, &["uv"]) {
            return self.uv_run(&consulta.args).map_err(refused);
        }
        for arg in &consulta.args {
            let value = option_value(arg).unwrap_or(arg);
            if looks_like_path(value) {
                self.argument_path(value).map_err(refused)?;
            }
        }
        Ok(())
    }

    /// `uv run [options] consulta [arguments]`, and nothing else uv can be
    /// asked to run.
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
                let value = option_value(arg).unwrap_or(arg);
                if looks_like_path(value) {
                    self.argument_path(value)?;
                }
                continue;
            }
            if let Some((flag, value)) = arg.split_once('=') {
                if UV_RUN_PATHS.contains(&flag) {
                    self.argument_path(value)?;
                    continue;
                }
            }
            if UV_RUN_PATHS.contains(&arg.as_str()) {
                let value = args
                    .next()
                    .ok_or_else(|| refuse(format!("`{arg}` needs a directory after it")))?;
                self.argument_path(value)?;
                continue;
            }
            if UV_RUN_FLAGS.contains(&arg.as_str()) {
                continue;
            }
            if arg.starts_with('-') {
                return Err(refuse(format!(
                    "`uv run {arg}` could choose what runs; the options allowed here are {} and {}",
                    UV_RUN_PATHS.join(", "),
                    UV_RUN_FLAGS.join(", ")
                )));
            }
            let name = Path::new(arg).file_name().unwrap_or_else(|| OsStr::new(""));
            if !named(name, &["consulta"]) {
                return Err(refuse(format!(
                    "runs {arg:?} through uv; only consulta runs here"
                )));
            }
            if arg.contains('/') {
                self.argument_path(arg)?;
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

    /// A path among a program's arguments: what it names may decide what
    /// runs (a project, a script), so it is held to what a secret is,
    /// except that the node's own user may write it (the image's
    /// `/opt/consulta` is the node's, for uv's own bookkeeping).
    fn argument_path(&self, value: &str) -> Result<(), Problem> {
        let path = Path::new(value);
        plain_absolute(path)?;
        let walked = walk(path)?;
        self.outside_agent_roots(path, &walked)?;
        self.not_a_node_secret(path, &walked)?;
        outside(KERNEL, path, &walked, "a kernel interface")?;
        self.chains(&walked, Writers::Foreign)
    }

    /// A non-program key's value, by its kind.
    pub fn path(&self, key: &str, value: &str) -> Result<(), Refusal> {
        if self.off {
            return Ok(());
        }
        let Some(kind) = kind_of(key) else {
            return Ok(());
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
        match kind {
            PathKind::SecretFile | PathKind::SecretDir => self.secret(&path, kind),
            PathKind::WriteDir | PathKind::ManagedDir | PathKind::Root => {
                self.directory(&path, kind)
            }
            PathKind::Executable | PathKind::ExecArgs => Ok(()),
        }
        .map_err(refused)
    }

    fn expand_home(&self, value: &str) -> Result<PathBuf, Problem> {
        let path = Path::new(value);
        match (path.strip_prefix("~"), &self.home) {
            (Ok(rest), Some(home)) => Ok(home.lexical.join(rest)),
            (Ok(_), None) => Err(refuse("starts with ~, and the node has no home directory")),
            (Err(_), _) => Ok(path.to_path_buf()),
        }
    }

    fn secret(&self, path: &Path, kind: PathKind) -> Result<(), Problem> {
        plain_absolute(path)?;
        let walked = walk(path)?;
        self.not_a_node_secret(path, &walked)?;
        self.outside_agent_roots(path, &walked)?;
        outside(KERNEL, path, &walked, "a kernel interface")?;
        if kind == PathKind::SecretFile {
            for store in &self.credential_stores {
                if path.starts_with(store) || walked.resolved.starts_with(store) {
                    return Err(refuse(format!(
                        "is inside {}, where the operator's own credentials are kept; the node sends this file's content on",
                        store.display()
                    )));
                }
            }
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
        self.chains(&walked, Writers::Foreign)
    }

    fn directory(&self, path: &Path, kind: PathKind) -> Result<(), Problem> {
        plain_absolute(path)?;
        let walked = walk(path)?;
        let resolved = &walked.resolved;
        let forms = [path, resolved.as_path()];
        if forms.iter().any(|p| p.parent().is_none()) {
            return Err(refuse("is the root directory"));
        }
        outside(SYSTEM_DIRS, path, &walked, "a system directory")?;
        if kind != PathKind::Root {
            if let Some(home) = &self.home {
                if forms
                    .iter()
                    .any(|p| *p == home.lexical || home.canonical.as_deref() == Some(*p))
                {
                    return Err(refuse("is the home directory itself"));
                }
            }
        }
        if forms.iter().any(|p| self.config.holds(p)) {
            return Err(refuse(format!("is inside {}", self.config.shown())));
        }
        for place in [&self.config, &self.state] {
            if forms.iter().any(|p| place.within(p)) {
                return Err(refuse(format!("holds {}", place.shown())));
            }
        }
        if kind == PathKind::WriteDir {
            if forms.iter().any(|p| self.state.holds(p)) {
                return Err(refuse(format!(
                    "is inside {}, beside the node's database and identity",
                    self.state.shown()
                )));
            }
            if let Some(root) = self
                .worktree_roots
                .iter()
                .find(|root| forms.iter().any(|p| root.holds(p)))
            {
                return Err(refuse(format!("is inside {}", root.shown())));
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
        self.chains(&walked, Writers::Foreign)
    }

    fn executable_at(&self, path: &Path) -> Result<(), Problem> {
        let walked = walk(path)?;
        self.outside_agent_roots(path, &walked)?;
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
        for root in &self.agent_roots {
            if root.holds(path) || root.holds(&walked.resolved) {
                return Err(refuse(format!(
                    "is inside {}, which an agent can write",
                    root.shown()
                )));
            }
            if let Some(link) = walked.links.iter().find(|dir| root.holds(dir)) {
                return Err(refuse(format!(
                    "goes through a symbolic link in {}, inside {}, which an agent can write",
                    link.display(),
                    root.shown()
                )));
            }
        }
        Ok(())
    }

    fn not_a_node_secret(&self, path: &Path, walked: &Walked) -> Result<(), Problem> {
        let target = std::fs::metadata(&walked.resolved).ok();
        for secret in &self.secrets {
            let same_path = path == secret
                || walked.resolved == *secret
                || std::fs::canonicalize(secret).is_ok_and(|c| c == walked.resolved);
            // A hard link elsewhere is the same file under another name.
            let same_file = match (&target, std::fs::metadata(secret)) {
                (Some(target), Ok(secret)) => {
                    target.dev() == secret.dev() && target.ino() == secret.ino()
                }
                _ => false,
            };
            if same_path || same_file {
                let name = secret.file_name().unwrap_or_default().to_string_lossy();
                return Err(refuse(format!("is the node's own {name}")));
            }
        }
        Ok(())
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
        let by_node = (self.euid != 0 && uid == self.euid && mode & 0o200 != 0)
            || (own_group && mode & 0o020 != 0 && !sticky);
        if writers == Writers::AnyButRoot && by_node {
            return Err(refuse(format!(
                "{} is writable by the node's own user or group, so anything the node can be made to write could replace what it runs; install it where only root can write, or under a package manager's prefix",
                at.display()
            )));
        }
        Ok(())
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

fn named(name: &OsStr, names: &[&str]) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    names.iter().any(|wanted| {
        name == *wanted
            || name
                .strip_prefix(wanted)
                .is_some_and(|rest| rest.starts_with('-'))
    })
}

fn kind_of(key: &str) -> Option<PathKind> {
    GUARDED
        .iter()
        .find(|(guarded, _)| *guarded == key)
        .map(|(_, kind)| *kind)
}

/// `--flag=value`'s value.
fn option_value(arg: &str) -> Option<&str> {
    arg.strip_prefix('-')
        .and_then(|_| arg.split_once('='))
        .map(|(_, value)| value)
}

fn looks_like_path(value: &str) -> bool {
    !value.contains("://")
        && (value.starts_with('/')
            || value.starts_with('~')
            || value.starts_with('.')
            || value.contains('/'))
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
    while let Some(part) = pending.pop() {
        if part == ".." {
            resolved.pop();
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
                    existing,
                    links,
                    exists: false,
                });
            }
            Err(e) => return Err(refuse(format!("{}: {e}", next.display()))),
        }
    }
    Ok(Walked {
        existing: resolved.clone(),
        resolved,
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
    let default;
    let before = match before {
        Some(before) => before,
        None => {
            default = Config::default();
            &default
        }
    };
    let guard = Guard::new(Layout::of_this_process(), &[before, after]);
    let already: BTreeSet<(String, String)> = guard
        .check(before)
        .into_iter()
        .map(|refusal| (refusal.key, refusal.value))
        .collect();
    let new: Vec<String> = guard
        .check(after)
        .into_iter()
        .filter(|refusal| {
            !refusal.missing && !already.contains(&(refusal.key.clone(), refusal.value.clone()))
        })
        .map(|refusal| refusal.to_string())
        .collect();
    match new.is_empty() {
        true => Ok(()),
        false => Err(format!("refused by the path guard: {}", new.join("; "))),
    }
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

/// Make `guard` the one every spawn and read below is checked against. The
/// first one installed stays; `serve` installs the guard for the file it
/// loaded before it runs anything.
pub fn install(guard: Guard) {
    let _ = INSTALLED.set(guard);
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

/// A file or directory key's value, checked now, before the node reads,
/// writes or replaces what it names.
pub fn path(key: &str, value: &Path) -> std::io::Result<()> {
    match current() {
        Some(guard) => Ok(guard.path(key, &value.to_string_lossy())?),
        None => Ok(()),
    }
}

/// Warn about every key in the file `serve` loaded that the guard refuses,
/// then install the guard for it. A start does not refuse: what is refused
/// is running, reading or writing what such a key names.
pub fn install_for_serve(cfg: &Config) {
    let guard = Guard::new(Layout::of_this_process(), &[cfg]);
    for refusal in guard.check(cfg) {
        match refusal.missing {
            true => tracing::debug!(%refusal, "node.toml names a path that does not exist"),
            false => tracing::warn!(
                %refusal,
                "node.toml names a path the path guard refuses; the node will not run, read or write it until the key is fixed"
            ),
        }
    }
    if let Some(path) = &guard.path_var {
        for dir in std::env::split_paths(path) {
            if let Some(root) = guard.agent_roots.iter().find(|root| root.holds(&dir)) {
                tracing::warn!(
                    entry = %dir.display(),
                    root = %root.shown(),
                    "a PATH entry is inside a place an agent can write; programs there are never run"
                );
            }
        }
    }
    install(guard);
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A node laid out under one temporary directory: its state, config and
    /// home, a worktree root, and a "package manager prefix" the tests can
    /// put trusted programs in.
    struct Node {
        root: tempfile::TempDir,
    }

    impl Node {
        fn new() -> Self {
            let node = Node {
                root: tempfile::Builder::new()
                    .prefix("tracon-path-guard")
                    .tempdir()
                    .unwrap(),
            };
            for dir in ["state", "config", "home", "work", "brew/bin", "volume"] {
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
            while at.starts_with(self.root.path()) && at != self.root.path() {
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
                at.pop();
            }
            path
        }

        fn at(&self, rel: &str) -> PathBuf {
            self.root.path().join(rel)
        }

        fn layout(&self) -> Layout {
            Layout {
                state_dir: self.at("state"),
                config_dir: self.at("config"),
                home: Some(self.at("home")),
                path_var: Some(
                    std::env::join_paths([self.at("home/bin"), PathBuf::from("/usr/bin")]).unwrap(),
                ),
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
            Guard::new(self.layout(), &[&self.config()])
        }

        fn guard_with(&self, layout: Layout, cfg: &Config) -> Guard {
            Guard::new(layout, &[cfg])
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
    /// (and `/tmp`), which the node-writable rule would refuse for a program
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
        let git = ["/usr/bin/git", "/bin/git"]
            .into_iter()
            .find(|p| Path::new(p).exists());
        let Some(_) = git else {
            return;
        };
        let guard = node.guard();
        let found = guard.program("publish.git", "git").unwrap();
        assert!(found.is_absolute() && found.ends_with("git"), "{found:?}");
    }

    #[test]
    fn a_program_the_nodes_user_can_write_outside_a_trusted_prefix_is_refused() {
        let node = Node::new();
        let gh = node.program("elsewhere/gh");
        let refusal = node.guard().program("publish.gh", &shown(&gh)).unwrap_err();
        assert!(!refusal.missing);
        assert!(
            refusal
                .reason
                .contains("writable by the node's own user or group"),
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
        let sh = sh();
        let dir = node.dir("work/bin");
        // Named `git`, pointing at a real root-owned program: the link
        // itself is what an agent could repoint.
        let link = dir.join("git");
        std::os::unix::fs::symlink(&sh, &link).unwrap();
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
    fn a_path_entry_inside_the_state_volume_is_never_used() {
        // Kubernetes: HOME is /state/home, so ~/.local/bin is on the volume
        // harness pods mount.
        let node = Node::new();
        node.program("volume/home/.local/bin/buildctl");
        let mut layout = node.layout();
        layout.home = Some(node.at("volume/home"));
        layout.path_var = Some(
            std::env::join_paths([node.at("volume/home/.local/bin"), node.at("brew/bin")]).unwrap(),
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
        // package manager's prefix and refused outside one.
        let dir = node.at("brew/own-group");
        let gh = node.program("brew/own-group/gh");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        node.guard().program("publish.gh", &shown(&gh)).unwrap();
        let dir = node.at("elsewhere/own-group");
        let gh = node.program("elsewhere/own-group/gh");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        let refusal = node.guard().program("publish.gh", &shown(&gh)).unwrap_err();
        assert!(refusal.reason.contains("own user or group"), "{refusal}");
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
    fn a_program_must_be_the_one_its_key_runs() {
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
    }

    #[test]
    fn a_bare_name_not_on_path_is_missing_not_unsafe() {
        let node = Node::new();
        let refusal = node
            .guard()
            .program("publish.glab", "glab-not-installed-anywhere")
            .unwrap_err();
        assert!(refusal.missing, "{refusal}");
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
    fn the_embedding_key_cannot_be_the_nodes_own_secrets() {
        let node = Node::new();
        let db = node.file("state/node.db", "SQLite format 3");
        let seed = node.file("state/node-identity.seed", &"ab".repeat(32));
        let guard = node.guard();
        for secret in [&db, &seed] {
            let refusal = guard
                .path("embed.api_key_file", &shown(secret))
                .unwrap_err();
            assert!(refusal.reason.contains("node's own"), "{refusal}");
        }
        // Not through a link outside the state directory, nor a hard link.
        let link = node.at("home/key");
        std::os::unix::fs::symlink(&seed, &link).unwrap();
        let refusal = guard.path("embed.api_key_file", &shown(&link)).unwrap_err();
        assert!(refusal.reason.contains("node's own"), "{refusal}");
        let hard = node.at("home/key-2");
        std::fs::hard_link(&seed, &hard).unwrap();
        let refusal = guard.path("embed.api_key_file", &shown(&hard)).unwrap_err();
        assert!(refusal.reason.contains("node-identity.seed"), "{refusal}");
    }

    #[test]
    fn the_embedding_key_cannot_be_an_operators_credentials_or_the_kernel() {
        let node = Node::new();
        let token = node.file("home/.config/gh/hosts.yml", "oauth_token: x");
        let guard = node.guard();
        let refusal = guard
            .path("embed.api_key_file", &shown(&token))
            .unwrap_err();
        assert!(refusal.reason.contains("credentials"), "{refusal}");
        let refusal = guard
            .path("embed.api_key_file", "/proc/self/environ")
            .unwrap_err();
        assert!(refusal.reason.contains("kernel"), "{refusal}");
        // The operator's own key file, in their config, is fine.
        let key = node.file("home/.config/embed.key", "sk-1");
        guard.path("embed.api_key_file", &shown(&key)).unwrap();
    }

    #[test]
    fn a_secret_file_an_agent_could_plant_is_refused() {
        let node = Node::new();
        let planted = node.file("work/tracon-1/token", "x");
        let refusal = node
            .guard()
            .path("runtime.kubernetes.builder.do_token_file", &shown(&planted))
            .unwrap_err();
        assert!(refusal.reason.contains("agent"), "{refusal}");
    }

    #[test]
    fn the_export_directory_stays_out_of_system_config_state_and_worktrees() {
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
            (shown(node.root.path()), "holds"),
        ] {
            let refusal = guard.path("docs.export_dir", &dir).unwrap_err();
            assert!(refusal.reason.contains(words), "{dir}: {refusal}");
        }
        guard
            .path("docs.export_dir", &shown(&node.at("home/notes")))
            .unwrap();
        guard.path("docs.export_dir", "~/notes").unwrap();
    }

    #[test]
    fn a_worktree_root_may_not_be_slash_or_hold_the_state_or_config() {
        let node = Node::new();
        let guard = node.guard();
        for dir in [
            "/".to_string(),
            shown(node.root.path()),
            "/usr/local/src".into(),
        ] {
            assert!(
                guard.path("session.worktree_root", &dir).is_err(),
                "{dir} was allowed"
            );
        }
        guard
            .path("session.worktree_root", &shown(&node.at("work")))
            .unwrap();
        guard.path("external.repo_roots", "~/src").unwrap();
        // A link to `/` is `/`.
        let link = node.at("home/everything");
        std::os::unix::fs::symlink("/", &link).unwrap();
        assert!(guard.path("external.repo_roots", &shown(&link)).is_err());
    }

    #[test]
    fn consulta_runs_only_consulta_through_uv() {
        let node = Node::new();
        let uv = node.program("brew/bin/uv");
        node.dir("home/src/consulta");
        let guard = node.guard();
        let consulta = |args: &[&str]| Consulta {
            command: shown(&uv),
            args: args.iter().map(|s| s.to_string()).collect(),
            timeout_secs: 60,
        };
        let project = shown(&node.at("home/src/consulta"));
        guard
            .consulta(&consulta(&["run", "--project", &project, "consulta"]))
            .unwrap();
        guard
            .consulta(&consulta(&[
                "run",
                "--frozen",
                &format!("--project={project}"),
                "consulta",
            ]))
            .unwrap();
        for (args, words) in [
            (vec!["run", "python", "-c", "x"], "only consulta"),
            (
                vec!["run", "--with", "evil", "consulta"],
                "could choose what runs",
            ),
            (vec!["tool", "run", "consulta"], "must start with `run`"),
            (vec!["run", "--project"], "needs a directory"),
            (vec!["run"], "names no program"),
        ] {
            let refusal = guard.consulta(&consulta(&args)).unwrap_err();
            assert_eq!(refusal.key, "consulta.args");
            assert!(refusal.reason.contains(words), "{args:?}: {refusal}");
        }
        let planted = shown(&node.dir("state/sessions/x"));
        let refusal = guard
            .consulta(&consulta(&["run", "--project", &planted, "consulta"]))
            .unwrap_err();
        assert!(refusal.reason.contains("agent"), "{refusal}");
        // A shell is not consulta's command at all.
        let refusal = guard
            .consulta(&Consulta {
                command: shown(&sh()),
                args: vec!["-c".into(), "id".into()],
                timeout_secs: 60,
            })
            .unwrap_err();
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
}
