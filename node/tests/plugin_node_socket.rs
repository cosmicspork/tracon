//! A plugin calling the node back on the socket the node serves its instance
//! (`tracon::plugin::host`), driven through the fake plugin
//! (`tests/support/fake_plugin.rs`), whose `node` tool forwards one call: it
//! raises a card for the agent whose call it is serving and waits on it, is
//! refused with another token or a method its manifest does not name, pushes
//! a notification, calls another plugin's tool as itself, decided by
//! policy, and withdraws a card it raised once it is moot.

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tracon::{
    broker::SharedBroker,
    config::{Config, PluginEntry},
    mcp::{CallContext, SessionAccess, Tools},
    plugin::install,
    policy::Policy,
    session::{Manager, OperatorAnswer, SessionError},
    store::{approvals, Store},
    stream::Bus,
};

/// The plugin that calls the node: it declares no credential, so every
/// channel is offered it.
const CALLER: &str = r#"name = "caller"
version = "1.0.0"
contract = 1
exec = ["bin/caller"]
node_methods = ["approval/request", "approval/status", "approval/withdraw", "notify", "tools/call"]

[[tool]]
name = "node"
hint = "write"
"#;

/// The same, naming only the approval methods.
const MODEST: &str = r#"name = "modest"
version = "1.0.0"
contract = 1
exec = ["bin/modest"]
node_methods = ["approval/request"]

[[tool]]
name = "node"
hint = "write"
"#;

/// One that calls the node for nothing, and is given no socket for it.
const OPEN: &str = r#"name = "open"
version = "1.0.0"
contract = 1
exec = ["bin/open"]

[[tool]]
name = "query"
hint = "read"
"#;

/// One that declares a credential, so it is offered only on a channel that
/// binds one; no channel here does.
const GUARDED: &str = r#"name = "guarded"
version = "1.0.0"
contract = 1
exec = ["bin/guarded"]

[[credential]]
name = "guarded-read"
pattern = "guarded-read-*"

[[tool]]
name = "query"
hint = "read"
"#;

struct Rig {
    store: Arc<Store>,
    tools: Arc<Tools>,
    manager: Manager,
    state: PathBuf,
    channel: String,
    /// An agent's session on the channel.
    ctx: CallContext,
}

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

fn rig(channel: &str, entries: Vec<PluginEntry>, policy: Policy) -> Rig {
    support::state::isolate();
    let identity = proto::keys::Identity::from_seed(&[7; 32]);
    let node = identity.node_id();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node(&node).unwrap();
    store.channel_put(channel, &[], "{}").unwrap();
    store
        .insert_session(&support::rows::session_row(channel, &node, channel))
        .unwrap();
    let state = Config::state_dir();
    for manifest in [CALLER, MODEST, OPEN, GUARDED] {
        support::plugin::install(&store, &state, manifest);
    }
    for plugin in ["caller", "modest"] {
        let data = install::data_dir(&state, plugin).join(channel);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("tools"), "node\n").unwrap();
    }
    let cfg = Arc::new(Config {
        plugin: entries,
        ..Default::default()
    });
    let broker: SharedBroker = Arc::new(toml::from_str("").unwrap());
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
        channel: channel.into(),
        ctx: CallContext::session(channel, channel, &node),
    }
}

impl Rig {
    async fn tool(&self, name: &str, args: Value) -> Value {
        self.tools
            .handle(
                &self.ctx,
                &json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": name, "arguments": args },
                }),
            )
            .await
            .unwrap()["result"]
            .clone()
    }

    /// `method` called on the node by `plugin`'s fake, through its `node`
    /// tool: `{status, body}`.
    async fn node_as(&self, plugin: &str, method: &str, params: Value, extra: Value) -> Value {
        let mut args = json!({ "method": method, "params": params });
        if let Value::Object(more) = extra {
            args.as_object_mut().unwrap().extend(more);
        }
        let out = self.tool(&format!("{plugin}__node"), args).await;
        assert_eq!(out["isError"], false, "{out}");
        serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    async fn node(&self, method: &str, params: Value) -> Value {
        self.node_as("caller", method, params, json!({})).await
    }

    async fn answer(
        &self,
        id: &str,
        option: &str,
        notes: Option<&str>,
        edit: Option<Value>,
    ) -> Result<(), SessionError> {
        self.tools
            .clone()
            .answer_approval(
                id,
                OperatorAnswer {
                    option_id: option.into(),
                    arguments: edit,
                    reason: None,
                    notes: notes.map(str::to_string),
                    arguments_hash: None,
                    preview_hash: None,
                    by: None,
                    statement: None,
                },
            )
            .await
    }

    fn data(&self, plugin: &str) -> PathBuf {
        install::data_dir(&self.state, plugin).join(&self.channel)
    }
}

fn result(out: &Value) -> &Value {
    assert_eq!(out["status"], 200, "{out}");
    assert!(out["body"]["error"].is_null(), "{out}");
    &out["body"]["result"]
}

fn error(out: &Value) -> &Value {
    assert!(!out["body"]["error"].is_null(), "{out}");
    &out["body"]["error"]
}

fn card(title: &str) -> Value {
    json!({
        "title": title,
        "summary": "An agent's edit to a **risk**.",
        "content": [
            { "format": "word_diff", "title": "Description", "before": "the old words", "after": "the new words" },
            { "format": "table", "rows": [{ "key": "Kind", "value": "risk" }] },
            { "format": "diff", "text": "@@ -1 +1 @@\n-a\n+b\n" },
            { "format": "markdown", "text": "Why: *because*." },
            { "format": "source", "language": "json", "text": "{}" },
        ],
    })
}

#[tokio::test]
async fn a_plugin_raises_a_card_for_the_agent_it_serves_and_waits_on_it() {
    let rig = rig("cards", vec![], allowing(&["caller__node"]));

    // Raised while it serves the agent's call, with that call's context.
    let out = rig
        .node_as(
            "caller",
            "approval/request",
            card("Edit risk R1"),
            json!({ "on_behalf": true }),
        )
        .await;
    let raised = result(&out).clone();
    assert_eq!(raised["state"], "pending");
    assert_eq!(raised["new"], true);
    let id = raised["approval_id"].as_str().unwrap().to_string();
    let row = rig.store.get_approval(&id).unwrap().unwrap();
    assert_eq!(row.tool, approvals::PLUGIN_CARD_TOOL);
    assert_eq!(row.title, "caller: Edit risk R1");
    assert_eq!(
        row.session_id.as_deref(),
        Some("cards"),
        "the agent's session waits on it"
    );
    assert_eq!(row.lane.as_deref(), Some("plugin:caller"));
    let args: Value = serde_json::from_str(&row.arguments).unwrap();
    assert_eq!(args["plugin"], "caller");
    assert_eq!(args["version"], "1.0.0");
    assert_eq!(args["requested_for"]["harness"], "fake");
    assert_eq!(args["requested_for"]["session"], "cards");
    assert_eq!(args["content"][0]["format"], "word_diff");
    assert_eq!(args["content"].as_array().unwrap().len(), 5);

    // The same request while it waits is the same card.
    let again = rig
        .node_as(
            "caller",
            "approval/request",
            card("Edit risk R1"),
            json!({ "on_behalf": true }),
        )
        .await;
    assert_eq!(result(&again)["approval_id"], id.as_str());
    assert_eq!(result(&again)["new"], false);

    let status = rig
        .node(
            "approval/status",
            json!({ "approval_id": id, "wait_secs": 0 }),
        )
        .await;
    assert_eq!(result(&status)["state"], "still_waiting");

    // It runs nothing, so it takes no edit.
    let refused = rig
        .answer(&id, "allow_once", None, Some(json!({ "title": "other" })))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("takes no edit"), "{refused}");
    rig.answer(&id, "allow_once", None, None).await.unwrap();
    let row = rig.store.get_approval(&id).unwrap().unwrap();
    assert_eq!(row.state, approvals::SUCCEEDED);
    let status = rig
        .node("approval/status", json!({ "approval_id": id }))
        .await;
    assert_eq!(result(&status)["state"], "approved");
    // The agent reads it too, as a card raised for it.
    let agent = rig
        .tool(
            "approval_status",
            json!({ "approval_id": id, "wait_secs": 0 }),
        )
        .await;
    let agent: Value = serde_json::from_str(agent["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(agent["state"], "succeeded");
    assert!(
        agent["message"].as_str().unwrap().contains("plugin caller"),
        "{agent}"
    );

    // Changes asked for come back with the notes; a raise with no context
    // is the channel's, for no session.
    let out = rig.node("approval/request", card("Edit risk R2")).await;
    let second = result(&out)["approval_id"].as_str().unwrap().to_string();
    assert_eq!(
        rig.store.get_approval(&second).unwrap().unwrap().session_id,
        None
    );
    rig.answer(&second, "request_changes", Some("keep the old words"), None)
        .await
        .unwrap();
    let status = rig
        .node(
            "approval/status",
            json!({ "approval_ids": [second.clone(), id.clone()], "wait_secs": 0 }),
        )
        .await;
    let views = &result(&status)["approvals"];
    assert_eq!(views[0]["state"], "changes_requested");
    assert_eq!(views[0]["notes"], "keep the old words");
    assert_eq!(views[1]["state"], "approved");
    assert_eq!(result(&status)["still_waiting"], false);

    // A card waiting wakes a status call that is waiting on it.
    let out = rig.node("approval/request", card("Edit risk R3")).await;
    let third = result(&out)["approval_id"].as_str().unwrap().to_string();
    let waiting = {
        let tools = rig.tools.clone();
        let ctx = rig.ctx.clone();
        let third = third.clone();
        tokio::spawn(async move {
            tools
                .handle(
                    &ctx,
                    &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                        "name": "caller__node",
                        "arguments": { "method": "approval/status", "params": { "approval_id": third, "wait_secs": 20 } },
                    }}),
                )
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    rig.answer(&third, "reject_once", None, None).await.unwrap();
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
        .await
        .expect("the wait ends on the decision")
        .unwrap()
        .unwrap();
    let out: Value =
        serde_json::from_str(out["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result(&out)["state"], "rejected");
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn the_socket_answers_only_its_instance_and_what_its_manifest_names() {
    let rig = rig(
        "refusals",
        vec![],
        allowing(&["caller__node", "modest__node"]),
    );
    let out = rig
        .node_as(
            "caller",
            "approval/request",
            card("x"),
            json!({ "token": "not-the-token" }),
        )
        .await;
    assert_eq!(out["status"], 401, "{out}");
    assert_eq!(error(&out)["code"], tracon::plugin::host::UNAUTHORIZED);
    assert!(rig
        .store
        .approvals_of_session("refusals")
        .unwrap()
        .is_empty());

    // Named in no manifest: not answered.
    let out = rig
        .node_as(
            "modest",
            "notify",
            json!({ "title": "t", "message": "m" }),
            json!({}),
        )
        .await;
    assert_eq!(error(&out)["code"], -32601);
    assert!(error(&out)["message"]
        .as_str()
        .unwrap()
        .contains("not in modest's node_methods"));
    // A plugin naming none is not given a socket at all.
    rig.manager
        .plugins()
        .ensure("open", "refusals")
        .await
        .unwrap();
    assert!(rig.data("open").join("pid").exists());
    assert!(!rig.data("open").join("node_socket").exists());
    let caller_socket = std::fs::read_to_string(rig.data("caller").join("node_socket")).unwrap();
    assert!(caller_socket.ends_with(".sock"), "{caller_socket}");

    // A card asked badly, and a context that is not the node's.
    let out = rig
        .node(
            "approval/request",
            json!({ "title": "x", "content": [{ "format": "html", "text": "<b>" }] }),
        )
        .await;
    assert_eq!(error(&out)["code"], -32602);
    let forged = {
        let mut ctx = json!({ "v": 1, "plugin": "caller", "caller": { "kind": "agent", "session": "refusals" } });
        ctx["signature"] = json!("00".repeat(64));
        ctx
    };
    let out = rig
        .node("approval/request", json!({ "title": "x", "content": [{ "format": "markdown", "text": "x" }], "on_behalf": forged }))
        .await;
    assert_eq!(error(&out)["code"], tracon::plugin::host::REFUSED);
    assert!(error(&out)["message"]
        .as_str()
        .unwrap()
        .contains("not signed by this node"));

    // Another plugin's card is not this one's to read.
    let out = rig
        .node_as("modest", "approval/request", card("mine"), json!({}))
        .await;
    let theirs = result(&out)["approval_id"].as_str().unwrap().to_string();
    let out = rig
        .node(
            "approval/status",
            json!({ "approval_id": theirs, "wait_secs": 0 }),
        )
        .await;
    assert!(
        error(&out)["message"].as_str().unwrap().contains("no card"),
        "{out}"
    );

    // Notify goes out as the plugin, its title led by its name.
    let out = rig
        .node(
            "notify",
            json!({ "title": "Done", "message": "The export is ready." }),
        )
        .await;
    let sent = result(&out);
    assert_eq!(sent["deduplicated"], false, "{out}");
    assert!(sent["notification_id"].is_string());
    let out = rig.node("notify", json!({ "title": "Done" })).await;
    assert!(error(&out)["message"]
        .as_str()
        .unwrap()
        .contains("message is required"));
    rig.manager.plugins().shutdown().await;
}

#[tokio::test]
async fn a_plugin_calls_another_plugins_tool_as_itself_decided_by_policy() {
    // Allowed by the signed policy: run, with the caller named in the
    // context the callee is sent.
    let rig = rig("p2p", vec![], allowing(&["caller__node", "open__query"]));
    let out = rig
        .node(
            "tools/call",
            json!({ "name": "open__query", "arguments": { "sql": "SELECT 1" } }),
        )
        .await;
    let text = result(&out)["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let echo: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(echo["tool"], "query");
    assert_eq!(
        echo["meta"]["caller"],
        json!({ "kind": "plugin", "session": null, "harness": null, "plugin": "caller" })
    );
    assert_eq!(echo["meta"]["channel"], "p2p");
    assert_eq!(echo["meta"]["approval_id"], Value::Null);
    // Logged on the channel under the plugin's lane.
    let events = rig.store.external_events_after("p2p", 0, 100).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.lane.as_deref() == Some("plugin:caller") && e.kind == "tool_decided"),
        "{events:?}"
    );
    // Its own tools it calls in-process.
    let out = rig
        .node(
            "tools/call",
            json!({ "name": "caller__node", "arguments": {} }),
        )
        .await;
    assert!(error(&out)["message"]
        .as_str()
        .unwrap()
        .contains("own tool"));
    rig.manager.plugins().shutdown().await;

    // Not allowed: it would be asked, and a plugin's call is never held.
    let rig = rig_with("p2p-asked", vec![], allowing(&["caller__node"]));
    let out = rig
        .node(
            "tools/call",
            json!({ "name": "open__query", "arguments": {} }),
        )
        .await;
    assert_eq!(error(&out)["code"], tracon::plugin::host::REFUSED);
    assert_eq!(error(&out)["data"]["decision"], "asked");
    assert!(error(&out)["message"]
        .as_str()
        .unwrap()
        .contains("from plugin caller is never held"));
    assert!(rig
        .store
        .approvals_of_session("p2p-asked")
        .unwrap()
        .is_empty());
    rig.manager.plugins().shutdown().await;

    // The operator's entry for the callee denies it, over the policy's allow.
    let deny = PluginEntry {
        name: "open".into(),
        deny: vec!["query".into()],
        ..Default::default()
    };
    let rig = rig_with(
        "p2p-denied",
        vec![deny],
        allowing(&["caller__node", "open__query"]),
    );
    let out = rig
        .node(
            "tools/call",
            json!({ "name": "open__query", "arguments": {} }),
        )
        .await;
    assert_eq!(error(&out)["data"]["decision"], "denied", "{out}");
    rig.manager.plugins().shutdown().await;
}

/// A name the channel is not offered is refused before any rule is read, on
/// a plugin's `tools/call` as on an agent's: the tool of no installed
/// plugin, one its plugin does not declare, and one of a plugin installed
/// but not offered on the channel, which says why. Nothing is asked.
#[tokio::test]
async fn a_tool_the_channel_is_not_offered_is_refused_before_anything_is_asked() {
    let rig = rig(
        "p2p-unknown",
        vec![],
        allowing(&["caller__node", "open__query"]),
    );
    for name in ["absent__query", "open__nope"] {
        let out = rig
            .node("tools/call", json!({ "name": name, "arguments": {} }))
            .await;
        assert_eq!(
            error(&out)["message"],
            format!("no tool named {name} on channel p2p-unknown; nothing was asked or run"),
            "{out}"
        );
    }
    let out = rig
        .node(
            "tools/call",
            json!({ "name": "guarded__query", "arguments": {} }),
        )
        .await;
    assert_eq!(error(&out)["code"], tracon::plugin::host::REFUSED);
    let message = error(&out)["message"].as_str().unwrap();
    assert!(
        message.contains("plugin guarded is not offered on channel p2p-unknown"),
        "{out}"
    );

    // The agent's own door: the same refusal, before policy.
    for name in ["no_such_tool", "absent__query", "guarded__query"] {
        let out = rig.tool(name, json!({})).await;
        assert_eq!(out["isError"], true, "{out}");
        let text = out["content"][0]["text"].as_str().unwrap();
        if name == "guarded__query" {
            assert!(
                text.contains("not offered on channel p2p-unknown"),
                "{text}"
            );
        } else {
            assert_eq!(
                text,
                format!("no tool named {name} on channel p2p-unknown; nothing was asked or run")
            );
        }
    }
    assert!(rig
        .store
        .approvals_of_session("p2p-unknown")
        .unwrap()
        .is_empty());

    // A tool it is offered that no rule names is still asked.
    let out = rig
        .tool("doc_write", json!({ "slug": "notes", "body": "hello" }))
        .await;
    assert_eq!(out["isError"], false, "{out}");
    let held: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(held["state"], "awaiting_operator", "{out}");
    assert_eq!(
        rig.store.approvals_of_session("p2p-unknown").unwrap().len(),
        1
    );
    rig.manager.plugins().shutdown().await;
}

/// [`rig`], for a second rig in one test.
fn rig_with(channel: &str, entries: Vec<PluginEntry>, policy: Policy) -> Rig {
    rig(channel, entries, policy)
}

#[tokio::test]
async fn a_plugin_withdraws_a_card_it_raised_and_nobody_is_said_to_have_decided_it() {
    let rig = rig(
        "withdraw",
        vec![],
        allowing(&["caller__node", "modest__node"]),
    );
    let out = rig
        .node_as(
            "caller",
            "approval/request",
            card("Edit risk R1"),
            json!({ "on_behalf": true }),
        )
        .await;
    let id = result(&out)["approval_id"].as_str().unwrap().to_string();
    assert_eq!(
        rig.store
            .get_approval(&id)
            .unwrap()
            .unwrap()
            .session_id
            .as_deref(),
        Some("withdraw")
    );

    // A status call waiting on it ends on the withdrawal, not as approved.
    let waiting = {
        let tools = rig.tools.clone();
        let ctx = rig.ctx.clone();
        let id = id.clone();
        tokio::spawn(async move {
            tools
                .handle(
                    &ctx,
                    &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                        "name": "approval_status",
                        "arguments": { "approval_id": id, "wait_secs": 20 },
                    }}),
                )
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let out = rig
        .node(
            "approval/withdraw",
            json!({ "approval_id": id, "reason": "accepted in the plugin's page" }),
        )
        .await;
    let withdrawn = result(&out);
    assert_eq!(withdrawn["state"], "withdrawn", "{out}");
    assert_eq!(withdrawn["withdrew"], true);
    assert_eq!(withdrawn["reason"], "accepted in the plugin's page");
    assert_eq!(withdrawn["decided_ms"], Value::Null);

    let out = tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
        .await
        .expect("the wait ends on the withdrawal")
        .unwrap()
        .unwrap();
    let agent: Value =
        serde_json::from_str(out["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(agent["state"], "withdrawn", "{agent}");
    assert!(
        agent["message"]
            .as_str()
            .unwrap()
            .contains("Plugin caller withdrew"),
        "{agent}"
    );

    // It left the queue, and status reads it as withdrawn with the reason.
    let queue = rig.store.open_approval_cards("[]").unwrap();
    assert!(queue.iter().all(|c| c.request.id != id), "{queue:?}");
    let status = rig
        .node(
            "approval/status",
            json!({ "approval_id": id, "wait_secs": 0 }),
        )
        .await;
    assert_eq!(result(&status)["state"], "withdrawn");
    assert_eq!(result(&status)["reason"], "accepted in the plugin's page");
    // The session's wait on it is over, told as withdrawn.
    let wait = rig
        .manager
        .waits()
        .of("withdraw")
        .into_iter()
        .find(|w| w.id == id)
        .expect("the session waited on it");
    assert_eq!(wait.decided.as_ref().unwrap()["state"], "withdrawn");

    // The audit names the plugin as who ended it, never the operator.
    let audit = tracon::mcp::audit::session_audit(&rig.store, "withdraw").unwrap();
    let entry = audit
        .iter()
        .find(|e| e["approval_id"] == id.as_str())
        .unwrap_or_else(|| panic!("{audit:?}"));
    assert_eq!(entry["decision"], "withdrawn", "{entry}");
    assert_eq!(entry["decided_by"], "plugin:caller", "{entry}");
    assert_eq!(entry["reason"], "accepted in the plugin's page");
    let events = rig
        .store
        .session_events_of_kinds("withdraw", &["approval_settled"])
        .unwrap();
    let settled = events
        .iter()
        .find(|e| e.payload["approval_id"] == id.as_str())
        .expect("its settling is in the session's log");
    assert_eq!(settled.payload["state"], "withdrawn");
    assert_eq!(settled.payload["withdrawn_by"], "plugin:caller");
    assert!(settled.payload.get("decided_by").is_none(), "{settled:?}");

    // Twice is nothing the second time, and keeps the first reason.
    let out = rig
        .node(
            "approval/withdraw",
            json!({ "approval_id": id, "reason": "again" }),
        )
        .await;
    assert_eq!(result(&out)["state"], "withdrawn");
    assert_eq!(result(&out)["withdrew"], false);
    assert_eq!(result(&out)["reason"], "accepted in the plugin's page");

    // One the operator decided stays decided, and is returned as it is.
    let out = rig.node("approval/request", card("Edit risk R2")).await;
    let decided = result(&out)["approval_id"].as_str().unwrap().to_string();
    rig.answer(&decided, "reject_once", None, None)
        .await
        .unwrap();
    let out = rig
        .node(
            "approval/withdraw",
            json!({ "approval_id": decided, "reason": "moot" }),
        )
        .await;
    assert_eq!(result(&out)["state"], "rejected", "{out}");
    assert_eq!(result(&out)["withdrew"], false);
    let row = rig.store.get_approval(&decided).unwrap().unwrap();
    assert_eq!(row.state, approvals::REJECTED);
    assert_ne!(row.reason.as_deref(), Some("moot"));

    // Another plugin's card, an unknown one, and a reason left out are
    // params it does not take.
    let out = rig
        .node_as("modest", "approval/request", card("mine"), json!({}))
        .await;
    let theirs = result(&out)["approval_id"].as_str().unwrap().to_string();
    for params in [
        json!({ "approval_id": theirs, "reason": "moot" }),
        json!({ "approval_id": "no-such-card", "reason": "moot" }),
        json!({ "approval_id": decided }),
        json!({ "approval_id": decided, "reason": " " }),
    ] {
        let out = rig.node("approval/withdraw", params.clone()).await;
        assert_eq!(error(&out)["code"], -32602, "{params}: {out}");
    }
    assert_eq!(
        rig.store.get_approval(&theirs).unwrap().unwrap().state,
        approvals::PENDING
    );

    // A manifest that does not name the method is not answered.
    let out = rig
        .node_as(
            "modest",
            "approval/withdraw",
            json!({ "approval_id": theirs, "reason": "moot" }),
            json!({}),
        )
        .await;
    assert_eq!(error(&out)["code"], -32601, "{out}");
    assert_eq!(
        rig.store.get_approval(&theirs).unwrap().unwrap().state,
        approvals::PENDING
    );
    rig.manager.plugins().shutdown().await;
}
