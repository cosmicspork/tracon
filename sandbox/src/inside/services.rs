//! The session's long-running services: started on the node's request, kept
//! up by their restart policy, watched for readiness, their output sent to
//! the node line by line, and stopped in reverse order at teardown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, BufReader};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::spawner::Spawner;
use crate::protocol::{
    Event, ExitStatus, LogStream, Probe, RestartPolicy, ServiceSpec, ServiceState, ServiceStatus,
    SpawnSpec, MAX_LOG_LINE,
};

/// Events not yet read by the node; beyond this the oldest are dropped and
/// the node is told how many.
const EVENT_BUFFER: usize = 4096;

struct Service {
    status: Mutex<ServiceStatus>,
    stop: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
    /// The current run's process, while there is one.
    pid: Mutex<Option<i32>>,
}

pub struct Supervisor {
    spawner: Spawner,
    events: broadcast::Sender<Event>,
    services: Mutex<HashMap<String, Arc<Service>>>,
    /// Start order, for stopping in reverse.
    order: Mutex<Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("service {0} is already running")]
    Exists(String),
    #[error("no service {0}")]
    NotFound(String),
    #[error("service {0}: {1}")]
    Invalid(String, &'static str),
}

impl Supervisor {
    pub fn new(spawner: Spawner) -> Self {
        Supervisor {
            spawner,
            events: broadcast::channel(EVENT_BUFFER).0,
            services: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn start(&self, specs: Vec<ServiceSpec>) -> Result<(), ServiceError> {
        let mut services = self.services.lock().unwrap();
        for spec in &specs {
            if spec.name.is_empty() {
                return Err(ServiceError::Invalid(spec.name.clone(), "empty name"));
            }
            if spec.argv.is_empty() {
                return Err(ServiceError::Invalid(spec.name.clone(), "empty argv"));
            }
            if let Some(existing) = services.get(&spec.name) {
                let finished = existing
                    .task
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_none_or(|t| t.is_finished());
                if !finished {
                    return Err(ServiceError::Exists(spec.name.clone()));
                }
            }
        }
        for spec in specs {
            let service = Arc::new(Service {
                status: Mutex::new(ServiceStatus {
                    name: spec.name.clone(),
                    state: ServiceState::Starting { attempt: 0 },
                    restarts: 0,
                }),
                stop: CancellationToken::new(),
                task: Mutex::new(None),
                pid: Mutex::new(None),
            });
            let run = Run {
                spec: spec.clone(),
                spawner: self.spawner.clone(),
                events: self.events.clone(),
                service: service.clone(),
            };
            *service.task.lock().unwrap() = Some(tokio::spawn(run.run()));
            services.insert(spec.name.clone(), service);
            let mut order = self.order.lock().unwrap();
            order.retain(|n| n != &spec.name);
            order.push(spec.name);
        }
        Ok(())
    }

    pub fn status(&self) -> Vec<ServiceStatus> {
        let services = self.services.lock().unwrap();
        self.order
            .lock()
            .unwrap()
            .iter()
            .filter_map(|name| services.get(name))
            .map(|s| s.status.lock().unwrap().clone())
            .collect()
    }

    /// Stop the named services (every one when `names` is empty), in reverse
    /// start order, each waited for before the next.
    pub async fn stop(&self, names: &[String]) -> Result<(), ServiceError> {
        let targets: Vec<Arc<Service>> = {
            let services = self.services.lock().unwrap();
            for name in names {
                if !services.contains_key(name) {
                    return Err(ServiceError::NotFound(name.clone()));
                }
            }
            self.order
                .lock()
                .unwrap()
                .iter()
                .rev()
                .filter(|name| names.is_empty() || names.contains(name))
                .filter_map(|name| services.get(name).cloned())
                .collect()
        };
        for service in targets {
            service.stop.cancel();
            let task = service.task.lock().unwrap().take();
            if let Some(task) = task {
                let _ = task.await;
            }
        }
        Ok(())
    }

    /// Forward a signal to every running service's process group.
    pub fn forward(&self, signal: i32) {
        let services = self.services.lock().unwrap();
        for service in services.values() {
            if let Some(pid) = *service.pid.lock().unwrap() {
                let _ = self.spawner.signal(pid, signal, true);
            }
        }
    }
}

struct Run {
    spec: ServiceSpec,
    spawner: Spawner,
    events: broadcast::Sender<Event>,
    service: Arc<Service>,
}

enum Outcome {
    Exited(ExitStatus),
    NotReady,
    Stopped,
}

impl Run {
    fn set(&self, state: ServiceState) {
        {
            let mut status = self.service.status.lock().unwrap();
            if matches!(state, ServiceState::Restarting { .. }) {
                status.restarts += 1;
            }
            status.state = state.clone();
        }
        let _ = self.events.send(Event::Service {
            name: self.spec.name.clone(),
            state,
        });
    }

    async fn run(self) {
        let backoff = self.spec.backoff;
        let mut delay = Duration::from_millis(backoff.initial_ms);
        let mut attempt = 0u32;
        loop {
            if self.service.stop.is_cancelled() {
                self.set(ServiceState::Stopped);
                return;
            }
            self.set(ServiceState::Starting { attempt });
            let started = Instant::now();
            let outcome = match self.once().await {
                Ok(outcome) => outcome,
                Err(reason) => {
                    self.set(ServiceState::Failed { reason });
                    return;
                }
            };
            let succeeded = match outcome {
                Outcome::Stopped => {
                    self.set(ServiceState::Stopped);
                    return;
                }
                Outcome::Exited(status) => {
                    self.set(ServiceState::Exited { status });
                    status.success()
                }
                Outcome::NotReady => false,
            };
            let again = match self.spec.restart {
                RestartPolicy::Never => false,
                RestartPolicy::OnFailure => !succeeded,
                RestartPolicy::Always => true,
            };
            if !again {
                if !succeeded {
                    self.set(ServiceState::Failed {
                        reason: "exited and its restart policy does not restart it".into(),
                    });
                }
                return;
            }
            if started.elapsed() >= Duration::from_millis(backoff.reset_after_ms) {
                delay = Duration::from_millis(backoff.initial_ms);
            }
            attempt += 1;
            self.set(ServiceState::Restarting {
                attempt,
                delay_ms: delay.as_millis() as u64,
            });
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.service.stop.cancelled() => {
                    self.set(ServiceState::Stopped);
                    return;
                }
            }
            delay = (delay * 2).min(Duration::from_millis(
                backoff.max_ms.max(backoff.initial_ms),
            ));
        }
    }

    /// One run of the service, from start to exit. `Err` is a start that
    /// cannot succeed by trying again (no such program, bad directory).
    async fn once(&self) -> Result<Outcome, String> {
        let spec = SpawnSpec {
            argv: self.spec.argv.clone(),
            env: self.spec.env.clone(),
            env_clear: false,
            cwd: self.spec.cwd.clone(),
            uid: self.spec.uid,
            gid: self.spec.gid,
            tty: None,
            stdin: false,
            stdout: true,
            stderr: true,
        };
        let mut child = self
            .spawner
            .spawn(&spec)
            .map_err(|e| format!("start: {e}"))?;
        let pid = child.pid;
        *self.service.pid.lock().unwrap() = Some(pid);
        self.set(ServiceState::Running { pid: pid as u32 });
        let pumps = [
            child.stdout.take().map(|s| self.pump(s, LogStream::Stdout)),
            child.stderr.take().map(|s| self.pump(s, LogStream::Stderr)),
        ];
        let mut exit = child.exit;

        let outcome = 'run: {
            if let Some(readiness) = &self.spec.readiness {
                let deadline = Duration::from_millis(readiness.timeout_ms);
                let interval = Duration::from_millis(readiness.interval_ms.max(10));
                tokio::select! {
                    status = exited(&mut exit) => break 'run Outcome::Exited(status),
                    _ = self.service.stop.cancelled() => {
                        self.halt(pid, &mut exit).await;
                        break 'run Outcome::Stopped;
                    }
                    ready = tokio::time::timeout(deadline, self.probe(&readiness.probe, interval)) => {
                        if ready.is_err() {
                            tracing::warn!(service = %self.spec.name, "not ready in time");
                            self.halt(pid, &mut exit).await;
                            break 'run Outcome::NotReady;
                        }
                    }
                }
            }
            self.set(ServiceState::Ready);
            tokio::select! {
                status = exited(&mut exit) => Outcome::Exited(status),
                _ = self.service.stop.cancelled() => {
                    self.halt(pid, &mut exit).await;
                    Outcome::Stopped
                }
            }
        };
        *self.service.pid.lock().unwrap() = None;
        // Its last lines before its exit, unless something it started still
        // holds the pipe open.
        for pump in pumps.into_iter().flatten() {
            let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
        }
        Ok(outcome)
    }

    /// Stop signal to the group, then SIGKILL after the timeout.
    async fn halt(&self, pid: i32, exit: &mut watch::Receiver<Option<ExitStatus>>) {
        let _ = self.spawner.signal(pid, self.spec.stop_signal, true);
        let grace = Duration::from_millis(self.spec.stop_timeout_ms);
        if tokio::time::timeout(grace, exited(exit)).await.is_err() {
            let _ = self.spawner.signal(pid, libc::SIGKILL, true);
            exited(exit).await;
        } else {
            // Whatever it left behind in its group goes too.
            let _ = self.spawner.signal(pid, libc::SIGKILL, true);
        }
    }

    async fn probe(&self, probe: &Probe, interval: Duration) {
        loop {
            let ready = match probe {
                Probe::Tcp(addr) => {
                    matches!(
                        tokio::time::timeout(interval, tokio::net::TcpStream::connect(addr)).await,
                        Ok(Ok(_))
                    )
                }
                Probe::Command(argv) => {
                    let mut spec = SpawnSpec::new(argv.clone());
                    spec.stdin = false;
                    spec.stdout = false;
                    spec.stderr = false;
                    spec.cwd = self.spec.cwd.clone();
                    spec.env = self.spec.env.clone();
                    spec.uid = self.spec.uid;
                    spec.gid = self.spec.gid;
                    match self.spawner.spawn(&spec) {
                        Ok(mut child) => exited(&mut child.exit).await.success(),
                        Err(_) => false,
                    }
                }
            };
            if ready {
                return;
            }
            tokio::time::sleep(interval).await;
        }
    }

    fn pump<R: AsyncRead + Unpin + Send + 'static>(
        &self,
        stream: R,
        which: LogStream,
    ) -> JoinHandle<()> {
        let events = self.events.clone();
        let service = self.spec.name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            loop {
                line.clear();
                // Read at most one bounded line; a longer one is sent in cuts.
                let read = (&mut reader)
                    .take(MAX_LOG_LINE as u64)
                    .read_until(b'\n', &mut line)
                    .await;
                match read {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if line.last() == Some(&b'\n') {
                            line.pop();
                        }
                        let _ = events.send(Event::Log {
                            service: service.clone(),
                            stream: which,
                            line: String::from_utf8_lossy(&line).into_owned(),
                        });
                    }
                }
            }
        })
    }
}

async fn exited(exit: &mut watch::Receiver<Option<ExitStatus>>) -> ExitStatus {
    loop {
        if let Some(status) = *exit.borrow_and_update() {
            return status;
        }
        if exit.changed().await.is_err() {
            return ExitStatus {
                code: None,
                signal: None,
            };
        }
    }
}
