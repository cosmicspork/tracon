//! Keeping a long session inside the model's context window.
//!
//! Two steps, cheapest first. Old tool outputs are pruned: replaced in place by
//! a note of their size, sparing the newest. If the conversation is still too
//! big, everything before its last stretch is summarised by the same model and
//! replaced by that summary. Nothing is ever truncated silently: a request the
//! upstream refuses as too long is compacted and asked again.
//!
//! Sizes are estimates. The model counts each request it answers; what has
//! been added since is estimated from its bytes, at the rate that request came
//! to.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation::{Block, Conversation, Measured, Message, Role};

/// The most recent stretch of the conversation kept as it is.
pub const KEEP_RAW: u64 = 20_000;
/// The newest tool output never pruned.
pub const PROTECT_OUTPUT: u64 = 40_000;
/// Pruning that saves less than this is not worth losing the outputs for.
pub const MIN_PRUNE_SAVING: u64 = 20_000;

/// The sizes compaction works to, in tokens: the constants above, scaled down
/// for a window too small to hold them below its threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub keep_raw: u64,
    pub protect_output: u64,
    pub min_prune_saving: u64,
}

impl Limits {
    pub fn for_window(window: Option<u64>) -> Self {
        let Some(t) = window.map(threshold) else {
            return Self {
                keep_raw: KEEP_RAW,
                protect_output: PROTECT_OUTPUT,
                min_prune_saving: MIN_PRUNE_SAVING,
            };
        };
        Self {
            keep_raw: KEEP_RAW.min(t * 2 / 5),
            protect_output: PROTECT_OUTPUT.min(t * 4 / 5),
            min_prune_saving: MIN_PRUNE_SAVING.min(t * 2 / 5),
        }
    }
}
/// A result this small costs less than the note that would replace it.
pub const MIN_PRUNE_RESULT: u64 = 50;
/// The summary's own length, at most.
pub const SUMMARY_MAX_TOKENS: u32 = 4096;
/// Bytes of one tool result the summariser is shown, head and tail.
const SUMMARY_RESULT_BYTES: usize = 8000;
/// Before any request has been measured.
const DEFAULT_BYTES_PER_TOKEN: f64 = 4.0;

/// What starts a compacted conversation, so a later compaction knows an
/// earlier summary when it sees one.
pub const SUMMARY_HEADER: &str = "[harness] The earlier part of this session was summarised to \
                                  fit the model's context window.";

pub const SUMMARISER: &str = "\
You write the notes a coding agent works from once its context is cleared. You are given the \
earlier part of its session, up to the line [end of transcript]. The work carries on after that \
point, but you do not see what follows and must not guess at it. The agent will also be given the \
operator's messages word for word and a record of every tool call in your transcript, so do not \
copy those out.

Write, in plain prose or short lists:
- what has been done and what it showed: files created or changed and how, test results, the \
exact errors that still matter;
- what was learned that is still needed: paths, names, line numbers, conventions;
- decisions taken, and why;
- what was being done when the transcript ends, and what the operator asked for that is not done \
yet.

Report only what the transcript shows happening. Anything the operator asked for that the \
transcript does not show being done has not been done: say that it remains, and never describe \
doing it or what it would show. Where an output was cut short, say what was shown and that the \
rest was not. Where something is uncertain, say so and tell the agent to check it. Never describe \
a file from another file's output. No more than about 1000 words.";

/// The size, in tokens, past which a conversation is compacted before the
/// next request: the window less room for the answer (15 %, at least 16k),
/// but never less than half the window.
pub fn threshold(window: u64) -> u64 {
    let reserve = (window * 15 / 100).max(16_384);
    window.saturating_sub(reserve).max(window / 2)
}

fn block_bytes(b: &Block) -> usize {
    match b {
        Block::Text(t) => t.len(),
        Block::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
        Block::ToolResult { content, .. } => content.len(),
        Block::Opaque(v) => v.to_string().len(),
    }
}

/// A message's bytes, with a little for the template's framing of it.
pub fn bytes(m: &Message) -> usize {
    8 + m.blocks.iter().map(block_bytes).sum::<usize>()
}

/// The bytes of a request holding `conv`, besides the tools' `overhead`.
pub fn request_bytes(conv: &Conversation, overhead: usize) -> usize {
    overhead + conv.system.len() + conv.messages.iter().map(bytes).sum::<usize>()
}

fn rate(conv: &Conversation) -> f64 {
    conv.measured
        .map_or(DEFAULT_BYTES_PER_TOKEN, |m| m.bytes_per_token)
}

fn tokens(bytes: usize, rate: f64) -> u64 {
    (bytes as f64 / rate).ceil() as u64
}

/// How many tokens the next request will be, its tools' bytes `overhead`
/// included: the last measurement plus what was added after it.
pub fn estimate(conv: &Conversation, overhead: usize) -> u64 {
    match conv.measured {
        Some(m) if m.messages <= conv.messages.len() => {
            let added: usize = conv.messages[m.messages..].iter().map(bytes).sum();
            m.tokens + tokens(added, m.bytes_per_token)
        }
        _ => tokens(request_bytes(conv, overhead), rate(conv)),
    }
}

/// The messages' own tokens, without the system prompt and tools.
pub fn messages_tokens(conv: &Conversation) -> u64 {
    tokens(conv.messages.iter().map(bytes).sum(), rate(conv))
}

/// Records what a request was measured at. `sent` is the request's bytes, as
/// `request_bytes` counts them; `prompt` and `answer` the model's counts.
pub fn measure(conv: &mut Conversation, sent: usize, prompt: u64, answer: u64) {
    let bytes_per_token = if prompt > 0 {
        (sent as f64 / prompt as f64).clamp(1.0, 16.0)
    } else {
        rate(conv)
    };
    conv.measured = Some(Measured {
        messages: conv.messages.len(),
        tokens: prompt + answer,
        bytes_per_token,
    });
}

/// After the messages changed under it, the measurement is the estimate.
fn remeasure(conv: &mut Conversation, overhead: usize) {
    let bytes_per_token = rate(conv);
    conv.measured = Some(Measured {
        messages: conv.messages.len(),
        tokens: tokens(request_bytes(conv, overhead), bytes_per_token),
        bytes_per_token,
    });
}

/// A tool result replaced by a note of its size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pruned {
    pub id: String,
    pub content: String,
}

/// The calls whose output nothing still depends on: a read or edit of a
/// file that a later edit or write changed, whose anchors are stale, and a
/// read that a later read of the same lines showed again. Mapped to the path.
fn superseded(conv: &Conversation) -> HashMap<String, String> {
    let failed: HashSet<&str> = conv
        .messages
        .iter()
        .flat_map(|m| &m.blocks)
        .filter_map(|b| match b {
            Block::ToolResult {
                id, is_error: true, ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    // In order: (id, tool, path, the lines a read showed).
    let calls: Vec<(&str, &str, String, (Value, Value))> = conv
        .messages
        .iter()
        .flat_map(|m| &m.blocks)
        .filter_map(|b| match b {
            Block::ToolUse { id, name, input }
                if matches!(name.as_str(), "read" | "edit" | "write")
                    && !failed.contains(id.as_str()) =>
            {
                let path = input["path"].as_str()?;
                let path = path.trim_start_matches("./").to_string();
                let lines = (input["offset"].clone(), input["limit"].clone());
                Some((id.as_str(), name.as_str(), path, lines))
            }
            _ => None,
        })
        .collect();
    let mut out = HashMap::new();
    for (i, (id, tool, path, lines)) in calls.iter().enumerate() {
        if *tool == "write" {
            continue;
        }
        let later = calls[i + 1..].iter().any(|(_, t, p, l)| {
            p == path
                && (matches!(*t, "edit" | "write")
                    || (*tool == "read" && *t == "read" && l == lines))
        });
        if later {
            out.insert(id.to_string(), path.clone());
        }
    }
    out
}

/// Tool outputs to replace. Superseded ones always (see [`superseded`]);
/// then the results past the newest `protect_output` tokens of output, oldest
/// last, if together they save at least `min_prune_saving`. Empty when
/// pruning is not worth it.
pub fn prune_plan(conv: &Conversation, limits: Limits) -> Vec<Pruned> {
    let rate = rate(conv);
    let stale = superseded(conv);
    let mut newer = 0u64;
    let mut saved = 0u64;
    let mut plan = Vec::new();
    let mut elided = Vec::new();
    for m in conv.messages.iter().rev() {
        for b in m.blocks.iter().rev() {
            let Block::ToolResult { id, content, .. } = b else {
                continue;
            };
            let size = tokens(content.len(), rate);
            let protected = newer < limits.protect_output;
            newer += size;
            if size < MIN_PRUNE_RESULT || content.starts_with('[') && content.contains(" tokens]") {
                continue;
            }
            if let Some(path) = stale.get(id) {
                elided.push(Pruned {
                    id: id.clone(),
                    content: format!(
                        "[Superseded by a later read or change of {path} - {size} tokens]"
                    ),
                });
                continue;
            }
            if protected {
                continue;
            }
            let note = format!("[Output truncated - {size} tokens]");
            saved += size - tokens(note.len(), rate).min(size);
            plan.push(Pruned {
                id: id.clone(),
                content: note,
            });
        }
    }
    if saved < limits.min_prune_saving {
        plan.clear();
    }
    elided.extend(plan);
    elided
}

/// Replaces the named results in place; an error stays an error.
pub fn apply_prune(messages: &mut [Message], pruned: &[Pruned]) {
    for m in messages {
        for b in &mut m.blocks {
            if let Block::ToolResult { id, content, .. } = b {
                if let Some(p) = pruned.iter().find(|p| &p.id == id) {
                    content.clone_from(&p.content);
                }
            }
        }
    }
}

pub fn prune(conv: &mut Conversation, pruned: &[Pruned], overhead: usize) {
    apply_prune(&mut conv.messages, pruned);
    remeasure(conv, overhead);
}

/// A message of tool results answers the call just before it, so the two are
/// never parted.
fn is_results(m: &Message) -> bool {
    m.role == Role::User
        && m.blocks
            .iter()
            .any(|b| matches!(b, Block::ToolResult { .. }))
}

/// Where to cut: messages before the index are summarised, from it on kept.
/// The cut keeps at most `keep` tokens, may keep nothing, and never falls
/// just before tool results. `None` when there is nothing worth
/// summarising: no work done yet, or only an earlier summary.
pub fn cut(conv: &Conversation, keep: u64) -> Option<usize> {
    let rate = rate(conv);
    let messages = &conv.messages;
    let mut kept = 0u64;
    let mut cut = messages.len();
    // Walking back, the earliest boundary whose suffix still fits.
    for i in (1..=messages.len()).rev() {
        if i < messages.len() {
            kept += tokens(bytes(&messages[i]), rate);
            if kept > keep {
                break;
            }
        }
        if i == messages.len() || !is_results(&messages[i]) {
            cut = i;
        }
    }
    let worked = messages[..cut]
        .iter()
        .any(|m| m.role == Role::Assistant && !m.blocks.is_empty());
    worked.then_some(cut)
}

const CALLS: &str = "\n\nThe tool calls made before this point, from the record, oldest first:\n";
const OPERATOR_MESSAGES: &str = "\n\nThe operator's messages, word for word, oldest first:";
const CARRY_ON: &str = "\n\nCarry on with the work from here.";
/// The operator's messages a summary restates, at most: the first and the
/// newest are kept, those between dropped first.
const MAX_OPERATOR_BYTES: usize = 16 * 1024;

/// The calls a summary lists, at most: the newest.
const MAX_CALLS: usize = 60;
/// How much of a call's input, or of an error it met, the record shows.
const CALL_DETAIL_BYTES: usize = 160;

fn shorten(text: &str) -> String {
    if text.len() <= CALL_DETAIL_BYTES {
        return text.to_string();
    }
    format!("{}…", &text[..floor(text, CALL_DETAIL_BYTES)])
}

/// One line per tool call among `messages`, oldest first, with how it ended,
/// those an earlier summary listed included. Taken from the conversation,
/// not the summariser, so it cannot be wrong about what ran.
fn call_record(messages: &[Message]) -> Vec<String> {
    let mut ended = std::collections::HashMap::new();
    for b in messages.iter().flat_map(|m| &m.blocks) {
        if let Block::ToolResult {
            id,
            content,
            is_error,
        } = b
        {
            ended.insert(id.as_str(), (content.as_str(), *is_error));
        }
    }
    let mut out = Vec::new();
    for b in messages.iter().flat_map(|m| &m.blocks) {
        match b {
            Block::Text(t) if t.starts_with(SUMMARY_HEADER) => {
                if let Some((_, listed)) = t.split_once(CALLS) {
                    let listed = [OPERATOR_MESSAGES, CARRY_ON]
                        .iter()
                        .find_map(|end| listed.split_once(end).map(|(l, _)| l))
                        .unwrap_or(listed);
                    out.extend(
                        listed
                            .lines()
                            .filter_map(|l| l.strip_prefix("- "))
                            .map(str::to_string),
                    );
                }
            }
            Block::ToolUse { id, name, input } => {
                let how = match ended.get(id.as_str()) {
                    Some((_, false)) => "ok".to_string(),
                    Some((content, true)) => {
                        format!("error: {}", shorten(content.lines().next().unwrap_or("")))
                    }
                    None => "no result".to_string(),
                };
                out.push(format!("{name} {} → {how}", shorten(&input.to_string())));
            }
            _ => {}
        }
    }
    // A count carried from an earlier summary adds to what is dropped here.
    let mut dropped: usize = 0;
    out.retain(|l| {
        let carried = l
            .strip_prefix('(')
            .and_then(|l| l.strip_suffix(" earlier calls not listed)"))
            .and_then(|n| n.parse::<usize>().ok());
        dropped += carried.unwrap_or(0);
        carried.is_none()
    });
    if out.len() > MAX_CALLS {
        dropped += out.len() - MAX_CALLS;
        out.drain(..out.len() - MAX_CALLS);
    }
    if dropped > 0 {
        out.insert(0, format!("({dropped} earlier calls not listed)"));
    }
    out
}

fn operator_heading(n: usize) -> String {
    format!("\n\n--- operator message {n} ---\n")
}

/// Every message the operator wrote among `messages`, in order, as given:
/// text the harness did not write itself, and those an earlier summary
/// restated.
fn operator_messages(messages: &[Message]) -> Vec<String> {
    static HEADING: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\n\n--- operator message \d+ ---\n").expect("heading pattern")
    });
    let mut out = Vec::new();
    for m in messages.iter().filter(|m| m.role == Role::User) {
        for b in &m.blocks {
            let Block::Text(t) = b else { continue };
            if t.starts_with(SUMMARY_HEADER) {
                if let Some((_, restated)) = t.split_once(OPERATOR_MESSAGES) {
                    let restated = restated.strip_suffix(CARRY_ON).unwrap_or(restated);
                    out.extend(HEADING.split(restated).skip(1).map(str::to_string));
                }
            } else if !t.starts_with("[harness]") {
                out.push(t.clone());
            }
        }
    }
    out
}

/// What the summariser is asked: the transcript of `messages`, each tool
/// result cut to its head and tail, and the oldest dropped past `budget`
/// tokens (with the first, the task or an earlier summary, kept).
pub fn summary_request(conv: &Conversation, upto: usize, budget: Option<u64>) -> Conversation {
    let messages = &conv.messages[..upto];
    let mut names = std::collections::HashMap::new();
    let mut parts: Vec<String> = Vec::new();
    for m in messages {
        let mut part = String::new();
        for b in &m.blocks {
            match (m.role, b) {
                (Role::User, Block::Text(t)) => {
                    let who = if t.starts_with(SUMMARY_HEADER) {
                        "[notes from an earlier summary]"
                    } else if t.starts_with("[harness]") {
                        "[harness]"
                    } else {
                        "[operator]"
                    };
                    part.push_str(&format!("{who}\n{t}\n\n"))
                }
                (Role::Assistant, Block::Text(t)) => {
                    part.push_str(&format!("[assistant]\n{t}\n\n"))
                }
                (_, Block::ToolUse { id, name, input }) => {
                    names.insert(id.as_str(), name.as_str());
                    part.push_str(&format!("[assistant called {name}] {input}\n\n"));
                }
                (
                    _,
                    Block::ToolResult {
                        id,
                        content,
                        is_error,
                    },
                ) => {
                    let name = names.get(id.as_str()).copied().unwrap_or("tool");
                    let label = if *is_error { "error from" } else { "result of" };
                    let shown = head_and_tail(&strip_anchors(content), CUT_OUTPUT);
                    part.push_str(&format!("[{label} {name}]\n{shown}\n\n"));
                }
                (_, Block::Opaque(_)) => {}
            }
        }
        parts.push(part);
    }
    let rate = rate(conv);
    if let Some(budget) = budget {
        let mut total: u64 = parts.iter().map(|p| tokens(p.len(), rate)).sum();
        let mut dropped = 0;
        while total > budget && parts.len() > 2 {
            total -= tokens(parts.remove(1).len(), rate);
            dropped += 1;
        }
        if dropped > 0 {
            parts.insert(
                1,
                format!("[{dropped} earlier messages left out to fit the summariser's window]\n\n"),
            );
        }
    }
    let mut request = Conversation::new(SUMMARISER);
    request.push(
        Role::User,
        vec![Block::Text(format!(
            "The transcript:\n\n{}[end of transcript]\n\nWrite the notes now. The work is not \
             finished: describe only what happened above.",
            parts.concat()
        ))],
    );
    request
}

/// Said where a tool output is cut for the summariser, who otherwise fills
/// the gap with a guess.
const CUT_OUTPUT: &str = "bytes of this output are not shown to you; do not guess what they held";

/// `content`'s head and tail, with `note` on how much was left between them.
fn head_and_tail(content: &str, note: &str) -> String {
    if content.len() <= SUMMARY_RESULT_BYTES {
        return content.to_string();
    }
    let head = floor(content, SUMMARY_RESULT_BYTES * 3 / 4);
    let tail = ceil(content, content.len() - SUMMARY_RESULT_BYTES / 4);
    format!(
        "{}\n[... {} {note} ...]\n{}",
        &content[..head],
        tail - head,
        &content[tail..]
    )
}

/// Read, grep and edit outputs with their `12:a3f9|` anchors turned back into
/// plain line numbers: a summary's anchors are stale by the time it is read,
/// and a summariser took the anchor format for the file's own.
fn strip_anchors(content: &str) -> std::borrow::Cow<'_, str> {
    static ANCHOR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?m)^([^\s:|]+:)?(\d+):[0-9a-f]{4}\|").expect("anchor pattern")
    });
    ANCHOR.replace_all(content, "${1}${2}\t")
}

fn floor(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// The message that stands in for everything before the cut.
pub fn summary_message(conv: &Conversation, upto: usize, summary: &str) -> String {
    let mut text = format!(
        "{SUMMARY_HEADER} The notes below were written by the model and may be wrong; the record \
         of tool calls and the operator's messages after them are exact. Where they disagree, \
         trust the record and the files. Files may have changed since anything shown; read a \
         file again before editing it.\n\nNotes on the work so far:\n\n{}",
        summary.trim()
    );
    let calls = call_record(&conv.messages[..upto]);
    if !calls.is_empty() {
        text.push_str(CALLS);
        text.push_str(
            &calls
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    // Word for word, not left to the summariser: what the agent works to.
    // Those still in the part kept are there already.
    let mut said: Vec<String> = operator_messages(&conv.messages[..upto])
        .iter()
        .map(|m| head_and_tail(m, "bytes left out"))
        .collect();
    let mut dropped = 0;
    while said.iter().map(String::len).sum::<usize>() > MAX_OPERATOR_BYTES && said.len() > 2 {
        said.remove(1);
        dropped += 1;
    }
    if dropped > 0 {
        said.insert(1, format!("({dropped} earlier messages left out)"));
    }
    if !said.is_empty() {
        text.push_str(OPERATOR_MESSAGES);
        for (i, m) in said.iter().enumerate() {
            text.push_str(&operator_heading(i + 1));
            text.push_str(m);
        }
    }
    if upto == conv.messages.len() {
        text.push_str(CARRY_ON);
    }
    text
}

/// Replaces all but the last `keep` messages with `summary`, merged into the
/// first kept message when that is the operator's, so turns still alternate.
pub fn apply_summary(messages: &mut Vec<Message>, summary: &str, keep: usize) {
    let kept = messages.split_off(messages.len().saturating_sub(keep));
    let mut first = vec![Block::Text(summary.to_string())];
    let mut kept = kept.into_iter().peekable();
    if let Some(m) = kept.next_if(|m| m.role == Role::User) {
        first.extend(m.blocks);
    }
    *messages = vec![Message {
        role: Role::User,
        blocks: first,
    }];
    messages.extend(kept);
}

pub fn summarise(conv: &mut Conversation, summary: &str, keep: usize, overhead: usize) {
    apply_summary(&mut conv.messages, summary, keep);
    remeasure(conv, overhead);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(role: Role, t: &str) -> Message {
        Message {
            role,
            blocks: vec![Block::Text(t.into())],
        }
    }

    fn call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse {
                id: id.into(),
                name: "read".into(),
                input: json!({"path": id}),
            }],
        }
    }

    fn tool(id: &str, name: &str, input: Value) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            }],
        }
    }

    /// A result of about `tokens` tokens at the default rate.
    fn result(id: &str, tokens: usize) -> Message {
        Message {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                id: id.into(),
                content: "x".repeat(tokens * 4),
                is_error: false,
            }],
        }
    }

    fn conv(messages: Vec<Message>) -> Conversation {
        let mut c = Conversation::new("system");
        c.messages = messages;
        c
    }

    #[test]
    fn the_threshold_leaves_room_for_the_answer() {
        assert_eq!(threshold(65_536), 65_536 - 16_384);
        assert_eq!(threshold(200_000), 170_000);
        // A small window still holds half its size before compacting.
        assert_eq!(threshold(16_384), 8_192);
    }

    #[test]
    fn a_small_window_scales_the_limits_down() {
        assert_eq!(
            Limits::for_window(Some(1_000_000)),
            Limits::for_window(None)
        );
        let l = Limits::for_window(Some(65_536));
        assert_eq!((l.keep_raw, l.protect_output), (19_660, 39_321));
        let l = Limits::for_window(Some(24_000));
        assert_eq!(
            (l.keep_raw, l.protect_output, l.min_prune_saving),
            (4_800, 9_600, 4_800)
        );
    }

    #[test]
    fn the_summariser_sees_line_numbers_not_anchors() {
        let read = format!(
            "{}\n{}\n[lines 1-2 of 9]",
            crate::hashline::format(1, "def f():"),
            crate::hashline::format(2, "    return 1|2")
        );
        assert_eq!(
            strip_anchors(&read),
            "1\tdef f():\n2\t    return 1|2\n[lines 1-2 of 9]"
        );
        let grep = format!("mesa/a.py:{}", crate::hashline::format(40, "x = 1"));
        assert_eq!(strip_anchors(&grep), "mesa/a.py:40\tx = 1");
        assert_eq!(strip_anchors("12:34 is a time"), "12:34 is a time");
    }

    #[test]
    fn the_estimate_adds_to_the_last_measurement() {
        let mut c = conv(vec![text(Role::User, "go")]);
        assert_eq!(estimate(&c, 0), tokens(request_bytes(&c, 0), 4.0));
        c.messages.push(call("c1"));
        measure(&mut c, 3000, 1000, 50);
        c.messages.push(Message {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                id: "c1".into(),
                content: "y".repeat(3000 - 8),
                is_error: false,
            }],
        });
        // Measured at 3 bytes a token, then 3000 bytes more.
        assert_eq!(estimate(&c, 0), 1050 + 1000);
    }

    #[test]
    fn pruning_spares_the_newest_output_and_small_results() {
        let c = conv(vec![
            text(Role::User, "go"),
            call("old"),
            result("old", 25_000),
            call("tiny"),
            result("tiny", 10),
            call("mid"),
            result("mid", 30_000),
            call("new"),
            result("new", 30_000),
        ]);
        let plan = prune_plan(&c, Limits::for_window(None));
        // `new` is within the newest 40k; `mid` starts inside it, so it is
        // spared too; `tiny` is too small to be worth it.
        let ids: Vec<_> = plan.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["old"]);
        assert_eq!(plan[0].content, "[Output truncated - 25000 tokens]");

        let mut c = c;
        prune(&mut c, &plan, 0);
        let Block::ToolResult { content, .. } = &c.messages[2].blocks[0] else {
            unreachable!()
        };
        assert_eq!(content, "[Output truncated - 25000 tokens]");
        // Done once: nothing is left worth pruning.
        assert!(prune_plan(&c, Limits::for_window(None)).is_empty());
    }

    #[test]
    fn a_superseded_read_or_edit_is_elided_whatever_its_age() {
        let c = conv(vec![
            text(Role::User, "go"),
            tool("r1", "read", json!({"path": "a.py"})),
            result("r1", 2_000),
            tool("e1", "edit", json!({"path": "./a.py", "edits": []})),
            result("e1", 500),
            tool("r2", "read", json!({"path": "a.py"})),
            result("r2", 2_000),
            tool(
                "p1",
                "read",
                json!({"path": "b.txt", "offset": 1, "limit": 400}),
            ),
            result("p1", 3_000),
            tool(
                "p2",
                "read",
                json!({"path": "b.txt", "offset": 401, "limit": 400}),
            ),
            result("p2", 3_000),
            tool(
                "p3",
                "read",
                json!({"path": "b.txt", "offset": 1, "limit": 400}),
            ),
            result("p3", 3_000),
        ]);
        let plan = prune_plan(&c, Limits::for_window(None));
        // All within the newest 40k, so only what is stale goes: the read
        // the edit made stale, and the first of two reads of the same lines.
        // The edit's own output stands: the read after it does not change
        // the file.
        let ids: Vec<_> = plan.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["p1", "r1"]);
        assert_eq!(
            plan[1].content,
            "[Superseded by a later read or change of a.py - 2000 tokens]"
        );
        // A call that failed changed nothing, so supersedes nothing.
        let mut c = conv(vec![
            text(Role::User, "go"),
            tool("r1", "read", json!({"path": "a.py"})),
            result("r1", 2_000),
            tool("w1", "write", json!({"path": "a.py", "content": ""})),
        ]);
        c.messages.push(Message {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                id: "w1".into(),
                content: "refused".into(),
                is_error: true,
            }],
        });
        assert!(prune_plan(&c, Limits::for_window(None)).is_empty());
    }

    #[test]
    fn pruning_that_saves_little_is_not_done() {
        let c = conv(vec![
            text(Role::User, "go"),
            call("old"),
            result("old", 5_000),
            call("new"),
            result("new", 45_000),
        ]);
        assert!(prune_plan(&c, Limits::for_window(None)).is_empty());
    }

    #[test]
    fn the_cut_never_parts_a_call_from_its_result() {
        let c = conv(vec![
            text(Role::User, "go"),
            call("a"),
            result("a", 30_000),
            call("b"),
            result("b", 15_000),
            text(Role::Assistant, "done with b"),
        ]);
        // The last 20k would start inside b's result: the cut goes before
        // b's call instead, keeping the call and its result together.
        assert_eq!(cut(&c, KEEP_RAW), Some(3));

        // A last result bigger than the stretch kept is summarised with the
        // rest, and nothing is kept.
        let c = conv(vec![text(Role::User, "go"), call("a"), result("a", 40_000)]);
        assert_eq!(cut(&c, KEEP_RAW), Some(3));
    }

    #[test]
    fn nothing_is_cut_before_any_work() {
        let c = conv(vec![text(Role::User, &"x".repeat(200_000))]);
        assert_eq!(cut(&c, KEEP_RAW), None);
        let summary = text(Role::User, &format!("{SUMMARY_HEADER} s"));
        let c = conv(vec![summary, text(Role::User, &"x".repeat(100_000))]);
        assert_eq!(cut(&c, KEEP_RAW), None);
    }

    #[test]
    fn the_summary_restates_the_operators_words_and_merges_with_the_operator() {
        let c = conv(vec![
            text(Role::User, "rename foo to bar"),
            call("a"),
            result("a", 100),
            text(Role::User, "[harness] a note"),
            text(Role::User, "and baz too"),
            call("b"),
            result("b", 100),
        ]);
        let s = summary_message(&c, 7, "renamed two of three");
        assert!(s.starts_with(SUMMARY_HEADER), "{s}");
        assert!(
            s.contains(
                "word for word, oldest first:\n\n--- operator message 1 ---\nrename foo to bar\
                 \n\n--- operator message 2 ---\nand baz too\n\nCarry on"
            ),
            "{s}"
        );
        assert!(
            !s.contains("a note"),
            "the harness's own words are not the operator's"
        );
        assert!(s.ends_with("Carry on with the work from here."), "{s}");
        // A later compaction finds them in the summary, and keeps them.
        let c = conv(vec![
            text(Role::User, &s),
            call("c"),
            result("c", 100),
            text(Role::User, "then commit"),
            call("d"),
        ]);
        let again = summary_message(&c, 3, "renamed all three");
        assert_eq!(
            operator_messages(&c.messages[..3]),
            ["rename foo to bar", "and baz too"]
        );
        assert!(
            again.contains("--- operator message 2 ---\nand baz too"),
            "{again}"
        );
        assert!(
            !again.contains("then commit"),
            "still in the part kept: {again}"
        );
        assert!(!again.contains("Carry on"), "{again}");

        let mut c = conv(vec![
            text(Role::User, "first"),
            call("a"),
            result("a", 100),
            text(Role::User, "second"),
            call("b"),
        ]);
        let s = summary_message(&c, 3, "did first");
        apply_summary(&mut c.messages, &s, 2);
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[0].role, Role::User);
        assert_eq!(c.messages[0].blocks[1], Block::Text("second".into()));
        assert_eq!(c.messages[1].role, Role::Assistant);
    }

    #[test]
    fn the_summary_lists_the_calls_that_ran_and_carries_them_over() {
        let failed = Message {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                id: "b".into(),
                content: "no such file\nmore".into(),
                is_error: true,
            }],
        };
        let c = conv(vec![
            text(Role::User, "go"),
            call("a"),
            result("a", 10),
            call("b"),
            failed,
        ]);
        let s = summary_message(&c, 5, "notes");
        assert!(
            s.contains(
                "oldest first:\n- read {\"path\":\"a\"} → ok\n- read {\"path\":\"b\"} → \
                 error: no such file\n\nThe operator's messages"
            ),
            "{s}"
        );
        let c = conv(vec![text(Role::User, &s), call("c"), result("c", 10)]);
        let calls = call_record(&c.messages);
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[2], "read {\"path\":\"c\"} → ok");

        let mut many = vec![text(Role::User, "go")];
        for i in 0..70 {
            many.push(call(&format!("c{i}")));
            many.push(result(&format!("c{i}"), 1));
        }
        let calls = call_record(&many);
        assert_eq!(calls.len(), MAX_CALLS + 1);
        assert_eq!(calls[0], "(10 earlier calls not listed)");
        let s = summary_message(&conv(many), 141, "notes");
        let again = call_record(&[text(Role::User, &s), call("z"), result("z", 1)]);
        assert_eq!(again[0], "(11 earlier calls not listed)");
        assert_eq!(again.len(), MAX_CALLS + 1);
    }

    #[test]
    fn a_long_run_of_operator_messages_keeps_the_first_and_the_newest() {
        let mut messages = Vec::new();
        for i in 0..20 {
            messages.push(text(
                Role::User,
                &format!("request {i} {}", "x".repeat(2000)),
            ));
            messages.push(call(&format!("c{i}")));
            messages.push(result(&format!("c{i}"), 10));
        }
        let c = conv(messages);
        let s = summary_message(&c, c.messages.len(), "much");
        assert!(s.contains("--- operator message 1 ---\nrequest 0 "));
        assert!(s.contains("earlier messages left out)"));
        assert!(s.contains("request 19 "));
        assert!(!s.contains("request 5 "));
    }

    #[test]
    fn the_summariser_sees_results_cut_and_named() {
        let c = conv(vec![
            text(Role::User, "go"),
            call("a"),
            result("a", 10_000),
            text(Role::Assistant, "read it"),
        ]);
        let r = summary_request(&c, 4, None);
        assert_eq!(r.system, SUMMARISER);
        let Block::Text(t) = &r.messages[0].blocks[0] else {
            unreachable!()
        };
        assert!(
            t.contains("[assistant called read] {\"path\":\"a\"}"),
            "{t}"
        );
        assert!(t.contains("[result of read]\nxxx"), "{t}");
        assert!(t.contains("not shown to you; do not guess"), "{t}");
        assert!(t.len() < 10_000, "{}", t.len());

        let many: Vec<_> = (0..50)
            .flat_map(|i| [call(&format!("c{i}")), result(&format!("c{i}"), 1_500)])
            .collect();
        let mut messages = vec![text(Role::User, "the task")];
        messages.extend(many);
        let c = conv(messages);
        let r = summary_request(&c, c.messages.len(), Some(20_000));
        let Block::Text(t) = &r.messages[0].blocks[0] else {
            unreachable!()
        };
        assert!(t.contains("[operator]\nthe task"), "the task is kept");
        assert!(t.contains("earlier messages left out"));
        assert!((t.len() as u64) / 4 <= 21_000, "{}", t.len());
    }
}
