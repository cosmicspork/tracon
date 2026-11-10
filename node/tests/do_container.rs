//! The DigitalOcean template's image as the node builds it
//! (`provider::digitalocean::images`), run in a real container standing in
//! for a DigitalOcean sandbox: the container starts the template's
//! `tracon-sandbox` service as the runtime's init would, the stand-in API's
//! `exec` runs the node's bootstrap delivery in it with `podman exec`, and
//! the supervisor dials the node's TCP listener with TLS and the run's
//! token. A command then runs in it over the protocol, reaching a fake model
//! through the gateway tunnel by the gateway's name.
//!
//! Opt-in: `TRACON_PODMAN_TESTS=1 TRACON_SANDBOX_BUNDLE=<dir holding
//! bin/tracon-sandbox>`, optionally `TRACON_DO_TEST_BASE` (default
//! `docker.io/library/ubuntu:24.04`, which must be present locally). Every
//! container and image it makes is named `spike-p8-…` and removed.

#![cfg(target_os = "linux")]

#[path = "support/mod.rs"]
mod support;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracon::config::{Config, HarnessListen};
use tracon::provider::digitalocean::listener::DoListener;
use tracon::provider::digitalocean::runtime::DoRuntime;
use tracon::provider::digitalocean::templates::{Source, TemplateState};
use tracon::provider::digitalocean::{images, ClientSource, HubRelay, Owner, Settings};
use tracon::provider::Requirements;
use tracon::runner::sandbox_files::FilesStore;
use tracon::runner::{Mount, RunnerCommand};

use support::do_api::FakeDo;

const CONTAINER: &str = "spike-p8-do-1";
const TAG: &str = "localhost/spike-p8-do-sandbox:test";

fn podman(args: &[&str]) -> std::process::Output {
    std::process::Command::new("podman")
        .args(args)
        .output()
        .unwrap()
}

async fn model() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post({
            let seen = seen.clone();
            move |body: String| async move {
                seen.lock().unwrap().push(body.clone());
                format!("pong: {body}")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_template_image_runs_a_session_as_a_digitalocean_sandbox_would() {
    if std::env::var_os("TRACON_PODMAN_TESTS").is_none() {
        eprintln!("skipped: set TRACON_PODMAN_TESTS=1 and TRACON_SANDBOX_BUNDLE");
        return;
    }
    let bundle = std::path::PathBuf::from(
        std::env::var("TRACON_SANDBOX_BUNDLE").expect("TRACON_SANDBOX_BUNDLE"),
    );
    let base = std::env::var("TRACON_DO_TEST_BASE")
        .unwrap_or_else(|_| "docker.io/library/ubuntu:24.04".into());
    support::state::isolate();
    let _ = podman(&["rm", "-f", CONTAINER]);

    // The node's own build of the template source.
    images::build(&base, &bundle, TAG).await.unwrap();
    let inspect = podman(&["image", "inspect", TAG, "--format", "{{.Digest}}"]);
    let digest = String::from_utf8_lossy(&inspect.stdout).trim().to_string();
    let image = format!("localhost/spike-p8-do-sandbox@{digest}");

    // The runtime's init starts the template's service, which waits.
    let started = podman(&[
        "run",
        "-d",
        "--name",
        CONTAINER,
        "--network",
        "host",
        "--entrypoint",
        "/etc/services.d/tracon-sandbox/run",
        TAG,
    ]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );

    let fake = FakeDo::start().await;
    let listener = DoListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        tracon::runner::protocol::node_server().unwrap(),
    )
    .await
    .unwrap();
    let node = listener.local_addr();
    let (model, seen) = model().await;
    let mut cfg = Config::default();
    cfg.runtime.podman.sandbox_protocol = true;
    cfg.runtime.podman.sandbox_files = true;
    cfg.runtime.podman.sandbox_tunnels = true;
    cfg.gateway.forward_port = free_port();
    cfg.gateway.proxy_port = free_port();
    cfg.gateway.qa_proxy_port = 0;
    cfg.gateway.harness_listen = HarnessListen::Tcp(model);
    let settings = Settings {
        credential: "do".into(),
        api_url: fake.url.clone(),
        image: image.clone(),
        listen: node.to_string(),
        node_addr: node.to_string(),
        ..Settings::default()
    };
    let client: ClientSource = {
        let fake = fake.clone();
        Arc::new(move || Ok(fake.client()))
    };
    let runtime = DoRuntime::with(
        client,
        settings,
        Source::Given(image.clone()),
        Owner::new("spike-p8", "abcd1234"),
        "tracon-gw".into(),
        listener.clone(),
        HubRelay::none(),
    )
    .unwrap();
    while !matches!(
        runtime.ensure_template(&image).await,
        TemplateState::Ready { .. }
    ) {}

    // The runtime's exec: the node's delivery, run in the sandbox.
    fake.on_exec(Arc::new(|_session: &str, argv: &[String]| {
        let mut args = vec!["exec", CONTAINER];
        args.extend(argv.iter().map(String::as_str));
        let out = podman(&args);
        if !out.status.success() {
            eprintln!("bootstrap: {}", String::from_utf8_lossy(&out.stderr));
        }
        out.status.code().unwrap_or(1)
    }));

    let tmp = tempfile::tempdir().unwrap();
    let store = FilesStore::new(tmp.path().join("store"));
    let seed = tmp.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    std::fs::write(seed.join("README"), "from the node\n").unwrap();
    store.import("spike-p8-workspace-1", &seed).unwrap();
    let runner = runtime.runner_with(
        &cfg,
        Requirements::default(),
        vec![Mount::volume("spike-p8-workspace-1", "/work", false)],
        image.clone(),
        store.clone(),
    );
    let port = cfg.gateway.forward_port;
    let script = format!(
        "cat README; grep -c '^127.0.0.1 tracon-gw$' /etc/hosts; \
         test -e /run/tracon/token && echo token-kept || echo token-gone; \
         test -e /run/tracon/bootstrap && echo bootstrap-kept || echo bootstrap-gone; \
         exec 3<>/dev/tcp/tracon-gw/{port}; \
         printf 'POST /v1/messages HTTP/1.0\\r\\nContent-Length: 5\\r\\n\\r\\nhello' >&3; \
         tail -n 1 <&3; echo; echo written > out.txt"
    );
    let out = runner
        .run_capture(RunnerCommand {
            argv: vec!["bash".into(), "-c".into(), script],
            name: "spike-p8-h-1".into(),
            workdir: Some("/work".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "{stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            "from the node",
            "1",
            "token-gone",
            "bootstrap-gone",
            "pong: hello"
        ],
        "{stdout}"
    );
    assert_eq!(seen.lock().unwrap().as_slice(), ["hello"]);

    // The run ended: the stand-in's session and config are gone, and the
    // supervisor with it.
    {
        let api = fake.api.lock().unwrap();
        assert!(api.sessions.is_empty());
        assert_eq!(api.deleted_configs.len(), 1);
    }
    let mut exited = false;
    for _ in 0..100 {
        let state = podman(&["inspect", CONTAINER, "--format", "{{.State.Status}}"]);
        if String::from_utf8_lossy(&state.stdout).trim() == "exited" {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = podman(&["rm", "-f", CONTAINER]);
    let _ = podman(&["rmi", "-f", TAG]);
    assert!(exited, "the supervisor did not end with its run");
}
