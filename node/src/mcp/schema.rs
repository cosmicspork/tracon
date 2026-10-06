//! What an operator reviewing a held call is shown and what their edit is
//! checked against before it runs: the tool's own `inputSchema`, known
//! whether or not this node holds the credential the tool needs; how its
//! prose is written; and the arguments that say what the call acts on, which
//! an edit may not change.
//!
//! The validator covers the part of JSON Schema the tool definitions use:
//! `type`, `properties`, `required`, `items`, `enum`, `minimum` and `maximum`.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Serialize;
use serde_json::Value;

use super::{docs, github, gitlab, jira, memory, review, work};

/// Arguments that name the issue, project, repository or change a call acts
/// on, for every tool. Changing one is a different call, which the agent
/// should make.
pub const IDENTIFYING: &[&str] = &["key", "project", "repo", "number", "iid"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    /// Where in the arguments, as `labels[1]` or `parent`; empty for the
    /// arguments as a whole.
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.field.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.field, self.message)
        }
    }
}

/// How a held call is laid out for the operator, per tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Presentation {
    /// `jira_wiki` or `markdown`: how the prose fields are written.
    pub format: &'static str,
    /// The arguments that carry long prose, to render as a document.
    pub prose_fields: Vec<&'static str>,
    /// The arguments an edit may not change: [`IDENTIFYING`] and the tool's
    /// own.
    pub locked_fields: Vec<&'static str>,
}

/// `(tool, format, prose fields, locked fields beyond IDENTIFYING)`.
const PRESENTATIONS: &[(&str, &str, &[&str], &[&str])] = &[
    (jira::ISSUE_CREATE, "jira_wiki", &["description"], &[]),
    (jira::ISSUE_UPDATE, "jira_wiki", &["description"], &[]),
    (jira::ISSUE_COMMENT, "jira_wiki", &["body"], &[]),
    (github::PR_COMMENT, "markdown", &["body"], &[]),
    (github::PR_REPLY, "markdown", &["body"], &["thread_id"]),
    (gitlab::MR_COMMENT, "markdown", &["body"], &[]),
    (gitlab::MR_REPLY, "markdown", &["body"], &["discussion_id"]),
    (docs::DOC_WRITE, "markdown", &["body"], &["slug", "if_hash"]),
    (memory::RETAIN, "markdown", &["body"], &["kind"]),
    (work::BRIEF_NOTE, "markdown", &["text"], &[]),
    (work::CRITERIA_LINK, "markdown", &["value"], &["criterion"]),
    (review::SUBMIT_REPORT, "markdown", &["body"], &["report_id"]),
];

pub fn presentation(tool: &str) -> Presentation {
    let entry = PRESENTATIONS.iter().find(|(name, ..)| *name == tool);
    let (format, prose, locked) = entry
        .map(|(_, format, prose, locked)| (*format, *prose, *locked))
        .unwrap_or(("markdown", &[], &[]));
    Presentation {
        format,
        prose_fields: prose.to_vec(),
        locked_fields: IDENTIFYING.iter().chain(locked).copied().collect(),
    }
}

/// The input schema of a tool by name.
pub fn input_schema(tool: &str) -> Option<&'static Value> {
    static REGISTRY: OnceLock<HashMap<String, Value>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            [
                jira::definitions(),
                github::definitions(),
                gitlab::definitions(),
                super::consulta::definitions(&[]),
                docs::definitions(),
                memory::definitions(),
                super::operator::definitions(),
                work::definitions(),
                review::definitions(),
            ]
            .into_iter()
            .flatten()
            .filter_map(|mut d| {
                let name = d.get("name")?.as_str()?.to_string();
                Some((name, d.get_mut("inputSchema")?.take()))
            })
            .collect()
        })
        .get(tool)
}

/// Why the operator's `edited` arguments cannot replace `original` for
/// `tool`. Only what the operator changed is held against them: a field the
/// agent sent and the operator left alone is the call as it was asked.
pub fn check_edit(tool: &str, original: &Value, edited: &Value) -> Vec<FieldError> {
    let Some(edited_map) = edited.as_object() else {
        return vec![FieldError {
            field: String::new(),
            message: "the arguments must be an object".into(),
        }];
    };
    let locked = presentation(tool).locked_fields;
    let mut errors: Vec<FieldError> = locked
        .iter()
        .filter(|name| original.get(**name) != edited_map.get(**name))
        .map(|name| FieldError {
            field: (*name).to_string(),
            message: "names what this call acts on and cannot be edited; reject the call and \
                      ask for a new one instead"
                .into(),
        })
        .collect();
    if let Some(schema) = input_schema(tool) {
        let touched = |field: &str| {
            let top = field.split(['.', '[']).next().unwrap_or(field);
            top.is_empty() || original.get(top) != edited_map.get(top)
        };
        errors.extend(
            validate(schema, edited)
                .into_iter()
                .filter(|e| touched(&e.field) && !locked.contains(&e.field.as_str())),
        );
    }
    errors
}

/// Every way `value` falls outside `schema`.
pub fn validate(schema: &Value, value: &Value) -> Vec<FieldError> {
    let mut errors = Vec::new();
    check(schema, value, "", &mut errors);
    errors
}

fn check(schema: &Value, value: &Value, path: &str, errors: &mut Vec<FieldError>) {
    let mut fail = |message: String| {
        errors.push(FieldError {
            field: path.to_string(),
            message,
        })
    };
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        if !is_type(kind, value) {
            fail(format!("must be {}", describe(kind)));
            return;
        }
    }
    if let Some(n) = value.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
            if n < min {
                fail(format!("must be at least {min}"));
                return;
            }
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
            if n > max {
                fail(format!("must be at most {max}"));
                return;
            }
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            let names: Vec<String> = options.iter().map(Value::to_string).collect();
            fail(format!("must be one of {}", names.join(", ")));
            return;
        }
    }
    match value {
        Value::Object(map) => {
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            for name in &required {
                if map.get(*name).is_none_or(Value::is_null) {
                    errors.push(FieldError {
                        field: join(path, name),
                        message: "is required".into(),
                    });
                }
            }
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (name, item) in map {
                    let Some(property) = properties.get(name) else {
                        continue;
                    };
                    // An optional field sent as null is a field left out.
                    if item.is_null() && !required.contains(&name.as_str()) {
                        continue;
                    }
                    check(property, item, &join(path, name), errors);
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check(item_schema, item, &format!("{path}[{i}]"), errors);
                }
            }
        }
        _ => {}
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

fn is_type(kind: &str, value: &Value) -> bool {
    match kind {
        "string" => value.is_string(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn describe(kind: &str) -> &str {
    match kind {
        "string" => "text",
        "integer" => "a whole number",
        "number" => "a number",
        "boolean" => "true or false",
        "array" => "a list",
        "object" => "an object",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_prose_tool_has_a_schema_without_its_credential() {
        for tool in [
            "issue_create",
            "issue_update",
            "issue_comment",
            "pr_comment",
            "mr_comment",
            "doc_write",
        ] {
            assert!(input_schema(tool).is_some(), "{tool}");
        }
        for tool in [
            "retain",
            "brief_note",
            "criteria_link",
            "submit_report",
            "query",
        ] {
            assert!(input_schema(tool).is_some(), "{tool}");
        }
        assert_eq!(presentation("issue_create").format, "jira_wiki");
        assert_eq!(presentation("mr_comment").format, "markdown");
        let doc = presentation("doc_write");
        assert_eq!(doc.prose_fields, ["body"]);
        assert!(doc.locked_fields.contains(&"slug") && doc.locked_fields.contains(&"key"));
        let unknown = presentation("pipeline_run");
        assert_eq!(unknown.format, "markdown");
        assert!(unknown.prose_fields.is_empty());
        assert_eq!(unknown.locked_fields, IDENTIFYING);
    }

    #[test]
    fn the_subset_in_use_is_checked_with_a_path_to_each_fault() {
        let schema = input_schema("issue_create").unwrap();
        let ok = json!({ "project": "WRK", "type": "Task", "summary": "s", "labels": ["a"] });
        assert!(validate(schema, &ok).is_empty());
        let bad = json!({ "project": "WRK", "type": "Task", "labels": ["a", 2], "priority": 3 });
        let fields: Vec<String> = validate(schema, &bad)
            .into_iter()
            .map(|e| e.field)
            .collect();
        assert_eq!(fields, ["summary", "labels[1]", "priority"]);
        let schema = input_schema("doc_search").unwrap();
        let errors = validate(schema, &json!({ "query": "q", "kind": "nope" }));
        assert_eq!(errors[0].field, "kind");
        assert!(
            errors[0].message.contains("one of"),
            "{}",
            errors[0].message
        );
        assert!(validate(schema, &json!({ "query": "q", "limit": 3.0 })).is_empty());
        let schema = input_schema("retain").unwrap();
        let errors = validate(
            schema,
            &json!({ "kind": "fact", "body": "b", "confidence": 2 }),
        );
        assert_eq!(errors[0].field, "confidence");
    }

    #[test]
    fn an_edit_is_held_only_to_what_the_operator_changed() {
        let asked = json!({ "key": "WRK-1", "summary": 7 });
        let fixed = json!({ "key": "WRK-1", "summary": 7, "description": "d" });
        assert!(check_edit("issue_update", &asked, &fixed).is_empty());
        let broke = json!({ "key": "WRK-1", "summary": 7, "labels": "x" });
        assert_eq!(
            check_edit("issue_update", &asked, &broke)[0].field,
            "labels"
        );
    }

    #[test]
    fn an_edit_may_not_change_what_the_call_acts_on() {
        let asked = json!({ "key": "WRK-1", "body": "b" });
        let moved = json!({ "key": "WRK-2", "body": "b" });
        let errors = check_edit("issue_comment", &asked, &moved);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].field, "key");
        let dropped = json!({ "body": "b" });
        assert_eq!(
            check_edit("issue_comment", &asked, &dropped)[0].field,
            "key"
        );
        assert_eq!(
            check_edit("issue_comment", &asked, &json!("b"))[0].field,
            ""
        );
        let fact = json!({ "kind": "fact", "body": "b" });
        let lesson = json!({ "kind": "lesson", "body": "b" });
        assert_eq!(check_edit("retain", &fact, &lesson)[0].field, "kind");
    }
}
