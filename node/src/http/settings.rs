//! The configuration the interface may read and write.
//!
//! An allowlist rather than the whole of `node.toml`, for two reasons. The
//! file carries things that are not settings — the mesh's hub, the runtime
//! kind — and writing it is a full re-serialise, so every key the interface
//! does not understand would be a key it could silently drop. What is here is
//! what an operator sets while standing a node up.

use serde_json::{json, Value};

use crate::config::{Config, ModelDecl};

/// Config as the interface sees it: the writable keys, plus a little context
/// it needs to explain itself. No secrets — the broker owns those, and none
/// of them live here.
pub fn config_view(cfg: &Config) -> Value {
    json!({
        "node_name": cfg.node_name,
        "harness": {
            "id": cfg.harness.id,
            "version": cfg.harness.version,
            "tools": cfg.harness.tools,
        },
        "session": {
            "budget_tokens": cfg.session.budget_tokens,
            "permission_timeout_secs": cfg.session.permission_timeout_secs,
            "default_channel": cfg.session.default_channel,
        },
        "review": {
            "max_diff_lines": cfg.review.max_diff_lines,
            "max_files": cfg.review.max_files,
        },
        "gateway": { "allow_hosts": cfg.gateway.allow_hosts },
        "publish": { "gh": cfg.publish.gh, "glab": cfg.publish.glab, "git": cfg.publish.git },
        "boundary": { "podman": cfg.boundary.podman },
        "external": {
            "enabled": cfg.external.enabled,
            "idle_timeout_secs": cfg.external.idle_timeout_secs,
        },
        // What a session's harness may load, beyond what the node decides.
        // The per-channel half — skills, instructions, agents — lives in the
        // store, behind `/api/manifest`.
        "launch": { "plugins": cfg.launch.plugins },
        // The catalogue, per provider: what the picker offers and what a
        // declaring harness is told. The rest of a provider entry — its
        // credential name, upstream, shape — is shown so the models can be
        // read in context, and is written only in node.toml: it decides where
        // a credential is sent.
        "providers": cfg.providers.iter().map(|(name, p)| (name.clone(), json!({
            "shape": p.shape,
            "upstream": p.upstream,
            "credential": p.credential,
            "login": p.login,
            "models": p.models,
        }))).collect::<serde_json::Map<String, Value>>(),
        "supervision": {
            "checks": cfg.supervision.checks,
            "timeout_secs": cfg.supervision.timeout_secs,
            "dependency_inputs": cfg.supervision.dependency_inputs,
            "max_snapshot_bytes": cfg.supervision.max_snapshot_bytes,
        },
        // Read-only: shown so the pane can say what this node is, set by
        // enrolling or by the runtime it was started under.
        "readonly": {
            "hub_url": cfg.mesh.hub_url,
            "runtime": cfg.runtime.kind,
            "config_path": Config::config_path().to_string_lossy(),
        },
    })
}

/// Apply a patch of the allowlisted keys, returning what changed.
///
/// Unknown keys are refused rather than ignored: a settings form that silently
/// drops what it was given is worse than one that says it cannot.
pub fn apply(cfg: &mut Config, patch: &Value) -> Result<Vec<String>, String> {
    let Some(obj) = patch.as_object() else {
        return Err("expected an object".into());
    };
    let mut changed = Vec::new();
    for (key, value) in obj {
        match key.as_str() {
            "node_name" => set_string(&mut cfg.node_name, value, "node_name", &mut changed)?,
            "harness" => {
                for (k, v) in object(value, "harness")? {
                    match k.as_str() {
                        "id" => {
                            // Written here, refused at the next start: an id
                            // the node has no adapter for is a crash loop
                            // under the service, so it never reaches the file.
                            let id = v.as_str().unwrap_or_default();
                            if id == crate::adapter::RETIRED {
                                return Err(crate::adapter::RETIRED_MESSAGE.into());
                            }
                            if v.is_string() && !crate::adapter::KNOWN.contains(&id) {
                                return Err(format!(
                                    "`harness.id` must be one of {}",
                                    crate::adapter::KNOWN.join(", ")
                                ));
                            }
                            set_string(&mut cfg.harness.id, v, "harness.id", &mut changed)?
                        }
                        "version" => set_string(
                            &mut cfg.harness.version,
                            v,
                            "harness.version",
                            &mut changed,
                        )?,
                        "tools" => {
                            let list = string_list(v, "harness.tools")?;
                            if cfg.harness.tools != list {
                                cfg.harness.tools = list;
                                changed.push("harness.tools".into());
                            }
                        }
                        other => return Err(unknown(&format!("harness.{other}"))),
                    }
                }
            }
            "session" => {
                for (k, v) in object(value, "session")? {
                    match k.as_str() {
                        "budget_tokens" => set_i64(
                            &mut cfg.session.budget_tokens,
                            v,
                            "session.budget_tokens",
                            &mut changed,
                        )?,
                        "permission_timeout_secs" => set_u64(
                            &mut cfg.session.permission_timeout_secs,
                            v,
                            "session.permission_timeout_secs",
                            &mut changed,
                        )?,
                        "default_channel" => set_string(
                            &mut cfg.session.default_channel,
                            v,
                            "session.default_channel",
                            &mut changed,
                        )?,
                        other => return Err(unknown(&format!("session.{other}"))),
                    }
                }
            }
            "review" => {
                for (k, v) in object(value, "review")? {
                    match k.as_str() {
                        "max_diff_lines" => set_i64(
                            &mut cfg.review.max_diff_lines,
                            v,
                            "review.max_diff_lines",
                            &mut changed,
                        )?,
                        "max_files" => set_usize(
                            &mut cfg.review.max_files,
                            v,
                            "review.max_files",
                            &mut changed,
                        )?,
                        other => return Err(unknown(&format!("review.{other}"))),
                    }
                }
            }
            "gateway" => {
                for (k, v) in object(value, "gateway")? {
                    match k.as_str() {
                        "allow_hosts" => {
                            let list = string_list(v, "gateway.allow_hosts")?;
                            if cfg.gateway.allow_hosts != list {
                                cfg.gateway.allow_hosts = list;
                                changed.push("gateway.allow_hosts".into());
                            }
                        }
                        other => return Err(unknown(&format!("gateway.{other}"))),
                    }
                }
            }
            "publish" => {
                for (k, v) in object(value, "publish")? {
                    match k.as_str() {
                        "gh" => set_string(&mut cfg.publish.gh, v, "publish.gh", &mut changed)?,
                        "glab" => {
                            set_string(&mut cfg.publish.glab, v, "publish.glab", &mut changed)?
                        }
                        "git" => set_string(&mut cfg.publish.git, v, "publish.git", &mut changed)?,
                        other => return Err(unknown(&format!("publish.{other}"))),
                    }
                }
            }
            "boundary" => {
                for (k, v) in object(value, "boundary")? {
                    match k.as_str() {
                        "podman" => set_string(
                            &mut cfg.boundary.podman,
                            v,
                            "boundary.podman",
                            &mut changed,
                        )?,
                        other => return Err(unknown(&format!("boundary.{other}"))),
                    }
                }
            }
            "supervision" => {
                for (k, v) in object(value, "supervision")? {
                    match k.as_str() {
                        "checks" => {
                            let checks = string_list(v, "supervision.checks")?;
                            if cfg.supervision.checks != checks {
                                cfg.supervision.checks = checks;
                                changed.push("supervision.checks".into());
                            }
                        }
                        "timeout_secs" => set_u64(
                            &mut cfg.supervision.timeout_secs,
                            v,
                            "supervision.timeout_secs",
                            &mut changed,
                        )?,
                        "dependency_inputs" => {
                            let inputs = string_map(v, "supervision.dependency_inputs")?;
                            if cfg.supervision.dependency_inputs != inputs {
                                cfg.supervision.dependency_inputs = inputs;
                                changed.push("supervision.dependency_inputs".into());
                            }
                        }
                        "max_snapshot_bytes" => set_u64(
                            &mut cfg.supervision.max_snapshot_bytes,
                            v,
                            "supervision.max_snapshot_bytes",
                            &mut changed,
                        )?,
                        other => return Err(unknown(&format!("supervision.{other}"))),
                    }
                }
            }
            "external" => {
                for (k, v) in object(value, "external")? {
                    match k.as_str() {
                        "enabled" => set_bool(
                            &mut cfg.external.enabled,
                            v,
                            "external.enabled",
                            &mut changed,
                        )?,
                        "idle_timeout_secs" => set_u64(
                            &mut cfg.external.idle_timeout_secs,
                            v,
                            "external.idle_timeout_secs",
                            &mut changed,
                        )?,
                        other => return Err(unknown(&format!("external.{other}"))),
                    }
                }
            }
            "launch" => {
                for (k, v) in object(value, "launch")? {
                    match k.as_str() {
                        "plugins" => {
                            let plugins = string_list(v, "launch.plugins")?;
                            if cfg.launch.plugins != plugins {
                                cfg.launch.plugins = plugins;
                                changed.push("launch.plugins".into());
                            }
                        }
                        other => return Err(unknown(&format!("launch.{other}"))),
                    }
                }
            }
            "providers" => {
                for (name, entry) in object(value, "providers")? {
                    let Some(provider) = cfg.providers.get_mut(name) else {
                        return Err(format!(
                            "`providers.{name}` is not a provider on this node; add it to node.toml first"
                        ));
                    };
                    for (k, v) in object(entry, &format!("providers.{name}"))? {
                        match k.as_str() {
                            "models" => {
                                let key = format!("providers.{name}.models");
                                let models = model_list(v, &key)?;
                                if provider.models != models {
                                    provider.models = models;
                                    changed.push(key);
                                }
                            }
                            other => return Err(unknown(&format!("providers.{name}.{other}"))),
                        }
                    }
                }
            }
            other => return Err(unknown(other)),
        }
    }
    Ok(changed)
}

fn set_bool(
    slot: &mut bool,
    v: &Value,
    key: &str,
    changed: &mut Vec<String>,
) -> Result<(), String> {
    let next = v
        .as_bool()
        .ok_or_else(|| format!("`{key}` expects true or false"))?;
    if *slot != next {
        *slot = next;
        changed.push(key.to_string());
    }
    Ok(())
}

fn unknown(key: &str) -> String {
    format!("`{key}` is not a setting this interface writes; edit node.toml directly")
}

fn object<'a>(v: &'a Value, key: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    v.as_object()
        .ok_or_else(|| format!("`{key}` expects an object"))
}

fn string_list(v: &Value, key: &str) -> Result<Vec<String>, String> {
    let items = v
        .as_array()
        .ok_or_else(|| format!("`{key}` expects a list of strings"))?;
    items
        .iter()
        .map(|i| {
            i.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("`{key}` expects a list of strings"))
        })
        .collect()
}

/// A catalogue as the form sends it: every entry needs an id, ids are unique,
/// and the limits are whole numbers. An empty list is accepted and means the
/// operator declared none — a declaring harness then offers nothing under
/// this provider, which is what `models = []` in node.toml means too.
fn model_list(v: &Value, key: &str) -> Result<Vec<ModelDecl>, String> {
    let items = v
        .as_array()
        .ok_or_else(|| format!("`{key}` expects a list of models"))?;
    let mut out: Vec<ModelDecl> = Vec::with_capacity(items.len());
    for item in items {
        let model: ModelDecl = serde_json::from_value(item.clone())
            .map_err(|e| format!("`{key}` has a model this node cannot read: {e}"))?;
        let id = model.id.trim();
        if id.is_empty() {
            return Err(format!("`{key}` has a model without an id"));
        }
        if out.iter().any(|m| m.id == id) {
            return Err(format!("`{key}` names `{id}` twice"));
        }
        out.push(ModelDecl {
            id: id.to_string(),
            name: model.name.trim().to_string(),
            ..model
        });
    }
    Ok(out)
}

fn string_map(v: &Value, key: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
    object(v, key)?
        .iter()
        .map(|(name, value)| {
            value
                .as_str()
                .map(|value| (name.clone(), value.to_string()))
                .ok_or_else(|| format!("`{key}` expects string values"))
        })
        .collect()
}

fn set_string(
    slot: &mut String,
    v: &Value,
    key: &str,
    changed: &mut Vec<String>,
) -> Result<(), String> {
    let next = v
        .as_str()
        .ok_or_else(|| format!("`{key}` expects a string"))?;
    if slot != next {
        *slot = next.to_string();
        changed.push(key.to_string());
    }
    Ok(())
}

fn set_u64(slot: &mut u64, v: &Value, key: &str, changed: &mut Vec<String>) -> Result<(), String> {
    let next = whole(v, key)?;
    if *slot != next {
        *slot = next;
        changed.push(key.to_string());
    }
    Ok(())
}

fn whole(v: &Value, key: &str) -> Result<u64, String> {
    v.as_u64()
        .ok_or_else(|| format!("`{key}` expects a whole number that is not negative"))
}

fn set_i64(slot: &mut i64, v: &Value, key: &str, changed: &mut Vec<String>) -> Result<(), String> {
    let next = whole(v, key)? as i64;
    if *slot != next {
        *slot = next;
        changed.push(key.to_string());
    }
    Ok(())
}

fn set_usize(
    slot: &mut usize,
    v: &Value,
    key: &str,
    changed: &mut Vec<String>,
) -> Result<(), String> {
    let next = whole(v, key)? as usize;
    if *slot != next {
        *slot = next;
        changed.push(key.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_carries_the_writable_keys_and_no_secrets() {
        let cfg = Config::default();
        let v = config_view(&cfg);
        assert!(v["harness"]["id"].is_string());
        assert!(v["boundary"]["podman"].is_string());
        assert!(v["readonly"]["config_path"].is_string());
        // The view is an allowlist, so assert the whole shape rather than
        // hunting for substrings: anything new here is a deliberate decision
        // that has to update this list, and nothing from the broker, the
        // identity, or the policy key is a setting.
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "boundary",
                "external",
                "gateway",
                "harness",
                "launch",
                "node_name",
                "providers",
                "publish",
                "readonly",
                "review",
                "session",
                "supervision",
            ]
        );
        // The catalogue is there per provider, with the entry's context and
        // never a credential value.
        assert_eq!(v["providers"]["anthropic"]["shape"], "anthropic");
        assert!(v["providers"]["anthropic"]["models"].is_array());
        assert!(v["providers"]["anthropic"].get("key").is_none());
    }

    /// The catalogue is written per provider: ids trimmed and unique, limits
    /// whole numbers, and the rest of the provider entry untouched.
    #[test]
    fn provider_models_are_written_and_the_rest_of_the_entry_is_not() {
        let mut cfg = Config::default();
        let changed = apply(
            &mut cfg,
            &json!({ "providers": { "openai-codex": { "models": [
                { "id": " gpt-6 ", "name": "GPT-6", "context": 1_000_000, "output": 200_000, "reasoning": true },
                { "id": "gpt-6-mini" }
            ] } } }),
        )
        .unwrap();
        assert_eq!(changed, ["providers.openai-codex.models"]);
        let models = &cfg.providers["openai-codex"].models;
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-6");
        assert_eq!(models[0].context, 1_000_000);
        assert_eq!(models[1].label(), "gpt-6-mini");
        assert_eq!(
            cfg.providers["openai-codex"].upstream,
            "https://chatgpt.com/backend-api"
        );

        // Re-applying is a no-op; an empty list is a decision, not an error.
        let again = apply(&mut cfg, &json!({ "providers": { "openai-codex": { "models": [
            { "id": "gpt-6", "name": "GPT-6", "context": 1_000_000, "output": 200_000, "reasoning": true },
            { "id": "gpt-6-mini" }
        ] } } })).unwrap();
        assert!(again.is_empty());
        let cleared = apply(
            &mut cfg,
            &json!({ "providers": { "anthropic": { "models": [] } } }),
        )
        .unwrap();
        assert_eq!(cleared, ["providers.anthropic.models"]);
        assert!(cfg.providers["anthropic"].models.is_empty());

        let dup = apply(
            &mut cfg,
            &json!({ "providers": { "anthropic": { "models": [{ "id": "a" }, { "id": "a" }] } } }),
        );
        assert!(dup.unwrap_err().contains("twice"));
        let blank = apply(
            &mut cfg,
            &json!({ "providers": { "anthropic": { "models": [{ "id": "  " }] } } }),
        );
        assert!(blank.unwrap_err().contains("without an id"));
        let nobody = apply(
            &mut cfg,
            &json!({ "providers": { "nobody": { "models": [] } } }),
        );
        assert!(nobody.unwrap_err().contains("not a provider"));
        let upstream = apply(
            &mut cfg,
            &json!({ "providers": { "anthropic": { "upstream": "http://x" } } }),
        );
        assert!(upstream
            .unwrap_err()
            .contains("providers.anthropic.upstream"));
    }

    #[test]
    fn a_patch_applies_only_what_changed() {
        let mut cfg = Config::default();
        let before = cfg.harness.id.clone();
        let changed = apply(
            &mut cfg,
            &json!({ "harness": { "id": "claude" }, "session": { "budget_tokens": 5_000_000 } }),
        )
        .unwrap();
        assert_eq!(cfg.harness.id, "claude");
        assert_eq!(cfg.session.budget_tokens, 5_000_000);
        assert!(changed.contains(&"harness.id".to_string()));
        assert!(changed.contains(&"session.budget_tokens".to_string()));
        assert_ne!(before, cfg.harness.id);

        // Re-applying the same values changes nothing, so the interface can
        // say honestly whether a restart is owed.
        let again = apply(&mut cfg, &json!({ "harness": { "id": "claude" } })).unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn a_harness_the_node_cannot_run_is_never_written() {
        let mut cfg = Config::default();
        let err = apply(&mut cfg, &json!({ "harness": { "id": "omp" } })).unwrap_err();
        assert!(err.contains("was removed"), "{err}");
        let err = apply(&mut cfg, &json!({ "harness": { "id": "aider" } })).unwrap_err();
        assert!(err.contains("opencode"), "{err}");
        assert_eq!(cfg.harness.id, Config::default().harness.id);
    }

    #[test]
    fn the_default_channel_is_a_session_setting() {
        let mut cfg = Config::default();
        assert_eq!(config_view(&cfg)["session"]["default_channel"], "");
        let changed = apply(
            &mut cfg,
            &json!({ "session": { "default_channel": "work" } }),
        )
        .unwrap();
        assert_eq!(changed, vec!["session.default_channel"]);
        assert_eq!(cfg.session.default_channel, "work");
        assert_eq!(config_view(&cfg)["session"]["default_channel"], "work");
        // Clearing it is a change too.
        let changed = apply(&mut cfg, &json!({ "session": { "default_channel": "" } })).unwrap();
        assert_eq!(changed, vec!["session.default_channel"]);
    }

    #[test]
    fn operator_can_set_candidate_check_identities() {
        let mut cfg = Config::default();
        let changed = apply(
            &mut cfg,
            &json!({
                "supervision": {
                    "checks": ["cargo test --locked"],
                    "timeout_secs": 120,
                    "dependency_inputs": { "lockfile": "sha256:abc" },
                    "max_snapshot_bytes": 1048576,
                }
            }),
        )
        .unwrap();
        assert_eq!(
            changed,
            vec![
                "supervision.checks",
                "supervision.dependency_inputs",
                "supervision.max_snapshot_bytes",
                "supervision.timeout_secs",
            ]
        );
        assert_eq!(
            cfg.supervision.checks,
            vec!["cargo test --locked".to_string()]
        );
        assert_eq!(
            cfg.supervision.dependency_inputs.get("lockfile"),
            Some(&"sha256:abc".to_string())
        );
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_dropped() {
        let mut cfg = Config::default();
        let err = apply(&mut cfg, &json!({ "mesh": { "hub_url": "https://x" } })).unwrap_err();
        assert!(err.contains("mesh"), "{err}");
        let err = apply(&mut cfg, &json!({ "harness": { "nope": 1 } })).unwrap_err();
        assert!(err.contains("harness.nope"), "{err}");
    }

    #[test]
    fn a_wrong_type_is_refused_with_the_key_named() {
        let mut cfg = Config::default();
        let err = apply(&mut cfg, &json!({ "session": { "budget_tokens": "lots" } })).unwrap_err();
        assert!(err.contains("session.budget_tokens"), "{err}");
        let err = apply(&mut cfg, &json!({ "gateway": { "allow_hosts": "a" } })).unwrap_err();
        assert!(err.contains("gateway.allow_hosts"), "{err}");
    }
}
