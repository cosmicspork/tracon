//! Wire encoders: the neutral conversation to a provider's API and back.
//!
//! Two: OpenAI Chat Completions, which llama.cpp and `api.openai.com` speak,
//! and the Responses API, which is all the Codex backend serves. The loop sees
//! neither, only [`Client`], [`Completion`] and [`Delta`].

pub mod openai_chat;
pub mod openai_responses;

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::conversation::{Block, Conversation};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub min_p: Option<f64>,
    /// Qwen3's switch, passed to the chat template.
    pub no_think: bool,
    /// A reasoning model's effort (`low`, `medium`, `high`, …); the
    /// upstream's default when absent. Responses only.
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub blocks: Vec<Block>,
    pub reasoning: String,
    /// `stop`, `tool_calls`, or `length` when the answer hit its output
    /// limit; the Responses encoder maps its status onto these.
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

/// The model client, whichever wire its provider speaks.
pub enum Client {
    Chat(openai_chat::Client),
    Responses(openai_responses::Client),
}

impl Client {
    /// `max_tokens` bounds the answer where the wire allows it.
    pub async fn complete(
        &self,
        conv: &Conversation,
        tools: &[Value],
        max_tokens: Option<u32>,
        on_delta: impl FnMut(Delta<'_>),
    ) -> Result<Completion, WireError> {
        match self {
            Self::Chat(c) => c.complete(conv, tools, max_tokens, on_delta).await,
            Self::Responses(c) => c.complete(conv, tools, on_delta).await,
        }
    }
}

/// `schema` in the strict form OpenAI's function calling enforces: every
/// property required, those that were optional allowed `null`, and no
/// properties but those named. `None` for a schema using anything this does
/// not know how to carry over, which is then sent as it is, not strict.
pub fn strict_schema(schema: &Value) -> Option<Value> {
    const LEAF: &[&str] = &["type", "description", "enum"];
    let o = schema.as_object()?;
    match o.get("type").and_then(Value::as_str)? {
        "object" => {
            if o.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "type" | "description" | "properties" | "required"
                )
            }) {
                return None;
            }
            let properties = o.get("properties")?.as_object()?;
            let required: Vec<&str> = o
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut out = serde_json::Map::new();
            for (name, p) in properties {
                let mut p = strict_schema(p)?;
                if !required.contains(&name.as_str()) {
                    nullable(&mut p)?;
                }
                out.insert(name.clone(), p);
            }
            let mut s = serde_json::Map::new();
            s.insert("type".into(), "object".into());
            if let Some(d) = o.get("description") {
                s.insert("description".into(), d.clone());
            }
            s.insert(
                "required".into(),
                Value::Array(out.keys().cloned().map(Value::String).collect()),
            );
            s.insert("properties".into(), Value::Object(out));
            s.insert("additionalProperties".into(), false.into());
            Some(Value::Object(s))
        }
        "array" => {
            if o.keys()
                .any(|k| !matches!(k.as_str(), "type" | "description" | "items"))
            {
                return None;
            }
            let mut s = o.clone();
            s.insert("items".into(), strict_schema(o.get("items")?)?);
            Some(Value::Object(s))
        }
        "string" | "integer" | "number" | "boolean" => o
            .keys()
            .all(|k| LEAF.contains(&k.as_str()))
            .then(|| schema.clone()),
        _ => None,
    }
}

/// Lets a strict property be `null`, which is how a strict call leaves an
/// optional one out.
fn nullable(p: &mut Value) -> Option<()> {
    let o = p.as_object_mut()?;
    let ty = o.get("type")?.as_str()?.to_string();
    o.insert("type".into(), serde_json::json!([ty, "null"]));
    if let Some(Value::Array(values)) = o.get_mut("enum") {
        values.push(Value::Null);
    }
    Some(())
}

pub(crate) fn http() -> Result<reqwest::Client, WireError> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // Prompt processing on a local model can take minutes before the
        // first byte; a read that stalls this long is a hung server.
        .read_timeout(Duration::from_secs(600))
        .build()?)
}

/// The upstream's `Retry-After`, in seconds. An HTTP-date is rare from model
/// APIs, and the backoff covers it.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The prompt no longer fits the model's context. The loop compacts the
    /// conversation and asks once more; never truncate.
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),
    #[error("upstream answered {status}: {body}")]
    Http {
        status: u16,
        body: String,
        /// The upstream's `Retry-After`, in seconds.
        retry_after: Option<u64>,
    },
    #[error("upstream reported an error mid-stream: {0}")]
    Stream(String),
    /// A Responses stream that ended in `response.failed`, with the code the
    /// upstream gave.
    #[error("upstream failed the response ({code}): {message}")]
    Failed { code: String, message: String },
    #[error("the stream ended before the response was complete")]
    Incomplete,
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
}

impl WireError {
    /// Whether asking again could get a different answer: the upstream was
    /// busy, briefly broken, or the connection or stream was lost. A refusal
    /// of the request itself never is.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Http { status, .. } => matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529),
            Self::Failed { code, .. } => matches!(
                code.as_str(),
                "server_error" | "server_is_overloaded" | "rate_limit_exceeded"
            ),
            Self::Incomplete | Self::Transport(_) => true,
            Self::ContextOverflow(_) | Self::Stream(_) => false,
        }
    }

    /// How long the upstream asked to be left alone, if it said.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Http {
                retry_after: Some(s),
                ..
            } => Some(Duration::from_secs(*s)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Registry;
    use serde_json::json;

    /// Every object, however deep, names all its properties as required and
    /// allows no others.
    fn closed(schema: &Value) -> bool {
        match schema["type"].as_str() {
            Some("object") => {
                let props = schema["properties"].as_object().unwrap();
                let required: Vec<_> = schema["required"].as_array().unwrap().iter().collect();
                schema["additionalProperties"] == false
                    && props.len() == required.len()
                    && props.values().all(closed)
            }
            Some("array") => closed(&schema["items"]),
            _ => true,
        }
    }

    #[test]
    fn every_built_in_tool_has_a_strict_form() {
        for spec in Registry::default().specs() {
            let strict = strict_schema(&spec["parameters"])
                .unwrap_or_else(|| panic!("{} has no strict form", spec["name"]));
            assert!(closed(&strict), "{strict:#}");
        }
        let edit = Registry::default().specs()[1]["parameters"].clone();
        let strict = strict_schema(&edit).unwrap();
        let item = &strict["properties"]["edits"]["items"];
        // Optional before, nullable now; required stays as it was.
        assert_eq!(item["properties"]["end"]["type"], json!(["string", "null"]));
        assert_eq!(item["properties"]["op"]["type"], "string");
        assert!(!item["properties"]["op"]["enum"]
            .as_array()
            .unwrap()
            .contains(&Value::Null));
    }

    #[test]
    fn a_schema_it_cannot_carry_over_is_left_alone() {
        for schema in [
            json!({"type": "object"}),
            json!({"type": "object", "properties": {"a": {"$ref": "#/defs/a"}}}),
            json!({"type": "object", "properties": {"a": {"type": "string"}}, "additionalProperties": true}),
            json!({"type": "object", "properties": {"a": {"oneOf": [{"type": "string"}]}}}),
            json!({"type": "object", "properties": {"a": {"type": ["string", "null"]}}}),
        ] {
            assert_eq!(strict_schema(&schema), None, "{schema}");
        }
        let opt_enum = json!({"type": "object", "properties": {"mode": {"type": "string", "enum": ["a", "b"]}}});
        assert_eq!(
            strict_schema(&opt_enum).unwrap()["properties"]["mode"]["enum"],
            json!(["a", "b", null])
        );
    }
}
