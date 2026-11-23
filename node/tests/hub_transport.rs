//! A node's requests to the hub over a connection the far side drops: one
//! dropped connection is retried rather than taken for an outage, and an
//! outage says why.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use proto::keys::Identity;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracon::config::Config;
use tracon::mesh::client::{HubError, MeshClient};
use tracon::store::Store;
use tracon::stream::Bus;

/// A stand-in hub that closes the first `drops` connections as soon as a
/// request arrives, without answering, and answers every later one with an
/// empty member list. Returns its URL and how many connections it took.
async fn flaky_hub(drops: usize) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let n = count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                if n < drops {
                    return; // dropped: closed with no answer
                }
                let body = "[]";
                let res = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(res.as_bytes()).await;
            });
        }
    });
    (url, accepted)
}

fn client(hub: &str) -> Arc<MeshClient> {
    let mut cfg = Config::default();
    cfg.mesh.hub_url = Some(hub.to_string());
    MeshClient::new(
        Identity::from_seed(&[7u8; 32]),
        hub,
        Arc::new(Store::open_in_memory().unwrap()),
        Bus::new(),
        Arc::new(cfg),
        Default::default(),
    )
}

#[tokio::test]
async fn a_dropped_connection_is_retried_once_before_the_hub_counts_as_down() {
    state::isolate();
    let (url, accepted) = flaky_hub(1).await;
    let c = client(&url);
    assert_eq!(c.refresh_members().await.unwrap(), 0);
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_hub_that_keeps_dropping_is_down_and_the_error_says_why() {
    state::isolate();
    let (url, accepted) = flaky_hub(usize::MAX).await;
    let c = client(&url);
    let err = c.refresh_members().await.unwrap_err();
    // Tried twice, no more.
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
    let HubError::Transport(msg) = err else {
        panic!("expected a transport error, got {err:?}");
    };
    // reqwest's own message alone names only the URL; the cause follows it.
    assert!(msg.starts_with("error sending request for url"), "{msg}");
    assert!(msg.to_lowercase().contains("closed"), "{msg}");
}
