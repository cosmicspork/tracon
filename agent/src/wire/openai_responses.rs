//! The OpenAI Responses API, streamed: all the Codex backend serves.
//!
//! Nothing is stored upstream (`store: false`), so every request replays the
//! whole conversation. A reasoning model's reasoning comes back as encrypted
//! items that must be sent again, in place, for it to carry its thinking from
//! one request to the next; they ride in the conversation as `Opaque` blocks
//! and are never read here. The system prompt goes in `instructions`.

use std::collections::BTreeMap;

use futures_util::StreamExt;
use serde_json::{json, Map, Value};

use super::{Completion, Delta, Sampling, Usage, WireError};
use crate::conversation::{Block, Conversation, Role};
use crate::sse::{Decoder, Frame};

pub struct Client {
    http: reqwest::Client,
    url: String,
    api_key: Option<String>,
    model: String,
    sampling: Sampling,
    parallel_tools: bool,
}

impl Client {
    /// The same endpoint and settings, for another model.
    pub fn with_model(&self, model: &str) -> Self {
        Self {
            http: self.http.clone(),
            url: self.url.clone(),
            api_key: self.api_key.clone(),
            model: model.to_string(),
            sampling: self.sampling.clone(),
            parallel_tools: self.parallel_tools,
        }
    }

    pub fn new(
        base_url: &str,
        api_key: Option<String>,
        model: &str,
        sampling: Sampling,
        parallel_tools: bool,
    ) -> Result<Self, WireError> {
        Ok(Self {
            http: super::http()?,
            url: format!("{}/responses", base_url.trim_end_matches('/')),
            api_key,
            model: model.to_string(),
            sampling,
            parallel_tools,
        })
    }

    /// No `max_output_tokens`: the Codex backend refuses it, as OpenCode's
    /// Codex plugin found, so an answer is bounded only by the model.
    pub fn body(&self, conv: &Conversation, tools: &[Value]) -> Value {
        let mut reasoning = Map::new();
        if let Some(effort) = &self.sampling.reasoning_effort {
            reasoning.insert("effort".into(), json!(effort));
        }
        // Summaries are what the operator sees of the thinking.
        reasoning.insert("summary".into(), json!("auto"));
        let mut body = json!({
            "model": self.model,
            "instructions": conv.system,
            "input": input(conv),
            "stream": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "reasoning": reasoning,
        });
        let o = body.as_object_mut().expect("object");
        if let Some(key) = &conv.cache_key {
            o.insert("prompt_cache_key".into(), json!(key));
        }
        if !tools.is_empty() {
            let tools: Vec<Value> = tools
                .iter()
                .map(|t| {
                    // The agent's own tools strictly, so the upstream holds
                    // the model to them; a node tool's schema as it came.
                    let strict = (t["own_schema"] == true)
                        .then(|| super::strict_schema(&t["parameters"]))
                        .flatten();
                    json!({
                        "type": "function",
                        "name": t["name"],
                        "description": t["description"],
                        "strict": strict.is_some(),
                        "parameters": strict.unwrap_or_else(|| t["parameters"].clone()),
                    })
                })
                .collect();
            o.insert("tools".into(), Value::Array(tools));
            o.insert("tool_choice".into(), json!("auto"));
            o.insert("parallel_tool_calls".into(), json!(self.parallel_tools));
        }
        if let Some(t) = self.sampling.temperature {
            o.insert("temperature".into(), json!(t));
        }
        body
    }

    pub async fn complete(
        &self,
        conv: &Conversation,
        tools: &[Value],
        mut on_delta: impl FnMut(Delta<'_>),
    ) -> Result<Completion, WireError> {
        let mut request = self
            .http
            .post(&self.url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&self.body(conv, tools));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        // With the body's `prompt_cache_key`, what Codex CLI sends so a
        // session's requests reach a warm prompt cache.
        if let Some(key) = &conv.cache_key {
            request = request.header("session_id", key);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let header = super::retry_after(response.headers());
            let body = response.text().await.unwrap_or_default();
            return Err(error_from(status.as_u16(), &body, header));
        }
        let mut decoder = Decoder::default();
        let mut assembler = Assembler::default();
        let mut stream = response.bytes_stream();
        'read: while let Some(chunk) = stream.next().await {
            for frame in decoder.push(&chunk?) {
                let Frame::Data(data) = frame else { continue };
                let Ok(event) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                assembler.push(&event, &mut on_delta)?;
                if assembler.done {
                    break 'read;
                }
            }
        }
        assembler.finish()
    }
}

/// The conversation as Responses input items. Calls and their results are
/// items of their own; reasoning items go back as they came, less the id an
/// unstored response cannot be looked up by.
fn input(conv: &Conversation) -> Vec<Value> {
    let mut out = Vec::new();
    for m in &conv.messages {
        for b in &m.blocks {
            match b {
                Block::Text(t) => {
                    let (role, kind) = match m.role {
                        Role::User => ("user", "input_text"),
                        Role::Assistant => ("assistant", "output_text"),
                    };
                    out.push(json!({
                        "type": "message",
                        "role": role,
                        "content": [{"type": kind, "text": t}],
                    }));
                }
                Block::ToolUse { id, name, input } => out.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": match input {
                        Value::String(raw) => raw.clone(),
                        other => other.to_string(),
                    },
                })),
                Block::ToolResult {
                    id,
                    content,
                    is_error,
                } => out.push(json!({
                    "type": "function_call_output",
                    "call_id": id,
                    "output": if *is_error { format!("ERROR: {content}") } else { content.clone() },
                })),
                Block::Opaque(item) => {
                    let mut item = item.clone();
                    if let Some(o) = item.as_object_mut() {
                        o.remove("id");
                        o.remove("status");
                    }
                    out.push(item);
                }
            }
        }
    }
    out
}

fn error_from(status: u16, body: &str, header: Option<u64>) -> WireError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error = parsed.as_ref().map(|v| &v["error"]).unwrap_or(&Value::Null);
    if is_overflow(error) {
        return WireError::ContextOverflow(error["message"].as_str().unwrap_or(body).to_string());
    }
    // The Codex backend says when a usage limit lifts in the body.
    let resets = error["resets_in_seconds"].as_u64();
    WireError::Http {
        status,
        body: body.chars().take(2000).collect(),
        retry_after: header.or(resets),
    }
}

fn is_overflow(error: &Value) -> bool {
    error["code"] == "context_length_exceeded"
        || error["type"] == "context_length_exceeded"
        || error["message"]
            .as_str()
            .is_some_and(|m| m.contains("exceeds the context window"))
}

/// Builds a completion from stream events, keyed by `type`. The finished
/// output items are authoritative; the deltas are only for showing.
#[derive(Debug, Default)]
pub struct Assembler {
    text: String,
    reasoning: String,
    items: BTreeMap<u64, Value>,
    status: Option<String>,
    incomplete_reason: Option<String>,
    usage: Option<Usage>,
    request_id: Option<String>,
    pub done: bool,
}

impl Assembler {
    pub fn push(
        &mut self,
        event: &Value,
        on_delta: &mut impl FnMut(Delta<'_>),
    ) -> Result<(), WireError> {
        let kind = event["type"].as_str().unwrap_or_default();
        match kind {
            "response.created" | "response.in_progress" => self.take_id(&event["response"]),
            "response.output_text.delta" => {
                if let Some(t) = event["delta"].as_str() {
                    self.text.push_str(t);
                    on_delta(Delta::Text(t));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(t) = event["delta"].as_str() {
                    self.reasoning.push_str(t);
                    on_delta(Delta::Reasoning(t));
                }
            }
            "response.reasoning_summary_part.done" => on_delta(Delta::Reasoning("\n\n")),
            "response.function_call_arguments.delta" => {
                if let Some(t) = event["delta"].as_str() {
                    on_delta(Delta::ToolCall(t));
                }
            }
            "response.output_item.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .unwrap_or(self.items.len() as u64);
                self.items.insert(index, event["item"].clone());
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                let r = &event["response"];
                self.take_id(r);
                self.status = r["status"].as_str().map(str::to_string);
                self.incomplete_reason = r["incomplete_details"]["reason"]
                    .as_str()
                    .map(str::to_string);
                if let Some(u) = r.get("usage").filter(|u| u.is_object()) {
                    self.usage = Some(Usage {
                        input: u["input_tokens"].as_u64().unwrap_or(0),
                        output: u["output_tokens"].as_u64().unwrap_or(0),
                        cache_read: u["input_tokens_details"]["cached_tokens"]
                            .as_u64()
                            .unwrap_or(0),
                    });
                }
                // Items the stream finished before; a response that lists
                // its output only here still counts.
                if self.items.is_empty() {
                    for (i, item) in r["output"].as_array().into_iter().flatten().enumerate() {
                        self.items.insert(i as u64, item.clone());
                    }
                }
                self.done = true;
            }
            "response.failed" => return Err(failure(&event["response"]["error"])),
            "error" => {
                let e = event.get("error").unwrap_or(event);
                return Err(failure(e));
            }
            "" => {
                // The gateway's word that the upstream broke off mid-answer.
                if let Some(e) = event.get("error") {
                    if e["type"] == "stream_incomplete" {
                        return Err(WireError::Incomplete);
                    }
                    return Err(failure(e));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn take_id(&mut self, response: &Value) {
        if self.request_id.is_none() {
            self.request_id = response["id"].as_str().map(str::to_string);
        }
    }

    pub fn finish(self) -> Result<Completion, WireError> {
        if !self.done {
            return Err(WireError::Incomplete);
        }
        let mut blocks = Vec::new();
        let mut calls = false;
        for (_, item) in self.items {
            match item["type"].as_str().unwrap_or_default() {
                "message" => {
                    let text: String = item["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c["text"].as_str())
                        .collect();
                    if !text.trim().is_empty() {
                        blocks.push(Block::Text(text));
                    }
                }
                "function_call" => {
                    calls = true;
                    let arguments = item["arguments"].as_str().unwrap_or_default();
                    let input = if arguments.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str::<Value>(arguments)
                            .ok()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| Value::String(arguments.to_string()))
                    };
                    blocks.push(Block::ToolUse {
                        id: item["call_id"].as_str().unwrap_or_default().to_string(),
                        name: item["name"].as_str().unwrap_or_default().to_string(),
                        input,
                    });
                }
                "reasoning" => blocks.push(Block::Opaque(item)),
                _ => {}
            }
        }
        // Text streamed but never finished as an item is still what was said.
        if !blocks.iter().any(|b| matches!(b, Block::Text(_))) && !self.text.trim().is_empty() {
            blocks.push(Block::Text(self.text));
        }
        let finish_reason = match (self.status.as_deref(), self.incomplete_reason.as_deref()) {
            (Some("incomplete"), Some("max_output_tokens")) => "length",
            (Some("incomplete"), Some(other)) => other,
            _ if calls => "tool_calls",
            _ => "stop",
        };
        Ok(Completion {
            blocks,
            reasoning: self.reasoning,
            finish_reason: Some(finish_reason.to_string()),
            usage: self.usage,
            request_id: self.request_id,
            timings: None,
        })
    }
}

fn failure(error: &Value) -> WireError {
    if is_overflow(error) {
        return WireError::ContextOverflow(
            error["message"].as_str().unwrap_or_default().to_string(),
        );
    }
    WireError::Failed {
        code: error["code"]
            .as_str()
            .or_else(|| error["type"].as_str())
            .unwrap_or("unknown")
            .to_string(),
        message: error["message"]
            .as_str()
            .map_or_else(|| error.to_string(), str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assemble(events: &[Value]) -> Result<Completion, WireError> {
        let mut a = Assembler::default();
        for e in events {
            a.push(e, &mut |_| {})?;
        }
        a.finish()
    }

    fn reasoning_item() -> Value {
        json!({"id": "rs_1", "type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}],
            "encrypted_content": "gAAAA..."})
    }

    #[test]
    fn a_stream_assembles_in_output_order_with_reasoning_kept_opaque() {
        let mut shown = Vec::new();
        let mut a = Assembler::default();
        let events = [
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "response.reasoning_summary_text.delta", "delta": "thinking"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item()}),
            json!({"type": "response.output_text.delta", "delta": "Let me look."}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "Let me look."}]}}),
            json!({"type": "response.function_call_arguments.delta", "delta": "{\"path\""}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": {
                "id": "fc_1", "type": "function_call", "call_id": "call_a", "name": "read",
                "arguments": "{\"path\":\"x\"}"}}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed",
                "usage": {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 600},
                    "output_tokens": 50, "output_tokens_details": {"reasoning_tokens": 20}}}}),
        ];
        for e in &events {
            a.push(e, &mut |d| {
                shown.push(match d {
                    Delta::Text(t) => format!("text:{t}"),
                    Delta::Reasoning(t) => format!("thought:{t}"),
                    Delta::ToolCall(t) => format!("call:{t}"),
                })
            })
            .unwrap();
        }
        assert!(a.done);
        let c = a.finish().unwrap();
        assert_eq!(
            c.blocks,
            vec![
                Block::Opaque(reasoning_item()),
                Block::Text("Let me look.".into()),
                Block::ToolUse {
                    id: "call_a".into(),
                    name: "read".into(),
                    input: json!({"path": "x"})
                },
            ]
        );
        assert_eq!(
            shown,
            ["thought:thinking", "text:Let me look.", "call:{\"path\""]
        );
        assert_eq!(c.request_id.as_deref(), Some("resp_1"));
        assert_eq!(c.finish_reason.as_deref(), Some("tool_calls"));
        // Reasoning is inside the output count, as the gateway reads it too.
        assert_eq!(
            c.usage,
            Some(Usage {
                input: 1000,
                output: 50,
                cache_read: 600
            })
        );
    }

    #[test]
    fn the_input_replays_the_conversation_with_reasoning_in_place() {
        let mut conv = Conversation::new("be brief");
        conv.cache_key = Some("s1".into());
        conv.push(Role::User, vec![Block::Text("go".into())]);
        conv.push(
            Role::Assistant,
            vec![
                Block::Opaque(reasoning_item()),
                Block::ToolUse {
                    id: "call_a".into(),
                    name: "read".into(),
                    input: json!({"path": "x"}),
                },
            ],
        );
        conv.push(
            Role::User,
            vec![Block::ToolResult {
                id: "call_a".into(),
                content: "no such file".into(),
                is_error: true,
            }],
        );
        conv.push(
            Role::Assistant,
            vec![Block::Text("It is not there.".into())],
        );
        let client = Client::new(
            "http://gw/model/openai-codex/v1/",
            None,
            "gpt-6-luna",
            Sampling {
                reasoning_effort: Some("high".into()),
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(client.url, "http://gw/model/openai-codex/v1/responses");
        let tools =
            vec![json!({"name": "read", "description": "d", "parameters": {"type": "object"}})];
        let b = client.body(&conv, &tools);
        assert_eq!(b["instructions"], "be brief");
        assert_eq!(b["store"], false);
        assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(b["reasoning"], json!({"effort": "high", "summary": "auto"}));
        assert_eq!(b["prompt_cache_key"], "s1");
        assert_eq!(
            b["tools"][0],
            json!({"type": "function", "name": "read", "description": "d",
            "parameters": {"type": "object"}, "strict": false})
        );
        assert!(b.get("max_output_tokens").is_none());
        assert!(b.get("temperature").is_none());
        assert_eq!(
            b["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}],
                    "encrypted_content": "gAAAA..."},
                {"type": "function_call", "call_id": "call_a", "name": "read", "arguments": "{\"path\":\"x\"}"},
                {"type": "function_call_output", "call_id": "call_a", "output": "ERROR: no such file"},
                {"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "It is not there."}]},
            ])
        );
    }

    #[test]
    fn an_answer_cut_at_its_limit_reads_as_length() {
        let c = assemble(&[
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "call_id": "c", "name": "write", "arguments": "{\"path\":"}}),
            json!({"type": "response.incomplete", "response": {"id": "resp_2", "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"}}}),
        ])
        .unwrap();
        assert_eq!(c.finish_reason.as_deref(), Some("length"));
        assert_eq!(
            c.blocks[0],
            Block::ToolUse {
                id: "c".into(),
                name: "write".into(),
                input: json!("{\"path\":")
            }
        );
    }

    #[test]
    fn failures_overflow_and_cut_streams_are_told_apart() {
        let overflow = json!({"type": "response.failed", "response": {"id": "r", "status": "failed",
            "error": {"code": "context_length_exceeded",
                "message": "Your input exceeds the context window of this model."}}});
        assert!(matches!(
            assemble(&[overflow]),
            Err(WireError::ContextOverflow(_))
        ));
        let busy = json!({"type": "response.failed", "response": {"error": {"code": "server_is_overloaded", "message": "busy"}}});
        let e = assemble(&[busy]).unwrap_err();
        assert!(e.retryable(), "{e}");
        let refused = json!({"type": "error", "code": "invalid_prompt", "message": "no"});
        let e = assemble(&[refused]).unwrap_err();
        assert!(matches!(&e, WireError::Failed { code, .. } if code == "invalid_prompt"));
        assert!(!e.retryable());
        let cut = json!({"error": {"type": "stream_incomplete", "message": "upstream broke off"}});
        assert!(matches!(assemble(&[cut]), Err(WireError::Incomplete)));
        // No `response.completed`: the stream was cut.
        let partial = json!({"type": "response.output_text.delta", "delta": "hal"});
        assert!(matches!(assemble(&[partial]), Err(WireError::Incomplete)));
    }

    #[test]
    fn http_errors_carry_overflow_and_when_a_limit_lifts() {
        let body = r#"{"error":{"message":"Your input exceeds the context window of this model.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        assert!(matches!(
            error_from(400, body, None),
            WireError::ContextOverflow(_)
        ));
        let limit = r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":3600}}"#;
        let e = error_from(429, limit, None);
        assert_eq!(e.retry_after(), Some(std::time::Duration::from_secs(3600)));
        assert!(matches!(
            error_from(400, r#"{"detail":"Instructions are not valid"}"#, None),
            WireError::Http { status: 400, .. }
        ));
    }
}
