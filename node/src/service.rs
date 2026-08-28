//! Installing the node under the platform's supervisor.
//!
//! The node deliberately does not daemonize, restart itself, or keep itself
//! alive: it logs to stdout, shuts down cleanly on SIGTERM, and is idempotent
//! on restart. Something else is supposed to run it. On Linux that is a
//! systemd user unit, on macOS a LaunchAgent — a *user* service in both cases,
//! because the node's state, credentials, and harness socket belong to the
//! logged-in operator, and rootless podman needs their session.
//!
//! This is a host-side recipe and stays one: it is reachable from the CLI and
//! never as a tool, so a session that breaks the build cannot restart, stop,
//! or reconfigure the node that gates it.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../deploy"]
#[include = "systemd/*"]
#[include = "launchd/*"]
struct Units;

const LINUX_UNIT: &str = "tracon.service";
const MAC_LABEL: &str = "com.tracon.node";

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

/// The unit text, with anything the platform cannot express as `%h` filled in.
fn unit_text() -> Result<String> {
    let name = if cfg!(target_os = "macos") {
        "launchd/com.tracon.node.plist"
    } else {
        "systemd/tracon.service"
    };
    let file = Units::get(name).with_context(|| format!("{name} is not embedded"))?;
    let text = String::from_utf8(file.data.to_vec()).context("unit file is not text")?;
    if cfg!(target_os = "macos") {
        let home = home()?;
        let bin = home.join(".local/bin/tracon");
        let logs = home.join("Library/Logs");
        std::fs::create_dir_all(&logs).ok();
        Ok(text
            .replace("__BIN__", &bin.to_string_lossy())
            .replace("__LOGS__", &logs.to_string_lossy()))
    } else {
        Ok(text)
    }
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
    std::fs::write(&path, unit_text()?).with_context(|| format!("writing {}", path.display()))?;
    match existing {
        Some(prev) if prev != unit_text()? => {
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
        if cfg!(target_os = "macos") {
            assert!(text.contains("com.tracon.node"));
            // Placeholders are the one thing that must not survive: launchd
            // does not expand anything.
            assert!(!text.contains("__BIN__"), "{text}");
            assert!(!text.contains("__LOGS__"), "{text}");
        } else {
            assert!(text.contains("ExecStart="));
            // The node needs time to end sessions and tear down containers.
            assert!(text.contains("KillSignal=SIGTERM"));
            assert!(text.contains("TimeoutStopSec="));
        }
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
