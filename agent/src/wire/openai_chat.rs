//! OpenAI Chat Completions, streamed.
//!
//! llama.cpp needs `--jinja` for tool calls to be parsed out of the model's
//! text into `tool_calls`; its built-in `--tools` must stay off, since the
//! tools here are the only ones the model may call.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{json, Map, Value};

use super::{Usage, WireError};
use crate::conversation::{Block, Conversation, Role};
use crate::sse::{Decoder, Frame};

#[derive(Debug, Clone, Default)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub min_p: Option<f64>,
    /// Qwen3's switch, passed to the chat template.
    pub no_think: bool,
}

pub struct Client {
    http: reqwest::Client,
    url: String,
    api_key: Option<String>,
    model: String,
    sampling: Sampling,
    parallel_tools: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub blocks: Vec<Block>,
    pub reasoning: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    pub request_id: Option<String>,
    /// llama.cpp's own timings (`predicted_per_second` and the rest), as sent.
    pub timings: Option<Value>,
}

pub enum Delta<'a> {
    Text(&'a str),
    Reasoning(&'a str),
    /// A fragment of a tool call's arguments.
    ToolCall(&'a str),
}

impl Client {
    pub fn new(
        base_url: &str,
        api_key: Option<String>,
        model: &str,
        sampling: Sampling,
        parallel_tools: bool,
    ) -> Result<Self, WireError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Prompt processing on a local model can take minutes before the
            // first byte; a read that stalls this long is a hung server.
            .read_timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self {
            http,
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            api_key,
            model: model.to_string(),
            sampling,
            parallel_tools,
        })
    }

    /// `max_tokens` bounds the answer; `None` leaves it to the server.
    pub fn body(&self, conv: &Conversation, tools: &[Value], max_tokens: Option<u32>) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": messages(conv),
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let o = body.as_object_mut().expect("object");
        if !tools.is_empty() {
            let tools: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t["name"],
                            "description": t["description"],
                            "parameters": t["parameters"],
                            // The schemas are not written for OpenAI's strict
                            // subset (optional fields, open objects).
                            "strict": false,
                        }
                    })
                })
                .collect();
            o.insert("tools".into(), Value::Array(tools));
            o.insert("tool_choice".into(), json!("auto"));
            o.insert("parallel_tool_calls".into(), json!(self.parallel_tools));
        }
        if let Some(n) = max_tokens {
            o.insert("max_tokens".into(), json!(n));
        }
        if let Some(t) = self.sampling.temperature {
            o.insert("temperature".into(), json!(t));
        }
        if let Some(p) = self.sampling.min_p {
            o.insert("min_p".into(), json!(p));
        }
        if self.sampling.no_think {
            o.insert(
                "chat_template_kwargs".into(),
                json!({"enable_thinking": false}),
            );
        }
        body
    }

    pub async fn complete(
        &self,
        conv: &Conversation,
        tools: &[Value],
        max_tokens: Option<u32>,
        mut on_delta: impl FnMut(Delta<'_>),
    ) -> Result<Completion, WireError> {
        let mut request = self
            .http
            .post(&self.url)
            .json(&self.body(conv, tools, max_tokens));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            // Seconds only: an HTTP-date is rare from model APIs, and the
            // backoff covers it.
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok());
            let body = response.text().await.unwrap_or_default();
            return Err(error_from(status.as_u16(), &body, retry_after));
        }
        let mut decoder = Decoder::default();
        let mut assembler = Assembler::default();
        let mut stream = response.bytes_stream();
        'read: while let Some(chunk) = stream.next().await {
            for frame in decoder.push(&chunk?) {
                match frame {
                    Frame::Done => {
                        assembler.done = true;
                        break 'read;
                    }
                    Frame::Data(data) => {
                        let Ok(value) = serde_json::from_str::<Value>(&data) else {
                            continue;
                        };
                        assembler.push(&value, &mut on_delta)?;
                    }
                }
            }
        }
        assembler.finish()
    }
}

fn error_from(status: u16, body: &str, retry_after: Option<u64>) -> WireError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error = parsed.as_ref().map(|v| &v["error"]);
    if let Some(e) = error {
        if e["type"] == "exceed_context_size_error" || e["code"] == "context_length_exceeded" {
            return WireError::ContextOverflow(e["message"].as_str().unwrap_or(body).to_string());
        }
    }
    WireError::Http {
        status,
        body: body.chars().take(2000).collect(),
        retry_after,
    }
}

fn messages(conv: &Conversation) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": conv.system})];
    for m in &conv.messages {
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for b in &m.blocks {
            match b {
                Block::Text(t) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
                Block::ToolUse { id, name, input } => calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": match input {
                            Value::String(raw) => raw.clone(),
                            other => other.to_string(),
                        },
                    }
                })),
                Block::ToolResult {
                    id,
                    content,
                    is_error,
                } => results.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": if *is_error { format!("ERROR: {content}") } else { content.clone() },
                })),
                // Chat Completions has nowhere to carry provider state.
                Block::Opaque(_) => {}
            }
        }
        match m.role {
            Role::Assistant => {
                let mut msg = Map::new();
                msg.insert("role".into(), json!("assistant"));
                msg.insert(
                    "content".into(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    },
                );
                if !calls.is_empty() {
                    msg.insert("tool_calls".into(), Value::Array(calls));
                }
                out.push(Value::Object(msg));
            }
            Role::User => {
                // Tool results answer the assistant turn just before, so they
                // come before anything else the user adds.
                out.extend(results);
                if !text.is_empty() {
                    out.push(json!({"role": "user", "content": text}));
                }
            }
        }
    }
    out
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Builds a completion from stream chunks. Tool calls arrive as fragments keyed
/// by `index`: the id and name once, the arguments in pieces.
#[derive(Debug, Default)]
pub struct Assembler {
    text: String,
    reasoning: String,
    calls: BTreeMap<u64, PartialCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    request_id: Option<String>,
    timings: Option<Value>,
    pub done: bool,
}

impl Assembler {
    pub fn push(
        &mut self,
        chunk: &Value,
        on_delta: &mut impl FnMut(Delta<'_>),
    ) -> Result<(), WireError> {
        if let Some(e) = chunk.get("error") {
            if e["type"] == "exceed_context_size_error" {
                return Err(WireError::ContextOverflow(
                    e["message"].as_str().unwrap_or_default().to_string(),
                ));
            }
            // The gateway's word that the upstream broke off mid-answer: the
            // same as the stream simply ending, said out loud.
            if e["type"] == "stream_incomplete" {
                return Err(WireError::Incomplete);
            }
            return Err(WireError::Stream(e.to_string()));
        }
        if self.request_id.is_none() {
            self.request_id = chunk["id"].as_str().map(str::to_string);
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(Usage {
                input: u["prompt_tokens"].as_u64().unwrap_or(0),
                output: u["completion_tokens"].as_u64().unwrap_or(0),
                cache_read: u["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0),
            });
        }
        if let Some(t) = chunk.get("timings") {
            self.timings = Some(t.clone());
        }
        let Some(choice) = chunk["choices"].get(0) else {
            return Ok(());
        };
        if let Some(r) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(r.to_string());
        }
        let delta = &choice["delta"];
        if let Some(t) = delta["content"].as_str() {
            self.text.push_str(t);
            on_delta(Delta::Text(t));
        }
        if let Some(t) = delta["reasoning_content"].as_str() {
            self.reasoning.push_str(t);
            on_delta(Delta::Reasoning(t));
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"]
                .as_u64()
                .unwrap_or_else(|| self.calls.keys().last().copied().unwrap_or(0));
            let partial = self.calls.entry(index).or_default();
            if let Some(id) = call["id"].as_str().filter(|s| !s.is_empty()) {
                if partial.id.is_empty() {
                    partial.id = id.to_string();
                }
            }
            if let Some(name) = call["function"]["name"].as_str() {
                if partial.name.is_empty() {
                    partial.name = name.to_string();
                }
            }
            if let Some(args) = call["function"]["arguments"].as_str() {
                partial.arguments.push_str(args);
                on_delta(Delta::ToolCall(args));
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Completion, WireError> {
        if !self.done && self.finish_reason.is_none() {
            return Err(WireError::Incomplete);
        }
        let mut blocks = Vec::new();
        if !self.text.trim().is_empty() {
            blocks.push(Block::Text(self.text));
        }
        for (i, (_, call)) in self.calls.into_iter().enumerate() {
            let input = if call.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(&call.arguments)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or(Value::String(call.arguments))
            };
            blocks.push(Block::ToolUse {
                id: if call.id.is_empty() {
                    format!("call_{i}")
                } else {
                    call.id
                },
                name: call.name,
                input,
            });
        }
        Ok(Completion {
            blocks,
            reasoning: self.reasoning,
            finish_reason: self.finish_reason,
            usage: self.usage,
            request_id: self.request_id,
            timings: self.timings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assemble(chunks: &[Value]) -> Result<Completion, WireError> {
        let mut a = Assembler::default();
        for c in chunks {
            a.push(c, &mut |_| {})?;
        }
        a.done = true;
        a.finish()
    }

    #[test]
    fn tool_call_fragments_assemble_by_index() {
        let c = assemble(&[
            json!({"id": "r1", "choices": [{"delta": {"content": "Let me look."}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "a", "function": {"name": "read", "arguments": "{\"pa"}}
            ]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 1, "id": "b", "function": {"name": "glob", "arguments": ""}},
                {"index": 0, "function": {"arguments": "th\": \"x\"}"}}
            ]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 1, "function": {"arguments": "{\"pattern\":\"*\"}"}}
            ]}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 4}}, "timings": {"predicted_per_second": 16.0}}),
        ])
        .unwrap();
        assert_eq!(
            c.blocks,
            vec![
                Block::Text("Let me look.".into()),
                Block::ToolUse {
                    id: "a".into(),
                    name: "read".into(),
                    input: json!({"path": "x"})
                },
                Block::ToolUse {
                    id: "b".into(),
                    name: "glob".into(),
                    input: json!({"pattern": "*"})
                },
            ]
        );
        assert_eq!(c.request_id.as_deref(), Some("r1"));
        assert_eq!(
            c.usage,
            Some(Usage {
                input: 10,
                output: 5,
                cache_read: 4
            })
        );
        assert_eq!(c.timings.unwrap()["predicted_per_second"], 16.0);
    }

    #[test]
    fn bad_arguments_are_kept_raw() {
        let c = assemble(&[json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "a", "function": {"name": "read", "arguments": "{path: x"}}
        ]}, "finish_reason": "tool_calls"}]})])
        .unwrap();
        assert_eq!(
            c.blocks[0],
            Block::ToolUse {
                id: "a".into(),
                name: "read".into(),
                input: json!("{path: x")
            }
        );
    }

    #[test]
    fn a_cut_stream_is_incomplete() {
        let mut a = Assembler::default();
        a.push(
            &json!({"choices": [{"delta": {"content": "hal"}}]}),
            &mut |_| {},
        )
        .unwrap();
        assert!(matches!(a.finish(), Err(WireError::Incomplete)));
    }

    #[test]
    fn context_overflow_is_recognised() {
        let body = r#"{"error":{"code":400,"message":"request (40000 tokens) exceeds the available context size (32768 tokens)","type":"exceed_context_size_error","n_prompt_tokens":40000,"n_ctx":32768}}"#;
        assert!(
            matches!(error_from(400, body, None), WireError::ContextOverflow(m) if m.contains("40000"))
        );
        assert!(matches!(
            error_from(400, "nope", None),
            WireError::Http { status: 400, .. }
        ));
        let mut a = Assembler::default();
        let mid = json!({"error": {"type": "exceed_context_size_error", "message": "full"}});
        assert!(matches!(
            a.push(&mid, &mut |_| {}),
            Err(WireError::ContextOverflow(_))
        ));
    }

    #[test]
    fn messages_put_tool_results_first_and_mark_errors() {
        let mut conv = Conversation::new("sys");
        conv.push(Role::User, vec![Block::Text("go".into())]);
        conv.push(
            Role::Assistant,
            vec![Block::ToolUse {
                id: "a".into(),
                name: "read".into(),
                input: json!({"path": "x"}),
            }],
        );
        conv.push(
            Role::User,
            vec![Block::ToolResult {
                id: "a".into(),
                content: "no such file".into(),
                is_error: true,
            }],
        );
        let m = messages(&conv);
        assert_eq!(m[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(m[1], json!({"role": "user", "content": "go"}));
        assert_eq!(m[2]["content"], Value::Null);
        assert_eq!(
            m[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"x\"}"
        );
        assert_eq!(
            m[3],
            json!({"role": "tool", "tool_call_id": "a", "content": "ERROR: no such file"})
        );
    }

    #[test]
    fn body_carries_tools_and_sampling() {
        let client = Client::new(
            "http://x/v1/",
            None,
            "m",
            Sampling {
                temperature: Some(0.6),
                min_p: Some(0.01),
                no_think: true,
            },
            false,
        )
        .unwrap();
        assert_eq!(client.url, "http://x/v1/chat/completions");
        let tools =
            vec![json!({"name": "read", "description": "d", "parameters": {"type": "object"}})];
        let b = client.body(&Conversation::new("s"), &tools, None);
        assert_eq!(b["tools"][0]["function"]["name"], "read");
        assert_eq!(b["parallel_tool_calls"], false);
        assert_eq!(b["min_p"], 0.01);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(b["stream_options"]["include_usage"], true);
    }
}
