//! The node's side: accept a sandbox's connection, check its token, and drive
//! it through [`SandboxConn`].
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use tracon_sandbox::carrier::Listener;
//! use tracon_sandbox::node::{SandboxServer, ServerOptions};
//! use tracon_sandbox::protocol::SpawnSpec;
//! use tracon_sandbox::tls::NodeIdentity;
//!
//! let identity = NodeIdentity::generate()?;
//! let server = SandboxServer::new(&identity, ServerOptions::default())?;
//! let token = server.tokens().issue("run-1");
//! // ...start the sandbox with the listener's address, `identity.pin()` and
//! // `token` in its environment...
//! let listener = Listener::Tcp(tokio::net::TcpListener::bind("127.0.0.1:0").await?);
//! let conn = server.accept(listener.accept().await?).await?;
//! let process = conn.spawn(SpawnSpec::new(["true"])).await?;
//! assert!(process.wait().await?.success());
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use h2::client::{ResponseFuture, SendRequest};
use h2::RecvStream;
use http::{Method, Request, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::carrier::{BoxedIo, Io, Listener};
use crate::files::{self, FilesError, Limits};
use crate::inside::{
    keepalive, read_frame, write_frame, CONNECTION_WINDOW, MAX_FRAME, STREAM_WINDOW,
};
use crate::io::{H2Reader, H2Writer};
use crate::protocol::{
    self, path, Ack, ErrorBody, Event, Exclusions, ExitStatus, ExportRequest, Hello, ImportRequest,
    ListenAddr, ProcRef, ResizeRequest, SandboxInfo, ServiceSpec, ServiceStatus, ServicesStart,
    ServicesStop, SignalRequest, SpawnSpec, Spawned, TreeSummary, TunnelListen, TunnelListening,
    TunnelRef, WinSize, ARGS_HEADER, DEFAULT_KEEPALIVE, MAX_JSON_BYTES,
};
use crate::tls::{CertPin, NodeIdentity, TlsError};

/// The longest line the node reads from the events stream.
const MAX_EVENT_LINE: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("tls: {0}")]
    Tls(io::Error),
    #[error("handshake: {0}")]
    Handshake(String),
    #[error("the sandbox presented an unknown or already used token")]
    BadToken,
    #[error("handshake timed out")]
    Timeout,
    #[error("connection: {0}")]
    H2(#[from] h2::Error),
    #[error("sandbox answered {status}: {message}")]
    Remote { status: StatusCode, message: String },
    #[error("sandbox protocol: {0}")]
    Protocol(String),
    #[error(transparent)]
    Files(#[from] FilesError),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy)]
pub struct ServerOptions {
    /// HTTP/2 PING interval; a pong missing for as long closes the connection.
    pub keepalive: Duration,
    /// TLS, hello and HTTP/2 setup together must finish within this.
    pub handshake_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            keepalive: DEFAULT_KEEPALIVE,
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

/// One-time tokens, each naming the run it was issued for. A token is
/// consumed by the first hello that presents it, accepted or not by anything
/// after, so a copy of a sandbox's environment is worth nothing once that
/// sandbox has connected.
#[derive(Clone, Default)]
pub struct TokenRegistry {
    // Keyed by the token's digest, so the table never holds a usable token
    // and a lookup does not compare secrets byte by byte.
    issued: Arc<Mutex<HashMap<[u8; 32], String>>>,
}

impl TokenRegistry {
    /// A fresh token for `run`: 32 random bytes, base64url.
    pub fn issue(&self, run: impl Into<String>) -> String {
        use ring::rand::SecureRandom as _;
        let mut bytes = [0u8; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .expect("system randomness");
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        self.issued
            .lock()
            .unwrap()
            .insert(Sha256::digest(token.as_bytes()).into(), run.into());
        token
    }

    /// Forget every unused token issued for `run`.
    pub fn revoke(&self, run: &str) {
        self.issued.lock().unwrap().retain(|_, r| r != run);
    }

    fn redeem(&self, token: &str) -> Option<String> {
        self.issued
            .lock()
            .unwrap()
            .remove(&<[u8; 32]>::from(Sha256::digest(token.as_bytes())))
    }
}

/// Accepts sandbox connections for any run whose token it issued.
pub struct SandboxServer {
    acceptor: tokio_rustls::TlsAcceptor,
    tokens: TokenRegistry,
    pin: CertPin,
    options: ServerOptions,
}

impl SandboxServer {
    pub fn new(identity: &NodeIdentity, options: ServerOptions) -> Result<Self, TlsError> {
        Ok(SandboxServer {
            acceptor: tokio_rustls::TlsAcceptor::from(identity.server_config()?),
            tokens: TokenRegistry::default(),
            pin: identity.pin(),
            options,
        })
    }

    pub fn tokens(&self) -> &TokenRegistry {
        &self.tokens
    }

    pub fn pin(&self) -> CertPin {
        self.pin
    }

    /// Accept the next connection on `listener` as a sandbox.
    pub async fn accept_from(&self, listener: &Listener) -> Result<SandboxConn, SandboxError> {
        let io = listener.accept().await?;
        self.accept(io).await
    }

    /// Connect to a sandbox listening on the unix socket at `path` (the
    /// sandbox's [`crate::carrier::Dial::Listen`], relayed out to the host)
    /// and run the handshake, trying again until `within` has passed.
    ///
    /// A relay may accept the connection before anything inside listens and
    /// then close it, so a connection that ends before TLS completes is
    /// tried again too. A refused token or a protocol error is not.
    pub async fn connect_to(
        &self,
        path: &Path,
        within: Duration,
    ) -> Result<SandboxConn, SandboxError> {
        let deadline = tokio::time::Instant::now() + within;
        let mut delay = Duration::from_millis(20);
        loop {
            let attempt = match tokio::net::UnixStream::connect(path).await {
                Ok(stream) => self.accept(stream).await,
                Err(error) => Err(SandboxError::Io(error)),
            };
            match attempt {
                Ok(conn) => return Ok(conn),
                Err(
                    error @ (SandboxError::Io(_) | SandboxError::Tls(_) | SandboxError::Timeout),
                ) if tokio::time::Instant::now() + delay < deadline => {
                    tracing::debug!(%error, path = %path.display(), "sandbox not listening yet");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(500));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Run the handshake on an accepted connection: TLS, then the hello,
    /// whose token must be one this server issued and nobody has used. No
    /// channel exists before that check passes.
    pub async fn accept(&self, io: impl Io) -> Result<SandboxConn, SandboxError> {
        let io: BoxedIo = Box::new(io);
        let handshake = async {
            let mut tls = self.acceptor.accept(io).await.map_err(SandboxError::Tls)?;
            if tls.get_ref().1.alpn_protocol() != Some(protocol::ALPN) {
                return Err(SandboxError::Handshake("no protocol agreed".into()));
            }
            let hello: Hello = read_frame(&mut tls)
                .await
                .map_err(|e| SandboxError::Handshake(format!("hello: {e}")))?;
            let Some(run) = self.tokens.redeem(&hello.token) else {
                let _ = write_frame(
                    &mut tls,
                    &Ack {
                        ok: false,
                        error: Some("unknown or already used token".into()),
                    },
                )
                .await;
                let _ = tls.shutdown().await;
                return Err(SandboxError::BadToken);
            };
            write_frame(
                &mut tls,
                &Ack {
                    ok: true,
                    error: None,
                },
            )
            .await?;
            let (send, connection) = h2::client::Builder::new()
                .initial_window_size(STREAM_WINDOW)
                .initial_connection_window_size(CONNECTION_WINDOW)
                .max_frame_size(MAX_FRAME)
                // Replies carry a status and a content type; nothing a
                // sandbox sends in headers needs more.
                .max_header_list_size(64 * 1024)
                .enable_push(false)
                .handshake::<_, Bytes>(tls)
                .await?;
            Ok((run, SandboxInfo::from(&hello), send, connection))
        };
        let (run, info, send, mut connection) =
            tokio::time::timeout(self.options.handshake_timeout, handshake)
                .await
                .map_err(|_| SandboxError::Timeout)??;
        let closed = CancellationToken::new();
        let ping_pong = connection.ping_pong().expect("ping_pong taken once");
        let driver = {
            let closed = closed.clone();
            let interval = self.options.keepalive;
            let run = run.clone();
            tokio::spawn(async move {
                tokio::select! {
                    result = &mut connection => {
                        if let Err(error) = result {
                            tracing::debug!(%run, %error, "sandbox connection ended");
                        }
                    }
                    _ = keepalive(ping_pong, interval) => {
                        tracing::warn!(%run, "sandbox stopped answering pings");
                    }
                }
                closed.cancel();
            })
        };
        Ok(SandboxConn {
            inner: Arc::new(Inner {
                run,
                info,
                send,
                closed,
                driver,
            }),
        })
    }
}

struct Inner {
    run: String,
    info: SandboxInfo,
    send: SendRequest<Bytes>,
    closed: CancellationToken,
    driver: JoinHandle<()>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

/// A connected sandbox. Cheap to clone; the connection closes when the last
/// clone is dropped.
#[derive(Clone)]
pub struct SandboxConn {
    inner: Arc<Inner>,
}

fn request(method: Method, path: &str, args: Option<String>) -> Request<()> {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("https://sandbox{path}"));
    if let Some(args) = args {
        builder = builder.header(ARGS_HEADER, args);
    }
    builder.body(()).expect("well-formed request")
}

async fn read_limited(mut body: RecvStream, limit: usize) -> Result<Vec<u8>, SandboxError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        let _ = body.flow_control().release_capacity(chunk.len());
        if bytes.len() + chunk.len() > limit {
            return Err(SandboxError::Protocol("reply too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The body of a successful reply, or the sandbox's error as an error.
async fn checked(response: ResponseFuture) -> Result<RecvStream, SandboxError> {
    let response = response.await?;
    let status = response.status();
    let body = response.into_body();
    if status.is_success() {
        return Ok(body);
    }
    let bytes = read_limited(body, MAX_JSON_BYTES).await?;
    let message = serde_json::from_slice::<ErrorBody>(&bytes)
        .map(|e| e.error)
        .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
    Err(SandboxError::Remote { status, message })
}

async fn json_body<T: DeserializeOwned>(body: RecvStream) -> Result<T, SandboxError> {
    let bytes = read_limited(body, MAX_JSON_BYTES).await?;
    serde_json::from_slice(&bytes).map_err(|e| SandboxError::Protocol(e.to_string()))
}

impl SandboxConn {
    /// The run whose token this sandbox presented.
    pub fn run(&self) -> &str {
        &self.inner.run
    }

    /// What the sandbox says about itself. Unverified: display it, never
    /// decide on it.
    pub fn info(&self) -> &SandboxInfo {
        &self.inner.info
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_cancelled()
    }

    /// Resolves when the connection has ended, including by keepalive.
    pub async fn closed(&self) {
        self.inner.closed.cancelled().await
    }

    async fn send(
        &self,
        request: Request<()>,
        end_of_stream: bool,
    ) -> Result<(ResponseFuture, h2::SendStream<Bytes>), SandboxError> {
        let mut send = self.inner.send.clone().ready().await?;
        Ok(send.send_request(request, end_of_stream)?)
    }

    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, SandboxError> {
        let (response, mut stream) = self.send(request(Method::POST, path, None), false).await?;
        let body = serde_json::to_vec(body).map_err(|e| SandboxError::Protocol(e.to_string()))?;
        stream.send_data(Bytes::from(body), true)?;
        json_body(checked(response).await?).await
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        args: Option<String>,
    ) -> Result<T, SandboxError> {
        let (response, _) = self.send(request(Method::GET, path, args), true).await?;
        json_body(checked(response).await?).await
    }

    async fn open_read(&self, path: &str, args: String) -> Result<H2Reader, SandboxError> {
        let (response, _) = self
            .send(request(Method::GET, path, Some(args)), true)
            .await?;
        Ok(H2Reader::new(checked(response).await?))
    }

    /// Start a process.
    pub async fn spawn(&self, spec: SpawnSpec) -> Result<Process, SandboxError> {
        let spawned: Spawned = self.post_json(path::SPAWN, &spec).await?;
        let piped = spec.tty.is_none();
        Ok(Process {
            conn: self.clone(),
            spawned,
            stdin: spec.stdin || !piped,
            stdout: spec.stdout || !piped,
            stderr: spec.stderr && piped,
        })
    }

    /// Attach to every stream `process` was started with. Each stream can be
    /// attached once; attach before waiting.
    pub async fn stdio(&self, process: &Process) -> Result<Stdio, SandboxError> {
        process.stdio().await
    }

    /// Unpack the tar archive `tar` into `dir` inside the sandbox.
    pub async fn import_tar<R: AsyncRead + Unpin>(
        &self,
        dir: impl Into<PathBuf>,
        mut tar: R,
        limits: Option<Limits>,
    ) -> Result<TreeSummary, SandboxError> {
        let args = protocol::encode_args(&ImportRequest {
            dir: dir.into(),
            limits,
        });
        let (response, stream) = self
            .send(request(Method::POST, path::IMPORT, Some(args)), false)
            .await?;
        let mut writer = H2Writer::new(stream);
        let sent = async {
            tokio::io::copy(&mut tar, &mut writer).await?;
            writer.shutdown().await
        }
        .await;
        // The sandbox's own refusal explains a failed send better than the
        // reset it causes.
        let summary = json_body(checked(response).await?).await?;
        sent?;
        Ok(summary)
    }

    /// Pack the local directory `local` and unpack it into `dir` inside the
    /// sandbox.
    pub async fn import_dir(
        &self,
        local: &Path,
        dir: impl Into<PathBuf>,
        exclusions: Exclusions,
    ) -> Result<TreeSummary, SandboxError> {
        self.import_dir_within(local, dir, exclusions, None).await
    }

    /// [`Self::import_dir`], asking the sandbox to unpack within `limits`
    /// rather than its defaults: a tree carrying what a preparation installed
    /// is larger than a checkout.
    pub async fn import_dir_within(
        &self,
        local: &Path,
        dir: impl Into<PathBuf>,
        exclusions: Exclusions,
        limits: Option<Limits>,
    ) -> Result<TreeSummary, SandboxError> {
        let args = protocol::encode_args(&ImportRequest {
            dir: dir.into(),
            limits,
        });
        let (response, stream) = self
            .send(request(Method::POST, path::IMPORT, Some(args)), false)
            .await?;
        let (packed, mut writer) =
            files::pack_async(local.to_path_buf(), exclusions, H2Writer::new(stream)).await;
        match packed {
            Ok(_) => writer.shutdown().await?,
            Err(error) => {
                writer.reset();
                return Err(error.into());
            }
        }
        json_body(checked(response).await?).await
    }

    /// The archive of `dir` as the sandbox packs it, for a caller that
    /// unpacks it itself with [`files::unpack_async`] (which holds it to the
    /// same rules [`Self::export_tar`] does). A transfer that fails halfway
    /// ends in a read error, never in a shorter archive.
    pub async fn export_reader(
        &self,
        dir: impl Into<PathBuf>,
        exclusions: Exclusions,
    ) -> Result<H2Reader, SandboxError> {
        let args = protocol::encode_args(&ExportRequest {
            dir: dir.into(),
            exclusions,
        });
        self.open_read(path::EXPORT, args).await
    }

    /// Export `dir` from the sandbox into `dest` on the node, validated as it
    /// is unpacked (see [`files::unpack`]). The archive lands in a staging
    /// directory beside `dest` and replaces `dest` only when every entry has
    /// passed, so a refused export never leaves a partial tree where a
    /// snapshot is expected.
    pub async fn export_tar(
        &self,
        dir: impl Into<PathBuf>,
        exclusions: Exclusions,
        dest: &Path,
        limits: Limits,
    ) -> Result<TreeSummary, SandboxError> {
        let args = protocol::encode_args(&ExportRequest {
            dir: dir.into(),
            exclusions,
        });
        let reader = self.open_read(path::EXPORT, args).await?;
        let parent = dest
            .parent()
            .ok_or_else(|| SandboxError::Protocol("export destination has no parent".into()))?;
        tokio::fs::create_dir_all(parent).await?;
        let staging = tempfile::Builder::new()
            .prefix(".sandbox-export-")
            .tempdir_in(parent)?;
        let summary = files::unpack_async(reader, staging.path().to_path_buf(), limits).await?;
        let staged = staging.keep();
        if tokio::fs::symlink_metadata(dest).await.is_ok() {
            tokio::fs::remove_dir_all(dest).await?;
        }
        if let Err(error) = tokio::fs::rename(&staged, dest).await {
            let _ = tokio::fs::remove_dir_all(&staged).await;
            return Err(error.into());
        }
        Ok(summary)
    }

    /// Listen on `listen` inside the sandbox and join each connection to
    /// `endpoint` on the node's side. The sandbox only ever knows the
    /// tunnel's name; where it leads is the node's.
    pub async fn open_tunnel(
        &self,
        name: impl Into<String>,
        listen: ListenAddr,
        endpoint: Endpoint,
    ) -> Result<Tunnel, SandboxError> {
        let name = name.into();
        let listening: TunnelListening = self
            .post_json(
                path::TUNNEL_LISTEN,
                &TunnelListen {
                    name: name.clone(),
                    listen,
                },
            )
            .await?;
        let acceptor = tokio::spawn(accept_loop(self.clone(), name.clone(), endpoint));
        Ok(Tunnel {
            conn: self.clone(),
            name,
            bound: listening.bound,
            acceptor,
        })
    }

    pub fn services(&self) -> Services {
        Services { conn: self.clone() }
    }

    /// Service state changes and log lines, from now on.
    pub async fn events(&self) -> Result<EventStream, SandboxError> {
        let (response, _) = self
            .send(request(Method::GET, path::EVENTS, None), true)
            .await?;
        Ok(EventStream {
            reader: BufReader::new(H2Reader::new(checked(response).await?)),
            line: Vec::new(),
        })
    }

    /// Ask the sandbox to stop its services and processes and exit.
    pub async fn shutdown(&self) -> Result<(), SandboxError> {
        let _: serde_json::Value = self.post_json(path::SHUTDOWN, &()).await?;
        Ok(())
    }
}

/// A process started in the sandbox.
pub struct Process {
    conn: SandboxConn,
    spawned: Spawned,
    stdin: bool,
    stdout: bool,
    stderr: bool,
}

/// A process's attached streams; `None` for one it was not given.
pub struct Stdio {
    pub stdin: Option<H2Writer>,
    pub stdout: Option<H2Reader>,
    pub stderr: Option<H2Reader>,
}

impl Process {
    /// The sandbox's handle for it.
    pub fn id(&self) -> u64 {
        self.spawned.id
    }

    /// Its pid inside the sandbox.
    pub fn pid(&self) -> u32 {
        self.spawned.pid
    }

    fn proc_ref(&self) -> String {
        protocol::encode_args(&ProcRef {
            id: self.spawned.id,
        })
    }

    /// Its stdin; shutting the writer down closes it.
    pub async fn stdin(&self) -> Result<H2Writer, SandboxError> {
        let (response, stream) = self
            .conn
            .send(
                request(Method::POST, path::STDIN, Some(self.proc_ref())),
                false,
            )
            .await?;
        tokio::spawn(async move {
            let _ = response.await;
        });
        Ok(H2Writer::new(stream))
    }

    pub async fn stdout(&self) -> Result<H2Reader, SandboxError> {
        self.conn.open_read(path::STDOUT, self.proc_ref()).await
    }

    pub async fn stderr(&self) -> Result<H2Reader, SandboxError> {
        self.conn.open_read(path::STDERR, self.proc_ref()).await
    }

    pub async fn stdio(&self) -> Result<Stdio, SandboxError> {
        Ok(Stdio {
            stdin: if self.stdin {
                Some(self.stdin().await?)
            } else {
                None
            },
            stdout: if self.stdout {
                Some(self.stdout().await?)
            } else {
                None
            },
            stderr: if self.stderr {
                Some(self.stderr().await?)
            } else {
                None
            },
        })
    }

    /// Send `signal` to the process alone.
    pub async fn signal(&self, signal: i32) -> Result<(), SandboxError> {
        self.send_signal(signal, false).await
    }

    /// Send `signal` to the process group it leads.
    pub async fn signal_group(&self, signal: i32) -> Result<(), SandboxError> {
        self.send_signal(signal, true).await
    }

    async fn send_signal(&self, signal: i32, group: bool) -> Result<(), SandboxError> {
        let _: serde_json::Value = self
            .conn
            .post_json(
                path::SIGNAL,
                &SignalRequest {
                    id: self.spawned.id,
                    signal,
                    group,
                },
            )
            .await?;
        Ok(())
    }

    pub async fn resize(&self, size: WinSize) -> Result<(), SandboxError> {
        let _: serde_json::Value = self
            .conn
            .post_json(
                path::RESIZE,
                &ResizeRequest {
                    id: self.spawned.id,
                    size,
                },
            )
            .await?;
        Ok(())
    }

    /// Wait for it to exit. Afterwards the sandbox forgets it.
    pub async fn wait(&self) -> Result<ExitStatus, SandboxError> {
        self.conn.get_json(path::WAIT, Some(self.proc_ref())).await
    }

    /// Wait, collecting everything it writes to stdout and stderr. For short
    /// commands; it holds all their output in memory.
    pub async fn output(&self) -> Result<Output, SandboxError> {
        let stdio = self.stdio().await?;
        if let Some(mut stdin) = stdio.stdin {
            stdin.shutdown().await?;
        }
        let read_all = |reader: Option<H2Reader>| async move {
            let mut bytes = Vec::new();
            if let Some(mut reader) = reader {
                reader.read_to_end(&mut bytes).await?;
            }
            Ok::<_, io::Error>(bytes)
        };
        let (stdout, stderr) = tokio::try_join!(read_all(stdio.stdout), read_all(stdio.stderr))?;
        let status = self.wait().await?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Where a tunnel leads on the node's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Tcp(String),
    Unix(PathBuf),
}

impl Endpoint {
    async fn connect(&self) -> io::Result<BoxedIo> {
        match self {
            Endpoint::Tcp(addr) => {
                let stream = tokio::net::TcpStream::connect(addr).await?;
                stream.set_nodelay(true)?;
                Ok(Box::new(stream))
            }
            Endpoint::Unix(path) => Ok(Box::new(tokio::net::UnixStream::connect(path).await?)),
        }
    }
}

/// An open tunnel. Dropping it stops taking new connections; [`Tunnel::close`]
/// also closes the listener inside the sandbox.
pub struct Tunnel {
    conn: SandboxConn,
    name: String,
    bound: ListenAddr,
    acceptor: JoinHandle<()>,
}

impl Tunnel {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The address the sandbox bound.
    pub fn bound(&self) -> &ListenAddr {
        &self.bound
    }

    pub async fn close(self) -> Result<(), SandboxError> {
        self.acceptor.abort();
        let _: serde_json::Value = self
            .conn
            .post_json(
                path::TUNNEL_CLOSE,
                &TunnelRef {
                    name: self.name.clone(),
                },
            )
            .await?;
        Ok(())
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.acceptor.abort();
    }
}

async fn accept_loop(conn: SandboxConn, name: String, endpoint: Endpoint) {
    let args = protocol::encode_args(&TunnelRef { name: name.clone() });
    loop {
        let Ok((response, to_sandbox)) = conn
            .send(
                request(Method::POST, path::TUNNEL_ACCEPT, Some(args.clone())),
                false,
            )
            .await
        else {
            return;
        };
        let from_sandbox = match checked(response).await {
            Ok(body) => body,
            Err(error) => {
                tracing::debug!(tunnel = %name, %error, "tunnel stopped accepting");
                return;
            }
        };
        let endpoint = endpoint.clone();
        let name = name.clone();
        tokio::spawn(async move {
            let local = match endpoint.connect().await {
                Ok(local) => local,
                Err(error) => {
                    // Dropping both halves resets the stream, and the
                    // sandbox closes the connection it accepted.
                    tracing::warn!(tunnel = %name, %error, "tunnel endpoint unreachable");
                    return;
                }
            };
            let (mut read, mut write) = tokio::io::split(local);
            let mut from_sandbox = H2Reader::new(from_sandbox);
            let mut to_sandbox = H2Writer::new(to_sandbox);
            let inbound = async {
                let _ = tokio::io::copy(&mut from_sandbox, &mut write).await;
                let _ = write.shutdown().await;
            };
            let outbound = async {
                if tokio::io::copy(&mut read, &mut to_sandbox).await.is_ok() {
                    let _ = to_sandbox.shutdown().await;
                }
            };
            tokio::join!(inbound, outbound);
        });
    }
}

/// The sandbox's service supervisor.
pub struct Services {
    conn: SandboxConn,
}

impl Services {
    pub async fn start(&self, services: Vec<ServiceSpec>) -> Result<(), SandboxError> {
        let _: serde_json::Value = self
            .conn
            .post_json(path::SERVICES_START, &ServicesStart { services })
            .await?;
        Ok(())
    }

    pub async fn status(&self) -> Result<Vec<ServiceStatus>, SandboxError> {
        self.conn.get_json(path::SERVICES_STATUS, None).await
    }

    /// Stop the named services, every one when `names` is empty.
    pub async fn stop(&self, names: Vec<String>) -> Result<(), SandboxError> {
        let _: serde_json::Value = self
            .conn
            .post_json(path::SERVICES_STOP, &ServicesStop { names })
            .await?;
        Ok(())
    }
}

/// The sandbox's events, one JSON object per line.
pub struct EventStream {
    reader: BufReader<H2Reader>,
    line: Vec<u8>,
}

impl EventStream {
    /// The next event, or `None` when the stream has ended.
    pub async fn next(&mut self) -> Result<Option<Event>, SandboxError> {
        self.line.clear();
        let read = (&mut self.reader)
            .take(MAX_EVENT_LINE as u64 + 1)
            .read_until(b'\n', &mut self.line)
            .await?;
        if read == 0 {
            return Ok(None);
        }
        if self.line.last() != Some(&b'\n') {
            return Err(SandboxError::Protocol(
                "event line too long or cut short".into(),
            ));
        }
        serde_json::from_slice(&self.line)
            .map(Some)
            .map_err(|e| SandboxError::Protocol(e.to_string()))
    }
}
