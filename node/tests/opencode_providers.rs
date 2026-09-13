//! Gate B, provider proofs: what OpenCode actually does with the provider
//! table this node writes for it.
//!
//! `docs/reference/opencode-v1.18.30/providers.md` settled the provider model
//! by reading the pinned release's source. Four of its conclusions are
//! properties of a running binary rather than of that source, and the manifest
//! says so: the header bytes the bundled ai-sdk providers put on the wire
//! (§2.6), Bun's `HTTP(S)_PROXY`/`NO_PROXY` handling (§4.2), whether a
//! self-hosted endpoint's usage survives to the gateway's counter (§6.3), and
//! whether the declared catalogue is the whole catalogue with the fetch off
//! (§7.3). This file proves each one, and fails if it stops holding.
//!
//! Three layers, in order of what they need:
//!
//! 1. The rendered configuration, which needs nothing — the Codex trap
//!    (finding 9) is asserted here, by construction, because the shape of the
//!    file is what forecloses it.
//! 2. The pinned binary against a fake upstream behind the real gateway,
//!    skipped with a message when the binary is not on this machine. This is
//!    where header bytes, allowlisted paths and the proxy are observed.
//! 3. The pinned binary against a real self-hosted OpenAI-compatible server,
//!    skipped when there is none, which is where a token count is a fact
//!    rather than a fixture.
//!
//! Hosted keys and both subscriptions stay the operator's: no credential for
//! them exists on a test machine, and a proof that needs one is not a proof a
//! test can make.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use axum::response::IntoResponse;
use axum::routing::any;
use serde_json::{json, Value};

use tracon::{
    adapter::{opencode::OpenCodeAdapter, HarnessAdapter, LaunchSpec},
    broker::Broker,
    config::{Config, ModelDecl, Provider, SHAPE_ANTHROPIC, SHAPE_OPENAI, SHAPE_OPENAI_CODEX},
    gateway::model::{base_url, harness_wiring, Wiring},
    http::api::AppState,
    mcp::Tools,
    runner::{Runner, RunnerCommand, RunnerError, Spawned},
    session::Manager,
    store::Store,
    stream::Bus,
};

use support::fake::FakeAdapter;

/// The shape a self-hosted OpenAI-compatible server is wired as. It is not one
/// of the three shapes this build names, which is the point: the gateway holds
/// an unknown shape to the OpenAI surface and the adapter renders it with the
/// OpenAI-compatible SDK, so a local server needs no new shape to be added.
const SHAPE_COMPATIBLE: &str = "openai-compatible";

/// The placeholder every provider entry carries. It is the session's token at
/// the gateway, so the only secret the runner holds names its own session.
const TOKEN: &str = "session-token-not-a-provider-key";

// ---------------------------------------------------------------------------
// 1. What the node writes
// ---------------------------------------------------------------------------

fn model(id: &str) -> ModelDecl {
    ModelDecl {
        id: id.into(),
        context: 200_000,
        output: 64_000,
        ..Default::default()
    }
}

fn provider(shape: &str, upstream: &str, models: &[&str]) -> Provider {
    Provider {
        credential: "cred".into(),
        upstream: upstream.into(),
        shape: shape.into(),
        models: models.iter().map(|id| model(id)).collect(),
        ..Default::default()
    }
}

/// A node configuration with one provider of each shape this node can wire.
fn every_shape() -> Config {
    let mut cfg = Config::default();
    cfg.providers.clear();
    cfg.providers.insert(
        "anthropic".into(),
        provider(SHAPE_ANTHROPIC, "https://api.anthropic.com", &["claude-x"]),
    );
    cfg.providers.insert(
        "openai".into(),
        provider(SHAPE_OPENAI, "https://api.openai.com", &["gpt-x"]),
    );
    cfg.providers.insert(
        "openai-codex".into(),
        provider(
            SHAPE_OPENAI_CODEX,
            "https://chatgpt.com/backend-api",
            &["gpt-x-codex"],
        ),
    );
    cfg.providers.insert(
        "local".into(),
        provider(SHAPE_COMPATIBLE, "http://127.0.0.1:8080", &["qwen-x"]),
    );
    cfg
}

/// The files the node mounts into the runner, parsed: the provider table, the
/// pinned catalogue, and the auth store.
struct Rendered {
    config: Value,
    catalogue: Value,
    auth: Value,
}

fn render(cfg: &Config) -> Rendered {
    let wiring = harness_wiring(cfg, "tracon-gw", TOKEN, |_, _| true);
    render_wiring(&wiring)
}

fn render_wiring(wiring: &Wiring) -> Rendered {
    let files = OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION).scratch_files(wiring);
    let read = |name: &str| -> Value {
        let body = files
            .iter()
            .find(|(file, _)| file == name)
            .map(|(_, body)| body.clone())
            .unwrap_or_else(|| panic!("{name} is not among the files the node writes"));
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{name} is not JSON: {e}"))
    };
    Rendered {
        config: read("opencode.json"),
        catalogue: read("models.json"),
        auth: read("auth.json"),
    }
}

/// The Anthropic shape: the SDK that speaks `x-api-key`, the gateway's URL
/// with the version segment the Messages path hangs off, and a placeholder
/// where a key would be. OpenCode reads no base-URL environment variable
/// (finding 8), so `options.baseURL` is the only thing standing between the
/// harness and `api.anthropic.com`.
#[test]
fn the_anthropic_shape_is_wired_to_the_gateway_with_a_placeholder_key() {
    let rendered = render(&every_shape());
    let p = &rendered.config["provider"]["anthropic"];
    assert_eq!(p["npm"], "@ai-sdk/anthropic");
    assert_eq!(
        p["options"]["baseURL"],
        "http://tracon-gw:7421/model/anthropic/v1"
    );
    assert_eq!(p["options"]["apiKey"], TOKEN);
    assert_eq!(p["models"]["claude-x"]["id"], "claude-x");
    // The version segment is not decoration: the gateway matches
    // `v1/messages` against the Anthropic allowlist, so a base URL without it
    // would send `POST /messages`, which is refused.
    assert_eq!(
        format!("{}/messages", p["options"]["baseURL"].as_str().unwrap()),
        "http://tracon-gw:7421/model/anthropic/v1/messages"
    );
}

/// The OpenAI shape. `custom().openai` forces the Responses API for a provider
/// with this id, so the path is `v1/responses` — also on the allowlist.
#[test]
fn the_openai_shape_is_wired_to_the_gateway_under_its_own_v1() {
    let rendered = render(&every_shape());
    let p = &rendered.config["provider"]["openai"];
    assert_eq!(p["npm"], "@ai-sdk/openai");
    assert_eq!(
        p["options"]["baseURL"],
        "http://tracon-gw:7421/model/openai/v1"
    );
    assert_eq!(p["options"]["apiKey"], TOKEN);
    assert_eq!(p["models"]["gpt-x"]["id"], "gpt-x");
}

/// **The Codex trap, foreclosed by construction** (finding 9,
/// `providers.md` §3.5 and "The trap to write down").
///
/// OpenCode's Codex plugin binds to the provider id `openai` and installs a
/// fetch that rewrites any path containing `/v1/responses` to
/// `chatgpt.com/backend-api/codex/responses`, carrying the subscription bearer
/// with it — bypassing the gateway, its allowlist, its ceiling and its
/// counting, with no error anywhere. Two things stop it, and both are
/// properties of this file rather than of the plugin: the Codex provider is
/// never named `openai`, and the auth store holds no `oauth` record, without
/// which the loader is never even run.
#[test]
fn the_codex_shape_keeps_its_own_provider_id_and_arms_no_oauth_loader() {
    let rendered = render(&every_shape());
    let codex = &rendered.config["provider"]["openai-codex"];
    assert_eq!(
        codex["options"]["baseURL"],
        "http://tracon-gw:7421/model/openai-codex/v1"
    );
    assert_eq!(codex["models"]["gpt-x-codex"]["id"], "gpt-x-codex");

    // No provider named `openai` carries a Codex model, under either the
    // config or the catalogue the picker reads.
    for table in [
        &rendered.config["provider"]["openai"],
        &rendered.catalogue["openai"],
    ] {
        for name in table["models"].as_object().into_iter().flatten() {
            assert!(
                !name.0.contains("codex"),
                "a Codex model under the provider id the Codex plugin binds to: {name:?}"
            );
        }
    }
    // And the loader that would do the rewriting never runs, because there is
    // no auth record of any kind for it to run for.
    assert_eq!(rendered.auth, json!({}));
    assert!(
        !rendered.auth.to_string().contains("oauth"),
        "an oauth record arms every subscription plugin inside the runner"
    );

    // The same config with the Codex provider named `openai` is what the trap
    // needs; this node cannot write one, because the name is the provider's
    // own and the two ids are distinct entries.
    let names: Vec<&str> = rendered.config["provider"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(names.contains(&"openai-codex"), "{names:?}");
}

/// A self-hosted OpenAI-compatible endpoint — llama.cpp, LM Studio, Ollama —
/// is a provider whose shape this build does not name. It gets the bundled
/// OpenAI-compatible SDK and an explicit base URL, which is the whole
/// configuration such a server needs (`providers.md` §5.1). The base URL is
/// the gateway's, never the server's: the runner reaches local inference
/// through a node-owned binding to the exact service, never by being handed
/// loopback.
#[test]
fn a_self_hosted_provider_is_openai_compatible_with_an_explicit_base_url() {
    let rendered = render(&every_shape());
    let p = &rendered.config["provider"]["local"];
    assert_eq!(p["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(
        p["options"]["baseURL"],
        "http://tracon-gw:7421/model/local/v1"
    );
    assert_eq!(p["options"]["apiKey"], TOKEN);
    // Not the endpoint itself. The runner never learns where the model
    // actually is.
    assert!(
        !rendered.config.to_string().contains("127.0.0.1:8080"),
        "the upstream address reached the runner: {}",
        rendered.config
    );
    // `includeUsage` is forced on by OpenCode for this SDK and must not be
    // turned off here, or the server stops reporting what a turn cost.
    assert!(
        p["options"]["includeUsage"] != json!(false),
        "includeUsage must never be set false: {p}"
    );
}

/// **Finding 19, in the rendering.** The gateway URL has to be in `api` as
/// well as in `options.baseURL`, in the provider entry and in the catalogue —
/// provider and model both. The session path the adapter drives resolves a
/// model's endpoint from the catalogue, and a provider carrying only
/// `options.baseURL` was served from the provider's own default host with the
/// gateway bypassed entirely. The live test that observed that needs the
/// pinned binary; this one does not, so the regression cannot reach a machine
/// that has no binary to catch it.
#[test]
fn the_gateway_url_is_in_every_field_a_base_url_is_resolved_from() {
    let rendered = render(&every_shape());
    for (name, npm) in [
        ("anthropic", "@ai-sdk/anthropic"),
        ("openai", "@ai-sdk/openai"),
        ("openai-codex", "@ai-sdk/openai"),
        ("local", "@ai-sdk/openai-compatible"),
    ] {
        let url = format!("http://tracon-gw:7421/model/{name}/v1");
        let provider = &rendered.config["provider"][name];
        assert_eq!(provider["options"]["baseURL"], url, "{name} config options");
        assert_eq!(provider["api"], url, "{name} config api");
        let catalogue = &rendered.catalogue[name];
        assert_eq!(catalogue["api"], url, "{name} catalogue provider");
        for (model, entry) in catalogue["models"].as_object().unwrap() {
            assert_eq!(entry["provider"]["api"], url, "{name}/{model} catalogue");
            assert_eq!(entry["provider"]["npm"], npm, "{name}/{model} catalogue");
        }
    }
    // And no provider's own host appears anywhere in what the runner reads.
    for written in [rendered.config.to_string(), rendered.catalogue.to_string()] {
        assert!(!written.contains("api.anthropic.com"), "{written}");
        assert!(!written.contains("api.openai.com"), "{written}");
        assert!(!written.contains("chatgpt.com"), "{written}");
    }
}

/// Everything this node does not serve is disabled by name as well as absent.
/// `amazon-bedrock` is the one that matters most: its credential chain can
/// reach the instance metadata service at 169.254.169.254 if any AWS variable
/// is in the environment (`providers.md` §5.2).
#[test]
fn the_providers_this_node_never_serves_are_disabled_and_bedrock_always_is() {
    let rendered = render(&every_shape());
    let disabled: Vec<&str> = rendered.config["disabled_providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(disabled.contains(&"amazon-bedrock"), "{disabled:?}");
    assert!(disabled.contains(&"github-copilot"), "{disabled:?}");
    assert!(disabled.contains(&"opencode"), "{disabled:?}");

    // And the allowlist on top is exactly what the node wired, so a provider
    // the gateway would refuse cannot be picked even if something else put it
    // in the table.
    let mut enabled: Vec<&str> = rendered.config["enabled_providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    enabled.sort_unstable();
    assert_eq!(
        enabled,
        ["anthropic", "local", "openai", "openai-codex"],
        "{enabled:?}"
    );
}

/// A provider the gateway would refuse is written nowhere the harness can
/// find it: not the provider table, not the allowlist, not the catalogue. A
/// provider exists in OpenCode through exactly four channels (§1.4) and the
/// node controls all four, so absence here is absence.
#[test]
fn a_provider_the_gateway_would_refuse_reaches_the_runner_through_no_channel() {
    let cfg = every_shape();
    let wiring = harness_wiring(&cfg, "tracon-gw", TOKEN, |name, _| name != "openai");
    let rendered = render_wiring(&wiring);
    assert!(rendered.config["provider"]["openai"].is_null());
    assert!(rendered.catalogue["openai"].is_null());
    assert!(!rendered.config["enabled_providers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "openai"));
    // Its models are not offered either, so the picker cannot name a session
    // that would be refused at its first model call.
    assert!(
        !rendered.catalogue.to_string().contains("gpt-x\""),
        "{}",
        rendered.catalogue
    );
}

/// The catalogue is the declared models and nothing else. With the fetch off
/// and no file, OpenCode's catalogue would be empty and config-declared
/// providers would still exist — so what the picker offers and what the
/// gateway will serve are the same list by construction (finding 11). The
/// live half of this is below.
#[test]
fn the_pinned_catalogue_is_exactly_the_declared_models() {
    let rendered = render(&every_shape());
    let catalogue = rendered.catalogue.as_object().unwrap();
    let mut ids: Vec<String> = Vec::new();
    for (name, entry) in catalogue {
        assert_eq!(entry["id"], name.as_str());
        // No environment variable supplies a key for any of them: the
        // placeholder is in the config and the credential is the gateway's.
        assert_eq!(entry["env"], json!([]));
        for model in entry["models"].as_object().unwrap().keys() {
            ids.push(format!("{name}/{model}"));
        }
    }
    ids.sort();
    assert_eq!(
        ids,
        [
            "anthropic/claude-x",
            "local/qwen-x",
            "openai-codex/gpt-x-codex",
            "openai/gpt-x",
        ]
    );
    // The config table declares the same set, so the two files cannot
    // disagree about what exists.
    let mut declared: Vec<String> = Vec::new();
    for (name, entry) in rendered.config["provider"].as_object().unwrap() {
        for model in entry["models"].as_object().unwrap().keys() {
            declared.push(format!("{name}/{model}"));
        }
    }
    declared.sort();
    assert_eq!(declared, ids);
}

/// The boundary the harness reaches the gateway over is plain HTTP on the
/// runner's own private network, so no certificate authority is installed in
/// the runner and nothing depends on Bun's `--use-system-ca` or
/// `NODE_EXTRA_CA_CERTS`. Recorded as a test rather than as a note: the day
/// that boundary becomes TLS, this is what has to change with it.
#[test]
fn the_gateway_boundary_is_plain_http_so_the_runner_installs_no_ca() {
    assert_eq!(
        base_url("tracon-gw", 7421, "anthropic"),
        "http://tracon-gw:7421/model/anthropic"
    );
    let rendered = render(&every_shape());
    let written = rendered.config.to_string();
    assert!(!written.contains("https://tracon-gw"), "{written}");
    assert!(
        !written.contains("NODE_EXTRA_CA_CERTS") && !written.contains("caFile"),
        "{written}"
    );
}

// ---------------------------------------------------------------------------
// 2. What the launch environment carries
// ---------------------------------------------------------------------------

/// A runner that starts nothing and keeps the command it was given, so a test
/// can read the environment the adapter built. The fake server it points at is
/// already listening.
struct EnvRunner {
    endpoint: SocketAddr,
    password: Arc<Mutex<String>>,
    seen: Arc<Mutex<Option<RunnerCommand>>>,
}

#[async_trait::async_trait]
impl Runner for EnvRunner {
    async fn spawn(&self, cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        if let Some((_, password)) = cmd
            .env
            .iter()
            .find(|(name, _)| name == "OPENCODE_SERVER_PASSWORD")
        {
            *self.password.lock().unwrap() = password.clone();
        }
        *self.seen.lock().unwrap() = Some(cmd);
        Ok(Spawned {
            stdin: Box::new(tokio::io::sink()),
            stdout: Box::new(tokio::io::empty()),
            done: Box::pin(std::future::pending()),
            endpoint: Some(self.endpoint.to_string()),
        })
    }

    async fn run_capture(&self, _cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        Ok(std::process::Output {
            status: Default::default(),
            stdout: b"1.18.30\n".to_vec(),
            stderr: Vec::new(),
        })
    }

    async fn kill(&self, _name: &str) -> Result<(), RunnerError> {
        Ok(())
    }
}

/// The environment a launch actually carries, as the runner sees it: the
/// subscription plugins off, external config off, and an auth store supplied
/// inline that holds nothing. These are the belt on top of the structural
/// answer above — none of them is load-bearing, and all of them are set.
#[tokio::test]
async fn the_launch_environment_disables_the_subscription_plugins_and_carries_an_empty_store() {
    state::isolate();
    let fake = support::fake_opencode::Fake::new("1.18.30", usize::MAX);
    let password = fake.password.clone();
    let endpoint = support::fake_opencode::serve(fake).await;
    let seen = Arc::new(Mutex::new(None));
    let runner = EnvRunner {
        endpoint,
        password,
        seen: seen.clone(),
    };
    let (handle, _rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, support::fake_opencode::spec())
        .await
        .expect("the harness starts");
    handle.close().await.ok();

    let cmd = seen.lock().unwrap().clone().expect("the launch recorded");
    let env: std::collections::HashMap<String, String> = cmd.env.into_iter().collect();
    assert_eq!(env["OPENCODE_DISABLE_DEFAULT_PLUGINS"], "true");
    assert_eq!(env["OPENCODE_PURE"], "1");
    assert_eq!(env["OPENCODE_DISABLE_PROJECT_CONFIG"], "true");
    assert_eq!(env["OPENCODE_DISABLE_MODELS_FETCH"], "true");
    assert!(env.contains_key("OPENCODE_MODELS_PATH"));
    // The store supplied inline, and nothing in it. A `wellknown` record
    // would fetch a remote descriptor and run a command the remote named to
    // mint a credential (§1.2); an `oauth` record would arm the plugins.
    let store: Value = serde_json::from_str(&env["OPENCODE_AUTH_CONTENT"]).unwrap();
    assert_eq!(store, json!({}));
    // No certificate authority and no proxy invented by the adapter: both
    // belong to the runner backend, which sets them for the boundary it built.
    assert!(!env.contains_key("NODE_EXTRA_CA_CERTS"), "{env:?}");
    assert!(!env.contains_key("SSL_CERT_FILE"), "{env:?}");
    assert!(!env.contains_key("HTTPS_PROXY"), "{env:?}");
    // And no AWS variable, which is what keeps the Bedrock credential chain
    // away from the instance metadata service even if the provider were on.
    assert!(
        !env.keys().any(|k| k.starts_with("AWS_")),
        "{:?}",
        env.keys().collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// 3. The pinned binary against a fake upstream behind the real gateway
// ---------------------------------------------------------------------------

/// How long to wait for the pinned binary to compose and send its first
/// provider call. Startup is a 184 MB Bun executable opening a workspace, and
/// a machine that is also running a local model has other things to do.
const LIVE_CALL_TIMEOUT: Duration = Duration::from_secs(90);

/// One request as the provider stub saw it, before anything was interpreted.
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<Recorded>>>,
    /// When set, the stub answers with no `usage` anywhere in the stream.
    silent: Arc<Mutex<bool>>,
}

impl Upstream {
    fn requests(&self) -> Vec<Recorded> {
        self.seen.lock().unwrap().clone()
    }

    /// Wait until the stub has seen a request, so a test asserts on what
    /// happened rather than racing the harness's own retry loop.
    async fn first(&self, within: Duration) -> Option<Recorded> {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if let Some(first) = self.requests().into_iter().next() {
                return Some(first);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }
}

/// Minimal valid streams, one per shape, copied from the pinned release's own
/// recorded fixtures (`packages/llm/test/fixtures/recordings/`). The harness
/// has to be able to finish a turn on them, or what it sends on the second
/// request is a retry rather than the request under test.
fn anthropic_stream(usage: bool) -> String {
    let start = if usage {
        r#"{"type":"message_start","message":{"model":"m","id":"msg_1","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":18,"output_tokens":2}}}"#
    } else {
        r#"{"type":"message_start","message":{"model":"m","id":"msg_1","type":"message","role":"assistant","content":[],"stop_reason":null}}"#
    };
    let delta = if usage {
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":18,"output_tokens":5}}"#
    } else {
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}"#
    };
    format!(
        "event: message_start\ndata: {start}\n\n\
         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"ok\"}}}}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\ndata: {delta}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn responses_stream(usage: bool) -> String {
    let usage = if usage {
        r#""usage":{"input_tokens":18,"output_tokens":5,"total_tokens":23}"#
    } else {
        r#""usage":null"#
    };
    let done = r#"{"id":"msg_1","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"text":"ok"}],"role":"assistant"}"#;
    format!(
        "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"in_progress\",\"model\":\"m\",\"output\":[],\"usage\":null}},\"sequence_number\":0}}\n\n\
         event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"item\":{{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"content\":[],\"role\":\"assistant\"}},\"output_index\":0,\"sequence_number\":1}}\n\n\
         event: response.content_part.added\ndata: {{\"type\":\"response.content_part.added\",\"content_index\":0,\"item_id\":\"msg_1\",\"output_index\":0,\"part\":{{\"type\":\"output_text\",\"annotations\":[],\"text\":\"\"}},\"sequence_number\":2}}\n\n\
         event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"content_index\":0,\"delta\":\"ok\",\"item_id\":\"msg_1\",\"output_index\":0,\"sequence_number\":3}}\n\n\
         event: response.output_text.done\ndata: {{\"type\":\"response.output_text.done\",\"content_index\":0,\"item_id\":\"msg_1\",\"output_index\":0,\"sequence_number\":4,\"text\":\"ok\"}}\n\n\
         event: response.content_part.done\ndata: {{\"type\":\"response.content_part.done\",\"content_index\":0,\"item_id\":\"msg_1\",\"output_index\":0,\"part\":{{\"type\":\"output_text\",\"annotations\":[],\"text\":\"ok\"}},\"sequence_number\":5}}\n\n\
         event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"item\":{done},\"output_index\":0,\"sequence_number\":6}}\n\n\
         event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"completed\",\"model\":\"m\",\"output\":[{done}],{usage}}},\"sequence_number\":7}}\n\n"
    )
}

fn chat_stream(usage: bool) -> String {
    let last = if usage {
        r#"{"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":14,"completion_tokens":2,"total_tokens":16}}"#
    } else {
        r#"{"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#
    };
    format!(
        "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"\"}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"ok\"}},\"finish_reason\":null}}]}}\n\n\
         data: {last}\n\n\
         data: [DONE]\n\n"
    )
}

/// A provider stub that records the bytes and answers whichever shape the path
/// asks for. It stands in for every hosted provider at once, which is what
/// makes a header assertion meaningful: nothing here knows which credential it
/// should have been given.
async fn start_upstream(upstream: Upstream) -> u16 {
    let app = axum::Router::new().fallback(any(move |req: Request<Body>| {
        let upstream = upstream.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(body, 1 << 22).await.unwrap();
            let path = parts.uri.path().to_string();
            upstream.seen.lock().unwrap().push(Recorded {
                method: parts.method.to_string(),
                uri: parts.uri.to_string(),
                headers: parts
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect(),
                body: String::from_utf8_lossy(&body).into_owned(),
            });
            let usage = !*upstream.silent.lock().unwrap();
            if path.ends_with("/models") {
                return (
                    [("content-type", "application/json")],
                    json!({"data": []}).to_string(),
                )
                    .into_response();
            }
            let stream = if path.ends_with("/messages") {
                anthropic_stream(usage)
            } else if path.ends_with("/responses") {
                responses_stream(usage)
            } else {
                chat_stream(usage)
            };
            (
                [
                    ("content-type", "text/event-stream"),
                    ("x-upstream", "stub"),
                ],
                stream,
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

/// The node: a real gateway on a real port with a real credential behind it,
/// the fake provider as its only upstream.
struct Node {
    cfg: Arc<Config>,
    /// Kept so the node outlives the launch that borrows its token.
    #[allow(dead_code)]
    manager: Manager,
    store: Arc<Store>,
    upstream: Upstream,
    /// Every request that arrived at the gateway, before it was matched or
    /// refused. What the harness sent is a fact about the harness, and the
    /// gateway's verdict on it is a separate one.
    gateway: Arc<Mutex<Vec<Recorded>>>,
    forward_port: u16,
    token: String,
    host: String,
}

impl Node {
    /// Wait until the gateway has seen a request. A harness that is retrying
    /// sends the same one again, so the first is what was sent.
    async fn at_the_gateway(&self, within: Duration) -> Option<Recorded> {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if let Some(first) = self.gateway.lock().unwrap().first().cloned() {
                return Some(first);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }

    /// Send a request to the gateway as the session would, with the
    /// placeholder the runner was given. Answers `(status, body)`.
    async fn through_the_gateway(&self, seen: &Recorded) -> (u16, String) {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut request = http
            .post(format!(
                "http://127.0.0.1:{}{}",
                self.forward_port, seen.uri
            ))
            .header("x-api-key", &self.token);
        for (name, value) in &seen.headers {
            if matches!(name.as_str(), "host" | "content-length" | "x-api-key") {
                continue;
            }
            request = request.header(name, value);
        }
        let answer = request.body(seen.body.clone()).send().await.unwrap();
        let status = answer.status().as_u16();
        (status, answer.text().await.unwrap_or_default())
    }
}

/// `credential` is what the broker holds for every provider: `api_key` for the
/// hosted-key paths, `oauth` for the subscription shaping.
async fn start_node(shapes: &[(&str, &str)], credential: &str, host: &str) -> Node {
    start_node_at(shapes, credential, host, None).await
}

/// The same, with the provider's upstream pointed somewhere real instead of
/// at the stub: a node-owned binding to an exact service, which is the only
/// way a runner ever reaches local inference.
async fn start_node_at(
    shapes: &[(&str, &str)],
    credential: &str,
    host: &str,
    upstream_url: Option<&str>,
) -> Node {
    state::isolate();
    let upstream = Upstream::default();
    let upstream_port = start_upstream(upstream.clone()).await;
    let upstream_url = upstream_url
        .map(str::to_string)
        .unwrap_or_else(|| format!("http://127.0.0.1:{upstream_port}"));
    // The gateway's own port has to be known before the configuration that
    // names it, because the harness is wired from that configuration.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let forward_port = listener.local_addr().unwrap().port();

    let mut cfg = Config::default();
    cfg.gateway.forward_port = forward_port;
    cfg.gateway.allow_hosts = vec![r"^127\.0\.0\.1$".into(), r"^localhost$".into()];
    cfg.providers.clear();
    for (name, shape) in shapes {
        cfg.providers.insert(
            (*name).to_string(),
            provider(shape, &upstream_url, &["test-model"]),
        );
    }
    let cfg = Arc::new(cfg);
    let store = Arc::new(Store::open_in_memory().unwrap());
    let broker: Broker = toml::from_str(credential).unwrap();
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
    let gateway: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
    let app = tracon::http::harness_router(AppState {
        manager: manager.clone(),
        cfg: cfg.clone(),
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    })
    .layer(axum::middleware::from_fn_with_state(
        gateway.clone(),
        record_at_the_gateway,
    ));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    store.ensure_peer_node("n1").unwrap();
    let session_id = "s-live";
    store
        .conn()
        .execute(
            "INSERT INTO session (id, node_id, channel, repo_path, branch, harness_id, harness_version, model,
                budget_tokens, tokens_used, state, turn_active, created_ms, updated_ms)
             VALUES (?1, 'n1', 'work', '/r', 'b', 'opencode', '1.18.30', 'm', 1000000, 0, 'running', 1, 1, 1)",
            [session_id],
        )
        .unwrap();
    let token = manager
        .register_tool_token_for_test(session_id, "work")
        .await;
    Node {
        cfg,
        manager,
        store,
        upstream,
        gateway,
        forward_port,
        token,
        host: host.to_string(),
    }
}

/// Keeps every request that reached the gateway, whatever the gateway then
/// made of it. The refusals matter as much as the forwards: a call the
/// gateway turns away is still a call the harness made, and this is the only
/// place both are visible.
async fn record_at_the_gateway(
    axum::extract::State(seen): axum::extract::State<Arc<Mutex<Vec<Recorded>>>>,
    request: Request<Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 1 << 22)
        .await
        .unwrap_or_default();
    seen.lock().unwrap().push(Recorded {
        method: parts.method.to_string(),
        uri: parts
            .uri
            .path_and_query()
            .map(ToString::to_string)
            .unwrap_or_default(),
        headers: parts
            .headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    next.run(Request::from_parts(parts, Body::from(body))).await
}

const API_KEY: &str = r#"
    [credentials.cred]
    kind = "api_key"
    provider = "stub"
    channels = ["work"]
    [credentials.cred.env]
    API_KEY = "real-provider-key"
"#;

const SUBSCRIPTION: &str = r#"
    [credentials.cred]
    kind = "oauth"
    provider = "stub"
    channels = ["work"]
    [credentials.cred.env]
    ACCESS_TOKEN = "real-subscription-token"
    REFRESH_TOKEN = "never-leaves-the-node"
"#;

/// Runs the pinned binary on this host through the local runner, the way the
/// adapter would inside a container, and keeps the endpoint and credential of
/// the launch so a test can ask the running server what it loaded.
struct LiveRunner {
    binary: String,
    launched: Arc<Mutex<Option<(String, String)>>>,
    log: Arc<Mutex<String>>,
}

/// Keeps a copy of the harness's own log while the adapter reads it. The
/// adapter drains stdout into memory of its own and surfaces it only on a
/// failed start, so without this a turn that went wrong after startup is
/// silent — and a silent live test is one nobody can diagnose.
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

/// The pinned binary, when this machine has it: named in
/// `TRACON_OPENCODE_BINARY`, or on `PATH`. A build at another version proves
/// nothing about the release this adapter was written against, so it is
/// skipped rather than run.
fn pinned_binary() -> Option<String> {
    let candidate = match std::env::var_os("TRACON_OPENCODE_BINARY") {
        Some(named) => {
            let path = std::path::PathBuf::from(named);
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

fn skip_no_binary() -> bool {
    if pinned_binary().is_some() {
        return false;
    }
    eprintln!(
        "skipped: the pinned OpenCode binary is not on this machine. Put it on PATH as \
         `opencode`, or name it in TRACON_OPENCODE_BINARY."
    );
    true
}

/// One launch of the real binary against this node.
struct Launched {
    handle: Box<dyn tracon::adapter::HarnessHandle>,
    endpoint: String,
    password: String,
    root: std::path::PathBuf,
    container: String,
    runner: LiveRunner,
}

impl Launched {
    /// Ask the running server something, with the credential the node minted
    /// for this launch. The directory is pinned on every request, exactly as
    /// the node's own client does it.
    async fn ask(&self, path: &str) -> Value {
        use base64::Engine;
        let credential =
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{}", self.password));
        let work = self.root.join("work");
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!(
                "http://{}{path}{}directory={}",
                self.endpoint,
                if path.contains('?') { "&" } else { "?" },
                work.to_string_lossy()
            ))
            .header("authorization", format!("Basic {credential}"))
            .send()
            .await
            .expect("the server answers")
            .json()
            .await
            .expect("the answer is JSON")
    }

    /// Wait until the harness lists the session's model as available.
    ///
    /// The catalogue is populated asynchronously while the server is already
    /// answering, and a prompt that arrives first is admitted and then never
    /// run: the drain fails resolving the model and nothing retries, so the
    /// session sits on `prompted` with no step. Observed intermittently
    /// against the pinned binary — worth knowing, because the node's own
    /// handshake does not wait for this either.
    async fn catalogue_settled(&self, model: &str) {
        let wanted = model.split_once('/').map(|(_, id)| id).unwrap_or(model);
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            let listed = self.ask("/api/model").await;
            if listed.to_string().contains(wanted) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        eprintln!("the harness never listed {model} as available");
    }

    /// Everything the harness has said on its own stdout so far.
    fn log(&self) -> String {
        self.runner.log.lock().unwrap().clone()
    }

    async fn shutdown(self) {
        self.handle.close().await.ok();
        self.runner.kill(&self.container).await.ok();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Run one turn and wait for it, but never longer than `within`: a turn that
/// the fake provider cannot satisfy ends in the harness's own retry loop, and
/// what is under test is what it sent, not whether it gave up.
async fn one_turn(node: &Node, live: &Launched, text: &str, within: Duration) {
    tokio::select! {
        outcome = live.handle.prompt(text.to_string()) => match outcome {
            Ok(result) => eprintln!("turn ended: {}", result.stop_reason),
            Err(e) => eprintln!("turn failed: {e}"),
        },
        // The request is the subject, not the turn: a harness whose call the
        // gateway refuses retries inside the turn for as long as it likes,
        // and waiting that out proves nothing that the first request did not.
        seen = node.at_the_gateway(within) => match seen {
            Some(seen) => eprintln!("the harness asked the gateway for {}", seen.uri),
            None => {
                eprintln!("nothing reached the gateway within {within:?}");
                // Why, in the harness's own words. A turn that never made a
                // provider call said so on its durable stream.
                let session = live.handle.harness_session_id().to_string();
                let history = live.ask(&format!("/api/session/{session}/history?after=0")).await;
                eprintln!("harness history: {history}");
            }
        },
    }
    // What the gateway made of it, so a failure says why rather than only
    // that nothing arrived.
    for event in node
        .store
        .events_after("s-live", 0, 200)
        .unwrap_or_default()
    {
        if matches!(
            event.kind.as_str(),
            "gateway_refused" | "provider_error" | "unmetered"
        ) {
            eprintln!("gateway: {} {}", event.kind, event.payload);
        }
    }
    let log = live.log();
    for line in log.lines().rev().take(25).collect::<Vec<_>>().iter().rev() {
        eprintln!("harness: {line}");
    }
}

/// Start the pinned binary against `node`, wired from the node's own
/// configuration: the same provider table, catalogue and auth store the
/// session manager would mount, staged under a throwaway home.
async fn launch(node: &Node, name: &str, model: &str, env: Vec<(String, String)>) -> Launched {
    let binary = pinned_binary().expect("checked by the caller");
    let root = state::scratch(name);
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let wiring = harness_wiring(&node.cfg, &node.host, &node.token, |_, _| true);
    let state_dir = root.join(".opencode");
    std::fs::create_dir_all(state_dir.join("run")).unwrap();
    for (file, body) in OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION).scratch_files(&wiring)
    {
        std::fs::write(state_dir.join(&file), body).unwrap();
    }
    let container = format!("tracon-{name}-{}", std::process::id());
    let runner = LiveRunner {
        binary,
        launched: Arc::new(Mutex::new(None)),
        log: Arc::new(Mutex::new(String::new())),
    };
    let spec = LaunchSpec {
        cwd_in_runner: work.to_string_lossy().into_owned(),
        model: model.into(),
        container_name: container.clone(),
        harness_home: root.to_string_lossy().into_owned(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        env,
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
    let live = Launched {
        handle,
        endpoint,
        password,
        root,
        container,
        runner,
    };
    live.catalogue_settled(model).await;
    live
}

/// **Where the provider call actually goes** — the proof the whole gateway
/// design rests on, made against the binary rather than against the manifest.
///
/// A provider entry that named the gateway only in `options.baseURL` was
/// served by this binary's session runner from the provider's *own* default
/// host: a real request to `api.anthropic.com`, with the gateway, its
/// allowlist, its ceiling and its counting all bypassed, and no error
/// anywhere. The base URL has to be in `api` as well, in the config and in the
/// pinned catalogue, because that is the field this path resolves from. This
/// test is what keeps that from regressing: nothing reaches the fake provider
/// except through the gateway, and what the gateway is asked for is a path on
/// its own allowlist.
#[tokio::test]
async fn every_provider_call_arrives_at_the_gateway_on_an_allowlisted_path() {
    if skip_no_binary() {
        return;
    }
    for (name, shape, allowed) in [
        ("anthropic", SHAPE_ANTHROPIC, vec!["/v1/messages"]),
        (
            "local",
            SHAPE_COMPATIBLE,
            vec!["/v1/chat/completions", "/v1/responses"],
        ),
    ] {
        let node = start_node(&[(name, shape)], API_KEY, "127.0.0.1").await;
        let live = launch(
            &node,
            &format!("routed-{name}"),
            &format!("{name}/test-model"),
            Vec::new(),
        )
        .await;
        one_turn(&node, &live, "say ok", LIVE_CALL_TIMEOUT).await;
        let seen = node
            .at_the_gateway(Duration::from_secs(5))
            .await
            .unwrap_or_else(|| panic!("{name}: no provider call reached the gateway"));
        live.shutdown().await;

        assert_eq!(seen.method, "POST", "{name}: {seen:?}");
        let path = seen.uri.split('?').next().unwrap().to_string();
        let tail = path
            .strip_prefix(&format!("/model/{name}"))
            .unwrap_or_else(|| panic!("{name}: {path} is not this provider's gateway path"));
        assert!(
            allowed.contains(&tail),
            "{name}: {tail} is not on the gateway's allowlist for this shape"
        );
        // And the allowlist is not asserted from a list in this file: the
        // gateway either forwarded the harness's own call to the provider or
        // it did not.
        let upstream = node
            .upstream
            .first(Duration::from_secs(10))
            .await
            .unwrap_or_else(|| panic!("{name}: the gateway forwarded nothing"));
        // §2.6, confirmed on the wire: the real credential under the header
        // this shape's provider uses, and the placeholder nowhere.
        if shape == SHAPE_ANTHROPIC {
            assert_eq!(upstream.header("x-api-key"), Some("real-provider-key"));
            assert!(upstream.header("authorization").is_none(), "{upstream:?}");
            assert_eq!(upstream.header("anthropic-version"), Some("2023-06-01"));
        } else {
            assert_eq!(
                upstream.header("authorization"),
                Some("Bearer real-provider-key")
            );
            assert!(upstream.header("x-api-key").is_none(), "{upstream:?}");
        }
        assert!(
            !format!("{:?}{}", upstream.headers, upstream.body).contains(&node.token),
            "{name}: the session token reached the provider"
        );
        // And it was counted, on the channel that spent it.
        let totals = node.store.usage_since(Some("work"), 0).unwrap();
        assert!(!totals.is_empty(), "{name}: nothing was counted");
    }
}

/// **What the runner presents, observed.** The placeholder in the provider
/// table is the session's token at the gateway, not a provider key, and it is
/// what each shape puts on the wire: `Authorization: Bearer` for the
/// OpenAI-compatible one, `x-api-key` for the Anthropic one — the header
/// names §2.6 predicted, now seen coming out of the binary rather than going
/// into the provider. So the gateway authenticates the harness's own call and
/// serves it: a turn completes through the gateway with no help from this
/// test.
///
/// The invariant the whole design rests on is the other half: the only secret
/// that ever leaves the runner is one that names its own session.
#[tokio::test]
async fn the_runner_presents_its_session_token_and_never_a_provider_key() {
    if skip_no_binary() {
        return;
    }
    for (name, shape, header) in [
        ("local", SHAPE_COMPATIBLE, "authorization"),
        ("anthropic", SHAPE_ANTHROPIC, "x-api-key"),
    ] {
        let node = start_node(&[(name, shape)], API_KEY, "127.0.0.1").await;
        let live = launch(
            &node,
            &format!("credential-{name}"),
            &format!("{name}/test-model"),
            Vec::new(),
        )
        .await;
        one_turn(&node, &live, "say ok", LIVE_CALL_TIMEOUT).await;
        let seen = node
            .at_the_gateway(Duration::from_secs(5))
            .await
            .unwrap_or_else(|| panic!("{name}: no provider call reached the gateway"));
        live.shutdown().await;

        let presented = seen
            .header(header)
            .map(|value| value.trim_start_matches("Bearer ").to_string())
            .unwrap_or_default();
        assert_eq!(
            presented, node.token,
            "{name}: the runner presented {presented:?} under {header} rather than its \
             session token"
        );
        // And because it did, the gateway served the harness's own call:
        // a turn reaches the provider through the gateway unaided.
        let totals = node.store.usage_since(Some("work"), 0).unwrap();
        assert!(
            totals.iter().any(|row| row.requests > 0),
            "{name}: the gateway did not serve the harness's own call"
        );
        assert!(
            !seen.body.contains("real-provider-key"),
            "{name}: a provider key was in the runner's request body"
        );
    }
}

/// **OpenCode's own request headers, observed**, which is what the gateway's
/// subscription shaping has to merge with rather than replace: the beta flags
/// this binary sets for the Anthropic shape are part of the request the
/// harness makes, and #166 prepends `oauth-2025-04-20` to them rather than
/// overwriting. Taking the flags from the wire rather than from the manifest
/// is the point — the header the gateway has to preserve is whatever the
/// pinned binary actually sends.
#[tokio::test]
async fn the_subscription_shaping_merges_with_the_flags_the_binary_sends() {
    if skip_no_binary() {
        return;
    }
    let node = start_node(&[("anthropic", SHAPE_ANTHROPIC)], SUBSCRIPTION, "127.0.0.1").await;
    let live = launch(&node, "subscription", "anthropic/test-model", Vec::new()).await;
    one_turn(&node, &live, "say ok", LIVE_CALL_TIMEOUT).await;
    let seen = node
        .at_the_gateway(Duration::from_secs(5))
        .await
        .expect("no provider call reached the gateway");
    live.shutdown().await;

    let sent = seen
        .header("anthropic-beta")
        .unwrap_or_default()
        .to_string();
    assert!(
        sent.contains("interleaved-thinking-2025-05-14"),
        "the pinned binary no longer sends the beta flags the merge was written for: {sent:?}"
    );

    // And what the gateway made of it, with the subscription credential bound
    // to the session: the harness's own call, forwarded.
    let upstream = node
        .upstream
        .first(Duration::from_secs(10))
        .await
        .expect("the gateway forwarded nothing");
    assert_eq!(
        upstream.header("authorization"),
        Some("Bearer real-subscription-token")
    );
    assert!(
        !format!("{:?}", upstream.headers).contains("never-leaves-the-node"),
        "the refresh token left the node"
    );
    let merged = upstream.header("anthropic-beta").unwrap_or_default();
    assert!(
        merged.starts_with("oauth-2025-04-20"),
        "the subscription flag is not first: {merged}"
    );
    for flag in sent.split(',').map(str::trim) {
        assert!(
            merged.contains(flag),
            "the binary's own flag {flag} did not survive the merge: {merged}"
        );
    }
    // And the system prompt opens with the sentence a subscription token is
    // honoured for, which the harness did not write and does not know about.
    let body: Value = serde_json::from_str(&upstream.body).expect("a JSON body");
    let first = body["system"][0]["text"].as_str().unwrap_or_default();
    assert!(
        first.starts_with("You are Claude Code"),
        "the subscription shaping did not apply: {first}"
    );
}

/// **Bun's proxy handling, observed** (`providers.md` §4.2). The runner
/// backends set `HTTP_PROXY`/`HTTPS_PROXY` at tracon's CONNECT proxy and
/// `NO_PROXY` at the gateway host, and upstream's own network documentation
/// warns that exempting the local server is required. Both halves are facts
/// about Bun's `fetch`, not about OpenCode, and an upgrade can change them.
///
/// The proxy here refuses everything that is not a CONNECT, exactly as
/// tracon's does, so a request that reaches it is both recorded and visibly
/// broken — which is what makes the `NO_PROXY` entry load-bearing rather than
/// decorative.
#[tokio::test]
async fn bun_honours_the_proxy_and_no_proxy_exempts_the_gateway() {
    if skip_no_binary() {
        return;
    }
    let proxy = RecordingProxy::start().await;

    // Without the gateway host exempted, the harness's provider call goes to
    // the proxy — which is how we know Bun's fetch honours the variable at
    // all — and never reaches the provider.
    let node = start_node(&[("anthropic", SHAPE_ANTHROPIC)], API_KEY, "localhost").await;
    let live = launch(
        &node,
        "proxied",
        "anthropic/test-model",
        vec![
            ("HTTP_PROXY".into(), proxy.url()),
            ("HTTPS_PROXY".into(), proxy.url()),
            ("NO_PROXY".into(), "127.0.0.1".into()),
        ],
    )
    .await;
    one_turn(&node, &live, "say ok", LIVE_CALL_TIMEOUT).await;
    let through = proxy.first_model_call(Duration::from_secs(10)).await;
    live.shutdown().await;
    let through = through.expect("Bun did not route the provider call through HTTP_PROXY");
    assert!(
        through.contains("/model/anthropic/v1/messages"),
        "the proxy saw something else: {through}"
    );
    assert!(
        node.gateway.lock().unwrap().is_empty(),
        "the call reached the gateway despite the proxy"
    );

    // With the gateway host exempted — which is what the runner backends set
    // — the same call goes direct and no model traffic reaches the proxy.
    let before = proxy.model_calls().len();
    let node = start_node(&[("anthropic", SHAPE_ANTHROPIC)], API_KEY, "localhost").await;
    let live = launch(
        &node,
        "no-proxy",
        "anthropic/test-model",
        vec![
            ("HTTP_PROXY".into(), proxy.url()),
            ("HTTPS_PROXY".into(), proxy.url()),
            ("NO_PROXY".into(), "localhost".into()),
        ],
    )
    .await;
    one_turn(&node, &live, "say ok", LIVE_CALL_TIMEOUT).await;
    let seen = node.at_the_gateway(Duration::from_secs(5)).await;
    live.shutdown().await;
    assert!(
        seen.is_some(),
        "the exempted call did not reach the gateway"
    );
    assert_eq!(
        proxy.model_calls().len(),
        before,
        "the exempted call went through the proxy anyway: {:?}",
        proxy.seen()
    );

    // What else arrived is worth seeing rather than asserting: the binary
    // reaches for the npm registry at session start even with `--pure` and
    // the default plugins off (`providers.md` §1.2), and the proxy is where
    // that lands. Nothing in this design depends on it stopping; the
    // allowlist is what makes it harmless either way.
    eprintln!("everything the proxy saw: {:?}", proxy.seen());
}

/// A proxy with tracon's own contract — only CONNECT is forwarded, everything
/// else is refused — that keeps the request line of whatever arrives.
struct RecordingProxy {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl RecordingProxy {
    async fn start() -> Self {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let recorder = recorder.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(socket);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    recorder.lock().unwrap().push(line.trim().to_string());
                    let mut socket = reader.into_inner();
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 403 Forbidden\r\ncontent-length: 26\r\n\r\n\
                              only CONNECT is proxied\r\n",
                        )
                        .await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { port, seen }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    /// Only the model calls. A session start reaches for other things too —
    /// the npm registry among them — and those are the allowlist's business,
    /// not this test's.
    fn model_calls(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter(|line| line.contains("/model/"))
            .collect()
    }

    /// Wait for a model call to arrive at the proxy.
    async fn first_model_call(&self, within: Duration) -> Option<String> {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if let Some(first) = self.model_calls().into_iter().next() {
                return Some(first);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }
}

/// **The catalogue, live.** With `OPENCODE_DISABLE_MODELS_FETCH` set,
/// `OPENCODE_MODELS_PATH` naming the node's file, and every route out of the
/// process pointed at a port nothing listens on, the running server offers
/// exactly the models this node declared — no upstream catalogue, no
/// build-time snapshot, nothing probed.
#[tokio::test]
async fn the_binary_offers_exactly_the_declared_catalogue_with_egress_blocked() {
    if skip_no_binary() {
        return;
    }
    let node = start_node(
        &[("anthropic", SHAPE_ANTHROPIC), ("local", SHAPE_COMPATIBLE)],
        API_KEY,
        "127.0.0.1",
    )
    .await;
    // Nothing listens here: any fetch the binary tries fails rather than
    // silently succeeding against the real catalogue host.
    let blackhole = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{port}")
    };
    let live = launch(
        &node,
        "catalogue",
        "anthropic/test-model",
        vec![
            ("HTTP_PROXY".into(), blackhole.clone()),
            ("HTTPS_PROXY".into(), blackhole),
            ("NO_PROXY".into(), "127.0.0.1".into()),
        ],
    )
    .await;
    let listed = live.ask("/config/providers").await;
    live.shutdown().await;

    let mut offered: Vec<String> = Vec::new();
    for p in listed["providers"].as_array().expect("a provider list") {
        let id = p["id"].as_str().unwrap();
        for model in p["models"].as_object().unwrap().keys() {
            offered.push(format!("{id}/{model}"));
        }
    }
    offered.sort();
    assert_eq!(
        offered,
        ["anthropic/test-model", "local/test-model"],
        "the picker offers something the node did not declare: {listed}"
    );
}

/// A provider that answers without reporting usage is marked unmetered rather
/// than recorded as a turn that cost nothing (finding 10), through the real
/// binary's own request rather than a synthesised one. OpenCode maps a missing
/// `usage` to zero with no estimator; so would a gateway that counted only
/// what it understood. Zero tokens and unknown tokens are different facts, and
/// only one of them is safe to hold a budget against.
#[tokio::test]
async fn a_provider_that_omits_usage_is_marked_unmetered_not_free() {
    if skip_no_binary() {
        return;
    }
    let node = start_node(&[("local", SHAPE_COMPATIBLE)], API_KEY, "127.0.0.1").await;
    *node.upstream.silent.lock().unwrap() = true;
    let live = launch(&node, "unmetered", "local/test-model", Vec::new()).await;
    one_turn(&node, &live, "say ok", LIVE_CALL_TIMEOUT).await;
    let seen = node
        .at_the_gateway(Duration::from_secs(5))
        .await
        .expect("no provider call reached the gateway");
    live.shutdown().await;
    // The harness's own call and one replay of it, so the row is written
    // whichever of the two the gateway served first.
    let (status, body) = node.through_the_gateway(&seen).await;
    assert_eq!(status, 200, "{body}");

    // The row is written when the response stream ends, not when it starts.
    let mut counted = Vec::new();
    for _ in 0..100 {
        counted = node.store.usage_since(Some("work"), 0).unwrap();
        if !counted.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // More than one row only means the harness's call and the replay were
    // both served; what matters is that requests were made and none of them
    // could be counted.
    assert!(
        counted.iter().any(|row| row.requests > 0),
        "no request was recorded at all: {counted:?}"
    );
    assert!(
        counted
            .iter()
            .all(|row| row.input_tokens == 0 && row.output_tokens == 0),
        "something was counted for a provider that reported nothing: {counted:?}"
    );
    let settled = tracon::metrics::settle_turn(&node.store, "s-live", None, None);
    assert!(
        settled.unmetered(),
        "a provider that reported nothing settled as {settled:?} rather than unmetered"
    );
}

// ---------------------------------------------------------------------------
// 4. The pinned binary against a real self-hosted server
// ---------------------------------------------------------------------------

/// The self-hosted OpenAI-compatible endpoint this machine offers, if it has
/// one: an OpenAI-compatible base URL in `TRACON_LOCAL_OPENAI_BASE_URL` (the
/// llama.cpp default is assumed when it is unset), a key in
/// `TRACON_LOCAL_OPENAI_KEY` or in the file `TRACON_LOCAL_OPENAI_KEY_FILE`,
/// and a model id in `TRACON_LOCAL_OPENAI_MODEL`.
struct LocalServer {
    base: String,
    key: String,
    model: String,
}

const LLAMA_CPP_DEFAULT: &str = "http://127.0.0.1:8080";

/// How long a real turn may take before the test gives up on it. A model that
/// has to be loaded from disk first is slow once and fast afterwards.
const LIVE_TURN_TIMEOUT: Duration = Duration::from_secs(300);

/// The gateway's own count for this channel, once it has one with tokens in
/// both directions. `None` if none appears within `within`.
async fn counted_turn(store: &Store, within: Duration) -> Option<tracon::store::UsageTotal> {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        let totals = store.usage_since(Some("work"), 0).unwrap_or_default();
        if let Some(row) = totals
            .iter()
            .find(|row| row.input_tokens > 0 && row.output_tokens > 0)
        {
            return Some(row.clone());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

async fn local_server() -> Option<LocalServer> {
    let base = std::env::var("TRACON_LOCAL_OPENAI_BASE_URL")
        .unwrap_or_else(|_| LLAMA_CPP_DEFAULT.to_string());
    let key = match std::env::var("TRACON_LOCAL_OPENAI_KEY") {
        Ok(key) => key,
        Err(_) => match std::env::var("TRACON_LOCAL_OPENAI_KEY_FILE") {
            Ok(path) => std::fs::read_to_string(path).ok()?.trim().to_string(),
            Err(_) => String::new(),
        },
    };
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let listed: Value = http
        .get(format!("{base}/v1/models"))
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let model = match std::env::var("TRACON_LOCAL_OPENAI_MODEL") {
        Ok(model) => model,
        Err(_) => listed["data"]
            .as_array()?
            .iter()
            .filter_map(|m| m["id"].as_str())
            // An embedding model cannot hold a conversation.
            .find(|id| !id.to_ascii_lowercase().contains("embedding"))?
            .to_string(),
    };
    Some(LocalServer { base, key, model })
}

/// **Non-zero gateway counts for a self-hosted endpoint, live** — the one
/// open item in `providers.md` §6.3 that no fixture can close. The pinned
/// binary composes a real request for a real model, that request goes through
/// the real gateway to a real OpenAI-compatible server, and the gateway's own
/// count of what came back is non-zero in both directions and agrees with the
/// figures the server itself reported.
///
/// The runner has no route to the model server: the provider's `upstream` is
/// the node's binding to that exact service, and the harness only ever sees
/// the gateway's URL. That holds for every backend — Podman and Kubernetes
/// reach a host-local server through the gateway's egress, never through the
/// runner's own network — and `LocalRunner` is used here precisely because it
/// is the backend with no network of its own to confuse the question.
#[tokio::test]
async fn a_self_hosted_turn_is_counted_by_the_gateway() {
    if skip_no_binary() {
        return;
    }
    let Some(server) = local_server().await else {
        eprintln!(
            "skipped: no self-hosted OpenAI-compatible server answered. Set \
             TRACON_LOCAL_OPENAI_BASE_URL (default {LLAMA_CPP_DEFAULT}) and, if it needs one, \
             TRACON_LOCAL_OPENAI_KEY or TRACON_LOCAL_OPENAI_KEY_FILE."
        );
        return;
    };
    let credential = format!(
        r#"
        [credentials.cred]
        kind = "api_key"
        provider = "local"
        channels = ["work"]
        [credentials.cred.env]
        API_KEY = "{}"
        "#,
        server.key
    );
    let node = start_node_at(
        &[("local", SHAPE_COMPATIBLE)],
        &credential,
        "127.0.0.1",
        Some(&server.base),
    )
    .await;
    // The model the node declares is the one this server actually serves.
    let mut cfg = (*node.cfg).clone();
    if let Some(provider) = cfg.providers.get_mut("local") {
        provider.models = vec![ModelDecl {
            id: server.model.clone(),
            context: 32_768,
            output: 512,
            ..Default::default()
        }];
    }
    let node = Node {
        cfg: Arc::new(cfg),
        ..node
    };

    let live = launch(
        &node,
        "self-hosted",
        &format!("local/{}", server.model),
        Vec::new(),
    )
    .await;
    one_turn(
        &node,
        &live,
        "Reply with the single word: ok. Use no tools.",
        LIVE_CALL_TIMEOUT,
    )
    .await;
    let seen = node
        .at_the_gateway(Duration::from_secs(5))
        .await
        .expect("no provider call reached the gateway");
    live.shutdown().await;

    // The harness's own request, through the gateway, to the real server. A
    // model that has to be loaded from disk first takes as long as it takes.
    let answered = tokio::time::timeout(LIVE_TURN_TIMEOUT, node.through_the_gateway(&seen));
    let (status, body) = answered.await.expect("the model answered in time");
    assert_eq!(status, 200, "the self-hosted server refused: {body}");

    let counted = counted_turn(&node.store, Duration::from_secs(30))
        .await
        .unwrap_or_else(|| {
            panic!(
                "the gateway counted nothing for a self-hosted turn: {:?} — a provider that \
                 cannot be counted must be marked unmetered, not billed as zero",
                node.store.usage_since(Some("work"), 0).unwrap()
            )
        });
    assert!(
        counted.input_tokens > 0 && counted.output_tokens > 0,
        "{counted:?}"
    );
    assert!(
        !tracon::metrics::settle_turn(&node.store, "s-live", None, None).unmetered(),
        "a turn the gateway counted was settled as unmetered"
    );

    // And the count is the server's own, not an approximation of it: the
    // figures the gateway recorded are the ones in the stream it passed
    // through, which is what makes it the budget rather than a guess.
    let reported = body
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<Value>(line.trim_start_matches("data:").trim()).ok()
        })
        .filter_map(|event| {
            let usage = &event["usage"];
            let input = usage["prompt_tokens"]
                .as_i64()
                .or(usage["input_tokens"].as_i64())?;
            let output = usage["completion_tokens"]
                .as_i64()
                .or(usage["output_tokens"].as_i64())?;
            Some((input, output))
        })
        .next_back()
        .expect("the self-hosted server reported no usage at all");
    assert_eq!(
        (counted.input_tokens, counted.output_tokens),
        reported,
        "the gateway counted something other than what the server reported"
    );
    eprintln!(
        "self-hosted, live: {} in / {} out through the gateway, model {}",
        counted.input_tokens, counted.output_tokens, server.model
    );
}
