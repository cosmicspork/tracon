//! What this app does that the node has no opinion about.
//!
//! These are not node settings and do not belong in `node.toml`: they are how
//! one person's desktop behaves, and the node's configuration is replicated,
//! shared, and read by a phone that has no dock and no ⌘Q. They live beside
//! it rather than in it, and the tray menu is the whole of the interface —
//! three checkboxes do not earn a window.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Show the window when the app starts. Off: it starts in the menu bar
    /// only, which is what a login item wants and what a double-click does
    /// not.
    pub open_window_at_launch: bool,
    /// ⌘Q quits. Off: ⌘Q closes to the menu bar and only the tray's Quit
    /// actually ends the app — which is what a tray app usually wants, and
    /// not what macOS conventionally means by ⌘Q, so it is a choice.
    pub cmd_q_quits: bool,
    /// Drop the dock icon while the window is closed. The app is in the menu
    /// bar either way; this decides whether it is also in the dock and the
    /// app switcher when there is no window to switch to.
    pub hide_dock_when_closed: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            open_window_at_launch: true,
            cmd_q_quits: true,
            hide_dock_when_closed: true,
        }
    }
}

impl Prefs {
    pub fn path() -> PathBuf {
        // Beside node.toml, which is where someone would look for it.
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tracon/desktop.json")
    }

    /// Read them, or the defaults. A file that does not parse is not worth
    /// refusing to start over: the defaults are all reasonable, and writing
    /// any preference rewrites it.
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    fn load_from(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::path())
    }

    fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}

/// The live copy, so the tray and the event loop agree.
pub struct Store(Mutex<Prefs>);

impl Store {
    pub fn new() -> Self {
        Self(Mutex::new(Prefs::load()))
    }

    pub fn get(&self) -> Prefs {
        self.0.lock().unwrap().clone()
    }

    /// Change one preference and write the file. A failed write is not worth
    /// interrupting anyone over — the setting still applies for this run.
    pub fn update(&self, f: impl FnOnce(&mut Prefs)) -> Prefs {
        let mut guard = self.0.lock().unwrap();
        f(&mut guard);
        if let Err(e) = guard.save() {
            eprintln!("tracon: could not save preferences: {e}");
        }
        guard.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tracon-prefs-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("desktop.json")
    }

    #[test]
    fn the_defaults_open_a_window_and_leave_cmd_q_alone() {
        // A double-clicked app that shows nothing reads as a failed launch,
        // and ⌘Q means quit until someone says otherwise.
        let p = Prefs::default();
        assert!(p.open_window_at_launch);
        assert!(p.cmd_q_quits);
        assert!(p.hide_dock_when_closed);
    }

    #[test]
    fn preferences_survive_a_round_trip() {
        let path = scratch("roundtrip");
        let p = Prefs {
            cmd_q_quits: false,
            open_window_at_launch: false,
            ..Default::default()
        };
        p.save_to(&path).unwrap();
        let back = Prefs::load_from(&path);
        assert!(!back.cmd_q_quits);
        assert!(!back.open_window_at_launch);
        assert!(back.hide_dock_when_closed);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_missing_or_broken_file_is_the_defaults_not_a_failure() {
        let path = scratch("broken");
        assert!(Prefs::load_from(&path).cmd_q_quits, "missing file");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(Prefs::load_from(&path).cmd_q_quits, "unparseable file");
        // A file naming only one preference keeps the defaults for the rest.
        std::fs::write(&path, r#"{"cmd_q_quits":false}"#).unwrap();
        let p = Prefs::load_from(&path);
        assert!(!p.cmd_q_quits);
        assert!(p.open_window_at_launch);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
