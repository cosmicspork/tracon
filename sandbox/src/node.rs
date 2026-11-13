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
//!
//! # A run outlives its connection
//!
//! A sandbox that says it can resume is given a resume secret with its
//! first ack. When its connection drops, the [`SandboxConn`] stays: requests
//! wait for the sandbox to redial (on Podman's and Kubernetes's carriers it
//! dials again, on Apple's the node connects in again) and prove the secret,
//! and the run is lost only if that does not happen within
//! [`ServerOptions::resume_within`]. Process output is read from the offset
//! the node had reached, stdin is sent again from what the node still keeps
//! and the sandbox skips what it already wrote, and a wait, a tunnel's
//! accept and the event stream are asked again. A request that was in
//! flight otherwise fails, as before.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
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
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
    ReadBuf,
};
use tokio::sync::watch;
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
    ListenAddr, ProcRef, ResizeRequest, Resume, SandboxInfo, ServiceSpec, ServiceStatus,
    ServicesStart, ServicesStop, SignalRequest, SpawnSpec, Spawned, TreeSummary, TunnelListen,
    TunnelListening, TunnelRef, WinSize, ARGS_HEADER, DEFAULT_KEEPALIVE, INPUT_KEPT,
    MAX_JSON_BYTES,
};
use crate::tls::{CertPin, NodeIdentity, TlsError};

/// The longest line the node reads from the events stream.
const MAX_EVENT_LINE: usize = 256 * 1024;

/// How long a run waits for its sandbox to redial when nothing says
/// otherwise.
pub const DEFAULT_RESUME_WITHIN: Duration = Duration::from_secs(120);

/// How long after a stream fails the node waits to see whether its
/// connection failed with it, before taking the failure as the stream's own.
const DROP_GRACE: Duration = Duration::from_secs(5);

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
    #[error("the sandbox's connection was lost and it did not come back")]
    Lost,
}

impl SandboxError {
    /// Whether this is the connection failing rather than the request: the
    /// kind of failure a redial can cure.
    fn of_connection(&self) -> bool {
        match self {
            SandboxError::H2(error) => h2_of_connection(error),
            SandboxError::Io(error) => io_of_connection(error),
            _ => false,
        }
    }
}

fn h2_of_connection(error: &h2::Error) -> bool {
    error.is_io() || error.is_go_away()
}

/// As [`h2_of_connection`] for what an [`H2Reader`] or [`H2Writer`] says: an
/// h2 error it wrapped, else the connection's own I/O error unwrapped, or a
/// stream that closed under it, which the connection closing would also do
/// ([`SandboxConn::worth_retrying`] tells which).
fn io_of_connection(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<h2::Error>())
        .is_none_or(h2_of_connection)
}

#[derive(Debug, Clone, Copy)]
pub struct ServerOptions {
    /// HTTP/2 PING interval; a pong missing for as long closes the connection.
    pub keepalive: Duration,
    /// TLS, hello and HTTP/2 setup together must finish within this.
    pub handshake_timeout: Duration,
    /// How long a run whose connection dropped waits for its sandbox to
    /// redial before it is lost. The sandbox is told to keep redialling for
    /// twice as long, so a node that takes up to this long to come back
    /// still finds it. Zero gives no resume secret: the first dropped
    /// connection ends the run.
    pub resume_within: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            keepalive: DEFAULT_KEEPALIVE,
            handshake_timeout: Duration::from_secs(10),
            resume_within: DEFAULT_RESUME_WITHIN,
        }
    }
}

/// Each run's tokens. A first-use token is consumed by the first hello that
/// presents it, accepted or not by anything after, so a copy of a sandbox's
/// environment is worth nothing once that sandbox has connected. One issued
/// with a window ([`TokenRegistry::issue_within`]) is also worth nothing
/// once the window has passed unused, for a provider that may keep a copy of
/// how it was delivered.
///
/// A resume secret is issued by the node itself, inside TLS, with the first
/// ack; it may be presented any number of times until the run is revoked.
#[derive(Clone, Default)]
pub struct TokenRegistry {
    // Keyed by the token's digest, so the table never holds a usable token
    // and a lookup does not compare secrets byte by byte.
    issued: Arc<Mutex<HashMap<[u8; 32], Issued>>>,
}

struct Issued {
    run: String,
    /// Redeemable until then; `None` for no window of its own.
    until: Option<std::time::Instant>,
    resume: bool,
}

/// What a presented token turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum Redeemed {
    First(String),
    Resume(String),
}

fn random_token() -> String {
    use ring::rand::SecureRandom as _;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .expect("system randomness");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl TokenRegistry {
    /// A fresh token for `run`: 32 random bytes, base64url.
    pub fn issue(&self, run: impl Into<String>) -> String {
        self.insert(run.into(), None, false)
    }

    /// A fresh token for `run` that is refused unless redeemed within
    /// `window` of now.
    pub fn issue_within(&self, run: impl Into<String>, window: Duration) -> String {
        self.insert(run.into(), Some(std::time::Instant::now() + window), false)
    }

    fn issue_resume(&self, run: &str) -> String {
        self.insert(run.to_string(), None, true)
    }

    fn insert(&self, run: String, until: Option<std::time::Instant>, resume: bool) -> String {
        let token = random_token();
        let mut issued = self.issued.lock().unwrap();
        // Expired tokens are dropped as new ones are issued, so a run whose
        // sandbox never came leaves nothing behind for long.
        let now = std::time::Instant::now();
        issued.retain(|_, i| i.until.is_none_or(|until| until > now));
        issued.insert(digest(&token), Issued { run, until, resume });
        token
    }

    /// Forget every token issued for `run`, its resume secret included.
    pub fn revoke(&self, run: &str) {
        self.issued.lock().unwrap().retain(|_, i| i.run != run);
    }

    /// Whether an unused, unexpired first-use token for `run` is
    /// outstanding.
    pub fn outstanding(&self, run: &str) -> bool {
        let now = std::time::Instant::now();
        self.issued
            .lock()
            .unwrap()
            .values()
            .any(|i| i.run == run && !i.resume && i.until.is_none_or(|until| until > now))
    }

    /// The digest of `run`'s resume secret: what a node keeps so the next
    /// process can take the run's sandbox back, never the secret itself.
    pub fn resume_digest(&self, run: &str) -> Option<[u8; 32]> {
        self.issued
            .lock()
            .unwrap()
            .iter()
            .find(|(_, i)| i.run == run && i.resume)
            .map(|(digest, _)| *digest)
    }

    /// Know `run`'s resume secret again by its digest.
    pub fn restore_resume(&self, run: &str, digest: [u8; 32]) {
        self.issued.lock().unwrap().insert(
            digest,
            Issued {
                run: run.to_string(),
                until: None,
                resume: true,
            },
        );
    }

    /// Whether `run` holds a resume secret.
    pub fn resumable(&self, run: &str) -> bool {
        self.issued
            .lock()
            .unwrap()
            .values()
            .any(|i| i.run == run && i.resume)
    }

    /// The run `token` was issued for, consuming it unless it is a resume
    /// secret. With `bound`, a token issued for another run is refused and
    /// left as it was: a carrier that belongs to one run cannot spend
    /// another's.
    fn redeem(&self, token: &str, bound: Option<&str>) -> Option<Redeemed> {
        let mut issued = self.issued.lock().unwrap();
        let key = digest(token);
        let found = issued.get(&key)?;
        if bound.is_some_and(|run| run != found.run) {
            return None;
        }
        if found.resume {
            return Some(Redeemed::Resume(found.run.clone()));
        }
        let found = issued.remove(&key)?;
        match found.until {
            Some(until) if until <= std::time::Instant::now() => None,
            _ => Some(Redeemed::First(found.run)),
        }
    }
}

type Runs = Arc<Mutex<HashMap<String, Weak<Inner>>>>;

/// Accepts sandbox connections for any run whose token it issued, and takes
/// back a run's sandbox when it redials with its resume secret. Cheap to
/// clone; clones share their tokens and runs.
#[derive(Clone)]
pub struct SandboxServer {
    acceptor: tokio_rustls::TlsAcceptor,
    tokens: TokenRegistry,
    pin: CertPin,
    options: ServerOptions,
    runs: Runs,
}

/// A connection that passed its handshake.
pub enum Accepted {
    /// A run's first connection.
    New(SandboxConn),
    /// A run's sandbox came back: the connection now carries the
    /// [`SandboxConn`] it had before.
    Resumed(SandboxConn),
}

impl Accepted {
    pub fn into_conn(self) -> SandboxConn {
        match self {
            Accepted::New(conn) | Accepted::Resumed(conn) => conn,
        }
    }
}

type ClientConnection = h2::client::Connection<tokio_rustls::server::TlsStream<BoxedIo>, Bytes>;

struct Handshaken {
    run: String,
    info: SandboxInfo,
    /// The run it resumes, for a resume secret.
    resumes: Option<Arc<Inner>>,
    resumable: bool,
    send: SendRequest<Bytes>,
    connection: ClientConnection,
}

impl SandboxServer {
    pub fn new(identity: &NodeIdentity, options: ServerOptions) -> Result<Self, TlsError> {
        Ok(SandboxServer {
            acceptor: tokio_rustls::TlsAcceptor::from(identity.server_config()?),
            tokens: TokenRegistry::default(),
            pin: identity.pin(),
            options,
            runs: Arc::default(),
        })
    }

    pub fn tokens(&self) -> &TokenRegistry {
        &self.tokens
    }

    pub fn pin(&self) -> CertPin {
        self.pin
    }

    pub fn options(&self) -> &ServerOptions {
        &self.options
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
        self.connect_to_bound(path, within, None)
            .await
            .map(Accepted::into_conn)
    }

    async fn connect_to_bound(
        &self,
        path: &Path,
        within: Duration,
        bound: Option<&str>,
    ) -> Result<Accepted, SandboxError> {
        let deadline = tokio::time::Instant::now() + within;
        let mut delay = Duration::from_millis(20);
        loop {
            let attempt = match tokio::net::UnixStream::connect(path).await {
                Ok(stream) => self.accept_bound(Box::new(stream), bound).await,
                Err(error) => Err(SandboxError::Io(error)),
            };
            match attempt {
                Ok(accepted) => return Ok(accepted),
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
    /// whose token must be one this server issued and nobody has used, or a
    /// live run's resume secret. No channel exists before that check passes.
    pub async fn accept(&self, io: impl Io) -> Result<SandboxConn, SandboxError> {
        self.accept_any(io).await.map(Accepted::into_conn)
    }

    /// [`Self::accept`], saying whether it was a run's first connection or
    /// its sandbox coming back.
    pub async fn accept_any(&self, io: impl Io) -> Result<Accepted, SandboxError> {
        self.accept_bound(Box::new(io), None).await
    }

    /// [`Self::accept_any`] on a carrier that belongs to `run`: a token or
    /// secret of any other run is refused, and not spent.
    pub async fn accept_for(&self, io: impl Io, run: &str) -> Result<Accepted, SandboxError> {
        self.accept_bound(Box::new(io), Some(run)).await
    }

    async fn accept_bound(
        &self,
        io: BoxedIo,
        bound: Option<&str>,
    ) -> Result<Accepted, SandboxError> {
        let handshaken =
            tokio::time::timeout(self.options.handshake_timeout, self.handshake(io, bound))
                .await
                .map_err(|_| SandboxError::Timeout)??;
        let Handshaken {
            run,
            info,
            resumes,
            resumable,
            send,
            connection,
        } = handshaken;
        if let Some(inner) = resumes {
            *inner.info.lock().unwrap() = info;
            if !inner.attach(send, connection) {
                return Err(SandboxError::Lost);
            }
            tracing::info!(%run, connections = inner.connections.load(Ordering::SeqCst), "a sandbox came back");
            return Ok(Accepted::Resumed(SandboxConn { inner }));
        }
        let inner = self.register(&run, info, resumable);
        inner.attach(send, connection);
        Ok(Accepted::New(SandboxConn { inner }))
    }

    fn register(&self, run: &str, info: SandboxInfo, resumable: bool) -> Arc<Inner> {
        let (link, _) = watch::channel(Link::Down(0));
        let inner = Arc::new(Inner {
            run: run.to_string(),
            info: Mutex::new(info),
            resumable,
            link,
            ended: CancellationToken::new(),
            connections: AtomicU64::new(0),
            closing: AtomicBool::new(false),
            driver: Mutex::new(None),
            tokens: self.tokens.clone(),
            runs: Arc::downgrade(&self.runs),
            resume_within: self.options.resume_within,
            keepalive: self.options.keepalive,
        });
        self.runs
            .lock()
            .unwrap()
            .insert(run.to_string(), Arc::downgrade(&inner));
        inner
    }

    /// A run an earlier node process started, known again by its resume
    /// secret's digest (as [`TokenRegistry::resume_digest`] gave it): away
    /// until its sandbox redials with the secret, and lost if that does not
    /// happen within [`ServerOptions::resume_within`]. Its carrier is the
    /// provider's to set up again.
    pub fn restore(&self, run: &str, digest: [u8; 32]) -> SandboxConn {
        self.tokens.restore_resume(run, digest);
        let info = SandboxInfo {
            version: "unknown".into(),
            arch: String::new(),
            pid: 0,
            init: false,
            uid: None,
        };
        let inner = self.register(run, info, true);
        inner.await_return(0);
        SandboxConn { inner }
    }

    async fn handshake(
        &self,
        io: BoxedIo,
        bound: Option<&str>,
    ) -> Result<Handshaken, SandboxError> {
        let mut tls = self.acceptor.accept(io).await.map_err(SandboxError::Tls)?;
        if tls.get_ref().1.alpn_protocol() != Some(protocol::ALPN) {
            return Err(SandboxError::Handshake("no protocol agreed".into()));
        }
        let hello: Hello = read_frame(&mut tls)
            .await
            .map_err(|e| SandboxError::Handshake(format!("hello: {e}")))?;
        let refuse = |why: &str| Ack {
            ok: false,
            error: Some(why.into()),
            resume: None,
        };
        let (run, resumes, resume) = match self.tokens.redeem(&hello.token, bound) {
            None => {
                let _ = write_frame(&mut tls, &refuse("unknown or already used token")).await;
                let _ = tls.shutdown().await;
                return Err(SandboxError::BadToken);
            }
            Some(Redeemed::Resume(run)) => {
                let live = self
                    .runs
                    .lock()
                    .unwrap()
                    .get(&run)
                    .and_then(Weak::upgrade)
                    .filter(|inner| !inner.ended.is_cancelled());
                let Some(inner) = live else {
                    self.tokens.revoke(&run);
                    let _ = write_frame(&mut tls, &refuse("the run is over")).await;
                    let _ = tls.shutdown().await;
                    return Err(SandboxError::BadToken);
                };
                (run, Some(inner), None)
            }
            Some(Redeemed::First(run)) => {
                let resume =
                    (hello.resumable && !self.options.resume_within.is_zero()).then(|| Resume {
                        secret: self.tokens.issue_resume(&run),
                        within_ms: 2 * self.options.resume_within.as_millis() as u64,
                    });
                (run, None, resume)
            }
        };
        let resumable = resumes.is_some() || resume.is_some();
        write_frame(
            &mut tls,
            &Ack {
                ok: true,
                error: None,
                resume,
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
        Ok(Handshaken {
            run,
            info: SandboxInfo::from(&hello),
            resumes,
            resumable,
            send,
            connection,
        })
    }

    /// End `run` now, whether or not a connection carries it: its sandbox is
    /// gone or being removed, so nothing should wait for it to come back.
    pub fn end_run(&self, run: &str) {
        let inner = self.runs.lock().unwrap().get(run).and_then(Weak::upgrade);
        match inner {
            Some(inner) => inner.finish(),
            None => self.tokens.revoke(run),
        }
    }

    /// Take `conn`'s sandbox back each time it redials on `listener`, a
    /// carrier that belongs to its run alone, until the run ends. `None`
    /// for a run that cannot resume.
    pub fn keep_accepting(&self, listener: Listener, conn: &SandboxConn) -> Option<JoinHandle<()>> {
        if !conn.is_resumable() {
            return None;
        }
        let server = self.clone();
        let run = conn.run().to_string();
        let ended = conn.inner.ended.clone();
        Some(tokio::spawn(async move {
            loop {
                // A run that has ended takes nothing more, however soon its
                // sandbox redials: on the same carrier, a later node process
                // may be the one to take it back.
                let io = tokio::select! {
                    biased;
                    _ = ended.cancelled() => return,
                    io = listener.accept() => io,
                };
                if ended.is_cancelled() {
                    return;
                }
                let io = match io {
                    Ok(io) => io,
                    Err(error) => {
                        tracing::debug!(%run, %error, "accept on a sandbox's carrier");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let accepted = tokio::select! {
                    biased;
                    _ = ended.cancelled() => return,
                    accepted = server.accept_bound(io, Some(&run)) => accepted,
                };
                if let Err(error) = accepted {
                    tracing::warn!(%run, %error, "refused a connection on a sandbox's carrier");
                }
            }
        }))
    }

    /// For a sandbox that listens (Apple's carrier): connect in at `path`
    /// again whenever `conn`'s connection drops, until its run ends. `None`
    /// for a run that cannot resume.
    pub fn keep_connecting(&self, path: PathBuf, conn: &SandboxConn) -> Option<JoinHandle<()>> {
        if !conn.is_resumable() {
            return None;
        }
        let server = self.clone();
        let run = conn.run().to_string();
        let ended = conn.inner.ended.clone();
        let mut link = conn.inner.link.subscribe();
        Some(tokio::spawn(async move {
            loop {
                loop {
                    if !matches!(*link.borrow_and_update(), Link::Up(_)) {
                        break;
                    }
                    tokio::select! {
                        changed = link.changed() => if changed.is_err() { return },
                        _ = ended.cancelled() => return,
                    }
                }
                let within = server.options.resume_within;
                let attempt = tokio::select! {
                    attempt = server.connect_to_bound(&path, within, Some(&run)) => attempt,
                    _ = ended.cancelled() => return,
                };
                if let Err(error) = attempt {
                    tracing::debug!(%run, %error, "the sandbox is not listening again yet");
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        _ = ended.cancelled() => return,
                    }
                }
            }
        }))
    }
}

/// One connection carrying a run.
struct Live {
    generation: u64,
    send: SendRequest<Bytes>,
    /// Cancelled once this connection has ended.
    closed: CancellationToken,
}

#[derive(Clone)]
enum Link {
    Up(Arc<Live>),
    /// The connection of this generation dropped; waiting for the sandbox.
    Down(u64),
    Lost,
}

struct Inner {
    run: String,
    info: Mutex<SandboxInfo>,
    /// The sandbox holds a resume secret.
    resumable: bool,
    link: watch::Sender<Link>,
    /// Cancelled once the run is over: lost, or every handle dropped.
    ended: CancellationToken,
    connections: AtomicU64,
    /// The node asked the sandbox to shut down: the connection that ends
    /// next ends the run, and nothing waits for it to redial.
    closing: AtomicBool,
    driver: Mutex<Option<JoinHandle<()>>>,
    tokens: TokenRegistry,
    runs: Weak<Mutex<HashMap<String, Weak<Inner>>>>,
    resume_within: Duration,
    keepalive: Duration,
}

impl Inner {
    /// Carry the run over `connection` from now on. `false` when the run is
    /// already over.
    fn attach(
        self: &Arc<Self>,
        send: SendRequest<Bytes>,
        mut connection: ClientConnection,
    ) -> bool {
        if self.ended.is_cancelled() {
            return false;
        }
        let generation = self.connections.fetch_add(1, Ordering::SeqCst) + 1;
        let closed = CancellationToken::new();
        let live = Arc::new(Live {
            generation,
            send,
            closed: closed.clone(),
        });
        let ping_pong = connection.ping_pong().expect("ping_pong taken once");
        let weak = Arc::downgrade(self);
        let interval = self.keepalive;
        let run = self.run.clone();
        let driver = tokio::spawn(async move {
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
            if let Some(inner) = weak.upgrade() {
                inner.dropped(generation);
            }
            closed.cancel();
            // Dropping it fails every stream still open on it.
            drop(connection);
        });
        let previous = self.link.send_replace(Link::Up(live));
        if let Link::Up(previous) = previous {
            previous.closed.cancel();
        }
        if let Some(old) = self.driver.lock().unwrap().replace(driver) {
            old.abort();
        }
        true
    }

    /// The connection of `generation` has ended. One that ends after the
    /// node asked the sandbox to shut down is the end of the run, as it is
    /// for a sandbox that cannot resume: the sandbox is exiting, not away.
    fn dropped(self: &Arc<Self>, generation: u64) {
        let closing = self.closing.load(Ordering::SeqCst);
        let mut now = None;
        self.link.send_if_modified(|link| match link {
            Link::Up(live) if live.generation == generation => {
                *link = match self.resumable && !closing {
                    true => Link::Down(generation),
                    false => Link::Lost,
                };
                now = Some(link.clone());
                true
            }
            _ => false,
        });
        match now {
            Some(Link::Lost) => {
                if closing {
                    tracing::debug!(run = %self.run, "a sandbox closed its connection as it shut down");
                }
                self.end()
            }
            Some(_) => {
                tracing::warn!(run = %self.run, within_s = self.resume_within.as_secs(), "lost a sandbox's connection; waiting for it to redial");
                self.await_return(generation);
            }
            None => {}
        }
    }

    /// Lose the run unless a connection after `generation` carries it
    /// within the resume window.
    fn await_return(self: &Arc<Self>, generation: u64) {
        let weak = Arc::downgrade(self);
        let within = self.resume_within;
        tokio::spawn(async move {
            tokio::time::sleep(within).await;
            if let Some(inner) = weak.upgrade() {
                inner.expire(generation);
            }
        });
    }

    /// The window for the connection of `generation` to come back has
    /// passed.
    fn expire(&self, generation: u64) {
        let lost = self.link.send_if_modified(|link| match link {
            Link::Down(dropped) if *dropped == generation => {
                *link = Link::Lost;
                true
            }
            _ => false,
        });
        if lost {
            tracing::warn!(run = %self.run, "a sandbox did not come back in time; its run is lost");
            self.end();
        }
    }

    /// Over now, connected or not.
    fn finish(&self) {
        if let Link::Up(live) = self.link.send_replace(Link::Lost) {
            live.closed.cancel();
        }
        if let Some(driver) = self.driver.lock().unwrap().take() {
            driver.abort();
        }
        self.end();
    }

    fn end(&self) {
        self.ended.cancel();
        self.tokens.revoke(&self.run);
        if let Some(runs) = self.runs.upgrade() {
            let mut runs = runs.lock().unwrap();
            if runs
                .get(&self.run)
                .is_some_and(|held| std::ptr::eq(held.as_ptr(), self))
            {
                runs.remove(&self.run);
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.lock().unwrap().take() {
            driver.abort();
        }
        self.ended.cancel();
        self.tokens.revoke(&self.run);
        if let Some(runs) = self.runs.upgrade() {
            let mut runs = runs.lock().unwrap();
            if runs
                .get(&self.run)
                .is_some_and(|held| held.strong_count() == 0)
            {
                runs.remove(&self.run);
            }
        }
    }
}

/// A connected sandbox. Cheap to clone; the run ends, and its connection
/// closes, when the last clone is dropped.
#[derive(Clone)]
pub struct SandboxConn {
    inner: Arc<Inner>,
}

/// A [`SandboxConn`] that does not keep the run open.
#[derive(Clone)]
pub struct WeakConn(Weak<Inner>);

impl WeakConn {
    pub fn upgrade(&self) -> Option<SandboxConn> {
        self.0.upgrade().map(|inner| SandboxConn { inner })
    }

    /// Resolves once no connection carries the run, or the run is gone.
    pub async fn disconnected(&self) {
        let Some(inner) = self.0.upgrade() else {
            return;
        };
        let mut link = inner.link.subscribe();
        drop(inner);
        while matches!(*link.borrow_and_update(), Link::Up(_)) {
            if link.changed().await.is_err() {
                return;
            }
        }
    }
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
    pub fn info(&self) -> SandboxInfo {
        self.inner.info.lock().unwrap().clone()
    }

    /// Whether the sandbox holds a resume secret, so a dropped connection
    /// need not end the run.
    pub fn is_resumable(&self) -> bool {
        self.inner.resumable
    }

    /// Whether the run is over: its connection ended and the sandbox did
    /// not come back in time (at once, for one that cannot resume).
    pub fn is_closed(&self) -> bool {
        self.inner.ended.is_cancelled()
    }

    /// Resolves when the run is over, as [`Self::is_closed`].
    pub async fn closed(&self) {
        self.inner.ended.cancelled().await
    }

    /// [`Self::closed`] for a task that must not hold the run open: it
    /// keeps no handle to the connection.
    pub fn ended(&self) -> tokio_util::sync::WaitForCancellationFutureOwned {
        self.inner.ended.clone().cancelled_owned()
    }

    pub fn downgrade(&self) -> WeakConn {
        WeakConn(Arc::downgrade(&self.inner))
    }

    /// Whether a connection carries the run right now.
    pub fn is_connected(&self) -> bool {
        matches!(*self.inner.link.borrow(), Link::Up(_))
    }

    /// Resolves once no connection carries the run: dropped, or lost.
    pub async fn disconnected(&self) {
        let mut link = self.inner.link.subscribe();
        while matches!(*link.borrow_and_update(), Link::Up(_)) {
            if link.changed().await.is_err() {
                return;
            }
        }
    }

    /// How many connections have carried the run: one, until its sandbox
    /// first comes back.
    pub fn connections(&self) -> u64 {
        self.inner.connections.load(Ordering::SeqCst)
    }

    /// Drop the connection carrying the run, as a failing network would:
    /// the run then waits for its sandbox to come back, or ends at once if
    /// it cannot. For tests and diagnostics.
    pub fn sever(&self) {
        let live = match &*self.inner.link.borrow() {
            Link::Up(live) => live.clone(),
            _ => return,
        };
        if let Some(driver) = self.inner.driver.lock().unwrap().take() {
            driver.abort();
        }
        self.inner.dropped(live.generation);
        live.closed.cancel();
    }

    /// The connection carrying the run, waiting for the sandbox to come back
    /// if it is away.
    async fn live(&self) -> Result<Arc<Live>, SandboxError> {
        let mut link = self.inner.link.subscribe();
        loop {
            match &*link.borrow_and_update() {
                Link::Up(live) => return Ok(live.clone()),
                Link::Lost => return Err(SandboxError::Lost),
                Link::Down(_) => {}
            }
            if link.changed().await.is_err() {
                return Err(SandboxError::Lost);
            }
        }
    }

    /// After a request on `live` failed (`of_connection` saying whether the
    /// failure looked like the connection's): whether that connection has
    /// gone and the run may yet come back, so the request is worth making
    /// again.
    async fn worth_retrying(&self, live: &Live, of_connection: bool) -> bool {
        if !self.inner.resumable || !of_connection {
            return false;
        }
        let gone = tokio::time::timeout(DROP_GRACE, live.closed.cancelled())
            .await
            .is_ok();
        gone && !self.is_closed()
    }

    async fn send_on(
        &self,
        live: &Live,
        request: Request<()>,
        end_of_stream: bool,
    ) -> Result<(ResponseFuture, h2::SendStream<Bytes>), SandboxError> {
        let mut send = live.send.clone().ready().await?;
        Ok(send.send_request(request, end_of_stream)?)
    }

    async fn send(
        &self,
        request: Request<()>,
        end_of_stream: bool,
    ) -> Result<(ResponseFuture, h2::SendStream<Bytes>), SandboxError> {
        let live = self.live().await?;
        self.send_on(&live, request, end_of_stream).await
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

    async fn get_json_on<T: DeserializeOwned>(
        &self,
        live: &Live,
        path: &str,
        args: Option<String>,
    ) -> Result<T, SandboxError> {
        let (response, _) = self
            .send_on(live, request(Method::GET, path, args), true)
            .await?;
        json_body(checked(response).await?).await
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        args: Option<String>,
    ) -> Result<T, SandboxError> {
        let live = self.live().await?;
        self.get_json_on(&live, path, args).await
    }

    async fn open_read_on(
        &self,
        live: &Live,
        path: &str,
        args: String,
    ) -> Result<H2Reader, SandboxError> {
        let (response, _) = self
            .send_on(live, request(Method::GET, path, Some(args)), true)
            .await?;
        Ok(H2Reader::new(checked(response).await?))
    }

    async fn open_read(&self, path: &str, args: String) -> Result<H2Reader, SandboxError> {
        let live = self.live().await?;
        self.open_read_on(&live, path, args).await
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

    /// Attach to every stream `process` was started with. Attach before
    /// waiting.
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
        let dir = dir.into();
        let args = protocol::encode_args(&ExportRequest {
            dir: dir.clone(),
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
        let summary =
            files::unpack_async(reader, staging.path().to_path_buf(), Some(dir), limits).await?;
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
    /// tunnel's name; where it leads is the node's. The listener stays
    /// through a dropped connection, and connections to it are taken again
    /// once the sandbox is back.
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

    /// A process an earlier node process started in this sandbox, by the
    /// id the sandbox gave it ([`Process::id`]), with every stream piped.
    pub fn process(&self, id: u64) -> Process {
        Process {
            conn: self.clone(),
            spawned: Spawned { id, pid: 0 },
            stdin: true,
            stdout: true,
            stderr: true,
        }
    }

    /// Take the connections of a tunnel already listening in the sandbox at
    /// `listen` (one an earlier node process opened) to `endpoint`.
    pub fn adopt_tunnel(
        &self,
        name: impl Into<String>,
        listen: ListenAddr,
        endpoint: Endpoint,
    ) -> Tunnel {
        let name = name.into();
        let acceptor = tokio::spawn(accept_loop(self.clone(), name.clone(), endpoint));
        Tunnel {
            conn: self.clone(),
            name,
            bound: listen,
            acceptor,
        }
    }

    /// Wait for a connection to carry the run: `false` if it is lost first.
    pub async fn connected(&self) -> bool {
        self.live().await.is_ok()
    }

    pub fn services(&self) -> Services {
        Services { conn: self.clone() }
    }

    /// Service state changes and log lines, from now on. Asked again over a
    /// new connection when the sandbox comes back; what happened while it
    /// was away is not sent.
    pub async fn events(&self) -> Result<EventStream, SandboxError> {
        let live = self.live().await?;
        let reader = self.open_events(&live).await?;
        Ok(EventStream {
            conn: self.clone(),
            live,
            reader,
            line: Vec::new(),
        })
    }

    async fn open_events(&self, live: &Live) -> Result<BufReader<H2Reader>, SandboxError> {
        let (response, _) = self
            .send_on(live, request(Method::GET, path::EVENTS, None), true)
            .await?;
        Ok(BufReader::new(H2Reader::new(checked(response).await?)))
    }

    /// Ask the sandbox to stop its services and processes and exit. Its
    /// connection closing from now on ends the run rather than waiting for
    /// a redial.
    pub async fn shutdown(&self) -> Result<(), SandboxError> {
        self.inner.closing.store(true, Ordering::SeqCst);
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
    pub stdin: Option<InputWriter>,
    pub stdout: Option<OutputReader>,
    pub stderr: Option<OutputReader>,
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

    fn proc_ref(&self, from: Option<u64>) -> String {
        protocol::encode_args(&ProcRef {
            id: self.spawned.id,
            from,
        })
    }

    /// Its stdin; shutting the writer down closes it. What is written goes
    /// on over a new connection when the sandbox comes back.
    pub async fn stdin(&self) -> Result<InputWriter, SandboxError> {
        let live = self.conn.live().await?;
        let (response, stream) = self
            .conn
            .send_on(
                &live,
                request(Method::POST, path::STDIN, Some(self.proc_ref(Some(0)))),
                false,
            )
            .await?;
        let (user, pump) = tokio::io::duplex(64 * 1024);
        tokio::spawn(pump_input(
            self.conn.clone(),
            self.spawned.id,
            pump,
            Some((H2Writer::new(stream), response, live)),
        ));
        Ok(InputWriter { user })
    }

    /// Its stdout, from the start; carried on from where it was cut off
    /// when the sandbox comes back.
    pub async fn stdout(&self) -> Result<OutputReader, SandboxError> {
        self.stdout_from(0).await
    }

    pub async fn stderr(&self) -> Result<OutputReader, SandboxError> {
        OutputReader::open(&self.conn, path::STDERR, self.spawned.id, 0).await
    }

    /// Its stdout from byte `offset` on: where a reader in an earlier node
    /// process had got to ([`OutputReader::offset`]).
    pub async fn stdout_from(&self, offset: u64) -> Result<OutputReader, SandboxError> {
        OutputReader::open(&self.conn, path::STDOUT, self.spawned.id, offset).await
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

    /// Wait for it to exit, asking again over a new connection when the
    /// sandbox comes back. The sandbox keeps the status for a later ask.
    pub async fn wait(&self) -> Result<ExitStatus, SandboxError> {
        loop {
            let live = self.conn.live().await?;
            match self
                .conn
                .get_json_on(&live, path::WAIT, Some(self.proc_ref(None)))
                .await
            {
                Ok(status) => return Ok(status),
                Err(error) => {
                    if !self.conn.worth_retrying(&live, error.of_connection()).await {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Wait, collecting everything it writes to stdout and stderr. For short
    /// commands; it holds all their output in memory.
    pub async fn output(&self) -> Result<Output, SandboxError> {
        let stdio = self.stdio().await?;
        if let Some(mut stdin) = stdio.stdin {
            stdin.shutdown().await?;
        }
        let read_all = |reader: Option<OutputReader>| async move {
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

type Reopening = Pin<Box<dyn Future<Output = io::Result<(H2Reader, Arc<Live>)>> + Send>>;

enum ReadState {
    Open(H2Reader, Arc<Live>),
    Reopening(Reopening),
    Done,
}

/// A process's stdout or stderr. When the connection carrying it drops, it
/// asks again from the byte it had reached once the sandbox is back, so
/// what it reads has neither gaps nor repeats.
pub struct OutputReader {
    conn: SandboxConn,
    path: &'static str,
    id: u64,
    /// Bytes handed to the reader so far: where to carry on from.
    offset: u64,
    state: ReadState,
}

impl OutputReader {
    async fn open(
        conn: &SandboxConn,
        path: &'static str,
        id: u64,
        from: u64,
    ) -> Result<Self, SandboxError> {
        let live = conn.live().await?;
        let args = protocol::encode_args(&ProcRef {
            id,
            from: Some(from),
        });
        let reader = conn.open_read_on(&live, path, args).await?;
        Ok(OutputReader {
            conn: conn.clone(),
            path,
            id,
            offset: from,
            state: ReadState::Open(reader, live),
        })
    }

    /// Bytes read so far.
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

async fn reopen_output(
    conn: SandboxConn,
    mut was: Arc<Live>,
    path: &'static str,
    id: u64,
    from: u64,
    mut failure: io::Error,
) -> io::Result<(H2Reader, Arc<Live>)> {
    loop {
        if !conn.worth_retrying(&was, io_of_connection(&failure)).await {
            return Err(failure);
        }
        let live = conn.live().await.map_err(io::Error::other)?;
        let args = protocol::encode_args(&ProcRef {
            id,
            from: Some(from),
        });
        match conn.open_read_on(&live, path, args).await {
            Ok(reader) => return Ok((reader, live)),
            Err(error) if error.of_connection() => {
                failure = io::Error::other(error);
                was = live;
            }
            Err(error) => return Err(io::Error::other(error)),
        }
    }
}

impl AsyncRead for OutputReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            match &mut this.state {
                ReadState::Open(reader, live) => {
                    let before = buf.filled().len();
                    match Pin::new(reader).poll_read(cx, buf) {
                        Poll::Ready(Ok(())) => {
                            let n = buf.filled().len() - before;
                            if n == 0 {
                                this.state = ReadState::Done;
                            }
                            this.offset += n as u64;
                            return Poll::Ready(Ok(()));
                        }
                        Poll::Ready(Err(error)) => {
                            if !this.conn.is_resumable() {
                                this.state = ReadState::Done;
                                return Poll::Ready(Err(error));
                            }
                            let live = live.clone();
                            this.state = ReadState::Reopening(Box::pin(reopen_output(
                                this.conn.clone(),
                                live,
                                this.path,
                                this.id,
                                this.offset,
                                error,
                            )));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                ReadState::Reopening(reopening) => match reopening.as_mut().poll(cx) {
                    Poll::Ready(Ok((reader, live))) => {
                        tracing::debug!(run = %this.conn.run(), from = this.offset, "output carried on over a new connection");
                        this.state = ReadState::Open(reader, live);
                    }
                    Poll::Ready(Err(error)) => {
                        this.state = ReadState::Done;
                        return Poll::Ready(Err(error));
                    }
                    Poll::Pending => return Poll::Pending,
                },
                ReadState::Done => return Poll::Ready(Ok(())),
            }
        }
    }
}

/// A process's stdin. Writes go to a task that sends them on and keeps the
/// last [`INPUT_KEPT`] bytes, to send again over a new connection when the
/// sandbox comes back; writes wait meanwhile. Once the process has closed
/// its stdin, or the run is lost, writes fail as on a broken pipe.
pub struct InputWriter {
    user: tokio::io::DuplexStream,
}

impl AsyncWrite for InputWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.user).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.user).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.user).poll_shutdown(cx)
    }
}

/// The tail of what was sent, from offset `base`.
#[derive(Default)]
struct Kept {
    base: u64,
    bytes: VecDeque<u8>,
}

impl Kept {
    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
        let over = self.bytes.len().saturating_sub(INPUT_KEPT);
        if over > 0 {
            self.bytes.drain(..over);
            self.base += over as u64;
        }
    }
}

type InputStream = (H2Writer, ResponseFuture, Arc<Live>);

async fn pump_input(
    conn: SandboxConn,
    id: u64,
    mut from_user: tokio::io::DuplexStream,
    mut stream: Option<InputStream>,
) {
    let mut kept = Kept::default();
    let mut ended = false;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let (mut writer, response, live) = match stream.take() {
            Some(stream) => stream,
            None => match reopen_input(&conn, id, &kept).await {
                Some(stream) => stream,
                None => return,
            },
        };
        let failure: SandboxError = loop {
            if ended {
                if let Err(error) = writer.shutdown().await {
                    break error.into();
                }
                match response.await {
                    Ok(_) => return,
                    Err(error) => break error.into(),
                }
            }
            let n = from_user.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                ended = true;
                continue;
            }
            kept.push(&buf[..n]);
            if let Err(error) = writer.write_all(&buf[..n]).await {
                break error.into();
            }
        };
        if !conn.worth_retrying(&live, failure.of_connection()).await {
            return;
        }
    }
}

/// A stdin stream over the current connection, with what is kept sent
/// again; the sandbox skips what it had written.
async fn reopen_input(conn: &SandboxConn, id: u64, kept: &Kept) -> Option<InputStream> {
    loop {
        let live = conn.live().await.ok()?;
        let attempt = async {
            let args = protocol::encode_args(&ProcRef {
                id,
                from: Some(kept.base),
            });
            let (response, stream) = conn
                .send_on(&live, request(Method::POST, path::STDIN, Some(args)), false)
                .await?;
            let mut writer = H2Writer::new(stream);
            let (a, b) = kept.bytes.as_slices();
            writer.write_all(a).await?;
            writer.write_all(b).await?;
            Ok::<_, SandboxError>((writer, response))
        }
        .await;
        match attempt {
            Ok((writer, response)) => return Some((writer, response, live)),
            Err(error) => {
                if !conn.worth_retrying(&live, error.of_connection()).await {
                    return None;
                }
            }
        }
    }
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
        let Ok(live) = conn.live().await else {
            return;
        };
        let accepted = async {
            let (response, to_sandbox) = conn
                .send_on(
                    &live,
                    request(Method::POST, path::TUNNEL_ACCEPT, Some(args.clone())),
                    false,
                )
                .await?;
            Ok::<_, SandboxError>((checked(response).await?, to_sandbox))
        }
        .await;
        let (from_sandbox, to_sandbox) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                if conn.worth_retrying(&live, error.of_connection()).await {
                    continue;
                }
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
    conn: SandboxConn,
    live: Arc<Live>,
    reader: BufReader<H2Reader>,
    line: Vec<u8>,
}

impl EventStream {
    /// The next event, or `None` when the stream has ended.
    pub async fn next(&mut self) -> Result<Option<Event>, SandboxError> {
        loop {
            self.line.clear();
            let read = (&mut self.reader)
                .take(MAX_EVENT_LINE as u64 + 1)
                .read_until(b'\n', &mut self.line)
                .await;
            let read = match read {
                Ok(read) => read,
                Err(error) => {
                    if !self
                        .conn
                        .worth_retrying(&self.live, io_of_connection(&error))
                        .await
                    {
                        return Err(error.into());
                    }
                    let live = self.conn.live().await?;
                    self.reader = self.conn.open_events(&live).await?;
                    self.live = live;
                    continue;
                }
            };
            if read == 0 {
                return Ok(None);
            }
            if self.line.last() != Some(&b'\n') {
                return Err(SandboxError::Protocol(
                    "event line too long or cut short".into(),
                ));
            }
            return serde_json::from_slice(&self.line)
                .map(Some)
                .map_err(|e| SandboxError::Protocol(e.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_redeemed_once() {
        let tokens = TokenRegistry::default();
        let token = tokens.issue("run-1");
        assert!(tokens.outstanding("run-1"));
        assert_eq!(
            tokens.redeem(&token, None),
            Some(Redeemed::First("run-1".into()))
        );
        assert_eq!(tokens.redeem(&token, None), None);
        assert!(!tokens.outstanding("run-1"));
    }

    #[test]
    fn a_windowed_token_is_refused_once_its_window_has_passed() {
        let tokens = TokenRegistry::default();
        let late = tokens.issue_within("late", Duration::from_millis(1));
        let early = tokens.issue_within("early", Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(20));
        assert!(!tokens.outstanding("late"));
        assert_eq!(tokens.redeem(&late, None), None);
        assert_eq!(
            tokens.redeem(&early, None),
            Some(Redeemed::First("early".into()))
        );
    }

    #[test]
    fn issuing_drops_expired_tokens() {
        let tokens = TokenRegistry::default();
        tokens.issue_within("gone", Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(20));
        tokens.issue("fresh");
        assert_eq!(tokens.issued.lock().unwrap().len(), 1);
    }

    #[test]
    fn revoking_forgets_the_runs_tokens() {
        let tokens = TokenRegistry::default();
        let token = tokens.issue_within("run", Duration::from_secs(60));
        let secret = tokens.issue_resume("run");
        tokens.revoke("run");
        assert_eq!(tokens.redeem(&token, None), None);
        assert_eq!(tokens.redeem(&secret, None), None);
        assert!(!tokens.resumable("run"));
    }

    #[test]
    fn a_resume_secret_works_until_revoked_and_is_no_first_use_token() {
        let tokens = TokenRegistry::default();
        let secret = tokens.issue_resume("run");
        assert!(!tokens.outstanding("run"));
        assert!(tokens.resumable("run"));
        for _ in 0..3 {
            assert_eq!(
                tokens.redeem(&secret, None),
                Some(Redeemed::Resume("run".into()))
            );
        }
        tokens.revoke("run");
        assert_eq!(tokens.redeem(&secret, None), None);
    }

    #[test]
    fn a_bound_carrier_neither_spends_nor_accepts_another_runs_token() {
        let tokens = TokenRegistry::default();
        let token = tokens.issue("a");
        let secret = tokens.issue_resume("a");
        assert_eq!(tokens.redeem(&token, Some("b")), None);
        assert_eq!(tokens.redeem(&secret, Some("b")), None);
        assert!(
            tokens.outstanding("a"),
            "a refusal elsewhere spends nothing"
        );
        assert_eq!(
            tokens.redeem(&secret, Some("a")),
            Some(Redeemed::Resume("a".into()))
        );
        assert_eq!(
            tokens.redeem(&token, Some("a")),
            Some(Redeemed::First("a".into()))
        );
    }

    #[test]
    fn only_a_dead_connection_is_worth_a_redial() {
        let io = io::Error::new(io::ErrorKind::BrokenPipe, "connection closed");
        assert!(io_of_connection(&io));
        let reset = h2::Error::from(h2::Reason::CANCEL);
        assert!(!h2_of_connection(&reset));
        assert!(!SandboxError::Remote {
            status: StatusCode::NOT_FOUND,
            message: "no".into()
        }
        .of_connection());
        assert!(!SandboxError::Lost.of_connection());
    }

    #[test]
    fn kept_input_holds_its_tail() {
        let mut kept = Kept::default();
        kept.push(&vec![1u8; INPUT_KEPT]);
        kept.push(&[2u8; 10]);
        assert_eq!(kept.base, 10);
        assert_eq!(kept.bytes.len(), INPUT_KEPT);
        assert_eq!(kept.bytes.back(), Some(&2));
    }
}
