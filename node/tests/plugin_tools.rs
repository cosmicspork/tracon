//! A plugin's tools brokered through the node's own MCP endpoint, against
//! the fake plugin (`tests/support/fake_plugin.rs`): offered only to a
//! channel binding a credential it declares, refused when what it lists
//! disagrees with its manifest, decided under `[[plugin]]` as an
//! `[[mcp_server]]` is, and called with a signed caller context that lends
//! only what the manifest declares and, for `lend = "approved"`, only to an
//! approved call, which runs only on the build its card showed and, unless
//! the plugin's entry opts out, only once its card showed a preview of the
//! call as it would run.

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

    /// This rig's node as it is once restarted: a new manager and tools on
    /// the same store, state and configuration, with nothing in memory.
    fn restarted(&self) -> Rig {
        let identity = proto::keys::Identity::from_seed(&[7; 32]);
        let policy = self.tools.policy.clone();
        let tools = Arc::new(Tools {
            broker: self.tools.broker.clone(),
            cfg: self.tools.cfg.clone(),
            policy: policy.clone(),
            http: reqwest::Client::new(),
            session: Default::default(),
            upstream: Default::default(),
        });
        let manager = Manager::new(
            self.store.clone(),
            Bus::new(),
            self.tools.cfg.clone(),
            self.node.clone(),
            tools.clone(),
            policy,
            Arc::new(tracon::runner::local::LocalBackend),
        );
        manager.plugins().set_signer(identity);
        let _ = tools.session.set(SessionAccess {
            store: self.store.clone(),
            manager: manager.clone(),
        });
        Rig {
            store: self.store.clone(),
            tools,
            manager,
            state: self.state.clone(),
            node: self.node.clone(),
            ctx: self.ctx.clone(),
            bare: self.bare.clone(),
        }
    }

    /// The plugin `plugin`'s tools as a `tools/list` on `ctx` offers them.
    async fn offered(&self, ctx: &CallContext, plugin: &str) -> Vec<Value> {
        let prefix = format!("{plugin}__");
        self.rpc(ctx, "tools/list", json!({})).await["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["name"].as_str().unwrap().starts_with(&prefix))
            .cloned()
            .collect()
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

/// `entry`, with Allow on its plugin's cards needing no preview.
fn unpreviewed(entry: PluginEntry) -> PluginEntry {
    PluginEntry {
        require_preview: false,
        ..entry
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
    assert!(rig.manager.plugins().listed("fake").is_none());

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
async fn listing_a_plugins_known_tools_starts_no_process() {
    let entry = PluginEntry {
        idle_timeout_secs: 1,
        ..allow("fake", &["query"])
    };
    let rig = rig("lazy", vec![entry], Policy::shipped());
    // The first listing of a build starts it once, to learn its tools.
    let first = rig.offered(&rig.ctx, "fake").await;
    assert_eq!(first.len(), 4, "{first:?}");
    assert_eq!(rig.lines("fake", "lazy", "started"), 1);
    let fail = first.iter().find(|t| t["name"] == "fake__fail").unwrap();
    assert!(
        fail["inputSchema"]["properties"]["operation_id"].is_object(),
        "{fail}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while rig.status("fake", "lazy").state != State::Stopped {
        assert!(Instant::now() < deadline, "an idle stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Listed again, its tools are offered exactly as before, and nothing
    // starts.
    assert_eq!(rig.offered(&rig.ctx, "fake").await, first);
    assert_eq!(rig.lines("fake", "lazy", "started"), 1);
    assert_eq!(rig.status("fake", "lazy").state, State::Stopped);
    // Nor once the node restarts: what the build listed is kept.
    rig.manager.plugins().shutdown().await;
    let rig = rig.restarted();
    assert_eq!(rig.offered(&rig.ctx, "fake").await, first);
    assert_eq!(rig.lines("fake", "lazy", "started"), 1);
    assert!(rig
        .manager
        .plugins()
        .status()
        .iter()
        .all(|i| i.plugin != "fake"));
    // So does the schema an approval page reads.
    assert!(rig.tools.plugin_schema("fake__fail").is_some());

    // A call starts it.
    echo(&rig.call("fake__query", json!({})).await);
    assert_eq!(rig.lines("fake", "lazy", "started"), 2);

    // An update forgets what the build before listed: the next listing
    // starts the new build to learn its own.
    support::plugin::install_build(
        &rig.store,
        &rig.state,
        &FAKE.replace("version = \"1.0.0\"", "version = \"1.1.0\""),
        &"fd".repeat(32),
    );
    rig.manager.plugins().stop_plugin("fake").await;
    assert_eq!(rig.offered(&rig.ctx, "fake").await.len(), 4);
    assert_eq!(rig.lines("fake", "lazy", "started"), 3);
    assert_eq!(rig.status("fake", "lazy").version.as_deref(), Some("1.1.0"));
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
    asked(
        &rig.call("fake__fail", json!({ "operation_id": "entries-fail-1" }))
            .await,
    );
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
    // And so does what the operator is shown of each tool.
    let build = rig
        .store
        .plugin_get("fake")
        .unwrap()
        .unwrap()
        .active
        .unwrap();
    let shown: Vec<Value> = rig.tools.plugin_tool_status(&build.manifest);
    let shown = |name: &str| {
        let tool = shown.iter().find(|t| t["name"] == name).unwrap();
        (
            tool["setting"].clone(),
            tool["decision"].clone(),
            tool["rule_id"].clone(),
        )
    };
    assert_eq!(
        shown("fake__query"),
        (json!("allow"), json!("allow"), json!("plugin.fake.allow"))
    );
    assert_eq!(
        shown("fake__describe"),
        (json!("deny"), json!("deny"), json!("plugin.fake.deny"))
    );
    assert_eq!(
        shown("fake__sleep"),
        (json!("ask"), json!("ask"), json!("plugin.fake.ask"))
    );
    assert_eq!(
        shown("fake__fail"),
        (Value::Null, json!("ask"), Value::Null)
    );
    let open = rig
        .store
        .plugin_get("open")
        .unwrap()
        .unwrap()
        .active
        .unwrap();
    let open = rig.tools.plugin_tool_status(&open.manifest);
    assert_eq!(open[0]["decision"], "allow");
    assert_eq!(open[0]["rule_id"], "test-allow");
    assert_eq!(open[0]["hint"], "read");
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
    let rig = rig(
        "lend",
        vec![unpreviewed(allow("fake", &["query"]))],
        Policy::shipped(),
    );
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

/// The bindings each call was lent, by name, sorted.
fn lent_names(seen: &Value) -> Vec<String> {
    let mut names: Vec<String> = seen["meta"]["credentials"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    names.sort();
    names
}

/// What a card says Allow lends, by binding, and why it would refuse.
fn card_lends(context: &Value) -> (Vec<String>, Vec<String>) {
    let credentials = context["credentials"].as_array().unwrap();
    let bindings = credentials
        .iter()
        .flat_map(|c| c["bindings"].as_array().unwrap().clone())
        .map(|b| b.as_str().unwrap().to_string())
        .collect();
    let refusals = credentials
        .iter()
        .filter_map(|c| c["refusal"].as_str().map(str::to_string))
        .collect();
    (bindings, refusals)
}

#[tokio::test]
async fn a_selecting_credential_lends_only_the_profile_the_call_names() {
    let rig = rig(
        "select",
        vec![unpreviewed(allow("fake", &["query"]))],
        Policy::shipped(),
    );
    // The build declares `select = "profile"` on both credentials, and the
    // channel binds two profiles of each.
    support::plugin::install_build(
        &rig.store,
        &rig.state,
        &FAKE
            .replace("version = \"1.0.0\"", "version = \"1.1.0\"")
            .replace(
                "pattern = \"fake-read-*\"",
                "pattern = \"fake-read-*\"\nselect = \"profile\"",
            )
            .replace(
                "pattern = \"fake-write-*\"",
                "pattern = \"fake-write-*\"\nselect = \"profile\"",
            ),
        &"5e".repeat(32),
    );
    rig.manager.plugins().stop_plugin("fake").await;
    let mut bindings = String::new();
    for name in [
        "fake-read-tst",
        "fake-read-prd",
        "fake-write-tst",
        "fake-write-prd",
    ] {
        bindings.push_str(&format!(
            "[credentials.{name}]\nchannels = [\"select\"]\n[credentials.{name}.env]\nDB_USER = \"{name}\"\n\n"
        ));
    }
    *rig.tools.broker.write().unwrap() = toml::from_str(&bindings).unwrap();
    rig.listed(&rig.ctx).await;

    // Allowed outright: only the read account of the profile it names.
    let seen = echo(&rig.call("fake__query", json!({ "profile": "tst" })).await);
    assert_eq!(lent_names(&seen), ["fake-read-tst"]);

    // Asked: the card says Allow lends that profile's read and write
    // accounts, and the approved run is lent exactly those.
    let id = asked(
        &rig.call("fake__describe", json!({ "profile": "tst" }))
            .await,
    );
    let row = rig.store.get_approval(&id).unwrap().unwrap();
    let card = rig.tools.plugin_approval_context(&row, None).unwrap();
    assert_eq!(card["credentials"][0]["select"], "profile");
    assert_eq!(
        card_lends(&card),
        (
            vec!["fake-read-tst".into(), "fake-write-tst".into()],
            vec![]
        )
    );
    let seen = approved_echo(&rig.approve(&id).await);
    assert_eq!(lent_names(&seen), ["fake-read-tst", "fake-write-tst"]);

    // An operator's edit of the profile changes what the card says and what
    // the run is lent.
    let id = asked(
        &rig.call("fake__describe", json!({ "profile": "tst" }))
            .await,
    );
    let row = rig.store.get_approval(&id).unwrap().unwrap();
    let edit = json!({ "profile": "prd" });
    let previewed = rig.tools.preview_edit(&row, &edit).await.unwrap().unwrap();
    assert_eq!(
        card_lends(&previewed["plugin"]),
        (
            vec!["fake-read-prd".into(), "fake-write-prd".into()],
            vec![]
        )
    );
    rig.tools
        .answer_approval(
            &id,
            OperatorAnswer {
                option_id: tracon::adapter::types::OPTION_ALLOW_ONCE.into(),
                arguments: Some(edit.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let seen = approved_echo(&rig.settled(&id).await);
    assert_eq!(seen["arguments"], edit);
    assert_eq!(lent_names(&seen), ["fake-read-prd", "fake-write-prd"]);

    // A profile the channel does not bind is refused before anything is
    // sent, naming the binding, and its card says so.
    let calls = rig.lines("fake", "select", "calls.jsonl");
    let out = rig.call("fake__query", json!({ "profile": "nope" })).await;
    assert_eq!(out["isError"], true, "{out}");
    assert!(
        text(&out).contains("names binding fake-read-nope, which this channel does not bind"),
        "{out}"
    );
    let id = asked(
        &rig.call("fake__describe", json!({ "profile": "nope" }))
            .await,
    );
    let row = rig.store.get_approval(&id).unwrap().unwrap();
    let (lent, refusals) = card_lends(&rig.tools.plugin_approval_context(&row, None).unwrap());
    assert!(lent.is_empty(), "{lent:?}");
    assert_eq!(refusals.len(), 2, "{refusals:?}");
    assert!(refusals[1].contains("fake-write-nope"), "{refusals:?}");
    let refused = rig.approve(&id).await;
    assert_eq!(refused.state, approvals::FAILED);
    let reason = refused.reason.unwrap_or_default();
    assert!(reason.contains("nothing ran"), "{reason}");
    assert!(reason.contains("fake-read-nope"), "{reason}");
    assert_eq!(rig.lines("fake", "select", "calls.jsonl"), calls);
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
    // Decided, the approval keeps the preview the operator approved, for
    // its page to show; never its token.
    let asked = approvals::arguments_hash(&json!({ "sql": "DROP x" }).to_string());
    let kept: Value =
        serde_json::from_str(&rig.store.preview_shown(&id, &asked).unwrap().unwrap()).unwrap();
    assert_eq!(kept["summary"], "fake describe");
    assert_eq!(kept["previewable"], true);
    assert_eq!(kept["body"], preview["body"]);
    assert!(kept.get("token_sha256").is_none() && kept.get("token").is_none());
    rig.manager.plugins().shutdown().await;
}

/// A plugin call's card is titled in words, without its arguments as JSON or
/// its bind values, and once its preview is shown, by the preview's summary.
#[tokio::test]
async fn a_plugin_calls_card_is_titled_by_its_target_then_its_preview() {
    let rig = rig("titled", vec![allow("fake", &["query"])], Policy::shipped());
    rig.listed(&rig.ctx).await;
    let id = asked(
        &rig.call(
            "fake__describe",
            json!({
                "profile": "qa",
                "params": { "v": "PlugTestA" },
                "sql": "UPDATE t SET name = :v\nWHERE id = 7",
            }),
        )
        .await,
    );
    let title = rig.store.get_approval(&id).unwrap().unwrap().title;
    assert_eq!(title, "fake: describe on qa · UPDATE t SET name = :v");
    assert!(
        !title.contains('{') && !title.contains("PlugTestA"),
        "{title}"
    );

    rig.knob("titled", "preview", "on");
    let shown = rig.open(&id).await.expect("a preview");
    assert_eq!(shown["preview"]["status"], "shown", "{shown}");
    assert_eq!(
        rig.store.get_approval(&id).unwrap().unwrap().title,
        "fake: describe on qa · describe"
    );
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
async fn a_plugin_without_preview_gets_the_generic_card_and_is_allowed_only_once_opted_out() {
    let rig = rig(
        "nopreview",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    rig.listed(&rig.ctx).await;
    let id = asked(&rig.call("fake__describe", json!({ "sql": "x" })).await);
    // Not even opened: nothing was shown, and the answer is sent to the card.
    let refused = rig.allow(&id, None).await.unwrap_err().to_string();
    assert!(refused.contains("open the card to allow it"), "{refused}");
    // Opened: the plugin answers -32601, so the card is the generic one,
    // and it says why Allow is held and what would let it through.
    let shown = rig.open(&id).await.expect("the card's hold");
    assert!(shown.get("preview").is_none(), "{shown}");
    assert_eq!(
        shown["allow_blocked"],
        "fake offered no preview of this call; allow without one by setting \
         `require_preview = false` under `[[plugin]] name = \"fake\"`"
    );
    assert_eq!(rig.store.preview_token(&id).unwrap(), None);
    let refused = rig.allow(&id, None).await.unwrap_err().to_string();
    assert!(
        refused.contains("`require_preview = false` under `[[plugin]] name = \"fake\"`"),
        "{refused}"
    );
    assert_eq!(rig.lines("fake", "nopreview", "calls.jsonl"), 0);
    assert_eq!(
        rig.store.get_approval(&id).unwrap().unwrap().state,
        approvals::PENDING
    );

    // The operator opts the plugin out, live: the same card is allowed.
    let path = rig.state.join("nopreview-node.toml");
    let text = "[[plugin]]\nname = \"fake\"\nallow = [\"query\"]\nrequire_preview = false\n";
    std::fs::write(&path, text).unwrap();
    let newer: Config = toml::from_str(text).unwrap();
    rig.tools.cfg.apply_live_at(
        &path,
        &newer,
        &tracon::config::edit::file_hash(&path).unwrap(),
    );
    assert!(rig.open(&id).await.is_none(), "no hold once opted out");
    let seen = approved_echo(&rig.approve(&id).await);
    assert!(seen["meta"].get("preview_token").is_none(), "{seen}");
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn a_refused_preview_is_shown_and_holds_allow_but_not_reject() {
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
    assert!(
        shown["allow_blocked"]
            .as_str()
            .unwrap()
            .starts_with("fake offered no preview of this call"),
        "{shown}"
    );
    assert_eq!(rig.store.preview_token(&id).unwrap(), None);
    let refused = rig.allow(&id, None).await.unwrap_err().to_string();
    assert!(refused.contains("no preview of this call"), "{refused}");
    // Rejecting needs no preview.
    rig.tools
        .answer_approval(
            &id,
            OperatorAnswer {
                option_id: tracon::adapter::types::OPTION_REJECT_ONCE.into(),
                reason: Some("not without seeing it".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let row = rig.store.get_approval(&id).unwrap().unwrap();
    assert_eq!(row.state, approvals::REJECTED);
    assert_eq!(rig.lines("fake", "refused", "calls.jsonl"), 0);

    // Nor does asking for changes, on another call it would not preview.
    let other = asked(&rig.call("fake__describe", json!({ "sql": "y" })).await);
    rig.tools
        .answer_approval(
            &other,
            OperatorAnswer {
                option_id: tracon::store::approvals::OPTION_REQUEST_CHANGES.into(),
                notes: Some("narrow it".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let row = rig.store.get_approval(&other).unwrap().unwrap();
    assert_eq!(row.state, approvals::CHANGES_REQUESTED);
    rig.manager.plugins().shutdown().await;
}

/// The rule is held where every answer lands, not on the page: an answer
/// through the session manager, as the API, a peer's relay and the queue's
/// card send it, is refused until the card has shown a preview, and allowed
/// once it has.
#[tokio::test]
async fn an_answer_from_outside_the_page_is_held_to_the_shown_preview() {
    let rig = rig("api", vec![allow("fake", &["query"])], Policy::shipped());
    rig.listed(&rig.ctx).await;
    rig.knob("api", "preview", "on");
    let id = asked(&rig.call("fake__describe", json!({ "sql": "x" })).await);
    let allow = || OperatorAnswer {
        option_id: tracon::adapter::types::OPTION_ALLOW_ONCE.into(),
        by: Some("api".into()),
        ..Default::default()
    };
    let refused = rig
        .manager
        .answer(&id, allow())
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("open the card to allow it"), "{refused}");
    assert_eq!(rig.lines("fake", "api", "calls.jsonl"), 0);
    assert!(rig.records("api", "previews.jsonl").is_empty());
    rig.open(&id).await.expect("a preview");
    rig.manager.answer(&id, allow()).await.unwrap();
    let seen = approved_echo(&rig.settled(&id).await);
    assert_eq!(seen["meta"]["preview_token"], "v1");
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
    // Shown, though it could not show the effect: Allow is not held.
    assert!(shown.get("allow_blocked").is_none(), "{shown}");
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

/// The target moved after the card opened and nothing has opened it since:
/// Allow takes the preview again, sees another token, and refuses before
/// anything is decided, so the page can show the fresh preview.
#[tokio::test]
async fn an_allow_whose_fresh_preview_differs_is_refused_as_stale_and_stays_pending() {
    let rig = rig("moved", vec![allow("fake", &["query"])], Policy::shipped());
    rig.listed(&rig.ctx).await;
    rig.knob("moved", "preview", "on");
    let id = asked(&rig.call("fake__describe", json!({ "sql": "UPDATE" })).await);
    let shown = rig.open(&id).await.unwrap();
    let read = shown["preview"]["token_sha256"]
        .as_str()
        .unwrap()
        .to_string();
    // Another card changed the same row.
    rig.knob("moved", "version", "v2");
    let refused = rig.allow(&id, Some(&read)).await.unwrap_err();
    assert!(
        matches!(refused, tracon::session::SessionError::Stale(_)),
        "{refused:?}"
    );
    assert!(
        refused.to_string().contains("nothing was decided"),
        "{refused}"
    );
    let row = rig.store.get_approval(&id).unwrap().unwrap();
    assert_eq!(row.state, approvals::PENDING);
    assert_eq!(row.decided_ms, None);
    // Nothing recorded: the kept preview is still the one the operator read,
    // and nothing reached the plugin as a call.
    assert_eq!(rig.store.preview_token(&id).unwrap().as_deref(), Some("v1"));
    assert_eq!(rig.lines("fake", "moved", "calls.jsonl"), 0);
    // Opened again, the card shows the fresh preview, and Allow on it runs.
    let fresh = rig.open(&id).await.unwrap();
    let hash = fresh["preview"]["token_sha256"].as_str().unwrap();
    assert_eq!(hash, sha256("v2"));
    rig.allow(&id, Some(hash)).await.unwrap();
    let seen = approved_echo(&rig.settled(&id).await);
    assert_eq!(seen["meta"]["preview_token"], "v2");
    rig.manager.plugins().shutdown().await;
}

/// A preview that cannot be taken again at Allow leaves staleness to the
/// plugin's check at dispatch: the answer is not refused for it.
#[tokio::test]
async fn an_allow_whose_fresh_preview_fails_is_left_to_the_dispatch_check() {
    let rig = rig("flaky", vec![allow("fake", &["query"])], Policy::shipped());
    rig.listed(&rig.ctx).await;
    rig.knob("flaky", "preview", "on");
    let id = asked(&rig.call("fake__describe", json!({ "sql": "UPDATE" })).await);
    let shown = rig.open(&id).await.unwrap();
    let read = shown["preview"]["token_sha256"]
        .as_str()
        .unwrap()
        .to_string();
    rig.knob("flaky", "preview", "refuse");
    rig.knob("flaky", "version", "v2");
    rig.allow(&id, Some(&read)).await.unwrap();
    let settled = rig.settled(&id).await;
    assert_eq!(settled.state, approvals::FAILED);
    let reason = settled.reason.as_deref().unwrap();
    assert!(reason.starts_with(approvals::STALE_REASON), "{reason}");
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn an_operators_edit_is_previewed_and_its_preview_is_what_allow_binds() {
    let rig = rig("edited", vec![allow("fake", &["query"])], Policy::shipped());
    rig.listed(&rig.ctx).await;
    rig.knob("edited", "preview", "on");
    let id = asked(
        &rig.call("fake__describe", json!({ "sql": "UPDATE a" }))
            .await,
    );
    let first = rig.open(&id).await.unwrap();
    let h1 = first["preview"]["token_sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let edit = json!({ "sql": "UPDATE b" });
    // Allowing the edit before it is previewed is refused, though the answer
    // names the preview the card showed: that was of another call.
    let unseen = rig
        .tools
        .answer_approval(
            &id,
            OperatorAnswer {
                option_id: tracon::adapter::types::OPTION_ALLOW_ONCE.into(),
                arguments: Some(edit.clone()),
                preview_hash: Some(h1.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        unseen.contains("no preview of this call as it would run"),
        "{unseen}"
    );
    // The operator edits the call; the plugin previews the edit, against a
    // target that has moved since the card opened.
    rig.knob("edited", "version", "v2");
    let row = rig.store.get_approval(&id).unwrap().unwrap();
    let second = rig.tools.preview_edit(&row, &edit).await.unwrap().unwrap();
    assert!(
        second["preview"]["body"]
            .as_str()
            .unwrap()
            .contains("+ UPDATE b"),
        "{second}"
    );
    let h2 = second["preview"]["token_sha256"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(h1, h2);
    let previews = rig.records("edited", "previews.jsonl");
    assert_eq!(previews.len(), 2);
    assert_eq!(
        previews[1]["arguments_sha256"],
        sha256(r#"{"sql":"UPDATE b"}"#),
        "the edit is what was previewed"
    );
    // An answer read against the preview before the edit is refused.
    let answer = |hash: &str| OperatorAnswer {
        option_id: tracon::adapter::types::OPTION_ALLOW_ONCE.into(),
        arguments: Some(edit.clone()),
        preview_hash: Some(hash.to_string()),
        ..Default::default()
    };
    let refused = rig
        .tools
        .answer_approval(&id, answer(&h1))
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("the preview changed since you read it"),
        "{refused}"
    );
    // Read against the edit's preview, the edit runs against its token.
    rig.tools.answer_approval(&id, answer(&h2)).await.unwrap();
    let seen = approved_echo(&rig.settled(&id).await);
    assert_eq!(seen["meta"]["preview_token"], "v2");
    assert_eq!(seen["arguments"], edit);
    // What is kept to show is the edit's preview, the one approved, and
    // only as the edit's: it is not a preview of the call as asked.
    let ran = approvals::arguments_hash(&edit.to_string());
    let kept: Value =
        serde_json::from_str(&rig.store.preview_shown(&id, &ran).unwrap().unwrap()).unwrap();
    let asked = rig
        .store
        .get_approval(&id)
        .unwrap()
        .unwrap()
        .arguments_hash();
    assert_eq!(rig.store.preview_shown(&id, &asked).unwrap(), None);
    assert_eq!(kept["body"], second["preview"]["body"]);
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn an_edit_an_answer_would_refuse_is_not_previewed() {
    let rig = rig(
        "badedit",
        vec![allow("fake", &["query"])],
        Policy::shipped(),
    );
    rig.listed(&rig.ctx).await;
    rig.knob("badedit", "preview", "on");
    let id = asked(&rig.call("fake__describe", json!({ "sql": "x" })).await);
    let row = rig.store.get_approval(&id).unwrap().unwrap();
    let errors = rig
        .tools
        .preview_edit(&row, &json!("not an object"))
        .await
        .unwrap_err();
    assert!(!errors.is_empty());
    assert!(rig.records("badedit", "previews.jsonl").is_empty());
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

/// A plugin that declares no credential, with a consequential tool and a
/// tool that carries prose.
const MOVE: &str = r#"name = "move"
version = "1.0.0"
contract = 1
exec = ["bin/move"]

[[tool]]
name = "transition"
hint = "write"
consequential = true

[[tool]]
name = "note"
hint = "write"
"#;

fn install_move(rig: &Rig, channel: &str) {
    support::plugin::install(&rig.store, &rig.state, MOVE);
    let data = rig.data("move", channel);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("tools"), "transition\nnote\n").unwrap();
}

/// What a `plugin_call` grant allowing exactly this call names.
fn plugin_call_target(tool: &str, args: &Value) -> String {
    format!("plugin:{tool}:{}", sha256(&args.to_string()))
}

/// A tool its manifest marks `consequential` takes an `operation_id` the node
/// keeps and never sends on, is asked however it is allowed by name, runs
/// unattended only under a grant naming that one call, and is replayed, not
/// repeated, under the same id.
#[tokio::test]
async fn a_consequential_plugin_tool_needs_a_grant_naming_the_call() {
    let channel = "consequential";
    let rig = rig(
        channel,
        vec![unpreviewed(allow("move", &["*"]))],
        allowing(&["move__transition", "move__note"]),
    );
    install_move(&rig, channel);
    // Listed with the node's operation_id, required.
    let listed = rig.rpc(&rig.ctx, "tools/list", json!({})).await;
    let transition = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "move__transition")
        .cloned()
        .expect("move__transition listed");
    assert_eq!(
        transition["inputSchema"]["properties"]["operation_id"]["type"],
        "string"
    );
    assert!(transition["inputSchema"]["required"]
        .as_array()
        .unwrap()
        .contains(&json!("operation_id")));
    let note = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "move__note")
        .cloned()
        .unwrap();
    assert!(note["inputSchema"]["properties"]
        .get("operation_id")
        .is_none());

    // No operation id: refused before anything is asked.
    let out = rig
        .call("move__transition", json!({ "sql": "WRK-1 to Done" }))
        .await;
    assert_eq!(out["isError"], true, "{out}");
    assert!(text(&out).contains("operation_id"), "{out}");
    // The bundle and the entry both allow it by name: it is asked anyway.
    let call = json!({ "sql": "WRK-1 to Done", "operation_id": "move-wrk-1-done" });
    let id = asked(&rig.call("move__transition", call.clone()).await);
    assert_eq!(rig.lines("move", channel, "calls.jsonl"), 0);
    // Approved, it runs once, without the node's operation_id.
    let seen = approved_echo(&rig.approve(&id).await);
    assert_eq!(seen["arguments"], json!({ "sql": "WRK-1 to Done" }));
    assert_eq!(rig.lines("move", channel, "calls.jsonl"), 1);
    // The same operation again is asked, and once allowed is the recorded
    // outcome, not another call.
    let again = asked(&rig.call("move__transition", call.clone()).await);
    let replayed = approved_echo(&rig.approve(&again).await);
    assert_eq!(replayed["arguments"], json!({ "sql": "WRK-1 to Done" }));
    assert_eq!(rig.lines("move", channel, "calls.jsonl"), 1);

    // A grant naming one call allows that call, under a new id, unattended.
    let granted = json!({ "sql": "WRK-2 to Done" });
    let now = tracon::store::now_ms();
    rig.store
        .authority_grant_insert(&tracon::store::AuthorityGrantRow {
            id: "g-move".into(),
            action: tracon::authority::PLUGIN_CALL.into(),
            verdict: "allow".into(),
            target: plugin_call_target("move__transition", &granted),
            channel: channel.into(),
            session_id: None,
            revision: None,
            expires_ms: None,
            revoked_ms: None,
            reason: "under test".into(),
            created_ms: now,
        })
        .unwrap();
    let mut with_id = granted.clone();
    with_id["operation_id"] = json!("move-wrk-2-done");
    let ran = rig.call("move__transition", with_id).await;
    assert_eq!(echo(&ran)["arguments"], granted);
    assert_eq!(rig.lines("move", channel, "calls.jsonl"), 2);
    // Any other call of the tool is not the one granted.
    asked(
        &rig.call(
            "move__transition",
            json!({ "sql": "WRK-3 to Done", "operation_id": "move-wrk-3-done" }),
        )
        .await,
    );
    // An ordinary tool of the same plugin runs as its entry allows.
    echo(&rig.call("move__note", json!({ "sql": "hello" })).await);
    rig.manager.plugins().shutdown().await;
}

/// A plugin tool's prose, the arguments its schema marks with a format, is
/// what the call says rather than what it does: a comment may name what a
/// denial names and is asked instead. What the call acts on is still matched.
#[tokio::test]
async fn a_plugin_tools_marked_prose_is_not_matched_against_a_denial() {
    let channel = "prose";
    let mut policy: Policy = toml::from_str(
        r#"
        version = 9
        [[rule]]
        id = "no-push"
        verdict = "deny"
        reason = "Under test."
        matches = ["git push"]
        "#,
    )
    .unwrap();
    policy.trusted = true;
    let rig = rig(channel, vec![], policy);
    install_move(&rig, channel);
    std::fs::write(rig.data("move", channel).join("marked"), "").unwrap();
    rig.listed(&rig.ctx).await;
    asked(
        &rig.call(
            "move__note",
            json!({ "sql": "Deployed after git push.", "ms": 1 }),
        )
        .await,
    );
    // Unmarked, the same words are matched.
    let out = rig
        .call("move__note", json!({ "sql": "fine", "where": "git push" }))
        .await;
    assert_eq!(out["isError"], true, "{out}");
    assert!(text(&out).contains("no-push"), "{out}");
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn an_approved_call_runs_only_on_the_build_its_card_showed() {
    let rig = rig(
        "rebuilt",
        vec![unpreviewed(allow("fake", &[]))],
        Policy::shipped(),
    );
    rig.listed(&rig.ctx).await;
    let first = asked(&rig.call("fake__describe", json!({ "table": "apps" })).await);
    let waiting = asked(
        &rig.call("fake__describe", json!({ "table": "users" }))
            .await,
    );
    let card = |id: &str| {
        let row = rig.store.get_approval(id).unwrap().unwrap();
        rig.tools.plugin_approval_context(&row, None).unwrap()
    };
    assert_eq!(card(&first)["version"], "1.0.0");
    assert_eq!(card(&first)["replaced_by"], Value::Null);

    // Updated while both wait: each card still shows the build it was asked
    // on, and says another is active now.
    support::plugin::install_build(
        &rig.store,
        &rig.state,
        &FAKE.replace("version = \"1.0.0\"", "version = \"1.1.0\""),
        &"fe".repeat(32),
    );
    // As an approved update card does.
    rig.manager.plugins().stop_plugin("fake").await;
    assert_eq!(card(&first)["version"], "1.0.0");
    assert_eq!(card(&first)["replaced_by"], "1.1.0");

    // Allowing it runs nothing on the new build.
    let calls = rig.lines("fake", "rebuilt", "calls.jsonl");
    let refused = rig.approve(&first).await;
    assert_eq!(refused.state, approvals::FAILED);
    let reason = refused.reason.unwrap_or_default();
    assert!(reason.contains("changed build"), "{reason}");
    assert!(reason.contains("fake 1.1.0"), "{reason}");
    assert_eq!(rig.lines("fake", "rebuilt", "calls.jsonl"), calls);

    // The same call asked again is a new card on the new build, and the one
    // waiting on the old build is expired rather than reused.
    let again = asked(
        &rig.call("fake__describe", json!({ "table": "users" }))
            .await,
    );
    assert_ne!(again, waiting);
    let expired = rig.store.get_approval(&waiting).unwrap().unwrap();
    assert_eq!(expired.state, approvals::EXPIRED);
    assert_eq!(card(&again)["version"], "1.1.0");
    assert_eq!(card(&again)["replaced_by"], Value::Null);
    let settled = rig.approve(&again).await;
    assert_eq!(settled.state, approvals::SUCCEEDED, "{:?}", settled.reason);
    assert_eq!(rig.lines("fake", "rebuilt", "calls.jsonl"), calls + 1);
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn an_entrys_channels_narrow_where_a_plugin_is_offered_and_started() {
    let entry = |name: &str, channels: &[&str]| PluginEntry {
        name: name.into(),
        channels: Some(channels.iter().map(|c| c.to_string()).collect()),
        ..Default::default()
    };
    // `open` declares no credential: listed for `narrow` only. `fake` is
    // bound only on `narrow` and listed only for `narrow-bare`: offered
    // nowhere, the two rules taken together.
    let rig = rig(
        "narrow",
        vec![entry("open", &["narrow"]), entry("fake", &["narrow-bare"])],
        Policy::shipped(),
    );
    let tools = rig.listed(&rig.ctx).await;
    assert!(tools.iter().any(|t| t == "open__query"), "{tools:?}");
    assert!(tools.iter().all(|t| !t.starts_with("fake__")), "{tools:?}");
    let bare = rig.listed(&rig.bare).await;
    assert!(bare.iter().all(|t| !t.contains("__")), "{bare:?}");

    for (ctx, name) in [(&rig.ctx, "fake__query"), (&rig.bare, "open__query")] {
        let out = rig.call_as(ctx, name, json!({ "sql": "SELECT 1" })).await;
        assert_eq!(out["isError"], true, "{name}: {out}");
        assert!(text(&out).contains("[[plugin]] channels"), "{name}: {out}");
    }
    // A format the plugin owns is not converted where it is not offered.
    let refused = rig
        .tools
        .convert("narrow", "fake_wiki", "markdown", "h1. T")
        .await
        .unwrap_err();
    assert!(refused.contains("[[plugin]] channels"), "{refused}");

    // Neither listing, call nor conversion started an instance where its
    // plugin is not offered.
    let running: Vec<(String, String)> = rig
        .manager
        .plugins()
        .status()
        .into_iter()
        .map(|i| (i.plugin, i.channel))
        .collect();
    assert_eq!(running, [("open".to_string(), "narrow".to_string())]);
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn a_card_held_when_its_plugins_channels_narrow_previews_and_runs_nothing() {
    let rig = rig(
        "narrowed",
        vec![unpreviewed(allow("fake", &["query"]))],
        Policy::shipped(),
    );
    rig.listed(&rig.ctx).await;
    let id = asked(&rig.call("fake__describe", json!({ "sql": "x" })).await);
    rig.manager.plugins().shutdown().await;

    let path = rig.state.join("narrowed-node.toml");
    let text = "[[plugin]]\nname = \"fake\"\nallow = [\"query\"]\nrequire_preview = false\nchannels = []\n";
    std::fs::write(&path, text).unwrap();
    let newer: Config = toml::from_str(text).unwrap();
    rig.tools.cfg.apply_live_at(
        &path,
        &newer,
        &tracon::config::edit::file_hash(&path).unwrap(),
    );
    assert!(rig
        .listed(&rig.ctx)
        .await
        .iter()
        .all(|t| !t.starts_with("fake__")));
    let shown = rig.open(&id).await.expect("the card says why");
    assert_eq!(shown["preview"]["status"], "refused", "{shown}");
    let why = shown["preview"]["error"].as_str().unwrap();
    assert!(why.contains("lists no channel"), "{why}");
    let settled = rig.approve(&id).await;
    assert_eq!(settled.state, approvals::FAILED, "{settled:?}");
    assert_eq!(rig.lines("fake", "narrowed", "calls.jsonl"), 0);
    // The instance the call was asked on was stopped above; nothing since
    // started it again.
    let fake = rig.status("fake", "narrowed");
    assert_eq!((fake.state, fake.pid), (State::Stopped, None));
    rig.manager.plugins().shutdown().await;
}
