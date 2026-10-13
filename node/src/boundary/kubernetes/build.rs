//! Building a repository's image on a cluster: `buildctl` in the node's pod,
//! over mutual TLS to a BuildKit daemon of the deployment's
//! (`[runtime.kubernetes.builder]`), pushing to a registry the harness pods
//! pull from.
//!
//! The context and the Dockerfile are sent from the node (`--local`), where
//! `repo_image` materialised them from the default branch, so the daemon
//! reads nothing of the node's but what it is given. The registry credential
//! is read from `DOCKER_CONFIG` in the node's pod and is not mounted in the
//! daemon, but it is not kept from it: BuildKit's session auth answers the
//! daemon's credential requests during a build with that username and
//! secret, for the `FROM` pulls and the push alike. A daemon that runs a
//! hostile Dockerfile can therefore obtain the push credential, which is why
//! it should be scoped to the registry and nothing else (a DigitalOcean
//! `doctl registry docker-config --read-write` credential is). The image is
//! reported by the digest the daemon pushed (`--metadata-file`), never by a
//! tag a later push could move.
//!
//! Images are named as Podman names them, `localhost/tracon-repo-…`, and that
//! prefix is the configured registry here: a build under one pushes to the
//! registry, and `exists`, `identity` and `remove` take either spelling.
//!
//! What a build stages (the context, the Containerfile, `buildctl`'s own
//! `TMPDIR`) goes under `<state_mount>/tmp` on the state volume, not the
//! node container's `/tmp`: a repository's tree can be larger than a pod's
//! ephemeral storage. A harness pod's `TMPDIR` is its own emptyDir instead
//! (`runner::kube::SCRATCH_MOUNT`); this one is the node's.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;

use crate::boundary::registry::{Reference, Registry};
use crate::boundary::{BuildFailure, BuiltImage, ImageBuild, ImageBuilder};
use crate::config::KubeBuilder;

const LOCAL: &str = "localhost/";

/// The directory under the state mount a build stages in.
pub const SCRATCH: &str = "tmp";

/// Why no repository image can be built, in the words both the `runtime`
/// check and a build that is attempted anyway report.
pub fn unusable(why: &str) -> String {
    format!("{why}, so a repository's image cannot be built")
}

/// How long `buildctl debug workers` may take to answer the check.
const WORKERS_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the whole check may take: it runs in every boundary check and
/// before every build.
const CHECK_BOUND: Duration = Duration::from_secs(15);

/// What this node pushes, by name under the registry: only these are ever
/// removed from it.
const OWN: &str = "tracon-repo-";

/// `90 seconds`, `45 minutes`: a deadline as the operator set it.
fn duration_words(duration: Duration) -> String {
    match duration.as_secs() {
        1 => "1 second".into(),
        secs if secs < 60 || secs % 60 != 0 => format!("{secs} seconds"),
        secs => format!("{} minutes", secs / 60),
    }
}

pub struct BuildKitBuilder {
    settings: KubeBuilder,
    /// `registry` without a trailing slash.
    prefix: String,
    registry: Registry,
    /// Where builds stage, and `buildctl`'s `TMPDIR`.
    scratch: PathBuf,
}

/// A `[runtime.kubernetes.builder]` that cannot be a builder. Handed out in
/// its place, so a repository whose image would be built hears why it was
/// not, rather than that the runtime cannot build at all.
pub struct Unusable(pub String);

#[async_trait]
impl ImageBuilder for Unusable {
    async fn build(&self, _build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
        Err(BuildFailure::transient(unusable(&self.0)))
    }
    /// Said before anything is staged, so a repository's tree is never
    /// materialised for a build that cannot happen.
    async fn unavailable(&self) -> Option<String> {
        Some(unusable(&self.0))
    }
    async fn exists(&self, _image: &str) -> bool {
        false
    }
    async fn identity(&self, _image: &str) -> Option<String> {
        None
    }
    async fn remove(&self, _image: &str) {}
}

/// Why `[runtime.kubernetes.builder]` cannot describe a builder, if it
/// cannot, without touching anything it names; else the registry prefix
/// images are pushed under. An edit of the table is held to this before it
/// is written (`config::edit`).
pub fn validate_settings(settings: &KubeBuilder) -> Result<String, String> {
    let addr = settings.addr.trim();
    if !addr.starts_with("tcp://") {
        return Err(format!(
            "addr must be tcp://host:port, the daemon's mTLS listener (got {addr:?})"
        ));
    }
    if settings.tls_dir.as_os_str().is_empty() {
        return Err("tls_dir must name the client certificate's directory".into());
    }
    if settings.docker_config.as_os_str().is_empty() {
        return Err("docker_config must name a directory holding config.json".into());
    }
    let prefix = settings.registry.trim().trim_end_matches('/').to_string();
    let namespaced = prefix
        .split_once('/')
        .is_some_and(|(_, namespace)| !namespace.is_empty());
    let probe = Reference::parse(&format!("{prefix}/probe:t")).ok();
    let qualified = probe.as_ref().is_some_and(|probe| {
        prefix.starts_with(&format!("{}/", probe.host)) || prefix == probe.host
    });
    if prefix.is_empty()
        || prefix.contains("://")
        || prefix.starts_with(LOCAL)
        || !qualified
        || !namespaced
        || !prefix
            .split('/')
            .next()
            .is_some_and(|host| host.contains(['.', ':']))
    {
        return Err(format!(
                "registry must be host/namespace, such as registry.digitalocean.com/<registry> (got {:?})",
                settings.registry
            ));
    }
    Ok(prefix)
}

impl BuildKitBuilder {
    /// The builder `[runtime.kubernetes.builder]` describes, staging under
    /// `scratch`, or why it cannot be one.
    pub fn new(settings: &KubeBuilder, scratch: PathBuf) -> Result<Self, String> {
        let prefix = validate_settings(settings)?;
        let registry = Registry::new(Some(settings.docker_config.clone()))
            .with_digitalocean_token(settings.do_token_file.clone());
        Ok(Self {
            settings: settings.clone(),
            prefix,
            registry,
            scratch,
        })
    }

    #[cfg(test)]
    fn new_for_test(settings: &KubeBuilder) -> Result<Self, String> {
        Self::new(
            settings,
            std::env::temp_dir().join("tracon-build-test-scratch"),
        )
    }

    #[doc(hidden)]
    pub fn with_registry(mut self, registry: Registry) -> Self {
        self.registry = registry;
        self
    }

    /// Where a build tagged `tag` is pushed: the configured registry, with a
    /// `localhost/` name moved into it.
    pub fn pushed_name(&self, tag: &str) -> String {
        if tag.starts_with(&format!("{}/", self.prefix)) {
            return tag.to_string();
        }
        format!("{}/{}", self.prefix, tag.strip_prefix(LOCAL).unwrap_or(tag))
    }

    /// The image a `localhost/` name refers to here; any other name is a
    /// registry's own already.
    pub fn resolve(&self, image: &str) -> String {
        match image.strip_prefix(LOCAL) {
            Some(rest) => format!("{}/{rest}", self.prefix),
            None => image.to_string(),
        }
    }

    fn connection(&self) -> Vec<OsString> {
        vec![
            "--addr".into(),
            self.settings.addr.trim().into(),
            "--tlsdir".into(),
            self.settings.tls_dir.clone().into(),
        ]
    }

    /// `buildctl … build` for one image.
    pub fn argv(&self, build: &ImageBuild<'_>, target: &str, metadata: &Path) -> Vec<OsString> {
        let directory = build
            .containerfile
            .parent()
            .unwrap_or(Path::new("."))
            .as_os_str();
        let filename = build
            .containerfile
            .file_name()
            .unwrap_or_default()
            .to_os_string();
        let mut argv = self.connection();
        argv.extend(["build".into(), "--frontend".into(), "dockerfile.v0".into()]);
        let mut local = |name: &str, path: &std::ffi::OsStr| {
            let mut value = OsString::from(format!("{name}="));
            value.push(path);
            argv.extend(["--local".into(), value]);
        };
        local("context", build.context.as_os_str());
        local("dockerfile", directory);
        let mut filename_opt = OsString::from("filename=");
        filename_opt.push(filename);
        argv.extend(["--opt".into(), filename_opt]);
        for (name, value) in build.labels {
            argv.extend(["--opt".into(), format!("label:{name}={value}").into()]);
        }
        argv.extend([
            "--output".into(),
            format!("type=image,name={target},push=true").into(),
            "--metadata-file".into(),
            metadata.as_os_str().to_os_string(),
            "--progress".into(),
            "plain".into(),
        ]);
        argv
    }

    fn command(&self, args: &[OsString]) -> tokio::process::Command {
        // Made here, not once at startup: the claim is mounted by then, and
        // something may have removed the directory since.
        let _ = std::fs::create_dir_all(&self.scratch);
        let mut command = tokio::process::Command::new(&self.settings.buildctl);
        command
            .args(args)
            .env("DOCKER_CONFIG", &self.settings.docker_config)
            .env("TMPDIR", &self.scratch)
            .stdin(std::process::Stdio::null())
            // A build abandoned at its deadline must not keep running: the
            // daemon cancels it when this client's session goes.
            .kill_on_drop(true);
        command
    }

    /// Whether the daemon answers over mTLS and the registry lets the
    /// credential push, in a sentence for the boundary report. Bounded as a
    /// whole: it runs in every boundary check and before every build.
    pub async fn check(&self) -> Result<String, String> {
        tokio::time::timeout(CHECK_BOUND, self.probe())
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "the builder check did not finish in {}s",
                    CHECK_BOUND.as_secs()
                ))
            })
    }

    async fn probe(&self) -> Result<String, String> {
        let mut args = self.connection();
        args.extend(["debug".into(), "workers".into()]);
        let out = tokio::time::timeout(WORKERS_TIMEOUT, self.command(&args).output())
            .await
            .map_err(|_| {
                format!(
                    "buildkitd at {} did not answer in {}s",
                    self.settings.addr,
                    WORKERS_TIMEOUT.as_secs()
                )
            })?
            .map_err(|error| format!("{}: {error}", self.settings.buildctl))?;
        if !out.status.success() {
            let said = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "buildkitd at {} refused `buildctl debug workers`: {}",
                self.settings.addr,
                said.trim().lines().last().unwrap_or("no output")
            ));
        }
        let workers = String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1)
            .filter(|line| !line.trim().is_empty())
            .count();
        if workers == 0 {
            return Err(format!(
                "buildkitd at {} has no workers",
                self.settings.addr
            ));
        }
        let confirmed = self
            .registry
            .check_push(&self.prefix)
            .await
            .map_err(|error| format!("registry {}: {error}", self.prefix))?;
        Ok(format!(
            "buildkitd at {} answers with {workers} worker(s) and {} {}",
            self.settings.addr,
            self.prefix,
            match confirmed {
                true => "grants the credential push",
                false => "accepts the credential (push not confirmed: the token does not say)",
            }
        ))
    }
}

/// The digest of what a build pushed, from `buildctl --metadata-file`.
pub fn pushed_digest(metadata: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(metadata).map_err(|error| format!("build metadata: {error}"))?;
    let digest = value["containerimage.digest"]
        .as_str()
        .ok_or("build metadata names no containerimage.digest")?;
    crate::config::immutable_image(&format!("x@{digest}"))
        .map_err(|_| format!("build metadata names no sha256 digest: {digest}"))?;
    Ok(digest.to_string())
}

/// What a failed build printed, when it failed fetching an image or a token
/// from a registry for a reason of the registry's or the network's (a rate
/// limit, a server error, a name that did not resolve, a connection the
/// egress policy dropped) rather than the recipe's. `None` for a step of the
/// Dockerfile that failed (whatever its own output says), and for an image
/// that is not there or a credential that is refused, which are the recipe's
/// or the operator's to fix and are not retried on their own.
pub fn fetch_failure(log: &str) -> Option<String> {
    const PASSING: &[&str] = &[
        "429",
        "too many requests",
        "500 internal server error",
        "502 bad gateway",
        "503 service unavailable",
        "504 gateway timeout",
        "dial tcp",
        "no such host",
        "i/o timeout",
        "tls handshake timeout",
        "connection reset",
        "connection refused",
        "context deadline exceeded",
        "deadlineexceeded",
        "unexpected eof",
    ];
    log.lines()
        .rev()
        .map(str::trim)
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("failed to solve")
                && !lower.contains("did not complete successfully")
                && PASSING.iter().any(|marker| lower.contains(marker))
        })
        .map(|line| line.chars().take(400).collect())
}

/// Without a tag: what a digest is appended to.
fn untagged(name: &str) -> &str {
    match name.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => name,
        _ => name,
    }
}

#[async_trait]
impl ImageBuilder for BuildKitBuilder {
    async fn build(&self, build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
        let failure = |error: String, log: &[u8]| BuildFailure {
            error,
            log_tail: crate::boundary::podman::log_tail(log),
            ..Default::default()
        };
        let target = self.pushed_name(build.tag);
        let stage = std::fs::create_dir_all(&self.scratch)
            .and_then(|()| tempfile::tempdir_in(&self.scratch))
            .map_err(|error| {
                BuildFailure::transient(unusable(&format!("{}: {error}", self.scratch.display())))
            })?;
        let metadata = stage.path().join("metadata.json");
        let argv = self.argv(&build, &target, &metadata);
        let timeout = self
            .settings
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(build.timeout);
        let out = match tokio::time::timeout(timeout, self.command(&argv).output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(error)) => {
                return Err(BuildFailure::transient(unusable(&format!(
                    "{}: {error}",
                    self.settings.buildctl
                ))))
            }
            Err(_) => {
                return Err(failure(
                    format!("the build did not finish in {}", duration_words(timeout)),
                    &[],
                ))
            }
        };
        let log = [out.stdout, out.stderr].concat();
        if !out.status.success() {
            // The daemon or the registry, rather than the Dockerfile, is
            // reported as the check reports it: one reason in both places.
            return Err(match self.check().await {
                Err(why) => BuildFailure {
                    transient: true,
                    ..failure(unusable(&why), &log)
                },
                Ok(_) => match fetch_failure(&String::from_utf8_lossy(&log)) {
                    // A registry that was busy or could not be reached
                    // said nothing about the recipe: tried again after
                    // the cooldown rather than held against it.
                    Some(said) => BuildFailure {
                        transient: true,
                        ..failure(
                            format!("the build could not fetch what it starts from: {said}"),
                            &log,
                        )
                    },
                    None => failure(
                        format!("buildctl build exited {}", out.status.code().unwrap_or(-1)),
                        &log,
                    ),
                },
            });
        }
        let digest = std::fs::read_to_string(&metadata)
            .map_err(|error| format!("build metadata: {error}"))
            .and_then(|text| pushed_digest(&text))
            .map_err(|error| failure(error, &log))?;
        Ok(BuiltImage {
            image: format!("{}@{digest}", untagged(&target)),
            log_tail: crate::boundary::podman::log_tail(&log),
        })
    }

    async fn exists(&self, image: &str) -> bool {
        self.registry.exists(&self.resolve(image)).await
    }

    async fn identity(&self, image: &str) -> Option<String> {
        self.registry.identity(&self.resolve(image)).await
    }

    fn scratch_dir(&self) -> Option<PathBuf> {
        Some(self.scratch.clone())
    }

    async fn presence(&self, image: &str) -> crate::boundary::Presence {
        self.registry.presence(&self.resolve(image)).await
    }

    async fn unavailable(&self) -> Option<String> {
        self.check().await.err().map(|why| unusable(&why))
    }

    /// A pod already scheduled on the image a rebuild replaces may not have
    /// pulled it yet.
    fn retains_previous(&self) -> bool {
        true
    }

    /// Only what this node pushed, `<registry>/tracon-repo-…`: a harness
    /// image, or one an entry pinned by hand in this registry or another, is
    /// never this node's to delete.
    async fn remove(&self, image: &str) {
        let image = self.resolve(image);
        if !image.starts_with(&format!("{}/{OWN}", self.prefix)) {
            return;
        }
        if let Err(error) = self.registry.remove(&image).await {
            tracing::warn!(%image, %error, "could not remove a superseded image from the registry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::registry::tests::{docker_config, serve};

    const DIGEST: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    fn settings(registry: &str) -> KubeBuilder {
        KubeBuilder {
            registry: registry.into(),
            tls_dir: "/etc/tracon/buildkit".into(),
            docker_config: "/etc/tracon/registry".into(),
            ..Default::default()
        }
    }

    fn strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// The table takes what it is not given from the defaults, and a node
    /// without it has no builder; either way it survives a save.
    #[test]
    fn the_builder_table_is_read_with_its_defaults() {
        let cfg: crate::config::Config = toml::from_str(
            "[runtime]\nkind = \"kubernetes\"\n\
             [runtime.kubernetes.builder]\nregistry = \"registry.digitalocean.com/cs\"\ntimeout_secs = 600\n",
        )
        .unwrap();
        let builder = cfg.runtime.kubernetes.builder.clone().unwrap();
        assert_eq!(builder.addr, "tcp://buildkitd.tracon-build.svc:1234");
        assert_eq!(builder.registry, "registry.digitalocean.com/cs");
        assert_eq!(builder.timeout_secs, Some(600));
        assert_eq!(builder.do_token_file, None);
        let saved = toml::to_string(&cfg).unwrap();
        let back: crate::config::Config = toml::from_str(&saved).unwrap();
        assert_eq!(back.runtime.kubernetes.builder, Some(builder));

        let none: crate::config::Config =
            toml::from_str("[runtime]\nkind = \"kubernetes\"\n").unwrap();
        assert_eq!(none.runtime.kubernetes.builder, None);
        assert!(!toml::to_string(&none).unwrap().contains("builder"));
    }

    #[test]
    fn a_misconfigured_builder_says_what_is_wrong() {
        assert!(
            BuildKitBuilder::new_for_test(&settings("registry.digitalocean.com/cosmicspork"))
                .is_ok()
        );
        for (registry, addr) in [
            ("", "tcp://b:1234"),
            ("cosmicspork", "tcp://b:1234"),
            ("registry.digitalocean.com", "tcp://b:1234"),
            ("registry.digitalocean.com/", "tcp://b:1234"),
            ("https://registry.digitalocean.com/x", "tcp://b:1234"),
            ("localhost/x", "tcp://b:1234"),
            (
                "registry.digitalocean.com/x",
                "unix:///run/buildkit/buildkitd.sock",
            ),
        ] {
            let mut bad = settings(registry);
            bad.addr = addr.into();
            assert!(
                BuildKitBuilder::new_for_test(&bad).is_err(),
                "{registry} {addr}"
            );
        }
    }

    /// The argv names the daemon over mTLS, sends the context and the
    /// Dockerfile's own directory, carries the labels, and pushes under the
    /// registry's name for a `localhost/` tag.
    #[test]
    fn the_build_is_rendered_as_one_buildctl_call() {
        let builder =
            BuildKitBuilder::new_for_test(&settings("registry.digitalocean.com/cosmicspork/"))
                .unwrap();
        let target = builder.pushed_name("localhost/tracon-repo-app-0123456789-src:abc");
        assert_eq!(
            target,
            "registry.digitalocean.com/cosmicspork/tracon-repo-app-0123456789-src:abc"
        );
        let labels = [("io.tracon.recipe".to_string(), "abc".to_string())];
        let build = ImageBuild {
            tag: "localhost/tracon-repo-app-0123456789-src:abc",
            containerfile: Path::new("/tmp/stage/Containerfile"),
            context: Path::new("/tmp/context"),
            labels: &labels,
            timeout: Duration::from_secs(60),
        };
        let argv = strings(&builder.argv(&build, &target, Path::new("/tmp/stage/meta.json")));
        assert_eq!(
            argv,
            [
                "--addr",
                "tcp://buildkitd.tracon-build.svc:1234",
                "--tlsdir",
                "/etc/tracon/buildkit",
                "build",
                "--frontend",
                "dockerfile.v0",
                "--local",
                "context=/tmp/context",
                "--local",
                "dockerfile=/tmp/stage",
                "--opt",
                "filename=Containerfile",
                "--opt",
                "label:io.tracon.recipe=abc",
                "--output",
                "type=image,name=registry.digitalocean.com/cosmicspork/tracon-repo-app-0123456789-src:abc,push=true",
                "--metadata-file",
                "/tmp/stage/meta.json",
                "--progress",
                "plain",
            ]
        );
    }

    #[test]
    fn names_move_into_the_registry_and_others_stay_as_they_are() {
        let builder =
            BuildKitBuilder::new_for_test(&settings("registry.digitalocean.com/cs")).unwrap();
        assert_eq!(
            builder.resolve("localhost/tracon-repo-a@sha256:00"),
            "registry.digitalocean.com/cs/tracon-repo-a@sha256:00"
        );
        assert_eq!(
            builder.resolve("ghcr.io/cosmicspork/tracon-harness-claude:1"),
            "ghcr.io/cosmicspork/tracon-harness-claude:1"
        );
        assert_eq!(
            builder.pushed_name("registry.digitalocean.com/cs/tracon-repo-a:1"),
            "registry.digitalocean.com/cs/tracon-repo-a:1"
        );
        assert_eq!(
            builder.pushed_name("tracon-repo-a:1"),
            "registry.digitalocean.com/cs/tracon-repo-a:1"
        );
        assert_eq!(duration_words(Duration::from_secs(90)), "90 seconds");
        assert_eq!(duration_words(Duration::from_secs(45 * 60)), "45 minutes");
        assert_eq!(untagged("host:5000/a/b:t"), "host:5000/a/b");
        assert_eq!(untagged("host:5000/a/b"), "host:5000/a/b");
    }

    #[test]
    fn the_digest_comes_from_the_metadata_file() {
        let metadata = serde_json::json!({
            "containerimage.config.digest": "sha256:3333333333333333333333333333333333333333333333333333333333333333",
            "containerimage.digest": DIGEST,
            "image.name": "registry.digitalocean.com/cs/a:t",
        });
        assert_eq!(pushed_digest(&metadata.to_string()).unwrap(), DIGEST);
        assert!(pushed_digest("{}")
            .unwrap_err()
            .contains("containerimage.digest"));
        assert!(pushed_digest(r#"{"containerimage.digest":"sha256:short"}"#).is_err());
        assert!(pushed_digest("not json").is_err());
    }

    /// A `buildctl` stand-in: records its argv and environment, and does
    /// what `$STUB` says.
    #[cfg(unix)]
    fn stub(dir: &Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("buildctl");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {log}/argv\necho \"$DOCKER_CONFIG\" > {log}/docker_config\n{body}\n",
                log = dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// A build writes the digest the daemon pushed, and the image is named by
    /// it under the registry; a failed one keeps what it printed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_build_reports_the_pushed_digest_or_what_it_printed() {
        let tmp = tempfile::tempdir().unwrap();
        let containerfile = tmp.path().join("Containerfile");
        std::fs::write(&containerfile, "FROM scratch\n").unwrap();
        let mut config = settings("registry.example.com/cs");
        config.buildctl = stub(
            tmp.path(),
            &format!(
                "while [ \"$1\" != --metadata-file ]; do shift; done\n\
                 echo '#1 pushing layers' >&2\n\
                 printf '{{\"containerimage.digest\":\"{DIGEST}\"}}' > \"$2\""
            ),
        );
        let builder = BuildKitBuilder::new_for_test(&config).unwrap();
        let built = builder
            .build(ImageBuild {
                tag: "localhost/tracon-repo-a:t",
                containerfile: &containerfile,
                context: tmp.path(),
                labels: &[],
                timeout: Duration::from_secs(30),
            })
            .await
            .unwrap();
        assert_eq!(
            built.image,
            format!("registry.example.com/cs/tracon-repo-a@{DIGEST}")
        );
        assert!(built.log_tail.contains("pushing layers"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("docker_config"))
                .unwrap()
                .trim(),
            "/etc/tracon/registry"
        );

        // A Dockerfile that fails, with the daemon and the registry well, is
        // the build's own failure; with the daemon gone it is the daemon's.
        config.buildctl = stub(
            tmp.path(),
            "if [ \"$5\" = debug ]; then printf 'ID PLATFORMS\\nw linux/amd64\\n'; exit 0; fi\n\
             echo 'ERROR: failed to solve: no such package' >&2; exit 1",
        );
        let (_fake, host) = serve().await;
        let credential = docker_config(&host);
        let fake_registry =
            || Registry::new(Some(credential.path().to_path_buf())).for_test("unused", "unused");
        let mut at_fake = config.clone();
        at_fake.registry = format!("{host}/cs");
        let failed = BuildKitBuilder::new_for_test(&at_fake)
            .unwrap()
            .with_registry(fake_registry())
            .build(ImageBuild {
                tag: "localhost/tracon-repo-a:t",
                containerfile: &containerfile,
                context: tmp.path(),
                labels: &[],
                timeout: Duration::from_secs(30),
            })
            .await
            .unwrap_err();
        assert_eq!(failed.error, "buildctl build exited 1");
        assert!(failed.log_tail.contains("no such package"));
        // The recipe's own failure: remembered against it.
        assert!(!failed.transient);

        // A registry that could not be reached while the build fetched its
        // source is not the recipe's failure.
        let fetching = |said: &str| {
            let mut config = at_fake.clone();
            config.buildctl = stub(
                tmp.path(),
                &format!(
                    "if [ \"$5\" = debug ]; then printf 'ID PLATFORMS\\nw linux/amd64\\n'; exit 0; fi\n\
                     echo '{said}' >&2; exit 1"
                ),
            );
            config
        };
        for (said, transient) in [
            (
                "error: failed to solve: ghcr.io/cosmicspork/tracon-harness-claude:9: failed to \
                 resolve source metadata for ghcr.io/cosmicspork/tracon-harness-claude:9: failed \
                 to do request: Head https://ghcr.io/v2/: dial tcp: lookup ghcr.io: no such host",
                true,
            ),
            (
                "error: failed to solve: failed to fetch anonymous token: unexpected status from \
                 GET request to https://auth.docker.io/token: 429 Too Many Requests",
                true,
            ),
            (
                "error: failed to solve: docker.io/library/nope:1: failed to resolve source \
                 metadata for docker.io/library/nope:1: docker.io/library/nope:1: not found",
                false,
            ),
            (
                "error: failed to solve: process \"/bin/sh -c curl x\" did not complete \
                 successfully: exit code: 7 (connection refused)",
                false,
            ),
        ] {
            let failed = BuildKitBuilder::new_for_test(&fetching(said))
                .unwrap()
                .with_registry(fake_registry())
                .build(ImageBuild {
                    tag: "localhost/tracon-repo-a:t",
                    containerfile: &containerfile,
                    context: tmp.path(),
                    labels: &[],
                    timeout: Duration::from_secs(30),
                })
                .await
                .unwrap_err();
            assert_eq!(failed.transient, transient, "{said}: {}", failed.error);
            if transient {
                assert!(
                    failed
                        .error
                        .starts_with("the build could not fetch what it starts from: "),
                    "{}",
                    failed.error
                );
            }
        }

        config.buildctl = stub(tmp.path(), "echo 'error: connection refused' >&2; exit 1");
        let failed = BuildKitBuilder::new_for_test(&config)
            .unwrap()
            .build(ImageBuild {
                tag: "localhost/tracon-repo-a:t",
                containerfile: &containerfile,
                context: tmp.path(),
                labels: &[],
                timeout: Duration::from_secs(30),
            })
            .await
            .unwrap_err();
        assert_eq!(
            failed.error,
            "buildkitd at tcp://buildkitd.tracon-build.svc:1234 refused `buildctl debug workers`: \
             error: connection refused, so a repository's image cannot be built"
        );
        // The builder's failure: not remembered against the recipe.
        assert!(failed.transient);

        // A client that cannot start is the builder's failure too.
        let mut missing = config.clone();
        missing.buildctl = tmp
            .path()
            .join("no-buildctl")
            .to_string_lossy()
            .into_owned();
        let failed = BuildKitBuilder::new_for_test(&missing)
            .unwrap()
            .build(ImageBuild {
                tag: "localhost/tracon-repo-a:t",
                containerfile: &containerfile,
                context: tmp.path(),
                labels: &[],
                timeout: Duration::from_secs(30),
            })
            .await
            .unwrap_err();
        assert!(
            failed.transient && failed.error.contains("no-buildctl"),
            "{}",
            failed.error
        );

        // An unusable table is said before anything is staged.
        let unusable_builder = Unusable("[runtime.kubernetes.builder] is not usable (x)".into());
        assert_eq!(
            unusable_builder.unavailable().await.as_deref(),
            Some("[runtime.kubernetes.builder] is not usable (x), so a repository's image cannot be built")
        );
        assert_eq!(unusable_builder.scratch_dir(), None);

        // Exited 0 but pushed nothing it would name.
        config.buildctl = stub(tmp.path(), "exit 0");
        let failed = BuildKitBuilder::new_for_test(&config)
            .unwrap()
            .build(ImageBuild {
                tag: "localhost/tracon-repo-a:t",
                containerfile: &containerfile,
                context: tmp.path(),
                labels: &[],
                timeout: Duration::from_secs(30),
            })
            .await
            .unwrap_err();
        assert!(failed.error.contains("build metadata"), "{}", failed.error);
    }

    /// A build past its deadline is given up, and the client is killed with
    /// it rather than left pushing.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_build_past_its_deadline_is_killed() {
        let tmp = tempfile::tempdir().unwrap();
        let containerfile = tmp.path().join("Containerfile");
        std::fs::write(&containerfile, "FROM scratch\n").unwrap();
        let mut config = settings("registry.example.com/cs");
        config.buildctl = stub(
            tmp.path(),
            &format!("echo $$ > {}/pid; exec sleep 30", tmp.path().display()),
        );
        config.timeout_secs = Some(1);
        let started = std::time::Instant::now();
        let failed = BuildKitBuilder::new_for_test(&config)
            .unwrap()
            .build(ImageBuild {
                tag: "localhost/tracon-repo-a:t",
                containerfile: &containerfile,
                context: tmp.path(),
                labels: &[],
                timeout: Duration::from_secs(600),
            })
            .await
            .unwrap_err();
        assert_eq!(failed.error, "the build did not finish in 1 second");
        assert!(!failed.transient);
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = std::fs::read_to_string(tmp.path().join("pid")).unwrap();
        let mut gone = false;
        for _ in 0..50 {
            let state = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
            // Gone, or a zombie waiting on its reaper.
            if state.map(|s| s.contains(") Z ")).unwrap_or(true) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(gone, "buildctl {pid} still runs");
    }

    /// `exists`, `identity` and `remove` take the `localhost/` spelling, and
    /// `remove` never touches another registry's image.
    #[tokio::test]
    async fn lookups_take_either_spelling_and_remove_stays_in_its_registry() {
        let (fake, host) = serve().await;
        fake.put("cs/tracon-repo-a", "t", DIGEST);
        fake.put("other/harness", "1", DIGEST);
        fake.put("cs/toolchain", "1", DIGEST);
        let config = docker_config(&host);
        let mut settings = settings(&format!("{host}/cs"));
        settings.docker_config = config.path().to_path_buf();
        let registry =
            Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");
        let builder = BuildKitBuilder::new_for_test(&settings)
            .unwrap()
            .with_registry(registry);

        assert_eq!(
            builder.identity("localhost/tracon-repo-a:t").await,
            Some(format!("{host}/cs/tracon-repo-a@{DIGEST}"))
        );
        assert!(
            builder
                .exists(&format!("localhost/tracon-repo-a@{DIGEST}"))
                .await
        );
        assert!(
            builder
                .exists(&format!("{host}/cs/tracon-repo-a@{DIGEST}"))
                .await
        );

        builder.remove(&format!("{host}/other/harness:1")).await;
        assert!(builder.exists(&format!("{host}/other/harness:1")).await);
        // An operator's own image in the same namespace is not this node's.
        builder.remove(&format!("{host}/cs/toolchain:1")).await;
        assert!(builder.exists(&format!("{host}/cs/toolchain:1")).await);
        builder.remove("localhost/tracon-repo-a:t").await;
        assert!(!builder.exists("localhost/tracon-repo-a:t").await);
    }

    /// Against a real daemon and registry, when one is given:
    /// `TRACON_E2E_BUILDKIT_ADDR` (tcp://…), `TRACON_E2E_BUILDKIT_TLS` (the
    /// client certificate's directory), `TRACON_E2E_REGISTRY` (a plain-HTTP
    /// registry with deletion on, as `host:port/namespace`) and
    /// `TRACON_E2E_BUILDCTL`. Builds an image and one `FROM` it by digest,
    /// as `repo_image` does, looks both up, and removes them.
    #[tokio::test]
    #[ignore = "needs a BuildKit daemon and a registry"]
    async fn a_real_daemon_builds_pushes_and_the_registry_answers() {
        let var = |name: &str| std::env::var(name).ok();
        let (Some(addr), Some(tls), Some(registry), Some(buildctl)) = (
            var("TRACON_E2E_BUILDKIT_ADDR"),
            var("TRACON_E2E_BUILDKIT_TLS"),
            var("TRACON_E2E_REGISTRY"),
            var("TRACON_E2E_BUILDCTL"),
        ) else {
            eprintln!("skipped: no daemon named");
            return;
        };
        let host = registry.split('/').next().unwrap().to_string();
        let config = tempfile::tempdir().unwrap();
        let auth = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode("e2e:e2e")
        };
        std::fs::write(
            config.path().join("config.json"),
            serde_json::json!({ "auths": { host.clone(): { "auth": auth } } }).to_string(),
        )
        .unwrap();
        let settings = KubeBuilder {
            addr,
            tls_dir: tls.into(),
            registry: registry.clone(),
            docker_config: config.path().to_path_buf(),
            buildctl,
            ..Default::default()
        };
        let plain = Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");
        let builder = BuildKitBuilder::new_for_test(&settings)
            .unwrap()
            .with_registry(plain);
        builder.check().await.unwrap();

        let stage = tempfile::tempdir().unwrap();
        let context = stage.path().join("context");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("a.txt"), "from the context\n").unwrap();
        let containerfile = stage.path().join("Containerfile");
        std::fs::write(
            &containerfile,
            "FROM docker.io/library/busybox:1.36\nCOPY a.txt /a.txt\nRUN cat /a.txt\n",
        )
        .unwrap();
        let labels = [("io.tracon.recipe".to_string(), "e2e".to_string())];
        let tag = format!("localhost/tracon-repo-e2e-{}-src:t", std::process::id());
        let source = builder
            .build(ImageBuild {
                tag: &tag,
                containerfile: &containerfile,
                context: &context,
                labels: &labels,
                timeout: Duration::from_secs(600),
            })
            .await
            .unwrap_or_else(|failure| panic!("{}\n{}", failure.error, failure.log_tail));
        assert!(source
            .image
            .starts_with(&format!("{registry}/tracon-repo-e2e-")));
        assert_eq!(
            builder.identity(&tag).await.as_deref(),
            Some(source.image.as_str())
        );
        assert!(builder.exists(&source.image).await);

        let layer = stage.path().join("Normalised.Containerfile");
        std::fs::write(&layer, format!("FROM {}\nWORKDIR /work\n", source.image)).unwrap();
        let empty = stage.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let layered = builder
            .build(ImageBuild {
                tag: &format!("localhost/tracon-repo-e2e-{}:t", std::process::id()),
                containerfile: &layer,
                context: &empty,
                labels: &labels,
                timeout: Duration::from_secs(600),
            })
            .await
            .unwrap_or_else(|failure| panic!("{}\n{}", failure.error, failure.log_tail));
        assert!(builder.exists(&layered.image).await);

        builder.remove(&layered.image).await;
        builder.remove(&tag).await;
        assert!(!builder.exists(&layered.image).await);
        assert!(!builder.exists(&source.image).await);
    }

    /// The check names the daemon's refusal, and a registry that refuses the
    /// credential, separately.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_check_says_which_half_is_wrong() {
        let (_fake, host) = serve().await;
        let tmp = tempfile::tempdir().unwrap();
        let config = docker_config(&host);
        let mut settings = settings(&format!("{host}/cs"));
        settings.docker_config = config.path().to_path_buf();
        let registry =
            || Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");

        settings.buildctl = stub(
            tmp.path(),
            "echo 'error: tls: failed to verify certificate: x509: certificate signed by unknown authority' >&2; exit 1",
        );
        let error = BuildKitBuilder::new_for_test(&settings)
            .unwrap()
            .with_registry(registry())
            .check()
            .await
            .unwrap_err();
        assert!(
            error.contains("refused `buildctl debug workers`") && error.contains("x509"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("argv")).unwrap(),
            "--addr\ntcp://buildkitd.tracon-build.svc:1234\n--tlsdir\n/etc/tracon/buildkit\ndebug\nworkers\n"
        );

        settings.buildctl = stub(tmp.path(), "echo 'ID PLATFORMS'; echo 'abc linux/amd64'");
        let said = BuildKitBuilder::new_for_test(&settings)
            .unwrap()
            .with_registry(registry())
            .check()
            .await
            .unwrap();
        assert!(
            said.contains("1 worker(s)") && said.contains("accepts the credential"),
            "{said}"
        );

        let empty = tempfile::tempdir().unwrap();
        let error = BuildKitBuilder::new_for_test(&settings)
            .unwrap()
            .with_registry(
                Registry::new(Some(empty.path().to_path_buf())).for_test("unused", "unused"),
            )
            .check()
            .await
            .unwrap_err();
        assert!(error.contains("holds no credential"), "{error}");
    }
}
