//! The wire contract between the node and the harnesses that speak JSON-RPC
//! over stdio: the shared vocabulary (`types`), its line framing (`codec`)
//! and a peer for either end (`rpc`).

pub mod codec;
pub mod rpc;
pub mod types;
