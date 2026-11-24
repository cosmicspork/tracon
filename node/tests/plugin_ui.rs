//! Plugin UIs (`tracon::http::plugin_ui`): the operator API mints a token
//! for one plugin and channel and starts the plugin's own loopback listener;
//! that listener serves the build's `ui/` under the token with a CSP that
//! lets the page talk only to its own origin, and forwards `api/...` to the
//! plugin (here the fake, `support/fake_plugin.rs`) as the operator's
//! request with a signed caller context, under policy and audit. A plugin
//! without `[ui]` gets no listener.

#[path = "support/mod.rs"]
mod support;
use support::http::{call, call_local};
use support::plugin::Verifier;

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use tracon::{
    config::{Config, PluginEntry, Plugins},
    http::plugin_ui::{self, UiBinding, PLUGIN_UI_CSP},
    plugin::install,
};

const INDEX: &str = "<!doctype html><title>pageant</title><script src=\"./app.js\"></script>";
const APP_JS: &str = "fetch('api/things')";

fn manifest(name: &str, ui: bool, credentials: &str) -> String {
    let mut text =
        format!("name = \"{name}\"\nversion = \"1.0.0\"\ncontract = 1\nexec = [\"bin/{name}\"]\n");
    text.push_str(credentials);
    if ui {
        text.push_str("\n[ui]\nentry = \"ui/index.html\"\ntitle = \"Pageant\"\n");
    }
    text
}

/// A port nothing listens on now, for a range of one.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Node {
    h: support::harness::Harness,
    state: PathBuf,
    port: u16,
}

/// A node with the fake installed as `name` (with a UI when `ui`), the
/// channel `channel`, and `entries`. Each test its own plugin name: the
/// binary's tests share a state directory.
async fn node(name: &str, ui: bool, channel: &str, entries: Vec<PluginEntry>) -> Node {
    node_declaring(name, ui, channel, entries, "").await
}

/// [`node`], with the build's manifest declaring `credentials`.
async fn node_declaring(
    name: &str,
    ui: bool,
    channel: &str,
    entries: Vec<PluginEntry>,
    credentials: &str,
) -> Node {
    let port = free_port();
    let h = support::harness::harness_with(Config {
        plugin: entries,
        plugins: Plugins {
            ui_port_first: port,
            ui_port_last: port,
            ..Plugins::default()
        },
        ..Default::default()
    })
    .await;
    h.manager
        .plugins()
        .set_signer(proto::keys::Identity::from_seed(&[7; 32]));
    h.store.channel_put(channel, &[], "{}").unwrap();
    let state = Config::state_dir();
    if ui {
        let dir = install::build_dir(&state, name, "1.0.0", support::plugin::DIGEST).join("ui");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), INDEX).unwrap();
        std::fs::write(dir.join("app.js"), APP_JS).unwrap();
    }
    support::plugin::install(&h.store, &state, &manifest(name, ui, credentials));
    let data = install::data_dir(&state, name).join(channel);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("tools"), "").unwrap();
    Node { h, state, port }
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

impl Node {
    /// `POST /api/plugins/{name}/ui` from the node's own machine.
    async fn open(&self, name: &str, channel: &str) -> (u16, Value) {
        let (status, body) = call_local(
            &self.h.operator,
            "POST",
            &format!("/api/plugins/{name}/ui"),
            Some(json!({ "channel": channel })),
        )
        .await;
        (status.as_u16(), body)
    }

    /// A request to `name`'s listener router, as a page on it sends one.
    async fn page(
        &self,
        name: &str,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Answer {
        let app = plugin_ui::router(self.h.state.clone(), name, self.port);
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", format!("127.0.0.1:{}", self.port));
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = b
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Answer {
            status,
            headers,
            body,
        }
    }

    /// What the fake's API was sent on `channel`, in order.
    fn calls(&self, name: &str, channel: &str) -> Vec<Value> {
        std::fs::read_to_string(
            install::data_dir(&self.state, name)
                .join(channel)
                .join("ui.jsonl"),
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

    async fn shutdown(self) {
        self.h.manager.plugins().shutdown().await;
    }
}

fn token_of(url: &str) -> String {
    url.split("/t/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The interface opens a page with its theme in the query
/// (`?tracon_theme=dark`, from `@tracon/ui`'s theme bridge); the file server
/// ignores the query, so the entry is still served.
#[tokio::test]
async fn a_query_on_the_entry_still_serves_it() {
    let n = node("pennant", true, "pages", vec![]).await;
    let (status, opened) = n.open("pennant", "pages").await;
    assert_eq!(status, 200, "{opened}");
    let token = token_of(opened["url"].as_str().unwrap());
    for path in [
        "?tracon_theme=dark",
        "index.html?tracon_theme=light",
        "index.html?tracon_theme=dark&x=1",
        "app.js?tracon_theme=dark",
    ] {
        let a = n
            .page("pennant", "GET", &format!("/t/{token}/{path}"), &[], None)
            .await;
        assert_eq!(a.status, 200, "{path}");
        let want = if path.starts_with("app.js") {
            APP_JS
        } else {
            INDEX
        };
        assert_eq!(String::from_utf8_lossy(&a.body), want, "{path}");
        assert_eq!(a.headers["content-security-policy"], PLUGIN_UI_CSP);
    }
}

#[tokio::test]
async fn a_token_opens_its_plugins_page_on_its_own_origin_with_the_csp() {
    let n = node("pageant", true, "pages", vec![]).await;

    // Off the node's machine, there is nothing to open: the listener is
    // loopback only.
    let (status, body) = call(
        &n.h.operator,
        "POST",
        "/api/plugins/pageant/ui",
        Some(json!({ "channel": "pages" })),
    )
    .await;
    assert_eq!(status.as_u16(), 409, "{body}");

    let (status, opened) = n.open("pageant", "pages").await;
    assert_eq!(status, 200, "{opened}");
    let origin = format!("http://127.0.0.1:{}", n.port);
    assert_eq!(opened["origin"], origin.as_str());
    assert_eq!(opened["title"], "Pageant");
    assert_eq!(opened["channel"], "pages");
    let url = opened["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{origin}/t/")), "{url}");
    assert!(url.ends_with("/index.html"), "{url}");
    let token = token_of(url);
    assert_eq!(n.h.manager.plugin_uis().port("pageant"), Some(n.port));

    // The listener is up on that port: the entry, over a real connection.
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let res = client.get(url).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.headers()["content-security-policy"].to_str().unwrap(),
        PLUGIN_UI_CSP
    );
    assert_eq!(res.text().await.unwrap(), INDEX);

    // Its files, under the token, with the CSP and no caching.
    for (path, want, media) in [
        ("", INDEX, "text/html"),
        ("index.html", INDEX, "text/html"),
        ("app.js", APP_JS, "javascript"),
    ] {
        let a = n
            .page("pageant", "GET", &format!("/t/{token}/{path}"), &[], None)
            .await;
        assert_eq!(a.status, 200, "{path}");
        assert_eq!(String::from_utf8_lossy(&a.body), want);
        let content_type = a.headers["content-type"].to_str().unwrap();
        assert!(content_type.contains(media), "{path}: {content_type}");
        assert_eq!(a.headers["content-security-policy"], PLUGIN_UI_CSP);
        assert_eq!(a.headers["cache-control"], "private, no-store");
        assert_eq!(a.headers["referrer-policy"], "no-referrer");
    }

    // Nothing outside ui/, nothing without the token, nothing under a token
    // for another plugin, and nothing for a Host that is not this listener.
    for path in [
        "missing.js",
        "../tracon-plugin.toml",
        "%2e%2e/tracon-plugin.toml",
        "..%2fbin/pageant",
    ] {
        let a = n
            .page("pageant", "GET", &format!("/t/{token}/{path}"), &[], None)
            .await;
        assert_eq!(a.status, 404, "{path}");
    }
    let a = n.page("pageant", "GET", "/index.html", &[], None).await;
    assert_eq!(a.status, 404);
    let a = n
        .page("pageant", "GET", "/t/not-a-token/index.html", &[], None)
        .await;
    assert_eq!(a.status, 404);
    let (other, _) = n.h.manager.plugin_uis().mint(UiBinding {
        plugin: "someone-else".into(),
        channel: "pages".into(),
    });
    let a = n
        .page(
            "pageant",
            "GET",
            &format!("/t/{other}/index.html"),
            &[],
            None,
        )
        .await;
    assert_eq!(a.status, 404);
    let app = plugin_ui::router(n.h.state.clone(), "pageant", n.port);
    let res = app
        .oneshot(
            Request::builder()
                .uri(format!("/t/{token}/index.html"))
                .header("host", format!("rebound.example:{}", n.port))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::MISDIRECTED_REQUEST);

    // Opening it again keeps the same port, so the same origin.
    let (status, again) = n.open("pageant", "pages").await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["origin"], origin.as_str());
    assert_ne!(token_of(again["url"].as_str().unwrap()), token);

    // Removed, its listener closes and its tokens are forgotten.
    let (status, body) = call_local(&n.h.operator, "DELETE", "/api/plugins/pageant", None).await;
    assert_eq!(status.as_u16(), 200, "{body}");
    assert_eq!(n.h.manager.plugin_uis().port("pageant"), None);
    assert_eq!(n.h.manager.plugin_uis().lookup(&token), None);
    n.shutdown().await;
}

#[tokio::test]
async fn api_requests_reach_the_plugin_as_the_operators_signed_and_logged() {
    let n = node("parlour", true, "parlour-ch", vec![]).await;
    let (status, opened) = n.open("parlour", "parlour-ch").await;
    assert_eq!(status, 200, "{opened}");
    let token = token_of(opened["url"].as_str().unwrap());
    let origin = format!("http://127.0.0.1:{}", n.port);

    // A change from the page, with its own origin.
    let body = r#"{"title":"Ship it"}"#;
    let a = n
        .page(
            "parlour",
            "POST",
            &format!("/t/{token}/api/things?limit=5&q=a%20b"),
            &[
                ("origin", origin.as_str()),
                ("content-type", "application/json"),
                ("cookie", "tracon_session=secret"),
                ("referer", "http://127.0.0.1:7420/plugins/parlour"),
                ("x-custom", "kept-out"),
            ],
            Some(body),
        )
        .await;
    assert_eq!(a.status, 200, "{}", String::from_utf8_lossy(&a.body));
    let echo = a.json();
    assert_eq!(echo["method"], "POST");
    assert_eq!(echo["uri"], "/api/things?limit=5&q=a%20b");
    assert_eq!(echo["body"], body);
    // Only the headers the proxy passes on reach the plugin.
    let sent = echo["headers"].as_object().unwrap();
    assert_eq!(sent["content-type"], "application/json");
    for absent in ["origin", "cookie", "referer", "x-custom"] {
        assert!(
            !sent.contains_key(absent),
            "{absent} reached the plugin: {sent:?}"
        );
    }
    // Only the answer's headers the page may see reach it, under the CSP.
    assert!(a.headers.get("set-cookie").is_none());
    assert!(a.headers.get("x-extra").is_none());
    assert_eq!(
        a.headers["content-disposition"],
        "inline; filename=\"echo.json\""
    );
    assert_eq!(a.headers["content-security-policy"], PLUGIN_UI_CSP);

    // The caller context: the operator, on the token's channel, bound to
    // the method, the path as sent and the body, and signed by the node.
    let ctx = &echo["context"];
    assert_eq!(ctx["tool"], "tracon/ui");
    assert_eq!(
        ctx["caller"],
        json!({ "kind": "operator", "session": null, "harness": null })
    );
    assert_eq!(ctx["channel"], "parlour-ch");
    assert_eq!(ctx["approval_id"], Value::Null);
    assert_eq!(ctx["credentials"], json!({}));
    let request = json!({
        "body_sha256": sha256_hex(body.as_bytes()),
        "method": "POST",
        "path": "/api/things?limit=5&q=a%20b",
    });
    let mut plugin = Verifier::new(
        "parlour",
        echo["own_instance"].as_str().unwrap(),
        echo["node_key"].as_str().unwrap(),
    );
    plugin
        .verify(
            ctx,
            "tracon/ui",
            &request,
            ctx["issued_ms"].as_i64().unwrap(),
        )
        .unwrap();
    // A context does not stand for another request.
    let other = json!({ "body_sha256": sha256_hex(b""), "method": "POST", "path": "/api/things?limit=5&q=a%20b" });
    assert_eq!(
        Verifier::new(
            "parlour",
            echo["own_instance"].as_str().unwrap(),
            echo["node_key"].as_str().unwrap(),
        )
        .verify(ctx, "tracon/ui", &other, ctx["issued_ms"].as_i64().unwrap())
        .unwrap_err()
        .0,
        3
    );

    // A read needs no Origin, and is not logged.
    let a = n
        .page(
            "parlour",
            "GET",
            &format!("/t/{token}/api/things"),
            &[],
            None,
        )
        .await;
    assert_eq!(a.status, 200);
    assert_eq!(a.json()["context"]["tool"], "tracon/ui");
    // The plugin's own failure is its answer.
    let a = n
        .page("parlour", "GET", &format!("/t/{token}/api/fail"), &[], None)
        .await;
    assert_eq!(a.status, 500);

    // A change without the page's origin, or from another, never reaches
    // the plugin.
    let before = n.calls("parlour", "parlour-ch").len();
    for headers in [
        vec![],
        vec![("origin", "http://127.0.0.1:7420")],
        vec![("origin", "null")],
    ] {
        let a = n
            .page(
                "parlour",
                "DELETE",
                &format!("/t/{token}/api/things/1"),
                &headers,
                None,
            )
            .await;
        assert_eq!(a.status, 403, "{headers:?}");
    }
    assert_eq!(n.calls("parlour", "parlour-ch").len(), before);

    // Only the change is in the channel's external log, under the UI's lane.
    let log = n.log("parlour-ch");
    let decided: Vec<&Value> = log
        .iter()
        .filter(|(lane, kind, _)| {
            assert_eq!(lane.as_deref(), Some("/plugins/parlour/ui"));
            kind == "tool_decided"
        })
        .map(|(_, _, p)| p)
        .collect();
    assert_eq!(decided.len(), 1, "{log:?}");
    assert_eq!(decided[0]["tool"], "parlour__ui");
    assert_eq!(decided[0]["decision"], "policy_allowed");
    assert_eq!(decided[0]["outcome"], "ok");
    assert!(n.h.store.list_sessions(None).unwrap().is_empty());
    n.shutdown().await;
}

#[tokio::test]
async fn a_pages_request_is_lent_only_the_binding_its_query_selects() {
    let n = node_declaring(
        "pantry",
        true,
        "pantry-ch",
        vec![],
        r#"
[[credential]]
name = "pantry-read"
pattern = "pantry-read-*"
select = "profile"

[[credential]]
name = "pantry-write"
pattern = "pantry-write-*"
lend = "approved"
select = "profile"
"#,
    )
    .await;
    let mut bindings = String::new();
    for name in [
        "pantry-read",
        "pantry-read-tst",
        "pantry-read-prd",
        "pantry-write-tst",
        "pantry-write-prd",
    ] {
        bindings.push_str(&format!(
            "[credentials.{name}]\nchannels = [\"pantry-ch\"]\n[credentials.{name}.env]\nDB_USER = \"{name}\"\n\n"
        ));
    }
    *n.h.tools.broker.write().unwrap() = toml::from_str(&bindings).unwrap();
    let (status, opened) = n.open("pantry", "pantry-ch").await;
    assert_eq!(status, 200, "{opened}");
    let token = token_of(opened["url"].as_str().unwrap());
    let origin = format!("http://127.0.0.1:{}", n.port);
    let lent = |a: &Answer| -> Vec<String> {
        assert_eq!(a.status, 200, "{}", String::from_utf8_lossy(&a.body));
        a.json()["context"]["credentials"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };

    // Selected: only that profile's read account, never another's.
    let a = n
        .page(
            "pantry",
            "GET",
            &format!("/t/{token}/api/rows?profile=tst&limit=5"),
            &[],
            None,
        )
        .await;
    assert_eq!(lent(&a), ["pantry-read-tst"]);
    assert_eq!(
        a.json()["context"]["credentials"]["pantry-read-tst"]["DB_USER"],
        "pantry-read-tst"
    );
    // A change is never approved, so the approved-only write account is
    // never lent to it, though its profile is bound.
    let a = n
        .page(
            "pantry",
            "POST",
            &format!("/t/{token}/api/rows?profile=prd"),
            &[
                ("origin", origin.as_str()),
                ("content-type", "application/json"),
            ],
            Some("{}"),
        )
        .await;
    assert_eq!(lent(&a), ["pantry-read-prd"]);

    // Absent: the bare binding only.
    for uri in ["api/rows", "api/rows?limit=5", "api/rows?profile="] {
        let a = n
            .page("pantry", "GET", &format!("/t/{token}/{uri}"), &[], None)
            .await;
        assert_eq!(lent(&a), ["pantry-read"], "{uri}");
    }

    // Unknown: refused before the plugin is asked, naming the binding, and
    // logged though it is a read.
    let before = n.calls("pantry", "pantry-ch").len();
    let a = n
        .page(
            "pantry",
            "GET",
            &format!("/t/{token}/api/rows?profile=nope"),
            &[],
            None,
        )
        .await;
    assert_eq!(a.status, 422, "{}", String::from_utf8_lossy(&a.body));
    let message = a.json()["error"]["message"].as_str().unwrap().to_string();
    assert_eq!(
        message,
        "nothing ran: plugin pantry cannot be lent what this call names on channel pantry-ch: \
         the call's profile names binding pantry-read-nope, which this channel does not bind"
    );
    let a = n
        .page(
            "pantry",
            "GET",
            &format!("/t/{token}/api/rows?profile=tst&profile=prd"),
            &[],
            None,
        )
        .await;
    assert_eq!(a.status, 422, "{}", String::from_utf8_lossy(&a.body));
    assert_eq!(n.calls("pantry", "pantry-ch").len(), before);
    let refused: Vec<Value> = n
        .log("pantry-ch")
        .into_iter()
        .filter(|(_, kind, p)| kind == "tool_decided" && p["decision"] == "refused")
        .map(|(_, _, p)| p)
        .collect();
    assert_eq!(refused.len(), 2, "{refused:?}");
    assert!(refused[0]["reason"]
        .as_str()
        .unwrap()
        .contains("pantry-read-nope"));
    n.shutdown().await;
}

#[tokio::test]
async fn an_entry_denying_the_ui_refuses_its_requests_before_the_plugin() {
    let n = node(
        "pavilion",
        true,
        "pavilion-ch",
        vec![PluginEntry {
            name: "pavilion".into(),
            deny: vec!["ui".into()],
            ..Default::default()
        }],
    )
    .await;
    let (status, opened) = n.open("pavilion", "pavilion-ch").await;
    assert_eq!(status, 200, "{opened}");
    let token = token_of(opened["url"].as_str().unwrap());
    // Its page is the node's to serve; its API is the plugin's, and denied.
    let a = n
        .page(
            "pavilion",
            "GET",
            &format!("/t/{token}/index.html"),
            &[],
            None,
        )
        .await;
    assert_eq!(a.status, 200);
    let a = n
        .page(
            "pavilion",
            "GET",
            &format!("/t/{token}/api/things"),
            &[],
            None,
        )
        .await;
    assert_eq!(a.status, 403);
    let message = a.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("[[plugin]] deny"), "{message}");
    assert!(n.calls("pavilion", "pavilion-ch").is_empty());
    // The refusal is logged, read or not.
    let decided: Vec<Value> = n
        .log("pavilion-ch")
        .into_iter()
        .filter(|(_, kind, _)| kind == "tool_decided")
        .map(|(_, _, p)| p)
        .collect();
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["decision"], "denied");
    n.shutdown().await;
}

#[tokio::test]
async fn a_plugin_without_a_ui_gets_no_listener() {
    let n = node("plainsong", false, "plain-ch", vec![]).await;
    let (status, body) = n.open("plainsong", "plain-ch").await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("has no UI"),
        "{body}"
    );
    assert_eq!(n.h.manager.plugin_uis().port("plainsong"), None);
    // Nor does one that is not installed.
    let (status, body) = n.open("nobody", "plain-ch").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(n.h.manager.plugin_uis().port("nobody"), None);
    // Its port was never given out: nothing listens there.
    assert!(std::net::TcpListener::bind(("127.0.0.1", n.port)).is_ok());
    n.shutdown().await;
}

#[tokio::test]
async fn a_page_on_a_channel_the_entry_does_not_list_is_refused_and_starts_nothing() {
    let n = node(
        "portico",
        true,
        "portico-ch",
        vec![PluginEntry {
            name: "portico".into(),
            channels: Some(vec!["elsewhere".into()]),
            ..Default::default()
        }],
    )
    .await;
    let (status, body) = n.open("portico", "portico-ch").await;
    assert_eq!(status, 403, "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("[[plugin]] channels"), "{message}");
    assert_eq!(n.h.manager.plugin_uis().port("portico"), None);
    assert!(n.h.manager.plugins().status().is_empty());
    n.shutdown().await;
}
