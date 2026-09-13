//! Installing the node under the platform's supervisor.
//!
//! The node deliberately does not daemonize, restart itself, or keep itself
//! alive: it logs to stdout, shuts down cleanly on SIGTERM, and is idempotent
//! on restart. Something else is supposed to run it. On Linux that is a
//! systemd user unit, on macOS a LaunchAgent — a *user* service in both cases,
//! because the node's state, credentials, and harness socket belong to the
//! logged-in operator, and rootless podman needs their session.
//!
//! This is primarily a host-side recipe: the CLI and desktop app are the
//! preferred way to install and diagnose it. The operator API exposes only
//! separately authenticated, loopback-only scheduling of these same fixed
//! install, remove, and restart operations; it never accepts a command, path,
//! or service name from a caller. A harness inside the boundary still cannot
//! drive them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rust_embed::Embed;
use serde::Serialize;

#[derive(Embed)]
#[folder = "../deploy"]
#[include = "systemd/*"]
#[include = "launchd/*"]
struct Units;

const LINUX_UNIT: &str = "tracon.service";
const MAC_LABEL: &str = "com.tracon.node";

/// What the node can truthfully learn about the user service without exposing
/// credentials, environment values, or supervisor output.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostics {
    pub platform: &'static str,
    pub supervisor: &'static str,
    pub container: Option<&'static str>,
    pub unit_path: Option<String>,
    pub unit_installed: bool,
    pub state: ServiceState,
    pub restart: LifecycleCapability,
    pub install: LifecycleCapability,
    pub uninstall: LifecycleCapability,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceState {
    /// `running`, `stopped`, or `unknown` when the supervisor cannot answer.
    pub state: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LifecycleCapability {
    pub available: bool,
    pub reason: String,
    pub recovery: String,
}

/// The only service lifecycle actions the HTTP surface may schedule. No route
/// accepts an executable, argument, unit name, or path from its caller.
#[derive(Debug, Clone, Copy)]
pub enum LifecycleAction {
    Install,
    Uninstall,
    Restart,
}

impl LifecycleAction {
    fn label(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
            Self::Restart => "restart",
        }
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// Where the unit file belongs on this platform.
fn unit_path() -> Result<PathBuf> {
    let home = home()?;
    if cfg!(target_os = "macos") {
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{MAC_LABEL}.plist")))
    } else {
        Ok(home.join(".config/systemd/user").join(LINUX_UNIT))
    }
}

fn container_kind() -> Option<&'static str> {
    if std::path::Path::new("/run/.containerenv").exists() {
        Some("podman container")
    } else if std::path::Path::new("/.dockerenv").exists() {
        Some("Docker container")
    } else if std::env::var_os("container").is_some() {
        Some("container")
    } else {
        None
    }
}

fn host_preflight() -> Result<()> {
    if let Some(kind) = container_kind() {
        bail!("this node runs in a {kind}, so it cannot control the host user service");
    }
    Ok(())
}

/// The fixed unit is intentionally self-contained and does not copy process
/// environment overrides. Managing it from an isolated/manual node would
/// therefore operate a different state or configuration root.
fn environment_overrides_preflight() -> Result<()> {
    for name in ["TRACON_STATE_DIR", "TRACON_CONFIG_DIR"] {
        if std::env::var_os(name).is_some_and(|value| !value.is_empty()) {
            bail!(
                "{name} is overridden for this serving process, but the fixed user-service unit does not preserve that override"
            );
        }
    }
    Ok(())
}

fn supervisor_preflight() -> Result<()> {
    let result = if cfg!(target_os = "macos") {
        Command::new("launchctl")
            .arg("version")
            .output()
            .map(|output| output.status)
            .context("running launchctl version")
    } else {
        Command::new("systemctl")
            .arg("--version")
            .output()
            .map(|output| output.status)
            .context("running systemctl --version")
    };
    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => bail!("the fixed user-service supervisor is not available to this node"),
        Err(error) => {
            bail!("the fixed user-service supervisor is not available to this node: {error}")
        }
    }
}

fn environment_preflight() -> Result<()> {
    host_preflight()?;
    environment_overrides_preflight()?;
    supervisor_preflight()
}

fn action_specific_preflight(action: LifecycleAction) -> Result<()> {
    match action {
        LifecycleAction::Install => {
            let binary = std::env::current_exe().context("finding this node binary")?;
            stable_binary(&binary)
        }
        LifecycleAction::Uninstall => home().map(|_| ()),
        LifecycleAction::Restart => {
            let path = unit_path()?;
            if !path.exists() {
                bail!(
                    "not installed (no unit at {}); install the fixed user service first",
                    path.display()
                );
            }
            Ok(())
        }
    }
}

fn action_preflight(action: LifecycleAction) -> Result<()> {
    environment_preflight()?;
    action_specific_preflight(action)?;
    service_target_preflight(supervisor_main_pid()?)
}

fn parse_systemd_main_pid(text: &str) -> Result<Option<u32>> {
    let raw = text.trim();
    if raw.is_empty() || raw == "0" {
        return Ok(None);
    }
    let pid = raw
        .parse::<u32>()
        .context("systemd reported a malformed service MainPID")?;
    Ok((pid != 0).then_some(pid))
}

fn parse_launchd_main_pid(text: &str) -> Result<Option<u32>> {
    let running = text
        .lines()
        .map(str::trim)
        .any(|line| line == "state = running");
    if !running {
        return Ok(None);
    }
    for line in text.lines().map(str::trim) {
        if let Some(raw) = line.strip_prefix("pid = ") {
            let pid = raw
                .parse::<u32>()
                .context("launchd reported a malformed service pid")?;
            if pid != 0 {
                return Ok(Some(pid));
            }
        }
    }
    bail!("launchd reports the fixed user service running but did not provide its pid");
}

/// The only fixed supervisor query that identifies a running service. A
/// failure to identify an active launchd job is deliberately not guessed.
fn supervisor_main_pid() -> Result<Option<u32>> {
    let output = if cfg!(target_os = "macos") {
        let target = format!("gui/{}/{MAC_LABEL}", uid()?);
        Command::new("launchctl")
            .args(["print", &target])
            .output()
            .context("running launchctl print")
    } else {
        Command::new("systemctl")
            .args([
                "--user",
                "show",
                LINUX_UNIT,
                "--property=MainPID",
                "--value",
            ])
            .output()
            .context("reading systemd MainPID")
    }?;
    if !output.status.success() {
        // A fixed unit that is absent or inactive has no process to protect.
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if cfg!(target_os = "macos") {
        parse_launchd_main_pid(&text)
    } else {
        parse_systemd_main_pid(&text)
    }
}

fn service_target_preflight(main_pid: Option<u32>) -> Result<()> {
    let current = std::process::id();
    if let Some(main_pid) = main_pid.filter(|pid| *pid != current) {
        bail!(
            "the fixed user service is active as PID {main_pid}, not this serving process (PID {current}); refusing to change another node's service"
        );
    }
    Ok(())
}

fn supervisor_state(
    unit_installed: bool,
    container: Option<&'static str>,
    main_pid: &Result<Option<u32>>,
) -> ServiceState {
    if let Some(kind) = container {
        return ServiceState {
            state: "unknown",
            detail: format!("{kind}; the node cannot inspect the host user service"),
        };
    }
    match main_pid {
        Ok(Some(pid)) if *pid == std::process::id() => ServiceState {
            state: "running",
            detail: format!("the fixed user service reports this serving process (PID {pid}) active"),
        },
        Ok(Some(pid)) => ServiceState {
            state: "running",
            detail: format!(
                "the fixed user service targets PID {pid}, not this serving process (PID {}); lifecycle changes are refused",
                std::process::id()
            ),
        },
        Ok(None) => ServiceState {
            state: "stopped",
            detail: if unit_installed {
                "the fixed user-service unit is installed but not active".into()
            } else {
                "no user-service unit is installed".into()
            },
        },
        Err(error) => ServiceState {
            state: "unknown",
            detail: format!("the user supervisor target could not be verified: {error}"),
        },
    }
}

fn lifecycle_capability(
    action: LifecycleAction,
    environment: &Result<(), String>,
    target: &Result<(), String>,
) -> LifecycleCapability {
    let (reason, recovery) = match action {
        LifecycleAction::Install => (
            "this node can install and start its fixed user-service unit after this response",
            "The page disconnects while the service starts. Reconnect here after the node answers.",
        ),
        LifecycleAction::Uninstall => (
            "this node can disable and remove its fixed user-service unit after this response",
            "The page disconnects. Node state and credentials remain; start the node another supported way to return.",
        ),
        LifecycleAction::Restart => (
            "this node can ask its own user supervisor to restart it after this response",
            "The page disconnects while the node restarts; reconnect here after the service answers.",
        ),
    };
    let check = match environment {
        Ok(()) => match target {
            Ok(()) => action_specific_preflight(action).map_err(|error| error.to_string()),
            Err(error) => Err(error.clone()),
        },
        Err(error) => Err(error.clone()),
    };
    match check {
        Ok(()) => LifecycleCapability {
            available: true,
            reason: reason.into(),
            recovery: recovery.into(),
        },
        Err(error) => LifecycleCapability {
            available: false,
            reason: error,
            recovery: "This node cannot safely operate that host service from here.".into(),
        },
    }
}

/// Safe diagnostics for the settings surface. Every supervisor invocation has
/// fixed program and arguments; command output is intentionally not returned.
pub fn diagnostics() -> Diagnostics {
    let container = container_kind();
    let unit_path = unit_path().ok();
    let unit_installed = unit_path.as_ref().is_some_and(|path| path.exists());
    // Read the fixed service target once for all capability rows. A unit that
    // names another running process is visible but never changeable here.
    let main_pid = if container.is_none() {
        supervisor_main_pid()
    } else {
        Ok(None)
    };
    let target = match &main_pid {
        Ok(pid) => service_target_preflight(*pid).map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let environment = environment_preflight().map_err(|error| error.to_string());
    Diagnostics {
        platform: if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        },
        supervisor: if cfg!(target_os = "macos") {
            "launchd"
        } else {
            "systemd user service"
        },
        container,
        unit_path: unit_path.map(|path| path.display().to_string()),
        unit_installed,
        state: supervisor_state(unit_installed, container, &main_pid),
        install: lifecycle_capability(LifecycleAction::Install, &environment, &target),
        uninstall: lifecycle_capability(LifecycleAction::Uninstall, &environment, &target),
        restart: lifecycle_capability(LifecycleAction::Restart, &environment, &target),
    }
}

/// Schedule one fixed service lifecycle operation after the HTTP response has
/// had time to leave this process. Scheduling is the only success this caller
/// can truthfully receive; a later diagnostic is the evidence of recovery.
pub fn schedule(action: LifecycleAction) -> Result<()> {
    action_preflight(action)?;
    std::thread::Builder::new()
        .name(format!("tracon-service-{}", action.label()))
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(350));
            if let Err(error) = action_preflight(action) {
                tracing::warn!(action = action.label(), %error, "scheduled user-service action refused after target changed");
                return;
            }
            let result = match action {
                LifecycleAction::Install => install(),
                LifecycleAction::Uninstall => uninstall(),
                LifecycleAction::Restart => restart(),
            };
            if let Err(error) = result {
                tracing::error!(action = action.label(), %error, "scheduled user-service action failed");
            }
        })
        .context("scheduling fixed user-service action")?;
    Ok(())
}

/// The unit text, naming the binary that runs this command rather than a fixed
/// install location, so `install.sh`'s `TRACON_BIN_DIR` and the copy the
/// desktop app places are both what the service runs.
fn unit_text() -> Result<String> {
    let name = if cfg!(target_os = "macos") {
        "launchd/com.tracon.node.plist"
    } else {
        "systemd/tracon.service"
    };
    let file = Units::get(name).with_context(|| format!("{name} is not embedded"))?;
    let text = String::from_utf8(file.data.to_vec()).context("unit file is not text")?;
    let bin = std::env::current_exe().context("finding this binary")?;
    stable_binary(&bin)?;
    let logs = home()?.join("Library/Logs");
    let path = service_path(home().ok());
    let path = if cfg!(target_os = "macos") {
        std::fs::create_dir_all(&logs).ok();
        path.replace('&', "&amp;").replace('<', "&lt;")
    } else {
        path.replace('%', "%%")
    };
    Ok(text
        .replace("__BIN__", &bin.to_string_lossy())
        .replace("__LOGS__", &logs.to_string_lossy())
        .replace("__PATH__", &path))
}

/// The PATH the node runs with. A service manager hands its services a
/// minimal one without podman, gh, glab or uv, so the unit carries the PATH
/// of whoever installed it, then the usual install locations.
fn service_path(home: Option<PathBuf>) -> String {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let usual = [
        home.map(|h| h.join(".local/bin")),
        Some("/opt/homebrew/bin".into()),
        Some("/usr/local/bin".into()),
        Some("/usr/bin".into()),
        Some("/bin".into()),
        Some("/usr/sbin".into()),
        Some("/sbin".into()),
    ];
    dirs.extend(usual.into_iter().flatten());
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| {
        d.is_absolute() && !d.to_string_lossy().contains(':') && seen.insert(d.clone())
    });
    std::env::join_paths(dirs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A unit may only name a binary that outlives this process. An AppImage runs
/// from a mount that disappears when it exits, and a translocated app from a
/// copy macOS throws away.
fn stable_binary(path: &Path) -> Result<()> {
    let s = path.to_string_lossy();
    if s.contains("/.mount_") || s.contains("/AppTranslocation/") {
        bail!(
            "{} is a temporary copy; install the CLI (the desktop app does this) and run `tracon service install` from it",
            path.display()
        );
    }
    Ok(())
}

/// The current user id, for launchd's `gui/<uid>` domain. Shelling out beats
/// a libc dependency and an unsafe block for one number.
fn uid() -> Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("running id -u")?;
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uid.is_empty() {
        bail!("could not read the current user id");
    }
    Ok(uid)
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Write the unit and start it. Safe to run again: an existing unit is
/// replaced and the service restarted, which is what an upgrade wants.
pub fn install() -> Result<()> {
    let path = unit_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let existing = std::fs::read_to_string(&path).ok();
    let text = unit_text()?;
    std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
    match existing {
        Some(prev) if prev != text => {
            println!("replaced the unit at {}", path.display())
        }
        Some(_) => println!("unit at {} is unchanged", path.display()),
        None => println!("wrote {}", path.display()),
    }

    if cfg!(target_os = "macos") {
        let target = format!("gui/{}", uid()?);
        // Booting out first makes this idempotent: launchd refuses to load a
        // label that is already loaded.
        let _ = run("launchctl", &["bootout", &target, &path.to_string_lossy()]);
        run(
            "launchctl",
            &["bootstrap", &target, &path.to_string_lossy()],
        )?;
        run(
            "launchctl",
            &["kickstart", "-k", &format!("{target}/{MAC_LABEL}")],
        )?;
        println!(
            "tracon is running under launchd; `launchctl print {target}/{MAC_LABEL}` for detail"
        );
    } else {
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", LINUX_UNIT])?;
        // Already-running nodes need the new unit applied, and an upgraded
        // binary needs the restart regardless.
        run("systemctl", &["--user", "restart", LINUX_UNIT])?;
        println!("tracon is running under systemd; `systemctl --user status tracon` for detail");
        // Without lingering the node stops at logout, which is exactly what a
        // node that is supposed to be reachable must not do.
        if !lingering() {
            println!();
            println!("note: this user does not linger, so the node stops when you log out.");
            println!("  sudo loginctl enable-linger $USER");
        }
    }
    Ok(())
}

/// Whether the user's services survive logout.
fn lingering() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Linger=yes"))
        .unwrap_or(false)
}

/// Restart the node under its supervisor, onto whatever binary the unit names.
pub fn restart() -> Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        bail!(
            "not installed (no unit at {}); run `tracon service install`",
            path.display()
        );
    }
    if cfg!(target_os = "macos") {
        run(
            "launchctl",
            &["kickstart", "-k", &format!("gui/{}/{MAC_LABEL}", uid()?)],
        )?;
    } else {
        run("systemctl", &["--user", "restart", LINUX_UNIT])?;
    }
    println!("restarted the node");
    Ok(())
}

/// Stop the service and remove the unit. The node's state is left alone.
pub fn uninstall() -> Result<()> {
    let path = unit_path()?;
    if cfg!(target_os = "macos") {
        let _ = run(
            "launchctl",
            &[
                "bootout",
                &format!("gui/{}", uid()?),
                &path.to_string_lossy(),
            ],
        );
    } else {
        let _ = run("systemctl", &["--user", "disable", "--now", LINUX_UNIT]);
    }
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        println!("removed {}", path.display());
    } else {
        println!("no unit at {}", path.display());
    }
    if !cfg!(target_os = "macos") {
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    println!("the node's state and credentials are untouched");
    Ok(())
}

/// What the supervisor says about it.
pub fn status() -> Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        println!("not installed (no unit at {})", path.display());
        println!("  tracon service install");
        return Ok(());
    }
    let (program, args): (&str, Vec<String>) = if cfg!(target_os = "macos") {
        (
            "launchctl",
            vec!["print".into(), format!("gui/{}/{MAC_LABEL}", uid()?)],
        )
    } else {
        (
            "systemctl",
            vec![
                "--user".into(),
                "status".into(),
                LINUX_UNIT.into(),
                "--no-pager".into(),
            ],
        )
    };
    let out = Command::new(program)
        .args(&args)
        .output()
        .with_context(|| format!("running {program}"))?;
    // `systemctl status` exits non-zero for a stopped unit, which is a report,
    // not a failure to report.
    print!("{}", String::from_utf8_lossy(&out.stdout));
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        eprint!("{err}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_for_this_platform_is_embedded_and_complete() {
        let text = unit_text().unwrap();
        assert!(!text.is_empty());
        // Placeholders are the one thing that must not survive: launchd
        // expands nothing, and systemd would run a file named `__BIN__`.
        assert!(!text.contains("__BIN__"), "{text}");
        // A service manager's own PATH has none of the tools the node runs.
        assert!(!text.contains("__PATH__"), "{text}");
        assert!(text.contains("/usr/bin"), "{text}");
        if cfg!(target_os = "macos") {
            assert!(text.contains("com.tracon.node"));
            assert!(!text.contains("__LOGS__"), "{text}");
        } else {
            assert!(text.contains("ExecStart="));
            // The node needs time to end sessions and tear down containers.
            assert!(text.contains("KillSignal=SIGTERM"));
            assert!(text.contains("TimeoutStopSec="));
        }
    }

    #[test]
    fn the_unit_names_the_binary_that_installed_it() {
        let exe = std::env::current_exe().unwrap();
        let text = unit_text().unwrap();
        assert!(text.contains(&*exe.to_string_lossy()), "{text}");
    }

    #[test]
    fn a_temporary_copy_cannot_be_named_by_a_unit() {
        assert!(stable_binary(Path::new("/tmp/.mount_traconAb12/usr/bin/tracon")).is_err());
        assert!(stable_binary(Path::new(
            "/private/var/folders/x/T/AppTranslocation/1A/d/tracon.app/Contents/MacOS/tracon"
        ))
        .is_err());
        assert!(stable_binary(Path::new("/home/op/.local/bin/tracon")).is_ok());
    }

    #[test]
    fn supervisor_pid_parsers_refuse_ambiguous_active_launchd_state() {
        assert_eq!(parse_systemd_main_pid("0\n").unwrap(), None);
        assert_eq!(parse_systemd_main_pid("4321\n").unwrap(), Some(4321));
        assert_eq!(
            parse_launchd_main_pid("state = running\npid = 4321\n").unwrap(),
            Some(4321)
        );
        assert_eq!(parse_launchd_main_pid("state = waiting\n").unwrap(), None);
        assert!(parse_launchd_main_pid("state = running\n").is_err());
    }

    #[test]
    fn another_supervisor_pid_cannot_be_lifecycled() {
        assert!(service_target_preflight(Some(std::process::id())).is_ok());
        assert!(service_target_preflight(Some(std::process::id().saturating_add(1))).is_err());
    }

    #[test]
    fn the_unit_goes_where_the_platform_looks_for_it() {
        let path = unit_path().unwrap();
        let s = path.to_string_lossy();
        if cfg!(target_os = "macos") {
            assert!(
                s.ends_with("Library/LaunchAgents/com.tracon.node.plist"),
                "{s}"
            );
        } else {
            assert!(s.ends_with(".config/systemd/user/tracon.service"), "{s}");
        }
    }
}
