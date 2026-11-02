//! Credentialed upstreams (`config::upstreams`): a client asks the granted
//! proxy for `http://<declared host>/…`, and the node opens its own TLS to
//! that host and writes the credential in. The client never holds it, no
//! other host is ever sent it, and a redirect does not carry it.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use tracon::broker::Attachment;
use tracon::gateway::proxy::{serve_granted, GrantSpec, Granted, Grants, Resolve};

/// What the operator's broker holds, as the header the node writes. Never
/// given to the client below.
const CREDENTIAL: &str = "Basic b3BzQGV4YW1wbGUuY29tOm5vdmEtbGljZW5jZS1rZXk=";
/// What a client that was told to send something sends: a placeholder.
const PLACEHOLDER: &str = "Basic cGxhY2Vob2xkZXI6cGxhY2Vob2xkZXI=";

/// A test root, and a certificate for `nova.example` it signed, valid until
/// 2126. Made with openssl for these tests alone.
const ROOT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBrzCCAVWgAwIBAgIULRhHJF0XiKlCe5d/uGgNsf6z/9owCgYIKoZIzj0EAwIw
JDEiMCAGA1UEAwwZdHJhY29uIHRlc3QgdXBzdHJlYW0gcm9vdDAgFw0yNjExMDIw
NTIzNDRaGA8yMTI2MTAwOTA1MjM0NFowJDEiMCAGA1UEAwwZdHJhY29uIHRlc3Qg
dXBzdHJlYW0gcm9vdDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABJ8izLE17gsr
LFs/7Ja9MYU9LHJ7kzRjYiO/a6yqIMSA8BUxPwXxPZJuIvguMPKJb280HhRizpbl
Qqh+ORHT+WqjYzBhMB0GA1UdDgQWBBQmIcNyJFWQCqBKhBWgF0YVeFpaSjAfBgNV
HSMEGDAWgBQmIcNyJFWQCqBKhBWgF0YVeFpaSjAPBgNVHRMBAf8EBTADAQH/MA4G
A1UdDwEB/wQEAwIBBjAKBggqhkjOPQQDAgNIADBFAiBbPp+lQax7X6T1UoTpoe/S
u84tDB7SGZCCpIjv+Y7g3QIhAJ7DObRxJVKtDxz+n502HCxVOnMVlE+OUUEJB0HW
8g/P
-----END CERTIFICATE-----";
const LEAF_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIB0DCCAXWgAwIBAgIUXpSFZhdhI9kpfUWq2NnYyUCxFBgwCgYIKoZIzj0EAwIw
JDEiMCAGA1UEAwwZdHJhY29uIHRlc3QgdXBzdHJlYW0gcm9vdDAgFw0yNjExMDIw
NTIzNDRaGA8yMTI2MTAwOTA1MjM0NFowFzEVMBMGA1UEAwwMbm92YS5leGFtcGxl
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEwk2PJhgurejeltKBWGW4MZ7mxo7C
o+XR7pU+iO7irM6qEfJNLls6uVpkkvhOQfQ3glkyrw/fn57qPZG/EGeDraOBjzCB
jDAMBgNVHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEF
BQcDATAXBgNVHREEEDAOggxub3ZhLmV4YW1wbGUwHQYDVR0OBBYEFEHXsMuO1e3g
U7lwKvs70bDKvULqMB8GA1UdIwQYMBaAFCYhw3IkVZAKoEqEFaAXRhV4WlpKMAoG
CCqGSM49BAMCA0kAMEYCIQCJURLCABU2DYBH4NWIjh51zcDzBjrZKeUPGOTvSC0o
9AIhAILHBKIYkYanEtkAhhK9DEkyqiXvgFb7C0eQeX5B4LQM
-----END CERTIFICATE-----";
const LEAF_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgroGmkjCRHNQjQD6X
xkrA++gKBHYHpK4osF4yODV07GyhRANCAATCTY8mGC6t6N6W0oFYZbgxnubGjsKj
5dHulT6I7uKszqoR8k0uWzq5WmSS+E5B9DeCWTKvD9+fnuo9kb8QZ4Ot
-----END PRIVATE KEY-----";

fn der(pem: &str) -> Vec<u8> {
    use base64::Engine;
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .unwrap()
}

/// Every request head an upstream was sent, in order.
type Heard = Arc<Mutex<Vec<String>>>;

/// Answer one request on `stream` as a registry would: a file, a redirect
/// to another host, or a redirect back to itself.
async fn answer<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, heard: Heard) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let path = head.split(' ').nth(1).unwrap_or_default().to_string();
    heard.lock().unwrap().push(head);
    let reply = match path.as_str() {
        "/away" => "HTTP/1.1 302 Found\r\nLocation: https://cdn.example/file.zip\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        "/again" => "HTTP/1.1 302 Found\r\nLocation: https://nova.example/dist/x.zip?sig=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        _ => "HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nzip-bytes".to_string(),
    };
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// A plain-HTTP upstream on loopback.
async fn plain_upstream() -> (SocketAddr, Heard) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heard: Heard = Default::default();
    let kept = heard.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(answer(stream, kept.clone()));
        }
    });
    (addr, heard)
}

/// An upstream that speaks TLS as `nova.example`, under the test root.
async fn tls_upstream() -> (SocketAddr, Heard) {
    use tokio_rustls::rustls;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![der(LEAF_PEM).into()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(der(LEAF_KEY_PEM).into()),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heard: Heard = Default::default();
    let kept = heard.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let heard = kept.clone();
            tokio::spawn(async move {
                if let Ok(stream) = acceptor.accept(stream).await {
                    answer(stream, heard).await;
                }
            });
        }
    });
    (addr, heard)
}

/// A resolver that sends each name to the address a test gave it, whatever
/// port was asked: the proxy only ever dials 443, which a test cannot bind.
fn resolving(names: Vec<(&'static str, SocketAddr)>) -> Resolve {
    Arc::new(move |host, _port| {
        let found: Vec<SocketAddr> = names
            .iter()
            .filter(|(name, _)| *name == host)
            .map(|(_, addr)| *addr)
            .collect();
        Box::pin(async move { Ok(found) })
    })
}

async fn proxy(granted: Granted) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_granted(listener, granted));
    addr
}

fn proxy_credentials(token: &str) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("prepare:{token}"));
    format!("Proxy-Authorization: Basic {encoded}\r\n")
}

/// Everything the proxy sends back for one request.
async fn ask(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read_to_end(&mut answer),
    )
    .await;
    String::from_utf8_lossy(&answer).into_owned()
}

fn get(url: &str, host: &str, token: &str, extra: &str) -> String {
    format!(
        "GET {url} HTTP/1.1\r\nHost: {host}\r\n{}{extra}Connection: close\r\n\r\n",
        proxy_credentials(token)
    )
}

fn nova() -> Attachment {
    Attachment::for_test("nova.example", "authorization", CREDENTIAL)
}

/// The whole path: a plain request for the declared host is sent on over
/// the node's own TLS, verified as that host, with the credential in place
/// of whatever the client sent, and with nothing said to the proxy carried
/// on.
#[tokio::test]
async fn a_declared_host_is_sent_its_credential_over_the_node_s_own_tls() {
    state::isolate();
    let (nova_addr, heard) = tls_upstream().await;
    let grants = Grants::default();
    let token = grants
        .issue(GrantSpec {
            client: "prepare".into(),
            upstreams: vec![nova()],
            ..Default::default()
        })
        .unwrap();
    let addr = proxy(
        Granted::dialling_any_address(grants.clone())
            .resolving_with(resolving(vec![("nova.example", nova_addr)]))
            .trusting_upstream_root(der(ROOT_PEM)),
    )
    .await;

    let answer = ask(
        addr,
        &get(
            "http://nova.example/dist/x.zip?v=1",
            "nova.example",
            &token,
            &format!("Authorization: {PLACEHOLDER}\r\n"),
        ),
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with("zip-bytes"), "{answer}");
    assert!(!answer.contains(CREDENTIAL), "{answer}");

    let heard = heard.lock().unwrap().clone();
    assert_eq!(heard.len(), 1, "{heard:?}");
    let head = heard[0].to_ascii_lowercase();
    assert!(head.starts_with("get /dist/x.zip?v=1 http/1.1"), "{head}");
    assert!(head.contains("host: nova.example\r\n"), "{head}");
    assert!(
        heard[0].contains(&format!("authorization: {CREDENTIAL}")),
        "{head}"
    );
    assert!(!heard[0].contains(PLACEHOLDER), "{head}");
    assert!(!head.contains("proxy-authorization"), "{head}");
}

/// An upstream whose certificate the node cannot verify is not sent the
/// credential: the TLS fails before any request is written.
#[tokio::test]
async fn an_upstream_the_node_cannot_verify_is_sent_nothing() {
    state::isolate();
    let (nova_addr, heard) = tls_upstream().await;
    let grants = Grants::default();
    let token = grants
        .issue(GrantSpec {
            client: "prepare".into(),
            upstreams: vec![nova()],
            ..Default::default()
        })
        .unwrap();
    // The web's roots, as in production: none of them signed the test root.
    let addr = proxy(
        Granted::dialling_any_address(grants.clone())
            .resolving_with(resolving(vec![("nova.example", nova_addr)])),
    )
    .await;
    let answer = ask(
        addr,
        &get("http://nova.example/dist/x.zip", "nova.example", &token, ""),
    )
    .await;
    assert!(
        answer.starts_with("HTTP/1.1 502 upstream TLS failed"),
        "{answer}"
    );
    assert!(heard.lock().unwrap().is_empty());
}

/// A credential goes to its declared host and to nothing else: not to a host
/// the grant reaches in plain HTTP, even one that names the declared host in
/// its `Host` header; not to a lookalike; not to the declared host on another
/// port; never through a CONNECT tunnel, which the declaration does not open;
/// and not for a client whose grant does not attach it.
#[tokio::test]
async fn a_credential_is_never_attached_to_an_undeclared_host() {
    state::isolate();
    let (nova_addr, nova_heard) = plain_upstream().await;
    let (other_addr, other_heard) = plain_upstream().await;
    let grants = Grants::default();
    let attached = grants
        .issue(GrantSpec {
            client: "prepare".into(),
            hosts: vec!["other.example".into()],
            plain_http: true,
            upstreams: vec![nova()],
            ..Default::default()
        })
        .unwrap();
    let bare = grants
        .issue(GrantSpec {
            client: "prepare".into(),
            hosts: vec!["nova.example".into()],
            ..Default::default()
        })
        .unwrap();
    let addr = proxy(
        Granted::dialling_any_address(grants.clone())
            .resolving_with(resolving(vec![
                ("nova.example", nova_addr),
                ("other.example", other_addr),
                ("nova.example.other.example", other_addr),
            ]))
            .upstreams_over_plain_http(),
    )
    .await;

    // Another host the grant reaches, asked with the declared host's name in
    // `Host`: forwarded as itself, with nothing attached.
    let answer = ask(
        addr,
        &get("http://other.example/x", "nova.example", &attached, ""),
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    // A lookalike, another port, and a tunnel: refused.
    for request in [
        get(
            "http://nova.example.other.example/x",
            "nova.example.other.example",
            &attached,
            "",
        ),
        get("http://nova.example:8080/x", "nova.example:8080", &attached, ""),
        format!(
            "CONNECT nova.example:443 HTTP/1.1\r\nHost: nova.example:443\r\n{}Connection: close\r\n\r\n",
            proxy_credentials(&attached)
        ),
    ] {
        let answer = ask(addr, &request).await;
        assert!(answer.starts_with("HTTP/1.1 403"), "{request}: {answer}");
    }
    // A grant that names the host but attaches nothing gets no plain HTTP.
    let answer = ask(
        addr,
        &get("http://nova.example/x", "nova.example", &bare, ""),
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");

    assert!(nova_heard.lock().unwrap().is_empty());
    let other = other_heard.lock().unwrap().clone();
    assert_eq!(other.len(), 1, "{other:?}");
    assert!(!other[0].contains(CREDENTIAL), "{}", other[0]);
    assert!(
        !other[0].to_ascii_lowercase().contains("authorization"),
        "{}",
        other[0]
    );
}

/// The node follows no redirect. One to another host reaches the client as
/// it was sent, and following it through the proxy carries nothing; one back
/// to the declared host is pointed at the proxy again, so the next request
/// is credentialed by host and not tunnelled bare.
#[tokio::test]
async fn a_redirect_to_another_host_does_not_carry_it() {
    state::isolate();
    let (nova_addr, nova_heard) = plain_upstream().await;
    let (cdn_addr, cdn_heard) = plain_upstream().await;
    let grants = Grants::default();
    let token = grants
        .issue(GrantSpec {
            client: "prepare".into(),
            hosts: vec!["cdn.example".into()],
            plain_http: true,
            upstreams: vec![nova()],
            ..Default::default()
        })
        .unwrap();
    let addr = proxy(
        Granted::dialling_any_address(grants.clone())
            .resolving_with(resolving(vec![
                ("nova.example", nova_addr),
                ("cdn.example", cdn_addr),
            ]))
            .upstreams_over_plain_http(),
    )
    .await;

    let away = ask(
        addr,
        &get("http://nova.example/away", "nova.example", &token, ""),
    )
    .await;
    assert!(away.starts_with("HTTP/1.1 302"), "{away}");
    assert!(
        away.to_ascii_lowercase()
            .contains("location: https://cdn.example/file.zip\r\n"),
        "{away}"
    );
    assert!(
        cdn_heard.lock().unwrap().is_empty(),
        "the node followed a redirect"
    );

    let again = ask(
        addr,
        &get("http://nova.example/again", "nova.example", &token, ""),
    )
    .await;
    assert!(
        again
            .to_ascii_lowercase()
            .contains("location: http://nova.example/dist/x.zip?sig=1\r\n"),
        "{again}"
    );

    // The client follows the first redirect through the proxy.
    let followed = ask(
        addr,
        &get("http://cdn.example/file.zip", "cdn.example", &token, ""),
    )
    .await;
    assert!(followed.starts_with("HTTP/1.1 200"), "{followed}");
    let cdn = cdn_heard.lock().unwrap().clone();
    assert_eq!(cdn.len(), 1, "{cdn:?}");
    assert!(!cdn[0].contains(CREDENTIAL), "{}", cdn[0]);
    // Both requests to the declared host were credentialed.
    let nova = nova_heard.lock().unwrap().clone();
    assert_eq!(nova.len(), 2, "{nova:?}");
    assert!(nova.iter().all(|head| head.contains(CREDENTIAL)));
}

/// The declaration names a host, not an address: one that resolves to this
/// machine is refused as any granted host is, before anything is sent.
#[tokio::test]
async fn a_declared_host_that_resolves_to_this_machine_is_sent_nothing() {
    state::isolate();
    let (nova_addr, heard) = plain_upstream().await;
    let grants = Grants::default();
    let token = grants
        .issue(GrantSpec {
            client: "prepare".into(),
            upstreams: vec![nova()],
            ..Default::default()
        })
        .unwrap();
    let addr = proxy(
        Granted::new(grants.clone())
            .resolving_with(resolving(vec![("nova.example", nova_addr)]))
            .upstreams_over_plain_http(),
    )
    .await;
    let answer = ask(
        addr,
        &get("http://nova.example/dist/x.zip", "nova.example", &token, ""),
    )
    .await;
    assert!(
        answer.starts_with("HTTP/1.1 403 resolves to an address on this machine"),
        "{answer}"
    );
    assert!(heard.lock().unwrap().is_empty());
}
