//! Jira search through the operator API, forwarded to the jira-review
//! plugin's paged `search` (here the fake plugin, `support/fake_plugin.rs`):
//! called as the operator with a signed caller context, decided by policy
//! and the `[[plugin]]` entry and never held, logged to the channel's
//! external log, and answered with the plugin's page unchanged. The plugin
//! absent, not offered, refusing or failing each has its own status, and the
//! token is never in a response.

#[path = "support/mod.rs"]
mod support;
use support::http::call;
use support::plugin::Verifier;

use std::path::PathBuf;

use serde_json::{json, Value};

use tracon::{
    broker::Credential,
    config::{Config, PluginEntry},
    plugin::install,
};

const TOKEN: &str = "jira-secret-token";
const TOOL: &str = "jira-review__search";

const JIRA_REVIEW: &str = r#"name = "jira-review"
version = "1.0.0"
contract = 1
exec = ["bin/jira-review"]

[[credential]]
name = "jira"
pattern = "jira-*"

[[tool]]
name = "search"
hint = "read"
"#;

/// The pages the fake answers, by cursor: jira-review's workboard shape.
fn pages() -> Value {
    json!({
        "": {
            "site": "https://jira.example",
            "account_email": "me@example.com",
            "issues": [{
                "id": "10",
                "key": "WRK-1",
                "summary": "WRK-1 summary",
                "status": "In Progress",
                "status_category": "indeterminate",
                "priority": "High",
                "issue_type": "Task",
                "labels": ["board", "ops"],
                "parent": { "key": "WRK-0", "summary": "The epic", "issue_type": "Epic" },
            }],
            "next_cursor": "page-2",
        },
        "page-2": {
            "site": "https://jira.example",
            "account_email": "me@example.com",
            "issues": [{
                "id": "11",
                "key": "WRK-2",
                "summary": "WRK-2 summary",
                "status": "To Do",
                "status_category": "new",
                "priority": "Low",
                "issue_type": "Task",
                "labels": [],
                "parent": null,
            }],
            "next_cursor": null,
        },
        "bad": "jira refused the search (400 Bad Request): [\"Error in the JQL Query\"]",
    })
}

struct Node {
    h: support::harness::Harness,
    state: PathBuf,
}

/// A node with `entries`, the jira-review fake installed unless `installed`
/// is false, and a `jira` credential bound to `channel` (and not to
/// `{channel}-bare`). Each test its own channel: the plugin's data directory
/// is shared by the binary's tests.
async fn node(channel: &str, entries: Vec<PluginEntry>, installed: bool) -> Node {
    let h = support::harness::harness_with(Config {
        plugin: entries,
        ..Default::default()
    })
    .await;
    h.manager
        .plugins()
        .set_signer(proto::keys::Identity::from_seed(&[7; 32]));
    let bare = format!("{channel}-bare");
    for ch in [channel, bare.as_str()] {
        h.store.channel_put(ch, &[], "{}").unwrap();
    }
    h.tools.broker.write().unwrap().put(
        "jira",
        Credential {
            env: [
                ("JIRA_URL", "https://jira.example/"),
                ("JIRA_EMAIL", "me@example.com"),
                ("JIRA_TOKEN", TOKEN),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            channels: vec![channel.to_string()],
            ..Credential::default()
        },
    );
    let state = Config::state_dir();
    if installed {
        support::plugin::install(&h.store, &state, JIRA_REVIEW);
        let data = install::data_dir(&state, "jira-review").join(channel);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("tools"), "search\n").unwrap();
        std::fs::write(data.join("search.json"), pages().to_string()).unwrap();
    }
    Node { h, state }
}

/// A node still on a signed bundle from before v26, which names no
/// jira-review read: trusted, and allowing nothing by name.
fn older_bundle(n: &Node) {
    let mut policy: tracon::policy::Policy = toml::from_str("version = 25").unwrap();
    policy.trusted = true;
    *n.h.tools.policy.write() = policy;
}

fn allowing() -> Vec<PluginEntry> {
    vec![PluginEntry {
        name: "jira-review".into(),
        allow: vec!["search".into()],
        ..Default::default()
    }]
}

impl Node {
    async fn search(&self, query: &str) -> (u16, Value) {
        let (status, body) = call(
            &self.h.operator,
            "GET",
            &format!("/api/jira/search?{query}"),
            None,
        )
        .await;
        assert!(!body.to_string().contains(TOKEN), "token leaked: {body}");
        (status.as_u16(), body)
    }

    /// What the fake was called with on `channel`, in order.
    fn calls(&self, channel: &str) -> Vec<Value> {
        std::fs::read_to_string(
            install::data_dir(&self.state, "jira-review")
                .join(channel)
                .join("calls.jsonl"),
        )
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
    }

    /// The channel's external log, as `(lane, kind, payload)`.
    fn log(&self, channel: &str) -> Vec<(Option<String>, String, Value)> {
        self.h
            .store
            .external_events_after(channel, 0, 100)
            .unwrap()
            .into_iter()
            .map(|r| (r.lane, r.kind, r.payload))
            .collect()
    }

    /// The `tool_decided` rows of the channel's external log.
    fn decided(&self, channel: &str) -> Vec<Value> {
        self.log(channel)
            .into_iter()
            .filter(|(lane, kind, _)| {
                assert_eq!(lane.as_deref(), Some("/api/jira/search"));
                kind == "tool_decided"
            })
            .map(|(_, _, payload)| payload)
            .collect()
    }

    async fn shutdown(self) {
        self.h.manager.plugins().shutdown().await;
    }
}

#[tokio::test]
async fn pages_are_the_plugins_called_as_the_operator_and_logged() {
    // The shipped bundle (v26) runs the search with no `[[plugin]]` entry.
    let n = node("pages", vec![], true).await;

    let (status, first) = n.search("channel=pages&jql=project%20%3D%20WRK").await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first, pages()[""]);
    assert_eq!(first["next_cursor"], "page-2");

    let (status, second) = n
        .search("channel=pages&jql=project%20%3D%20WRK&cursor=page-2")
        .await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second, pages()["page-2"]);
    assert_eq!(second["next_cursor"], Value::Null);

    let calls = n.calls("pages");
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0]["tool"], "search");
    assert_eq!(
        calls[0]["arguments"],
        json!({ "jql": "project = WRK", "page": true })
    );
    assert_eq!(
        calls[1]["arguments"],
        json!({ "jql": "project = WRK", "page": true, "cursor": "page-2" })
    );

    // The caller context names the operator, and lends the channel's jira.
    let meta = &calls[0]["meta"];
    assert_eq!(
        meta["caller"],
        json!({ "kind": "operator", "session": null, "harness": null })
    );
    assert_eq!(meta["channel"], "pages");
    assert_eq!(meta["approval_id"], Value::Null);
    assert_eq!(meta["credentials"]["jira"]["JIRA_TOKEN"], TOKEN);
    let mut plugin = Verifier::new(
        "jira-review",
        calls[0]["own_instance"].as_str().unwrap(),
        calls[0]["node_key"].as_str().unwrap(),
    );
    plugin
        .verify(
            meta,
            "search",
            &calls[0]["arguments"],
            meta["issued_ms"].as_i64().unwrap(),
        )
        .unwrap();

    // Each call is in the channel's external log, under the route's lane,
    // and no session holds it.
    let log = n.log("pages");
    assert_eq!(
        log.iter()
            .filter(|(_, kind, _)| kind == "tool_call")
            .count(),
        2,
        "{log:?}"
    );
    let decided = n.decided("pages");
    assert_eq!(decided.len(), 2, "{decided:?}");
    for row in &decided {
        assert_eq!(row["tool"], TOOL);
        assert_eq!(row["decision"], "policy_allowed");
        assert_eq!(row["rule"], "brokered-reads");
        assert_eq!(row["outcome"], "ok");
    }
    assert!(n.h.store.list_sessions(None).unwrap().is_empty());
    n.shutdown().await;
}

/// On a bundle from before v26, the operator's entry is what lets it run.
#[tokio::test]
async fn an_older_bundle_runs_the_search_under_the_operators_entry() {
    let n = node("entry", allowing(), true).await;
    older_bundle(&n);
    let (status, page) = n.search("channel=entry&jql=x").await;
    assert_eq!(status, 200, "{page}");
    let decided = n.decided("entry");
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["rule"], "plugin.jira-review.allow");
    n.shutdown().await;
}

#[tokio::test]
async fn a_denying_entry_refuses_the_search_before_the_plugin() {
    let n = node(
        "denied",
        vec![PluginEntry {
            name: "jira-review".into(),
            deny: vec!["search".into()],
            ..Default::default()
        }],
        true,
    )
    .await;
    let (status, body) = n.search("channel=denied&jql=x").await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body.to_string()
            .contains("the operator does not allow search on plugin jira-review"),
        "{body}"
    );
    assert!(n.calls("denied").is_empty());
    let decided = n.decided("denied");
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["decision"], "denied");
    assert_eq!(decided[0]["rule"], "plugin.jira-review.deny");
    n.shutdown().await;
}

#[tokio::test]
async fn a_search_that_would_be_asked_is_refused_naming_the_entry() {
    let n = node("asked", vec![], true).await;
    older_bundle(&n);
    let (status, body) = n.search("channel=asked&jql=x").await;
    assert_eq!(status, 403, "{body}");
    let text = body.to_string();
    assert!(text.contains(r#"allow = [\"search\"]"#), "{body}");
    assert!(text.contains("jira-review"), "{body}");
    assert!(n.calls("asked").is_empty());
    let decided = n.decided("asked");
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["decision"], "refused");
    assert!(
        decided[0]["reason"]
            .as_str()
            .unwrap()
            .contains("never held"),
        "{decided:?}"
    );
    n.shutdown().await;
}

#[tokio::test]
async fn without_the_plugin_the_route_says_how_to_add_it() {
    let n = node("absent", allowing(), false).await;
    let (status, body) = n.search("channel=absent&jql=x").await;
    assert_eq!(status, 503, "{body}");
    assert!(
        body.to_string()
            .contains("tracon plugin add cosmicspork/jira-review"),
        "{body}"
    );
    n.shutdown().await;
}

#[tokio::test]
async fn a_channel_the_plugin_is_not_offered_on_is_refused() {
    let n = node("offered", allowing(), true).await;
    let (status, body) = n.search("channel=offered-bare&jql=x").await;
    assert_eq!(status, 422, "{body}");
    assert!(body.to_string().contains("jira-*"), "{body}");
    assert!(n.calls("offered-bare").is_empty());

    let (status, body) = n.search("channel=nowhere&jql=x").await;
    assert_eq!(status, 422, "{body}");

    let (status, body) = n.search("channel=offered&jql=%20").await;
    assert_eq!(status, 422, "{body}");
    n.shutdown().await;
}

#[tokio::test]
async fn the_plugins_error_is_a_bad_gateway_with_its_text() {
    let n = node("failing", allowing(), true).await;
    let (status, body) = n.search("channel=failing&jql=x&cursor=bad").await;
    assert_eq!(status, 502, "{body}");
    assert!(
        body.to_string().contains("Error in the JQL Query"),
        "{body}"
    );
    let decided = n.decided("failing");
    assert_eq!(decided[0]["outcome"], "error");
    n.shutdown().await;
}
