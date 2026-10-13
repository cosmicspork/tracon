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
use super::passkey::{AdminRequest, Refusal};
use crate::config::{
    edit::{self, Change, EditError, REDACTED},
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

impl From<Refusal> for Refused {
    fn from(refusal: Refusal) -> Self {
        refusal.into_response().into()
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
        "editable_at_node": !spec.host_only || config_schema::editable_at_node(spec.key),
    });
    if spec.host_only {
        v["why_not"] = json!(config_schema::host_only_reason(spec.key));
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
    // What the node runs: the live keys of `running_hash`, the rest of the
    // file it started from, and what a restart would change.
    v["running_hash"] = json!(s.cfg.running_hash());
    v["pending_restart"] = json!(crate::config::live::pending_restart_at(
        &s.cfg,
        &Config::config_path()
    ));
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

/// Every key an operation names, with the value it writes there: each leaf
/// of what a `set` or an `insert` writes (the key itself where that is an
/// empty table or array), and the key a `remove` or `reorder` names.
fn named(ops: &[Op]) -> Vec<(Vec<String>, Option<toml::Value>)> {
    let path = |key: &KeyPath| -> Vec<String> {
        key.schema_key().into_iter().map(str::to_string).collect()
    };
    let mut out = Vec::new();
    for op in ops {
        let (at, value) = match op {
            Op::Set { key, value } => (path(key), Some(value.clone())),
            Op::Insert { array, entry, .. } => {
                (path(array), Some(toml::Value::Table(entry.clone())))
            }
            Op::Remove { key } => (path(key), None),
            Op::Reorder { array, .. } => (path(array), None),
        };
        match value {
            Some(value) => {
                let mut leaves = Vec::new();
                collect_leaves(&at, &value, &mut leaves);
                if leaves.is_empty() {
                    out.push((at, Some(value)));
                } else {
                    out.extend(leaves.into_iter().map(|(k, v)| (k, Some(v.clone()))));
                }
            }
            None => out.push((at, None)),
        }
    }
    out
}

/// Whether any operation writes a value at a secret key. Such a request is
/// always confirmed, even when the file already holds that value: answering
/// "nothing changed" before a confirmation would tell a cookie alone whether
/// it guessed a secret.
fn names_a_secret(ops: &[Op]) -> bool {
    named(ops).iter().any(|(key, value)| {
        let segs: Vec<&str> = key.iter().map(String::as_str).collect();
        value.is_some() && config_schema::lookup(&segs).secret
    })
}

/// Whether `key` (an exact schema key) may not be changed by this caller,
/// and why.
fn host_only(segs: &[&str], local: bool) -> Option<&'static str> {
    let spec = config_schema::lookup(segs);
    (spec.host_only && !spec.key.is_empty())
        .then_some(spec.key)
        .filter(|key| !local || !config_schema::editable_at_node(key))
        .map(config_schema::host_only_reason)
}

/// What an operation may not name, decided from the request alone, before
/// the file is read: a key the node does not read, a secret's placeholder
/// sent back as its value, a key only the host may change. The answer is
/// the same whatever the file holds, so it tells a caller nothing about it.
fn check_named(ops: &[Op], local: bool) -> Vec<KeyError> {
    let mut errors = Vec::new();
    for (key, value) in named(ops) {
        let segs: Vec<&str> = key.iter().map(String::as_str).collect();
        let shown = segs.join(".");
        if value.is_some() && !config_schema::known(&segs) {
            errors.push(KeyError {
                key: shown.clone(),
                message: format!("{shown} is not a node.toml key this node reads"),
            });
            continue;
        }
        if value.as_ref().and_then(toml::Value::as_str) == Some(REDACTED)
            && config_schema::lookup(&segs).secret
        {
            errors.push(KeyError {
                key: shown.clone(),
                message: format!(
                    "{shown} is a secret, sent as {REDACTED:?} and never read back: set a new value, or leave the key out"
                ),
            });
        }
        if let Some(why) = host_only(&segs, local) {
            errors.push(KeyError {
                key: shown.clone(),
                message: format!("{shown} {why}"),
            });
        }
    }
    dedup(errors)
}

/// What the changes an edit would make may not do: write a key the node
/// does not read, or touch a host-only key, under a table the request set
/// whole. Only what is new is looked at: an entry of an array of tables the
/// edit leaves as it was is not the edit's doing.
fn check_changes(changes: &[Change], local: bool) -> Vec<KeyError> {
    let mut errors = Vec::new();
    for change in changes {
        let at: Vec<String> = change
            .key
            .schema_key()
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut leaves = Vec::new();
        if let Some(after) = &change.after {
            match (after, &change.before) {
                (toml::Value::Array(now), Some(toml::Value::Array(was))) => {
                    for entry in now.iter().filter(|e| !was.contains(e)) {
                        collect_leaves(&at, entry, &mut leaves);
                    }
                }
                _ => collect_leaves(&at, after, &mut leaves),
            }
        }
        for (key, _) in &leaves {
            let segs: Vec<&str> = key.iter().map(String::as_str).collect();
            if !config_schema::known(&segs) {
                errors.push(KeyError {
                    key: change.key.to_string(),
                    message: format!("{} is not a node.toml key this node reads", segs.join(".")),
                });
            }
        }
        let mut touched: Vec<Vec<String>> = leaves.into_iter().map(|(k, _)| k).collect();
        if let Some(before) = &change.before {
            let mut was = Vec::new();
            collect_leaves(&at, before, &mut was);
            touched.extend(was.into_iter().map(|(k, _)| k));
        }
        touched.push(at);
        for key in touched {
            let segs: Vec<&str> = key.iter().map(String::as_str).collect();
            if let Some(why) = host_only(&segs, local) {
                errors.push(KeyError {
                    key: change.key.to_string(),
                    message: format!("{} {why}", segs.join(".")),
                });
            }
        }
    }
    dedup(errors)
}

fn dedup(mut errors: Vec<KeyError>) -> Vec<KeyError> {
    let mut seen = std::collections::BTreeSet::new();
    errors.retain(|e| seen.insert((e.key.clone(), e.message.clone())));
    errors
}

fn refused_keys(errors: &[KeyError]) -> Refused {
    let message = errors
        .iter()
        .map(|e| e.message.clone())
        .collect::<Vec<_>>()
        .join("; ");
    refusal(StatusCode::UNPROCESSABLE_ENTITY, "refused", message, errors)
}

/// The version a request was decided against: `given` (its body's
/// `expected_hash`) or its `If-Match`, which must agree. Required.
pub(crate) fn expected_version(given: Option<&str>, req: &AdminRequest) -> Result<String, Refused> {
    match (given, req.if_match()) {
        (Some(a), Some(b)) if a.trim() != b => Err(refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "expected_hash and If-Match name different versions".into(),
            &[],
        )),
        (Some(hash), _) | (None, Some(hash)) => Ok(hash.trim().to_string()),
        (None, None) => Err(refusal(
            StatusCode::PRECONDITION_REQUIRED,
            "precondition_required",
            "name the version this edit was decided against: expected_hash (or If-Match) is the hash the node gave with what was read".into(),
            &[],
        )),
    }
}

/// The file's text and what it loads as, if it is still the version
/// `expected` names; a conflict carrying the version now if not.
pub(crate) fn read_expected(expected: &str) -> Result<(String, Config), Refused> {
    let (text, hash) = edit::read_text(&Config::config_path()).map_err(edit_refusal)?;
    if hash != expected {
        return Err(conflict(&hash));
    }
    let config = edit::preview(&text, &[]).map_err(edit_refusal)?.config;
    Ok((text, config))
}

/// What `ops` would change in `text`, and the line the passkey prompt shows
/// for it (`config_prompt`). An edit whose sensitive keys do not all fit
/// that line is refused, to be split.
pub(crate) fn prompt(text: &str, ops: &[Op]) -> Result<(Vec<Change>, String), Refused> {
    let seen = edit::preview(text, ops).map_err(edit_refusal)?;
    let parse = |t: &str| toml::from_str::<toml::Table>(t).unwrap_or_default();
    let summary = super::config_prompt::summarize(&parse(text), &parse(&seen.text), &seen.changes)
        .map_err(|why| refusal(StatusCode::UNPROCESSABLE_ENTITY, "too_large", why, &[]))?;
    Ok((seen.changes, summary))
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
    let body: PatchBody = req.json()?;
    let expected = expected_version(body.expected_hash.as_deref(), &req)?;
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
    let local = req.is_local();
    let errors = check_named(&ops, local);
    if !errors.is_empty() {
        return Err(refused_keys(&errors));
    }

    let (text, _) = read_expected(&expected)?;
    let (changes, summary) = prompt(&text, &ops)?;
    let errors = check_changes(&changes, local);
    if !errors.is_empty() {
        return Err(refused_keys(&errors));
    }
    if changes.is_empty() && !names_a_secret(&ops) {
        return Ok(Json(json!({
            "changed": [],
            "hash": expected,
            "restart_required": false,
        })));
    }
    req.confirm(&s, &summary).await?;

    let outcome = edit::edit_live(&s.cfg, s.store(), &ops, Some(&expected), actor(&req))
        .map_err(edit_refusal)?;
    let (restart_required, pending) = restart_owed(&s, &outcome).await;
    Ok(Json(json!({
        "changed": outcome.changed(),
        "old_hash": outcome.old_hash,
        "hash": outcome.new_hash,
        "restart_required": restart_required,
        "pending_restart": pending,
        "audit_id": outcome.audit_id,
    })))
}

/// What the node owes a restart once `outcome` is written, as the restart
/// banner says it (`GET /api/restart`): every restart-only key that differs
/// from the file the node started from, not only this edit's. The banner is
/// told at once rather than at the next tick. A node that never recorded
/// the file it started from (a test's) says whether this edit changed a
/// restart-only key, with no list.
pub(crate) async fn restart_owed(s: &AppState, outcome: &edit::EditOutcome) -> (bool, Value) {
    let pending = crate::config::live::pending_restart_at(&s.cfg, &Config::config_path());
    if outcome.wrote() {
        crate::restart::publish(&s.manager).await;
    }
    match pending {
        Some(pending) => (
            !pending.keys.is_empty() || pending.unparseable,
            serde_json::to_value(&pending).unwrap_or_default(),
        ),
        None => (outcome.restart_required, Value::Null),
    }
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

    fn op_set(key: &str, value: toml::Value) -> Op {
        Op::set(KeyPath::parse(key).unwrap(), value)
    }

    #[test]
    fn what_an_operation_names_is_checked_before_the_file_is_read() {
        let s = |v: &str| toml::Value::String(v.into());
        assert!(check_named(&[op_set("publish.gh", s("/usr/bin/gh"))], false).is_empty());
        let errors = check_named(&[op_set("publish.ghh", s("x"))], false);
        assert!(
            errors[0].message.contains("not a node.toml key"),
            "{}",
            errors[0].message
        );
        // Inside a table set whole, and an entry inserted, too.
        let table = toml::Value::Table(toml::from_str("gh = \"gh\"\ntypo = 1").unwrap());
        let errors = check_named(&[op_set("publish", table)], false);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].key, "publish.typo");
        let entry: toml::Table = toml::from_str("path = \"/x\"\nmystery = 1").unwrap();
        let errors = check_named(&[Op::append(KeyPath::keys(&["repo"]), entry)], false);
        assert_eq!(errors[0].key, "repo.mystery");
        // Removing a key nobody reads is how a typo is cleaned up.
        assert!(
            check_named(&[Op::remove(KeyPath::parse("qa.browser").unwrap())], false).is_empty()
        );
        // Host-only keys: refused from elsewhere; at the node, all but the
        // state mount may change.
        for key in [
            "gateway.harness_listen",
            "boundary.gateway_image",
            "boundary.harness_image",
            "runtime.kubernetes.harness_image",
        ] {
            assert_eq!(check_named(&[op_set(key, s("x"))], false).len(), 1, "{key}");
            assert!(
                check_named(&[op_set(key, s("x"))], true).is_empty(),
                "{key}"
            );
        }
        let mount = op_set("runtime.kubernetes.state_mount", s("/elsewhere"));
        for local in [false, true] {
            let errors = check_named(std::slice::from_ref(&mount), local);
            assert!(
                errors[0].message.contains("on the host"),
                "{}",
                errors[0].message
            );
        }
    }

    #[test]
    fn a_secret_named_is_always_confirmed() {
        // No key is secret in the schema today; an unknown key is held to
        // the rule, and is refused by name before anything else.
        assert!(!names_a_secret(&[op_set(
            "review.max_files",
            toml::Value::Integer(1)
        )]));
        assert!(names_a_secret(&[op_set(
            "qa.token",
            toml::Value::String("guess".into())
        )]));
    }

    #[test]
    fn only_new_entries_of_a_whole_array_are_checked() {
        let entry = |t: &str| toml::Value::Table(toml::from_str(t).unwrap());
        let old = entry("path = \"/a\"\nmystery = 1");
        let new = entry("path = \"/b\"");
        let changed = change(
            "repo",
            Some(toml::Value::Array(vec![old.clone()])),
            Some(toml::Value::Array(vec![old, new])),
        );
        assert!(check_changes(&[changed], false).is_empty());
        let mount = |v: Option<&str>| v.map(|v| toml::Value::String(v.into()));
        let errors = check_changes(
            &[change(
                "runtime.kubernetes.state_mount",
                mount(Some("/state")),
                None,
            )],
            true,
        );
        assert!(
            errors[0].message.contains("on the host"),
            "{}",
            errors[0].message
        );
    }
}
