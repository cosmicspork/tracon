//! A session's write-ahead record, so a replaced process can carry it on.
//!
//! One JSON object per line, appended and flushed before the step it records
//! takes effect: the prompt before the model sees it, the model's response
//! before any of its tool calls runs, a call's start once it is allowed and
//! before it runs, and its result once it has one. Reading it back rebuilds the
//! conversation and says, for a turn the process died in, which calls never
//! started and which started with no result: what happened to those is not
//! known, and the model is told so rather than shown a guess.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::compact;
use crate::conversation::{Block, Message, Role};

/// What the model is told about a call the process died before starting.
pub const NOT_RUN: &str = "not run: the harness restarted before this call ran. Call it again \
                           if it is still needed.";
/// What the model is told about a call the process died during.
pub const OUTCOME_UNKNOWN: &str = "outcome unknown: the harness stopped while this call ran, so \
                                   whether it finished, and what it changed, is not known. Check \
                                   its effects before running it again.";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Session {
        id: String,
        cwd: String,
    },
    /// A prompt, or a response with the tool calls it makes.
    Message {
        role: Role,
        blocks: Vec<Block>,
    },
    /// The call was shown to the node and asked about.
    ToolAsked {
        id: String,
    },
    /// The call was allowed and is about to run.
    ToolStarted {
        id: String,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    /// Old tool results replaced by a note of their size.
    Pruned {
        results: Vec<compact::Pruned>,
    },
    /// Everything but the last `keep` messages replaced by `summary`.
    Compacted {
        summary: String,
        keep: usize,
    },
}

pub struct Journal {
    path: PathBuf,
    file: Mutex<File>,
}

impl Journal {
    /// The journal of session `id` under `dir`, created if new and appended to
    /// if not.
    pub fn open(dir: &Path, id: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path(dir, id);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.jsonl"))
    }

    /// Appends a record and flushes it, so it outlives this process. A record
    /// that cannot be written is not worth stopping the turn for: the session
    /// still works, it just could not be carried on from here.
    pub fn write(&self, record: &Record) {
        let Ok(line) = serde_json::to_string(record) else {
            return;
        };
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if writeln!(file, "{line}")
            .and_then(|()| file.flush())
            .is_err()
        {
            tracing::warn!(path = %self.path.display(), "journal record not written");
        }
    }
}

/// A call the process died around, and what it is now answered with.
#[derive(Debug, Clone, PartialEq)]
pub struct Interrupted {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
    /// Whether the node was shown it before the process died.
    pub asked: bool,
    pub content: &'static str,
}

#[derive(Debug, Default)]
pub struct Restored {
    pub cwd: Option<String>,
    pub messages: Vec<Message>,
    /// Calls answered here rather than from the record: their results are not
    /// in the journal yet, and the caller writes them.
    pub interrupted: Vec<Interrupted>,
}

/// Rebuilds a session from its journal. Every tool call in it comes back with
/// a result, recorded or decided here, so the conversation is well formed for
/// the next prompt.
pub fn restore(path: &Path) -> std::io::Result<Restored> {
    let file = File::open(path)?;
    let mut out = Restored::default();
    let mut open: Option<OpenCalls> = None;
    for line in BufReader::new(file).lines() {
        let line = line?;
        // A line cut short by the process dying mid-write is the last one, and
        // records nothing that had taken effect.
        let Ok(record) = serde_json::from_str::<Record>(&line) else {
            continue;
        };
        match record {
            Record::Session { cwd, .. } => out.cwd = Some(cwd),
            Record::Message { role, blocks } => {
                if let Some(calls) = open.take() {
                    calls.close(&mut out);
                }
                let calls: Vec<_> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { id, name, input } => {
                            Some((id.clone(), name.clone(), input.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                out.messages.push(Message { role, blocks });
                if role == Role::Assistant && !calls.is_empty() {
                    open = Some(OpenCalls {
                        calls,
                        ..Default::default()
                    });
                }
            }
            Record::ToolAsked { id } => {
                if let Some(o) = open.as_mut() {
                    o.asked.insert(id);
                }
            }
            Record::ToolStarted { id } => {
                if let Some(o) = open.as_mut() {
                    o.started.insert(id);
                }
            }
            Record::ToolResult {
                id,
                content,
                is_error,
            } => {
                if let Some(o) = open.as_mut() {
                    o.results.insert(id, (content, is_error));
                }
            }
            // Compaction runs between requests, with every call answered.
            Record::Pruned { results } => {
                if let Some(calls) = open.take() {
                    calls.close(&mut out);
                }
                compact::apply_prune(&mut out.messages, &results);
            }
            Record::Compacted { summary, keep } => {
                if let Some(calls) = open.take() {
                    calls.close(&mut out);
                }
                compact::apply_summary(&mut out.messages, &summary, keep);
            }
        }
    }
    if let Some(calls) = open {
        calls.close(&mut out);
    }
    Ok(out)
}

/// The tool calls of the last response, and what the journal says of each.
#[derive(Default)]
struct OpenCalls {
    calls: Vec<(String, String, serde_json::Value)>,
    asked: HashSet<String>,
    started: HashSet<String>,
    results: HashMap<String, (String, bool)>,
}

impl OpenCalls {
    fn close(mut self, out: &mut Restored) {
        let mut blocks = Vec::new();
        for (id, name, input) in self.calls {
            let (content, is_error) = match self.results.remove(&id) {
                Some(r) => r,
                None => {
                    let content = if self.started.contains(&id) {
                        OUTCOME_UNKNOWN
                    } else {
                        NOT_RUN
                    };
                    out.interrupted.push(Interrupted {
                        id: id.clone(),
                        name,
                        input,
                        asked: self.asked.contains(&id),
                        content,
                    });
                    (content.to_string(), true)
                }
            };
            blocks.push(Block::ToolResult {
                id,
                content,
                is_error,
            });
        }
        out.messages.push(Message {
            role: Role::User,
            blocks,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str) -> Block {
        Block::ToolUse {
            id: id.into(),
            name: "shell".into(),
            input: json!({"command": "true"}),
        }
    }

    #[test]
    fn a_turn_cut_short_is_closed_call_by_call() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path(), "s1").unwrap();
        j.write(&Record::Session {
            id: "s1".into(),
            cwd: "/work".into(),
        });
        j.write(&Record::Message {
            role: Role::User,
            blocks: vec![Block::Text("go".into())],
        });
        j.write(&Record::Message {
            role: Role::Assistant,
            blocks: vec![call("done"), call("running"), call("asked"), call("never")],
        });
        for id in ["done", "running", "asked"] {
            j.write(&Record::ToolAsked { id: id.into() });
        }
        j.write(&Record::ToolStarted { id: "done".into() });
        j.write(&Record::ToolResult {
            id: "done".into(),
            content: "ok".into(),
            is_error: false,
        });
        j.write(&Record::ToolStarted {
            id: "running".into(),
        });
        drop(j);
        // A line the process died writing.
        let path = Journal::path(dir.path(), "s1");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "{{\"kind\":\"tool_res").unwrap();

        let r = restore(&path).unwrap();
        assert_eq!(r.cwd.as_deref(), Some("/work"));
        assert_eq!(r.messages.len(), 3);
        let Some(Message { role, blocks }) = r.messages.last() else {
            unreachable!()
        };
        assert_eq!(*role, Role::User);
        let results: Vec<_> = blocks
            .iter()
            .map(|b| match b {
                Block::ToolResult { id, content, .. } => (id.as_str(), content.as_str()),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            results,
            [
                ("done", "ok"),
                ("running", OUTCOME_UNKNOWN),
                ("asked", NOT_RUN),
                ("never", NOT_RUN)
            ]
        );
        let asked: Vec<_> = r
            .interrupted
            .iter()
            .map(|i| (i.id.as_str(), i.asked))
            .collect();
        assert_eq!(
            asked,
            [("running", true), ("asked", true), ("never", false)]
        );
    }

    #[test]
    fn a_compacted_session_comes_back_compacted() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path(), "s3").unwrap();
        let message = |role, blocks| Record::Message { role, blocks };
        j.write(&message(Role::User, vec![Block::Text("go".into())]));
        for id in ["a", "b"] {
            j.write(&message(Role::Assistant, vec![call(id)]));
            j.write(&Record::ToolResult {
                id: id.into(),
                content: format!("output of {id}"),
                is_error: false,
            });
        }
        j.write(&Record::Pruned {
            results: vec![compact::Pruned {
                id: "a".into(),
                content: "[Output truncated - 9 tokens]".into(),
            }],
        });
        j.write(&message(Role::Assistant, vec![call("c")]));
        j.write(&Record::ToolResult {
            id: "c".into(),
            content: "output of c".into(),
            is_error: false,
        });
        j.write(&Record::Compacted {
            summary: "[harness] summary".into(),
            keep: 2,
        });
        j.write(&message(Role::Assistant, vec![Block::Text("done".into())]));

        let r = restore(&Journal::path(dir.path(), "s3")).unwrap();
        assert!(r.interrupted.is_empty());
        let got: Vec<_> = r.messages.iter().map(|m| (m.role, &m.blocks)).collect();
        let (call_c, result_c) = (
            vec![call("c")],
            vec![Block::ToolResult {
                id: "c".into(),
                content: "output of c".into(),
                is_error: false,
            }],
        );
        assert_eq!(
            got,
            [
                (Role::User, &vec![Block::Text("[harness] summary".into())]),
                (Role::Assistant, &call_c),
                (Role::User, &result_c),
                (Role::Assistant, &vec![Block::Text("done".into())]),
            ]
        );

        // Pruning before the summary is applied where it was made.
        let j = Journal::open(dir.path(), "s4").unwrap();
        j.write(&message(Role::User, vec![Block::Text("go".into())]));
        j.write(&message(Role::Assistant, vec![call("a")]));
        j.write(&Record::ToolResult {
            id: "a".into(),
            content: "long".into(),
            is_error: true,
        });
        j.write(&Record::Pruned {
            results: vec![compact::Pruned {
                id: "a".into(),
                content: "[Output truncated - 1 tokens]".into(),
            }],
        });
        let r = restore(&Journal::path(dir.path(), "s4")).unwrap();
        assert_eq!(
            r.messages[2].blocks,
            [Block::ToolResult {
                id: "a".into(),
                content: "[Output truncated - 1 tokens]".into(),
                is_error: true,
            }]
        );
    }

    #[test]
    fn a_finished_turn_comes_back_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path(), "s2").unwrap();
        let messages = [
            (Role::User, vec![Block::Text("hi".into())]),
            (Role::Assistant, vec![Block::Text("hello".into())]),
        ];
        for (role, blocks) in &messages {
            j.write(&Record::Message {
                role: *role,
                blocks: blocks.clone(),
            });
        }
        let r = restore(&Journal::path(dir.path(), "s2")).unwrap();
        assert!(r.interrupted.is_empty());
        let got: Vec<_> = r.messages.into_iter().map(|m| (m.role, m.blocks)).collect();
        assert_eq!(got, messages);
    }
}
