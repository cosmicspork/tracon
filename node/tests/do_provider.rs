//! The DigitalOcean provider end to end, minus DigitalOcean: a stand-in for
//! its API (`support::do_api`) whose `exec` plays the template's
//! `tracon-sandbox` service by starting `tracon-sandbox`'s own loop, which
//! dials the node's TCP listener exactly as a sandbox there would: the
//! address and pin from the bootstrap, TLS, the run's token.
//!
//! Template registration → create → the bootstrap exec → TLS connect →
//! isolation read back → the run's files and a turn against a fake model
//! through the gateway tunnel → destroy, its config with it. Then what the
//! listener refuses: a spent token, a scanner, a sandbox whose stored
//! egress was widened.
//!
//! The loop reaps every child of its process with `waitpid(-1)`, so this is
//! one test function in a file of its own (see `protocol_runner.rs`).

#![cfg(target_os = "linux")]

#[path = "support/mod.rs"]
mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tracon::config::{Config, HarnessListen};
use tracon::provider::capabilities::Route;
use tracon::provider::digitalocean::bootstrap::{Delivery, Dial as BootstrapDial};
use tracon::provider::digitalocean::listener::DoListener;
use tracon::provider::digitalocean::runtime::DoRuntime;
use tracon::provider::digitalocean::templates::{Source, TemplateState};
use tracon::provider::digitalocean::{ClientSource, HubRelay, Owner, Settings};
use tracon::provider::{Handle, Requirements, SandboxProvider as _, Status};
use tracon::runner::sandbox_files::FilesStore;
use tracon::runner::{Mount, RunnerCommand};
use tracon_sandbox::carrier::Dial;
use tracon_sandbox::inside;
use tracon_sandbox::tls::CertPin;

use support::do_api::FakeDo;

const IMAGE: &str =
    "ghcr.io/example/tracon-do-sandbox@sha256:1111111111111111222222222222222222222222222222222222222222222222";

/// A sandbox's loop on its thread, and how it ended.
type Sandbox = std::thread::JoinHandle<Result<(), String>>;

fn sandbox(config: inside::Config) -> Sandbox {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let ended = runtime.block_on(inside::run(config));
        runtime.shutdown_timeout(Duration::from_secs(1));
        ended.map(|_| ()).map_err(|e| e.to_string())
    })
}

async fn join(thread: Sandbox) -> Result<(), String> {
    tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || thread.join().unwrap()),
    )
    .await
    .expect("a sandbox did not end")
    .unwrap()
}

/// A model behind the gateway: answers each request with what it was sent.
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

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

const CLAUDE: &str = tracon::adapter::claude::ClaudeAdapter::ID;

/// The catalogue on a DigitalOcean sandbox: a service with a bundle runs
/// inside under the supervisor, with the sandbox's own environment (home,
/// and the node's allowlist proxy through the tunnel rather than the
/// runtime's); one with only an image, and one listening where a tunnel
/// does, are refused at once with why. The harness gets the same
/// environment.
async fn services_run_inside_with_the_sandboxs_own_egress(
    runtime: &Arc<DoRuntime>,
    cfg: &Config,
    store: &FilesStore,
    tmp: &tempfile::TempDir,
) {
    let mut cfg = cfg.clone();
    let port = free_port();
    let seen = tmp.path().join("service-env");
    let service = |name: &str, port: u16, bundle: bool| tracon::config::Service {
        name: name.into(),
        image: if bundle {
            String::new()
        } else {
            "pg@sha256:1".into()
        },
        bundle: bundle.then(|| "/srv/bundles/x".into()),
        port,
        ..Default::default()
    };
    cfg.service = vec![
        service("web", port, true),
        service("pg", free_port(), false),
        service("clash", cfg.gateway.forward_port, true),
    ];
    let support = runtime.service_support(&cfg);
    assert!(support[0].1.is_ok(), "{support:?}");
    assert!(support[1]
        .1
        .as_ref()
        .unwrap_err()
        .contains("DigitalOcean runs nothing beside"));

    let runner = runtime.runner_with(
        &cfg,
        Requirements::default(),
        Vec::new(),
        IMAGE.into(),
        CLAUDE,
        store.clone(),
    );
    // The session: its harness waits for the test to finish with it.
    let session = runner
        .spawn(RunnerCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                r#"echo "$HOME|$HTTPS_PROXY|$https_proxy|$NO_PROXY|$CLAUDE_CONFIG_DIR|$DISABLE_AUTOUPDATER"; read done"#.into(),
            ],
            name: "tracon-h-do3".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut lines = BufReader::new(session.stdout).lines();
    let proxy = format!("http://tracon-gw:{}", cfg.gateway.proxy_port);
    assert_eq!(
        lines.next_line().await.unwrap().unwrap(),
        format!(
            "/home/harness|{proxy}|{proxy}|tracon-gw,localhost,127.0.0.1,::1|/home/harness/.claude|1"
        )
    );

    let sidecar = |name: &str, port: u16, argv: Option<Vec<String>>| tracon::runner::Sidecar {
        name: tracon::runner::sidecar_name("tracon-h-do3", name),
        session_container: "tracon-h-do3".into(),
        image: String::new(),
        command: Vec::new(),
        port,
        ready: None,
        in_sandbox: argv.map(|argv| tracon::runner::InSandbox {
            service: name.into(),
            argv,
            env: [("ONLY_MINE".to_string(), "yes".to_string())].into(),
            restart: tracon::config::ServiceRestart::Never,
            ready_within: Duration::from_secs(20),
        }),
    };
    let web = sidecar(
        "web",
        port,
        Some(vec![
            "sh".into(),
            "-c".into(),
            format!(
                r#"echo "$HOME|$HTTPS_PROXY|$ONLY_MINE" > {}; exec python3 -m http.server {port} --bind 127.0.0.1"#,
                s(&seen)
            ),
        ]),
    );
    runner.start_sidecar(&web).await.unwrap();
    let mut ready = false;
    for _ in 0..200 {
        if runner.probe_sidecar(&web).await.unwrap() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "the service never answered");
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        format!("/home/harness|{proxy}|yes\n")
    );

    let pg = runner
        .start_sidecar(&sidecar("pg", free_port(), None))
        .await
        .unwrap_err()
        .to_string();
    assert!(pg.contains("give it a bundle"), "{pg}");
    let clash = runner
        .start_sidecar(&sidecar(
            "clash",
            cfg.gateway.forward_port,
            Some(vec!["true".into()]),
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(clash.contains("gateway tunnel already does"), "{clash}");

    runner.kill(&web.name).await.unwrap();
    let mut stdin = session.stdin;
    stdin.write_all(b"done\n").await.unwrap();
    drop(stdin);
    assert_eq!(session.done.await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_runs_on_digitalocean_minus_digitalocean() {
    support::state::isolate();
    let fake = FakeDo::start().await;
    fake.api.lock().unwrap().template_builds = 1;

    let server = tracon::runner::protocol::node_server().unwrap();
    let listener = DoListener::bind("127.0.0.1:0".parse().unwrap(), server.clone())
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
        image: IMAGE.into(),
        listen: node.to_string(),
        node_addr: node.to_string(),
        limit: 2,
        ..Settings::default()
    };
    let client: ClientSource = {
        let fake = fake.clone();
        Arc::new(move || Ok(fake.client()))
    };
    let runtime = DoRuntime::with(
        client,
        settings,
        Source::Given(IMAGE.into()),
        Owner::new("tracon", "abcd1234"),
        "tracon-gw".into(),
        listener.clone(),
        HubRelay::none(),
    )
    .unwrap();
    let provider = runtime.provider().clone();
    // The stand-in's supervisor is this test's own loop, as its user.
    let me = tracon_sandbox::inside::user::current().uid;
    provider.expect_uid(Some(me));

    // Each exec plays the template's service: read the bootstrap the node
    // delivered and dial as a sandbox there would.
    let threads: Arc<Mutex<Vec<Sandbox>>> = Arc::default();
    let widen: Arc<Mutex<bool>> = Arc::default();
    fake.on_exec({
        let threads = threads.clone();
        let widen = widen.clone();
        let api = fake.api.clone();
        Arc::new(move |session: &str, argv: &[String]| {
            let Some(d) = Delivery::parse(argv) else {
                return 2;
            };
            if *widen.lock().unwrap() {
                // Someone widened the stored egress behind the node's back.
                let mut api = api.lock().unwrap();
                let config = api.sessions[session]["config_id"]
                    .as_str()
                    .unwrap()
                    .to_string();
                api.configs.get_mut(&config).unwrap()["manifest"]["egress"]["allow_ips"] =
                    serde_json::json!([d.dial.value().split(':').next().unwrap(), "198.51.100.9"]);
            }
            let mut config = inside::Config::new(
                Dial::Tcp(d.dial.value().to_string()),
                CertPin::parse(d.pin).unwrap(),
                d.token,
            );
            config.server_name = d.server_name.to_string();
            threads.lock().unwrap().push(sandbox(config));
            0
        })
    });

    // No session before its template is ready, and none is registered per
    // session: the first look registers it, a later one finds it ready.
    assert!(runtime.health(&cfg).is_err());
    assert!(matches!(
        runtime.ensure_template(IMAGE).await,
        TemplateState::Building { .. }
    ));
    assert!(runtime.health(&cfg).is_err());
    assert!(matches!(
        runtime.ensure_template(IMAGE).await,
        TemplateState::Ready { .. }
    ));
    runtime.health(&cfg).unwrap();
    assert_eq!(fake.api.lock().unwrap().templates.len(), 1);

    // What Settings shows of it: the promises, by what makes them so, and
    // how its sandboxes reach the node.
    let report = serde_json::to_value(runtime.report(&cfg).await).unwrap();
    assert_eq!(report["provider"], "digitalocean");
    assert_eq!(report["verified"], true);
    assert_eq!(report["enforced"], true);
    let value = |capability: &str| {
        report["lines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["capability"] == capability)
            .map(|l| l["value"].clone())
            .unwrap()
    };
    assert_eq!(value("tier"), "microvm");
    assert_eq!(value("route"), "node-only");
    assert_eq!(value("dns"), "provider-open");
    assert_eq!(value("privilege"), "root-in-vm");
    // No sandbox has said who it runs as yet.
    assert!(report["elsewhere"]["reported"].is_null(), "{report}");
    assert_eq!(value("image_identity"), "derived");
    let elsewhere = &report["elsewhere"];
    assert_eq!(elsewhere["reach"], "address");
    assert_eq!(elsewhere["dial"], node.to_string());
    assert_eq!(elsewhere["egress"], serde_json::json!(["127.0.0.1"]));
    assert_eq!(elsewhere["healthy"], true);
    assert!(elsewhere.get("why").is_none(), "{elsewhere}");
    assert_eq!(elsewhere["capacity"]["limit"], 2);
    let template = provider.templates().name_for(IMAGE).unwrap();
    assert_eq!(template, "tracon-tabcd1234-1111111111111111");

    // A workspace the node keeps, sent over the files channel.
    let tmp = tempfile::tempdir().unwrap();
    let store = FilesStore::new(tmp.path().join("store"));
    let seed = tmp.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    std::fs::write(seed.join("README"), "from the node\n").unwrap();
    store.import("tracon-workspace-do1", &seed).unwrap();
    let work = tmp.path().join("inside/work");
    let mounts = vec![Mount::volume("tracon-workspace-do1", s(&work), false)];

    let runner = runtime.runner_with(
        &cfg,
        Requirements {
            route: Some(vec![Route::NodeOnly]),
            ..Requirements::default()
        },
        mounts.clone(),
        IMAGE.into(),
        CLAUDE,
        store.clone(),
    );

    // One turn: the prompt in, the model through the gateway's tunnel, the
    // answer out, and a file written back to the node.
    let port = cfg.gateway.forward_port;
    let script = format!(
        "cat README; read prompt; curl -sS -X POST --data \"$prompt\" http://127.0.0.1:{port}/v1/messages; echo; echo done > out.txt"
    );
    let spawned = runner
        .spawn(RunnerCommand {
            argv: vec!["sh".into(), "-c".into(), script],
            name: "tracon-h-do1".into(),
            workdir: Some(s(&work)),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut stdin = spawned.stdin;
    let mut lines = BufReader::new(spawned.stdout).lines();
    assert_eq!(
        lines.next_line().await.unwrap().as_deref(),
        Some("from the node")
    );
    stdin.write_all(b"hello model\n").await.unwrap();
    stdin.flush().await.unwrap();
    assert_eq!(
        lines.next_line().await.unwrap().as_deref(),
        Some("pong: hello model")
    );
    drop(stdin);
    assert_eq!(spawned.done.await.unwrap(), 0);
    assert_eq!(seen.lock().unwrap().as_slice(), ["hello model"]);
    assert_eq!(
        std::fs::read_to_string(store.tree("tracon-workspace-do1").unwrap().join("out.txt"))
            .unwrap(),
        "done\n"
    );

    let (manifest, exec, deleted_sessions, deleted_configs) = {
        let api = fake.api.lock().unwrap();
        (
            api.manifests[0].clone(),
            api.execs[0].clone(),
            api.deleted_sessions.clone(),
            api.deleted_configs.clone(),
        )
    };
    // What DigitalOcean stores holds nothing of the run.
    assert_eq!(
        manifest,
        serde_json::json!({
            "name": "tracon-h-do1-nabcd1234",
            "harness": "none",
            "template": template,
            "keep_warm": true,
            "egress": {"allow_ips": ["127.0.0.1"]},
        })
    );
    let delivered = Delivery::parse(&exec.1).unwrap();
    assert_eq!(delivered.dial, BootstrapDial::Addr(&node.to_string()));
    assert_eq!(delivered.pin, server.pin().to_string());
    assert_eq!(delivered.server_name, "tracon-node");
    assert_eq!(delivered.gateway_host, "tracon-gw");
    assert!(!manifest.to_string().contains(delivered.token));
    // Destroyed with the config its create left.
    assert_eq!(deleted_sessions, std::slice::from_ref(&exec.0));
    assert_eq!(deleted_configs.len(), 1);
    let ended = std::mem::take(&mut *threads.lock().unwrap());
    for thread in ended {
        join(thread).await.unwrap();
    }

    services_run_inside_with_the_sandboxs_own_egress(&runtime, &cfg, &store, &tmp).await;
    let ended = std::mem::take(&mut *threads.lock().unwrap());
    for thread in ended {
        join(thread).await.unwrap();
    }
    // Settings says who the harness ran as, from the supervisor's hello.
    let report = serde_json::to_value(runtime.report(&cfg).await).unwrap();
    assert_eq!(report["elsewhere"]["reported"]["uid"], me);
    assert_eq!(report["elsewhere"]["expected_uid"], me);

    // A supervisor that is not the sandbox's user (an older one that never
    // dropped root, say) is refused before anything is sent into it.
    provider.expect_uid(Some(me + 1));
    let refused = runner
        .run_capture(RunnerCommand {
            argv: vec!["true".into()],
            name: "tracon-h-do4".into(),
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains(&format!("not uid {}", me + 1)),
        "{refused}"
    );
    provider.expect_uid(Some(me));
    let ended = std::mem::take(&mut *threads.lock().unwrap());
    for thread in ended {
        let _ = join(thread).await;
    }

    // A spent token is refused: a second sandbox presenting it gets nowhere.
    let refused_before = listener.refused();
    let mut replay = inside::Config::new(
        Dial::Tcp(node.to_string()),
        CertPin::parse(delivered.pin).unwrap(),
        delivered.token,
    );
    replay.connect_timeout = Duration::from_secs(2);
    let replayed = join(sandbox(replay)).await;
    assert!(replayed.is_err(), "a spent token was accepted");
    assert!(listener.refused() > refused_before);

    // A scanner gets a closed connection and no work.
    let refused_before = listener.refused();
    let mut scan = tokio::net::TcpStream::connect(node).await.unwrap();
    scan.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(15), scan.read_to_end(&mut buf)).await;
    // Closed, with at most TLS's own alert: no HTTP, nothing of a sandbox.
    assert!(
        matches!(read, Ok(Ok(n)) if n < 64) || matches!(read, Ok(Err(_))),
        "{read:?} {buf:?}"
    );
    for _ in 0..50 {
        if listener.refused() > refused_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(listener.refused() > refused_before);

    // A sandbox whose stored egress no longer names the node alone is read
    // back as unverified and removed before anything is sent into it.
    *widen.lock().unwrap() = true;
    let refused = runner
        .run_capture(RunnerCommand {
            argv: vec!["true".into()],
            name: "tracon-h-do2".into(),
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("not isolated as required"), "{refused}");
    assert!(refused.contains("route is unverified"), "{refused}");
    let ended = std::mem::take(&mut *threads.lock().unwrap());
    for thread in ended {
        let _ = join(thread).await;
    }
    {
        let api = fake.api.lock().unwrap();
        assert!(api.sessions.is_empty(), "{:?}", api.sessions);
        assert_eq!(api.deleted_configs.len(), 4);
    }

    // Read back: paused is not lost, gone is lost, and only this node's
    // live sessions count against its limit.
    let (paused, other, gone) = {
        let mut api = fake.api.lock().unwrap();
        (
            api.put_session("tracon-h-p-nabcd1234", "SESSION_STATUS_PAUSED"),
            api.put_session("tracon-h-p-nffff0000", "SESSION_STATUS_READY"),
            api.put_session("tracon-h-q-nabcd1234", "SESSION_STATUS_DESTROYED"),
        )
    };
    assert_eq!(
        provider.status(&Handle(paused.clone())).await.unwrap(),
        Status::Paused
    );
    assert_eq!(
        provider.status(&Handle("missing".into())).await.unwrap(),
        Status::Lost
    );
    let capacity = provider.capacity().await.unwrap();
    assert_eq!((capacity.running, capacity.limit), (1, Some(2)));
    let _ = (other, gone);
    // Killing by name, as a fresh node process does, finds this node's.
    provider
        .destroy(&Handle("tracon-h-p".into()))
        .await
        .unwrap();
    assert!(!fake.api.lock().unwrap().sessions.contains_key(&paused));
}
