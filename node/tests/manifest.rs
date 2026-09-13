//! The launch manifest end to end: importing a skill through the interface,
//! what the node refuses, what a launch renders, and — against the pinned
//! binary — what the harness actually loads.
//!
//! The through-line of every case here is that OpenCode's own answers are not
//! good enough. It resolves a duplicate skill name by overwriting one of them
//! and which one wins varies run to run; it fetches `skills.urls` over plain
//! HTTP with no off-switch; it follows symlinks in every scan; it resolves a
//! plugin by a bare existence check with no integrity verification. Each of
//! those is refused on the node, before a session exists, and each refusal has
//! a test.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use support::fake::FakeAdapter;
use tracon::{
    adapter::{opencode::OpenCodeAdapter, HarnessAdapter, LaunchSpec},
    broker::Broker,
    config::Config,
    http::{
        api::AppState,
        auth::{self, AuthState},
    },
    manifest,
    mcp::Tools,
    runner::{Runner, RunnerCommand, RunnerError, Spawned},
    session::Manager,
    store::Store,
    stream::Bus,
};

const LOCAL: &str = "127.0.0.1:5000";
const REMOTE: &str = "203.0.113.7:5000";

struct Node {
    app: axum::Router,
    store: Arc<Store>,
}

fn node() -> Node {
    node_with(Config::default())
}

/// A node running a particular configuration. The manifest pane builds from
/// the *running* node's config rather than node.toml, so a test about
/// `[launch]` has to start a node with it rather than write the file.
fn node_with(cfg: Config) -> Node {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let cfg = Arc::new(cfg);
    let tools = Arc::new(Tools {
        broker: Broker::default().shared(),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
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
        manager,
        cfg,
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
    let app = tracon::http::router(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, auth::guard));
    Node { app, store }
}

async fn call(
    n: &Node,
    method: &str,
    uri: &str,
    peer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:7420");
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let mut req = b
        .body(match body {
            Some(v) => Body::from(v.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    if let Some(p) = peer {
        let addr: SocketAddr = p.parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
    }
    let res = n.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A skill package on disk, under this test's own scratch.
fn package(dir: &Path, name: &str, body: &str) -> PathBuf {
    let package = dir.join(name);
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: A test skill\n---\n\n{body}\n"),
    )
    .unwrap();
    package
}

// ---------------------------------------------------------------------------
// Through the interface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_skill_is_imported_shown_and_removed() {
    let n = node();
    let root = state::scratch("manifest-import");
    let dir = package(&root, "release-notes", "Write the notes.");

    let (s, v) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "channel": "work", "source": dir.to_string_lossy() })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "release-notes");
    assert_eq!(v["files"], json!(["SKILL.md"]));
    // Never silently: what the operator is told is that this is code.
    assert!(
        v["warnings"][0]
            .as_str()
            .unwrap_or_default()
            .contains("Skill content is code"),
        "{v}"
    );
    // And that nothing running changed.
    assert_eq!(v["applies_at"], "next launch");

    let (s, v) = call(&n, "GET", "/api/manifest?channel=work", Some(LOCAL), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["name"], "release-notes");
    assert_eq!(v["items"][0]["kind"], "skill");
    assert_eq!(v["next"]["skills"][0]["name"], "release-notes");
    assert!(v["next"]["digest"].as_str().is_some_and(|d| d.len() == 64));
    assert!(v["error"].is_null(), "{v}");

    // Another channel is another manifest.
    let (_, other) = call(
        &n,
        "GET",
        "/api/manifest?channel=personal",
        Some(LOCAL),
        None,
    )
    .await;
    assert_eq!(other["items"], json!([]));
    assert_ne!(other["next"]["digest"], v["next"]["digest"]);

    let (s, _) = call(
        &n,
        "DELETE",
        "/api/manifest/skill/release-notes?channel=work",
        Some(LOCAL),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = call(&n, "GET", "/api/manifest?channel=work", Some(LOCAL), None).await;
    assert_eq!(v["items"], json!([]));
}

/// Importing a skill puts executable content where a harness will run it, so
/// it is done at the node itself, like every other write that decides what
/// the node executes.
#[tokio::test]
async fn importing_a_skill_is_refused_off_the_machine() {
    let n = node();
    let root = state::scratch("manifest-remote");
    let dir = package(&root, "release-notes", "Write the notes.");
    for peer in [Some(REMOTE), None] {
        let (s, v) = call(
            &n,
            "POST",
            "/api/manifest/skills",
            peer,
            Some(json!({ "channel": "work", "source": dir.to_string_lossy() })),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{peer:?}: {v}");
    }
}

/// The sharpest of the refusals. Upstream logs a warning and overwrites, and
/// `concurrency: "unbounded"` means the winner varies between runs — so a
/// project skill can shadow a global one on Tuesday and not on Wednesday.
#[tokio::test]
async fn a_second_package_claiming_an_imported_name_is_refused() {
    let n = node();
    let root = state::scratch("manifest-duplicate");
    let first = package(&root.join("a"), "release-notes", "The real one.");
    let second = package(&root.join("b"), "release-notes", "Something else.");

    let (s, _) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": first.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, v) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": second.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("nondeterministically"), "{message}");

    // Re-importing the *same* package is an update, not a duplicate: the
    // operator changed a file and wants the change.
    std::fs::write(
        first.join("SKILL.md"),
        "---\nname: release-notes\ndescription: A test skill\n---\n\nChanged.\n",
    )
    .unwrap();
    let (s, again) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": first.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{again}");
    let (_, view) = call(&n, "GET", "/api/manifest?channel=work", Some(LOCAL), None).await;
    assert_eq!(view["items"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn a_url_source_is_refused_at_the_interface() {
    let n = node();
    let (s, v) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": "https://skills.example/pack", "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("plain HTTP"), "{message}");
}

/// A symlink is the only way a validated package can still name bytes outside
/// itself, and upstream's scans follow them everywhere.
#[cfg(unix)]
#[tokio::test]
async fn a_package_that_links_out_of_itself_is_refused() {
    let n = node();
    let root = state::scratch("manifest-escape");
    std::fs::write(root.join("secret"), "not the harness's").unwrap();
    let dir = package(&root, "release-notes", "Write the notes.");
    std::os::unix::fs::symlink(root.join("secret"), dir.join("leak.md")).unwrap();

    let (s, v) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": dir.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("symlink"), "{message}");
}

/// A plugin the image did not bake is refused at build, which the pane shows
/// as a manifest that will not launch rather than as a session that fails.
#[tokio::test]
async fn a_plugin_outside_the_images_cache_makes_the_manifest_refuse_to_build() {
    let mut cfg = Config::default();
    cfg.launch.plugins = vec!["@example/audit@1.0.0".into()];
    let n = node_with(cfg);

    let (s, v) = call(&n, "GET", "/api/manifest?channel=work", Some(LOCAL), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["next"].is_null(), "{v}");
    let error = v["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("packages/@example/audit@1.0.0/node_modules/@example/audit"),
        "{error}"
    );
    // The image's own list is shown beside it, so the reason is legible.
    assert_eq!(v["baked_plugins"], json!([]));
}

/// A formatter enabled without a command is the one shape that leaves
/// OpenCode resolving the binary itself — and for three of them that is an
/// install over a network the runner does not have.
#[tokio::test]
async fn a_formatter_without_a_command_is_refused_by_the_settings_allowlist() {
    let n = node();
    let (s, v) = call(
        &n,
        "PUT",
        "/api/config",
        Some(LOCAL),
        Some(json!({ "launch": { "formatters": { "prettier": [] } } })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("absolute command"), "{message}");
}

// ---------------------------------------------------------------------------
// What a launch renders
// ---------------------------------------------------------------------------

fn wiring_with(manifest: manifest::LaunchManifest) -> tracon::gateway::model::Wiring {
    let mut cfg = Config::default();
    cfg.providers.clear();
    cfg.providers.insert(
        "anthropic".into(),
        tracon::config::Provider {
            credential: "anthropic".into(),
            upstream: "https://api.anthropic.com".into(),
            shape: tracon::config::SHAPE_ANTHROPIC.into(),
            models: vec![tracon::config::ModelDecl {
                id: "claude-x".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let mut wiring =
        tracon::gateway::model::harness_wiring(&cfg, "tracon-gw", "session-token", |_, _| true);
    wiring.manifest = manifest;
    wiring
}

fn built(store: &Store, channel: &str) -> manifest::LaunchManifest {
    let (skills, instructions, agents) = store.manifest_contents(channel).unwrap();
    manifest::build(manifest::Inputs {
        channel,
        skills,
        instructions,
        agents,
        plugins: &[],
        baked: &[],
        lsp: Vec::new(),
        formatters: Vec::new(),
        providers: vec!["anthropic".into()],
        policy_revision: "1".into(),
    })
    .unwrap()
}

fn rendered(wiring: &tracon::gateway::model::Wiring) -> Vec<(String, String)> {
    OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION).scratch_files(wiring)
}

/// The config the node writes names one skill root, through the variable the
/// launch environment resolves, and never names a URL.
#[tokio::test]
async fn the_rendered_config_points_at_one_skill_root_and_no_urls() {
    let n = node();
    let root = state::scratch("manifest-render");
    let dir = package(&root, "release-notes", "Write the notes.");
    let (s, _) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": dir.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let wiring = wiring_with(built(&n.store, "work"));
    let files = rendered(&wiring);
    let config: Value = serde_json::from_str(
        &files
            .iter()
            .find(|(name, _)| name == "opencode.json")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(
        config["skills"]["paths"],
        json!(["{env:TRACON_SKILL_ROOT}"]),
        "{config}"
    );
    assert!(config["skills"]["urls"].is_null(), "{config}");
    // Off by configuration rather than by a default that might change.
    assert_eq!(config["lsp"], json!(false));
    assert_eq!(config["formatter"], json!(false));
    assert!(config["plugin"].is_null(), "{config}");

    // The package itself is staged under that root, one directory per skill.
    assert!(
        files
            .iter()
            .any(|(name, body)| name == "skills/release-notes/SKILL.md"
                && body.contains("Write the notes.")),
        "{:?}",
        files.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
}

/// A new revision is for the next launch. The files a running session was
/// staged from are already on disk and in its recorded digest; editing the
/// manifest cannot reach either.
#[tokio::test]
async fn a_later_revision_does_not_alter_what_a_running_session_was_staged_from() {
    let n = node();
    let root = state::scratch("manifest-revision");
    let dir = package(&root, "release-notes", "The first version.");
    let (s, _) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": dir.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // What a launch would stage, and the revision it would record.
    let first = n.store.manifest_record(&built(&n.store, "work")).unwrap();
    assert_eq!(first.revision, 1);
    let staged = rendered(&wiring_with(first.clone()));

    // The operator edits the package and re-imports it.
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: release-notes\ndescription: A test skill\n---\n\nThe second version.\n",
    )
    .unwrap();
    let (s, _) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": dir.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let second = n.store.manifest_record(&built(&n.store, "work")).unwrap();
    assert_eq!(second.revision, 2);
    assert_ne!(second.digest, first.digest);

    // The running session's files are unchanged — they are a function of the
    // manifest it holds, not of the store.
    assert_eq!(staged, rendered(&wiring_with(first.clone())));
    assert!(staged
        .iter()
        .any(|(name, body)| name == "skills/release-notes/SKILL.md"
            && body.contains("The first version.")));

    // And the digest a session recorded still resolves to those bytes.
    let looked_up = n.store.manifest_by_digest(&first.digest).unwrap().unwrap();
    assert_eq!(looked_up.revision, 1);
    assert!(looked_up.skills[0].files[0]
        .text
        .contains("The first version."));

    // What the next launch would stage is the second version.
    assert!(rendered(&wiring_with(second))
        .iter()
        .any(|(name, body)| name == "skills/release-notes/SKILL.md"
            && body.contains("The second version.")));
}

/// A session carries the digest it launched under, written once.
#[tokio::test]
async fn a_session_records_the_manifest_it_launched_under() {
    let n = node();
    let mut row = support::rows::session_row("s-manifest", "n1", "work");
    row.state = "running".into();
    n.store
        .put_node(&support::rows::node_row("n1", "n"))
        .unwrap();
    n.store.insert_session(&row).unwrap();
    assert!(n
        .store
        .get_session("s-manifest")
        .unwrap()
        .unwrap()
        .manifest_digest
        .is_none());

    let manifest = n.store.manifest_record(&built(&n.store, "work")).unwrap();
    n.store
        .update_session(
            "s-manifest",
            tracon::store::SessionPatch {
                manifest_digest: Some(manifest.digest.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    let (s, v) = call(&n, "GET", "/api/sessions/s-manifest", Some(LOCAL), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["session"]["manifest_digest"], manifest.digest);
}

// ---------------------------------------------------------------------------
// Against the pinned binary
// ---------------------------------------------------------------------------

/// The live case: the manifest's skill is what the harness lists, and the
/// project's own `.claude/skills` and `.opencode/tool` are not there at all.
///
/// Both halves matter and neither is provable from the config file. Skill
/// discovery walks six roots and `OPENCODE_PURE` does not cover custom tools
/// at all — `OPENCODE_DISABLE_PROJECT_CONFIG` is what keeps `.opencode/tool`
/// out, and the only way to know it worked is to ask a running server.
#[tokio::test]
async fn the_manifests_skill_is_listed_and_the_projects_are_not() {
    state::isolate();
    let Some(binary) = pinned_binary() else {
        eprintln!(
            "skipped: the pinned OpenCode binary is not on this machine. \
             Put it on PATH as `opencode`, or name it in TRACON_OPENCODE_BINARY."
        );
        return;
    };
    let root = state::scratch("manifest-live");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();

    // What a repository could put in front of the harness, and must not reach
    // it: a Claude-compatible skill, an OpenCode project skill, and a custom
    // tool that `OPENCODE_PURE` would happily have loaded.
    let planted_skill = work.join(".claude/skills/planted");
    std::fs::create_dir_all(&planted_skill).unwrap();
    std::fs::write(
        planted_skill.join("SKILL.md"),
        "---\nname: planted\ndescription: Must not be loaded\n---\n\nplanted body\n",
    )
    .unwrap();
    let project_skill = work.join(".opencode/skills/projectskill");
    std::fs::create_dir_all(&project_skill).unwrap();
    std::fs::write(
        project_skill.join("SKILL.md"),
        "---\nname: projectskill\ndescription: Must not be loaded\n---\n\nproject body\n",
    )
    .unwrap();
    let tool_dir = work.join(".opencode/tool");
    std::fs::create_dir_all(&tool_dir).unwrap();
    std::fs::write(
        tool_dir.join("plantedtool.ts").as_path(),
        "export default { description: 'planted', args: {}, \
         async execute() { return 'planted' } }\n",
    )
    .unwrap();

    // The node's own skill, through the manifest.
    let source = package(&root, "release-notes", "The manifest's own body.");
    let entry = manifest::read_package(&manifest::SkillSource::Dir(source)).unwrap();
    let built = manifest::build(manifest::Inputs {
        channel: "work",
        skills: vec![entry],
        instructions: Vec::new(),
        agents: Vec::new(),
        plugins: &[],
        baked: &[],
        lsp: Vec::new(),
        formatters: Vec::new(),
        providers: vec!["anthropic".into()],
        policy_revision: "1".into(),
    })
    .unwrap();

    let adapter = OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION);
    let state_dir = root.join(".opencode");
    std::fs::create_dir_all(state_dir.join("run")).unwrap();
    for (name, body) in adapter.scratch_files(&wiring_with(built)) {
        let staged = state_dir.join(&name);
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(staged, body).unwrap();
    }

    let runner = LiveRunner {
        binary,
        launched: Arc::new(Mutex::new(None)),
    };
    let container = format!("tracon-manifest-live-{}", std::process::id());
    let spec = LaunchSpec {
        cwd_in_runner: work.to_string_lossy().into_owned(),
        model: "anthropic/claude-x".into(),
        container_name: container.clone(),
        harness_home: root.to_string_lossy().into_owned(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        env: Vec::new(),
        system_prompt_file: None,
        cursor: None,
    };
    let (handle, _rx) = adapter
        .launch(&runner, spec)
        .await
        .expect("the pinned binary starts");
    let (endpoint, password) = runner
        .launched
        .lock()
        .unwrap()
        .clone()
        .expect("the launch recorded its endpoint");

    let ask = |path: String| {
        let endpoint = endpoint.clone();
        let password = password.clone();
        async move {
            use base64::Engine;
            let credential =
                base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(format!("http://{endpoint}{path}"))
                .header("authorization", format!("Basic {credential}"))
                .send()
                .await
                .expect("the server answers")
                .text()
                .await
                .unwrap_or_default()
        }
    };

    let directory = urlencode(&work.to_string_lossy());
    let skills = ask(format!("/skill?directory={directory}")).await;
    let tools = ask(format!("/experimental/tool/ids?directory={directory}")).await;
    handle.close().await.ok();
    runner.kill(&container).await.ok();

    assert!(
        skills.contains("release-notes"),
        "the manifest's skill is not listed: {skills}"
    );
    assert!(
        !skills.contains("planted"),
        "a project `.claude/skills` skill reached the harness: {skills}"
    );
    assert!(
        !skills.contains("projectskill"),
        "a project `.opencode/skills` skill reached the harness: {skills}"
    );
    // The negative only means something if the route answered at all: a 404
    // contains neither the planted tool nor a builtin.
    assert!(
        tools.contains("bash"),
        "the tool listing did not answer with the builtins: {tools}"
    );
    assert!(
        !tools.contains("plantedtool"),
        "a project `.opencode/tool` custom tool reached the harness: {tools}"
    );
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// The pinned binary, when this machine has it. A build at another version
/// proves nothing about the release this adapter was written against.
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
    if reported != OpenCodeAdapter::PINNED_VERSION {
        eprintln!(
            "skipped: {candidate} reports {reported}, not the pinned {}",
            OpenCodeAdapter::PINNED_VERSION
        );
        return None;
    }
    Some(candidate)
}

struct LiveRunner {
    binary: String,
    launched: Arc<Mutex<Option<(String, String)>>>,
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
        let spawned = tracon::runner::local::LocalRunner.spawn(cmd).await?;
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
