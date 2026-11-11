//! The portable template key: what a DigitalOcean template holds, computed
//! alike by any node of any architecture, so one node can ask another for a
//! template by it.
//!
//! DigitalOcean's Harness Runtime runs amd64 alone, and a template belongs
//! to the team, not a node: any node with the team's token can start a
//! sandbox from any template by name. So a node that cannot build amd64 (an
//! Apple one) asks a peer that can ([`super::shared`]) for the template
//! named for its key, and takes it once that peer says it is ready
//! ([`super::peers`]). Nothing node-local is in the key:
//!
//! - the stock base by its release, as its registry's index digest, never
//!   the platform manifest a node pinned;
//! - the repository by its forge identity, and its recipe as Git objects
//!   and text: the `build` steps, or the Dockerfile's blob and its context's
//!   tree; `prepare`, `egress`, the lockfiles' blobs, the cache environment;
//! - the harness release, the do-sandbox Containerfile and its `run`;
//! - each catalogue service bundle by name and version, not path.
//!
//! The template is named from the key under a team-wide stem
//! (`<prefix>-tp-<key>`), the same on every node.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use sha2::{Digest as _, Sha256};

use crate::config::Config;
use crate::sandbox_images::Lockfile;

const SCHEME: &str = "tracon-do-portable/1";

/// The repository a portable key is given under as an image reference, so
/// it travels where a local image's digest does: `<this>/<label>@sha256:<key>`.
pub const IMAGE_REPOSITORY: &str = "tracon-do-portable";

/// How a repository's image is made, as portable as Git makes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recipe {
    /// Prepared on the stock base itself.
    Base,
    /// `[[repo]] build`: root steps on the stock base, reaching `egress`.
    Steps {
        steps: Vec<String>,
        egress: Vec<String>,
    },
    /// The repository's Dockerfile and its build context, by Git object.
    Dockerfile {
        path: String,
        blob: String,
        context: String,
        tree: String,
    },
}

/// A repository's part of a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPart {
    /// `host/owner/name`.
    pub identity: String,
    pub recipe: Recipe,
    pub lockfiles: Vec<Lockfile>,
    pub prepare: Vec<String>,
    pub egress: Vec<String>,
}

/// Everything a template is made from, as any node names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortableInputs {
    /// The stock base, `name@sha256:<index digest>`.
    pub base: String,
    /// The harness bundle's release.
    pub harness: String,
    /// Each catalogue service with a bundle, `(name, version)`, by name.
    pub services: Vec<(String, String)>,
    /// `None` for the stock base's own template.
    pub repo: Option<RepoPart>,
}

impl PortableInputs {
    pub fn key(&self) -> String {
        let mut hash = Sha256::new();
        let mut part = |text: &str| {
            hash.update(text.as_bytes());
            hash.update([0]);
        };
        part(SCHEME);
        part(&self.base);
        part(&self.harness);
        part(super::images::CONTAINERFILE);
        part(super::bootstrap::RUN_SCRIPT);
        part("services");
        for (name, version) in &self.services {
            part(name);
            part(version);
        }
        if let Some(repo) = &self.repo {
            part("repo");
            part(&repo.identity);
            match &repo.recipe {
                Recipe::Base => part("base"),
                Recipe::Steps { steps, egress } => {
                    part("build");
                    for step in steps {
                        part(step);
                    }
                    part("build-egress");
                    for host in egress {
                        part(host);
                    }
                }
                Recipe::Dockerfile {
                    path,
                    blob,
                    context,
                    tree,
                } => {
                    part("dockerfile");
                    for text in [path, blob, context, tree] {
                        part(text);
                    }
                }
            }
            part("lockfiles");
            for lockfile in &repo.lockfiles {
                part(&lockfile.path);
                part(&lockfile.blob);
            }
            part("prepare");
            for command in &repo.prepare {
                part(command);
            }
            part("egress");
            for host in &repo.egress {
                part(host);
            }
            part("env");
            for (name, value) in crate::environment::cache_env() {
                part(&name);
                part(&value);
            }
        }
        hex::encode(hash.finalize())
    }

    /// The key as an image reference: what a session placed on a peer's
    /// template runs in, as far as this node's runners are concerned.
    pub fn image(&self) -> String {
        let label = match &self.repo {
            None => "base".to_string(),
            Some(repo) => label_of(&repo.identity),
        };
        image(&label, &self.key())
    }
}

fn label_of(identity: &str) -> String {
    let label: String = identity
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let label = label.trim_matches('-');
    match label.is_empty() {
        true => "repo".into(),
        false => label.chars().take(60).collect(),
    }
}

/// `<IMAGE_REPOSITORY>/<label>@sha256:<key>`.
pub fn image(label: &str, key: &str) -> String {
    format!("{IMAGE_REPOSITORY}/{label}@sha256:{key}")
}

/// The key a portable image reference carries, if it is one.
pub fn key_of(image: &str) -> Option<&str> {
    let (name, digest) = image.rsplit_once('@')?;
    name.strip_prefix(IMAGE_REPOSITORY)?.strip_prefix('/')?;
    let key = digest.strip_prefix("sha256:")?;
    valid_key(key).then_some(key)
}

/// A key as [`PortableInputs::key`] spells it: 64 lower-case hex characters.
pub fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The team-wide stem shared templates are named under, `<prefix>-tp-`.
pub fn team_stem(prefix: &str) -> String {
    format!("{prefix}-tp-")
}

/// The template named for `key` under `team_stem`.
pub fn team_name(team_stem: &str, key: &str) -> String {
    format!("{team_stem}{}", &key[..key.len().min(16)])
}

/// The harness bundle's release: its manifest's version where it has one,
/// else this build's.
pub fn harness_release(cfg: &Config) -> String {
    crate::bundle::effective(cfg)
        .and_then(|dir| std::fs::read(dir.join("manifest.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|manifest| manifest["version"].as_str().map(str::to_string))
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// Each catalogue service with a bundle, by name and version: the
/// release's browser by its Chrome, any other by its manifest's version.
pub fn services(cfg: &Config) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = cfg
        .service
        .iter()
        .filter(|service| service.bundle.is_some())
        .map(|service| {
            let version = match crate::service_bundle::is_release(service) {
                true => format!("release:{}", crate::service_bundle::CHROME_VERSION),
                false => crate::service_bundle::resolve(service)
                    .and_then(|dir| std::fs::read(dir.join("manifest.json")).ok())
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .and_then(|manifest| manifest["version"].as_str().map(str::to_string))
                    .map_or_else(|| "unversioned".into(), |v| format!("bundle:{v}")),
            };
            (service.name.clone(), version)
        })
        .collect();
    out.sort();
    out
}

/// The stock base by its release: `configured`'s name and the digest its
/// registry names for it (an index, for a multi-platform base), asked once
/// per process. An image only a host's store holds has no release.
pub async fn base_release(configured: &str) -> Result<String, String> {
    static KNOWN: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
    if let Some(known) = KNOWN
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|known| known.get(configured).cloned())
    {
        return Ok(known);
    }
    if crate::boundary::podman::local_only(configured) {
        return Err(format!(
            "the sandbox base {configured} is only in this host's store, so no other node can \
             name it"
        ));
    }
    let reference = crate::boundary::registry::Reference::parse(configured)
        .map_err(|e| format!("the sandbox base {configured}: {e}"))?;
    let digest = match reference.is_digest() {
        true => reference.reference.clone(),
        false => crate::boundary::registry::Registry::new(None)
            .digest(configured)
            .await
            .map_err(|e| format!("the sandbox base {configured}'s release digest: {e}"))?
            .ok_or_else(|| format!("the sandbox base {configured} is not in its registry"))?,
    };
    let release = format!("{}@{digest}", reference.name);
    KNOWN
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(configured.to_string(), release.clone());
    Ok(release)
}

/// The stock base's own template's inputs.
pub async fn base_inputs(cfg: &Config) -> Result<PortableInputs, String> {
    Ok(PortableInputs {
        base: base_release(&cfg.sandbox.base_image_ref()).await?,
        harness: harness_release(cfg),
        services: services(cfg),
        repo: None,
    })
}

/// `repo`'s template's inputs, as its default branch has it now, and that
/// branch's commit. `identity` is the repository's forge identity.
pub async fn repo_inputs(
    cfg: &Config,
    repo: &Path,
    identity: &str,
) -> Result<(PortableInputs, String), String> {
    let repos = cfg.repos();
    let entry = repos
        .iter()
        .find(|entry| entry.matches(repo))
        .ok_or_else(|| format!("no [[repo]] entry here is for {identity}"))?;
    if entry.image.is_some() {
        return Err(format!(
            "{identity}'s entry names an image of its own, which no other node can build"
        ));
    }
    let (_, commit) = crate::repo_image::default_commit(repo).await?;
    let dir = repo.to_string_lossy();
    let resolve = |revision: String| {
        let dir = dir.clone();
        async move {
            crate::review::resolve(&dir, &revision)
                .await
                .map_err(|_| format!("{revision} is not in {identity}"))
        }
    };
    let recipe = match (&entry.dockerfile, entry.build.is_empty()) {
        (_, false) => Recipe::Steps {
            steps: entry.build.clone(),
            egress: entry.egress_hosts()?,
        },
        (Some(path), true) => {
            let context = match entry.context.as_deref() {
                Some(".") => String::new(),
                Some(context) => context.to_string(),
                None => path
                    .rsplit_once('/')
                    .map(|(directory, _)| directory.to_string())
                    .unwrap_or_default(),
            };
            let tree = match context.is_empty() {
                true => format!("{commit}^{{tree}}"),
                false => format!("{commit}:{context}"),
            };
            Recipe::Dockerfile {
                path: path.clone(),
                blob: resolve(format!("{commit}:{path}")).await?,
                tree: resolve(tree).await?,
                context,
            }
        }
        (None, true) => Recipe::Base,
    };
    let inputs = PortableInputs {
        base: base_release(&cfg.sandbox.base_image_ref()).await?,
        harness: harness_release(cfg),
        services: services(cfg),
        repo: Some(RepoPart {
            identity: identity.to_string(),
            recipe,
            lockfiles: crate::sandbox_images::lockfiles(repo, &commit).await,
            prepare: entry.prepare.clone(),
            egress: entry.egress_hosts()?,
        }),
    };
    Ok((inputs, commit))
}

/// The forge identity of the repository at `repo`: its managed clone's
/// path, or its `origin`.
pub async fn forge_identity(cfg: &Config, repo: &Path) -> Option<String> {
    if let Some(identity) = crate::forge::managed_identity(&Config::state_dir(), repo) {
        return Some(identity);
    }
    crate::corpus::project::identify("", repo, &cfg.live().publish.git)
        .await
        .2
        .filter(|identity| crate::mesh::recipe::valid_identity(identity).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PortableInputs {
        PortableInputs {
            base: "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:1111".into(),
            harness: "0.60.0".into(),
            services: vec![("browser".into(), "release:157".into())],
            repo: None,
        }
    }

    fn repo() -> RepoPart {
        RepoPart {
            identity: "github.com/acme/app".into(),
            recipe: Recipe::Steps {
                steps: vec!["apt-get install -y php8.5-cli".into()],
                egress: vec!["snapshot.ubuntu.com".into()],
            },
            lockfiles: vec![Lockfile {
                path: "composer.lock".into(),
                blob: "abc".into(),
            }],
            prepare: vec!["composer install".into()],
            egress: vec!["packagist.org".into()],
        }
    }

    /// Every part of what a template holds moves its key, and nothing of
    /// the node that computes it is in it.
    #[test]
    fn the_key_moves_with_everything_a_template_holds() {
        let plain = base().key();
        assert!(valid_key(&plain));
        assert_eq!(base().key(), plain, "the same inputs, the same key");
        let with_repo = PortableInputs {
            repo: Some(repo()),
            ..base()
        };
        let key = with_repo.key();
        assert_ne!(key, plain);
        let moved = |change: &dyn Fn(&mut PortableInputs)| {
            let mut inputs = with_repo.clone();
            change(&mut inputs);
            inputs.key()
        };
        for changed in [
            moved(&|i| i.base = "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:2222".into()),
            moved(&|i| i.harness = "0.61.0".into()),
            moved(&|i| i.services[0].1 = "release:158".into()),
            moved(&|i| i.services.clear()),
            moved(&|i| i.repo.as_mut().unwrap().identity = "github.com/acme/other".into()),
            moved(&|i| i.repo.as_mut().unwrap().lockfiles[0].blob = "def".into()),
            moved(&|i| i.repo.as_mut().unwrap().prepare.push("bun install".into())),
            moved(&|i| i.repo.as_mut().unwrap().egress.clear()),
            moved(&|i| i.repo.as_mut().unwrap().recipe = Recipe::Base),
            moved(&|i| {
                i.repo.as_mut().unwrap().recipe = Recipe::Dockerfile {
                    path: ".devcontainer/Dockerfile".into(),
                    blob: "b".into(),
                    context: ".devcontainer".into(),
                    tree: "t".into(),
                }
            }),
        ] {
            assert_ne!(changed, key);
        }
    }

    #[test]
    fn a_key_travels_as_an_image_reference_and_names_a_team_template() {
        let inputs = PortableInputs {
            repo: Some(repo()),
            ..base()
        };
        let image = inputs.image();
        assert!(image.starts_with("tracon-do-portable/github-com-acme-app@sha256:"));
        assert_eq!(key_of(&image), Some(inputs.key().as_str()));
        assert_eq!(
            super::super::templates::digest_of(&image).unwrap(),
            format!("sha256:{}", inputs.key())
        );
        assert_eq!(key_of(&base().image()), Some(base().key().as_str()));
        assert!(base().image().starts_with("tracon-do-portable/base@"));
        // A local image is not a portable one.
        let local = format!("localhost/tracon-snapshot-app@sha256:{}", "a".repeat(64));
        assert_eq!(key_of(&local), None);
        assert_eq!(key_of("tracon-do-portable/base@sha256:ABC"), None);
        let stem = team_stem("tracon");
        assert_eq!(stem, "tracon-tp-");
        assert_eq!(
            team_name(&stem, &inputs.key()),
            format!("tracon-tp-{}", &inputs.key()[..16])
        );
    }
}
