//! The sandbox relay: TLS the hub does not terminate. A sandbox reaches it two
//! ways: a TCP connection to the relay listener (`TRACON_HUB_RELAY_ADDR`), or
//! a WebSocket to [`proto::relay::CONNECT_PATH`] on the hub's own URL, which
//! works behind a proxy that terminates TLS and passes only HTTP; that
//! carries the same bytes in binary messages ([`ws_bytes`]). Either way the
//! relay reads the ClientHello only far enough to learn the server name, maps
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
//! does not dial back in time — is closed without a byte written (on the
//! WebSocket carrier, nothing but a close).

pub mod sni;
pub mod ws_bytes;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    Extension, Json,
};
use futures_util::{SinkExt, StreamExt};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
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
    /// Of those, how many one client (one address) may hold, so a few
    /// idle connections cannot take every slot from real sandboxes.
    pub max_handshakes_per_client: usize,
    /// How long a TCP connection has to send a complete ClientHello.
    pub hello_timeout: Duration,
    /// How long a WebSocket has: a sandbox sends its ClientHello as soon as
    /// the upgrade completes, so anything slower is not one.
    pub ws_hello_timeout: Duration,
    /// How long a node has to dial back for a connection it was offered.
    pub dial_back_timeout: Duration,
}

impl RelayConfig {
    pub fn new(domain: &str) -> Self {
        Self {
            domain: domain.trim_matches('.').to_ascii_lowercase(),
            max_streams_per_node: 16,
            max_handshakes: 256,
            max_handshakes_per_client: 8,
            hello_timeout: Duration::from_secs(10),
            ws_hello_timeout: Duration::from_secs(3),
            dial_back_timeout: Duration::from_secs(15),
        }
    }
}

/// Offers waiting for a node's listen stream to carry them.
const OFFER_BUFFER: usize = 64;
/// One read from the client and one WebSocket message toward the node.
const CHUNK: usize = 64 * 1024;
/// The largest message a sandbox's WebSocket may send: twice what a sandbox
/// sends, and the bound on what an unauthenticated one can make the hub
/// buffer in one message before its ClientHello is read.
const MAX_CLIENT_MESSAGE: usize = 2 * proto::relay::MAX_MESSAGE;

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
    /// Client → its connections in handshake.
    in_handshake: Mutex<HashMap<String, usize>>,
    generation: AtomicU64,
}

/// One client's place in handshake; dropping it gives the place back.
struct ClientSlot {
    relay: Arc<SandboxRelay>,
    client: String,
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        let mut held = self.relay.in_handshake.lock().unwrap();
        if let Some(n) = held.get_mut(&self.client) {
            *n -= 1;
            if *n == 0 {
                held.remove(&self.client);
            }
        }
    }
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
            in_handshake: Mutex::new(HashMap::new()),
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

    /// A place in handshake for `client`, unless it holds its share.
    fn client_slot(self: &Arc<Self>, client: &str) -> Option<ClientSlot> {
        let mut held = self.in_handshake.lock().unwrap();
        let n = held.entry(client.to_string()).or_insert(0);
        if *n >= self.cfg.max_handshakes_per_client {
            return None;
        }
        *n += 1;
        Some(ClientSlot {
            relay: self.clone(),
            client: client.to_string(),
        })
    }

    /// Connections `client` has in handshake now.
    pub fn in_handshake(&self, client: &str) -> usize {
        self.in_handshake
            .lock()
            .unwrap()
            .get(client)
            .copied()
            .unwrap_or(0)
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
                Ok((mut conn, peer)) => {
                    let relay = self.clone();
                    tokio::spawn(async move {
                        // No per-client share here: behind a load balancer
                        // every connection would come from one address.
                        let within = relay.cfg.hello_timeout;
                        if let Err(why) = relay.handle(&mut conn, None, within).await {
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

    /// Route one sandbox connection, whichever carrier brought it, giving
    /// it `hello_within` to send its ClientHello, and splice it to its node
    /// until it ends. With a `client`, it counts against that client's
    /// share of handshakes. An `Err` is a connection refused before any byte
    /// of it was answered.
    async fn handle<C>(
        self: Arc<Self>,
        conn: &mut C,
        client: Option<&str>,
        hello_within: Duration,
    ) -> Result<(), &'static str>
    where
        C: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let slot = match client {
            Some(client) => Some(
                self.client_slot(client)
                    .ok_or("this client has too many connections in handshake")?,
            ),
            None => None,
        };
        let handshake = self
            .handshakes
            .clone()
            .try_acquire_owned()
            .map_err(|_| "too many connections in handshake")?;
        let (head, host) = tokio::time::timeout(hello_within, read_hello(conn))
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
        drop(slot);
        tracing::debug!(node = %label, "sandbox relay stream open");
        splice(conn, head, socket).await;
        drop(stream);
        Ok(())
    }
}

/// Read until the bytes hold a whole ClientHello. Returns every byte read,
/// to forward unchanged, and the server name.
async fn read_hello<C: AsyncRead + Unpin>(conn: &mut C) -> Result<(Vec<u8>, String), &'static str> {
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
async fn splice<C>(conn: &mut C, head: Vec<u8>, socket: WebSocket)
where
    C: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (mut rd, mut wr) = tokio::io::split(conn);
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
                Message::Binary(b) => {
                    wr.write_all(&b).await.map_err(|_| ())?;
                    // A WebSocket client's bytes leave only on a flush.
                    wr.flush().await.map_err(|_| ())?;
                }
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

/// Who a WebSocket's handshake place is counted against: the address
/// Cloudflare names in `CF-Connecting-IP`, else the first one in
/// `X-Forwarded-For` (another proxy's), else the peer's. Nothing proves a
/// header came from a proxy, so a client that reaches the hub directly can
/// set one, but all that buys is a bucket of its own, as a second address
/// would.
fn client_of(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty() && v.len() <= 64)
            .map(str::to_string)
    };
    header("cf-connecting-ip")
        .or_else(|| header("x-forwarded-for"))
        .or_else(|| peer.map(|p| p.ip().to_string()))
        .unwrap_or_else(|| "unknown".into())
}

/// `GET /v0/relay/connect`: a sandbox's WebSocket, unauthenticated as the
/// relay listener is. It is routed and refused exactly as a TCP connection
/// is; a refusal is a close and nothing else.
pub async fn connect(
    State(s): State<AppState>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(relay) = s.relay.clone() else {
        return err(StatusCode::NOT_FOUND, "this hub has no sandbox relay").into_response();
    };
    let client = client_of(&headers, peer.map(|Extension(ConnectInfo(p))| p));
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .max_frame_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |socket| async move {
            let mut conn = ws_bytes::WsBytes::new(socket);
            let within = relay.cfg.ws_hello_timeout;
            if let Err(why) = relay.handle(&mut conn, Some(&client), within).await {
                tracing::debug!(why, "sandbox relay WebSocket closed unanswered");
            }
            conn.close().await;
        })
}

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

    // ------------------------------------------- the WebSocket carrier

    use proto::keys::Identity;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    const WAIT: Duration = Duration::from_secs(10);

    fn node_key(id: &Identity) -> [u8; 32] {
        hex::decode(id.node_id()).unwrap().try_into().unwrap()
    }

    /// The hub's router on a loopback port, with `relay`.
    async fn serve_hub(relay: &Arc<SandboxRelay>) -> std::net::SocketAddr {
        let mut state = crate::state_for(
            Arc::new(crate::store::MemoryFrames::new()),
            relay.members.clone(),
            crate::HubConfig::default(),
            Arc::new(crate::pokes::PokeHub::new()),
            None,
        );
        state.relay = Some(relay.clone());
        let app = crate::app_with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        addr
    }

    /// A node that dials back for every offer and plays a TLS server that
    /// echoes each message, answers its client's half-close with `bye` and
    /// one of its own, and records what it got.
    fn fake_node(
        hub: std::net::SocketAddr,
        id: Identity,
        mut offers: mpsc::Receiver<String>,
    ) -> Arc<Mutex<Vec<u8>>> {
        let got = Arc::new(Mutex::new(Vec::new()));
        let seen = got.clone();
        let id = Arc::new(id);
        tokio::spawn(async move {
            while let Some(offer) = offers.recv().await {
                let (id, seen) = (id.clone(), seen.clone());
                tokio::spawn(async move {
                    let path = proto::relay::stream_path(&offer);
                    let mut req = format!("ws://{hub}{path}").into_client_request().unwrap();
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    for (k, v) in proto::auth::signed_headers(&id, "GET", &path, b"", now) {
                        req.headers_mut().insert(k, v.parse().unwrap());
                    }
                    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
                    while let Some(Ok(msg)) = ws.next().await {
                        match msg {
                            WsMessage::Binary(b) if b.is_empty() => {
                                ws.send(WsMessage::Binary(Bytes::from_static(b"bye")))
                                    .await
                                    .unwrap();
                                ws.send(WsMessage::Binary(Bytes::new())).await.unwrap();
                            }
                            WsMessage::Binary(b) => {
                                seen.lock().unwrap().extend_from_slice(&b);
                                ws.send(WsMessage::Binary(b)).await.unwrap();
                            }
                            WsMessage::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });
        got
    }

    async fn sandbox(hub: std::net::SocketAddr) -> Client {
        sandbox_as(hub, None).await
    }

    /// A sandbox whose front names it as `client` in `CF-Connecting-IP`.
    async fn sandbox_as(hub: std::net::SocketAddr, client: Option<&str>) -> Client {
        let url = format!("ws://{hub}{}", proto::relay::CONNECT_PATH);
        let mut req = url.into_client_request().unwrap();
        if let Some(client) = client {
            req.headers_mut()
                .insert("cf-connecting-ip", client.parse().unwrap());
        }
        tokio_tungstenite::connect_async(req).await.unwrap().0
    }

    /// Bytes until `want` have come, or what a close or end gave instead.
    async fn read_bytes(ws: &mut Client, want: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < want {
            match tokio::time::timeout(WAIT, ws.next())
                .await
                .expect("in time")
            {
                Some(Ok(WsMessage::Binary(b))) if b.is_empty() => break,
                Some(Ok(WsMessage::Binary(b))) => out.extend_from_slice(&b),
                Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
        out
    }

    /// What the relay answers a sandbox that sends `first`: every message
    /// up to the end, which must come.
    async fn answered(hub: std::net::SocketAddr, first: &[u8]) -> Vec<WsMessage> {
        answered_as(hub, None, first).await
    }

    async fn answered_as(
        hub: std::net::SocketAddr,
        client: Option<&str>,
        first: &[u8],
    ) -> Vec<WsMessage> {
        let mut ws = sandbox_as(hub, client).await;
        if !first.is_empty() {
            ws.send(WsMessage::Binary(Bytes::copy_from_slice(first)))
                .await
                .unwrap();
        }
        let mut got = Vec::new();
        loop {
            match tokio::time::timeout(WAIT, ws.next())
                .await
                .expect("the relay ended the connection")
            {
                Some(Ok(msg)) => got.push(msg),
                None | Some(Err(_)) => return got,
            }
        }
    }

    #[tokio::test]
    async fn a_sandbox_reaches_its_node_over_the_hubs_own_url() {
        let node = Identity::from_seed(&[5; 32]);
        let relay = relay_with(&[node_key(&node)]);
        let hub = serve_hub(&relay).await;
        let (host, offers, _listening) = relay.register(node_key(&node)).unwrap();
        let got = fake_node(hub, node, offers);

        let mut ws = sandbox(hub).await;
        let hello = sni::tests::client_hello(&host);
        // The hello in two messages: the relay reads on until it has it all.
        let (a, b) = hello.split_at(20);
        ws.send(WsMessage::Binary(Bytes::copy_from_slice(a)))
            .await
            .unwrap();
        ws.send(WsMessage::Binary(Bytes::copy_from_slice(b)))
            .await
            .unwrap();
        assert_eq!(read_bytes(&mut ws, hello.len()).await, hello);

        let big = vec![7u8; 300_000];
        for part in big.chunks(proto::relay::MAX_MESSAGE) {
            ws.send(WsMessage::Binary(Bytes::copy_from_slice(part)))
                .await
                .unwrap();
        }
        assert_eq!(read_bytes(&mut ws, big.len()).await, big);

        // Half-close: the node still answers, then ends its side.
        ws.send(WsMessage::Binary(Bytes::new())).await.unwrap();
        assert_eq!(read_bytes(&mut ws, 3).await, b"bye");
        assert!(read_bytes(&mut ws, 1).await.is_empty(), "the node's end");
        let mut all = hello.clone();
        all.extend_from_slice(&big);
        assert_eq!(
            *got.lock().unwrap(),
            all,
            "the node got every byte, in order"
        );
    }

    #[tokio::test]
    async fn what_the_websocket_carrier_will_not_route_gets_only_a_close() {
        let (node, stranger) = (Identity::from_seed(&[6; 32]), Identity::from_seed(&[7; 32]));
        let mut cfg = RelayConfig::new("relay.example");
        cfg.ws_hello_timeout = Duration::from_millis(300);
        let members = Arc::new(MemoryMembers::new());
        let relay = SandboxRelay::new(cfg, members.clone());
        members
            .put(&Member {
                node_id: node.node_id(),
                x25519_pub: String::new(),
                binding_sig: String::new(),
                name: "n".into(),
                channels: vec![proto::frame::MESH_CHANNEL.into()],
                admitted_ms: 0,
                admitted_by: "test".into(),
                role: MemberRole::Node,
            })
            .unwrap();
        let hub = serve_hub(&relay).await;
        let (_host, _offers, _listening) = relay.register(node_key(&node)).unwrap();
        // Registered, but not a member.
        let (stranger_host, _s_offers, _s) = relay.register(node_key(&stranger)).unwrap();

        let only_a_close = |got: &[WsMessage]| matches!(got, [WsMessage::Close(_)]);
        for first in [
            sni::tests::client_hello(&format!("{}.relay.example", "0".repeat(32))),
            sni::tests::client_hello(&stranger_host),
            sni::tests::client_hello("x.elsewhere.example"),
            b"GET / HTTP/1.1\r\n\r\n".to_vec(),
            // Nothing at all: no hello in time.
            Vec::new(),
        ] {
            let got = answered(hub, &first).await;
            assert!(only_a_close(&got), "{got:?}");
        }
    }

    #[test]
    fn a_client_is_named_by_its_front_else_its_address() {
        let peer: std::net::SocketAddr = "198.51.100.4:5555".parse().unwrap();
        let mut h = HeaderMap::new();
        assert_eq!(client_of(&h, Some(peer)), "198.51.100.4");
        assert_eq!(client_of(&h, None), "unknown");
        h.insert("x-forwarded-for", "203.0.113.9, 10.0.0.1".parse().unwrap());
        assert_eq!(client_of(&h, Some(peer)), "203.0.113.9");
        h.insert("cf-connecting-ip", " 192.0.2.7 ".parse().unwrap());
        assert_eq!(client_of(&h, Some(peer)), "192.0.2.7");
    }

    #[tokio::test]
    async fn one_client_at_its_share_of_handshakes_leaves_the_rest_to_others() {
        let node = Identity::from_seed(&[9; 32]);
        let mut cfg = RelayConfig::new("relay.example");
        cfg.max_handshakes_per_client = 2;
        cfg.ws_hello_timeout = Duration::from_secs(5);
        let members = Arc::new(MemoryMembers::new());
        members
            .put(&Member {
                node_id: node.node_id(),
                x25519_pub: String::new(),
                binding_sig: String::new(),
                name: "n".into(),
                channels: vec![proto::frame::MESH_CHANNEL.into()],
                admitted_ms: 0,
                admitted_by: "test".into(),
                role: MemberRole::Node,
            })
            .unwrap();
        let relay = SandboxRelay::new(cfg, members);
        let hub = serve_hub(&relay).await;
        let (host, offers, _listening) = relay.register(node_key(&node)).unwrap();
        fake_node(hub, node, offers);

        // One client opens its share and sends nothing.
        let idle = [
            sandbox_as(hub, Some("192.0.2.1")).await,
            sandbox_as(hub, Some("192.0.2.1")).await,
        ];
        let start = tokio::time::Instant::now();
        while relay.in_handshake("192.0.2.1") < 2 {
            assert!(
                start.elapsed() < WAIT,
                "the idle connections were never counted"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Its next is closed at once, long before any hello timeout.
        let start = tokio::time::Instant::now();
        let got = answered_as(hub, Some("192.0.2.1"), b"").await;
        assert!(matches!(got.as_slice(), [WsMessage::Close(_)]), "{got:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
        // Another client is routed meanwhile.
        let mut other = sandbox_as(hub, Some("192.0.2.2")).await;
        let hello = sni::tests::client_hello(&host);
        other
            .send(WsMessage::Binary(Bytes::copy_from_slice(&hello)))
            .await
            .unwrap();
        assert_eq!(read_bytes(&mut other, hello.len()).await, hello);
        assert_eq!(
            relay.in_handshake("192.0.2.2"),
            0,
            "its place is given back"
        );
        drop(idle);
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
