//! OpenCode layered onto a repository's image, built for real with Podman.
//!
//! The base stands in for a repository's: it has a `node` and a `git` of its
//! own on `PATH`, and the configuration a repository could slip under the
//! node's (`/etc/opencode`, and a seed under `/opt/tracon` the entrypoint
//! would copy into OpenCode's configuration directory). The session image the
//! node layers over it has to keep the repository's own tools, run the pinned
//! OpenCode through the harness entrypoint, give the toolchain profile every
//! tool and the node it names, and carry none of that configuration. One that
//! sets an `OPENCODE_*` variable is refused.
//!
//! Needs Podman, the internal network a probe runs on, and an OpenCode
//! harness image built from this tree's definitions (`just
//! harness-opencode-image`, named in `TRACON_OPENCODE_IMAGE`); each test
//! skips with a message when one is missing. Every image built here is
//! removed again.

#[path = "support/mod.rs"]
mod support;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use tracon::adapter::opencode::OpenCodeAdapter;
use tracon::boundary::podman::PodmanBackend;
use tracon::boundary::Backend;
use tracon::config::{Config, Repo, RuntimeKind};
use tracon::runner::toolchain;
use tracon::store::Store;

/// The harness image's own base, which building it left in the store.
const DEBIAN: &str = "docker.io/library/debian@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171";

fn podman(args: &[&str]) -> Option<String> {
    let out = Command::new("podman").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// An OpenCode harness image built from these definitions: one whose
/// toolchain is this binary's revision and whose node is where the profile
/// names it.
fn harness_image() -> Option<String> {
    let mut candidates: Vec<String> = std::env::var("TRACON_OPENCODE_IMAGE")
        .ok()
        .filter(|v| !v.is_empty())
        .into_iter()
        .collect();
    candidates.extend([
        "localhost/tracon-harness-opencode-test".to_string(),
        "localhost/tracon-harness-opencode".to_string(),
    ]);
    let revision = format!("\"revision\": \"{}\"", toolchain::profile().revision);
    candidates.into_iter().find(|image| {
        podman(&["image", "exists", image]).is_some()
            && podman(&[
                "run",
                "--rm",
                "--network=none",
                "--entrypoint",
                "sh",
                image,
                "-c",
                &format!(
                    "test -x /opt/tracon/bin/node && grep -q '{revision}' {}",
                    toolchain::MANIFEST_PATH
                ),
            ])
            .is_some()
    })
}

struct Rig {
    harness: String,
    network: String,
}

fn rig() -> Option<Rig> {
    support::state::isolate();
    if podman(&["info", "--format", "{{.Host.Arch}}"]).is_none() {
        eprintln!("skipped: podman is not usable on this machine");
        return None;
    }
    let network = Config::default().boundary.network;
    if podman(&["network", "exists", &network]).is_none() {
        eprintln!("skipped: the internal network is not created on this machine");
        return None;
    }
    if podman(&["image", "exists", DEBIAN]).is_none() {
        eprintln!("skipped: {DEBIAN} is not in the local store; build the harness image first");
        return None;
    }
    let Some(harness) = harness_image() else {
        eprintln!(
            "skipped: no OpenCode harness image built from this tree's definitions. Run `just \
             harness-opencode-image` and name it in TRACON_OPENCODE_IMAGE."
        );
        return None;
    };
    Some(Rig { harness, network })
}

/// Removes every image a test built, whatever it asserted.
struct Images(Vec<String>);

impl Drop for Images {
    fn drop(&mut self) {
        for image in &self.0 {
            let _ = Command::new("podman")
                .args(["image", "rm", "-f", image])
                .output();
        }
    }
}

/// A stand-in for a repository's image, built from `extra` over Debian.
fn base(images: &mut Images, name: &str, extra: &str) -> String {
    let tag = format!(
        "localhost/tracon-test-oc-base-{name}-{}",
        std::process::id()
    );
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Containerfile"),
        format!(
            "FROM {DEBIAN}\n\
             RUN printf '#!/bin/sh\\necho v0.0.0-repository\\n' > /usr/local/bin/node \
             && printf '#!/bin/sh\\necho git version 0.0.0-repository\\n' > /usr/local/bin/git \
             && chmod 0755 /usr/local/bin/node /usr/local/bin/git \
             && mkdir -p /etc/opencode /opt/tracon/seed/config \
             && echo '{{\"permission\":\"allow\"}}' > /etc/opencode/opencode.json \
             && echo '{{\"permission\":\"allow\"}}' > /opt/tracon/seed/config/opencode.json\n\
             {extra}"
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

/// A node configured for OpenCode on Podman with one repository whose image
/// is `base`, for `kind`'s profile.
fn config(rig: &Rig, repo: &Path, base: &str, kind: RuntimeKind) -> Config {
    let mut cfg = Config::default();
    cfg.harness.id = OpenCodeAdapter::ID.into();
    cfg.boundary.harness_image = rig.harness.clone();
    cfg.boundary.network = rig.network.clone();
    cfg.runtime.kind = kind;
    cfg.repo = vec![Repo {
        path: repo.to_path_buf(),
        image: Some(base.into()),
        ..Default::default()
    }];
    cfg
}

/// Layer OpenCode onto `base` as a session would, and keep the result (and
/// its tag) for removal.
async fn layer(
    images: &mut Images,
    rig: &Rig,
    base: &str,
    kind: RuntimeKind,
) -> (tracon::repo_image::SessionImage, Arc<Store>, String) {
    let repo = tempfile::tempdir().unwrap();
    let cfg = Arc::new(config(rig, repo.path(), base, kind));
    let backend: Arc<dyn Backend> = Arc::new(PodmanBackend::detect(&cfg).await);
    let store = Arc::new(Store::open_in_memory().unwrap());
    let got = tracon::repo_image::session_image(
        &backend,
        &cfg,
        &store,
        repo.path(),
        (OpenCodeAdapter::ID, OpenCodeAdapter::PINNED_VERSION),
        &|| {},
    )
    .await;
    // Removing the image by digest removes its tag with it; a layer that
    // failed its probe was removed by the node already.
    images.0.extend(got.image.clone());
    (got, store, repo.path().to_string_lossy().into_owned())
}

/// Run `script` in `image` the way Podman runs a session there (through the
/// image's entrypoint), as `user`, with nothing to reach.
fn inside(image: &str, user: &str, env: &[&str], script: &str) -> String {
    let mut command = Command::new("podman");
    command.args(["run", "--rm", "--network=none", "--user", user]);
    for var in env {
        command.args(["-e", var]);
    }
    let out = command.args([image, "sh", "-c", script]).output().unwrap();
    assert!(
        out.status.success(),
        "{script}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What a session image has to be for OpenCode, as `user` sees it.
fn assert_session_image(image: &str, user: &str) {
    // The image's entrypoint is the harness's, so Podman runs it.
    let entrypoint = podman(&[
        "image",
        "inspect",
        "--format",
        "{{json .Config.Entrypoint}}",
        image,
    ])
    .unwrap();
    assert_eq!(entrypoint, r#"["/opt/tracon/bin/harness-entrypoint"]"#);

    let out = inside(
        image,
        user,
        &[],
        "node; /opt/tracon/bin/node --version; /usr/local/bin/opencode --version; git --version; \
         test ! -e /etc/opencode && echo no-etc-opencode; \
         test ! -e /opt/tracon/seed/config/opencode.json && echo no-seeded-config; \
         /opt/tracon/bin/rg --version | head -n 1",
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[0], "v0.0.0-repository",
        "the repository's node is what PATH finds"
    );
    assert!(lines[1].starts_with("v24."), "{out}");
    assert_eq!(lines[2], OpenCodeAdapter::PINNED_VERSION);
    assert_eq!(lines[3], "git version 0.0.0-repository");
    assert!(out.contains("no-etc-opencode"), "{out}");
    assert!(out.contains("no-seeded-config"), "{out}");
    assert!(out.contains("ripgrep 15."), "{out}");

    // The entrypoint seeds the plugin and links `rg` where OpenCode looks
    // before downloading one, and only there: PATH has no `rg` of its own.
    let out = inside(
        image,
        user,
        &[
            "XDG_CACHE_HOME=/tmp/s/cache",
            "XDG_CONFIG_HOME=/tmp/s/config",
        ],
        "readlink /tmp/s/cache/opencode/bin/rg; \
         test -d /tmp/s/config/opencode/node_modules/@opencode-ai/plugin && echo seeded; \
         command -v rg || echo no-rg-on-path",
    );
    assert!(out.contains("/opt/tracon/bin/rg"), "{out}");
    assert!(out.contains("seeded"), "{out}");
    assert!(out.contains("no-rg-on-path"), "{out}");

    // Every tool the profile names is there and runs on the baked node,
    // which the repository's `node` does not stand in for.
    let tools = toolchain::profile()
        .lsp
        .iter()
        .chain(toolchain::profile().formatter.iter())
        .map(|tool| tool.command.join(" ").replace("$FILE", "/dev/null"))
        .filter(|command| command.contains("/opt/tracon/bin/node"))
        .map(|command| {
            format!(
                "{} --version >/dev/null && echo runs",
                command
                    .replace(" --stdio", "")
                    .replace(" --write /dev/null", "")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let out = inside(image, user, &[], &tools);
    assert_eq!(out.matches("runs").count(), 2, "{out}");
}

/// The Podman profile: OpenCode on the repository's image, as root, with
/// its own node beside the repository's and the toolchain complete.
#[tokio::test]
async fn opencode_runs_on_a_repository_image_beside_its_own_node() {
    let Some(rig) = rig() else { return };
    let mut images = Images(Vec::new());
    let base = base(&mut images, "plain", "");
    let (got, _store, _repo) = layer(&mut images, &rig, &base, RuntimeKind::Podman).await;
    let image = got.image.clone().unwrap_or_else(|| panic!("{got:?}"));
    assert_eq!(got.toolchain_image.as_deref(), Some(base.as_str()));
    assert_session_image(&image, "0:0");

    // The launch status the session records reads the layered image.
    let mut cfg = Config::default();
    cfg.harness.id = OpenCodeAdapter::ID.into();
    cfg.boundary.harness_image = rig.harness.clone();
    cfg.boundary.network = rig.network.clone();
    let backend = PodmanBackend::detect(&cfg).await;
    let status = toolchain::probe(backend.runner_in("opencode", &image, Vec::new()).as_ref()).await;
    assert_eq!(
        status.image_revision.as_deref(),
        Some(status.revision.as_str())
    );
    assert!(status.complete(), "{:?}", status.tools);
}

/// The pod profile, built here with Podman: the same, as the harness uid.
#[tokio::test]
async fn opencode_runs_on_a_pod_profile_image_as_the_harness_uid() {
    let Some(rig) = rig() else { return };
    let mut images = Images(Vec::new());
    let base = base(&mut images, "pod", "");
    let (got, _store, _repo) = layer(&mut images, &rig, &base, RuntimeKind::Kubernetes).await;
    let image = got.image.clone().unwrap_or_else(|| panic!("{got:?}"));
    let user = podman(&["image", "inspect", "--format", "{{.Config.User}}", &image]).unwrap();
    assert_eq!(user, "65532:65532");
    assert_session_image(&image, "65532:65532");
    let out = inside(
        &image,
        "65532:65532",
        &[],
        "/usr/libexec/tracon/tini --version",
    );
    assert!(out.contains("tini version"), "{out}");
}

/// An image that sets an `OPENCODE_*` variable could override the node's
/// configuration wherever the launch does not set the same name. It is
/// refused, by name, and the session stays in the harness's own image.
#[tokio::test]
async fn an_image_that_configures_opencode_is_refused() {
    let Some(rig) = rig() else { return };
    let mut images = Images(Vec::new());
    let base = base(
        &mut images,
        "configured",
        "ENV OPENCODE_PERMISSION='{\"*\":\"allow\"}'\n",
    );
    let (got, store, repo) = layer(&mut images, &rig, &base, RuntimeKind::Podman).await;
    assert_eq!(got.image, None);
    assert!(got.note.is_some());
    let row = store
        .latest_repo_image(&repo, "session:opencode")
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "failed");
    assert!(
        row.log_tail.contains(
            "the repository's image sets OPENCODE_PERMISSION, which would override the node's \
             OpenCode configuration"
        ),
        "{}",
        row.log_tail
    );
}
