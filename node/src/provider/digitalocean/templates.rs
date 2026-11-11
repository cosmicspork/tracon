//! The custom templates a node's DigitalOcean sandboxes start from: one per
//! local image digest (the pinned stock base, or a repository's prepared
//! snapshot, whose digest moves with its key: base, repository image,
//! lockfiles, recipe), registered as part of preparing that image (a rebase
//! that took about seven minutes when tried), never per session. A session
//! is placed on DigitalOcean only in an image whose template is ready.
//!
//! The template's source is either an image the operator built and named
//! (`[digitalocean] image`, used for every session), or one the node builds
//! from its own image, the harness bundle it verified and the catalogue's
//! service bundles it holds, and pushes to the operator's registry
//! (`[digitalocean] registry`, see [`super::images`]). A built template is
//! named for everything it was built from, not the image alone: a release
//! moves the harness bundle, and a new browser bundle or a change to the
//! template's own recipe moves what the sandbox runs, so each makes a new
//! template rather than leaving sessions on the old one's binaries. While
//! the new one builds (about seven minutes), an image whose previous
//! template this node process saw ready keeps starting sandboxes from that
//! one; once the new one is ready it takes over, and only then is the old
//! one stale and swept. A node process that starts after its template's
//! name moved (an upgrade) has no previous one to fall back on, and waits.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::do_harness::{Client, DoError, Template, TemplateStatus};

/// Where one image's template stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TemplateState {
    /// Not looked for yet.
    Unknown,
    /// Its source is being built and pushed by this node.
    Preparing,
    /// Registered and being built by the runtime.
    Building {
        template_id: String,
    },
    Ready {
        template_id: String,
    },
    Failed {
        reason: String,
    },
}

impl TemplateState {
    pub fn describe(&self) -> String {
        match self {
            Self::Unknown => "its template has not been looked for yet".into(),
            Self::Preparing => "its template's image is being built and pushed".into(),
            Self::Building { .. } => {
                "its template is being built, which takes several minutes once per image".into()
            }
            Self::Ready { .. } => "its template is ready".into(),
            Self::Failed { reason } => format!("its template failed: {reason}"),
        }
    }
}

/// The digest an image reference names, `sha256:<hex>`: a template is
/// registered per digest, so a reference without one is refused.
pub fn digest_of(image: &str) -> Result<&str, String> {
    let digest = image
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .ok_or_else(|| format!("{image} names no digest: give it as name@sha256:<hex>"))?;
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| format!("{image}'s digest is not sha256"))?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{image}'s digest is not 64 hex characters"));
    }
    Ok(digest)
}

/// Where a template's source image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The operator's own, holding the bundle and the service already: the
    /// one image every session there runs in.
    Given(String),
    /// Built by the node from each local image and pushed here.
    Built {
        registry: String,
        prefix: String,
        bundle: PathBuf,
        /// The catalogue's service bundles: each one's directory on this
        /// host and where a sandbox has it (`/opt/tracon-services/<name>`).
        /// Those present when a template is built are baked into it.
        services: Vec<(PathBuf, String)>,
    },
}

/// A bundle directory as a template's name takes it: its manifest where it
/// has one (the release's do, and say what is in them), else its path.
fn bundle_identity(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("manifest.json"))
        .unwrap_or_else(|_| dir.to_string_lossy().into_owned().into_bytes())
}

/// The services of `services` whose bundle is on this host now, by name.
pub fn present(services: &[(PathBuf, String)]) -> Vec<(PathBuf, String)> {
    services
        .iter()
        .filter(|(dir, target)| dir.is_dir() && service_name(target).is_some())
        .cloned()
        .collect()
}

/// The catalogue name a bundle's sandbox path ends in.
pub fn service_name(target: &str) -> Option<&str> {
    target
        .strip_prefix(crate::config::SERVICE_BUNDLES_TARGET)?
        .strip_prefix('/')
        .filter(|name| !name.is_empty() && !name.contains('/') && *name != "..")
}

/// A template this node process saw ready, and what it holds.
#[derive(Debug, Clone, PartialEq)]
struct Known {
    name: String,
    /// The catalogue services baked into it, by name; `None` for an image
    /// of the operator's.
    services: Option<Vec<String>>,
}

pub struct Templates {
    base_template: String,
    stem: String,
    source: Source,
    /// By template name.
    states: Mutex<HashMap<String, TemplateState>>,
    /// The last template seen ready for each image, by the image's digest:
    /// what its sandboxes start from while a newer one builds.
    ready: Mutex<HashMap<String, Known>>,
}

impl Templates {
    /// `stem` is the node's own (`<prefix>-t<node>`); a template's name adds
    /// its image's digest, so a new image is a new template and never a
    /// rebuild of one sessions start from.
    pub fn new(source: Source, base_template: &str, stem: &str) -> Result<Self, String> {
        if let Source::Given(image) = &source {
            digest_of(image)?;
        }
        Ok(Templates {
            base_template: base_template.to_string(),
            stem: format!("{stem}-"),
            source,
            states: Mutex::new(HashMap::new()),
            ready: Mutex::new(HashMap::new()),
        })
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The template named for `image`: the operator's by its digest alone;
    /// one the node builds by its digest and what the node adds to it (the
    /// template's recipe, the harness bundle, the service bundles present).
    pub fn name_for(&self, image: &str) -> Result<String, String> {
        let digest = digest_of(image)?;
        let hex = match &self.source {
            Source::Given(_) => digest.trim_start_matches("sha256:").to_string(),
            Source::Built {
                bundle, services, ..
            } => {
                let mut hash = Sha256::new();
                for part in [
                    b"tracon-do-template/1".as_slice(),
                    digest.as_bytes(),
                    super::images::CONTAINERFILE.as_bytes(),
                    super::bootstrap::RUN_SCRIPT.as_bytes(),
                    &bundle_identity(bundle),
                ] {
                    hash.update(part);
                    hash.update([0]);
                }
                for (dir, target) in present(services) {
                    hash.update(target.as_bytes());
                    hash.update([0]);
                    hash.update(bundle_identity(&dir));
                    hash.update([0]);
                }
                hex::encode(hash.finalize())
            }
        };
        Ok(format!("{}{}", self.stem, &hex[..16]))
    }

    /// The catalogue services a template named for `image` now would hold:
    /// those the node bakes into the templates it builds, by name. `None`
    /// for an image of the operator's, whose contents the node takes on
    /// trust.
    pub fn current_services(&self) -> Option<Vec<String>> {
        match &self.source {
            Source::Given(_) => None,
            Source::Built { services, .. } => Some(
                present(services)
                    .iter()
                    .filter_map(|(_, target)| service_name(target).map(str::to_string))
                    .collect(),
            ),
        }
    }

    /// The catalogue services the template `name` holds, by name, as this
    /// node process saw it ready. `None` for an image of the operator's,
    /// taken on trust; one it never saw ready holds none it knows of.
    pub fn services_of(&self, name: &str) -> Option<Vec<String>> {
        if let Source::Given(_) = self.source {
            return None;
        }
        let known = self.ready.lock().unwrap();
        Some(
            known
                .values()
                .find(|k| k.name == name)
                .and_then(|k| k.services.clone())
                .unwrap_or_default(),
        )
    }

    /// The templates of `image` to keep: its current one, and while that is
    /// not ready, the one its sandboxes start from instead.
    pub fn kept(&self, image: &str) -> Vec<String> {
        let Ok(current) = self.name_for(image) else {
            return Vec::new();
        };
        let mut kept = vec![current.clone()];
        if let Some(previous) = self.previous(image).filter(|p| *p != current) {
            if !matches!(self.state(image), TemplateState::Ready { .. }) {
                kept.push(previous);
            }
        }
        kept
    }

    /// The template last seen ready for `image`.
    fn previous(&self, image: &str) -> Option<String> {
        let digest = digest_of(image).ok()?;
        self.ready
            .lock()
            .unwrap()
            .get(digest)
            .map(|k| k.name.clone())
    }

    /// The image a session runs in when it names none of its own: the
    /// operator's, else `fallback` (the node's pinned base).
    pub fn default_image(&self, fallback: Option<String>) -> Option<String> {
        match &self.source {
            Source::Given(image) => Some(image.clone()),
            Source::Built { .. } => fallback,
        }
    }

    /// Whether sessions in `image` can have a template here: with an image
    /// of the operator's, only that one.
    pub fn serves(&self, image: &str) -> bool {
        match &self.source {
            Source::Given(given) => given == image,
            Source::Built { .. } => digest_of(image).is_ok(),
        }
    }

    pub fn state(&self, image: &str) -> TemplateState {
        self.name_for(image)
            .ok()
            .and_then(|name| self.states.lock().unwrap().get(&name).cloned())
            .unwrap_or(TemplateState::Unknown)
    }

    /// The template `image`'s sandboxes start from, by name, or why there
    /// is none yet: its current one once ready, else the one last seen ready
    /// while the current one builds.
    pub fn ready(&self, image: &str) -> Result<String, String> {
        if !self.serves(image) {
            return Err(format!("DigitalOcean has no template for {image}"));
        }
        match self.state(image) {
            TemplateState::Ready { .. } => self.name_for(image),
            other => self.previous(image).ok_or_else(|| other.describe()),
        }
    }

    /// Whether `name` is a template this node registered that none of
    /// `wanted` (template names) is.
    pub fn is_stale(&self, name: &str, wanted: &HashSet<String>) -> bool {
        name.starts_with(&self.stem) && !wanted.contains(name)
    }

    /// Find `image`'s template, registering it (and first building and
    /// pushing its source, where the node makes it) when there is none, and
    /// record where it stands. Cheap once ready: one listing. `token` is
    /// what the registry push presents.
    pub async fn ensure(
        &self,
        client: &Client,
        token: &str,
        image: &str,
    ) -> Result<TemplateState, DoError> {
        if !self.serves(image) {
            return Err(DoError::Invalid(format!(
                "DigitalOcean has no template for {image}"
            )));
        }
        let name = self.name_for(image).map_err(DoError::Invalid)?;
        // Taken with the name, which is named for them.
        let services = self.current_services();
        let found = client
            .list_templates()
            .await?
            .into_iter()
            .find(|t| t.name == name);
        let template = match found {
            Some(template) => template,
            None => {
                {
                    // One preparation per image at a time.
                    let mut states = self.states.lock().unwrap();
                    if states.get(&name) == Some(&TemplateState::Preparing) {
                        return Ok(TemplateState::Preparing);
                    }
                    states.insert(name.clone(), TemplateState::Preparing);
                }
                let source = match &self.source {
                    Source::Given(image) => Ok(image.clone()),
                    Source::Built {
                        registry,
                        prefix,
                        bundle,
                        services,
                    } => {
                        super::images::build_and_push(
                            image,
                            &super::images::Contents {
                                bundle,
                                services: &present(services),
                            },
                            registry,
                            prefix,
                            token,
                        )
                        .await
                    }
                };
                let source = match source {
                    Ok(source) => source,
                    Err(reason) => {
                        let failed = TemplateState::Failed { reason };
                        self.states.lock().unwrap().insert(name, failed.clone());
                        return Ok(failed);
                    }
                };
                tracing::info!(
                    template = %name, %image, %source,
                    "registering a DigitalOcean template for this image"
                );
                match client
                    .create_template(&name, &self.base_template, &source)
                    .await
                {
                    Ok(template) => template,
                    Err(error) => {
                        self.states.lock().unwrap().remove(&name);
                        return Err(error);
                    }
                }
            }
        };
        let state = state_of(&template);
        if let TemplateState::Ready { .. } = state {
            // Named for what is here now, so it holds what is here now.
            self.ready.lock().unwrap().insert(
                digest_of(image).map_err(DoError::Invalid)?.to_string(),
                Known {
                    name: name.clone(),
                    services,
                },
            );
        }
        self.states.lock().unwrap().insert(name, state.clone());
        Ok(state)
    }
}

fn state_of(template: &Template) -> TemplateState {
    match &template.status {
        TemplateStatus::Ready => TemplateState::Ready {
            template_id: template.template_id.clone(),
        },
        TemplateStatus::Failed => TemplateState::Failed {
            reason: format!("template {} is FAILED", template.name),
        },
        _ => TemplateState::Building {
            template_id: template.template_id.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: &str = "ghcr.io/example/sandbox@sha256:1111111111111111222222222222222222222222222222222222222222222222";
    const SNAPSHOT: &str = "localhost/tracon-snapshot-app@sha256:3333333333333333444444444444444444444444444444444444444444444444";

    fn built() -> Source {
        Source::Built {
            registry: "registry.digitalocean.com/acme".into(),
            prefix: "tracon".into(),
            bundle: "/state/bundles/0.45.0/amd64".into(),
            services: Vec::new(),
        }
    }

    /// A release moves the harness bundle, and a browser bundle arriving or
    /// moving changes what the sandbox holds: each is a template of its own,
    /// so no session runs an older release's binaries, and the one it
    /// supersedes is stale.
    #[test]
    fn a_built_template_is_named_for_what_goes_into_it() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = |version: &str| {
            let dir = tmp.path().join("bundles").join(version);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("manifest.json"),
                format!("{{\"version\":\"{version}\"}}"),
            )
            .unwrap();
            dir
        };
        let browser = tmp.path().join("browser");
        let source = |bundle: PathBuf| Source::Built {
            registry: "r".into(),
            prefix: "tracon".into(),
            bundle,
            services: vec![(browser.clone(), "/opt/tracon-services/browser".into())],
        };
        let old = Templates::new(source(bundle("0.48.1")), "sandbox", "tracon-tabcd1234").unwrap();
        let new = Templates::new(source(bundle("0.49.0")), "sandbox", "tracon-tabcd1234").unwrap();
        let before = new.name_for(SNAPSHOT).unwrap();
        assert!(
            before.starts_with("tracon-tabcd1234-")
                && before.len() == "tracon-tabcd1234-".len() + 16
        );
        assert_ne!(old.name_for(SNAPSHOT).unwrap(), before);
        assert_ne!(new.name_for(IMAGE).unwrap(), before);
        assert_eq!(new.current_services(), Some(Vec::new()));

        // The browser's bundle arrives: a template that holds it.
        std::fs::create_dir_all(&browser).unwrap();
        std::fs::write(browser.join("manifest.json"), "{\"version\":\"157\"}").unwrap();
        let with_browser = new.name_for(SNAPSHOT).unwrap();
        assert_ne!(with_browser, before);
        assert_eq!(new.current_services(), Some(vec!["browser".to_string()]));
        let wanted: HashSet<String> = [with_browser.clone()].into();
        assert!(new.is_stale(&before, &wanted));
        // And the same inputs, the same name.
        assert_eq!(new.name_for(SNAPSHOT).unwrap(), with_browser);
        // The operator's image is named for its digest alone.
        let given = Templates::new(Source::Given(IMAGE.into()), "sandbox", "t").unwrap();
        assert_eq!(given.current_services(), None);
    }

    /// Seen ready, as `ensure` records a template it finds ready.
    fn seen_ready(t: &Templates, image: &str) -> String {
        let name = t.name_for(image).unwrap();
        t.states.lock().unwrap().insert(
            name.clone(),
            TemplateState::Ready {
                template_id: name.clone(),
            },
        );
        t.ready.lock().unwrap().insert(
            digest_of(image).unwrap().to_string(),
            Known {
                name: name.clone(),
                services: t.current_services(),
            },
        );
        name
    }

    /// A bundle arriving renames the image's template. Sandboxes keep
    /// starting from the one seen ready, which is kept from the sweep, until
    /// the new one is ready; each is judged by what its own template holds.
    #[test]
    fn the_last_ready_template_serves_while_a_new_one_builds() {
        let tmp = tempfile::tempdir().unwrap();
        let browser = tmp.path().join("browser");
        let t = Templates::new(
            Source::Built {
                registry: "r".into(),
                prefix: "tracon".into(),
                bundle: tmp.path().join("bundle"),
                services: vec![(browser.clone(), "/opt/tracon-services/browser".into())],
            },
            "coding-base",
            "tracon-tabcd1234",
        )
        .unwrap();
        assert!(t.ready(SNAPSHOT).is_err(), "nothing seen ready yet");
        let old = seen_ready(&t, SNAPSHOT);
        assert_eq!(t.ready(SNAPSHOT).unwrap(), old);

        std::fs::create_dir_all(&browser).unwrap();
        let new = t.name_for(SNAPSHOT).unwrap();
        assert_ne!(new, old);
        assert_eq!(
            t.ready(SNAPSHOT).unwrap(),
            old,
            "the old one serves meanwhile"
        );
        assert_eq!(t.kept(SNAPSHOT), [new.clone(), old.clone()]);
        assert_eq!(t.services_of(&old), Some(Vec::new()));
        // Another image has nothing to fall back on.
        assert!(t.ready(IMAGE).is_err());

        assert_eq!(seen_ready(&t, SNAPSHOT), new);
        assert_eq!(t.ready(SNAPSHOT).unwrap(), new);
        assert_eq!(t.kept(SNAPSHOT), std::slice::from_ref(&new));
        assert!(t.is_stale(&old, &t.kept(SNAPSHOT).into_iter().collect()));
        assert_eq!(t.services_of(&new), Some(vec!["browser".to_string()]));
    }

    #[test]
    fn a_service_is_named_by_where_its_bundle_is() {
        assert_eq!(
            service_name("/opt/tracon-services/browser"),
            Some("browser")
        );
        assert_eq!(service_name("/opt/tracon-services/"), None);
        assert_eq!(service_name("/opt/tracon-services/a/b"), None);
        assert_eq!(service_name("/elsewhere/browser"), None);
    }

    #[test]
    fn a_template_is_named_for_its_node_and_image_digest() {
        let t = Templates::new(
            Source::Given(IMAGE.into()),
            "coding-base",
            "tracon-tabcd1234",
        )
        .unwrap();
        let name = t.name_for(IMAGE).unwrap();
        assert_eq!(name, "tracon-tabcd1234-1111111111111111");
        let wanted: HashSet<String> = [name.clone()].into();
        assert!(t.is_stale("tracon-tabcd1234-3333333333333333", &wanted));
        assert!(!t.is_stale(&name, &wanted));
        assert!(!t.is_stale("tracon-tffff0000-3333333333333333", &wanted));
        assert!(!t.is_stale("hand-made", &wanted));
        assert_eq!(
            t.ready(IMAGE).unwrap_err(),
            TemplateState::Unknown.describe()
        );
    }

    #[test]
    fn a_given_image_serves_alone_and_a_built_source_serves_any_pinned_image() {
        let given = Templates::new(Source::Given(IMAGE.into()), "coding-base", "t").unwrap();
        assert!(given.serves(IMAGE));
        assert!(!given.serves(SNAPSHOT));
        assert_eq!(
            given.default_image(Some(SNAPSHOT.into())).as_deref(),
            Some(IMAGE)
        );
        let built = Templates::new(built(), "coding-base", "t").unwrap();
        assert!(built.serves(SNAPSHOT));
        assert!(!built.serves("localhost/x:latest"));
        assert_eq!(
            built.default_image(Some(SNAPSHOT.into())).as_deref(),
            Some(SNAPSHOT)
        );
        assert!(built.ready("localhost/x:latest").is_err());
    }

    #[test]
    fn an_image_without_a_digest_is_refused() {
        assert!(Templates::new(
            Source::Given("ghcr.io/example/sandbox:1".into()),
            "sandbox",
            "t"
        )
        .is_err());
        assert!(digest_of("x@sha256:abc").is_err());
        assert!(digest_of("x@md5:abc").is_err());
    }
}
