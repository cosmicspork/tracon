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
