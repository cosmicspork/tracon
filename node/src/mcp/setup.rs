//! Setting a repository up once, as tools: draft its `[[repo]]` entry from
//! what its default branch holds, try a draft in a fresh container, and
//! propose one to the operator. See `repo_setup` for what each reads and
//! runs.
//!
//! Drafting reads and trying runs nothing a session could not: the trial is
//! the default branch, prepared with egress only to hosts this session can
//! already reach. Neither is asked, except a trial of a Dockerfile this node
//! has built nothing from: building it runs with the network, outside the
//! boundary, so that trial is held for the operator first. A proposal always
//! is, whatever the policy says: the node writes the entry when the operator
//! allows it, and the operator may edit it on the card first.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::{
    config::Repo,
    mcp::{CallContext, SessionAccess},
    repo_setup,
};

pub const DRAFT: &str = "repo_setup_draft";
pub const TRY: &str = "repo_setup_try";
pub const PROPOSE: &str = "repo_setup_propose";

/// What a trial held for the operator builds, beside the entry's own fields
/// and `repo`: where on the default branch the Dockerfile was read, and the
/// recipe hash an allowed trial builds and nothing else.
const BUILD_SOURCE: &str = "build_source";
const BUILD_RECIPE: &str = "build_recipe";

/// The fields of an entry an agent may draft. Not `path`, which the node
/// takes from the session, and not `session_egress` or
/// `prepare_credentials`, which are the operator's alone.
const ENTRY_FIELDS: &[&str] = &[
    "image",
    "dockerfile",
    "context",
    "checks",
    "prepare",
    "egress",
    "timeout_secs",
];

const MAX_WHY_CHARS: usize = 1000;
const MAX_RUN_NOTES_CHARS: usize = 4000;

fn entry_properties() -> Value {
    let strings = |description: &str| json!({ "type": "array", "items": { "type": "string" }, "description": description });
    json!({
        "image": {
            "type": "string",
            "description": "A toolchain image pinned by digest (`name@sha256:…`). Name this or `dockerfile`, not both; neither means the node's harness image.",
        },
        "dockerfile": {
            "type": "string",
            "description": "The repository's dev Dockerfile, as a path inside it (commonly `.devcontainer/Dockerfile`). The node builds it from the default branch.",
        },
        "context": {
            "type": "string",
            "description": "The build context for `dockerfile`, inside the repository; `.` is the root. Left out, it is the Dockerfile's directory.",
        },
        "checks": strings("The commands that must pass before a change is reviewed. Each runs in a fresh copy of the prepared tree, with no network and the dependency cache read-only."),
        "prepare": strings("Commands run once before the checks, with the dependency cache writable and `egress` reachable: installs and fetches (`cargo fetch --locked`, `bun install --frozen-lockfile`). Prefer flags that skip install scripts."),
        "egress": strings("What preparation may reach: a preset (`crates`, `npm`, `pypi`, `packagist`, `github`) or a host name."),
        "timeout_secs": {
            "type": "integer",
            "minimum": 1,
            "description": "How long preparation, and each check, may take.",
        },
    })
}

pub fn definitions() -> Vec<Value> {
    let mut try_properties = entry_properties();
    try_properties["trial_id"] = json!({
        "type": "string",
        "description": "Read a trial already started, waiting for it if it is still running.",
    });
    try_properties["wait_secs"] = json!({
        "type": "integer",
        "description": format!("How long to block, up to {}. 0 returns at once.", super::wait::MAX_WAIT_SECS),
    });
    // The same fields as a trial, said once there: a proposal follows a
    // trial, so the session has already loaded what each one means.
    let mut propose_properties = entry_properties();
    for (name, property) in propose_properties.as_object_mut().unwrap() {
        if name != "checks" {
            property.as_object_mut().unwrap().remove("description");
        }
    }
    propose_properties["checks"]["description"] =
        json!("The commands that must pass before a change is reviewed.");
    propose_properties["why"] = json!({
        "type": "string",
        "description": "What the trial showed, and anything you could not settle, in a few sentences the operator will read.",
    });
    propose_properties["run_notes"] = json!({
        "type": "string",
        "description": "Optional: how to run the application and give it data, only as you saw it work. Once allowed, a standing note every session on this channel reads.",
    });
    propose_properties["repo"] = json!({
        "type": "string",
        "description": "Filled in by the node: the repository this session works in.",
    });
    vec![
        json!({
            "name": DRAFT,
            "description": "Draft this repository's `[[repo]]` entry (its image, preparation, \
                checks and egress) from what its default branch holds: a devcontainer, lockfiles, \
                a `just check` recipe, `package.json` scripts, Cargo. Returns the draft, where each \
                field came from, what it could not settle, and the entry the node holds now. Hosts \
                the operator opened to this session are suggested in `egress`. The repo-setup skill \
                says how to try and propose it.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": TRY,
            "description": format!(
                "Try an entry the way required checks run: the default branch in a fresh \
                 container, prepared as `prepare` says, then each check on a copy with no network \
                 and the cache read-only. Give the fields to try, or none to try the draft. \
                 Preparation reaches only hosts this session can already reach; the rest are \
                 listed in `egress_not_tried`, and request_egress asks for one. Building a \
                 `dockerfile` this node has not built before is the operator's to allow, and \
                 approval_status then gives the started trial. {} Returns `running` (call again \
                 with its `trial_id`), `passed`, or `failed` with each step's exit and the end of \
                 its output. One trial runs per session at a time. {}",
                super::aside::ASKED,
                super::wait::pointer()
            ),
            "inputSchema": { "type": "object", "properties": try_properties },
        }),
        json!({
            "name": PROPOSE,
            "description": format!(
                "Propose this repository's `[[repo]]` entry, with repo_setup_try's fields, to the \
                 operator, who may edit it on the card. The node writes it only once they allow \
                 it, replacing the entry it holds now and keeping whether that entry opens its \
                 egress to sessions. Try it first and say in `why` what the trial showed. Leaving \
                 a check out does not waive it: the operator reads the list against what the \
                 repository holds. The operator is always asked. {}",
                super::aside::ASKED
            ),
            "inputSchema": {
                "type": "object",
                "properties": propose_properties,
                "required": ["checks"],
            },
        }),
    ]
}

/// The repository this session works in, and the session.
fn session_repo(access: &SessionAccess, ctx: &CallContext) -> Result<(String, PathBuf), String> {
    let session_id = ctx.session_id().ok_or(
        "set a repository up from a session the node runs; a harness you run yourself has no \
         repository the node can read",
    )?;
    let repo = crate::environment::origin_repo(&access.store, Some(session_id))?
        .ok_or("this session has no repository to set up")?;
    Ok((session_id.to_string(), repo))
}

/// The hosts the operator opened to this session, each with its approval.
fn approved_hosts(access: &SessionAccess, session_id: &str) -> Vec<(String, String)> {
    access
        .store
        .allowed_egress_asks(session_id)
        .unwrap_or_default()
        .iter()
        .filter_map(|approval| {
            super::egress::host_of(approval)
                .ok()
                .map(|host| (host, approval.id.clone()))
        })
        .collect()
}

/// The entry the arguments name, for `repo`. `None` when they name no field.
pub fn entry_from(args: &Value, repo: &std::path::Path) -> Result<Option<Repo>, String> {
    let fields: serde_json::Map<String, Value> = ENTRY_FIELDS
        .iter()
        .filter_map(|field| {
            args.get(*field)
                .filter(|value| !value.is_null())
                .map(|value| (field.to_string(), value.clone()))
        })
        .collect();
    if fields.is_empty() {
        return Ok(None);
    }
    let mut entry: Repo = serde_json::from_value(Value::Object(fields))
        .map_err(|error| format!("the entry does not parse: {error}"))?;
    entry.path = repo.to_path_buf();
    // A proposal never opens the repository's egress to its sessions.
    entry.session_egress = false;
    crate::config::validate_repos(std::slice::from_ref(&entry))?;
    Ok(Some(entry))
}

pub async fn call(
    access: &SessionAccess,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let (session_id, repo) = session_repo(access, ctx)?;
    let cfg = access.manager.cfg();
    match name {
        DRAFT => {
            let approved = approved_hosts(access, &session_id);
            let draft = repo_setup::draft(cfg, &repo, &approved).await?;
            let mut out = serde_json::to_value(&draft).map_err(|e| e.to_string())?;
            out["next"] = json!(format!(
                "Try it with {TRY} (no arguments tries this draft), fix what fails, then propose \
                 it with {PROPOSE}."
            ));
            Ok(out)
        }
        TRY => {
            let id = match args.get("trial_id").and_then(Value::as_str) {
                Some(id) => {
                    let trial = repo_setup::trial(id)
                        .filter(|trial| trial.session_id == session_id)
                        .ok_or("no such trial for this session; a node restart drops trials")?;
                    trial.id
                }
                None => {
                    if let Some(running) = repo_setup::running_for(&session_id) {
                        let mut out = serde_json::to_value(&running).map_err(|e| e.to_string())?;
                        out["note"] = json!(
                            "a trial is already running for this session; this is that trial"
                        );
                        return Ok(out);
                    }
                    let entry = try_entry(access, &session_id, &repo, args).await?;
                    start_trial(access, &session_id, repo, entry, None)?.id
                }
            };
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(super::wait::wait_secs(args));
            loop {
                let trial = repo_setup::trial(&id).ok_or("the trial is gone")?;
                if trial.state != "running" || tokio::time::Instant::now() >= deadline {
                    return serde_json::to_value(&trial).map_err(|e| e.to_string());
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        _ => Err(format!("{name} is not a setup tool")),
    }
}

/// The entry a trial tries: the fields the arguments name, or the draft.
async fn try_entry(
    access: &SessionAccess,
    session_id: &str,
    repo: &std::path::Path,
    args: &Value,
) -> Result<Repo, String> {
    match entry_from(args, repo)? {
        Some(entry) => Ok(entry),
        None => {
            let approved = approved_hosts(access, session_id);
            Ok(repo_setup::draft(access.manager.cfg(), repo, &approved)
                .await?
                .entry)
        }
    }
}

fn start_trial(
    access: &SessionAccess,
    session_id: &str,
    repo: PathBuf,
    entry: Repo,
    build: Option<String>,
) -> Result<repo_setup::Trial, String> {
    let grants = access
        .manager
        .egress_grants()
        .filter(|grants| grants.holds_session(session_id));
    let reachable = |host: &str| {
        grants
            .as_ref()
            .is_some_and(|grants| grants.session_allows(session_id, host))
    };
    repo_setup::start(
        access.manager.backend().clone(),
        access.manager.cfg().clone(),
        access.store.clone(),
        session_id,
        repo,
        entry,
        reachable,
        build,
    )
}

/// The trial to hold for the operator, when this call would start one that
/// has to build its Dockerfile first: the entry it tries, written out in
/// full so a later draft cannot change what was allowed, with what the build
/// is of. `None` for every other call to the tool, which runs at once.
pub async fn build_to_ask(
    access: &SessionAccess,
    ctx: &CallContext,
    args: &Value,
) -> Result<Option<Value>, String> {
    if args.get("trial_id").and_then(Value::as_str).is_some() {
        return Ok(None);
    }
    let (session_id, repo) = session_repo(access, ctx)?;
    if repo_setup::running_for(&session_id).is_some() {
        return Ok(None);
    }
    let entry = try_entry(access, &session_id, &repo, args).await?;
    let (backend, cfg) = (access.manager.backend(), access.manager.cfg());
    let Some(recipe) =
        repo_setup::build_needed(backend.as_ref(), cfg, &access.store, &repo, &entry).await
    else {
        return Ok(None);
    };
    let mut asked = serde_json::to_value(&entry).map_err(|e| e.to_string())?;
    let object = asked.as_object_mut().ok_or("the entry is not an object")?;
    object.retain(|field, _| ENTRY_FIELDS.contains(&field.as_str()));
    object.insert("repo".into(), json!(repo.display().to_string()));
    object.insert(
        BUILD_SOURCE.into(),
        json!(format!(
            "{} at {}",
            recipe.source_ref,
            recipe
                .source_commit
                .get(..12)
                .unwrap_or(&recipe.source_commit)
        )),
    );
    object.insert(BUILD_RECIPE.into(), json!(recipe.hash));
    Ok(Some(asked))
}

/// Start a trial the operator allowed to build its Dockerfile. It builds
/// only the recipe they were shown; the trial runs on, and the agent reads
/// it with the `trial_id` this returns.
pub fn start_allowed(
    access: &SessionAccess,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let (session_id, repo) = session_repo(access, ctx)?;
    if args.get("repo").and_then(Value::as_str) != Some(&*repo.to_string_lossy()) {
        return Err("the trial names a repository other than this session's".into());
    }
    let recipe = args
        .get(BUILD_RECIPE)
        .and_then(Value::as_str)
        .ok_or("the trial names no build")?;
    let entry = entry_from(args, &repo)?.ok_or("the trial names no field")?;
    if let Some(running) = repo_setup::running_for(&session_id) {
        return Err(format!(
            "trial {} is still running for this session; nothing was built",
            running.id
        ));
    }
    let trial = start_trial(access, &session_id, repo, entry, Some(recipe.to_string()))?;
    let mut out = serde_json::to_value(&trial).map_err(|e| e.to_string())?;
    out["next"] = json!(format!(
        "The image is building. Call {TRY} with this trial_id to wait for the trial."
    ));
    Ok(out)
}

/// The proposal as it is put to the operator: the arguments as asked, with
/// the repository the node will write the entry for, and checked now so a
/// card is never raised for an entry the node would refuse.
pub fn proposal(access: &SessionAccess, ctx: &CallContext, args: &Value) -> Result<Value, String> {
    let (_, repo) = session_repo(access, ctx)?;
    entry_from(args, &repo)?.ok_or("name the entry's fields; `checks` at least")?;
    let mut args = args.clone();
    let object = args
        .as_object_mut()
        .ok_or("the arguments must be an object")?;
    object.insert("repo".into(), json!(repo.display().to_string()));
    if let Some(why) = object.get("why").and_then(Value::as_str) {
        let why: String = why.trim().chars().take(MAX_WHY_CHARS).collect();
        object.insert("why".into(), json!(why));
    }
    match object
        .get("run_notes")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        Some(notes) if !notes.is_empty() => {
            let notes: String = notes.chars().take(MAX_RUN_NOTES_CHARS).collect();
            object.insert("run_notes".into(), json!(notes));
        }
        _ => {
            object.remove("run_notes");
        }
    }
    Ok(args)
}

/// The name the run notes for `repo` are kept under among the channel's
/// operator notes: one per repository, so a later proposal replaces them.
pub fn run_notes_name(repo: &std::path::Path) -> String {
    let name = repo
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.display().to_string());
    format!("Running {name}")
}

/// Write an allowed proposal. Only an approval runs this.
///
/// The entry is the node's, for every channel; the run notes go to the
/// operator notes of the channel the session asked from, the one place a
/// session's standing text comes from. Notes are written after the entry, so
/// a refused entry leaves no notes describing a setup that does not exist.
pub fn write(access: &SessionAccess, ctx: &CallContext, args: &Value) -> Result<Value, String> {
    let (session_id, repo) = session_repo(access, ctx)?;
    if args.get("repo").and_then(Value::as_str) != Some(&*repo.to_string_lossy()) {
        return Err("the proposal names a repository other than this session's".into());
    }
    let entry = entry_from(args, &repo)?.ok_or("the proposal names no field")?;
    let saved = repo_setup::save(
        access.manager.cfg(),
        &access.store,
        &repo,
        entry,
        &format!("an approved repository setup (session {session_id})"),
    )?;
    let repos = access.manager.cfg().repos();
    let written = repos.iter().find(|entry| entry.matches(&repo)).cloned();
    let mut out = json!({ "repo": saved, "entry": written });
    let notes = args
        .get("run_notes")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|notes| !notes.is_empty());
    if let Some(notes) = notes {
        let channel = access
            .store
            .get_session(&session_id)
            .map_err(|e| e.to_string())?
            .ok_or("this session is gone")?
            .channel;
        let name = run_notes_name(&repo);
        access
            .store
            .manifest_put_text(
                &channel,
                crate::store::manifest::KIND_INSTRUCTION,
                &name,
                notes,
            )
            .map_err(|e| e.to_string())?;
        out["run_notes"] = json!({
            "channel": channel,
            "name": name,
            "note": "kept in this channel's operator notes; the next session launched on it reads them",
        });
    }
    Ok(out)
}
