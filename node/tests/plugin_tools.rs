//! A plugin's tools brokered through the node's own MCP endpoint, against
//! the fake plugin (`tests/support/fake_plugin.rs`): offered only to a
//! channel binding a credential it declares, refused when what it lists
//! disagrees with its manifest, decided under `[[plugin]]` as an
//! `[[mcp_server]]` is, and called with a signed caller context that lends
//! only what the manifest declares and, for `lend = "approved"`, only to an
//! approved call.

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use support::plugin::Verifier;
use tracon::{
    broker::SharedBroker,
    config::{Config, PluginEntry},
    mcp::{CallContext, SessionAccess, Tools},
    plugin::{install, supervisor::State},
    policy::Policy,
    session::{Manager, OperatorAnswer},
    store::{approvals, Store},
    stream::Bus,
};

const FAKE: &str = r#"name = "fake"
version = "1.0.0"
contract = 1
exec = ["bin/fake"]
egress = ["db.internal:1521"]
formats = ["fake_wiki"]

[[credential]]
name = "fake-read"
pattern = "fake-read-*"

[[credential]]
name = "fake-write"
pattern = "fake-write-*"
lend = "approved"

[[tool]]
name = "query"
hint = "read"

[[tool]]
name = "describe"
hint = "read"

[[tool]]
name = "sleep"
hint = "read"

[[tool]]
name = "fail"
hint = "write"
consequential = true
"#;

/// What the fake lists for `FAKE`.
const FAKE_TOOLS: &str = "query\ndescribe\nsleep\nfail\n";

/// A plugin that declares no credential: offered to every channel.
const OPEN: &str = r#"name = "open"
version = "1.0.0"
contract = 1
exec = ["bin/open"]

[[tool]]
name = "query"
hint = "read"
"#;

struct Rig {
    store: Arc<Store>,
    tools: Arc<Tools>,
    manager: Manager,
    state: PathBuf,
    node: String,
    /// A session on a channel binding `fake-read-qa`, `fake-write-qa` and
    /// `github`.
    ctx: CallContext,
    /// A session on a channel binding nothing.
    bare: CallContext,
}

/// A policy that allows `names` by rule, as a signed bundle would.
fn allowing(names: &[&str]) -> Policy {
    let names: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
    let mut policy: Policy = toml::from_str(&format!(
        r#"
        version = 9
        [[rule]]
        id = "test-allow"
        verdict = "allow"
        reason = "Under test."
        kinds = ["tool"]
        matches = [{}]
        "#,
        names.join(", ")
    ))
    .unwrap();
    policy.trusted = true;
    policy
}

/// Each test its own channel: the binary's state directory, and so each
/// plugin's data directory, is shared.
fn rig(channel: &str, entries: Vec<PluginEntry>, policy: Policy) -> Rig {
    support::state::isolate();
    let identity = proto::keys::Identity::from_seed(&[7; 32]);
    let node = identity.node_id();
    let bare = format!("{channel}-bare");
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node(&node).unwrap();
    for (id, ch) in [(channel, channel), (bare.as_str(), bare.as_str())] {
        store.channel_put(ch, &[], "{}").unwrap();
        store
            .insert_session(&support::rows::session_row(id, &node, ch))
            .unwrap();
    }
    let state = Config::state_dir();
    support::plugin::install(&store, &state, FAKE);
    support::plugin::install(&store, &state, OPEN);
    for ch in [channel, bare.as_str()] {
        let data = install::data_dir(&state, "fake").join(ch);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("tools"), FAKE_TOOLS).unwrap();
    }
    let cfg = Arc::new(Config {
        plugin: entries,
        ..Default::default()
    });
    let broker: SharedBroker = Arc::new(
        toml::from_str(&format!(
            r#"
            [credentials.fake-read-qa]
            channels = ["{channel}"]
            [credentials.fake-read-qa.env]
            DB_USER = "reader"
            DB_PASSWORD = "read-secret"

            [credentials.fake-write-qa]
            channels = ["{channel}"]
            [credentials.fake-write-qa.env]
            DB_USER = "writer"
            DB_PASSWORD = "write-secret"

            [credentials.github]
            channels = ["{channel}"]
            [credentials.github.env]
            TOKEN = "gh-secret"
            "#
        ))
        .unwrap(),
    );
    let policy = Arc::new(parking_lot::RwLock::new(policy));
    let tools = Arc::new(Tools {
        broker,
        cfg: cfg.clone(),
        policy: policy.clone(),
        http: reqwest::Client::new(),
        session: Default::default(),
        upstream: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg,
        node.clone(),
        tools.clone(),
        policy,
        Arc::new(tracon::runner::local::LocalBackend),
    );
    manager.plugins().set_signer(identity);
    let _ = tools.session.set(SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    Rig {
        store,
        tools,
        manager,
        state,
        ctx: CallContext::session(channel, channel, &node),
        bare: CallContext::session(&bare, &bare, &node),
        node,
    }
}

impl Rig {
    async fn rpc(&self, ctx: &CallContext, method: &str, params: Value) -> Value {
        self.tools
            .handle(
                ctx,
                &json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }),
            )
            .await
            .unwrap()["result"]
            .clone()
    }

    async fn listed(&self, ctx: &CallContext) -> Vec<String> {
        self.rpc(ctx, "tools/list", json!({})).await["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    async fn call_as(&self, ctx: &CallContext, name: &str, args: Value) -> Value {
        self.rpc(
            ctx,
            "tools/call",
            json!({ "name": name, "arguments": args }),
        )
        .await
    }

    async fn call(&self, name: &str, args: Value) -> Value {
        self.call_as(&self.ctx, name, args).await
    }

    fn data(&self, plugin: &str, channel: &str) -> PathBuf {
        install::data_dir(&self.state, plugin).join(channel)
    }

    fn lines(&self, plugin: &str, channel: &str, file: &str) -> usize {
        std::fs::read_to_string(self.data(plugin, channel).join(file))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn status(&self, plugin: &str, channel: &str) -> tracon::plugin::supervisor::InstanceStatus {
        self.manager
            .plugins()
            .status()
            .into_iter()
            .find(|i| i.plugin == plugin && i.channel == channel)
            .expect("an instance")
    }

    /// Allow approval `id` and wait for the call it holds to settle.
    async fn approve(&self, id: &str) -> approvals::ApprovalRow {
        self.allow(id, None).await.unwrap();
        self.settled(id).await
    }

    /// Allow approval `id`, naming the preview it was read with.
    async fn allow(
        &self,
        id: &str,
        preview_hash: Option<&str>,
    ) -> Result<(), tracon::session::SessionError> {
        self.tools
            .answer_approval(
                id,
                OperatorAnswer {
                    option_id: tracon::adapter::types::OPTION_ALLOW_ONCE.into(),
                    preview_hash: preview_hash.map(str::to_string),
                    ..Default::default()
                },
            )
            .await
    }

    /// The card for approval `id` opened: its preview, if the plugin gave one.
    async fn open(&self, id: &str) -> Option<Value> {
        let row = self.store.get_approval(id).unwrap().unwrap();
        let args: Value = serde_json::from_str(&row.arguments).unwrap();
        self.tools.approval_preview(&row, &args).await
    }

    fn knob(&self, channel: &str, name: &str, value: &str) {
        std::fs::write(self.data("fake", channel).join(name), value).unwrap();
    }

    fn records(&self, channel: &str, file: &str) -> Vec<Value> {
        std::fs::read_to_string(self.data("fake", channel).join(file))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Wait for approval `id`'s call to settle.
    async fn settled(&self, id: &str) -> approvals::ApprovalRow {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let row = self.store.get_approval(id).unwrap().unwrap();
            if row.state != approvals::PENDING && row.state != approvals::RUNNING {
                return row;
            }
            assert!(Instant::now() < deadline, "the approved call settles");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// What the fake says it was sent.
fn echo(result: &Value) -> Value {
    assert_eq!(result["isError"], false, "{result}");
    serde_json::from_str(&text(result)).unwrap()
}

fn asked(result: &Value) -> String {
    let held: Value = serde_json::from_str(&text(result)).unwrap();
    assert_eq!(held["state"], "awaiting_operator", "{result}");
    held["approval_id"].as_str().unwrap().to_string()
}

fn allow(plugin: &str, tools: &[&str]) -> PluginEntry {
    PluginEntry {
        name: plugin.into(),
        allow: tools.iter().map(|t| t.to_string()).collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn tools_are_offered_to_a_channel_binding_a_credential_the_plugin_declares() {
    let rig = rig(
        "offered",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    let tools = rig.listed(&rig.ctx).await;
    for name in ["fake__query", "fake__describe", "open__query"] {
        assert!(tools.iter().any(|t| t == name), "{name}: {tools:?}");
    }
    // Nothing bound, so none of fake's; open declares none, so it is
    // offered everywhere.
    let bare = rig.listed(&rig.bare).await;
    assert!(bare.iter().all(|t| !t.starts_with("fake__")), "{bare:?}");
    assert!(bare.iter().any(|t| t == "open__query"), "{bare:?}");
    // Nor is a plan session offered any.
    let plan = rig
        .tools
        .list_for("offered", &rig.node, tracon::session::Phase::Plan);
    assert!(plan
        .iter()
        .all(|t| !t["name"].as_str().unwrap().contains("__")));

    // Knowing the name does not help: refused before anything is asked.
    let out = rig
        .call_as(&rig.bare, "fake__query", json!({ "sql": "SELECT 1" }))
        .await;
    assert_eq!(out["isError"], true);
    assert!(
        text(&out).contains("not offered on channel offered-bare"),
        "{out}"
    );
    assert!(rig
        .store
        .approvals_of_session("offered-bare")
        .unwrap()
        .is_empty());

    // A float has no canonical form: refused before it is decided.
    let out = rig.call("fake__describe", json!({ "ratio": 0.5 })).await;
    assert_eq!(out["isError"], true);
    assert!(text(&out).contains("pass decimals as strings"), "{out}");
    // A tool the manifest does not declare is not the plugin's.
    let out = rig.call("fake__drop", json!({})).await;
    assert!(text(&out).contains("declares no tool drop"), "{out}");
    assert!(rig
        .store
        .approvals_of_session("offered")
        .unwrap()
        .is_empty());
    assert_eq!(rig.lines("fake", "offered", "calls.jsonl"), 0);
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn a_plugin_whose_tools_disagree_with_its_manifest_is_refused() {
    let rig = rig(
        "mismatch",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    std::fs::write(
        rig.data("fake", "mismatch").join("tools"),
        "query\nsleep\nfail\nextra\n",
    )
    .unwrap();
    let tools = rig.listed(&rig.ctx).await;
    assert!(tools.iter().all(|t| !t.starts_with("fake__")), "{tools:?}");
    let status = rig.status("fake", "mismatch");
    assert_eq!(status.state, State::Unhealthy);
    let error = status.last_error.unwrap();
    assert!(
        error.contains("lists extra not declared")
            && error.contains("declares describe not listed"),
        "{error}"
    );
    // Its tools are refused with why, without a card, and it is not started
    // again for every ask.
    let out = rig.call("fake__query", json!({})).await;
    assert_eq!(out["isError"], true);
    assert!(text(&out).contains("disagree with its manifest"), "{out}");
    assert!(rig
        .store
        .approvals_of_session("mismatch")
        .unwrap()
        .is_empty());
    assert!(rig
        .manager
        .plugins()
        .ensure("fake", "mismatch")
        .await
        .is_err());
    assert_eq!(rig.lines("fake", "mismatch", "started"), 1);

    // Fixed and restarted, it serves.
    std::fs::write(rig.data("fake", "mismatch").join("tools"), FAKE_TOOLS).unwrap();
    rig.manager
        .plugins()
        .restart("fake", "mismatch")
        .await
        .unwrap();
    assert!(rig
        .listed(&rig.ctx)
        .await
        .iter()
        .any(|t| t == "fake__query"));
    echo(&rig.call("fake__query", json!({})).await);
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn plugin_entries_decide_as_mcp_server_entries_do() {
    let entry = PluginEntry {
        name: "fake".into(),
        allow: vec!["query".into()],
        ask: vec!["sleep".into()],
        deny: vec!["describe".into()],
        ..Default::default()
    };
    let rig = rig(
        "entries",
        vec![entry],
        allowing(&["fake__sleep", "open__query"]),
    );
    rig.listed(&rig.ctx).await;
    // Allowed by its entry: runs at once.
    echo(&rig.call("fake__query", json!({ "sql": "SELECT 1" })).await);
    assert_eq!(rig.lines("fake", "entries", "calls.jsonl"), 1);
    // Denied by its entry: refused, and nothing reaches the plugin.
    let out = rig.call("fake__describe", json!({})).await;
    assert_eq!(out["isError"], true);
    assert!(text(&out).contains("plugin.fake.deny"), "{out}");
    // Its ask holds over the policy's allow.
    asked(&rig.call("fake__sleep", json!({ "ms": 1 })).await);
    // A tool neither names is asked.
    asked(&rig.call("fake__fail", json!({})).await);
    assert_eq!(rig.lines("fake", "entries", "calls.jsonl"), 1);
    // The signed policy alone allows a tool of a plugin with no entry.
    echo(&rig.call("open__query", json!({})).await);
    assert_eq!(rig.lines("open", "entries", "calls.jsonl"), 1);
    // A session's explained standing agrees.
    let session = rig.store.get_session("entries").unwrap().unwrap();
    let explained =
        tracon::explain::session_authority(&rig.store, &rig.tools, &rig.tools.cfg, &session, None);
    let standing = |name: &str| {
        explained
            .actions
            .iter()
            .find(|a| a.name == name)
            .map(|a| (a.standing.verdict, a.standing.rule_id.clone()))
    };
    use tracon::policy::Verdict;
    assert_eq!(
        standing("fake__query"),
        Some((Verdict::Allow, Some("plugin.fake.allow".into())))
    );
    assert_eq!(
        standing("fake__describe"),
        Some((Verdict::Deny, Some("plugin.fake.deny".into())))
    );
    assert_eq!(standing("fake__fail").map(|s| s.0), Some(Verdict::Ask));
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn each_call_carries_a_signed_caller_context_bound_to_its_instance() {
    let rig = rig("meta", vec![allow("fake", &["query"])], Policy::shipped());
    rig.listed(&rig.ctx).await;
    let args = json!({ "sql": "SELECT 1", "limit": 5, "params": { "k": "v" } });
    let seen = echo(&rig.call("fake__query", args.clone()).await);
    let meta = &seen["meta"];
    assert_eq!(seen["node_key"], rig.node.as_str());
    assert_eq!(seen["arguments"], args);
    assert_eq!(meta["plugin"], "fake");
    assert_eq!(meta["tool"], "query");
    assert_eq!(meta["instance"], seen["own_instance"]);
    assert_eq!(
        meta["caller"],
        json!({ "kind": "agent", "session": "meta", "harness": "fake" })
    );
    assert_eq!(meta["channel"], "meta");
    assert_eq!(meta["approval_id"], Value::Null);
    let issued = meta["issued_ms"].as_i64().unwrap();
    let expires = meta["expires_ms"].as_i64().unwrap();
    assert_eq!(expires - issued, 30_000);
    let own = seen["own_instance"].as_str().unwrap();
    let mut plugin = Verifier::new("fake", own, &rig.node);
    plugin.verify(meta, "query", &args, issued).unwrap();
    // Once only, and not once expired.
    assert_eq!(
        plugin.verify(meta, "query", &args, issued).unwrap_err().0,
        6
    );
    assert_eq!(
        Verifier::new("fake", own, &rig.node)
            .verify(meta, "query", &args, expires + 5_001)
            .unwrap_err()
            .0,
        5
    );
    // Nor for other arguments, another tool, or another process.
    let fresh = || Verifier::new("fake", own, &rig.node);
    assert_eq!(
        fresh()
            .verify(meta, "query", &json!({ "sql": "DROP" }), issued)
            .unwrap_err()
            .0,
        3
    );
    assert_eq!(
        Verifier::new("fake", &"0".repeat(64), &rig.node)
            .verify(meta, "query", &args, issued)
            .unwrap_err()
            .0,
        2
    );
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn credentials_are_lent_per_call_and_an_approved_only_one_only_when_approved() {
    let rig = rig("lend", vec![allow("fake", &["query"])], Policy::shipped());
    rig.listed(&rig.ctx).await;
    // Allowed by the entry: the read credential, not the approved-only one,
    // and never one the manifest does not declare.
    let seen = echo(&rig.call("fake__query", json!({})).await);
    let lent = seen["meta"]["credentials"].as_object().unwrap();
    let names: Vec<&String> = lent.keys().collect();
    assert_eq!(names, ["fake-read-qa"], "{lent:?}");
    assert_eq!(
        lent["fake-read-qa"],
        json!({ "DB_USER": "reader", "DB_PASSWORD": "read-secret" })
    );

    // Asked and approved: both, with the card it was approved on.
    let id = asked(&rig.call("fake__describe", json!({ "table": "apps" })).await);
    let settled = rig.approve(&id).await;
    assert_eq!(settled.state, approvals::SUCCEEDED, "{:?}", settled.reason);
    let result: Value = serde_json::from_str(settled.result.as_deref().unwrap()).unwrap();
    let seen = echo(&json!({ "content": result["content"], "isError": false }));
    let meta = &seen["meta"];
    assert_eq!(meta["approval_id"], id.as_str());
    let mut names: Vec<&String> = meta["credentials"].as_object().unwrap().keys().collect();
    names.sort();
    assert_eq!(names, ["fake-read-qa", "fake-write-qa"]);
    assert_eq!(meta["credentials"]["fake-write-qa"]["DB_USER"], "writer");
    assert!(meta["credentials"].get("github").is_none());

    // None of them was ever in its environment.
    let env = std::fs::read_to_string(rig.data("fake", "lend").join("env.json")).unwrap();
    let env: serde_json::Map<String, Value> = serde_json::from_str(&env).unwrap();
    let mut keys: Vec<&str> = env.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        ["TRACON_CONTRACT", "TRACON_NODE_KEY", "TRACON_PLUGIN_TOKEN"]
    );
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn a_call_after_a_restart_carries_the_new_instances_token() {
    let rig = rig(
        "restart",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    rig.listed(&rig.ctx).await;
    let first = echo(&rig.call("fake__query", json!({})).await);
    assert_eq!(first["meta"]["instance"], first["own_instance"]);
    rig.manager
        .plugins()
        .restart("fake", "restart")
        .await
        .unwrap();
    let second = echo(&rig.call("fake__query", json!({})).await);
    assert_eq!(second["meta"]["instance"], second["own_instance"]);
    assert_ne!(second["own_instance"], first["own_instance"]);
    // A plugin that forgets the MCP session is asked again in a new one,
    // with a context of its own.
    std::fs::write(rig.data("fake", "restart").join("forget"), "").unwrap();
    let third = echo(&rig.call("fake__query", json!({})).await);
    assert_ne!(third["meta"]["nonce"], second["meta"]["nonce"]);
    assert_eq!(rig.lines("fake", "restart", "calls.jsonl"), 3);
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn a_call_in_flight_holds_off_the_idle_stop() {
    let entry = PluginEntry {
        idle_timeout_secs: 1,
        ..allow("fake", &["query", "sleep"])
    };
    let rig = rig("idle", vec![entry], Policy::shipped());
    rig.listed(&rig.ctx).await;
    // Longer than the idle timeout: the process stays for it.
    echo(&rig.call("fake__sleep", json!({ "ms": 2500 })).await);
    assert_eq!(rig.lines("fake", "idle", "started"), 1);
    // Then it goes, and the next call starts it again.
    let deadline = Instant::now() + Duration::from_secs(5);
    while rig.status("fake", "idle").state != State::Stopped {
        assert!(Instant::now() < deadline, "an idle stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    echo(&rig.call("fake__query", json!({})).await);
    assert_eq!(rig.lines("fake", "idle", "started"), 2);
    rig.manager.plugins().shutdown().await;
}

fn sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// What an approved call's fake saw, from its settled row.
fn approved_echo(settled: &approvals::ApprovalRow) -> Value {
    assert_eq!(settled.state, approvals::SUCCEEDED, "{:?}", settled.reason);
    let result: Value = serde_json::from_str(settled.result.as_deref().unwrap()).unwrap();
    echo(&json!({ "content": result["content"], "isError": false }))
}

#[tokio::test]
async fn a_preview_is_shown_and_its_token_kept() {
    let rig = rig(
        "preview",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    rig.listed(&rig.ctx).await;
    rig.knob("preview", "preview", "on");
    let id = asked(&rig.call("fake__describe", json!({ "sql": "DROP x" })).await);
    let shown = rig.open(&id).await.expect("a preview");
    let preview = &shown["preview"];
    assert_eq!(preview["status"], "shown", "{shown}");
    assert_eq!(preview["previewable"], true);
    assert_eq!(preview["summary"], "fake describe");
    assert!(preview["body"].as_str().unwrap().contains("+ DROP x"));
    assert_eq!(preview["token_sha256"], sha256("v1"));
    assert!(preview.get("token").is_none());
    assert_eq!(rig.store.preview_token(&id).unwrap().as_deref(), Some("v1"));
    // Previewed as an asked call: no approval, nothing lent only on one.
    let previews = rig.records("preview", "previews.jsonl");
    assert_eq!(previews.len(), 1);
    let ctx = &previews[0];
    assert_eq!(ctx["approval_id"], Value::Null);
    assert!(ctx.get("preview_token").is_none());
    assert_eq!(ctx["tool"], "describe");
    assert!(ctx["credentials"].get("fake-write-qa").is_none(), "{ctx}");

    let seen = approved_echo(&rig.approve(&id).await);
    assert_eq!(seen["meta"]["preview_token"], "v1");
    assert_eq!(seen["meta"]["approval_id"], id.as_str());
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn an_approval_gone_stale_between_page_and_dispatch_fails_as_stale() {
    let rig = rig("stale", vec![allow("fake", &["query"])], Policy::shipped());
    rig.listed(&rig.ctx).await;
    rig.knob("stale", "preview", "on");
    let id = asked(&rig.call("fake__describe", json!({ "sql": "UPDATE" })).await);
    rig.open(&id).await.expect("a preview");
    // The target moves after the operator read the card.
    rig.knob("stale", "version", "v2");
    let settled = rig.approve(&id).await;
    assert_eq!(settled.state, approvals::FAILED);
    let reason = settled.reason.as_deref().unwrap();
    assert!(reason.starts_with(approvals::STALE_REASON), "{reason}");
    assert!(
        reason.contains("the issue moved since the preview"),
        "{reason}"
    );
    assert!(reason.contains("Call fake__describe again"), "{reason}");
    let status = rig
        .call("approval_status", json!({ "approval_id": id }))
        .await;
    let status: Value = serde_json::from_str(&text(&status)).unwrap();
    assert_eq!(status["state"], "failed");
    assert_eq!(status["stale"], true);
    let calls = rig.records("stale", "calls.jsonl");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["refused"], "stale");
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn a_plugin_without_preview_gets_the_generic_card() {
    let rig = rig(
        "nopreview",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    rig.listed(&rig.ctx).await;
    let id = asked(&rig.call("fake__describe", json!({ "sql": "x" })).await);
    assert!(rig.open(&id).await.is_none());
    assert_eq!(rig.store.preview_token(&id).unwrap(), None);
    let seen = approved_echo(&rig.approve(&id).await);
    assert!(seen["meta"].get("preview_token").is_none(), "{seen}");
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn a_refused_preview_is_shown_and_the_call_can_still_be_decided() {
    let rig = rig(
        "refused",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    rig.listed(&rig.ctx).await;
    rig.knob("refused", "preview", "refuse");
    let id = asked(&rig.call("fake__describe", json!({ "sql": "x" })).await);
    let shown = rig.open(&id).await.expect("the refusal");
    assert_eq!(shown["preview"]["status"], "refused");
    assert!(
        shown["preview"]["error"]
            .as_str()
            .unwrap()
            .contains("plugin fake: cannot preview"),
        "{shown}"
    );
    assert_eq!(rig.store.preview_token(&id).unwrap(), None);
    let seen = approved_echo(&rig.approve(&id).await);
    assert!(seen["meta"].get("preview_token").is_none());
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn an_unpreviewable_call_keeps_no_token() {
    let rig = rig(
        "unpreviewable",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    rig.listed(&rig.ctx).await;
    rig.knob("unpreviewable", "preview", "unpreviewable");
    let id = asked(&rig.call("fake__describe", json!({ "sql": "x" })).await);
    let shown = rig.open(&id).await.expect("a preview");
    assert_eq!(shown["preview"]["status"], "shown");
    assert_eq!(shown["preview"]["previewable"], false);
    assert_eq!(shown["preview"]["token_sha256"], Value::Null);
    assert_eq!(rig.store.preview_token(&id).unwrap(), None);
    let seen = approved_echo(&rig.approve(&id).await);
    assert!(seen["meta"].get("preview_token").is_none());
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn an_answer_bound_to_an_older_preview_is_refused() {
    let rig = rig(
        "twotabs",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    rig.listed(&rig.ctx).await;
    rig.knob("twotabs", "preview", "on");
    let id = asked(&rig.call("fake__describe", json!({ "sql": "x" })).await);
    let first = rig.open(&id).await.unwrap();
    let h1 = first["preview"]["token_sha256"]
        .as_str()
        .unwrap()
        .to_string();
    rig.knob("twotabs", "version", "v2");
    let second = rig.open(&id).await.unwrap();
    let h2 = second["preview"]["token_sha256"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(h1, h2);
    let refused = rig.allow(&id, Some(&h1)).await.unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("the preview changed since you read it"),
        "{refused}"
    );
    assert_eq!(
        rig.store.get_approval(&id).unwrap().unwrap().state,
        approvals::PENDING
    );
    // Answered on what the second tab read, it runs against that.
    rig.allow(&id, Some(&h2)).await.unwrap();
    let seen = approved_echo(&rig.settled(&id).await);
    assert_eq!(seen["meta"]["preview_token"], "v2");
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn prose_in_a_plugin_format_is_rendered_by_its_owner() {
    let rig = rig(
        "formats",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    rig.knob("formats", "marked", "");
    rig.listed(&rig.ctx).await;
    let shown = rig.tools.plugin_presentation("fake__describe").unwrap();
    assert_eq!(shown.prose_fields, ["sql"]);
    assert_eq!(shown.plugin_format.as_deref(), Some("fake_wiki"));
    assert_eq!(shown.format, "markdown");
    assert_eq!(
        rig.tools
            .convert("formats", "fake_wiki", "markdown", "h1. T\nsome *b*")
            .await
            .unwrap(),
        "# T\nsome **b**"
    );
    assert_eq!(
        rig.tools
            .convert("formats", "fake_wiki", "fake_wiki", "# T")
            .await
            .unwrap(),
        "h1. T"
    );
    let rendered = rig
        .tools
        .approval_rendering("formats", &shown, &json!({ "sql": "h1. Up" }))
        .await
        .unwrap();
    assert_eq!(rendered["rendered"]["sql"], "# Up", "{rendered}");

    // Without its owner, nothing renders it, and the card shows the source.
    rig.store.plugin_remove("fake", false).unwrap();
    let refused = rig
        .tools
        .convert("formats", "fake_wiki", "markdown", "h1. T")
        .await
        .unwrap_err();
    assert!(refused.contains("no installed plugin owns"), "{refused}");
    let rendered = rig
        .tools
        .approval_rendering("formats", &shown, &json!({ "sql": "h1. Up" }))
        .await
        .unwrap();
    assert!(rendered.get("rendered").is_none());
    assert!(rendered["render_error"].is_string(), "{rendered}");
    rig.manager.plugins().shutdown().await;
}
