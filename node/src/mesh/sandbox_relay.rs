//! The node's half of the hub's sandbox relay (`proto::relay`): reached by a
//! sandbox that can only dial a public address, from behind NAT. While
//! `[mesh.sandbox_relay]` is on, the node holds a signed listen stream to the
//! hub; for each connection the hub announces, it opens the local sandbox
//! listener and dials the hub back for that connection as a WebSocket, then
//! copies bytes between the two. The bytes are the sandbox's TLS to this
//! node's own listener: neither this bridge nor the hub terminates it.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use proto::auth::signed_headers;
use proto::keys::Identity;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{watch, Semaphore};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_tungstenite::WebSocketStream;

use crate::config::SandboxRelay as RelayConfig;

const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// One read from the local listener and one WebSocket message to the hub.
const CHUNK: usize = 64 * 1024;
/// RFC 6455's fixed GUID for `Sec-WebSocket-Accept`.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

static PUBLISHED: std::sync::OnceLock<watch::Receiver<Option<String>>> = std::sync::OnceLock::new();

/// The hub relay's name for this node while the node holds its listen
/// stream; `None` with no relay, or while it is not held.
pub fn current_host() -> Option<String> {
    PUBLISHED.get().and_then(|host| host.borrow().clone())
}

pub struct SandboxRelay {
    identity: Arc<Identity>,
    hub_url: String,
    target: String,
    http: reqwest::Client,
    streams: Arc<Semaphore>,
    host: watch::Sender<Option<String>>,
}

impl SandboxRelay {
    pub fn new(identity: Arc<Identity>, hub_url: &str, cfg: &RelayConfig) -> Arc<Self> {
        Arc::new(Self {
            identity,
            hub_url: hub_url.trim_end_matches('/').to_string(),
            target: cfg.target.clone(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("http client"),
            streams: Arc::new(Semaphore::new(cfg.max_streams.max(1))),
            host: watch::channel(None).0,
        })
    }

    /// The name a sandbox puts in SNI to reach this node, once the hub has
    /// accepted the listen stream; `None` while it is not held.
    pub fn host(&self) -> watch::Receiver<Option<String>> {
        self.host.subscribe()
    }

    /// Make this relay's name the one [`current_host`] reads: the node's
    /// sandbox providers tell their sandboxes it.
    pub fn publish(&self) {
        let _ = PUBLISHED.set(self.host());
    }

    /// Hold the listen stream for as long as the node runs, reconnecting
    /// with backoff.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let relay = self.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match relay.listen_once().await {
                    Ok(()) => backoff = Duration::from_secs(1),
                    Err(e) => {
                        tracing::debug!(error = %e, "sandbox relay listen stream ended");
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                }
                relay.host.send_replace(None);
                tokio::time::sleep(backoff).await;
            }
        })
    }

    fn signed(&self, path: &str) -> reqwest::RequestBuilder {
        let ts = (crate::store::now_ms() / 1000).max(0) as u64;
        let mut req = self.http.get(format!("{}{}", self.hub_url, path));
        for (k, v) in signed_headers(&self.identity, "GET", path, b"", ts) {
            req = req.header(k, v);
        }
        req
    }

    async fn listen_once(self: &Arc<Self>) -> Result<(), String> {
        let res = self
            .signed(proto::relay::LISTEN_PATH)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            tracing::warn!(%status, %body, "hub refused the sandbox relay listen stream");
            return Err(format!("{status}: {body}"));
        }
        let mut stream = res.bytes_stream();
        let mut buf = String::new();
        let mut event = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(i) = buf.find('\n') {
                let line = buf[..i].trim_end_matches('\r').to_string();
                buf.drain(..=i);
                if let Some(name) = line.strip_prefix("event:") {
                    event = name.trim().to_string();
                } else if let Some(data) = line.strip_prefix("data:") {
                    self.on_event(&event, data.trim());
                } else if line.is_empty() {
                    event.clear();
                }
            }
        }
        Ok(())
    }

    fn on_event(self: &Arc<Self>, event: &str, data: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            return;
        };
        match event {
            proto::relay::READY_EVENT => {
                if let Some(host) = v["host"].as_str() {
                    tracing::info!(%host, "serving the hub's sandbox relay");
                    self.host.send_replace(Some(host.to_string()));
                }
            }
            proto::relay::OPEN_EVENT => {
                // An id is ours to dial for only as the hub names it; anything
                // else would put an arbitrary path in a signed request.
                if let Some(id) = v["id"]
                    .as_str()
                    .filter(|id| id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
                {
                    let relay = self.clone();
                    let id = id.to_string();
                    tokio::spawn(async move {
                        if let Err(e) = relay.take(&id).await {
                            tracing::debug!(error = %e, "relayed connection not bridged");
                        }
                    });
                }
            }
            _ => {}
        }
    }

    /// Bridge one relayed connection. An offer this node does not take times
    /// out at the hub, which closes the client unanswered.
    async fn take(&self, id: &str) -> Result<(), String> {
        let _permit = self
            .streams
            .clone()
            .try_acquire_owned()
            .map_err(|_| "at the relayed stream limit".to_string())?;
        let local = TcpStream::connect(&self.target)
            .await
            .map_err(|e| format!("local sandbox listener {}: {e}", self.target))?;
        let ws = self.dial(id).await?;
        splice(local, ws).await;
        Ok(())
    }

    async fn dial(&self, id: &str) -> Result<WebSocketStream<reqwest::Upgraded>, String> {
        let mut key = [0u8; 16];
        rand::fill(&mut key);
        let key = base64::engine::general_purpose::STANDARD.encode(key);
        let res = self
            .signed(&proto::relay::stream_path(id))
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", &key)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
            return Err(format!("hub answered the dial-back with {}", res.status()));
        }
        let expected = {
            let mut h = Sha1::new();
            h.update(key.as_bytes());
            h.update(WS_GUID.as_bytes());
            base64::engine::general_purpose::STANDARD.encode(h.finalize())
        };
        let accept = res
            .headers()
            .get("sec-websocket-accept")
            .and_then(|v| v.to_str().ok());
        if accept != Some(expected.as_str()) {
            return Err("hub's WebSocket accept does not match the key".into());
        }
        let upgraded = res.upgrade().await.map_err(|e| e.to_string())?;
        Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
    }
}

/// Copy bytes both ways until both directions end or either side aborts.
async fn splice(local: TcpStream, ws: WebSocketStream<reqwest::Upgraded>) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (mut rd, mut wr) = local.into_split();
    let up = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = rd.read(&mut buf).await.map_err(|_| ())?;
            // Empty is end of stream this way (`proto::relay`).
            let msg = Bytes::copy_from_slice(&buf[..n]);
            ws_tx.send(Message::Binary(msg)).await.map_err(|_| ())?;
            if n == 0 {
                return Ok::<(), ()>(());
            }
        }
    };
    let down = async {
        while let Some(msg) = ws_rx.next().await {
            match msg.map_err(|_| ())? {
                Message::Binary(b) if b.is_empty() => {
                    let _ = wr.shutdown().await;
                    return Ok(());
                }
                Message::Binary(b) => wr.write_all(&b).await.map_err(|_| ())?,
                Message::Close(_) => return Err(()),
                _ => {}
            }
        }
        Err(())
    };
    if tokio::try_join!(up, down).is_ok() {
        let _ = ws_tx.send(Message::Close(None)).await;
    }
}
