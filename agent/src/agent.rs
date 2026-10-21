//! The loop: ask the model, run the tools it called, hand back the results,
//! until it answers without a tool call or the turn limit is reached.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

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
        let mut requests = 0;
        // Responses in a row whose calls could not be run as written.
        let mut malformed = 0;
        loop {
            if self.max_turns.is_some_and(|max| requests >= max) {
                return Ok(Outcome::MaxTurns);
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
            let completion = match self.request(conv, &specs, turn, &mut first).await {
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
            let elapsed = started.elapsed();
            let ttft = first.map(|f| f - started);
            if let Some(u) = &completion.usage {
                totals.usage.input += u.input;
                totals.usage.output += u.output;
                totals.usage.cache_read += u.cache_read;
                self.hooks
                    .usage(u, completion.request_id.as_deref(), u.input + u.output);
            }
            // Generation speed over the time after the first token; llama.cpp's
            // own figure is in `timings` when it sends them.
            let tokens_per_s = match (&completion.usage, ttft) {
                (Some(u), Some(ttft)) if elapsed > ttft => {
                    Some(u.output as f64 / (elapsed - ttft).as_secs_f64())
                }
                _ => None,
            };
            let text: String = completion
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
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

    /// One model request, made again while it fails in a way another attempt
    /// could fix and nothing of an answer has been shown. `Ok(None)` is a
    /// cancel.
    async fn request(
        &self,
        conv: &Conversation,
        specs: &[Value],
        turn: u32,
        first: &mut Option<Instant>,
    ) -> Result<Option<Completion>, Failed> {
        let mut attempt = 0u32;
        loop {
            let mut shown = String::new();
            let request = self.client.complete(conv, specs, |d: Delta<'_>| {
                first.get_or_insert_with(Instant::now);
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
