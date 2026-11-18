//! The node as this app sees it: a process the user service runs, which the
//! app installs, updates and talks to, and never runs itself.
//!
//! Earlier versions spawned the node as the app's own child and handed it
//! across the app's updates with a record beside the node's state. A node
//! started that way may still be running when this version first launches;
//! the record identifies it, so it can be stopped before the service takes
//! the port.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// How long to wait for a node to answer after the service starts it.
/// Generous: the first start of the day opens the store, runs migrations, and
/// verifies the boundary before it listens.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a stop may take. The node ends its sessions and removes their
/// containers on SIGTERM, and cutting that short leaks containers.
const STOP_TIMEOUT: Duration = Duration::from_secs(90);

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

/// The record an earlier version of this app left for a node it spawned.
///
/// A pid is not an identity: the kernel hands the number back out, so a record
/// left by a node that died can name a stranger, and adopting one blocks setup
/// behind it while stopping one sends it SIGTERM and then SIGKILL. The start
/// time is what ties the record to a process — it is fixed for that process's
/// life and never reused — and the binary is a second check on top.
///
/// This mirrors `tracon::process::Record` in the node crate, where the same
/// logic is tested; the copy exists because this runs before the app has
/// installed the CLI that carries that crate. Keep the two in agreement.
#[derive(Debug, Deserialize)]
struct Handoff {
    pid: u32,
    /// Absent in records written before start times were recorded. Such a
    /// record is refused: nothing in it separates the node it named from
    /// whatever holds that pid now.
    #[serde(default)]
    started: Option<String>,
    #[serde(default)]
    exe: Option<String>,
}

fn handoff_path(state_dir: &Path) -> PathBuf {
    state_dir.join("desktop-node.json")
}

/// The start time of a process, as an opaque token: only ever compared for
/// equality. Linux reads field 22 of `/proc/<pid>/stat`, counted from the last
/// `)` because the executable name in field 2 may hold spaces and parentheses.
#[cfg(target_os = "linux")]
fn started(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(19).map(str::to_string)
}

/// macOS has no `/proc`; `ps` reports the start time to the second.
#[cfg(not(target_os = "linux"))]
fn started(pid: u32) -> Option<String> {
    ps_field(pid, "lstart=")
}

#[cfg(target_os = "linux")]
fn exe(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn exe(pid: u32) -> Option<String> {
    ps_field(pid, "comm=")
}

#[cfg(not(target_os = "linux"))]
fn ps_field(pid: u32, field: &str) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Whether the record still names the process it was written for, read from
/// the system now. The reason is for saying out loud why a record was refused.
fn still_that_process(record: &Handoff) -> Result<u32, String> {
    if record.pid == 0 {
        return Err("the record names no process".into());
    }
    let Some(recorded) = record.started.as_deref() else {
        return Err(
            "the record carries no start time, so nothing ties it to a running process".into(),
        );
    };
    let Some(found) = started(record.pid) else {
        return Err(format!("no process {} is running", record.pid));
    };
    if found != recorded {
        return Err(format!(
            "pid {} was reused: it started at {found}, the record at {recorded}",
            record.pid
        ));
    }
    if let Some(recorded_exe) = record.exe.as_deref() {
        let running = exe(record.pid);
        if running.as_deref() != Some(recorded_exe) {
            return Err(format!(
                "pid {} is running {}, not {recorded_exe}",
                record.pid,
                running
                    .as_deref()
                    .unwrap_or("something this app cannot see")
            ));
        }
    }
    Ok(record.pid)
}

/// The record for a node an earlier version of this app spawned, while it
/// still names that process. One that does not — gone, reused, or too old to
/// carry a start time — is refused out loud and removed.
fn verified_handoff(state_dir: &Path) -> Option<Handoff> {
    let path = handoff_path(state_dir);
    let bytes = std::fs::read(&path).ok()?;
    let refuse = |why: String| {
        eprintln!("tracon: ignoring {}: {why}", path.display());
        let _ = std::fs::remove_file(&path);
        None
    };
    let record: Handoff = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(e) => return refuse(format!("the record is unreadable ({e})")),
    };
    match still_that_process(&record) {
        Ok(_) => Some(record),
        Err(why) => refuse(why),
    }
}

/// The pid of a node an earlier version of this app spawned, while it is
/// still running and still that node.
pub fn migrated_node(state_dir: &Path) -> Option<u32> {
    verified_handoff(state_dir).map(|record| record.pid)
}

/// Stop that node so the service can take its port: SIGTERM, then wait for
/// it to end its sessions. Each wait re-checks the identity, so a pid the
/// kernel re-issues while the node is shutting down is never the one that
/// gets SIGKILL.
pub fn stop_migrated_node(state_dir: &Path) {
    let Some(record) = verified_handoff(state_dir) else {
        return;
    };
    let pid = record.pid;
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline && still_that_process(&record).is_ok() {
        std::thread::sleep(Duration::from_millis(200));
    }
    if still_that_process(&record).is_ok() {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    let _ = std::fs::remove_file(handoff_path(state_dir));
}

/// What `tracon --version` prints, reduced to the version.
pub fn parse_version_output(out: &str) -> Option<String> {
    out.split_whitespace()
        .last()
        .filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

pub fn version_of(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    parse_version_output(&String::from_utf8_lossy(&out.stdout))
}

/// The node binary this app carries: beside the executable in a bundle (an
/// AppImage's `usr/bin`, a `.app`'s `Contents/MacOS`), or `TRACON_BIN`.
pub fn sidecar_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TRACON_BIN") {
        return Some(PathBuf::from(p));
    }
    let p = std::env::current_exe().ok()?.parent()?.join("tracon");
    p.is_file().then_some(p)
}

/// Where the CLI is installed and what the service runs: the location
/// `install.sh` uses too.
pub fn installed_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin/tracon"))
}

/// Copy the node binary this app carries to where the CLI and the service
/// run it from.
pub fn install_cli() -> Result<PathBuf, String> {
    let from = sidecar_path().ok_or("this build of the app carries no node binary")?;
    let to = installed_path().ok_or("HOME is not set")?;
    copy_executable(&from, &to)?;
    Ok(to)
}

fn copy_executable(from: &Path, to: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = to.parent().ok_or("nowhere to install to")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    // Staged beside the target: the rename cannot cross a volume, and a
    // service starting at that moment never runs a half-written file.
    let staged = dir.join(format!(".tracon.install-{}", std::process::id()));
    let put = || -> std::io::Result<()> {
        std::fs::copy(from, &staged)?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        unquarantine(&staged);
        std::fs::rename(&staged, to)
    };
    put().map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("installing {}: {e}", to.display())
    })
}

/// A file copied out of a downloaded app keeps its quarantine flag, and
/// Gatekeeper may refuse to run a quarantined unsigned binary.
fn unquarantine(path: &Path) {
    if cfg!(target_os = "macos") {
        let _ = Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(path)
            .stderr(Stdio::null())
            .status();
    }
}

/// Launched from Finder, this process carries launchd's minimal PATH, and the
/// tools the CLI calls (launchctl aside: podman, gh, glab) live in the
/// package-manager prefixes a login shell would have added.
pub fn enriched_path(current: Option<String>) -> String {
    let mut path = current.unwrap_or_default();
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let present = std::env::split_paths(&path).any(|p| p == Path::new(dir));
        if !present {
            if !path.is_empty() {
                path.push(':');
            }
            path.push_str(dir);
        }
    }
    path
}

fn on_path(path_var: &str, dir: &Path) -> bool {
    std::env::split_paths(path_var).any(|p| p == dir)
}

/// The line that puts `dir` on a terminal's PATH, when the login shell does
/// not have it. This process's own PATH is launchd's, which says nothing
/// about a terminal, so the shell is asked.
pub async fn path_hint(dir: &Path) -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let asked = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(&shell)
            .args(["-lic", "printf '\\n%s' \"$PATH\""])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output(),
    )
    .await;
    let path = match asked {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .last()
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    };
    (!on_path(&path, dir)).then(|| format!("export PATH=\"{}:$PATH\"", dir.display()))
}

pub async fn answering(http: &reqwest::Client, url: &str) -> bool {
    http.get(format!("{url}/api/node"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// What holds a restart of the node up: the work this node is doing itself.
/// Sessions on other mesh nodes, external harnesses, and sessions idle on the
/// operator are not in it; a restart cuts none of them off mid-turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Busy {
    /// Sessions on this node with a turn running.
    pub turns: usize,
    /// Required checks running.
    pub checks: usize,
    /// Review submissions whose checks have not all run.
    pub reviews: usize,
    /// This node's managed sessions whose turn cannot be seen (the queue of a
    /// node too old to say what it is doing): any of them may be running one.
    pub sessions: usize,
}

impl Busy {
    pub fn idle(&self) -> bool {
        self.turns == 0 && self.checks == 0 && self.reviews == 0 && self.sessions == 0
    }

    /// What is being waited for, in the tray's words: "1 turn on this node",
    /// "2 checks", joined.
    pub fn describe(&self) -> String {
        let count = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
        let mut parts = Vec::new();
        if self.turns > 0 {
            parts.push(format!("{} on this node", count(self.turns, "turn")));
        }
        if self.sessions > 0 {
            parts.push(format!("{} on this node", count(self.sessions, "session")));
        }
        if self.checks > 0 {
            parts.push(count(self.checks, "check"));
        }
        if self.reviews > 0 {
            parts.push(count(self.reviews, "review submission"));
        }
        match parts.split_last() {
            None => "nothing".into(),
            Some((last, [])) => last.clone(),
            Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        }
    }
}

/// Whether the node can be restarted now, and what restarting it anyway
/// would end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Holdup {
    pub busy: Busy,
    /// The live sessions a restart ends, labelled for the tray, as the node's
    /// own restart lists them.
    pub closing: Vec<String>,
}

/// What holds a restart of the node up, if it answers. None when it does
/// not: a node that cannot be asked is not idle.
///
/// The node says itself, in its restart status (`GET /api/restart`), what
/// its own `when_idle` restart waits for; that is the one definition of busy
/// used. A node too old to say is counted from its queue instead
/// ([`queue_holdup`]).
pub async fn holdup(http: &reqwest::Client, url: &str) -> Option<Holdup> {
    let res = http
        .get(format!("{url}/api/restart"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    let code = res.status();
    if code.is_success() {
        let status = res.json::<serde_json::Value>().await.ok();
        if let Some(holdup) = status.as_ref().and_then(restart_holdup) {
            return Some(holdup);
        }
    } else if code != reqwest::StatusCode::NOT_FOUND {
        return None;
    }
    let node = get_json(http, &format!("{url}/api/node"))
        .await
        .and_then(|v| v["id"].as_str().map(str::to_string));
    let queue = get_json(http, &format!("{url}/api/queue")).await?;
    Some(queue_holdup(&queue, node.as_deref()))
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<serde_json::Value> {
    let res = http
        .get(url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.json().await.ok()
}

/// The holdup from the node's restart status, if it carries `busy`.
pub fn restart_holdup(status: &serde_json::Value) -> Option<Holdup> {
    let busy = status.get("busy")?.as_object()?;
    let n = |key: &str| busy.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    Some(Holdup {
        busy: Busy {
            turns: busy
                .get("turns")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len),
            checks: n("checks"),
            reviews: n("reviews"),
            sessions: 0,
        },
        closing: status["closing"]
            .as_array()
            .map(|a| a.iter().map(session_label).collect())
            .unwrap_or_default(),
    })
}

/// The holdup an older node's queue implies. The queue lists the whole mesh,
/// so only this node's own sessions count (`node`, from `/api/node`; when it
/// cannot be read, every session does), and external harnesses, which only
/// lose their MCP connection over a restart, never do. A session idle on the
/// operator does not count either; one whose row does not say whether a turn
/// is running does.
pub fn queue_holdup(queue: &serde_json::Value, node: Option<&str>) -> Holdup {
    let own: Vec<&serde_json::Value> = queue["running"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|s| s["state"] != "closed" && s["harness_id"] != "external")
                .filter(|s| node.is_none_or(|node| s["node_id"] == node))
                .collect()
        })
        .unwrap_or_default();
    let mut busy = Busy::default();
    for s in &own {
        let turn = &s["turn_active"];
        match turn.as_i64().or_else(|| turn.as_bool().map(i64::from)) {
            Some(0) => {}
            Some(_) => busy.turns += 1,
            None => busy.sessions += 1,
        }
    }
    Holdup {
        busy,
        closing: own
            .iter()
            .filter(|s| s["state"] != "suspended")
            .map(|s| session_label(s))
            .collect(),
    }
}

/// A session as the tray names it: its branch, else its repository, else
/// its id, and what state it is in.
fn session_label(s: &serde_json::Value) -> String {
    let text = |key: &str| s[key].as_str().filter(|t| !t.is_empty());
    let name = text("branch")
        .map(str::to_string)
        .or_else(|| {
            text("repo_path").map(|p| {
                Path::new(p)
                    .file_name()
                    .map_or(p.to_string(), |f| f.to_string_lossy().into_owned())
            })
        })
        .or_else(|| text("id").map(|id| id.chars().take(8).collect()))
        .unwrap_or_else(|| "a session".into());
    match text("state") {
        Some(state) => format!("{name} · {}", state.replace('_', " ")),
        None => name,
    }
}

/// Whether the node refuses to run harnesses because its boundary images are
/// missing or were built from other definitions — the one refusal a `tracon
/// setup` fixes on its own. Asks the node to re-check, so the answer is now's.
pub async fn images_stale(http: &reqwest::Client, url: &str) -> bool {
    let refused = async {
        let v: serde_json::Value = http
            .get(format!("{url}/api/node"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        Some(v["state"] == "refused")
    };
    if refused.await != Some(true) {
        return false;
    }
    let checked: Option<serde_json::Value> = async {
        http.post(format!("{url}/api/boundary/check"))
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()
    }
    .await;
    checked
        .and_then(|v| v["checks"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .any(|c| {
            let detail = c["detail"].as_str().unwrap_or_default();
            c["ok"] == false
                && (detail.contains("other definitions") || detail.contains("run `tracon setup`"))
        })
}

/// Build what the boundary needs, as `tracon setup` does, through the node.
pub async fn run_setup(http: &reqwest::Client, url: &str) -> Result<(), String> {
    let res = http
        .post(format!("{url}/api/boundary/setup"))
        .json(&serde_json::json!({ "rebuild": false }))
        .timeout(Duration::from_secs(660))
        .send()
        .await
        .map_err(|e| format!("setting up the boundary: {e}"))?;
    if res.status().is_success() {
        Ok(())
    } else {
        let body = res.text().await.unwrap_or_default();
        Err(format!("setting up the boundary failed: {body}"))
    }
}

/// What an app that just updated owes the machine, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Put the node binary this app carries where the CLI and the unit are.
    InstallCli,
    /// Restart the service onto it, once nothing is running.
    RestartService,
}

/// Before the service is installed the setup page owns all of this. After, a
/// CLI that is not the one the app carries is replaced, and a running node on
/// another version is restarted onto it. A node that is not answering is a
/// different problem, and restarting it is not the fix.
pub fn plan(
    running: Option<&str>,
    carried: &str,
    installed: Option<&str>,
    service_installed: bool,
) -> Vec<Step> {
    let mut steps = Vec::new();
    if !service_installed {
        return steps;
    }
    if installed != Some(carried) {
        steps.push(Step::InstallCli);
    }
    if running.is_some_and(|r| r != carried) {
        steps.push(Step::RestartService);
    }
    steps
}

/// Wait for a node the service is starting to answer.
pub async fn wait_ready(http: &reqwest::Client, url: &str) -> bool {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if answering(http, url).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    false
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tracon-wrapper-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn install_cli_copies_the_sidecar_and_marks_it_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("install");
        let from = dir.join("tracon-sidecar");
        std::fs::write(&from, "new").unwrap();
        let to = dir.join("home/.local/bin/tracon");
        copy_executable(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "new");
        let mode = std::fs::metadata(&to).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        // An older install is replaced in place, and nothing staged is left.
        std::fs::write(&from, "newer").unwrap();
        copy_executable(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "newer");
        let leftovers: Vec<_> = std::fs::read_dir(to.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".tracon.install")
            })
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sleeper() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// The record the app writes for a node it spawned: the pid, bound to that
    /// process by its start time.
    fn record_for(pid: u32) -> String {
        format!(
            r#"{{"pid":{pid},"started":"{}"}}"#,
            started(pid).expect("the process is running")
        )
    }

    #[test]
    fn a_stale_handoff_record_is_ignored_and_a_live_one_is_found() {
        let dir = scratch("handoff");
        assert_eq!(migrated_node(&dir), None);
        let mut sleeper = sleeper();
        let pid = sleeper.id();
        std::fs::write(handoff_path(&dir), record_for(pid)).unwrap();
        assert_eq!(migrated_node(&dir), Some(pid));
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert_eq!(migrated_node(&dir), None);
        assert!(!handoff_path(&dir).exists(), "a stale record is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_that_cannot_identify_its_process_is_refused_and_the_pid_left_alone() {
        let dir = scratch("unverifiable");
        let mut sleeper = sleeper();
        let pid = sleeper.id();
        // What earlier versions wrote: the number and nothing else. The pid
        // may be anyone's by now, so it is neither adopted nor signalled.
        std::fs::write(
            handoff_path(&dir),
            format!(r#"{{"pid":{pid},"binary":"/opt/tracon"}}"#),
        )
        .unwrap();
        assert_eq!(migrated_node(&dir), None);
        assert!(!handoff_path(&dir).exists());

        // And a record whose pid the kernel has since handed to something
        // else: the number is live, the process behind it is not the node.
        std::fs::write(
            handoff_path(&dir),
            format!(r#"{{"pid":{pid},"started":"not-when-it-started"}}"#),
        )
        .unwrap();
        assert_eq!(migrated_node(&dir), None);
        assert!(!handoff_path(&dir).exists());

        stop_migrated_node(&dir);
        assert!(
            started(pid).is_some(),
            "a refused record must not get the process behind that pid killed"
        );
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_naming_another_binary_is_refused() {
        let dir = scratch("binary");
        let mut sleeper = sleeper();
        let pid = sleeper.id();
        std::fs::write(
            handoff_path(&dir),
            format!(
                r#"{{"pid":{pid},"started":"{}","exe":"/nowhere/tracon"}}"#,
                started(pid).unwrap()
            ),
        )
        .unwrap();
        assert_eq!(migrated_node(&dir), None);
        assert!(!handoff_path(&dir).exists());
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stopping_a_migrated_node_ends_it_and_clears_the_record() {
        let dir = scratch("stop");
        let mut sleeper = sleeper();
        let pid = sleeper.id();
        std::fs::write(handoff_path(&dir), record_for(pid)).unwrap();
        let reaper = std::thread::spawn(move || sleeper.wait());
        stop_migrated_node(&dir);
        reaper.join().unwrap().unwrap();
        assert!(!handoff_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_path_gains_the_package_manager_prefixes_once() {
        let got = enriched_path(Some("/usr/bin:/bin".into()));
        assert_eq!(got, "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin");
        assert_eq!(enriched_path(Some(got.clone())), got);
        assert_eq!(enriched_path(None), "/opt/homebrew/bin:/usr/local/bin");
    }

    #[test]
    fn a_directory_is_on_the_path_only_as_a_whole_entry() {
        let dir = Path::new("/home/op/.local/bin");
        assert!(on_path("/usr/bin:/home/op/.local/bin", dir));
        assert!(!on_path("/usr/bin:/home/op/.local/bin/extra", dir));
        assert!(!on_path("", dir));
    }

    #[test]
    fn an_update_moves_the_cli_then_the_service_and_nothing_before_setup() {
        use Step::*;
        // Before the service exists, the setup page is in charge.
        assert!(plan(Some("0.14.0"), "0.15.0", Some("0.14.0"), false).is_empty());
        // Both behind: the CLI first, because the unit names it.
        assert_eq!(
            plan(Some("0.14.0"), "0.15.0", Some("0.14.0"), true),
            vec![InstallCli, RestartService]
        );
        // A CLI someone removed comes back; the node is already current.
        assert_eq!(plan(Some("0.15.0"), "0.15.0", None, true), vec![InstallCli]);
        assert!(plan(Some("0.15.0"), "0.15.0", Some("0.15.0"), true).is_empty());
        // A node that does not answer is not restarted by an update.
        assert_eq!(plan(None, "0.15.0", Some("0.14.0"), true), vec![InstallCli]);
    }

    fn session(id: &str, node: &str, harness: &str, state: &str, turn: i64) -> serde_json::Value {
        serde_json::json!({
            "id": id, "node_id": node, "harness_id": harness, "state": state,
            "turn_active": turn, "branch": format!("feat/{id}"), "repo_path": "/src/repo",
        })
    }

    #[test]
    fn only_this_nodes_own_turns_hold_a_restart_up_in_an_older_nodes_queue() {
        let queue = serde_json::json!({ "running": [
            // A peer's session, mirrored here: the mesh-wide queue lists it.
            session("peer", "bazzite", "claude", "running", 1),
            // An external harness only loses its MCP connection.
            session("ext", "mac", "external", "running", 1),
            // Idle on the operator: a restart cuts nothing off.
            session("idle", "mac", "claude", "waiting_on_you", 0),
            session("parked", "mac", "claude", "suspended", 0),
        ]});
        let h = queue_holdup(&queue, Some("mac"));
        assert!(h.busy.idle(), "{h:?}");
        // What restarting now would end: the live one, not the suspended one.
        assert_eq!(h.closing, vec!["feat/idle · waiting on you".to_string()]);

        let mut busy = queue.clone();
        busy["running"]
            .as_array_mut()
            .unwrap()
            .push(session("mine", "mac", "claude", "running", 1));
        let h = queue_holdup(&busy, Some("mac"));
        assert_eq!(
            h.busy,
            Busy {
                turns: 1,
                ..Busy::default()
            }
        );
        assert_eq!(h.busy.describe(), "1 turn on this node");

        // A closed session never counts.
        let closed = serde_json::json!({ "running": [session("c", "mac", "claude", "closed", 1)] });
        assert!(queue_holdup(&closed, Some("mac")).busy.idle());
    }

    #[test]
    fn without_the_nodes_id_every_managed_session_counts() {
        let queue = serde_json::json!({ "running": [
            session("peer", "bazzite", "claude", "running", 1),
            session("ext", "mac", "external", "running", 1),
        ]});
        assert_eq!(queue_holdup(&queue, None).busy.turns, 1);
    }

    #[test]
    fn a_row_that_does_not_say_whether_a_turn_runs_counts() {
        let mut row = session("old", "mac", "claude", "waiting_on_you", 0);
        row.as_object_mut().unwrap().remove("turn_active");
        let h = queue_holdup(&serde_json::json!({ "running": [row] }), Some("mac"));
        assert_eq!(h.busy.sessions, 1);
        assert_eq!(h.busy.describe(), "1 session on this node");
    }

    #[test]
    fn the_nodes_restart_status_is_read_for_what_it_waits_for() {
        let status = serde_json::json!({
            "busy": { "turns": ["s1"], "checks": 2, "reviews": 0 },
            "closing": [
                { "id": "s1", "branch": "feat/x", "state": "running" },
                { "id": "0193abcdef", "branch": null, "repo_path": "", "state": "waiting_on_you" },
            ],
        });
        let h = restart_holdup(&status).unwrap();
        assert_eq!(
            h.busy,
            Busy {
                turns: 1,
                checks: 2,
                ..Busy::default()
            }
        );
        assert_eq!(h.busy.describe(), "1 turn on this node and 2 checks");
        assert_eq!(
            h.closing,
            vec!["feat/x · running", "0193abcd · waiting on you"]
        );
        // A status without `busy` is an older node's; the queue is asked.
        assert_eq!(restart_holdup(&serde_json::json!({ "config": {} })), None);
    }

    #[test]
    fn what_holds_a_restart_up_is_said_in_a_line() {
        let b = Busy {
            turns: 2,
            checks: 1,
            reviews: 3,
            sessions: 0,
        };
        assert_eq!(
            b.describe(),
            "2 turns on this node, 1 check and 3 review submissions"
        );
        assert_eq!(
            Busy {
                checks: 2,
                ..Busy::default()
            }
            .describe(),
            "2 checks"
        );
    }

    /// A node that answers each path with what `routes` gives it, and records
    /// what it was asked.
    async fn fake_node(
        routes: Vec<(&'static str, u16, serde_json::Value)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0; 4096];
                let n = socket.read(&mut request).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..n]).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                seen.lock().unwrap().push(path.clone());
                let (code, body) = routes
                    .iter()
                    .find(|(p, _, _)| *p == path)
                    .map(|(_, c, b)| (*c, b.to_string()))
                    .unwrap_or((404, "{}".into()));
                let _ = socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        (url, asked)
    }

    fn busy_queue() -> serde_json::Value {
        serde_json::json!({ "running": [session("mine", "mac", "claude", "running", 1)] })
    }

    #[tokio::test]
    async fn the_nodes_own_word_on_busy_is_used_when_it_offers_it() {
        let (url, asked) = fake_node(vec![
            (
                "/api/restart",
                200,
                serde_json::json!({ "busy": { "turns": [], "checks": 0, "reviews": 0 }, "closing": [] }),
            ),
            ("/api/node", 200, serde_json::json!({ "id": "mac" })),
            ("/api/queue", 200, busy_queue()),
        ])
        .await;
        let h = holdup(&reqwest::Client::new(), &url).await.unwrap();
        assert!(h.busy.idle(), "{h:?}");
        assert_eq!(*asked.lock().unwrap(), vec!["/api/restart".to_string()]);
    }

    #[tokio::test]
    async fn an_older_node_is_counted_from_its_queue() {
        let (url, asked) = fake_node(vec![
            ("/api/node", 200, serde_json::json!({ "id": "mac" })),
            ("/api/queue", 200, busy_queue()),
        ])
        .await;
        let h = holdup(&reqwest::Client::new(), &url).await.unwrap();
        assert_eq!(h.busy.turns, 1, "{h:?}");
        assert!(asked.lock().unwrap().contains(&"/api/queue".to_string()));
    }

    #[tokio::test]
    async fn a_node_that_does_not_answer_is_not_idle() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(holdup(&reqwest::Client::new(), &url).await, None);
        // Nor one that answers with an error.
        let (url, _) = fake_node(vec![("/api/restart", 500, serde_json::json!({}))]).await;
        assert_eq!(holdup(&reqwest::Client::new(), &url).await, None);
    }

    #[test]
    fn the_version_is_read_from_what_the_binary_prints() {
        assert_eq!(
            parse_version_output("tracon 0.13.1\n"),
            Some("0.13.1".into())
        );
        assert_eq!(parse_version_output(""), None);
        assert_eq!(parse_version_output("tracon: command not found"), None);
    }
}
