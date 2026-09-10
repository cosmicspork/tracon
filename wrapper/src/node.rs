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
#[derive(Debug, Deserialize)]
struct Handoff {
    pid: u32,
}

fn handoff_path(state_dir: &Path) -> PathBuf {
    state_dir.join("desktop-node.json")
}

/// Whether a process with this pid exists. Signal 0 delivers nothing and
/// only asks; a pid that is gone answers ESRCH, one owned by someone else
/// answers EPERM — and is therefore alive.
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The pid of a node an earlier version of this app spawned, while it is
/// still running. A record naming a process that is gone is removed.
pub fn migrated_node(state_dir: &Path) -> Option<u32> {
    let bytes = std::fs::read(handoff_path(state_dir)).ok()?;
    let record: Handoff = serde_json::from_slice(&bytes).ok()?;
    if process_alive(record.pid) {
        Some(record.pid)
    } else {
        let _ = std::fs::remove_file(handoff_path(state_dir));
        None
    }
}

/// Stop that node so the service can take its port: SIGTERM, then wait for
/// it to end its sessions.
pub fn stop_migrated_node(state_dir: &Path) {
    let Some(pid) = migrated_node(state_dir) else {
        return;
    };
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline && process_alive(pid) {
        std::thread::sleep(Duration::from_millis(200));
    }
    if process_alive(pid) {
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

    #[test]
    fn a_stale_handoff_record_is_ignored_and_a_live_one_is_found() {
        let dir = scratch("handoff");
        assert_eq!(migrated_node(&dir), None);
        let mut sleeper = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = sleeper.id();
        std::fs::write(
            handoff_path(&dir),
            format!(r#"{{"pid":{pid},"binary":"/opt/tracon"}}"#),
        )
        .unwrap();
        assert_eq!(migrated_node(&dir), Some(pid));
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert_eq!(migrated_node(&dir), None);
        assert!(!handoff_path(&dir).exists(), "a stale record is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stopping_a_migrated_node_ends_it_and_clears_the_record() {
        let dir = scratch("stop");
        let mut sleeper = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = sleeper.id();
        std::fs::write(handoff_path(&dir), format!(r#"{{"pid":{pid}}}"#)).unwrap();
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
    fn the_version_is_read_from_what_the_binary_prints() {
        assert_eq!(
            parse_version_output("tracon 0.13.1\n"),
            Some("0.13.1".into())
        );
        assert_eq!(parse_version_output(""), None);
        assert_eq!(parse_version_output("tracon: command not found"), None);
    }
}
