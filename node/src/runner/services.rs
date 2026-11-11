//! Catalogue services inside a session's sandbox, on Podman and Apple: the
//! sandbox's own supervisor starts
//! the service from its mounted bundle, watches its readiness, restarts it by
//! its policy and stops it with the sandbox. Nothing is started beside the
//! session, so the service has exactly the sandbox's network: reached on its
//! loopback, reaching what it reaches.
//!
//! The node keeps each live sandbox's connection here by the sandbox's name,
//! which is the session's container name a [`Sidecar`] carries, so a
//! `service_start` made through a fresh runner finds the session it is for.
//! The services' output and state changes are logged by the node, and the
//! last lines of each are kept to say why one failed.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::http::StatusCode;
use tracon_sandbox::node::{SandboxConn, SandboxError};
use tracon_sandbox::protocol::{Event, Probe, Readiness, RestartPolicy, ServiceSpec, ServiceState};

use super::{sidecar_name, InSandbox, RunnerError, Sidecar};
use crate::config::ServiceRestart;

/// Lines of each service's output kept for a failure's explanation.
const TAIL_LINES: usize = 10;
/// How often the supervisor asks whether a starting service answers.
const PROBE_INTERVAL: Duration = Duration::from_millis(250);

struct Live {
    /// The run this connection was made for: a later sandbox of the same
    /// name must not be unregistered by an earlier one's teardown.
    run: String,
    conn: SandboxConn,
    /// The catalogue names started in it.
    started: Mutex<BTreeSet<String>>,
    tails: Arc<Mutex<HashMap<String, VecDeque<String>>>>,
    watching: tokio::sync::OnceCell<()>,
}

fn live() -> &'static Mutex<HashMap<String, Arc<Live>>> {
    static LIVE: OnceLock<Mutex<HashMap<String, Arc<Live>>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A sandbox `name` is connected for `run`.
pub(super) fn register(name: &str, run: &str, conn: SandboxConn) {
    live().lock().unwrap().insert(
        name.to_string(),
        Arc::new(Live {
            run: run.to_string(),
            conn,
            started: Mutex::new(BTreeSet::new()),
            tails: Arc::new(Mutex::new(HashMap::new())),
            watching: tokio::sync::OnceCell::new(),
        }),
    );
}

/// The sandbox `name` made for `run` is going.
pub(super) fn unregister(name: &str, run: &str) {
    let mut map = live().lock().unwrap();
    if map.get(name).is_some_and(|live| live.run == run) {
        map.remove(name);
    }
}

fn find(sandbox: &str) -> Option<Arc<Live>> {
    live().lock().unwrap().get(sandbox).cloned()
}

/// The sandbox `name` was taken back from an earlier node process: the
/// services still running in it are this process's to watch and stop.
pub(super) async fn adopt(sandbox: &str) {
    let Some(live) = find(sandbox) else {
        return;
    };
    let running = match live.conn.services().status().await {
        Ok(status) => status,
        Err(error) => {
            tracing::warn!(%sandbox, %error, "could not ask a sandbox taken back about its services");
            return;
        }
    };
    let names: Vec<String> = running
        .into_iter()
        .filter(|s| !matches!(s.state, ServiceState::Stopped | ServiceState::Failed { .. }))
        .map(|s| s.name)
        .collect();
    if names.is_empty() {
        return;
    }
    let _ = live.watch(sandbox).await;
    live.started.lock().unwrap().extend(names);
}

/// The sandbox running the service a sidecar name stands for, if one does.
pub(super) fn sandbox_of(name: &str) -> Option<String> {
    let map = live().lock().unwrap();
    map.iter().find_map(|(sandbox, live)| {
        live.started
            .lock()
            .unwrap()
            .iter()
            .any(|service| sidecar_name(sandbox, service) == name)
            .then(|| sandbox.clone())
    })
}

/// What the supervisor is asked to run for `sidecar`.
pub fn service_spec(sidecar: &Sidecar, spec: &InSandbox) -> ServiceSpec {
    let addr = format!("127.0.0.1:{}", sidecar.port);
    let probe = match &sidecar.ready {
        Some(path) => Probe::Http {
            addr,
            path: path.clone(),
        },
        None => Probe::Tcp(addr),
    };
    let mut service = ServiceSpec::new(spec.service.clone(), spec.argv.clone());
    service.env = spec.env.clone();
    service.readiness = Some(Readiness {
        probe,
        interval_ms: PROBE_INTERVAL.as_millis() as u64,
        timeout_ms: spec.ready_within.as_millis() as u64,
    });
    service.restart = match spec.restart {
        ServiceRestart::Never => RestartPolicy::Never,
        ServiceRestart::OnFailure => RestartPolicy::OnFailure,
        ServiceRestart::Always => RestartPolicy::Always,
    };
    service
}

fn not_connected(sidecar: &Sidecar) -> String {
    format!(
        "the session's sandbox {} is not connected to this node process, so service {} \
         cannot run in it",
        sidecar.session_container, sidecar.name
    )
}

/// Start the service in the session's sandbox. Already running is what was
/// wanted.
pub(super) async fn start(sidecar: &Sidecar, spec: &InSandbox) -> Result<(), RunnerError> {
    let live = find(&sidecar.session_container)
        .ok_or_else(|| RunnerError::Other(not_connected(sidecar)))?;
    // Watching before starting, so its first lines are not missed.
    live.watch(&sidecar.session_container).await?;
    live.tails.lock().unwrap().remove(&spec.service);
    match live
        .conn
        .services()
        .start(vec![service_spec(sidecar, spec)])
        .await
    {
        Ok(())
        | Err(SandboxError::Remote {
            status: StatusCode::CONFLICT,
            ..
        }) => {
            live.started.lock().unwrap().insert(spec.service.clone());
            Ok(())
        }
        Err(error) => Err(RunnerError::Other(format!(
            "start service {} in sandbox {}: {error}",
            spec.service, sidecar.session_container
        ))),
    }
}

/// Whether the supervisor has seen it answer. A service it gave up on is
/// [`RunnerError::ServiceFailed`], with its last words.
pub(super) async fn probe(sidecar: &Sidecar, spec: &InSandbox) -> Result<bool, RunnerError> {
    let live = find(&sidecar.session_container)
        .ok_or_else(|| RunnerError::ServiceFailed(not_connected(sidecar)))?;
    let status = live
        .conn
        .services()
        .status()
        .await
        .map_err(|e| RunnerError::Other(format!("ask sandbox about {}: {e}", spec.service)))?;
    let state = status
        .into_iter()
        .find(|s| s.name == spec.service)
        .map(|s| s.state);
    let failed = |why: String| Err(RunnerError::ServiceFailed(live.explain(&spec.service, why)));
    match state {
        Some(ServiceState::Ready) => Ok(true),
        Some(ServiceState::Failed { reason }) => failed(reason),
        Some(ServiceState::Stopped) => failed("stopped".into()),
        None => failed("not running in the session's sandbox".into()),
        Some(_) => Ok(false),
    }
}

/// Stop the service a sidecar name stands for, when one of the live
/// sandboxes runs it; `None` when none does.
pub(super) async fn stop(name: &str) -> Option<Result<(), RunnerError>> {
    let (live, service) = {
        let map = live().lock().unwrap();
        map.iter().find_map(|(sandbox, live)| {
            let started = live.started.lock().unwrap();
            started
                .iter()
                .find(|service| sidecar_name(sandbox, service) == name)
                .map(|service| (live.clone(), service.clone()))
        })?
    };
    let stopped = live.conn.services().stop(vec![service.clone()]).await;
    live.started.lock().unwrap().remove(&service);
    Some(match stopped {
        Ok(())
        | Err(SandboxError::Remote {
            status: StatusCode::NOT_FOUND,
            ..
        }) => Ok(()),
        Err(error) => Err(RunnerError::Other(format!(
            "stop service {service}: {error}"
        ))),
    })
}

impl Live {
    /// Read the sandbox's events for as long as it is connected: each
    /// service's state changes and output go to the node's log, and the last
    /// lines of each are kept.
    async fn watch(&self, sandbox: &str) -> Result<(), RunnerError> {
        self.watching
            .get_or_try_init(|| async {
                let mut events = self.conn.events().await.map_err(|e| {
                    RunnerError::Other(format!("watch sandbox {sandbox}'s services: {e}"))
                })?;
                let tails = self.tails.clone();
                let sandbox = sandbox.to_string();
                tokio::spawn(async move {
                    while let Ok(Some(event)) = events.next().await {
                        record(&sandbox, &tails, event);
                    }
                });
                Ok(())
            })
            .await
            .map(|_| ())
    }

    fn explain(&self, service: &str, why: String) -> String {
        let tails = self.tails.lock().unwrap();
        match tails.get(service).filter(|tail| !tail.is_empty()) {
            None => why,
            Some(tail) => format!(
                "{why}; its last output:\n{}",
                tail.iter().cloned().collect::<Vec<_>>().join("\n")
            ),
        }
    }
}

fn record(sandbox: &str, tails: &Mutex<HashMap<String, VecDeque<String>>>, event: Event) {
    match event {
        Event::Log {
            service,
            stream,
            line,
        } => {
            tracing::info!(target: "tracon::service", %sandbox, %service, ?stream, "{line}");
            let mut tails = tails.lock().unwrap();
            let tail = tails.entry(service).or_default();
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        Event::Service { name, state } => {
            tracing::info!(target: "tracon::service", %sandbox, service = %name, ?state, "service state");
        }
        Event::Lagged { missed } => {
            tracing::warn!(target: "tracon::service", %sandbox, missed, "missed service events");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidecar(ready: Option<&str>) -> (Sidecar, InSandbox) {
        let spec = InSandbox {
            service: "browser".into(),
            argv: vec!["/opt/tracon-services/browser/run".into()],
            env: [("A".to_string(), "b".to_string())].into(),
            restart: ServiceRestart::Always,
            ready_within: Duration::from_secs(60),
        };
        let sidecar = Sidecar {
            name: sidecar_name("tracon-h-s1", "browser"),
            session_container: "tracon-h-s1".into(),
            image: String::new(),
            command: Vec::new(),
            port: 9222,
            ready: ready.map(str::to_string),
            in_sandbox: Some(spec.clone()),
        };
        (sidecar, spec)
    }

    #[test]
    fn the_supervisor_probes_the_catalogue_port_on_loopback() {
        let (with_path, spec) = sidecar(Some("/json/version"));
        let service = service_spec(&with_path, &spec);
        assert_eq!(service.name, "browser");
        assert_eq!(service.argv, ["/opt/tracon-services/browser/run"]);
        assert_eq!(service.env["A"], "b");
        assert_eq!(service.restart, RestartPolicy::Always);
        let readiness = service.readiness.unwrap();
        assert_eq!(readiness.timeout_ms, 60_000);
        assert!(matches!(
            readiness.probe,
            Probe::Http { ref addr, ref path } if addr == "127.0.0.1:9222" && path == "/json/version"
        ));
        let (tcp, spec) = sidecar(None);
        assert!(matches!(
            service_spec(&tcp, &spec).readiness.unwrap().probe,
            Probe::Tcp(ref addr) if addr == "127.0.0.1:9222"
        ));
    }

    #[tokio::test]
    async fn a_session_with_no_live_sandbox_says_so() {
        let (sidecar, spec) = sidecar(None);
        let error = start(&sidecar, &spec).await.unwrap_err();
        assert!(error.to_string().contains("not connected"), "{error}");
        assert!(matches!(
            probe(&sidecar, &spec).await,
            Err(RunnerError::ServiceFailed(_))
        ));
        assert!(stop(&sidecar.name).await.is_none());
    }
}
