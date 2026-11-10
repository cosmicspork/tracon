//! `tracon-sandbox` and the node's side of its protocol.
//!
//! A sandbox — a container, a pod, a provider's microVM — runs one binary,
//! `tracon-sandbox`, which dials its node once and carries everything the node
//! needs over that single connection: spawning processes and their stdio,
//! moving trees in and out as tar, forwarding listeners inside the sandbox to
//! endpoints the node names, and supervising the session's services.
//!
//! # The connection
//!
//! ```text
//! sandbox (TLS client)                         node (TLS server)
//!   dial TCP or a unix socket  ───────────────▶  accept
//!   TLS 1.3, ALPN tracon-sandbox/1, the node's certificate pinned by SHA-256
//!   hello {token, version, pid, resumable, ...} ▶  token checked, consumed
//!                                   ◀────────── ack {ok, resume: {secret, within}}
//!   HTTP/2 server                ◀═══════════▶  HTTP/2 client
//!
//!   ...the connection drops; everything inside keeps running...
//!   dial again                 ───────────────▶  accept
//!   hello {token: secret, ...}       ────────▶  the run's secret, kept
//!                                   ◀────────── ack {ok}
//!   HTTP/2 again; the node carries each stream on from where it stopped
//! ```
//!
//! The sandbox dials out because that is the one direction every provider
//! allows: a Podman container on `--network none` reaches a unix socket
//! mounted into it, a pod reaches the node pod's port, and a provider VM
//! reaches the node's public (or relayed) address on 443. Nothing ever needs
//! to reach into the sandbox.
//!
//! Once dialled, the roles turn round: the node is the HTTP/2 *client*, so it
//! is the side that opens streams, and every channel is a request it makes.
//! A sandbox-to-node flow — a connection accepted by a tunnel's listener, a
//! service's log line — is the response to a request the node left open
//! (`tunnel/accept`, `events`), never a stream the sandbox starts.
//!
//! # Why HTTP/2 and not yamux
//!
//! Both multiplex streams with per-stream flow control over one connection.
//! HTTP/2 was chosen because:
//!
//! - it has connection-level keepalive built in (PING frames, answered by the
//!   peer's stack whatever its application is doing), which the long idle
//!   holds through a provider's proxy need, and yamux's ping is optional and
//!   unevenly implemented;
//! - flow control is per stream *and* per connection, with windows the node
//!   sets, so a process that floods stdout cannot starve a tunnel;
//! - a request's method, path and status give every channel open, refusal and
//!   error semantics without inventing a framing for them, and a reset stream
//!   is an unambiguous "this transfer failed" (an export that breaks halfway
//!   can never look like a complete, shorter tree);
//! - the `h2` crate is the one hyper uses, so it is the best-exercised
//!   implementation in the dependency graph, where yamux would be new.
//!
//! # Authority
//!
//! None lives inside the sandbox. The agent there may replace or tamper with
//! this binary, so the node treats everything it receives as hostile: the
//! token grants one run's channels once, every tar that comes back is
//! unpacked by [`files::unpack`] with its path, link and size rules, and every
//! JSON body is size-bounded. The sandbox side validates too, but only to
//! fail early; nothing relies on it.
//!
//! # Modules
//!
//! - [`protocol`]: the wire types both sides share.
//! - [`tls`]: the node's identity, the pin, and both TLS configurations.
//! - [`carrier`]: TCP, a unix socket, or a WebSocket to the hub's relay
//!   through an HTTP proxy, and the SELinux socket label a unix listener
//!   needs to be reachable from a confined container.
//! - [`files`]: tar packing (with the checkout exclusions) and validated
//!   unpacking.
//! - [`node`]: the node's API — [`node::SandboxServer`] and
//!   [`node::SandboxConn`].
//! - [`inside`]: what runs in the sandbox: the client, the process table, the
//!   reaper, tunnels and the service supervisor.

pub mod carrier;
pub mod files;
pub mod inside;
mod io;
pub mod node;
pub mod protocol;
pub mod tls;

pub use io::{H2Reader, H2Writer};
