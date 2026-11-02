//! The custom template a node's DigitalOcean sandboxes start from: one per
//! image digest, registered as part of preparing the provider (a rebase that
//! took about seven minutes when tried), never per session. A session is
//! placed on DigitalOcean only once its template is ready.

use std::sync::Mutex;

use serde::Serialize;

use crate::do_harness::{Client, DoError, Template, TemplateStatus};

/// Where the template for the configured image stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TemplateState {
    /// Not looked for yet.
    Unknown,
    /// Registered and being built.
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
            Self::Building { .. } => {
                "its template is being built, which takes several minutes once per image".into()
            }
            Self::Ready { .. } => "its template is ready".into(),
            Self::Failed { reason } => format!("its template failed to build: {reason}"),
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

pub struct Templates {
    /// The source image, by digest.
    image: String,
    base_template: String,
    /// The template's name: the node's own and the digest's.
    name: String,
    /// What every template this node registered is named after.
    stem: String,
    state: Mutex<TemplateState>,
}

impl Templates {
    /// `stem` is the node's own (`<prefix>-t<node>`); the name adds the
    /// digest, so a new image is a new template and never a rebuild of the
    /// one sessions start from.
    pub fn new(image: &str, base_template: &str, stem: &str) -> Result<Self, String> {
        let digest = digest_of(image)?;
        let hex = digest.trim_start_matches("sha256:");
        Ok(Templates {
            image: image.to_string(),
            base_template: base_template.to_string(),
            name: format!("{stem}-{}", &hex[..16]),
            stem: format!("{stem}-"),
            state: Mutex::new(TemplateState::Unknown),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub fn digest(&self) -> &str {
        digest_of(&self.image).unwrap_or_default()
    }

    pub fn state(&self) -> TemplateState {
        self.state.lock().unwrap().clone()
    }

    /// The template id to start from, or why there is none yet.
    pub fn ready(&self) -> Result<String, String> {
        match self.state() {
            TemplateState::Ready { template_id } => Ok(template_id),
            other => Err(other.describe()),
        }
    }

    /// Whether `name` is a template this node registered for an image other
    /// than the current one.
    pub fn is_stale(&self, name: &str) -> bool {
        name.starts_with(&self.stem) && name != self.name
    }

    /// Find this image's template, registering it when there is none, and
    /// record where it stands. Cheap once ready: one listing.
    pub async fn ensure(&self, client: &Client) -> Result<TemplateState, DoError> {
        let found = client
            .list_templates()
            .await?
            .into_iter()
            .find(|t| t.name == self.name);
        let template = match found {
            Some(template) => template,
            None => {
                tracing::info!(
                    template = %self.name, image = %self.image,
                    "registering the DigitalOcean template for this image"
                );
                client
                    .create_template(&self.name, &self.base_template, &self.image)
                    .await?
            }
        };
        let state = state_of(&template);
        *self.state.lock().unwrap() = state.clone();
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

    #[test]
    fn a_template_is_named_for_its_node_and_digest() {
        let t = Templates::new(IMAGE, "sandbox", "tracon-tabcd1234").unwrap();
        assert_eq!(t.name(), "tracon-tabcd1234-1111111111111111");
        assert!(t.is_stale("tracon-tabcd1234-3333333333333333"));
        assert!(!t.is_stale(t.name()));
        assert!(!t.is_stale("tracon-tffff0000-3333333333333333"));
        assert!(!t.is_stale("hand-made"));
        assert_eq!(t.ready().unwrap_err(), TemplateState::Unknown.describe());
    }

    #[test]
    fn an_image_without_a_digest_is_refused() {
        assert!(Templates::new("ghcr.io/example/sandbox:1", "sandbox", "t").is_err());
        assert!(digest_of("x@sha256:abc").is_err());
        assert!(digest_of("x@md5:abc").is_err());
    }
}
