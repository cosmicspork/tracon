//! Offering a `[[repo]]` recipe to channel co-members, and deciding the
//! offers peers made this node (`crate::mesh::recipe`).
//!
//! The sender's half reads its own entry, keeps only the portable part, and
//! sends it to the other members of one channel as a mesh command. The
//! receiver's half stages what arrives and does nothing else with it until
//! the operator, having seen what would change, accepts it onto an entry of
//! their choosing (through the one `[[repo]]` writer) or declines it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::api::{ApiError, ApiResult, AppState};
use super::auth::{AdminAction, LocalOrAdministrator};
use crate::config::{Config, Repo};
use crate::mesh::forward::CommandError;
use crate::mesh::recipe::{self, RepoRecipe, MAX_PENDING_PER_SENDER, OFFER_TTL_MS};
use crate::store::now_ms;
use crate::store::recipe_offers::{RecipeOfferRow, Staged};

fn unprocessable(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, message)
}

#[derive(Deserialize)]
pub struct OfferBody {
    /// The entry to offer, by its `path` as the table writes it.
    #[serde(default)]
    path: Option<String>,
    /// The repository's forge identity: which entry to offer when `path` is
    /// not given, and what the offer names the repository as when the
    /// entry's path does not say.
    #[serde(default)]
    repo: Option<String>,
    channel: String,
    /// Some of the channel's other members; all of them when left out.
    #[serde(default)]
    to_nodes: Option<Vec<String>>,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/repos/offer`: offer one entry's recipe to the other members of
/// a channel this node is in. Each receiver stages it for its own operator;
/// the answer says, per node, whether it was staged, is waiting in the
/// outbox for a node that is away, or was refused and why.
pub async fn offer(
    State(s): State<AppState>,
    AdminAction(Json(body)): AdminAction<Json<OfferBody>>,
) -> ApiResult<Json<Value>> {
    let mesh = s.mesh.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "this node is not on a mesh, so it has no peer to offer a recipe to",
        )
    })?;
    let channel = body.channel.trim().to_string();
    if channel.starts_with('@') || !proto::frame::valid_channel(&channel) {
        return Err(unprocessable(
            "a recipe is offered on an ordinary channel, to that channel's members",
        ));
    }
    let members = s.store().nodes_in_channel(&channel)?;
    if !members.contains(&s.node_id) {
        return Err(unprocessable(format!(
            "this node is not a member of {channel}"
        )));
    }
    let peers: Vec<String> = members
        .into_iter()
        .filter(|node| *node != s.node_id)
        .collect();
    let targets = match body.to_nodes {
        Some(mut nodes) => {
            nodes.sort();
            nodes.dedup();
            if let Some(stranger) = nodes.iter().find(|node| !peers.contains(node)) {
                return Err(unprocessable(format!(
                    "{stranger} is not another member of {channel}; a recipe is offered only to \
                     channel co-members"
                )));
            }
            nodes
        }
        None => peers,
    };
    if targets.is_empty() {
        return Err(unprocessable(format!(
            "no other node is a member of {channel}"
        )));
    }

    let repos = s.cfg.repos();
    let given_repo = body
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    if let Some(repo) = given_repo {
        recipe::valid_identity(repo).map_err(unprocessable)?;
    }
    let entry = match (body.path.as_deref().map(str::trim), given_repo) {
        (Some(path), _) if !path.is_empty() => repos
            .iter()
            .find(|entry| same_path(&entry.path, Path::new(path)))
            .cloned()
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    format!("no [[repo]] entry is {path}"),
                )
            })?,
        (_, Some(repo)) => {
            let index = entry_for_identity(&s.cfg, &repos, repo)
                .await
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::NOT_FOUND,
                        format!("no [[repo]] entry answers for {repo}"),
                    )
                })?;
            repos[index].clone()
        }
        _ => {
            return Err(unprocessable(
                "name the entry to offer by its path or its repository",
            ))
        }
    };
    let identity = match given_repo {
        Some(repo) => repo.to_string(),
        None => identity_of_entry(&s.cfg, &entry).await.ok_or_else(|| {
            unprocessable(format!(
                "{} does not say which repository it is: name its forge identity \
                 (github.com/owner/name) as `repo`",
                entry.path.display()
            ))
        })?,
    };
    let offered = RepoRecipe::from_entry(&entry, &identity);
    offered.validate().map_err(unprocessable)?;
    let recipe = serde_json::to_value(&offered).map_err(|e| unprocessable(e.to_string()))?;
    let note = recipe::clean_note(body.note);
    let expires_ms = now_ms().saturating_add(OFFER_TTL_MS);
    let wait = Duration::from_secs(s.cfg.mesh.command_timeout_secs.max(1));
    let state = &s;

    let sends = targets.iter().map(|node| {
        let command = proto::frame::Command::RecipeOffer {
            recipe: recipe.clone(),
            from_channel: channel.clone(),
            note: note.clone(),
            not_after_ms: expires_ms,
        };
        let mesh = mesh.clone();
        async move {
            // The command may wait in the outbox for as long as the offer
            // stands: a node that is away receives it when it returns.
            let result = mesh
                .command_until(node, command, wait, Some(expires_ms))
                .await;
            let name = node_name(state, node);
            match result {
                Ok(answer) => json!({
                    "node": node, "name": name, "state": "offered",
                    "offer_id": answer["offer_id"],
                }),
                Err(CommandError::Timeout) => json!({
                    "node": node, "name": name, "state": "queued",
                    "detail": "the node did not answer yet; the offer waits in the outbox and reaches it when it returns",
                }),
                Err(error) => json!({
                    "node": node, "name": name, "state": "refused",
                    "detail": error.to_string(),
                }),
            }
        }
    });
    let results = futures_util::future::join_all(sends).await;
    tracing::info!(repo = %identity, channel = %channel, nodes = targets.len(), "repository recipe offered");
    Ok(Json(json!({
        "repo": identity,
        "recipe": recipe,
        "expires_ms": expires_ms,
        "results": results,
    })))
}

/// An offer a peer sent: checked, staged for this node's operator, and
/// announced on their devices. The executor has already held the sender to
/// `from_channel`'s membership.
pub(super) async fn receive(
    s: &AppState,
    sender: &str,
    recipe: Value,
    from_channel: String,
    note: Option<String>,
    not_after_ms: i64,
) -> ApiResult<Value> {
    let offered = RepoRecipe::parse(&recipe).map_err(unprocessable)?;
    let now = now_ms();
    let expires_ms = not_after_ms.min(now.saturating_add(OFFER_TTL_MS));
    if expires_ms <= now {
        return Err(ApiError::new(
            StatusCode::GONE,
            "this offer lapsed before it arrived",
        ));
    }
    let row = RecipeOfferRow {
        id: uuid::Uuid::now_v7().to_string(),
        sender_node: sender.to_string(),
        channel: from_channel.clone(),
        repo: offered.repo.clone(),
        // Stored as this node parsed it, so what the operator reads is what
        // an accept applies.
        recipe: serde_json::to_value(&offered).map_err(|e| unprocessable(e.to_string()))?,
        note: recipe::clean_note(note),
        received_ms: now,
        expires_ms,
        state: "pending".into(),
        decided_ms: None,
        decided_path: None,
    };
    let superseded = match s.store().recipe_offer_stage(&row, MAX_PENDING_PER_SENDER)? {
        Staged::Held { superseded } => superseded,
        Staged::TooMany => {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "this node already holds {MAX_PENDING_PER_SENDER} undecided offers from the \
                     sender; its operator has not decided them"
                ),
            ))
        }
    };
    let sender_name = node_name(s, sender);
    tracing::info!(from = %sender, repo = %row.repo, offer = %row.id, superseded, "repository recipe offer staged");
    let (store, cfg, id) = (s.store().clone(), s.cfg.clone(), row.id.clone());
    let body = format!(
        "{sender_name} offers a recipe for {} on {from_channel}. Nothing changes until you accept it.",
        row.repo
    );
    tokio::spawn(async move {
        crate::notify::send_operator(
            &store,
            &cfg,
            &format!("recipe-offer-{id}"),
            "Repository recipe offered".into(),
            body,
            "/settings".into(),
            &[],
        )
        .await;
    });
    Ok(json!({
        "offer_id": row.id,
        "state": "pending",
        "expires_ms": expires_ms,
        "superseded": superseded,
    }))
}

/// `GET /api/repos/offers`: every offer this node holds, newest first.
pub async fn list(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    s.store().recipe_offers_expire(now_ms())?;
    let offers: Vec<Value> = s
        .store()
        .recipe_offers(100)?
        .into_iter()
        .map(|row| offer_json(&s, &row))
        .collect();
    Ok(Json(json!({ "offers": offers })))
}

#[derive(Deserialize, Default)]
pub struct Placement {
    /// The `[[repo]]` path to apply the offer to: an entry's own path, or a
    /// new entry's. Left out, the entry already answering for the offered
    /// repository, or a new entry named by its forge identity.
    #[serde(default)]
    path: Option<String>,
    /// Build the offered Dockerfile in place of an image this node pinned.
    #[serde(default)]
    replace_image: bool,
}

/// `GET /api/repos/offers/{id}?path=&replace_image=`: the offer, the entry
/// it would be applied to, and what would change there.
pub async fn view(
    State(s): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(placement): Query<Placement>,
) -> ApiResult<Json<Value>> {
    s.store().recipe_offers_expire(now_ms())?;
    let row = offer_row(&s, &id)?;
    let offered = RepoRecipe::parse(&row.recipe).map_err(unprocessable)?;
    let repos = s.cfg.repos();
    let target = target(&s.cfg, &repos, &offered, placement.path.as_deref()).await?;
    let before = target.index.map(|i| &repos[i]);
    let applied = offered.apply(before, target.path.clone(), placement.replace_image);
    Ok(Json(json!({
        "offer": offer_json(&s, &row),
        "target": {
            "path": target.path,
            "exists": target.index.is_some(),
        },
        "before": before,
        "after": applied.entry,
        "changes": recipe::changes(before, &applied.entry),
        "notes": applied.notes,
        "kept": recipe::LOCAL_ONLY,
    })))
}

/// `POST /api/repos/offers/{id}/accept {path?, replace_image?}`: apply the
/// offer to an entry and write it to `node.toml` through the `[[repo]]`
/// writer, which keeps every comment and every field the offer does not
/// carry. An admin action: off the node's machine it needs a passkey
/// assertion for this exact request, because what an entry's `prepare`
/// names, the node runs. When the entry builds an image and the repository
/// is on this node, the build starts now.
pub async fn accept(
    State(s): State<AppState>,
    UrlPath(id): UrlPath<String>,
    AdminAction(Json(placement)): AdminAction<Json<Placement>>,
) -> ApiResult<Json<Value>> {
    s.store().recipe_offers_expire(now_ms())?;
    let row = offer_row(&s, &id)?;
    if row.state != "pending" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("this offer is {}, not waiting for a decision", row.state),
        ));
    }
    let offered = RepoRecipe::parse(&row.recipe).map_err(unprocessable)?;
    let repos = s.cfg.repos();
    let target = target(&s.cfg, &repos, &offered, placement.path.as_deref()).await?;
    let path = target.path.clone();
    let actor = format!("recipe offer from {}", node_name(&s, &row.sender_node));
    let (written, outcome) =
        crate::config::edit::update_repos(&s.cfg, s.store(), &actor, |entries| {
            // Found again in the table as it is under the lock, by path: an
            // entry may have moved since the target was resolved.
            let index = entries
                .iter()
                .position(|entry| same_path(&entry.path, &path));
            let applied = offered.apply(
                index.map(|i| &entries[i]),
                path.clone(),
                placement.replace_image,
            );
            match index {
                Some(i) => entries[i] = applied.entry.clone(),
                None => entries.push(applied.entry.clone()),
            }
            crate::config::validate_repos(entries).map_err(unprocessable)?;
            Ok::<_, ApiError>(applied.entry)
        })?;
    let decided =
        s.store()
            .recipe_offer_decide(&id, "accepted", Some(&path.to_string_lossy()), now_ms())?;
    if !decided {
        tracing::warn!(offer = %id, "an accepted offer was decided elsewhere while it was written");
    }
    let building = start_build(&s, &written);
    tracing::info!(offer = %id, repo = %row.repo, path = %path.display(), building, "repository recipe accepted");
    Ok(Json(json!({
        "accepted": true,
        "path": path,
        "changed": outcome.wrote(),
        "restart_required": outcome.restart_required,
        "building": building,
    })))
}

/// `POST /api/repos/offers/{id}/decline`.
pub async fn decline(
    _: LocalOrAdministrator,
    State(s): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<Json<Value>> {
    let row = offer_row(&s, &id)?;
    if !s
        .store()
        .recipe_offer_decide(&id, "declined", None, now_ms())?
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("this offer is {}, not waiting for a decision", row.state),
        ));
    }
    Ok(Json(json!({ "declined": true })))
}

fn offer_row(s: &AppState, id: &str) -> ApiResult<RecipeOfferRow> {
    s.store()
        .recipe_offer(id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such offer"))
}

fn offer_json(s: &AppState, row: &RecipeOfferRow) -> Value {
    let mut value = json!(row);
    value["sender_name"] = json!(node_name(s, &row.sender_node));
    value
}

fn node_name(s: &AppState, node: &str) -> String {
    s.store()
        .get_node(node)
        .ok()
        .flatten()
        .map(|row| row.name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| node.chars().take(12).collect())
}

/// Where an offer lands: the entry at `path`, or a new one there.
struct Target {
    path: PathBuf,
    index: Option<usize>,
}

async fn target(
    cfg: &Config,
    repos: &[Repo],
    offered: &RepoRecipe,
    chosen: Option<&str>,
) -> ApiResult<Target> {
    let chosen = chosen.map(str::trim).filter(|p| !p.is_empty());
    let Some(chosen) = chosen else {
        return Ok(match entry_for_identity(cfg, repos, &offered.repo).await {
            Some(index) => Target {
                path: repos[index].path.clone(),
                index: Some(index),
            },
            None => Target {
                path: PathBuf::from(&offered.repo),
                index: None,
            },
        });
    };
    let path = PathBuf::from(chosen);
    if let Some(index) = repos.iter().position(|entry| same_path(&entry.path, &path)) {
        return Ok(Target {
            path: repos[index].path.clone(),
            index: Some(index),
        });
    }
    let expanded = crate::config::expand_home(&path);
    if expanded.is_absolute() {
        if expanded
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(unprocessable("a repository path does not climb with `..`"));
        }
        // The first matching entry wins, so a new entry below one that
        // already answers for this checkout would never apply.
        if let Some(shadow) = repos.iter().find(|entry| entry.matches(&expanded)) {
            return Err(unprocessable(format!(
                "the entry {} already answers for {chosen}; accept the offer onto that entry",
                shadow.path.display()
            )));
        }
    } else if !crate::config::safe_relative_path(chosen) {
        return Err(unprocessable(format!(
            "{chosen} is neither an absolute checkout nor a repository path like github.com/owner/name"
        )));
    }
    Ok(Target { path, index: None })
}

/// The first entry that answers for the repository `identity` names: one
/// whose relative path it ends in, one naming this node's managed clone of
/// it, or one naming a checkout whose origin it is.
async fn entry_for_identity(cfg: &Config, repos: &[Repo], identity: &str) -> Option<usize> {
    let clone = crate::forge::managed_root(&Config::state_dir()).join(identity);
    for (index, entry) in repos.iter().enumerate() {
        let path = crate::config::expand_home(&entry.path);
        if !path.is_absolute() {
            if crate::config::repo_path_matches(&entry.path, Path::new(identity)) {
                return Some(index);
            }
            continue;
        }
        if entry.matches(&clone) {
            return Some(index);
        }
        if path.is_dir() && origin_of(cfg, &path).await.as_deref() == Some(identity) {
            return Some(index);
        }
    }
    None
}

/// The forge identity an entry is for: its own path when that is one, else
/// the origin of the checkout it names.
async fn identity_of_entry(cfg: &Config, entry: &Repo) -> Option<String> {
    if let Some(identity) = recipe::identity_of_path(&entry.path) {
        return Some(identity);
    }
    let path = crate::config::expand_home(&entry.path);
    if path.is_absolute() && path.is_dir() {
        return origin_of(cfg, &path)
            .await
            .filter(|identity| recipe::valid_identity(identity).is_ok());
    }
    None
}

async fn origin_of(cfg: &Config, repo: &Path) -> Option<String> {
    crate::corpus::project::identify("", repo, &cfg.publish.git)
        .await
        .2
}

fn same_path(configured: &Path, given: &Path) -> bool {
    crate::config::expand_home(configured) == crate::config::expand_home(given)
}

/// Start building an accepted entry's image when it names a Dockerfile, the
/// runtime builds, and the repository it names is on this node. The first
/// session or check would build it otherwise, and wait.
fn start_build(s: &AppState, entry: &Repo) -> bool {
    if entry.dockerfile.is_none() || s.manager.backend().image_builder().is_none() {
        return false;
    }
    let Ok(repo) = super::environments::repo_root(&entry.path.to_string_lossy()) else {
        return false;
    };
    let (backend, cfg, store) = (
        s.manager.backend().clone(),
        s.cfg.clone(),
        s.store().clone(),
    );
    tokio::spawn(async move {
        crate::repo_image::ensure_base(backend.as_ref(), &cfg, &store, &repo, true).await;
    });
    true
}
