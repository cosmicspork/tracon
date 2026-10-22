//! Restarting the node from its own API, without cutting work off.
//!
//! A restart is what applies a `node.toml` key the running node cannot take
//! in live (`config::live`). It is asked for in one of two modes:
//!
//! - `when_idle`, the default: the node stops taking new sessions and new
//!   turns, waits (bounded) for the turns and required checks already running
//!   to finish, then restarts. If they have not finished by the deadline the
//!   restart is abandoned, the node takes work again, and the operator is told.
//! - `now`: the node restarts at once, ending every live session. The caller
//!   is shown which sessions those are and has to confirm them.
//!
//! Either way the file is read again just before the restart: one that
//! changed since the request named it, or that no longer loads, is not
//! restarted into.
//!
//! What restarts the node is whatever supervises it. Under the fixed user
//! service (systemd, launchd) that is the service's own restart, as the
//! maintenance route has always done. Where the deployment says the
//! container is restarted when its process exits ([`EXIT_ENV`], which the
//! Kubernetes manifests set), the node shuts down the way SIGTERM shuts it
//! down and exits 0, and the kubelet starts it again on the same volume and
//! pod IP. Anywhere else nothing would start it again, so the request is
//! refused with the reason.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::session::state::SessionState;
use crate::session::Manager;
use crate::stream::Frame;

/// How long `when_idle` waits for running work by default.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(30 * 60);
/// The longest wait a caller may ask for.
pub const MAX_WAIT: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a restart handed to the supervisor may take before the node,
/// still running and not stopping, decides it did not happen. Longer than
/// the user unit's stop timeout.
pub const WATCHDOG: Duration = Duration::from_secs(120);

/// Set to `1` where the container is restarted whenever its process exits
/// (a Kubernetes Deployment's pods, a container run with a restart policy of
/// always): a restart is then a clean exit.
pub const EXIT_ENV: &str = "TRACON_RESTART_BY_EXIT";
pub const EXIT: &str = "exit for the container to be restarted";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    WhenIdle,
    Now,
}

/// What starts the node again after it stops. The system's answer in
/// production; a test's stand-in otherwise. Both calls may block (they run
/// `systemctl`); callers on the runtime use [`mechanism_of`].
pub trait Supervisor: Send + Sync {
    /// The mechanism that would restart this node, or why there is none.
    fn mechanism(&self) -> Result<&'static str, String>;
    /// Restart. Returns once the restart is under way: the process is about
    /// to be stopped, or is stopping itself.
    fn restart(&self) -> Result<(), String>;
}

/// The supervisor this process actually runs under.
pub struct System;

fn exit_opted_in() -> bool {
    std::env::var(EXIT_ENV).is_ok_and(|v| v.trim() == "1")
}

impl Supervisor for System {
    fn mechanism(&self) -> Result<&'static str, String> {
        if exit_opted_in() {
            return Ok(EXIT);
        }
        crate::service::service_restart_available().map_err(|why| {
            format!(
                "{why}; nothing would start this node again, so restart it the way it was started \
                 (a container restarted whenever it exits sets {EXIT_ENV}=1)"
            )
        })
    }

    fn restart(&self) -> Result<(), String> {
        match self.mechanism()? {
            EXIT => {
                request_exit();
                Ok(())
            }
            _ => crate::service::schedule(crate::service::LifecycleAction::Restart)
                .map_err(|e| e.to_string()),
        }
    }
}

/// [`Supervisor::mechanism`], off the async threads.
pub async fn mechanism_of(supervisor: Arc<dyn Supervisor>) -> Result<&'static str, String> {
    tokio::task::spawn_blocking(move || supervisor.mechanism())
        .await
        .unwrap_or_else(|e| Err(e.to_string()))
}

async fn restart_with(supervisor: Arc<dyn Supervisor>) -> Result<(), String> {
    tokio::task::spawn_blocking(move || supervisor.restart())
        .await
        .unwrap_or_else(|e| Err(e.to_string()))
}

fn exit_sender() -> &'static tokio::sync::watch::Sender<bool> {
    static EXIT_ASKED: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> =
        std::sync::OnceLock::new();
    EXIT_ASKED.get_or_init(|| tokio::sync::watch::channel(false).0)
}

/// Ask `serve` to shut down as SIGTERM would and the process to exit 0, for
/// a supervisor that starts it again.
pub fn request_exit() {
    tracing::info!("restart requested: shutting down for the supervisor to start the node again");
    exit_sender().send_replace(true);
}

/// Resolves once `request_exit` has been called.
pub async fn exit_requested() {
    let mut rx = exit_sender().subscribe();
    let _ = rx.wait_for(|asked| *asked).await;
}

/// Whether this process is stopping to be started again.
pub fn exit_was_requested() -> bool {
    *exit_sender().borrow()
}

static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// `serve` has begun to shut down, for whatever reason.
pub fn note_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
}

fn shutting_down() -> bool {
    SHUTTING_DOWN.load(Ordering::SeqCst) || exit_was_requested()
}

/// A restart asked for and not yet done.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub mode: Mode,
    pub requested_ms: i64,
    /// When `when_idle` stops waiting and gives up.
    pub deadline_ms: i64,
    /// `draining` while it waits, `restarting` once the supervisor has it.
    pub state: &'static str,
    /// When the supervisor was handed it.
    pub restarting_ms: Option<i64>,
    /// The `node.toml` version it was decided against.
    pub expected_hash: String,
    #[serde(skip)]
    generation: u64,
}

#[derive(Default)]
struct State {
    plan: Option<Plan>,
    generation: u64,
    /// Why the last restart did not happen.
    last_error: Option<String>,
    /// The status last published, so an unchanged one is not sent again.
    published: Option<Value>,
}

/// The node's restart state, held by the session manager: it is what refuses
/// new sessions and turns while a restart waits.
pub struct Restart {
    state: parking_lot::Mutex<State>,
    supervisor: parking_lot::RwLock<Arc<dyn Supervisor>>,
    poll: parking_lot::RwLock<Duration>,
    watchdog: parking_lot::RwLock<Duration>,
}

impl Default for Restart {
    fn default() -> Self {
        Self {
            state: Default::default(),
            supervisor: parking_lot::RwLock::new(Arc::new(System)),
            poll: parking_lot::RwLock::new(Duration::from_secs(2)),
            watchdog: parking_lot::RwLock::new(WATCHDOG),
        }
    }
}

impl Restart {
    /// A restart is waiting: new sessions and turns are refused.
    pub fn draining(&self) -> bool {
        self.state.lock().plan.is_some()
    }

    /// The refusal a new session or turn gets while a restart waits.
    pub fn refusal(&self) -> Option<String> {
        self.state.lock().plan.as_ref().map(|plan| match plan.mode {
            Mode::WhenIdle => {
                "the node is restarting once the running turns finish; try again after it is back"
                    .to_string()
            }
            Mode::Now => "the node is restarting; try again after it is back".to_string(),
        })
    }

    pub fn plan(&self) -> Option<Plan> {
        self.state.lock().plan.clone()
    }

    /// Stand a test's supervisor in for the system's.
    pub fn set_supervisor(&self, supervisor: Arc<dyn Supervisor>) {
        *self.supervisor.write() = supervisor;
    }

    /// How often a waiting restart looks at what is running.
    pub fn set_poll(&self, every: Duration) {
        *self.poll.write() = every;
    }

    /// How long a restart handed to the supervisor may take ([`WATCHDOG`]).
    pub fn set_watchdog(&self, after: Duration) {
        *self.watchdog.write() = after;
    }

    fn supervisor(&self) -> Arc<dyn Supervisor> {
        self.supervisor.read().clone()
    }
}

/// A restart request, as `POST /api/admin/restart` takes it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub mode: Mode,
    /// The `node.toml` version the caller decided against: a restart loads
    /// the file as it is, so it must be the one the caller saw.
    pub expected_hash: String,
    /// Required for `now`: the caller has seen which sessions it ends.
    #[serde(default)]
    pub confirm: bool,
    /// How long `when_idle` waits, in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug)]
pub enum Refused {
    /// The file is not the version named, or does not load.
    Conflict(String),
    /// Nothing would start the node again.
    Unavailable(String),
    /// `now` without `confirm`: these sessions would end.
    ConfirmRequired(Vec<Value>),
}

/// What is running that `when_idle` waits for.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Busy {
    /// Sessions on this node with a turn running.
    pub turns: Vec<String>,
    /// Required checks running.
    pub checks: i64,
    /// Review submissions whose checks have not all run: a review's checks
    /// run one after another, so between two of them none is running, and
    /// the review is still in flight. New ones are refused during a drain.
    /// The node's own runs of an external candidate's checks count too.
    pub reviews: i64,
}

impl Busy {
    pub fn idle(&self) -> bool {
        self.turns.is_empty() && self.checks == 0 && self.reviews == 0
    }
}

/// What is running now.
pub fn busy(manager: &Manager) -> Busy {
    let store = manager.store();
    let turns = store
        .list_sessions(None)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| {
            s.node_id == manager.node_id()
                && s.turn_active != 0
                && !SessionState::from_stored(&s.state).is_terminal()
        })
        .map(|s| s.id)
        .collect();
    Busy {
        turns,
        checks: store
            .running_check_run_ids()
            .map(|ids| ids.len() as i64)
            .unwrap_or(0),
        // A submission's checks, and the node's own of an external one.
        reviews: (manager.checking().in_flight() + manager.external_checks().in_flight()) as i64,
    }
}

/// The sessions a restart ends: every live one this node runs, as
/// `session::reconcile_after_restart` would find them.
pub fn closing(manager: &Manager) -> Vec<Value> {
    manager
        .store()
        .list_sessions(None)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| {
            s.node_id == manager.node_id()
                && !SessionState::from_stored(&s.state).is_terminal()
                && s.state != SessionState::Suspended.as_str()
        })
        .map(|s| {
            json!({
                "id": s.id,
                "channel": s.channel,
                "repo_path": s.repo_path,
                "branch": s.branch,
                "state": s.state,
                "turn_active": s.turn_active != 0,
            })
        })
        .collect()
}

/// Everything the interface shows about restarting: what a restart would
/// change, whether one can be done here and how, and the one waiting.
pub async fn status(manager: &Manager) -> Value {
    let cfg = manager.cfg();
    let pending = crate::config::live::pending_restart_at(cfg, &Config::config_path());
    let restart = manager.restart();
    let mechanism = mechanism_of(restart.supervisor()).await;
    let (plan, last_error) = {
        let state = restart.state.lock();
        (state.plan.clone(), state.last_error.clone())
    };
    json!({
        "config": pending,
        "restart": {
            "available": mechanism.is_ok(),
            "mechanism": mechanism.as_ref().ok(),
            "reason": mechanism.as_ref().err(),
        },
        "busy": busy(manager),
        "plan": plan,
        "closing": closing(manager),
        "last_error": last_error,
    })
}

/// Send the status on the stream if what the banner says changed since it
/// was last sent. Called from the node's 15-second tick, so the look itself
/// is cheap: the file's hash and diff, and the plan. Asking the supervisor
/// (which runs `systemctl`) is left to a change.
pub async fn publish(manager: &Manager) {
    let shown = {
        let pending =
            crate::config::live::pending_restart_at(manager.cfg(), &Config::config_path());
        let state = manager.restart().state.lock();
        json!({
            "config": pending,
            "plan": state.plan,
            "last_error": state.last_error,
        })
    };
    {
        let mut state = manager.restart().state.lock();
        if state.published.as_ref() == Some(&shown) {
            return;
        }
        state.published = Some(shown);
    }
    manager.bus().publish(Frame::Restart(status(manager).await));
}

/// The file as it is now, if it is `expected` and loads; else why not.
fn file_is(expected: &str, since: &str) -> Result<(), String> {
    let snapshot = crate::config::edit::snapshot_at(&Config::config_path())
        .map_err(|e| format!("{e}; the node would not start from it"))?;
    if snapshot.hash != expected {
        return Err(format!(
            "node.toml changed since {since}; it is now {}",
            snapshot.hash
        ));
    }
    Ok(())
}

/// Ask for a restart. On success the restart is waiting (`when_idle`) or
/// under way (`now`), and the status is returned.
pub async fn request(manager: &Manager, req: Request) -> Result<Value, Refused> {
    file_is(&req.expected_hash, "it was read").map_err(Refused::Conflict)?;
    let restart = manager.restart().clone();
    mechanism_of(restart.supervisor())
        .await
        .map_err(Refused::Unavailable)?;
    if req.mode == Mode::Now && !req.confirm {
        return Err(Refused::ConfirmRequired(closing(manager)));
    }
    let wait = req
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_WAIT)
        .clamp(Duration::from_secs(1), MAX_WAIT);
    let now = crate::store::now_ms();
    let generation = {
        let mut state = restart.state.lock();
        // Asked again while one waits: `now` hurries it along, and a second
        // `when_idle` is the one already waiting.
        let waiting = state
            .plan
            .as_ref()
            .is_some_and(|plan| plan.state == "restarting" || req.mode == Mode::WhenIdle);
        if waiting {
            None
        } else {
            state.generation += 1;
            state.last_error = None;
            state.plan = Some(Plan {
                mode: req.mode,
                requested_ms: now,
                deadline_ms: now + wait.as_millis() as i64,
                state: "draining",
                restarting_ms: None,
                expected_hash: req.expected_hash.clone(),
                generation: state.generation,
            });
            Some(state.generation)
        }
    };
    let Some(generation) = generation else {
        tracing::info!("a restart is already waiting");
        return Ok(status(manager).await);
    };
    tracing::warn!(mode = ?req.mode, wait_secs = wait.as_secs(), "restart requested; new sessions and turns are refused until it happens");
    publish(manager).await;
    tokio::spawn(drive(manager.clone(), generation));
    Ok(status(manager).await)
}

/// Withdraw a restart: one still waiting for running work, or one the
/// supervisor was handed longer than the watchdog ago and that evidently did
/// not happen. The node takes work again. `false` when there is none to
/// withdraw.
pub async fn cancel(manager: &Manager) -> bool {
    let restart = manager.restart();
    let stuck_after = restart.watchdog.read().as_millis() as i64;
    let cancelled = {
        let mut state = restart.state.lock();
        let withdrawable = state.plan.as_ref().is_some_and(|plan| match plan.state {
            "draining" => true,
            // Not once the node is stopping: the restart is happening.
            _ => {
                !shutting_down()
                    && plan
                        .restarting_ms
                        .is_some_and(|at| crate::store::now_ms() - at >= stuck_after)
            }
        });
        if withdrawable {
            state.plan = None;
            state.generation += 1;
        }
        withdrawable
    };
    if cancelled {
        tracing::info!("restart withdrawn; the node takes work again");
        publish(manager).await;
    }
    cancelled
}

/// Wait for the work a `when_idle` restart waits for, then restart; or give
/// up at the deadline.
async fn drive(manager: Manager, generation: u64) {
    let restart = manager.restart().clone();
    let plan = loop {
        let Some(plan) = restart.plan().filter(|p| p.generation == generation) else {
            return;
        };
        if plan.mode == Mode::Now {
            // Long enough for the answer to the request to leave.
            tokio::time::sleep(Duration::from_millis(300)).await;
            break plan;
        }
        let busy = busy(&manager);
        if busy.idle() {
            break plan;
        }
        if crate::store::now_ms() >= plan.deadline_ms {
            let plural =
                |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
            let why = format!(
                "gave up waiting: {}, {} and {} still running at the deadline",
                plural(busy.turns.len(), "turn"),
                plural(busy.checks as usize, "check"),
                plural(busy.reviews as usize, "review submission"),
            );
            abandon(&manager, generation, why).await;
            return;
        }
        let poll = *restart.poll.read();
        tokio::time::sleep(poll).await;
    };
    // What the restart would load, read again now: the wait may have been
    // long, and the file is only restarted into if it is still the one named.
    if let Err(why) = file_is(&plan.expected_hash, "the restart was asked for") {
        abandon(&manager, generation, why).await;
        return;
    }
    {
        let mut state = restart.state.lock();
        match &mut state.plan {
            Some(plan) if plan.generation == generation => {
                plan.state = "restarting";
                plan.restarting_ms = Some(crate::store::now_ms());
            }
            // Withdrawn while it waited.
            _ => return,
        }
    }
    publish(&manager).await;
    tracing::warn!("restarting the node");
    if let Err(why) = restart_with(restart.supervisor()).await {
        abandon(
            &manager,
            generation,
            format!("the restart could not be started: {why}"),
        )
        .await;
        return;
    }
    // Handed over. If the node is still here and not stopping once the
    // supervisor has had ample time, the restart did not happen: say so,
    // and take work again rather than refuse it for ever.
    let after = *restart.watchdog.read();
    tokio::time::sleep(after).await;
    if shutting_down() {
        return;
    }
    abandon(
        &manager,
        generation,
        format!(
            "the node was handed to its supervisor to restart and is still running {} s later",
            after.as_secs()
        ),
    )
    .await;
}

/// Give a restart up: the node takes work again, the reason is kept for the
/// banner, and the operator's devices are told.
async fn abandon(manager: &Manager, generation: u64, why: String) {
    {
        let mut state = manager.restart().state.lock();
        if state.plan.as_ref().map(|p| p.generation) != Some(generation) {
            return;
        }
        state.plan = None;
        state.last_error = Some(why.clone());
    }
    tracing::warn!(%why, "restart abandoned; the node takes work again");
    publish(manager).await;
    crate::notify::send_operator(
        manager.store(),
        manager.cfg(),
        &uuid::Uuid::now_v7().to_string(),
        "Restart abandoned".into(),
        format!("{why}."),
        "/settings".into(),
        &[],
    )
    .await;
}
