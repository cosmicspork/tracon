//! The image a DigitalOcean template is built from, made by the node: one of
//! its own sandbox images (the pinned stock base, or a repository's prepared
//! snapshot) with the harness bundle the node verified into `<state>/bundles`
//! at `/opt/tracon`, each catalogue service bundle it holds at
//! `/opt/tracon-services/<name>` (nothing is mounted on DigitalOcean, so they
//! travel in the image, as the harness's binaries do), and the
//! `tracon-sandbox` service. Pushed to the operator's registry
//! (`[digitalocean] registry`), which the runtime pulls from when it
//! registers the template. One per local image digest and what goes into it:
//! the template is named from both (`super::templates`), so nothing is built
//! or pushed again for an image that already has one.
//!
//! A Podman node builds it locally and pushes it with the API token. A
//! Kubernetes node has no image store of its own: its BuildKit
//! (`[runtime.kubernetes.builder]`) builds it from the same context and
//! pushes it with the builder's credential, so `registry` there must be the
//! builder's ([`build_and_push_with`]).

use std::path::{Path, PathBuf};

use base64::Engine as _;

use super::bootstrap::RUN_SCRIPT;

/// The Containerfile the source is built from, as `containers/do-sandbox`
/// holds it.
pub const CONTAINERFILE: &str = include_str!("../../../../containers/do-sandbox/Containerfile");

/// The tag a local image's source is pushed under: the node's own name and
/// the first 16 hex characters of the image's digest.
pub fn tag_for(registry: &str, prefix: &str, from: &str) -> Result<String, String> {
    let digest = super::templates::digest_of(from)?;
    let hex = digest.trim_start_matches("sha256:");
    Ok(format!(
        "{}/{prefix}-do-sandbox:{}",
        registry.trim_end_matches('/'),
        &hex[..16]
    ))
}

/// Copy the bundle's files and directories, modes kept; anything else in
/// it is refused (the node verified it holds nothing else).
fn copy_bundle(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_bundle(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        } else {
            return Err(std::io::Error::other(format!(
                "{} is neither a file nor a directory",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

/// What the node adds to an image of its own.
#[derive(Debug, Clone, Copy)]
pub struct Contents<'a> {
    /// The harness bundle, for `/opt/tracon`.
    pub bundle: &'a Path,
    /// Each service bundle's directory here and where a sandbox has it
    /// (`/opt/tracon-services/<name>`).
    pub services: &'a [(PathBuf, String)],
}

/// Write the build context: the Containerfile, the service's `run`, and a
/// copy of each bundle (a copy rather than a mount, which an SELinux host
/// would keep the build from reading).
pub fn stage(context: &Path, contents: &Contents<'_>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(context)?;
    copy_bundle(contents.bundle, &context.join("bundle"))?;
    // Present when empty, so the Containerfile's COPY always has a source.
    let services = context.join("services");
    std::fs::create_dir_all(&services)?;
    for (dir, target) in contents.services {
        let name = super::templates::service_name(target).ok_or_else(|| {
            std::io::Error::other(format!(
                "{target} is not under {}",
                crate::config::SERVICE_BUNDLES_TARGET
            ))
        })?;
        copy_bundle(dir, &services.join(name))?;
    }
    std::fs::write(context.join("Containerfile"), CONTAINERFILE)?;
    let run = context.join("run");
    std::fs::write(&run, RUN_SCRIPT)?;
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

pub fn build_argv(context: &Path, from: &str, tag: &str) -> Vec<String> {
    vec![
        "build".into(),
        "--pull=never".into(),
        "--build-arg".into(),
        format!("BASE={from}"),
        "--tag".into(),
        tag.into(),
        context.display().to_string(),
    ]
}

pub fn push_argv(tag: &str, digestfile: &Path) -> Vec<String> {
    vec![
        "push".into(),
        "--digestfile".into(),
        digestfile.display().to_string(),
        tag.into(),
    ]
}

/// The registry's host, as an auth file keys it.
pub fn registry_host(registry: &str) -> &str {
    registry.split('/').next().unwrap_or(registry)
}

/// An auth file for DigitalOcean's registry, which takes the API token as
/// both user and password. Written for one push and removed with its
/// directory; never on an argv.
pub fn auth_json(registry: &str, token: &str) -> String {
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{token}:{token}"));
    serde_json::json!({ "auths": { registry_host(registry): { "auth": auth } } }).to_string()
}

async fn podman(what: &str, argv: &[String], auth: Option<&Path>) -> Result<(), String> {
    let mut command = tokio::process::Command::new(crate::boundary::podman::podman_bin());
    command.args(argv).stdin(std::process::Stdio::null());
    if let Some(auth) = auth {
        command.env("REGISTRY_AUTH_FILE", auth);
    }
    let out = command
        .output()
        .await
        .map_err(|e| format!("{what} the template source: {e}"))?;
    match out.status.success() {
        true => Ok(()),
        false => Err(format!(
            "{what} the template source: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Build `from` + the bundles + the service locally, as `tag`.
pub async fn build(from: &str, contents: &Contents<'_>, tag: &str) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let context: PathBuf = dir.path().join("context");
    stage(&context, contents).map_err(|e| format!("stage the template source: {e}"))?;
    podman("build", &build_argv(&context, from, tag), None).await
}

/// Build `from` + the bundles + the service, push it, and return it by
/// digest, `tag@sha256:…`.
pub async fn build_and_push(
    from: &str,
    contents: &Contents<'_>,
    registry: &str,
    prefix: &str,
    token: &str,
) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let tag = tag_for(registry, prefix, from)?;
    build(from, contents, &tag).await?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    let auth = dir.path().join("auth.json");
    std::fs::write(&auth, auth_json(registry, token)).map_err(|e| e.to_string())?;
    let digestfile = dir.path().join("digest");
    podman("push", &push_argv(&tag, &digestfile), Some(&auth)).await?;
    let digest = std::fs::read_to_string(&digestfile).map_err(|e| e.to_string())?;
    let digest = digest.trim();
    super::templates::digest_of(&format!("x@{digest}"))?;
    let repository = tag.rsplit_once(':').map(|(r, _)| r).unwrap_or(&tag);
    Ok(format!("{repository}@{digest}"))
}

/// The Containerfile with `from` as its base's default: a builder that takes
/// no build arguments (`ImageBuild` has none) builds on it all the same.
pub fn containerfile_from(from: &str) -> Result<String, String> {
    crate::boundary::registry::Reference::parse(from)
        .map_err(|e| format!("{from} is not an image reference: {e}"))?;
    let file = CONTAINERFILE.replacen("ARG BASE\n", &format!("ARG BASE={from}\n"), 1);
    match file != CONTAINERFILE {
        true => Ok(file),
        false => Err("containers/do-sandbox/Containerfile declares no ARG BASE".into()),
    }
}

/// Build `from` + the bundles + the service on `builder`, which pushes it
/// to its own registry, and return it by digest,
/// `<registry>/<prefix>-do-sandbox@sha256:…`. Refused unless `registry` is
/// that one: the builder would otherwise move the tag into its own, where
/// the runtime is not told to look.
pub async fn build_and_push_with(
    builder: &dyn crate::boundary::ImageBuilder,
    from: &str,
    contents: &Contents<'_>,
    registry: &str,
    prefix: &str,
) -> Result<String, String> {
    let tag = tag_for(registry, prefix, from)?;
    let pushes_to = builder
        .registry()
        .ok_or("this node's builder pushes to no registry")?;
    let repository = tag.rsplit_once(':').map(|(r, _)| r).unwrap_or(&tag);
    if !repository.starts_with(&format!("{}/", pushes_to.trim_end_matches('/'))) {
        return Err(format!(
            "digitalocean.registry {registry} is not the builder's registry {pushes_to}, \
             which is where it pushes"
        ));
    }
    let containerfile = containerfile_from(from)?;
    // On the builder's scratch (a pod's state volume), not a small /tmp.
    let scratch = builder.scratch_dir().unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    let dir = tempfile::tempdir_in(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    let context = dir.path().join("context");
    stage(&context, contents).map_err(|e| format!("stage the template source: {e}"))?;
    let file = context.join("Containerfile");
    std::fs::write(&file, containerfile).map_err(|e| format!("stage the template source: {e}"))?;
    let built = builder
        .build(crate::boundary::ImageBuild {
            tag: &tag,
            containerfile: &file,
            context: &context,
            labels: &[],
            timeout: crate::repo_image::BUILD_TIMEOUT,
        })
        .await
        .map_err(|failure| format!("build the template source: {}", failure.error))?;
    super::templates::digest_of(&built.image)?;
    match built.image.strip_prefix(repository) {
        Some(rest) if rest.starts_with('@') => Ok(built.image),
        _ => Err(format!(
            "the builder pushed {} rather than under {repository}",
            built.image
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: &str = "localhost/tracon-snapshot-app@sha256:aaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_source_is_tagged_for_its_image() {
        assert_eq!(
            tag_for("registry.digitalocean.com/acme/", "tracon", FROM).unwrap(),
            "registry.digitalocean.com/acme/tracon-do-sandbox:aaaaaaaaaaaaaaaa"
        );
        assert!(tag_for("r", "tracon", "localhost/x:latest").is_err());
    }

    #[test]
    fn the_build_stands_on_the_local_image_and_the_token_stays_off_argv() {
        let argv = build_argv(Path::new("/c"), FROM, "r/t:1");
        assert!(argv.contains(&format!("BASE={FROM}")));
        assert!(argv.contains(&"--pull=never".to_string()));
        let auth = auth_json("registry.digitalocean.com/acme", "dop_v1_secret");
        assert!(!argv.join(" ").contains("dop_v1_secret"));
        let v: serde_json::Value = serde_json::from_str(&auth).unwrap();
        assert!(v["auths"]["registry.digitalocean.com"]["auth"].is_string());
    }

    #[test]
    fn the_context_holds_the_containerfile_and_the_service() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("b");
        std::fs::create_dir_all(bundle.join("bin")).unwrap();
        std::fs::write(bundle.join("bin/tracon-sandbox"), "x").unwrap();
        let browser = dir.path().join("browser");
        std::fs::create_dir_all(&browser).unwrap();
        std::fs::write(browser.join("run"), "#!/bin/sh\n").unwrap();
        let context = dir.path().join("c");
        stage(
            &context,
            &Contents {
                bundle: &bundle,
                services: &[(browser, "/opt/tracon-services/browser".into())],
            },
        )
        .unwrap();
        let file = std::fs::read_to_string(context.join("Containerfile")).unwrap();
        assert!(file.contains("FROM ${BASE}"));
        assert!(file.contains("COPY bundle/ /opt/tracon/"));
        assert!(file.contains(&format!(
            "COPY services/ {}/",
            crate::config::SERVICE_BUNDLES_TARGET
        )));
        assert!(context.join("bundle/bin/tracon-sandbox").is_file());
        assert!(context.join("services/browser/run").is_file());
        // None held: the directory is there for the COPY all the same.
        let bare = dir.path().join("bare");
        stage(
            &bare,
            &Contents {
                bundle: &bundle,
                services: &[],
            },
        )
        .unwrap();
        assert!(bare.join("services").is_dir());
        assert!(stage(
            &dir.path().join("bad"),
            &Contents {
                bundle: &bundle,
                services: &[(dir.path().join("browser"), "/elsewhere/browser".into())],
            },
        )
        .is_err());
        assert_eq!(
            std::fs::read_to_string(context.join("run")).unwrap(),
            RUN_SCRIPT
        );
    }

    /// A builder that records what it was given, as BuildKit would see it,
    /// and says it pushed under the tag's name.
    struct Recording {
        registry: String,
        scratch: PathBuf,
        seen: std::sync::Mutex<Option<Seen>>,
    }

    #[derive(Debug, Clone)]
    struct Seen {
        tag: String,
        containerfile: String,
        context: PathBuf,
        files: Vec<String>,
    }

    const PUSHED: &str = "sha256:5555555555555555666666666666666666666666666666666666666666666666";

    #[async_trait::async_trait]
    impl crate::boundary::ImageBuilder for Recording {
        async fn build(
            &self,
            build: crate::boundary::ImageBuild<'_>,
        ) -> Result<crate::boundary::BuiltImage, crate::boundary::BuildFailure> {
            let mut files = files_under(build.context);
            files.sort();
            *self.seen.lock().unwrap() = Some(Seen {
                tag: build.tag.to_string(),
                containerfile: std::fs::read_to_string(build.containerfile).unwrap(),
                context: build.context.to_path_buf(),
                files,
            });
            let name = build.tag.rsplit_once(':').unwrap().0;
            Ok(crate::boundary::BuiltImage {
                image: format!("{name}@{PUSHED}"),
                log_tail: String::new(),
            })
        }
        async fn exists(&self, _image: &str) -> bool {
            false
        }
        async fn identity(&self, _image: &str) -> Option<String> {
            None
        }
        async fn remove(&self, _image: &str) {}
        fn scratch_dir(&self) -> Option<PathBuf> {
            Some(self.scratch.clone())
        }
        fn registry(&self) -> Option<String> {
            Some(self.registry.clone())
        }
    }

    fn files_under(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                match path.is_dir() {
                    true => pending.push(path),
                    false => out.push(
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    ),
                }
            }
        }
        out
    }

    /// A Kubernetes node's BuildKit gets the context Podman would, staged on
    /// its scratch, with the base as the Containerfile's default since it
    /// takes no build arguments; the image comes back by the digest it
    /// pushed. Another registry than the builder's is refused unbuilt.
    #[tokio::test]
    async fn a_builder_of_the_nodes_builds_and_pushes_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("b");
        std::fs::create_dir_all(bundle.join("bin")).unwrap();
        std::fs::write(bundle.join("bin/tracon-sandbox"), "x").unwrap();
        let browser = dir.path().join("browser");
        std::fs::create_dir_all(&browser).unwrap();
        std::fs::write(browser.join("run"), "#!/bin/sh\n").unwrap();
        let builder = Recording {
            registry: "registry.digitalocean.com/acme".into(),
            scratch: dir.path().join("state/tmp"),
            seen: Default::default(),
        };
        let services = [(browser, "/opt/tracon-services/browser".to_string())];
        let contents = Contents {
            bundle: &bundle,
            services: &services,
        };
        let pushed = build_and_push_with(
            &builder,
            FROM,
            &contents,
            "registry.digitalocean.com/acme/",
            "tracon",
        )
        .await
        .unwrap();
        assert_eq!(
            pushed,
            format!("registry.digitalocean.com/acme/tracon-do-sandbox@{PUSHED}")
        );
        let seen = builder.seen.lock().unwrap().clone().unwrap();
        assert_eq!(
            seen.tag,
            "registry.digitalocean.com/acme/tracon-do-sandbox:aaaaaaaaaaaaaaaa"
        );
        assert!(seen.containerfile.contains(&format!("ARG BASE={FROM}\n")));
        assert!(seen.containerfile.contains("FROM ${BASE}"));
        assert!(seen.context.starts_with(dir.path().join("state/tmp")));
        for file in ["bundle/bin/tracon-sandbox", "services/browser/run", "run"] {
            assert!(seen.files.contains(&file.to_string()), "{:?}", seen.files);
        }
        // The staging went with the build.
        assert!(!seen.context.exists());

        *builder.seen.lock().unwrap() = None;
        let refused = build_and_push_with(
            &builder,
            FROM,
            &contents,
            "registry.digitalocean.com/other",
            "tracon",
        )
        .await
        .unwrap_err();
        assert!(
            refused.contains("is not the builder's registry"),
            "{refused}"
        );
        assert!(builder.seen.lock().unwrap().is_none());
    }

    /// A template's name hashes the Containerfile as it is kept: the base's
    /// default is only ever written into a builder's copy.
    #[test]
    fn the_base_is_the_containerfiles_default_for_a_builder() {
        let file = containerfile_from(FROM).unwrap();
        assert_eq!(file.matches("ARG BASE=").count(), 1);
        assert!(CONTAINERFILE.contains("ARG BASE\n"));
        assert!(containerfile_from("not a reference\nRUN x").is_err());
    }
}
