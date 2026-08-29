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
//! already answering is left alone and simply used.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

#[derive(Default)]
pub struct Node {
    child: Mutex<Option<Child>>,
    /// Whether this process started the node. Nothing else may be stopped: a
    /// node found already running belongs to whoever started it.
    owned: AtomicBool,
    stopping: AtomicBool,
}

/// Where the node's binary is. `TRACON_BIN` wins; otherwise the install
/// location `install.sh` and `cargo install` both use, then the PATH.
fn binary() -> String {
    if let Ok(p) = std::env::var("TRACON_BIN") {
        return p;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = std::path::Path::new(&home).join(".local/bin/tracon");
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    "tracon".into()
}

async fn answering(http: &reqwest::Client, url: &str) -> bool {
    http.get(format!("{url}/api/node"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

impl Node {
    /// Whether this process is responsible for the node's lifetime.
    pub fn owned(&self) -> bool {
        self.owned.load(Ordering::SeqCst)
    }

    fn spawn(&self) -> std::io::Result<()> {
        // stdout and stderr are inherited on purpose: run the app from a
        // terminal and the node's log is right there, which is the whole
        // debugging story for a wrapper that owns a child process.
        let child = Command::new(binary())
            .arg("serve")
            .stdin(Stdio::null())
            .spawn()?;
        *self.child.lock().unwrap() = Some(child);
        self.owned.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Adopt a running node, or start one. Returns once it answers.
    pub async fn ensure(self: &Arc<Self>, http: &reqwest::Client, url: &str) -> Result<(), String> {
        if answering(http, url).await {
            // Somebody else's node — a `tracon service` unit, or one left
            // running in a terminal. Use it; never stop it.
            return Ok(());
        }
        self.spawn()
            .map_err(|e| format!("starting {}: {e}", binary()))?;
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

    /// The child's exit status, if it has exited.
    fn exited(&self) -> Option<i32> {
        let mut guard = self.child.lock().unwrap();
        let child = guard.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
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

    /// Stop the node, if this process started it. SIGTERM and wait: the node
    /// ends its sessions and removes their containers on the way out, so
    /// killing it outright leaves containers behind.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if !self.owned() {
            return;
        }
        let mut guard = self.child.lock().unwrap();
        let Some(child) = guard.as_mut() else { return };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        // Shelling out beats a libc dependency and an unsafe block for one
        // signal, which is the same trade `node/src/service.rs` makes.
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
        let deadline = std::time::Instant::now() + STOP_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(_) => return,
            }
        }
        // It would not go. Better a killed node than an app that will not quit.
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_is_overridable_and_falls_back_to_the_path() {
        std::env::set_var("TRACON_BIN", "/opt/tracon");
        assert_eq!(binary(), "/opt/tracon");
        std::env::remove_var("TRACON_BIN");
        // Without an override it is either the install location or the PATH
        // name — never empty, which would spawn the shell's own argv[0].
        assert!(!binary().is_empty());
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
}
