//! The neutral conversation every wire encoder reads and writes.
//!
//! Encoders translate to and from their wire; the loop only ever sees these
//! blocks. `Opaque` carries provider state that must round-trip untouched
//! (OpenAI encrypted reasoning, later Anthropic signed thinking) and is never
//! inspected here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct Conversation {
    pub system: String,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Block {
    Text(String),
    /// `input` is the parsed arguments, or the raw string when the model's
    /// arguments were not JSON: kept so the turn replays exactly as sent.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    Opaque(Value),
}

impl Conversation {
    pub fn new(system: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            messages: Vec::new(),
        }
    }

    pub fn push(&mut self, role: Role, blocks: Vec<Block>) {
        self.messages.push(Message { role, blocks });
    }
}
