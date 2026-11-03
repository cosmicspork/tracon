//! Answering the node's requests.

use std::future::poll_fn;
use std::sync::Arc;

use bytes::Bytes;
use h2::server::SendResponse;
use h2::RecvStream;
use http::{Method, Request, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::AsyncWriteExt as _;
use tokio_util::sync::CancellationToken;

use super::procs::{OutputError, ProcError, Procs, Stream};
use super::services::{ServiceError, Supervisor};
use super::tunnels::{TunnelError, Tunnels};
use crate::files::{self, FilesError};
use crate::io::{H2Reader, H2Writer};
use crate::protocol::{
    self, path, ErrorBody, Event, ExportRequest, ImportRequest, ProcRef, ResizeRequest,
    ServicesStart, ServicesStop, SignalRequest, SpawnSpec, TunnelListen, TunnelListening,
    TunnelRef, ARGS_HEADER, MAX_JSON_BYTES,
};

pub struct State {
    pub procs: Procs,
    pub tunnels: Tunnels,
    pub services: Supervisor,
    /// Cancelled when the node asks the sandbox to shut down.
    pub shutdown: CancellationToken,
}

struct Failure(StatusCode, String);

impl Failure {
    fn bad(message: impl Into<String>) -> Self {
        Failure(StatusCode::BAD_REQUEST, message.into())
    }
}

impl From<ProcError> for Failure {
    fn from(error: ProcError) -> Self {
        let status = match error {
            ProcError::NotFound(_) => StatusCode::NOT_FOUND,
            ProcError::Taken => StatusCode::CONFLICT,
            ProcError::Io(_) => StatusCode::UNPROCESSABLE_ENTITY,
        };
        Failure(status, error.to_string())
    }
}

impl From<OutputError> for Failure {
    fn from(error: OutputError) -> Self {
        let status = match error {
            OutputError::Gone(_) => StatusCode::GONE,
            OutputError::Beyond(_) => StatusCode::RANGE_NOT_SATISFIABLE,
            OutputError::Failed(_) => StatusCode::UNPROCESSABLE_ENTITY,
        };
        Failure(status, error.to_string())
    }
}

impl From<TunnelError> for Failure {
    fn from(error: TunnelError) -> Self {
        let status = match error {
            TunnelError::Exists(_) => StatusCode::CONFLICT,
            TunnelError::NotFound(_) | TunnelError::Closed(_) => StatusCode::NOT_FOUND,
            TunnelError::Io(_) => StatusCode::UNPROCESSABLE_ENTITY,
        };
        Failure(status, error.to_string())
    }
}

impl From<ServiceError> for Failure {
    fn from(error: ServiceError) -> Self {
        let status = match error {
            ServiceError::Exists(_) => StatusCode::CONFLICT,
            ServiceError::NotFound(_) => StatusCode::NOT_FOUND,
            ServiceError::Invalid(..) => StatusCode::BAD_REQUEST,
        };
        Failure(status, error.to_string())
    }
}

impl From<FilesError> for Failure {
    fn from(error: FilesError) -> Self {
        Failure(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
    }
}

fn reply<T: Serialize>(respond: &mut SendResponse<Bytes>, status: StatusCode, body: &T) {
    let body = serde_json::to_vec(body).expect("replies serialize");
    let response = Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(())
        .expect("static response");
    if let Ok(mut stream) = respond.send_response(response, false) {
        let _ = stream.send_data(Bytes::from(body), true);
    }
}

fn reply_ok<T: Serialize>(respond: &mut SendResponse<Bytes>, body: &T) {
    reply(respond, StatusCode::OK, body);
}

fn start_stream(respond: &mut SendResponse<Bytes>) -> Result<H2Writer, h2::Error> {
    let response = Response::builder()
        .status(StatusCode::OK)
        .body(())
        .expect("static response");
    Ok(H2Writer::new(respond.send_response(response, false)?))
}

fn args<T: DeserializeOwned>(request: &Request<RecvStream>) -> Result<T, Failure> {
    let value = request
        .headers()
        .get(ARGS_HEADER)
        .ok_or_else(|| Failure::bad("missing arguments"))?;
    protocol::decode_args(value.as_bytes()).map_err(Failure::bad)
}

async fn json<T: DeserializeOwned>(body: &mut RecvStream) -> Result<T, Failure> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|e| Failure::bad(e.to_string()))?;
        let _ = body.flow_control().release_capacity(chunk.len());
        if bytes.len() + chunk.len() > MAX_JSON_BYTES {
            return Err(Failure(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body too large".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|e| Failure::bad(e.to_string()))
}

/// Resolves when the node gives up on the request.
async fn reset(respond: &mut SendResponse<Bytes>) {
    let _ = poll_fn(|cx| respond.poll_reset(cx)).await;
}

#[derive(Serialize)]
struct Done {
    ok: bool,
}

const DONE: Done = Done { ok: true };

/// Answer one request. `gone` is cancelled when the connection it came on
/// is lost, so nothing waits on a stream that can no longer be answered.
pub async fn handle(
    state: Arc<State>,
    gone: CancellationToken,
    request: Request<RecvStream>,
    mut respond: SendResponse<Bytes>,
) {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    if let Err(Failure(status, error)) =
        route(&state, &gone, &method, &path, request, &mut respond).await
    {
        reply(&mut respond, status, &ErrorBody { error });
    }
}

async fn route(
    state: &Arc<State>,
    gone: &CancellationToken,
    method: &Method,
    path: &str,
    request: Request<RecvStream>,
    respond: &mut SendResponse<Bytes>,
) -> Result<(), Failure> {
    match (method, path) {
        (&Method::POST, path::SPAWN) => {
            let spec: SpawnSpec = json(&mut request.into_body()).await?;
            let spawned = state.procs.spawn(&spec)?;
            reply_ok(respond, &spawned);
        }
        (&Method::POST, path::STDIN) => {
            let target: ProcRef = args(&request)?;
            let input = state.procs.input(target.id)?;
            let mut stdin = tokio::select! {
                stdin = input.lock() => stdin,
                _ = gone.cancelled() => return Ok(()),
            };
            if let Some(why) = &stdin.closed {
                reply_ok(respond, &serde_json::json!({ "closed": why }));
                return Ok(());
            }
            let from = target.from.unwrap_or(stdin.written);
            if from > stdin.written {
                return Err(Failure(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    format!(
                        "stdin from byte {from}: only {} were written",
                        stdin.written
                    ),
                ));
            }
            // What a body resent over a new connection repeats.
            let mut skip = stdin.written - from;
            let mut body = request.into_body();
            loop {
                let next = tokio::select! {
                    next = body.data() => next,
                    // Lost with the connection: stdin stays open for the
                    // node to carry on over the next.
                    _ = gone.cancelled() => return Ok(()),
                };
                let mut chunk = match next {
                    None => {
                        stdin.close("the node ended it").await;
                        reply_ok(respond, &serde_json::json!({ "bytes": stdin.written }));
                        return Ok(());
                    }
                    Some(Ok(chunk)) => chunk,
                    Some(Err(error)) => {
                        // The node ending the stream on purpose closes stdin
                        // as a pipe's writer closing it would; a connection
                        // that failed leaves it open.
                        if error.is_reset() && error.is_remote() {
                            stdin.close("the node reset it").await;
                        }
                        return Ok(());
                    }
                };
                let _ = body.flow_control().release_capacity(chunk.len());
                let repeated = (skip.min(chunk.len() as u64)) as usize;
                skip -= repeated as u64;
                let _ = chunk.split_to(repeated);
                if chunk.is_empty() {
                    continue;
                }
                let written = tokio::select! {
                    written = stdin.write(&chunk) => written,
                    _ = gone.cancelled() => return Ok(()),
                };
                if let Err(error) = written {
                    // The process closed its stdin. The node's writer then
                    // sees its stream end, as a pipe writer sees EPIPE.
                    stdin.close(error.to_string()).await;
                    reply_ok(respond, &serde_json::json!({ "closed": error.to_string() }));
                    return Ok(());
                }
            }
        }
        (&Method::GET, path::STDOUT | path::STDERR) => {
            let target: ProcRef = args(&request)?;
            let which = if path == path::STDOUT {
                Stream::Stdout
            } else {
                Stream::Stderr
            };
            let output = state.procs.output(target.id, which)?;
            let mut from = target.from.unwrap_or(0);
            output.check(from)?;
            let attachment = output.attach();
            let Ok(mut writer) = start_stream(respond) else {
                return Ok(());
            };
            loop {
                let chunk = tokio::select! {
                    chunk = output.read_at(from) => chunk,
                    _ = gone.cancelled() => return Ok(()),
                };
                if !output.is_current(attachment) {
                    writer.reset();
                    return Ok(());
                }
                match chunk {
                    Ok(chunk) if chunk.is_empty() => {
                        let _ = writer.shutdown().await;
                        return Ok(());
                    }
                    Ok(chunk) => {
                        let sent = tokio::select! {
                            sent = writer.write_all(&chunk) => sent,
                            _ = gone.cancelled() => return Ok(()),
                        };
                        if sent.is_err() {
                            return Ok(());
                        }
                        from += chunk.len() as u64;
                        output.sent(from);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "output stream failed");
                        writer.reset();
                        return Ok(());
                    }
                }
            }
        }
        (&Method::POST, path::SIGNAL) => {
            let signal: SignalRequest = json(&mut request.into_body()).await?;
            state.procs.signal(signal.id, signal.signal, signal.group)?;
            reply_ok(respond, &DONE);
        }
        (&Method::POST, path::RESIZE) => {
            let resize: ResizeRequest = json(&mut request.into_body()).await?;
            state.procs.resize(resize.id, resize.size)?;
            reply_ok(respond, &DONE);
        }
        (&Method::GET, path::WAIT) => {
            let target: ProcRef = args(&request)?;
            tokio::select! {
                status = state.procs.wait(target.id) => reply_ok(respond, &status?),
                _ = reset(respond) => {}
                _ = gone.cancelled() => {}
            }
        }
        (&Method::POST, path::IMPORT) => {
            let import: ImportRequest = args(&request)?;
            tokio::fs::create_dir_all(&import.dir)
                .await
                .map_err(|e| Failure(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
            let reader = H2Reader::new(request.into_body());
            let mount = Some(import.dir.clone());
            let summary =
                files::unpack_async(reader, import.dir, mount, import.limits.unwrap_or_default())
                    .await?;
            reply_ok(respond, &summary);
        }
        (&Method::GET, path::EXPORT) => {
            let export: ExportRequest = args(&request)?;
            let metadata = tokio::fs::symlink_metadata(&export.dir)
                .await
                .map_err(|e| Failure(StatusCode::NOT_FOUND, e.to_string()))?;
            if !metadata.is_dir() {
                return Err(Failure::bad("not a directory"));
            }
            let Ok(writer) = start_stream(respond) else {
                return Ok(());
            };
            let (packed, mut writer) =
                files::pack_async(export.dir, export.exclusions, writer).await;
            match packed {
                Ok(_) => {
                    let _ = writer.shutdown().await;
                }
                Err(error) => {
                    tracing::warn!(%error, "export failed");
                    writer.reset();
                }
            }
        }
        (&Method::POST, path::TUNNEL_LISTEN) => {
            let listen: TunnelListen = json(&mut request.into_body()).await?;
            let bound = state.tunnels.listen(&listen.name, &listen.listen).await?;
            reply_ok(respond, &TunnelListening { bound });
        }
        (&Method::POST, path::TUNNEL_ACCEPT) => {
            let tunnel: TunnelRef = args(&request)?;
            let connection = tokio::select! {
                connection = state.tunnels.accept(&tunnel.name) => connection?,
                _ = reset(respond) => return Ok(()),
                _ = gone.cancelled() => return Ok(()),
            };
            let Ok(mut to_node) = start_stream(respond) else {
                return Ok(());
            };
            let mut from_node = H2Reader::new(request.into_body());
            let (mut read, mut write) = tokio::io::split(connection);
            let inbound = async {
                let copied = tokio::io::copy(&mut from_node, &mut write).await;
                let _ = write.shutdown().await;
                copied
            };
            let outbound = async {
                match tokio::io::copy(&mut read, &mut to_node).await {
                    Ok(_) => {
                        let _ = to_node.shutdown().await;
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            };
            let _ = tokio::join!(inbound, outbound);
        }
        (&Method::POST, path::TUNNEL_CLOSE) => {
            let tunnel: TunnelRef = json(&mut request.into_body()).await?;
            state.tunnels.close(&tunnel.name)?;
            reply_ok(respond, &DONE);
        }
        (&Method::POST, path::SERVICES_START) => {
            let start: ServicesStart = json(&mut request.into_body()).await?;
            state.services.start(start.services)?;
            reply_ok(respond, &DONE);
        }
        (&Method::GET, path::SERVICES_STATUS) => {
            reply_ok(respond, &state.services.status());
        }
        (&Method::POST, path::SERVICES_STOP) => {
            let stop: ServicesStop = json(&mut request.into_body()).await?;
            state.services.stop(&stop.names).await?;
            reply_ok(respond, &DONE);
        }
        (&Method::GET, path::EVENTS) => {
            let mut events = state.services.subscribe();
            let Ok(mut writer) = start_stream(respond) else {
                return Ok(());
            };
            loop {
                let received = tokio::select! {
                    received = events.recv() => received,
                    _ = gone.cancelled() => return Ok(()),
                };
                let event = match received {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        Event::Lagged { missed }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let mut line = serde_json::to_vec(&event).expect("events serialize");
                line.push(b'\n');
                if writer.write_all(&line).await.is_err() {
                    return Ok(());
                }
            }
            let _ = writer.shutdown().await;
        }
        (&Method::POST, path::SHUTDOWN) => {
            reply_ok(respond, &DONE);
            state.shutdown.cancel();
        }
        _ => {
            return Err(Failure(
                StatusCode::NOT_FOUND,
                format!("no route {method} {path}"),
            ))
        }
    }
    Ok(())
}
