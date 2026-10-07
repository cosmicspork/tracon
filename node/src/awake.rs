//! Keeping the machine awake while a session works, and saying so when it
//! slept anyway.
//!
//! A desktop host that idle-suspends in the middle of a turn stops the session
//! dead (2026-09-30, execute session `01a0f4b3`, 54 minutes asleep during the
//! harness's compaction), and one that sleeps while a permission card waits
//! woke to find the card "denied: unanswered" because its deadline was the
//! wall clock (2026-10-03, session `01a10447`). So:
//!
//! - While any turn on this node is running or any permission is waiting, the
//!   node holds a sleep inhibitor — logind's, through `systemd-inhibit`, on
//!   Linux; a power assertion, through `caffeinate`, on macOS — and releases it
//!   when the node is idle. The holder is a child that ends with the node, so a
//!   node that dies never leaves the machine pinned awake.
//! - A suspend that happens anyway (the lid closed, the operator chose to) is
//!   noticed from the gap between the wall clock and the monotonic one, which
//!   does not advance while the host sleeps. It is recorded on every session it
//!   interrupted, and the deadlines of what was waiting are moved on by the time
//!   the node was asleep: a card's deadline counts only time the node was
//!   awake. (A session's own expiry check is on the monotonic clock already.)
//!
//! What is held, and the last suspend, are served at `/api/awake` and pushed as
//! an `awake` frame for the interface to show.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use serde_json::json;

use crate::session::state::{event_kind as ek, SessionState};
use crate::session::Manager;
use crate::stream::Frame;

/// A gap between the clocks shorter than this is the wall clock being
/// corrected, or a busy machine, not a suspend.
pub const SUSPEND_THRESHOLD: Duration = Duration::from_secs(30);

/// How often the node looks at what is running.
const TICK: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Status {
    /// The inhibitor is held now.
    pub held: bool,
    /// What it is held for, in a phrase.
    pub reason: Option<String>,
    /// How it is held: `logind` or `caffeinate`. `None` where this host
    /// offers neither, which the interface says.
    pub method: Option<&'static str>,
    /// Why holding it failed, the last time it was tried.
    pub error: Option<String>,
    /// The last suspend the node noticed: when it woke and how long it slept.
    pub last_suspend: Option<Suspend>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Suspend {
    pub woke_ms: i64,
    pub asleep_ms: i64,
}

/// The two clocks as they read at the last look.
struct Clocks {
    wall: SystemTime,
    mono: Instant,
}

impl Clocks {
    fn now() -> Self {
        Self {
            wall: SystemTime::now(),
            mono: Instant::now(),
        }
    }
}

/// How long the host slept between two readings: the wall clock's advance
/// beyond the monotonic clock's, when that is more than a correction.
pub fn asleep_between(wall_elapsed: Duration, mono_elapsed: Duration) -> Option<Duration> {
    let gap = wall_elapsed.checked_sub(mono_elapsed)?;
    (gap >= SUSPEND_THRESHOLD).then_some(gap)
}

static CURRENT: std::sync::OnceLock<Arc<Awake>> = std::sync::OnceLock::new();

/// Make this the node's watcher, for the API to read. The first wins.
pub fn install(awake: Arc<Awake>) {
    let _ = CURRENT.set(awake);
}

/// The node's watcher, once `serve` has started one.
pub fn current() -> Option<&'static Arc<Awake>> {
    CURRENT.get()
}

pub struct Awake {
    manager: Manager,
    method: Option<&'static str>,
    clocks: Mutex<Clocks>,
    holder: tokio::sync::Mutex<Option<tokio::process::Child>>,
    status: Mutex<Status>,
}

impl Awake {
    pub fn new(manager: Manager) -> Arc<Self> {
        let method = inhibitor_method();
        Arc::new(Self {
            manager,
            method,
            clocks: Mutex::new(Clocks::now()),
            holder: tokio::sync::Mutex::new(None),
            status: Mutex::new(Status {
                method,
                ..Default::default()
            }),
        })
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// Look, and hold or release, every few seconds for the node's life.
    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.catch_up().await;
            let reason = self.needed();
            self.apply(reason).await;
        }
    }

    /// Notice a suspend since the last look, once. Anything that is about to
    /// act on a wall-clock deadline calls this first, so a deadline is moved
    /// on before it can be judged.
    pub async fn catch_up(&self) {
        let asleep = {
            let mut last = self.clocks.lock().unwrap();
            let now = Clocks::now();
            let wall = now.wall.duration_since(last.wall).unwrap_or_default();
            let mono = now.mono.duration_since(last.mono);
            *last = now;
            asleep_between(wall, mono)
        };
        if let Some(asleep) = asleep {
            self.suspended(asleep).await;
        }
    }

    /// What the watcher does when it finds the host slept for `asleep`.
    pub async fn suspended(&self, asleep: Duration) {
        let asleep_ms = asleep.as_millis() as i64;
        let store = self.manager.store();
        let node_id = self.manager.node_id().to_string();
        let held = self.status.lock().unwrap().held;
        tracing::warn!(asleep_ms, held, "the host was suspended");
        // The waiting cards keep the time they had left.
        let _ = store.extend_open_permissions(&node_id, asleep_ms);
        let _ = store.extend_pending_approvals(&node_id, asleep_ms);
        // An interruption, on every session it caught live.
        for s in store.list_sessions(None).unwrap_or_default() {
            if s.node_id != node_id || SessionState::from_stored(&s.state).is_terminal() {
                continue;
            }
            self.manager.record_event(
                &s.id,
                ek::HOST_SUSPENDED,
                json!({
                    "asleep_ms": asleep_ms,
                    "inhibitor_held": held,
                    "turn_active": s.turn_active != 0,
                    "state": s.state,
                }),
            );
        }
        self.manager.publish_queue().await;
        let changed = {
            let mut status = self.status.lock().unwrap();
            status.last_suspend = Some(Suspend {
                woke_ms: crate::store::now_ms(),
                asleep_ms,
            });
            status.clone()
        };
        self.publish(&changed);
    }

    /// Why the machine should stay awake now, or `None` when the node is idle.
    fn needed(&self) -> Option<String> {
        let store = self.manager.store();
        let node_id = self.manager.node_id();
        let waiting = store
            .open_permissions()
            .unwrap_or_default()
            .iter()
            .filter(|p| p.node_id == node_id)
            .count();
        let working = store
            .list_sessions(None)
            .unwrap_or_default()
            .iter()
            .filter(|s| {
                s.node_id == node_id
                    && s.turn_active != 0
                    && !SessionState::from_stored(&s.state).is_terminal()
            })
            .count();
        reason(working, waiting)
    }

    async fn apply(&self, reason: Option<String>) {
        let mut holder = self.holder.lock().await;
        // A holder that ended by itself (logind refused it, the binary went
        // away) is not held.
        if let Some(child) = holder.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                *holder = None;
            }
        }
        let mut error = None;
        match (&reason, holder.is_some()) {
            (Some(why), false) => match self.method.map(|m| spawn_holder(m, why)) {
                Some(Ok(child)) => *holder = Some(child),
                Some(Err(e)) => error = Some(e),
                None => {}
            },
            (None, true) => {
                if let Some(mut child) = holder.take() {
                    let _ = child.kill().await;
                }
            }
            _ => {}
        }
        let held = holder.is_some();
        drop(holder);
        let changed = {
            let mut status = self.status.lock().unwrap();
            // A failure stands until a hold succeeds or nothing needs one.
            let error = match (&reason, held) {
                (Some(_), false) => error.or_else(|| status.error.clone()),
                _ => None,
            };
            let next = Status {
                held,
                reason: reason.filter(|_| held),
                method: self.method,
                error,
                last_suspend: status.last_suspend,
            };
            if *status == next {
                return;
            }
            *status = next.clone();
            next
        };
        if changed.held {
            tracing::info!(reason = ?changed.reason, "holding the machine awake");
        } else if let Some(e) = &changed.error {
            tracing::warn!(error = %e, "could not hold the machine awake");
        } else {
            tracing::info!("released the machine to sleep");
        }
        self.publish(&changed);
    }

    fn publish(&self, status: &Status) {
        self.manager.bus().publish(Frame::Awake(
            serde_json::to_value(status).unwrap_or_default(),
        ));
    }
}

/// The phrase for what is keeping the machine awake.
pub fn reason(working: usize, waiting: usize) -> Option<String> {
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    match (working, waiting) {
        (0, 0) => None,
        (w, 0) => Some(format!("{} working", plural(w, "session", "sessions"))),
        (0, p) => Some(format!(
            "{} waiting",
            plural(p, "permission", "permissions")
        )),
        (w, p) => Some(format!(
            "{} working, {} waiting",
            plural(w, "session", "sessions"),
            plural(p, "permission", "permissions")
        )),
    }
}

/// Which inhibitor this host offers.
fn inhibitor_method() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        std::path::Path::new("/usr/bin/caffeinate")
            .exists()
            .then_some("caffeinate")
    } else if cfg!(target_os = "linux") {
        on_path("systemd-inhibit").then_some("logind")
    } else {
        None
    }
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

/// The command that holds the inhibitor for as long as it runs, and stops
/// running when this process does.
pub fn holder_command(method: &str, why: &str, pid: u32) -> Option<(String, Vec<String>)> {
    match method {
        // Block both: an idle suspend is the one that bit, and a sleep key
        // pressed by habit while a turn runs is the other. logind lets the
        // operator override a block explicitly, which is theirs to do.
        "logind" => Some((
            "systemd-inhibit".into(),
            vec![
                "--what=sleep:idle".into(),
                "--who=tracon".into(),
                format!("--why=A tracon session is working ({why})"),
                "--mode=block".into(),
                "tail".into(),
                format!("--pid={pid}"),
                "-f".into(),
                "/dev/null".into(),
            ],
        )),
        // `-i` holds off idle sleep; `-w` ends the assertion with this process.
        "caffeinate" => Some((
            "/usr/bin/caffeinate".into(),
            vec!["-i".into(), "-w".into(), pid.to_string()],
        )),
        _ => None,
    }
}

fn spawn_holder(method: &str, why: &str) -> Result<tokio::process::Child, String> {
    let (program, args) =
        holder_command(method, why, std::process::id()).ok_or("no inhibitor on this host")?;
    tokio::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start {program}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suspend_is_the_wall_clock_running_on_without_the_monotonic_one() {
        let s = Duration::from_secs;
        assert_eq!(asleep_between(s(65), s(5)), Some(s(60)));
        // A clock correction, or a slow tick, is not a suspend.
        assert_eq!(asleep_between(s(10), s(5)), None);
        // The wall clock set back is not one either.
        assert_eq!(asleep_between(s(1), s(5)), None);
    }

    #[test]
    fn the_reason_says_what_is_keeping_the_machine_up() {
        assert_eq!(reason(0, 0), None);
        assert_eq!(reason(1, 0).as_deref(), Some("1 session working"));
        assert_eq!(reason(0, 2).as_deref(), Some("2 permissions waiting"));
        assert_eq!(
            reason(2, 1).as_deref(),
            Some("2 sessions working, 1 permission waiting")
        );
    }

    #[test]
    fn the_holder_ends_with_the_node() {
        let (program, args) = holder_command("logind", "1 session working", 42).unwrap();
        assert_eq!(program, "systemd-inhibit");
        assert!(args.contains(&"--what=sleep:idle".to_string()), "{args:?}");
        assert!(args.contains(&"--mode=block".to_string()), "{args:?}");
        assert!(args.contains(&"--pid=42".to_string()), "{args:?}");
        let (program, args) = holder_command("caffeinate", "x", 42).unwrap();
        assert_eq!(program, "/usr/bin/caffeinate");
        assert_eq!(args, vec!["-i", "-w", "42"]);
        assert!(holder_command("none", "x", 1).is_none());
    }
}
