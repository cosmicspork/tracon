//! `tracon-agent`: the harness tracon owns, for models that are not Claude.
//!
//! The node supervises and does not reason; this binary is the reasoning half,
//! and runs inside the boundary in the same trust position as `claude`. It
//! holds no credential, catalogue or settings store.
//!
//! The model loop, two wire encoders (OpenAI Chat Completions for llama.cpp,
//! the Responses API for the Codex backend) and six in-boundary tools, driven by
//! the node over ACP or run `--standalone`. See `agent/README.md`.

pub mod agent;
pub mod compact;
pub mod conversation;
pub mod events;
pub mod hashline;
pub mod journal;
pub mod prompt;
pub mod providers;
pub mod server;
pub mod sse;
pub mod subagent;
pub mod tools;
pub mod wire;
