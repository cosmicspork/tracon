//! The providers file the node stages for a session: where the gateway serves
//! each provider, the variable holding the session's key for it, and the
//! models declared under it. The harness holds no catalogue of its own; this
//! file, written per session, is all it knows.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Provider {
    pub name: String,
    pub shape: String,
    /// The gateway's base URL for it, without a version suffix.
    pub base_url: String,
    /// The environment variable holding the session's key for it.
    pub key_env: String,
    #[serde(default)]
    pub models: Vec<Model>,
}

#[derive(Debug, Deserialize)]
pub struct Model {
    pub id: String,
    /// Tokens; zero when the node declared none.
    #[serde(default)]
    pub context: u64,
}

/// What the client needs for one `provider/model`.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolved {
    pub base_url: String,
    pub model: String,
    pub key_env: String,
    pub context_window: Option<u64>,
}

pub fn load(path: &Path) -> anyhow::Result<Vec<Provider>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

/// Finds `provider/model` among the providers. Only the OpenAI shape is
/// spoken here; a model under another is refused by name, not sent where its
/// wire is not understood.
pub fn resolve(providers: &[Provider], model: &str) -> anyhow::Result<Resolved> {
    let (name, id) = model
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("model `{model}` must be provider/model"))?;
    let provider = providers
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| anyhow::anyhow!("no provider `{name}` is wired to this session"))?;
    if provider.shape != "openai" {
        anyhow::bail!(
            "provider `{name}` speaks `{}`; tracon-agent speaks the OpenAI shape only",
            provider.shape
        );
    }
    let context = provider
        .models
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.context)
        .filter(|c| *c > 0);
    Ok(Resolved {
        base_url: format!("{}/v1", provider.base_url.trim_end_matches('/')),
        model: id.to_string(),
        key_env: provider.key_env.clone(),
        context_window: context,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers() -> Vec<Provider> {
        serde_json::from_value(serde_json::json!([
            {"name": "local", "shape": "openai", "base_url": "http://tracon-gw:7421/model/local",
             "key_env": "TRACON_PROVIDER_KEY_LOCAL", "models": [{"id": "qwen3-8b", "context": 32768}]},
            {"name": "anthropic", "shape": "anthropic", "base_url": "http://tracon-gw:7421/model/anthropic",
             "key_env": "TRACON_PROVIDER_KEY_ANTHROPIC"}
        ]))
        .unwrap()
    }

    #[test]
    fn resolves_a_declared_model() {
        assert_eq!(
            resolve(&providers(), "local/qwen3-8b").unwrap(),
            Resolved {
                base_url: "http://tracon-gw:7421/model/local/v1".into(),
                model: "qwen3-8b".into(),
                key_env: "TRACON_PROVIDER_KEY_LOCAL".into(),
                context_window: Some(32768),
            }
        );
        // Undeclared models still run; only the window is unknown.
        assert_eq!(
            resolve(&providers(), "local/other").unwrap().context_window,
            None
        );
    }

    #[test]
    fn refuses_what_it_cannot_speak() {
        assert!(resolve(&providers(), "qwen3-8b").is_err());
        assert!(resolve(&providers(), "nope/x").is_err());
        let e = resolve(&providers(), "anthropic/claude")
            .unwrap_err()
            .to_string();
        assert!(e.contains("OpenAI shape only"), "{e}");
    }
}
