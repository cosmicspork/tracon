//! The vocabulary every adapter translates into: tool calls, permission
//! options, usage.
//!
//! These shapes were first read from the Agent Client Protocol, which the
//! retired omp harness spoke, and they outlived it because every harness has
//! the same things to say in its own spelling — the OpenCode adapter and the
//! Claude Code adapter each translate into this, and the supervisor, the MCP
//! layer and the store read only this. `tracon-agent` speaks it as its wire
//! protocol, so both directions serialize. Unknown fields
//! are kept in `extra` maps and unknown update variants land in
//! `SessionUpdate::Other`, so adapter drift shows up as data rather than as a
//! decode failure.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Extra = BTreeMap<String, Value>;

// ---- client → agent ------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FsCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

/// The revision of these shapes. Every one of them was read from protocol 1
/// of the wire format they came from, so 1 is both the floor and the ceiling:
/// an adapter whose harness answers with anything outside the range is
/// refused rather than driven on the chance that the shapes still line up.
pub const PROTOCOL_MIN: u32 = 1;
pub const PROTOCOL_MAX: u32 = 1;

impl InitializeParams {
    /// What the node declares: no client filesystem, no client terminal. The
    /// harness reads and runs inside its own runner.
    pub fn node() -> Self {
        Self {
            protocol_version: PROTOCOL_MAX,
            client_capabilities: ClientCapabilities {
                fs: FsCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<AgentInfo>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: String,
    pub mcp_servers: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    pub session_id: String,
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub current_value: Value,
    #[serde(default)]
    pub options: Vec<ConfigChoice>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigChoice {
    pub value: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionParams {
    pub session_id: String,
    pub config_id: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionResult {
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    pub stop_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cached_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl Usage {
    /// Tokens to charge the budget for this turn: fresh input, cache writes and
    /// output, the same quantity the gateway counts. A cache read is the model
    /// rereading context it already paid for, and a long session rereads its
    /// whole history every step, so charging it at the full rate made a budget
    /// mostly a measure of context length. It stays in `total_tokens` and
    /// `cached_read_tokens` for the record. A harness that reports only a total
    /// is charged that total less what it says it read, so the meter fails
    /// closed rather than reading zero.
    pub fn charged(&self) -> u64 {
        let parts = self.input_tokens + self.output_tokens + self.cache_write_tokens;
        if parts > 0 {
            parts
        } else {
            self.total_tokens.saturating_sub(self.cached_read_tokens)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdParams {
    pub session_id: String,
}

// ---- agent → client ------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateParams {
    pub session_id: String,
    pub update: SessionUpdate,
    /// Monotonic per session, from harnesses that number their updates
    /// (`tracon-agent`, which mirrors each to its `events.jsonl`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// `session/update` payloads. Decoded by the `sessionUpdate` discriminator;
/// anything not modelled here is kept whole in `Other`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    AgentMessageChunk(Chunk),
    AgentThoughtChunk(Chunk),
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    Plan(Plan),
    UsageUpdate(UsageUpdate),
    ConfigOptionUpdate(ConfigOptionUpdate),
    SessionInfoUpdate(Value),
    AvailableCommandsUpdate(Value),
    #[serde(untagged)]
    Other(Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chunk {
    pub content: ContentBlock,
    #[serde(default)]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub raw_input: Option<Value>,
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default)]
    pub locations: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub raw_output: Option<Value>,
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

impl ToolCallUpdate {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_deref(),
            Some("completed") | Some("failed") | Some("cancelled")
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub entries: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageUpdate {
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub used: Option<u64>,
    #[serde(default)]
    pub cost: Option<Cost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cost {
    pub amount: f64,
    #[serde(default)]
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdate {
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    pub session_id: String,
    pub tool_call: ToolCall,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
    /// The tool's own name, from harnesses that send it (`tracon-agent`).
    /// ACP's tool call carries only a display title and a coarse kind; the
    /// node maps the name and the input to what its policy reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestPermissionResult {
    pub outcome: PermissionOutcome,
    /// Why a call was refused, for harnesses that tell their model
    /// (`tracon-agent`): the node's policy speaks for itself rather than
    /// reading as the operator's choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    Selected {
        #[serde(rename = "optionId")]
        option_id: String,
    },
    Cancelled,
}

pub const OPTION_ALLOW_ONCE: &str = "allow_once";
pub const OPTION_REJECT_ONCE: &str = "reject_once";
/// Offered by the node, never by a harness: allow this, and what shares its
/// scope, for the rest of the session. The harness is answered `allow_once`.
pub const OPTION_ALLOW_SESSION: &str = "allow_session";

pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
    pub const SESSION_PROMPT: &str = "session/prompt";
    pub const SESSION_CANCEL: &str = "session/cancel";
    pub const SESSION_CLOSE: &str = "session/close";
    pub const SESSION_UPDATE: &str = "session/update";
    pub const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
}

pub fn model_choices(options: &[ConfigOption]) -> Option<&ConfigOption> {
    options
        .iter()
        .find(|o| o.id == "model" || o.category.as_deref() == Some("model"))
}

#[cfg(test)]
mod tests {
    use super::Usage;

    #[test]
    fn a_cache_read_is_never_charged() {
        // Parts reported: fresh input, cache writes and output; not the reads.
        let u = Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 100_015,
            cached_read_tokens: 100_000,
            cache_write_tokens: 0,
        };
        assert_eq!(u.charged(), 15);
        let u = Usage {
            cache_write_tokens: 1_200,
            ..u
        };
        assert_eq!(u.charged(), 1_215);

        // Only a total: charged the total less what was read, never zero.
        let u = Usage {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 900,
            cached_read_tokens: 600,
            cache_write_tokens: 0,
        };
        assert_eq!(u.charged(), 300);
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use serde_json::json;

    /// What the agent serializes is what the node decodes, `seq` included.
    #[test]
    fn updates_round_trip() {
        let params = SessionUpdateParams {
            session_id: "s".into(),
            update: SessionUpdate::ToolCall(ToolCall {
                tool_call_id: "c1".into(),
                title: "read a.py".into(),
                kind: Some("read".into()),
                status: Some("pending".into()),
                raw_input: Some(json!({"path": "a.py"})),
                content: Vec::new(),
                locations: vec![json!({"path": "a.py"})],
            }),
            seq: Some(7),
        };
        let wire = serde_json::to_value(&params).unwrap();
        assert_eq!(wire["update"]["sessionUpdate"], "tool_call");
        assert_eq!(wire["update"]["toolCallId"], "c1");
        assert_eq!(wire["seq"], 7);
        let back: SessionUpdateParams = serde_json::from_value(wire).unwrap();
        assert!(matches!(back.update, SessionUpdate::ToolCall(c) if c.title == "read a.py"));
        assert_eq!(back.seq, Some(7));

        let chunk = SessionUpdate::AgentMessageChunk(Chunk {
            content: ContentBlock::Text { text: "hi".into() },
            message_id: None,
        });
        let wire = serde_json::to_value(&chunk).unwrap();
        assert_eq!(
            wire,
            json!({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hi"}, "messageId": null})
        );
    }

    #[test]
    fn permission_answers_decode() {
        let r: RequestPermissionResult = serde_json::from_value(
            json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
        )
        .unwrap();
        assert_eq!(
            r.outcome,
            PermissionOutcome::Selected {
                option_id: OPTION_ALLOW_ONCE.into()
            }
        );
        let r: RequestPermissionResult =
            serde_json::from_value(json!({"outcome": {"outcome": "cancelled"}})).unwrap();
        assert_eq!(r.outcome, PermissionOutcome::Cancelled);
    }
}
