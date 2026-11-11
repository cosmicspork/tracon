//! Catalogue services inside a session's sandbox, against real Podman: with
//! `[runtime.podman] sandbox_services`, `service_start` has the sandbox's
//! supervisor run the service from its mounted bundle, on the sandbox's
//! loopback, with no container beside the session.
//!
//! Runs only when asked for:
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 TRACON_SANDBOX_BUNDLE=<bundle>/<version>/amd64 \
//!   TRACON_BROWSER_BUNDLE=<browser>/<version>/amd64 \
//!   TRACON_BROWSER_RELEASE=<dir holding tracon-browser-<chrome>-amd64.tar.gz and its .sha256> \
//!   TRACON_SANDBOX_TEST_IMAGE=ghcr.io/cosmicspork/tracon-sandbox-base:<version> \
//!   cargo test -p tracon --test podman_services -- --test-threads 1
//! ```
//!
//! The harness bundle needs `bin/tracon-sandbox` built from this tree; the
//! browser bundle is `scripts/browser-bundle.sh`'s, unpacked, and the release
//! directory holds the same as a release attaches it (a release's own assets
//! will do, for the Chrome this tree pins), served from a local server as
//! this version's release. The image must be in the
//! local store and have Node and Python, as the stock base does: Node drives
//! the browser over CDP as an agent would, Python stands in for a service.
//! `TRACON_SERVICES_TEST_TUNNELS=1` runs the sandboxes with
//! `[runtime.podman] sandbox_tunnels` (`--network none`). Everything made is
//! named with `TRACON_SANDBOX_TEST_PREFIX` (default
//! `svctest-`, which is also the node's `[boundary] resource_prefix`, so a
//! workspace volume is `svctest-workspace-<id>`) and removed again.

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tracon::config::{Config, HarnessListen, Service, ServiceRestart};
use tracon::runner::podman::RunSpec;
use tracon::runner::{Mount, Runner, RunnerCommand};
use tracon::sidecars;
use tracon::store::{now_ms, NewEvent, SessionRow, Store};

use support::podman::{podman, requested};

struct Rig {
    prefix: String,
    cfg: Config,
    image: String,
    network: String,
    volumes: Vec<String>,
    _short: tempfile::TempDir,
}

impl Rig {
    fn new(tag: &str) -> Option<Rig> {
        if !requested() {
            return None;
        }
        support::state::isolate();
        let base =
            std::env::var("TRACON_SANDBOX_TEST_PREFIX").unwrap_or_else(|_| "svctest-".into());
        claim_resource_prefix(base.trim_end_matches('-'));
        let prefix = format!("{base}{tag}-");
        let bundle = std::env::var("TRACON_SANDBOX_BUNDLE")
            .map(PathBuf::from)
            .expect("TRACON_PODMAN_TESTS=1 needs TRACON_SANDBOX_BUNDLE");
        assert!(bundle.join("bin/tracon-sandbox").is_file());
        let image = std::env::var("TRACON_SANDBOX_TEST_IMAGE")
            .expect("these tests need TRACON_SANDBOX_TEST_IMAGE, an image with node and python3");
        assert!(podman(&["image", "exists", &image]).is_some(), "{image}");
        let network = format!("{prefix}net");
        let _ = podman(&["network", "rm", "-f", &network]);
        assert!(
            podman(&["network", "create", "--disable-dns", "--internal", &network]).is_some(),
            "create {network}"
        );
        let short = tempfile::Builder::new()
            .prefix("svc")
            .tempdir_in("/tmp")
            .unwrap();
        let mut cfg = Config::default();
        // Each switch this suite runs is set below; the rest stay off.
        cfg.runtime.podman = tracon::config::PodmanRuntime::legacy();
        cfg.boundary.network = network.clone();
        cfg.boundary.gateway_container = format!("{prefix}gw");
        cfg.boundary.harness_image = image.clone();
        cfg.gateway.harness_listen = HarnessListen::Unix(short.path().join("harness.sock"));
        cfg.runtime.podman.sandbox_protocol = true;
        cfg.runtime.podman.sandbox_services = true;
        // The sandbox with no network of its own: services are on its
        // loopback either way.
        cfg.runtime.podman.sandbox_tunnels =
            std::env::var("TRACON_SERVICES_TEST_TUNNELS").is_ok_and(|v| v == "1");
        cfg.sandbox.bundle = Some(bundle);
        cfg.boundary.resource_prefix = tracon::config::resource_prefix().to_string();
        Some(Rig {
            prefix,
            cfg,
            image,
            network,
            volumes: Vec::new(),
            _short: short,
        })
    }

    fn name(&self, what: &str) -> String {
        format!("{}{what}", self.prefix)
    }

    fn runner(&self, mounts: Vec<Mount>) -> Arc<dyn Runner> {
        let mut spec = RunSpec::from_config(&self.cfg, true);
        spec.image = self.image.clone();
        spec.extra_mounts = mounts;
        tracon::boundary::podman::protocol_runner(&self.cfg, spec)
    }

    fn command(&self, what: &str, script: &str) -> RunnerCommand {
        RunnerCommand {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            name: self.name(what),
            ..Default::default()
        }
    }

    /// Nothing of this rig's is left in Podman: no sandbox, no container
    /// beside one, and no socket directory.
    fn assert_gone(&self) {
        let left = podman(&[
            "ps",
            "-a",
            "--format",
            "{{.Names}}",
            "--filter",
            &format!("name=^{}", self.prefix),
        ])
        .unwrap_or_default();
        assert!(left.is_empty(), "left behind: {left}");
        let sockets = self.cfg.sandbox_socket_dir();
        assert_eq!(std::fs::read_dir(&sockets).unwrap().count(), 0);
    }
}

/// Names the process's runtime resources (a workspace's volume) from the
/// test prefix, never the default node's: the first configuration a process
/// loads as its own is what does that.
fn claim_resource_prefix(prefix: &str) {
    let path = Config::config_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    if !path.exists() {
        std::fs::write(
            &path,
            format!("[boundary]\nresource_prefix = \"{prefix}\"\n"),
        )
        .unwrap();
    }
    Config::load();
    assert!(
        !tracon::config::resource_prefix().starts_with("tracon"),
        "the test's runtime names must not be a node's"
    );
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(ids) = podman(&["ps", "-aq", "--filter", &format!("name=^{}", self.prefix)]) {
            for id in ids.lines() {
                let _ = podman(&["rm", "-f", id]);
            }
        }
        for volume in &self.volumes {
            let _ = podman(&["volume", "rm", "-f", volume]);
        }
        let _ = podman(&["network", "rm", "-f", &self.network]);
    }
}

/// A session row whose container is `container`, and a recorder of its
/// `service` events.
fn session(store: &Arc<Store>, container: &str) -> (SessionRow, impl Fn(Value) + Send + Sync) {
    store.ensure_peer_node("n1").unwrap();
    let row: SessionRow = serde_json::from_value(json!({
        "id": "s1", "node_id": "n1", "channel": "personal", "repo_path": "/r",
        "branch": "b", "harness_id": "fake", "harness_version": "1", "model": "m",
        "phase": "execute", "budget_tokens": 1000, "tokens_used": 0,
        "state": "running", "turn_active": 0, "created_ms": 0, "updated_ms": 0,
        "container_name": container,
    }))
    .unwrap();
    store.insert_session(&row).unwrap();
    let store = store.clone();
    let record = move |payload: Value| {
        store
            .append_event(&NewEvent {
                session_id: "s1".into(),
                work_item_id: None,
                kind: "service".into(),
                ref_id: None,
                payload,
                at_ms: now_ms(),
                mono_ms: 0,
            })
            .unwrap();
    };
    (row, record)
}

/// What an agent in the session does with the browser: open a page over
/// CDP, render it, and save a screenshot in its workspace for `show_work`.
const SCREENSHOT: &str = r#"
(async () => {
  const html = '<h1 style="font-family:sans-serif">Shown through the sandbox</h1>' +
    '<p style="font-family:monospace">a service under tracon-sandbox</p>';
  const base = 'http://127.0.0.1:9222';
  const target = await (await fetch(base + '/json/new?about:blank', { method: 'PUT' })).json();
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((ok, no) => { ws.onopen = ok; ws.onerror = no; });
  let next = 0;
  const pending = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); }
  };
  const call = (method, params = {}) => new Promise((done) => {
    const id = ++next;
    pending.set(id, done);
    ws.send(JSON.stringify({ id, method, params }));
  });
  const value = async (expression) =>
    (await call('Runtime.evaluate', { expression, returnByValue: true })).result.result.value;
  await call('Emulation.setDeviceMetricsOverride', { width: 640, height: 160, deviceScaleFactor: 1, mobile: false });
  await call('Page.navigate', { url: 'data:text/html,' + encodeURIComponent(html) });
  for (let i = 0; i < 100 && !(await value('document.readyState === "complete" && !!document.querySelector("h1")')); i++) {
    await new Promise((r) => setTimeout(r, 100));
  }
  const textWidth = await value('(() => { const r = document.createRange(); r.selectNodeContents(document.querySelector("h1")); return r.getBoundingClientRect().width; })()');
  const shot = await call('Page.captureScreenshot', { format: 'png' });
  const png = Buffer.from(shot.result.data, 'base64');
  require('fs').writeFileSync('/work/shot.png', png);
  console.log(JSON.stringify({ text_width: textWidth, png_bytes: png.length }));
  process.exit(0);
})().catch((e) => { console.error(e); process.exit(1); });
"#;

async fn mcp_tool(h: &support::harness::Harness, token: &str, name: &str, args: Value) -> Value {
    use tower::ServiceExt as _;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args },
    });
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp/s1")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let res = h.harness.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(text).unwrap_or(json!({ "raw": text, "error": v["result"]["isError"] }))
}

/// The whole of it through the node's own tools: a session's
/// `service_start browser` has its sandbox's supervisor start the bundle's
/// Chrome, the session drives it over CDP on its loopback, and `show_work`
/// carries the screenshot out of the workspace the node exports.
#[tokio::test]
async fn the_browser_runs_in_the_session_sandbox_and_show_work_carries_its_screenshot() {
    let Some(mut rig) = Rig::new("browser") else {
        return;
    };
    let browser = std::env::var("TRACON_BROWSER_BUNDLE")
        .map(PathBuf::from)
        .expect("this test needs TRACON_BROWSER_BUNDLE");
    assert!(browser.join("run").is_file(), "{}", browser.display());
    rig.cfg.service = vec![Service {
        name: "browser".into(),
        bundle: Some(browser),
        port: 9222,
        ready: Some("/json/version".into()),
        ..Service::default()
    }];
    tracon::config::validate_services(&rig.cfg.service).unwrap();
    // The workspace volume the node exports for show_work.
    let work = tracon::workspace::volume_name("s1");
    eprintln!("workspace volume {work}");
    let _ = podman(&["volume", "rm", "-f", &work]);
    rig.volumes.push(work.clone());
    let backend = Arc::new(tracon::boundary::podman::PodmanBackend::detect(&rig.cfg).await);
    let h = support::harness::harness_on(rig.cfg.clone(), backend.clone()).await;
    let runner = rig.runner(vec![Mount::volume(&work, "/work", false)]);

    let mut init = rig.command(
        "init",
        "cd /work && git init -q && git -c user.email=a@b -c user.name=a commit -q --allow-empty -m init",
    );
    init.mounts = Vec::new();
    let out = runner.run_capture(init).await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The session: it waits to be told the browser is up, then drives it.
    let mut harness = rig.command("h", "read go && node -e \"$SCREENSHOT\"");
    harness.env = vec![("SCREENSHOT".into(), SCREENSHOT.into())];
    let session_container = rig.name("h");
    let spawned = runner.spawn(harness).await.unwrap();
    let mut row = support::rows::session_row("s1", "n1", "personal");
    row.container_name = Some(session_container.clone());
    h.store.insert_session(&row).unwrap();
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;

    let started = std::time::Instant::now();
    let view = mcp_tool(
        &h,
        &token,
        "service_start",
        json!({ "name": "browser", "wait_secs": 90 }),
    )
    .await;
    eprintln!("service_start: {view} in {:?}", started.elapsed());
    assert_eq!(view["state"], "ready", "{view}");
    assert_eq!(view["url"], "http://127.0.0.1:9222/json/version");
    // In the sandbox, not beside it, and recorded so.
    assert!(sidecars::containers(&h.store, "s1").is_empty());
    let recorded = sidecars::recorded(&h.store, "s1")
        .remove("browser")
        .unwrap();
    assert_eq!(
        recorded.sandbox.as_deref(),
        Some(session_container.as_str())
    );
    let beside = format!("{session_container}-svc-browser");
    assert!(podman(&["container", "exists", &beside]).is_none());
    let tree = podman(&["exec", &session_container, "ps", "-eo", "pid,ppid,args"]).unwrap();
    eprintln!("{tree}");
    assert!(tree.contains("/opt/tracon-services/browser/headless-shell/headless-shell"));
    if rig.cfg.runtime.podman.sandbox_tunnels {
        let links = podman(&["exec", &session_container, "ls", "/sys/class/net"]).unwrap();
        assert_eq!(links, "lo", "no network but loopback");
    }
    let status = mcp_tool(&h, &token, "service_status", json!({ "name": "browser" })).await;
    assert_eq!(status["state"], "ready", "{status}");

    let mut stdin = spawned.stdin;
    stdin.write_all(b"go\n").await.unwrap();
    let mut lines = BufReader::new(spawned.stdout).lines();
    let line = tokio::time::timeout(Duration::from_secs(60), lines.next_line())
        .await
        .expect("the screenshot in time")
        .unwrap()
        .expect("a line from the session");
    let shot: Value = serde_json::from_str(&line).unwrap();
    eprintln!("screenshot: {shot}");
    assert!(shot["text_width"].as_f64().unwrap() > 100.0, "{shot}");
    assert!(shot["png_bytes"].as_u64().unwrap() > 2_000, "{shot}");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), spawned.done)
            .await
            .expect("the session ended")
            .unwrap(),
        0
    );
    // The session's end took the browser with it.
    rig.assert_gone();

    let shown = mcp_tool(
        &h,
        &token,
        "show_work",
        json!({ "title": "The page, rendered in the session's browser", "paths": ["shot.png"] }),
    )
    .await;
    eprintln!("show_work: {shown}");
    assert!(
        shown.get("error").is_none() && shown.get("raw").is_none(),
        "{shown}"
    );
    let held = h.store.shown_work_for_session("s1").unwrap();
    assert_eq!(held.len(), 1, "{shown}");
    let files: Vec<_> = held[0]
        .row
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.size_bytes))
        .collect();
    assert_eq!(files, [("shot.png", shot["png_bytes"].as_u64().unwrap())]);
    if let Ok(dir) = std::env::var("TRACON_SERVICES_TEST_OUT") {
        let snapshot = tracon::workspace::snapshot_path("s1");
        std::fs::copy(
            snapshot.join("shot.png"),
            PathBuf::from(dir).join("shot.png"),
        )
        .unwrap();
    }
    rig.assert_gone();
}

/// Serves `dir` as a release server would: `/v<version>/<file>`. Every path
/// asked for is kept.
async fn serve_release(dir: PathBuf) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::extract::Path as AxPath;
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = asked.clone();
    let app = axum::Router::new().route(
        "/{tag}/{file}",
        axum::routing::get(move |AxPath((tag, file)): AxPath<(String, String)>| {
            let (dir, seen) = (dir.clone(), seen.clone());
            async move {
                seen.lock().unwrap().push(format!("{tag}/{file}"));
                match std::fs::read(dir.join(&tag).join(&file)) {
                    Ok(bytes) => (axum::http::StatusCode::OK, bytes),
                    Err(_) => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), asked)
}

/// `bundle = "release"`: nobody unpacks the browser. The node fetches its
/// release's browser bundle (`TRACON_BROWSER_RELEASE`, a directory holding
/// `tracon-browser-<chrome>-<arch>.tar.gz` and its `.sha256` as a release
/// attaches them, served here as this version's release), checks it, keeps
/// it under its state directory, and a session's sandbox runs Chrome from
/// it, answering CDP on its loopback.
#[tokio::test]
async fn the_release_browser_is_fetched_and_runs_in_the_session_sandbox() {
    let Some(mut rig) = Rig::new("release") else {
        return;
    };
    let assets = std::env::var("TRACON_BROWSER_RELEASE")
        .map(PathBuf::from)
        .expect("this test needs TRACON_BROWSER_RELEASE");
    let chrome = tracon::service_bundle::CHROME_VERSION;
    let arch = tracon::bundle::host_arch().unwrap();
    let served = tempfile::tempdir().unwrap();
    let tag = served
        .path()
        .join(format!("v{}", env!("CARGO_PKG_VERSION")));
    std::fs::create_dir_all(&tag).unwrap();
    for suffix in ["", ".sha256"] {
        let name = format!("tracon-browser-{chrome}-{arch}.tar.gz{suffix}");
        assert!(
            assets.join(&name).is_file(),
            "{name} in {}",
            assets.display()
        );
        std::os::unix::fs::symlink(assets.join(&name), tag.join(&name)).unwrap();
    }
    let (url, asked) = serve_release(served.path().to_path_buf()).await;
    rig.cfg.sandbox.release_url = Some(url);
    rig.cfg.service = vec![Service {
        name: "browser".into(),
        bundle: Some("release".into()),
        port: 9222,
        ready: Some("/json/version".into()),
        ..Service::default()
    }];
    tracon::config::validate_services(&rig.cfg.service).unwrap();
    assert!(tracon::service_bundle::wanted(&rig.cfg));
    let missing = tracon::service_bundle::missing(&rig.cfg, false).unwrap();
    assert!(missing.contains("not here"), "{missing}");

    // What the boundary's converge does at start, then at the next start.
    let started = std::time::Instant::now();
    assert!(tracon::service_bundle::converge(&rig.cfg).await.unwrap());
    eprintln!("fetched and checked in {:?}", started.elapsed());
    assert!(!tracon::service_bundle::converge(&rig.cfg).await.unwrap());
    assert_eq!(
        asked.lock().unwrap().len(),
        2,
        "{:?}",
        asked.lock().unwrap()
    );
    assert_eq!(tracon::service_bundle::missing(&rig.cfg, false), None);
    let fetched = tracon::service_bundle::resolve(&rig.cfg.service[0]).unwrap();
    assert_eq!(
        fetched,
        Config::state_dir().join(format!("bundles/browser/{chrome}/{arch}"))
    );
    assert!(fetched.join("run").is_file());

    // A session's sandbox, and the browser started in it.
    let runner = rig.runner(Vec::new());
    let session_container = rig.name("h");
    let mut harness = rig.command(
        "h",
        "read go && node -e \"fetch('http://127.0.0.1:9222/json/version').then(r => r.json()).then(v => console.log(JSON.stringify(v)))\"",
    );
    harness.mounts = Vec::new();
    let spawned = runner.spawn(harness).await.unwrap();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let (row, record) = session(&store, &session_container);
    let cx = sidecars::Context {
        cfg: &rig.cfg,
        store: &store,
        runner: runner.as_ref(),
        session: &row,
        record: &record,
    };
    let view = sidecars::start(&cx, "browser", Duration::from_secs(90))
        .await
        .unwrap();
    eprintln!("service_start: {view}");
    assert_eq!(view["state"], "ready", "{view}");
    let mounts = podman(&[
        "inspect",
        &session_container,
        "--format",
        "{{range .Mounts}}{{.Source}}={{.Destination}} {{end}}",
    ])
    .unwrap();
    assert!(
        mounts.contains(&format!(
            "{}=/opt/tracon-services/browser",
            fetched.display()
        )),
        "{mounts}"
    );
    let tree = podman(&["exec", &session_container, "ps", "-eo", "args"]).unwrap();
    assert!(
        tree.contains("/opt/tracon-services/browser/headless-shell/headless-shell"),
        "{tree}"
    );

    let mut stdin = spawned.stdin;
    stdin.write_all(b"go\n").await.unwrap();
    let mut lines = BufReader::new(spawned.stdout).lines();
    let line = tokio::time::timeout(Duration::from_secs(60), lines.next_line())
        .await
        .expect("CDP answered in time")
        .unwrap()
        .expect("a line from the session");
    let version: Value = serde_json::from_str(&line).unwrap();
    eprintln!("CDP on 127.0.0.1:9222: {version}");
    assert!(
        version["Browser"]
            .as_str()
            .unwrap()
            .ends_with(&format!("Chrome/{chrome}")),
        "{version}"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), spawned.done)
            .await
            .expect("the session ended")
            .unwrap(),
        0
    );
    rig.assert_gone();
}

/// A service bundle: `run` counts its starts in /tmp, fails the first
/// `fail_first` of them, then serves HTTP on its first argument.
fn flaky_bundle(fail_first: u32) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::Builder::new()
        .prefix("svc-bundle")
        .tempdir_in("/tmp")
        .unwrap();
    let run = dir.path().join("run");
    std::fs::write(
        &run,
        format!(
            "#!/bin/sh\n\
             starts=/tmp/$(basename \"$(dirname \"$0\")\")-starts\n\
             n=$(( $(cat \"$starts\" 2>/dev/null || echo 0) + 1 ))\n\
             echo \"$n\" > \"$starts\"\n\
             echo \"start $n\"\n\
             if [ \"$n\" -le {fail_first} ]; then echo \"boom $n\" >&2; exit 1; fi\n\
             exec python3 -m http.server \"$1\" --bind 127.0.0.1\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The image's own user reads it.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

#[tokio::test]
async fn a_crashing_service_restarts_by_its_policy_and_goes_with_its_sandbox() {
    let Some(mut rig) = Rig::new("restart") else {
        return;
    };
    let bundle = flaky_bundle(1);
    let service = |name: &str, port: u16, restart| Service {
        name: name.into(),
        bundle: Some(bundle.path().to_path_buf()),
        command: vec![port.to_string()],
        port,
        ready: Some("/".into()),
        restart,
        timeout_secs: 20,
        ..Service::default()
    };
    rig.cfg.service = vec![
        service("flaky", 8765, ServiceRestart::OnFailure),
        service("fragile", 8766, ServiceRestart::Never),
    ];
    let runner = rig.runner(Vec::new());
    let session_container = rig.name("h");
    let spawned = runner.spawn(rig.command("h", "read go")).await.unwrap();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let (row, record) = session(&store, &session_container);
    let cx = sidecars::Context {
        cfg: &rig.cfg,
        store: &store,
        runner: runner.as_ref(),
        session: &row,
        record: &record,
    };
    let in_sandbox = |script: &str| {
        podman(&["exec", &session_container, "sh", "-c", script]).unwrap_or_default()
    };

    // It fails its first start and its policy starts it again.
    let view = sidecars::start(&cx, "flaky", Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(view["state"], "ready", "{view}");
    assert_eq!(in_sandbox("cat /tmp/flaky-starts"), "2");

    // The same, under `never`: failed, with its last words.
    let view = sidecars::start(&cx, "fragile", Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(view["state"], "failed", "{view}");
    assert!(
        view["detail"].as_str().unwrap().contains("boom 1"),
        "{view}"
    );

    // Killed while ready, it comes back.
    // The bracket keeps the shell's own command line from matching.
    in_sandbox("pkill -f 'http[.]server 8765'");
    let mut back = false;
    for _ in 0..60 {
        if in_sandbox("cat /tmp/flaky-starts") == "3" {
            let view = sidecars::status(&cx, "flaky", Duration::from_secs(20))
                .await
                .unwrap();
            assert_eq!(view["state"], "ready", "{view}");
            back = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(back, "restarted after a crash");

    // Stopping a session's services stops them in its sandbox, which stays.
    sidecars::stop_all(runner.as_ref(), &store, "s1").await;
    assert_eq!(in_sandbox("pgrep -f 'http[.]server' || echo none"), "none");
    assert!(podman(&["container", "exists", &session_container]).is_some());

    let mut stdin = spawned.stdin;
    stdin.write_all(b"go\n").await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), spawned.done)
            .await
            .expect("the session ended")
            .unwrap(),
        0
    );
    rig.assert_gone();
}
