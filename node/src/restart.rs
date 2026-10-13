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
//! What restarts the node is whatever supervises it. Under the fixed user
//! service (systemd, launchd) that is the service's own restart, as the
//! maintenance route has always done. In a Kubernetes pod the node shuts down
//! the way SIGTERM shuts it down and exits 0; the kubelet starts the
//! container again, on the same volume and the same pod IP. Anywhere else
//! nothing would start it again, so the request is refused with the reason.

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

pub const KUBERNETES: &str = "kubernetes";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    WhenIdle,
    Now,
}

/// What starts the node again after it stops. The system's answer in
/// production; a test's stand-in otherwise.
pub trait Supervisor: Send + Sync {
    /// The mechanism that would restart this node, or why there is none.
    fn mechanism(&self) -> Result<&'static str, String>;
    /// Restart. Returns once the restart is under way: the process is about
    /// to be stopped, or is stopping itself.
    fn restart(&self) -> Result<(), String>;
}

/// The supervisor this process actually runs under.
pub struct System;

impl Supervisor for System {
    fn mechanism(&self) -> Result<&'static str, String> {
        if crate::service::in_kubernetes() {
            return Ok(KUBERNETES);
        }
        crate::service::service_restart_available().map_err(|why| {
            format!(
                "{why}; nothing would start this node again, so restart it the way it was started"
            )
        })
    }

    fn restart(&self) -> Result<(), String> {
        match self.mechanism()? {
            KUBERNETES => {
                request_exit();
                Ok(())
            }
            _ => crate::service::schedule(crate::service::LifecycleAction::Restart)
                .map_err(|e| e.to_string()),
        }
    }
}

fn exit_sender() -> &'static tokio::sync::watch::Sender<bool> {
    static EXIT: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> = std::sync::OnceLock::new();
    EXIT.get_or_init(|| tokio::sync::watch::channel(false).0)
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

/// A restart asked for and not yet done.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub mode: Mode,
    pub requested_ms: i64,
    /// When `when_idle` stops waiting and gives up.
    pub deadline_ms: i64,
    /// `draining` while it waits, `restarting` once the supervisor has it.
    pub state: &'static str,
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
}

impl Default for Restart {
    fn default() -> Self {
        Self {
            state: Default::default(),
            supervisor: parking_lot::RwLock::new(Arc::new(System)),
            poll: parking_lot::RwLock::new(Duration::from_secs(2)),
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
}

impl Busy {
    pub fn idle(&self) -> bool {
        self.turns.is_empty() && self.checks == 0
    }
}

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
pub fn status(manager: &Manager) -> Value {
    let cfg = manager.cfg();
    let pending = crate::config::live::pending_restart_at(cfg, &Config::config_path());
    let restart = manager.restart();
    let mechanism = restart.supervisor().mechanism();
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
        "plan": plan,
        "busy": busy(manager),
        "closing": closing(manager),
        "last_error": last_error,
    })
}

/// Send the status on the stream if what the banner says changed since it
/// was last sent. Called from the node's 15-second tick, so the look itself
/// is cheap: the file's hash and diff, and the plan. Asking the supervisor
/// (which runs `systemctl`) is left to a change.
pub fn publish(manager: &Manager) {
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
    manager.bus().publish(Frame::Restart(status(manager)));
}

/// Ask for a restart. On success the restart is waiting (`when_idle`) or
/// under way (`now`), and the status is returned.
pub fn request(manager: &Manager, req: Request) -> Result<Value, Refused> {
    let path = Config::config_path();
    let snapshot = crate::config::edit::snapshot_at(&path).map_err(|e| {
        Refused::Conflict(format!(
            "{e}; the node would not start from it, so it is not restarted"
        ))
    })?;
    if snapshot.hash != req.expected_hash {
        return Err(Refused::Conflict(format!(
            "node.toml changed since it was read; it is now {}",
            snapshot.hash
        )));
    }
    let restart = manager.restart().clone();
    restart
        .supervisor()
        .mechanism()
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
        match &mut state.plan {
            // Asked again while one waits: `now` hurries it along, and a
            // second `when_idle` is the one already waiting.
            Some(plan) if plan.state == "restarting" || req.mode == Mode::WhenIdle => {
                let plan = plan.clone();
                drop(state);
                tracing::info!(mode = ?plan.mode, "a restart is already waiting");
                return Ok(status(manager));
            }
            _ => {}
        }
        state.generation += 1;
        state.last_error = None;
        state.plan = Some(Plan {
            mode: req.mode,
            requested_ms: now,
            deadline_ms: now + wait.as_millis() as i64,
            state: "draining",
            expected_hash: req.expected_hash.clone(),
            generation: state.generation,
        });
        state.generation
    };
    tracing::warn!(mode = ?req.mode, wait_secs = wait.as_secs(), "restart requested; new sessions and turns are refused until it happens");
    publish(manager);
    tokio::spawn(drive(manager.clone(), generation));
    Ok(status(manager))
}

/// Withdraw a waiting restart: the node takes work again. `false` when none
/// was waiting or it is already under way.
pub fn cancel(manager: &Manager) -> bool {
    let cancelled = {
        let mut state = manager.restart().state.lock();
        match &state.plan {
            Some(plan) if plan.state == "draining" => {
                state.plan = None;
                state.generation += 1;
                true
            }
            _ => false,
        }
    };
    if cancelled {
        tracing::info!("restart withdrawn; the node takes work again");
        publish(manager);
    }
    cancelled
}

/// Wait for the work a `when_idle` restart waits for, then restart; or give
/// up at the deadline.
async fn drive(manager: Manager, generation: u64) {
    let restart = manager.restart().clone();
    loop {
        let Some(plan) = restart.plan().filter(|p| p.generation == generation) else {
            return;
        };
        if plan.mode == Mode::Now {
            // Long enough for the answer to the request to leave.
            tokio::time::sleep(Duration::from_millis(300)).await;
            break;
        }
        let busy = busy(&manager);
        if busy.idle() {
            break;
        }
        if crate::store::now_ms() >= plan.deadline_ms {
            abandon(&manager, generation, &busy).await;
            return;
        }
        let poll = *restart.poll.read();
        tokio::time::sleep(poll).await;
    }
    {
        let mut state = restart.state.lock();
        match &mut state.plan {
            Some(plan) if plan.generation == generation => plan.state = "restarting",
            // Withdrawn while it waited.
            _ => return,
        }
    }
    publish(&manager);
    tracing::warn!("restarting the node");
    if let Err(why) = restart.supervisor().restart() {
        tracing::error!(%why, "the restart could not be started; the node takes work again");
        {
            let mut state = restart.state.lock();
            state.plan = None;
            state.last_error = Some(why);
        }
        publish(&manager);
    }
}

async fn abandon(manager: &Manager, generation: u64, busy: &Busy) {
    let why = format!(
        "gave up waiting: {} turn{} and {} check{} still running at the deadline",
        busy.turns.len(),
        if busy.turns.len() == 1 { "" } else { "s" },
        busy.checks,
        if busy.checks == 1 { "" } else { "s" },
    );
    {
        let mut state = manager.restart().state.lock();
        if state.plan.as_ref().map(|p| p.generation) != Some(generation) {
            return;
        }
        state.plan = None;
        state.last_error = Some(why.clone());
    }
    tracing::warn!(%why, "restart abandoned; the node takes work again");
    publish(manager);
    crate::notify::send_operator(
        manager.store(),
        manager.cfg(),
        &uuid::Uuid::now_v7().to_string(),
        "Restart abandoned".into(),
        format!("{why}. Restart now from Settings to end them."),
        "/settings".into(),
        &[],
    )
    .await;
}
