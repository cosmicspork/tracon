//! Shared by the hub's integration tests.

#![allow(dead_code)]

/// Seconds since the epoch, for signature timestamps the hub checks against
/// its replay window.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Milliseconds, for HLC stamps and envelope send times.
pub fn now_ms() -> i64 {
    now() as i64 * 1000
}
