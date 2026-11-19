//! Plugins for the integration tests: installing the fake plugin
//! (`fake_plugin.rs`) as the operator's allowed card would, and a model of a
//! plugin's side of the caller context (contract v1), written apart from the
//! node's own (`tracon::plugin::context`) so the two can disagree.

use std::collections::HashSet;
use std::path::Path;

use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::Value;
use sha2::{Digest, Sha256};

use tracon::{
    plugin::{install, manifest::Manifest},
    store::{plugins::Pending, Store},
};

/// The digest every build installed here is recorded under.
pub const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Install the fake plugin as the plugin `manifest_text` names, active as
/// the operator's allowed card leaves it.
/// The tests of one binary share its state directory, so a build already
/// written there is left as it is: rewriting it under a running instance
/// would change what it was checked against.
pub fn install(store: &Store, state: &Path, manifest_text: &str) {
    static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let manifest = Manifest::parse(manifest_text).unwrap();
    let name = manifest.name.clone();
    let version = manifest.version.clone();
    let dir = install::build_dir(state, &name, &version, DIGEST);
    let tree_digest = {
        let _one = WRITING.lock().unwrap_or_else(|p| p.into_inner());
        let file = dir.join("tracon-plugin.toml");
        if std::fs::read_to_string(&file).ok().as_deref() != Some(manifest_text) {
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::write(&file, manifest_text).unwrap();
        }
        // Linked rather than copied where it can be: a copy just written can
        // be busy (ETXTBSY) when another thread forks.
        let exe = dir.join(&manifest.exec[0]);
        if !exe.exists() && std::fs::hard_link(env!("CARGO_BIN_EXE_fake_plugin"), &exe).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_fake_plugin"), &exe).unwrap();
        }
        install::tree_digest(&dir).unwrap()
    };
    store
        .plugin_stage_pending(
            &name,
            &format!("local:{name}.tar.gz"),
            &Pending {
                build: tracon::store::plugins::Build {
                    version,
                    digest: DIGEST.into(),
                    tree_digest,
                    manifest,
                },
                approval_id: format!("card-{name}"),
                tag: None,
                changelog: None,
            },
        )
        .unwrap();
    store
        .plugin_activate_pending(&name, &format!("card-{name}"))
        .unwrap()
        .unwrap();
}

/// How a plugin checks the caller context it is sent, checks 1 to 6 of
/// contract v1 (7, an approval for a write tool, is the plugin's own list).
pub struct Verifier {
    plugin: String,
    instance: String,
    key: VerifyingKey,
    seen: HashSet<String>,
}

/// The context's JSON as signed: serde_json's own map is ordered by key
/// bytes and written without whitespace, so its text is the canonical form
/// once no float is in it.
fn canonical(value: &Value) -> Option<String> {
    fn has_float(v: &Value) -> bool {
        match v {
            Value::Number(n) => !n.is_i64() && !n.is_u64(),
            Value::Array(items) => items.iter().any(has_float),
            Value::Object(map) => map.values().any(has_float),
            _ => false,
        }
    }
    assert_eq!(
        serde_json::json!({"b": 1, "a": 2}).to_string(),
        r#"{"a":2,"b":1}"#,
        "serde_json keeps insertion order here; this model needs sorted maps"
    );
    (!has_float(value)).then(|| value.to_string())
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

impl Verifier {
    /// For the plugin `plugin`, running with a token whose sha256 is
    /// `instance`, started with `TRACON_NODE_KEY` `node_key`.
    pub fn new(plugin: &str, instance: &str, node_key: &str) -> Self {
        let key: [u8; 32] = hex::decode(node_key).unwrap().try_into().unwrap();
        Self {
            plugin: plugin.into(),
            instance: instance.into(),
            key: VerifyingKey::from_bytes(&key).unwrap(),
            seen: HashSet::new(),
        }
    }

    /// The check `ctx` fails for a call of `tool` with `arguments` at
    /// `now_ms`, with why.
    pub fn verify(
        &mut self,
        ctx: &Value,
        tool: &str,
        arguments: &Value,
        now_ms: i64,
    ) -> Result<(), (u8, String)> {
        let fail = |check: u8, why: &str| Err((check, why.to_string()));
        if ctx["v"] != 1 || ctx["plugin"] != self.plugin.as_str() {
            return fail(1, "version or plugin");
        }
        if ctx["instance"] != self.instance.as_str() {
            return fail(2, "instance");
        }
        let arguments = match arguments {
            Value::Null => serde_json::json!({}),
            other => other.clone(),
        };
        let Some(args) = canonical(&arguments) else {
            return fail(3, "a float in the arguments");
        };
        if ctx["tool"] != tool || ctx["arguments_sha256"] != hex_sha256(args.as_bytes()).as_str() {
            return fail(3, "tool or arguments");
        }
        let Some(signature) = ctx["signature"]
            .as_str()
            .filter(|s| !s.chars().any(|c| c.is_ascii_uppercase()))
            .and_then(|s| hex::decode(s).ok())
            .and_then(|b| <[u8; 64]>::try_from(b).ok())
        else {
            return fail(4, "signature is not 64 bytes of lowercase hex");
        };
        let mut unsigned = ctx.clone();
        unsigned.as_object_mut().unwrap().remove("signature");
        let Some(body) = canonical(&unsigned) else {
            return fail(4, "a float in the context");
        };
        let mut signed = b"tracon/v1/plugin-call".to_vec();
        signed.extend_from_slice(&(body.len() as u32).to_be_bytes());
        signed.extend_from_slice(body.as_bytes());
        if self
            .key
            .verify_strict(&signed, &Signature::from_bytes(&signature))
            .is_err()
        {
            return fail(4, "signature does not verify");
        }
        let (Some(issued), Some(expires)) = (ctx["issued_ms"].as_i64(), ctx["expires_ms"].as_i64())
        else {
            return fail(5, "times");
        };
        if now_ms < issued - 5_000 || now_ms > expires + 5_000 || expires - issued > 60_000 {
            return fail(5, "not fresh");
        }
        let nonce = ctx["nonce"].as_str().unwrap_or_default();
        if nonce.len() != 32 || !nonce.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
            return fail(6, "nonce shape");
        }
        if !self.seen.insert(nonce.to_string()) {
            return fail(6, "nonce seen");
        }
        Ok(())
    }
}
