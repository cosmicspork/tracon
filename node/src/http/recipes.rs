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

use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::api::{ApiError, ApiResult, AppState};
use super::auth::{AdminAction, LocalOrAdministrator};
use crate::config::{Config, Repo};
use crate::mesh::forward::CommandError;
use crate::mesh::recipe::{
    self, Applied, Choices, Credentials, RepoRecipe, MAX_PENDING_PER_SENDER, OFFER_TTL_MS,
};
use crate::store::now_ms;
use crate::store::recipe_offers::{CredentialHoldRow, RecipeOfferRow, Staged, MAX_OFFERS_PER_HOUR};

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
                .map_err(unprocessable)?
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
    let note = recipe::note(body.note).map_err(unprocessable)?;
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
    let note = recipe::note(note).map_err(unprocessable)?;
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
        note,
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
        Staged::TooOften => {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "the sender has offered this node {MAX_OFFERS_PER_HOUR} recipes in the last \
                     hour; offer again later"
                ),
            ))
        }
    };
    let sender_name = node_name(s, sender);
    tracing::info!(from = %sender, repo = %row.repo, offer = %row.id, superseded, "repository recipe offer staged");
    let (store, cfg) = (s.store().clone(), s.cfg.clone());
    // One notification per sender and repository: a newer offer replaces the
    // one on the operator's devices rather than adding to it.
    let tag = format!(
        "recipe-offer-{}-{}",
        sender.chars().take(16).collect::<String>(),
        row.repo.replace('/', "-")
    );
    let body = format!(
        "{sender_name} offers a recipe for {} on {from_channel}. Nothing changes until you accept it.",
        row.repo
    );
    tokio::spawn(async move {
        crate::notify::send_operator(
            &store,
            &cfg,
            &tag,
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

/// `GET /api/repos/offers`: every offer this node holds, undecided first.
/// A pending offer whose sender no longer shares its channel is withdrawn.
pub async fn list(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let now = now_ms();
    s.store().recipe_offers_expire(now)?;
    let mut rows = s.store().recipe_offers(100)?;
    for row in rows.iter_mut().filter(|row| row.state == "pending") {
        if !still_shared(&s, row)?
            && s.store()
                .recipe_offer_decide(&row.id, "withdrawn", None, now)?
        {
            row.state = "withdrawn".into();
        }
    }
    let offers: Vec<Value> = rows.iter().map(|row| offer_json(&s, row)).collect();
    Ok(Json(json!({ "offers": offers })))
}

#[derive(Deserialize, Default)]
pub struct Placement {
    /// The `[[repo]]` path to apply the offer to: an entry's own path, or a
    /// new entry's. Left out, the entry whose path is the offered
    /// repository's identity (or that names its clone), or a new entry
    /// named by that identity.
    #[serde(default)]
    path: Option<String>,
    /// Build the offered Dockerfile in place of an image this node pinned.
    #[serde(default)]
    replace_image: bool,
    /// Keep lending the entry's credentials to a preparation the offer
    /// changes. Without it they are dropped.
    #[serde(default)]
    keep_credentials: bool,
}

impl Placement {
    fn choices(&self) -> Choices {
        Choices {
            replace_image: self.replace_image,
            keep_credentials: self.keep_credentials,
        }
    }
}

/// What an accept would write, worked out the same way for the view and for
/// the accept itself.
struct Prepared {
    row: RecipeOfferRow,
    offered: RepoRecipe,
    target: Target,
    before: Option<Repo>,
    applied: Applied,
    /// What the operator reviewed: see [`viewed_hash`].
    hash: String,
}

async fn prepare(s: &AppState, id: &str, placement: &Placement) -> ApiResult<Prepared> {
    let now = now_ms();
    s.store().recipe_offers_expire(now)?;
    let row = offer_row(s, id)?;
    if row.state == "pending" && !still_shared(s, &row)? {
        s.store().recipe_offer_decide(id, "withdrawn", None, now)?;
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "this offer is withdrawn: its sender no longer shares {} with this node",
                row.channel
            ),
        ));
    }
    let offered = RepoRecipe::parse(&row.recipe).map_err(unprocessable)?;
    let repos = s.cfg.repos();
    let target = target(&s.cfg, &repos, &offered, placement.path.as_deref()).await?;
    let before = target.index.map(|i| repos[i].clone());
    let applied = offered.apply(before.as_ref(), target.path.clone(), placement.choices());
    let hash = viewed_hash(
        &target.path,
        before.is_some(),
        &applied.entry,
        placement.choices(),
    );
    Ok(Prepared {
        row,
        offered,
        target,
        before,
        applied,
        hash,
    })
}

/// The identity of what an operator reviewed: where the offer lands, whether
/// onto an existing entry, the entry as it would be written, and the choices
/// that shaped it. An accept names it, and is refused if the table has moved
/// so that the same choices would now write something else.
fn viewed_hash(path: &Path, exists: bool, after: &Repo, choices: Choices) -> String {
    let seen = json!({
        "path": path,
        "exists": exists,
        "after": after,
        "replace_image": choices.replace_image,
        "keep_credentials": choices.keep_credentials,
    });
    hex::encode(Sha256::digest(seen.to_string().as_bytes()))
}

/// `GET /api/repos/offers/{id}?path=&replace_image=&keep_credentials=`: the
/// offer, the entry it would be applied to, what would change there, and the
/// `viewed_hash` an accept of exactly this names.
pub async fn view(
    State(s): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(placement): Query<Placement>,
) -> ApiResult<Json<Value>> {
    let p = prepare(&s, &id, &placement).await?;
    Ok(Json(json!({
        "offer": offer_json(&s, &p.row),
        "target": {
            "path": p.target.path,
            "exists": p.target.index.is_some(),
        },
        "before": p.before,
        "after": p.applied.entry,
        "changes": recipe::changes(p.before.as_ref(), &p.applied.entry),
        "notes": p.applied.notes,
        "kept": p.applied.kept,
        "credentials": credentials_json(&p.applied.credentials),
        "replace_image": placement.replace_image,
        "keep_credentials": placement.keep_credentials,
        "viewed_hash": p.hash,
    })))
}

fn credentials_json(credentials: &Credentials) -> Value {
    let (state, names) = match credentials {
        Credentials::None => ("none", &[][..]),
        Credentials::Unaffected(n) => ("unaffected", &n[..]),
        Credentials::Dropped(n) => ("dropped", &n[..]),
        Credentials::KeptOnChange(n) => ("kept", &n[..]),
    };
    json!({ "state": state, "names": names })
}

#[derive(Deserialize)]
struct AcceptBody {
    #[serde(flatten)]
    placement: Placement,
    /// The `viewed_hash` of the view the operator decided on.
    viewed_hash: String,
}

/// `POST /api/repos/offers/{id}/accept {path?, replace_image?,
/// keep_credentials?, viewed_hash}`: apply the offer to an entry and write it
/// to `node.toml` through the `[[repo]]` writer, which keeps every comment
/// and every field the offer does not carry.
///
/// An admin action, confirmed off the node with a passkey for this exact
/// request, whose prompt names the sender, the repository, the fields that
/// change and what becomes of lent credentials: what an entry's `prepare`
/// names, the node runs. Refused unless the operator reviewed what it would
/// write. When the entry builds an image and the repository is on this node,
/// the build starts now, unless credentials are lent to a preparation the
/// offer changed; then it waits for the operator.
pub async fn accept(
    State(s): State<AppState>,
    UrlPath(id): UrlPath<String>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
            .into_response();
    };
    if let Err(e) = super::passkey::json_or_empty(&parts.headers, &bytes) {
        return e.into_response();
    }
    let decision: AcceptBody = match serde_json::from_slice(&bytes) {
        Ok(decision) => decision,
        Err(e) => {
            return unprocessable(format!(
                "an accept names what was reviewed (`viewed_hash`, from the offer's view): {e}"
            ))
            .into_response()
        }
    };
    let prepared = match prepare(&s, &id, &decision.placement).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    if prepared.row.state != "pending" {
        return ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "this offer is {}, not waiting for a decision",
                prepared.row.state
            ),
        )
        .into_response();
    }
    if prepared.hash != decision.viewed_hash {
        return stale().into_response();
    }
    let summary = accept_summary(&s, &prepared);
    if let Err(refusal) = super::passkey::authorize_as(&s, &parts, &bytes, Some(summary)).await {
        return refusal.into_response();
    }
    match commit(&s, &id, prepared, &decision).await {
        Ok(answer) => Json(answer).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct StartBody {
    /// The held entry's `path`, as the table writes it.
    path: String,
}

/// `POST /api/repos/holds/start {path}`: start the build an accepted offer
/// left held, lending the credentials the operator chose to keep to the
/// preparation it changed. An admin action whose prompt says what is lent.
pub async fn start_held(State(s): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 16 * 1024).await else {
        return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
            .into_response();
    };
    if let Err(e) = super::passkey::json_or_empty(&parts.headers, &bytes) {
        return e.into_response();
    }
    let body: StartBody = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(e) => return unprocessable(format!("name the held entry's path: {e}")).into_response(),
    };
    let repos = s.cfg.repos();
    let held = repos
        .iter()
        .find(|entry| same_path(&entry.path, Path::new(&body.path)))
        .and_then(|entry| recipe::held(s.store(), entry).map(|hold| (entry.clone(), hold)));
    let Some((entry, hold)) = held else {
        return ApiError::new(StatusCode::NOT_FOUND, format!("{} is not held", body.path))
            .into_response();
    };
    if let Err(refusal) =
        super::passkey::authorize_as(&s, &parts, &bytes, Some(start_summary(&hold))).await
    {
        return refusal.into_response();
    }
    match s.store().credential_hold_release(&hold.path) {
        Ok(_) => {}
        Err(e) => return ApiError::from(e).into_response(),
    }
    let building = start_build(&s, &entry);
    tracing::info!(path = %hold.path, building, "held repository build started by the operator");
    Json(json!({ "released": true, "building": building })).into_response()
}

fn stale() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "this would no longer write what you reviewed (the repository table or your choices \
         changed); review the offer again",
    )
}

/// The passkey prompt's line: who offered what, which fields change, and
/// what becomes of the credentials the entry lends.
fn accept_summary(s: &AppState, p: &Prepared) -> String {
    let fields: Vec<String> = recipe::changes(p.before.as_ref(), &p.applied.entry)
        .iter()
        .filter_map(|c| c["field"].as_str().map(str::to_string))
        .filter(|f| f != "prepare_credentials")
        .collect();
    summary_line(
        &node_name(s, &p.row.sender_node),
        &p.offered.repo,
        &fields,
        &p.applied.credentials,
    )
}

fn summary_line(sender: &str, repo: &str, fields: &[String], credentials: &Credentials) -> String {
    // What happens to lent credentials comes first, so a long line cannot
    // push it out of the prompt.
    let sender = recipe::display_name(sender);
    let what = format!(
        "accept recipe from {sender} for {repo} ({})",
        if fields.is_empty() {
            "changes nothing".to_string()
        } else {
            format!("changes {}", fields.join(", "))
        }
    );
    let line = match credentials {
        Credentials::None => {
            let mut what = what;
            what[..1].make_ascii_uppercase();
            what
        }
        Credentials::KeptOnChange(n) => format!(
            "KEEPS LENDING {} to {sender}'s changed preparation: {what}",
            n.join(", ")
        ),
        Credentials::Unaffected(n) => format!(
            "Keeps lending {} to the unchanged preparation: {what}",
            n.join(", ")
        ),
        Credentials::Dropped(n) => format!("Stops lending {}: {what}", n.join(", ")),
    };
    // The prompt shows a line, not a document.
    line.chars().take(240).collect()
}

/// The prompt for starting a held build: what is lent, to what.
fn start_summary(hold: &CredentialHoldRow) -> String {
    format!(
        "Build {} with the offered preparation, lending {}",
        hold.repo,
        hold.credentials.join(", ")
    )
    .chars()
    .take(240)
    .collect()
}

/// Write a reviewed, confirmed accept. The offer is claimed first, so two
/// accepts cannot both apply it, and released again if nothing was written.
async fn commit(
    s: &AppState,
    id: &str,
    prepared: Prepared,
    decision: &AcceptBody,
) -> ApiResult<Value> {
    if !s.store().recipe_offer_claim(id, now_ms())? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this offer is no longer waiting for a decision",
        ));
    }
    let Prepared {
        row,
        offered,
        target,
        ..
    } = prepared;
    let path = target.path.clone();
    let choices = decision.placement.choices();
    let actor = format!("recipe offer from {}", node_name(s, &row.sender_node));
    let written = crate::config::edit::update_repos(&s.cfg, s.store(), &actor, |entries| {
        // Found again in the table as it is under the lock, by path, and
        // held to what the operator reviewed.
        let index = entries
            .iter()
            .position(|entry| same_path(&entry.path, &path));
        let applied = offered.apply(index.map(|i| &entries[i]), path.clone(), choices);
        if viewed_hash(&path, index.is_some(), &applied.entry, choices) != decision.viewed_hash {
            return Err(stale());
        }
        match index {
            Some(i) => entries[i] = applied.entry.clone(),
            None => entries.push(applied.entry.clone()),
        }
        crate::config::validate_repos(entries).map_err(unprocessable)?;
        // Held before the file or the live table names the new preparation,
        // so nothing can build and warm it with the credentials in between.
        if let Credentials::KeptOnChange(names) = &applied.credentials {
            s.store().credential_hold_put(&CredentialHoldRow {
                path: path.to_string_lossy().into(),
                offer_id: id.to_string(),
                sender_node: row.sender_node.clone(),
                repo: row.repo.clone(),
                credentials: names.clone(),
                lending_hash: recipe::lending_hash(&applied.entry),
                created_ms: now_ms(),
            })?;
        }
        Ok::<_, ApiError>(applied)
    });
    let (applied, outcome) = match written {
        Ok(written) => written,
        Err(error) => {
            s.store().recipe_offer_release(id)?;
            return Err(error);
        }
    };
    if !matches!(applied.credentials, Credentials::KeptOnChange(_)) {
        // Whatever an earlier accept held, the entry now lends nothing to a
        // preparation the operator has not started.
        s.store().credential_hold_release(&path.to_string_lossy())?;
    }
    if !s
        .store()
        .recipe_offer_accepted(id, &path.to_string_lossy(), now_ms())?
    {
        tracing::warn!(offer = %id, "an accepted offer's claim was lost while it was written");
    }
    let building = applied.credentials.allows_build() && start_build(s, &applied.entry);
    tracing::info!(offer = %id, repo = %row.repo, path = %path.display(), building, "repository recipe accepted");
    Ok(json!({
        "accepted": true,
        "path": path,
        "changed": outcome.wrote(),
        "restart_required": outcome.restart_required,
        "building": building,
        "credentials": credentials_json(&applied.credentials),
    }))
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

/// Whether the sender and this node are both still members of the channel
/// the offer came on, by this node's own record.
fn still_shared(s: &AppState, row: &RecipeOfferRow) -> ApiResult<bool> {
    let members = s.store().nodes_in_channel(&row.channel)?;
    Ok(members.contains(&row.sender_node) && members.contains(&s.node_id))
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

/// A peer's name as the operator is shown it: clean and bounded.
fn node_name(s: &AppState, node: &str) -> String {
    s.store()
        .get_node(node)
        .ok()
        .flatten()
        .map(|row| recipe::display_name(&row.name))
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
        return Ok(
            match entry_for_identity(cfg, repos, &offered.repo)
                .await
                .map_err(unprocessable)?
            {
                Some(index) => Target {
                    path: repos[index].path.clone(),
                    index: Some(index),
                },
                None => Target {
                    path: PathBuf::from(&offered.repo),
                    index: None,
                },
            },
        );
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
/// whose path is that identity, one naming this node's managed clone of it,
/// or one naming a checkout whose origin it is. A relative entry that only
/// ends the same way (`acme/app` for `github.com/acme/app`) might be for
/// another forge's repository of that name, so it is not chosen for the
/// operator; they name the entry instead.
async fn entry_for_identity(
    cfg: &Config,
    repos: &[Repo],
    identity: &str,
) -> Result<Option<usize>, String> {
    let clone = crate::forge::managed_root(&Config::state_dir()).join(identity);
    for (index, entry) in repos.iter().enumerate() {
        let path = crate::config::expand_home(&entry.path);
        if !path.is_absolute() {
            if entry.path == Path::new(identity) {
                return Ok(Some(index));
            }
            if crate::config::repo_path_matches(&entry.path, Path::new(identity)) {
                return Err(format!(
                    "the entry {} answers for {identity} by the end of its path, and might be for \
                     another repository of that name; name the entry to apply the offer to",
                    entry.path.display()
                ));
            }
            continue;
        }
        if entry.matches(&clone) {
            return Ok(Some(index));
        }
        if path.is_dir() && origin_of(cfg, &path).await.as_deref() == Some(identity) {
            return Ok(Some(index));
        }
    }
    Ok(None)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What the passkey prompt says is what the accept does to credentials,
    /// first, and a peer's name cannot push it out or hide it.
    #[test]
    fn the_prompt_leads_with_the_credentials() {
        let fields = vec!["prepare".to_string(), "egress".to_string()];
        let line = |credentials: Credentials| {
            summary_line("laptop", "github.com/acme/hdr", &fields, &credentials)
        };
        assert_eq!(
            line(Credentials::KeptOnChange(vec!["nova".into()])),
            "KEEPS LENDING nova to laptop's changed preparation: accept recipe from laptop for \
             github.com/acme/hdr (changes prepare, egress)"
        );
        assert_eq!(
            line(Credentials::Unaffected(vec!["nova".into()])),
            "Keeps lending nova to the unchanged preparation: accept recipe from laptop for \
             github.com/acme/hdr (changes prepare, egress)"
        );
        assert_eq!(
            line(Credentials::Dropped(vec!["nova".into()])),
            "Stops lending nova: accept recipe from laptop for github.com/acme/hdr (changes \
             prepare, egress)"
        );
        assert_eq!(
            summary_line("laptop", "github.com/acme/hdr", &[], &Credentials::None),
            "Accept recipe from laptop for github.com/acme/hdr (changes nothing)"
        );
        let sneaky = format!("{}\u{202E}", "a".repeat(500));
        let kept = summary_line(
            &sneaky,
            "github.com/acme/hdr",
            &fields,
            &Credentials::KeptOnChange(vec!["nova".into()]),
        );
        assert!(kept.starts_with("KEEPS LENDING nova to aaaa"), "{kept}");
        assert!(!kept.contains('\u{202E}'));
        assert!(kept.contains("…"));
        assert_eq!(
            start_summary(&CredentialHoldRow {
                path: "github.com/acme/hdr".into(),
                offer_id: "o".into(),
                sender_node: "n".into(),
                repo: "github.com/acme/hdr".into(),
                credentials: vec!["nova".into()],
                lending_hash: String::new(),
                created_ms: 0,
            }),
            "Build github.com/acme/hdr with the offered preparation, lending nova"
        );
    }

    /// The reviewed identity covers every choice that shapes the write.
    #[test]
    fn the_viewed_hash_covers_the_choices() {
        let entry = Repo::default();
        let base = viewed_hash(Path::new("a"), true, &entry, Choices::default());
        for other in [
            viewed_hash(Path::new("b"), true, &entry, Choices::default()),
            viewed_hash(Path::new("a"), false, &entry, Choices::default()),
            viewed_hash(
                Path::new("a"),
                true,
                &entry,
                Choices {
                    keep_credentials: true,
                    ..Default::default()
                },
            ),
            viewed_hash(
                Path::new("a"),
                true,
                &Repo {
                    prepare: vec!["x".into()],
                    ..Default::default()
                },
                Choices::default(),
            ),
        ] {
            assert_ne!(base, other);
        }
    }
}
