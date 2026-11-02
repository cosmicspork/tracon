//! What runs inside the sandbox: dial the node, prove the run's token, then
//! serve the node's requests until it says stop, the connection is lost, or
//! a signal arrives — and tear everything down either way.

mod fd;
mod procs;
mod server;
mod services;
pub mod spawner;
mod tunnels;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;

use crate::carrier::Dial;
use crate::protocol::{Ack, Hello, DEFAULT_KEEPALIVE, DEFAULT_SERVER_NAME, MAX_HELLO_BYTES};
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
    /// How long to keep dialling a node that is not answering yet.
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
    /// The connection closed or stopped answering pings.
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

async fn dial(config: &Config) -> Result<crate::carrier::BoxedIo, RunError> {
    let deadline = tokio::time::Instant::now() + config.connect_timeout;
    let mut delay = Duration::from_millis(50);
    loop {
        match config.dial.connect().await {
            Ok(io) => return Ok(io),
            Err(error) if tokio::time::Instant::now() + delay >= deadline => {
                return Err(RunError::Dial(config.dial.to_string(), error));
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

/// Dial, prove the token, serve until the end, tear down. Must run inside a
/// Tokio runtime, in a process where nothing else waits for children.
pub async fn run(config: Config) -> Result<Ending, RunError> {
    let spawner = Spawner::new();
    // Installed before anything is started, so no exit goes unnoticed.
    let _reaper = spawner
        .start_reaper()
        .map_err(|e| RunError::Handshake(format!("install SIGCHLD handler: {e}")))?;
    let mut terminate = Signals::install()
        .map_err(|e| RunError::Handshake(format!("install signal handlers: {e}")))?;

    let client = tls::client_config(config.pin).map_err(|e| RunError::Tls(e.to_string()))?;
    let server_name = rustls::pki_types::ServerName::try_from(config.server_name.clone())
        .map_err(|e| RunError::Tls(e.to_string()))?;
    let io = tokio::select! {
        io = dial(&config) => io?,
        signal = terminate.next() => return Ok(Ending::Signal(signal)),
    };
    let handshake = async {
        let mut tls = tokio_rustls::TlsConnector::from(client)
            .connect(server_name, io)
            .await
            .map_err(|e| RunError::Tls(e.to_string()))?;
        if tls.get_ref().1.alpn_protocol() != Some(crate::protocol::ALPN) {
            return Err(RunError::Tls("the node did not agree the protocol".into()));
        }
        let hello = Hello {
            token: config.token.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            arch: std::env::consts::ARCH.into(),
            pid: std::process::id(),
            init: is_init(),
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
        h2::server::Builder::new()
            .initial_window_size(STREAM_WINDOW)
            .initial_connection_window_size(CONNECTION_WINDOW)
            .max_frame_size(MAX_FRAME)
            .max_concurrent_streams(MAX_STREAMS)
            .handshake::<_, bytes::Bytes>(tls)
            .await
            .map_err(|e| RunError::Handshake(e.to_string()))
    };
    let mut connection = tokio::time::timeout(Duration::from_secs(30), handshake)
        .await
        .map_err(|_| RunError::Handshake("timed out".into()))??;
    tracing::info!(node = %config.dial, "connected");

    let state = Arc::new(State {
        procs: procs::Procs::new(spawner.clone()),
        tunnels: tunnels::Tunnels::default(),
        services: services::Supervisor::new(spawner.clone()),
        shutdown: CancellationToken::new(),
    });
    let ping_pong = connection.ping_pong().expect("ping_pong taken once");
    let closing = CancellationToken::new();
    let drive = {
        let state = state.clone();
        let closing = closing.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = connection.accept() => match accepted {
                        Some(Ok((request, respond))) => {
                            tokio::spawn(server::handle(state.clone(), request, respond));
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
    let mut drive = drive;
    let alive = keepalive(ping_pong, config.keepalive);
    tokio::pin!(alive);

    let mut driven = false;
    let ending = loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break Ending::Shutdown,
            _ = &mut drive => {
                driven = true;
                break Ending::Lost;
            }
            _ = &mut alive => break Ending::Lost,
            signal = terminate.next() => {
                if Signals::TERMINATING.contains(&signal) {
                    break Ending::Signal(signal);
                }
                state.procs.forward(signal);
                state.services.forward(signal);
            }
        }
    };
    tracing::info!(?ending, "tearing down");

    // Services first, newest first, so their stop events still reach the
    // node while the connection lasts; then whatever the node started.
    let _ = state.services.stop(&[]).await;
    state.procs.terminate_all(config.stop_grace).await;
    state.tunnels.close_all();
    closing.cancel();
    if !driven {
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
