//! The verifier around `gh`. Most tests use `fixtures/fake-gh`, which records
//! what it was asked; the `real_gh_*` tests run the pinned `gh` on a recorded
//! tracon release attestation and need `TRACON_TEST_GH` (CI's `test` job sets
//! it and runs them with `--ignored`).
//!
//! The recorded fixture is the v0.51.2 release's
//! `tracon-bundle-0.51.2-amd64.tar.gz.sha256`, which `release.yml` attests. To
//! regenerate it, should a trusted-root rotation ever stop it verifying:
//!
//! ```sh
//! cd attest/tests/fixtures
//! curl -fsSLO https://github.com/cosmicspork/tracon/releases/download/v0.51.2/tracon-bundle-0.51.2-amd64.tar.gz.sha256
//! curl -fsS https://api.github.com/repos/cosmicspork/tracon/attestations/sha256:$(sha256sum tracon-bundle-0.51.2-amd64.tar.gz.sha256 | cut -c1-64) \
//!   | tee lookup.json | jq -c '{attestations: [.attestations[] | {bundle}]}' > attestations.json
//! curl -fsS -o attestation-bundle.json.sn "$(jq -r '.attestations[0].bundle_url' lookup.json)"
//! rm lookup.json
//! gh attestation trusted-root > trusted_root.jsonl
//! ```
//!
//! `attestation-bundle.json.sn` is the same bundle as served at its
//! `bundle_url`: snappy-compressed, as a lookup that leaves the bundle out
//! names it.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};
use tracon_attest::{download_bundle, Error, Outcome, Policy, Verifier};

const RELEASE_POLICY: Policy<'static> = Policy {
    repo: "cosmicspork/tracon",
    signer_workflow: Some("cosmicspork/tracon/.github/workflows/release.yml"),
    source_ref: Some("refs/heads/main"),
    deny_self_hosted_runners: true,
};
const PLUGIN_POLICY: Policy<'static> = Policy {
    repo: "cosmicspork/sql-review",
    signer_workflow: None,
    source_ref: None,
    deny_self_hosted_runners: true,
};
const FIXTURE_ARTIFACT: &str = "tracon-bundle-0.51.2-amd64.tar.gz.sha256";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fake_gh() -> Verifier {
    Verifier::new(fixtures().join("fake-gh"), Duration::from_secs(10))
}

/// A fresh directory for one test, so `label` must be unique to it.
fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tracon-attest-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn artifact(dir: &Path, name: &str, contents: &[u8]) -> (PathBuf, [u8; 32]) {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    (path, Sha256::digest(contents).into())
}

fn recorded_argv(artifact: &Path) -> Vec<String> {
    let mut argv = artifact.as_os_str().to_owned();
    argv.push(".argv");
    std::fs::read_to_string(argv)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Answer one request with `head` (the status line and any headers, without
/// the blank line) and `body`, then close. Resolves to the request line.
async fn serve_once(head: String, body: Vec<u8>) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sent, request_line) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let read = socket.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..read]);
        let _ = sent.send(request.lines().next().unwrap_or_default().to_string());
        socket
            .write_all(format!("{head}\r\nConnection: close\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let _ = socket.write_all(&body).await;
    });
    (format!("http://{address}"), request_line)
}

type Route = (&'static str, &'static str, Vec<u8>);

/// Answer each request whose path starts with a route's prefix with that
/// route's content type and body, and any other with 404, until the test
/// ends. `routes` is given the server's base URL, so a body can name the
/// server itself. Returns the base URL, and the request lines as they come.
async fn serve_routes(
    routes: impl FnOnce(&str) -> Vec<Route>,
) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes = routes(&base);
    let (sent, requests) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = vec![0; 8192];
            let read = socket.read(&mut request).await.unwrap_or(0);
            let line = String::from_utf8_lossy(&request[..read])
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            let path = line.split(' ').nth(1).unwrap_or_default().to_string();
            let _ = sent.send(line);
            let (status, content_type, body) = routes
                .iter()
                .find(|(prefix, _, _)| path.starts_with(prefix))
                .map(|(_, content_type, body)| ("200 OK", *content_type, body.clone()))
                .unwrap_or(("404 Not Found", "text/plain", Vec::new()));
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        }
    });
    (base, requests)
}

async fn serve_json(status: &str, body: Vec<u8>) -> (String, oneshot::Receiver<String>) {
    serve_once(
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}",
            body.len()
        ),
        body,
    )
    .await
}

#[tokio::test]
async fn the_release_policy_asks_gh_exactly_what_the_wrapper_always_has() {
    let dir = scratch("release-argv");
    let (path, _) = artifact(&dir, "tracon.AppImage", b"app");
    let bundle = dir.join("tracon.intoto.jsonl");
    std::fs::write(&bundle, "{}\n").unwrap();

    fake_gh()
        .verify_bundle(&path, &bundle, &RELEASE_POLICY)
        .await
        .unwrap();

    let path = path.to_str().unwrap();
    let bundle = bundle.to_str().unwrap();
    assert_eq!(
        recorded_argv(Path::new(path)),
        [
            "attestation",
            "verify",
            path,
            "--bundle",
            bundle,
            "--hostname",
            "github.com",
            "--repo",
            "cosmicspork/tracon",
            "--signer-workflow",
            "cosmicspork/tracon/.github/workflows/release.yml",
            "--source-ref",
            "refs/heads/main",
            "--deny-self-hosted-runners",
        ]
    );
}

#[tokio::test]
async fn optional_policy_flags_are_left_out_and_a_trusted_root_goes_last() {
    let dir = scratch("plugin-argv");
    let (path, _) = artifact(&dir, "plugin.tar.gz", b"plugin");
    let bundle = dir.join("plugin.jsonl");
    std::fs::write(&bundle, "{}\n").unwrap();

    fake_gh()
        .with_trusted_root("/roots/trusted_root.jsonl")
        .verify_bundle(&path, &bundle, &PLUGIN_POLICY)
        .await
        .unwrap();

    let argv = recorded_argv(&path);
    assert_eq!(
        argv[5..],
        [
            "--hostname",
            "github.com",
            "--repo",
            "cosmicspork/sql-review",
            "--deny-self-hosted-runners",
            "--custom-trusted-root",
            "/roots/trusted_root.jsonl",
        ]
    );
}

#[tokio::test]
async fn a_rejection_carries_what_gh_said() {
    let dir = scratch("gh-says-no");
    let (path, _) = artifact(&dir, "reject.tar.gz", b"x");
    let bundle = dir.join("b.jsonl");
    std::fs::write(&bundle, "{}\n").unwrap();

    let error = fake_gh()
        .verify_bundle(&path, &bundle, &RELEASE_POLICY)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Rejected { detail } if detail == "signature subject mismatch"),
        "{error:?}"
    );

    // With nothing on stderr, the exit status stands in.
    let (path, _) = artifact(&dir, "silent.tar.gz", b"x");
    let error = fake_gh()
        .verify_bundle(&path, &bundle, &RELEASE_POLICY)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Rejected { detail } if detail.contains('3')),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_verifier_that_hangs_times_out_and_a_missing_one_cannot_spawn() {
    let dir = scratch("timeout");
    let (path, _) = artifact(&dir, "slow.tar.gz", b"x");
    let bundle = dir.join("b.jsonl");
    std::fs::write(&bundle, "{}\n").unwrap();

    let slow = Verifier::new(fixtures().join("fake-gh"), Duration::from_millis(300));
    let error = slow
        .verify_bundle(&path, &bundle, &RELEASE_POLICY)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Timeout), "{error:?}");

    let missing = Verifier::new(dir.join("no-such-gh"), Duration::from_secs(5));
    let error = missing
        .verify_bundle(&path, &bundle, &RELEASE_POLICY)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Spawn(_)), "{error:?}");
}

#[tokio::test]
async fn download_bundle_replaces_an_existing_file() {
    let dir = scratch("download-replace");
    let path = dir.join("b.jsonl");
    std::fs::write(&path, "stale contents that are longer\n").unwrap();
    let (base, _) = serve_json("200 OK", b"{\"fresh\":1}\n".to_vec()).await;

    download_bundle(
        &reqwest::Client::new(),
        &format!("{base}/b.jsonl"),
        &path,
        64,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"fresh\":1}\n");
}

#[tokio::test]
async fn download_bundle_refuses_more_than_the_cap_however_it_arrives() {
    let dir = scratch("download-cap");
    let path = dir.join("b.jsonl");
    let client = reqwest::Client::new();

    // Announced.
    let (base, _) = serve_json("200 OK", vec![b'x'; 65]).await;
    let error = download_bundle(&client, &format!("{base}/b"), &path, 64)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Bundle(_)), "{error:?}");
    assert!(!path.exists());

    // Streamed, with no length given.
    let (base, _) = serve_once("HTTP/1.1 200 OK".into(), vec![b'x'; 65]).await;
    let error = download_bundle(&client, &format!("{base}/b"), &path, 64)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Bundle(_)), "{error:?}");
    assert!(!path.exists());
}

#[tokio::test]
async fn download_bundle_refuses_an_error_status() {
    let dir = scratch("download-status");
    let path = dir.join("b.jsonl");
    let (base, _) = serve_json("404 Not Found", b"{}".to_vec()).await;
    let error = download_bundle(&reqwest::Client::new(), &format!("{base}/b"), &path, 64)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Bundle(_)), "{error:?}");
    assert!(!path.exists());
}

#[tokio::test]
async fn a_release_artifact_without_attestations_is_absent() {
    let dir = scratch("absent");
    let (path, digest) = artifact(&dir, "plugin.tar.gz", b"plugin");
    let client = reqwest::Client::new();

    let (base, request) =
        serve_json("404 Not Found", b"{\"message\":\"Not Found\"}".to_vec()).await;
    let outcome = fake_gh()
        .verify_release_artifact(&client, &base, &path, &digest, &PLUGIN_POLICY)
        .await;
    assert_eq!(outcome, Outcome::Absent);
    assert_eq!(
        request.await.unwrap(),
        format!(
            "GET /repos/cosmicspork/sql-review/attestations/sha256:{} HTTP/1.1",
            hex::encode(digest)
        )
    );

    let (base, _) = serve_json("200 OK", b"{\"attestations\":[]}".to_vec()).await;
    let outcome = fake_gh()
        .verify_release_artifact(&client, &base, &path, &digest, &PLUGIN_POLICY)
        .await;
    assert_eq!(outcome, Outcome::Absent);
    assert!(!dir.join("plugin.tar.gz.argv").exists());
}

#[tokio::test]
async fn a_lookup_that_fails_is_never_absent() {
    let dir = scratch("lookup-fails");
    let (path, digest) = artifact(&dir, "plugin.tar.gz", b"plugin");
    let client = reqwest::Client::new();

    for (status, body) in [
        ("500 Internal Server Error", b"{}".to_vec()),
        ("403 Forbidden", b"{}".to_vec()),
        ("200 OK", b"not json".to_vec()),
        ("200 OK", b"{\"attestations\":[{\"bundle\":null}]}".to_vec()),
    ] {
        let (base, _) = serve_json(status, body).await;
        let outcome = fake_gh()
            .verify_release_artifact(&client, &base, &path, &digest, &PLUGIN_POLICY)
            .await;
        assert!(
            matches!(outcome, Outcome::Failed(_)),
            "{status}: {outcome:?}"
        );
    }

    // Nothing listening.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let outcome = fake_gh()
        .verify_release_artifact(&client, &base, &path, &digest, &PLUGIN_POLICY)
        .await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert!(!dir.join("plugin.tar.gz.argv").exists());
}

#[tokio::test]
async fn an_artifact_that_does_not_match_its_digest_is_never_looked_up() {
    let dir = scratch("digest-mismatch");
    let (path, _) = artifact(&dir, "plugin.tar.gz", b"plugin");
    let other: [u8; 32] = Sha256::digest(b"another plugin").into();
    // Never answered: a lookup would hang until the client's timeout.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    let outcome = fake_gh()
        .verify_release_artifact(
            &reqwest::Client::new(),
            &base,
            &path,
            &other,
            &PLUGIN_POLICY,
        )
        .await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert!(!dir.join("plugin.tar.gz.argv").exists());
}

#[tokio::test]
async fn an_attestation_that_verifies_is_verified_and_its_bundle_is_removed() {
    let dir = scratch("verified");
    let (path, digest) = artifact(&dir, "plugin.tar.gz", b"plugin");
    let body = br#"{"attestations":[{"bundle":{"n":1},"bundle_url":"https://example.test"},{"bundle":{"n":2}}]}"#;
    let (base, _) = serve_json("200 OK", body.to_vec()).await;

    let outcome = fake_gh()
        .verify_release_artifact(
            &reqwest::Client::new(),
            &base,
            &path,
            &digest,
            &PLUGIN_POLICY,
        )
        .await;
    assert_eq!(outcome, Outcome::Verified);

    let argv = recorded_argv(&path);
    assert_eq!(argv[2], path.to_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(dir.join("plugin.tar.gz.bundle")).unwrap(),
        "{\"n\":1}\n{\"n\":2}\n"
    );
    assert!(!Path::new(&argv[4]).exists(), "{} was left behind", argv[4]);
}

#[tokio::test]
async fn an_attestation_gh_rejects_fails_with_its_detail() {
    let dir = scratch("gh-refuses");
    let (path, digest) = artifact(&dir, "reject.tar.gz", b"plugin");
    let (base, _) = serve_json("200 OK", br#"{"attestations":[{"bundle":{}}]}"#.to_vec()).await;

    let outcome = fake_gh()
        .verify_release_artifact(
            &reqwest::Client::new(),
            &base,
            &path,
            &digest,
            &PLUGIN_POLICY,
        )
        .await;
    assert!(
        matches!(&outcome, Outcome::Failed(detail) if detail.contains("signature subject mismatch")),
        "{outcome:?}"
    );
}

/// The recorded attestation's bundle as GitHub serves it at its
/// `bundle_url`, snappy-compressed. Fetched from the `bundle_url` the API
/// gave beside the inline bundle in `attestations.json`.
fn recorded_bundle_snappy() -> Vec<u8> {
    std::fs::read(fixtures().join("attestation-bundle.json.sn")).unwrap()
}

/// An attestation lookup that leaves the bundle out and names where it is,
/// under `base`.
fn by_url(base: &str) -> Vec<u8> {
    serde_json::json!({ "attestations": [{
        "bundle": null,
        "bundle_url": format!("{base}/bundles/67146635.json.sn?sig=x"),
        "repository_id": 1,
    }] })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn a_bundle_left_out_is_fetched_from_its_url_and_decompressed() {
    let dir = scratch("bundle-url");
    let (path, digest) = artifact(&dir, "plugin.tar.gz", b"plugin");
    let (base, mut requests) = serve_routes(|base| {
        vec![
            ("/repos/", "application/json", by_url(base)),
            (
                "/bundles/",
                "application/x-snappy",
                recorded_bundle_snappy(),
            ),
        ]
    })
    .await;
    let outcome = fake_gh()
        .verify_release_artifact(
            &reqwest::Client::new(),
            &base,
            &path,
            &digest,
            &PLUGIN_POLICY,
        )
        .await;
    assert_eq!(outcome, Outcome::Verified);
    assert!(requests
        .recv()
        .await
        .unwrap()
        .contains("/repos/cosmicspork/sql-review/attestations/"));
    assert!(requests
        .recv()
        .await
        .unwrap()
        .contains("/bundles/67146635.json.sn?sig=x"));
    // What gh was handed is the very bundle GitHub also gives inline.
    let handed = std::fs::read_to_string(dir.join("plugin.tar.gz.bundle")).unwrap();
    let handed: serde_json::Value = serde_json::from_str(handed.trim_end()).unwrap();
    assert_eq!(handed, recorded_attestations()["attestations"][0]["bundle"]);
}

#[tokio::test]
async fn a_bundle_url_off_githubs_bundle_store_is_not_followed() {
    let dir = scratch("bundle-url-elsewhere");
    let (path, digest) = artifact(&dir, "plugin.tar.gz", b"plugin");
    let (elsewhere, mut fetched) = serve_routes(|_| {
        vec![(
            "/bundles/",
            "application/x-snappy",
            recorded_bundle_snappy(),
        )]
    })
    .await;
    let (api, _) = serve_json("200 OK", by_url(&elsewhere)).await;
    let outcome = fake_gh()
        .verify_release_artifact(
            &reqwest::Client::new(),
            &api,
            &path,
            &digest,
            &PLUGIN_POLICY,
        )
        .await;
    assert!(
        matches!(&outcome, Outcome::Failed(detail) if detail.contains("not where GitHub keeps bundles")),
        "{outcome:?}"
    );
    assert!(fetched.try_recv().is_err(), "the bundle was fetched");
    assert!(!dir.join("plugin.tar.gz.argv").exists(), "gh was asked");
}

#[tokio::test]
async fn a_bundle_that_is_not_snappy_or_not_json_fails() {
    for (label, content_type, body, says) in [
        (
            "bundle-not-snappy",
            "application/x-snappy",
            b"{}".to_vec(),
            "snappy",
        ),
        (
            "bundle-not-json",
            "application/json",
            b"not json".to_vec(),
            "not JSON",
        ),
    ] {
        let dir = scratch(label);
        let (path, digest) = artifact(&dir, "plugin.tar.gz", b"plugin");
        let (base, _) = serve_routes(|base| {
            let lookup = serde_json::json!({ "attestations": [{
                "bundle_url": format!("{base}/bundles/1.json"),
            }] });
            vec![
                (
                    "/repos/",
                    "application/json",
                    lookup.to_string().into_bytes(),
                ),
                ("/bundles/", content_type, body),
            ]
        })
        .await;
        let outcome = fake_gh()
            .verify_release_artifact(
                &reqwest::Client::new(),
                &base,
                &path,
                &digest,
                &PLUGIN_POLICY,
            )
            .await;
        assert!(
            matches!(&outcome, Outcome::Failed(detail) if detail.contains(says)),
            "{label}: {outcome:?}"
        );
    }
}

fn real_gh() -> Verifier {
    let gh = std::env::var_os("TRACON_TEST_GH")
        .expect("TRACON_TEST_GH must name the pinned gh for the real_gh_* tests");
    Verifier::new(gh, Duration::from_secs(60))
        .with_trusted_root(fixtures().join("trusted_root.jsonl"))
}

fn recorded_attestations() -> serde_json::Value {
    let body = std::fs::read(fixtures().join("attestations.json")).unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn real_gh_outcome(
    label: &str,
    contents: &[u8],
    status: &str,
    attestations: &serde_json::Value,
) -> Outcome {
    let dir = scratch(label);
    let (path, digest) = artifact(&dir, FIXTURE_ARTIFACT, contents);
    let (base, _) = serve_json(status, attestations.to_string().into_bytes()).await;
    real_gh()
        .verify_release_artifact(
            &reqwest::Client::new(),
            &base,
            &path,
            &digest,
            &RELEASE_POLICY,
        )
        .await
}

#[tokio::test]
#[ignore = "needs TRACON_TEST_GH"]
async fn real_gh_verifies_a_release_attestation() {
    let contents = std::fs::read(fixtures().join(FIXTURE_ARTIFACT)).unwrap();
    let outcome =
        real_gh_outcome("real-valid", &contents, "200 OK", &recorded_attestations()).await;
    assert_eq!(outcome, Outcome::Verified);
}

#[tokio::test]
#[ignore = "needs TRACON_TEST_GH"]
async fn real_gh_verifies_a_release_attestation_fetched_by_its_bundle_url() {
    let dir = scratch("real-bundle-url");
    let contents = std::fs::read(fixtures().join(FIXTURE_ARTIFACT)).unwrap();
    let (path, digest) = artifact(&dir, FIXTURE_ARTIFACT, &contents);
    let (base, _) = serve_routes(|base| {
        vec![
            ("/repos/", "application/json", by_url(base)),
            (
                "/bundles/",
                "application/x-snappy",
                recorded_bundle_snappy(),
            ),
        ]
    })
    .await;
    let outcome = real_gh()
        .verify_release_artifact(
            &reqwest::Client::new(),
            &base,
            &path,
            &digest,
            &RELEASE_POLICY,
        )
        .await;
    assert_eq!(outcome, Outcome::Verified);
}

#[tokio::test]
#[ignore = "needs TRACON_TEST_GH"]
async fn real_gh_rejects_a_tampered_artifact() {
    let mut contents = std::fs::read(fixtures().join(FIXTURE_ARTIFACT)).unwrap();
    contents[0] ^= 1;
    // Looked up by the tampered file's own digest, and answered with the
    // original's attestation, as a lying API would.
    let outcome = real_gh_outcome(
        "real-tampered",
        &contents,
        "200 OK",
        &recorded_attestations(),
    )
    .await;
    // Refused by gh itself, not by the digest check before it.
    assert!(
        matches!(&outcome, Outcome::Failed(detail) if detail.starts_with("the attestation did not verify")),
        "{outcome:?}"
    );
}

#[tokio::test]
#[ignore = "needs TRACON_TEST_GH"]
async fn real_gh_verifies_when_any_one_attestation_does() {
    // GitHub can hold several attestations for one digest. One with a broken
    // signature, listed first, must neither pass alone nor sink the good one.
    let contents = std::fs::read(fixtures().join(FIXTURE_ARTIFACT)).unwrap();
    let good = recorded_attestations()["attestations"][0].clone();
    let mut broken = good.clone();
    let signature = &mut broken["bundle"]["dsseEnvelope"]["signatures"][0]["sig"];
    let mut sig = signature.as_str().unwrap().to_string();
    let first = if sig.starts_with('B') { "C" } else { "B" };
    sig.replace_range(..1, first);
    *signature = sig.into();

    let only_broken = serde_json::json!({ "attestations": [broken.clone()] });
    let outcome = real_gh_outcome("real-broken", &contents, "200 OK", &only_broken).await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");

    let both = serde_json::json!({ "attestations": [broken, good] });
    let outcome = real_gh_outcome("real-both", &contents, "200 OK", &both).await;
    assert_eq!(outcome, Outcome::Verified);
}

#[tokio::test]
#[ignore = "needs TRACON_TEST_GH"]
async fn real_gh_is_not_asked_when_there_is_no_attestation() {
    let contents = std::fs::read(fixtures().join(FIXTURE_ARTIFACT)).unwrap();
    let outcome = real_gh_outcome(
        "real-absent",
        &contents,
        "404 Not Found",
        &recorded_attestations(),
    )
    .await;
    assert_eq!(outcome, Outcome::Absent);
}
