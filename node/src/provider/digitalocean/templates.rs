//! The custom templates a node's DigitalOcean sandboxes start from: one per
//! local image digest (the pinned stock base, or a repository's prepared
//! snapshot, whose digest moves with its key: base, repository image,
//! lockfiles, recipe), registered as part of preparing that image (a rebase
//! that took about seven minutes when tried), never per session. A session
//! is placed on DigitalOcean only in an image whose template is ready.
//!
//! The template's source is either an image the operator built and named
//! (`[digitalocean] image`, used for every session), or one the node builds
//! from its own image and the harness bundle it verified, and pushes to the
//! operator's registry (`[digitalocean] registry`, see [`super::images`]).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;

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
    },
}

pub struct Templates {
    base_template: String,
    stem: String,
    source: Source,
    /// By template name.
    states: Mutex<HashMap<String, TemplateState>>,
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
        })
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The template named for `image`.
    pub fn name_for(&self, image: &str) -> Result<String, String> {
        let hex = digest_of(image)?.trim_start_matches("sha256:");
        Ok(format!("{}{}", self.stem, &hex[..16]))
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
    /// is none yet.
    pub fn ready(&self, image: &str) -> Result<String, String> {
        if !self.serves(image) {
            return Err(format!("DigitalOcean has no template for {image}"));
        }
        match self.state(image) {
            TemplateState::Ready { .. } => self.name_for(image),
            other => Err(other.describe()),
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
                    } => {
                        super::images::build_and_push(image, bundle, registry, prefix, token).await
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
        }
    }

    #[test]
    fn a_template_is_named_for_its_node_and_image_digest() {
        let t = Templates::new(Source::Given(IMAGE.into()), "sandbox", "tracon-tabcd1234").unwrap();
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
        let given = Templates::new(Source::Given(IMAGE.into()), "sandbox", "t").unwrap();
        assert!(given.serves(IMAGE));
        assert!(!given.serves(SNAPSHOT));
        assert_eq!(
            given.default_image(Some(SNAPSHOT.into())).as_deref(),
            Some(IMAGE)
        );
        let built = Templates::new(built(), "sandbox", "t").unwrap();
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
