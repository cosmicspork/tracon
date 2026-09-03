//! The harness-agnostic seam. The trait has been here from the first commit
//! because adapters are the part that rots; what a harness is called, where it
//! keeps its state, and what it must find in that state directory all live
//! behind it rather than being spelled `omp` throughout the node.

pub mod claude;
pub mod omp;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::gateway::model::Wiring;
use crate::runner::Runner;

/// Where a harness keeps its state inside the runner, and what it calls that
/// place. The node mounts a directory it owns at `dir` under the harness's
/// home and sets `env` to the same path, so the mount and the harness's own
/// idea of its state directory cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Directory under the harness's home, including the leading dot.
    pub dir: &'static str,
    /// The environment variable that names it.
    pub env: &'static str,
}

/// The harness ids this node has an adapter for.
pub const KNOWN: &[&str] = &["omp", "claude"];

/// The layout for a harness id, for the few callers that have the config but
/// not the adapter (the boundary preflight). An unknown id gets omp's, which
/// only that preflight can reach: `adapter_for` refuses the id first, so no
/// session ever runs against a layout that is not its harness's.
pub fn layout(harness_id: &str) -> Layout {
    if harness_id == claude::ClaudeAdapter::ID {
        return claude::ClaudeAdapter::layout();
    }
    OMP_LAYOUT
}

const OMP_LAYOUT: Layout = Layout {
    dir: ".omp",
    env: "OMP_STATE_DIR",
};

/// The adapter for the configured harness. An unknown id fails here, at
/// startup, rather than silently running whichever harness happens to be the
/// default — a node that thinks it is running something else is worse than a
/// node that will not start.
pub fn adapter_for(cfg: &Config) -> Result<Arc<dyn HarnessAdapter>, AdapterError> {
    match cfg.harness.id.as_str() {
        omp::OmpAdapter::ID => Ok(Arc::new(omp::OmpAdapter::new(cfg.harness.version.clone()))),
        claude::ClaudeAdapter::ID => Ok(Arc::new(claude::ClaudeAdapter::new(
            cfg.harness.version.clone(),
        ))),
        other => Err(AdapterError::Protocol(format!(
            "no adapter for harness `{other}`; this node knows {}",
            KNOWN.join(", ")
        ))),
    }
}

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
    /// Device authorization code, when the provider uses a browser-independent flow.
    pub device_code: Option<String>,
    pub stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    pub done: futures_core::future::BoxFuture<'static, Result<i32, crate::runner::RunnerError>>,
    /// Everything the login printed, for the reason when it fails.
    pub output: std::sync::Arc<std::sync::Mutex<String>>,
}

/// What a login left in the harness's store, as the broker keeps it.
#[derive(Clone, Default, PartialEq)]
pub struct LiftedToken {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_ms: Option<i64>,
    pub identity: Option<String>,
    pub account_id: Option<String>,
}

impl std::fmt::Debug for LiftedToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiftedToken")
            .field("access", &"<redacted>")
            .field("refresh", &self.refresh.as_ref().map(|_| "<redacted>"))
            .field("expires_ms", &self.expires_ms)
            .field("identity", &self.identity)
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[async_trait]
pub trait HarnessAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn pinned_version(&self) -> &str;

    /// Where this harness keeps its state, and what it calls that place.
    fn layout(&self) -> Layout {
        layout(self.id())
    }

    /// Files the harness must find in its state directory, as paths relative
    /// to that directory and their contents. This is where a harness's own
    /// configuration lives: the node materializes it per session rather than
    /// writing into the operator's real one.
    fn scratch_files(&self, _wiring: &Wiring) -> Vec<(String, String)> {
        Vec::new()
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_harness_picks_the_adapter() {
        let mut cfg = Config::default();
        cfg.harness.id = "omp".into();
        let Ok(a) = adapter_for(&cfg) else {
            panic!("omp has an adapter")
        };
        assert_eq!(a.id(), "omp");
        assert_eq!(a.layout().dir, ".omp");
        assert_eq!(a.layout().env, "OMP_STATE_DIR");
    }

    /// Falling back to omp would run a harness the operator did not ask for,
    /// under a version pin that does not describe it. Refusing at startup is
    /// the only safe answer.
    #[test]
    fn an_unknown_harness_is_refused_rather_than_guessed_at() {
        let mut cfg = Config::default();
        cfg.harness.id = "opencode".into();
        let err = match adapter_for(&cfg) {
            Ok(_) => panic!("an unknown harness must not resolve"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("opencode"), "{err}");
        assert!(err.contains("omp"), "{err}");
    }

    #[test]
    fn lifted_token_debug_redacts_secrets() {
        let token = LiftedToken {
            access: "access-secret".into(),
            refresh: Some("refresh-secret".into()),
            account_id: Some("account-secret".into()),
            ..Default::default()
        };
        let debug = format!("{token:?}");
        for secret in ["access-secret", "refresh-secret", "account-secret"] {
            assert!(!debug.contains(secret), "{debug}");
        }
    }
}
