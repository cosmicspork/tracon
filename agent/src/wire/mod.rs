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
