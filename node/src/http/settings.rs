//! The configuration the interface may read and write.
//!
//! An allowlist rather than the whole of `node.toml`, for two reasons. The
//! file carries things that are not settings — the mesh's hub, the runtime
//! kind — and writing it is a full re-serialise, so every key the interface
//! does not understand would be a key it could silently drop. What is here is
//! what an operator sets while standing a node up.

use serde_json::{json, Value};

use crate::config::Config;

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
        },
        "review": {
            "max_diff_lines": cfg.review.max_diff_lines,
            "max_files": cfg.review.max_files,
        },
        "gateway": { "allow_hosts": cfg.gateway.allow_hosts },
        "publish": { "gh": cfg.publish.gh, "glab": cfg.publish.glab, "git": cfg.publish.git },
        "boundary": { "podman": cfg.boundary.podman },
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
                        "id" => set_string(&mut cfg.harness.id, v, "harness.id", &mut changed)?,
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
            other => return Err(unknown(other)),
        }
    }
    Ok(changed)
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
                "gateway",
                "harness",
                "node_name",
                "publish",
                "readonly",
                "review",
                "session",
            ]
        );
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
