//! A fake Kubernetes API, enough of `pods` for the Kubernetes runner: create,
//! get, delete and logs, with each pod's life told by a `Script` rather than
//! by a kubelet. Enough of `secrets` for a run's own Secret: create, list,
//! patch (a merge of metadata; data stays immutable) and delete, every call
//! recorded. And `selfsubjectaccessreviews`, allowing everything a test has
//! not denied. And `networkpolicies` get, from what a test put in place, with
//! a gate a test can hold reads at and names it can make unreadable.
//!
//! A kubelet's one rule about Secrets is kept: a pod whose `secretKeyRef`
//! names a Secret or key that does not exist when it is created never starts
//! (`CreateContainerConfigError`).
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

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use k8s_openapi::api::authorization::v1::{SelfSubjectAccessReview, SubjectAccessReviewStatus};
use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
    ContainerStatus, Pod, PodStatus, Secret,
};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use serde_json::{json, Value};
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
    secrets: HashMap<String, Secret>,
    secrets_created: Vec<Secret>,
    secrets_deleted: Vec<String>,
    /// `(secret, merge patch body)`, in order.
    secret_patches: Vec<(String, Value)>,
    /// For each pod created, the Secrets that existed at that moment.
    secrets_at_pod_create: Vec<(String, Vec<String>)>,
    /// `(resource, verb)` an access review answers no to.
    denied: Vec<(String, String)>,
    /// A Secret request answers 403, as the API does for a Role without
    /// the rule, whatever the access reviews say.
    secrets_forbidden: bool,
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

    /// The life of pods whose name starts with `prefix`.
    pub fn script(self, prefix: &str, script: Script) -> Self {
        self.inner
            .lock()
            .unwrap()
            .scripts
            .push((prefix.into(), script));
        self
    }

    /// Access reviews answer no to `verb` on `resource`.
    pub fn deny(self, resource: &str, verb: &str) -> Self {
        self.inner
            .lock()
            .unwrap()
            .denied
            .push((resource.into(), verb.into()));
        self
    }

    /// Every Secret request is refused 403.
    pub fn forbid_secrets(self) -> Self {
        self.inner.lock().unwrap().secrets_forbidden = true;
        self
    }

    /// Put a NetworkPolicy in place under its name.
    pub fn insert_policy(&self, policy: NetworkPolicy) {
        let name = policy.metadata.name.clone().unwrap_or_default();
        self.inner.lock().unwrap().policies.insert(name, policy);
    }

    /// Reads of this policy answer 403.
    pub fn forbid_policy(self, name: &str) -> Self {
        self.inner
            .lock()
            .unwrap()
            .policies_forbidden
            .push(name.into());
        self
    }

    /// Hold every policy read until a permit is added to the returned gate.
    pub fn hold_policy_reads(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        self.inner.lock().unwrap().policy_gate = Some(gate.clone());
        gate
    }

    /// How many policy reads have arrived.
    pub fn policy_reads(&self) -> usize {
        self.inner.lock().unwrap().policy_reads
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route(
                "/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/{name}",
                get(read_policy),
            )
            .route("/api/v1/namespaces/{ns}/pods", get(list).post(create))
            .route(
                "/api/v1/namespaces/{ns}/pods/{name}",
                get(read).delete(delete),
            )
            .route("/api/v1/namespaces/{ns}/pods/{name}/log", get(logs))
            .route(
                "/api/v1/namespaces/{ns}/secrets",
                get(list_secrets).post(create_secret),
            )
            .route(
                "/api/v1/namespaces/{ns}/secrets/{name}",
                get(read_secret).delete(delete_secret).patch(patch_secret),
            )
            .route(
                "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
                post(review),
            )
            .with_state(self.clone())
    }

    /// Every Secret as it was created, in order.
    pub fn secrets_created(&self) -> Vec<Secret> {
        self.inner.lock().unwrap().secrets_created.clone()
    }

    /// The names of the Secrets that existed and were deleted, in order.
    pub fn secrets_deleted(&self) -> Vec<String> {
        self.inner.lock().unwrap().secrets_deleted.clone()
    }

    /// The Secrets that exist now, as they stand (patches applied).
    pub fn secrets_live(&self) -> Vec<Secret> {
        let inner = self.inner.lock().unwrap();
        let mut live: Vec<Secret> = inner.secrets.values().cloned().collect();
        live.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
        live
    }

    pub fn secret_patches(&self) -> Vec<(String, Value)> {
        self.inner.lock().unwrap().secret_patches.clone()
    }

    /// For each pod, by name, the Secrets that existed when it was created.
    pub fn secrets_at_pod_create(&self) -> Vec<(String, Vec<String>)> {
        self.inner.lock().unwrap().secrets_at_pod_create.clone()
    }

    /// Put a Secret in place as if an earlier node had made it.
    pub fn insert_secret(&self, secret: Secret) {
        let name = secret.metadata.name.clone().unwrap_or_default();
        self.inner.lock().unwrap().secrets.insert(name, secret);
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
    pod.metadata.uid = Some(uuid::Uuid::now_v7().to_string());
    inner.created.push(pod.clone());
    let mut existing: Vec<String> = inner.secrets.keys().cloned().collect();
    existing.sort();
    inner.secrets_at_pod_create.push((name.clone(), existing));
    let script = inner
        .scripts
        .iter()
        .find(|(prefix, _)| name.starts_with(prefix.as_str()))
        .map(|(_, script)| script.clone())
        .unwrap_or_else(|| inner.default.clone());
    let script = match missing_secret_ref(&inner.secrets, &pod) {
        Some(_) => Script::never_starts("CreateContainerConfigError"),
        None => script,
    };
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

/// The first `secretKeyRef` in `pod` naming a Secret or a key that does not
/// exist, as a kubelet would refuse it.
fn missing_secret_ref(secrets: &HashMap<String, Secret>, pod: &Pod) -> Option<String> {
    let containers = pod.spec.as_ref()?.containers.iter();
    for container in containers {
        for var in container.env.iter().flatten() {
            let Some(selector) = var
                .value_from
                .as_ref()
                .and_then(|from| from.secret_key_ref.as_ref())
            else {
                continue;
            };
            let found = secrets
                .get(&selector.name)
                .and_then(|s| s.data.as_ref())
                .is_some_and(|data| data.contains_key(&selector.key));
            if !found {
                return Some(format!("{}/{}", selector.name, selector.key));
            }
        }
    }
    None
}

fn secrets_forbidden(name: &str) -> Response {
    status_error(
        StatusCode::FORBIDDEN,
        "Forbidden",
        format!(
            "secrets \"{name}\" is forbidden: User \"system:serviceaccount:tracon:tracon-node\" \
             cannot access resource \"secrets\""
        ),
    )
}

fn secret_not_found(name: &str) -> Response {
    status_error(
        StatusCode::NOT_FOUND,
        "NotFound",
        format!("secrets \"{name}\" not found"),
    )
}

async fn create_secret(
    State(api): State<KubeApi>,
    Path(ns): Path<String>,
    Json(mut secret): Json<Secret>,
) -> Response {
    let name = secret.metadata.name.clone().unwrap_or_default();
    let mut inner = api.inner.lock().unwrap();
    if inner.secrets_forbidden {
        return secrets_forbidden(&name);
    }
    if inner.secrets.contains_key(&name) {
        return status_error(
            StatusCode::CONFLICT,
            "AlreadyExists",
            format!("secrets \"{name}\" already exists"),
        );
    }
    secret.metadata.namespace = Some(ns);
    secret.metadata.uid = Some(uuid::Uuid::now_v7().to_string());
    secret.metadata.creation_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
    inner.secrets_created.push(secret.clone());
    inner.secrets.insert(name, secret.clone());
    (StatusCode::CREATED, Json(secret)).into_response()
}

#[derive(serde::Deserialize)]
struct ListQuery {
    #[serde(rename = "labelSelector")]
    label_selector: Option<String>,
}

/// `key` or `key=value`, comma-separated: the subset of selectors the node
/// uses.
fn selects(selector: &str, secret: &Secret) -> bool {
    let labels = secret.metadata.labels.clone().unwrap_or_default();
    selector
        .split(',')
        .filter(|term| !term.is_empty())
        .all(|term| match term.split_once('=') {
            Some((key, value)) => labels.get(key).is_some_and(|v| v == value),
            None => labels.contains_key(term),
        })
}

/// Metadata only, as a `PartialObjectMetadataList`: what the node asks for,
/// and all a list of Secrets needs to say to it.
async fn list_secrets(State(api): State<KubeApi>, Query(query): Query<ListQuery>) -> Response {
    let inner = api.inner.lock().unwrap();
    if inner.secrets_forbidden {
        return secrets_forbidden("");
    }
    let selector = query.label_selector.unwrap_or_default();
    let items: Vec<Value> = inner
        .secrets
        .values()
        .filter(|secret| selects(&selector, secret))
        .map(|secret| {
            json!({
                "apiVersion": "meta.k8s.io/v1",
                "kind": "PartialObjectMetadata",
                "metadata": secret.metadata,
            })
        })
        .collect();
    Json(json!({
        "kind": "PartialObjectMetadataList",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": {},
        "items": items,
    }))
    .into_response()
}

async fn read_secret(
    State(api): State<KubeApi>,
    Path((_ns, name)): Path<(String, String)>,
) -> Response {
    let inner = api.inner.lock().unwrap();
    if inner.secrets_forbidden {
        return secrets_forbidden(&name);
    }
    match inner.secrets.get(&name) {
        Some(secret) => Json(secret.clone()).into_response(),
        None => secret_not_found(&name),
    }
}

async fn delete_secret(
    State(api): State<KubeApi>,
    Path((_ns, name)): Path<(String, String)>,
) -> Response {
    let mut inner = api.inner.lock().unwrap();
    if inner.secrets_forbidden {
        return secrets_forbidden(&name);
    }
    match inner.secrets.remove(&name) {
        Some(secret) => {
            inner.secrets_deleted.push(name);
            Json(secret).into_response()
        }
        None => secret_not_found(&name),
    }
}

/// A JSON merge patch of `metadata`. `data` of an immutable Secret is
/// refused, as the API refuses it.
async fn patch_secret(
    State(api): State<KubeApi>,
    Path((_ns, name)): Path<(String, String)>,
    body: String,
) -> Response {
    let Ok(patch) = serde_json::from_str::<Value>(&body) else {
        return status_error(StatusCode::BAD_REQUEST, "BadRequest", "not JSON".into());
    };
    let mut inner = api.inner.lock().unwrap();
    if inner.secrets_forbidden {
        return secrets_forbidden(&name);
    }
    let Some(secret) = inner.secrets.get(&name).cloned() else {
        return secret_not_found(&name);
    };
    if secret.immutable == Some(true)
        && (patch.get("data").is_some() || patch.get("stringData").is_some())
    {
        return status_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid",
            format!("Secret \"{name}\" is invalid: data: Forbidden: field is immutable when `immutable` is set"),
        );
    }
    let mut merged = serde_json::to_value(&secret).unwrap();
    if let (Some(into), Some(from)) = (
        merged.get_mut("metadata").and_then(Value::as_object_mut),
        patch.get("metadata").and_then(Value::as_object),
    ) {
        for (key, value) in from {
            into.insert(key.clone(), value.clone());
        }
    }
    let merged: Secret = serde_json::from_value(merged).unwrap();
    inner.secret_patches.push((name.clone(), patch));
    inner.secrets.insert(name, merged.clone());
    Json(merged).into_response()
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
