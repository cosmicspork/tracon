//! Wire encoders: the neutral conversation to a provider's API and back.
//!
//! Phase 0 has one, OpenAI Chat Completions, which llama.cpp and
//! `api.openai.com` both speak. The Responses encoder for the Codex backend
//! arrives in Phase 4.

pub mod openai_chat;

use std::time::Duration;

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
    Http {
        status: u16,
        body: String,
        /// The upstream's `Retry-After`, in seconds.
        retry_after: Option<u64>,
    },
    #[error("upstream reported an error mid-stream: {0}")]
    Stream(String),
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
