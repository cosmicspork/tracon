//! The loop: ask the model, run the tools it called, hand back the results,
//! until it answers without a tool call or the turn limit is reached.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::compact;
use crate::conversation::{Block, Conversation, Role};
use crate::events::Events;
use crate::journal::{Journal, Record};
use crate::tools::{Context, Output, Registry};
use crate::wire::openai_chat::{Client, Completion, Delta};
use crate::wire::{Usage, WireError};

/// How much of a tool result goes into the events file.
const PREVIEW: usize = 2000;

/// Waits between attempts at one model request; one more attempt than waits.
pub(crate) const BACKOFF: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];
/// A `Retry-After` longer than this is the upstream saying "not soon": the
/// turn ends with the error rather than holding the session that long.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Markup a chat template uses for a tool's answer. In the model's own text it
/// means the model wrote the result itself instead of waiting for it.
const FABRICATED: [&str; 4] = [
    "<tool_response>",
    "<tool_result>",
    "<|im_start|>tool",
    "<function_results>",
];
/// A call written into the text, which the server's template did not parse out
/// as a call.
const UNPARSED_CALL: &str = "<tool_call>";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Finished {
        text: String,
    },
    MaxTurns,
    ContextOverflow {
        message: String,
    },
    Cancelled,
    /// The loop stopped the turn itself, for a reason the operator is shown.
    Stopped {
        reason: String,
    },
}

/// What made the conversation be compacted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The next request would have come near the context window.
    Threshold,
    /// The upstream refused a request as longer than its context.
    Overflow,
}

/// A compaction, as the operator is told of it. Token counts are estimates.
#[derive(Debug, Clone, Serialize)]
pub struct Compaction {
    pub trigger: Trigger,
    pub tokens_before: u64,
    pub tokens_after: u64,
    /// Old tool results replaced by a note of their size.
    pub pruned_results: usize,
    /// Messages replaced by the summary; 0 when pruning was enough.
    pub summarised_messages: usize,
    pub summary: Option<String>,
}

enum Compacting {
    Done,
    /// Nothing could be made smaller, or the summary could not be had.
    Nothing,
    Cancelled,
}

/// The answer to asking whether a tool call may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permit {
    Allow,
    /// Refused, with what the model is told.
    Deny(String),
    /// The turn is being cancelled.
    Cancelled,
}

/// What the loop reports as it goes and asks before acting. Standalone runs
/// answer nothing and allow everything; under the node every call is asked.
#[async_trait]
pub trait Hooks: Send + Sync {
    fn delta(&self, _delta: &Delta<'_>) {}
    fn tool_call(&self, _id: &str, _name: &str, _input: &Value) {}
    async fn permit(&self, _id: &str, _name: &str, _input: &Value) -> Permit {
        Permit::Allow
    }
    fn tool_result(&self, _id: &str, _name: &str, _output: &Output) {}
    /// After each model request: its usage and id, and how much of the context
    /// the conversation now fills.
    fn usage(&self, _usage: &Usage, _request_id: Option<&str>, _context_used: u64) {}
    /// A model request failed and is about to be made again.
    fn retrying(&self, _attempt: u32, _wait: Duration, _error: &WireError) {}
    /// The conversation was made smaller to fit the context window.
    fn compacted(&self, _compaction: &Compaction) {}
}

/// No one to ask: `--standalone`.
pub struct Unattended;

impl Hooks for Unattended {}

#[derive(Debug, Default, Serialize)]
pub struct Totals {
    pub turns: u32,
    pub usage: Usage,
    pub tool_calls: u32,
    pub tool_errors: u32,
}

pub struct Agent<'a> {
    pub client: &'a Client,
    pub tools: &'a Registry,
    pub cx: &'a Context,
    pub events: &'a Events,
    pub hooks: &'a dyn Hooks,
    pub cancel: &'a CancellationToken,
    /// Where each step is written before it takes effect; `None` standalone.
    pub journal: Option<&'a Journal>,
    /// Per run: one prompt's worth of model requests. `None` runs until the
    /// model answers without a tool call, or the run is cancelled.
    pub max_turns: Option<u32>,
    /// The model's window in tokens. Known, the conversation is compacted
    /// before it reaches it; unknown, only once the upstream refuses a
    /// request as too long.
    pub context_window: Option<u64>,
}

/// A request that failed for good, and any answer text already shown.
struct Failed {
    error: WireError,
    shown: String,
}

impl Agent<'_> {
    fn record(&self, record: Record) {
        if let Some(j) = self.journal {
            j.write(&record);
        }
    }

    fn push(&self, conv: &mut Conversation, role: Role, blocks: Vec<Block>) {
        self.record(Record::Message {
            role,
            blocks: blocks.clone(),
        });
        conv.push(role, blocks);
    }

    pub async fn run(
        &self,
        conv: &mut Conversation,
        totals: &mut Totals,
    ) -> Result<Outcome, WireError> {
        let specs = self.tools.specs();
        let overhead = Value::Array(specs.clone()).to_string().len();
        let mut requests = 0;
        // Responses in a row whose calls could not be run as written.
        let mut malformed = 0;
        // The last request was refused as too long, and the conversation
        // compacted since: a second refusal ends the turn.
        let mut overflowed = false;
        // What the last compaction left, when that was still past the
        // threshold: compacting again before much is added would only
        // summarise the summary.
        let mut stuck_at: Option<u64> = None;
        loop {
            if self.max_turns.is_some_and(|max| requests >= max) {
                return Ok(Outcome::MaxTurns);
            }
            if let Some(window) = self.context_window {
                let size = compact::estimate(conv, overhead);
                let threshold = compact::threshold(window);
                let grown = stuck_at.is_none_or(|at| {
                    size >= at + compact::Limits::for_window(Some(window)).keep_raw / 2
                });
                if size >= threshold && grown {
                    match self
                        .compact(conv, totals, Trigger::Threshold, overhead)
                        .await
                    {
                        Compacting::Cancelled => return Ok(Outcome::Cancelled),
                        Compacting::Done | Compacting::Nothing => {}
                    }
                    let after = compact::estimate(conv, overhead);
                    stuck_at = (after >= threshold).then_some(after);
                }
            }
            requests += 1;
            totals.turns += 1;
            let turn = totals.turns;
            self.events.emit(
                "request",
                json!({"turn": turn, "messages": conv.messages.len() + 1}),
            );
            let started = Instant::now();
            let mut first: Option<Instant> = None;
            let sent = compact::request_bytes(conv, overhead);
            let completion = match self
                .request(conv, &specs, None, true, turn, &mut first)
                .await
            {
                Ok(Some(c)) => c,
                Ok(None) => {
                    self.events
                        .emit("cancelled", json!({"turn": turn, "during": "request"}));
                    return Ok(Outcome::Cancelled);
                }
                Err(Failed {
                    error: WireError::ContextOverflow(message),
                    ..
                }) => {
                    // Compacted and asked once more; never truncated.
                    if !overflowed {
                        self.events
                            .emit("overflow", json!({"turn": turn, "message": message}));
                        match self
                            .compact(conv, totals, Trigger::Overflow, overhead)
                            .await
                        {
                            Compacting::Done => {
                                overflowed = true;
                                continue;
                            }
                            Compacting::Cancelled => return Ok(Outcome::Cancelled),
                            Compacting::Nothing => {}
                        }
                    }
                    self.events.emit(
                        "error",
                        json!({"turn": turn, "kind": "context_overflow", "message": message}),
                    );
                    return Ok(Outcome::ContextOverflow { message });
                }
                Err(Failed { error, shown }) => {
                    self.events
                        .emit("error", json!({"turn": turn, "message": error.to_string()}));
                    // The operator saw this much of an answer; the model's
                    // record says the same, so the next prompt starts from it.
                    if !shown.trim().is_empty() {
                        self.push(conv, Role::Assistant, vec![Block::Text(shown)]);
                    }
                    return Err(error);
                }
            };
            overflowed = false;
            let elapsed = started.elapsed();
            let ttft = first.map(|f| f - started);
            if let Some(u) = &completion.usage {
                add(totals, u);
                self.hooks
                    .usage(u, completion.request_id.as_deref(), u.input + u.output);
            }
            let measured = completion.usage.clone();
            // Generation speed over the time after the first token; llama.cpp's
            // own figure is in `timings` when it sends them.
            let tokens_per_s = match (&completion.usage, ttft) {
                (Some(u), Some(ttft)) if elapsed > ttft => {
                    Some(u.output as f64 / (elapsed - ttft).as_secs_f64())
                }
                _ => None,
            };
            let text = text_of(&completion.blocks);
            self.events.emit(
                "response",
                json!({
                    "turn": turn,
                    "request_id": completion.request_id,
                    "finish_reason": completion.finish_reason,
                    "usage": completion.usage,
                    "elapsed_ms": elapsed.as_millis() as u64,
                    "ttft_ms": ttft.map(|t| t.as_millis() as u64),
                    "tokens_per_s": tokens_per_s,
                    "server_timings": completion.timings,
                    "reasoning_chars": completion.reasoning.chars().count(),
                    "text": text,
                }),
            );
            let calls: Vec<(String, String, Value)> = completion
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();
            let finish_reason = completion.finish_reason.clone();
            // Written before any call runs: a process that dies from here on
            // is carried on knowing which calls were made.
            if !completion.blocks.is_empty() {
                self.push(conv, Role::Assistant, completion.blocks);
            }
            if let Some(u) = measured {
                compact::measure(conv, sent, u.input, u.output);
            }
            if let Some(marker) = FABRICATED.iter().find(|m| text.contains(**m)) {
                let reason = format!(
                    "the model wrote a tool's result itself ({marker}) instead of waiting for \
                     one; the turn was stopped so nothing acts on an invented result"
                );
                self.events
                    .emit("stopped", json!({"turn": turn, "reason": reason}));
                self.close_unrun(conv, &calls, "not run: the turn was stopped");
                return Ok(Outcome::Stopped { reason });
            }
            if calls.is_empty() {
                if text.contains(UNPARSED_CALL) {
                    malformed += 1;
                    let problem = "a tool call was written into the text, where it is not a \
                                   call; nothing ran. Call the tool through the tools interface";
                    if let Some(stop) = self.malformed_stop(malformed, turn, problem) {
                        return Ok(stop);
                    }
                    self.push(
                        conv,
                        Role::User,
                        vec![Block::Text(format!("[harness] {problem}."))],
                    );
                    continue;
                }
                return Ok(Outcome::Finished { text });
            }
            let problems: Vec<Option<String>> = calls
                .iter()
                .map(|(_, name, input)| self.problem(name, input, finish_reason.as_deref()))
                .collect();
            if let Some(first) = problems.iter().flatten().next().cloned() {
                malformed += 1;
                let mut results = Vec::new();
                for ((id, name, input), problem) in calls.iter().zip(&problems) {
                    let out = Output::error(problem.clone().unwrap_or_else(|| {
                        "not run: another call in the same response was malformed; make each \
                         call again, well formed"
                            .into()
                    }));
                    // Shown, never asked: nothing about it could run.
                    self.hooks.tool_call(id, name, input);
                    self.hooks.tool_result(id, name, &out);
                    self.record(Record::ToolResult {
                        id: id.clone(),
                        content: out.content.clone(),
                        is_error: true,
                    });
                    results.push(Block::ToolResult {
                        id: id.clone(),
                        content: out.content,
                        is_error: true,
                    });
                }
                conv.push(Role::User, results);
                if let Some(stop) = self.malformed_stop(malformed, turn, &first) {
                    return Ok(stop);
                }
                continue;
            }
            malformed = 0;
            let mut results = Vec::new();
            let mut cancelled = false;
            for (id, name, input) in calls {
                self.events.emit(
                    "tool_call",
                    json!({"turn": turn, "id": id, "name": name, "input": input}),
                );
                self.hooks.tool_call(&id, &name, &input);
                self.record(Record::ToolAsked { id: id.clone() });
                let started = Instant::now();
                // Every call has a result, even one never run, so the
                // conversation stays well formed for the next prompt.
                let out = if cancelled {
                    Output::error("not run: the turn was cancelled")
                } else {
                    match self.hooks.permit(&id, &name, &input).await {
                        Permit::Allow => {
                            self.record(Record::ToolStarted { id: id.clone() });
                            let run = self.tools.run(self.cx, &name, input);
                            tokio::select! {
                                out = run => out,
                                _ = self.cancel.cancelled() => {
                                    cancelled = true;
                                    Output::error("stopped: the turn was cancelled")
                                }
                            }
                        }
                        Permit::Deny(why) => Output::error(why),
                        Permit::Cancelled => {
                            cancelled = true;
                            Output::error("not run: the turn was cancelled")
                        }
                    }
                };
                self.record(Record::ToolResult {
                    id: id.clone(),
                    content: out.content.clone(),
                    is_error: out.is_error,
                });
                self.hooks.tool_result(&id, &name, &out);
                totals.tool_calls += 1;
                if out.is_error {
                    totals.tool_errors += 1;
                }
                self.events.emit(
                    "tool_result",
                    json!({
                        "turn": turn,
                        "id": id,
                        "name": name,
                        "is_error": out.is_error,
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                        "bytes": out.content.len(),
                        "content": out.content.chars().take(PREVIEW).collect::<String>(),
                    }),
                );
                results.push(Block::ToolResult {
                    id,
                    content: out.content,
                    is_error: out.is_error,
                });
            }
            conv.push(Role::User, results);
            if cancelled || self.cancel.is_cancelled() {
                self.events
                    .emit("cancelled", json!({"turn": turn, "during": "tools"}));
                return Ok(Outcome::Cancelled);
            }
        }
    }

    /// Makes the conversation smaller: old tool outputs pruned, then, if it
    /// still does not fit, or the upstream refused it, everything before its
    /// last stretch summarised by the model. Each step is journaled before
    /// the conversation changes.
    async fn compact(
        &self,
        conv: &mut Conversation,
        totals: &mut Totals,
        trigger: Trigger,
        overhead: usize,
    ) -> Compacting {
        let turn = totals.turns;
        let before = compact::estimate(conv, overhead);
        let limits = compact::Limits::for_window(self.context_window);
        let pruned = compact::prune_plan(conv, limits);
        if !pruned.is_empty() {
            self.record(Record::Pruned {
                results: pruned.clone(),
            });
            compact::prune(conv, &pruned, overhead);
        }
        let fits = self
            .context_window
            .is_some_and(|w| compact::estimate(conv, overhead) < compact::threshold(w));
        let mut summarised = 0;
        let mut summary = None;
        let upto = match trigger {
            Trigger::Threshold if fits => None,
            Trigger::Threshold => compact::cut(conv, limits.keep_raw),
            // The estimate was wrong, so it is no guide to what fits: keep a
            // quarter of the messages at most.
            Trigger::Overflow => compact::cut(
                conv,
                limits.keep_raw.min(compact::messages_tokens(conv) / 4),
            ),
        };
        if let Some(upto) = upto {
            // The summariser's own request has to fit, answer included.
            let budget = self.context_window.map(|w| {
                compact::threshold(w).saturating_sub(u64::from(compact::SUMMARY_MAX_TOKENS) + 1024)
            });
            let request = compact::summary_request(conv, upto, budget);
            self.events.emit(
                "summarising",
                json!({"turn": turn, "trigger": trigger, "messages": upto}),
            );
            let started = Instant::now();
            let mut first = None;
            let asked = self
                .request(
                    &request,
                    &[],
                    Some(compact::SUMMARY_MAX_TOKENS),
                    false,
                    turn,
                    &mut first,
                )
                .await;
            let completion = match asked {
                Ok(Some(c)) => Some(c),
                Ok(None) => return Compacting::Cancelled,
                Err(Failed { error, .. }) => {
                    self.events.emit(
                        "error",
                        json!({"turn": turn, "kind": "summary", "message": error.to_string()}),
                    );
                    None
                }
            };
            let text = completion.as_ref().map(|c| text_of(&c.blocks));
            self.events.emit(
                "summary",
                json!({
                    "turn": turn,
                    "request_id": completion.as_ref().and_then(|c| c.request_id.clone()),
                    "usage": completion.as_ref().and_then(|c| c.usage.clone()),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                    "text": text,
                }),
            );
            if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
                let message = compact::summary_message(conv, upto, &text);
                let keep = conv.messages.len() - upto;
                self.record(Record::Compacted {
                    summary: message.clone(),
                    keep,
                });
                compact::summarise(conv, &message, keep, overhead);
                summarised = upto;
                summary = Some(text);
            }
            // Counted whether or not it was used: the upstream answered it.
            if let Some(c) = &completion {
                if let Some(u) = &c.usage {
                    add(totals, u);
                    self.hooks.usage(
                        u,
                        c.request_id.as_deref(),
                        compact::estimate(conv, overhead),
                    );
                }
            }
        }
        if pruned.is_empty() && summary.is_none() {
            return Compacting::Nothing;
        }
        let compaction = Compaction {
            trigger,
            tokens_before: before,
            tokens_after: compact::estimate(conv, overhead),
            pruned_results: pruned.len(),
            summarised_messages: summarised,
            summary,
        };
        let mut event = json!(compaction);
        event["turn"] = json!(turn);
        self.events.emit("compacted", event);
        self.hooks.compacted(&compaction);
        Compacting::Done
    }

    /// One model request, made again while it fails in a way another attempt
    /// could fix and nothing of an answer has been shown. `Ok(None)` is a
    /// cancel. `show` streams the answer to the hooks as it comes.
    async fn request(
        &self,
        conv: &Conversation,
        specs: &[Value],
        max_tokens: Option<u32>,
        show: bool,
        turn: u32,
        first: &mut Option<Instant>,
    ) -> Result<Option<Completion>, Failed> {
        let mut attempt = 0u32;
        loop {
            let mut shown = String::new();
            let request = self
                .client
                .complete(conv, specs, max_tokens, |d: Delta<'_>| {
                    first.get_or_insert_with(Instant::now);
                    if !show {
                        return;
                    }
                    if let Delta::Text(t) = d {
                        shown.push_str(t);
                    }
                    self.hooks.delta(&d);
                });
            let error = tokio::select! {
                r = request => match r {
                    Ok(c) => return Ok(Some(c)),
                    Err(e) => e,
                },
                _ = self.cancel.cancelled() => return Ok(None),
            };
            // Reasoning and whitespace were never an answer; text the operator
            // read was, and asking again would answer twice.
            let wait = BACKOFF.get(attempt as usize).copied();
            let retry = error.retryable() && shown.trim().is_empty();
            let wait = match (retry, wait, error.retry_after()) {
                (true, Some(_), Some(asked)) if asked > MAX_RETRY_AFTER => None,
                (true, Some(backoff), asked) => Some(asked.map_or(backoff, |a| a.max(backoff))),
                _ => None,
            };
            let Some(wait) = wait else {
                return Err(Failed { error, shown });
            };
            attempt += 1;
            *first = None;
            self.events.emit(
                "retry",
                json!({
                    "turn": turn,
                    "attempt": attempt,
                    "wait_ms": wait.as_millis() as u64,
                    "error": error.to_string(),
                }),
            );
            self.hooks.retrying(attempt, wait, &error);
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.cancel.cancelled() => return Ok(None),
            }
        }
    }

    /// Why a call cannot run as written, said to the model.
    fn problem(&self, name: &str, input: &Value, finish_reason: Option<&str>) -> Option<String> {
        if finish_reason == Some("length") {
            return Some(
                "not run: the response hit its output limit while writing this call, so its \
                 arguments are cut off. Make the call again, with less in it"
                    .into(),
            );
        }
        if name.trim().is_empty() {
            return Some("not run: the call names no tool".into());
        }
        if self.tools.get(name).is_none() {
            return Some(format!(
                "not run: there is no tool `{name}`; the tools are: {}",
                self.tools.names().join(", ")
            ));
        }
        match input {
            Value::Object(_) => None,
            Value::String(raw) => Some(format!(
                "not run: the arguments were not valid JSON: {raw}\nCall {name} again with a \
                 JSON object."
            )),
            _ => Some(format!(
                "not run: the arguments must be a JSON object. Call {name} again with one."
            )),
        }
    }

    /// A second malformed response in a row ends the turn.
    fn malformed_stop(&self, malformed: u32, turn: u32, problem: &str) -> Option<Outcome> {
        if malformed < 2 {
            return None;
        }
        let reason =
            format!("the model made two malformed responses in a row; the last: {problem}");
        self.events
            .emit("stopped", json!({"turn": turn, "reason": reason}));
        Some(Outcome::Stopped { reason })
    }

    /// Answers calls that will not run, so the conversation stays well formed.
    fn close_unrun(&self, conv: &mut Conversation, calls: &[(String, String, Value)], why: &str) {
        if calls.is_empty() {
            return;
        }
        let mut results = Vec::new();
        for (id, name, input) in calls {
            let out = Output::error(why);
            self.hooks.tool_call(id, name, input);
            self.hooks.tool_result(id, name, &out);
            self.record(Record::ToolResult {
                id: id.clone(),
                content: why.into(),
                is_error: true,
            });
            results.push(Block::ToolResult {
                id: id.clone(),
                content: why.into(),
                is_error: true,
            });
        }
        conv.push(Role::User, results);
    }
}

fn add(totals: &mut Totals, u: &Usage) {
    totals.usage.input += u.input;
    totals.usage.output += u.output;
    totals.usage.cache_read += u.cache_read;
}

fn text_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
