//! Owner streams: a request for a session another node owns, answered by that
//! node over a bounded encrypted stream the hub only relays.
//!
//! The durable command path ([`super::forward`]) is the right shape for a
//! verdict or a prompt: one message, one ack, retried until it lands. It is the
//! wrong shape for the native harness API, which is a request with a body, a
//! response with a body, and — for the event stream and the terminal — no end
//! at all. Replaying those through the outbox would repeat mutations and fill
//! the hub with bytes nobody will read again.
//!
//! So a stream is its own family (`proto::stream`), and three rules carry it.
//!
//! **The owner decides, from its own state.** The serving node's
//! authorization is not evidence. On [`StreamOpen`] the owner re-checks that
//! the session is its own, that the opener is still a member of the session's
//! channel *by the owner's own record*, and then runs the request through its
//! own gateway — the route matrix, the pinned directory, the policy, the
//! intent row. An operator the serving node was happy to admit is refused here
//! if this node does not grant them that channel.
//!
//! **Input is never replayed.** A stream that dies is gone. The serving node
//! re-opens on the *next* request; it never re-sends a body it already sent,
//! and the owner never retries a mutation whose answer it lost — the intent row
//! written before dispatch (#196/#198) is marked uncertain and travels back as
//! a flag on the response head.
//!
//! **Everything is bounded.** Credits in both directions, a response size cap,
//! an open timeout and an idle timeout, and a cap on how many streams one node
//! may have open. The hub bounds the same traffic again from the outside.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use proto::auth::signed_headers;
use proto::stream::{
    closed, new_stream_id, refused, Direction, StreamEnvelope, StreamFrame, StreamKind, StreamOpen,
    MAX_CHUNK_BYTES, MAX_OPEN_HEADERS, STREAM_PROTOCOL_VERSION,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Notify};

use super::client::{HubError, MeshClient};
use crate::store::now_ms;

/// Chunks a sender may have outstanding before it must wait for a grant. Small
/// enough that a stalled reader costs one window of memory, large enough that a
/// healthy transfer is never round-trip bound.
pub const INITIAL_CREDIT: u32 = 16;

/// How much plaintext one chunk carries.
const CHUNK_BYTES: usize = 32 * 1024;

/// Request headers an open may carry. An allowlist because the request is
/// being re-made on another machine: nothing that authenticates the caller
/// here (cookies, authorization) and nothing that would pin the harness to
/// this node's idea of scope may travel.
const REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "content-type",
    "last-event-id",
    "if-none-match",
];

/// Response headers the answer may carry back. Everything the gateway already
/// strips stays stripped; this is a second allowlist on top of it.
const RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "cache-control",
    "etag",
    "last-modified",
    "vary",
    "content-disposition",
];

/// What the owner was asked to do, once the stream has carried the whole
/// request. Deliberately plain data: the executor is the node's own HTTP
/// state, and it re-derives everything else.
pub struct OwnerRequest {
    /// The verified node id of the node that opened the stream.
    pub sender: String,
    pub session_id: String,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// Who the serving node says is asking. Evidence, never a grant.
    pub operator: Option<String>,
}

/// The owner's answer, as the local gateway produced it.
pub struct OwnerResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    /// The gateway could not learn whether a mutation happened. Reported, not
    /// retried.
    pub uncertain: bool,
}

/// What drives the local gateway for a stream. Implemented by the HTTP
/// layer's state, which has everything a local request would.
#[async_trait::async_trait]
pub trait StreamExecutor: Send + Sync {
    async fn serve(&self, request: OwnerRequest) -> OwnerResponse;
}

// ---------------------------------------------------------------------------
// Credit
// ---------------------------------------------------------------------------

/// The send window. A sender takes a credit per chunk and waits when there are
/// none; the reader grants one back as it consumes. Nothing is ever dropped to
/// keep going, which is what makes "bounded memory" and "no loss" the same
/// statement.
#[derive(Debug)]
pub struct Credit {
    state: Mutex<CreditState>,
    notify: Notify,
}

#[derive(Debug)]
struct CreditState {
    available: u32,
    closed: bool,
    /// How often a sender had to wait. Observable so a test can assert that
    /// backpressure actually bit rather than merely not breaking.
    waits: u64,
}

impl Credit {
    pub fn new(initial: u32) -> Self {
        Self {
            state: Mutex::new(CreditState {
                available: initial,
                closed: false,
                waits: 0,
            }),
            notify: Notify::new(),
        }
    }

    /// Take one credit, waiting for a grant if there are none. `false` when
    /// the stream closed while waiting.
    pub async fn take(&self) -> bool {
        loop {
            {
                let mut s = self.state.lock().unwrap();
                if s.closed {
                    return false;
                }
                if s.available > 0 {
                    s.available -= 1;
                    return true;
                }
                s.waits += 1;
            }
            self.notify.notified().await;
        }
    }

    pub fn grant(&self, n: u32) {
        {
            let mut s = self.state.lock().unwrap();
            s.available = s.available.saturating_add(n);
        }
        self.notify.notify_waiters();
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.notify.notify_waiters();
    }

    pub fn waits(&self) -> u64 {
        self.state.lock().unwrap().waits
    }

    pub fn available(&self) -> u32 {
        self.state.lock().unwrap().available
    }
}

// ---------------------------------------------------------------------------
// The writer
// ---------------------------------------------------------------------------

/// One end of one stream, sealing and posting frames in order.
pub struct StreamWriter {
    client: Arc<MeshClient>,
    channel: String,
    epoch: String,
    peer: String,
    stream_id: String,
    direction: Direction,
    seq: Mutex<u64>,
    credit: Arc<Credit>,
}

impl StreamWriter {
    /// Seal and post one frame. Frames are posted in the order they are
    /// sealed, because the sequence number is the nonce.
    pub async fn send(&self, frame: &StreamFrame) -> Result<(), HubError> {
        let ring = self.client.keyring(&self.channel)?;
        let seq = {
            let mut s = self.seq.lock().unwrap();
            let n = *s;
            *s += 1;
            n
        };
        let env = StreamEnvelope::seal(
            self.client.identity(),
            &self.channel,
            &self.peer,
            &ring,
            &self.epoch,
            &self.stream_id,
            seq,
            self.direction,
            frame,
            now_ms(),
        )
        .map_err(|e| HubError::Local(e.to_string()))?;
        self.client.stream_post(&env).await
    }

    /// Send a chunk, waiting on the credit window first. `false` when the
    /// stream closed before there was room.
    pub async fn send_chunk(&self, bytes: &[u8], fin: bool) -> bool {
        if !self.credit.take().await {
            return false;
        }
        self.send(&StreamFrame::chunk(bytes, fin)).await.is_ok()
    }

    pub async fn close(&self, reason: &str, detail: Option<&str>) {
        let _ = self
            .send(&StreamFrame::Close {
                reason: reason.to_string(),
                detail: detail.map(str::to_string),
            })
            .await;
        self.client.stream_delete(&self.stream_id).await;
    }

    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }
}

// ---------------------------------------------------------------------------
// The router
// ---------------------------------------------------------------------------

struct Inbound {
    peer: String,
    /// The direction the peer seals in.
    peer_dir: Direction,
    /// The next sequence number expected from the peer.
    next_seq: u64,
    tx: mpsc::Sender<StreamFrame>,
    /// The credit *this* node holds for sending on this stream.
    credit: Arc<Credit>,
}

#[derive(Default, Debug)]
pub struct StreamStats {
    pub opened: AtomicU64,
    pub served: AtomicU64,
    pub refused: AtomicU64,
    pub replays_dropped: AtomicU64,
    pub relay_drops: AtomicU64,
    pub fenced: AtomicU64,
}

/// Everything this node knows about streams: the ones it opened, the ones
/// addressed to it, and the epoch that fences them.
pub struct StreamRouter {
    weak: Weak<MeshClient>,
    /// This run of this node. A restart mints a new one, which is what fences
    /// a serving node still holding a stream to the process that died.
    owner_epoch: String,
    inbound: Mutex<HashMap<String, Inbound>>,
    /// The owner epoch last seen per `(owner node, session)`, so the next open
    /// can name it and be fenced rather than answered by a stranger.
    seen_epochs: Mutex<HashMap<(String, String), String>>,
    executor: std::sync::OnceLock<Arc<dyn StreamExecutor>>,
    pub stats: StreamStats,
}

impl StreamRouter {
    pub fn new(weak: Weak<MeshClient>) -> Self {
        Self {
            weak,
            owner_epoch: uuid::Uuid::now_v7().to_string(),
            inbound: Mutex::new(HashMap::new()),
            seen_epochs: Mutex::new(HashMap::new()),
            executor: std::sync::OnceLock::new(),
            stats: StreamStats::default(),
        }
    }

    pub fn set_executor(&self, executor: Arc<dyn StreamExecutor>) {
        let _ = self.executor.set(executor);
    }

    pub fn owner_epoch(&self) -> &str {
        &self.owner_epoch
    }

    pub fn open_count(&self) -> usize {
        self.inbound.lock().unwrap().len()
    }

    fn forget(&self, stream_id: &str) {
        if let Some(i) = self.inbound.lock().unwrap().remove(stream_id) {
            i.credit.close();
        }
    }

    /// The hub said it dropped a stream, or the peer closed it out of band.
    /// Both ends stop; nothing is replayed.
    pub fn relay_closed(&self, stream_id: &str, reason: &str, detail: &str) {
        let entry = self.inbound.lock().unwrap().remove(stream_id);
        if let Some(i) = entry {
            self.stats.relay_drops.fetch_add(1, Ordering::Relaxed);
            i.credit.close();
            let _ = i.tx.try_send(StreamFrame::Close {
                reason: reason.to_string(),
                detail: Some(detail.to_string()),
            });
        }
    }

    /// One sealed envelope off the relay connection.
    pub fn ingest(&self, raw: &str) {
        let Some(client) = self.weak.upgrade() else {
            return;
        };
        let Ok(env) = serde_json::from_str::<StreamEnvelope>(raw) else {
            return;
        };
        let sender = match env.verify() {
            Ok(key) => hex::encode(key),
            Err(e) => {
                tracing::warn!(stream = %env.stream_id, error = %e, "stream frame failed verification; dropped");
                return;
            }
        };
        if env.recipient != client.node_id() || sender == client.node_id() {
            return;
        }
        let Ok(ring) = client.keyring(&env.channel) else {
            tracing::warn!(channel = %env.channel, "a stream arrived on a channel this node holds no key for");
            return;
        };

        enum Route {
            Existing(mpsc::Sender<StreamFrame>, Arc<Credit>, Direction),
            New,
        }
        let route = {
            let mut map = self.inbound.lock().unwrap();
            match map.get_mut(&env.stream_id) {
                Some(e) => {
                    // A frame from anyone but the peer, out of order, or
                    // repeated, is a replay: the sequence is the nonce, so
                    // there is nothing to weigh.
                    if e.peer != sender || env.seq != e.next_seq {
                        self.stats.replays_dropped.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    e.next_seq += 1;
                    Route::Existing(e.tx.clone(), e.credit.clone(), e.peer_dir)
                }
                None => {
                    if env.seq != 0 {
                        self.stats.replays_dropped.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    Route::New
                }
            }
        };

        match route {
            Route::Existing(tx, credit, dir) => {
                let Ok(frame) = env.open(&ring, client.identity(), dir) else {
                    tracing::warn!(stream = %env.stream_id, "a stream frame could not be opened");
                    return;
                };
                match frame {
                    StreamFrame::Flow { credit: n } => credit.grant(n),
                    other => {
                        // A close ends the stream here, and so does a reader
                        // that has gone away: either way the entry goes, and
                        // nothing is retried.
                        let closing = matches!(other, StreamFrame::Close { .. });
                        if tx.try_send(other).is_err() || closing {
                            self.forget(&env.stream_id);
                        }
                    }
                }
            }
            Route::New => {
                let Ok(StreamFrame::Open(open)) =
                    env.open(&ring, client.identity(), Direction::Serving)
                else {
                    // Either it does not open, or it is not an open. Both are
                    // frames this node has no stream for; there is nobody to
                    // refuse to, because a refusal would need a key exchange
                    // the opener never completed.
                    return;
                };
                self.begin_owner_stream(client, env, sender, *open);
            }
        }
    }

    /// Register a stream this node is about to open. The entry exists before
    /// the first frame is sent, so an answer that races the post is not lost.
    fn register(
        &self,
        stream_id: &str,
        peer: &str,
        peer_dir: Direction,
        credit: Arc<Credit>,
    ) -> mpsc::Receiver<StreamFrame> {
        let (tx, rx) = mpsc::channel(INITIAL_CREDIT as usize * 4);
        self.inbound.lock().unwrap().insert(
            stream_id.to_string(),
            Inbound {
                peer: peer.to_string(),
                peer_dir,
                next_seq: 0,
                tx,
                credit,
            },
        );
        rx
    }

    #[allow(clippy::too_many_arguments)]
    fn writer(
        &self,
        client: Arc<MeshClient>,
        channel: &str,
        epoch: &str,
        peer: &str,
        stream_id: &str,
        direction: Direction,
        credit: Arc<Credit>,
    ) -> Arc<StreamWriter> {
        Arc::new(StreamWriter {
            client,
            channel: channel.to_string(),
            epoch: epoch.to_string(),
            peer: peer.to_string(),
            stream_id: stream_id.to_string(),
            direction,
            seq: Mutex::new(0),
            credit,
        })
    }
}

// ---------------------------------------------------------------------------
// The owner side
// ---------------------------------------------------------------------------

impl StreamRouter {
    fn begin_owner_stream(
        &self,
        client: Arc<MeshClient>,
        env: StreamEnvelope,
        sender: String,
        open: StreamOpen,
    ) {
        let credit = Arc::new(Credit::new(INITIAL_CREDIT));
        let rx = self.register(&env.stream_id, &sender, Direction::Serving, credit.clone());
        // The open itself was seq 0; the next frame from the peer is 1.
        if let Some(e) = self.inbound.lock().unwrap().get_mut(&env.stream_id) {
            e.next_seq = 1;
        }
        let writer = self.writer(
            client.clone(),
            &env.channel,
            &env.epoch,
            &sender,
            &env.stream_id,
            Direction::Owner,
            credit,
        );
        let Some(router) = client.streams_arc() else {
            return;
        };
        self.stats.served.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            router
                .serve_stream(client, writer, rx, env, sender, open)
                .await;
        });
    }

    async fn serve_stream(
        self: Arc<Self>,
        client: Arc<MeshClient>,
        writer: Arc<StreamWriter>,
        mut rx: mpsc::Receiver<StreamFrame>,
        env: StreamEnvelope,
        sender: String,
        open: StreamOpen,
    ) {
        let stream_id = env.stream_id.clone();
        let refuse = |reason: &'static str, detail: String| {
            let writer = writer.clone();
            let router = self.clone();
            let stream_id = stream_id.clone();
            async move {
                router.stats.refused.fetch_add(1, Ordering::Relaxed);
                let _ = writer
                    .send(&StreamFrame::Refused {
                        reason: reason.to_string(),
                        detail: Some(detail),
                    })
                    .await;
                client_forget(&router, &writer, &stream_id).await;
            }
        };

        if open.protocol_version != STREAM_PROTOCOL_VERSION {
            refuse(
                refused::VERSION,
                format!(
                    "this node speaks owner-stream protocol {STREAM_PROTOCOL_VERSION}, not {}",
                    open.protocol_version
                ),
            )
            .await;
            return;
        }
        if open.headers.len() > MAX_OPEN_HEADERS {
            refuse(
                refused::MALFORMED,
                format!("an open may name at most {MAX_OPEN_HEADERS} headers"),
            )
            .await;
            return;
        }

        // The session must be this node's, and the stream must be on the
        // session's own channel: a member of one channel cannot reach a
        // session running on another through a shared `@mesh` membership.
        let row = match client.store().get_session(&open.session_id) {
            Ok(Some(row)) if row.node_id == client.node_id() => row,
            _ => {
                refuse(
                    refused::UNKNOWN_SESSION,
                    format!("no session {} runs on this node", open.session_id),
                )
                .await;
                return;
            }
        };
        if row.channel != env.channel {
            refuse(
                refused::UNKNOWN_SESSION,
                format!(
                    "session {} is on channel {}, not {}",
                    open.session_id, row.channel, env.channel
                ),
            )
            .await;
            return;
        }
        // Authorization is repeated here, against this node's own record of
        // who holds the channel. The serving node having admitted the
        // operator is not evidence of anything on this machine.
        if !self.member_of(&client, &sender, &row.channel) {
            refuse(
                refused::REVOKED_MEMBER,
                format!("{sender} is not a member of channel {} here", row.channel),
            )
            .await;
            return;
        }
        if open
            .owner_epoch
            .as_deref()
            .is_some_and(|e| e != self.owner_epoch)
        {
            self.stats.fenced.fetch_add(1, Ordering::Relaxed);
            refuse(
                refused::FENCED,
                "this node restarted since that stream; re-open rather than resume".into(),
            )
            .await;
            return;
        }

        // The request body, as chunks, bounded by what the open declared.
        let cap = open.body_len.min(client.cfg.mesh.stream_max_body_bytes);
        let idle = Duration::from_secs(client.cfg.mesh.stream_idle_secs.max(1));
        let mut body = Vec::new();
        if open.body_len > 0 {
            if open.body_len > client.cfg.mesh.stream_max_body_bytes {
                refuse(
                    refused::MALFORMED,
                    format!("a request body of {} bytes is too large", open.body_len),
                )
                .await;
                return;
            }
            loop {
                match tokio::time::timeout(idle, rx.recv()).await {
                    Ok(Some(StreamFrame::Chunk { bytes, fin })) => {
                        let Some(part) = (StreamFrame::Chunk { bytes, fin: false }).chunk_bytes()
                        else {
                            refuse(refused::MALFORMED, "a chunk was not valid base64".into()).await;
                            return;
                        };
                        body.extend_from_slice(&part);
                        let _ = writer.send(&StreamFrame::Flow { credit: 1 }).await;
                        if body.len() as u64 > cap {
                            refuse(
                                refused::MALFORMED,
                                "the body outran what it declared".into(),
                            )
                            .await;
                            return;
                        }
                        if fin {
                            break;
                        }
                    }
                    Ok(Some(StreamFrame::Close { .. })) | Ok(None) | Err(_) => {
                        client_forget(&self, &writer, &stream_id).await;
                        return;
                    }
                    Ok(Some(_)) => {}
                }
            }
        }
        if let Some(want) = &open.body_sha256 {
            let got = hex::encode(Sha256::digest(&body));
            if &got != want {
                refuse(
                    refused::MALFORMED,
                    "the body does not match the digest the open declared".into(),
                )
                .await;
                return;
            }
        }

        let Some(executor) = self.executor.get().cloned() else {
            refuse(
                refused::REFUSED,
                "this node is not ready to serve streams".into(),
            )
            .await;
            return;
        };

        let answer = executor
            .serve(OwnerRequest {
                sender: sender.clone(),
                session_id: open.session_id.clone(),
                method: open.method.clone(),
                path: open.path.clone(),
                query: open.query.clone(),
                headers: open.headers.clone(),
                body: Bytes::from(body),
                operator: open.operator.clone(),
            })
            .await;

        if writer
            .send(&StreamFrame::Head {
                status: answer.status,
                headers: answer.headers,
                owner_epoch: self.owner_epoch.clone(),
                uncertain: answer.uncertain,
            })
            .await
            .is_err()
        {
            client_forget(&self, &writer, &stream_id).await;
            return;
        }

        // The response, chunk by chunk, under the credit window. Membership is
        // re-read as it goes: a key revoked mid-stream closes the stream
        // rather than finishing the answer.
        let mut data = answer.body.into_data_stream();
        let mut sent: u64 = 0;
        let max = client.cfg.mesh.stream_max_response_bytes;
        loop {
            let next = tokio::time::timeout(idle, data.next()).await;
            match next {
                Ok(Some(Ok(bytes))) => {
                    if !self.member_of(&client, &sender, &row.channel) {
                        writer
                            .close(closed::REVOKED, Some("channel membership was revoked"))
                            .await;
                        self.forget(&stream_id);
                        return;
                    }
                    sent += bytes.len() as u64;
                    if sent > max {
                        writer
                            .close(closed::TOO_LARGE, Some("the response outran its cap"))
                            .await;
                        self.forget(&stream_id);
                        return;
                    }
                    for part in bytes.chunks(CHUNK_BYTES) {
                        if !writer.send_chunk(part, false).await {
                            self.forget(&stream_id);
                            return;
                        }
                    }
                }
                Ok(Some(Err(_))) | Err(_) => {
                    writer
                        .close(closed::LOCAL_ERROR, Some("the answer was cut short"))
                        .await;
                    self.forget(&stream_id);
                    return;
                }
                Ok(None) => break,
            }
        }
        let _ = writer.send_chunk(&[], true).await;
        writer.close(closed::DONE, None).await;
        self.forget(&stream_id);
    }

    /// This node's own record of who holds a channel. Written by the hub's
    /// member list, cleared when a node leaves, so a removal takes effect
    /// here without either side being asked to be honest about it.
    fn member_of(&self, client: &Arc<MeshClient>, node_id: &str, channel: &str) -> bool {
        client
            .store()
            .node_channels(node_id)
            .map(|c| c.iter().any(|name| name == channel))
            .unwrap_or(false)
    }
}

async fn client_forget(router: &Arc<StreamRouter>, writer: &Arc<StreamWriter>, stream_id: &str) {
    router.forget(stream_id);
    writer.client.stream_delete(stream_id).await;
}

// ---------------------------------------------------------------------------
// The serving side
// ---------------------------------------------------------------------------

/// A request for a session another node owns.
pub struct RemoteRequest {
    pub owner: String,
    pub channel: String,
    pub session_id: String,
    pub kind: StreamKind,
    pub method: Method,
    /// Already normalised, without a leading slash.
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub operator: Option<String>,
}

fn refusal(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({ "error": { "type": "tracon_stream", "message": message } })),
    )
        .into_response()
}

impl StreamRouter {
    /// Open a stream to the owner and answer the operator with what comes
    /// back. One request, one stream: nothing here retries, and nothing here
    /// re-sends a body.
    pub async fn request(self: &Arc<Self>, req: RemoteRequest) -> Response {
        let Some(client) = self.weak.upgrade() else {
            return refusal(StatusCode::SERVICE_UNAVAILABLE, "the mesh is not running");
        };
        if !client.peer_reachable(&req.owner) {
            return refusal(
                StatusCode::GATEWAY_TIMEOUT,
                "the node that owns this session is unreachable",
            );
        }
        if self.open_count() >= client.cfg.mesh.stream_max_concurrent {
            return refusal(
                StatusCode::TOO_MANY_REQUESTS,
                "too many owner streams are open on this node",
            );
        }
        let fenced = {
            let key = (req.owner.clone(), req.session_id.clone());
            self.seen_epochs.lock().unwrap().get(&key).cloned()
        };
        match self.attempt(&client, &req, fenced.clone()).await {
            Attempt::Answered(response) => response,
            Attempt::Fenced(detail) => {
                // The owner restarted. Forget the epoch and re-open — but only
                // when there is no body to re-send. A mutation is never
                // replayed on this node's initiative; the operator asks again.
                self.seen_epochs
                    .lock()
                    .unwrap()
                    .remove(&(req.owner.clone(), req.session_id.clone()));
                if !req.body.is_empty() {
                    return refusal(
                        StatusCode::CONFLICT,
                        &format!(
                            "the owning node restarted before this request was answered ({detail}); \
                             it was not re-sent, because a request with a body is never replayed"
                        ),
                    );
                }
                match self.attempt(&client, &req, None).await {
                    Attempt::Answered(response) => response,
                    Attempt::Fenced(detail) | Attempt::Failed(detail) => {
                        refusal(StatusCode::BAD_GATEWAY, &detail)
                    }
                }
            }
            Attempt::Failed(detail) => refusal(StatusCode::BAD_GATEWAY, &detail),
        }
    }

    async fn attempt(
        self: &Arc<Self>,
        client: &Arc<MeshClient>,
        req: &RemoteRequest,
        owner_epoch: Option<String>,
    ) -> Attempt {
        let stream_id = new_stream_id();
        let ring = match client.keyring(&req.channel) {
            Ok(ring) => ring,
            Err(e) => return Attempt::Failed(e.to_string()),
        };
        let epoch = hex::encode(ring.newest().id());
        let credit = Arc::new(Credit::new(INITIAL_CREDIT));
        let mut rx = self.register(&stream_id, &req.owner, Direction::Owner, credit.clone());
        let writer = self.writer(
            client.clone(),
            &req.channel,
            &epoch,
            &req.owner,
            &stream_id,
            Direction::Serving,
            credit,
        );
        self.stats.opened.fetch_add(1, Ordering::Relaxed);

        let headers: Vec<(String, String)> = req
            .headers
            .iter()
            .filter(|(name, _)| REQUEST_HEADERS.contains(&name.as_str()))
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_string(), v.to_string()))
            })
            .take(MAX_OPEN_HEADERS)
            .collect();
        let open = StreamOpen {
            protocol_version: STREAM_PROTOCOL_VERSION,
            session_id: req.session_id.clone(),
            kind: req.kind,
            method: req.method.as_str().to_ascii_uppercase(),
            path: req.path.clone(),
            query: req.query.clone(),
            headers,
            body_sha256: (!req.body.is_empty()).then(|| hex::encode(Sha256::digest(&req.body))),
            body_len: req.body.len() as u64,
            operator: req.operator.clone(),
            owner_epoch,
        };
        if let Err(e) = writer.send(&StreamFrame::Open(Box::new(open))).await {
            self.forget(&stream_id);
            return Attempt::Failed(e.to_string());
        }
        for part in req.body.chunks(MAX_CHUNK_BYTES.min(CHUNK_BYTES)) {
            let last = part.as_ptr_range().end == req.body.as_ptr_range().end;
            if !writer.send_chunk(part, last).await {
                self.forget(&stream_id);
                return Attempt::Failed("the owner stopped reading the request".into());
            }
        }

        let open_timeout = Duration::from_secs(client.cfg.mesh.stream_open_timeout_secs.max(1));
        let head = match tokio::time::timeout(open_timeout, rx.recv()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                self.forget(&stream_id);
                return Attempt::Failed("the stream ended before the owner answered".into());
            }
            Err(_) => {
                writer
                    .close(closed::TIMEOUT, Some("no answer in time"))
                    .await;
                self.forget(&stream_id);
                return Attempt::Failed("the owner did not answer in time".into());
            }
        };
        match head {
            StreamFrame::Refused { reason, detail } => {
                let detail = detail.unwrap_or_else(|| reason.clone());
                client_forget(self, &writer, &stream_id).await;
                if reason == refused::FENCED {
                    return Attempt::Fenced(detail);
                }
                Attempt::Answered(refusal(
                    match reason.as_str() {
                        refused::UNKNOWN_SESSION => StatusCode::NOT_FOUND,
                        refused::REVOKED_MEMBER | refused::REFUSED => StatusCode::FORBIDDEN,
                        refused::VERSION => StatusCode::NOT_IMPLEMENTED,
                        refused::BUSY => StatusCode::TOO_MANY_REQUESTS,
                        _ => StatusCode::BAD_GATEWAY,
                    },
                    &detail,
                ))
            }
            StreamFrame::Head {
                status,
                headers,
                owner_epoch,
                uncertain,
            } => {
                self.seen_epochs.lock().unwrap().insert(
                    (req.owner.clone(), req.session_id.clone()),
                    owner_epoch.clone(),
                );
                Attempt::Answered(self.clone().response(
                    client.clone(),
                    writer,
                    rx,
                    stream_id,
                    status,
                    headers,
                    uncertain,
                ))
            }
            StreamFrame::Close { reason, detail } => {
                client_forget(self, &writer, &stream_id).await;
                Attempt::Failed(detail.unwrap_or(reason))
            }
            other => {
                client_forget(self, &writer, &stream_id).await;
                Attempt::Failed(format!("the owner answered with a {} frame", other.name()))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn response(
        self: Arc<Self>,
        client: Arc<MeshClient>,
        writer: Arc<StreamWriter>,
        rx: mpsc::Receiver<StreamFrame>,
        stream_id: String,
        status: u16,
        headers: Vec<(String, String)>,
        uncertain: bool,
    ) -> Response {
        let idle = Duration::from_secs(client.cfg.mesh.stream_idle_secs.max(1));
        let max = client.cfg.mesh.stream_max_response_bytes;
        let state = BodyState {
            rx,
            writer,
            router: self,
            stream_id,
            read: 0,
            max,
            idle,
            done: false,
        };
        let body = Body::from_stream(futures_util::stream::unfold(
            state,
            |mut state| async move {
                if state.done {
                    return None;
                }
                loop {
                    match tokio::time::timeout(state.idle, state.rx.recv()).await {
                        Ok(Some(StreamFrame::Chunk { bytes, fin })) => {
                            let frame = StreamFrame::Chunk { bytes, fin };
                            let part = frame.chunk_bytes().unwrap_or_default();
                            // A credit back for every chunk taken: the owner's
                            // window reopens exactly as fast as this reader
                            // drains, and no faster.
                            let _ = state.writer.send(&StreamFrame::Flow { credit: 1 }).await;
                            state.read += part.len() as u64;
                            if state.read > state.max {
                                state.done = true;
                                return Some((
                                    Err(std::io::Error::other("the answer outran its size cap")),
                                    state,
                                ));
                            }
                            if fin {
                                state.done = true;
                            }
                            if part.is_empty() {
                                if state.done {
                                    return None;
                                }
                                continue;
                            }
                            return Some((Ok(Bytes::from(part)), state));
                        }
                        Ok(Some(StreamFrame::Close { reason, detail })) => {
                            state.done = true;
                            // A stream that ends cleanly ends the body; one that
                            // was dropped ends it as an error, so the reader sees
                            // a truncated answer rather than a complete one.
                            return if reason == closed::DONE {
                                None
                            } else {
                                Some((Err(std::io::Error::other(detail.unwrap_or(reason))), state))
                            };
                        }
                        Ok(Some(_)) => continue,
                        Ok(None) => {
                            state.done = true;
                            return Some((
                                Err(std::io::Error::other(
                                    "the stream ended before the answer did",
                                )),
                                state,
                            ));
                        }
                        Err(_) => {
                            state.done = true;
                            return Some((
                                Err(std::io::Error::other("the owner stopped sending")),
                                state,
                            ));
                        }
                    }
                }
            },
        ));

        let mut out = Response::builder().status(status);
        for (name, value) in headers {
            if !RESPONSE_HEADERS.contains(&name.as_str()) {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                out = out.header(n, v);
            }
        }
        // The one thing the operator must not have to guess: whether the
        // owner knows what happened. Never a retry, always a statement.
        if uncertain {
            out = out.header("tracon-outcome", "uncertain");
        }
        out = out.header("tracon-served-by", "owner-stream");
        out.body(body)
            .unwrap_or_else(|_| refusal(StatusCode::BAD_GATEWAY, "the owner answered unusably"))
    }
}

enum Attempt {
    Answered(Response),
    Fenced(String),
    Failed(String),
}

struct BodyState {
    rx: mpsc::Receiver<StreamFrame>,
    writer: Arc<StreamWriter>,
    router: Arc<StreamRouter>,
    stream_id: String,
    read: u64,
    max: u64,
    idle: Duration,
    done: bool,
}

impl Drop for BodyState {
    fn drop(&mut self) {
        // The operator's client went away, or the body ended. Either way this
        // end is finished: the relay slot goes back, and the owner is told so
        // it stops producing into a pipe nobody reads.
        self.router.forget(&self.stream_id);
        let writer = self.writer.clone();
        let reason = if self.done {
            closed::DONE
        } else {
            closed::PEER_GONE
        };
        tokio::spawn(async move {
            writer.close(reason, None).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

impl MeshClient {
    fn streams_arc(&self) -> Option<Arc<StreamRouter>> {
        Some(self.streams.clone())
    }

    /// Post one sealed stream frame to the relay, waiting out the hub's own
    /// backpressure rather than dropping anything.
    pub(super) async fn stream_post(&self, env: &StreamEnvelope) -> Result<(), HubError> {
        let body = serde_json::to_vec(env).map_err(|e| HubError::Local(e.to_string()))?;
        let mut waited = Duration::ZERO;
        let ceiling = Duration::from_secs(self.cfg.mesh.stream_idle_secs.max(1));
        loop {
            match self.post("/v0/streams", body.clone()).await {
                Ok(_) => return Ok(()),
                Err(HubError::Refused { status: 429, body })
                    if body.contains("backpressure") && waited < ceiling =>
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    waited += Duration::from_millis(25);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Tell the relay this end is done, so the far end stops waiting even if
    /// the sealed close was the frame that did not make it.
    pub(super) async fn stream_delete(&self, stream_id: &str) {
        let path = format!("/v0/streams/{stream_id}");
        let ts = (now_ms() / 1000).max(0) as u64;
        let mut req = self.http.delete(format!("{}{}", self.hub_url(), path));
        for (k, v) in signed_headers(self.identity(), "DELETE", &path, b"", ts) {
            req = req.header(k, v);
        }
        let _ = req.send().await;
    }

    /// Hold the relay connection and feed everything it delivers to the
    /// router. A dropped connection ends every stream on it: nothing is
    /// replayed, which is the whole point of the family.
    pub(super) async fn stream_loop(&self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.stream_once().await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(e) => {
                    tracing::debug!(error = %e, "hub stream connection ended");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    async fn stream_once(&self) -> Result<(), HubError> {
        let path = "/v0/streams";
        let ts = (now_ms() / 1000).max(0) as u64;
        let mut req = self.http.get(format!("{}{}", self.hub_url(), path));
        for (k, v) in signed_headers(self.identity(), "GET", path, b"", ts) {
            req = req.header(k, v);
        }
        let res = req
            .send()
            .await
            .map_err(|e| HubError::Transport(e.to_string()))?;
        if !res.status().is_success() {
            return Err(HubError::Refused {
                status: res.status().as_u16(),
                body: res.text().await.unwrap_or_default(),
            });
        }
        let mut stream = res.bytes_stream();
        let mut buf = String::new();
        let mut event = String::new();
        let mut data = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| HubError::Transport(e.to_string()))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(i) = buf.find('\n') {
                let line = buf[..i].trim_end_matches('\r').to_string();
                buf.drain(..=i);
                if line.is_empty() {
                    self.dispatch_relay(&event, &data);
                    event.clear();
                    data.clear();
                } else if let Some(rest) = line.strip_prefix("event:") {
                    event = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data.push_str(rest.trim_start());
                }
            }
        }
        Ok(())
    }

    fn dispatch_relay(&self, event: &str, data: &str) {
        match event {
            "stream" => self.streams.ingest(data),
            "control" => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                    return;
                };
                let (Some(id), Some(reason)) =
                    (value["stream_id"].as_str(), value["reason"].as_str())
                else {
                    return;
                };
                let detail = value["detail"].as_str().unwrap_or(reason);
                tracing::debug!(stream = id, reason, "the relay ended a stream");
                self.streams.relay_closed(id, reason, detail);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The executor: the node's own gateway, re-run for a remote operator
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl StreamExecutor for crate::http::api::AppState {
    async fn serve(&self, request: OwnerRequest) -> OwnerResponse {
        let method = Method::from_bytes(request.method.as_bytes()).unwrap_or(Method::GET);
        let mut headers = HeaderMap::new();
        for (name, value) in &request.headers {
            if !REQUEST_HEADERS.contains(&name.as_str()) {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(n, v);
            }
        }
        let mut mounted = format!("/api/opencode/{}/{}", request.session_id, request.path);
        if let Some(q) = &request.query {
            mounted.push('?');
            mounted.push_str(q);
        }
        let uri: Uri = match mounted.parse() {
            Ok(uri) => uri,
            Err(_) => {
                return OwnerResponse {
                    status: StatusCode::BAD_REQUEST.as_u16(),
                    headers: Vec::new(),
                    body: Body::from("the path does not parse"),
                    uncertain: false,
                }
            }
        };
        // Straight through the same handler a local operator reaches, so the
        // route matrix, the pinned directory, the injected credential, the
        // policy and the intent row all apply exactly as they do here.
        let response = crate::gateway::opencode::handle(
            axum::extract::State(self.clone()),
            axum::extract::Path((request.session_id.clone(), request.path.clone())),
            method,
            uri,
            headers,
            request.body,
        )
        .await;
        let uncertain = self
            .store()
            .opencode_session_of(&request.session_id)
            .ok()
            .flatten()
            .is_some_and(|row| row.is_uncertain());
        let (parts, body) = response.into_parts();
        OwnerResponse {
            status: parts.status.as_u16(),
            headers: parts
                .headers
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|v| (name.as_str().to_string(), v.to_string()))
                })
                .collect(),
            body,
            uncertain,
        }
    }
}

/// The gateway's hook for a session another node owns.
///
/// Thin on purpose: everything about what the call *means* stays on the owner,
/// where the route matrix, the pinned directory, the injected credential and
/// the intent row live. This end only decides what kind of stream to open and
/// carries the bytes.
pub async fn remote_gateway(
    mesh: &Arc<MeshClient>,
    row: &crate::store::SessionRow,
    method: &Method,
    uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(raw_tail) = uri
        .path()
        .strip_prefix("/api/opencode/")
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, tail)| tail)
    else {
        return refusal(StatusCode::NOT_FOUND, "not a harness API path");
    };
    let Some(path) = crate::gateway::model::normalised(raw_tail) else {
        return refusal(
            StatusCode::BAD_REQUEST,
            "the path does not normalise to an unambiguous route",
        );
    };
    let joined = path.join("/");
    let kind = stream_kind(&joined, &headers);
    let operator = crate::http::auth::session_hash(&headers);
    mesh.streams()
        .request(RemoteRequest {
            owner: row.node_id.clone(),
            channel: row.channel.clone(),
            session_id: row.id.clone(),
            kind,
            method: method.clone(),
            path: joined,
            query: uri.query().map(str::to_string),
            headers,
            body,
            operator,
        })
        .await
}

/// What shape the answer will be. An event stream and a terminal upgrade are
/// not requests that end; they are pipes, and the owner reads them as such.
fn stream_kind(joined: &str, headers: &HeaderMap) -> StreamKind {
    if joined.ends_with("/connect") {
        return StreamKind::Ws;
    }
    let wants_events = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    if wants_events || joined.ends_with("/event") {
        StreamKind::Sse
    } else {
        StreamKind::Http
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn credit_blocks_until_it_is_granted() {
        let credit = Arc::new(Credit::new(1));
        assert!(credit.take().await);
        assert_eq!(credit.available(), 0);
        let waiting = credit.clone();
        let handle = tokio::spawn(async move { waiting.take().await });
        // The waiter parks rather than sending anyway.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished());
        assert!(credit.waits() >= 1, "the sender waited");
        credit.grant(1);
        assert!(handle.await.unwrap());
    }

    #[tokio::test]
    async fn a_closed_credit_stops_a_waiter_rather_than_hanging() {
        let credit = Arc::new(Credit::new(0));
        let waiting = credit.clone();
        let handle = tokio::spawn(async move { waiting.take().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        credit.close();
        assert!(!handle.await.unwrap());
    }
}
