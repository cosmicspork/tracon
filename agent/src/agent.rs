//! The loop: ask the model, run the tools it called, hand back the results,
//! until it answers without a tool call or the turn limit is reached.

use std::time::Instant;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::conversation::{Block, Conversation, Role};
use crate::events::Events;
use crate::tools::{Context, Output, Registry};
use crate::wire::openai_chat::{Client, Delta};
use crate::wire::{Usage, WireError};

/// How much of a tool result goes into the events file.
const PREVIEW: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Finished { text: String },
    MaxTurns,
    ContextOverflow { message: String },
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
    /// After each model request: its usage, and how much of the context the
    /// conversation now fills.
    fn usage(&self, _usage: &Usage, _context_used: u64) {}
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
    /// Per run: one prompt's worth of model requests. `None` runs until the
    /// model answers without a tool call, or the run is cancelled.
    pub max_turns: Option<u32>,
}

impl Agent<'_> {
    pub async fn run(
        &self,
        conv: &mut Conversation,
        totals: &mut Totals,
    ) -> Result<Outcome, WireError> {
        let specs = self.tools.specs();
        let mut requests = 0;
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
            let request = self.client.complete(conv, &specs, |d: Delta<'_>| {
                first.get_or_insert_with(Instant::now);
                self.hooks.delta(&d);
            });
            let result = tokio::select! {
                r = request => r,
                _ = self.cancel.cancelled() => {
                    self.events.emit("cancelled", json!({"turn": turn, "during": "request"}));
                    return Ok(Outcome::Cancelled);
                }
            };
            let completion = match result {
                Ok(c) => c,
                Err(WireError::ContextOverflow(message)) => {
                    self.events.emit(
                        "error",
                        json!({"turn": turn, "kind": "context_overflow", "message": message}),
                    );
                    return Ok(Outcome::ContextOverflow { message });
                }
                Err(e) => {
                    self.events
                        .emit("error", json!({"turn": turn, "message": e.to_string()}));
                    return Err(e);
                }
            };
            let elapsed = started.elapsed();
            let ttft = first.map(|f| f - started);
            if let Some(u) = &completion.usage {
                totals.usage.input += u.input;
                totals.usage.output += u.output;
                totals.usage.cache_read += u.cache_read;
                self.hooks.usage(u, u.input + u.output);
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
            if !completion.blocks.is_empty() {
                conv.push(Role::Assistant, completion.blocks);
            }
            if calls.is_empty() {
                return Ok(Outcome::Finished { text });
            }
            let mut results = Vec::new();
            let mut cancelled = false;
            for (id, name, input) in calls {
                self.events.emit(
                    "tool_call",
                    json!({"turn": turn, "id": id, "name": name, "input": input}),
                );
                self.hooks.tool_call(&id, &name, &input);
                let started = Instant::now();
                // Every call has a result, even one never run, so the
                // conversation stays well formed for the next prompt.
                let out = if cancelled {
                    Output::error("not run: the turn was cancelled")
                } else {
                    match self.hooks.permit(&id, &name, &input).await {
                        Permit::Allow => {
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
}
