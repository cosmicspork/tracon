//! A fake Kubernetes API, enough of `pods` for the Kubernetes runner: create,
//! get, delete and logs, with each pod's life told by a `Script` rather than
//! by a kubelet.
//!
//! The client is handed the router itself as its transport (`client`), so no
//! socket is opened and a `start_paused` test drives a pod through minutes of
//! its life in no time: a pod's phase is worked out from tokio's clock at each
//! read. A test that needs a real listener (an `attach`, which upgrades to a
//! WebSocket) can serve `router` on a loopback port instead.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
    ContainerStatus, Pod, PodStatus,
};
use serde_json::json;
use tokio::time::Instant;

/// One pod's life from the moment it is created: `pending` waiting to start
/// (with the container's waiting reason, as a kubelet reports a pull), then
/// `running`, then ended with `exit_code`.
#[derive(Debug, Clone)]
pub struct Script {
    pub pending: Duration,
    /// The container's waiting `(reason, message)` while pending; `None`
    /// reports no container status at all, as an unscheduled pod does.
    pub waiting: Option<(String, String)>,
    pub running: Duration,
    /// `None` is a pod that ends without its container reporting an exit:
    /// an eviction.
    pub exit_code: Option<i32>,
    pub logs: String,
    /// What the kubelet reports as the container's `imageID` once started.
    pub image_id: String,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            pending: Duration::ZERO,
            waiting: Some(("ContainerCreating".into(), String::new())),
            running: Duration::ZERO,
            exit_code: Some(0),
            logs: String::new(),
            image_id: String::new(),
        }
    }
}

impl Script {
    pub fn exits(code: i32) -> Self {
        Self {
            exit_code: Some(code),
            ..Default::default()
        }
    }

    /// A pod that stays `Pending` for good, waiting for `reason`.
    pub fn never_starts(reason: &str) -> Self {
        Self {
            pending: Duration::MAX,
            waiting: Some((reason.into(), String::new())),
            ..Default::default()
        }
    }

    pub fn pending(mut self, pending: Duration) -> Self {
        self.pending = pending;
        self
    }

    pub fn running(mut self, running: Duration) -> Self {
        self.running = running;
        self
    }

    pub fn logs(mut self, logs: &str) -> Self {
        self.logs = logs.into();
        self
    }

    pub fn image_id(mut self, image_id: &str) -> Self {
        self.image_id = image_id.into();
        self
    }

    pub fn evicted(mut self) -> Self {
        self.exit_code = None;
        self
    }
}

struct Stored {
    pod: Pod,
    created: Instant,
    script: Script,
}

#[derive(Default)]
struct Inner {
    /// By pod-name prefix, first match wins; otherwise `default`.
    scripts: Vec<(String, Script)>,
    default: Script,
    pods: HashMap<String, Stored>,
    created: Vec<Pod>,
    deleted: Vec<String>,
}

#[derive(Clone)]
pub struct KubeApi {
    namespace: String,
    inner: Arc<Mutex<Inner>>,
}

impl KubeApi {
    pub fn new(namespace: &str) -> Self {
        Self {
            namespace: namespace.into(),
            inner: Arc::default(),
        }
    }

    /// The life every pod gets unless a prefix script claims it.
    pub fn default_script(self, script: Script) -> Self {
        self.inner.lock().unwrap().default = script;
        self
    }

    /// The life of pods whose name starts with `prefix`.
    pub fn script(self, prefix: &str, script: Script) -> Self {
        self.inner
            .lock()
            .unwrap()
            .scripts
            .push((prefix.into(), script));
        self
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/api/v1/namespaces/{ns}/pods", get(list).post(create))
            .route(
                "/api/v1/namespaces/{ns}/pods/{name}",
                get(read).delete(delete),
            )
            .route("/api/v1/namespaces/{ns}/pods/{name}/log", get(logs))
            .with_state(self.clone())
    }

    /// A client whose transport is this API, in process.
    pub fn client(&self) -> kube::Client {
        kube::Client::new(self.router(), self.namespace.clone())
    }

    /// Every pod as it was created, in order.
    pub fn created(&self) -> Vec<Pod> {
        self.inner.lock().unwrap().created.clone()
    }

    /// The names of the pods that existed and were deleted, in order.
    pub fn deleted(&self) -> Vec<String> {
        self.inner.lock().unwrap().deleted.clone()
    }

    /// The names of the pods that exist now.
    pub fn live(&self) -> Vec<String> {
        let mut names: Vec<_> = self.inner.lock().unwrap().pods.keys().cloned().collect();
        names.sort();
        names
    }
}

fn status_error(code: StatusCode, reason: &str, message: String) -> Response {
    (
        code,
        Json(json!({
            "kind": "Status",
            "apiVersion": "v1",
            "metadata": {},
            "status": "Failure",
            "reason": reason,
            "message": message,
            "code": code.as_u16(),
        })),
    )
        .into_response()
}

fn not_found(name: &str) -> Response {
    status_error(
        StatusCode::NOT_FOUND,
        "NotFound",
        format!("pods \"{name}\" not found"),
    )
}

/// The pod as it stands now, by its script and the time since it was made.
fn observed(stored: &Stored) -> Pod {
    let mut pod = stored.pod.clone();
    let script = &stored.script;
    let elapsed = stored.created.elapsed();
    let container = pod
        .spec
        .as_ref()
        .and_then(|s| s.containers.first())
        .cloned()
        .unwrap_or_default();
    let base = ContainerStatus {
        name: container.name.clone(),
        image: container.image.clone().unwrap_or_default(),
        ..Default::default()
    };
    let started = elapsed >= script.pending;
    let ran = elapsed
        .checked_sub(script.pending)
        .is_some_and(|ran| ran >= script.running);
    let status = if !started {
        PodStatus {
            phase: Some("Pending".into()),
            container_statuses: script.waiting.as_ref().map(|(reason, message)| {
                vec![ContainerStatus {
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some(reason.clone()),
                            message: (!message.is_empty()).then(|| message.clone()),
                        }),
                        ..Default::default()
                    }),
                    ..base.clone()
                }]
            }),
            ..Default::default()
        }
    } else if !ran {
        PodStatus {
            phase: Some("Running".into()),
            pod_ip: Some("10.244.0.42".into()),
            container_statuses: Some(vec![ContainerStatus {
                image_id: script.image_id.clone(),
                ready: true,
                started: Some(true),
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning::default()),
                    ..Default::default()
                }),
                ..base.clone()
            }]),
            ..Default::default()
        }
    } else {
        match script.exit_code {
            Some(code) => PodStatus {
                phase: Some(if code == 0 { "Succeeded" } else { "Failed" }.into()),
                container_statuses: Some(vec![ContainerStatus {
                    image_id: script.image_id.clone(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: code,
                            reason: Some(if code == 0 { "Completed" } else { "Error" }.into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..base.clone()
                }]),
                ..Default::default()
            },
            None => PodStatus {
                phase: Some("Failed".into()),
                reason: Some("Evicted".into()),
                message: Some("The node was low on resource: ephemeral-storage.".into()),
                ..Default::default()
            },
        }
    };
    pod.status = Some(status);
    pod
}

async fn create(
    State(api): State<KubeApi>,
    Path(ns): Path<String>,
    Json(mut pod): Json<Pod>,
) -> Response {
    let name = pod.metadata.name.clone().unwrap_or_default();
    let mut inner = api.inner.lock().unwrap();
    if inner.pods.contains_key(&name) {
        return status_error(
            StatusCode::CONFLICT,
            "AlreadyExists",
            format!("pods \"{name}\" already exists"),
        );
    }
    pod.metadata.namespace = Some(ns);
    inner.created.push(pod.clone());
    let script = inner
        .scripts
        .iter()
        .find(|(prefix, _)| name.starts_with(prefix.as_str()))
        .map(|(_, script)| script.clone())
        .unwrap_or_else(|| inner.default.clone());
    let stored = Stored {
        pod,
        created: Instant::now(),
        script,
    };
    let body = observed(&stored);
    inner.pods.insert(name, stored);
    (StatusCode::CREATED, Json(body)).into_response()
}

async fn list(State(api): State<KubeApi>) -> Response {
    let inner = api.inner.lock().unwrap();
    let items: Vec<Pod> = inner.pods.values().map(observed).collect();
    Json(json!({
        "kind": "PodList",
        "apiVersion": "v1",
        "metadata": {},
        "items": items,
    }))
    .into_response()
}

async fn read(State(api): State<KubeApi>, Path((_ns, name)): Path<(String, String)>) -> Response {
    let inner = api.inner.lock().unwrap();
    match inner.pods.get(&name) {
        Some(stored) => Json(observed(stored)).into_response(),
        None => not_found(&name),
    }
}

async fn delete(State(api): State<KubeApi>, Path((_ns, name)): Path<(String, String)>) -> Response {
    let mut inner = api.inner.lock().unwrap();
    match inner.pods.remove(&name) {
        Some(stored) => {
            inner.deleted.push(name);
            Json(observed(&stored)).into_response()
        }
        None => not_found(&name),
    }
}

async fn logs(State(api): State<KubeApi>, Path((_ns, name)): Path<(String, String)>) -> Response {
    let inner = api.inner.lock().unwrap();
    let Some(stored) = inner.pods.get(&name) else {
        return not_found(&name);
    };
    if stored.created.elapsed() < stored.script.pending {
        return status_error(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            format!("container in pod \"{name}\" is waiting to start: ContainerCreating"),
        );
    }
    (
        [(header::CONTENT_TYPE, "text/plain")],
        stored.script.logs.clone(),
    )
        .into_response()
}
