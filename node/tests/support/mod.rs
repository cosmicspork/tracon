//! Everything the integration tests share. Included from each test binary as
//! `#[path = "support/mod.rs"] mod support;` — a file-per-binary layout,
//! since `tests/*.rs` are separate crates and cannot share a real module.

#![allow(dead_code)]

pub mod events;
pub mod fake;
pub mod harness;
pub mod http;
pub mod mesh;
pub mod rows;
pub mod state;
