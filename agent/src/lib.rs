//! `tracon-agent`: the harness tracon owns, for models that are not Claude.
//!
//! The node supervises and does not reason; this binary is the reasoning half,
//! and runs inside the boundary in the same trust position as `claude` or
//! `opencode`. It holds no credential, catalogue or settings store.
//!
//! Phase 0 is a standalone spike: the model loop, one wire encoder (OpenAI
//! Chat Completions, which llama.cpp speaks) and six in-boundary tools, with no
//! node and no permission gate. See `plan-tracon-native-harness`.

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
