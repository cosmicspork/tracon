//! The rootless Podman boundary: an internal network, a gateway container
//! carrying the allowlist proxy and the node forward, and a harness container
//! on the internal network only. Phase 0 proved it by hand; this is that
//! proof as code.

pub mod checks;
pub mod setup;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use super::{Backend, BoundaryError, BoundaryReport};
use crate::config::Config;
use crate::runner::podman::{PodmanRunner, RunSpec};
use crate::runner::{Mount, Runner};

pub struct PodmanBackend {
    cfg: Config,
    selinux: bool,
}

impl PodmanBackend {
    pub async fn detect(cfg: &Config) -> Self {
        // Before the first spawn: everything after this, `selinux_enabled`
        // included, goes through `podman()`.
        let _ = PODMAN_BIN.set(resolve_podman_env(cfg));
        Self {
            cfg: cfg.clone(),
            selinux: selinux_enabled().await,
        }
    }

    pub fn spec(&self) -> RunSpec {
        RunSpec::from_config(&self.cfg, self.selinux)
    }
}

#[async_trait]
impl Backend for PodmanBackend {
    fn kind(&self) -> &'static str {
        "podman"
    }

    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
        setup::setup(cfg, rebuild).await
    }

    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport {
        checks::check_all(cfg, self.selinux, deep).await
    }

    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        let mut spec = self.spec();
        spec.extra_mounts = extra_mounts;
        Arc::new(PodmanRunner::new(spec))
    }

    fn harness_host(&self) -> String {
        self.cfg.boundary.gateway_container.clone()
    }

    fn harness_home(&self) -> String {
        crate::session::materialize::PODMAN_HARNESS_HOME.into()
    }

    async fn reconcile(&self, names: &[String]) {
        for name in names {
            let _ = podman(&["rm", "-f", "-i", name]).await;
        }
    }
}

static PODMAN_BIN: OnceLock<String> = OnceLock::new();

/// The resolved podman binary. Before any backend was detected (unit tests,
/// mostly) this is the bare name, which resolves through PATH as it always
/// did.
pub(crate) fn podman_bin() -> &'static str {
    PODMAN_BIN.get().map(String::as_str).unwrap_or("podman")
}

/// Resolve which podman to run. The node often lives under a launcher whose
/// PATH is not a login shell's — Finder hands out launchd's minimal PATH,
/// which has no Homebrew — so a bare `Command::new("podman")` fails with
/// ENOENT while the terminal works fine. Order: the operator's explicit
/// config, verbatim; a PATH hit; well-known install locations; the bare name,
/// so the spawn error still says what was tried.
pub fn resolve_podman(
    explicit: &str,
    path_var: Option<&std::ffi::OsStr>,
    candidates: &[&Path],
) -> String {
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    if let Some(path) = path_var {
        for dir in std::env::split_paths(path) {
            let p = dir.join("podman");
            if p.is_file() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    for c in candidates {
        if c.is_file() {
            return c.to_string_lossy().into_owned();
        }
    }
    "podman".into()
}

pub(crate) fn resolve_podman_env(cfg: &Config) -> String {
    let candidates = [
        PathBuf::from("/opt/homebrew/bin/podman"),
        PathBuf::from("/usr/local/bin/podman"),
        PathBuf::from("/usr/bin/podman"),
    ];
    let refs: Vec<&Path> = candidates.iter().map(PathBuf::as_path).collect();
    resolve_podman(
        &cfg.boundary.podman,
        std::env::var_os("PATH").as_deref(),
        &refs,
    )
}

/// Run `podman` with args and return stdout, or the stderr as an error.
pub(crate) async fn podman(args: &[&str]) -> Result<String, BoundaryError> {
    let bin = podman_bin();
    let out = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                BoundaryError::Other(format!(
                    "podman not found (tried `{bin}`); install podman or set `podman = \"/path/to/podman\"` under [boundary] in node.toml"
                ))
            } else {
                BoundaryError::Io(e)
            }
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(BoundaryError::Podman(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

pub(crate) async fn podman_json(args: &[&str]) -> Result<serde_json::Value, BoundaryError> {
    let text = podman(args).await?;
    Ok(serde_json::from_str(&text)?)
}

/// Whether the container host enforces SELinux (Podman needs `label=disable`
/// for bind mounts when it does).
pub async fn selinux_enabled() -> bool {
    podman(&["info", "--format", "{{.Host.Security.SELinuxEnabled}}"])
        .await
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::resolve_podman;
    use std::path::Path;

    fn touch(dir: &Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "").unwrap();
        p
    }

    #[test]
    fn an_explicit_config_value_wins_even_when_it_does_not_exist() {
        // The operator said so; a wrong path should fail loudly at spawn, not
        // be silently second-guessed.
        let got = resolve_podman("/nonexistent/podman", None, &[]);
        assert_eq!(got, "/nonexistent/podman");
    }

    #[test]
    fn a_path_hit_beats_the_candidates() {
        let dir = std::env::temp_dir().join(format!("tracon-podman-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("on-path")).unwrap();
        std::fs::create_dir_all(dir.join("candidate")).unwrap();
        let on_path = touch(&dir.join("on-path"), "podman");
        let candidate = touch(&dir.join("candidate"), "podman");
        let path_var = std::env::join_paths([dir.join("on-path")]).unwrap();
        let got = resolve_podman("", Some(&path_var), &[&candidate]);
        assert_eq!(got, on_path.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidates_rescue_a_minimal_path_and_the_bare_name_is_last() {
        let dir = std::env::temp_dir().join(format!("tracon-podman-cand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let candidate = touch(&dir, "podman");
        let empty = std::ffi::OsString::new();
        let got = resolve_podman("", Some(&empty), &[&candidate]);
        assert_eq!(got, candidate.to_string_lossy());
        // Nothing anywhere: the bare name, so the spawn error names it.
        assert_eq!(resolve_podman("", Some(&empty), &[]), "podman");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
