//! Generated display names for the text the node writes: approval titles,
//! errors, readiness, orientation and CLI listings. The derivation and the
//! word lists are `proto::names`, shared with the interface, so a name the
//! node writes is the one the operator sees on the page.

pub use proto::names::{generated, kind};

/// Whether `id` is a whole id as the node mints them: a UUID or 64 hex
/// characters. Anything shorter is a prefix, which names nothing.
pub fn is_full_id(id: &str) -> bool {
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_hexdigit());
    match id.len() {
        64 => hex(id),
        36 => {
            let parts: Vec<&str> = id.split('-').collect();
            parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12]) && parts.iter().all(|p| hex(p))
        }
        _ => false,
    }
}

/// A record someone named by id, read back: its generated name for a whole
/// id, or the text as given (cut to 12 characters) for a prefix or anything
/// else, which names nothing.
pub fn handle(kind: &str, given: &str) -> String {
    let given = given.trim();
    if is_full_id(given) {
        generated(kind, given)
    } else {
        given.chars().take(12).collect()
    }
}

pub fn session(id: &str) -> String {
    generated(kind::SESSION, id)
}

pub fn task(id: &str) -> String {
    generated(kind::TASK, id)
}

pub fn manifest(digest: &str) -> String {
    generated(kind::MANIFEST, digest)
}

/// A node by its own name when there is one, else its generated `node-…` name.
/// An id that is not a node id (64 hex characters) is already something to
/// read, and reads as given.
pub fn node(id: &str, name: Option<&str>) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_string(),
        None if id.len() == 64 && is_full_id(id) => generated(kind::NODE, id),
        None => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_id_is_named_and_a_prefix_is_read_as_given() {
        let uuid = "019a9e5c-7f20-7a3b-8c1d-2e3f4a5b6c7d";
        assert!(is_full_id(uuid));
        assert_eq!(handle(kind::SESSION, uuid), "session-cubic-briar-06");
        assert!(!is_full_id("019a9e5c"));
        assert_eq!(handle(kind::TASK, " 0b0e29c4 "), "0b0e29c4");
        assert!(is_full_id(&"ab".repeat(32)));
        assert!(!is_full_id(&"zz".repeat(32)));
        assert_eq!(node("x", Some("bazzite")), "bazzite");
        assert!(node(&"ab".repeat(32), Some(" ")).starts_with("node-"));
        assert_eq!(node("peer-b", None), "peer-b");
    }
}
