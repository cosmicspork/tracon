//! Opening a link in the operator's own browser.
//!
//! The opener plugin hands a URL to `xdg-open` from this process's PATH, with
//! this process's environment. Inside an AppImage both are the bundle's: its
//! `usr/bin` comes first on PATH, so the `xdg-open` that runs is the bundled
//! copy (old enough that it does not know KDE 6, and falls back to something
//! that opens nothing), and `LD_LIBRARY_PATH` names the bundle's libraries, so
//! a Flatpak browser started from it loads the wrong ones and dies. The
//! bundle's environment is right for the webview and wrong for anything the
//! host runs.
//!
//! So the launcher is the host's, started with the host's environment: every
//! variable put together from the parts that do not point into the bundle, and
//! the bundle's own markers dropped. Only the child gets that environment;
//! this process keeps its own. Which URLs may be opened is the caller's to
//! decide before it gets here, as it was before.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use tauri::Url;

/// Open `url` with the host's launcher. Only `http` and `https` reach a
/// browser from here; the callers have already narrowed that further.
pub fn open(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("refused to open a `{}` link", url.scheme()));
    }
    spawn(url.as_str())
}

#[cfg(target_os = "linux")]
fn spawn(url: &str) -> Result<(), String> {
    let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
    let env = host_environment(std::env::vars_os(), appdir.as_deref());
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin"));
    // `xdg-open` decides by desktop; `gio open` is what GNOME's own does, and
    // is there on hosts that have GLib but not xdg-utils.
    let (program, args): (PathBuf, &[&str]) = if let Some(p) = find(&path, "xdg-open") {
        (p, &[])
    } else if let Some(p) = find(&path, "gio") {
        (p, &["open"])
    } else {
        return Err("neither xdg-open nor gio is installed on this host".into());
    };
    let mut child = std::process::Command::new(&program)
        .args(args)
        .arg(url)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", program.display()))?;
    // Reaped off this thread: `xdg-open` returns once the browser has the URL,
    // and a launcher left unreaped is a zombie for the app's lifetime.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn(url: &str) -> Result<(), String> {
    let mut child = std::process::Command::new("/usr/bin/open")
        .arg(url)
        .spawn()
        .map_err(|e| format!("could not start open: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn spawn(url: &str) -> Result<(), String> {
    let _ = url;
    Err("opening links is not supported on this platform".into())
}

/// Markers an AppImage's runtime sets for its own use. A child that inherits
/// them can mistake itself for part of the bundle (a second AppImage does).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const BUNDLE_MARKERS: [&str; 4] = ["APPIMAGE", "APPDIR", "OWD", "ARGV0"];

/// The environment as the host would have it: each variable without the parts
/// that point into the bundle, and dropped when nothing else is left of it.
/// Outside an AppImage nothing points into one and this changes nothing.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn host_environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    appdir: Option<&Path>,
) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(key, _)| !BUNDLE_MARKERS.iter().any(|m| OsStr::new(m) == key))
        .filter_map(|(key, value)| {
            let entries: Vec<&[u8]> = value.as_encoded_bytes().split(|b| *b == b':').collect();
            let kept: Vec<&[u8]> = entries
                .iter()
                .copied()
                .filter(|entry| !in_bundle(entry, appdir))
                .collect();
            if kept.len() == entries.len() {
                return Some((key, value));
            }
            let kept: Vec<&[u8]> = kept.into_iter().filter(|e| !e.is_empty()).collect();
            if kept.is_empty() {
                return None;
            }
            let joined = kept.join(&b':');
            // Every byte came from an `OsString` split on an ASCII `:`, so it
            // is still a valid encoding of one.
            let joined = unsafe { OsString::from_encoded_bytes_unchecked(joined) };
            Some((key, joined))
        })
        .collect()
}

/// Whether one entry of a variable names something inside the running bundle:
/// under `APPDIR`, or in the directories an AppImage mounts or extracts to.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn in_bundle(entry: &[u8], appdir: Option<&Path>) -> bool {
    let text = String::from_utf8_lossy(entry);
    let path = Path::new(text.as_ref());
    if !path.is_absolute() {
        return false;
    }
    if appdir.is_some_and(|app| app.is_absolute() && path.starts_with(app)) {
        return true;
    }
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        name.starts_with(".mount_") || name.starts_with("appimage_extracted_")
    })
}

/// The first executable `name` on a PATH.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn find(path: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn get<'a>(env: &'a [(OsString, OsString)], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_str().unwrap())
    }

    #[test]
    fn the_bundle_is_taken_out_of_the_childs_environment() {
        let app = Path::new("/tmp/.mount_tracoXYZ");
        let env = host_environment(
            vars(&[
                (
                    "PATH",
                    "/tmp/.mount_tracoXYZ/usr/bin:/usr/local/bin:/usr/bin",
                ),
                (
                    "LD_LIBRARY_PATH",
                    "/tmp/.mount_tracoXYZ/usr/lib:/tmp/.mount_tracoXYZ/usr/lib64",
                ),
                (
                    "XDG_DATA_DIRS",
                    "/tmp/.mount_tracoXYZ/usr/share:/usr/local/share:/usr/share",
                ),
                (
                    "GDK_PIXBUF_MODULE_FILE",
                    "/tmp/.mount_tracoXYZ/usr/lib/loaders.cache",
                ),
                ("APPDIR", "/tmp/.mount_tracoXYZ"),
                ("APPIMAGE", "/home/me/tracon.AppImage"),
                ("OWD", "/home/me"),
                ("HOME", "/home/me"),
                ("KDE_SESSION_VERSION", "6"),
                ("XDG_CURRENT_DESKTOP", "KDE"),
            ]),
            Some(app),
        );
        assert_eq!(get(&env, "PATH"), Some("/usr/local/bin:/usr/bin"));
        assert_eq!(
            get(&env, "XDG_DATA_DIRS"),
            Some("/usr/local/share:/usr/share")
        );
        // Wholly the bundle's: gone, not set empty, which some loaders read
        // as "the current directory".
        assert_eq!(get(&env, "LD_LIBRARY_PATH"), None);
        assert_eq!(get(&env, "GDK_PIXBUF_MODULE_FILE"), None);
        for marker in ["APPDIR", "APPIMAGE", "OWD"] {
            assert_eq!(get(&env, marker), None, "{marker}");
        }
        // The host's own say in which launcher runs is untouched.
        assert_eq!(get(&env, "HOME"), Some("/home/me"));
        assert_eq!(get(&env, "KDE_SESSION_VERSION"), Some("6"));
        assert_eq!(get(&env, "XDG_CURRENT_DESKTOP"), Some("KDE"));
    }

    #[test]
    fn an_extracted_bundle_is_recognised_without_appdir() {
        let env = host_environment(
            vars(&[("PATH", "/tmp/appimage_extracted_abc/usr/bin:/usr/bin")]),
            None,
        );
        assert_eq!(get(&env, "PATH"), Some("/usr/bin"));
    }

    #[test]
    fn outside_a_bundle_nothing_changes() {
        let input = vars(&[
            ("PATH", "/usr/bin::/bin"),
            ("LD_LIBRARY_PATH", "/opt/lib"),
            ("DISPLAY", ":0"),
            ("URL_LIKE", "http://example.com:8080"),
        ]);
        assert_eq!(host_environment(input.clone(), None), input);
    }

    #[test]
    fn only_web_links_reach_the_launcher() {
        for refused in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "tel:123",
            "data:text/html,x",
        ] {
            let url = Url::parse(refused).unwrap();
            assert!(open(&url).is_err(), "{refused}");
        }
    }
}
