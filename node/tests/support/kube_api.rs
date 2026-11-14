//! A fake Kubernetes API, enough of `pods` for the sandbox provider: create,
//! get, list, delete and logs, with each pod's life told by a `Script` rather
//! than by a kubelet. And `selfsubjectaccessreviews`, allowing everything.
//! And `networkpolicies` get and list, from what a test put in place. Any
//! request for `secrets` is recorded and refused: the node makes none.
//!
//! The client is handed the router itself as its transport (`client`), so no
//! socket is opened and a `start_paused` test drives a pod through minutes of
//! its life in no time: a pod's phase is worked out from tokio's clock at each
//! read.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use k8s_openapi::api::authorization::v1::{SelfSubjectAccessReview, SubjectAccessReviewStatus};
use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
    ContainerStatus, Pod, PodStatus,
};
use k8s_openapi::api::networking::v1::NetworkPolicy;
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
    /// The pod's address once it runs.
    pub pod_ip: String,
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
            pod_ip: "10.244.0.42".into(),
        }
    }
}

impl Script {
    pub fn running(mut self, running: Duration) -> Self {
        self.running = running;
        self
    }

    pub fn pod_ip(mut self, ip: &str) -> Self {
        self.pod_ip = ip.into();
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
    /// `METHOD path` of every request for a Secret, in order.
    secret_requests: Vec<String>,
    /// `(resource, verb)` an access review answers no to.
    denied: Vec<(String, String)>,
    policies: HashMap<String, NetworkPolicy>,
    /// Policy names a read of answers 403.
    policies_forbidden: Vec<String>,
    /// Held reads wait for a permit here, one per read.
    policy_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// How many policy reads have arrived, held or not.
    policy_reads: usize,
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

    /// Put a NetworkPolicy in place under its name.
    pub fn insert_policy(&self, policy: NetworkPolicy) {
        let name = policy.metadata.name.clone().unwrap_or_default();
        self.inner.lock().unwrap().policies.insert(name, policy);
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route(
                "/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/{name}",
                get(read_policy),
            )
            .route(
                "/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies",
                get(list_policies),
            )
            .route("/api/v1/namespaces/{ns}/pods", get(list).post(create))
            .route(
                "/api/v1/namespaces/{ns}/pods/{name}",
                get(read).delete(delete),
            )
            .route("/api/v1/namespaces/{ns}/pods/{name}/log", get(logs))
            .route("/api/v1/namespaces/{ns}/secrets", any(secret_request))
            .route(
                "/api/v1/namespaces/{ns}/secrets/{name}",
                any(secret_request),
            )
            .route(
                "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
                post(review),
            )
            .with_state(self.clone())
    }

    /// `METHOD path` of every request for a Secret, in order.
    pub fn secret_requests(&self) -> Vec<String> {
        self.inner.lock().unwrap().secret_requests.clone()
    }

    /// Put a pod in place as if an earlier node had made it, with `script`'s
    /// life counted from now.
    pub fn insert_pod(&self, mut pod: Pod, script: Script) {
        let name = pod.metadata.name.clone().unwrap_or_default();
        pod.metadata.uid = Some(uuid::Uuid::now_v7().to_string());
        self.inner.lock().unwrap().pods.insert(
            name,
            Stored {
                pod,
                created: Instant::now(),
                script,
            },
        );
    }

    /// A client whose transport is this API, in process.
    pub fn client(&self) -> kube::Client {
        kube::Client::new(self.router(), self.namespace.clone())
    }

    /// Every pod as it was created, in order.
    pub fn created(&self) -> Vec<Pod> {
        self.inner.lock().unwrap().created.clone()
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
            pod_ip: Some(script.pod_ip.clone()),
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
    pod.metadata.uid = Some(uuid::Uuid::now_v7().to_string());
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

/// Any request for a Secret: recorded, and refused as the shipped Role,
/// which grants nothing on Secrets, would have it refused.
async fn secret_request(
    State(api): State<KubeApi>,
    method: axum::http::Method,
    uri: axum::http::Uri,
) -> Response {
    api.inner
        .lock()
        .unwrap()
        .secret_requests
        .push(format!("{method} {}", uri.path()));
    status_error(
        StatusCode::FORBIDDEN,
        "Forbidden",
        "secrets is forbidden: User \"system:serviceaccount:tracon:tracon-node\" cannot \
         access resource \"secrets\""
            .into(),
    )
}

async fn review(
    State(api): State<KubeApi>,
    Json(mut review): Json<SelfSubjectAccessReview>,
) -> Response {
    let inner = api.inner.lock().unwrap();
    let attributes = review.spec.resource_attributes.clone().unwrap_or_default();
    let resource = attributes.resource.unwrap_or_default();
    let verb = attributes.verb.unwrap_or_default();
    let allowed = !inner
        .denied
        .iter()
        .any(|(r, v)| *r == resource && *v == verb);
    review.status = Some(SubjectAccessReviewStatus {
        allowed,
        ..Default::default()
    });
    (StatusCode::CREATED, Json(review)).into_response()
}

/// Every policy in place, as a list (`list` on networkpolicies).
async fn list_policies(State(api): State<KubeApi>) -> Response {
    let inner = api.inner.lock().unwrap();
    let mut items: Vec<NetworkPolicy> = inner.policies.values().cloned().collect();
    items.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
    Json(json!({
        "kind": "NetworkPolicyList",
        "apiVersion": "networking.k8s.io/v1",
        "metadata": {},
        "items": items,
    }))
    .into_response()
}

async fn read_policy(
    State(api): State<KubeApi>,
    Path((_ns, name)): Path<(String, String)>,
) -> Response {
    let gate = {
        let mut inner = api.inner.lock().unwrap();
        inner.policy_reads += 1;
        inner.policy_gate.clone()
    };
    if let Some(gate) = gate {
        gate.acquire().await.expect("gate open").forget();
    }
    let inner = api.inner.lock().unwrap();
    if inner.policies_forbidden.contains(&name) {
        return status_error(
            StatusCode::FORBIDDEN,
            "Forbidden",
            format!("networkpolicies.networking.k8s.io \"{name}\" is forbidden"),
        );
    }
    match inner.policies.get(&name) {
        Some(policy) => Json(policy.clone()).into_response(),
        None => status_error(
            StatusCode::NOT_FOUND,
            "NotFound",
            format!("networkpolicies.networking.k8s.io \"{name}\" not found"),
        ),
    }
}
