//! The hub client. Everything outbound goes through a durable outbox and is
//! drained in order; everything inbound is pulled by cursor per channel. The
//! hub's SSE stream is only a hint to pull sooner. A hub outage therefore
//! costs latency, never state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use proto::auth::signed_headers;
use proto::frame::{Envelope, Payload, MESH_CHANNEL};
use proto::keyring::Keyring;
use proto::keys::Identity;
use serde_json::Value;
use tokio::sync::{mpsc, watch, Notify};

use super::mirror::{Applied, Mirror};
use super::{frames, HubState, MeshState};
use crate::config::Config;
use crate::store::{now_ms, NodeRow, Store};
use crate::stream::{Bus, Frame};

const OUTBOX_BATCH: i64 = 50;
const PULL_LIMIT: usize = 200;
const SEEN_RETENTION_MS: i64 = 30 * 86_400_000;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("hub unreachable: {0}")]
    Transport(String),
    #[error("hub refused ({status}): {body}")]
    Refused { status: u16, body: String },
    #[error("{0}")]
    Local(String),
}

pub struct MeshClient {
    identity: Arc<Identity>,
    hub_url: String,
    store: Arc<Store>,
    bus: Bus,
    cfg: Arc<Config>,
    http: reqwest::Client,
    state: watch::Sender<MeshState>,
    mirror: Mirror,
    /// Wakes the pull loop (a poke, or a local reason to check).
    pull_wake: Notify,
    /// Wakes the outbox drain (a frame was queued).
    drain_wake: Notify,
    delivered: AtomicUsize,
    undecryptable: AtomicU64,
    /// Peer node id → X25519 public key hex, from the member list and hellos.
    peers: Mutex<HashMap<String, String>>,
}

impl MeshClient {
    pub fn new(
        identity: Identity,
        hub_url: &str,
        store: Arc<Store>,
        bus: Bus,
        cfg: Arc<Config>,
    ) -> Arc<Self> {
        let hub_url = hub_url.trim_end_matches('/').to_string();
        let self_id = identity.node_id();
        let (state, _) = watch::channel(MeshState {
            hub: HubState::Unreachable { since_ms: now_ms() },
            hub_url: Some(hub_url.clone()),
            node_id: self_id.clone(),
            fingerprint: proto::enroll::fingerprint_hex(&self_id),
            ..Default::default()
        });
        Arc::new(Self {
            identity: Arc::new(identity),
            hub_url,
            store: store.clone(),
            bus: bus.clone(),
            cfg,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("http client"),
            state,
            mirror: Mirror {
                store,
                bus,
                self_id,
            },
            pull_wake: Notify::new(),
            drain_wake: Notify::new(),
            delivered: AtomicUsize::new(0),
            undecryptable: AtomicU64::new(0),
            peers: Mutex::new(HashMap::new()),
        })
    }

    pub fn node_id(&self) -> String {
        self.identity.node_id()
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn state(&self) -> watch::Receiver<MeshState> {
        self.state.subscribe()
    }

    pub fn snapshot(&self) -> MeshState {
        let mut s = self.state.borrow().clone();
        s.queued = self.store.outbox_len().unwrap_or(0);
        s.delivered_since_reconnect = self.delivered.load(Ordering::Relaxed);
        s.undecryptable = self.undecryptable.load(Ordering::Relaxed);
        s
    }

    /// Start the background loops. Returns the sender the bus should tap.
    pub fn spawn(self: &Arc<Self>) -> mpsc::Sender<Frame> {
        let (tap_tx, tap_rx) = mpsc::channel::<Frame>(1024);
        let c = self.clone();
        tokio::spawn(async move { c.tap_loop(tap_rx).await });
        let c = self.clone();
        tokio::spawn(async move { c.drain_loop().await });
        let c = self.clone();
        tokio::spawn(async move { c.pull_loop().await });
        let c = self.clone();
        tokio::spawn(async move { c.sse_loop().await });
        let c = self.clone();
        tokio::spawn(async move { c.heartbeat_loop().await });
        let c = self.clone();
        tokio::spawn(async move { c.presence_loop().await });
        tap_tx
    }

    // ------------------------------------------------------------ outbound

    /// Convert a locally published frame into sealed envelopes in the outbox.
    pub fn on_frame(&self, frame: &Frame) {
        for (channel, payload) in frames::to_payloads(frame, &self.store, &self.node_id()) {
            if let Err(e) = self.enqueue(&channel, None, &payload) {
                tracing::warn!(channel = %channel, error = %e, "frame not queued for the mesh");
            }
        }
    }

    /// Seal a payload under the channel's newest epoch and queue it.
    pub fn enqueue(
        &self,
        channel: &str,
        recipient: Option<&str>,
        payload: &Payload,
    ) -> Result<(), HubError> {
        let ring = self.keyring(channel)?;
        let env =
            Envelope::seal_channel(&self.identity, channel, recipient, &ring, payload, now_ms())
                .map_err(|e| HubError::Local(e.to_string()))?;
        self.push_envelope(&env)
    }

    /// Seal a payload to one peer and queue it. The peer's X25519 key must be
    /// known (from the member list or its hello).
    pub fn enqueue_direct(
        &self,
        channel: &str,
        recipient: &str,
        payload: &Payload,
    ) -> Result<(), HubError> {
        let x = self
            .peers
            .lock()
            .unwrap()
            .get(recipient)
            .cloned()
            .ok_or_else(|| HubError::Local(format!("no sealing key known for {recipient}")))?;
        let pk = proto::keys::key32(&x)
            .map(x25519_dalek_public)
            .ok_or_else(|| HubError::Local("peer sealing key is malformed".into()))?;
        let env = Envelope::seal_direct(&self.identity, channel, recipient, &pk, payload, now_ms())
            .map_err(|e| HubError::Local(e.to_string()))?;
        self.push_envelope(&env)
    }

    fn push_envelope(&self, env: &Envelope) -> Result<(), HubError> {
        let json = serde_json::to_string(env).map_err(|e| HubError::Local(e.to_string()))?;
        self.store
            .outbox_push(&env.channel, &json)
            .map_err(|e| HubError::Local(e.to_string()))?;
        self.drain_wake.notify_one();
        Ok(())
    }

    fn keyring(&self, channel: &str) -> Result<Keyring, HubError> {
        let row = self
            .store
            .channel_get(channel)
            .map_err(|e| HubError::Local(e.to_string()))?
            .ok_or_else(|| HubError::Local(format!("no keyring for channel {channel}")))?;
        Keyring::from_bytes(&row.keyring).map_err(|e| HubError::Local(e.to_string()))
    }

    /// Post queued envelopes in order until the outbox is empty or the hub
    /// refuses. Returns how many were delivered.
    pub async fn drain_once(&self) -> Result<usize, HubError> {
        let mut delivered = 0;
        loop {
            let batch = self
                .store
                .outbox_peek(OUTBOX_BATCH)
                .map_err(|e| HubError::Local(e.to_string()))?;
            if batch.is_empty() {
                return Ok(delivered);
            }
            for (id, channel, envelope) in batch {
                match self.post("/v0/frames", envelope.into_bytes()).await {
                    Ok(_) => {
                        let _ = self.store.outbox_delete(id);
                        delivered += 1;
                        self.delivered.fetch_add(1, Ordering::Relaxed);
                        self.set_state_ok();
                    }
                    // The hub understood and said no: a frame it will never
                    // take (bad signature, not a member of that channel). Drop
                    // it rather than block everything behind it.
                    Err(HubError::Refused { status, body }) if status < 500 => {
                        tracing::warn!(channel = %channel, status, body = %body, "hub refused a frame; dropped");
                        let _ = self.store.outbox_delete(id);
                        self.note_refusal(format!("{status}: {body}"));
                    }
                    Err(e) => {
                        self.set_state_down(e.to_string());
                        return Err(e);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------- inbound

    /// Pull every member channel from its cursor. Returns frames applied.
    pub async fn pull_once(&self) -> Result<usize, HubError> {
        let channels = self
            .store
            .channel_list()
            .map_err(|e| HubError::Local(e.to_string()))?;
        let mut applied = 0;
        for ch in channels {
            applied += self.pull_channel(&ch.name).await?;
        }
        let _ = self.store.seen_prune(now_ms() - SEEN_RETENTION_MS);
        Ok(applied)
    }

    async fn pull_channel(&self, channel: &str) -> Result<usize, HubError> {
        let mut applied = 0;
        loop {
            let after = self
                .store
                .cursor_get(channel)
                .map_err(|e| HubError::Local(e.to_string()))?;
            let path = format!("/v0/frames?channel={channel}&after={after}&limit={PULL_LIMIT}");
            let page: Value = match self.get_json(&path).await {
                Ok(v) => v,
                Err(HubError::Refused { status: 410, body }) => {
                    // Behind retention. Jump to what the hub still holds; the
                    // owners' periodic snapshots fill in what was missed.
                    let oldest = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|v| v["oldest"].as_u64())
                        .unwrap_or(after + 1);
                    tracing::warn!(
                        channel,
                        after,
                        oldest,
                        "cursor behind hub retention; resyncing"
                    );
                    let _ = self.store.cursor_set(channel, oldest.saturating_sub(1));
                    continue;
                }
                Err(HubError::Refused { status: 403, .. }) => {
                    // Not (yet) a member of this channel on the hub; nothing to
                    // pull and nothing to alarm about.
                    return Ok(0);
                }
                Err(e) => return Err(e),
            };
            self.set_state_ok();
            let frames = page["frames"].as_array().cloned().unwrap_or_default();
            let mut last = after;
            for item in &frames {
                let Some(seq) = item["seq"].as_u64() else {
                    continue;
                };
                last = seq;
                if let Ok(env) = serde_json::from_value::<Envelope>(item["envelope"].clone()) {
                    if self.ingest(&env) {
                        applied += 1;
                    }
                }
            }
            if last > after {
                let _ = self.store.cursor_set(channel, last);
            }
            if page["next"].is_null() || frames.is_empty() {
                return Ok(applied);
            }
        }
    }

    /// Verify, dedupe, open, and apply one envelope. `true` if it changed
    /// local state.
    fn ingest(&self, env: &Envelope) -> bool {
        let sender_key = match env.verify() {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(id = %env.id, error = %e, "frame failed verification; dropped");
                return false;
            }
        };
        let sender = hex::encode(sender_key);
        if sender == self.node_id() {
            return false;
        }
        if !self.store.seen_insert(&env.id, now_ms()).unwrap_or(true) {
            return false;
        }
        let payload = if env.is_direct() {
            env.open_direct(&self.identity)
        } else {
            match self.keyring(&env.channel) {
                Ok(ring) => env.open_channel(&ring, &self.identity),
                Err(_) => Err(proto::frame::FrameError::UnknownEpoch("no keyring".into())),
            }
        };
        let payload = match payload {
            Ok(p) => p,
            Err(proto::frame::FrameError::NotRecipient) => return false,
            Err(e) => {
                self.undecryptable.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(channel = %env.channel, sender = %sender, error = %e, "frame could not be opened");
                return false;
            }
        };
        // Learn a peer's sealing key from its hello, so direct frames to it
        // need no member-list round trip.
        if let Payload::Hello { node, .. } = &payload {
            if let Some(x) = node["x25519_pub"].as_str() {
                self.peers
                    .lock()
                    .unwrap()
                    .insert(sender.clone(), x.to_string());
            }
        }
        match self.mirror.apply(&sender, &env.channel, payload) {
            Applied::Stored => true,
            Applied::Duplicate => false,
            Applied::Impersonation => {
                tracing::warn!(sender = %sender, "peer spoke for another node; dropped");
                self.note_refusal(format!("{sender} spoke for another node"));
                false
            }
            Applied::Unhandled(kind) => {
                tracing::debug!(kind, sender = %sender, "payload kind not handled here");
                false
            }
            Applied::Malformed => {
                tracing::warn!(sender = %sender, "malformed payload; dropped");
                false
            }
        }
    }

    // ------------------------------------------------------------ presence

    /// Announce this node on `@mesh`: posted directly, not queued, because a
    /// stale hello is worse than none.
    pub async fn hello(&self) -> Result<(), HubError> {
        let Some(row) = self
            .store
            .get_node(&self.node_id())
            .map_err(|e| HubError::Local(e.to_string()))?
        else {
            return Err(HubError::Local("no self node row yet".into()));
        };
        let ring = self.keyring(MESH_CHANNEL)?;
        let payload = Payload::Hello {
            node: row.to_json(),
            contract: proto::CONTRACT_VERSION,
        };
        let env = Envelope::seal_channel(
            &self.identity,
            MESH_CHANNEL,
            None,
            &ring,
            &payload,
            now_ms(),
        )
        .map_err(|e| HubError::Local(e.to_string()))?;
        let body = serde_json::to_vec(&env).map_err(|e| HubError::Local(e.to_string()))?;
        match self.post("/v0/frames", body).await {
            Ok(_) => {
                self.set_state_ok();
                Ok(())
            }
            Err(e) => {
                self.set_state_down(e.to_string());
                Err(e)
            }
        }
    }

    /// This node's open state per channel, queued for peers.
    pub fn send_snapshots(&self) {
        for (channel, payload) in frames::snapshots(&self.store, &self.node_id()) {
            let _ = self.enqueue(&channel, None, &payload);
        }
    }

    /// Refresh the member list: peers' sealing keys and channel bindings, and
    /// a placeholder node row for any member we have not heard from.
    pub async fn refresh_members(&self) -> Result<usize, HubError> {
        let list: Vec<Value> = serde_json::from_value(self.get_json("/v0/members").await?)
            .map_err(|e| HubError::Local(e.to_string()))?;
        let me = self.node_id();
        let mut n = 0;
        for m in &list {
            let Some(id) = m["node_id"].as_str() else {
                continue;
            };
            let channels: Vec<String> = m["channels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let _ = self.store.node_channels_set(id, &channels);
            if id == me {
                continue;
            }
            n += 1;
            if let Some(x) = m["x25519_pub"].as_str().filter(|x| !x.is_empty()) {
                self.peers
                    .lock()
                    .unwrap()
                    .insert(id.to_string(), x.to_string());
            }
            if self.store.get_node(id).ok().flatten().is_none() {
                let _ = self.store.put_node(&NodeRow {
                    id: id.to_string(),
                    name: m["name"].as_str().unwrap_or("").to_string(),
                    state: "unknown".into(),
                    failed_check: None,
                    failed_detail: None,
                    harness_id: String::new(),
                    harness_pinned: String::new(),
                    harness_found: None,
                    models_json: None,
                    checked_at_ms: None,
                    is_self: 0,
                    x25519_pub: m["x25519_pub"].as_str().map(String::from),
                    last_seen_ms: None,
                    reachable: 0,
                });
            }
        }
        self.set_state_ok();
        Ok(n)
    }

    /// Recompute which peers count as reachable: the hub is up and the peer
    /// said hello within three heartbeats. Publishes a node frame per change.
    pub fn presence_tick(&self, now: i64) {
        let hub_up = matches!(self.state.borrow().hub, HubState::Connected);
        let window = 3 * self.cfg.mesh.heartbeat_secs as i64 * 1000;
        for n in self.store.list_nodes().unwrap_or_default() {
            if n.is_self != 0 {
                continue;
            }
            let fresh = n.last_seen_ms.is_some_and(|t| now - t < window);
            let reachable = hub_up && fresh;
            if self.store.set_reachable(&n.id, reachable).unwrap_or(false) {
                if let Ok(Some(row)) = self.store.get_node(&n.id) {
                    self.bus.publish_untapped(Frame::Node(row.to_json()));
                }
            }
        }
    }

    pub fn wake_pull(&self) {
        self.pull_wake.notify_one();
    }

    // --------------------------------------------------------------- loops

    async fn tap_loop(&self, mut rx: mpsc::Receiver<Frame>) {
        while let Some(frame) = rx.recv().await {
            self.on_frame(&frame);
        }
    }

    async fn drain_loop(&self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.drain_once().await {
                Ok(_) => {
                    backoff = Duration::from_secs(1);
                    tokio::select! {
                        _ = self.drain_wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    }
                }
                Err(_) => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    async fn pull_loop(&self) {
        let poll = Duration::from_secs(self.cfg.mesh.poll_secs.max(1));
        loop {
            if let Err(e) = self.pull_once().await {
                self.set_state_down(e.to_string());
            }
            tokio::select! {
                _ = self.pull_wake.notified() => {}
                _ = tokio::time::sleep(poll) => {}
            }
        }
    }

    async fn sse_loop(&self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.sse_once().await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(e) => {
                    tracing::debug!(error = %e, "hub event stream ended");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// One connection to `/v0/events`; every `frames`/`sync` event wakes a pull.
    async fn sse_once(&self) -> Result<(), HubError> {
        let path = "/v0/events";
        let ts = now_unix();
        let mut req = self.http.get(format!("{}{}", self.hub_url, path));
        for (k, v) in signed_headers(&self.identity, "GET", path, b"", ts) {
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
        self.set_state_ok();
        let mut stream = res.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| HubError::Transport(e.to_string()))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(i) = buf.find('\n') {
                let line = buf[..i].trim().to_string();
                buf.drain(..=i);
                if line == "event: frames" || line == "event: sync" {
                    self.pull_wake.notify_one();
                }
            }
        }
        Ok(())
    }

    async fn heartbeat_loop(&self) {
        let every = Duration::from_secs(self.cfg.mesh.heartbeat_secs.max(5));
        let mut n: u64 = 0;
        loop {
            if n.is_multiple_of(5) {
                let _ = self.refresh_members().await;
            }
            if self.hello().await.is_ok() && n.is_multiple_of(10) {
                self.send_snapshots();
            }
            n += 1;
            tokio::time::sleep(every).await;
        }
    }

    async fn presence_loop(&self) {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            self.presence_tick(now_ms());
        }
    }

    // ----------------------------------------------------------- transport

    async fn post(&self, path: &str, body: Vec<u8>) -> Result<Value, HubError> {
        let ts = now_unix();
        let mut req = self
            .http
            .post(format!("{}{}", self.hub_url, path))
            .header("content-type", "application/json");
        for (k, v) in signed_headers(&self.identity, "POST", path, &body, ts) {
            req = req.header(k, v);
        }
        let res = req
            .body(body)
            .send()
            .await
            .map_err(|e| HubError::Transport(e.to_string()))?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(HubError::Refused {
                status: status.as_u16(),
                body: text,
            });
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn get_json(&self, path: &str) -> Result<Value, HubError> {
        let ts = now_unix();
        let mut req = self.http.get(format!("{}{}", self.hub_url, path));
        for (k, v) in signed_headers(&self.identity, "GET", path, b"", ts) {
            req = req.header(k, v);
        }
        let res = req
            .send()
            .await
            .map_err(|e| HubError::Transport(e.to_string()))?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(HubError::Refused {
                status: status.as_u16(),
                body: text,
            });
        }
        serde_json::from_str(&text).map_err(|e| HubError::Transport(e.to_string()))
    }

    /// A signed GET for one-shot commands that have no running client.
    pub async fn get_once(
        identity: &Identity,
        hub_url: &str,
        path: &str,
    ) -> Result<Value, HubError> {
        let hub_url = hub_url.trim_end_matches('/');
        let ts = now_unix();
        let mut req = reqwest::Client::new().get(format!("{hub_url}{path}"));
        for (k, v) in signed_headers(identity, "GET", path, b"", ts) {
            req = req.header(k, v);
        }
        let res = req
            .send()
            .await
            .map_err(|e| HubError::Transport(e.to_string()))?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(HubError::Refused {
                status: status.as_u16(),
                body: text,
            });
        }
        serde_json::from_str(&text).map_err(|e| HubError::Transport(e.to_string()))
    }

    // --------------------------------------------------------------- state

    fn set_state_ok(&self) {
        let now = now_ms();
        self.state.send_if_modified(|s| {
            let was_down = !matches!(s.hub, HubState::Connected);
            s.hub = HubState::Connected;
            s.last_ok_ms = Some(now);
            s.last_error = None;
            if was_down {
                self.delivered.store(0, Ordering::Relaxed);
                self.bus
                    .publish_untapped(Frame::Mesh(serde_json::json!(self.snapshot_of(s))));
                self.pull_wake.notify_one();
                self.drain_wake.notify_one();
            }
            was_down
        });
    }

    fn set_state_down(&self, error: String) {
        let now = now_ms();
        self.state.send_if_modified(|s| {
            let was_up = matches!(s.hub, HubState::Connected);
            if was_up {
                s.hub = HubState::Unreachable { since_ms: now };
            }
            s.last_error = Some(error);
            if was_up {
                self.bus
                    .publish_untapped(Frame::Mesh(serde_json::json!(self.snapshot_of(s))));
                self.presence_tick(now);
            }
            was_up
        });
    }

    fn note_refusal(&self, what: String) {
        self.state.send_modify(|s| s.last_refusal = Some(what));
    }

    fn snapshot_of(&self, s: &MeshState) -> MeshState {
        let mut out = s.clone();
        out.queued = self.store.outbox_len().unwrap_or(0);
        out.delivered_since_reconnect = self.delivered.load(Ordering::Relaxed);
        out.undecryptable = self.undecryptable.load(Ordering::Relaxed);
        out
    }
}

fn now_unix() -> u64 {
    (now_ms() / 1000).max(0) as u64
}

fn x25519_dalek_public(k: [u8; 32]) -> x25519_dalek::PublicKey {
    x25519_dalek::PublicKey::from(k)
}
