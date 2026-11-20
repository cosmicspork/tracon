//! Fork a session: a new session that carries on from the source's branch
//! and context, on a branch of its own, while the source keeps running.
//!
//! On this node no package is made: the fork's workspace is seeded from a
//! snapshot of the source's workspace repository (`workspace::fork_from_repo`)
//! and the session starts at once. On another node in the channel it is the
//! move's package (`transfers`), with the branch's commits, the source as it
//! stands and how the fork starts, delivered over the mesh; the receiver
//! starts the fork once the package is staged and tells this node, which
//! records the fork on the source.
//!
//! Either way the fork gets its own branch (the source's name plus a suffix
//! unless the operator names one), its own workspace, budget and model, and
//! a task of its own or none: the source keeps its task. Its first prompt
//! says it is a fork of the source at the head commit, with the operator's
//! note. The fork names its source (`forked_from`); the source records a
//! `forked` event naming the fork.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::api::{ApiError, ApiResult, AppState};
use crate::session::{NewSession, Phase};
use crate::store::SessionRow;
use crate::transfers::{self, ContextSelection, ForkItem, TransferFork, TransferIntent};

/// `POST /api/sessions/{id}/fork`.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ForkBody {
    /// The node the fork runs on. Absent, empty or this node's id: here.
    #[serde(default)]
    pub destination_node: Option<String>,
    /// The fork's branch. Absent: the source's, with `-fork` (and a number
    /// when that is taken).
    #[serde(default)]
    pub branch: Option<String>,
    /// What the fork is for, delivered in its first prompt.
    #[serde(default)]
    pub note: String,
    /// Markdown documents of the channel to hand the fork (slugs).
    #[serde(default)]
    pub documents: Vec<String>,
    /// Memories of the channel to hand the fork (ids).
    #[serde(default)]
    pub memories: Vec<String>,
    /// Absent: the source's model.
    #[serde(default)]
    pub model: Option<String>,
    /// Absent: the source's budget.
    #[serde(default)]
    pub budget_tokens: Option<i64>,
    /// Absent: the source's harness, unless a model is chosen, whose
    /// credential then picks it.
    #[serde(default)]
    pub harness: Option<String>,
    /// An existing task for the fork (never the source's).
    #[serde(default)]
    pub work_item_id: Option<String>,
    /// A new task made for the fork.
    #[serde(default)]
    pub new_item: Option<ForkItem>,
}

/// The task a fork starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkTask {
    None,
    /// One that already existed.
    Existing(String),
    /// One made for this fork.
    Made(String),
}

impl ForkTask {
    pub fn item(&self) -> Option<&str> {
        match self {
            ForkTask::None => None,
            ForkTask::Existing(item) | ForkTask::Made(item) => Some(item),
        }
    }
}

/// The task the fork takes on the node it starts on: the one it was given,
/// a new one made for it, or none. The source's own task is refused: the
/// source keeps it.
pub fn fork_task(
    s: &AppState,
    channel: &str,
    project_id: Option<&str>,
    source_item: Option<&str>,
    fork: &TransferFork,
) -> ApiResult<ForkTask> {
    if let Some(item) = fork.work_item_id.as_deref() {
        if Some(item) == source_item {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "the source keeps its task; give the fork a new task, another one, or none",
            ));
        }
        let found = s
            .store()
            .work_get(item)?
            .filter(|row| row.channel == channel)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("no task {item} on {channel}"),
                )
            })?;
        return Ok(ForkTask::Existing(found.id));
    }
    let Some(new_item) = &fork.new_item else {
        return Ok(ForkTask::None);
    };
    let item = crate::corpus::work::create(
        s.store(),
        s.manager.bus(),
        &s.node_id,
        crate::corpus::work::NewWork {
            channel: channel.to_string(),
            project_id: project_id.map(str::to_string),
            title: new_item.title.clone(),
            body: new_item.body.clone(),
            deps: vec![],
            priority: s.cfg.live().work.default_priority,
            discovered_from: None,
            discovered_by_session: None,
            discovered_by_lane: None,
        },
    )
    .map_err(super::api::work_err)?;
    Ok(ForkTask::Made(item.id))
}

/// The phase a fork starts in: execute, on its branch, unless it is on a
/// task just made for it and the channel's execute phase requires a plan,
/// which a new task does not have yet; then it plans first.
pub fn fork_phase(s: &AppState, channel: &str, task: &ForkTask) -> Phase {
    let requires_plan = s.manager.bindings(channel)["phases"]["execute"]["requires_plan"]
        .as_bool()
        .unwrap_or(true);
    match task {
        ForkTask::Made(_) if requires_plan => Phase::Plan,
        _ => Phase::Execute,
    }
}

/// Where a fork started, as its source records it.
pub struct Forked<'a> {
    pub to_session: &'a str,
    pub to_node: &'a str,
    pub branch: &'a str,
    pub head_sha: &'a str,
    pub transfer_id: Option<&'a str>,
}

/// Record on `source` (a session this node runs) that a fork of it started.
/// `false` when the source is not this node's, or the fork is already
/// recorded (a repeated confirmation).
pub fn record_forked(s: &AppState, source: &str, forked: Forked<'_>) -> bool {
    let owned = s
        .store()
        .get_session(source)
        .ok()
        .flatten()
        .is_some_and(|row| row.node_id == s.node_id);
    if !owned {
        return false;
    }
    let known = s
        .store()
        .session_events_of_kind(source, crate::session::state::event_kind::FORKED)
        .unwrap_or_default()
        .iter()
        .any(|event| event.payload["to_session"].as_str() == Some(forked.to_session));
    if known {
        return false;
    }
    s.manager.record_event(
        source,
        crate::session::state::event_kind::FORKED,
        json!({
            "to_session": forked.to_session,
            "to_node": forked.to_node,
            "branch": forked.branch,
            "head_sha": forked.head_sha,
            "transfer_id": forked.transfer_id,
        }),
    );
    true
}

/// The branch a fork of `source` gets unless the operator names one: the
/// source's with `-fork`, numbered from 2 when a session already has it.
pub fn default_branch(s: &AppState, source: &SessionRow) -> ApiResult<String> {
    let taken: std::collections::HashSet<String> = s
        .store()
        .list_sessions(None)?
        .into_iter()
        .filter(|row| row.channel == source.channel)
        .map(|row| row.branch)
        .collect();
    let stem = format!("{}-fork", source.branch);
    Ok(std::iter::once(stem.clone())
        .chain((2..1000).map(|n| format!("{stem}-{n}")))
        .find(|name| !taken.contains(name))
        .unwrap_or_else(|| format!("{stem}-{}", uuid::Uuid::now_v7().simple())))
}

fn conflict(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, message)
}

/// Fork session `id`, here or on another node of its channel. Here, the
/// fork starts at once (201, with the session). There, the package is
/// queued for that node (202), which starts the fork once it arrives.
pub async fn fork_session(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ForkBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let source = s
        .store()
        .get_session(&id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such session"))?;
    if source.node_id != s.node_id {
        return Err(conflict(format!(
            "{} runs on node {}: fork it there",
            crate::names::session(&source.id),
            crate::names::node(&source.node_id, None)
        )));
    }
    if source.harness_id == crate::session::external::HARNESS_ID {
        return Err(conflict(
            "an external session works in the operator's own checkout, which a fork cannot \
             copy; branch it there",
        ));
    }
    if source.phase == Phase::Review.as_str() {
        return Err(conflict("a review session has no work of its own to fork"));
    }
    let branch = match body.branch.as_deref().map(str::trim) {
        Some(branch) if !branch.is_empty() => branch.to_string(),
        _ => default_branch(&s, &source)?,
    };
    if !transfers::branch_name(&branch) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{branch:?} is not a branch name"),
        ));
    }
    if branch == source.branch {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a fork gets a branch of its own: name one other than its source's",
        ));
    }
    if body.budget_tokens.is_some_and(|budget| budget < 0) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "budget_tokens cannot be negative",
        ));
    }
    let model = body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);
    let harness = body
        .harness
        .as_deref()
        .map(str::trim)
        .filter(|harness| !harness.is_empty())
        .map(str::to_string);
    let new_item = match &body.new_item {
        Some(item) if !item.title.trim().is_empty() => Some(item.clone()),
        Some(_) => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "a new task for the fork needs a title",
            ))
        }
        None => None,
    };
    let work_item_id = body
        .work_item_id
        .as_deref()
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string);
    if work_item_id.is_some() && new_item.is_some() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "give the fork an existing task or a new one, not both",
        ));
    }
    if work_item_id.is_some() && work_item_id == source.work_item_id {
        return Err(conflict(
            "the source keeps its task; give the fork a new task, another one, or none",
        ));
    }
    let fork = TransferFork {
        branch,
        model,
        budget_tokens: body.budget_tokens,
        harness,
        work_item_id,
        new_item,
    };
    let selection = ContextSelection {
        documents: body.documents,
        memories: body.memories,
        note: body.note,
    };
    let destination = body
        .destination_node
        .map(|node| node.trim().to_string())
        .filter(|node| !node.is_empty() && node != &s.node_id);
    if destination.is_some() && s.mesh.is_none() {
        return Err(transfers::TransferError::MeshUnavailable.into());
    }
    let git = s.cfg.live().publish.git.clone();
    let snapshot = s.manager.snapshot_workspace(&source.id).await?;
    let head = crate::workspace::branch_head(&git, &snapshot, &source.branch)
        .await
        .map_err(|error| conflict(format!("its branch could not be read: {error}")))?;
    match destination {
        None => fork_here(&s, &source, &snapshot, &head, fork, selection).await,
        Some(node) => fork_there(&s, &source, &snapshot, &head, fork, selection, &node).await,
    }
}

/// The fork on this node: its workspace branched from the source's
/// repository, no package, and the session started at once.
async fn fork_here(
    s: &AppState,
    source: &SessionRow,
    snapshot: &std::path::Path,
    head: &str,
    fork: TransferFork,
    selection: ContextSelection,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let channel = source.channel.clone();
    let context = transfers::select_context(s.store(), &channel, selection)?;
    let task = fork_task(
        s,
        &channel,
        source.project_id.as_deref(),
        source.work_item_id.as_deref(),
        &fork,
    )?;
    let described = transfers::describe_source(
        s.store(),
        source,
        TransferIntent::Fork,
        s.manager.waits().of(&source.id),
    )?;
    let prompt = transfers::fork_prompt(&transfers::ForkPrompt {
        source: &described,
        origin_node: &s.node_id,
        head_sha: head,
        source_branch: Some(&source.branch),
        branch: &fork.branch,
        note: &context.note,
        history: true,
        work_item_id: task.item(),
    });
    let mut spec = NewSession {
        channel: channel.clone(),
        repo_path: String::new(),
        branch: Some(fork.branch.clone()),
        base: None,
        work_item_id: task.item().map(str::to_string),
        model: fork.model.clone().unwrap_or_else(|| source.model.clone()),
        budget_tokens: Some(fork.budget_tokens.unwrap_or(source.budget_tokens)),
        initial_prompt: Some(prompt),
        node_id: None,
        phase: fork_phase(s, &channel, &task),
        review_id: None,
        base_sha: None,
        workspace_id: None,
        parent_session: None,
        continued_from: None,
        forked_from: Some(source.id.clone()),
        // The source's harness goes with its model, while this node still
        // runs it; a model of the operator's own choosing picks its harness
        // by its credential.
        harness: fork.harness.clone().or_else(|| {
            (fork.model.is_none() && crate::adapter::is_enabled(&s.cfg, &source.harness_id))
                .then(|| source.harness_id.clone())
        }),
        on_exhaustion: None,
        told_reviews: Vec::new(),
        project_id: source.project_id.clone(),
    };
    // Everything `create` would refuse outright is refused before a
    // workspace is made, so a fork doomed to fail leaves none behind.
    s.manager.preflight(&spec)?;
    let git = s.cfg.live().publish.git.clone();
    let workspace_id = format!("fork-{}", uuid::Uuid::now_v7());
    let workspace = seed_here(s, &git, snapshot, head, &fork.branch, &workspace_id, {
        json!({
            "forked_from": source.id,
            "origin_node": s.node_id,
            "channel": channel,
            "head_sha": head,
            "context": context,
            "source": described,
        })
    })
    .await?;
    // The fork reads its forge as its source does: the same clone.
    let origin = crate::environment::origin_repo(s.store(), Some(&source.id))
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    s.store().record_workspace_origin(
        &workspace.id,
        origin.as_deref().and_then(|origin| origin.to_str()),
    )?;
    spec.workspace_id = Some(workspace.id.clone());
    let session = s.manager.create(spec).await?;
    record_forked(
        s,
        &source.id,
        Forked {
            to_session: &session.id,
            to_node: &s.node_id,
            branch: &session.branch,
            head_sha: head,
            transfer_id: None,
        },
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "delivery": "local",
            "destination_node": s.node_id,
            "session": session,
            "forked_from": source.id,
            "head_sha": head,
            "branch": session.branch,
            "workspace_id": workspace.id,
            "work_item_id": session.work_item_id,
        })),
    ))
}

/// Seed the fork's workspace from the source's snapshot and hand it to the
/// runtime, with the fork's context beside the work as an import's is.
async fn seed_here(
    s: &AppState,
    git: &str,
    snapshot: &std::path::Path,
    head: &str,
    branch: &str,
    id: &str,
    context: Value,
) -> ApiResult<crate::workspace::Workspace> {
    let stage = crate::workspace::staging_path(id);
    let seeded = async {
        let workspace = crate::workspace::fork_from_repo(git, snapshot, head, branch, id).await?;
        crate::workspace::write_context(&stage, &context)?;
        // The context is provenance for the session, not part of its work:
        // keep it out of what a `git add -A` would put up for review.
        let exclude = stage.join(".git").join("info").join("exclude");
        let mut rules = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !rules.lines().any(|line| line.trim() == "/.tracon/") {
            if !rules.is_empty() && !rules.ends_with('\n') {
                rules.push('\n');
            }
            rules.push_str("/.tracon/\n");
            std::fs::write(&exclude, rules)?;
        }
        crate::workspace::import(s.manager.backend().as_ref(), &workspace, &stage).await?;
        Ok::<_, crate::workspace::WorkspaceError>(workspace)
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    seeded.map_err(|error| conflict(format!("the fork's workspace could not be made: {error}")))
}

/// The fork on another node: the move's package, with the fork's start,
/// queued for that node, which starts it once the package is staged.
async fn fork_there(
    s: &AppState,
    source: &SessionRow,
    snapshot: &std::path::Path,
    head: &str,
    fork: TransferFork,
    selection: ContextSelection,
    destination: &str,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let candidate_id = capture_candidate(s, source, snapshot, head).await?;
    let git = super::api::capture_branch_from(s, source, snapshot, head)
        .await
        .map_err(|error| conflict(format!("the branch's history could not be read: {error}")))?;
    let described = transfers::describe_source(
        s.store(),
        source,
        TransferIntent::Fork,
        s.manager.waits().of(&source.id),
    )?;
    let (identity, _) = crate::mesh::identity::load_or_generate()
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let branch = fork.branch.clone();
    let transfer = transfers::export_with(
        s.store(),
        &identity,
        &candidate_id,
        selection,
        Some(destination.to_string()),
        transfers::Extras {
            git: Some(git),
            source: Some(described),
            fork: Some(fork),
        },
    )?;
    let pieces = super::api::deliver_mesh(s, &transfer, destination, None)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "delivery": "mesh_queued",
            "destination_node": destination,
            "transfer_id": transfer.id(),
            "forked_from": source.id,
            "head_sha": head,
            "branch": branch,
            "pieces": pieces,
            "note": "The fork starts on that node once the package arrives; this session \
                     carries on.",
        })),
    ))
}

/// The candidate a fork's package is made from: the source's head, with its
/// files kept, recorded as `submit_review` records one. One already kept is
/// reused.
async fn capture_candidate(
    s: &AppState,
    source: &SessionRow,
    snapshot: &std::path::Path,
    head: &str,
) -> ApiResult<String> {
    let id = crate::store::candidate_id(head, &source.channel);
    let kept = s.store().candidate(&id)?.is_some_and(|row| {
        serde_json::from_str::<Value>(&row.capture_json)
            .ok()
            .and_then(|capture| capture["materialized"].as_bool())
            .unwrap_or(false)
    });
    if kept {
        return Ok(id);
    }
    let max_bytes = s.cfg.live().supervision.max_snapshot_bytes;
    let tree = crate::review::read_candidate(&snapshot.to_string_lossy(), head, max_bytes)
        .await
        .map_err(|error| conflict(format!("its head could not be captured: {error}")))?;
    let row = crate::store::CandidateRow {
        id: id.clone(),
        head_sha: head.to_string(),
        tree_sha: Some(tree.tree_sha.clone()),
        channel: source.channel.clone(),
        owner_session_id: Some(source.id.clone()),
        source_kind: "git".into(),
        captured_ms: crate::store::now_ms(),
        capture_json: json!({
            "method": "hardened_git_tree",
            "materialized": true,
            "max_snapshot_bytes": max_bytes,
        })
        .to_string(),
    };
    if !s.store().insert_candidate(&row)? {
        s.store()
            .record_candidate_snapshot(&id, &tree.tree_sha, &row.capture_json)?;
    }
    s.store().insert_candidate_files(&id, &tree.files)?;
    s.store().record_candidate_materialized(&id)?;
    Ok(id)
}
