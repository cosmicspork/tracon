//! Repository image builds against real Podman: an unbuilt recipe asked for
//! by several runs at once is built once, every run gets the one image,
//! another build of the repository leaves that image resolving under its own
//! tag, and a pinned image that is gone is refused at once instead of being
//! pulled from `https://localhost/v2/`.
//!
//! Runs only when asked for:
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 \
//!   TRACON_SANDBOX_BASE=<the stock base image, in the local store> \
//!   cargo test -p tracon --test image_builds
//! ```
//!
//! Everything the test makes is named with `TRACON_IMAGE_TEST_PREFIX`
//! (default `tracon-test-ib-`) and removed again.

#[path = "support/mod.rs"]
mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use tracon::config::{Config, Repo};
use tracon::provider::podman::PodmanProvider;
use tracon::repo_image::{ensure_base, Ensured};
use tracon::runner::podman::RunSpec;
use tracon::store::Store;

use support::podman::{podman, requested};

/// Removes what the test made, however it ends.
struct Cleanup {
    prefix: String,
    /// Images no name of the prefix's is on any more.
    ids: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for id in &self.ids {
            let _ = podman(&["image", "rm", "-f", id]);
        }
        if let Some(ids) = podman(&["ps", "-aq", "--filter", &format!("name=^{}", self.prefix)]) {
            for id in ids.lines() {
                let _ = podman(&["rm", "-f", id]);
            }
        }
        let reference = format!("reference=localhost/tracon-repo-{}*", self.prefix);
        if let Some(ids) = podman(&["images", "-q", "--filter", &reference]) {
            for id in ids.lines() {
                let _ = podman(&["image", "rm", "-f", id]);
            }
        }
    }
}

fn git(repo: &Path, script: &str) {
    let status = std::process::Command::new("sh")
        .args(["-c", script])
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "{script}");
}

/// `cat /built` in `image`, never pulled, as the node's sandboxes run a
/// local image (`RunSpec::sandbox_args`): its exit status, its output, and
/// how long it took.
fn run(_cfg: &Config, image: &str, name: &str) -> (bool, String, Duration) {
    let began = Instant::now();
    let out = std::process::Command::new("podman")
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            "--pull=never",
            "--name",
            name,
            image,
            "cat",
            "/built",
        ])
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), said, began.elapsed())
}

#[tokio::test]
async fn runs_asked_for_together_build_once_and_a_removed_image_is_never_pulled() {
    if !requested() {
        return;
    }
    support::state::isolate();
    let base = std::env::var("TRACON_SANDBOX_BASE").expect("TRACON_SANDBOX_BASE");
    assert!(podman(&["image", "exists", &base]).is_some(), "{base}");
    let prefix =
        std::env::var("TRACON_IMAGE_TEST_PREFIX").unwrap_or_else(|_| "tracon-test-ib-".into());
    let mut cleanup = Cleanup {
        prefix: prefix.clone(),
        ids: Vec::new(),
    };

    // An unbuilt repository on the stock base, named with the prefix so its
    // image is too.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join(format!("{prefix}app"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("Dockerfile"),
        format!("ARG TRACON_SANDBOX_BASE={base}\nFROM ${{TRACON_SANDBOX_BASE}}\nUSER 0:0\nRUN date +%s%N > /built\n"),
    )
    .unwrap();
    git(
        &repo,
        "git init -q -b main . && git add -A \
         && git -c user.email=t@e -c user.name=t -c commit.gpgsign=false commit -qm one \
         && git update-ref refs/remotes/origin/main HEAD \
         && git symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main",
    );
    let cfg = Config {
        repo: vec![Repo {
            path: repo.clone(),
            dockerfile: Some("Dockerfile".into()),
            ..Default::default()
        }],
        ..Config::default()
    };
    let backend = tracon::boundary::podman::PodmanBackend::detect(&cfg).await;
    let store = Store::open_in_memory().unwrap();
    let path = repo.to_string_lossy().into_owned();

    // Two runs' environments and a forced build, all at once.
    let (one, two, forced) = tokio::join!(
        tracon::repo_image::environment(&backend, &cfg, &store, Some(&repo)),
        tracon::repo_image::environment(&backend, &cfg, &store, Some(&repo)),
        async {
            while store.latest_repo_image(&path, "base").unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            ensure_base(&backend, &cfg, &store, &repo, true).await
        }
    );
    let (one, two) = (one.unwrap(), two.unwrap());
    let built: Vec<_> = store
        .repo_images()
        .unwrap()
        .into_iter()
        .filter(|row| row.kind == "base")
        .collect();
    assert_eq!(built.len(), 1, "built once: {built:?}");
    let image = built[0].image.clone();
    assert_eq!(one.image, image);
    assert_eq!(two.image, image);
    assert_eq!(forced, Ensured::Current(image.clone()));

    // Both start, in the image they were given.
    let mut built_at = String::new();
    for (n, _) in [&one, &two].iter().enumerate() {
        let (ok, said, _) = run(&cfg, &image, &format!("{prefix}run-{n}"));
        assert!(ok, "{said}");
        built_at = said.trim().to_string();
    }

    // Another builder tags the same repository, under the tag every build
    // of a recipe once shared and under one of its own: the pinned
    // reference still resolves through its build's own tag, and runs.
    cleanup
        .ids
        .push(podman(&["image", "inspect", "--format", "{{.Id}}", &image]).unwrap());
    let name = image.split_once('@').unwrap().0;
    let other = dir.path().join("Other.Containerfile");
    std::fs::write(
        &other,
        format!("FROM {base}\nUSER 0:0\nRUN echo other > /built\n"),
    )
    .unwrap();
    for tag in [
        format!("{name}:{}", &built[0].recipe_hash[..12]),
        format!("{name}:{}-other", &built[0].recipe_hash[..12]),
    ] {
        assert!(podman(&[
            "build",
            "-q",
            "-f",
            &other.to_string_lossy(),
            "-t",
            &tag,
            &dir.path().to_string_lossy(),
        ])
        .is_some());
    }
    assert!(podman(&["image", "exists", &image]).is_some());
    let (ok, said, _) = run(&cfg, &image, &format!("{prefix}run-after"));
    assert!(ok, "{said}");
    assert!(said.contains(&built_at), "the image it was given: {said}");

    // A cached rebuild leaves one image under two builds' tags; superseding
    // it removes it by its digest all the same.
    assert!(podman(&["tag", &image, &format!("{name}:cached-rebuild")]).is_some());
    use tracon::boundary::ImageBuilder;
    backend.remove(&image).await;
    assert!(podman(&["image", "exists", &image]).is_none());

    let (ok, said, took) = run(&cfg, &image, &format!("{prefix}run-gone"));
    assert!(!ok);
    assert!(said.contains("image not known"), "{said}");
    assert!(!said.contains("registry"), "never pulled: {said}");
    assert!(took < Duration::from_secs(10), "at once: {took:?}");

    let provider = PodmanProvider::from_config(&cfg, RunSpec::from_config(&cfg));
    let refused = provider.entry_for(&image).await;
    let Err(error) = refused else {
        panic!("a removed image is refused");
    };
    let error = error.to_string();
    assert!(error.contains("no longer in Podman's store"), "{error}");
}
