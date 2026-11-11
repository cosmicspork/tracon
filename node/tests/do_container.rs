//! The DigitalOcean template's image as the node builds it
//! (`provider::digitalocean::images`), run in a real container standing in
//! for a DigitalOcean sandbox: the container starts the template's
//! `tracon-sandbox` service as root, as the runtime's init would, the
//! stand-in API's `exec` runs the node's bootstrap delivery in it with
//! `podman exec` (as root, as the runtime's exec does), and the supervisor
//! dials the node's TCP listener with TLS and the run's token, having become
//! the sandbox's user. A command then runs in it over the protocol as that
//! user, reaching a fake model through the gateway tunnel by the gateway's
//! name, and a service baked into the template runs beside it under the
//! supervisor, as that user too.
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

    // The node's own build of the template source, with one service bundle:
    // a TCP listener in Perl, which even a bare Ubuntu has.
    let services_dir = tempfile::tempdir().unwrap();
    let echo = services_dir.path().join("echo");
    std::fs::create_dir_all(&echo).unwrap();
    std::fs::write(
        echo.join("run"),
        "#!/bin/sh\nid -u > /tmp/echo-uid\nexec perl -MIO::Socket::INET -e '$s = IO::Socket::INET->new(LocalAddr => \"127.0.0.1\", LocalPort => $ENV{ECHO_PORT}, Listen => 5, ReuseAddr => 1) or die $!; while ($c = $s->accept) { print $c \"ok\\n\"; close $c }'\n",
    )
    .unwrap();
    std::fs::set_permissions(
        echo.join("run"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    images::build(
        &base,
        &images::Contents {
            bundle: &bundle,
            services: &[(echo, "/opt/tracon-services/echo".into())],
        },
        TAG,
    )
    .await
    .unwrap();
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
        tracon::adapter::claude::ClaudeAdapter::ID,
        store.clone(),
    );
    let port = cfg.gateway.forward_port;
    let echo_port = free_port();
    // The harness waits for a line, so the service can start beside it.
    let script = format!(
        "read go; cat README; grep -c '^127.0.0.1 tracon-gw$' /etc/hosts; \
         echo \"$(id -u):$(id -g) $HOME $PWD $(stat -c %u /work)\"; \
         test -r /run/tracon && echo bootstrap-readable || echo bootstrap-closed; \
         cat /tmp/echo-uid; exec 4<>/dev/tcp/127.0.0.1/{echo_port}; head -n 1 <&4; \
         exec 3<>/dev/tcp/tracon-gw/{port}; \
         printf 'POST /v1/messages HTTP/1.0\\r\\nContent-Length: 5\\r\\n\\r\\nhello' >&3; \
         tail -n 1 <&3; echo; echo written > out.txt"
    );
    let session = runner
        .spawn(RunnerCommand {
            argv: vec!["bash".into(), "-c".into(), script],
            name: "spike-p8-h-1".into(),
            workdir: Some("/work".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let echo = tracon::runner::Sidecar {
        name: tracon::runner::sidecar_name("spike-p8-h-1", "echo"),
        session_container: "spike-p8-h-1".into(),
        image: String::new(),
        command: Vec::new(),
        port: echo_port,
        ready: None,
        in_sandbox: Some(tracon::runner::InSandbox {
            service: "echo".into(),
            argv: vec!["/opt/tracon-services/echo/run".into()],
            env: [("ECHO_PORT".to_string(), echo_port.to_string())].into(),
            restart: tracon::config::ServiceRestart::Never,
            ready_within: Duration::from_secs(20),
        }),
    };
    runner.start_sidecar(&echo).await.unwrap();
    let mut ready = false;
    for _ in 0..200 {
        if runner.probe_sidecar(&echo).await.unwrap() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "the baked service never answered");
    let mut stdin = session.stdin;
    tokio::io::AsyncWriteExt::write_all(&mut stdin, b"go\n")
        .await
        .unwrap();
    drop(stdin);
    let mut stdout = String::new();
    tokio::io::AsyncReadExt::read_to_string(&mut { session.stdout }, &mut stdout)
        .await
        .unwrap();
    assert_eq!(session.done.await.unwrap(), 0, "{stdout}");
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            "from the node",
            "1",
            "65532:65532 /home/harness /work 65532",
            "bootstrap-closed",
            "65532",
            "ok",
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
