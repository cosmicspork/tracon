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
    extract::{Path, State},
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
        "editable_by_peer": refused_to(&spec.key.split('.').collect::<Vec<_>>(), Editor::Peer).is_none(),
    });
    if let Some(why) = config_schema::peer_refusal(&spec.key.split('.').collect::<Vec<_>>()) {
        v["why_not_by_peer"] = json!(why);
    }
    if spec.host_only {
        v["why_not"] = json!(config_schema::host_only_reason(spec.key));
    }
    if let Some(shape) = config_schema::shape(spec.key) {
        v["shape"] = json!(shape);
    }
    if let Some(array) = config_schema::array_of(spec.key) {
        v["array"] = json!(array);
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
    view(&s).map(Json)
}

/// What `GET /api/config` answers. An admin peer's `ConfigRead` is answered
/// with exactly this, so a peer sees no more of the file than this node's
/// own Settings pane does.
pub(crate) fn view(s: &AppState) -> Result<Value, Refused> {
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
    Ok(v)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    /// The file's `hash` as the caller read it. `If-Match` may carry it
    /// instead; one of the two is required.
    #[serde(default)]
    expected_hash: Option<String>,
    /// [`PatchOp`]s, read by [`parse_ops`]. Kept as sent, because an edit
    /// of a peer is confirmed (and its statement bound) to these very values.
    ops: Vec<Value>,
}

/// A request's operations as `config::edit` takes them: at least one, at
/// most [`MAX_OPS`], each reading as a [`PatchOp`]; or what is wrong.
fn parse_ops(raw: &[Value]) -> Result<Vec<Op>, Refused> {
    if raw.is_empty() {
        return Err(refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "no operations".into(),
            &[],
        ));
    }
    if raw.len() > MAX_OPS {
        return Err(refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            format!("at most {MAX_OPS} operations in one edit"),
            &[],
        ));
    }
    let mut typed = Vec::with_capacity(raw.len());
    let mut errors = Vec::new();
    for (i, op) in raw.iter().enumerate() {
        match serde_json::from_value::<PatchOp>(op.clone()) {
            Ok(op) => typed.push(op),
            Err(e) => errors.push(KeyError {
                key: format!("ops[{i}]"),
                message: e.to_string(),
            }),
        }
    }
    if !errors.is_empty() {
        return Err(refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "an operation does not read".into(),
            &errors,
        ));
    }
    ops_of(&typed).map_err(|errors| {
        refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "an operation does not read".into(),
            &errors,
        )
    })
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

/// Who is making an edit, which decides the keys it may change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Editor {
    /// The operator at the node's own machine.
    Local,
    /// The operator elsewhere, confirmed with this node's passkey.
    Remote,
    /// An admin peer, its operator confirmed with that peer's passkey
    /// (`mesh::admin`): everything a remote edit may change, but who may
    /// administer this node.
    Peer,
}

impl Editor {
    fn of(req: &AdminRequest) -> Self {
        match req.is_local() {
            true => Editor::Local,
            false => Editor::Remote,
        }
    }
}

/// Whether `key` (an exact schema key) may not be changed by this editor,
/// and why.
fn refused_to(segs: &[&str], editor: Editor) -> Option<&'static str> {
    if editor == Editor::Peer {
        if let Some(why) = config_schema::peer_refusal(segs) {
            return Some(why);
        }
    }
    let spec = config_schema::lookup(segs);
    (spec.host_only && !spec.key.is_empty())
        .then_some(spec.key)
        .filter(|key| editor != Editor::Local || !config_schema::editable_at_node(key))
        .map(config_schema::host_only_reason)
}

/// What an operation may not name, decided from the request alone, before
/// the file is read: a key the node does not read, a secret's placeholder
/// sent back as its value, a key only the host may change. The answer is
/// the same whatever the file holds, so it tells a caller nothing about it.
fn check_named(ops: &[Op], editor: Editor) -> Vec<KeyError> {
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
        if let Some(why) = refused_to(&segs, editor) {
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
fn check_changes(changes: &[Change], editor: Editor) -> Vec<KeyError> {
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
            if let Some(why) = refused_to(&segs, editor) {
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

/// The first character in `text` a confirmation prompt could not show:
/// a control (`\n`, `\t` and `\r` among them), format or line or paragraph
/// separator character (Unicode Cc, Cf, Zl, Zp), other than those `allow`
/// names.
pub(crate) fn hidden_char(text: &str, allow: &[char]) -> Option<char> {
    text.chars()
        .find(|c| super::passkey::hidden(*c) && !allow.contains(c))
}

/// Where in `value` (a request body) a string or a key holds a character a
/// confirmation prompt could not show, and which.
fn hidden_in(value: &Value, at: &str) -> Option<(String, char)> {
    match value {
        Value::String(text) => hidden_char(text, &[]).map(|c| (at.to_string(), c)),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| hidden_in(v, &format!("{at}[{i}]"))),
        Value::Object(map) => map.iter().find_map(|(k, v)| {
            let here = match at.is_empty() {
                true => k.clone(),
                false => format!("{at}.{k}"),
            };
            match hidden_char(k, &[]) {
                Some(c) => Some((here, c)),
                None => hidden_in(v, &here),
            }
        }),
        _ => None,
    }
}

/// Refuse a request body any of whose strings or keys holds a character a
/// prompt could not show. A newline, a tab, a zero-width space or a
/// direction override in a command, a path or a key would make what the
/// operator confirms read differently from what is written. Decided from
/// the request alone, before anything else.
pub(crate) fn refuse_hidden(value: &Value) -> Result<(), Refused> {
    match hidden_in(value, "") {
        None => Ok(()),
        Some((at, c)) => {
            let at = if at.is_empty() {
                "the body".to_string()
            } else {
                at
            };
            Err(refusal(
                StatusCode::UNPROCESSABLE_ENTITY,
                "hidden_character",
                format!(
                    "{at} holds U+{:04X}, a control, format or separator character a confirmation could not show; it is refused",
                    c as u32
                ),
                &[KeyError {
                    key: at.clone(),
                    message: format!("holds U+{:04X}", c as u32),
                }],
            ))
        }
    }
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

/// The file's text, its hash and what it loads as, now.
pub(crate) fn read_now() -> Result<(String, String, Config), Refused> {
    let (text, hash) = edit::read_text(&Config::config_path()).map_err(edit_refusal)?;
    let config = edit::preview(&text, &[]).map_err(edit_refusal)?.config;
    Ok((text, hash, config))
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
    prompt_with(
        &Config::config_path(),
        text,
        ops,
        super::config_prompt::summarize,
    )
}

/// [`prompt`], in `budget` characters of the prompt's line: what is left of
/// one once a peer's `On <peer>: ` has taken its share. `file` is what a
/// refusal names the text: the peer's file, not this node's.
fn prompt_within(
    file: &std::path::Path,
    text: &str,
    ops: &[Op],
    budget: usize,
) -> Result<(Vec<Change>, String), Refused> {
    prompt_with(file, text, ops, |before, after, effective, changes| {
        super::config_prompt::summarize_within(before, after, effective, changes, budget)
    })
}

fn prompt_with(
    file: &std::path::Path,
    text: &str,
    ops: &[Op],
    summarize: impl FnOnce(
        &toml::Table,
        &toml::Table,
        &toml::Table,
        &[Change],
    ) -> Result<String, String>,
) -> Result<(Vec<Change>, String), Refused> {
    let seen = edit::preview_of(file, text, ops).map_err(edit_refusal)?;
    let parse = |t: &str| toml::from_str::<toml::Table>(t).unwrap_or_default();
    let effective = edit::preview_of(file, text, &[])
        .ok()
        .and_then(|now| toml::Table::try_from(&now.config).ok())
        .unwrap_or_default();
    let summary = summarize(&parse(text), &parse(&seen.text), &effective, &seen.changes)
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
    let raw: Value = req.json()?;
    refuse_hidden(&raw)?;
    let body: PatchBody = serde_json::from_value(raw).map_err(|e| {
        refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            format!("the request body does not read: {e}"),
            &[],
        )
    })?;
    let expected = expected_version(body.expected_hash.as_deref(), &req)?;
    let ops = parse_ops(&body.ops)?;
    let editor = Editor::of(&req);
    let errors = check_named(&ops, editor);
    if !errors.is_empty() {
        return Err(refused_keys(&errors));
    }

    let (text, _) = read_expected(&expected)?;
    let (changes, summary) = prompt(&text, &ops)?;
    let errors = check_changes(&changes, editor);
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

// ---------------------------------------------------------------------------
// A peer's node.toml.
//
// The admin peer (A) reads and edits another node's (B's) file over the
// mesh. B answers a read with exactly `view`, and runs an edit exactly as
// `patch_config` does, as an editor that may not change `[mesh]
// admin_peers`, once `mesh::admin` has admitted the statement A's operator
// confirmed for it. Both answer `{status, body}`: the status and body the
// same request made at B would have had, which A relays as its own answer.
// ---------------------------------------------------------------------------

/// A refusal as the `{status, body}` a peer relays.
async fn relayed(refused: Refused) -> Value {
    let response = refused.into_response();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    json!({ "status": status, "body": body })
}

/// B: an admin peer's `ConfigRead`, admitted.
pub(crate) async fn peer_read(s: &AppState) -> Value {
    match view(s) {
        Ok(body) => json!({ "status": 200, "body": body }),
        Err(refused) => relayed(refused).await,
    }
}

/// B: an admin peer's `ConfigPatch`, admitted (`mesh::admin::admit` checked
/// and spent its statement), audited as `admitted` and announced as every
/// save is.
pub(crate) async fn peer_patch(
    s: &AppState,
    expected: &str,
    raw: &[Value],
    admitted: &crate::mesh::admin::Admitted,
) -> Value {
    match peer_patch_here(s, expected, raw, admitted).await {
        Ok(body) => json!({ "status": 200, "body": body }),
        Err(refused) => relayed(refused).await,
    }
}

async fn peer_patch_here(
    s: &AppState,
    expected: &str,
    raw: &[Value],
    admitted: &crate::mesh::admin::Admitted,
) -> Result<Value, Refused> {
    // As a local PATCH: nothing a prompt could not show, decided before
    // anything else, here as on the node that confirmed it.
    refuse_hidden(&json!({ "expected_hash": expected, "ops": raw }))?;
    let ops = parse_ops(raw)?;
    let errors = check_named(&ops, Editor::Peer);
    if !errors.is_empty() {
        return Err(refused_keys(&errors));
    }
    let (text, _) = read_expected(expected)?;
    let (changes, _) = prompt(&text, &ops)?;
    let errors = check_changes(&changes, Editor::Peer);
    if !errors.is_empty() {
        return Err(refused_keys(&errors));
    }
    if changes.is_empty() && !names_a_secret(&ops) {
        return Ok(json!({ "changed": [], "hash": expected, "restart_required": false }));
    }
    let outcome = edit::edit_live(&s.cfg, s.store(), &ops, Some(expected), &admitted.actor())
        .map_err(edit_refusal)?;
    tracing::info!(actor = %admitted.actor(), summary = %admitted.summary, "node.toml edited by an admin peer");
    let (restart_required, pending) = restart_owed(s, &outcome).await;
    Ok(json!({
        "changed": outcome.changed(),
        "old_hash": outcome.old_hash,
        "hash": outcome.new_hash,
        "restart_required": restart_required,
        "pending_restart": pending,
        "audit_id": outcome.audit_id,
    }))
}

/// A refusal of a peer request in the shape every refusal has, with `kind`
/// for a client to act on.
fn peer_refusal(status: StatusCode, kind: &str, message: String) -> Refused {
    refusal(status, kind, message, &[])
}

/// A: send `command` to peer `node_id` and relay what it answered. Never
/// queued: a config command for a peer that is away is refused here, and one
/// that reaches it late is refused there (`not_after_ms`).
async fn on_peer(
    s: &AppState,
    node_id: &str,
    command: proto::frame::Command,
) -> Result<Value, Refused> {
    use crate::mesh::forward::CommandError;
    let mesh = s.mesh.as_ref().ok_or_else(|| {
        peer_refusal(
            StatusCode::CONFLICT,
            "no_hub",
            "no hub configured; this node cannot reach peers".into(),
        )
    })?;
    let known = s
        .store()
        .get_node(node_id)
        .ok()
        .flatten()
        .is_some_and(|row| row.is_self == 0);
    if !known {
        return Err(peer_refusal(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such peer".into(),
        ));
    }
    if !mesh.peer_reachable(node_id) {
        return Err(peer_refusal(
            StatusCode::GATEWAY_TIMEOUT,
            "unreachable",
            "that node is unreachable; its configuration is read and changed only while it is \
             connected, never queued, so try again when it returns"
                .into(),
        ));
    }
    let timeout = std::time::Duration::from_secs(s.cfg.mesh.command_timeout_secs.max(1));
    let answer = mesh
        .command(node_id, command, timeout)
        .await
        .map_err(|e| match e {
            CommandError::OlderPeer { .. } => {
                peer_refusal(StatusCode::CONFLICT, "older_build", e.to_string())
            }
            CommandError::Timeout => {
                peer_refusal(StatusCode::GATEWAY_TIMEOUT, "timeout", e.to_string())
            }
            CommandError::Refused(m) if m.contains("admin_peers") => {
                peer_refusal(StatusCode::FORBIDDEN, "not_admin_peer", m)
            }
            CommandError::Refused(m) if m.contains("older build") => {
                peer_refusal(StatusCode::CONFLICT, "older_build", m)
            }
            CommandError::Refused(m) => peer_refusal(StatusCode::FORBIDDEN, "refused", m),
            CommandError::Local(m) => {
                peer_refusal(StatusCode::INTERNAL_SERVER_ERROR, "internal", m)
            }
        })?;
    let status = answer["status"]
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .and_then(|n| StatusCode::from_u16(n).ok());
    match status {
        Some(StatusCode::OK) => Ok(answer["body"].clone()),
        Some(status) => Err(relay_refusal(
            status,
            &answer["body"],
            &super::peer_admin::peer_label(s, node_id),
        )),
        None => Err(peer_refusal(
            StatusCode::BAD_GATEWAY,
            "peer_error",
            "the peer's answer does not read".into(),
        )),
    }
}

/// The longest a peer's own words are relayed.
const MAX_PEER_MESSAGE: usize = 600;

/// A peer's refusal, as this node answers it: never its body as sent. A
/// peer is not this node, and what it says must not pass for this node's
/// own answer: a `401` or a `step_up_required` relayed whole would have this
/// node's interface open a passkey prompt the peer worded. So it is always
/// `peer_refused`, with the peer's message cleaned and capped beneath this
/// node's own line, the peer's kind and fields as data, and, for a
/// conflict, the peer's version (a hash, or nothing). Statuses an edit's
/// refusal has (`404`, `409`, `412`, `422`, `428`) are kept so a form can
/// act on them; any other becomes `403` (a refusal) or `502` (an error).
fn relay_refusal(status: StatusCode, body: &Value, peer: &str) -> Refused {
    let clean = |v: &Value, max: usize| {
        v.as_str()
            .map(|t| super::passkey::clean_text(t, max))
            .unwrap_or_default()
    };
    let error = &body["error"];
    let said = clean(&error["message"], MAX_PEER_MESSAGE);
    let status = match status.as_u16() {
        404 | 409 | 412 | 422 | 428 => status,
        400..=499 => StatusCode::FORBIDDEN,
        _ => StatusCode::BAD_GATEWAY,
    };
    let message = match said.is_empty() {
        true => format!("{peer} refused the request"),
        false => format!("{peer} refused: {said}"),
    };
    let mut out = json!({
        "code": status.as_u16(),
        "kind": "peer_refused",
        "message": message,
        "peer_kind": clean(&error["kind"], 64),
    });
    if let Some(fields) = error["fields"].as_array() {
        out["fields"] = fields
            .iter()
            .take(MAX_OPS)
            .map(|f| {
                json!({
                    "field": clean(&f["field"], 160),
                    "message": clean(&f["message"], 300),
                })
            })
            .collect();
    }
    let mut v = json!({ "error": out });
    if let Some(hash) = body["hash"]
        .as_str()
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        v["hash"] = json!(hash);
    }
    (status, Json(v)).into_response().into()
}

fn with_node(mut v: Value, s: &AppState, node_id: &str) -> Value {
    v["node"] = json!({
        "id": node_id,
        "name": super::peer_admin::peer_label(s, node_id),
        "is_self": node_id == s.node_id,
    });
    v
}

/// `GET /api/nodes/{id}/config`: that node's `node.toml`, as its own `GET
/// /api/config` shows it, read over the mesh. A peer answers only if its
/// `[mesh] admin_peers` names this node; otherwise the refusal says so
/// (`not_admin_peer`).
pub async fn get_node_config(
    State(s): State<AppState>,
    Path(node_id): Path<String>,
) -> Result<Json<Value>, Refused> {
    let v = match node_id == s.node_id {
        true => view(&s)?,
        false => on_peer(&s, &node_id, proto::frame::Command::ConfigRead).await?,
    };
    Ok(Json(with_node(v, &s, &node_id)))
}

/// `PATCH /api/nodes/{id}/config`: edit that node's `node.toml`, the body
/// as `PATCH /api/config` takes it.
///
/// For a peer, decided here first, against the file as the peer shows it
/// (`ConfigRead`): the request must read, the file must still be the version
/// it names, and it must change only keys a peer may (not `[mesh]
/// admin_peers`, nor a host-only key). The prompt lists what it changes there,
/// worked out here from that file, as `On <peer>: ...` within one prompt's
/// length. Then it is confirmed here (a passkey off this machine), this node
/// signs what was confirmed, and the peer runs it only if it names this node
/// in its `[mesh] admin_peers` and the statement holds for exactly this edit
/// on it (`mesh::admin`). The peer checks everything again, against its own
/// file and its own host.
pub async fn patch_node_config(
    State(s): State<AppState>,
    Path(node_id): Path<String>,
    req: AdminRequest,
) -> Result<Json<Value>, Refused> {
    if node_id == s.node_id {
        return patch_config(State(s), req).await;
    }
    let raw: Value = req.json()?;
    refuse_hidden(&raw)?;
    let body: PatchBody = serde_json::from_value(raw).map_err(|e| {
        refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            format!("the request body does not read: {e}"),
            &[],
        )
    })?;
    let expected = expected_version(body.expected_hash.as_deref(), &req)?;
    let ops = parse_ops(&body.ops)?;
    let errors = check_named(&ops, Editor::Peer);
    if !errors.is_empty() {
        return Err(refused_keys(&errors));
    }
    let current = on_peer(&s, &node_id, proto::frame::Command::ConfigRead).await?;
    let hash = current["hash"].as_str().unwrap_or_default();
    if hash != expected {
        return Err(conflict(hash));
    }
    let text = current["toml"].as_str().unwrap_or_default();
    let prefix = super::peer_admin::prefix(&s, &node_id);
    let budget = super::passkey::MAX_SUMMARY.saturating_sub(prefix.chars().count());
    // Checked here against the peer's text: a refusal names its file, which
    // is not at this node's path.
    let file = format!(
        "node.toml on {}",
        super::peer_admin::peer_label(&s, &node_id)
    );
    let (changes, line) = prompt_within(std::path::Path::new(&file), text, &ops, budget)?;
    let errors = check_changes(&changes, Editor::Peer);
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
    let summary = format!("{prefix}{line}");
    let op_hash = crate::mesh::admin::config_patch_op_hash(&node_id, &expected, &body.ops);
    let statement = super::peer_admin::confirm(&s, req.parts(), &node_id, &op_hash, &summary)?;
    let not_after_ms = statement.not_after_ms;
    let answer = on_peer(
        &s,
        &node_id,
        proto::frame::Command::ConfigPatch {
            expected_hash: expected,
            ops: body.ops,
            statement,
            not_after_ms,
        },
    )
    .await?;
    Ok(Json(with_node(answer, &s, &node_id)))
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
        assert!(check_named(&[op_set("publish.gh", s("/usr/bin/gh"))], Editor::Remote).is_empty());
        let errors = check_named(&[op_set("publish.ghh", s("x"))], Editor::Remote);
        assert!(
            errors[0].message.contains("not a node.toml key"),
            "{}",
            errors[0].message
        );
        // Inside a table set whole, and an entry inserted, too.
        let table = toml::Value::Table(toml::from_str("gh = \"gh\"\ntypo = 1").unwrap());
        let errors = check_named(&[op_set("publish", table)], Editor::Remote);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].key, "publish.typo");
        let entry: toml::Table = toml::from_str("path = \"/x\"\nmystery = 1").unwrap();
        let errors = check_named(
            &[Op::append(KeyPath::keys(&["repo"]), entry)],
            Editor::Remote,
        );
        assert_eq!(errors[0].key, "repo.mystery");
        // Removing a key nobody reads is how a typo is cleaned up.
        assert!(check_named(
            &[Op::remove(KeyPath::parse("qa.browser").unwrap())],
            Editor::Remote
        )
        .is_empty());
        // Host-only keys: refused from elsewhere; at the node, all but the
        // state mount may change.
        for key in [
            "gateway.harness_listen",
            "boundary.gateway_image",
            "boundary.harness_image",
            "runtime.kubernetes.harness_image",
        ] {
            assert_eq!(
                check_named(&[op_set(key, s("x"))], Editor::Remote).len(),
                1,
                "{key}"
            );
            assert!(
                check_named(&[op_set(key, s("x"))], Editor::Local).is_empty(),
                "{key}"
            );
        }
        let mount = op_set("runtime.kubernetes.state_mount", s("/elsewhere"));
        for editor in [Editor::Remote, Editor::Local, Editor::Peer] {
            let errors = check_named(std::slice::from_ref(&mount), editor);
            assert!(
                errors[0].message.contains("on the host"),
                "{}",
                errors[0].message
            );
        }
    }

    /// The SPA's screenshots and UI audit render the settings form from
    /// `spa/fixtures/config.json`; its schema is this one, so the form they
    /// show is the form a node serves. `TRACON_WRITE_FIXTURES=1` rewrites it.
    #[test]
    fn the_spa_fixture_describes_every_key_as_the_node_does() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../spa/fixtures/config.json");
        let text = std::fs::read_to_string(&path).expect("spa/fixtures/config.json");
        let mut fixture: Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<Value> = KEYS.iter().map(spec_json).collect();
        if std::env::var_os("TRACON_WRITE_FIXTURES").is_some() {
            fixture["keys"] = json!(keys);
            let mut out = serde_json::to_string_pretty(&fixture).unwrap();
            out.push('\n');
            std::fs::write(&path, out).unwrap();
            return;
        }
        assert!(
            fixture["keys"] == json!(keys),
            "spa/fixtures/config.json describes the keys differently from config_schema; \
             rewrite it with TRACON_WRITE_FIXTURES=1 cargo test -p tracon --lib the_spa_fixture"
        );
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
        assert!(check_changes(&[changed], Editor::Remote).is_empty());
        let mount = |v: Option<&str>| v.map(|v| toml::Value::String(v.into()));
        let errors = check_changes(
            &[change(
                "runtime.kubernetes.state_mount",
                mount(Some("/state")),
                None,
            )],
            Editor::Local,
        );
        assert!(
            errors[0].message.contains("on the host"),
            "{}",
            errors[0].message
        );
    }

    /// A peer this node lets administer it may change what a remote edit
    /// may, but never who may administer this node: not the key, not a
    /// table holding it, not by removing it.
    #[test]
    fn a_peer_never_changes_who_may_administer_the_node() {
        let peers = toml::Value::Array(vec![toml::Value::String("ab".repeat(32))]);
        let set = op_set("mesh.admin_peers", peers.clone());
        assert!(check_named(std::slice::from_ref(&set), Editor::Remote).is_empty());
        assert!(check_named(std::slice::from_ref(&set), Editor::Local).is_empty());
        let errors = check_named(std::slice::from_ref(&set), Editor::Peer);
        assert!(
            errors[0].message.contains("never by a peer"),
            "{}",
            errors[0].message
        );
        let removed = Op::remove(KeyPath::parse("mesh.admin_peers").unwrap());
        assert_eq!(check_named(&[removed], Editor::Peer).len(), 1);
        // A table set or removed whole that holds it, before or after.
        let mut table = toml::Table::new();
        table.insert("admin_peers".into(), peers.clone());
        let errors = check_changes(
            &[change(
                "mesh",
                Some(toml::Value::Table(table.clone())),
                None,
            )],
            Editor::Peer,
        );
        assert!(!errors.is_empty());
        let errors = check_changes(
            &[change("mesh", None, Some(toml::Value::Table(table)))],
            Editor::Peer,
        );
        assert!(!errors.is_empty());
        // The rest of [mesh] and [auth], and where credentials go, too; the
        // keys around them are a peer's to change.
        for key in [
            "mesh.hub_url",
            "auth.public_origin",
            "providers.anthropic.upstream",
            "embed.base_url",
        ] {
            let op = op_set(key, toml::Value::String("https://h.example".into()));
            assert_eq!(
                check_named(std::slice::from_ref(&op), Editor::Peer).len(),
                1,
                "{key}"
            );
            assert!(check_named(&[op], Editor::Remote).is_empty(), "{key}");
        }
        let models = op_set("review.max_files", toml::Value::Integer(3));
        assert!(check_named(&[models], Editor::Peer).is_empty());
        let image = op_set("boundary.harness_image", toml::Value::String("x".into()));
        assert_eq!(check_named(&[image], Editor::Peer).len(), 1);
    }

    #[test]
    fn operations_are_read_from_the_values_sent() {
        assert!(parse_ops(&[]).is_err());
        assert!(parse_ops(&[json!({ "op": "set", "key": "a", "value": 1, "x": 1 })]).is_err());
        assert_eq!(
            parse_ops(&[json!({ "op": "set", "key": "review.max_files", "value": 1 })])
                .unwrap_or_else(|_| panic!("reads"))
                .len(),
            1
        );
    }

    async fn relayed_body(r: Refused) -> (StatusCode, Value) {
        let response = r.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// A peer's refusal is never passed through as this node's own: a
    /// forged step-up or sign-in demand comes back as `peer_refused`, with
    /// no `step_up` for the interface to act on.
    #[tokio::test]
    async fn a_peers_refusal_is_relayed_as_its_and_never_as_a_step_up() {
        let forged = json!({
            "error": { "code": 403, "kind": "step_up_required", "message": "confirm\u{202e} this" },
            "step_up": { "challenge_id": "x", "options": {} },
        });
        for status in [StatusCode::FORBIDDEN, StatusCode::UNAUTHORIZED] {
            let (st, v) = relayed_body(relay_refusal(status, &forged, "beta")).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
            assert_eq!(v["error"]["kind"], "peer_refused", "{v}");
            assert_eq!(v["error"]["peer_kind"], "step_up_required", "{v}");
            assert!(v.get("step_up").is_none(), "{v}");
            assert_eq!(v["error"]["message"], "beta refused: confirm this");
        }
        let conflict = json!({
            "error": { "kind": "conflict", "message": "changed" },
            "hash": "ab".repeat(32),
        });
        let (st, v) = relayed_body(relay_refusal(StatusCode::CONFLICT, &conflict, "beta")).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(v["hash"], "ab".repeat(32));
        let bad_hash = json!({ "error": {}, "hash": "not a hash" });
        let (_, v) = relayed_body(relay_refusal(StatusCode::CONFLICT, &bad_hash, "beta")).await;
        assert!(v.get("hash").is_none(), "{v}");
        let (st, _) = relayed_body(relay_refusal(StatusCode::IM_A_TEAPOT, &json!({}), "b")).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _) = relayed_body(relay_refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({}),
            "b",
        ))
        .await;
        assert_eq!(st, StatusCode::BAD_GATEWAY);
    }
}
