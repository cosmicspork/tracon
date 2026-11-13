//! Everything the integration tests share. Included from each test binary as
//! `#[path = "support/mod.rs"] mod support;` — a file-per-binary layout,
//! since `tests/*.rs` are separate crates and cannot share a real module.

#![allow(dead_code)]

pub mod browser;
pub mod do_api;
pub mod events;
pub mod fake;
pub mod harness;
pub mod http;
pub mod kube_api;
pub mod mesh;
pub mod oauth_fake;
pub mod podman;
pub mod rows;
pub mod soft_passkey;
pub mod state;

/// The node's side for a loopback sandbox, with a short resume window: a
/// loopback sandbox is a thread, which a cut connection cannot kill, so it
/// ends only once its redials run out; with the node's default that is four
/// minutes. One per test binary, as `node_server` is one per node process.
pub fn sandbox_server() -> std::sync::Arc<tracon_sandbox::node::SandboxServer> {
    use std::sync::{Arc, OnceLock};
    use tracon_sandbox::node::{SandboxServer, ServerOptions};
    static SERVER: OnceLock<Arc<SandboxServer>> = OnceLock::new();
    SERVER
        .get_or_init(|| {
            let identity = tracon_sandbox::tls::NodeIdentity::generate().unwrap();
            let options = ServerOptions {
                resume_within: std::time::Duration::from_secs(1),
                ..ServerOptions::default()
            };
            Arc::new(SandboxServer::new(&identity, options).unwrap())
        })
        .clone()
}
