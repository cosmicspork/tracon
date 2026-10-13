//! `node.toml`, whole, for an operator who is not at the node.
//!
//! `GET /api/config` is the file as it is written, what it loads as, its
//! hash, and what each key is (`config_schema`): whether a change applies
//! live or waits for a restart, whether it names a program or a host path the
//! path guard holds to rules, whether it is a secret. A secret's value is
//! never sent; neither is the value of a key the node does not know, which
//! the schema holds to the same rule.
//!
//! `PATCH /api/config` is typed operations on the file, decided against the
//! hash the caller read: set a key, remove one, insert, remove or reorder the
//! entries of an array of tables. It is an admin action: off this machine
//! the node confirms it with a passkey bound to exactly this request, and the
//! prompt names the keys the request changes, worked out by the node from
//! the file and the request it is confirming. At the node it needs no
//! confirmation. Every write goes through `config::edit`, which validates,
//! runs the path guard, audits and announces it.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::api::{ApiError, AppState};
use super::passkey::{config_summary, AdminRequest};
use crate::config::{
    edit::{self, Change, EditError, Preview, REDACTED},
    path_guard, Config, KeyPath, Op,
};
use crate::config_schema::{self, KEYS};

/// The most operations one request may carry: a form saves one section at
/// a time, and the repository table is its own route.
const MAX_OPS: usize = 256;

/// A refusal already shaped as the response to send. Boxed: a response is
/// large, and most calls succeed.
pub struct Refused(Box<Response>);

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        *self.0
    }
}

impl From<Response> for Refused {
    fn from(response: Response) -> Self {
        Refused(Box::new(response))
    }
}

impl From<ApiError> for Refused {
    fn from(error: ApiError) -> Self {
        error.into_response().into()
    }
}

/// One key a request may not change, and why.
struct KeyError {
    key: String,
    message: String,
}

/// A refusal in the shape every other refusal has (`error.code`,
/// `error.message`), with what a form needs to point at: `error.kind`,
/// `error.fields` (one entry per key) and, on a conflict, the file's `hash`
/// now.
fn refusal(status: StatusCode, kind: &str, message: String, fields: &[KeyError]) -> Refused {
    let mut error = json!({ "code": status.as_u16(), "kind": kind, "message": message });
    if !fields.is_empty() {
        error["fields"] = fields
            .iter()
            .map(|f| json!({ "field": f.key, "message": f.message }))
            .collect();
    }
    (status, Json(json!({ "error": error })))
        .into_response()
        .into()
}

fn conflict(current: &str) -> Refused {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": {
                "code": 409,
                "kind": "conflict",
                "message": "node.toml changed since it was read; read it again and decide against what it is now",
            },
            "hash": current,
        })),
    )
        .into_response()
        .into()
}

/// An edit refused, as the caller should see it: a stale version is a
/// conflict carrying the version now; a refusal of the path guard names
/// each key it refused.
pub(crate) fn edit_refusal(error: EditError) -> Refused {
    match error {
        EditError::Conflict { current } => conflict(&current),
        EditError::Refused(refusals) => {
            let fields: Vec<KeyError> = refusals
                .iter()
                .map(|r| KeyError {
                    key: r.key.clone(),
                    message: format!("{:?}: {}", r.value, r.reason),
                })
                .collect();
            refusal(
                StatusCode::UNPROCESSABLE_ENTITY,
                "refused",
                path_guard::describe(&refusals),
                &fields,
            )
        }
        EditError::Op(message) | EditError::Invalid(message) => refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            edit::without_excerpt(&message),
            &[],
        ),
        other => ApiError::from(other).into(),
    }
}

/// The file's table as a reader may see it: every secret, and every key the
/// node does not know, replaced with the placeholder.
fn redacted_table(table: &toml::Table) -> toml::Value {
    edit::redact(&[], &toml::Value::Table(table.clone()))
}

fn spec_json(spec: &config_schema::KeySpec) -> Value {
    let mut v = json!({
        "key": spec.key,
        "apply": spec.apply,
        "admin": spec.admin,
        "exec_path": spec.exec_path,
        "fs_path": spec.fs_path,
        "path": spec.path,
        "secret": spec.secret,
        "editable": !spec.host_only,
    });
    if spec.host_only {
        v["why_not"] = json!(config_schema::HOST_ONLY_REASON);
    }
    v
}

/// `GET /api/config`: the settings the Settings pane writes (the fields
/// `settings::config_view` names, kept while the pane still reads them), and
/// the whole file beside them: `document` as written, `effective` as it
/// loads (every default filled in), `toml` as text, `hash` to decide an edit
/// against, and `keys`, the schema. Secrets are the placeholder in all of
/// them. `toml` is rendered from the redacted document rather than sent as
/// written: a comment can hold anything, a commented-out key among it.
pub async fn get_config(State(s): State<AppState>) -> Result<Json<Value>, Refused> {
    let path = Config::config_path();
    let (text, hash) = edit::read_text(&path).map_err(edit_refusal)?;
    let snapshot = edit::snapshot_at(&path).map_err(edit_refusal)?;
    if snapshot.hash != hash {
        // Changed between the two reads: one more try, from the start, is
        // what a client would do with a conflict.
        return Err(conflict(&snapshot.hash));
    }
    let document: toml::Table = toml::from_str(&text).map_err(|e| {
        ApiError::new(
            StatusCode::CONFLICT,
            edit::without_excerpt(&format!("node.toml does not parse: {e}")),
        )
        .into_response()
    })?;
    let document = redacted_table(&document);
    let effective = toml::Table::try_from(&snapshot.config)
        .map(|t| redacted_table(&t))
        .map_err(|e| {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        })?;
    let rendered = toml::to_string(&document).unwrap_or_default();

    let mut v = super::settings::config_view(&snapshot.config);
    // What is running, so the pane can say a change is owed a restart rather
    // than pretend it already took.
    v["running"] = json!({
        "harness_id": s.cfg.harness.id,
        "harness_version": s.cfg.harness.version,
        "node_name": s.cfg.node_name,
    });
    v["hash"] = json!(hash);
    v["document"] = serde_json::to_value(&document).unwrap_or_default();
    v["effective"] = serde_json::to_value(&effective).unwrap_or_default();
    v["toml"] = json!(rendered);
    v["redacted"] = json!(REDACTED);
    v["keys"] = KEYS.iter().map(spec_json).collect();
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    /// The file's `hash` as the caller read it. `If-Match` may carry it
    /// instead; one of the two is required.
    #[serde(default)]
    expected_hash: Option<String>,
    ops: Vec<PatchOp>,
}

/// One operation, as the API spells it. Keys are written as `node.toml`'s
/// own dotted paths, an array entry by its index: `publish.gh`,
/// `repo[2].egress`, `providers."my.provider".upstream`.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum PatchOp {
    /// Set a key, or an entry of an array of tables, to a value. A table is
    /// merged key by key: keys it does not name are removed.
    Set { key: String, value: Value },
    /// Remove a key, or with an index an array entry.
    Remove { key: String },
    /// Add an entry to an array of tables, at `index` or after the last.
    Insert {
        array: String,
        #[serde(default)]
        index: Option<usize>,
        entry: serde_json::Map<String, Value>,
    },
    /// Put an array of tables' entries in a new order: entry `k` is the
    /// one that was at `order[k]`.
    Reorder { array: String, order: Vec<usize> },
}

/// A JSON value as TOML has it. `null` has no TOML form: a key is removed
/// with `remove`, not set to nothing.
fn to_toml(value: &Value) -> Result<toml::Value, String> {
    Ok(match value {
        Value::Null => return Err("null is not a TOML value; remove the key instead".into()),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => toml::Value::Integer(i),
            (None, Some(_)) if n.is_u64() => {
                return Err(format!("{n} is larger than a TOML integer holds"))
            }
            (None, Some(f)) => toml::Value::Float(f),
            (None, None) => return Err(format!("{n} is not a TOML number")),
        },
        Value::String(text) => toml::Value::String(text.clone()),
        Value::Array(values) => {
            toml::Value::Array(values.iter().map(to_toml).collect::<Result<_, _>>()?)
        }
        Value::Object(map) => toml::Value::Table(to_table(map)?),
    })
}

fn to_table(map: &serde_json::Map<String, Value>) -> Result<toml::Table, String> {
    map.iter()
        .map(|(k, v)| Ok((k.clone(), to_toml(v)?)))
        .collect()
}

fn key_path(text: &str) -> Result<KeyPath, String> {
    let path = KeyPath::parse(text)?;
    match path.0.first() {
        Some(edit::Seg::Key(_)) => Ok(path),
        _ => Err(format!("{text:?} is not a key path")),
    }
}

/// The request's operations as `config::edit` takes them, or what is wrong
/// with each one that does not read.
fn ops_of(body: &[PatchOp]) -> Result<Vec<Op>, Vec<KeyError>> {
    let mut ops = Vec::with_capacity(body.len());
    let mut errors = Vec::new();
    for (i, op) in body.iter().enumerate() {
        let at = format!("ops[{i}]");
        let made = match op {
            PatchOp::Set { key, value } => key_path(key).and_then(|key| {
                Ok(Op::Set {
                    value: to_toml(value).map_err(|e| format!("{key}: {e}"))?,
                    key,
                })
            }),
            PatchOp::Remove { key } => key_path(key).map(Op::remove),
            PatchOp::Insert {
                array,
                index,
                entry,
            } => key_path(array).and_then(|array| {
                Ok(Op::Insert {
                    entry: to_table(entry).map_err(|e| format!("{array}: {e}"))?,
                    array,
                    index: *index,
                })
            }),
            PatchOp::Reorder { array, order } => key_path(array).map(|array| Op::Reorder {
                array,
                order: order.clone(),
            }),
        };
        match made {
            Ok(op) => ops.push(op),
            Err(message) => errors.push(KeyError { key: at, message }),
        }
    }
    match errors.is_empty() {
        true => Ok(ops),
        false => Err(errors),
    }
}

/// What a change may not do before anything is confirmed: set a key the
/// node does not read, send a secret's placeholder back as its value, or
/// touch a key only the host may change. Each refused key once.
fn check_changes(changes: &[Change]) -> Vec<KeyError> {
    let mut errors = Vec::new();
    for change in changes {
        let at: Vec<String> = change
            .key
            .schema_key()
            .into_iter()
            .map(str::to_string)
            .collect();
        let before = change.before.as_ref().map(|v| (at.clone(), v));
        let after = change.after.as_ref().map(|v| (at.clone(), v));
        let mut leaves = Vec::new();
        if let Some((at, value)) = &after {
            collect_leaves(at, value, &mut leaves);
        }
        for (key, value) in &leaves {
            let segs: Vec<&str> = key.iter().map(String::as_str).collect();
            if !config_schema::known(&segs) {
                errors.push(KeyError {
                    key: change.key.to_string(),
                    message: format!("{} is not a node.toml key this node reads", segs.join(".")),
                });
            } else if config_schema::lookup(&segs).secret && value.as_str() == Some(REDACTED) {
                errors.push(KeyError {
                    key: change.key.to_string(),
                    message: format!(
                        "{} is a secret, sent as {REDACTED:?} and never read back: set a new value, or leave the key out",
                        segs.join(".")
                    ),
                });
            }
        }
        let mut touched = leaves.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        if let Some((at, value)) = &before {
            let mut was = Vec::new();
            collect_leaves(at, value, &mut was);
            touched.extend(was.into_iter().map(|(k, _)| k));
        }
        touched.push(at.clone());
        if let Some(key) = touched.iter().find(|key| {
            let segs: Vec<&str> = key.iter().map(String::as_str).collect();
            config_schema::lookup(&segs).host_only
        }) {
            errors.push(KeyError {
                key: change.key.to_string(),
                message: format!("{} {}", key.join("."), config_schema::HOST_ONLY_REASON),
            });
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    errors.retain(|e| seen.insert((e.key.clone(), e.message.clone())));
    errors
}

/// Every value under `value` that is not itself a table (or an array of
/// tables), with the table keys that lead to it.
fn collect_leaves<'a>(
    at: &[String],
    value: &'a toml::Value,
    out: &mut Vec<(Vec<String>, &'a toml::Value)>,
) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                let mut below = at.to_vec();
                below.push(key.clone());
                collect_leaves(&below, value, out);
            }
        }
        toml::Value::Array(values)
            if !values.is_empty() && values.iter().all(toml::Value::is_table) =>
        {
            for value in values {
                collect_leaves(at, value, out);
            }
        }
        _ => out.push((at.to_vec(), value)),
    }
}

fn actor(req: &AdminRequest) -> &'static str {
    match req.is_local() {
        true => "Settings (at the node)",
        false => "Settings (confirmed with a passkey)",
    }
}

/// `PATCH /api/config`: apply typed operations to `node.toml`, if the file
/// is still the version the caller read.
///
/// In order: the request must read, name the version it expects, and still
/// be decided against the file as it is (`409` with the hash now if not);
/// its operations must apply and leave a file that loads, and touch only
/// keys an edit may (`422`, key by key). Only then is it confirmed: at the
/// node it needs nothing, elsewhere a passkey, and the prompt lists the keys
/// it changes. The write itself runs the path guard against the host, so a
/// refusal of the guard comes after the confirmation, never before it: what
/// the guard knows about the host's files is not for a caller who has not
/// confirmed.
pub async fn patch_config(
    State(s): State<AppState>,
    req: AdminRequest,
) -> Result<Json<Value>, Refused> {
    let body: PatchBody = req.json().map_err(IntoResponse::into_response)?;
    let expected = match (body.expected_hash.as_deref(), req.if_match()) {
        (Some(a), Some(b)) if a != b => {
            return Err(refusal(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid",
                "expected_hash and If-Match name different versions".into(),
                &[],
            ))
        }
        (Some(hash), _) | (None, Some(hash)) => hash.trim().to_string(),
        (None, None) => {
            return Err(refusal(
                StatusCode::PRECONDITION_REQUIRED,
                "precondition_required",
                "name the version this edit was decided against: expected_hash (or If-Match) is GET /api/config's hash".into(),
                &[],
            ))
        }
    };
    if body.ops.is_empty() {
        return Err(refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "no operations".into(),
            &[],
        ));
    }
    if body.ops.len() > MAX_OPS {
        return Err(refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            format!("at most {MAX_OPS} operations in one edit"),
            &[],
        ));
    }
    let ops = ops_of(&body.ops).map_err(|errors| {
        refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "an operation does not read".into(),
            &errors,
        )
    })?;

    let path = Config::config_path();
    let (text, hash) = edit::read_text(&path).map_err(edit_refusal)?;
    if hash != expected {
        return Err(conflict(&hash));
    }
    let Preview { changes, .. } = edit::preview(&text, &ops).map_err(edit_refusal)?;
    if changes.is_empty() {
        return Ok(Json(json!({
            "changed": [],
            "hash": hash,
            "restart_required": false,
        })));
    }
    let errors = check_changes(&changes);
    if !errors.is_empty() {
        let message = errors
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "refused",
            message,
            &errors,
        ));
    }

    let keys: Vec<String> = changes.iter().map(|c| c.key.to_string()).collect();
    req.confirm(&s, &config_summary(&keys))
        .await
        .map_err(IntoResponse::into_response)?;

    let outcome = edit::edit_live(&s.cfg, s.store(), &ops, Some(&expected), actor(&req))
        .map_err(edit_refusal)?;
    Ok(Json(json!({
        "changed": outcome.changed(),
        "old_hash": outcome.old_hash,
        "hash": outcome.new_hash,
        "restart_required": outcome.restart_required,
        "audit_id": outcome.audit_id,
    })))
}

/// The keys `ops` would change in the file as it is now, for a route that
/// writes `node.toml` through its own body and confirms with them.
pub(crate) fn keys_changed(ops: &[Op]) -> Result<Vec<String>, Refused> {
    let (text, _) = edit::read_text(&Config::config_path()).map_err(edit_refusal)?;
    let preview = edit::preview(&text, ops).map_err(edit_refusal)?;
    Ok(preview.changes.iter().map(|c| c.key.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(key: &str, before: Option<toml::Value>, after: Option<toml::Value>) -> Change {
        Change {
            key: KeyPath::parse(key).unwrap(),
            before,
            after,
        }
    }

    #[test]
    fn json_values_map_onto_toml_and_null_does_not() {
        assert_eq!(
            to_toml(&json!({ "a": [1, 2.5, "x", true] })).unwrap(),
            toml::Value::Table(toml::from_str("a = [1, 2.5, \"x\", true]").unwrap())
        );
        assert!(to_toml(&json!(null)).unwrap_err().contains("remove"));
        assert!(to_toml(&json!({ "a": null })).is_err());
        assert!(to_toml(&json!(u64::MAX)).is_err());
    }

    #[test]
    fn a_patch_op_reads_by_its_tag_and_nothing_else() {
        let ops: Vec<PatchOp> = serde_json::from_value(json!([
            { "op": "set", "key": "publish.gh", "value": "/usr/bin/gh" },
            { "op": "remove", "key": "repo[1]" },
            { "op": "insert", "array": "repo", "entry": { "path": "/src/x" } },
            { "op": "reorder", "array": "repo", "order": [1, 0] },
        ]))
        .unwrap();
        let ops = ops_of(&ops).unwrap_or_else(|_| panic!("ops read"));
        assert_eq!(ops.len(), 4);
        for bad in [
            json!({ "op": "set", "key": "publish.gh" }),
            json!({ "op": "set", "key": "publish.gh", "value": 1, "extra": true }),
            json!({ "op": "rename", "key": "publish.gh" }),
        ] {
            assert!(
                serde_json::from_value::<PatchOp>(bad.clone()).is_err(),
                "{bad}"
            );
        }
        let bad: Vec<PatchOp> = serde_json::from_value(json!([
            { "op": "set", "key": "a..b", "value": 1 },
            { "op": "remove", "key": "[0]" },
        ]))
        .unwrap();
        let errors = ops_of(&bad).err().unwrap();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].key, "ops[0]");
    }

    #[test]
    fn unknown_keys_secrets_sent_back_and_host_only_keys_are_refused() {
        let s = |v: &str| Some(toml::Value::String(v.into()));
        assert!(check_changes(&[change("publish.gh", None, s("/usr/bin/gh"))]).is_empty());
        let errors = check_changes(&[change("publish.ghh", None, s("x"))]);
        assert!(
            errors[0].message.contains("not a node.toml key"),
            "{}",
            errors[0].message
        );
        // Inside a table set whole, too.
        let table = toml::Value::Table(toml::from_str("gh = \"gh\"\ntypo = 1").unwrap());
        let errors = check_changes(&[change("publish", None, Some(table))]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("publish.typo"));
        // Removing a key nobody reads is how a typo is cleaned up.
        assert!(check_changes(&[change("qa.browser", s("x"), None)]).is_empty());
        for (before, after) in [(None, s("/elsewhere")), (s("/state"), None)] {
            let errors = check_changes(&[change("runtime.kubernetes.state_mount", before, after)]);
            assert!(
                errors[0].message.contains("on the host"),
                "{}",
                errors[0].message
            );
        }
    }
}
