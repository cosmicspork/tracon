//! Running the node, when nothing else is.
//!
//! The node deliberately does not daemonize: it logs to stdout, shuts down
//! cleanly on SIGTERM, and is idempotent on restart, so something else is meant
//! to run it. On a laptop that something can be this app rather than the
//! platform — the node is wanted while you are logged in and working, and a
//! tray icon is a more honest representation of "it is running" than a unit
//! file you have to ask about.
//!
//! It adopts before it spawns. Two nodes over one state directory would fight
//! over the same SQLite file and the same harness socket, so a node that is
//! already answering is left alone and simply used — unless it is one a
//! previous run of this app started and handed over across an update, which
//! is claimed back and owned as if this run had started it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long to wait for a node to answer after starting it. Generous: the
/// first start of the day opens the store, runs migrations, and verifies the
/// boundary before it listens.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a stop may take. The same 90s the systemd unit allows, and for the
/// same reason: shutdown ends sessions and tears down their containers, and
/// cutting that short leaks containers rather than saving time. A node with
/// nothing running stops immediately.
const STOP_TIMEOUT: Duration = Duration::from_secs(90);

/// Restart a node that dies, unless it is dying immediately and repeatedly —
/// then it is misconfigured, and respawning it forever only hides the reason.
const MAX_RAPID_RESTARTS: u32 = 3;
const RAPID: Duration = Duration::from_secs(20);

/// A node this process is responsible for: the child it spawned, or one a
/// previous run of this app left behind for it, known only by pid.
enum Proc {
    Child(Child),
    Adopted(u32),
}

#[derive(Default)]
pub struct Node {
    child: Mutex<Option<Proc>>,
    /// Whether this process started the node, or claimed one its predecessor
    /// did. Nothing else may be stopped: a node found already running with no
    /// handoff record belongs to whoever started it.
    owned: AtomicBool,
    stopping: AtomicBool,
    /// Set by an update about to restart the app: the node is left running
    /// for the next run to claim, rather than stopped on the way out.
    handoff: AtomicBool,
}

/// What `ensure` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ensured {
    /// This run started the node.
    Started,
    /// A node a previous run of this app handed over was claimed back.
    Claimed,
    /// A node someone else runs is being used and will never be stopped.
    Foreign,
}

/// The record a run leaves for its successor: which process it started, from
/// which binary. Written beside the node's own state so the next run finds
/// it whatever directory it was launched from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub pid: u32,
    pub binary: String,
}

/// The node's state directory, as the node computes it (`Config::state_dir`
/// in the node crate): an explicit override, else the platform's state or
/// local-data directory.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TRACON_STATE_DIR") {
        return PathBuf::from(dir);
    }
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tracon")
}

fn handoff_path(state_dir: &Path) -> PathBuf {
    state_dir.join("desktop-node.json")
}

impl Handoff {
    fn write(&self, state_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(state_dir)?;
        std::fs::write(
            handoff_path(state_dir),
            serde_json::to_vec_pretty(self).unwrap_or_default(),
        )
    }

    fn read(state_dir: &Path) -> Option<Self> {
        let bytes = std::fs::read(handoff_path(state_dir)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn clear(state_dir: &Path) {
        let _ = std::fs::remove_file(handoff_path(state_dir));
    }
}

/// Whether a process with this pid exists. Signal 0 delivers nothing and
/// only asks; a pid that is gone answers ESRCH, one owned by someone else
/// answers EPERM — and is therefore alive.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// What `tracon --version` prints, reduced to the version.
pub fn parse_version_output(out: &str) -> Option<String> {
    out.split_whitespace()
        .last()
        .filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

/// The version of the node binary this app would start.
pub fn binary_version() -> Option<String> {
    let out = Command::new(binary()).arg("--version").output().ok()?;
    parse_version_output(&String::from_utf8_lossy(&out.stdout))
}

/// Whether a running node should be replaced by the binary at hand. Only a
/// version that is known on both sides and differs is a reason; not knowing
/// either is a reason to leave a working node alone.
pub fn needs_restart(running: Option<&str>, sidecar: Option<&str>) -> bool {
    match (running, sidecar) {
        (Some(r), Some(s)) => r != s,
        _ => false,
    }
}

/// Where the node's binary is. `TRACON_BIN` wins; otherwise the install
/// location `install.sh` and `cargo install` both use, then the PATH.
/// The node binary, in order: an explicit override, the sidecar the bundle
/// ships beside this executable (an AppImage's `usr/bin`, a `.app`'s
/// `Contents/MacOS`), the install location, then whatever is on PATH.
fn binary() -> String {
    binary_from(
        std::env::var("TRACON_BIN").ok(),
        std::env::current_exe().ok(),
        std::env::var_os("HOME").map(std::path::PathBuf::from),
    )
}

fn binary_from(
    override_bin: Option<String>,
    exe: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
) -> String {
    if let Some(p) = override_bin {
        return p;
    }
    let sidecar = exe.and_then(|e| e.parent().map(|d| d.join("tracon")));
    let installed = home.map(|h| h.join(".local/bin/tracon"));
    for p in [sidecar, installed].into_iter().flatten() {
        if p.is_file() {
            return p.to_string_lossy().into_owned();
        }
    }
    "tracon".into()
}

/// Launched from Finder, this process carries launchd's minimal PATH, and the
/// node's children (podman, gh, glab) live in the package-manager prefixes a
/// login shell would have added. Append them so the node behaves the same
/// double-clicked as it does from a terminal.
fn enriched_path(current: Option<String>) -> String {
    let mut path = current.unwrap_or_default();
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let present = std::env::split_paths(&path).any(|p| p == std::path::Path::new(dir));
        if !present {
            if !path.is_empty() {
                path.push(':');
            }
            path.push_str(dir);
        }
    }
    path
}

async fn answering(http: &reqwest::Client, url: &str) -> bool {
    http.get(format!("{url}/api/node"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// The version the running node reports, if it answers.
pub async fn running_version(http: &reqwest::Client, url: &str) -> Option<String> {
    let v: serde_json::Value = http
        .get(format!("{url}/api/health"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    v["version"].as_str().map(str::to_string)
}

/// How many sessions the node has running, if it answers. None when it does
/// not: a node that cannot be asked is not idle.
pub async fn running_sessions(http: &reqwest::Client, url: &str) -> Option<usize> {
    let v: serde_json::Value = http
        .get(format!("{url}/api/queue"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some(
        v["running"]
            .as_array()
            .map(|a| a.iter().filter(|s| s["state"] != "closed").count())
            .unwrap_or(0),
    )
}

impl Node {
    /// Whether this process is responsible for the node's lifetime.
    pub fn owned(&self) -> bool {
        self.owned.load(Ordering::SeqCst)
    }

    /// Whether an update asked for the node to be left running on exit.
    pub fn handing_off(&self) -> bool {
        self.handoff.load(Ordering::SeqCst)
    }

    /// Leave the node running when this process exits, for the next run of
    /// this app to claim. Called by an update about to restart the app.
    pub fn hand_off(&self) {
        self.handoff.store(true, Ordering::SeqCst);
    }

    fn spawn(&self) -> std::io::Result<()> {
        // stdout and stderr are inherited on purpose: run the app from a
        // terminal and the node's log is right there, which is the whole
        // debugging story for a wrapper that owns a child process.
        let bin = binary();
        let child = Command::new(&bin)
            .arg("serve")
            .env("PATH", enriched_path(std::env::var("PATH").ok()))
            .stdin(Stdio::null())
            .spawn()?;
        // The record is best effort: without it the next run treats the node
        // as foreign, which is the safe mistake.
        let _ = Handoff {
            pid: child.id(),
            binary: bin,
        }
        .write(&state_dir());
        *self.child.lock().unwrap() = Some(Proc::Child(child));
        self.owned.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Take over a node a previous run of this app started, if the record it
    /// left names a process that is still there.
    fn claim(&self, state_dir: &Path) -> bool {
        let Some(record) = Handoff::read(state_dir) else {
            return false;
        };
        if !process_alive(record.pid) {
            Handoff::clear(state_dir);
            return false;
        }
        *self.child.lock().unwrap() = Some(Proc::Adopted(record.pid));
        self.owned.store(true, Ordering::SeqCst);
        true
    }

    /// Adopt a running node, or start one. Returns once it answers.
    pub async fn ensure(
        self: &Arc<Self>,
        http: &reqwest::Client,
        url: &str,
    ) -> Result<Ensured, String> {
        if answering(http, url).await {
            // A node a previous run of this app handed over is claimed back.
            // Anything else — a `tracon service` unit, or one left running in
            // a terminal — is used and never stopped.
            return Ok(if self.claim(&state_dir()) {
                Ensured::Claimed
            } else {
                Ensured::Foreign
            });
        }
        self.spawn()
            .map_err(|e| format!("starting {}: {e}", binary()))?;
        self.wait_ready(http, url).await?;
        Ok(Ensured::Started)
    }

    async fn wait_ready(&self, http: &reqwest::Client, url: &str) -> Result<(), String> {
        let deadline = std::time::Instant::now() + READY_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if answering(http, url).await {
                return Ok(());
            }
            // A node that exited is not going to start answering.
            if let Some(code) = self.exited() {
                return Err(format!("the node exited with status {code}"));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        Err("the node did not start answering".into())
    }

    /// Replace the node with the binary at hand: stop it and start again.
    /// Only for a node this process owns; a foreign one is not ours to
    /// restart. Returns once the new one answers.
    pub async fn restart(&self, http: &reqwest::Client, url: &str) -> Result<(), String> {
        if !self.owned() {
            return Err("the node is not run by this app".into());
        }
        self.stop_process();
        self.spawn()
            .map_err(|e| format!("starting {}: {e}", binary()))?;
        self.wait_ready(http, url).await
    }

    /// The node's exit status, if it has exited. For a claimed node there is
    /// no status to collect, only whether the process is still there.
    fn exited(&self) -> Option<i32> {
        let mut guard = self.child.lock().unwrap();
        match guard.as_mut()? {
            Proc::Child(child) => match child.try_wait() {
                Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
                _ => None,
            },
            Proc::Adopted(pid) => (!process_alive(*pid)).then_some(-1),
        }
    }

    /// Watch the node we started and bring it back if it dies. Returns when the
    /// app is quitting, or when the node has failed too fast too often to be
    /// worth restarting.
    pub async fn supervise(self: Arc<Self>, http: reqwest::Client, url: String) -> Option<String> {
        let mut rapid = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if self.stopping.load(Ordering::SeqCst) || !self.owned() {
                return None;
            }
            let Some(code) = self.exited() else { continue };
            let started = std::time::Instant::now();
            if let Err(e) = self.spawn() {
                return Some(format!(
                    "the node stopped ({code}) and would not restart: {e}"
                ));
            }
            let mut ready = false;
            let deadline = std::time::Instant::now() + READY_TIMEOUT;
            while std::time::Instant::now() < deadline {
                if answering(&http, &url).await {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            rapid = if !ready || started.elapsed() < RAPID {
                rapid + 1
            } else {
                0
            };
            if rapid >= MAX_RAPID_RESTARTS {
                return Some(format!(
                    "the node has stopped {rapid} times in a row; leaving it down"
                ));
            }
        }
    }

    /// Stop the node, if this process owns it and no update asked for it to
    /// be left running. SIGTERM and wait: the node ends its sessions and
    /// removes their containers on the way out, so killing it outright leaves
    /// containers behind.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if self.handing_off() {
            return;
        }
        self.stop_process();
    }

    fn stop_process(&self) {
        if !self.owned() {
            return;
        }
        let mut guard = self.child.lock().unwrap();
        let Some(proc_) = guard.as_mut() else { return };
        let pid = match proc_ {
            Proc::Child(child) => {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    Handoff::clear(&state_dir());
                    return;
                }
                child.id()
            }
            Proc::Adopted(pid) => *pid,
        };
        // Shelling out for the signal is the same trade `node/src/service.rs`
        // makes; the liveness probe is the one place libc is used.
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        let deadline = std::time::Instant::now() + STOP_TIMEOUT;
        let mut gone = false;
        while std::time::Instant::now() < deadline {
            let done = match proc_ {
                Proc::Child(child) => !matches!(child.try_wait(), Ok(None)),
                Proc::Adopted(pid) => !process_alive(*pid),
            };
            if done {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if !gone {
            // It would not go. Better a killed node than an app that will not quit.
            match proc_ {
                Proc::Child(child) => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                Proc::Adopted(pid) => {
                    let _ = Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                }
            }
        }
        *guard = None;
        Handoff::clear(&state_dir());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_is_overridable_and_falls_back_to_the_path() {
        assert_eq!(
            binary_from(Some("/opt/tracon".into()), None, None),
            "/opt/tracon"
        );
        // Without an override and no install, it is the PATH name — never
        // empty, which would spawn the shell's own argv[0].
        assert_eq!(
            binary_from(
                None,
                Some("/nonexistent/app".into()),
                Some("/nonexistent".into())
            ),
            "tracon"
        );
        assert_eq!(binary_from(None, None, None), "tracon");
    }

    /// A bundle carries its own node, and that one wins over an install the
    /// operator may have left behind at an older version.
    #[test]
    fn the_sidecar_beside_the_executable_beats_the_install_location() {
        let dir =
            std::env::temp_dir().join(format!("tracon-wrapper-sidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bundle")).unwrap();
        std::fs::create_dir_all(dir.join("home/.local/bin")).unwrap();
        std::fs::write(dir.join("bundle/tracon"), "").unwrap();
        std::fs::write(dir.join("home/.local/bin/tracon"), "").unwrap();
        let exe = Some(dir.join("bundle/tracon-wrapper"));
        let home = Some(dir.join("home"));
        let got = binary_from(None, exe.clone(), home.clone());
        assert_eq!(got, dir.join("bundle/tracon").to_string_lossy());
        std::fs::remove_file(dir.join("bundle/tracon")).unwrap();
        let got = binary_from(None, exe, home);
        assert_eq!(got, dir.join("home/.local/bin/tracon").to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_path_gains_the_package_manager_prefixes_once() {
        let got = enriched_path(Some("/usr/bin:/bin".into()));
        assert_eq!(got, "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin");
        // Already present: untouched, not appended again.
        assert_eq!(enriched_path(Some(got.clone())), got);
        assert_eq!(enriched_path(None), "/opt/homebrew/bin:/usr/local/bin");
    }

    /// Stopping a node this process did not start would kill something the
    /// operator's service manager owns.
    #[test]
    fn a_node_it_did_not_start_is_never_stopped() {
        let n = Node::default();
        assert!(!n.owned());
        n.stop();
        assert!(n.child.lock().unwrap().is_none());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tracon-wrapper-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_handoff_record_round_trips_and_clears() {
        let dir = scratch("handoff");
        assert!(Handoff::read(&dir).is_none());
        let record = Handoff {
            pid: 4242,
            binary: "/opt/tracon".into(),
        };
        record.write(&dir).unwrap();
        assert_eq!(Handoff::read(&dir), Some(record));
        Handoff::clear(&dir);
        assert!(Handoff::read(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record naming a process that is gone is stale and is not claimed;
    /// one naming a live process is, and the node is then owned.
    #[test]
    fn a_stale_record_is_not_claimed_and_a_live_one_is() {
        let dir = scratch("claim");
        let mut sleeper = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = sleeper.id();
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        Handoff {
            pid,
            binary: "x".into(),
        }
        .write(&dir)
        .unwrap();
        let n = Node::default();
        assert!(!n.claim(&dir));
        assert!(!n.owned());
        assert!(Handoff::read(&dir).is_none(), "a stale record is cleared");

        let mut sleeper = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        Handoff {
            pid: sleeper.id(),
            binary: "x".into(),
        }
        .write(&dir)
        .unwrap();
        let n = Node::default();
        assert!(n.claim(&dir));
        assert!(n.owned());
        assert!(n.exited().is_none());
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert_eq!(n.exited(), Some(-1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An update about to restart the app leaves the node for the next run.
    #[test]
    fn a_handoff_leaves_the_child_running() {
        let mut sleeper = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = sleeper.id();
        let n = Node::default();
        *n.child.lock().unwrap() = Some(Proc::Adopted(pid));
        n.owned.store(true, Ordering::SeqCst);
        n.hand_off();
        n.stop();
        assert!(process_alive(pid), "handed off, so still running");
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    #[test]
    fn a_restart_is_owed_only_when_both_versions_are_known_and_differ() {
        assert!(needs_restart(Some("0.12.2"), Some("0.13.0")));
        assert!(!needs_restart(Some("0.13.0"), Some("0.13.0")));
        assert!(!needs_restart(None, Some("0.13.0")));
        assert!(!needs_restart(Some("0.13.0"), None));
        assert_eq!(
            parse_version_output("tracon 0.13.1\n"),
            Some("0.13.1".into())
        );
        assert_eq!(parse_version_output(""), None);
        assert_eq!(parse_version_output("tracon: command not found"), None);
    }
}
