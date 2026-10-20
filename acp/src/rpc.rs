//! A JSON-RPC peer over a pair of byte streams: outgoing requests with awaited
//! responses, outgoing notifications, and a channel of inbound requests and
//! notifications to service. The node holds one over a harness's stdio; the
//! harness holds one over its own stdin and stdout.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot, Mutex},
};

/// The peer's write half. A child's stdin, a pod attach or a process's own
/// stdout look the same here.
pub type Writer = Box<dyn AsyncWrite + Send + Unpin>;
/// The peer's read half.
pub type Reader = Box<dyn AsyncRead + Send + Unpin>;

use crate::codec::{self, Id, Message, RpcError};

#[derive(Debug, thiserror::Error)]
pub enum RpcClientError {
    #[error("peer closed before responding")]
    PeerClosed,
    // The detail a harness puts in `data` is usually the only actionable part;
    // dropping it leaves the operator with "Internal error".
    #[error("rpc error {}: {}{}", .0.code, .0.message, detail(&.0.data))]
    Remote(RpcError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode: {0}")]
    Decode(String),
}

/// An inbound request or notification the peer received.
#[derive(Debug)]
pub enum Incoming {
    Request {
        id: Id,
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, RpcError>>,
    },
    Notification {
        method: String,
        params: Value,
    },
}

type Pending = Arc<Mutex<HashMap<Id, oneshot::Sender<Result<Value, RpcError>>>>>;

#[derive(Clone)]
pub struct Peer {
    stdin: Arc<Mutex<Writer>>,
    next_id: Arc<AtomicI64>,
    pending: Pending,
}

impl Peer {
    /// Wire a pair of streams into a peer. Returns the peer, a receiver of
    /// inbound requests/notifications, and the read-loop future (spawn it).
    pub fn new(
        stdin: Writer,
        stdout: Reader,
    ) -> (
        Self,
        mpsc::Receiver<Incoming>,
        impl std::future::Future<Output = ()>,
    ) {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (incoming_tx, incoming_rx) = mpsc::channel(256);
        let peer = Self {
            stdin: Arc::new(Mutex::new(stdin)),
            next_id: Arc::new(AtomicI64::new(1)),
            pending: pending.clone(),
        };
        let read = read_loop(stdout, pending, incoming_tx, peer.stdin.clone());
        (peer, incoming_rx, read)
    }

    pub async fn request<P: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, RpcClientError> {
        let id = Id::Num(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let line = codec::encode_request(&id, method, params);
        if let Err(e) = self.write_line(&line).await {
            self.pending.lock().await.remove(&id);
            return Err(e.into());
        }
        match rx.await {
            Ok(Ok(v)) => {
                serde_json::from_value(v).map_err(|e| RpcClientError::Decode(e.to_string()))
            }
            Ok(Err(e)) => Err(RpcClientError::Remote(e)),
            Err(_) => Err(RpcClientError::PeerClosed),
        }
    }

    pub async fn notify<P: Serialize>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<(), RpcClientError> {
        let line = codec::encode_notification(method, params);
        self.write_line(&line).await.map_err(Into::into)
    }

    async fn write_line(&self, line: &str) -> std::io::Result<()> {
        let mut w = self.stdin.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await
    }
}

async fn respond(stdin: &Arc<Mutex<Writer>>, id: &Id, result: Result<Value, RpcError>) {
    let line = codec::encode_response(id, result);
    let mut w = stdin.lock().await;
    let _ = w.write_all(line.as_bytes()).await;
    let _ = w.write_all(b"\n").await;
    let _ = w.flush().await;
}

async fn read_loop(
    stdout: Reader,
    pending: Pending,
    incoming: mpsc::Sender<Incoming>,
    stdin: Arc<Mutex<Writer>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let msg = match codec::decode(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(%line, error = %e, "undecodable acp line");
                continue;
            }
        };
        match msg {
            Message::Response { id, result } => {
                if let Some(tx) = pending.lock().await.remove(&id) {
                    let _ = tx.send(result);
                }
            }
            Message::Notification { method, params } => {
                let _ = incoming
                    .send(Incoming::Notification { method, params })
                    .await;
            }
            Message::Request { id, method, params } => {
                let (reply, wait) = oneshot::channel();
                if incoming
                    .send(Incoming::Request {
                        id: id.clone(),
                        method,
                        params,
                        reply,
                    })
                    .await
                    .is_err()
                {
                    respond(&stdin, &id, Err(RpcError::method_not_found("no handler"))).await;
                    continue;
                }
                let stdin = stdin.clone();
                tokio::spawn(async move {
                    let result = wait
                        .await
                        .unwrap_or_else(|_| Err(RpcError::method_not_found("handler dropped")));
                    respond(&stdin, &id, result).await;
                });
            }
        }
    }
    // Peer's stdout closed: fail every awaiting request.
    let mut pend = pending.lock().await;
    for (_, tx) in pend.drain() {
        let _ = tx.send(Err(RpcError {
            code: 0,
            message: "peer closed".into(),
            data: None,
        }));
    }
}

fn detail(data: &Option<Value>) -> String {
    match data {
        Some(Value::Object(map)) => map
            .get("details")
            .and_then(Value::as_str)
            .map(|d| format!(" ({d})"))
            .unwrap_or_default(),
        Some(v) => format!(" ({v})"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Two peers back to back: requests, responses and notifications cross in
    /// both directions, as between the node and `tracon-agent`.
    #[tokio::test]
    async fn two_peers_talk_both_ways() {
        let (a_io, b_io) = tokio::io::duplex(4096);
        let (a_read, a_write) = tokio::io::split(a_io);
        let (b_read, b_write) = tokio::io::split(b_io);
        let (a, mut a_in, a_loop) = Peer::new(Box::new(a_write), Box::new(a_read));
        let (b, mut b_in, b_loop) = Peer::new(Box::new(b_write), Box::new(b_read));
        tokio::spawn(a_loop);
        tokio::spawn(b_loop);
        tokio::spawn(async move {
            while let Some(m) = b_in.recv().await {
                if let Incoming::Request {
                    method,
                    params,
                    reply,
                    ..
                } = m
                {
                    let _ = match method.as_str() {
                        "double" => reply.send(Ok(json!(params["n"].as_i64().unwrap() * 2))),
                        other => reply.send(Err(RpcError::method_not_found(other))),
                    };
                }
            }
        });
        let n: i64 = a.request("double", &json!({"n": 21})).await.unwrap();
        assert_eq!(n, 42);
        assert!(matches!(
            a.request::<_, Value>("nope", &json!({})).await,
            Err(RpcClientError::Remote(e)) if e.code == RpcError::METHOD_NOT_FOUND
        ));
        b.notify("hello", &json!({"x": 1})).await.unwrap();
        match a_in.recv().await.unwrap() {
            Incoming::Notification { method, params } => {
                assert_eq!(method, "hello");
                assert_eq!(params["x"], 1);
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }
}
