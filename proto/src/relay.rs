//! The hub's sandbox relay: how a node behind NAT is reached by a sandbox that
//! can only dial a public address. The sandbox opens TLS naming the node in
//! SNI, either straight to the hub's relay listener or inside a WebSocket to
//! [`CONNECT_PATH`] on the hub's own URL; the hub reads nothing past that name
//! and hands the raw bytes to the node over a connection the node dialled out.
//! TLS is end to end, pinned to the node's certificate; the hub never holds a
//! key for it.
//!
//! On [`CONNECT_PATH`] the sandbox's bytes travel as binary messages of at
//! most [`MAX_MESSAGE`] bytes each, with the same end-of-stream convention as
//! the dial-back below.
//!
//! The node holds [`LISTEN_PATH`] open (signed, SSE) while it opts in. Each
//! connection the hub accepts for it arrives there as an [`OPEN_EVENT`] whose
//! data is `{"id": "<hex>"}`; the node answers by dialling
//! [`stream_path`]`(id)` as a WebSocket (signed) and the two byte streams are
//! spliced. Binary messages carry the bytes in order; an empty binary message
//! is end of stream in that direction (TCP half-close), and a close frame
//! ends both.

/// Held open by a node that serves the relay.
pub const LISTEN_PATH: &str = "/v0/relay/listen";

/// The WebSocket a sandbox opens, unauthenticated, to reach a node through
/// the hub's own HTTPS URL: for a hub behind a proxy that terminates TLS and
/// passes only HTTP, where the relay listener cannot be published.
pub const CONNECT_PATH: &str = "/v0/relay/connect";

/// The largest binary message a sandbox sends on [`CONNECT_PATH`].
pub const MAX_MESSAGE: usize = 64 * 1024;

/// The SSE event announcing one relayed connection to dial back for.
pub const OPEN_EVENT: &str = "open";

/// The SSE event sent first on [`LISTEN_PATH`]: `{"host": "<sni>"}`, the
/// name a sandbox must put in SNI to reach this node.
pub const READY_EVENT: &str = "ready";

/// The WebSocket a node dials to take one relayed connection.
pub fn stream_path(id: &str) -> String {
    format!("/v0/relay/streams/{id}")
}

/// Hex characters of the node id that name it in SNI. A full node id is 64
/// characters, one more than a DNS label may hold; the first 128 bits keep a
/// label no member can take from another without a preimage.
pub const LABEL_HEX: usize = 32;

/// The DNS label a node is reached under: the first [`LABEL_HEX`] hex
/// characters of its id. Derived, never chosen, so one node cannot register
/// another's name.
pub fn sandbox_label(node_id: &str) -> Option<String> {
    let id = node_id.to_ascii_lowercase();
    (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id[..LABEL_HEX].into())
}

/// `<label>.<domain>`: what a sandbox dials and names in SNI.
pub fn sandbox_host(node_id: &str, domain: &str) -> Option<String> {
    let domain = domain.trim_matches('.').to_ascii_lowercase();
    sandbox_label(node_id).map(|l| format!("{l}.{domain}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_is_a_dns_label_derived_from_the_id() {
        let id = "AB".repeat(32);
        let label = sandbox_label(&id).unwrap();
        assert_eq!(label, "ab".repeat(16));
        assert!(label.len() <= 63);
        assert_eq!(
            sandbox_host(&id, "Relay.Example.com.").unwrap(),
            format!("{label}.relay.example.com")
        );
        assert!(sandbox_label("abc").is_none());
        assert!(sandbox_label(&"zz".repeat(32)).is_none());
    }
}
