//! What a compacted context summarises, kept: before a compaction or a
//! context reset, the node saves the session's full-text conversation and its
//! scratchpad, so the summary the model works from afterwards is of something
//! that still exists.
//!
//! **Where.** In the session's scratch volume, the package its harness state
//! is backed up in (`materialize::OWN_STATE`, the native harness's
//! `harness/run`), under [`DIR`]: one directory per save, named for its index
//! and what it was saved before (`0001-compaction`, `0002-restart`). Nothing
//! mounts `saved/` into a sandbox, so the harness can neither read nor change
//! what was saved, and nothing a sandbox sends back replaces it. It goes
//! wherever the harness state goes: it is there again when the session is
//! reopened or its harness relaunched, it is measured and removed with the
//! session's runtime storage, and it is read back through the session
//! (`GET /api/sessions/{id}/saved…`, `tracon session saved`).
//!
//! **What.** Each save holds:
//!
//! - `conversation.md`: the session's log as the node recorded it, rendered
//!   as text: the orientation, each prompt, the agent's messages and
//!   thinking, every tool call with its input and result, plans, and earlier
//!   compactions' summaries;
//! - `transcript/`: the harness's own record of the conversation, copied from
//!   the node's copy of its state (`HarnessAdapter::transcripts`): Claude
//!   Code's `projects/<dir>/<id>.jsonl` and its subagents', `tracon-agent`'s
//!   journal. Tool output the log cut short (`truncated`) is whole here;
//! - `scratchpad/`: the session's scratchpad, the directory the agent is
//!   given at [`scratchpad_target`] for notes it keeps across its context;
//! - `save.json`: the [`Manifest`].
//!
//! **When.** A compaction the operator asks for is saved before its turn is
//! dispatched. One the harness starts itself is saved as it says it has
//! begun (Claude Code's `compacting` status, `tracon-agent`'s
//! `context_compacting`), or at its `context_compacted` when no start was
//! said. Both harnesses append a compaction to their record rather than
//! rewriting it, so a save taken as it runs still holds everything before
//! it. A harness relaunched after its process died is saved before the new
//! process rebuilds its context. Each save first takes the running sandbox's
//! trees back, so the transcript and the scratchpad are as they are now, not
//! as the last periodic take-back left them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adapter::HarnessAdapter;
use crate::boundary::Backend;
use crate::store::{EventRow, Store};

/// Where the saves are, in the session's scratch volume.
pub const DIR: &str = "saved";
/// Where the session's scratchpad is, in its scratch volume.
pub const SCRATCHPAD: &str = "scratchpad";
pub const CONVERSATION: &str = "conversation.md";
pub const TRANSCRIPT: &str = "transcript";
pub const MANIFEST: &str = "save.json";
/// What an export says it is, so a reader without tracon knows the format.
pub const EXPORT_KIND: &str = "tracon.saved_context";
pub const EXPORT_VERSION: u32 = 1;

/// Why a save was taken.
pub const BEFORE_COMPACTION: &str = "compaction";
pub const BEFORE_RESTART: &str = "restart";

/// The most events a conversation is rendered from. A session's log is
/// bounded long before this.
const MAX_EVENTS: i64 = 1_000_000;
const EVENTS_PAGE: i64 = 5_000;

/// Where a sandbox finds the session's scratchpad: beside its harness state,
/// under the harness's home.
pub fn scratchpad_target(home: &str) -> String {
    format!("{home}/{SCRATCHPAD}")
}

/// A harness session id that cannot leave the directory it is looked for in.
pub fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// What one save is, as `save.json` says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub index: u32,
    /// Its directory under [`DIR`].
    pub name: String,
    /// [`BEFORE_COMPACTION`] or [`BEFORE_RESTART`].
    pub before: String,
    /// The compaction's trigger, as its `context_compacting` says
    /// (`manual`, `threshold`, `overflow`), when it said one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    pub harness: String,
    pub harness_session_id: String,
    pub created_ms: i64,
    /// The last event of the log `conversation.md` was rendered from.
    pub through_seq: i64,
    pub conversation_bytes: u64,
    /// Whether the running sandbox's trees were taken back first. `false`
    /// when there was no running sandbox (a relaunch's, whose process died)
    /// or the take-back failed: the node's last copy was saved.
    pub synced: bool,
    pub transcripts: Vec<SavedFile>,
    pub scratchpad: Vec<SavedFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedFile {
    /// Relative to the save's directory.
    pub path: String,
    pub bytes: u64,
}

/// One save read back: its manifest and conversation, and its files.
#[derive(Debug, Clone, Serialize)]
pub struct Saved {
    pub manifest: Manifest,
    pub conversation: String,
    pub files: Vec<SavedFile>,
}

/// When a save is taken.
#[derive(Debug, Clone)]
pub struct Moment {
    pub before: &'static str,
    pub trigger: Option<String>,
    pub harness_session_id: String,
    /// The session's sandbox, whose trees are taken back first.
    pub container: String,
}

/// Takes a session's saves. Cheap to clone; one per session's supervisor.
#[derive(Clone)]
pub struct Saver {
    backend: Arc<dyn Backend>,
    adapter: Arc<dyn HarnessAdapter>,
    session_id: String,
    volume: String,
}

impl Saver {
    pub fn new(
        backend: Arc<dyn Backend>,
        adapter: Arc<dyn HarnessAdapter>,
        session_id: &str,
    ) -> Self {
        Self {
            backend,
            adapter,
            session_id: session_id.to_string(),
            volume: crate::workspace::scratch_volume_name(session_id),
        }
    }

    pub fn volume(&self) -> &str {
        &self.volume
    }

    /// Save the conversation and the scratchpad now, into the session's
    /// scratch volume. The event payload of the `context_saved` it is
    /// recorded as, or why it could not be saved.
    pub async fn save(&self, store: &Store, moment: &Moment) -> Result<Manifest, String> {
        // The sandbox's own trees first, without the volume's lock: the
        // take-back takes it.
        let synced = match crate::runner::sandbox_files::sync_named(&moment.container).await {
            Some(Ok(())) => true,
            Some(Err(error)) => {
                tracing::warn!(session = %self.session_id, %error, "saving the context from the node's last copy");
                false
            }
            None => false,
        };
        // The log through now, before anything else is said.
        let events = session_events(store, &self.session_id)?;
        let through_seq = events.last().map_or(0, |e| e.seq);
        let conversation = render_conversation(&self.session_id, &events, moment);

        let lock = crate::workspace::volume_lock(&self.volume);
        let _held = lock.lock().await;
        let node_copy = NodeCopy::open(self.backend.as_ref(), &self.volume).await?;
        let root = node_copy.root();
        let index = next_index(&root.join(DIR));
        let name = format!("{index:04}-{}", moment.before);
        let tmp =
            tempfile::tempdir_in(crate::config::Config::state_dir()).map_err(|e| e.to_string())?;
        let staged = tmp.path().join(&name);
        std::fs::create_dir_all(&staged).map_err(|e| e.to_string())?;
        std::fs::write(staged.join(CONVERSATION), &conversation).map_err(|e| e.to_string())?;

        let mut transcripts = Vec::new();
        for relative in self.adapter.transcripts(&root, &moment.harness_session_id) {
            let source = root.join(&relative);
            let dest = staged.join(TRANSCRIPT).join(&relative);
            copy_entry(&source, &dest).map_err(|e| e.to_string())?;
        }
        list_files(&staged, &staged.join(TRANSCRIPT), &mut transcripts);
        let mut scratchpad = Vec::new();
        let pad = root.join(SCRATCHPAD);
        if pad.is_dir() {
            crate::workspace::copy_tree(&pad, &staged.join(SCRATCHPAD), false)
                .map_err(|e| e.to_string())?;
            list_files(&staged, &staged.join(SCRATCHPAD), &mut scratchpad);
        }
        let manifest = Manifest {
            index,
            name: name.clone(),
            before: moment.before.to_string(),
            trigger: moment.trigger.clone(),
            harness: self.adapter.id().to_string(),
            harness_session_id: moment.harness_session_id.clone(),
            created_ms: crate::store::now_ms(),
            through_seq,
            conversation_bytes: conversation.len() as u64,
            synced,
            transcripts,
            scratchpad,
        };
        std::fs::write(
            staged.join(MANIFEST),
            serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        node_copy
            .add(self.backend.as_ref(), &self.volume, &name, &staged)
            .await?;
        Ok(manifest)
    }
}

/// The event a save is recorded as.
pub fn event_payload(manifest: &Manifest) -> Value {
    json!({
        "index": manifest.index,
        "name": manifest.name,
        "before": manifest.before,
        "trigger": manifest.trigger,
        "through_seq": manifest.through_seq,
        "conversation_bytes": manifest.conversation_bytes,
        "transcript_files": manifest.transcripts.len(),
        "scratchpad_files": manifest.scratchpad.len(),
        "synced": manifest.synced,
    })
}

/// Every save of a session, oldest first.
pub async fn list(backend: &dyn Backend, session_id: &str) -> Result<Vec<Manifest>, String> {
    let volume = crate::workspace::scratch_volume_name(session_id);
    let Some(copy) = NodeCopy::existing(backend, &volume).await? else {
        return Ok(Vec::new());
    };
    let dir = copy.root().join(DIR);
    let mut saves: Vec<Manifest> = save_dirs(&dir)
        .into_iter()
        .filter_map(|(_, path)| {
            serde_json::from_slice(&std::fs::read(path.join(MANIFEST)).ok()?).ok()
        })
        .collect();
    saves.sort_by_key(|m| m.index);
    Ok(saves)
}

/// One save, read back, with its files' contents when `with_contents`:
/// `None` when the session has no save of that index.
pub async fn read(
    backend: &dyn Backend,
    session_id: &str,
    index: u32,
    with_contents: bool,
) -> Result<Option<(Saved, Vec<Value>)>, String> {
    let volume = crate::workspace::scratch_volume_name(session_id);
    let Some(copy) = NodeCopy::existing(backend, &volume).await? else {
        return Ok(None);
    };
    let Some((_, dir)) = save_dirs(&copy.root().join(DIR))
        .into_iter()
        .find(|(i, _)| *i == index)
    else {
        return Ok(None);
    };
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(dir.join(MANIFEST)).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let conversation = std::fs::read_to_string(dir.join(CONVERSATION)).unwrap_or_default();
    let mut files = Vec::new();
    list_files(&dir, &dir.join(TRANSCRIPT), &mut files);
    list_files(&dir, &dir.join(SCRATCHPAD), &mut files);
    let mut contents = Vec::new();
    if with_contents {
        for file in &files {
            let bytes = std::fs::read(dir.join(&file.path)).map_err(|e| e.to_string())?;
            contents.push(match String::from_utf8(bytes) {
                Ok(text) => json!({ "path": file.path, "bytes": file.bytes, "text": text }),
                Err(raw) => json!({
                    "path": file.path,
                    "bytes": file.bytes,
                    "base64": base64::engine::general_purpose::STANDARD.encode(raw.into_bytes()),
                }),
            });
        }
    }
    Ok(Some((
        Saved {
            manifest,
            conversation,
            files,
        },
        contents,
    )))
}

/// A save as one JSON document, readable with nothing running: what it is,
/// its manifest, the conversation, and every file it holds (text as text,
/// anything else as base64).
pub fn export(session_id: &str, saved: &Saved, contents: Vec<Value>) -> Value {
    json!({
        "kind": EXPORT_KIND,
        "version": EXPORT_VERSION,
        "session_id": session_id,
        "manifest": saved.manifest,
        "conversation": saved.conversation,
        "files": contents,
    })
}

/// The node's copy of a volume, where it is read and written: the files
/// store's own tree where the node keeps the volume, or else a copy exported
/// out of the runtime and imported back whole.
enum NodeCopy {
    Kept(PathBuf),
    Exported(tempfile::TempDir),
}

impl NodeCopy {
    async fn open(backend: &dyn Backend, volume: &str) -> Result<Self, String> {
        if let Some(tree) = backend
            .files_for(volume)
            .and_then(|store| store.tree(volume))
        {
            return Ok(NodeCopy::Kept(tree));
        }
        let tmp =
            tempfile::tempdir_in(crate::config::Config::state_dir()).map_err(|e| e.to_string())?;
        let out = tmp.path().join("volume");
        if backend.volume_exists(volume).await {
            backend
                .export_volume(volume, &out)
                .await
                .map_err(|e| e.to_string())?;
        }
        std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        Ok(NodeCopy::Exported(tmp))
    }

    /// The copy of a volume that exists, for reading.
    async fn existing(backend: &dyn Backend, volume: &str) -> Result<Option<Self>, String> {
        if let Some(tree) = backend
            .files_for(volume)
            .and_then(|store| store.tree(volume))
        {
            return Ok(Some(NodeCopy::Kept(tree)));
        }
        if !backend.volume_exists(volume).await {
            return Ok(None);
        }
        Self::open(backend, volume).await.map(Some)
    }

    fn root(&self) -> PathBuf {
        match self {
            NodeCopy::Kept(tree) => tree.clone(),
            NodeCopy::Exported(tmp) => tmp.path().join("volume"),
        }
    }

    /// Put the staged save at `saved/<name>`. Into a kept tree beside it
    /// first and then renamed, so a save is either whole or not there.
    async fn add(
        &self,
        backend: &dyn Backend,
        volume: &str,
        name: &str,
        staged: &Path,
    ) -> Result<(), String> {
        let dir = self.root().join(DIR);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let partial = dir.join(format!(".partial-{}", uuid::Uuid::now_v7().simple()));
        if let Err(error) = crate::workspace::copy_tree(staged, &partial, false) {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(error.to_string());
        }
        std::fs::rename(&partial, dir.join(name)).map_err(|e| {
            let _ = std::fs::remove_dir_all(&partial);
            e.to_string()
        })?;
        if let NodeCopy::Exported(_) = self {
            backend
                .import_volume(volume, &self.root())
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// The saves under `dir`, by index.
fn save_dirs(dir: &Path) -> Vec<(u32, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let index = name.split('-').next()?.parse::<u32>().ok()?;
            Some((index, e.path()))
        })
        .collect();
    found.sort_by_key(|(index, _)| *index);
    found
}

fn next_index(dir: &Path) -> u32 {
    save_dirs(dir).last().map_or(1, |(index, _)| index + 1)
}

/// Copy a file or a directory, never following a link out of it.
fn copy_entry(source: &Path, dest: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if meta.is_dir() {
        crate::workspace::copy_tree(source, dest, false)
            .map_err(|e| std::io::Error::other(e.to_string()))
    } else if meta.is_file() {
        std::fs::copy(source, dest).map(|_| ())
    } else {
        Ok(())
    }
}

/// Every regular file under `under`, relative to `base`, in path order.
fn list_files(base: &Path, under: &Path, out: &mut Vec<SavedFile>) {
    let Ok(entries) = std::fs::read_dir(under) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            list_files(base, &entry.path(), out);
        } else if meta.is_file() {
            if let Ok(relative) = entry.path().strip_prefix(base) {
                out.push(SavedFile {
                    path: relative.to_string_lossy().into_owned(),
                    bytes: meta.len(),
                });
            }
        }
    }
}

fn session_events(store: &Store, session_id: &str) -> Result<Vec<EventRow>, String> {
    let mut events = Vec::new();
    let mut after = 0;
    loop {
        let page = store
            .events_after(session_id, after, EVENTS_PAGE)
            .map_err(|e| e.to_string())?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        let full = page.len() as i64 == EVENTS_PAGE;
        events.extend(page);
        if !full || events.len() as i64 >= MAX_EVENTS {
            break;
        }
    }
    Ok(events)
}

/// The conversation the log holds, as Markdown: what was said and done, in
/// order, every word of it the log kept.
pub fn render_conversation(session_id: &str, events: &[EventRow], moment: &Moment) -> String {
    let mut out = format!("# Conversation of session {session_id}\n\n");
    let before = match moment.before {
        BEFORE_RESTART => "its harness was restarted".to_string(),
        _ => match moment.trigger.as_deref() {
            Some(trigger) => format!("a compaction ({trigger})"),
            None => "a compaction".to_string(),
        },
    };
    out.push_str(&format!(
        "Saved by the node before {before}, from its log through event {}. Tool output the log \
         cut short is whole in `{TRANSCRIPT}/`, the harness's own record.\n",
        events.last().map_or(0, |e| e.seq),
    ));
    for e in events {
        let p = &e.payload;
        let at = utc(e.at_ms);
        let text = |key: &str| p[key].as_str().unwrap_or("").to_string();
        match e.kind.as_str() {
            "orientation" => {
                out.push_str(&format!("\n## Orientation · {at}\n\n{}\n", text("text")));
            }
            "user_prompt" => {
                let who = match p["origin"].as_str() {
                    Some("kickoff") => "The node's kickoff",
                    Some("node") => "The node",
                    _ => "Operator",
                };
                out.push_str(&format!("\n## {who} · {at}\n\n{}\n", text("text")));
                for queued in p["queued"].as_array().into_iter().flatten() {
                    let queued = queued["text"].as_str().or(queued.as_str()).unwrap_or("");
                    out.push_str(&format!("\n{queued}\n"));
                }
            }
            "prompt_queued" => {
                out.push_str(&format!(
                    "\n## Operator, while the turn ran · {at}\n\n{}\n",
                    text("text")
                ));
            }
            "message" => out.push_str(&format!("\n## Agent · {at}\n\n{}\n", text("text"))),
            "thought" => out.push_str(&format!(
                "\n### Thinking · {at}\n\n{}\n",
                quote(&text("text"))
            )),
            "tool_call" => {
                out.push_str(&format!("\n### Tool call: {} · {at}\n", text("title")));
                if !p["raw_input"].is_null() {
                    let input = serde_json::to_string_pretty(&p["raw_input"]).unwrap_or_default();
                    out.push_str(&format!("\n{}\n", fenced(&input, "json")));
                }
            }
            "tool_result" => {
                let status = p["status"].as_str().unwrap_or("done");
                let cut = if p["truncated"] == true {
                    ", cut short in the log"
                } else {
                    ""
                };
                out.push_str(&format!("\n### Tool result ({status}{cut}) · {at}\n"));
                let output = match &p["output"] {
                    Value::String(s) => unwrap_text(s),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                if !output.is_empty() {
                    out.push_str(&format!("\n{}\n", fenced(&output, "")));
                }
            }
            "plan" => {
                let plan = serde_json::to_string_pretty(p).unwrap_or_default();
                out.push_str(&format!("\n### Plan · {at}\n\n{}\n", fenced(&plan, "json")));
            }
            "context_compacted" => {
                out.push_str(&format!("\n## Compacted · {at}\n"));
                if let Some(summary) = p["summary"].as_str() {
                    out.push_str(&format!(
                        "\nThe summary the model worked from afterwards:\n\n{}\n",
                        quote(summary)
                    ));
                }
            }
            "context_saved" => out.push_str(&format!(
                "\n## Saved before {} · {at}\n\nSave {} of this session.\n",
                p["before"].as_str().unwrap_or("a compaction"),
                p["index"]
            )),
            "harness_restarted" => out.push_str(&format!(
                "\n## Harness restarted ({}) · {at}\n",
                p["outcome"].as_str().unwrap_or("")
            )),
            "error" => {
                if let Some(error) = p["error"].as_str() {
                    out.push_str(&format!("\n## Error · {at}\n\n{error}\n"));
                }
            }
            _ => {}
        }
    }
    out
}

/// A tool's output as the log keeps it: the JSON of what the harness sent.
/// A plain string, or `{"text": …}`/`{"output": …}`, reads as its text.
fn unwrap_text(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::String(s)) => s,
        Ok(v) => ["text", "output", "stdout", "content"]
            .iter()
            .find_map(|key| v[*key].as_str().map(str::to_string))
            .unwrap_or_else(|| raw.to_string()),
        Err(_) => raw.to_string(),
    }
}

/// `text` in a code fence longer than any run of backticks inside it.
fn fenced(text: &str, lang: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{lang}\n{}\n{fence}", text.trim_end_matches('\n'))
}

fn quote(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Milliseconds since the epoch as `YYYY-MM-DD HH:MM:SS UTC`.
fn utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(seq: i64, kind: &str, payload: Value) -> EventRow {
        EventRow {
            seq,
            node_id: "n1".into(),
            session_id: "s1".into(),
            work_item_id: None,
            kind: kind.into(),
            ref_id: None,
            payload,
            at_ms: 1_764_000_000_000 + seq * 1000,
            mono_ms: seq,
        }
    }

    fn moment(before: &'static str, trigger: Option<&str>) -> Moment {
        Moment {
            before,
            trigger: trigger.map(str::to_string),
            harness_session_id: "h1".into(),
            container: "c1".into(),
        }
    }

    /// The conversation is every word the log kept, in order: prompts, the
    /// agent's messages and thinking, each tool call's input and result.
    #[test]
    fn the_conversation_is_rendered_whole_from_the_log() {
        let events = vec![
            row(1, "orientation", json!({ "text": "You are in /work." })),
            row(2, "user_prompt", json!({ "text": "Add the ledger table." })),
            row(
                3,
                "thought",
                json!({ "text": "A table, then a view.\n\nMaybe." }),
            ),
            row(
                4,
                "message",
                json!({ "text": "Writing the migration now." }),
            ),
            row(
                5,
                "tool_call",
                json!({ "title": "Write migration", "raw_input": { "path": "m.sql" } }),
            ),
            row(
                6,
                "tool_result",
                json!({ "status": "completed", "output": "\"has ``` inside\"", "truncated": true }),
            ),
            row(7, "usage", json!({ "used": 10 })),
            row(
                8,
                "context_compacted",
                json!({ "trigger": "threshold", "summary": "The table is written." }),
            ),
        ];
        let text = render_conversation("s1", &events, &moment(BEFORE_COMPACTION, Some("manual")));
        let order = [
            "# Conversation of session s1",
            "before a compaction (manual), from its log through event 8",
            "## Orientation · 2025-11-24 ",
            "You are in /work.",
            "## Operator · ",
            "Add the ledger table.",
            "### Thinking · ",
            "> A table, then a view.\n>\n> Maybe.",
            "## Agent · ",
            "Writing the migration now.",
            "### Tool call: Write migration",
            "\"path\": \"m.sql\"",
            "### Tool result (completed, cut short in the log)",
            "````\nhas ``` inside\n````",
            "## Compacted",
            "> The table is written.",
        ];
        let mut from = 0;
        for want in order {
            let at = text[from..]
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} not after byte {from} in:\n{text}"));
            from += at + want.len();
        }
        assert!(
            !text.contains("\"used\""),
            "usage is not conversation: {text}"
        );
    }

    #[test]
    fn a_restart_save_says_so() {
        let text = render_conversation("s1", &[], &moment(BEFORE_RESTART, None));
        assert!(text.contains("before its harness was restarted"), "{text}");
    }

    #[test]
    fn times_are_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_709_251_199_000), "2024-02-29 23:59:59 UTC");
    }

    #[test]
    fn only_a_plain_id_is_looked_for() {
        assert!(plain_id("8f1c-ab_2"));
        assert!(!plain_id(""));
        assert!(!plain_id("../other"));
        assert!(!plain_id("a/b"));
    }

    /// Saves are numbered on from the last one there, whatever they were
    /// saved before; a half-written one is not counted.
    #[test]
    fn saves_are_numbered_on_from_the_last() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(next_index(dir.path()), 1);
        std::fs::create_dir_all(dir.path().join("0001-compaction")).unwrap();
        std::fs::create_dir_all(dir.path().join("0003-restart")).unwrap();
        std::fs::create_dir_all(dir.path().join(".partial-x")).unwrap();
        assert_eq!(next_index(dir.path()), 4);
    }
}
