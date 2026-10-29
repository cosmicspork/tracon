//! The ledger as the agent sees it: `work_ready` lists what is unblocked
//! (the tool sorts, the model picks), `work_discover` records work found
//! mid-session linked to its origin instead of evaporating with the
//! session, and `work_close` closes the item a harness the operator runs
//! names. A session the node started never closes the item it holds: that
//! is the operator's call, once its change has landed, so until then the
//! item stays open for CI failures and review comments to come back to.

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
            "name": WORK_CLOSE,
            "description": "Close a work item because it is done. An item a session is \
                            working on is the operator's to close, and no session closes its \
                            own. A harness you run yourself passes the item's `id`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Only for a harness you run yourself: the item to close." },
                    "summary": { "type": "string" },
                },
            },
        }),
        json!({
            "name": WORK_LIST,
            "description": "The work ledger on this channel in the order sessions take it: each \
                            item's id, title, state, what blocks it, priority (with its level's \
                            name), project and the session holding it. No bodies; work_show reads \
                            one. A session sees its own project unless it names another. A harness \
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
                            given change. Closing this session's own item, or one another session \
                            is working on, is refused: the operator closes those. The operator is \
                            asked first. An asked call \
                            returns `awaiting_operator` with an `approval_id` at once, and \
                            approval_status reports the outcome.",
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
                            twice. Close finished work instead. An item a session is working on \
                            cannot be deleted. The operator is asked first. An asked call returns \
                            `awaiting_operator` with an `approval_id` at once, and approval_status \
                            reports the outcome.",
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
pub(crate) async fn external_project(
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

/// Why a session's `work_update` closing the item it holds is refused, or
/// `None` when it is not that call. Closing ends the session on the item, and
/// that is the operator's call: the item stays open through CI and review
/// until they close it.
fn closing_own(ctx: &CallContext, held: Option<&str>, item: &str, closing: bool) -> Option<String> {
    (closing && !ctx.is_external() && held == Some(item)).then(|| {
        format!(
            "{item} is the item this session holds; the operator closes it, once its change \
             is merged"
        )
    })
}

/// A session's `work_update` that would close its own item, refused before
/// the operator is asked about a call that cannot run.
pub fn refusal_before_asking(
    access: &SessionAccess,
    ctx: &CallContext,
    args: &Value,
) -> Option<String> {
    if args["state"].as_str() != Some(tracon_sync::work::CLOSED) {
        return None;
    }
    let held = access
        .store
        .get_session(ctx.session_id()?)
        .ok()
        .flatten()?
        .work_item_id?;
    let item = corpus::work::find(&access.store, &ctx.channel, args["id"].as_str()?.trim()).ok()?;
    closing_own(
        ctx,
        Some(&held),
        &item.id,
        item.state != tracon_sync::work::CLOSED,
    )
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
                    "session {} holds {}; the operator closes it from the task page",
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
        // No session is offered it (`Tools::NOT_EXECUTE`); said again here
        // so a call that reaches this anyway closes nothing.
        WORK_CLOSE => Err(
            "the operator closes the item a session holds; it stays open \
                           through CI and review until they do"
                .into(),
        ),
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
            // Closing ends the session on the item, and that is the
            // operator's call, not the holder's: a session's item stays open
            // through CI and review until the operator closes it.
            if let Some(refusal) = closing_own(ctx, item_id.as_deref(), &item.id, closing) {
                return Err(refusal);
            }
            if closing {
                if let Some(h) = access
                    .store
                    .session_holding(&item.id)
                    .map_err(|e| e.to_string())?
                {
                    return Err(format!(
                        "session {} is working on {}; the operator closes it",
                        h.id, item.id
                    ));
                }
            }
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
            if closing {
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
        other => Err(format!("no work tool named {other}")),
    }
}
