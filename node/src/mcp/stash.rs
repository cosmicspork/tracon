//! `stash_put` and `stash_get`: bytes a session keeps per repository because
//! they are expensive to rebuild, found again by a name and a digest of the
//! inputs they were built from (`crate::stash`).
//!
//! The node computes the digest itself, from its own snapshot of the
//! workspace, so an entry is only ever handed back for the inputs it was
//! built from: a changed input misses. A hit is unpacked into the session's
//! running sandbox at the path it was stashed from.
//!
//! Only sessions the node runs are offered these. A check or a preparation
//! has no tools, and the stash is never mounted into a run, so nothing an
//! agent stashed reaches a check. Like `show_work` they are dispatched before
//! the policy: they reach no forge and nothing outside this node, and what
//! they keep is read back only by another session's `stash_get`.

use std::path::Path;

use serde_json::{json, Value};

use crate::{
    mcp::{CallContext, SessionAccess},
    stash::{self, Input, Scope, Stash, StashError},
    store::now_ms,
};

pub const PUT: &str = "stash_put";
pub const GET: &str = "stash_get";

const MAX_INPUTS: usize = 64;

/// One stash operation at a time on this node: a get's tree is not evicted
/// while it is being unpacked, and two puts do not evict past each other.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn definitions() -> Vec<Value> {
    let key = json!({
        "type": "string",
        "description": "The name the entry is kept under, such as `before-screenshots`.",
    });
    let inputs = json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "What the bytes are built from: files or directories of your workspace, \
                        relative to the repository root, or a Git object as \
                        `<revision>:<path>` (`origin/main:spa`, or `origin/main:` for the \
                        whole tree). The node digests them itself; the same key with other \
                        inputs is another entry.",
    });
    vec![
        json!({
            "name": PUT,
            "description": "Keep bytes that are expensive to rebuild for later sessions on this \
                 repository, such as \"before\" screenshots of the default branch: `path` \
                 (a file or directory in your workspace, relative to the repository root) is \
                 kept under `key` and a digest of `inputs`, which the node computes from its \
                 own snapshot of your workspace. stash_get hands it back only while the \
                 inputs are unchanged. Files git ignores are not read, so write what you \
                 stash to a path no ignore rule matches, and keep it out of your commits. \
                 Bytes only: what you learned belongs in a memory. Each repository has 1 GB; \
                 past that the least recently used entries are evicted. Checks never read \
                 the stash.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": key,
                    "inputs": inputs,
                    "path": { "type": "string", "description": "The file or directory to keep, relative to the repository root." },
                },
                "required": ["key", "inputs", "path"],
            },
        }),
        json!({
            "name": GET,
            "description": "Restore what stash_put kept under `key` for these `inputs`, at the \
                            path it was kept from, over what your workspace has there. It \
                            hits only while every input is as it was when it was put; \
                            otherwise it misses (`hit: false`) and you rebuild, then \
                            stash_put again. Check before you rebuild something expensive, \
                            such as \"before\" screenshots of the default branch.",
            "inputSchema": {
                "type": "object",
                "properties": { "key": key, "inputs": inputs },
                "required": ["key", "inputs"],
            },
        }),
    ]
}

pub async fn call(
    access: &SessionAccess,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let session_id = ctx
        .session_id()
        .ok_or("the stash is kept for sessions this node runs")?;
    let key = args.get("key").and_then(Value::as_str).unwrap_or("").trim();
    if let Some(refusal) = stash::key_refusal(key) {
        return Err(refusal);
    }
    let named = inputs_arg(args)?;
    let path = match name {
        PUT => Some(path_arg(args)?),
        _ => None,
    };
    let session = access
        .store
        .get_session(session_id)
        .map_err(|e| e.to_string())?
        .ok_or("this session is gone")?;
    let repo = crate::environment::origin_repo(&access.store, Some(session_id))?
        .ok_or("this session has no repository to keep a stash for")?;
    let scope = Scope {
        channel: session.channel.clone(),
        repo: repo.to_string_lossy().into_owned(),
    };

    let _held = LOCK.lock().await;
    let root = access
        .manager
        .snapshot_workspace(session_id)
        .await
        .map_err(|e| e.to_string())?;
    let tool = if path.is_some() { PUT } else { GET };
    let inputs = resolve_inputs(&root, &named, tool).await?;
    if let Some(path) = &path {
        exists(&root, path, tool)?;
    }
    let digest = {
        let root = root.clone();
        let inputs = inputs.clone();
        tokio::task::spawn_blocking(move || stash::digest(&root, &inputs))
            .await
            .map_err(|e| e.to_string())?
            .map_err(refused)?
    };
    let store = Stash::node();
    match path {
        Some(path) => {
            let key = key.to_string();
            let scope_for = scope.clone();
            let digest_for = digest.clone();
            let session_for = session_id.to_string();
            let put = tokio::task::spawn_blocking(move || {
                store.put(
                    &scope_for,
                    &key,
                    &digest_for,
                    &inputs,
                    &root,
                    &path,
                    Some(&session_for),
                    now_ms(),
                )
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(refused)?;
            let mut out = json!({
                "key": put.meta.key,
                "digest": digest,
                "path": put.meta.path,
                "files": put.meta.files,
                "bytes": put.meta.bytes,
                "repository_bytes": put.used_bytes,
                "limit_bytes": stash::LIMIT_BYTES,
                "note": "Kept for sessions on this repository. stash_get with the same key \
                         and inputs restores it while the inputs are unchanged.",
            });
            if !put.skipped.is_empty() {
                out["skipped"] = json!(put.skipped);
            }
            if !put.evicted.is_empty() {
                out["evicted"] = json!(put.evicted);
            }
            Ok(out)
        }
        None => {
            let found = {
                let key = key.to_string();
                let scope = scope.clone();
                let digest = digest.clone();
                tokio::task::spawn_blocking(move || store.get(&scope, &key, &digest, now_ms()))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(refused)?
            };
            let hit = match found {
                Ok(hit) => hit,
                Err(miss) => {
                    let note = if miss.other_inputs > 0 {
                        format!(
                            "Nothing kept under {key} for these inputs as they are now; {} \
                             entr{} under {key} {} built from other inputs and will not be \
                             used. Rebuild, then stash_put.",
                            miss.other_inputs,
                            if miss.other_inputs == 1 { "y" } else { "ies" },
                            if miss.other_inputs == 1 {
                                "was"
                            } else {
                                "were"
                            },
                        )
                    } else {
                        format!(
                            "Nothing kept under {key} for these inputs. Rebuild, then stash_put."
                        )
                    };
                    return Ok(json!({
                        "hit": false,
                        "key": key,
                        "digest": digest,
                        "note": note,
                    }));
                }
            };
            restore(&session, &hit.data).await?;
            Ok(json!({
                "hit": true,
                "key": hit.meta.key,
                "digest": digest,
                "path": hit.meta.path,
                "files": hit.meta.files,
                "bytes": hit.meta.bytes,
                "stashed_ms": hit.meta.created_ms,
                "note": format!(
                    "Restored to {} in your workspace, built from these inputs as they are now.",
                    hit.meta.path
                ),
            }))
        }
    }
}

fn refused(error: StashError) -> String {
    match error {
        StashError::Refused(why) => why,
        other => other.to_string(),
    }
}

/// The inputs as the agent named them: at least one, each a workspace path
/// or `<revision>:<path>`.
fn inputs_arg(args: &Value) -> Result<Vec<String>, String> {
    let Some(items) = args.get("inputs").and_then(Value::as_array) else {
        return Err("inputs is a list of what the bytes are built from".into());
    };
    if items.is_empty() {
        return Err(
            "name at least one input, so a changed input misses rather than returning stale bytes"
                .into(),
        );
    }
    if items.len() > MAX_INPUTS {
        return Err(format!("at most {MAX_INPUTS} inputs"));
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "each input is a path or `<revision>:<path>`".to_string())
        })
        .collect()
}

fn path_arg(args: &Value) -> Result<String, String> {
    let asked = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or("path is the file or directory to keep, relative to the repository root")?;
    relative(asked)
}

/// A path relative to the repository root, outside Git's own directory.
fn relative(asked: &str) -> Result<String, String> {
    let path = super::show::clean(asked)
        .map_err(|_| format!("{asked:?} is not a path relative to the repository root"))?;
    if path.split('/').any(|segment| segment == ".git") {
        return Err(format!("{path} is Git's own directory"));
    }
    Ok(path)
}

fn exists(root: &Path, path: &str, tool: &str) -> Result<(), String> {
    if std::fs::symlink_metadata(root.join(path)).is_err() {
        return Err(super::show::missing_for(tool, root, path));
    }
    Ok(())
}

/// Each input read against the snapshot: a path that must be there, or a
/// revision Git resolves to the object it names.
async fn resolve_inputs(root: &Path, named: &[String], tool: &str) -> Result<Vec<Input>, String> {
    let mut out = Vec::with_capacity(named.len());
    for input in named {
        match input.split_once(':') {
            Some((revision, path)) => {
                if revision.is_empty()
                    || revision.starts_with('-')
                    || revision
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control())
                {
                    return Err(format!(
                        "{input:?} is not `<revision>:<path>`, such as `origin/main:spa`"
                    ));
                }
                if !path.is_empty() {
                    relative(path)?;
                }
                let object = crate::review::resolve(&root.to_string_lossy(), input)
                    .await
                    .ok()
                    .filter(|object| !object.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "{input} names nothing in your workspace's repository; fetch_branch \
                             brings a branch of origin as origin/<branch>"
                        )
                    })?;
                out.push(Input::Object {
                    named: input.clone(),
                    object,
                });
            }
            None => {
                let path = relative(input)?;
                exists(root, &path, tool)?;
                out.push(Input::Path(path));
            }
        }
    }
    Ok(out)
}

/// Unpack a hit over the session's workspace in its running sandbox, where
/// the agent looks and the next sync takes it back from.
async fn restore(session: &crate::store::SessionRow, data: &Path) -> Result<(), String> {
    let workspace_id = session
        .repo_path
        .strip_prefix("workspace://")
        .unwrap_or(&session.id);
    let volume = crate::workspace::volume_name(workspace_id);
    let (conn, dir) = crate::runner::sandbox_files::live_target(&volume).ok_or(
        "stash_get restores into this session's running sandbox, and none holds its workspace",
    )?;
    conn.import_dir(data, dir, tracon_sandbox::protocol::Exclusions::None)
        .await
        .map_err(|error| format!("the stashed files could not be restored: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_are_required_and_bounded() {
        assert!(inputs_arg(&json!({})).is_err());
        assert!(inputs_arg(&json!({ "inputs": [] })).is_err());
        assert!(inputs_arg(&json!({ "inputs": [""] })).is_err());
        assert!(inputs_arg(&json!({ "inputs": [1] })).is_err());
        let many: Vec<String> = (0..=MAX_INPUTS).map(|i| format!("f{i}")).collect();
        assert!(inputs_arg(&json!({ "inputs": many })).is_err());
        assert_eq!(
            inputs_arg(&json!({ "inputs": [" spa ", "origin/main:spa"] })).unwrap(),
            ["spa", "origin/main:spa"]
        );
    }

    #[test]
    fn paths_stay_inside_the_repository_and_out_of_git() {
        assert_eq!(relative("./shots/").unwrap(), "shots");
        for bad in ["/etc", "../x", "a/../../b", ".git", "a/.git/config", ""] {
            assert!(relative(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_revision_input_is_its_object_and_an_option_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let sh = |script: &str| {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .current_dir(dir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        sh(
            "git init -q -b main && mkdir spa && echo one > spa/a && git add . \
            && git -c user.name=t -c user.email=t@t commit -qm one",
        );
        let tree = sh("git rev-parse main:spa");
        let named = ["main:spa".to_string(), "spa".to_string()];
        let inputs = resolve_inputs(dir.path(), &named, GET).await.unwrap();
        assert_eq!(
            inputs,
            [
                Input::Object {
                    named: "main:spa".into(),
                    object: tree
                },
                Input::Path("spa".into())
            ]
        );
        for bad in ["--output=x:y", ":spa", "nope:spa", "main:../x"] {
            assert!(
                resolve_inputs(dir.path(), &[bad.to_string()], GET)
                    .await
                    .is_err(),
                "{bad}"
            );
        }
        let missing = resolve_inputs(dir.path(), &["shots".to_string()], GET)
            .await
            .unwrap_err();
        assert!(missing.contains("not in the workspace"), "{missing}");
    }

    #[test]
    fn the_definitions_say_bytes_only_and_that_checks_never_read_it() {
        let defs = definitions();
        let put = defs[0]["description"].as_str().unwrap();
        assert!(put.contains("memory"), "{put}");
        assert!(put.contains("Checks never read"), "{put}");
        assert!(
            put.contains("1 GB") && stash::LIMIT_BYTES == 1 << 30,
            "{put}"
        );
    }
}
