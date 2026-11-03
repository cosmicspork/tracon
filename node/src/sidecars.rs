//! Services beside a session: a browser, a database, a mail catcher, started
//! by name from the operator's `[[service]]` catalogue.
//!
//! A session never names an image or a command. It asks for `browser`, and
//! the policy bundle decides whether it may (`service_start` with `name`), so
//! one service can run unattended while another is asked. The service joins
//! the session container's network namespace: it is reached on the session's
//! loopback, and it reaches exactly what the session's network does — the
//! gateway and nothing of its own. It lives as long as the session's
//! container: every path that removes that container removes its services,
//! found from the session's own `service` events, so none is left running
//! after the session it served.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::{Config, Service};
use crate::runner::{sidecar_name, InSandbox, Runner, RunnerError, Sidecar};
use crate::store::{now_ms, SessionRow, Store};

pub const STARTED: &str = "started";
pub const READY: &str = "ready";
pub const FAILED: &str = "failed";
pub const STARTING: &str = "starting";
pub const NOT_STARTED: &str = "not_started";

const PROBE_INTERVAL: Duration = Duration::from_millis(500);

/// A service's latest recorded state on one session.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Recorded {
    pub name: String,
    /// Its own container; empty for one run inside the session's sandbox.
    pub container: String,
    /// The session's sandbox it runs in, from its bundle.
    pub sandbox: Option<String>,
    pub state: String,
    pub detail: Option<String>,
    /// When it was started: the clock its readiness timeout runs on.
    pub started_ms: i64,
}

/// Each service's latest state on a session, from its events.
pub fn recorded(store: &Store, session_id: &str) -> BTreeMap<String, Recorded> {
    let mut out: BTreeMap<String, Recorded> = BTreeMap::new();
    for (payload, at_ms) in store.session_service_events(session_id).unwrap_or_default() {
        let (Some(name), Some(state)) = (
            payload.get("name").and_then(Value::as_str),
            payload.get("state").and_then(Value::as_str),
        ) else {
            continue;
        };
        let started_ms = match (state, out.get(name)) {
            (STARTED, _) | (_, None) => at_ms,
            (_, Some(previous)) => previous.started_ms,
        };
        out.insert(
            name.to_string(),
            Recorded {
                name: name.to_string(),
                container: payload
                    .get("container")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                sandbox: payload
                    .get("sandbox")
                    .and_then(Value::as_str)
                    .filter(|_| payload.get("in_sandbox") == Some(&Value::Bool(true)))
                    .map(str::to_string),
                state: state.to_string(),
                detail: payload
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                started_ms,
            },
        );
    }
    out
}

/// The containers of every service a session started: what removing the
/// session's container has to remove too.
pub fn containers(store: &Store, session_id: &str) -> Vec<String> {
    recorded(store, session_id)
        .into_values()
        .map(|service| service.container)
        .filter(|container| !container.is_empty())
        .collect()
}

/// Remove a session's services: their containers, and those in its sandbox
/// by the name the runner stops them by. Errors are logged, not returned:
/// this runs on the way out of a session, where a service that is already
/// gone is fine.
pub async fn stop_all(runner: &dyn Runner, store: &Store, session_id: &str) {
    let services = recorded(store, session_id).into_values();
    let names = services.filter_map(|service| match service.sandbox {
        Some(sandbox) => Some(sidecar_name(&sandbox, &service.name)),
        None => Some(service.container).filter(|container| !container.is_empty()),
    });
    for name in names {
        if let Err(error) = runner.kill(&name).await {
            tracing::warn!(service = %name, %error, "could not remove a session's service");
        }
    }
}

fn catalogue_names(cfg: &Config) -> String {
    let names: Vec<&str> = cfg.service.iter().map(|s| s.name.as_str()).collect();
    if names.is_empty() {
        "the node's catalogue is empty".into()
    } else {
        format!("the catalogue offers {}", names.join(", "))
    }
}

pub fn lookup<'a>(cfg: &'a Config, name: &str) -> Result<&'a Service, String> {
    cfg.service
        .iter()
        .find(|service| service.name == name)
        .ok_or_else(|| format!("no service named {name:?}; {}", catalogue_names(cfg)))
}

fn sidecar(service: &Service, session_container: &str) -> Sidecar {
    Sidecar {
        name: sidecar_name(session_container, &service.name),
        session_container: session_container.to_string(),
        image: service.image.clone(),
        command: service.command.clone(),
        port: service.port,
        ready: service.ready.clone(),
        in_sandbox: service.bundle.as_ref().map(|_| InSandbox {
            service: service.name.clone(),
            argv: service.sandbox_argv(),
            env: service.env.clone(),
            restart: service.restart,
            ready_within: Duration::from_secs(service.timeout_secs()),
        }),
    }
}

/// A `service` event, saying where it runs: the session's sandbox with its
/// bundle, or a container of its own (from its image, said when it starts).
fn event(cx: &Context<'_>, service: &Service, sidecar: &Sidecar, state: &str) -> Value {
    let mut out = json!({ "name": service.name, "state": state });
    if cx.runner.sidecar_in_sandbox(sidecar) {
        out["in_sandbox"] = json!(true);
        out["sandbox"] = json!(sidecar.session_container);
        out["bundle"] = json!(service.bundle);
        out["mounted_at"] = json!(service.bundle_target());
        return out;
    }
    out["container"] = json!(sidecar.name);
    if state == STARTED {
        out["image"] = json!(service.image);
        if let Some(bundle) = &service.bundle {
            out["bundle"] = json!(bundle);
        }
    }
    out
}

fn failed(cx: &Context<'_>, service: &Service, sidecar: &Sidecar, detail: &str) -> Value {
    let mut out = event(cx, service, sidecar, FAILED);
    out["detail"] = json!(detail);
    out
}

fn view(service: &Service, state: &str, detail: Option<&str>) -> Value {
    let address = format!("127.0.0.1:{}", service.port);
    let mut out = json!({
        "name": service.name,
        "state": state,
        "address": address,
    });
    if let Some(ready) = &service.ready {
        out["url"] = json!(format!("http://{address}{ready}"));
    }
    if let Some(detail) = detail {
        out["detail"] = json!(detail);
    }
    if state == STARTING {
        out["next"] = json!("not ready yet; call service_status to keep waiting");
    }
    out
}

/// Everything a start or a status call needs, gathered by the caller.
pub struct Context<'a> {
    pub cfg: &'a Config,
    pub store: &'a Store,
    pub runner: &'a dyn Runner,
    pub session: &'a SessionRow,
    /// Records one `service` event on the session.
    pub record: &'a (dyn Fn(Value) + Send + Sync),
}

fn container(session: &SessionRow) -> Result<&str, String> {
    session
        .container_name
        .as_deref()
        .ok_or_else(|| "this session has no container for a service to join".to_string())
}

/// Start `name` beside the session unless it is already running, then wait up
/// to `wait` for it to answer.
pub async fn start(cx: &Context<'_>, name: &str, wait: Duration) -> Result<Value, String> {
    let service = lookup(cx.cfg, name)?;
    let session_container = container(cx.session)?;
    let sidecar = sidecar(service, session_container);
    let current = recorded(cx.store, &cx.session.id).remove(name);
    let started_ms = match &current {
        Some(running) if running.state == STARTED || running.state == READY => running.started_ms,
        _ => {
            if let Err(error) = cx.runner.start_sidecar(&sidecar).await {
                let detail = error.to_string();
                (cx.record)(failed(cx, service, &sidecar, &detail));
                return Ok(view(service, FAILED, Some(&detail)));
            }
            (cx.record)(event(cx, service, &sidecar, STARTED));
            now_ms()
        }
    };
    await_ready(cx, service, &sidecar, started_ms, current.as_ref(), wait).await
}

/// Where `name` stands, waiting up to `wait` for a starting service.
pub async fn status(cx: &Context<'_>, name: &str, wait: Duration) -> Result<Value, String> {
    let service = lookup(cx.cfg, name)?;
    let session_container = container(cx.session)?;
    let Some(current) = recorded(cx.store, &cx.session.id).remove(name) else {
        return Ok(view(service, NOT_STARTED, None));
    };
    if current.state == FAILED || current.state == "stopped" {
        return Ok(view(service, &current.state, current.detail.as_deref()));
    }
    let sidecar = sidecar(service, session_container);
    await_ready(
        cx,
        service,
        &sidecar,
        current.started_ms,
        Some(&current),
        wait,
    )
    .await
}

async fn await_ready(
    cx: &Context<'_>,
    service: &Service,
    sidecar: &Sidecar,
    started_ms: i64,
    current: Option<&Recorded>,
    wait: Duration,
) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + wait;
    let timeout_ms = (service.timeout_secs() * 1000) as i64;
    loop {
        let ready = match cx.runner.probe_sidecar(sidecar).await {
            Ok(ready) => ready,
            // Given up on where it runs: failed now, not at the timeout.
            Err(RunnerError::ServiceFailed(detail)) => {
                let _ = cx.runner.kill(&sidecar.name).await;
                (cx.record)(failed(cx, service, sidecar, &detail));
                return Ok(view(service, FAILED, Some(&detail)));
            }
            Err(error) => return Err(error.to_string()),
        };
        if ready {
            if current.is_none_or(|c| c.state != READY) {
                (cx.record)(event(cx, service, sidecar, READY));
            }
            return Ok(view(service, READY, None));
        }
        // A supervised service is timed by its supervisor, each start, and
        // restarted by its policy; it fails when the supervisor gives up.
        if !cx.runner.supervises_sidecar(sidecar) && now_ms() - started_ms >= timeout_ms {
            let detail = format!(
                "did not answer on {} within {}s",
                service.port,
                service.timeout_secs()
            );
            // The container is removed rather than left half-started; asking
            // again starts it afresh.
            let _ = cx.runner.kill(&sidecar.name).await;
            (cx.record)(failed(cx, service, sidecar, &detail));
            return Ok(view(service, FAILED, Some(&detail)));
        }
        if tokio::time::Instant::now() + PROBE_INTERVAL > deadline {
            return Ok(view(service, STARTING, None));
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// The catalogue as a session is told it: names, ports and what each answers.
pub fn describe(cfg: &Config) -> String {
    if cfg.service.is_empty() {
        return "No services are configured on this node.".into();
    }
    cfg.service
        .iter()
        .map(|s| {
            format!(
                "`{}` on 127.0.0.1:{}{}",
                s.name,
                s.port,
                s.ready
                    .as_deref()
                    .map(|path| format!(" (ready at {path})"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{RunnerCommand, RunnerError, Spawned};
    use crate::session::state::event_kind as ek;
    use crate::store::NewEvent;
    use async_trait::async_trait;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    const PINNED: &str =
        "img@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[derive(Default)]
    struct FakeRunner {
        started: Mutex<Vec<Sidecar>>,
        killed: Mutex<Vec<String>>,
        /// Probes before the service answers; `usize::MAX` never.
        ready_after: usize,
        probes: AtomicUsize,
        /// Runs services under a supervisor of its own.
        supervises: bool,
        /// Probes before the supervisor gives up on it.
        fails_after: Option<usize>,
    }

    #[async_trait]
    impl Runner for FakeRunner {
        async fn spawn(&self, _: RunnerCommand) -> Result<Spawned, RunnerError> {
            unreachable!()
        }
        async fn run_capture(&self, _: RunnerCommand) -> Result<std::process::Output, RunnerError> {
            unreachable!()
        }
        async fn kill(&self, name: &str) -> Result<(), RunnerError> {
            self.killed.lock().unwrap().push(name.into());
            Ok(())
        }
        async fn start_sidecar(&self, sidecar: &Sidecar) -> Result<(), RunnerError> {
            self.started.lock().unwrap().push(sidecar.clone());
            Ok(())
        }
        async fn probe_sidecar(&self, _: &Sidecar) -> Result<bool, RunnerError> {
            let probes = self.probes.fetch_add(1, Ordering::SeqCst);
            if self.fails_after.is_some_and(|n| probes >= n) {
                return Err(RunnerError::ServiceFailed("exited with 1; boom".into()));
            }
            Ok(probes >= self.ready_after)
        }
        fn supervises_sidecar(&self, _: &Sidecar) -> bool {
            self.supervises
        }
        fn sidecar_in_sandbox(&self, _: &Sidecar) -> bool {
            self.supervises
        }
    }

    fn service_events(store: &Store) -> Vec<Value> {
        store
            .session_service_events("s1")
            .unwrap()
            .into_iter()
            .map(|(payload, _)| payload)
            .collect()
    }

    fn cfg(timeout_secs: u64) -> Config {
        Config {
            service: vec![Service {
                name: "browser".into(),
                image: PINNED.into(),
                command: vec!["--remote-debugging-port=9222".into()],
                port: 9222,
                ready: Some("/json/version".into()),
                timeout_secs,
                ..Service::default()
            }],
            ..Config::default()
        }
    }

    fn session(store: &Store) -> SessionRow {
        store.ensure_peer_node("n1").unwrap();
        let row: SessionRow = serde_json::from_value(json!({
            "id": "s1", "node_id": "n1", "channel": "personal", "repo_path": "/r",
            "branch": "b", "harness_id": "fake", "harness_version": "1", "model": "m",
            "phase": "execute", "budget_tokens": 1000, "tokens_used": 0,
            "state": "running", "turn_active": 0, "created_ms": 0, "updated_ms": 0,
            "container_name": "tracon-h-s1",
        }))
        .unwrap();
        store.insert_session(&row).unwrap();
        row
    }

    fn recorder(store: Arc<Store>) -> impl Fn(Value) + Send + Sync {
        move |payload| {
            store
                .append_event(&NewEvent {
                    session_id: "s1".into(),
                    work_item_id: None,
                    kind: ek::SERVICE.into(),
                    ref_id: None,
                    payload,
                    at_ms: now_ms(),
                    mono_ms: 0,
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_service_joins_the_session_and_is_removed_with_it() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let row = session(&store);
        let cfg = cfg(30);
        let runner = FakeRunner {
            ready_after: 1,
            ..Default::default()
        };
        let record = recorder(store.clone());
        let cx = Context {
            cfg: &cfg,
            store: &store,
            runner: &runner,
            session: &row,
            record: &record,
        };
        let v = start(&cx, "browser", Duration::from_secs(5)).await.unwrap();
        assert_eq!(v["state"], READY, "{v}");
        assert_eq!(v["url"], "http://127.0.0.1:9222/json/version");
        let started = runner.started.lock().unwrap().clone();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].session_container, "tracon-h-s1");
        assert_eq!(started[0].name, "tracon-h-s1-svc-browser");
        // Its events name the container it runs in and the image it runs.
        let events = service_events(&store);
        assert_eq!(
            events[0],
            json!({
                "name": "browser", "state": STARTED,
                "container": "tracon-h-s1-svc-browser", "image": PINNED,
            })
        );
        assert_eq!(
            events[1],
            json!({ "name": "browser", "state": READY, "container": "tracon-h-s1-svc-browser" })
        );

        // Asked again: still the one container.
        let v = start(&cx, "browser", Duration::from_secs(1)).await.unwrap();
        assert_eq!(v["state"], READY);
        assert_eq!(runner.started.lock().unwrap().len(), 1);

        let err = start(&cx, "postgres", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.contains("catalogue offers browser"), "{err}");

        stop_all(&runner, &store, "s1").await;
        assert_eq!(
            runner.killed.lock().unwrap().clone(),
            vec!["tracon-h-s1-svc-browser"]
        );
    }

    #[tokio::test]
    async fn a_service_that_never_answers_fails_after_its_timeout() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let row = session(&store);
        let cfg = cfg(1);
        let runner = FakeRunner {
            ready_after: usize::MAX,
            ..Default::default()
        };
        let record = recorder(store.clone());
        let cx = Context {
            cfg: &cfg,
            store: &store,
            runner: &runner,
            session: &row,
            record: &record,
        };
        let v = status(&cx, "browser", Duration::ZERO).await.unwrap();
        assert_eq!(v["state"], NOT_STARTED);
        let v = start(&cx, "browser", Duration::ZERO).await.unwrap();
        assert_eq!(v["state"], STARTING, "{v}");
        let v = status(&cx, "browser", Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(v["state"], FAILED, "{v}");
        assert!(v["detail"].as_str().unwrap().contains("within 1s"));
        assert_eq!(
            runner.killed.lock().unwrap().clone(),
            vec!["tracon-h-s1-svc-browser"]
        );
        // Asking again starts it afresh.
        start(&cx, "browser", Duration::ZERO).await.unwrap();
        assert_eq!(runner.started.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_bundled_service_is_described_for_the_sandbox_and_timed_by_its_supervisor() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let row = session(&store);
        let mut cfg = cfg(1);
        cfg.service[0].image = String::new();
        cfg.service[0].bundle = Some("/srv/browser/amd64".into());
        cfg.service[0].restart = crate::config::ServiceRestart::Always;
        let runner = FakeRunner {
            ready_after: usize::MAX,
            supervises: true,
            ..Default::default()
        };
        let record = recorder(store.clone());
        let cx = Context {
            cfg: &cfg,
            store: &store,
            runner: &runner,
            session: &row,
            record: &record,
        };
        let v = start(&cx, "browser", Duration::ZERO).await.unwrap();
        assert_eq!(v["state"], STARTING, "{v}");
        let started = runner.started.lock().unwrap()[0].clone();
        let in_sandbox = started.in_sandbox.unwrap();
        assert_eq!(in_sandbox.service, "browser");
        assert_eq!(
            in_sandbox.argv,
            [
                "/opt/tracon-services/browser/run",
                "--remote-debugging-port=9222"
            ]
        );
        assert_eq!(in_sandbox.ready_within, Duration::from_secs(1));
        assert_eq!(in_sandbox.restart, crate::config::ServiceRestart::Always);
        assert_eq!(
            recorded(&store, "s1")["browser"].state,
            STARTED,
            "recorded as started"
        );
        // Recorded as what runs: the session's sandbox and the bundle, with
        // no container or image of its own.
        assert_eq!(
            service_events(&store)[0],
            json!({
                "name": "browser", "state": STARTED, "in_sandbox": true,
                "sandbox": "tracon-h-s1", "bundle": "/srv/browser/amd64",
                "mounted_at": "/opt/tracon-services/browser",
            })
        );
        assert!(
            containers(&store, "s1").is_empty(),
            "no container to reconcile"
        );

        // Past the catalogue's timeout, still restarting: not failed, since
        // its supervisor restarts it by its policy.
        let v = status(&cx, "browser", Duration::from_millis(1600))
            .await
            .unwrap();
        assert_eq!(v["state"], STARTING, "{v}");
        assert!(runner.killed.lock().unwrap().is_empty());

        // Leaving the session still stops it in its sandbox, by the name the
        // runner knows it by.
        stop_all(&runner, &store, "s1").await;
        assert_eq!(
            runner.killed.lock().unwrap().clone(),
            vec!["tracon-h-s1-svc-browser"]
        );
    }

    #[tokio::test]
    async fn a_service_its_supervisor_gave_up_on_fails_at_once_with_why() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let row = session(&store);
        let cfg = cfg(30);
        let runner = FakeRunner {
            ready_after: usize::MAX,
            supervises: true,
            fails_after: Some(1),
            ..Default::default()
        };
        let record = recorder(store.clone());
        let cx = Context {
            cfg: &cfg,
            store: &store,
            runner: &runner,
            session: &row,
            record: &record,
        };
        let v = start(&cx, "browser", Duration::from_secs(5)).await.unwrap();
        assert_eq!(v["state"], FAILED, "{v}");
        assert!(v["detail"].as_str().unwrap().contains("boom"), "{v}");
        assert_eq!(recorded(&store, "s1")["browser"].state, FAILED);
        assert_eq!(
            runner.killed.lock().unwrap().clone(),
            vec!["tracon-h-s1-svc-browser"]
        );
    }
}
