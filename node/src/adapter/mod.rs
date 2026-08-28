//! The harness-agnostic seam. One implementation (`omp`) exists; the trait is
//! here from the first commit because adapters are the part that rots.

pub mod omp;

use std::path::Path;

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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// Built-in tools the harness may use. Empty leaves its default set.
    pub tools: Vec<String>,
    /// Environment for the harness process: the gateway wiring.
    pub env: Vec<(String, String)>,
    /// A file inside the runner appended to the harness's system prompt: the
    /// session's orientation.
    pub system_prompt_file: Option<String>,
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

/// A provider login the harness is running: the URL for the operator, the
/// subprocess's stdin for the paste-back, and its exit.
pub struct LoginFlow {
    pub url: String,
    pub stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    pub done: futures_core::future::BoxFuture<'static, Result<i32, crate::runner::RunnerError>>,
    /// Everything the login printed, for the reason when it fails.
    pub output: std::sync::Arc<std::sync::Mutex<String>>,
}

/// What a login left in the harness's store, as the broker keeps it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiftedToken {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_ms: Option<i64>,
    pub identity: Option<String>,
}

#[async_trait]
pub trait HarnessAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn pinned_version(&self) -> &str;
    async fn version(&self, runner: &dyn Runner) -> Result<HarnessVersion, AdapterError>;
    /// List the models the harness offers, wired to the gateway by `env`.
    async fn probe_models(
        &self,
        runner: &dyn Runner,
        env: Vec<(String, String)>,
    ) -> Result<Vec<ModelOption>, AdapterError>;
    async fn launch(
        &self,
        runner: &dyn Runner,
        spec: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError>;

    /// Run the harness's own login for `provider` inside the runner, against
    /// the store the runner mounts. Returns once the URL is known.
    async fn login(
        &self,
        _runner: &dyn Runner,
        _provider: &str,
        _name: &str,
    ) -> Result<LoginFlow, AdapterError> {
        Err(AdapterError::Protocol(
            "this harness has no login flow".into(),
        ))
    }

    /// Refresh the stored token for `provider` in place.
    async fn refresh(
        &self,
        _runner: &dyn Runner,
        _provider: &str,
        _name: &str,
    ) -> Result<(), AdapterError> {
        Err(AdapterError::Protocol("this harness has no refresh".into()))
    }

    /// Read the token a login or refresh left in `store_dir`.
    async fn lift(&self, _store_dir: &Path, _provider: &str) -> Result<LiftedToken, AdapterError> {
        Err(AdapterError::Protocol("this harness has no lift".into()))
    }
}

#[async_trait]
pub trait HarnessHandle: Send + Sync {
    fn harness_session_id(&self) -> &str;
    async fn prompt(&self, text: String) -> Result<TurnResult, AdapterError>;
    async fn cancel(&self) -> Result<(), AdapterError>;
    async fn close(&self) -> Result<(), AdapterError>;
}
