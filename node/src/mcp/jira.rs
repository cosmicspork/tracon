//! Jira, as narrow tools: search, read an issue, comment on it, edit its
//! fields, create one, link two, and transition one. A transition moves what the
//! operator is actually working on, so it runs only with current scoped
//! authority for that issue; the fields the other writes touch are named
//! rather than passed through. The API token never leaves the node.

use serde_json::{json, Map, Value};

use crate::{broker::SharedBroker, mcp::CallContext};

pub const CREDENTIAL: &str = "jira";
pub const ISSUE: &str = "issue";
pub const ISSUE_COMMENT: &str = "issue_comment";
pub const ISSUE_UPDATE: &str = "issue_update";
pub const ISSUE_CREATE: &str = "issue_create";
pub const ISSUE_LINK: &str = "issue_link";
pub const ISSUE_SEARCH: &str = "issue_search";
pub const ISSUE_TRANSITION: &str = "issue_transition";
pub const ISSUE_TRANSITIONS: &str = "issue_transitions";

/// What a search row carries: enough to pick an issue, not to read it.
const SEARCH_FIELDS: &str = "summary,status,assignee,priority,issuetype,parent,labels";

/// What `issue` reads: enough to write to the issue safely, since a label
/// list, a parent or a link is what a write would otherwise have to guess.
const ISSUE_FIELDS: &str = "summary,status,assignee,description,comment,issuetype,priority,\
                            labels,parent,issuelinks,subtasks,created,updated";

/// The fields either write may set. Status is deliberately absent; so is
/// everything else Jira would accept, because a field this list does not name
/// is one nobody decided an agent should write.
const EDITABLE: &[&str] = &["summary", "description", "priority", "labels", "parent"];

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": ISSUE_SEARCH,
            "description": "Search issues with JQL. One row per issue: key, type, summary, status, \
                            priority, assignee, parent, labels. Read one in full with issue.",
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
            "description": "A Jira issue: summary, status, assignee, description, labels, \
                            parent, links to other issues (each with its direction and wording, \
                            e.g. \"is blocked by\"), subtasks, when it was created and updated, \
                            and its most recent comments.",
            "inputSchema": {
                "type": "object",
                "properties": { "key": { "type": "string", "description": "Issue key, e.g. WRK-123." } },
                "required": ["key"],
            },
        }),
        json!({
            "name": ISSUE_COMMENT,
            "description": format!(
                "Post one comment on a Jira issue. The body is Jira wiki markup (*bold*, \
                 {{{{code}}}}, * bullets), not Markdown. Status changes are issue_transition, not \
                 a comment. Not for GitHub issues: that is github_issue_comment. {}",
                super::aside::ASKED
            ),
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
            "description": format!(
                "Edit an issue's fields: summary, description, priority, labels, and parent. \
                 Status is not a field here; it changes only through issue_transition. {}",
                super::aside::ASKED
            ),
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
            "description": format!(
                "Create an issue in a project. Once it is allowed and has run, approval_status's \
                 result carries its key and a link to it. {}",
                super::aside::ASKED
            ),
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
            "name": ISSUE_LINK,
            "description": format!(
                "Link two Jira issues, read in the type's outward wording: from blocks to, for \
                 type Blocks. type is the link type's name as Jira lists it (Blocks, Relates, \
                 ...); one this Jira does not have is refused with the names it does. {}",
                super::aside::ASKED
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Issue key, e.g. WRK-123." },
                    "to": { "type": "string", "description": "Issue key, e.g. WRK-124." },
                    "type": { "type": "string", "description": "Link type name, e.g. Blocks." },
                },
                "required": ["from", "to", "type"],
            },
        }),
        json!({
            "name": ISSUE_TRANSITIONS,
            "description": "The transitions a Jira issue offers right now: each one's id, name, \
                            and the status it leads to. The id is what issue_transition takes; \
                            ids differ between projects and workflows, so read them here.",
            "inputSchema": {
                "type": "object",
                "properties": { "key": { "type": "string", "description": "e.g. WRK-123" } },
                "required": ["key"],
            },
        }),
        json!({
            "name": ISSUE_TRANSITION,
            "description": "Transition a Jira issue using a concrete transition id, as \
                            issue_transitions lists them. Requires current scoped authority for that issue.",
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
    before_mutation: Option<&(dyn Fn() -> Result<(), String> + Send + Sync)>,
) -> Result<Value, String> {
    let (url, email, token) = credential(broker, &ctx.channel, &ctx.node_id)?;
    let (email, token) = (&email, &token);
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
                .map(|a| a.iter().map(search_row).collect())
                .unwrap_or_default();
            Ok(json!({ "issues": issues }))
        }
        ISSUE => {
            let key = issue_key(args.get("key"), "key")?;
            let res = http
                .get(format!("{url}/rest/api/2/issue/{key}"))
                .query(&[("fields", ISSUE_FIELDS)])
                .basic_auth(email, Some(token))
                .send()
                .await
                .map_err(|e| format!("jira: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(refusal("jira answered", status, &v));
            }
            Ok(issue_view(&v))
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
        ISSUE_LINK => {
            let (from, to, asked) = link_args(args)?;
            let types = link_types(http, &ctx.channel, &url, email, token).await?;
            let kind = link_type(&types, asked)?;
            // Jira's create names the sides the opposite way to how an issue
            // reads its own links: `from` shows the link as an `outwardIssue`
            // `to` ("from blocks to"), which makes `to` the link's outward
            // side. Not verified against a live Jira; the read-back below is
            // what holds the call to the wording it was asked for.
            let res = http
                .post(format!("{url}/rest/api/2/issueLink"))
                .basic_auth(email, Some(token))
                .json(&json!({
                    "type": { "name": kind.name },
                    "inwardIssue": { "key": from },
                    "outwardIssue": { "key": to },
                }))
                .send()
                .await
                .map_err(|e| format!("jira: {e}"))?;
            let status = res.status();
            if !status.is_success() {
                let v: Value = res.json().await.unwrap_or(Value::Null);
                return Err(refusal("jira refused the link", status, &v));
            }
            // A link is created with 201 and no body, so whether it reads
            // the way it was asked is read from `from` itself.
            let res = http
                .get(format!("{url}/rest/api/2/issue/{from}"))
                .query(&[("fields", "issuelinks")])
                .basic_auth(email, Some(token))
                .send()
                .await
                .map_err(|e| {
                    format!("jira linked {from} and {to} but did not answer the read-back: {e}")
                })?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(refusal(
                    &format!("jira linked {from} and {to} but refused the read-back"),
                    status,
                    &v,
                ));
            }
            linked(&links(&v["fields"]), from, to, &kind)
        }
        ISSUE_TRANSITIONS => {
            let key = issue_key(args.get("key"), "key")?;
            let res = http
                .get(format!("{url}/rest/api/2/issue/{key}/transitions"))
                .basic_auth(email, Some(token))
                .send()
                .await
                .map_err(|e| format!("jira: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(refusal("jira answered", status, &v));
            }
            let transitions: Vec<Value> = v["transitions"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|t| {
                            json!({
                                "id": t["id"],
                                "name": t["name"],
                                "to": t["to"]["name"],
                                "category": t["to"]["statusCategory"]["name"],
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({ "key": key, "transitions": transitions }))
        }
        ISSUE_TRANSITION => {
            let key = issue_key(args.get("key"), "key")?;
            let transition = args
                .get("transition_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| {
                    !v.is_empty()
                        && v.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                })
                .ok_or("transition_id is required")?;
            if let Some(recheck) = before_mutation {
                recheck()?;
            }
            let res = http
                .post(format!("{url}/rest/api/2/issue/{key}/transitions"))
                .basic_auth(email, Some(token))
                .json(&json!({ "transition": { "id": transition } }))
                .send()
                .await
                .map_err(|e| format!("mutation-outcome-unknown: jira: {e}"))?;
            let status = res.status();
            if !status.is_success() {
                let v: Value = res.json().await.unwrap_or(Value::Null);
                let error = refusal("jira refused the transition", status, &v);
                return Err(if status.is_server_error() {
                    format!("mutation-outcome-unknown: {error}")
                } else {
                    error
                });
            }
            Ok(json!({ "key": key, "transition_id": transition }))
        }
        other => Err(format!("no jira tool named {other}")),
    }
}

/// The status a held `issue_transition` would move its issue to, read from
/// the transitions the issue offers now: a read, so a person deciding the
/// call sees "In Review" rather than "31". `None` when Jira cannot say.
pub async fn transition_target(
    broker: &SharedBroker,
    http: &reqwest::Client,
    ctx: &CallContext,
    args: &Value,
) -> Option<String> {
    let id = args.get("transition_id")?.as_str()?.trim();
    let listed = call(broker, http, ctx, ISSUE_TRANSITIONS, args, None)
        .await
        .ok()?;
    target_of(&listed, id)
}

/// Where the transition `id` in an `issue_transitions` listing leads: the
/// status, or the transition's own name when the status is not given.
fn target_of(listed: &Value, id: &str) -> Option<String> {
    let found = listed["transitions"]
        .as_array()?
        .iter()
        .find(|t| t["id"].as_str() == Some(id))?;
    [&found["to"], &found["name"]]
        .into_iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|name| !name.is_empty())
        .map(str::to_string)
}

/// One `issue_search` row.
fn search_row(i: &Value) -> Value {
    let f = &i["fields"];
    json!({
        "key": i["key"],
        "type": f["issuetype"]["name"],
        "summary": f["summary"],
        "status": f["status"]["name"],
        "priority": f["priority"]["name"],
        "assignee": f["assignee"]["displayName"],
        "parent": f["parent"]["key"],
        "labels": labels(f),
    })
}

/// What `issue` answers, from Jira's issue as it came back.
fn issue_view(v: &Value) -> Value {
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
    let subtasks: Vec<Value> = f["subtasks"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| {
                    json!({
                        "key": t["key"],
                        "summary": t["fields"]["summary"],
                        "status": t["fields"]["status"]["name"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let p = &f["parent"];
    json!({
        "key": v["key"],
        "type": f["issuetype"]["name"],
        "summary": f["summary"],
        "status": f["status"]["name"],
        "priority": f["priority"]["name"],
        "assignee": f["assignee"]["displayName"],
        "labels": labels(f),
        "parent": if p.is_object() {
            json!({ "key": p["key"], "summary": p["fields"]["summary"] })
        } else {
            Value::Null
        },
        "links": links(f),
        "subtasks": subtasks,
        "created": f["created"],
        "updated": f["updated"],
        "description": f["description"],
        "comments": comments,
    })
}

/// An issue's links to other issues, each read from this issue's side.
///
/// Jira lists a link on each of its two issues, naming the other one as the
/// side it is on: `outwardIssue` X on this issue reads "this blocks X",
/// `inwardIssue` X reads "this is blocked by X".
fn links(fields: &Value) -> Vec<Value> {
    fields["issuelinks"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| {
                    let (direction, other) = if l["outwardIssue"].is_object() {
                        ("outward", &l["outwardIssue"])
                    } else if l["inwardIssue"].is_object() {
                        ("inward", &l["inwardIssue"])
                    } else {
                        return None;
                    };
                    Some(json!({
                        "type": l["type"]["name"],
                        "direction": direction,
                        "label": l["type"][direction],
                        "key": other["key"],
                        "summary": other["fields"]["summary"],
                        "status": other["fields"]["status"]["name"],
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One of the instance's issue link types: its name, and how each side
/// reads it ("blocks", "is blocked by").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkType {
    pub name: String,
    pub outward: String,
    pub inward: String,
}

/// `from`, `to` and `type` of an `issue_link`, checked before anything is
/// asked or sent.
fn link_args(args: &Value) -> Result<(&str, &str, &str), String> {
    let from = issue_key(args.get("from"), "from")?;
    let to = issue_key(args.get("to"), "to")?;
    if from.eq_ignore_ascii_case(to) {
        return Err(format!("{from} cannot be linked to itself"));
    }
    let kind = args
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("type is required (a link type name, e.g. Blocks)")?;
    Ok((from, to, kind))
}

/// The link type named `asked`, ignoring case, or a refusal listing the
/// names this Jira has, so the agent can choose without another tool.
fn link_type(types: &[LinkType], asked: &str) -> Result<LinkType, String> {
    types
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(asked))
        .cloned()
        .ok_or_else(|| {
            let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
            if names.is_empty() {
                return format!("this Jira has no link type {asked:?}; it lists none");
            }
            format!(
                "this Jira has no link type {asked:?}; it has {}",
                names.join(", ")
            )
        })
}

/// The instance's link types, read once per channel and Jira for the
/// node's lifetime: they change about as often as the workflow does.
async fn link_types(
    http: &reqwest::Client,
    channel: &str,
    url: &str,
    email: &str,
    token: &str,
) -> Result<Vec<LinkType>, String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    /// Keyed by channel and Jira URL.
    type Read = HashMap<(String, String), Vec<LinkType>>;
    static READ: OnceLock<Mutex<Read>> = OnceLock::new();
    let cache = READ.get_or_init(Default::default);
    let at = (channel.to_string(), url.to_string());
    if let Some(types) = cache.lock().unwrap().get(&at) {
        return Ok(types.clone());
    }
    let res = http
        .get(format!("{url}/rest/api/2/issueLinkType"))
        .basic_auth(email, Some(token))
        .send()
        .await
        .map_err(|e| format!("jira: {e}"))?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(refusal("jira did not list its link types", status, &v));
    }
    let types: Vec<LinkType> = v["issueLinkTypes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some(LinkType {
                name: t["name"].as_str()?.to_string(),
                outward: t["outward"].as_str().unwrap_or_default().to_string(),
                inward: t["inward"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect();
    if !types.is_empty() {
        cache.lock().unwrap().insert(at, types.clone());
    }
    Ok(types)
}

/// The link type a held `issue_link` names, read from Jira at ask time:
/// `Err` with the refusal when Jira has no such type or the keys are not
/// keys, `Ok(None)` when Jira cannot say, which leaves the call to be asked
/// and refused when it runs.
pub async fn link_target(
    broker: &SharedBroker,
    http: &reqwest::Client,
    ctx: &CallContext,
    args: &Value,
) -> Result<Option<LinkType>, String> {
    let (_, _, asked) = link_args(args)?;
    let Ok((url, email, token)) = credential(broker, &ctx.channel, &ctx.node_id) else {
        return Ok(None);
    };
    match link_types(http, &ctx.channel, &url, &email, &token).await {
        Ok(types) if !types.is_empty() => link_type(&types, asked).map(Some),
        _ => Ok(None),
    }
}

/// What `issue_link` answers, from `from`'s links as Jira reads them after
/// the create: the link toward `to` in the type's outward wording, or an
/// error when it reads the other way. A reversed link is left for the
/// operator to remove; deleting it here would be a second write nobody asked
/// for.
fn linked(links: &[Value], from: &str, to: &str, kind: &LinkType) -> Result<Value, String> {
    let toward: Vec<&Value> = links
        .iter()
        .filter(|l| {
            l["type"].as_str() == Some(kind.name.as_str())
                && l["key"]
                    .as_str()
                    .is_some_and(|k| k.eq_ignore_ascii_case(to))
        })
        .collect();
    // A type that reads the same both ways ("relates to") has no wrong way.
    let symmetric = kind.inward == kind.outward;
    if let Some(l) = toward
        .iter()
        .find(|l| symmetric || l["direction"] == "outward")
    {
        return Ok(json!({ "from": from, "to": to, "type": kind.name, "label": l["label"] }));
    }
    if !toward.is_empty() {
        return Err(format!(
            "jira created the {} link reversed: {from} {} {to}, not {from} {} {to}. Nothing \
             was removed; the operator can delete it from either issue.",
            kind.name, kind.inward, kind.outward
        ));
    }
    Err(format!(
        "jira accepted the {} link but {from} does not show it toward {to}",
        kind.name
    ))
}

/// An issue's labels, or none when Jira leaves the field out.
fn labels(fields: &Value) -> Value {
    json!(fields["labels"].as_array().cloned().unwrap_or_default())
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
                let s = v.as_str().ok_or_else(|| format!("{k} expects a string"))?;
                if k == "summary" && s.trim().is_empty() {
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
                        let l = l.as_str().ok_or("labels expects a list of strings")?;
                        if l.trim().is_empty() {
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

/// JIRA_URL without a trailing slash, JIRA_EMAIL, JIRA_TOKEN.
fn credential(
    broker: &SharedBroker,
    channel: &str,
    node_id: &str,
) -> Result<(String, String, String), String> {
    let env = broker
        .read()
        .unwrap()
        .env_for(CREDENTIAL, channel, node_id)
        .map_err(|e| e.to_string())?;
    let get = |key: &str| {
        env.get(key)
            .cloned()
            .ok_or_else(|| format!("credential jira has no {key}"))
    };
    let url = get("JIRA_URL")?.trim_end_matches('/').to_string();
    Ok((url, get("JIRA_EMAIL")?, get("JIRA_TOKEN")?))
}

/// Why a page of search results could not be had.
#[derive(Debug)]
pub enum SearchError {
    /// The channel has no usable credential, or Jira rejected the query.
    Request(String),
    /// Jira could not be reached, or refused for any other reason.
    Upstream(String),
}

/// One page of a JQL search for a local client of the operator API: every
/// matching issue's board fields, 100 at a time, behind an opaque cursor.
pub async fn search_page(
    broker: &SharedBroker,
    http: &reqwest::Client,
    channel: &str,
    node_id: &str,
    jql: &str,
    cursor: Option<&str>,
) -> Result<Value, SearchError> {
    const FIELDS: &str = "summary,status,priority,issuetype,labels,parent";
    let (url, email, token) = credential(broker, channel, node_id).map_err(SearchError::Request)?;
    let site = reqwest::Url::parse(&url)
        .map(|u| u.origin().ascii_serialization())
        .map_err(|e| SearchError::Request(format!("credential jira has a bad JIRA_URL: {e}")))?;
    let transport = |e: reqwest::Error| SearchError::Upstream(format!("jira: {e}"));

    let mut v3 = vec![("jql", jql), ("fields", FIELDS), ("maxResults", "100")];
    if let Some(c) = cursor {
        v3.push(("nextPageToken", c));
    }
    let mut res = http
        .get(format!("{url}/rest/api/3/search/jql"))
        .query(&v3)
        .basic_auth(&email, Some(&token))
        .send()
        .await
        .map_err(transport)?;
    // Data Center has only the older endpoint, which pages by offset.
    let mut start_at = None;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        let start: u64 = match cursor {
            Some(c) => c
                .parse()
                .map_err(|_| SearchError::Request(format!("bad cursor: {c}")))?,
            None => 0,
        };
        let start_str = start.to_string();
        res = http
            .get(format!("{url}/rest/api/2/search"))
            .query(&[
                ("jql", jql),
                ("fields", FIELDS),
                ("maxResults", "100"),
                ("startAt", start_str.as_str()),
            ])
            .basic_auth(&email, Some(&token))
            .send()
            .await
            .map_err(transport)?;
        start_at = Some(start);
    }
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let message = refusal("jira refused the search", status, &v);
        return Err(if status == reqwest::StatusCode::BAD_REQUEST {
            SearchError::Request(message)
        } else {
            SearchError::Upstream(message)
        });
    }
    let rows = v["issues"].as_array().cloned().unwrap_or_default();
    let next_cursor = match start_at {
        None if v["isLast"].as_bool() == Some(true) => None,
        None => v["nextPageToken"].as_str().map(str::to_string),
        Some(start) => {
            let next = start + rows.len() as u64;
            (!rows.is_empty() && v["total"].as_u64().is_some_and(|t| next < t))
                .then(|| next.to_string())
        }
    };
    let issues: Vec<Value> = rows
        .iter()
        .map(|i| {
            let f = &i["fields"];
            let p = &f["parent"];
            json!({
                "id": i["id"],
                "key": i["key"],
                "summary": f["summary"],
                "status": f["status"]["name"],
                "status_category": f["status"]["statusCategory"]["key"],
                "priority": f["priority"]["name"],
                "issue_type": f["issuetype"]["name"],
                "labels": f["labels"].as_array().cloned().unwrap_or_default(),
                "parent": if p.is_object() {
                    json!({
                        "key": p["key"],
                        "summary": p["fields"]["summary"],
                        "issue_type": p["fields"]["issuetype"]["name"],
                    })
                } else {
                    Value::Null
                },
            })
        })
        .collect();
    Ok(json!({
        "site": site,
        "account_email": email,
        "issues": issues,
        "next_cursor": next_cursor,
    }))
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
    fn a_transition_is_named_by_the_status_it_leads_to() {
        let listed = json!({ "key": "WRK-1", "transitions": [
            { "id": "21", "name": "Start review", "to": "In Review" },
            { "id": "31", "name": "Close", "to": null },
            { "id": "41", "name": " ", "to": "" },
        ] });
        assert_eq!(target_of(&listed, "21").as_deref(), Some("In Review"));
        assert_eq!(target_of(&listed, "31").as_deref(), Some("Close"));
        assert_eq!(target_of(&listed, "41"), None);
        assert_eq!(target_of(&listed, "99"), None);
        assert_eq!(target_of(&json!({}), "21"), None);
    }

    /// Trimmed from a real v2 issue: one link each way, a parent, labels and
    /// a subtask, with the fields Jira sends that `issue` does not keep.
    fn linked_issue() -> Value {
        let status = |name: &str| json!({ "name": name, "statusCategory": { "key": "new" } });
        json!({
            "key": "WRK-16",
            "fields": {
                "summary": "Replace the scripts",
                "status": status("In Progress"),
                "issuetype": { "name": "Task" },
                "priority": { "name": "Minor" },
                "assignee": null,
                "description": "h2. Goal",
                "labels": ["etl", "fusion"],
                "parent": { "id": "1", "key": "WRK-10", "fields": {
                    "summary": "The epic", "status": status("Open"),
                    "issuetype": { "name": "Epic" } } },
                "issuelinks": [
                    { "id": "1", "type": { "id": "10000", "name": "Blocks",
                        "inward": "is blocked by", "outward": "blocks" },
                      "outwardIssue": { "key": "WRK-20", "fields": {
                        "summary": "Clean up", "status": status("Open") } } },
                    { "id": "2", "type": { "id": "10000", "name": "Blocks",
                        "inward": "is blocked by", "outward": "blocks" },
                      "inwardIssue": { "key": "WRK-15", "fields": {
                        "summary": "Add a flavor", "status": status("To Do") } } },
                ],
                "subtasks": [ { "id": "3", "key": "WRK-17", "fields": {
                    "summary": "Port one", "status": status("Done"),
                    "issuetype": { "name": "Subtask" } } } ],
                "created": "2025-10-21T13:13:10.850-0500",
                "updated": "2026-08-13T15:51:11.203-0500",
                "comment": { "comments": [] },
            },
        })
    }

    #[test]
    fn an_issue_carries_what_a_write_to_it_needs() {
        let view = issue_view(&linked_issue());
        assert_eq!(view["labels"], json!(["etl", "fusion"]));
        assert_eq!(
            view["parent"],
            json!({ "key": "WRK-10", "summary": "The epic" })
        );
        assert_eq!(
            view["links"],
            json!([
                { "type": "Blocks", "direction": "outward", "label": "blocks",
                  "key": "WRK-20", "summary": "Clean up", "status": "Open" },
                { "type": "Blocks", "direction": "inward", "label": "is blocked by",
                  "key": "WRK-15", "summary": "Add a flavor", "status": "To Do" },
            ])
        );
        assert_eq!(
            view["subtasks"],
            json!([{ "key": "WRK-17", "summary": "Port one", "status": "Done" }])
        );
        assert_eq!(view["created"], json!("2025-10-21T13:13:10.850-0500"));
        assert_eq!(view["updated"], json!("2026-08-13T15:51:11.203-0500"));
        assert_eq!(view["status"], json!("In Progress"));
        assert_eq!(view["comments"], json!([]));
    }

    #[test]
    fn an_issue_with_none_of_them_says_so_rather_than_leaving_them_out() {
        let view = issue_view(&json!({ "key": "WRK-1", "fields": { "summary": "Bare" } }));
        assert_eq!(view["labels"], json!([]));
        assert_eq!(view["parent"], Value::Null);
        assert_eq!(view["links"], json!([]));
        assert_eq!(view["subtasks"], json!([]));
    }

    #[test]
    fn a_search_row_carries_labels_and_the_parent_key() {
        let row = search_row(&linked_issue());
        assert_eq!(row["labels"], json!(["etl", "fusion"]));
        assert_eq!(row["parent"], json!("WRK-10"));
        assert_eq!(search_row(&json!({ "fields": {} }))["labels"], json!([]));
    }

    fn blocks() -> LinkType {
        LinkType {
            name: "Blocks".into(),
            outward: "blocks".into(),
            inward: "is blocked by".into(),
        }
    }

    #[test]
    fn a_link_type_is_found_by_name_or_refused_with_the_names_there_are() {
        let relates = LinkType {
            name: "Relates".into(),
            outward: "relates to".into(),
            inward: "relates to".into(),
        };
        let types = [blocks(), relates];
        assert_eq!(link_type(&types, "blocks").unwrap(), blocks());
        let e = link_type(&types, "Precedes").unwrap_err();
        assert!(e.contains("Blocks, Relates"), "{e}");
    }

    #[test]
    fn a_link_names_two_different_issues_and_a_type() {
        let e =
            link_args(&json!({ "from": "WRK-1", "to": "wrk-1", "type": "Blocks" })).unwrap_err();
        assert!(e.contains("itself"), "{e}");
        let e = link_args(&json!({ "from": "WRK-1", "to": "not a key", "type": "Blocks" }))
            .unwrap_err();
        assert!(e.starts_with("to"), "{e}");
        let e = link_args(&json!({ "from": "WRK-1", "to": "WRK-2" })).unwrap_err();
        assert!(e.contains("type"), "{e}");
    }

    #[test]
    fn a_link_read_back_the_way_it_was_asked_is_returned_in_its_wording() {
        let mut issue = linked_issue();
        let read = links(&issue["fields"]);
        assert_eq!(
            linked(&read, "WRK-16", "WRK-20", &blocks()).unwrap(),
            json!({ "from": "WRK-16", "to": "WRK-20", "type": "Blocks", "label": "blocks" })
        );
        // WRK-15 shows on WRK-16 as blocking it: asked the other way round.
        let e = linked(&read, "WRK-16", "WRK-15", &blocks()).unwrap_err();
        assert!(e.contains("reversed"), "{e}");
        assert!(e.contains("WRK-16 is blocked by WRK-15"), "{e}");
        issue["fields"]["issuelinks"] = json!([]);
        let e = linked(&links(&issue["fields"]), "WRK-16", "WRK-20", &blocks()).unwrap_err();
        assert!(e.contains("does not show it"), "{e}");
    }

    #[test]
    fn a_link_type_that_reads_the_same_both_ways_cannot_be_reversed() {
        let relates = LinkType {
            name: "Relates".into(),
            outward: "relates to".into(),
            inward: "relates to".into(),
        };
        let read = vec![json!({ "type": "Relates", "direction": "inward",
                                "label": "relates to", "key": "WRK-2" })];
        assert_eq!(
            linked(&read, "WRK-1", "WRK-2", &relates).unwrap()["label"],
            "relates to"
        );
    }

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

    #[test]
    fn summary_description_and_labels_are_sent_as_given() {
        let f = fields_from(
            &json!({ "summary": "  Fix it ", "description": "\n body \n", "labels": ["a"] }),
            ISSUE_UPDATE,
            &[],
        )
        .unwrap();
        assert_eq!(f["summary"], json!("  Fix it "));
        assert_eq!(f["description"], json!("\n body \n"));
        let e = fields_from(&json!({ "summary": "  " }), ISSUE_UPDATE, &[]).unwrap_err();
        assert!(e.contains("summary cannot be empty"), "{e}");
        let e = fields_from(&json!({ "labels": [" "] }), ISSUE_UPDATE, &[]).unwrap_err();
        assert!(e.contains("cannot be empty"), "{e}");
        // A padded label is refused, not trimmed into a different one.
        let e = fields_from(&json!({ "labels": [" a"] }), ISSUE_UPDATE, &[]).unwrap_err();
        assert!(e.contains("whitespace"), "{e}");
    }
}
