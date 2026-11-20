//! Names on the tool surface: an agent and the operator share one handle for
//! a record. Outputs carry `name` beside every session, review, report,
//! approval and task `id` (and `<kind>_name` beside a `<kind>_id`), and id
//! arguments accept a name, a generated name or a long enough prefix. A name
//! several records answer to is refused with the candidates, never guessed.
//!
//! Only this node's own tools: a plugin's or another server's arguments and
//! results are theirs, and pass through untouched.

use serde_json::{Map, Value};

use super::work;
use crate::names::{self, is_full_id, kind, Resolved};
use crate::store::Store;

/// The record kind an argument names, by its key, for every tool here.
fn kind_of_arg(key: &str) -> Option<&'static str> {
    Some(match key {
        "session_id" => kind::SESSION,
        "review_id" | "review_ids" => kind::REVIEW,
        "report_id" | "report_ids" => kind::REPORT,
        "approval_id" | "approval_ids" => kind::APPROVAL,
        _ => return None,
    })
}

/// A work tool's `id` and `deps` name tasks.
fn task_args(tool: &str) -> &'static [&'static str] {
    match tool {
        work::WORK_SHOW | work::WORK_DELETE | work::WORK_CLOSE => &["id"],
        work::WORK_UPDATE => &["id", "deps"],
        work::WORK_DISCOVER => &["deps"],
        _ => &[],
    }
}

/// One argument value, resolved: a full id stays, anything else that names
/// exactly one record becomes its id, and anything that names none is left
/// for the tool to judge as it always has (a short prefix, say).
fn resolve_one(store: &Store, channel: &str, kind: &str, given: &str) -> Result<String, String> {
    if is_full_id(given) {
        return Ok(given.to_string());
    }
    match names::resolve(store, Some(kind), given, Some(channel)).map_err(|e| e.to_string())? {
        Resolved::One(hit) => Ok(hit.id),
        Resolved::None => Ok(given.to_string()),
        Resolved::Many(hits) => Err(names::ambiguous(given.trim(), &hits)),
    }
}

fn resolve_value(
    store: &Store,
    channel: &str,
    kind: &str,
    value: &mut Value,
    changed: &mut bool,
) -> Result<(), String> {
    match value {
        Value::String(s) => {
            let id = resolve_one(store, channel, kind, s)?;
            if id != *s {
                *s = id;
                *changed = true;
            }
        }
        Value::Array(items) => {
            for item in items {
                resolve_value(store, channel, kind, item, changed)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `args` with every id argument that is a name replaced by the id it
/// names, or `None` when nothing changed.
pub(super) fn resolve_args(
    store: &Store,
    channel: &str,
    tool: &str,
    args: &Value,
) -> Result<Option<Value>, String> {
    let Some(obj) = args.as_object() else {
        return Ok(None);
    };
    let mut out = obj.clone();
    let mut changed = false;
    for (key, value) in out.iter_mut() {
        let kind = if task_args(tool).contains(&key.as_str()) {
            kind::TASK
        } else if let Some(kind) = kind_of_arg(key) {
            kind
        } else {
            continue;
        };
        resolve_value(store, channel, kind, value, &mut changed)?;
    }
    Ok(changed.then_some(Value::Object(out)))
}

/// The name an output gives `id`, when it is a session, review, report,
/// approval or task this node holds.
fn name_for(store: &Store, id: &str) -> Option<String> {
    if !is_full_id(id) {
        return None;
    }
    let kind = store.kind_of_record(id).ok().flatten()?;
    Some(names::display(store, &kind, id))
}

/// The key a `<kind>_id` field's name goes under.
fn name_key(key: &str) -> Option<String> {
    let stem = key.strip_suffix("_id")?;
    matches!(
        stem,
        "session" | "review" | "report" | "approval" | "work_item" | "task"
    )
    .then(|| format!("{stem}_name"))
}

fn annotate_object(store: &Store, obj: &mut Map<String, Value>) {
    let mut add: Vec<(String, String)> = Vec::new();
    for (key, value) in obj.iter() {
        let Some(id) = value.as_str() else { continue };
        let target = if key == "id" {
            "name".to_string()
        } else if let Some(k) = name_key(key) {
            k
        } else {
            continue;
        };
        if obj.contains_key(&target) {
            continue;
        }
        if let Some(name) = name_for(store, id) {
            add.push((target, name));
        }
    }
    for (k, v) in add {
        obj.insert(k, Value::String(v));
    }
    for value in obj.values_mut() {
        annotate(store, value);
    }
}

/// Put `name` beside every record id in a tool's output.
pub(super) fn annotate(store: &Store, value: &mut Value) {
    match value {
        Value::Object(obj) => annotate_object(store, obj),
        Value::Array(items) => {
            for item in items {
                annotate(store, item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_name_key_is_only_for_named_kinds() {
        assert_eq!(name_key("session_id").as_deref(), Some("session_name"));
        assert_eq!(name_key("work_item_id").as_deref(), Some("work_item_name"));
        assert_eq!(name_key("doc_id"), None);
        assert_eq!(name_key("id"), None);
    }

    #[test]
    fn outputs_carry_names_and_arguments_accept_them() {
        let store = Store::open_in_memory().unwrap();
        let tid = "ab".repeat(32);
        store
            .write_change(
                "n1",
                "personal",
                "work_item",
                tracon_sync::ChangeOp::Upsert,
                &tid,
                json!({"channel": "personal", "title": "Ship it", "body": "", "state": "open",
                       "priority": 0, "deps_json": "[]", "created_ms": 1, "updated_ms": 1}),
            )
            .unwrap();
        let task = names::generated("task", &tid);
        let mut out = json!({
            "id": tid,
            "items": [{ "task_id": tid, "name": "kept" }],
            "other": "not-an-id",
            "doc_id": tid,
        });
        annotate(&store, &mut out);
        assert_eq!(out["name"], task);
        assert_eq!(out["items"][0]["task_name"], task);
        assert_eq!(out["items"][0]["name"], "kept");
        assert!(out.get("doc_name").is_none());

        let resolved = resolve_args(
            &store,
            "personal",
            work::WORK_UPDATE,
            &json!({ "id": task, "deps": [task.to_uppercase()] }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolved["id"], tid.as_str());
        assert_eq!(resolved["deps"][0], tid.as_str());
        // A full id, a key that names no record, or an unknown handle is left
        // alone, for the tool to judge.
        for args in [
            json!({ "id": tid }),
            json!({ "title": "x" }),
            json!({ "id": "abab" }),
        ] {
            assert!(resolve_args(&store, "personal", work::WORK_SHOW, &args)
                .unwrap()
                .is_none());
        }
        // `id` is a task only on the work tools.
        assert!(
            resolve_args(&store, "personal", "doc_read", &json!({ "id": task }))
                .unwrap()
                .is_none()
        );
    }
}
