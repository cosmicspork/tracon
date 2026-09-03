//! A harness whose login prints a URL and then waits for one line on stdin;
//! the line becomes the access token it "stores". What the provider tests and
//! the mesh e2e drive instead of a real `omp auth-broker login`.

#![allow(dead_code)]

use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use tracon::{
    adapter::{
        AdapterError, HarnessAdapter, HarnessEvent, HarnessHandle, HarnessVersion, LaunchSpec,
        LiftedToken, LoginFlow, ModelOption,
    },
    runner::Runner,
};

#[derive(Default)]
pub struct LoginFake {
    pub stored: Arc<Mutex<Option<String>>>,
    pub refreshed: Arc<Mutex<u32>>,
    pub account_id: Arc<Mutex<Option<String>>>,
    pub login_delay_ms: AtomicU64,
    pub login_calls: AtomicUsize,
}

#[async_trait]
impl HarnessAdapter for LoginFake {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn pinned_version(&self) -> &str {
        "1"
    }
    async fn version(&self, _r: &dyn Runner) -> Result<HarnessVersion, AdapterError> {
        Ok(HarnessVersion {
            found: "1".into(),
            pinned: "1".into(),
        })
    }
    async fn probe_models(
        &self,
        _r: &dyn Runner,
        _env: Vec<(String, String)>,
    ) -> Result<Vec<ModelOption>, AdapterError> {
        Ok(vec![])
    }
    async fn launch(
        &self,
        _r: &dyn Runner,
        _s: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError> {
        Err(AdapterError::Protocol("no".into()))
    }
    async fn login(
        &self,
        _r: &dyn Runner,
        provider: &str,
        _name: &str,
    ) -> Result<LoginFlow, AdapterError> {
        self.login_calls.fetch_add(1, Ordering::SeqCst);
        let delay = self.login_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        let (client, server) = tokio::io::duplex(1024);
        let stored = self.stored.clone();
        let provider = provider.to_string();
        let url = format!("https://login.example/{provider}");
        let output = Arc::new(Mutex::new(String::new()));
        let done = Box::pin(async move {
            let mut lines = tokio::io::BufReader::new(server).lines();
            match lines.next_line().await {
                Ok(Some(code)) if !code.is_empty() => {
                    *stored.lock().unwrap() = Some(format!("{provider}:{code}"));
                    Ok(0)
                }
                _ => Ok(1),
            }
        });
        Ok(LoginFlow {
            url,
            stdin: Box::new(client),
            done,
            output,
        })
    }
    async fn refresh(
        &self,
        _r: &dyn Runner,
        _provider: &str,
        _name: &str,
    ) -> Result<(), AdapterError> {
        *self.refreshed.lock().unwrap() += 1;
        if let Some(s) = self.stored.lock().unwrap().as_mut() {
            s.push_str("+r");
        }
        Ok(())
    }
    async fn lift(&self, _dir: &Path, _provider: &str) -> Result<LiftedToken, AdapterError> {
        let access = self
            .stored
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AdapterError::Protocol("nothing stored".into()))?;
        Ok(LiftedToken {
            access,
            refresh: Some("rt".into()),
            expires_ms: Some(tracon::store::now_ms() + 2 * 3600 * 1000),
            identity: Some("op@example".into()),
            account_id: self.account_id.lock().unwrap().clone(),
        })
    }
}
