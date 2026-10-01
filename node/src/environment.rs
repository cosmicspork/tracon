//! Safe project preparation in a runtime-owned workspace.  Preparation is a
//! separate, credential-free runner command; it never interprets repository
//! setup hooks or grants the agent an additional mount.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    boundary::Backend,
    config::{Config, Repo},
    runner::{Mount, RunnerCommand},
    store::Store,
    workspace::Workspace,
};

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid devcontainer.json: {0}")]
    Devcontainer(String),
    #[error("devcontainer requires a pinned image or an operator-approved image")]
    UnapprovedImage,
    #[error("devcontainer field {0} is not permitted for restricted execution")]
    UnsafeDevcontainer(&'static str),
    #[error("no supported locked dependency input was found")]
    NoDependencyInput,
    #[error("runtime refused: {0}")]
    Runtime(String),
    #[error("preparation command failed ({exit}): {tail}")]
    PrepareFailed { exit: i32, tail: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyInput {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparationPlan {
    /// The project-selected image. None means the already-approved harness
    /// image, which is the safe fallback for conventional lockfile projects.
    pub image: Option<String>,
    pub dependency_inputs: Vec<DependencyInput>,
    pub command: String,
    pub cache_volume: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparedEnvironment {
    pub image: String,
    pub cache_volume: String,
    pub dependency_inputs: Vec<DependencyInput>,
    pub command: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Verification {
    pub command: String,
    pub ok: bool,
    pub exit: i32,
    pub tail: String,
    pub elapsed_ms: u64,
}

/// `image_source` for an image the node built from the repository's Dockerfile.
pub const REPOSITORY_DOCKERFILE: &str = "repository Dockerfile";

/// Everything the node resolved for one repository's checks and preparation:
/// the operator's `[[repo]]` entry where there is one, the node-wide answer
/// for whatever it left out. Resolved once and passed around whole, so the run
/// path and the approval path cannot disagree about what ran or where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoEnvironment {
    /// The configured image reference, before any runtime confirmed a digest
    /// for it.
    pub image: String,
    /// Whose image that is. A reader of a check that could not run has to be
    /// told: the operator's entry for this repository, the image the node
    /// built from the repository's Dockerfile, or the node's own harness image
    /// because the entry (or the whole table) names none — or names a
    /// Dockerfile nothing has built yet.
    pub image_source: &'static str,
    pub checks: Vec<String>,
    pub timeout_secs: u64,
    /// Commands run before each check, with `egress` reachable and the cache
    /// writable.
    pub prepare: Vec<String>,
    /// Literal hosts preparation may reach; presets already expanded.
    pub egress: Vec<String>,
    /// `egress` as the entry wrote it — preset names and hosts — and only
    /// when the entry opens it to the repository's sessions. What a session
    /// is told it can reach, in the words its operator used.
    pub session_egress: Vec<String>,
    /// The repository's dependency cache: written by preparation, read-only
    /// to a check.
    pub cache_volume: String,
}

/// The environment for `repo`: the first matching `[[repo]]` entry over the
/// node-wide defaults, or the defaults alone when nothing matches.
///
/// The fallback is what the node did before the table existed, so a node with
/// no entries behaves exactly as it used to — minus the pretence that a
/// missing tool was the agent's failing check.
///
/// An entry that names a Dockerfile resolves to what this node last built from
/// it (`repo_image`). This only reads: building is `repo_image::ensure_base`,
/// which a run calls first and an approval never does, so approving cannot
/// start a build and both resolve the same image afterwards.
pub fn environment_for(cfg: &Config, store: &Store, repo: Option<&Path>) -> RepoEnvironment {
    // `repo` is `None` for work the node cannot tie to a repository at all; no
    // entry can claim that, so every one of them is skipped.
    let entry = repo.and_then(|repo| cfg.repo.iter().find(|entry| entry.matches(repo)));
    let checks = match entry.and_then(|entry| entry.checks.as_ref()) {
        Some(checks) => checks,
        None => &cfg.supervision.checks,
    };
    let built = || {
        let repo = repo?.to_string_lossy();
        store
            .ready_repo_image(&repo, crate::repo_image::BASE)
            .ok()
            .flatten()
    };
    let (image, image_source) = match entry {
        Some(Repo {
            image: Some(image), ..
        }) => (image.clone(), "repository toolchain"),
        Some(Repo {
            dockerfile: Some(_),
            ..
        }) => match built() {
            Some(row) => (row.image, REPOSITORY_DOCKERFILE),
            None => (
                harness_image(cfg),
                "harness image (repository image not built)",
            ),
        },
        _ => (harness_image(cfg), "harness image"),
    };
    let mut hash = Sha256::new();
    if let Some(repo) = repo {
        hash.update(repo.as_os_str().as_encoded_bytes());
    }
    hash.update([0]);
    hash.update(image.as_bytes());
    RepoEnvironment {
        checks: checks
            .iter()
            .map(|command| command.trim())
            .filter(|command| !command.is_empty())
            .map(str::to_string)
            .collect(),
        timeout_secs: entry
            .and_then(|entry| entry.timeout_secs)
            .unwrap_or(cfg.supervision.timeout_secs)
            .max(1),
        prepare: entry.map(|entry| entry.prepare.clone()).unwrap_or_default(),
        // Refused at load when it does not parse; a table built any other way
        // fails closed, with no egress at all.
        egress: entry
            .and_then(|entry| entry.egress_hosts().ok())
            .unwrap_or_default(),
        session_egress: entry
            .filter(|entry| entry.session_egress && entry.egress_hosts().is_ok())
            .map(|entry| entry.egress.clone())
            .unwrap_or_default(),
        cache_volume: format!("tracon-cache-{}", &hex::encode(hash.finalize())[..24]),
        image,
        image_source,
    }
}

impl RepoEnvironment {
    /// Whether the repository's entry answers for the image, so nothing in the
    /// repository itself is asked.
    pub fn names_image(&self) -> bool {
        self.image_source != "harness image"
    }
}

/// The repository a session's work came from. A session resumed on an
/// existing workspace records `workspace://<id>`, and a workspace's id is the
/// session that first imported it, so the chain is followed back to a real
/// path — bounded, because a store is not trusted to be acyclic. `None` for a
/// session the store no longer has, or one with no repository (a harness the
/// operator runs themselves).
pub fn origin_repo(store: &Store, session_id: &str) -> Result<Option<PathBuf>, String> {
    let mut id = session_id.to_string();
    for _ in 0..8 {
        let Some(session) = store.get_session(&id).map_err(|error| error.to_string())? else {
            return Ok(None);
        };
        match session.repo_path.strip_prefix("workspace://") {
            Some(workspace) if workspace != id => id = workspace.to_string(),
            Some(_) => return Ok(None),
            None if session.repo_path.trim().is_empty() => return Ok(None),
            None => return Ok(Some(session.repo_path.into())),
        }
    }
    Ok(None)
}

/// The environment of the repository a session is working in. A store that
/// cannot answer resolves to the node-wide defaults, as a session with no
/// repository does.
pub fn session_environment(cfg: &Config, store: &Store, session_id: &str) -> RepoEnvironment {
    let repo = origin_repo(store, session_id).ok().flatten();
    environment_for(cfg, store, repo.as_deref())
}

/// The image the node's own sessions run in, for this runtime kind.
pub fn harness_image(cfg: &Config) -> String {
    match cfg.runtime.kind {
        crate::config::RuntimeKind::Podman => cfg.boundary.harness_image.clone(),
        crate::config::RuntimeKind::Kubernetes => cfg.runtime.kubernetes.harness_image.clone(),
    }
}

/// A workspace's own dependency cache, mounted writable into its sessions.
/// What an agent installs lands here and nowhere a check reads.
pub fn session_cache_volume(workspace_id: &str) -> String {
    crate::workspace::volume_name(workspace_id).replacen("tracon-workspace-", "tracon-cache-w-", 1)
}

/// Where each package manager keeps what it fetched, under the cache mount.
/// Preparation and the check after it get the same names, so what one
/// downloaded the other finds.
pub fn cache_env() -> Vec<(String, String)> {
    [
        ("CARGO_HOME", "/cache/cargo"),
        ("npm_config_cache", "/cache/npm"),
        ("BUN_INSTALL_CACHE_DIR", "/cache/bun"),
        ("PIP_CACHE_DIR", "/cache/pip"),
        ("UV_CACHE_DIR", "/cache/uv"),
        ("COMPOSER_CACHE_DIR", "/cache/composer"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect()
}

/// A preparation's way out, and the proxy variables that send its container
/// through it instead of through the harness proxy. Dropping it closes it.
pub struct ScopedEgress {
    _grant: Option<crate::boundary::EgressGrant>,
    pub env: Vec<(String, String)>,
}

/// What a session is told when it asks for a host its repository does not
/// open to it. It names the one thing that changes the answer, which is the
/// operator's to change and not the agent's to work around.
pub const SESSION_REFUSAL: &str =
    "not reachable from a session; add it to this repository's egress";

/// A session's own way out: the hosts every harness may reach, and its
/// repository's registries where the entry opens them to sessions. Held for
/// the life of the session. Every refusal under it is this session's, and is
/// recorded on it (`gateway::proxy::Grants::on_refusal`).
pub fn session_grant(
    cfg: &Config,
    environment: &RepoEnvironment,
    session_id: &str,
) -> crate::gateway::proxy::GrantSpec {
    let hosts = match environment.session_egress.is_empty() {
        true => Vec::new(),
        false => environment.egress.clone(),
    };
    crate::gateway::proxy::GrantSpec {
        client: "session".into(),
        session_id: Some(session_id.to_string()),
        hosts,
        patterns: cfg.gateway.allow_hosts.clone(),
        refusal: SESSION_REFUSAL.into(),
        ..Default::default()
    }
}

/// What a preparation is told when it asks for a host its repository's entry
/// does not name.
pub const PREPARATION_REFUSAL: &str =
    "not reachable from preparation; add it to this repository's egress";

/// Open the backend's egress to exactly `hosts` for one preparation. The
/// grant is this preparation's own: its credentials reach these hosts and
/// nobody else's do, so it neither waits for another run nor widens one. No
/// hosts means nothing is opened and the container keeps the harness proxy,
/// which serves it nothing.
///
/// The grant filters by host, not by method: a host named here that accepts
/// uploads accepts them from this preparation. That is the cost of naming it.
pub fn scoped_egress(backend: &dyn Backend, hosts: &[String]) -> Result<ScopedEgress, String> {
    if hosts.is_empty() {
        return Ok(ScopedEgress {
            _grant: None,
            env: Vec::new(),
        });
    }
    let grant = backend
        .egress_grant(crate::gateway::proxy::GrantSpec {
            client: "prepare".into(),
            hosts: hosts.to_vec(),
            refusal: PREPARATION_REFUSAL.into(),
            ..Default::default()
        })
        .map_err(|error| format!("could not open preparation egress: {error}"))?;
    Ok(ScopedEgress {
        env: grant.env(),
        _grant: Some(grant),
    })
}

/// Inspect normal project conventions without executing repository-controlled
/// commands. `devcontainer.json` supplies an image only when it is a plain,
/// pinned image configuration; setup hooks, mounts, sockets, privilege, and
/// feature installers are rejected rather than partially honored.
///
/// A repository whose `[[repo]]` entry names its image is not asked: the
/// operator already answered, and a devcontainer that builds (as most do)
/// would otherwise refuse a preparation the entry makes possible.
pub fn inspect(
    workspace: &Path,
    environment: &RepoEnvironment,
) -> Result<PreparationPlan, EnvironmentError> {
    let image = match environment.names_image() {
        true => None,
        false => inspect_devcontainer(workspace)?,
    };
    let mut inputs = Vec::new();
    let mut command = None;
    for (file, prepare) in [
        ("Cargo.lock", "cargo fetch --locked"),
        ("package-lock.json", "npm ci --ignore-scripts"),
        ("npm-shrinkwrap.json", "npm ci --ignore-scripts"),
        ("bun.lock", "bun install --frozen-lockfile --ignore-scripts"),
        (
            "bun.lockb",
            "bun install --frozen-lockfile --ignore-scripts",
        ),
        (
            "pnpm-lock.yaml",
            "pnpm install --frozen-lockfile --ignore-scripts",
        ),
        (
            "yarn.lock",
            "yarn install --frozen-lockfile --ignore-scripts",
        ),
        (
            "composer.lock",
            "composer install --no-interaction --no-scripts --prefer-dist",
        ),
    ] {
        let path = workspace.join(file);
        if path.is_file() {
            inputs.push(DependencyInput {
                path: file.into(),
                sha256: file_hash(&path)?,
            });
            command.get_or_insert(prepare);
        }
    }
    let command = command.ok_or(EnvironmentError::NoDependencyInput)?;
    let mut hash = Sha256::new();
    if let Some(image) = &image {
        hash.update(image.as_bytes());
    }
    for input in &inputs {
        hash.update(input.path.as_bytes());
        hash.update(input.sha256.as_bytes());
    }
    Ok(PreparationPlan {
        image,
        dependency_inputs: inputs,
        command: command.to_string(),
        cache_volume: format!("tracon-cache-{}", &hex::encode(hash.finalize())[..24]),
    })
}

/// Execute dependency preparation in an isolated cache. Only a project image
/// with an immutable digest, or one an operator explicitly approved in config,
/// can replace the node's own prepared runner image. `environment` is that of
/// the repository the workspace came from, so preparation happens in the same
/// toolchain image its required checks will run in and reaches the hosts its
/// `[[repo]]` entry names.
pub async fn prepare(
    backend: &dyn Backend,
    cfg: &Config,
    workspace: &Workspace,
    plan: &PreparationPlan,
    environment: &RepoEnvironment,
) -> Result<PreparedEnvironment, EnvironmentError> {
    let image = approved_image(environment, cfg, plan.image.as_deref())?;
    let egress = scoped_egress(backend, &environment.egress).map_err(EnvironmentError::Runtime)?;
    let mut env = cache_env();
    env.push(("HOME".into(), "/cache/home".into()));
    env.extend(egress.env.iter().cloned());
    let runner = backend.runner(Vec::new());
    let started = Instant::now();
    let output = runner
        .run_capture(RunnerCommand {
            argv: vec!["sh".into(), "-lc".into(), plan.command.clone()],
            env,
            mounts: vec![
                workspace.mount("/work", false),
                Mount::volume(plan.cache_volume.clone(), "/cache", false),
            ],
            workdir: Some("/work".into()),
            image: Some(image.clone()),
            name: format!("tracon-prepare-{}", workspace.id),
            expose: None,
        })
        .await;
    drop(egress);
    let output = output.map_err(|e| EnvironmentError::Runtime(e.to_string()))?;
    let exit = output.status.code().unwrap_or(-1);
    let tail = tail(&[output.stdout, output.stderr].concat());
    if !output.status.success() {
        return Err(EnvironmentError::PrepareFailed { exit, tail });
    }
    Ok(PreparedEnvironment {
        image,
        cache_volume: plan.cache_volume.clone(),
        dependency_inputs: plan.dependency_inputs.clone(),
        command: plan.command.clone(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// Run operator-selected candidate verification in a fresh restricted command.
/// The caller supplies commands from node policy, never from repository files
/// or an agent tool request; the candidate workspace gets no broker credential.
pub async fn verify(
    backend: &dyn Backend,
    workspace: &Workspace,
    prepared: &PreparedEnvironment,
    commands: &[String],
) -> Result<Vec<Verification>, EnvironmentError> {
    let runner = backend.runner(Vec::new());
    let mut result = Vec::new();
    for (n, command) in commands.iter().enumerate() {
        if command.trim().is_empty() {
            continue;
        }
        let started = Instant::now();
        let output = runner
            .run_capture(RunnerCommand {
                argv: vec!["sh".into(), "-lc".into(), command.clone()],
                env: Vec::new(),
                mounts: vec![
                    workspace.mount("/work", true),
                    Mount::volume(prepared.cache_volume.clone(), "/cache", true),
                ],
                workdir: Some("/work".into()),
                image: Some(prepared.image.clone()),
                name: format!("tracon-verify-{}-{n}", workspace.id),
                expose: None,
            })
            .await
            .map_err(|e| EnvironmentError::Runtime(e.to_string()))?;
        result.push(Verification {
            command: command.clone(),
            ok: output.status.success(),
            exit: output.status.code().unwrap_or(-1),
            tail: tail(&[output.stdout, output.stderr].concat()),
            elapsed_ms: started.elapsed().as_millis() as u64,
        });
    }
    Ok(result)
}

fn inspect_devcontainer(workspace: &Path) -> Result<Option<String>, EnvironmentError> {
    let path = workspace.join(".devcontainer/devcontainer.json");
    if !path.exists() {
        return Ok(None);
    }
    let source = std::fs::read_to_string(&path)?;
    let value: Value =
        serde_json::from_str(&source).map_err(|e| EnvironmentError::Devcontainer(e.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| EnvironmentError::Devcontainer("top level must be an object".into()))?;
    for unsafe_key in [
        "privileged",
        "capAdd",
        "mounts",
        "workspaceMount",
        "runArgs",
        "initializeCommand",
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "features",
        "containerEnv",
        "remoteEnv",
    ] {
        if object.get(unsafe_key).is_some_and(non_empty) {
            return Err(EnvironmentError::UnsafeDevcontainer(unsafe_key));
        }
    }
    match object.get("image") {
        Some(Value::String(image)) if !image.trim().is_empty() => Ok(Some(image.clone())),
        Some(_) => Err(EnvironmentError::Devcontainer(
            "image must be a string".into(),
        )),
        None => Err(EnvironmentError::Devcontainer(
            "a build-based devcontainer is not allowed; use an approved image digest".into(),
        )),
    }
}

fn non_empty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(false) => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
        _ => true,
    }
}

fn approved_image(
    environment: &RepoEnvironment,
    cfg: &Config,
    selected: Option<&str>,
) -> Result<String, EnvironmentError> {
    match selected {
        // Nothing in the devcontainer: the repository's own toolchain image,
        // which is the harness image when the operator has named none. This is
        // the same resolution required checks use, so preparation validates the
        // environment those checks will actually get.
        None => Ok(environment.image.clone()),
        Some(image)
            if image.contains("@sha256:")
                || cfg
                    .runtime
                    .approved_images
                    .iter()
                    .any(|allowed| allowed == image) =>
        {
            Ok(image.to_string())
        }
        Some(_) => Err(EnvironmentError::UnapprovedImage),
    }
}

fn file_hash(path: &Path) -> Result<String, std::io::Error> {
    let mut input = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut input, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn tail(bytes: &[u8]) -> String {
    const MAX: usize = 4096;
    let start = bytes.len().saturating_sub(MAX);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Repo;

    /// An entry answers for what it names and nothing else: a repository with
    /// no entry, and an entry that leaves a field out, stay on the node-wide
    /// answer for it.
    #[test]
    fn a_repo_entry_overrides_only_what_it_names() {
        let image =
            "localhost/tc@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut cfg = Config::default();
        cfg.boundary.harness_image = "localhost/harness".into();
        cfg.supervision.checks = vec![" just check ".into(), String::new()];
        cfg.supervision.timeout_secs = 900;
        cfg.repo = vec![
            Repo {
                path: "/src/app".into(),
                image: Some(image.into()),
                checks: Some(vec!["just check".into(), "just test".into()]),
                timeout_secs: Some(1800),
                prepare: vec!["composer install".into()],
                egress: vec!["packagist".into()],
                ..Default::default()
            },
            Repo {
                path: "owner/notes".into(),
                checks: Some(Vec::new()),
                ..Default::default()
            },
        ];

        let store = Store::open_in_memory().unwrap();
        let app = environment_for(&cfg, &store, Some(Path::new("/src/app")));
        assert_eq!(app.image, image);
        assert_eq!(app.image_source, "repository toolchain");
        assert_eq!(app.checks, ["just check", "just test"]);
        assert_eq!(app.timeout_secs, 1800);
        assert_eq!(app.prepare, ["composer install"]);
        assert!(app.egress.contains(&"repo.packagist.org".to_string()));
        // Preparation's hosts are not a session's until the entry says so.
        assert!(app.session_egress.is_empty());
        cfg.repo[0].session_egress = true;
        let closed = session_grant(&cfg, &app, "s1");
        assert!(closed.hosts.is_empty());
        assert_eq!(closed.patterns, cfg.gateway.allow_hosts);
        cfg.repo[0].session_egress = true;
        let opened = environment_for(&cfg, &store, Some(Path::new("/src/app")));
        assert_eq!(opened.session_egress, ["packagist"]);
        // The session's grant: the registries, expanded to the hosts they
        // are, beside what every harness may reach, and no plain HTTP.
        let grant = session_grant(&cfg, &opened, "s1");
        assert_eq!(grant.hosts, opened.egress);
        assert_eq!(grant.patterns, cfg.gateway.allow_hosts);
        assert_eq!(grant.session_id.as_deref(), Some("s1"));
        assert_eq!(grant.refusal, SESSION_REFUSAL);
        assert!(!grant.plain_http);

        // No image, no preparation, and explicitly no checks.
        let notes = environment_for(
            &cfg,
            &store,
            Some(Path::new("/clones/github.com/owner/notes")),
        );
        assert_eq!(notes.image, "localhost/harness");
        assert_eq!(notes.image_source, "harness image");
        assert!(notes.checks.is_empty());
        assert_eq!(notes.timeout_secs, 900);
        assert!(notes.prepare.is_empty() && notes.egress.is_empty());

        for unmatched in [Some(Path::new("/src/other")), None] {
            let other = environment_for(&cfg, &store, unmatched);
            assert_eq!(other.image_source, "harness image");
            assert_eq!(other.checks, ["just check"]);
            assert!(other.prepare.is_empty());
        }

        // An entry that names a Dockerfile resolves to what the node built
        // from it, and says so while nothing has been built.
        cfg.repo.push(Repo {
            path: "/src/built".into(),
            dockerfile: Some(".devcontainer/Dockerfile".into()),
            ..Default::default()
        });
        let unbuilt = environment_for(&cfg, &store, Some(Path::new("/src/built")));
        assert_eq!(unbuilt.image, "localhost/harness");
        assert_eq!(
            unbuilt.image_source,
            "harness image (repository image not built)"
        );
        assert!(unbuilt.names_image() && !notes.names_image());
        let build = store
            .start_repo_image(&crate::store::RepoImageStart {
                repo_path: "/src/built",
                kind: crate::repo_image::BASE,
                recipe_hash: "r1",
                source_ref: "origin/main",
                source_commit: "c1",
                parent_image: "",
            })
            .unwrap();
        assert_eq!(
            environment_for(&cfg, &store, Some(Path::new("/src/built"))).image,
            "localhost/harness"
        );
        store.finish_repo_image(&build, Ok(image), "", &[]).unwrap();
        let built = environment_for(&cfg, &store, Some(Path::new("/src/built")));
        assert_eq!(built.image, image);
        assert_eq!(built.image_source, REPOSITORY_DOCKERFILE);

        // One cache per repository and image: two repositories never share
        // what their preparations fetched.
        assert_ne!(app.cache_volume, notes.cache_volume);
        assert_eq!(
            app.cache_volume,
            environment_for(&cfg, &store, Some(Path::new("/src/app"))).cache_volume
        );
    }
}
