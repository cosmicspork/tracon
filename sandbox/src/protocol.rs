//! What the two sides say to each other: the hello that precedes HTTP/2, the
//! paths the node requests, and the JSON bodies they carry.
//!
//! Streaming requests (stdio, tar, tunnels) carry their small arguments as
//! base64url JSON in the [`ARGS_HEADER`] so the body is the raw stream;
//! control requests carry them as the JSON body.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// The protocol's ALPN identifier. Both sides insist on it, so a TLS
/// connection that reached the wrong listener fails at the handshake instead
/// of speaking HTTP/2 to something else.
pub const ALPN: &[u8] = b"tracon-sandbox/1";

/// The name the sandbox sends as SNI when none is configured. The pin, not the
/// name, authenticates the node; a relay that routes by SNI can be given a
/// per-node name instead.
pub const DEFAULT_SERVER_NAME: &str = "tracon-node";

/// Hello and ack are each one length-prefixed JSON object, no larger than this.
pub const MAX_HELLO_BYTES: usize = 4096;

/// The largest JSON body either side reads for a control request or reply.
pub const MAX_JSON_BYTES: usize = 1024 * 1024;

/// The largest base64url-encoded [`ARGS_HEADER`] value the sandbox accepts.
pub const MAX_ARGS_HEADER_BYTES: usize = 64 * 1024;

/// Arguments of a streaming request, as base64url (no padding) JSON.
pub const ARGS_HEADER: &str = "tracon-args";

/// How often each side sends an HTTP/2 PING when nothing says otherwise. A
/// provider's proxy drops a connection idle for much longer than this.
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(20);

/// What the sandbox says first, inside TLS and before HTTP/2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    /// The per-run token from the sandbox's bootstrap environment.
    pub token: String,
    /// Everything below is what the sandbox *claims*. The node may log or
    /// display it but must never decide anything on it.
    pub version: String,
    pub arch: String,
    pub pid: u32,
    /// Whether it runs as its namespace's init.
    pub init: bool,
}

impl std::fmt::Display for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tracon-sandbox {} ({}, pid {}{})",
            self.version,
            self.arch,
            self.pid,
            if self.init { ", init" } else { "" }
        )
    }
}

/// Everything in [`Hello`] except the token: what the node keeps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxInfo {
    pub version: String,
    pub arch: String,
    pub pid: u32,
    pub init: bool,
}

impl From<&Hello> for SandboxInfo {
    fn from(hello: &Hello) -> Self {
        SandboxInfo {
            version: hello.version.clone(),
            arch: hello.arch.clone(),
            pid: hello.pid,
            init: hello.init,
        }
    }
}

/// The node's answer to [`Hello`]. A refusal is followed by the node closing
/// the connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Request paths. Every one is a request the node makes.
pub mod path {
    pub const SPAWN: &str = "/v1/proc/spawn";
    pub const STDIN: &str = "/v1/proc/stdin";
    pub const STDOUT: &str = "/v1/proc/stdout";
    pub const STDERR: &str = "/v1/proc/stderr";
    pub const SIGNAL: &str = "/v1/proc/signal";
    pub const RESIZE: &str = "/v1/proc/resize";
    pub const WAIT: &str = "/v1/proc/wait";
    pub const IMPORT: &str = "/v1/files/import";
    pub const EXPORT: &str = "/v1/files/export";
    pub const TUNNEL_LISTEN: &str = "/v1/tunnel/listen";
    pub const TUNNEL_ACCEPT: &str = "/v1/tunnel/accept";
    pub const TUNNEL_CLOSE: &str = "/v1/tunnel/close";
    pub const SERVICES_START: &str = "/v1/services/start";
    pub const SERVICES_STATUS: &str = "/v1/services/status";
    pub const SERVICES_STOP: &str = "/v1/services/stop";
    pub const EVENTS: &str = "/v1/events";
    pub const SHUTDOWN: &str = "/v1/shutdown";
}

/// Encode a streaming request's arguments for [`ARGS_HEADER`].
pub fn encode_args<T: Serialize>(args: &T) -> String {
    let json = serde_json::to_vec(args).expect("protocol arguments serialize");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// Decode [`ARGS_HEADER`], refusing anything over [`MAX_ARGS_HEADER_BYTES`].
pub fn decode_args<T: for<'de> Deserialize<'de>>(value: &[u8]) -> Result<T, String> {
    if value.len() > MAX_ARGS_HEADER_BYTES {
        return Err("arguments too large".into());
    }
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|e| format!("arguments: {e}"))?;
    serde_json::from_slice(&json).map_err(|e| format!("arguments: {e}"))
}

/// Refusals and failures come back as a non-2xx status with this body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

/// Start a process inside the sandbox.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// The program and its arguments; `argv[0]` is looked up on `PATH`.
    pub argv: Vec<String>,
    /// Added to (or, with `env_clear`, instead of) the sandbox's own
    /// environment, which never includes its bootstrap variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub env_clear: bool,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub gid: Option<u32>,
    /// Run on a pseudo-terminal of this size: stdin and stdout are its master
    /// side, stderr is merged into it, and the process leads a new session.
    #[serde(default)]
    pub tty: Option<WinSize>,
    /// Which of the three streams are pipes the node attaches to; the others
    /// are `/dev/null`. Ignored with `tty`. A pipe nobody attaches to is never
    /// drained, so a process writing to one blocks: that is the backpressure.
    #[serde(default = "yes")]
    pub stdin: bool,
    #[serde(default = "yes")]
    pub stdout: bool,
    #[serde(default = "yes")]
    pub stderr: bool,
}

fn yes() -> bool {
    true
}

impl SpawnSpec {
    pub fn new<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        SpawnSpec {
            argv: argv.into_iter().map(Into::into).collect(),
            stdin: true,
            stdout: true,
            stderr: true,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Spawned {
    pub id: u64,
    pub pid: u32,
}

/// Names one spawned process in a streaming or control request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ProcRef {
    pub id: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SignalRequest {
    pub id: u64,
    pub signal: i32,
    /// Signal the process group the process leads rather than the process
    /// alone. Every spawned process leads one.
    #[serde(default)]
    pub group: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ResizeRequest {
    pub id: u64,
    pub size: WinSize,
}

/// How a process ended.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExitStatus {
    /// Its exit code, when it exited.
    pub code: Option<i32>,
    /// The signal that killed it, when one did.
    pub signal: Option<i32>,
}

impl ExitStatus {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// From a raw `waitpid` status.
    pub fn from_raw(status: i32) -> Self {
        if libc::WIFEXITED(status) {
            ExitStatus {
                code: Some(libc::WEXITSTATUS(status)),
                signal: None,
            }
        } else if libc::WIFSIGNALED(status) {
            ExitStatus {
                code: None,
                signal: Some(libc::WTERMSIG(status)),
            }
        } else {
            ExitStatus {
                code: None,
                signal: None,
            }
        }
    }
}

/// Which files an export leaves behind.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Exclusions {
    /// Everything under the directory.
    None,
    /// What the node's checkout export keeps: with a `.git` directory, the
    /// files the index tracks, the untracked ones the ignore rules leave in,
    /// and the repository itself; without one, everything.
    #[default]
    Checkout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRequest {
    pub dir: PathBuf,
    #[serde(default)]
    pub limits: Option<crate::files::Limits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRequest {
    pub dir: PathBuf,
    #[serde(default)]
    pub exclusions: Exclusions,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreeSummary {
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    pub bytes: u64,
}

/// Where a tunnel listens inside the sandbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ListenAddr {
    /// `host:port`; port 0 picks one, and the bound address comes back.
    Tcp(String),
    Unix(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelListen {
    pub name: String,
    pub listen: ListenAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelListening {
    /// The address actually bound.
    pub bound: ListenAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelRef {
    pub name: String,
}

/// A long-running process the sandbox keeps up for the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSpec {
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub gid: Option<u32>,
    #[serde(default)]
    pub readiness: Option<Readiness>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default)]
    pub backoff: Backoff,
    /// Sent to the service's process group to stop it; SIGKILL follows after
    /// `stop_timeout_ms`.
    #[serde(default = "sigterm")]
    pub stop_signal: i32,
    #[serde(default = "five_seconds")]
    pub stop_timeout_ms: u64,
}

fn sigterm() -> i32 {
    libc::SIGTERM
}

fn five_seconds() -> u64 {
    5_000
}

impl ServiceSpec {
    pub fn new<I, S>(name: impl Into<String>, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        ServiceSpec {
            name: name.into(),
            argv: argv.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
            cwd: None,
            uid: None,
            gid: None,
            readiness: None,
            restart: RestartPolicy::default(),
            backoff: Backoff::default(),
            stop_signal: sigterm(),
            stop_timeout_ms: five_seconds(),
        }
    }
}

/// When a service counts as ready.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Readiness {
    pub probe: Probe,
    #[serde(default = "quarter_second")]
    pub interval_ms: u64,
    /// Not ready this long after starting counts as a failed start.
    #[serde(default = "thirty_seconds")]
    pub timeout_ms: u64,
}

fn quarter_second() -> u64 {
    250
}

fn thirty_seconds() -> u64 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Probe {
    /// A TCP connect to `host:port` succeeds.
    Tcp(String),
    /// The command exits 0.
    Command(Vec<String>),
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    Never,
    #[default]
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Backoff {
    pub initial_ms: u64,
    pub max_ms: u64,
    /// A run at least this long resets the delay to `initial_ms`.
    pub reset_after_ms: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            initial_ms: 500,
            max_ms: 30_000,
            reset_after_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServicesStart {
    pub services: Vec<ServiceSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServicesStop {
    /// Every service when empty.
    #[serde(default)]
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServiceState {
    Starting {
        attempt: u32,
    },
    Running {
        pid: u32,
    },
    Ready,
    Exited {
        status: ExitStatus,
    },
    Restarting {
        attempt: u32,
        delay_ms: u64,
    },
    /// Gave up: the restart policy says no more, or it could not start.
    Failed {
        reason: String,
    },
    /// Stopped on request or at teardown.
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceStatus {
    pub name: String,
    pub state: ServiceState,
    pub restarts: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// One line of the `events` stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Service {
        name: String,
        #[serde(flatten)]
        state: ServiceState,
    },
    Log {
        service: String,
        stream: LogStream,
        line: String,
    },
    /// The node read too slowly and this many events were dropped.
    Lagged { missed: u64 },
}

/// The longest log line sent whole; longer ones are cut here.
pub const MAX_LOG_LINE: usize = 16 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_round_trip_and_bound() {
        let args = TunnelRef { name: "db".into() };
        let encoded = encode_args(&args);
        let back: TunnelRef = decode_args(encoded.as_bytes()).unwrap();
        assert_eq!(back.name, "db");
        let huge = "A".repeat(MAX_ARGS_HEADER_BYTES + 1);
        assert!(decode_args::<TunnelRef>(huge.as_bytes()).is_err());
    }

    #[test]
    fn events_are_flat_json() {
        let event = Event::Service {
            name: "db".into(),
            state: ServiceState::Starting { attempt: 2 },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "service", "name": "db", "state": "starting", "attempt": 2})
        );
        let back: Event = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn spawn_spec_defaults_to_piped_stdio() {
        let spec: SpawnSpec = serde_json::from_str(r#"{"argv":["true"]}"#).unwrap();
        assert!(spec.stdin && spec.stdout && spec.stderr);
    }
}
