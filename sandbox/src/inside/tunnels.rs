//! Listeners inside the sandbox whose connections the node takes, one
//! `tunnel/accept` request each, and joins to an endpoint of its own.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use crate::carrier::BoxedIo;
use crate::protocol::ListenAddr;

/// Connections accepted and not yet taken by the node wait here.
const BACKLOG: usize = 64;

struct Tunnel {
    queue: Arc<AsyncMutex<mpsc::Receiver<BoxedIo>>>,
    acceptor: JoinHandle<()>,
    socket: Option<PathBuf>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.acceptor.abort();
        if let Some(path) = &self.socket {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Default)]
pub struct Tunnels {
    open: Mutex<HashMap<String, Tunnel>>,
}

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("tunnel {0} is already listening")]
    Exists(String),
    #[error("no tunnel {0}")]
    NotFound(String),
    #[error("tunnel {0} is closed")]
    Closed(String),
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl Tunnels {
    pub async fn listen(&self, name: &str, listen: &ListenAddr) -> Result<ListenAddr, TunnelError> {
        if self.open.lock().unwrap().contains_key(name) {
            return Err(TunnelError::Exists(name.into()));
        }
        let (sender, receiver) = mpsc::channel::<BoxedIo>(BACKLOG);
        let (acceptor, bound, socket) = match listen {
            ListenAddr::Tcp(addr) => {
                let listener = TcpListener::bind(addr).await?;
                let bound = ListenAddr::Tcp(listener.local_addr()?.to_string());
                let task = tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        let _ = stream.set_nodelay(true);
                        if sender.send(Box::new(stream)).await.is_err() {
                            break;
                        }
                    }
                });
                (task, bound, None)
            }
            ListenAddr::Unix(path) => {
                let listener = UnixListener::bind(path)?;
                let task = tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        if sender.send(Box::new(stream)).await.is_err() {
                            break;
                        }
                    }
                });
                (task, ListenAddr::Unix(path.clone()), Some(path.clone()))
            }
        };
        let tunnel = Tunnel {
            queue: Arc::new(AsyncMutex::new(receiver)),
            acceptor,
            socket,
        };
        let mut open = self.open.lock().unwrap();
        if open.contains_key(name) {
            // Lost a race with another listen of the same name; dropping
            // this one closes its listener.
            return Err(TunnelError::Exists(name.into()));
        }
        open.insert(name.to_string(), tunnel);
        Ok(bound)
    }

    /// The next connection to `name`'s listener.
    pub async fn accept(&self, name: &str) -> Result<BoxedIo, TunnelError> {
        let queue = self
            .open
            .lock()
            .unwrap()
            .get(name)
            .map(|t| t.queue.clone())
            .ok_or_else(|| TunnelError::NotFound(name.into()))?;
        let mut queue = queue.lock().await;
        queue
            .recv()
            .await
            .ok_or_else(|| TunnelError::Closed(name.into()))
    }

    pub fn close(&self, name: &str) -> Result<(), TunnelError> {
        self.open
            .lock()
            .unwrap()
            .remove(name)
            .map(drop)
            .ok_or_else(|| TunnelError::NotFound(name.into()))
    }

    pub fn close_all(&self) {
        self.open.lock().unwrap().clear();
    }
}
