//! Wire encoders: the neutral conversation to a provider's API and back.
//!
//! Phase 0 has one, OpenAI Chat Completions, which llama.cpp and
//! `api.openai.com` both speak. The Responses encoder for the Codex backend
//! arrives in Phase 4.

pub mod openai_chat;

use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The prompt no longer fits the model's context. Compaction answers this
    /// from Phase 2; until then the run stops and says so. Never truncate.
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),
    #[error("upstream answered {status}: {body}")]
    Http { status: u16, body: String },
    #[error("upstream reported an error mid-stream: {0}")]
    Stream(String),
    #[error("the stream ended before the response was complete")]
    Incomplete,
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
}
