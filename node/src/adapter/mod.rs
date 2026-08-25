//! The harness-agnostic seam. One implementation (`omp`) exists; the trait is
//! here from the first commit because adapters are the part that rots.

pub mod omp;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::runner::Runner;

#[derive(Debug, Clone)]
pub struct HarnessVersion {
    pub found: String,
    pub pinned: String,
}

impl HarnessVersion {
    pub fn matches(&self) -> bool {
        self.found == self.pinned
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelOption {
    pub value: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool_call_id: Option<String>,
    pub title: String,
    pub kind: Option<String>,
    pub raw_input: Option<Value>,
    pub options: Vec<crate::acp::types::PermissionOption>,
}

/// The operator's answer to a permission request, or the request being withdrawn.
#[derive(Debug)]
pub enum PermissionReply {
    Selected(String),
    Cancelled,
}

#[derive(Debug)]
pub enum HarnessEvent {
    MessageChunk {
        message_id: Option<String>,
        text: String,
    },
    ThoughtChunk {
        message_id: Option<String>,
        text: String,
    },
    ToolCall(crate::acp::types::ToolCall),
    ToolCallUpdate(crate::acp::types::ToolCallUpdate),
    Plan(Value),
    Usage {
        size: Option<u64>,
        used: Option<u64>,
        cost_usd: Option<f64>,
    },
    Permission {
        request: PermissionRequest,
        reply: oneshot::Sender<PermissionReply>,
    },
    Models(Vec<ModelOption>),
    Other(Value),
    Exited {
        code: Option<i32>,
    },
}

#[derive(Debug, Clone)]
pub struct TurnResult {
    pub stop_reason: String,
    pub usage: crate::acp::types::Usage,
}

pub struct LaunchSpec {
    pub cwd_in_runner: String,
    pub model: String,
    pub container_name: String,
    /// MCP servers offered to the harness at session start. The node's own
    /// tools reach it this way and no other.
    pub mcp_servers: Vec<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("runner: {0}")]
    Runner(#[from] crate::runner::RunnerError),
    #[error("rpc: {0}")]
    Rpc(#[from] crate::acp::rpc::RpcClientError),
    #[error("harness has no stdio pipe")]
    NoPipe,
    #[error("version mismatch: found {found}, pinned {pinned}")]
    VersionMismatch { found: String, pinned: String },
    #[error("model {0:?} is not offered by the harness")]
    UnknownModel(String),
    #[error("{0}")]
    Protocol(String),
}

#[async_trait]
pub trait HarnessAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn pinned_version(&self) -> &str;
    async fn version(&self, runner: &dyn Runner) -> Result<HarnessVersion, AdapterError>;
    async fn probe_models(&self, runner: &dyn Runner) -> Result<Vec<ModelOption>, AdapterError>;
    async fn launch(
        &self,
        runner: &dyn Runner,
        spec: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError>;
}

#[async_trait]
pub trait HarnessHandle: Send + Sync {
    fn harness_session_id(&self) -> &str;
    async fn prompt(&self, text: String) -> Result<TurnResult, AdapterError>;
    async fn cancel(&self) -> Result<(), AdapterError>;
    async fn close(&self) -> Result<(), AdapterError>;
}
