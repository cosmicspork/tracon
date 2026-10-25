//! The ledger as the agent sees it: `work_ready` lists what is unblocked
//! (the tool sorts, the model picks), `work_discover` records work found
//! mid-session linked to its origin instead of evaporating with the
//! session, and `work_close` closes the item this session holds — or, for a
//! harness the operator runs, which holds none, the item it names.
//!
//! `brief_read` and `brief_note` are the item's product brief, when it has
//! one: who the work is for and what would make it good, as the operator
//! recorded it. Reading is free; writing a line is asked, and the node holds
//! a session to what a session can honestly claim — never a decision, and
//! never an observation with nothing to point at.
//!
//! `criteria_read` and `criteria_link` are the same brief read as acceptance
//! criteria: per criterion, what points at it and how far the evidence got.
//! A session may say what it thinks would settle a criterion, and that is all
//! it may say — there is deliberately no tool here for recording that a
//! criterion was met. Passing one's own checks is not the customer agreeing
//! with the standard, so the verdict is the operator's, over the interface.

use serde_json::{json, Value};

use crate::{
    config::Config,
    corpus,
    mcp::{CallContext, SessionAccess},
};

pub const WORK_READY: &str = "work_ready";
pub const WORK_DISCOVER: &str = "work_discover";
pub const WORK_CLOSE: &str = "work_close";
pub const WORK_LIST: &str = "work_list";
pub const WORK_SHOW: &str = "work_show";
pub const WORK_UPDATE: &str = "work_update";
pub const WORK_DELETE: &str = "work_delete";
pub const BRIEF_READ: &str = "brief_read";
pub const BRIEF_NOTE: &str = "brief_note";
pub const CRITERIA_READ: &str = "criteria_read";
pub const CRITERIA_LINK: &str = "criteria_link";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": WORK_READY,
            "description": "Ready work on this channel: open items whose dependencies are all closed, \
                            in the order the node computes (priority, then age). Items another session \
                            holds are omitted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "default": 10 },
                },
            },
        }),
        json!({
            "name": WORK_DISCOVER,
            "description": "Record work you found but are not doing now. In a session the node started \
                            it is linked to this session's item as discovered-from and bound to this \
                            session's project, so it survives the session. A harness you run yourself \
                            holds no item, so its items are linked to none and are marked with your \
                            lane; pass `worktree` to bind the item to that repository's project, or \
                            leave it out and the item belongs to the whole channel, with no project. \
                            Say what and why; add deps if it must wait on other items.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "deps": { "type": "array", "items": { "type": "string" } },
                    "priority": {
                        "type": "integer",
                        "description": "Higher runs first. Leave it out for the channel's default; \
                                        work_list names the levels.",
                    },
                    "worktree": {
                        "type": "string",
                        "description": "Only for a harness you run yourself: the absolute path of a \
                                        worktree of the repository the item is for. Its repository \
                                        must be under [external] repo_roots.",
                    },
                },
                "required": ["title"],
            },
        }),
        json!({
            "name": BRIEF_READ,
            "description": "The product brief of the item this session holds, when it has one: intended \
                            user, problem, source references, constraints, success criteria and open \
                            questions. Every line says whether the customer was observed saying it, \
                            someone inferred it, or the operator decided it, and what it points at. \
                            An item without a brief is normal; the tool says so.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": BRIEF_NOTE,
            "description": "Add a line to the brief of the item this session holds. Say which section, \
                            and point at what the line rests on. You may record `inferred` (you reasoned \
                            to it) or `observed` (the customer said or did it) — an observation must cite \
                            a doc, url, evidence id or session. You may not record a decision: that is \
                            the operator's. The line is written with a reference to this session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "field": {
                        "type": "string",
                        "enum": ["intended_user", "problem", "source_references", "constraints", "success_criteria", "unresolved_questions"],
                    },
                    "provenance": { "type": "string", "enum": ["observed", "inferred"], "default": "inferred" },
                    "text": { "type": "string" },
                    "refs": {
                        "type": "array",
                        "description": "What the line rests on, as {kind, value} with kind one of doc, session, evidence, work, url, file.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": crate::corpus::brief::REF_KINDS },
                                "value": { "type": "string" },
                            },
                            "required": ["kind", "value"],
                        },
                    },
                },
                "required": ["field", "text"],
            },
        }),
        json!({
            "name": CRITERIA_READ,
            "description": "The acceptance criteria of the item this session holds, read as coverage: \
                            per criterion, whether the standard is agreed or is still somebody's \
                            proposal, what points at it (checks), what the \
                            checks say about one candidate, and whether a person has judged it. It also \
                            says what is uncovered, what nobody has judged, which lines are assumptions, \
                            and which questions are still open. Checks passing is not the same as a \
                            criterion being met: only the operator can say that.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "candidate": {
                        "type": "string",
                        "description": "The candidate to read the check results against. Without it the \
                                        criteria and their links are still returned, with no results.",
                    },
                },
            },
        }),
        json!({
            "name": CRITERIA_LINK,
            "description": "Say which check you think would settle one acceptance criterion: one of \
                            the commands the operator configured (any other is recorded and reported as \
                            never running). Anything a check cannot settle is the operator's judgement, \
                            not a link. It is written under the criterion in the brief \
                            as your proposal, which is all it can be: only the operator decides what good \
                            means, and your proposal passing does not establish that the customer agreed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "criterion": {
                        "type": "string",
                        "description": "The criterion's key from criteria_read, or its text verbatim.",
                    },
                    "kind": { "type": "string", "enum": crate::corpus::brief::LINK_KINDS },
                    "value": {
                        "type": "string",
                        "description": "The command.",
                    },
                    "provenance": {
                        "type": "string",
                        "enum": ["observed", "inferred"],
                        "default": "inferred",
                        "description": "`observed` only for something someone was seen to need, and it \
                                        must cite where that was seen.",
                    },
                    "refs": {
                        "type": "array",
                        "description": "What this rests on, as {kind, value} with kind one of doc, session, evidence, work, url, file.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": crate::corpus::brief::REF_KINDS },
                                "value": { "type": "string" },
                            },
                            "required": ["kind", "value"],
                        },
                    },
                },
                "required": ["criterion", "kind", "value"],
            },
        }),
        json!({
            "name": WORK_CLOSE,
            "description": "Close a work item because it is done. In a session the node started \
                            that is the session's own item, and the session ends with it. A \
                            harness you run yourself passes the item's `id`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The item to close. Only for a harness you run yourself." },
                    "summary": { "type": "string" },
                },
            },
        }),
        json!({
            "name": WORK_LIST,
            "description": "The work ledger on this channel in the order sessions take it: each \
                            item's id, title, state, what blocks it, priority (with its level's \
                            name), project and the session holding it. No bodies; work_show reads \
                            one. A session sees its own project unless it names another; a harness \
                            you run yourself sees the whole channel, or one project by `project` or \
                            by a `worktree` of its repository.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "A project's name or id, or `none` for items with no project." },
                    "worktree": { "type": "string", "description": "Only for a harness you run yourself: a worktree of the project's repository." },
                    "state": {
                        "type": "string",
                        "enum": ["ready", "blocked", "in_session", "open", "closed", "all"],
                        "default": "open",
                    },
                    "query": { "type": "string", "description": "Words every item must contain in its title or body." },
                    "limit": { "type": "integer", "default": 25 },
                },
            },
        }),
        json!({
            "name": WORK_SHOW,
            "description": "One work item in full: its body, priority, project, readiness, the \
                            items it waits on, what was discovered from it, and its sessions. \
                            Without an `id`, the item this session holds. An id may be \
                            shortened to its first 6 or more characters.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                },
            },
        }),
        json!({
            "name": WORK_UPDATE,
            "description": "Change a work item on this channel: its title, body, priority, the \
                            items it waits on, its project, or reopen or close it. Only the fields \
                            given change. The operator is asked first. Closing an item a session \
                            is working on is refused unless it is this session's own.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "priority": { "type": "string", "description": "A level's name (work_list shows them) or a number; higher runs first." },
                    "deps": { "type": "array", "items": { "type": "string" }, "description": "The whole list of items this one waits on." },
                    "project": { "type": "string", "description": "A project's name or id, or `none`." },
                    "worktree": { "type": "string", "description": "Only for a harness you run yourself: set the project to this worktree's repository." },
                    "state": { "type": "string", "enum": ["open", "closed"] },
                    "summary": { "type": "string", "description": "Why, when closing." },
                },
                "required": ["id"],
            },
        }),
        json!({
            "name": WORK_DELETE,
            "description": "Delete a work item on this channel, for one recorded by mistake or \
                            twice. Close finished work instead. The operator is asked first, and \
                            an item a session is working on cannot be deleted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                },
                "required": ["id"],
            },
        }),
    ]
}

/// A priority as an agent sent it: a number, or a level's name or a number
/// as text. Absent is `None`.
fn priority_arg(cfg: &Config, v: &Value) -> Result<Option<i64>, String> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("priority {n} is not a whole number")),
        Value::String(t) => cfg.live().work.parse_priority(t).map(Some),
        other => Err(format!("priority {other} is not a level or a number")),
    }
}

/// The project names this node knows on a channel, by id.
fn project_names(
    store: &crate::store::Store,
    channel: &str,
) -> std::collections::HashMap<String, String> {
    store
        .project_list(channel)
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect()
}

/// An item as the ledger tools show it.
fn item_json(
    cfg: &Config,
    view: &crate::store::WorkView,
    names: &std::collections::HashMap<String, String>,
    with_body: bool,
) -> Value {
    use tracon_sync::work::{Blocker, Readiness};
    let item = &view.item;
    let state = match &view.readiness {
        Readiness::Closed => "closed",
        _ if view.session_id.is_some() => "in_session",
        Readiness::Ready => "ready",
        Readiness::Blocked { .. } => "blocked",
    };
    let blocked_by: Vec<Value> = match &view.readiness {
        Readiness::Blocked { by } => by
            .iter()
            .map(|b| match b {
                Blocker::Open { id } => json!({ "open": id }),
                Blocker::Unknown { id } => json!({ "unknown": id }),
                Blocker::Cycle => json!("cycle"),
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut v = json!({
        "id": item.id,
        "title": item.title,
        "state": state,
        "priority": item.priority,
        "priority_name": cfg.live().work.priority_name(item.priority),
        "project": item.project_id.as_ref().map(|id| json!({
            "id": id,
            "name": names.get(id),
        })),
        "deps": item.deps,
        "holder": view.session_id,
    });
    if !blocked_by.is_empty() {
        v["blocked_by"] = json!(blocked_by);
    }
    if with_body {
        v["body"] = json!(item.body);
        v["discovered_from"] = json!(item.discovered_from);
        v["plan"] = json!(item.phase_plan_slug);
        v["brief"] = json!(item.brief_slug);
        v["created_ms"] = json!(item.created_ms);
        v["updated_ms"] = json!(item.updated_ms);
    }
    v
}

/// The project an argument names: `none` clears it, anything else is a
/// project's name or id on the channel.
fn project_arg(
    store: &crate::store::Store,
    channel: &str,
    text: &str,
) -> Result<Option<String>, String> {
    if text.trim().eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    corpus::work::find_project(store, channel, text).map(Some)
}

/// The project a harness outside the boundary means by `path`: the
/// repository of that worktree, found as `submit_review` finds it and named as
/// a session launched on it would name it, so its items sit with theirs.
async fn external_project(
    cfg: &Config,
    store: &crate::store::Store,
    channel: &str,
    path: &str,
) -> Result<String, String> {
    let roots: Vec<_> = cfg
        .external
        .repo_roots
        .iter()
        .map(|r| crate::config::expand_home(r))
        .collect();
    let (_, repo) = crate::review::locate_repo(path, &roots)
        .await
        .map_err(|e| e.to_string())?;
    let (id, name, remote) = corpus::project::identify(
        channel,
        std::path::Path::new(&repo),
        &cfg.live().publish.git,
    )
    .await;
    store
        .project_put(&crate::store::ProjectRow {
            id: id.clone(),
            channel: channel.to_string(),
            name,
            remote_url: remote,
            created_ms: crate::store::now_ms(),
        })
        .map_err(|e| e.to_string())?;
    Ok(id)
}

pub async fn call(
    cfg: &Config,
    access: &SessionAccess,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let session = match ctx.session_id() {
        Some(id) => access.store.get_session(id).map_err(|e| e.to_string())?,
        None => None,
    };
    let (project_id, item_id) = session
        .as_ref()
        .map(|s| (s.project_id.clone(), s.work_item_id.clone()))
        .unwrap_or((None, None));
    let external = ctx.is_external();
    match name {
        WORK_READY => {
            let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
            let items = access
                .store
                .work_ready(&ctx.channel, project_id.as_deref())
                .map_err(|e| e.to_string())?;
            let out: Vec<Value> = items
                .iter()
                .take(limit)
                .map(|v| {
                    json!({
                        "id": v.item.id, "title": v.item.title, "priority": v.item.priority,
                        "body": v.item.body, "deps": v.item.deps,
                    })
                })
                .collect();
            Ok(json!({ "items": out }))
        }
        WORK_DISCOVER => {
            let title = args["title"].as_str().unwrap_or("").trim();
            if title.is_empty() {
                return Err("work_discover needs a title".into());
            }
            let deps: Vec<String> = args["deps"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let worktree = args["worktree"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            // A session's project is the one it was launched on; only a
            // harness outside the boundary names one, by a worktree of it.
            let project_id = match (external, worktree) {
                (false, Some(_)) => {
                    return Err("worktree is for a harness you run yourself; \
                                this session's items go to its own project"
                        .into())
                }
                (true, Some(path)) => {
                    Some(external_project(cfg, &access.store, &ctx.channel, path).await?)
                }
                (true, None) => None,
                (false, None) => project_id,
            };
            let item = corpus::work::create(
                &access.store,
                access.manager.bus(),
                &ctx.node_id,
                corpus::work::NewWork {
                    channel: ctx.channel.clone(),
                    project_id,
                    title: title.to_string(),
                    body: args["body"].as_str().unwrap_or("").to_string(),
                    deps,
                    priority: priority_arg(cfg, &args["priority"])?
                        .unwrap_or(cfg.live().work.default_priority),
                    discovered_from: item_id,
                    discovered_by_session: ctx.session_id().map(str::to_string),
                    // An unlabelled harness is still not the operator.
                    discovered_by_lane: external
                        .then(|| ctx.lane().unwrap_or("external").to_string()),
                },
            )
            .map_err(|e| e.to_string())?;
            Ok(json!({
                "id": item.id, "title": item.title, "project_id": item.project_id,
                "discovered_from": item.discovered_from,
            }))
        }
        WORK_CLOSE if external => {
            let id = args["id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("work_close needs the item's id")?;
            let item = corpus::work::find(&access.store, &ctx.channel, id)?;
            // Closing an item ends the session working on it; that is not a
            // call a harness outside the boundary makes for someone else.
            if let Some(s) = access
                .store
                .session_holding(&item.id)
                .map_err(|e| e.to_string())?
            {
                return Err(format!(
                    "session {} is working on {}; let it finish, or kill it first",
                    s.id, item.id
                ));
            }
            let item = corpus::work::close(
                &access.store,
                access.manager.bus(),
                &ctx.node_id,
                &item.id,
                ctx.session_id(),
            )
            .map_err(|e| e.to_string())?;
            access.manager.record_for(
                ctx,
                crate::session::state::event_kind::WORK_CLOSED,
                None,
                json!({ "item": item.id, "summary": args["summary"].as_str().unwrap_or("") }),
            );
            Ok(json!({ "id": item.id, "state": item.state }))
        }
        WORK_CLOSE => {
            let (Some(session_id), Some(id)) = (ctx.session_id(), item_id) else {
                return Err("this session holds no work item".into());
            };
            let item = corpus::work::close(
                &access.store,
                access.manager.bus(),
                &ctx.node_id,
                &id,
                Some(session_id),
            )
            .map_err(|e| e.to_string())?;
            access
                .manager
                .item_closed(session_id, args["summary"].as_str().unwrap_or(""))
                .await;
            Ok(json!({ "id": item.id, "state": item.state }))
        }
        WORK_LIST => {
            let project = match (args["project"].as_str(), args["worktree"].as_str()) {
                (Some(p), _) if !p.trim().is_empty() => {
                    match project_arg(&access.store, &ctx.channel, p)? {
                        Some(id) => Some(id),
                        None => Some("none".to_string()),
                    }
                }
                (_, Some(path)) if !path.trim().is_empty() => {
                    if !external {
                        return Err("worktree is for a harness you run yourself; \
                                    name a project instead"
                            .into());
                    }
                    Some(external_project(cfg, &access.store, &ctx.channel, path.trim()).await?)
                }
                _ => project_id.clone(),
            };
            let state = args["state"].as_str().unwrap_or("open");
            let state = (state != "all").then_some(state);
            let limit = args["limit"].as_u64().unwrap_or(25).clamp(1, 100) as usize;
            let items = corpus::work::select(
                &access.store,
                &ctx.channel,
                project.as_deref(),
                state,
                args["query"].as_str(),
            )
            .map_err(|e| e.to_string())?;
            let names = project_names(&access.store, &ctx.channel);
            let total = items.len();
            let out: Vec<Value> = items
                .iter()
                .take(limit)
                .map(|v| item_json(cfg, v, &names, false))
                .collect();
            Ok(json!({
                "items": out,
                "total": total,
                "priority_levels": cfg.live().work.levels(),
            }))
        }
        WORK_SHOW => {
            let id = match args["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                Some(id) => corpus::work::find(&access.store, &ctx.channel, id)?.id,
                None => item_id.ok_or("name the item by its id; this session holds none")?,
            };
            let view = access
                .store
                .work_status(&ctx.channel, None)
                .map_err(|e| e.to_string())?;
            let titles: std::collections::HashMap<&str, (&str, &str)> = view
                .iter()
                .map(|v| {
                    (
                        v.item.id.as_str(),
                        (v.item.title.as_str(), v.item.state.as_str()),
                    )
                })
                .collect();
            let this = view
                .iter()
                .find(|v| v.item.id == id)
                .ok_or_else(|| format!("no work item {id} on channel {}", ctx.channel))?;
            let names = project_names(&access.store, &ctx.channel);
            let mut out = item_json(cfg, this, &names, true);
            out["deps"] = json!(this
                .item
                .deps
                .iter()
                .map(|d| match titles.get(d.as_str()) {
                    Some((title, state)) => json!({ "id": d, "title": title, "state": state }),
                    None => json!({ "id": d, "title": null, "state": "unknown" }),
                })
                .collect::<Vec<_>>());
            out["discovered"] = json!(view
                .iter()
                .filter(|v| v.item.discovered_from.as_deref() == Some(id.as_str()))
                .map(|v| json!({ "id": v.item.id, "title": v.item.title, "state": v.item.state }))
                .collect::<Vec<_>>());
            out["sessions"] = json!(access
                .store
                .sessions_of_work_item(&id)
                .map_err(|e| e.to_string())?
                .iter()
                .map(|s| json!({ "id": s.id, "phase": s.phase, "state": s.state, "branch": s.branch }))
                .collect::<Vec<_>>());
            Ok(out)
        }
        WORK_UPDATE => {
            let id = args["id"].as_str().unwrap_or("").trim();
            if id.is_empty() {
                return Err("work_update needs the item's id".into());
            }
            let item = corpus::work::find(&access.store, &ctx.channel, id)?;
            let mut patch = corpus::work::Patch {
                title: args["title"].as_str().map(str::to_string),
                body: args["body"].as_str().map(str::to_string),
                priority: priority_arg(cfg, &args["priority"])?,
                ..Default::default()
            };
            if let Some(deps) = args["deps"].as_array() {
                let mut full = Vec::new();
                for d in deps.iter().filter_map(Value::as_str) {
                    let dep = corpus::work::find(&access.store, &ctx.channel, d)
                        .map_err(|e| format!("deps: {e}"))?;
                    if dep.id == item.id {
                        return Err("deps: an item cannot wait on itself".into());
                    }
                    full.push(dep.id);
                }
                patch.deps = Some(full);
            }
            match (args["project"].as_str(), args["worktree"].as_str()) {
                (Some(p), _) if !p.trim().is_empty() => {
                    patch.project_id = Some(project_arg(&access.store, &ctx.channel, p)?);
                }
                (_, Some(path)) if !path.trim().is_empty() => {
                    if !external {
                        return Err("worktree is for a harness you run yourself; \
                                    name a project instead"
                            .into());
                    }
                    patch.project_id = Some(Some(
                        external_project(cfg, &access.store, &ctx.channel, path.trim()).await?,
                    ));
                }
                _ => {}
            }
            let closing = match args["state"].as_str() {
                Some(tracon_sync::work::CLOSED) => item.state != tracon_sync::work::CLOSED,
                Some(tracon_sync::work::OPEN) | None => false,
                Some(other) => return Err(format!("state {other:?} is not open or closed")),
            };
            patch.state = args["state"].as_str().map(str::to_string);
            // Closing ends the session on the item, which is the holder's
            // own call to make: this session may close its own item.
            let own = closing
                && match access
                    .store
                    .session_holding(&item.id)
                    .map_err(|e| e.to_string())?
                {
                    Some(h) if Some(h.id.as_str()) == ctx.session_id() => true,
                    Some(h) => {
                        return Err(format!(
                            "session {} is working on {}; it closes the item, or the operator does",
                            h.id, item.id
                        ))
                    }
                    None => false,
                };
            let updated = corpus::work::update(
                &access.store,
                access.manager.bus(),
                &ctx.node_id,
                &item.id,
                patch,
                ctx.session_id(),
            )
            .map_err(|e| e.to_string())?;
            let summary = args["summary"].as_str().unwrap_or("");
            if own {
                if let Some(session_id) = ctx.session_id() {
                    access.manager.item_closed(session_id, summary).await;
                }
            } else if closing {
                access.manager.record_for(
                    ctx,
                    crate::session::state::event_kind::WORK_CLOSED,
                    None,
                    json!({ "item": updated.id, "summary": summary }),
                );
            }
            let names = project_names(&access.store, &ctx.channel);
            let view = crate::store::WorkView {
                readiness: access
                    .store
                    .work_status(&ctx.channel, None)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|v| v.item.id == updated.id)
                    .map(|v| v.readiness)
                    .unwrap_or(tracon_sync::work::Readiness::Ready),
                session_id: None,
                item: updated,
            };
            Ok(item_json(cfg, &view, &names, false))
        }
        WORK_DELETE => {
            let id = args["id"].as_str().unwrap_or("").trim();
            if id.is_empty() {
                return Err("work_delete needs the item's id".into());
            }
            let item = corpus::work::find(&access.store, &ctx.channel, id)?;
            if let Some(h) = access
                .store
                .session_holding(&item.id)
                .map_err(|e| e.to_string())?
            {
                return Err(format!(
                    "session {} is working on {}; an item in use is not deleted",
                    h.id, item.id
                ));
            }
            corpus::work::remove(&access.store, access.manager.bus(), &ctx.node_id, &item.id)
                .map_err(|e| e.to_string())?;
            Ok(json!({ "id": item.id, "deleted": true }))
        }
        BRIEF_READ => {
            let Some(id) = item_id else {
                return Err("this session holds no work item, so it has no brief".into());
            };
            let view = corpus::brief::for_item(&access.store, &id).map_err(|e| e.to_string())?;
            match view {
                Some(view) => {
                    let summary = corpus::brief::summary(&view);
                    Ok(json!({ "brief": view, "summary": summary }))
                }
                None => Ok(json!({
                    "brief": null,
                    "summary": format!("{} has no brief; work from the item and the plan", &id[..12.min(id.len())]),
                })),
            }
        }
        BRIEF_NOTE => {
            let (Some(session_id), Some(id)) = (ctx.session_id(), item_id) else {
                return Err("this session holds no work item, so it has no brief".into());
            };
            let field = args["field"].as_str().unwrap_or("").trim().to_string();
            let text = args["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err("brief_note needs the line's text".into());
            }
            let refs: Vec<corpus::brief::RefInput> =
                serde_json::from_value(args.get("refs").cloned().unwrap_or_else(|| json!([])))
                    .map_err(|e| format!("refs must be {{kind, value}} objects: {e}"))?;
            let view = corpus::brief::append_for_item(
                &access.store,
                access.manager.bus(),
                &ctx.node_id,
                &id,
                vec![corpus::brief::SectionInput {
                    field,
                    notes: None,
                    replace: false,
                    entries: Some(vec![corpus::brief::EntryInput {
                        provenance: Some(session_provenance(args).to_string()),
                        text,
                        refs,
                        links: vec![],
                    }]),
                }],
                &corpus::brief::Author::Session(session_id.to_string()),
            )
            .map_err(|e| e.to_string())?;
            Ok(json!({ "slug": view.slug, "summary": corpus::brief::summary(&view) }))
        }
        CRITERIA_READ => {
            let Some(id) = item_id else {
                return Err("this session holds no work item, so it has no criteria".into());
            };
            let candidate = args["candidate"]
                .as_str()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            // Without one named, the newest attempt at the item: that is what
            // the session is looking at, and reading the criteria against no
            // candidate at all would report every check as having no result.
            let candidate = match candidate {
                Some(id) => Some(id),
                None => corpus::criteria::newest_candidate(&access.store, &id)
                    .map_err(|e| e.to_string())?,
            };
            let view = corpus::criteria::for_item(&access.store, cfg, &id, candidate.as_deref())
                .map_err(|e| match e {
                    corpus::criteria::CriteriaError::MissingCandidate(_) => {
                        "no candidate for this item".to_string()
                    }
                    other => other.to_string(),
                })?;
            match view {
                Some(view) => {
                    let summary = view.summary.clone();
                    Ok(json!({ "criteria": view, "summary": summary }))
                }
                None => Ok(json!({
                    "criteria": null,
                    "summary": format!(
                        "{} has no brief, so no criteria are stated; work from the item and the plan",
                        &id[..12.min(id.len())],
                    ),
                })),
            }
        }
        CRITERIA_LINK => {
            let (Some(session_id), Some(id)) = (ctx.session_id(), item_id) else {
                return Err("this session holds no work item, so it has no criteria".into());
            };
            let criterion = args["criterion"].as_str().unwrap_or("").trim();
            if criterion.is_empty() {
                return Err("criteria_link needs the criterion's key or its text".into());
            }
            let refs: Vec<corpus::brief::RefInput> =
                serde_json::from_value(args.get("refs").cloned().unwrap_or_else(|| json!([])))
                    .map_err(|e| format!("refs must be {{kind, value}} objects: {e}"))?;
            corpus::criteria::add_link(
                &access.store,
                access.manager.bus(),
                &ctx.node_id,
                &id,
                criterion,
                corpus::brief::LinkInput {
                    provenance: Some(session_provenance(args).to_string()),
                    kind: args["kind"].as_str().unwrap_or("").to_string(),
                    value: args["value"].as_str().unwrap_or("").to_string(),
                    refs,
                },
                None,
                &corpus::brief::Author::Session(session_id.to_string()),
            )
            .map_err(|e| e.to_string())?;
            let candidate = corpus::criteria::newest_candidate(&access.store, &id)
                .map_err(|e| e.to_string())?;
            let view = corpus::criteria::for_item(&access.store, cfg, &id, candidate.as_deref())
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "summary": view.as_ref().map(|v| v.summary.clone()),
                "standing": format!(
                    "your proposal, recorded as `{}`; the operator decides what good means",
                    session_provenance(args)
                ),
            }))
        }
        other => Err(format!("no work tool named {other}")),
    }
}

/// The schemas promise `inferred` when a session names no provenance; left to
/// the store, an omitted value would be recorded as `unattributed` instead.
fn session_provenance(args: &Value) -> &str {
    args["provenance"].as_str().unwrap_or("inferred")
}
