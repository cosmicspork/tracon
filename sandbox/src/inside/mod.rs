//! What runs inside the sandbox: dial the node, prove the run's token, then
//! serve the node's requests until it says stop, the run is lost, or a
//! signal arrives — and tear everything down either way.
//!
//! A dropped connection does not end the run when the node gave a resume
//! secret: everything started keeps running, output piles up in each
//! process's bounded log (and the process blocks once that is full), and the
//! sandbox redials with the secret, with backoff, for as long as the node
//! said it would wait. A listening sandbox listens again. The node refusing
//! the secret (the run is over, or it no longer knows it) or presenting
//! another certificate ends the run at once; so does the window passing.

mod fd;
mod procs;
mod server;
mod services;
pub mod spawner;
mod tunnels;
pub mod user;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::carrier::{BoxedIo, Dial};
use crate::protocol::{
    Ack, Hello, Resume, DEFAULT_KEEPALIVE, DEFAULT_SERVER_NAME, MAX_HELLO_BYTES,
};
use crate::tls::{self, CertPin};
use server::State;
use spawner::Spawner;

/// Everything the sandbox needs to find and prove itself to its node.
#[derive(Debug, Clone)]
pub struct Config {
    pub dial: Dial,
    pub pin: CertPin,
    pub token: String,
    /// Sent as SNI; see [`DEFAULT_SERVER_NAME`].
    pub server_name: String,
    pub keepalive: Duration,
    /// How long to keep dialling a node that is not answering yet, or to
    /// wait for it to connect to a listening sandbox.
    pub connect_timeout: Duration,
    /// How long processes get between SIGTERM and SIGKILL at teardown.
    pub stop_grace: Duration,
}

impl Config {
    pub fn new(dial: Dial, pin: CertPin, token: impl Into<String>) -> Self {
        Config {
            dial,
            pin,
            token: token.into(),
            server_name: DEFAULT_SERVER_NAME.into(),
            keepalive: DEFAULT_KEEPALIVE,
            connect_timeout: Duration::from_secs(30),
            stop_grace: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("dial {0}: {1}")]
    Dial(String, io::Error),
    #[error("tls: {0}")]
    Tls(String),
    #[error("the node refused this sandbox: {0}")]
    Refused(String),
    #[error("handshake: {0}")]
    Handshake(String),
}

/// Why the sandbox stopped serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The node asked it to.
    Shutdown,
    /// The connection closed or stopped answering pings, and the node did
    /// not take it back.
    Lost,
    /// A terminating signal arrived.
    Signal(i32),
}

/// Whether this process is its PID namespace's init.
pub fn is_init() -> bool {
    std::process::id() == 1
}

/// Under another init, become the subreaper for everything started here, so
/// orphans are reaped by this process rather than escaping to that init.
pub fn become_subreaper() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }
}

/// The node's stream within `within`: dialled, retried with backoff, or for
/// a listening sandbox, the next connection in.
async fn dial(target: &Dial, within: Duration) -> Result<BoxedIo, RunError> {
    if let Dial::Listen(_) = target {
        return match tokio::time::timeout(within, target.connect()).await {
            Ok(Ok(io)) => Ok(io),
            Ok(Err(error)) => Err(RunError::Dial(target.to_string(), error)),
            Err(_) => Err(RunError::Dial(
                target.to_string(),
                io::Error::new(io::ErrorKind::TimedOut, "the node did not connect"),
            )),
        };
    }
    let deadline = Instant::now() + within;
    let mut delay = Duration::from_millis(50);
    loop {
        // A WebSocket dial is several exchanges, any of which a proxy can
        // leave unanswered; none may outlast the window.
        let left = deadline.saturating_duration_since(Instant::now());
        let attempt = match tokio::time::timeout(left, target.connect()).await {
            Ok(attempt) => attempt,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "no answer in time")),
        };
        match attempt {
            Ok(io) => return Ok(io),
            Err(error) if Instant::now() + delay >= deadline => {
                return Err(RunError::Dial(target.to_string(), error));
            }
            Err(error) => {
                tracing::debug!(%error, "node not answering yet");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
            }
        }
    }
}

pub(crate) async fn write_frame<W: AsyncWrite + Unpin, T: serde::Serialize>(
    io: &mut W,
    value: &T,
) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    if body.len() > MAX_HELLO_BYTES {
        return Err(io::Error::other("frame too large"));
    }
    io.write_all(&(body.len() as u16).to_be_bytes()).await?;
    io.write_all(&body).await?;
    io.flush().await
}

pub(crate) async fn read_frame<R: AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    io: &mut R,
) -> io::Result<T> {
    let mut len = [0u8; 2];
    io.read_exact(&mut len).await?;
    let len = u16::from_be_bytes(len) as usize;
    if len > MAX_HELLO_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    io.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The HTTP/2 settings both ends use: windows large enough for a fast link
/// with some latency, small enough that a stalled reader holds little.
pub(crate) const STREAM_WINDOW: u32 = 1024 * 1024;
pub(crate) const CONNECTION_WINDOW: u32 = 8 * 1024 * 1024;
pub(crate) const MAX_FRAME: u32 = 256 * 1024;
pub(crate) const MAX_STREAMS: u32 = 4096;

/// How long one TLS handshake, hello and HTTP/2 setup may take.
const HANDSHAKE_WITHIN: Duration = Duration::from_secs(30);

/// Ping every `interval`; resolve when a pong fails to come back within it.
pub(crate) async fn keepalive(mut ping_pong: h2::PingPong, interval: Duration) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tick.tick().await;
        match tokio::time::timeout(interval, ping_pong.ping(h2::Ping::opaque())).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::debug!(%error, "keepalive ping failed");
                return;
            }
            Err(_) => {
                tracing::warn!("keepalive: no pong within {interval:?}");
                return;
            }
        }
    }
}

type Connection = h2::server::Connection<tokio_rustls::client::TlsStream<BoxedIo>, bytes::Bytes>;

/// A connection that has passed the hello, and what the node said of
/// coming back.
struct Connected {
    connection: Connection,
    resume: Option<Resume>,
}

/// TLS against the pin, the hello with `credential`, the node's ack, then
/// HTTP/2 with the roles reversed.
async fn handshake(
    config: &Config,
    client: Arc<rustls::ClientConfig>,
    io: BoxedIo,
    credential: &str,
) -> Result<Connected, RunError> {
    let server_name = rustls::pki_types::ServerName::try_from(config.server_name.clone())
        .map_err(|e| RunError::Tls(e.to_string()))?;
    let mut tls = tokio_rustls::TlsConnector::from(client)
        .connect(server_name, io)
        .await
        .map_err(|e| match pin_refused(&e) {
            true => RunError::Refused(format!("the node's certificate does not match: {e}")),
            false => RunError::Tls(e.to_string()),
        })?;
    if tls.get_ref().1.alpn_protocol() != Some(crate::protocol::ALPN) {
        return Err(RunError::Tls("the node did not agree the protocol".into()));
    }
    let hello = Hello {
        token: credential.to_string(),
        version: env!("CARGO_PKG_VERSION").into(),
        arch: std::env::consts::ARCH.into(),
        pid: std::process::id(),
        init: is_init(),
        resumable: true,
        uid: Some(user::current().uid),
    };
    write_frame(&mut tls, &hello)
        .await
        .map_err(|e| RunError::Handshake(e.to_string()))?;
    let ack: Ack = read_frame(&mut tls)
        .await
        .map_err(|e| RunError::Handshake(format!("no answer to hello: {e}")))?;
    if !ack.ok {
        return Err(RunError::Refused(ack.error.unwrap_or_default()));
    }
    let connection = h2::server::Builder::new()
        .initial_window_size(STREAM_WINDOW)
        .initial_connection_window_size(CONNECTION_WINDOW)
        .max_frame_size(MAX_FRAME)
        .max_concurrent_streams(MAX_STREAMS)
        .handshake::<_, bytes::Bytes>(tls)
        .await
        .map_err(|e| RunError::Handshake(e.to_string()))?;
    Ok(Connected {
        connection,
        resume: ack.resume,
    })
}

/// Whether the TLS failure is the node's certificate not matching the pin:
/// a node whose identity changed, which redialling cannot fix.
fn pin_refused(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .is_some_and(|e| matches!(e, rustls::Error::InvalidCertificate(_)))
}

/// Dial and prove `credential` once: the dial within `within`, the rest
/// within `handshake_within`.
async fn connect(
    config: &Config,
    client: Arc<rustls::ClientConfig>,
    credential: &str,
    within: Duration,
    handshake_within: Duration,
) -> Result<Connected, RunError> {
    let io = dial(&config.dial, within).await?;
    tokio::time::timeout(handshake_within, handshake(config, client, io, credential))
        .await
        .map_err(|_| RunError::Handshake("timed out".into()))?
}

/// How one connection ended.
enum Served {
    Shutdown {
        drive: JoinHandle<()>,
        closing: CancellationToken,
    },
    Signal {
        signal: i32,
        drive: JoinHandle<()>,
        closing: CancellationToken,
    },
    Lost,
}

/// Serve the node's requests on `connection` until it is lost, the node
/// asks for a shutdown, or a terminating signal arrives. Signals that do
/// not terminate are passed on meanwhile.
async fn serve(
    state: &Arc<State>,
    mut connection: Connection,
    keepalive_every: Duration,
    terminate: &mut Signals,
) -> Served {
    let ping_pong = connection.ping_pong().expect("ping_pong taken once");
    let closing = CancellationToken::new();
    // Every request answered on this connection gives up when it is lost.
    let gone = CancellationToken::new();
    let mut drive = {
        let state = state.clone();
        let closing = closing.clone();
        let gone = gone.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = connection.accept() => match accepted {
                        Some(Ok((request, respond))) => {
                            tokio::spawn(server::handle(state.clone(), gone.clone(), request, respond));
                        }
                        Some(Err(error)) => {
                            tracing::debug!(%error, "connection error");
                            return;
                        }
                        None => return,
                    },
                    _ = closing.cancelled() => {
                        connection.graceful_shutdown();
                        // Drain what is in flight, then the connection ends.
                        while let Some(Ok(_)) = connection.accept().await {}
                        return;
                    }
                }
            }
        })
    };
    let alive = keepalive(ping_pong, keepalive_every);
    tokio::pin!(alive);
    let served = loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break Served::Shutdown { drive, closing },
            _ = &mut drive => break Served::Lost,
            _ = &mut alive => {
                // Dropping the connection fails every stream still open on
                // it, so their handlers let go of what they hold.
                drive.abort();
                break Served::Lost;
            }
            signal = terminate.next() => {
                if Signals::TERMINATING.contains(&signal) {
                    break Served::Signal { signal, drive, closing };
                }
                state.procs.forward(signal);
                state.services.forward(signal);
            }
        }
    };
    if let Served::Lost = served {
        gone.cancel();
    }
    served
}

/// How a redial ended.
enum Redialled {
    Connected(Box<Connection>),
    Signal(i32),
    Shutdown,
    GaveUp(String),
}

/// Redial with the resume secret until the node takes the sandbox back or
/// `resume.within_ms` has passed. A refusal ends it at once: the node no
/// longer knows the run, or is not the node this sandbox trusts.
async fn redial(
    config: &Config,
    client: &Arc<rustls::ClientConfig>,
    resume: &Resume,
    state: &Arc<State>,
    terminate: &mut Signals,
) -> Redialled {
    let deadline = Instant::now() + Duration::from_millis(resume.within_ms);
    let mut delay = Duration::from_millis(100);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Redialled::GaveUp(format!(
                "the node did not take it back within {} s",
                resume.within_ms / 1000
            ));
        }
        let attempt = tokio::select! {
            attempt = connect(
                config,
                client.clone(),
                &resume.secret,
                left,
                HANDSHAKE_WITHIN.min(left.max(Duration::from_secs(1))),
            ) => attempt,
            _ = state.shutdown.cancelled() => return Redialled::Shutdown,
            signal = terminate.next() => {
                if Signals::TERMINATING.contains(&signal) {
                    return Redialled::Signal(signal);
                }
                state.procs.forward(signal);
                state.services.forward(signal);
                continue;
            }
        };
        match attempt {
            Ok(connected) => return Redialled::Connected(Box::new(connected.connection)),
            Err(RunError::Refused(why)) => return Redialled::GaveUp(why),
            Err(error) => {
                tracing::debug!(%error, "redial failed");
                let pause = delay.min(deadline.saturating_duration_since(Instant::now()));
                tokio::select! {
                    _ = tokio::time::sleep(pause) => {}
                    _ = state.shutdown.cancelled() => return Redialled::Shutdown,
                }
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
}

/// Dial, prove the token, serve until the end (redialling when the node
/// allows it), tear down. Must run inside a Tokio runtime, in a process where
/// nothing else waits for children.
pub async fn run(config: Config) -> Result<Ending, RunError> {
    let spawner = Spawner::new();
    // Installed before anything is started, so no exit goes unnoticed.
    let _reaper = spawner
        .start_reaper()
        .map_err(|e| RunError::Handshake(format!("install SIGCHLD handler: {e}")))?;
    let mut terminate = Signals::install()
        .map_err(|e| RunError::Handshake(format!("install signal handlers: {e}")))?;

    let client = tls::client_config(config.pin).map_err(|e| RunError::Tls(e.to_string()))?;
    let first = tokio::select! {
        connected = connect(
            &config,
            client.clone(),
            &config.token,
            config.connect_timeout,
            HANDSHAKE_WITHIN,
        ) => connected?,
        signal = terminate.next() => return Ok(Ending::Signal(signal)),
    };
    tracing::info!(node = %config.dial, resumable = first.resume.is_some(), "connected");
    let resume = first.resume;

    let state = Arc::new(State {
        procs: procs::Procs::new(spawner.clone()),
        tunnels: tunnels::Tunnels::default(),
        services: services::Supervisor::new(spawner.clone()),
        shutdown: CancellationToken::new(),
    });

    let mut connection = first.connection;
    let (ending, last) = loop {
        match serve(&state, connection, config.keepalive, &mut terminate).await {
            Served::Shutdown { drive, closing } => {
                break (Ending::Shutdown, Some((drive, closing)))
            }
            Served::Signal {
                signal,
                drive,
                closing,
            } => break (Ending::Signal(signal), Some((drive, closing))),
            Served::Lost => {}
        }
        let Some(resume) = &resume else {
            break (Ending::Lost, None);
        };
        tracing::warn!(
            within_s = resume.within_ms / 1000,
            "lost the node; everything keeps running while it redials"
        );
        match redial(&config, &client, resume, &state, &mut terminate).await {
            Redialled::Connected(next) => {
                tracing::info!(node = %config.dial, "reconnected");
                connection = *next;
            }
            Redialled::Signal(signal) => break (Ending::Signal(signal), None),
            Redialled::Shutdown => break (Ending::Shutdown, None),
            Redialled::GaveUp(why) => {
                tracing::warn!(%why, "the node did not take this sandbox back");
                break (Ending::Lost, None);
            }
        }
    };
    tracing::info!(?ending, "tearing down");

    // Services first, newest first, so their stop events still reach the
    // node while the connection lasts; then whatever the node started.
    let _ = state.services.stop(&[]).await;
    state.procs.terminate_all(config.stop_grace).await;
    state.tunnels.close_all();
    if let Some((mut drive, closing)) = last {
        closing.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut drive).await;
        drive.abort();
    }
    Ok(ending)
}

struct Signals {
    streams: Vec<(i32, tokio::signal::unix::Signal)>,
}

impl Signals {
    /// End the sandbox; everything else is passed on to what it runs.
    const TERMINATING: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGQUIT];
    const FORWARDED: [i32; 6] = [
        libc::SIGTERM,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGHUP,
        libc::SIGUSR1,
        libc::SIGUSR2,
    ];

    fn install() -> io::Result<Self> {
        let streams = Self::FORWARDED
            .iter()
            .map(|&number| Ok((number, signal(SignalKind::from_raw(number))?)))
            .collect::<io::Result<_>>()?;
        Ok(Signals { streams })
    }

    async fn next(&mut self) -> i32 {
        std::future::poll_fn(|cx| {
            for (number, stream) in &mut self.streams {
                if stream.poll_recv(cx).is_ready() {
                    return std::task::Poll::Ready(*number);
                }
            }
            std::task::Poll::Pending
        })
        .await
    }
}
