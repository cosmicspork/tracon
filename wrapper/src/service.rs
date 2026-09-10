//! The node's user service, as `tracon service install` writes it. The app
//! asks whether it is there and running, and installs or restarts it through
//! the CLI it installed, so the unit is the node's own and names that binary.

use std::path::{Path, PathBuf};
use std::process::Command;

const MAC_LABEL: &str = "com.tracon.node";
const LINUX_UNIT: &str = "tracon.service";

fn unit_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
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

pub fn running() -> bool {
    if cfg!(target_os = "macos") {
        let uid = unsafe { libc::getuid() };
        Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/{MAC_LABEL}")])
            .output()
            .map(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains("state = running")
            })
            .unwrap_or(false)
    } else {
        Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", LINUX_UNIT])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
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
