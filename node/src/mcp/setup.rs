//! Setting a repository up once, as tools: draft its `[[repo]]` entry from
//! what its default branch holds, try a draft in a fresh container, and
//! propose one to the operator. See `repo_setup` for what each reads and
//! runs.
//!
//! Drafting reads and trying runs nothing a session could not: the trial is
//! the default branch, prepared with egress only to hosts this session can
//! already reach. Neither is asked. A proposal always is, whatever the
//! policy says: the node writes the entry when the operator allows it, and
//! the operator may edit it on the card first.

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

/// The fields of an entry an agent may draft. Not `path`, which the node
/// takes from the session, and not `session_egress`, which is the
/// operator's alone.
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
    let mut propose_properties = entry_properties();
    propose_properties["why"] = json!({
        "type": "string",
        "description": "What the trial showed, and anything you could not settle, in a few sentences the operator will read.",
    });
    propose_properties["repo"] = json!({
        "type": "string",
        "description": "Filled in by the node: the repository this session works in.",
    });
    vec![
        json!({
            "name": DRAFT,
            "description": "Draft this repository's `[[repo]]` entry — the image its checks run in, the \
                preparation that installs its dependencies, the checks that must pass before a change \
                is reviewed, and what preparation may reach — from what its default branch already \
                holds: a devcontainer, lockfiles, a `just check` recipe or `package.json` scripts, \
                Cargo. Returns the draft, where each field came from, notes on what it could not \
                settle, and the entry the node holds now. Hosts the operator opened to this session \
                (`request_egress`) are suggested in `egress`. Then try the draft with repo_setup_try, \
                fix what fails, and propose it with repo_setup_propose. How to run the application \
                and give it data is not part of the entry: that stays with each task.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": TRY,
            "description": format!(
                "Try an entry in a fresh container: the default branch, prepared as `prepare` says \
                 with a cache of its own, then each check on a copy of the result with no network \
                 and the cache read-only — the way required checks run. Give the fields to try, or \
                 none to try the draft. Preparation reaches only the `egress` hosts this session can \
                 already reach; the rest are listed in `egress_not_tried`, and asking for one with \
                 request_egress is how it becomes reachable (and suggested in the next draft). A \
                 `dockerfile` the node has not built yet is tried in the harness image, which the \
                 result says. Blocks for up to {} seconds and returns the trial: `running` (call \
                 again with its `trial_id`), `passed`, or `failed` with each step's exit and the end \
                 of its output. One trial runs per session at a time.",
                super::wait::MAX_WAIT_SECS
            ),
            "inputSchema": { "type": "object", "properties": try_properties },
        }),
        json!({
            "name": PROPOSE,
            "description": "Propose this repository's `[[repo]]` entry to the operator, who answers on a \
                card and may edit it first; the node writes it when they allow it, never before. It \
                replaces the entry the node holds for this repository now, and keeps whether that \
                entry opens its egress to sessions. Returns `awaiting_operator` with an approval id \
                at once: read the answer with approval_status. Try the entry first and say what the \
                trial showed in `why`. You cannot waive a check by leaving it out: the operator reads \
                the list against what the repository holds.",
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
                    let entry = match entry_from(args, &repo)? {
                        Some(entry) => entry,
                        None => {
                            let approved = approved_hosts(access, &session_id);
                            repo_setup::draft(cfg, &repo, &approved).await?.entry
                        }
                    };
                    let grants = access
                        .manager
                        .egress_grants()
                        .filter(|grants| grants.holds_session(&session_id));
                    let reachable = |host: &str| {
                        grants
                            .as_ref()
                            .is_some_and(|grants| grants.session_allows(&session_id, host))
                    };
                    repo_setup::start(
                        access.manager.backend().clone(),
                        cfg.clone(),
                        access.store.clone(),
                        &session_id,
                        repo,
                        entry,
                        reachable,
                    )?
                    .id
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
    Ok(args)
}

/// Write an allowed proposal. Only an approval runs this.
pub fn write(access: &SessionAccess, ctx: &CallContext, args: &Value) -> Result<Value, String> {
    let (_, repo) = session_repo(access, ctx)?;
    if args.get("repo").and_then(Value::as_str) != Some(&*repo.to_string_lossy()) {
        return Err("the proposal names a repository other than this session's".into());
    }
    let entry = entry_from(args, &repo)?.ok_or("the proposal names no field")?;
    let saved = repo_setup::save(access.manager.cfg(), &repo, entry)?;
    let repos = access.manager.cfg().repos();
    let written = repos.iter().find(|entry| entry.matches(&repo)).cloned();
    Ok(json!({ "repo": saved, "entry": written }))
}
