//! Writes to the work ledger. Every path (API, CLI, agent tool, review
//! publish) comes through here so an item is stamped, logged, and published
//! to the mesh the same way a document is.

use serde_json::Value;
use tracon_sync::work::{item_id, WorkItem, CLOSED, OPEN};
use tracon_sync::ChangeOp;

use crate::store::{now_ms, work_change_row, Store, StoreError};
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
