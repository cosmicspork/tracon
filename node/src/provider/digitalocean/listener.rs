//! The node's listener for sandboxes that dial it over a network: one TCP
//! listener for every run, each connection handed to the run whose token it
//! presented.
//!
//! It sits on a public address (directly, or behind the hub relay), so
//! most of what arrives is not a sandbox: one trial saw sixty scanner
//! handshakes in three hours. A connection becomes anything only after TLS
//! and a token this node issued and nobody has used; until then it has no
//! channel and costs one handshake, bounded in time by the server's
//! handshake timeout and in number by [`HANDSHAKES`]. A connection that
//! proves a token for a run nobody is waiting on any more is closed. A
//! sandbox back after a dropped connection proves its run's resume secret
//! instead, and carries that run on.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;
use tokio::sync::{oneshot, Semaphore};
use tracon_sandbox::node::{Accepted, SandboxConn, SandboxServer};
use tracon_sandbox::tls::CertPin;

/// Handshakes in flight at once; a connection past this is closed unread.
pub const HANDSHAKES: usize = 64;

pub struct DoListener {
    server: Arc<SandboxServer>,
    addr: SocketAddr,
    waiting: Mutex<HashMap<String, oneshot::Sender<SandboxConn>>>,
    refused: AtomicU64,
    accepted: AtomicU64,
    resumed: AtomicU64,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DoListener {
    /// Listen on `addr` for sandboxes proving tokens `server` issued.
    pub async fn bind(addr: SocketAddr, server: Arc<SandboxServer>) -> std::io::Result<Arc<Self>> {
        let tcp = TcpListener::bind(addr).await?;
        let listener = Arc::new(DoListener {
            server,
            addr: tcp.local_addr()?,
            waiting: Mutex::new(HashMap::new()),
            refused: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            resumed: AtomicU64::new(0),
            task: Mutex::new(None),
        });
        let task = tokio::spawn(accept_loop(Arc::downgrade(&listener), tcp));
        *listener.task.lock().unwrap() = Some(task);
        Ok(listener)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn pin(&self) -> CertPin {
        self.server.pin()
    }

    /// Connections closed before or at the handshake: no TLS, a bad or
    /// spent token, too many at once, or a run nobody waits on.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }

    /// Connections that took a run back after its connection dropped.
    pub fn resumed(&self) -> u64 {
        self.resumed.load(Ordering::Relaxed)
    }

    /// Wait for `run`'s sandbox: said before the sandbox is told where to
    /// dial, so its connection cannot arrive before anyone waits for it.
    pub fn expect(&self, run: &str) -> oneshot::Receiver<SandboxConn> {
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(run.to_string(), tx);
        rx
    }

    /// Stop waiting for `run`.
    pub fn forget(&self, run: &str) {
        self.waiting.lock().unwrap().remove(run);
    }

    fn deliver(&self, conn: SandboxConn) {
        let waiter = self.waiting.lock().unwrap().remove(conn.run());
        match waiter.map(|tx| tx.send(conn)) {
            Some(Ok(())) => {
                self.accepted.fetch_add(1, Ordering::Relaxed);
            }
            // Nobody waits (any more): dropping the last handle closes it.
            Some(Err(_)) | None => {
                tracing::debug!("a sandbox connected for a run nobody waits on; closed");
                self.refused.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

async fn accept_loop(this: std::sync::Weak<DoListener>, tcp: TcpListener) {
    {
        let slots = Arc::new(Semaphore::new(HANDSHAKES));
        loop {
            let (stream, peer) = match tcp.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::debug!(%error, "sandbox listener accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            let Some(listener) = this.upgrade() else {
                return;
            };
            let Ok(slot) = slots.clone().try_acquire_owned() else {
                listener.refused.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let _ = stream.set_nodelay(true);
            tokio::spawn(async move {
                let _slot = slot;
                match listener.server.accept_any(stream).await {
                    Ok(Accepted::New(conn)) => listener.deliver(conn),
                    // A sandbox back after a dropped connection: the run's
                    // own handle carries on over it, so nobody waits for it.
                    Ok(Accepted::Resumed(conn)) => {
                        listener.resumed.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(%peer, run = %conn.run(), "a sandbox came back");
                    }
                    Err(error) => {
                        listener.refused.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(%peer, %error, "closed a connection to the sandbox listener");
                    }
                }
            });
        }
    }
}

impl Drop for DoListener {
    fn drop(&mut self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
}
