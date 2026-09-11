//! Forges through the operator API: listing is channel-scoped through the
//! broker, a missing credential means an absent forge rather than an error,
//! and a bound-elsewhere credential answers with the refusal, per forge.

#[path = "support/mod.rs"]
mod support;
use support::http::call;
use support::state;

use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use serde_json::json;
use axum::response::IntoResponse;

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

/// A forge on loopback: one GitHub-shaped listing, one GitLab-shaped one.
async fn fake_forge() -> std::net::SocketAddr {
    let app = axum::Router::new()
        .route(
            "/user/repos",
            axum::routing::get(|| async {
                axum::Json(json!([
                    { "full_name": "me/proj", "private": true, "default_branch": "main",
                      "pushed_at": "2026-08-29T10:00:00Z" },
                    { "full_name": "me/site", "private": false, "default_branch": "main",
                      "pushed_at": "2026-08-20T10:00:00Z" }
                ]))
            }),
        )
        .route(
            "/api/v4/projects",
            axum::routing::get(|| async {
                axum::Json(json!([
                    { "path_with_namespace": "group/tool", "visibility": "private",
                      "default_branch": "main", "last_activity_at": "2026-08-28T10:00:00Z" }
                ]))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A GitHub-shaped forge that requires the picker to ask explicitly for page
/// two. The next link uses the request host so the node's strict host check
/// still accepts this loopback fixture.
async fn paged_github_forge() -> std::net::SocketAddr {
    let app = axum::Router::new().route(
        "/user/repos",
        axum::routing::get(
            |headers: axum::http::HeaderMap,
             axum::extract::Query(query): axum::extract::Query<BTreeMap<String, String>>| async move {
                let page = query.get("page").map(String::as_str).unwrap_or("1");
                let rows = match page {
                    "1" => json!([{ "full_name": "me/first", "private": true }]),
                    "2" => json!([{ "full_name": "me/second", "private": false }]),
                    _ => json!([]),
                };
                let mut response = axum::Json(rows).into_response();
                if page == "1" {
                    let host = headers.get("host").and_then(|value| value.to_str().ok()).unwrap();
                    let next = format!(
                        "<http://{host}/user/repos?per_page=50&sort=pushed&page=2>; rel=\"next\""
                    );
                    response
                        .headers_mut()
                        .insert(axum::http::header::LINK, next.parse().unwrap());
                }
                response
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// The redirect destination represents an attacker-controlled host. Its only
/// observable is whether a GitLab token ever reaches it.
async fn redirect_target(seen: Arc<AtomicBool>) -> std::net::SocketAddr {
    let app = axum::Router::new().route(
        "/stolen",
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let seen = seen.clone();
            async move {
                if headers.contains_key("private-token") {
                    seen.store(true, Ordering::Relaxed);
                }
                axum::http::StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A malicious forge redirecting an authenticated API call to another host.
async fn redirecting_gitlab_forge(target: std::net::SocketAddr) -> std::net::SocketAddr {
    let destination = format!("http://{target}/stolen");
    let app = axum::Router::new().route(
        "/api/v4/projects",
        axum::routing::get(move || {
            let destination = destination.clone();
            async move {
                let mut response = axum::http::StatusCode::FOUND.into_response();
                response
                    .headers_mut()
                    .insert(axum::http::header::LOCATION, destination.parse().unwrap());
                response
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn node_with(broker: SharedBroker) -> axum::Router {
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

fn cred(env: &[(&str, &str)], channels: &[&str]) -> Credential {
    Credential {
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
        channels: channels.iter().map(|c| c.to_string()).collect(),
        ..Credential::default()
    }
}

#[tokio::test]
async fn forge_listing_is_channel_scoped_and_per_forge_honest() {
    let addr = fake_forge().await;
    let broker = Broker::default().shared();
    {
        let mut b = broker.write().unwrap();
        // gh may serve personal; glab exists but is bound to work only.
        b.put(
            "gh",
            cred(
                &[
                    ("GH_TOKEN", "fake-token-for-tests"),
                    ("GITHUB_API", &format!("http://{addr}")),
                ],
                &["personal"],
            ),
        );
        b.put(
            "glab",
            cred(
                &[
                    ("GITLAB_TOKEN", "fake-token-for-tests"),
                    ("GITLAB_HOST", &format!("http://{addr}")),
                ],
                &["work"],
            ),
        );
    }
    let app = node_with(broker);

    let (status, body) = call(&app, "GET", "/api/forge/repos?channel=personal", None).await;
    assert_eq!(status, 200);
    let forges = body["forges"].as_array().unwrap();
    assert_eq!(forges.len(), 2);
    let github = &forges[0];
    assert_eq!(github["forge"], "github");
    assert_eq!(github["repos"].as_array().unwrap().len(), 2);
    assert_eq!(github["repos"][0]["full_name"], "me/proj");
    assert_eq!(github["repos"][0]["owner"], "me");
    assert_eq!(github["repos"][0]["host"], "github.com");
    assert_eq!(github["repos"][0]["private"], true);
    // glab is present but refused for this channel, and says why.
    let gitlab = &forges[1];
    assert_eq!(gitlab["forge"], "gitlab");
    assert_eq!(gitlab["repos"].as_array().unwrap().len(), 0);
    assert_eq!(
        gitlab["error"],
        "credential glab is not bound to channel personal"
    );

    // On work, gitlab lists and github is the refused one.
    let (_, body) = call(&app, "GET", "/api/forge/repos?channel=work", None).await;
    let forges = body["forges"].as_array().unwrap();
    assert!(forges[0]["error"].as_str().is_some());
    assert_eq!(forges[1]["repos"][0]["full_name"], "group/tool");
    assert_eq!(forges[1]["repos"][0]["owner"], "group");
}

#[tokio::test]
async fn forge_listing_pages_only_the_requested_provider() {
    let addr = paged_github_forge().await;
    let broker = Broker::default().shared();
    broker.write().unwrap().put(
        "gh",
        cred(
            &[
                ("GH_TOKEN", "fake-token-for-tests"),
                ("GITHUB_API", &format!("http://{addr}")),
            ],
            &["personal"],
        ),
    );
    let app = node_with(broker);

    let (_, first) = call(&app, "GET", "/api/forge/repos?channel=personal", None).await;
    assert_eq!(first["forges"][0]["repos"][0]["full_name"], "me/first");
    assert_eq!(first["forges"][0]["next_cursor"], "2");
    assert_eq!(first["forges"][0]["complete"], false);

    let (_, second) = call(
        &app,
        "GET",
        "/api/forge/repos?channel=personal&forge=github&cursor=2",
        None,
    )
    .await;
    assert_eq!(second["forges"].as_array().unwrap().len(), 1);
    assert_eq!(second["forges"][0]["repos"][0]["full_name"], "me/second");
    assert!(second["forges"][0]["next_cursor"].is_null());
    assert_eq!(second["forges"][0]["complete"], true);
}

#[tokio::test]
async fn forge_listing_never_follows_a_credentialed_redirect() {
    let seen = Arc::new(AtomicBool::new(false));
    let target = redirect_target(seen.clone()).await;
    let redirector = redirecting_gitlab_forge(target).await;
    let broker = Broker::default().shared();
    broker.write().unwrap().put(
        "glab",
        cred(
            &[
                ("GITLAB_TOKEN", "fake-token-for-tests"),
                ("GITLAB_HOST", &format!("http://{redirector}")),
            ],
            &["personal"],
        ),
    );
    let app = node_with(broker);

    let (status, body) = call(
        &app,
        "GET",
        "/api/forge/repos?channel=personal&forge=gitlab",
        None,
    )
    .await;

    assert_eq!(status, 200);
    assert!(body["forges"][0]["error"].as_str().is_some());
    assert!(!seen.load(Ordering::Relaxed));
}

#[tokio::test]
async fn a_forge_with_no_credential_is_absent_not_an_error() {
    let app = node_with(Broker::default().shared());
    let (status, body) = call(&app, "GET", "/api/forge/repos?channel=personal", None).await;
    assert_eq!(status, 200);
    assert_eq!(body["forges"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn cloning_without_a_usable_credential_is_refused_with_the_reason() {
    let app = node_with(Broker::default().shared());
    let (status, body) = call(
        &app,
        "POST",
        "/api/repos/clone",
        Some(json!({
            "channel": "personal", "forge": "github",
            "host": "github.com", "owner": "me", "name": "proj"
        })),
    )
    .await;
    assert_eq!(status, 409);
    assert!(body["error"]["message"].as_str().unwrap().contains("gh"));
}

#[tokio::test]
async fn hostile_clone_paths_are_refused_before_anything_runs() {
    let broker = Broker::default().shared();
    broker.write().unwrap().put(
        "gh",
        cred(&[("GH_TOKEN", "fake-token-for-tests")], &["personal"]),
    );
    let app = node_with(broker);
    let (status, _) = call(
        &app,
        "POST",
        "/api/repos/clone",
        Some(json!({
            "channel": "personal", "forge": "github",
            "host": "github.com", "owner": "..", "name": "proj"
        })),
    )
    .await;
    assert_eq!(status, 422);
}

#[tokio::test]
async fn an_existing_managed_clone_answers_with_its_path() {
    let broker = Broker::default().shared();
    broker.write().unwrap().put(
        "gh",
        cred(&[("GH_TOKEN", "fake-token-for-tests")], &["personal"]),
    );
    let app = node_with(broker);
    // isolate() pointed the state dir at scratch; plant a clone there.
    let dest = tracon::forge::managed_root(&Config::state_dir())
        .join("github.com")
        .join("me")
        .join("proj");
    std::fs::create_dir_all(dest.join(".git")).unwrap();
    let (status, body) = call(
        &app,
        "POST",
        "/api/repos/clone",
        Some(json!({
            "channel": "personal", "forge": "github",
            "host": "github.com", "owner": "me", "name": "proj"
        })),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["repo_path"].as_str().unwrap(), dest.to_str().unwrap());
    // And the recents endpoint offers it before any session ran.
    let (_, body) = call(&app, "GET", "/api/repos/recent", None).await;
    assert!(
        body["managed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["full_name"] == "me/proj"),
        "{body}"
    );
}

#[tokio::test]
async fn a_gitlab_group_path_clones_under_its_host() {
    let broker = Broker::default().shared();
    broker.write().unwrap().put(
        "glab",
        cred(&[("GITLAB_TOKEN", "fake-token-for-tests")], &["work"]),
    );
    let app = node_with(broker);
    let dest = tracon::forge::managed_root(&Config::state_dir())
        .join("gitlab.example")
        .join("group")
        .join("sub")
        .join("tool");
    std::fs::create_dir_all(dest.join(".git")).unwrap();
    let (status, body) = call(
        &app,
        "POST",
        "/api/repos/clone",
        Some(json!({
            "channel": "work", "forge": "gitlab",
            "host": "gitlab.example", "owner": "group/sub", "name": "tool"
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["repo_path"].as_str().unwrap(), dest.to_str().unwrap());
    // The recents list is shared with the other clone tests in this process.
    let (_, body) = call(&app, "GET", "/api/repos/recent", None).await;
    let managed = body["managed"].as_array().unwrap();
    let row = managed
        .iter()
        .find(|m| m["full_name"] == "group/sub/tool")
        .unwrap_or_else(|| panic!("nested clone not listed: {body}"));
    assert_eq!(row["repo_path"].as_str().unwrap(), dest.to_str().unwrap());
    assert_eq!(row["host"], "gitlab.example");
}

#[tokio::test]
async fn a_rejected_token_says_to_replace_it() {
    let app = axum::Router::new().route(
        "/user/repos",
        axum::routing::get(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({ "message": "Bad credentials" })),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let broker = Broker::default().shared();
    broker.write().unwrap().put(
        "gh",
        cred(
            &[
                ("GH_TOKEN", "fake-token-for-tests"),
                ("GITHUB_API", &format!("http://{addr}")),
            ],
            &["personal"],
        ),
    );
    let app = node_with(broker);
    let (_, body) = call(&app, "GET", "/api/forge/repos?channel=personal", None).await;
    let error = body["forges"][0]["error"].as_str().unwrap();
    assert!(error.contains("401"), "{error}");
    assert!(error.contains("Settings"), "{error}");
}
