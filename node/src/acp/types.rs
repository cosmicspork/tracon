//! The subset of ACP the node uses, as captured from `omp acp` 18.0.4.
//! Unknown fields are kept in `extra` maps and unknown update variants land in
//! `SessionUpdate::Other`, so adapter drift shows up as data rather than as a
//! decode failure.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Extra = BTreeMap<String, Value>;

// ---- client → agent ------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FsCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

impl InitializeParams {
    /// What the node declares: no client filesystem, no client terminal. The
    /// harness reads and runs inside its own runner.
    pub fn node() -> Self {
        Self {
            protocol_version: 1,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub agent_info: Option<AgentInfo>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: String,
    pub mcp_servers: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionParams {
    pub session_id: String,
    pub config_id: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionResult {
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    pub stop_reason: String,
    #[serde(default)]
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
}

impl Usage {
    /// Tokens to charge the budget for this turn. `totalTokens` is what the
    /// harness reports and is preferred, but it is `#[serde(default)]`: a harness
    /// that reports the parts and omits the total would otherwise charge zero and
    /// never hit the budget. Fall back to the sum so the meter fails closed.
    pub fn charged(&self) -> u64 {
        self.total_tokens
            .max(self.input_tokens + self.output_tokens + self.cached_read_tokens)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdParams {
    pub session_id: String,
}

// ---- agent → client ------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateParams {
    pub session_id: String,
    pub update: SessionUpdate,
}

/// `session/update` payloads. Decoded by the `sessionUpdate` discriminator;
/// anything not modelled here is kept whole in `Other`.
#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdate {
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    pub session_id: String,
    pub tool_call: ToolCall,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
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

#[derive(Debug, Clone, Serialize)]
pub struct RequestPermissionResult {
    pub outcome: PermissionOutcome,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
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
    fn charged_prefers_total_but_falls_back_to_the_sum() {
        // A harness reporting the total is charged the total.
        let u = Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 100,
            cached_read_tokens: 2,
        };
        assert_eq!(u.charged(), 100);

        // A harness that omits `totalTokens` (it is `#[serde(default)]`) is
        // charged the parts, not zero, so the budget still bites.
        let u = Usage {
            input_tokens: 40,
            output_tokens: 30,
            total_tokens: 0,
            cached_read_tokens: 5,
        };
        assert_eq!(u.charged(), 75);
    }
}
