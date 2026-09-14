//! The tour that captures `docs/reference/opencode-v1.18.30/ui-route-trace.tsv`.
//!
//! Everything here runs only under `TRACON_UI_TRACE=1`, on a machine that has
//! the vendored bundle, the pinned binary, and a Chromium. It stands up the
//! whole arrangement on real ports — the model gateway with a fake upstream
//! behind it, the pinned binary wired to that gateway by the node's own
//! adapter, the operator router that mints the capability, and the UI origin
//! serving the pinned tree — then drives a browser through it over CDP and
//! writes down every request the browser made.
//!
//! The browser is driven with a WebSocket client written here rather than a
//! crate: CDP is one JSON object per text frame, the connection is to loopback,
//! and a dependency whose only user is a gated test is a dependency the release
//! build carries for nothing.
//!
//! ## What the tour can and cannot make happen
//!
//! A prompt typed into the native UI is **not forwarded** — the gateway hands
//! it to the session manager, so the turn is tracon's (`gateway::opencode`,
//! `Mediation::Prompt`). This tour does not stand up a supervised session, so
//! that call is recorded for what it is (mediated, and refused by the manager
//! because no actor owns the session) and the model turn is started out of
//! band, against the harness's own API with its own credential. The browser
//! then sees the real turn arrive on the real stream and answers the real
//! permission — which is the part of the path that only a browser can show.
//!
//! The steps marked `probe` in the trace are same-origin `fetch` calls made
//! **from the loaded app page**, not clicks: they carry the app's cookie and
//! the app's origin and reach the same router, and they are how the deny list
//! is asked about routes whose controls a headless DOM makes unreliable to
//! find. The steps that are not marked `probe` are the app's own traffic.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use axum::response::IntoResponse;
use axum::routing::any;
use base64::Engine;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tracon::{
    adapter::{opencode::OpenCodeAdapter, HarnessAdapter, LaunchSpec, NativeApi},
    broker::Broker,
    config::{Config, ModelDecl, Provider, SHAPE_ANTHROPIC},
    gateway::model::harness_wiring,
    http::{api::AppState, auth::AuthState, ui},
    mcp::Tools,
    runner::{Runner, RunnerCommand, RunnerError, Spawned},
    session::Manager,
    store::Store,
    stream::Bus,
};

use super::{Row, Trace};
use crate::state;
use crate::support::fake::FakeAdapter;

/// The tracon session the capability is minted for.
const SESSION: &str = "s-ui";
/// A second tracon session, so "a second session's URL" has something real to
/// refuse.
const OTHER_SESSION: &str = "s-other";
const CHANNEL: &str = "work";
const MODEL: &str = "anthropic/test-model";

/// The credential the broker holds for the fake upstream. Never reaches the
/// runner: the gateway swaps the session's placeholder for it.
const BROKER: &str = r#"
    [credentials.cred]
    kind = "api_key"
    provider = "stub"
    channels = ["work"]
    [credentials.cred.env]
    API_KEY = "real-provider-key"
"#;

// ---------------------------------------------------------------------------
// What the machine has to have
// ---------------------------------------------------------------------------

fn pinned_binary() -> Option<String> {
    let candidate = match std::env::var_os("TRACON_OPENCODE_BINARY") {
        Some(named) => {
            let path = PathBuf::from(named);
            path.is_file()
                .then(|| path.to_string_lossy().into_owned())?
        }
        None => std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join("opencode"))
            .find(|candidate| candidate.is_file())
            .map(|candidate| candidate.to_string_lossy().into_owned())?,
    };
    let reported = std::process::Command::new(&candidate)
        .arg("--version")
        .output()
        .ok()?;
    let reported = String::from_utf8_lossy(&reported.stdout).trim().to_string();
    if reported != ui::PINNED_VERSION {
        eprintln!(
            "skipped: {candidate} reports {reported}, not the pinned {}",
            ui::PINNED_VERSION
        );
        return None;
    }
    Some(candidate)
}

/// A Chromium to drive. `TRACON_CHROMIUM` names one; otherwise the usual
/// names on `PATH`, then a Playwright download, which is what a machine that
/// has ever run a browser test already has.
fn chromium() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("TRACON_CHROMIUM") {
        let path = PathBuf::from(named);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for name in ["chromium", "chromium-browser", "google-chrome", "chrome"] {
            if let Some(found) = std::env::split_paths(&paths)
                .map(|dir| dir.join(name))
                .find(|c| c.is_file())
            {
                return Some(found);
            }
        }
    }
    let cache = dirs::cache_dir()?.join("ms-playwright");
    let mut found: Vec<PathBuf> = std::fs::read_dir(cache)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join("chrome-linux64/chrome"))
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found.pop()
}

fn vendored_bundle() -> Option<Arc<ui::Bundle>> {
    let dir = std::env::var_os("TRACON_OPENCODE_UI_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::state_dir()
                .or_else(dirs::data_local_dir)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("tracon/opencode-ui")
        });
    match ui::Bundle::load(&dir) {
        Ok(bundle) => Some(Arc::new(bundle)),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The fake upstream model
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<String>>>,
}

/// One short answer with one tool call, then plain text once the tool has run.
///
/// The tool call is the point: OpenCode's ruleset is all-`ask` (finding 1), so
/// running `bash` raises a permission request, which is what puts the native
/// UI's permission control on the screen and its reply route on the trace.
fn anthropic_turn(with_tool: bool) -> String {
    let head = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":18,\"output_tokens\":2}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Reading the tree.\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    let tool = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"bash\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"echo tracon\\\",\\\"description\\\":\\\"say hello\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    );
    let stop = if with_tool { "tool_use" } else { "end_turn" };
    format!(
        "{head}{}event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop}\",\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":18,\"output_tokens\":9}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        if with_tool { tool } else { "" }
    )
}

async fn start_upstream(upstream: Upstream) -> u16 {
    let calls = Arc::new(Mutex::new(0usize));
    let app = axum::Router::new().fallback(any(move |req: Request<Body>| {
        let upstream = upstream.clone();
        let calls = calls.clone();
        async move {
            let (parts, body) = req.into_parts();
            let _ = axum::body::to_bytes(body, 1 << 22).await;
            let path = parts.uri.path().to_string();
            upstream
                .seen
                .lock()
                .unwrap()
                .push(format!("{} {path}", parts.method));
            if path.ends_with("/models") {
                return (
                    [("content-type", "application/json")],
                    json!({ "data": [] }).to_string(),
                )
                    .into_response();
            }
            let nth = {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            (
                [("content-type", "text/event-stream")],
                anthropic_turn(nth == 1),
            )
                .into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

// ---------------------------------------------------------------------------
// The node
// ---------------------------------------------------------------------------

struct Node {
    cfg: Arc<Config>,
    manager: Manager,
    store: Arc<Store>,
    state: AppState,
    token: String,
    upstream: Upstream,
}

async fn start_node(bundle: Arc<ui::Bundle>, operator_origin: &str) -> Node {
    let upstream = Upstream::default();
    let upstream_port = start_upstream(upstream.clone()).await;

    // The gateway's port has to be known before the configuration that names
    // it, because the harness is wired from that configuration.
    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let forward_port = gateway_listener.local_addr().unwrap().port();

    let mut cfg = Config::default();
    cfg.gateway.forward_port = forward_port;
    cfg.gateway.allow_hosts = vec![r"^127\.0\.0\.1$".into(), r"^localhost$".into()];
    cfg.providers.clear();
    cfg.providers.insert(
        "anthropic".into(),
        Provider {
            credential: "cred".into(),
            upstream: format!("http://127.0.0.1:{upstream_port}"),
            shape: SHAPE_ANTHROPIC.into(),
            models: vec![ModelDecl {
                id: "test-model".into(),
                context: 200_000,
                output: 64_000,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let cfg = Arc::new(cfg);

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let broker: Broker = toml::from_str(BROKER).unwrap();
    let tools = Arc::new(Tools {
        broker: broker.shared(),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg.clone(),
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let state = AppState {
        manager: manager.clone(),
        cfg: cfg.clone(),
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: Arc::new(AuthState::load(&store, "127.0.0.1".into())),
        enroll: Default::default(),
    };

    // The model gateway the harness talks to.
    let harness_app = tracon::http::harness_router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(gateway_listener, harness_app).await;
    });

    // The UI origin, on its configured port, serving the pinned tree.
    let ui_state = ui::UiState::new(state.clone(), Some(bundle), operator_origin);
    let ui_app = ui::router(ui_state);
    let ui_listener = tokio::net::TcpListener::bind(cfg.ui.opencode_listen)
        .await
        .expect("the UI origin's port is free");
    tokio::spawn(async move {
        let _ = axum::serve(ui_listener, ui_app).await;
    });

    for id in [SESSION, OTHER_SESSION] {
        let mut row = crate::support::rows::session_row(id, "n1", CHANNEL);
        row.harness_id = "opencode".into();
        row.model = MODEL.into();
        row.turn_active = 1;
        store.insert_session(&row).unwrap();
    }
    let token = manager.register_tool_token_for_test(SESSION, CHANNEL).await;

    Node {
        cfg,
        manager,
        store,
        state,
        token,
        upstream,
    }
}

// ---------------------------------------------------------------------------
// The pinned binary, launched by the node's own adapter
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct LiveRunner {
    binary: String,
    launched: Arc<Mutex<Option<(String, String)>>>,
    log: Arc<Mutex<String>>,
}

struct Tee {
    inner: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    log: Arc<Mutex<String>>,
}

impl tokio::io::AsyncRead for Tee {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut this.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &polled {
            let fresh = String::from_utf8_lossy(&buf.filled()[before..]).into_owned();
            this.log.lock().unwrap().push_str(&fresh);
        }
        polled
    }
}

#[async_trait::async_trait]
impl Runner for LiveRunner {
    async fn spawn(&self, mut cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        cmd.argv[0] = self.binary.clone();
        let password = cmd
            .env
            .iter()
            .find(|(name, _)| name == "OPENCODE_SERVER_PASSWORD")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        let mut spawned = tracon::runner::local::LocalRunner.spawn(cmd).await?;
        spawned.stdout = Box::new(Tee {
            inner: spawned.stdout,
            log: self.log.clone(),
        });
        if let Some(endpoint) = spawned.endpoint.clone() {
            *self.launched.lock().unwrap() = Some((endpoint, password));
        }
        Ok(spawned)
    }

    async fn run_capture(
        &self,
        mut cmd: RunnerCommand,
    ) -> Result<std::process::Output, RunnerError> {
        cmd.argv[0] = self.binary.clone();
        cmd.workdir = None;
        tracon::runner::local::LocalRunner.run_capture(cmd).await
    }

    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        tracon::runner::local::LocalRunner.kill(name).await
    }
}

struct Live {
    handle: Box<dyn tracon::adapter::HarnessHandle>,
    endpoint: String,
    password: String,
    work: PathBuf,
    root: PathBuf,
    container: String,
    runner: LiveRunner,
    http: reqwest::Client,
}

impl Live {
    fn credential(&self) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{}", self.password))
        )
    }

    fn base(&self) -> String {
        format!("http://{}", self.endpoint)
    }

    fn native_api(&self, session: &str) -> NativeApi {
        NativeApi {
            base: self.base(),
            authorization: self.credential(),
            directory: self.work.to_string_lossy().into_owned(),
            session_id: session.into(),
        }
    }

    /// Ask the harness something directly, with the node's own credential and
    /// the directory pinned, exactly as the gateway would.
    async fn ask(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let url = format!(
            "{}{path}{}directory={}",
            self.base(),
            if path.contains('?') { "&" } else { "?" },
            self.work.to_string_lossy()
        );
        let mut req = self
            .http
            .request(method.parse().unwrap(), url)
            .header("authorization", self.credential());
        if let Some(body) = body {
            req = req.json(&body);
        }
        match req.send().await {
            Ok(r) => {
                let status = r.status().as_u16();
                let text = r.text().await.unwrap_or_default();
                (
                    status,
                    serde_json::from_str(&text).unwrap_or(Value::String(text)),
                )
            }
            Err(e) => (0, Value::String(e.to_string())),
        }
    }

    async fn catalogue_settled(&self, model: &str) -> bool {
        let wanted = model.split_once('/').map(|(_, id)| id).unwrap_or(model);
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let (_, listed) = self.ask("GET", "/api/model", None).await;
            if listed.to_string().contains(wanted) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        false
    }

    fn log(&self) -> String {
        self.runner.log.lock().unwrap().clone()
    }

    async fn shutdown(self) {
        self.handle.close().await.ok();
        self.runner.kill(&self.container).await.ok();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn launch(node: &Node, binary: &str) -> Live {
    let root = state::scratch("ui-route-trace");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // A file to have a diff about, so the changes view has something to show.
    std::fs::write(work.join("README.md"), "# tracon route trace\n").unwrap();

    let wiring = harness_wiring(&node.cfg, "127.0.0.1", &node.token, |_, _| true);
    let state_dir = root.join(".opencode");
    std::fs::create_dir_all(state_dir.join("run")).unwrap();
    for (file, body) in OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION).scratch_files(&wiring)
    {
        std::fs::write(state_dir.join(&file), body).unwrap();
    }
    let container = format!("tracon-ui-trace-{}", std::process::id());
    let runner = LiveRunner {
        binary: binary.to_string(),
        launched: Arc::new(Mutex::new(None)),
        log: Arc::new(Mutex::new(String::new())),
    };
    let spec = LaunchSpec {
        cwd_in_runner: work.to_string_lossy().into_owned(),
        model: MODEL.into(),
        container_name: container.clone(),
        harness_home: root.to_string_lossy().into_owned(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        env: Vec::new(),
        system_prompt_file: None,
        cursor: None,
    };
    let (handle, _rx) = OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION)
        .launch(&runner, spec)
        .await
        .expect("the pinned binary starts");
    let (endpoint, password) = runner
        .launched
        .lock()
        .unwrap()
        .clone()
        .expect("the launch recorded its endpoint");
    Live {
        handle,
        endpoint,
        password,
        work,
        root,
        container,
        runner,
        http: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap(),
    }
}

// ---------------------------------------------------------------------------
// A CDP client
// ---------------------------------------------------------------------------

/// A WebSocket carrying one JSON object per text frame, which is all CDP is.
struct Cdp {
    stream: tokio::net::TcpStream,
    pending: Vec<u8>,
    next_id: i64,
    events: Vec<Value>,
}

impl Cdp {
    async fn connect(url: &str) -> std::io::Result<Self> {
        let rest = url.strip_prefix("ws://").unwrap_or(url);
        let (authority, path) = match rest.split_once('/') {
            Some((a, p)) => (a.to_string(), format!("/{p}")),
            None => (rest.to_string(), "/".to_string()),
        };
        let mut stream = tokio::net::TcpStream::connect(&authority).await?;
        // A constant key: the handshake's nonce guards caches and proxies, and
        // there is neither between here and a loopback DevTools endpoint.
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let n = stream.read(&mut byte).await?;
            if n == 0 {
                return Err(std::io::Error::other("the DevTools endpoint closed"));
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        if !head.starts_with("HTTP/1.1 101") {
            return Err(std::io::Error::other(format!(
                "the DevTools endpoint refused the upgrade: {}",
                head.lines().next().unwrap_or_default()
            )));
        }
        Ok(Self {
            stream,
            pending: Vec::new(),
            next_id: 1,
            events: Vec::new(),
        })
    }

    async fn send_text(&mut self, text: &str) -> std::io::Result<()> {
        let payload = text.as_bytes();
        let mut frame = vec![0x81u8];
        let mask = [0x27u8, 0x1d, 0x5c, 0x0b];
        match payload.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n if n <= u16::MAX as usize => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame).await
    }

    /// One message, or `None` if nothing arrived before `deadline`.
    async fn read_message(&mut self, deadline: Instant) -> Option<Value> {
        let mut assembled = Vec::new();
        loop {
            let (fin, opcode, payload) = self.read_frame(deadline).await?;
            match opcode {
                0x9 => {
                    // A pong, so the endpoint does not decide we are gone.
                    let mut frame = vec![0x8Au8, 0x80 | payload.len() as u8, 0, 0, 0, 0];
                    frame.extend_from_slice(&payload);
                    let _ = self.stream.write_all(&frame).await;
                    continue;
                }
                0x8 => return None,
                0xA => continue,
                _ => {}
            }
            assembled.extend_from_slice(&payload);
            if fin {
                return serde_json::from_slice(&assembled).ok();
            }
        }
    }

    async fn read_frame(&mut self, deadline: Instant) -> Option<(bool, u8, Vec<u8>)> {
        let header = self.take(2, deadline).await?;
        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let len = match header[1] & 0x7f {
            126 => {
                let n = self.take(2, deadline).await?;
                u16::from_be_bytes([n[0], n[1]]) as usize
            }
            127 => {
                let n = self.take(8, deadline).await?;
                u64::from_be_bytes(n.try_into().ok()?) as usize
            }
            n => n as usize,
        };
        let mask = if masked {
            Some(self.take(4, deadline).await?)
        } else {
            None
        };
        let mut payload = self.take(len, deadline).await?;
        if let Some(mask) = mask {
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[i % 4];
            }
        }
        Some((fin, opcode, payload))
    }

    async fn take(&mut self, n: usize, deadline: Instant) -> Option<Vec<u8>> {
        while self.pending.len() < n {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let mut buf = vec![0u8; 64 << 10];
            let read = tokio::time::timeout(deadline - now, self.stream.read(&mut buf))
                .await
                .ok()?
                .ok()?;
            if read == 0 {
                return None;
            }
            self.pending.extend_from_slice(&buf[..read]);
        }
        Some(self.pending.drain(..n).collect())
    }

    /// Send a command and wait for its answer, keeping every event that
    /// arrives in the meantime.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({ "id": id, "method": method, "params": params }).to_string();
        if let Err(e) = self.send_text(&message).await {
            eprintln!("cdp: sending {method}: {e}");
            return Value::Null;
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Some(message) = self.read_message(deadline).await {
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(error) = message.get("error") {
                    eprintln!("cdp: {method} answered {error}");
                }
                return message.get("result").cloned().unwrap_or(Value::Null);
            }
            if message.get("method").is_some() {
                self.events.push(message);
            }
        }
        eprintln!("cdp: {method} never answered");
        Value::Null
    }

    /// Collect events for a while, so the page's own traffic is seen.
    async fn settle(&mut self, how_long: Duration) {
        let deadline = Instant::now() + how_long;
        while Instant::now() < deadline {
            match self.read_message(deadline).await {
                Some(message) if message.get("method").is_some() => self.events.push(message),
                Some(_) => {}
                None => return,
            }
        }
    }

    /// Run an expression in the page and return its value.
    async fn eval(&mut self, expression: &str) -> Value {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true,
                    "userGesture": true,
                }),
            )
            .await;
        result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null)
    }
}

/// Chromium, headless, with a throwaway profile and a DevTools endpoint.
struct Browser {
    child: std::process::Child,
    port: u16,
    profile: PathBuf,
}

impl Browser {
    async fn launch(binary: &Path, profile: PathBuf) -> Option<Self> {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
            l.local_addr().ok()?.port()
        };
        let _ = std::fs::create_dir_all(&profile);
        let child = std::process::Command::new(binary)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-sandbox",
                "--disable-dev-shm-usage",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                &format!("--remote-debugging-port={port}"),
                &format!("--user-data-dir={}", profile.display()),
                "about:blank",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        Some(Self {
            child,
            port,
            profile,
        })
    }

    /// The page target's debugger URL, once the endpoint is up.
    async fn page(&self) -> Option<String> {
        let http = reqwest::Client::builder().no_proxy().build().ok()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(r) = http
                .get(format!("http://127.0.0.1:{}/json/list", self.port))
                .send()
                .await
            {
                if let Ok(targets) = r.json::<Vec<Value>>().await {
                    if let Some(url) = targets
                        .iter()
                        .find(|t| t["type"] == "page")
                        .and_then(|t| t["webSocketDebuggerUrl"].as_str())
                    {
                        return Some(url.to_string());
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        None
    }

    fn close(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

// ---------------------------------------------------------------------------
// What the browser did
// ---------------------------------------------------------------------------

/// One request the browser made, as CDP reported it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    status: u16,
}

/// Turn CDP's network events into requests with their answers, on this origin
/// only. A request with no response never happened as far as the trace is
/// concerned; the tour prints them so a silent failure is not silent.
fn requests(events: &[Value], origin: &str) -> Vec<Seen> {
    let mut sent: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut out = Vec::new();
    let mut dangling = Vec::new();
    for event in events {
        let params = &event["params"];
        match event["method"].as_str() {
            Some("Network.requestWillBeSent") => {
                let (Some(id), Some(url), Some(method)) = (
                    params["requestId"].as_str(),
                    params["request"]["url"].as_str(),
                    params["request"]["method"].as_str(),
                ) else {
                    continue;
                };
                if !url.starts_with(origin) {
                    continue;
                }
                let path = url[origin.len()..]
                    .split(['?', '#'])
                    .next()
                    .unwrap_or("/")
                    .to_string();
                let path = if path.is_empty() { "/".into() } else { path };
                sent.insert(id.to_string(), (method.to_string(), path));
            }
            Some("Network.responseReceived") => {
                let Some(id) = params["requestId"].as_str() else {
                    continue;
                };
                if let Some((method, path)) = sent.remove(id) {
                    out.push(Seen {
                        method,
                        path,
                        status: params["response"]["status"].as_u64().unwrap_or(0) as u16,
                    });
                }
            }
            Some("Network.loadingFailed") => {
                if let Some(id) = params["requestId"].as_str() {
                    if let Some((method, path)) = sent.remove(id) {
                        dangling.push(format!(
                            "{method} {path}: {}",
                            params["errorText"].as_str().unwrap_or("failed")
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    for (method, path) in sent.into_values() {
        dangling.push(format!("{method} {path}: no response"));
    }
    if !dangling.is_empty() {
        eprintln!("requests with no answer: {dangling:?}");
    }
    out
}

/// Rewrite the opaque ids so two captures of the same tour are the same file.
///
/// The two session ids are named apart rather than both becoming `ses_x`: the
/// whole point of the second-session step is that a capability for one session
/// is refused for the other, and a trace that conflated them would record the
/// refusal against the same row as the success.
fn stable(path: &str, mine: &str, theirs: &str) -> String {
    let mut out = Vec::new();
    for segment in path.split('/') {
        let replaced = if segment == mine {
            Some("ses_x".to_string())
        } else if segment == theirs {
            Some("ses_other".to_string())
        } else {
            Trace::PLACEHOLDERS
                .iter()
                .find(|(prefix, _)| segment.starts_with(prefix) && segment.len() > prefix.len())
                .map(|(_, with)| (*with).to_string())
        };
        out.push(replaced.unwrap_or_else(|| segment.to_string()));
    }
    out.join("/")
}

// ---------------------------------------------------------------------------
// The tour
// ---------------------------------------------------------------------------

/// A probe made from the loaded page: same origin, same cookie, same router.
fn probe(method: &str, path: &str, body: &str) -> String {
    format!(
        "(async () => {{ try {{ const r = await fetch({path:?}, {{ method: {method:?}, \
         headers: {{ 'content-type': 'application/json' }}{} }}); return r.status }} \
         catch (e) {{ return String(e) }} }})()",
        if method == "GET" {
            String::new()
        } else {
            format!(", body: {body:?}")
        }
    )
}

pub async fn run(trace_path: &Path) {
    state::isolate();
    let Some(binary) = pinned_binary() else {
        eprintln!(
            "skipped: the pinned OpenCode binary is not on this machine. Put it on PATH as \
             `opencode`, or name it in TRACON_OPENCODE_BINARY."
        );
        return;
    };
    let Some(chromium) = chromium() else {
        eprintln!("skipped: no Chromium on this machine; name one in TRACON_CHROMIUM");
        return;
    };
    let Some(bundle) = vendored_bundle() else {
        return;
    };
    let digest = bundle.digest().to_string();

    let operator_origin = "http://127.0.0.1:7420";
    let node = start_node(bundle, operator_origin).await;
    let origin = node.cfg.ui.opencode_origin().unwrap();
    let live = launch(&node, &binary).await;
    eprintln!("harness at {} in {}", live.base(), live.work.display());
    if !live.catalogue_settled(MODEL).await {
        eprintln!("the harness never listed {MODEL}; the turn will not run");
    }

    // Two real harness sessions, made out of band: the gateway forbids session
    // creation on purpose, and what is under test is the origin.
    let mut harness_sessions = Vec::new();
    for title in ["tracon route trace", "another session"] {
        let (status, created) = live
            .ask("POST", "/api/session", Some(json!({ "title": title })))
            .await;
        let id = created["data"]["id"]
            .as_str()
            .or_else(|| created["id"].as_str())
            .unwrap_or_else(|| panic!("no session id in {status} {created}"))
            .to_string();
        harness_sessions.push(id);
    }
    let mine = harness_sessions[0].clone();
    let theirs = harness_sessions[1].clone();
    eprintln!("harness sessions {mine} and {theirs}");

    node.manager
        .register_native_api_for_test(SESSION, live.native_api(&mine))
        .await;
    node.manager
        .register_native_api_for_test(OTHER_SESSION, live.native_api(&theirs))
        .await;

    // The capability, minted the way "Open in OpenCode" mints it.
    let minted = match ui::open(
        axum::extract::State(node.state.clone()),
        axum::http::HeaderMap::new(),
        axum::extract::Path(SESSION.to_string()),
    )
    .await
    {
        Ok(minted) => minted.0,
        Err(_) => panic!("the operator route would not mint a capability"),
    };
    let boot_url = minted["url"].as_str().expect("a url").to_string();

    let Some(browser) = Browser::launch(&chromium, live.root.join("chromium")).await else {
        eprintln!("skipped: Chromium would not start");
        live.shutdown().await;
        return;
    };
    let Some(page) = browser.page().await else {
        eprintln!("skipped: Chromium's DevTools endpoint never came up");
        browser.close();
        live.shutdown().await;
        return;
    };
    let Ok(mut cdp) = Cdp::connect(&page).await else {
        eprintln!("skipped: could not attach to Chromium");
        browser.close();
        live.shutdown().await;
        return;
    };
    cdp.call("Network.enable", json!({})).await;
    cdp.call("Page.enable", json!({})).await;
    cdp.call("Runtime.enable", json!({})).await;

    let mut steps: Vec<(&str, usize)> = Vec::new();
    let mark = |cdp: &Cdp, steps: &mut Vec<(&'static str, usize)>, name: &'static str| {
        steps.push((name, cdp.events.len()));
    };

    // --- load -------------------------------------------------------------
    mark(&cdp, &mut steps, "load");
    cdp.call("Page.navigate", json!({ "url": boot_url })).await;
    cdp.settle(Duration::from_secs(12)).await;
    let landed = cdp.eval("location.pathname").await;
    eprintln!("the page landed on {landed}");

    // --- prompt -----------------------------------------------------------
    // Typed into the app's own composer, with real key events. The gateway
    // mediates it rather than forwarding it, so what this records is the
    // route and the class; the turn itself is started out of band below.
    mark(&cdp, &mut steps, "prompt");
    cdp.eval(
        "(() => { const t = document.querySelector('textarea') \
         || document.querySelector('[contenteditable=true]'); if (t) { t.focus(); return true } \
         return false })()",
    )
    .await;
    cdp.call(
        "Input.insertText",
        json!({ "text": "read README.md and say hello" }),
    )
    .await;
    for kind in ["keyDown", "keyUp"] {
        cdp.call(
            "Input.dispatchKeyEvent",
            json!({
                "type": kind,
                "key": "Enter",
                "code": "Enter",
                "windowsVirtualKeyCode": 13,
                "nativeVirtualKeyCode": 13,
            }),
        )
        .await;
    }
    cdp.settle(Duration::from_secs(6)).await;

    // The turn the browser watches, started against the harness's own API.
    let (status, _) = live
        .ask(
            "POST",
            &format!("/api/session/{mine}/prompt"),
            Some(json!({
                "model": { "providerID": "anthropic", "modelID": "test-model" },
                "prompt": { "text": "read README.md and say hello" },
            })),
        )
        .await;
    eprintln!("out-of-band prompt: {status}");

    // --- permission -------------------------------------------------------
    mark(&cdp, &mut steps, "permission");
    let mut permission = None;
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        cdp.settle(Duration::from_secs(2)).await;
        let (_, pending) = live
            .ask("GET", &format!("/api/session/{mine}/permission"), None)
            .await;
        if let Some(id) = first_id(&pending) {
            permission = Some(id);
            break;
        }
    }
    match &permission {
        Some(id) => {
            eprintln!("the harness raised permission {id}");
            // The app's own control first: a button offering to allow it.
            let clicked = cdp
                .eval(
                    "(() => { const b = [...document.querySelectorAll('button')] \
                     .find(b => /allow|approve|accept|once|yes/i.test(b.textContent || '')); \
                     if (b) { b.click(); return b.textContent } return null })()",
                )
                .await;
            cdp.settle(Duration::from_secs(4)).await;
            let answered = cdp
                .events
                .iter()
                .any(|e| e.to_string().contains("/permission/"));
            if clicked.is_null() || !answered {
                eprintln!("no permission control found; answering through the same route");
                mark(&cdp, &mut steps, "permission-probe");
                // `always` on purpose: the gateway narrows it to `once` and
                // records the attempted broadening (finding 2), and the reply
                // that reaches the harness is the narrowed one.
                let status = cdp
                    .eval(&probe(
                        "POST",
                        &format!("/api/session/{mine}/permission/{id}/reply"),
                        "{\"reply\":\"always\"}",
                    ))
                    .await;
                eprintln!("permission reply: {status}");
                cdp.settle(Duration::from_secs(4)).await;
            }
        }
        None => eprintln!("no permission was raised; the trace will not carry that route"),
    }
    cdp.settle(Duration::from_secs(6)).await;

    // --- the views the app has --------------------------------------------
    // Each of these is clicked in the app first and then asked for directly.
    // The click is the app's own traffic; the probes are how the routes those
    // controls use are reached when the control cannot be found in a headless
    // DOM, and they are recorded under their own step name so the trace never
    // passes one off as the other.
    mark(&cdp, &mut steps, "diff");
    click(&mut cdp, "/diff|changes|review/i").await;
    mark(&cdp, &mut steps, "diff-probe");
    probe_all(
        &mut cdp,
        &[
            ("GET", format!("/session/{mine}/diff"), ""),
            ("GET", "/file/status".to_string(), ""),
            ("GET", "/vcs/status".to_string(), ""),
            ("GET", "/vcs/diff".to_string(), ""),
        ],
    )
    .await;

    mark(&cdp, &mut steps, "model");
    click(&mut cdp, "/model/i").await;
    mark(&cdp, &mut steps, "model-probe");
    probe_all(
        &mut cdp,
        &[
            ("GET", "/api/model".to_string(), ""),
            ("GET", "/config/providers".to_string(), ""),
            (
                "POST",
                format!("/api/session/{mine}/model"),
                "{\"providerID\":\"anthropic\",\"modelID\":\"test-model\"}",
            ),
        ],
    )
    .await;

    mark(&cdp, &mut steps, "settings");
    click(&mut cdp, "/settings|preferences/i").await;
    mark(&cdp, &mut steps, "settings-probe");
    probe_all(
        &mut cdp,
        &[
            ("GET", "/config".to_string(), ""),
            ("GET", "/api/agent".to_string(), ""),
            ("GET", "/api/command".to_string(), ""),
            ("GET", "/skill".to_string(), ""),
        ],
    )
    .await;

    // --- the deny list, asked from the page ------------------------------
    mark(&cdp, &mut steps, "share-probe");
    probe_all(
        &mut cdp,
        &[
            ("POST", format!("/session/{mine}/share"), "{}"),
            ("DELETE", format!("/session/{mine}/share"), "{}"),
        ],
    )
    .await;

    mark(&cdp, &mut steps, "fork-probe");
    probe_all(
        &mut cdp,
        &[
            ("POST", format!("/session/{mine}/fork"), "{}"),
            ("POST", format!("/session/{mine}/init"), "{}"),
        ],
    )
    .await;

    mark(&cdp, &mut steps, "config-write-probe");
    probe_all(
        &mut cdp,
        &[
            ("PATCH", "/config".into(), "{\"plugin\":[\"anything\"]}"),
            (
                "PATCH",
                "/global/config".into(),
                "{\"plugin\":[\"anything\"]}",
            ),
            (
                "PATCH",
                format!("/session/{mine}"),
                "{\"permission\":{\"bash\":\"allow\"}}",
            ),
            ("POST", "/pty".into(), "{\"command\":\"sh\"}"),
        ],
    )
    .await;

    // --- a second session's URL -------------------------------------------
    mark(&cdp, &mut steps, "second-session");
    let other = ui::session_route(&origin, &theirs);
    cdp.call(
        "Page.navigate",
        json!({ "url": format!("{origin}{other}") }),
    )
    .await;
    cdp.settle(Duration::from_secs(10)).await;
    mark(&cdp, &mut steps, "second-session-probe");
    probe_all(
        &mut cdp,
        &[
            ("GET", format!("/api/session/{theirs}"), ""),
            ("POST", format!("/session/{theirs}/prompt_async"), "{}"),
        ],
    )
    .await;

    // --- what nobody owns --------------------------------------------------
    mark(&cdp, &mut steps, "unowned-probe");
    probe_all(
        &mut cdp,
        &[
            ("GET", "/nope".into(), ""),
            ("GET", "/assets/not-a-file.js".into(), ""),
            ("GET", "/index.html".into(), ""),
            ("POST", "/nope".into(), "{}"),
        ],
    )
    .await;

    steps.push(("end", cdp.events.len()));

    // ----------------------------------------------------------------------
    // Write it down
    // ----------------------------------------------------------------------
    let refused = refusals(&node, &mine, &theirs);
    let mut rows: BTreeMap<(String, String, u16), Vec<String>> = BTreeMap::new();
    for window in steps.windows(2) {
        let (name, from) = window[0];
        let (_, to) = window[1];
        for seen in requests(&cdp.events[from..to], &origin) {
            let path = stable(&seen.path, &mine, &theirs);
            rows.entry((seen.method.clone(), path, seen.status))
                .or_default()
                .push(name.to_string());
        }
    }

    let mut trace = Trace::default();
    for ((method, path, status), steps) in rows {
        let class = classify(&method, &path, status);
        // Recorded wherever the node wrote one, not only where the class is a
        // refusal: a `readable` route refused because it named another
        // session's id is refused for a reason the operator can read, and the
        // trace should say so.
        let evidence = if refused.contains(&(method.clone(), path.clone())) {
            "gateway_refused".to_string()
        } else {
            "-".to_string()
        };
        let count = steps.len();
        let mut named: Vec<String> = steps;
        named.dedup();
        trace.rows.push(Row {
            method,
            path,
            status,
            class,
            count,
            steps: named.join(","),
            evidence,
        });
    }
    trace.rows.sort();

    trace
        .headers
        .insert("binary-version".into(), ui::PINNED_VERSION.into());
    trace.headers.insert("bundle-digest".into(), digest);
    trace
        .headers
        .insert("bundle-version".into(), ui::PINNED_VERSION.into());
    trace.headers.insert("origin".into(), origin.clone());
    trace.headers.insert(
        "permission".into(),
        permission.unwrap_or_else(|| "none raised".into()),
    );
    trace.headers.insert(
        "steps".into(),
        steps
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| *name != "end")
            .collect::<Vec<_>>()
            .join(","),
    );
    trace.headers.insert(
        "upstream-calls".into(),
        node.upstream.seen.lock().unwrap().join(" | "),
    );

    std::fs::write(trace_path, trace.render()).expect("the trace is written");
    eprintln!(
        "wrote {} rows to {}",
        trace.rows.len(),
        trace_path.display()
    );
    for line in live
        .log()
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .iter()
        .rev()
    {
        eprintln!("harness: {line}");
    }

    browser.close();
    live.shutdown().await;
}

/// The class that answered, decided by the same two functions the check
/// re-derives it with — so the trace records the router's own reading rather
/// than the tour's opinion of it.
fn classify(method: &str, path: &str, status: u16) -> String {
    match ui::trace_case(None, method, path) {
        ui::trace::PAGE => ui::trace::PAGE.into(),
        ui::trace::BOOT => ui::trace::BOOT.into(),
        ui::trace::API => tracon::gateway::opencode::trace_class(method, path).into(),
        // Nothing but the bundle could have answered a 200 here.
        _ if status == 200 => ui::trace::ASSET.into(),
        _ if status == 404 => ui::trace::NONE.into(),
        _ => tracon::gateway::opencode::trace::UNKNOWN.into(),
    }
}

/// Every (method, path) the node wrote a `gateway_refused` for.
fn refusals(node: &Node, mine: &str, theirs: &str) -> std::collections::BTreeSet<(String, String)> {
    let mut out = std::collections::BTreeSet::new();
    for session in [SESSION, OTHER_SESSION] {
        for event in node
            .store
            .events_after(session, 0, 2000)
            .unwrap_or_default()
        {
            if event.kind != "gateway_refused" {
                continue;
            }
            let payload = &event.payload;
            if let (Some(method), Some(path)) =
                (payload["method"].as_str(), payload["path"].as_str())
            {
                out.insert((method.to_string(), stable(path, mine, theirs)));
            }
        }
    }
    out
}

fn first_id(pending: &Value) -> Option<String> {
    fn walk(value: &Value) -> Option<String> {
        match value {
            Value::Object(map) => {
                if let Some(id) = map.get("id").and_then(Value::as_str) {
                    if id.starts_with("per_") {
                        return Some(id.to_string());
                    }
                }
                map.values().find_map(walk)
            }
            Value::Array(items) => items.iter().find_map(walk),
            _ => None,
        }
    }
    walk(pending)
}

/// Click the app's own control, when a headless DOM has one to find.
async fn click(cdp: &mut Cdp, pattern: &str) {
    let clicked = cdp
        .eval(&format!(
            "(() => {{ const b = [...document.querySelectorAll('button,a,[role=button]')] \
             .find(e => {pattern}.test(e.textContent || e.getAttribute('aria-label') || '')); \
             if (b) {{ b.click(); return b.textContent || 'clicked' }} return null }})()"
        ))
        .await;
    cdp.settle(Duration::from_secs(4)).await;
    match clicked {
        Value::Null => eprintln!("no control matching {pattern}"),
        text => eprintln!("clicked {text}"),
    }
}

/// Ask for a list of routes from the loaded page: same origin, same cookie,
/// same router as everything the app itself sends.
async fn probe_all(cdp: &mut Cdp, routes: &[(&str, String, &str)]) {
    for (method, path, body) in routes {
        let status = cdp.eval(&probe(method, path, body)).await;
        eprintln!("probe {method} {path} -> {status}");
    }
    cdp.settle(Duration::from_secs(3)).await;
}
