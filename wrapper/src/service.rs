//! The node's user service, as `tracon service install` writes it. The app
//! asks whether it is there and running, and installs or restarts it through
//! the CLI it installed, so the unit is the node's own and names that binary.

use std::path::{Path, PathBuf};
use std::process::Command;

const MAC_LABEL: &str = "com.tracon.node";
const LINUX_UNIT: &str = "tracon.service";

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn unit_path() -> Option<PathBuf> {
    let home = home()?;
    Some(if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents")
            .join(format!("{MAC_LABEL}.plist"))
    } else {
        home.join(".config/systemd/user").join(LINUX_UNIT)
    })
}

pub fn installed() -> bool {
    unit_path().is_some_and(|p| p.exists())
}

/// What the supervisor says about the unit right now.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UnitState {
    /// Active, with the node running.
    pub running: bool,
    /// The unit has a process of its own: running, or still starting or
    /// stopping across a restart. A node that answers while this holds is the
    /// unit's; one that answers while it does not is someone else's.
    pub has_process: bool,
    /// The node keeps exiting: systemd has given up (`failed`) or is waiting to
    /// start it again after it exited (`auto-restart`); launchd is not
    /// running it and its last exit was not clean.
    pub failing: bool,
}

/// `systemctl --user show` output for `ActiveState`, `SubState` and `MainPID`.
fn parse_systemd_show(text: &str) -> UnitState {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_default()
    };
    let active = field("ActiveState");
    let sub = field("SubState");
    let pid = field("MainPID").parse::<u32>().unwrap_or(0);
    UnitState {
        running: active == "active",
        has_process: pid != 0,
        failing: active == "failed" || (active == "activating" && sub.starts_with("auto-restart")),
    }
}

/// `launchctl print gui/<uid>/<label>` output.
fn parse_launchctl_print(text: &str) -> UnitState {
    let lines = || text.lines().map(str::trim);
    let running = lines().any(|l| l == "state = running");
    let pid = lines()
        .find_map(|l| l.strip_prefix("pid = ")?.parse::<u32>().ok())
        .unwrap_or(0);
    // "(never exited)" parses as nothing, which is not a failure.
    let last_exit = lines().find_map(|l| {
        l.strip_prefix("last exit code = ")
            .or_else(|| l.strip_prefix("last exit status = "))?
            .parse::<i64>()
            .ok()
    });
    UnitState {
        running,
        has_process: running && pid != 0,
        failing: !running && last_exit.is_some_and(|code| code != 0),
    }
}

pub fn state() -> UnitState {
    if cfg!(target_os = "macos") {
        let uid = unsafe { libc::getuid() };
        Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/{MAC_LABEL}")])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_launchctl_print(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default()
    } else {
        Command::new("systemctl")
            .args([
                "--user",
                "show",
                LINUX_UNIT,
                "--property=ActiveState",
                "--property=SubState",
                "--property=MainPID",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_systemd_show(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default()
    }
}

/// A point in the service's log, so what the node said after it can be told
/// from what it said on some earlier run.
#[derive(Debug, Clone, Copy)]
pub struct LogMark {
    /// Unix seconds, for `journalctl --since`.
    unix_secs: u64,
    /// Bytes already in the launchd log file.
    offset: u64,
}

/// Where launchd writes the node's output: the plist `tracon service install`
/// writes sends both streams there.
fn mac_log_path() -> Option<PathBuf> {
    home().map(|h| h.join("Library/Logs/tracon.log"))
}

pub fn log_mark() -> LogMark {
    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(1))
        .unwrap_or(0);
    let offset = if cfg!(target_os = "macos") {
        mac_log_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0)
    } else {
        0
    };
    LogMark { unix_secs, offset }
}

/// The last lines the node wrote under the service, since `mark` when given.
fn recent_log(mark: Option<LogMark>) -> String {
    if cfg!(target_os = "macos") {
        use std::io::{Read, Seek, SeekFrom};
        let Some(mut file) = mac_log_path().and_then(|p| std::fs::File::open(p).ok()) else {
            return String::new();
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        const TAIL: u64 = 16 * 1024;
        let from = match mark {
            // A file shorter than the mark was rotated: all of it is new.
            Some(m) if m.offset <= len => m.offset.max(len.saturating_sub(TAIL)),
            _ => len.saturating_sub(TAIL),
        };
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(from)).is_err() || file.read_to_end(&mut bytes).is_err() {
            return String::new();
        }
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        // `_SYSTEMD_USER_UNIT` is what journald stamps on the process's own
        // output; `-u` would add systemd's lines about the unit, and the last
        // of those is always "Failed with result 'exit-code'".
        let mut cmd = Command::new("journalctl");
        cmd.args([
            "--user",
            &format!("_SYSTEMD_USER_UNIT={LINUX_UNIT}"),
            "-n",
            "40",
            "-o",
            "cat",
            "--no-pager",
        ]);
        if let Some(m) = mark {
            cmd.arg(format!("--since=@{}", m.unix_secs));
        }
        cmd.output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }
}

/// Terminal colour sequences: the node's log lines carry them.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

const MAX_REASON: usize = 400;

/// Why the node last stopped, read from what it wrote: the `Error:` a failed
/// command exits with (and its causes), else its last `ERROR` line or panic.
/// With `fallback`, the last line of any kind when neither is there — only
/// for a unit known to be failing, where the last thing said is the best
/// guess; otherwise it would be an ordinary log line presented as a reason.
fn reason_from_log(log: &str, fallback: bool) -> Option<String> {
    let lines: Vec<String> = log
        .lines()
        .map(|l| strip_ansi(l).trim_end().to_string())
        .collect();
    let reason = if let Some(i) = lines.iter().rposition(|l| l.starts_with("Error: ")) {
        let mut parts = vec![lines[i]["Error: ".len()..].trim().to_string()];
        for line in &lines[i + 1..] {
            if line.trim().is_empty() || line == "Caused by:" {
                continue;
            }
            // Causes are indented; anything else is a later run talking.
            if !line.starts_with(char::is_whitespace) {
                break;
            }
            let line = line.trim();
            // anyhow numbers its causes: `0: …`.
            let cause = line
                .split_once(": ")
                .filter(|(n, _)| n.chars().all(|c| c.is_ascii_digit()))
                .map_or(line, |(_, rest)| rest);
            parts.push(cause.to_string());
        }
        parts.join(": ")
    } else if let Some(line) = lines
        .iter()
        .rev()
        .find(|l| l.contains(" ERROR ") || l.contains("panicked at"))
    {
        line.trim().to_string()
    } else if fallback {
        lines
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())?
            .trim()
            .to_string()
    } else {
        return None;
    };
    if reason.is_empty() {
        return None;
    }
    Some(match reason.char_indices().nth(MAX_REASON) {
        Some((cut, _)) => format!("{}…", &reason[..cut]),
        None => reason,
    })
}

/// Why the unit is failing, when it is.
pub fn failure(state: UnitState) -> Option<String> {
    state
        .failing
        .then(|| reason_from_log(&recent_log(None), true))
        .flatten()
}

/// The reason the node gave for exiting since `mark`, if it gave one.
pub fn error_since(mark: LogMark) -> Option<String> {
    reason_from_log(&recent_log(Some(mark)), false)
}

/// `why`, followed by what the node last said about it.
pub fn explain(why: String, reason: Option<String>) -> String {
    match reason {
        Some(reason) => format!("{why}: {reason}"),
        None => why,
    }
}

pub fn install(cli: &Path) -> Result<(), String> {
    run(cli, &["service", "install"])
}

pub fn restart(cli: &Path) -> Result<(), String> {
    run(cli, &["service", "restart"])
}

fn run(cli: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new(cli)
        .args(args)
        .env(
            "PATH",
            crate::node::enriched_path(std::env::var("PATH").ok()),
        )
        .output()
        .map_err(|e| format!("running {}: {e}", cli.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let said = if err.trim().is_empty() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        err.trim().to_string()
    };
    Err(format!("tracon {}: {said}", args.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_says_failing_while_it_waits_to_restart_a_node_that_exited() {
        let crash_loop =
            parse_systemd_show("ActiveState=activating\nSubState=auto-restart\nMainPID=0\n");
        assert_eq!(
            crash_loop,
            UnitState {
                running: false,
                has_process: false,
                failing: true
            }
        );
        assert!(parse_systemd_show("ActiveState=failed\nSubState=failed\nMainPID=0\n").failing);
        // Newer systemd splits the wait in two.
        assert!(
            parse_systemd_show("ActiveState=activating\nSubState=auto-restart-queued\nMainPID=0")
                .failing
        );

        let healthy = parse_systemd_show("ActiveState=active\nSubState=running\nMainPID=4321\n");
        assert_eq!(
            healthy,
            UnitState {
                running: true,
                has_process: true,
                failing: false
            }
        );
        // Mid-restart: the old node is still stopping, and it is the unit's.
        let stopping =
            parse_systemd_show("ActiveState=deactivating\nSubState=stop-sigterm\nMainPID=4321\n");
        assert!(stopping.has_process && !stopping.running && !stopping.failing);
        // Stopped on purpose is not failing.
        assert_eq!(
            parse_systemd_show("ActiveState=inactive\nSubState=dead\nMainPID=0\n"),
            UnitState::default()
        );
    }

    #[test]
    fn launchd_says_failing_when_the_last_exit_was_not_clean() {
        let running = parse_launchctl_print(
            "com.tracon.node = {\n\tstate = running\n\tpid = 812\n\tlast exit code = (never exited)\n}",
        );
        assert!(running.running && running.has_process && !running.failing);
        let crashed = parse_launchctl_print(
            "com.tracon.node = {\n\tstate = not running\n\tlast exit code = 1\n}",
        );
        assert!(!crashed.running && !crashed.has_process && crashed.failing);
        let stopped = parse_launchctl_print(
            "com.tracon.node = {\n\tstate = not running\n\tlast exit code = 0\n}",
        );
        assert!(!stopped.failing);
    }

    /// What the journal held while the node crash-looped on a retired harness,
    /// colours and all.
    #[test]
    fn the_reason_is_the_error_the_node_exited_with() {
        let log = "\u{1b}[2m2026-09-14T16:22:29Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m tracon::http: opened store\n\
                   Error: the `omp` harness was removed at the OpenCode cutover.\n\
                   \u{1b}[2m2026-09-14T16:22:35Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m tracon::http: opened store\n\
                   Error: the `omp` harness was removed at the OpenCode cutover.\n\n";
        assert_eq!(
            reason_from_log(log, false).as_deref(),
            Some("the `omp` harness was removed at the OpenCode cutover.")
        );

        let caused =
            "Error: open store\n\nCaused by:\n    0: migrating node.db\n    1: disk full\n2026 INFO a later run\n";
        assert_eq!(
            reason_from_log(caused, false).as_deref(),
            Some("open store: migrating node.db: disk full")
        );

        let error_line = "2026 INFO serving\n2026 ERROR tracon::http: bind 127.0.0.1:7420: address in use\n2026 INFO shutting down\n";
        assert_eq!(
            reason_from_log(error_line, false).as_deref(),
            Some("2026 ERROR tracon::http: bind 127.0.0.1:7420: address in use")
        );

        // An ordinary log is not a reason, unless the unit is known to fail.
        let quiet = "2026 INFO serving\n\n";
        assert_eq!(reason_from_log(quiet, false), None);
        assert_eq!(
            reason_from_log(quiet, true).as_deref(),
            Some("2026 INFO serving")
        );
        assert_eq!(reason_from_log("", true), None);

        let long = format!("Error: {}", "x".repeat(1000));
        let cut = reason_from_log(&long, false).unwrap();
        assert_eq!(cut.chars().count(), MAX_REASON + 1);
    }
}
