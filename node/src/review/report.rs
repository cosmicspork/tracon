//! Standalone narrative report invariants.
//!
//! Unlike a code review, a report has no repository, diff, candidate, or
//! publish target. Its content hash only protects the narrative version an
//! operator reads from being acknowledged after a resubmission.

use sha2::{Digest, Sha256};

pub const MAX_TITLE_BYTES: usize = 256;
pub const MAX_BODY_BYTES: usize = 32 * 1024;

pub fn validate(title: &str, body: &str) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err("title is required".into());
    }
    if body.trim().is_empty() {
        return Err("body is required".into());
    }
    if title.len() > MAX_TITLE_BYTES {
        return Err(format!("title exceeds {MAX_TITLE_BYTES} bytes"));
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(format!("body exceeds {MAX_BODY_BYTES} bytes"));
    }
    Ok(())
}

/// The hash is intentionally over length-delimited fields, so two distinct
/// title/body pairs never alias through a separator in user-controlled text.
pub fn content_hash(title: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    for field in [title.as_bytes(), body.as_bytes()] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    hex::encode(hasher.finalize())
}
