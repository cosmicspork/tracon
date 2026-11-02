//! `tracon-sandbox`: dial the node named in the environment and serve it.
//!
//! Configured entirely from its bootstrap environment, which it removes from
//! its own before anything is started, so no child ever inherits the token:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `TRACON_SANDBOX_ADDR` | the node as `host:port` (TCP), or |
//! | `TRACON_SANDBOX_SOCKET` | the node's unix socket, or |
//! | `TRACON_SANDBOX_LISTEN` | a unix socket to listen on for the node to connect to |
//! | `TRACON_SANDBOX_PIN` | SHA-256 of the node's certificate (`sha256:<hex>`) |
//! | `TRACON_SANDBOX_TOKEN` | this run's one-time token, or |
//! | `TRACON_SANDBOX_TOKEN_FILE` | a file holding it |
//! | `TRACON_SANDBOX_SERVER_NAME` | SNI to send (default `tracon-node`) |
//! | `TRACON_SANDBOX_KEEPALIVE_MS` | HTTP/2 ping interval (default 20000) |
//! | `TRACON_SANDBOX_CONNECT_TIMEOUT_MS` | how long to keep dialling, or to wait for the node (default 30000) |
//! | `TRACON_SANDBOX_LOG` | log filter (default `info`), to stderr |
//!
//! As PID 1 it is the namespace's init: it reaps every orphan and turns
//! SIGTERM, SIGINT and SIGQUIT into an orderly teardown. Under another init
//! it makes itself the subreaper of everything it starts and behaves the
//! same way.

use std::process::ExitCode;
use std::time::Duration;

use tracon_sandbox::carrier::Dial;
use tracon_sandbox::inside::{self, Config, Ending};
use tracon_sandbox::tls::CertPin;

const USAGE_ERROR: u8 = 64;
const CONNECTION_LOST: u8 = 3;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn millis(name: &str, default: u64) -> Result<Duration, String> {
    match var(name) {
        None => Ok(Duration::from_millis(default)),
        Some(text) => text
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .ok_or_else(|| format!("{name} must be a positive number of milliseconds")),
    }
}

fn config_from_env() -> Result<Config, String> {
    let dial = match (
        var("TRACON_SANDBOX_ADDR"),
        var("TRACON_SANDBOX_SOCKET"),
        var("TRACON_SANDBOX_LISTEN"),
    ) {
        (Some(addr), None, None) => Dial::Tcp(addr),
        (None, Some(path), None) => Dial::Unix(path.into()),
        (None, None, Some(path)) => Dial::Listen(path.into()),
        _ => {
            return Err(
                "set exactly one of TRACON_SANDBOX_ADDR, TRACON_SANDBOX_SOCKET and TRACON_SANDBOX_LISTEN"
                    .into(),
            )
        }
    };
    let pin = CertPin::parse(&var("TRACON_SANDBOX_PIN").ok_or("TRACON_SANDBOX_PIN is not set")?)
        .map_err(|e| format!("TRACON_SANDBOX_PIN: {e}"))?;
    let token = match (
        var("TRACON_SANDBOX_TOKEN"),
        var("TRACON_SANDBOX_TOKEN_FILE"),
    ) {
        (Some(token), None) => token,
        (None, Some(path)) => {
            let token = std::fs::read_to_string(&path)
                .map_err(|e| format!("TRACON_SANDBOX_TOKEN_FILE: {e}"))?;
            // Spent once read; a read-only mount keeps it, which is harmless
            // once the node has redeemed it.
            let _ = std::fs::remove_file(&path);
            token.trim().to_string()
        }
        _ => {
            return Err(
                "set exactly one of TRACON_SANDBOX_TOKEN and TRACON_SANDBOX_TOKEN_FILE".into(),
            )
        }
    };
    let mut config = Config::new(dial, pin, token);
    if let Some(name) = var("TRACON_SANDBOX_SERVER_NAME") {
        config.server_name = name;
    }
    config.keepalive = millis("TRACON_SANDBOX_KEEPALIVE_MS", 20_000)?;
    config.connect_timeout = millis("TRACON_SANDBOX_CONNECT_TIMEOUT_MS", 30_000)?;
    Ok(config)
}

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("tracon-sandbox {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let filter = var("TRACON_SANDBOX_LOG").unwrap_or_else(|| "info".into());
    let config = config_from_env();
    // Before the runtime starts any thread: nothing started from here on
    // may inherit the token.
    inside::spawner::scrub_environment();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let config = match config {
        Ok(config) => config,
        Err(error) => {
            eprintln!("tracon-sandbox: {error}");
            return ExitCode::from(USAGE_ERROR);
        }
    };
    if !inside::is_init() {
        inside::become_subreaper();
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("tracon-sandbox: start runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let ending = runtime.block_on(inside::run(config));
    // Whatever is left (a stuck blocking task) must not hold the exit.
    runtime.shutdown_timeout(Duration::from_secs(1));
    match ending {
        Ok(Ending::Shutdown) | Ok(Ending::Signal(_)) => ExitCode::SUCCESS,
        Ok(Ending::Lost) => ExitCode::from(CONNECTION_LOST),
        Err(error) => {
            eprintln!("tracon-sandbox: {error}");
            ExitCode::FAILURE
        }
    }
}
