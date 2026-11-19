//! The caller context against the plugins' own test vectors
//! (`tests/fixtures/plugin_contract/`): the node writes a context and its
//! signing bytes exactly as sql-review and jira-review read them, and the
//! tests' model of a plugin (`support::plugin::Verifier`) refuses each
//! tampered case at the check the vectors name.

#[path = "support/mod.rs"]
mod support;

use std::collections::BTreeMap;

use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{json, Value};

use support::plugin::Verifier;
use tracon::plugin::context;

fn vectors(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/plugin_contract/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The parts of a vector file both shapes share, read from either.
struct Vectors {
    file: Value,
    valid: Value,
    signing_bytes: String,
    call: Value,
    cases: Vec<Value>,
}

fn read(name: &str) -> Vectors {
    let file = vectors(name);
    let valid = file["valid"].clone();
    // sql-review names one call for the file and `signing_bytes`;
    // jira-review a call per case and `signing_bytes_hex`.
    let call = valid
        .get("call")
        .or_else(|| file.get("call"))
        .cloned()
        .unwrap();
    let signing_bytes = valid
        .get("signing_bytes")
        .or_else(|| valid.get("signing_bytes_hex"))
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    let cases = file
        .get("tampered")
        .or_else(|| file.get("refused"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap();
    Vectors {
        file,
        valid,
        signing_bytes,
        call,
        cases,
    }
}

fn key(file: &Value) -> SigningKey {
    let seed: [u8; 32] = hex::decode(file["seed"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    SigningKey::from_bytes(&seed)
}

fn the_node_writes_what(name: &str) {
    let v = read(name);
    let ctx = &v.valid["context"];
    let key = key(&v.file);
    assert_eq!(
        hex::encode(key.verifying_key().to_bytes()),
        v.file["public_key"]
    );
    let bytes = context::signing_bytes(ctx).unwrap();
    assert_eq!(
        hex::encode(&bytes),
        v.signing_bytes,
        "{name}: signing bytes"
    );
    assert_eq!(
        hex::encode(key.sign(&bytes).to_bytes()),
        v.valid["signature"],
        "{name}: signature"
    );
    assert_eq!(
        context::arguments_sha256(&v.call["arguments"]).unwrap(),
        ctx["arguments_sha256"],
        "{name}: arguments hash"
    );
    if let Some(canonical) = v.valid.get("arguments_canonical") {
        assert_eq!(
            context::canonical(&v.call["arguments"]).unwrap(),
            *canonical
        );
    }
    assert_eq!(
        context::instance_of(v.file["plugin_token"].as_str().unwrap()),
        v.file["instance"],
        "{name}: instance"
    );

    // A context the node builds from the same call is the vectors' own, but
    // for its nonce, which is fresh each time.
    let caller = context::CallerId {
        kind: "agent",
        session: ctx["caller"]["session"].as_str().map(str::to_string),
        harness: ctx["caller"]["harness"].as_str().map(str::to_string),
    };
    let credentials: BTreeMap<String, BTreeMap<String, String>> =
        serde_json::from_value(ctx["credentials"].clone()).unwrap();
    let issued = ctx["issued_ms"].as_i64().unwrap();
    let mut built = context::build(
        &context::Draft {
            plugin: ctx["plugin"].as_str().unwrap(),
            token: v.file["plugin_token"].as_str().unwrap(),
            tool: v.call["tool"].as_str().unwrap(),
            arguments: &v.call["arguments"],
            caller: &caller,
            channel: ctx["channel"].as_str().unwrap(),
            approval_id: ctx["approval_id"].as_str(),
            credentials: &credentials,
        },
        issued,
        |b| Ok(key.sign(b)),
    )
    .unwrap();
    assert_eq!(
        built["expires_ms"].as_i64().unwrap() - issued,
        ctx["expires_ms"].as_i64().unwrap() - issued,
        "{name}: lifetime"
    );
    built["nonce"] = ctx["nonce"].clone();
    let resigned = key.sign(&context::signing_bytes(&built).unwrap());
    built["signature"] = json!(hex::encode(resigned.to_bytes()));
    assert_eq!(&built, ctx, "{name}: built context");
}

fn the_plugin_side_refuses_what(name: &str) {
    let v = read(name);
    let plugin = v.valid["context"]["plugin"].as_str().unwrap();
    let instance = v.file["instance"].as_str().unwrap();
    let node_key = v.file["public_key"].as_str().unwrap();
    let now = v.file["now_ms"].as_i64().unwrap();
    let verifier = || Verifier::new(plugin, instance, node_key);
    let tool = v.call["tool"].as_str().unwrap();
    verifier()
        .verify(&v.valid["context"], tool, &v.call["arguments"], now)
        .unwrap();
    for case in &v.cases {
        let check = case["check"].as_u64().unwrap() as u8;
        let call = case.get("call").unwrap_or(&v.call);
        let tool = call["tool"].as_str().unwrap();
        let mut verifier = verifier();
        let replayed = case["replay"] == true || case["after"] == "valid";
        if replayed {
            let first = if case["replay"] == true {
                &case["context"]
            } else {
                &v.valid["context"]
            };
            verifier
                .verify(first, tool, &call["arguments"], now)
                .unwrap();
        }
        let outcome = verifier.verify(&case["context"], tool, &call["arguments"], now);
        // An approval for a write tool is the plugin's own list to keep.
        if check == 7 {
            assert!(outcome.is_ok(), "{name}: {}: {outcome:?}", case["name"]);
            continue;
        }
        assert_eq!(
            outcome.map_err(|(check, _)| check),
            Err(check),
            "{name}: {}",
            case["name"]
        );
    }
}

#[test]
fn the_node_writes_sql_reviews_vectors() {
    the_node_writes_what("sql-review");
}

#[test]
fn the_node_writes_jira_reviews_vectors() {
    the_node_writes_what("jira-review");
}

#[test]
fn the_plugin_model_refuses_sql_reviews_tampered_contexts() {
    the_plugin_side_refuses_what("sql-review");
}

#[test]
fn the_plugin_model_refuses_jira_reviews_tampered_contexts() {
    the_plugin_side_refuses_what("jira-review");
}
