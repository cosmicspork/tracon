//! Claude Code layered onto a repository's image, built for real with Podman.
//!
//! The base stands in for a repository's: a `git` of its own on `PATH`, and
//! the Claude Code configuration a repository could slip under the node's:
//! managed settings in `/etc/claude-code`, and a `.claude` directory and
//! `.claude.json` in the harness's home. It also sets the three variables a
//! session always sets for itself, which are not refused. The session image
//! the node layers over it has to run the pinned Claude Code and carry none of
//! that configuration; an image that would change the harness is refused, and
//! the session's note says why.
//!
//! Runs only with `TRACON_PODMAN_TESTS=1` (`just check-podman`), against a
//! Claude Code harness image built from this tree's definitions and named in
//! `TRACON_CLAUDE_IMAGE`, on a network of its own: see `support::podman`.
//! Every image and network made here is removed again.

#[path = "support/mod.rs"]
mod support;

use std::process::Command;
use std::sync::Arc;

use support::podman::{podman, Network};
use tracon::adapter::claude::ClaudeAdapter;
use tracon::boundary::podman::PodmanBackend;
use tracon::boundary::Backend;
use tracon::config::{Config, Repo, RuntimeKind};
use tracon::store::Store;

/// The harness image's own base, in the store wherever a harness image was
/// built.
const DEBIAN: &str = "docker.io/library/debian@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171";

/// What the harness image installs; the probe holds the layer to it.
const PINNED: &str = ClaudeAdapter::PINNED_VERSION;

struct Rig {
    harness: String,
    network: Network,
}

fn rig() -> Option<Rig> {
    support::state::isolate();
    if !support::podman::requested() {
        return None;
    }
    assert!(
        podman(&["info", "--format", "{{.Host.Arch}}"]).is_some(),
        "podman is not usable on this machine"
    );
    let harness = support::podman::named_image("TRACON_CLAUDE_IMAGE");
    assert!(
        podman(&[
            "run",
            "--rm",
            "--network=none",
            "--entrypoint",
            "sh",
            &harness,
            "-c",
            &format!(
                "test -x /usr/bin/tini-static && {} --version | grep -q {PINNED}",
                ClaudeAdapter::BINARY
            ),
        ])
        .is_some(),
        "{harness} is not a Claude Code harness image at {PINNED} with a static init; build \
         containers/harness-claude from this tree"
    );
    assert!(
        podman(&["image", "exists", DEBIAN]).is_some(),
        "{DEBIAN} is not in the local store; building the harness image leaves it there"
    );
    Some(Rig {
        harness,
        network: Network::create(),
    })
}

/// The label every base here carries, and so every image built over one,
/// including what a refused build leaves untagged.
const LABEL: &str = "tracon-test-claude-layer";

/// Removes every image a test built, whatever it asserted.
struct Images(Vec<String>);

impl Drop for Images {
    fn drop(&mut self) {
        for image in &self.0 {
            let _ = Command::new("podman")
                .args(["image", "rm", "-f", image])
                .output();
        }
        let marker = format!("label={LABEL}={}", std::process::id());
        if let Some(left) = podman(&["images", "-a", "-q", "--filter", &marker]) {
            for id in left.lines().filter(|id| !id.is_empty()) {
                let _ = Command::new("podman")
                    .args(["image", "rm", "-f", id])
                    .output();
            }
        }
    }
}

/// A stand-in for a repository's image, built from `extra` over Debian, with
/// Claude Code configuration everywhere the harness could have read it.
fn base(images: &mut Images, name: &str, extra: &str) -> String {
    let tag = format!(
        "localhost/tracon-test-claude-base-{name}-{}",
        std::process::id()
    );
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Containerfile"),
        format!(
            "FROM {DEBIAN}\n\
             LABEL {LABEL}={pid}\n\
             ENV CLAUDE_CONFIG_DIR=/home/node/.claude ANTHROPIC_BASE_URL=http://example.invalid \
             ANTHROPIC_API_KEY=placeholder\n\
             RUN printf '#!/bin/sh\\necho git version 0.0.0-repository\\n' > /usr/local/bin/git \
             && chmod 0755 /usr/local/bin/git \
             && for home in /root /home/harness; do mkdir -p $home/.claude \
             && echo '{{\"permissions\":{{\"defaultMode\":\"bypassPermissions\"}}}}' > \
             $home/.claude/settings.json && echo '{{}}' > $home/.claude.json; done \
             && mkdir -p /etc/claude-code/managed-settings.d \
             && echo '{{\"permissions\":{{\"defaultMode\":\"bypassPermissions\"}}}}' > \
             /etc/claude-code/managed-settings.json\n\
             {extra}",
            pid = std::process::id()
        ),
    )
    .unwrap();
    let built = Command::new("podman")
        .args(["build", "-q", "-t", &tag])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    images.0.push(tag.clone());
    tag
}

/// Layer Claude Code onto `base` as a session would, for `kind`'s profile.
async fn layer(
    images: &mut Images,
    rig: &Rig,
    base: &str,
    kind: RuntimeKind,
) -> (tracon::repo_image::SessionImage, Arc<Store>, String) {
    let repo = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.harness.id = ClaudeAdapter::ID.into();
    cfg.boundary.harness_image = rig.harness.clone();
    cfg.boundary.network = rig.network.0.clone();
    cfg.runtime.kind = kind;
    cfg.repo = vec![Repo {
        path: repo.path().to_path_buf(),
        image: Some(base.into()),
        ..Default::default()
    }];
    let cfg = Arc::new(cfg);
    let backend: Arc<dyn Backend> = Arc::new(PodmanBackend::detect(&cfg).await);
    let store = Arc::new(Store::open_in_memory().unwrap());
    let got = tracon::repo_image::session_image(
        &backend,
        &cfg,
        &store,
        repo.path(),
        (ClaudeAdapter::ID, PINNED),
        &|| {},
    )
    .await;
    images.0.extend(got.image.clone());
    (got, store, repo.path().to_string_lossy().into_owned())
}

fn inside(image: &str, user: &str, script: &str) -> String {
    let out = Command::new("podman")
        .args([
            "run",
            "--rm",
            "--network=none",
            "--user",
            user,
            image,
            "sh",
            "-c",
            script,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What a session image has to be for Claude Code, as `user`, at `home`.
fn assert_session_image(image: &str, user: &str, home: &str) {
    let out = inside(
        image,
        user,
        &format!(
            "/usr/local/bin/claude --version; git --version; echo \"config=$CLAUDE_CONFIG_DIR\"; \
             test ! -e /etc/claude-code && echo no-managed-settings; \
             test ! -e {home}/.claude && test ! -e {home}/.claude.json && echo no-home-config"
        ),
    );
    assert!(out.contains(&format!("{PINNED} (Claude Code)")), "{out}");
    assert!(out.contains("git version 0.0.0-repository"), "{out}");
    assert!(
        out.contains(&format!("config={home}/.claude")),
        "the layer's CLAUDE_CONFIG_DIR wins: {out}"
    );
    assert!(out.contains("no-managed-settings"), "{out}");
    assert!(out.contains("no-home-config"), "{out}");
}

/// The Podman profile: Claude Code on the repository's image, with none of
/// the configuration the image held, and the three variables the session
/// sets for itself let through.
#[tokio::test]
async fn claude_runs_on_a_repository_image_without_its_configuration() {
    let Some(rig) = rig() else { return };
    let mut images = Images(Vec::new());
    let base = base(&mut images, "plain", "");
    let (got, _store, _repo) = layer(&mut images, &rig, &base, RuntimeKind::Podman).await;
    let image = got.image.clone().unwrap_or_else(|| panic!("{got:?}"));
    assert_session_image(&image, "0:0", "/root");
}

/// The pod profile, built here with Podman: the same, as the harness uid,
/// with its init.
#[tokio::test]
async fn claude_runs_on_a_pod_profile_image_as_the_harness_uid() {
    let Some(rig) = rig() else { return };
    let mut images = Images(Vec::new());
    let base = base(&mut images, "pod", "");
    let (got, _store, _repo) = layer(&mut images, &rig, &base, RuntimeKind::Kubernetes).await;
    let image = got.image.clone().unwrap_or_else(|| panic!("{got:?}"));
    let user = podman(&["image", "inspect", "--format", "{{.Config.User}}", &image]).unwrap();
    assert_eq!(user, "65532:65532");
    assert_session_image(&image, "65532:65532", "/home/harness");
    let out = inside(&image, "65532:65532", "/usr/libexec/tracon/tini --version");
    assert!(out.contains("tini version"), "{out}");
}

/// An image that would configure Claude Code, or change what it loads,
/// underneath the node is refused, and the session's note says which and
/// why. The session stays in the harness's own image.
#[tokio::test]
async fn an_image_that_would_change_claude_is_refused_by_name() {
    let Some(rig) = rig() else { return };
    let sets = |name: &str| {
        format!(
            "the repository's image sets {name}, which would reach the harness and change how \
             it is configured or what it loads"
        )
    };
    for (name, extra, note) in [
        (
            "bedrock",
            "ENV CLAUDE_CODE_USE_BEDROCK=1\n".to_string(),
            sets("CLAUDE_CODE_USE_BEDROCK"),
        ),
        (
            "model",
            "ENV ANTHROPIC_MODEL=claude-x\n".to_string(),
            sets("ANTHROPIC_MODEL"),
        ),
        (
            "node",
            "ENV NODE_OPTIONS=--max-old-space-size=4096\n".to_string(),
            sets("NODE_OPTIONS"),
        ),
        (
            "bun",
            "ENV BUN_OPTIONS=--preload=/work/x.js\n".to_string(),
            sets("BUN_OPTIONS"),
        ),
        (
            "loader",
            "ENV LD_PRELOAD=\n".to_string(),
            sets("LD_PRELOAD"),
        ),
        (
            "preload",
            "RUN echo /usr/lib/nothing-here.so > /etc/ld.so.preload\n".to_string(),
            "the repository's image preloads libraries through /etc/ld.so.preload, which would \
             load them into the harness"
                .to_string(),
        ),
        (
            "linked",
            "RUN mkdir -p /work && mv /usr/local/bin /work/b && ln -s /work/b /usr/local/bin\n"
                .to_string(),
            "the repository's image makes /usr/local/bin a symbolic link, which would let it \
             choose what the harness runs"
                .to_string(),
        ),
    ] {
        let mut images = Images(Vec::new());
        let base = base(&mut images, name, &extra);
        let (got, store, repo) = layer(&mut images, &rig, &base, RuntimeKind::Podman).await;
        assert_eq!(got.image, None, "{name}");
        assert_eq!(got.note.as_deref(), Some(note.as_str()), "{name}");
        let row = store
            .latest_repo_image(&repo, "session:claude")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "failed", "{name}");
    }
}
