//! A plugin's install card, answered as the operator answers any card: allowing
//! it makes the staged build the plugin's active one; rejecting it drops the
//! build; it takes no edit and no request for changes, since a plugin is
//! installed as published or not at all.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness, Harness};

use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use tracon::{
    config::Config,
    mcp::{SessionAccess, Tools},
    plugin::{self, install, Ask, Offered},
    session::{Manager, OperatorAnswer},
    store::{approvals, Store},
    stream::Bus,
};

fn rig() -> (Arc<Store>, Arc<Tools>) {
    support::state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    store.channel_put("personal", &[], "{}").unwrap();
    let cfg = Arc::new(Config::default());
    let tools = Arc::new(Tools {
        broker: Arc::new(Default::default()),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
        upstream: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg,
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let _ = tools.session.set(SessionAccess {
        store: store.clone(),
        manager,
    });
    (store, tools)
}

/// A plugin archive as a release ships it, named as one.
fn archive(name: &str, version: &str) -> (Vec<u8>, String) {
    let manifest = format!(
        "name = \"{name}\"\nversion = \"{version}\"\ncontract = 1\nexec = [\"bin/{name}\"]\n\n[[tool]]\nname = \"query\"\nhint = \"read\"\n"
    );
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    {
        let mut builder = tar::Builder::new(&mut gz);
        for (path, body, mode) in [
            (
                "tracon-plugin.toml".to_string(),
                manifest.into_bytes(),
                0o644,
            ),
            (
                format!("bin/{name}"),
                b"#!/bin/sh\nexit 0\n".to_vec(),
                0o755,
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(mode);
            header.set_entry_type(tar::EntryType::Regular);
            builder
                .append_data(&mut header, path, body.as_slice())
                .unwrap();
        }
        builder.finish().unwrap();
    }
    let asset = format!("{name}-{version}-{}.tar.gz", install::target().unwrap());
    (gz.finish().unwrap(), asset)
}

fn card(store: &Store, name: &str, version: &str) -> approvals::ApprovalRow {
    let (bytes, asset) = archive(name, version);
    let staged = install::stage_local(&Config::state_dir(), &bytes, &asset).unwrap();
    let ask = Ask {
        node_id: "n1",
        channel: "personal",
        expiry_secs: 600,
        mcp_servers: Vec::new(),
    };
    match plugin::offer(store, &Config::state_dir(), &ask, staged).unwrap() {
        Offered::Asked { card, .. } => card,
        other => panic!("{other:?}"),
    }
}

fn answer(option: &str) -> OperatorAnswer {
    OperatorAnswer {
        option_id: option.into(),
        by: Some("operator (test)".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn allowing_an_install_card_installs_its_build_and_it_takes_no_edit() {
    if install::target().is_none() {
        return;
    }
    let (store, tools) = rig();
    let card = card(&store, "allowed", "1.0.0");
    assert_eq!(card.tool, approvals::PLUGIN_INSTALL_TOOL);
    let listed = store.open_approval_cards(approvals::CARD_OPTIONS).unwrap();
    let shown = serde_json::to_string(
        listed
            .iter()
            .find(|c| c.id == card.id)
            .expect("the card is listed"),
    )
    .unwrap();
    assert!(shown.contains("Install"), "{shown}");
    assert!(!shown.contains("request_changes"), "{shown}");

    // An edit, or a request for changes, decides nothing.
    let mut edited: Value = serde_json::from_str(&card.arguments).unwrap();
    edited["version"] = json!("9.9.9");
    let refused = tools
        .answer_approval(
            &card.id,
            OperatorAnswer {
                arguments: Some(edited),
                ..answer("allow_once")
            },
        )
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("takes no edit"), "{refused}");
    let refused = tools
        .answer_approval(
            &card.id,
            OperatorAnswer {
                notes: Some("smaller please".into()),
                ..answer("request_changes")
            },
        )
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("installed as published"),
        "{refused}"
    );
    let row = store.plugin_get("allowed").unwrap().unwrap();
    assert!(
        row.active.is_none(),
        "nothing is installed before it is allowed"
    );

    tools
        .answer_approval(&card.id, answer("allow_once"))
        .await
        .unwrap();
    let row = store.plugin_get("allowed").unwrap().unwrap();
    assert_eq!(row.state, "active");
    assert_eq!(row.active.unwrap().version, "1.0.0");
    let settled = store.get_approval(&card.id).unwrap().unwrap();
    assert_eq!(settled.state, approvals::SUCCEEDED);
    let result: Value = serde_json::from_str(settled.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["installed"], "1.0.0");
}

#[tokio::test]
async fn rejecting_an_install_card_drops_its_build() {
    if install::target().is_none() {
        return;
    }
    let (store, tools) = rig();
    let card = card(&store, "rejected", "1.0.0");
    let dir = install::bin_dir(&Config::state_dir()).join("rejected");
    assert!(dir.exists());
    tools
        .answer_approval(&card.id, answer("reject_once"))
        .await
        .unwrap();
    assert!(store.plugin_get("rejected").unwrap().is_none());
    assert!(!dir.exists());
    let settled = store.get_approval(&card.id).unwrap().unwrap();
    assert_eq!(settled.state, approvals::REJECTED);
}

#[tokio::test]
async fn a_build_changed_on_disk_after_its_card_is_not_installed() {
    if install::target().is_none() {
        return;
    }
    let (store, tools) = rig();
    let card = card(&store, "tampered", "1.0.0");
    let row = store.plugin_get("tampered").unwrap().unwrap();
    let build = row.pending.unwrap().build;
    let dir = install::build_dir(
        &Config::state_dir(),
        "tampered",
        &build.version,
        &build.digest,
    );
    std::fs::write(dir.join("bin/tampered"), b"#!/bin/sh\nevil\n").unwrap();
    tools
        .answer_approval(&card.id, answer("allow_once"))
        .await
        .unwrap();
    let settled = store.get_approval(&card.id).unwrap().unwrap();
    assert_eq!(settled.state, approvals::FAILED);
    assert!(
        settled
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("changed on disk"),
        "{settled:?}"
    );
    assert!(store.plugin_get("tampered").unwrap().is_none());
}

/// One request to the operator API, from the node's own machine.
async fn send(h: &Harness, method: &str, uri: &str, body: Body, json: bool) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:7420")
        .header(
            "content-type",
            if json {
                "application/json"
            } else {
                "application/octet-stream"
            },
        )
        .body(body)
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:5000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let res = h.operator.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn the_api_stages_an_archive_and_the_card_installs_it() {
    if install::target().is_none() {
        return;
    }
    let h = harness().await;
    let (bytes, asset) = archive("by-api", "0.1.0");
    let (s, v) = send(
        &h,
        "POST",
        &format!("/api/plugins/archive?name={asset}&channel=personal"),
        Body::from(bytes),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "by-api");
    assert_eq!(v["new"], true);
    let id = v["approval_id"].as_str().unwrap().to_string();
    let (_, listed) = send(&h, "GET", "/api/plugins", Body::empty(), true).await;
    let row = &listed["plugins"][0];
    assert_eq!(row["state"], "pending");
    assert_eq!(row["pending"]["approval_state"], "pending");
    assert!(row["active"].is_null());

    let (s, v) = send(
        &h,
        "POST",
        &format!("/api/permissions/{id}/answer"),
        Body::from(json!({ "option_id": "allow_once" }).to_string()),
        true,
    )
    .await;
    assert!(s.is_success(), "{s}: {v}");
    let (_, listed) = send(&h, "GET", "/api/plugins", Body::empty(), true).await;
    let row = &listed["plugins"][0];
    assert_eq!(row["state"], "active", "{listed}");
    assert_eq!(row["active"]["version"], "0.1.0");
    assert_eq!(row["active"]["tools"], json!(["by-api__query"]));

    // Nothing before it to go back to.
    let (s, v) = send(
        &h,
        "POST",
        "/api/plugins/by-api/rollback",
        Body::empty(),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    // An owner outside the allowlist is refused before GitHub is asked.
    let (s, v) = send(
        &h,
        "POST",
        "/api/plugins",
        Body::from(json!({ "repo": "evil/plugin", "channel": "personal" }).to_string()),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not an allowed plugin owner"));

    let (s, v) = send(
        &h,
        "DELETE",
        "/api/plugins/by-api?purge=true",
        Body::empty(),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, listed) = send(&h, "GET", "/api/plugins", Body::empty(), true).await;
    assert_eq!(listed["plugins"], json!([]));
}
