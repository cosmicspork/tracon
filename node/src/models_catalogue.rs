//! OpenCode's own model catalogue (`https://models.opencode.ai/api.json`),
//! fetched and cached so `config::default_models` can offer real defaults for
//! any provider name the catalogue knows, not only the three built-in ones.
//! See `docs/reference/opencode-v1.18.30/providers.md` §7.1 for the source
//! this mirrors, and §1.1 for the model fields it documents (`id`, `name`,
//! `limit.context`, `limit.output`, `reasoning`, `attachment` — the same
//! fields `config::ModelDecl` already carries).
//!
//! Modeled structurally on `providers.rs`'s OAuth-refresh loop
//! (`REFRESH_TICK_SECS`/`refresh_loop`, `tokio::time::interval`), but this is
//! a background fetch of external data, not a credential renewal: a failed
//! fetch never blanks the cache, it just leaves the last good copy in place
//! — on disk and in memory — and logs a warning.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Config, ModelDecl};

/// Where OpenCode's own catalogue is served from.
const CATALOGUE_URL: &str = "https://models.opencode.ai/api.json";
/// The fetch host, checked against `[gateway] allow_hosts` before every
/// fetch. This is a node-process-direct request outside the
/// gateway/tinyproxy enforcement paths (`node/src/gateway/proxy.rs`), so it
/// holds itself to the same egress allowlist an operator audits against
/// rather than being a silent hole in it.
const CATALOGUE_HOST: &str = "models.opencode.ai";
/// How often the background loop refreshes the cache.
pub const REFRESH_TICK_SECS: u64 = 24 * 60 * 60;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// What this node has learned about OpenCode's catalogue: when, and what
/// models each provider key it named declares.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Catalogue {
    /// When this was fetched, in epoch ms. `None` means never — this node is
    /// running on the hardcoded fallback tier alone.
    pub fetched_ms: Option<i64>,
    pub providers: BTreeMap<String, Vec<ModelDecl>>,
}

static CACHE: OnceLock<RwLock<Option<Catalogue>>> = OnceLock::new();

fn cell() -> &'static RwLock<Option<Catalogue>> {
    CACHE.get_or_init(|| RwLock::new(load_from_disk()))
}

fn cache_path() -> PathBuf {
    Config::state_dir().join("models_catalogue.json")
}

fn load_from_disk() -> Option<Catalogue> {
    let path = cache_path();
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&text) {
        Ok(catalogue) => Some(catalogue),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "model catalogue cache is corrupt; ignoring");
            None
        }
    }
}

/// Atomic write: a temp file beside the target, renamed into place — the
/// same shape as `Broker::save_at` (`node/src/broker/mod.rs:296`).
/// `Config::save`/`save_to` write their TOML directly with no temp file, so
/// they are not actually atomic; the sealed store's pattern is the one this
/// follows for a file a concurrent reader (another process, or this one's own
/// next `cached()` call) could otherwise catch half-written.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::now_v7()));
    std::fs::write(&tmp, bytes)?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// The cached catalogue, whatever this node currently has. Empty
/// (`fetched_ms: None`, no providers) when nothing has ever been fetched or
/// read from disk — never an error, since a node that hasn't fetched yet, or
/// can't, still has to start.
pub fn cached() -> Catalogue {
    cell().read().unwrap().clone().unwrap_or_default()
}

/// This provider's declared models, from the cached catalogue alone. `None`
/// when the catalogue has never been populated, does not know this provider,
/// or declares no models for it — `config::default_models`'s tier 1 asks
/// this first and falls back when it comes back `None`.
pub fn provider_models(provider: &str) -> Option<Vec<ModelDecl>> {
    cell()
        .read()
        .unwrap()
        .as_ref()?
        .providers
        .get(provider)
        .filter(|models| !models.is_empty())
        .cloned()
}

/// Replace the in-memory cache and persist it, so the next process start
/// reads what this one fetched without waiting for its own first tick.
fn install(catalogue: Catalogue) {
    match serde_json::to_vec_pretty(&catalogue) {
        Ok(bytes) => {
            if let Err(error) = write_atomic(&cache_path(), &bytes) {
                tracing::warn!(%error, "could not persist the model catalogue cache");
            }
        }
        Err(error) => tracing::warn!(%error, "could not serialize the model catalogue cache"),
    }
    *cell().write().unwrap() = Some(catalogue);
}

/// Seed the in-memory cache directly, bypassing disk and the network. For
/// integration tests only, the same convention as
/// `Manager::register_tool_token_for_test` (`node/src/session/mod.rs`).
pub fn install_for_test(catalogue: Catalogue) {
    *cell().write().unwrap() = Some(catalogue);
}

/// Raw wire shape of `api.json`: an object keyed by provider id, each
/// carrying a `models` object keyed by model id. Fields this node has no use
/// for (`family`, `cost`, `modalities`, ...) are simply dropped on
/// deserialize.
#[derive(Deserialize, Default)]
struct RawProvider {
    #[serde(default)]
    models: BTreeMap<String, RawModel>,
}

#[derive(Deserialize, Default)]
struct RawModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    attachment: bool,
    #[serde(default)]
    limit: RawLimit,
}

#[derive(Deserialize, Default)]
struct RawLimit {
    #[serde(default)]
    context: u64,
    #[serde(default)]
    output: u64,
}

fn parse_catalogue(text: &str) -> Result<BTreeMap<String, Vec<ModelDecl>>, serde_json::Error> {
    let raw: BTreeMap<String, RawProvider> = serde_json::from_str(text)?;
    Ok(raw
        .into_iter()
        .map(|(provider_id, provider)| {
            let mut models: Vec<ModelDecl> = provider
                .models
                .into_iter()
                .map(|(key, m)| ModelDecl {
                    // The wire model carries its own `id`, which always
                    // matches the map key in practice; the key is a fallback
                    // for a payload that ever omitted it, not the primary
                    // source.
                    id: if m.id.is_empty() { key } else { m.id },
                    name: m.name,
                    context: m.limit.context,
                    output: m.limit.output,
                    reasoning: m.reasoning,
                    attachment: m.attachment,
                })
                .collect();
            models.sort_by(|a, b| a.id.cmp(&b.id));
            (provider_id, models)
        })
        .collect())
}

async fn fetch_and_install(cfg: &Config, client: &reqwest::Client) -> Result<(), String> {
    // The upstream must also pass the egress allowlist, the same property
    // `gateway/model.rs`'s own upstream check enforces
    // (`super::proxy::Allowlist::new(&cfg.gateway.allow_hosts).allows(host)`)
    // — a background fetcher that skipped this would be a silent hole in
    // what a locked-down node's `allow_hosts` promises to audit.
    let allowed = crate::gateway::proxy::Allowlist::new(&cfg.gateway.allow_hosts)
        .map(|allow| allow.allows(CATALOGUE_HOST))
        .unwrap_or(false);
    if !allowed {
        tracing::warn!(
            host = CATALOGUE_HOST,
            "model catalogue host is not on gateway.allow_hosts; skipping fetch, keeping cached copy"
        );
        return Ok(());
    }
    let response = client
        .get(CATALOGUE_URL)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| e.to_string())?;
    let text = response.text().await.map_err(|e| e.to_string())?;
    let providers = parse_catalogue(&text).map_err(|e| e.to_string())?;
    install(Catalogue {
        fetched_ms: Some(crate::store::now_ms()),
        providers,
    });
    Ok(())
}

/// The background refresh loop: modeled on `providers.rs`'s
/// `refresh_loop`/`REFRESH_TICK_SECS` idiom, but for external catalogue data
/// rather than a credential that must be renewed. `tokio::time::interval`'s
/// first tick fires immediately, so this loop's own first iteration is also
/// the one-shot startup fetch — no separate call is needed before spawning
/// it.
pub async fn refresh_loop(cfg: Arc<Config>) {
    // No proxy: this is the node's own fetch, the same reasoning as
    // `ui_bundle::fetch`'s release download — an ambient `HTTPS_PROXY` in a
    // node's environment is for the harness boundary, not for this.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(FETCH_TIMEOUT)
        .build()
        .expect("http client");
    let mut tick = tokio::time::interval(Duration::from_secs(REFRESH_TICK_SECS));
    loop {
        tick.tick().await;
        match fetch_and_install(&cfg, &client).await {
            Ok(()) => tracing::debug!("model catalogue refreshed"),
            Err(error) => {
                tracing::warn!(%error, "model catalogue fetch failed; keeping cached copy")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_fields_and_drops_the_rest() {
        let text = r#"{
            "anthropic": {
                "id": "anthropic",
                "env": ["ANTHROPIC_API_KEY"],
                "npm": "@ai-sdk/anthropic",
                "name": "Anthropic",
                "models": {
                    "claude-sonnet-4-6": {
                        "id": "claude-sonnet-4-6",
                        "name": "Claude Sonnet 4.6",
                        "family": "claude-sonnet",
                        "attachment": true,
                        "reasoning": true,
                        "tool_call": true,
                        "cost": { "input": 3, "output": 15 },
                        "limit": { "context": 1000000, "output": 128000 }
                    },
                    "claude-haiku-4-5": {
                        "name": "Claude Haiku 4.5",
                        "attachment": false,
                        "reasoning": false,
                        "limit": { "context": 200000, "output": 64000 }
                    }
                }
            },
            "empty-provider": { "id": "empty-provider", "models": {} }
        }"#;
        let providers = parse_catalogue(text).unwrap();
        let anthropic = &providers["anthropic"];
        assert_eq!(anthropic.len(), 2);
        // Sorted by id: "claude-haiku-4-5" < "claude-sonnet-4-6".
        assert_eq!(anthropic[0].id, "claude-haiku-4-5");
        assert_eq!(anthropic[0].name, "Claude Haiku 4.5");
        assert_eq!(anthropic[0].context, 200_000);
        assert_eq!(anthropic[0].output, 64_000);
        assert!(!anthropic[0].reasoning);
        assert!(!anthropic[0].attachment);
        assert_eq!(anthropic[1].id, "claude-sonnet-4-6");
        assert_eq!(anthropic[1].context, 1_000_000);
        assert!(anthropic[1].reasoning);
        assert!(anthropic[1].attachment);
        assert!(providers["empty-provider"].is_empty());
    }

    #[test]
    fn a_model_missing_its_own_id_falls_back_to_the_map_key() {
        let text = r#"{
            "custom": { "models": { "my-model": { "name": "Mine" } } }
        }"#;
        let providers = parse_catalogue(text).unwrap();
        assert_eq!(providers["custom"][0].id, "my-model");
    }

    #[test]
    fn round_trips_through_json_the_way_the_disk_cache_does() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".to_string(),
            vec![ModelDecl {
                id: "claude-x".into(),
                name: "Claude X".into(),
                context: 200_000,
                output: 64_000,
                reasoning: true,
                attachment: true,
            }],
        );
        let catalogue = Catalogue {
            fetched_ms: Some(1_700_000_000_000),
            providers,
        };
        let text = serde_json::to_string(&catalogue).unwrap();
        let back: Catalogue = serde_json::from_str(&text).unwrap();
        assert_eq!(back, catalogue);
    }
}
