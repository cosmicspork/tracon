//! Jira, as five narrow tools: search, read an issue, comment on it, edit its
//! fields, and create one. Transitions are not among them: moving a ticket's status
//! desyncs what the operator is actually working on, so the verb does not
//! exist here, and the fields the writes touch are named rather than passed
//! through. The API token never leaves the node.

use serde_json::{json, Map, Value};

use crate::{broker::SharedBroker, mcp::CallContext};

pub const CREDENTIAL: &str = "jira";
pub const ISSUE: &str = "issue";
pub const ISSUE_COMMENT: &str = "issue_comment";
pub const ISSUE_UPDATE: &str = "issue_update";
pub const ISSUE_CREATE: &str = "issue_create";
pub const ISSUE_SEARCH: &str = "issue_search";
pub const ISSUE_TRANSITION: &str = "issue_transition";

/// What a search row carries: enough to pick an issue, not to read it.
const SEARCH_FIELDS: &str = "summary,status,assignee,priority,issuetype,parent";

/// The fields either write may set. Status is deliberately absent; so is
/// everything else Jira would accept, because a field this list does not name
/// is one nobody decided an agent should write.
const EDITABLE: &[&str] = &["summary", "description", "priority", "labels", "parent"];

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": ISSUE_SEARCH,
            "description": "Search issues with JQL. One row per issue: key, type, summary, status, \
                            priority, assignee, parent. Read one in full with issue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "jql": { "type": "string", "description": "e.g. project = WRK AND statusCategory != Done ORDER BY updated DESC" },
                    "limit": { "type": "integer", "description": "At most 50; 20 unless you say." },
                },
                "required": ["jql"],
            },
        }),
        json!({
            "name": ISSUE,
            "description": "A Jira issue: summary, status, assignee, description, and its most \
                            recent comments.",
            "inputSchema": {
                "type": "object",
                "properties": { "key": { "type": "string", "description": "Issue key, e.g. WRK-123." } },
                "required": ["key"],
            },
        }),
        json!({
            "name": ISSUE_COMMENT,
            "description": "Post one comment on a Jira issue. The body is Jira wiki markup \
                            (*bold*, {{code}}, * bullets), not Markdown. Status changes are the \
                            operator's; say what you would transition and why instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string" },
                    "body": { "type": "string" },
                },
                "required": ["key", "body"],
            },
        }),
        json!({
            "name": ISSUE_UPDATE,
            "description": "Edit an issue's fields: summary, description, priority, labels, and \
                            parent. Status is not a field here, so say what you would transition \
                            and why with issue_comment.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Issue key, e.g. WRK-123." },
                    "summary": { "type": "string" },
                    "description": { "type": "string" },
                    "priority": { "type": "string", "description": "Priority name, e.g. High." },
                    "labels": { "type": "array", "items": { "type": "string" },
                                "description": "Replaces the issue's labels. Single tokens, no spaces." },
                    "parent": { "type": "string", "description": "Parent issue key." },
                },
                "required": ["key"],
            },
        }),
        json!({
            "name": ISSUE_CREATE,
            "description": "Create an issue in a project. Returns its key and a link to it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project key, e.g. WRK." },
                    "type": { "type": "string", "description": "Issue type name, e.g. Task." },
                    "summary": { "type": "string" },
                    "description": { "type": "string" },
                    "priority": { "type": "string" },
                    "labels": { "type": "array", "items": { "type": "string" } },
                    "parent": { "type": "string", "description": "Parent issue key." },
                },
                "required": ["project", "type", "summary"],
            },
        }),
        json!({
            "name": ISSUE_TRANSITION,
            "description": "Transition a Jira issue using a concrete transition id. Requires current scoped authority for that issue.",
            "inputSchema": { "type": "object", "properties": {
                "key": { "type": "string" }, "transition_id": { "type": "string" },
                "operation_id": { "type": "string" }
            }, "required": ["key", "transition_id", "operation_id"] },
        }),
    ]
}

pub async fn call(
    broker: &SharedBroker,
    http: &reqwest::Client,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let env = broker
        .read()
        .unwrap()
        .env_for(CREDENTIAL, &ctx.channel, &ctx.node_id)
        .map_err(|e| e.to_string())?;
    let url = env
        .get("JIRA_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .ok_or("credential jira has no JIRA_URL")?;
    let email = env
        .get("JIRA_EMAIL")
        .ok_or("credential jira has no JIRA_EMAIL")?;
    let token = env
        .get("JIRA_TOKEN")
        .ok_or("credential jira has no JIRA_TOKEN")?;
    match name {
        ISSUE_SEARCH => {
            let jql = args
                .get("jql")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|j| !j.is_empty())
                .ok_or("jql is required")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(20)
                .clamp(1, 50)
                .to_string();
            let query = [
                ("jql", jql),
                ("fields", SEARCH_FIELDS),
                ("maxResults", limit.as_str()),
            ];
            let search = |path: &str| {
                http.get(format!("{url}{path}"))
                    .query(&query)
                    .basic_auth(email, Some(token))
                    .send()
            };
            // Cloud answers only the newer endpoint; Data Center only the
            // older one. Both return the same rows.
            let mut res = search("/rest/api/3/search/jql")
                .await
                .map_err(|e| format!("jira: {e}"))?;
            if res.status() == reqwest::StatusCode::NOT_FOUND {
                res = search("/rest/api/2/search")
                    .await
                    .map_err(|e| format!("jira: {e}"))?;
            }
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(refusal("jira refused the search", status, &v));
            }
            let issues: Vec<Value> = v["issues"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|i| {
                            let f = &i["fields"];
                            json!({
                                "key": i["key"],
                                "type": f["issuetype"]["name"],
                                "summary": f["summary"],
                                "status": f["status"]["name"],
                                "priority": f["priority"]["name"],
                                "assignee": f["assignee"]["displayName"],
                                "parent": f["parent"]["key"],
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({ "issues": issues }))
        }
        ISSUE => {
            let key = issue_key(args.get("key"), "key")?;
            let res = http
                .get(format!(
                    "{url}/rest/api/2/issue/{key}\
                     ?fields=summary,status,assignee,description,comment,issuetype,priority"
                ))
                .basic_auth(email, Some(token))
                .send()
                .await
                .map_err(|e| format!("jira: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(refusal("jira answered", status, &v));
            }
            let f = &v["fields"];
            let comments: Vec<Value> = f["comment"]["comments"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .rev()
                        .take(10)
                        .map(|c| {
                            json!({
                                "author": c["author"]["displayName"],
                                "created": c["created"],
                                "body": c["body"],
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({
                "key": v["key"],
                "type": f["issuetype"]["name"],
                "summary": f["summary"],
                "status": f["status"]["name"],
                "priority": f["priority"]["name"],
                "assignee": f["assignee"]["displayName"],
                "description": f["description"],
                "comments": comments,
            }))
        }
        ISSUE_COMMENT => {
            let key = issue_key(args.get("key"), "key")?;
            let body = args
                .get("body")
                .and_then(Value::as_str)
                .filter(|b| !b.trim().is_empty())
                .ok_or("body is required")?;
            let res = http
                .post(format!("{url}/rest/api/2/issue/{key}/comment"))
                .basic_auth(email, Some(token))
                .json(&json!({ "body": body }))
                .send()
                .await
                .map_err(|e| format!("jira: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(refusal("jira refused the comment", status, &v));
            }
            Ok(json!({ "id": v["id"], "created": v["created"] }))
        }
        ISSUE_UPDATE => {
            let key = issue_key(args.get("key"), "key")?;
            let fields = fields_from(args, ISSUE_UPDATE, &[])?;
            if fields.is_empty() {
                return Err(format!(
                    "nothing to update: name at least one of {}",
                    EDITABLE.join(", ")
                ));
            }
            let named: Vec<String> = fields.keys().cloned().collect();
            let res = http
                .put(format!("{url}/rest/api/2/issue/{key}"))
                .basic_auth(email, Some(token))
                .json(&json!({ "fields": fields }))
                .send()
                .await
                .map_err(|e| format!("jira: {e}"))?;
            let status = res.status();
            if !status.is_success() {
                let v: Value = res.json().await.unwrap_or(Value::Null);
                return Err(refusal("jira refused the update", status, &v));
            }
            // A successful edit answers 204 with no body: there is nothing to
            // read back but what was asked for.
            Ok(json!({ "key": key, "updated": named }))
        }
        ISSUE_CREATE => {
            let project = args
                .get("project")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|p| {
                    !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .ok_or("project is required (a project key, e.g. WRK)")?;
            let kind = args
                .get("type")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or("type is required (an issue type name, e.g. Task)")?;
            let mut fields = fields_from(args, ISSUE_CREATE, &["project", "type"])?;
            if !fields.contains_key("summary") {
                return Err("summary is required".into());
            }
            fields.insert("project".into(), json!({ "key": project }));
            fields.insert("issuetype".into(), json!({ "name": kind }));
            let res = http
                .post(format!("{url}/rest/api/2/issue"))
                .basic_auth(email, Some(token))
                .json(&json!({ "fields": fields }))
                .send()
                .await
                .map_err(|e| format!("jira: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(refusal("jira refused the new issue", status, &v));
            }
            let key = v["key"].as_str().unwrap_or_default().to_string();
            Ok(json!({ "key": key, "id": v["id"], "url": format!("{url}/browse/{key}") }))
        }
        ISSUE_TRANSITION => {
            let key = issue_key(args.get("key"), "key")?;
            let transition = args.get("transition_id").and_then(Value::as_str).map(str::trim)
                .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
                .ok_or("transition_id is required")?;
            let res = http
                .post(format!("{url}/rest/api/2/issue/{key}/transitions"))
                .basic_auth(email, Some(token))
                .json(&json!({ "transition": { "id": transition } }))
                .send().await.map_err(|e| format!("mutation-outcome-unknown: jira: {e}"))?;
            let status = res.status();
            if !status.is_success() {
                let v: Value = res.json().await.unwrap_or(Value::Null);
                let error = refusal("jira refused the transition", status, &v);
                return Err(if status.is_server_error() {
                    format!("mutation-outcome-unknown: {error}")
                } else { error });
            }
            Ok(json!({ "key": key, "transition_id": transition }))
        }
        other => Err(format!("no jira tool named {other}")),
    }
}

/// An issue key, validated before it reaches a URL or a field.
fn issue_key<'a>(v: Option<&'a Value>, what: &str) -> Result<&'a str, String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .ok_or_else(|| format!("{what} is required (e.g. WRK-123)"))
}

/// The `fields` object for a write, from the arguments the tool was given.
///
/// An argument outside the allowlist is refused rather than dropped: an agent
/// that asked to close a ticket should be told the verb does not exist here,
/// not watch the call succeed at doing less than it said.
fn fields_from(
    args: &Value,
    verb: &str,
    structural: &[&str],
) -> Result<Map<String, Value>, String> {
    let mut fields = Map::new();
    for (k, v) in args.as_object().into_iter().flatten() {
        if k == "key" || structural.contains(&k.as_str()) {
            continue;
        }
        if !EDITABLE.contains(&k.as_str()) {
            return Err(format!(
                "{verb} does not set {k}; it writes only {}. A status change is the operator's — \
                 say what you would transition and why with {ISSUE_COMMENT}.",
                EDITABLE.join(", ")
            ));
        }
        match k.as_str() {
            "summary" | "description" => {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("{k} expects a string"))?
                    .trim();
                if k == "summary" && s.is_empty() {
                    return Err("summary cannot be empty".into());
                }
                fields.insert(k.clone(), json!(s));
            }
            "priority" => {
                let p = v
                    .as_str()
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .ok_or("priority expects a name, e.g. High")?;
                fields.insert(k.clone(), json!({ "name": p }));
            }
            "labels" => {
                let items = v
                    .as_array()
                    .ok_or("labels expects a list of strings")?
                    .iter()
                    .map(|l| {
                        let l = l.as_str().ok_or("labels expects a list of strings")?.trim();
                        if l.is_empty() {
                            return Err("a label cannot be empty".to_string());
                        }
                        if l.chars().any(char::is_whitespace) {
                            return Err(format!(
                                "label {l:?} has whitespace; a Jira label is a single token"
                            ));
                        }
                        Ok(json!(l))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                fields.insert(k.clone(), json!(items));
            }
            "parent" => {
                let p = issue_key(Some(v), "parent")?;
                fields.insert(k.clone(), json!({ "key": p }));
            }
            _ => unreachable!("every editable field is handled"),
        }
    }
    Ok(fields)
}

/// What Jira said, including the per-field messages: `errorMessages` carries
/// the general refusal and `errors` the one naming the field that was wrong.
/// A rejected parent or an unknown priority only ever appears in the latter.
fn refusal(what: &str, status: reqwest::StatusCode, v: &Value) -> String {
    let general = v["errorMessages"].to_string();
    let fields = &v["errors"];
    if fields.is_null() || fields.as_object().is_some_and(|o| o.is_empty()) {
        format!("{what} ({status}): {general}")
    } else {
        format!("{what} ({status}): {general} {fields}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_the_tools_do_not_write_is_refused_by_name() {
        let e = fields_from(
            &json!({ "key": "WRK-1", "status": "Done" }),
            ISSUE_UPDATE,
            &[],
        )
        .unwrap_err();
        assert!(e.contains("status"), "{e}");
        assert!(e.contains(ISSUE_COMMENT), "{e}");
    }

    #[test]
    fn a_label_is_a_single_token_and_a_parent_is_a_key() {
        let e = fields_from(&json!({ "labels": ["needs review"] }), ISSUE_UPDATE, &[]).unwrap_err();
        assert!(e.contains("whitespace"), "{e}");
        let e = fields_from(&json!({ "parent": "not a key" }), ISSUE_UPDATE, &[]).unwrap_err();
        assert!(e.contains("parent"), "{e}");
    }

    #[test]
    fn the_written_fields_carry_jiras_own_shapes() {
        let f = fields_from(
            &json!({ "key": "WRK-1", "priority": "High", "parent": "WRK-9", "labels": ["a", "b"] }),
            ISSUE_UPDATE,
            &[],
        )
        .unwrap();
        assert_eq!(f["priority"], json!({ "name": "High" }));
        assert_eq!(f["parent"], json!({ "key": "WRK-9" }));
        assert_eq!(f["labels"], json!(["a", "b"]));
        assert!(!f.contains_key("key"));
    }
}
