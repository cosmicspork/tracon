//! The configuration the interface may read and write.
//!
//! An allowlist rather than the whole of `node.toml`: the file carries things
//! that are not settings (the mesh's hub, the runtime kind), and what is here
//! is what an operator sets while standing a node up. A patch becomes typed
//! operations on the file (`config::edit`), which writes them in place and
//! audits them.

use serde_json::{json, Value};

use crate::config::{Config, KeyPath, ModelDecl, Op};

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
            "approval_expiry_secs": cfg.session.approval_expiry_secs,
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

/// What a patch asks for: the settings it changes, by the names the form
/// uses, and the edit of `node.toml` that makes them so.
#[derive(Debug, Default)]
pub struct Patch {
    pub changed: Vec<String>,
    pub ops: Vec<Op>,
}

impl Patch {
    fn set(&mut self, key: &str, value: impl Into<toml::Value>) -> Result<(), String> {
        let path = KeyPath::parse(key)?;
        self.ops.push(Op::set(path, value));
        self.changed.push(key.to_string());
        Ok(())
    }
}

/// Map a patch of the allowlisted keys onto an edit of `node.toml`,
/// against the configuration `cfg` the file loads as now. Only what differs
/// from `cfg` becomes an operation, so the patch says honestly whether
/// anything changed.
///
/// Unknown keys are refused rather than ignored: a settings form that silently
/// drops what it was given is worse than one that says it cannot.
pub fn apply(cfg: &Config, patch: &Value) -> Result<Patch, String> {
    let Some(obj) = patch.as_object() else {
        return Err("expected an object".into());
    };
    let mut out = Patch::default();
    for (key, value) in obj {
        match key.as_str() {
            "node_name" => set_string(&cfg.node_name, value, "node_name", &mut out)?,
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
                            set_string(&cfg.harness.id, v, "harness.id", &mut out)?
                        }
                        "version" => {
                            set_string(&cfg.harness.version, v, "harness.version", &mut out)?
                        }
                        "tools" => set_list(&cfg.harness.tools, v, "harness.tools", &mut out)?,
                        other => return Err(unknown(&format!("harness.{other}"))),
                    }
                }
            }
            "session" => {
                for (k, v) in object(value, "session")? {
                    match k.as_str() {
                        "budget_tokens" => set_whole(
                            cfg.session.budget_tokens,
                            v,
                            "session.budget_tokens",
                            &mut out,
                        )?,
                        "permission_timeout_secs" => set_whole(
                            cfg.session.permission_timeout_secs,
                            v,
                            "session.permission_timeout_secs",
                            &mut out,
                        )?,
                        "approval_expiry_secs" => set_whole(
                            cfg.session.approval_expiry_secs,
                            v,
                            "session.approval_expiry_secs",
                            &mut out,
                        )?,
                        "default_channel" => set_string(
                            &cfg.session.default_channel,
                            v,
                            "session.default_channel",
                            &mut out,
                        )?,
                        other => return Err(unknown(&format!("session.{other}"))),
                    }
                }
            }
            "review" => {
                for (k, v) in object(value, "review")? {
                    match k.as_str() {
                        "max_diff_lines" => set_whole(
                            cfg.review.max_diff_lines,
                            positive(v, "review.max_diff_lines")?,
                            "review.max_diff_lines",
                            &mut out,
                        )?,
                        "max_files" => set_whole(
                            cfg.review.max_files,
                            positive(v, "review.max_files")?,
                            "review.max_files",
                            &mut out,
                        )?,
                        other => return Err(unknown(&format!("review.{other}"))),
                    }
                }
            }
            "gateway" => {
                for (k, v) in object(value, "gateway")? {
                    match k.as_str() {
                        "allow_hosts" => {
                            set_list(&cfg.gateway.allow_hosts, v, "gateway.allow_hosts", &mut out)?
                        }
                        other => return Err(unknown(&format!("gateway.{other}"))),
                    }
                }
            }
            "publish" => {
                for (k, v) in object(value, "publish")? {
                    match k.as_str() {
                        "gh" => set_string(&cfg.publish.gh, v, "publish.gh", &mut out)?,
                        "glab" => set_string(&cfg.publish.glab, v, "publish.glab", &mut out)?,
                        "git" => set_string(&cfg.publish.git, v, "publish.git", &mut out)?,
                        other => return Err(unknown(&format!("publish.{other}"))),
                    }
                }
            }
            "boundary" => {
                for (k, v) in object(value, "boundary")? {
                    match k.as_str() {
                        "podman" => {
                            set_string(&cfg.boundary.podman, v, "boundary.podman", &mut out)?
                        }
                        other => return Err(unknown(&format!("boundary.{other}"))),
                    }
                }
            }
            "supervision" => {
                for (k, v) in object(value, "supervision")? {
                    match k.as_str() {
                        "checks" => {
                            set_list(&cfg.supervision.checks, v, "supervision.checks", &mut out)?
                        }
                        "timeout_secs" => set_whole(
                            cfg.supervision.timeout_secs,
                            v,
                            "supervision.timeout_secs",
                            &mut out,
                        )?,
                        "dependency_inputs" => {
                            let key = "supervision.dependency_inputs";
                            let inputs = string_map(v, key)?;
                            if cfg.supervision.dependency_inputs != inputs {
                                out.set(
                                    key,
                                    toml::Value::Table(
                                        inputs.into_iter().map(|(k, v)| (k, v.into())).collect(),
                                    ),
                                )?;
                            }
                        }
                        "max_snapshot_bytes" => set_whole(
                            cfg.supervision.max_snapshot_bytes,
                            v,
                            "supervision.max_snapshot_bytes",
                            &mut out,
                        )?,
                        other => return Err(unknown(&format!("supervision.{other}"))),
                    }
                }
            }
            "external" => {
                for (k, v) in object(value, "external")? {
                    match k.as_str() {
                        "enabled" => {
                            let next = v
                                .as_bool()
                                .ok_or("`external.enabled` expects true or false")?;
                            if cfg.external.enabled != next {
                                out.set("external.enabled", next)?;
                            }
                        }
                        // External harnesses no longer attach, so nothing
                        // times out; an older interface may still send it.
                        "idle_timeout_secs" => {}
                        other => return Err(unknown(&format!("external.{other}"))),
                    }
                }
            }
            "launch" => {
                for (k, v) in object(value, "launch")? {
                    match k.as_str() {
                        "plugins" => set_list(&cfg.launch.plugins, v, "launch.plugins", &mut out)?,
                        other => return Err(unknown(&format!("launch.{other}"))),
                    }
                }
            }
            "providers" => {
                for (name, entry) in object(value, "providers")? {
                    let Some(provider) = cfg.providers.get(name) else {
                        return Err(format!(
                            "`providers.{name}` is not a provider on this node; add it to node.toml first"
                        ));
                    };
                    for (k, v) in object(entry, &format!("providers.{name}"))? {
                        match k.as_str() {
                            "models" => {
                                let key = format!("providers.{name}.models");
                                let models = model_list(v, &key)?;
                                if provider.models == models {
                                    continue;
                                }
                                let at = KeyPath::keys(&["providers", name]);
                                // A provider the file does not name comes
                                // from the defaults on load, entry by entry:
                                // a partial entry written here would shadow
                                // the whole default one and lose its
                                // credential and upstream. Write it whole.
                                let listed = toml::Value::try_from(&models)
                                    .map_err(|e| format!("`{key}`: {e}"))?;
                                let op = match cfg.declared_providers.contains(name) {
                                    true => Op::set(at.key("models"), listed),
                                    false => {
                                        let mut whole = toml::Table::try_from(provider)
                                            .map_err(|e| format!("`{key}`: {e}"))?;
                                        // Written even when empty: `models = []`
                                        // is a declaration, and leaving it out
                                        // would load the defaults back.
                                        whole.insert("models".into(), listed);
                                        Op::set(at, whole)
                                    }
                                };
                                out.ops.push(op);
                                out.changed.push(key);
                            }
                            other => return Err(unknown(&format!("providers.{name}.{other}"))),
                        }
                    }
                }
            }
            other => return Err(unknown(other)),
        }
    }
    Ok(out)
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

fn set_list(current: &[String], v: &Value, key: &str, out: &mut Patch) -> Result<(), String> {
    let next = string_list(v, key)?;
    if current != next.as_slice() {
        out.set(key, next)?;
    }
    Ok(())
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

fn set_string(current: &str, v: &Value, key: &str, out: &mut Patch) -> Result<(), String> {
    let next = v
        .as_str()
        .ok_or_else(|| format!("`{key}` expects a string"))?;
    if current != next {
        out.set(key, next)?;
    }
    Ok(())
}

/// A whole-number setting, whatever integer type the struct keeps it in.
/// TOML integers are signed 64-bit, so that is the ceiling.
fn set_whole<N>(current: N, v: &Value, key: &str, out: &mut Patch) -> Result<(), String>
where
    N: TryInto<i64>,
{
    let next = i64::try_from(whole(v, key)?).map_err(|_| format!("`{key}` is too large"))?;
    if current.try_into().ok() != Some(next) {
        out.set(key, next)?;
    }
    Ok(())
}

fn whole(v: &Value, key: &str) -> Result<u64, String> {
    v.as_u64()
        .ok_or_else(|| format!("`{key}` expects a whole number that is not negative"))
}

/// A cap of zero refuses every submission, which is never what setting one means.
fn positive<'a>(v: &'a Value, key: &str) -> Result<&'a Value, String> {
    if whole(v, key)? < 1 {
        return Err(format!("`{key}` must be at least 1"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node.toml as the patches land on it: each patch is mapped to
    /// operations against what the file loads as, the operations are applied
    /// to its text, and the text is loaded again.
    struct File {
        text: String,
        cfg: Config,
    }

    impl File {
        fn new() -> Self {
            Self::from("")
        }

        fn from(text: &str) -> Self {
            let cfg = Config::from_toml(text, std::path::Path::new("node.toml")).unwrap();
            Self {
                text: text.into(),
                cfg,
            }
        }

        fn apply(&mut self, patch: &Value) -> Result<Vec<String>, String> {
            let patch = apply(&self.cfg, patch)?;
            self.text = crate::config::edit::apply_to_text(&self.text, &patch.ops)
                .map_err(|e| e.to_string())?;
            self.cfg = Config::from_toml(&self.text, std::path::Path::new("node.toml"))?;
            Ok(patch.changed)
        }
    }

    #[test]
    fn a_review_cap_of_zero_is_refused() {
        for key in ["max_diff_lines", "max_files"] {
            let err = positive(&json!(0), &format!("review.{key}")).unwrap_err();
            assert!(err.contains("at least 1"), "{err}");
        }
        assert!(positive(&json!(1), "review.max_files").is_ok());
    }

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
        let mut file = File::new();
        let changed = file.apply(
            &json!({ "providers": { "openai-codex": { "models": [
                { "id": " gpt-6 ", "name": "GPT-6", "context": 1_000_000, "output": 200_000, "reasoning": true },
                { "id": "gpt-6-mini" }
            ] } } }),
        )
        .unwrap();
        assert_eq!(changed, ["providers.openai-codex.models"]);
        let models = &file.cfg.providers["openai-codex"].models;
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-6");
        assert_eq!(models[0].context, 1_000_000);
        assert_eq!(models[1].label(), "gpt-6-mini");
        assert_eq!(
            file.cfg.providers["openai-codex"].upstream,
            "https://chatgpt.com/backend-api"
        );

        // Re-applying is a no-op; an empty list is a decision, not an error.
        let again = file.apply(&json!({ "providers": { "openai-codex": { "models": [
            { "id": "gpt-6", "name": "GPT-6", "context": 1_000_000, "output": 200_000, "reasoning": true },
            { "id": "gpt-6-mini" }
        ] } } })).unwrap();
        assert!(again.is_empty());
        let cleared = file
            .apply(&json!({ "providers": { "anthropic": { "models": [] } } }))
            .unwrap();
        assert_eq!(cleared, ["providers.anthropic.models"]);
        assert!(file.text.contains("models = []"), "{}", file.text);

        let dup = file.apply(
            &json!({ "providers": { "anthropic": { "models": [{ "id": "a" }, { "id": "a" }] } } }),
        );
        assert!(dup.unwrap_err().contains("twice"));
        let blank =
            file.apply(&json!({ "providers": { "anthropic": { "models": [{ "id": "  " }] } } }));
        assert!(blank.unwrap_err().contains("without an id"));
        let nobody = file.apply(&json!({ "providers": { "nobody": { "models": [] } } }));
        assert!(nobody.unwrap_err().contains("not a provider"));
        let upstream =
            file.apply(&json!({ "providers": { "anthropic": { "upstream": "http://x" } } }));
        assert!(upstream
            .unwrap_err()
            .contains("providers.anthropic.upstream"));
    }

    #[test]
    fn a_patch_applies_only_what_changed() {
        let mut file = File::new();
        let before = file.cfg.harness.id.clone();
        let changed = file.apply(&json!({ "harness": { "id": "claude" }, "session": { "budget_tokens": 5_000_000 } }),
        )
        .unwrap();
        assert_eq!(file.cfg.harness.id, "claude");
        assert_eq!(file.cfg.session.budget_tokens, 5_000_000);
        assert!(changed.contains(&"harness.id".to_string()));
        assert!(changed.contains(&"session.budget_tokens".to_string()));
        assert_ne!(before, file.cfg.harness.id);

        // Re-applying the same values changes nothing, so the interface can
        // say honestly whether a restart is owed.
        let again = file
            .apply(&json!({ "harness": { "id": "claude" } }))
            .unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn a_harness_the_node_cannot_run_is_never_written() {
        let mut file = File::new();
        let err = file
            .apply(&json!({ "harness": { "id": "omp" } }))
            .unwrap_err();
        assert!(err.contains("was removed"), "{err}");
        let err = file
            .apply(&json!({ "harness": { "id": "aider" } }))
            .unwrap_err();
        assert!(err.contains("opencode"), "{err}");
        assert_eq!(file.cfg.harness.id, Config::default().harness.id);
    }

    #[test]
    fn the_default_channel_is_a_session_setting() {
        let mut file = File::new();
        assert_eq!(config_view(&file.cfg)["session"]["default_channel"], "");
        let changed = file
            .apply(&json!({ "session": { "default_channel": "work" } }))
            .unwrap();
        assert_eq!(changed, vec!["session.default_channel"]);
        assert_eq!(file.cfg.session.default_channel, "work");
        assert_eq!(config_view(&file.cfg)["session"]["default_channel"], "work");
        // Clearing it is a change too.
        let changed = file
            .apply(&json!({ "session": { "default_channel": "" } }))
            .unwrap();
        assert_eq!(changed, vec!["session.default_channel"]);
    }

    #[test]
    fn operator_can_set_candidate_check_identities() {
        let mut file = File::new();
        let changed = file
            .apply(&json!({
                "supervision": {
                    "checks": ["cargo test --locked"],
                    "timeout_secs": 120,
                    "dependency_inputs": { "lockfile": "sha256:abc" },
                    "max_snapshot_bytes": 1048576,
                }
            }))
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
            file.cfg.supervision.checks,
            vec!["cargo test --locked".to_string()]
        );
        assert_eq!(
            file.cfg.supervision.dependency_inputs.get("lockfile"),
            Some(&"sha256:abc".to_string())
        );
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_dropped() {
        let mut file = File::new();
        let err = file
            .apply(&json!({ "mesh": { "hub_url": "https://x" } }))
            .unwrap_err();
        assert!(err.contains("mesh"), "{err}");
        let err = file
            .apply(&json!({ "harness": { "nope": 1 } }))
            .unwrap_err();
        assert!(err.contains("harness.nope"), "{err}");
    }

    #[test]
    fn a_wrong_type_is_refused_with_the_key_named() {
        let mut file = File::new();
        let err = file
            .apply(&json!({ "session": { "budget_tokens": "lots" } }))
            .unwrap_err();
        assert!(err.contains("session.budget_tokens"), "{err}");
        let err = file
            .apply(&json!({ "gateway": { "allow_hosts": "a" } }))
            .unwrap_err();
        assert!(err.contains("gateway.allow_hosts"), "{err}");
    }
}
