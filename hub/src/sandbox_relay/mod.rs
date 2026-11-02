//! The sandbox relay: a TLS listener the hub does not terminate. It reads a
//! connection's ClientHello only far enough to learn the server name, maps
//! `<label>.<domain>` to the node that registered that label, and splices the
//! raw bytes, ClientHello included, onto a WebSocket that node dialled out to
//! the hub for this one connection. The hub holds no certificate or key for
//! the relayed TLS, so it cannot read, alter or answer what passes; the client
//! pins the node's certificate end to end. See `proto::relay` for the node's
//! half of the exchange.
//!
//! Only a node that holds an authenticated `@mesh` membership can register,
//! and only under the label derived from its own id. Anything else — a name
//! nobody serves, no ClientHello, a node over its stream limit, a node that
//! does not dial back in time — is closed without a byte written.

pub mod sni;

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    Extension, Json,
};
use futures_util::{SinkExt, StreamExt};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

use crate::auth::Owner;
use crate::store::{MemberRole, MemberStore};
use crate::AppState;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    /// The suffix every relayed name ends in: a sandbox reaches a node at
    /// `<label>.<domain>`, with a wildcard DNS record pointing at the hub.
    pub domain: String,
    /// Relayed connections one node may have open or waiting at once.
    pub max_streams_per_node: usize,
    /// Accepted connections, across every node, still sending their
    /// ClientHello or waiting for their node to dial back.
    pub max_handshakes: usize,
    /// How long a connection has to send a complete ClientHello.
    pub hello_timeout: Duration,
    /// How long a node has to dial back for a connection it was offered.
    pub dial_back_timeout: Duration,
}

impl RelayConfig {
    pub fn new(domain: &str) -> Self {
        Self {
            domain: domain.trim_matches('.').to_ascii_lowercase(),
            max_streams_per_node: 16,
            max_handshakes: 256,
            hello_timeout: Duration::from_secs(10),
            dial_back_timeout: Duration::from_secs(15),
        }
    }
}

/// Offers waiting for a node's listen stream to carry them.
const OFFER_BUFFER: usize = 64;
/// One read from the client and one WebSocket message toward the node.
const CHUNK: usize = 64 * 1024;

struct Registration {
    node: [u8; 32],
    generation: u64,
    offers: mpsc::Sender<String>,
}

struct Pending {
    node: [u8; 32],
    socket: oneshot::Sender<WebSocket>,
}

pub struct SandboxRelay {
    cfg: RelayConfig,
    members: Arc<dyn MemberStore>,
    /// Label → the node's live listen stream.
    nodes: Mutex<HashMap<String, Registration>>,
    /// Label → its stream budget. Kept across a node's reconnects so streams
    /// still open from a previous listen count against the new one.
    budgets: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// Offer id → the connection waiting for the node to dial back.
    pending: Mutex<HashMap<String, Pending>>,
    handshakes: Arc<Semaphore>,
    generation: AtomicU64,
}

/// A node's registration, held by its listen stream; dropping it
/// unregisters unless a newer listen has replaced it.
pub struct Listening {
    relay: Arc<SandboxRelay>,
    label: String,
    generation: u64,
}

impl Drop for Listening {
    fn drop(&mut self) {
        let mut nodes = self.relay.nodes.lock().unwrap();
        if nodes
            .get(&self.label)
            .is_some_and(|r| r.generation == self.generation)
        {
            nodes.remove(&self.label);
        }
    }
}

impl SandboxRelay {
    pub fn new(cfg: RelayConfig, members: Arc<dyn MemberStore>) -> Arc<Self> {
        Arc::new(Self {
            handshakes: Arc::new(Semaphore::new(cfg.max_handshakes)),
            cfg,
            members,
            nodes: Mutex::new(HashMap::new()),
            budgets: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
        })
    }

    pub fn domain(&self) -> &str {
        &self.cfg.domain
    }

    /// Nodes with a live listen stream.
    pub fn registered(&self) -> usize {
        self.nodes.lock().unwrap().len()
    }

    /// Register `node` under its own label, replacing (and so ending) any
    /// listen stream it already had. Returns the host it serves and the
    /// offers for it.
    pub fn register(
        self: &Arc<Self>,
        node: [u8; 32],
    ) -> Option<(String, mpsc::Receiver<String>, Listening)> {
        let label = proto::relay::sandbox_label(&hex::encode(node))?;
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(OFFER_BUFFER);
        self.nodes.lock().unwrap().insert(
            label.clone(),
            Registration {
                node,
                generation,
                offers: tx,
            },
        );
        let host = format!("{label}.{}", self.cfg.domain);
        Some((
            host,
            rx,
            Listening {
                relay: self.clone(),
                label,
                generation,
            },
        ))
    }

    /// The connection waiting under `id`, if it was offered to `node`.
    fn take_pending(&self, id: &str, node: &[u8; 32]) -> Option<oneshot::Sender<WebSocket>> {
        let mut pending = self.pending.lock().unwrap();
        if pending.get(id).is_some_and(|p| &p.node == node) {
            pending.remove(id).map(|p| p.socket)
        } else {
            None
        }
    }

    /// The node serving `host`, if it still holds a listen stream and is
    /// still a node member of this hub.
    fn route(&self, host: &str) -> Option<([u8; 32], String, mpsc::Sender<String>)> {
        let label = host.strip_suffix(&format!(".{}", self.cfg.domain))?;
        if label.contains('.') {
            return None;
        }
        let (node, offers) = {
            let nodes = self.nodes.lock().unwrap();
            let r = nodes.get(label)?;
            (r.node, r.offers.clone())
        };
        let member = self.members.get(&hex::encode(node)).ok().flatten();
        if !member.is_some_and(|m| m.role == MemberRole::Node) {
            self.nodes.lock().unwrap().remove(label);
            return None;
        }
        Some((node, label.to_string(), offers))
    }

    fn budget(&self, label: &str) -> Arc<Semaphore> {
        self.budgets
            .lock()
            .unwrap()
            .entry(label.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.cfg.max_streams_per_node)))
            .clone()
    }

    /// Accept relayed connections until the listener fails.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((conn, peer)) => {
                    let relay = self.clone();
                    tokio::spawn(async move {
                        if let Err(why) = relay.handle(conn).await {
                            tracing::debug!(%peer, why, "sandbox relay connection closed unanswered");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sandbox relay accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    async fn handle(self: Arc<Self>, mut conn: TcpStream) -> Result<(), &'static str> {
        let handshake = self
            .handshakes
            .clone()
            .try_acquire_owned()
            .map_err(|_| "too many connections in handshake")?;
        let (head, host) = tokio::time::timeout(self.cfg.hello_timeout, read_hello(&mut conn))
            .await
            .map_err(|_| "no ClientHello in time")??;
        let (node, label, offers) = self.route(&host).ok_or("no node serves that name")?;
        let stream: OwnedSemaphorePermit = self
            .budget(&label)
            .try_acquire_owned()
            .map_err(|_| "node is at its stream limit")?;

        let mut raw = [0u8; 16];
        OsRng.fill_bytes(&mut raw);
        let id = hex::encode(raw);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), Pending { node, socket: tx });
        if offers.try_send(id.clone()).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err("node's listen stream is gone or full");
        }
        let socket = match tokio::time::timeout(self.cfg.dial_back_timeout, rx).await {
            Ok(Ok(socket)) => socket,
            _ => {
                self.pending.lock().unwrap().remove(&id);
                return Err("node did not dial back");
            }
        };
        drop(handshake);
        tracing::debug!(node = %label, "sandbox relay stream open");
        splice(conn, head, socket).await;
        drop(stream);
        Ok(())
    }
}

/// Read until the bytes hold a whole ClientHello. Returns every byte read,
/// to forward unchanged, and the server name.
async fn read_hello(conn: &mut TcpStream) -> Result<(Vec<u8>, String), &'static str> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    loop {
        let n = conn.read(&mut chunk).await.map_err(|_| "read failed")?;
        if n == 0 {
            return Err("closed before a ClientHello");
        }
        buf.extend_from_slice(&chunk[..n]);
        match sni::server_name(&buf) {
            sni::Hello::Incomplete => continue,
            sni::Hello::Sni(host) => return Ok((buf, host)),
            sni::Hello::Refused(why) => return Err(why),
        }
    }
}

/// Copy bytes both ways until both directions end or either side aborts.
/// Nothing here looks at what it copies.
async fn splice(conn: TcpStream, head: Vec<u8>, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (mut rd, mut wr) = conn.into_split();
    let up = async {
        ws_tx
            .send(Message::Binary(Bytes::from(head)))
            .await
            .map_err(|_| ())?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = rd.read(&mut buf).await.map_err(|_| ())?;
            // Empty is end of stream this way: the node half-closes its side.
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

// ------------------------------------------------------------------ routes

fn err(status: StatusCode, msg: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": msg})))
}

/// `GET /v0/relay/listen`: a node serves the relay for as long as it holds
/// this open. The first event names the host it is reached at; each `open`
/// after it is a connection to dial back for.
pub async fn listen(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<Value>)> {
    let relay = s
        .relay
        .as_ref()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "this hub has no sandbox relay"))?;
    let me = crate::routes::member_of(&s, &owner)?;
    if me.role != MemberRole::Node {
        return Err(err(
            StatusCode::FORBIDDEN,
            "only a node may serve the relay",
        ));
    }
    let (host, offers, listening) = relay
        .register(owner.0)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "not a node id"))?;
    tracing::info!(%host, "node serving the sandbox relay");
    let ready = Event::default()
        .event(proto::relay::READY_EVENT)
        .data(json!({ "host": host }).to_string());
    let opens = ReceiverStream::new(offers).map(move |id| {
        let _held = &listening;
        Ok(Event::default()
            .event(proto::relay::OPEN_EVENT)
            .data(json!({ "id": id }).to_string()))
    });
    let stream = tokio_stream::once(Ok(ready)).chain(opens);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(25))))
}

/// `GET /v0/relay/streams/{id}`: the node takes one offered connection as a
/// WebSocket. Only the node it was offered to may take it, and only once.
pub async fn stream(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(relay) = s.relay.as_ref() else {
        return err(StatusCode::NOT_FOUND, "this hub has no sandbox relay").into_response();
    };
    let Some(waiting) = relay.take_pending(&id, &owner.0) else {
        return err(StatusCode::NOT_FOUND, "no such relayed connection").into_response();
    };
    ws.on_upgrade(move |socket| async move {
        let _ = waiting.send(socket);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Member, MemoryMembers};

    fn relay_with(nodes: &[[u8; 32]]) -> Arc<SandboxRelay> {
        let members = Arc::new(MemoryMembers::new());
        for n in nodes {
            members
                .put(&Member {
                    node_id: hex::encode(n),
                    x25519_pub: String::new(),
                    binding_sig: String::new(),
                    name: "n".into(),
                    channels: vec![proto::frame::MESH_CHANNEL.into()],
                    admitted_ms: 0,
                    admitted_by: "test".into(),
                    role: MemberRole::Node,
                })
                .unwrap();
        }
        SandboxRelay::new(RelayConfig::new("Relay.Example."), members)
    }

    #[test]
    fn a_connection_is_taken_once_and_only_by_the_node_it_was_offered_to() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let relay = relay_with(&[a, b]);
        let (tx, _rx) = oneshot::channel();
        relay.pending.lock().unwrap().insert(
            "id".into(),
            Pending {
                node: a,
                socket: tx,
            },
        );
        assert!(relay.take_pending("id", &b).is_none());
        assert!(relay.take_pending("id", &a).is_some());
        assert!(relay.take_pending("id", &a).is_none());
    }

    #[test]
    fn names_route_to_the_registered_member_and_nothing_else() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let relay = relay_with(&[a]);
        let (host, _offers, listening) = relay.register(a).unwrap();
        assert_eq!(host, format!("{}.relay.example", "01".repeat(16)));
        assert_eq!(relay.route(&host).map(|r| r.0), Some(a));
        assert!(relay.route(&format!("x.{host}")).is_none());
        assert!(relay
            .route(&host.replace("relay.example", "other.example"))
            .is_none());
        // Registered but not a member: never a target, and dropped.
        let (b_host, _b_offers, _b) = relay.register(b).unwrap();
        assert!(relay.route(&b_host).is_none());
        assert_eq!(relay.registered(), 1);

        // A reconnect replaces the registration; the old one's drop leaves it.
        let (_, _offers2, again) = relay.register(a).unwrap();
        drop(listening);
        assert!(relay.route(&host).is_some());
        drop(again);
        assert!(relay.route(&host).is_none());
    }
}
