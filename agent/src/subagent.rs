//! Subagents: a task handed to a child conversation that works with the same
//! tools, asks the node before each call like its parent, and hands back an
//! object shaped by a schema the parent chose, not prose.
//!
//! Children run in this process, on the session's gateway key, so the
//! parent's budget pays for them and each of their requests is counted and
//! matched like the parent's. Several spawned in one response run at once.
//! A child cannot spawn: one level only. A child's model may differ from the
//! parent's, but only among those the node declares under the same provider.

use serde_json::{json, Value};

pub const SPAWN: &str = "spawn";
pub const FINISH: &str = "finish";
/// Children of one response running at once; the rest wait their turn.
pub const MAX_PARALLEL: usize = 4;
/// A child's model requests, at most, before it is stopped.
pub const CHILD_MAX_TURNS: u32 = 60;

/// Put before the parent's system prompt for a child.
pub const CHILD_ORIENTATION: &str = "\
You are a subagent: another agent handed you one task, below, and is waiting for its result. \
Work on that task alone, with the tools you have; you cannot hand work on to further agents. \
When it is done, or cannot be done, call `finish` once with the result in the shape it asks \
for; that ends your work, and nothing you write outside `finish` reaches the agent waiting \
for you.";

/// What a child returns when the parent names no shape.
pub fn default_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "description": "What was found or done, with the specifics the caller needs."
            }
        },
        "required": ["summary"]
    })
}

pub fn spawn_spec() -> Value {
    json!({
        "name": SPAWN,
        "description": "Hand a self-contained task to a subagent, which works on it with the same \
            tools and returns an object in the shape you give. Several spawn calls in one \
            response run at the same time: use them for independent pieces of work, such as \
            investigating separate parts of the code. The subagent starts with nothing of this \
            conversation, so say in `task` everything it needs.",
        "parameters": {
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task, complete in itself: what to do, where, and what to report."
                },
                "model": {
                    "type": "string",
                    "description": "A model id under this session's provider, for a cheaper or stronger \
                        subagent. Default: your own model."
                },
                "result_schema": {
                    "type": "object",
                    "description": "JSON Schema (an object type) for the result. Default: {summary: string}."
                }
            },
            "required": ["task"]
        },
        // Its `result_schema` is any schema, which the strict subset cannot
        // describe.
        "own_schema": false,
    })
}

pub fn finish_spec(schema: &Value) -> Value {
    json!({
        "name": FINISH,
        "description": "Return your result to the agent that gave you the task. Call it once, \
            when the task is done or cannot be done; it ends your work.",
        "parameters": schema,
        "own_schema": true,
    })
}

/// `value` without the `null`s a strict wire has the model send for the
/// properties it leaves out (`strict_schema` makes every one required and
/// nullable): a property the parent's schema left optional and does not
/// itself allow to be null is dropped, at any depth, before the result is
/// checked against that schema and returned.
pub fn settle(schema: &Value, value: &Value) -> Value {
    match value {
        Value::Object(o) => {
            let required: Vec<&str> = schema["required"]
                .as_array()
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let out = o
                .iter()
                .filter(|(k, v)| {
                    !(v.is_null()
                        && !required.contains(&k.as_str())
                        && !allows_null(&schema["properties"][k.as_str()]))
                })
                .map(|(k, v)| (k.clone(), settle(&schema["properties"][k.as_str()], v)))
                .collect();
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| settle(&schema["items"], v)).collect())
        }
        _ => value.clone(),
    }
}

fn allows_null(schema: &Value) -> bool {
    match &schema["type"] {
        Value::String(t) => t == "null",
        Value::Array(ts) => ts.iter().any(|t| t == "null"),
        // No type: anything, null included.
        _ => true,
    }
}

/// Whether `value` has the shape `schema` describes, as far as the subset
/// models are given here goes: types, required properties, closed objects,
/// array items and enums. The error says where it does not.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, value, "result")
}

fn check(schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    if let Some(options) = schema["enum"].as_array() {
        if !options.contains(value) {
            return Err(format!(
                "{at} must be one of {}",
                Value::Array(options.clone())
            ));
        }
    }
    let types: Vec<&str> = match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
        _ => return Ok(()),
    };
    let fits = types.iter().any(|t| match *t {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    });
    if !fits {
        return Err(format!("{at} must be {}, not {value}", types.join(" or ")));
    }
    match value {
        Value::Object(o) => {
            for r in schema["required"].as_array().into_iter().flatten() {
                let Some(r) = r.as_str() else { continue };
                if !o.contains_key(r) {
                    return Err(format!("{at} is missing `{r}`"));
                }
            }
            let properties = schema["properties"].as_object();
            for (k, v) in o {
                match properties.and_then(|p| p.get(k)) {
                    Some(s) => check(s, v, &format!("{at}.{k}"))?,
                    None if schema["additionalProperties"] == false => {
                        return Err(format!("{at} has `{k}`, which the schema does not allow"))
                    }
                    None => {}
                }
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                check(&schema["items"], v, &format!("{at}[{i}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The models a child may be given: the parent's own, and those the node
/// declares under the same provider, with their windows.
#[derive(Debug, Clone, Default)]
pub struct Spawner {
    pub model: String,
    pub window: Option<u64>,
    pub siblings: Vec<(String, Option<u64>)>,
}

impl Spawner {
    /// The window of `model`, if a child may use it.
    pub fn allows(&self, model: &str) -> Option<Option<u64>> {
        if model == self.model {
            return Some(self.window);
        }
        self.siblings
            .iter()
            .find(|(id, _)| id == model)
            .map(|(_, w)| *w)
    }

    pub fn models(&self) -> Vec<&str> {
        let mut out = vec![self.model.as_str()];
        out.extend(
            self.siblings
                .iter()
                .map(|(id, _)| id.as_str())
                .filter(|id| *id != self.model),
        );
        out
    }
}

/// A short name for a child in what the operator sees: its task's start.
pub fn label(task: &str) -> String {
    let line = task.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(40).collect();
    if line.chars().count() > 40 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_is_checked_against_its_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "files": {"type": "array", "items": {"type": "string"}},
                "verdict": {"type": "string", "enum": ["ok", "broken"]},
                "count": {"type": "integer"},
                "note": {"type": ["string", "null"]}
            },
            "required": ["files", "verdict"],
            "additionalProperties": false
        });
        assert_eq!(
            validate(
                &schema,
                &json!({"files": ["a.py"], "verdict": "ok", "note": null})
            ),
            Ok(())
        );
        let e = |v| validate(&schema, &v).unwrap_err();
        assert_eq!(e(json!({"files": []})), "result is missing `verdict`");
        assert!(
            e(json!({"files": [1], "verdict": "ok"})).starts_with("result.files[0] must be string")
        );
        assert!(e(json!({"files": [], "verdict": "meh"})).contains("must be one of"));
        assert!(e(json!({"files": [], "verdict": "ok", "count": 1.5})).contains("integer"));
        assert!(e(json!({"files": [], "verdict": "ok", "extra": 1})).contains("does not allow"));
        assert!(e(json!("text")).starts_with("result must be object"));
        assert_eq!(
            validate(&default_schema(), &json!({"summary": "done"})),
            Ok(())
        );
    }

    #[test]
    fn a_strict_wires_nulls_for_optional_properties_are_dropped() {
        let schema = json!({
            "type": "object",
            "properties": {
                "verdict": {"type": "string"},
                "note": {"type": "string"},
                "maybe": {"type": ["string", "null"]},
                "items": {"type": "array", "items": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "line": {"type": "integer"}},
                    "required": ["path"]
                }}
            },
            "required": ["verdict"]
        });
        let sent = json!({"verdict": "ok", "note": null, "maybe": null,
            "items": [{"path": "a.rs", "line": null}]});
        let settled = settle(&schema, &sent);
        assert_eq!(
            settled,
            json!({"verdict": "ok", "maybe": null, "items": [{"path": "a.rs"}]})
        );
        assert_eq!(validate(&schema, &settled), Ok(()));
        // A required property's null is the model's answer, and is refused.
        let missing = settle(&schema, &json!({"verdict": null}));
        assert!(validate(&schema, &missing)
            .unwrap_err()
            .starts_with("result.verdict must be string"));
    }

    #[test]
    fn a_child_may_use_the_providers_models_only() {
        let s = Spawner {
            model: "gpt-6".into(),
            window: Some(400_000),
            siblings: vec![("gpt-6".into(), Some(400_000)), ("gpt-6-mini".into(), None)],
        };
        assert_eq!(s.allows("gpt-6"), Some(Some(400_000)));
        assert_eq!(s.allows("gpt-6-mini"), Some(None));
        assert_eq!(s.allows("claude-opus-5"), None);
        assert_eq!(s.models(), ["gpt-6", "gpt-6-mini"]);
    }

    #[test]
    fn a_label_is_the_tasks_start() {
        assert_eq!(label("Find the bug\nin parser.rs"), "Find the bug");
        assert_eq!(label(&"x".repeat(50)).chars().count(), 41);
    }
}
