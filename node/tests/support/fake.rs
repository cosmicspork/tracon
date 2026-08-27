//! A harness that emits what the test asks it to, so the supervisor's own
//! behaviour is what is under test. Shared by the session and mesh tests.

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use tracon::adapter::{
    AdapterError, HarnessAdapter, HarnessEvent, HarnessHandle, HarnessVersion, LaunchSpec,
    ModelOption, TurnResult,
};
use tracon::runner::Runner;

/// A harness that emits what the test asks it to, so the supervisor's own
/// behaviour is what is under test.
pub struct FakeAdapter {
    pub tx: Arc<Mutex<Option<mpsc::Sender<HarnessEvent>>>>,
    /// Tokens the next turn reports; the budget accumulates these.
    pub tokens: Arc<Mutex<u64>>,
}

pub struct FakeHandle {
    pub prompts: Arc<Mutex<Vec<String>>>,
    pub tokens: Arc<Mutex<u64>>,
    pub killed: Arc<Mutex<bool>>,
}

#[async_trait]
impl HarnessHandle for FakeHandle {
    fn harness_session_id(&self) -> &str {
        "fake"
    }
    async fn prompt(&self, text: String) -> Result<TurnResult, AdapterError> {
        self.prompts.lock().await.push(text);
        let total = *self.tokens.lock().await;
        Ok(TurnResult {
            stop_reason: "end_turn".into(),
            usage: tracon::acp::types::Usage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: total,
                cached_read_tokens: 0,
            },
        })
    }
    async fn cancel(&self) -> Result<(), AdapterError> {
        Ok(())
    }
    async fn close(&self) -> Result<(), AdapterError> {
        *self.killed.lock().await = true;
        Ok(())
    }
}

#[async_trait]
impl HarnessAdapter for FakeAdapter {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn pinned_version(&self) -> &str {
        "1.0.0"
    }
    async fn version(&self, _r: &dyn Runner) -> Result<HarnessVersion, AdapterError> {
        Ok(HarnessVersion {
            found: "1.0.0".into(),
            pinned: "1.0.0".into(),
        })
    }
    async fn probe_models(&self, _r: &dyn Runner) -> Result<Vec<ModelOption>, AdapterError> {
        Ok(vec![ModelOption {
            value: "m/a".into(),
            name: "A".into(),
        }])
    }
    async fn launch(
        &self,
        _r: &dyn Runner,
        _spec: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError> {
        let (tx, rx) = mpsc::channel(64);
        *self.tx.lock().await = Some(tx);
        Ok((
            Box::new(FakeHandle {
                prompts: Arc::new(Mutex::new(Vec::new())),
                tokens: self.tokens.clone(),
                killed: Arc::new(Mutex::new(false)),
            }),
            rx,
        ))
    }
}
