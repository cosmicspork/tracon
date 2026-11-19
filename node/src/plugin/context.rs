//! The caller context every call to a plugin carries: contract v1's
//! `params._meta.tracon`, signed with the node's identity key, the key a
//! plugin is given as `TRACON_NODE_KEY`.
//!
//! ```json
//! {
//!   "v": 1,
//!   "plugin": "<manifest name>",
//!   "instance": "<hex sha256 of TRACON_PLUGIN_TOKEN>",
//!   "tool": "<tool name, unprefixed>",
//!   "arguments_sha256": "<hex sha256 of the canonical JSON of params.arguments>",
//!   "caller": { "kind": "operator" | "agent" | "plugin", "session": "<id>" | null, "harness": "<id>" | null },
//!   "channel": "<channel>",
//!   "approval_id": "<id>" | null,
//!   "issued_ms": 0,
//!   "expires_ms": 0,
//!   "nonce": "<32 lowercase hex>",
//!   "credentials": { "<binding>": { "<FIELD>": "<value>" } },
//!   "signature": "<hex ed25519>"
//! }
//! ```
//!
//! The signature covers `tracon/v1/plugin-call`, the canonical JSON of the
//! object without `signature` as a big-endian u32 length, then that JSON.
//! Canonical JSON sorts object keys by byte order, has no whitespace,
//! escapes strings as `serde_json` does and holds integers only: a float has
//! no canonical form, so a call whose arguments hold one is refused before it
//! is decided. The plugin's side checks, in order: version and plugin,
//! instance, tool and arguments, signature, time (5 s of skew, a lifetime of
//! at most 60 s), a nonce it has not seen, and an approval for a write tool.
//! sql-review's README, "Caller context wire format, v1", is the other side
//! of this; `tests/fixtures/plugin_contract/` holds both plugins' vectors.

use std::collections::BTreeMap;

use ed25519_dalek::Signature;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// What the signed bytes start with.
pub const LABEL: &str = "tracon/v1/plugin-call";

/// The wire format's version, `v`.
pub const VERSION: u64 = 1;

/// How long a context is good for once issued. It is checked when the
/// plugin receives the call, so a long call is unaffected.
pub const LIFETIME_MS: i64 = 30_000;

/// A value with no canonical form: it holds a float.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotCanonical;

impl std::fmt::Display for NotCanonical {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "pass decimals as strings: a float has no canonical form in the plugin contract",
        )
    }
}

/// The canonical JSON of `value`.
pub fn canonical(value: &Value) -> Result<String, NotCanonical> {
    let mut out = String::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &Value, out: &mut String) -> Result<(), NotCanonical> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out)?;
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        Value::Number(n) if n.is_f64() => return Err(NotCanonical),
        other => out.push_str(&other.to_string()),
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `arguments_sha256` for a call's arguments: absent or `null` ones hash as
/// `{}`, as the plugin reads a call without them.
pub fn arguments_sha256(arguments: &Value) -> Result<String, NotCanonical> {
    let body = match arguments {
        Value::Null => "{}".to_string(),
        other => canonical(other)?,
    };
    Ok(sha256_hex(body.as_bytes()))
}

/// What the node signs for `context`: the label, then the canonical JSON of
/// the context without its `signature`, length first.
pub fn signing_bytes(context: &Value) -> Result<Vec<u8>, NotCanonical> {
    let mut unsigned = context.clone();
    if let Value::Object(map) = &mut unsigned {
        map.remove("signature");
    }
    let body = canonical(&unsigned)?;
    let mut out = Vec::with_capacity(LABEL.len() + 4 + body.len());
    out.extend_from_slice(LABEL.as_bytes());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body.as_bytes());
    Ok(out)
}

/// `instance`: what a plugin process computes from its own
/// `TRACON_PLUGIN_TOKEN`, so the token itself never travels.
pub fn instance_of(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

/// Who is calling, as the context names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerId {
    pub kind: &'static str,
    pub session: Option<String>,
    pub harness: Option<String>,
}

/// One call's context before it is issued and signed.
pub struct Draft<'a> {
    pub plugin: &'a str,
    /// The instance's `TRACON_PLUGIN_TOKEN`.
    pub token: &'a str,
    /// The plugin's own name for the tool.
    pub tool: &'a str,
    pub arguments: &'a Value,
    pub caller: &'a CallerId,
    pub channel: &'a str,
    pub approval_id: Option<&'a str>,
    /// What is lent to this call, by binding.
    pub credentials: &'a BTreeMap<String, BTreeMap<String, String>>,
}

/// `draft` issued at `now_ms` with a fresh nonce, and signed by `sign`.
pub fn build(
    draft: &Draft<'_>,
    now_ms: i64,
    sign: impl FnOnce(&[u8]) -> Result<Signature, String>,
) -> Result<Value, String> {
    let mut context = json!({
        "v": VERSION,
        "plugin": draft.plugin,
        "instance": instance_of(draft.token),
        "tool": draft.tool,
        "arguments_sha256": arguments_sha256(draft.arguments).map_err(|e| e.to_string())?,
        "caller": {
            "kind": draft.caller.kind,
            "session": draft.caller.session,
            "harness": draft.caller.harness,
        },
        "channel": draft.channel,
        "approval_id": draft.approval_id,
        "issued_ms": now_ms,
        "expires_ms": now_ms + LIFETIME_MS,
        "nonce": hex::encode(rand::random::<[u8; 16]>()),
        "credentials": draft.credentials,
    });
    let bytes = signing_bytes(&context).map_err(|e| e.to_string())?;
    context["signature"] = json!(hex::encode(sign(&bytes)?.to_bytes()));
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_and_refuses_floats() {
        let v = json!({"b": [1, null, "x\"y"], "a": {"d": true, "c": -2}});
        assert_eq!(
            canonical(&v).unwrap(),
            r#"{"a":{"c":-2,"d":true},"b":[1,null,"x\"y"]}"#
        );
        assert_eq!(canonical(&json!({"f": [1.5]})), Err(NotCanonical));
        assert!(NotCanonical
            .to_string()
            .contains("pass decimals as strings"));
    }

    #[test]
    fn missing_arguments_hash_as_an_empty_object() {
        assert_eq!(arguments_sha256(&Value::Null).unwrap(), sha256_hex(b"{}"));
        assert_eq!(arguments_sha256(&json!({})).unwrap(), sha256_hex(b"{}"));
    }

    #[test]
    fn signing_bytes_are_label_length_body() {
        let bytes = signing_bytes(&json!({"z": 1, "a": "x", "signature": "ff"})).unwrap();
        let body = br#"{"a":"x","z":1}"#;
        let mut want = LABEL.as_bytes().to_vec();
        want.extend_from_slice(&(body.len() as u32).to_be_bytes());
        want.extend_from_slice(body);
        assert_eq!(bytes, want);
    }

    #[test]
    fn a_built_context_verifies_under_the_identity_key() {
        let identity = proto::keys::Identity::from_seed(&[9; 32]);
        let caller = CallerId {
            kind: "agent",
            session: Some("s1".into()),
            harness: Some("claude".into()),
        };
        let credentials = BTreeMap::from([(
            "sql-read".to_string(),
            BTreeMap::from([("DB_BACKEND".to_string(), "sqlite".to_string())]),
        )]);
        let args = json!({"sql": "SELECT 1", "limit": 5});
        let draft = Draft {
            plugin: "sql-review",
            token: "t0k3n",
            tool: "query",
            arguments: &args,
            caller: &caller,
            channel: "personal",
            approval_id: None,
            credentials: &credentials,
        };
        let ctx = build(&draft, 1_000, |b| Ok(identity.sign(b))).unwrap();
        assert_eq!(ctx["v"], 1);
        assert_eq!(ctx["instance"], instance_of("t0k3n"));
        assert_eq!(ctx["expires_ms"], 1_000 + LIFETIME_MS);
        assert_eq!(ctx["approval_id"], Value::Null);
        assert_eq!(ctx["credentials"]["sql-read"]["DB_BACKEND"], "sqlite");
        let nonce = ctx["nonce"].as_str().unwrap();
        assert_eq!(nonce.len(), 32);
        assert!(nonce.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        let signature: [u8; 64] = hex::decode(ctx["signature"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        identity
            .verifying_key()
            .verify_strict(
                &signing_bytes(&ctx).unwrap(),
                &Signature::from_bytes(&signature),
            )
            .unwrap();
        // Each context has its own nonce.
        let again = build(&draft, 1_000, |b| Ok(identity.sign(b))).unwrap();
        assert_ne!(again["nonce"], ctx["nonce"]);
        // Arguments with a float are refused before anything is signed.
        let float = json!({"x": 0.5});
        let refused = build(
            &Draft {
                arguments: &float,
                ..draft
            },
            1_000,
            |_| panic!("not signed"),
        );
        assert!(refused.unwrap_err().contains("float"));
    }
}
