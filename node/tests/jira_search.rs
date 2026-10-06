//! Jira search through the operator API: pages joined by cursor on Cloud,
//! the offset fallback on Data Center, the board fields mapped, a channel
//! without the credential refused, and the token never in a response.

#[path = "support/mod.rs"]
mod support;
use support::http::call;
use support::state;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{extract::Query, http::StatusCode, routing::get, Json, Router};
use serde_json::{json, Value};

use tracon::{
    broker::{Broker, Credential, SharedBroker},
    config::Config,
    http::api::AppState,
    mcp::Tools,
    session::Manager,
    store::Store,
    stream::Bus,
};

use support::fake::FakeAdapter;

const TOKEN: &str = "jira-secret-token";

fn issue(id: &str, key: &str, parent: Value) -> Value {
    let mut fields = json!({
        "summary": format!("{key} summary"),
        "status": { "name": "In Progress", "statusCategory": { "key": "indeterminate" } },
        "priority": { "name": "High" },
        "issuetype": { "name": "Task" },
        "labels": ["board", "ops"],
    });
    if !parent.is_null() {
        fields["parent"] = parent;
    }
    json!({ "id": id, "key": key, "self": format!("https://jira.example/rest/api/3/issue/{id}"), "fields": fields })
}

/// Cloud: two pages behind `nextPageToken`.
async fn cloud() -> String {
    let app = Router::new().route(
        "/rest/api/3/search/jql",
        get(|Query(q): Query<BTreeMap<String, String>>| async move {
            assert_eq!(
                q["fields"],
                "summary,status,priority,issuetype,labels,parent"
            );
            assert_eq!(q["maxResults"], "100");
            match q.get("nextPageToken").map(String::as_str) {
                None => Json(json!({
                    "issues": [issue("10", "WRK-1", json!({
                        "key": "WRK-0",
                        "fields": { "summary": "The epic", "issuetype": { "name": "Epic" } },
                    }))],
                    "nextPageToken": "page-2",
                    "isLast": false,
                })),
                Some("page-2") => Json(json!({
                    "issues": [issue("11", "WRK-2", Value::Null)],
                    "isLast": true,
                })),
                Some(other) => panic!("unexpected cursor {other}"),
            }
        }),
    );
    serve(app).await
}

/// Data Center: the newer endpoint is absent, the older pages by offset.
async fn data_center() -> String {
    let app = Router::new()
        .route(
            "/rest/api/3/search/jql",
            get(|| async { (StatusCode::NOT_FOUND, Json(json!({}))) }),
        )
        .route(
            "/rest/api/2/search",
            get(|Query(q): Query<BTreeMap<String, String>>| async move {
                let start: usize = q["startAt"].parse().unwrap();
                let issues: Vec<Value> = (start..(start + 2).min(3))
                    .map(|n| issue(&n.to_string(), &format!("DC-{n}"), Value::Null))
                    .collect();
                Json(json!({ "issues": issues, "startAt": start, "total": 3 }))
            }),
        );
    serve(app).await
}

async fn refusing() -> String {
    let app = Router::new().route(
        "/rest/api/3/search/jql",
        get(|Query(q): Query<BTreeMap<String, String>>| async move {
            if q["jql"] == "bad" {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "errorMessages": ["Error in the JQL Query"] })),
                )
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "errorMessages": ["Unauthorized"] })),
                )
            }
        }),
    );
    serve(app).await
}

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    base
}

fn broker_for(base: &str, channels: &[&str]) -> SharedBroker {
    let broker = Broker::default().shared();
    broker.write().unwrap().put(
        "jira",
        Credential {
            env: [
                ("JIRA_URL", format!("{base}/")),
                ("JIRA_EMAIL", "me@example.com".to_string()),
                ("JIRA_TOKEN", TOKEN.to_string()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
            channels: channels.iter().map(|c| c.to_string()).collect(),
            ..Credential::default()
        },
    );
    broker
}

fn node_with(broker: SharedBroker) -> Router {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let cfg = Arc::new(Config::default());
    let tools = Arc::new(Tools {
        broker,
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
        auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    };
    tracon::http::router(state)
}

async fn search(app: &Router, query: &str) -> (u16, Value) {
    let (status, body) = call(app, "GET", &format!("/api/jira/search?{query}"), None).await;
    assert!(!body.to_string().contains(TOKEN), "token leaked: {body}");
    (status.as_u16(), body)
}

#[tokio::test]
async fn cloud_pages_join_by_cursor_and_map_the_board_fields() {
    let base = cloud().await;
    let app = node_with(broker_for(&base, &["work"]));

    let (status, first) = search(&app, "channel=work&jql=project%20%3D%20WRK").await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["site"], base);
    assert_eq!(first["account_email"], "me@example.com");
    assert_eq!(first["next_cursor"], "page-2");
    assert_eq!(
        first["issues"],
        json!([{
            "id": "10",
            "key": "WRK-1",
            "summary": "WRK-1 summary",
            "status": "In Progress",
            "status_category": "indeterminate",
            "priority": "High",
            "issue_type": "Task",
            "labels": ["board", "ops"],
            "parent": { "key": "WRK-0", "summary": "The epic", "issue_type": "Epic" },
        }])
    );
    assert!(!first.to_string().contains("rest/api"), "{first}");

    let (status, second) = search(&app, "channel=work&jql=project%20%3D%20WRK&cursor=page-2").await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["next_cursor"], Value::Null);
    assert_eq!(second["issues"][0]["key"], "WRK-2");
    assert_eq!(second["issues"][0]["parent"], Value::Null);
}

#[tokio::test]
async fn data_center_falls_back_to_offset_paging() {
    let base = data_center().await;
    let app = node_with(broker_for(&base, &["work"]));

    let (status, first) = search(&app, "channel=work&jql=x").await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["next_cursor"], "2");
    assert_eq!(first["issues"].as_array().unwrap().len(), 2);

    let (status, second) = search(&app, "channel=work&jql=x&cursor=2").await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["issues"][0]["key"], "DC-2");
    assert_eq!(second["next_cursor"], Value::Null);
}

#[tokio::test]
async fn a_channel_without_the_credential_is_refused() {
    let base = cloud().await;
    let app = node_with(broker_for(&base, &["work"]));
    let (status, body) = search(&app, "channel=personal&jql=x").await;
    assert_eq!(status, 422, "{body}");

    let app = node_with(Broker::default().shared());
    let (status, body) = search(&app, "channel=work&jql=x").await;
    assert_eq!(status, 422, "{body}");

    let (status, body) = search(&app, "channel=work&jql=%20").await;
    assert_eq!(status, 422, "{body}");
}

#[tokio::test]
async fn jira_refusals_map_to_request_or_upstream() {
    let base = refusing().await;
    let app = node_with(broker_for(&base, &["work"]));

    let (status, body) = search(&app, "channel=work&jql=bad").await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body.to_string().contains("Error in the JQL Query"),
        "{body}"
    );

    let (status, body) = search(&app, "channel=work&jql=x").await;
    assert_eq!(status, 502, "{body}");
}
