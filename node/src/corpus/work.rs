//! Writes to the work ledger. Every path (API, CLI, agent tool, review
//! publish) comes through here so an item is stamped, logged, and published
//! to the mesh the same way a document is.

use serde_json::Value;
use tracon_sync::work::{item_id, Readiness, WorkItem, CLOSED, OPEN};
use tracon_sync::ChangeOp;

use crate::store::{now_ms, work_change_row, Store, StoreError, WorkView};
use crate::stream::Bus;

pub struct NewWork {
    pub channel: String,
    pub project_id: Option<String>,
    pub title: String,
    pub body: String,
    pub deps: Vec<String>,
    pub priority: i64,
    pub discovered_from: Option<String>,
    pub discovered_by_session: Option<String>,
    /// The lane of the harness outside the boundary that recorded it.
    pub discovered_by_lane: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkError {
    #[error("title is required")]
    Title,
    #[error("no work item {0}")]
    Missing(String),
    #[error("{0}")]
    Store(#[from] StoreError),
}

/// Mint and publish a new item. The id is a hash over channel, project,
/// site, time, and title, so it is the same wherever it is later seen.
pub fn create(store: &Store, bus: &Bus, site: &str, w: NewWork) -> Result<WorkItem, WorkError> {
    let title = w.title.trim().to_string();
    if title.is_empty() {
        return Err(WorkError::Title);
    }
    let now = now_ms();
    let id = item_id(
        &w.channel,
        w.project_id.as_deref().unwrap_or(""),
        site,
        now,
        &title,
    );
    let mut deps = w.deps;
    deps.sort();
    deps.dedup();
    let item = WorkItem {
        id: id.clone(),
        channel: w.channel.clone(),
        project_id: w.project_id,
        title,
        body: w.body.trim().to_string(),
        state: OPEN.into(),
        priority: w.priority,
        deps,
        discovered_from: w.discovered_from,
        discovered_by_session: w.discovered_by_session,
        discovered_by_lane: w.discovered_by_lane,
        phase_plan_slug: None,
        closed_by_session: None,
        created_ms: now,
        updated_ms: now,
    };
    put(store, bus, site, &item)?;
    Ok(item)
}

/// Fields an update may change. `None` leaves a field alone.
#[derive(Default, serde::Deserialize)]
pub struct Patch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub deps: Option<Vec<String>>,
    pub priority: Option<i64>,
    /// `open` or `closed`.
    pub state: Option<String>,
    /// The project the item belongs to: absent leaves it, `null` clears it.
    /// The id stays what it was minted as; it is a name, not an address.
    #[serde(default, deserialize_with = "present")]
    pub project_id: Option<Option<String>>,
}

/// A field that was sent, `null` included, as `Some`; one left out stays
/// `None` through `#[serde(default)]`.
fn present<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Option::<String>::deserialize(d).map(Some)
}

pub fn update(
    store: &Store,
    bus: &Bus,
    site: &str,
    id: &str,
    patch: Patch,
    by_session: Option<&str>,
) -> Result<WorkItem, WorkError> {
    let mut item = store
        .work_get(id)?
        .ok_or_else(|| WorkError::Missing(id.into()))?;
    if let Some(t) = patch.title {
        let t = t.trim().to_string();
        if t.is_empty() {
            return Err(WorkError::Title);
        }
        item.title = t;
    }
    if let Some(b) = patch.body {
        item.body = b.trim().to_string();
    }
    if let Some(mut d) = patch.deps {
        d.retain(|x| x != id);
        d.sort();
        d.dedup();
        item.deps = d;
    }
    if let Some(p) = patch.priority {
        item.priority = p;
    }
    if let Some(project) = patch.project_id {
        item.project_id = project.filter(|p| !p.trim().is_empty());
    }
    match patch.state.as_deref() {
        Some(CLOSED) if item.state != CLOSED => {
            item.state = CLOSED.into();
            item.closed_by_session = by_session.map(str::to_string);
        }
        Some(OPEN) if item.state != OPEN => {
            item.state = OPEN.into();
            item.closed_by_session = None;
        }
        _ => {}
    }
    item.updated_ms = now_ms();
    put(store, bus, site, &item)?;
    Ok(item)
}

/// Whether an item answers a search: every word of `query` appears in its
/// title or body, ignoring case, or is the start of its id.
pub fn matches(item: &WorkItem, query: &str) -> bool {
    let title = item.title.to_lowercase();
    let body = item.body.to_lowercase();
    query.split_whitespace().all(|word| {
        let word = word.to_lowercase();
        title.contains(&word) || body.contains(&word) || item.id.starts_with(&word)
    })
}

/// The ledger in ready order, narrowed: `project` is a project id or `none`
/// for items with no project; `state` is `ready`, `blocked`, `in_session`,
/// `open` (everything not closed), `closed`, or anything else for all;
/// `query` keeps the items that `matches` it.
pub fn select(
    store: &Store,
    channel: &str,
    project: Option<&str>,
    state: Option<&str>,
    query: Option<&str>,
) -> Result<Vec<WorkView>, StoreError> {
    let none = project == Some("none");
    let items = store.work_status(channel, project.filter(|_| !none))?;
    Ok(items
        .into_iter()
        .filter(|v| !none || v.item.project_id.is_none())
        .filter(|v| match state {
            Some("ready") => v.readiness == Readiness::Ready && v.session_id.is_none(),
            Some("blocked") => matches!(v.readiness, Readiness::Blocked { .. }),
            Some("in_session") => v.readiness != Readiness::Closed && v.session_id.is_some(),
            Some("open") => v.readiness != Readiness::Closed,
            Some("closed") => v.readiness == Readiness::Closed,
            _ => true,
        })
        .filter(|v| query.is_none_or(|q| matches(&v.item, q)))
        .collect())
}

/// The shortest id prefix an agent or the CLI may name an item by.
pub const MIN_PREFIX: usize = 6;

/// An item on one channel, by its id or an unambiguous prefix of it. An
/// item on another channel is not found: a caller acts on its own channel.
pub fn find(store: &Store, channel: &str, id: &str) -> Result<WorkItem, String> {
    let id = id.trim();
    if let Some(item) = store.work_get(id).map_err(|e| e.to_string())? {
        if item.channel == channel {
            return Ok(item);
        }
        return Err(format!("no work item {id} on channel {channel}"));
    }
    if id.len() < MIN_PREFIX {
        return Err(format!(
            "no work item {id:?}: name it by its id or at least {MIN_PREFIX} characters of it"
        ));
    }
    let mut hits: Vec<WorkItem> = store
        .work_list(channel, None)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|i| i.id.starts_with(id))
        .collect();
    match hits.len() {
        0 => Err(format!("no work item {id} on channel {channel}")),
        1 => Ok(hits.remove(0)),
        n => Err(format!("{id} names {n} work items; give more of the id")),
    }
}

/// A project on a channel, by its name (any case), its id or a prefix of it.
pub fn find_project(store: &Store, channel: &str, text: &str) -> Result<String, String> {
    let text = text.trim();
    let projects = store.project_list(channel).map_err(|e| e.to_string())?;
    if let Some(p) = projects.iter().find(|p| p.id == text) {
        return Ok(p.id.clone());
    }
    let named: Vec<_> = projects
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case(text))
        .collect();
    if named.len() == 1 {
        return Ok(named[0].id.clone());
    }
    if named.len() > 1 {
        return Err(format!(
            "{} projects on {channel} are named {text:?}; name it by its id",
            named.len()
        ));
    }
    let prefixed: Vec<_> = projects
        .iter()
        .filter(|p| text.len() >= MIN_PREFIX && p.id.starts_with(text))
        .collect();
    match prefixed.len() {
        1 => Ok(prefixed[0].id.clone()),
        0 => {
            let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
            Err(if names.is_empty() {
                format!("no project {text:?} on {channel}: none is known yet")
            } else {
                format!(
                    "no project {text:?} on {channel}; known: {}",
                    names.join(", ")
                )
            })
        }
        n => Err(format!("{text} names {n} projects; give more of the id")),
    }
}

/// Close an item, recording which session did it.
pub fn close(
    store: &Store,
    bus: &Bus,
    site: &str,
    id: &str,
    by_session: Option<&str>,
) -> Result<WorkItem, WorkError> {
    update(
        store,
        bus,
        site,
        id,
        Patch {
            state: Some(CLOSED.into()),
            ..Default::default()
        },
        by_session,
    )
}

/// The document a plan session writes for an item: `plan-<id prefix>`.
pub fn plan_slug(item_id: &str) -> String {
    format!("plan-{}", &item_id[..12.min(item_id.len())])
}

/// Record the plan artifact a plan session wrote for an item.
pub fn set_plan(
    store: &Store,
    bus: &Bus,
    site: &str,
    id: &str,
    slug: &str,
) -> Result<WorkItem, WorkError> {
    let mut item = store
        .work_get(id)?
        .ok_or_else(|| WorkError::Missing(id.into()))?;
    item.phase_plan_slug = Some(slug.to_string());
    item.updated_ms = now_ms();
    put(store, bus, site, &item)?;
    Ok(item)
}

/// Tombstone an item.
pub fn remove(store: &Store, bus: &Bus, site: &str, id: &str) -> Result<bool, WorkError> {
    let Some(item) = store.work_get(id)? else {
        return Ok(false);
    };
    super::write(
        store,
        bus,
        site,
        &item.channel,
        "work_item",
        ChangeOp::Delete,
        id,
        Value::Null,
    )?;
    Ok(true)
}

fn put(store: &Store, bus: &Bus, site: &str, item: &WorkItem) -> Result<(), StoreError> {
    super::write(
        store,
        bus,
        site,
        &item.channel,
        "work_item",
        ChangeOp::Upsert,
        &item.id,
        work_change_row(item),
    )?;
    Ok(())
}
