//! The hub's sandbox relay, end to end: a scratch hub with a relay listener,
//! a node with no listener anyone else can reach that serves the relay over
//! a connection it dialled out, and a TLS client that knows only the hub's
//! address and the node's pinned certificate.

#[path = "support/mod.rs"]
mod support;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hub::sandbox_relay::{RelayConfig, SandboxRelay as HubRelay};
use hub::store::{Member, MemberStore, MemoryFrames, MemoryMembers};
use proto::frame::MESH_CHANNEL;
use proto::keys::Identity;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use support::mesh::{identity, WAIT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracon::config::SandboxRelay as NodeRelayConfig;
use tracon::mesh::sandbox_relay::SandboxRelay;

const DOMAIN: &str = "relay.test";

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A self-signed P-256 certificate and its PKCS#8 key: the node's.
fn self_signed(cn: &str) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    use openssl::{
        asn1::Asn1Time, bn::BigNum, ec, hash::MessageDigest, nid::Nid, pkey::PKey, x509,
    };
    let group = ec::EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let key = PKey::from_ec_key(ec::EcKey::generate(&group).unwrap()).unwrap();
    let mut name = x509::X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", cn).unwrap();
    let name = name.build();
    let mut b = x509::X509::builder().unwrap();
    b.set_version(2).unwrap();
    b.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    b.set_subject_name(&name).unwrap();
    b.set_issuer_name(&name).unwrap();
    b.set_pubkey(&key).unwrap();
    b.set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    b.set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    b.sign(&key, MessageDigest::sha256()).unwrap();
    (
        CertificateDer::from(b.build().to_der().unwrap()),
        PrivateKeyDer::Pkcs8(key.private_key_to_pkcs8().unwrap().into()),
    )
}

/// Accepts exactly one certificate, byte for byte, whatever name it is for:
/// how a sandbox pins its node.
#[derive(Debug)]
struct Pinned(CertificateDer<'static>, Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.0.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "certificate is not the pinned one".into(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.1.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.1.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.1.signature_verification_algorithms.supported_schemes()
    }
}

fn client_config(pin: &CertificateDer<'static>) -> Arc<rustls::ClientConfig> {
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned(pin.clone(), provider())))
            .with_no_client_auth(),
    )
}

/// The node's sandbox listener: TLS under the node's certificate, answering
/// each line with `node got: <line>`. Loopback only; the client never dials it.
async fn sandbox_listener(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> SocketAddr {
    let cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = vec![0u8; 1 << 20];
                let mut line = Vec::new();
                loop {
                    let n = match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    for &b in &buf[..n] {
                        line.push(b);
                        if b == b'\n' {
                            let mut reply = b"node got: ".to_vec();
                            reply.extend_from_slice(&line);
                            if tls.write_all(&reply).await.is_err() {
                                return;
                            }
                            line.clear();
                        }
                    }
                }
            });
        }
    });
    addr
}

/// A TCP forwarder that keeps a copy of every byte in each direction.
#[derive(Clone, Default)]
struct Tap {
    up: Arc<Mutex<Vec<u8>>>,
    down: Arc<Mutex<Vec<u8>>>,
}

async fn tap(to: SocketAddr) -> (SocketAddr, Tap) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let t = Tap::default();
    let seen = t.clone();
    tokio::spawn(async move {
        loop {
            let (a, _) = listener.accept().await.unwrap();
            let b = TcpStream::connect(to).await.unwrap();
            let (mut ar, mut aw) = a.into_split();
            let (mut br, mut bw) = b.into_split();
            let (up, down) = (seen.up.clone(), seen.down.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                while let Ok(n) = ar.read(&mut buf).await {
                    if n == 0 {
                        let _ = bw.shutdown().await;
                        break;
                    }
                    up.lock().unwrap().extend_from_slice(&buf[..n]);
                    if bw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                while let Ok(n) = br.read(&mut buf).await {
                    if n == 0 {
                        let _ = aw.shutdown().await;
                        break;
                    }
                    down.lock().unwrap().extend_from_slice(&buf[..n]);
                    if aw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (addr, t)
}

struct Hub {
    url: String,
    relay_addr: SocketAddr,
    members: Arc<MemoryMembers>,
    relay: Arc<HubRelay>,
}

async fn start_hub(admitted: &[&Identity], max_streams_per_node: usize) -> Hub {
    let members = Arc::new(MemoryMembers::new());
    for id in admitted {
        members
            .put(&Member {
                node_id: id.node_id(),
                x25519_pub: id.x25519_hex(),
                binding_sig: proto::enroll::sign_binding(id),
                name: "n".into(),
                channels: vec![MESH_CHANNEL.into()],
                admitted_ms: 0,
                admitted_by: "test".into(),
                role: Default::default(),
            })
            .unwrap();
    }
    let mut cfg = RelayConfig::new(DOMAIN);
    cfg.max_streams_per_node = max_streams_per_node;
    cfg.dial_back_timeout = Duration::from_secs(3);
    let relay = HubRelay::new(cfg, members.clone());
    let mut state = hub::state_for(
        Arc::new(MemoryFrames::new()),
        members.clone(),
        hub::HubConfig::default(),
        Arc::new(hub::pokes::PokeHub::new()),
        None,
    );
    state.relay = Some(relay.clone());
    let app = hub::app_with_state(state);
    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", api.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(api, app).await.unwrap() });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = listener.local_addr().unwrap();
    tokio::spawn(relay.clone().serve(listener));
    Hub {
        url,
        relay_addr,
        members,
        relay,
    }
}

/// Start the node's relay bridge to `target` and wait until the hub has it.
async fn serve_relay(id: Identity, hub: &Hub, target: SocketAddr) -> String {
    let relay = SandboxRelay::new(
        Arc::new(id),
        &hub.url,
        &NodeRelayConfig {
            enabled: true,
            target: target.to_string(),
            max_streams: 16,
        },
    );
    relay.spawn();
    let mut host = relay.host();
    let served = tokio::time::timeout(WAIT, host.wait_for(|h| h.is_some()))
        .await
        .expect("the node registered with the hub's relay")
        .unwrap()
        .clone()
        .unwrap();
    served
}

async fn tls_to(
    addr: SocketAddr,
    host: &str,
    pin: &CertificateDer<'static>,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    tokio_rustls::TlsConnector::from(client_config(pin))
        .connect(ServerName::try_from(host.to_string()).unwrap(), tcp)
        .await
}

async fn round_trip(tls: &mut tokio_rustls::client::TlsStream<TcpStream>, line: &str) -> String {
    tls.write_all(line.as_bytes()).await.unwrap();
    let want = format!("node got: {line}");
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while got.len() < want.len() {
        let n = tokio::time::timeout(WAIT, tls.read(&mut buf))
            .await
            .expect("reply in time")
            .unwrap();
        assert!(n > 0, "connection closed before the reply");
        got.extend_from_slice(&buf[..n]);
    }
    String::from_utf8(got).unwrap()
}

/// What the relay says to a connection it will not route: nothing at all.
async fn answered_with(addr: SocketAddr, first: &[u8]) -> Vec<u8> {
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(first).await.unwrap();
    let mut got = Vec::new();
    tokio::time::timeout(WAIT, tcp.read_to_end(&mut got))
        .await
        .expect("the relay closed the connection")
        .ok();
    got
}

/// The first flight a TLS client sends for `host`.
fn client_hello(host: &str, pin: &CertificateDer<'static>) -> Vec<u8> {
    let mut conn = rustls::ClientConnection::new(
        client_config(pin),
        ServerName::try_from(host.to_string()).unwrap(),
    )
    .unwrap();
    let mut out = Vec::new();
    conn.write_tls(&mut out).unwrap();
    out
}

#[tokio::test]
async fn a_node_behind_nat_is_reached_through_the_hub_with_its_own_certificate() {
    let node = identity(41);
    let node_id = node.node_id();
    let hub = start_hub(&[&node], 16).await;
    let (cert, key) = self_signed("tracon-node");
    let target = sandbox_listener(cert.clone(), key).await;
    let host = serve_relay(node, &hub, target).await;
    assert_eq!(
        host,
        proto::relay::sandbox_host(&node_id, DOMAIN).unwrap(),
        "the name is derived from the node id"
    );

    // The client knows the hub's address and the node's certificate.
    let mut tls = tls_to(hub.relay_addr, &host, &cert).await.unwrap();
    let peer = tls.get_ref().1.peer_certificates().unwrap()[0].clone();
    assert_eq!(peer, cert, "the client sees the node's certificate");
    assert_eq!(round_trip(&mut tls, "hello\n").await, "node got: hello\n");
    // A large payload crosses in several WebSocket messages and comes back.
    let big = format!("{}\n", "x".repeat(300_000));
    assert_eq!(round_trip(&mut tls, &big).await, format!("node got: {big}"));

    // A second client while the first is open: one stream each.
    let mut other = tls_to(hub.relay_addr, &host, &cert).await.unwrap();
    assert_eq!(round_trip(&mut other, "two\n").await, "node got: two\n");
    assert_eq!(round_trip(&mut tls, "still\n").await, "node got: still\n");

    // A client pinning some other certificate refuses the node's.
    let (wrong, _) = self_signed("someone-else");
    assert!(tls_to(hub.relay_addr, &host, &wrong).await.is_err());
}

#[tokio::test]
async fn the_hub_copies_ciphertext_it_cannot_read() {
    let node = identity(42);
    let hub = start_hub(&[&node], 16).await;
    let (cert, key) = self_signed("tracon-node");
    let sandbox = sandbox_listener(cert.clone(), key).await;
    // One tap where the hub's bytes come in from the client, one where they
    // leave the node's bridge for the sandbox listener.
    let (at_hub, client_side) = tap(hub.relay_addr).await;
    let (at_node, node_side) = tap(sandbox).await;
    let host = serve_relay(node, &hub, at_node).await;

    let secret = "the-plaintext-marker-only-endpoints-know";
    let mut tls = tls_to(at_hub, &host, &cert).await.unwrap();
    assert_eq!(
        round_trip(&mut tls, &format!("{secret}\n")).await,
        format!("node got: {secret}\n")
    );
    tls.shutdown().await.unwrap();
    drop(tls);

    let settled = || {
        *client_side.up.lock().unwrap() == *node_side.up.lock().unwrap()
            && *client_side.down.lock().unwrap() == *node_side.down.lock().unwrap()
    };
    support::mesh::wait_for("both taps to see the same bytes", settled).await;
    let up = client_side.up.lock().unwrap().clone();
    let down = client_side.down.lock().unwrap().clone();
    assert!(!up.is_empty() && !down.is_empty());
    // Every byte the hub received it delivered unchanged, in both directions,
    // ClientHello included, so it neither terminated nor rewrote the TLS.
    assert_eq!(up, *node_side.up.lock().unwrap());
    assert_eq!(down, *node_side.down.lock().unwrap());
    // And what passed through it is ciphertext: the plaintext is nowhere in it.
    for (what, bytes) in [("client to node", &up), ("node to client", &down)] {
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "plaintext visible {what}"
        );
    }
}

#[tokio::test]
async fn what_the_relay_will_not_route_is_closed_unanswered() {
    let node = identity(43);
    let stranger = identity(44);
    let hub = start_hub(&[&node], 1).await;
    let (cert, key) = self_signed("tracon-node");
    let target = sandbox_listener(cert.clone(), key).await;
    let host = serve_relay(node, &hub, target).await;

    // A name nobody serves, another domain, a node not registered, not TLS.
    for name in [
        format!("{}.{DOMAIN}", "0".repeat(32)),
        host.replace(DOMAIN, "elsewhere.test"),
        proto::relay::sandbox_host(&stranger.node_id(), DOMAIN).unwrap(),
    ] {
        let got = answered_with(hub.relay_addr, &client_hello(&name, &cert)).await;
        assert!(got.is_empty(), "{name} was answered");
    }
    assert!(
        answered_with(hub.relay_addr, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .is_empty()
    );

    // The stream limit (one here): a second connection while one is open
    // gets nothing, and one after it closes is served again.
    let mut first = tls_to(hub.relay_addr, &host, &cert).await.unwrap();
    assert_eq!(round_trip(&mut first, "one\n").await, "node got: one\n");
    assert!(answered_with(hub.relay_addr, &client_hello(&host, &cert))
        .await
        .is_empty());
    first.shutdown().await.unwrap();
    drop(first);
    let mut again = None;
    for _ in 0..50 {
        if let Ok(mut tls) = tls_to(hub.relay_addr, &host, &cert).await {
            if tls.write_all(b"again\n").await.is_ok() {
                again = Some(tls);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut again = again.expect("served again once the first stream closed");
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(WAIT, again.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"node got: again\n");
    drop(again);

    // A node that is no longer a member is not a target any more.
    hub.members.remove(&identity(43).node_id()).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(answered_with(hub.relay_addr, &client_hello(&host, &cert))
        .await
        .is_empty());
    assert_eq!(hub.relay.registered(), 0);
}

#[tokio::test]
async fn only_a_member_node_may_listen() {
    let node = identity(45);
    let stranger = identity(46);
    let hub = start_hub(&[&node], 16).await;
    let client = reqwest::Client::new();
    let get = |id: &Identity, path: &str| {
        let mut req = client.get(format!("{}{}", hub.url, path));
        for (k, v) in proto::auth::signed_headers(id, "GET", path, b"", now_secs()) {
            req = req.header(k, v);
        }
        req.send()
    };
    let res = get(&stranger, proto::relay::LISTEN_PATH).await.unwrap();
    assert_eq!(res.status(), 403);
    let res = client
        .get(format!("{}{}", hub.url, proto::relay::LISTEN_PATH))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    // A dial-back for a connection nobody was offered.
    let res = get(&node, &proto::relay::stream_path(&"ab".repeat(16)))
        .await
        .unwrap();
    assert_ne!(res.status(), 101);
}
