//! A stand-in for DigitalOcean's Harness Runtime API, holding sessions,
//! configs, templates and checkpoints in memory the way the live API was
//! seen to: a create from a manifest stores a config named after the
//! session, removing the session leaves that config, a template builds
//! before it is ready. What an `exec` does is the test's (`on_exec`): a
//! real sandbox's template service is played by the test.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

pub type OnExec = Arc<dyn Fn(&str, &[String]) -> i32 + Send + Sync>;

#[derive(Default)]
pub struct Api {
    pub sessions: BTreeMap<String, Value>,
    pub configs: BTreeMap<String, Value>,
    pub templates: BTreeMap<String, Value>,
    /// Session id → its checkpoints, newest first.
    pub checkpoints: BTreeMap<String, Vec<Value>>,
    /// Every manifest a create was sent, as sent.
    pub manifests: Vec<Value>,
    /// Every exec: the session and its argv.
    pub execs: Vec<(String, Vec<String>)>,
    pub deleted_sessions: Vec<String>,
    pub deleted_configs: Vec<String>,
    pub deleted_checkpoints: Vec<String>,
    pub deleted_templates: Vec<String>,
    /// Gets a template answers BUILDING before it is READY.
    pub template_builds: u32,
    next: u64,
}

impl Api {
    fn id(&mut self, kind: &str) -> String {
        self.next += 1;
        format!("0190c0de-0000-7000-8000-{kind}{:010}", self.next)
    }

    /// A session of `name` that is not this API's caller's doing (another
    /// node's, or one left from before), with its config.
    pub fn put_session(&mut self, name: &str, status: &str) -> String {
        let id = self.id("aa");
        let config = self.id("cc");
        self.configs.insert(
            config.clone(),
            json!({"id": config, "name": name, "created_at": "2026-11-02T00:00:00Z",
                   "manifest": {"name": name, "harness": "none"}}),
        );
        self.sessions.insert(
            id.clone(),
            json!({"session_id": id, "name": name, "agent_kind": "AGENT_KIND_NONE",
                   "status": status, "config_id": config}),
        );
        id
    }

    pub fn put_checkpoint(
        &mut self,
        session: &str,
        status: &str,
        created_at: &str,
        size: u64,
    ) -> String {
        let id = self.id("cp");
        self.checkpoints
            .entry(session.to_string())
            .or_default()
            .push(json!({
                "checkpoint_id": id, "session_id": session, "status": status,
                "kind": "implicit", "size_bytes": size.to_string(), "created_at": created_at,
            }));
        id
    }

    pub fn put_template(&mut self, name: &str, status: &str) -> String {
        let id = self.id("tt");
        self.templates.insert(
            id.clone(),
            json!({"template_id": id, "name": name, "status": status}),
        );
        id
    }
}

#[derive(Clone)]
pub struct FakeDo {
    pub api: Arc<Mutex<Api>>,
    pub on_exec: Arc<Mutex<Option<OnExec>>>,
    pub url: String,
}

type S = FakeDo;

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"id": "not_found", "message": "The resource you were accessing could not be found."})),
    )
        .into_response()
}

const P: &str = "/v2/harness-runtime";

impl FakeDo {
    pub async fn start() -> FakeDo {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = FakeDo {
            api: Arc::default(),
            on_exec: Arc::default(),
            url: format!("http://{}", listener.local_addr().unwrap()),
        };
        let app = Router::new()
            .route(
                &format!("{P}/templates"),
                get(templates).post(template_create),
            )
            .route(&format!("{P}/templates/{{id}}"), delete(template_delete))
            .route(&format!("{P}/sessions"), get(sessions).post(session_create))
            .route(
                &format!("{P}/sessions/{{id}}"),
                get(session_get).delete(session_delete),
            )
            .route(&format!("{P}/sessions/{{id}}/sandbox/exec"), post(exec))
            .route(
                &format!("{P}/sessions/{{id}}/checkpoints"),
                get(checkpoints).post(checkpoint_create),
            )
            .route(
                &format!("{P}/sessions/{{id}}/checkpoints/{{cp}}"),
                delete(checkpoint_delete),
            )
            .route(&format!("{P}/sessions/{{id}}/fork"), post(fork))
            .route(&format!("{P}/configs"), get(configs))
            .route(
                &format!("{P}/configs/{{id}}"),
                get(config_get).delete(config_delete),
            )
            .with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        fake
    }

    pub fn on_exec(&self, f: OnExec) {
        *self.on_exec.lock().unwrap() = Some(f);
    }

    pub fn client(&self) -> tracon::do_harness::Client {
        tracon::do_harness::Client::with_base_url(&self.url, "do-test-token").unwrap()
    }
}

async fn templates(State(s): State<S>) -> Response {
    let mut api = s.api.lock().unwrap();
    let building = api.template_builds;
    let list: Vec<Value> = api
        .templates
        .values_mut()
        .map(|t| {
            if t["status"] == "TEMPLATE_STATUS_BUILDING" && building == 0 {
                t["status"] = json!("TEMPLATE_STATUS_READY");
            }
            t.clone()
        })
        .collect();
    api.template_builds = api.template_builds.saturating_sub(1);
    Json(json!({"templates": list, "next_page_token": ""})).into_response()
}

async fn template_create(State(s): State<S>, Json(body): Json<Value>) -> Response {
    let mut api = s.api.lock().unwrap();
    let id = api.id("tt");
    let digest = body["source_oci_ref"]
        .as_str()
        .and_then(|r| r.rsplit_once('@'))
        .map(|(_, d)| d.to_string());
    let template = json!({
        "template_id": id, "name": body["name"], "status": "TEMPLATE_STATUS_BUILDING",
        "spec": {"base_template": body["base_template"], "image": {"digest": digest}},
    });
    api.templates.insert(id, template.clone());
    Json(json!({"template": template})).into_response()
}

async fn template_delete(State(s): State<S>, Path(id): Path<String>) -> Response {
    let mut api = s.api.lock().unwrap();
    match api.templates.remove(&id) {
        Some(t) => {
            api.deleted_templates
                .push(t["name"].as_str().unwrap_or("").into());
            StatusCode::NO_CONTENT.into_response()
        }
        None => not_found(),
    }
}

async fn sessions(State(s): State<S>) -> Response {
    let api = s.api.lock().unwrap();
    let list: Vec<Value> = api.sessions.values().cloned().collect();
    Json(json!({"sessions": list, "next_page_token": ""})).into_response()
}

async fn session_create(State(s): State<S>, body: String) -> Response {
    let manifest: Value = serde_json::from_str(&body).unwrap();
    let mut api = s.api.lock().unwrap();
    api.manifests.push(manifest.clone());
    let id = api.id("aa");
    let config = api.id("cc");
    let name = manifest["name"].as_str().unwrap_or("").to_string();
    api.configs.insert(
        config.clone(),
        json!({"id": config, "name": name, "created_at": "2026-11-02T00:00:00Z", "manifest": manifest}),
    );
    // Provisioning when made; ready the first time it is read.
    let session = json!({
        "session_id": id, "name": name, "agent_kind": "AGENT_KIND_NONE",
        "status": "SESSION_STATUS_PROVISIONING", "keep_warm": manifest["keep_warm"],
        "config_id": config,
    });
    api.sessions.insert(id, session.clone());
    (StatusCode::CREATED, Json(json!({"session": session}))).into_response()
}

async fn session_get(State(s): State<S>, Path(id): Path<String>) -> Response {
    let mut api = s.api.lock().unwrap();
    let Some(session) = api.sessions.get_mut(&id) else {
        return not_found();
    };
    let shown = session.clone();
    if session["status"] == "SESSION_STATUS_PROVISIONING" {
        session["status"] = json!("SESSION_STATUS_READY");
    }
    Json(json!({"session": shown})).into_response()
}

async fn session_delete(State(s): State<S>, Path(id): Path<String>) -> Response {
    let mut api = s.api.lock().unwrap();
    match api.sessions.remove(&id) {
        Some(_) => {
            api.deleted_sessions.push(id);
            StatusCode::NO_CONTENT.into_response()
        }
        None => not_found(),
    }
}

async fn exec(State(s): State<S>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let argv: Vec<String> = serde_json::from_value(body["argv"].clone()).unwrap_or_default();
    {
        let mut api = s.api.lock().unwrap();
        if !api.sessions.contains_key(&id) {
            return not_found();
        }
        api.execs.push((id.clone(), argv.clone()));
    }
    let hook = s.on_exec.lock().unwrap().clone();
    let code = hook.map(|f| f(&id, &argv)).unwrap_or(0);
    Json(json!({"exit_code": code, "stdout": "", "stderr": ""})).into_response()
}

async fn checkpoints(State(s): State<S>, Path(id): Path<String>) -> Response {
    let api = s.api.lock().unwrap();
    if !api.sessions.contains_key(&id) {
        return not_found();
    }
    let list = api.checkpoints.get(&id).cloned().unwrap_or_default();
    Json(json!({"checkpoints": list, "next_page_token": ""})).into_response()
}

async fn checkpoint_create(State(s): State<S>, Path(id): Path<String>) -> Response {
    let mut api = s.api.lock().unwrap();
    let cp = api.put_checkpoint(&id, "READY", "2026-11-02T12:00:00Z", 51_000_000_000);
    let made = api.checkpoints[&id]
        .iter()
        .find(|c| c["checkpoint_id"] == cp.as_str())
        .cloned();
    Json(json!({"checkpoint": made})).into_response()
}

async fn checkpoint_delete(State(s): State<S>, Path((id, cp)): Path<(String, String)>) -> Response {
    let mut api = s.api.lock().unwrap();
    if let Some(list) = api.checkpoints.get_mut(&id) {
        list.retain(|c| c["checkpoint_id"] != cp.as_str());
    }
    api.deleted_checkpoints.push(cp);
    StatusCode::NO_CONTENT.into_response()
}

async fn fork(State(s): State<S>, Path(id): Path<String>) -> Response {
    let mut api = s.api.lock().unwrap();
    let Some(parent) = api.sessions.get(&id).cloned() else {
        return not_found();
    };
    let child = api.id("aa");
    let config = api.id("cc");
    let name = format!("{}-fork", parent["name"].as_str().unwrap_or(""));
    let manifest = api
        .configs
        .get(parent["config_id"].as_str().unwrap_or(""))
        .map(|c| c["manifest"].clone());
    api.configs.insert(
        config.clone(),
        json!({"id": config, "name": name, "created_at": "2026-11-02T00:00:00Z",
               "manifest": manifest}),
    );
    let session = json!({
        "session_id": child, "name": name, "agent_kind": "AGENT_KIND_NONE",
        "status": "SESSION_STATUS_READY", "config_id": config, "parent_session_id": id,
    });
    api.sessions.insert(child, session.clone());
    Json(json!({"sessions": [session]})).into_response()
}

async fn configs(State(s): State<S>) -> Response {
    let api = s.api.lock().unwrap();
    let list: Vec<Value> = api
        .configs
        .values()
        .map(|c| {
            let mut c = c.clone();
            c.as_object_mut().unwrap().remove("manifest");
            c
        })
        .collect();
    Json(json!({"configs": list, "next_page_token": ""})).into_response()
}

async fn config_get(State(s): State<S>, Path(id): Path<String>) -> Response {
    let api = s.api.lock().unwrap();
    match api.configs.get(&id) {
        Some(c) => Json(json!({"config": c})).into_response(),
        None => not_found(),
    }
}

async fn config_delete(State(s): State<S>, Path(id): Path<String>) -> Response {
    let mut api = s.api.lock().unwrap();
    match api.configs.remove(&id) {
        Some(_) => {
            api.deleted_configs.push(id);
            StatusCode::NO_CONTENT.into_response()
        }
        None => not_found(),
    }
}
