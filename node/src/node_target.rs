//! Which node a CLI command talks to, and how `serve` says where it is.
//!
//! A node command acts on the node the caller means. That is `TRACON_URL`
//! when it is set; otherwise the address `serve` reads from `TRACON_LISTEN`;
//! otherwise the address the node on this state directory recorded in
//! `serve.json` when it bound; and only then the default,
//! `http://127.0.0.1:7420`. Without the middle two, a shell that set
//! `TRACON_STATE_DIR` for a scratch node drove the node on :7420 instead.
//!
//! `serve.json` is only a hint. One left by a node that crashed names a pid
//! that is gone, and is ignored with a warning; a pid that was reused is
//! caught by the CLI's identity check before anything is written.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file `serve` writes into its state directory.
pub const SERVE_FILE: &str = "serve.json";

/// The default node address, when nothing names another.
pub const DEFAULT_URL: &str = "http://127.0.0.1:7420";

/// What `serve` records about itself once its operator listener is bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeRecord {
    pub url: String,
    pub node_id: String,
    pub pid: u32,
}

pub fn serve_file(state_dir: &Path) -> PathBuf {
    state_dir.join(SERVE_FILE)
}

/// The URL a client on this machine reaches a listen address at. A node
/// bound to every interface answers on loopback, so that is what is named.
pub fn url_for(listen: SocketAddr) -> String {
    let ip = match listen.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        }
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
        ip => ip,
    };
    format!("http://{}", SocketAddr::new(ip, listen.port()))
}

/// Removes `serve.json` when dropped, if it is still this process's: a
/// second node that started on the same directory since owns it now.
#[derive(Debug)]
pub struct ServeFileGuard {
    path: PathBuf,
    pid: u32,
}

impl Drop for ServeFileGuard {
    fn drop(&mut self) {
        if read_record(&self.path).is_some_and(|r| r.pid == self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Record where this node listens, `0600`, replacing any file a previous
/// run left. The guard removes it again on a clean exit.
pub fn write_serve_file(
    state_dir: &Path,
    listen: SocketAddr,
    node_id: &str,
) -> std::io::Result<ServeFileGuard> {
    let record = ServeRecord {
        url: url_for(listen),
        node_id: node_id.to_string(),
        pid: std::process::id(),
    };
    std::fs::create_dir_all(state_dir)?;
    let path = serve_file(state_dir);
    let tmp = state_dir.join(format!(".{SERVE_FILE}.{}", record.pid));
    let body = serde_json::to_vec_pretty(&record).map_err(std::io::Error::other)?;
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(ServeFileGuard {
        path,
        pid: record.pid,
    })
}

fn read_record(path: &Path) -> Option<ServeRecord> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether a process with this pid exists.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 delivers nothing; it only asks whether the pid
        // exists and may be signalled.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        // Someone else's process is still a live one.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Where a target URL came from, for the messages that name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `TRACON_URL`.
    Url,
    /// `TRACON_LISTEN`, the address `serve` binds.
    Listen,
    /// `serve.json` in the state directory.
    ServeFile(PathBuf),
    /// Nothing named one.
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Url => write!(f, "TRACON_URL"),
            Source::Listen => write!(f, "TRACON_LISTEN"),
            Source::ServeFile(path) => write!(f, "{}", path.display()),
            Source::Default => write!(f, "the default address"),
        }
    }
}

/// The node a command talks to, and anything the caller should be told
/// about how it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub url: String,
    pub source: Source,
    pub warnings: Vec<String>,
}

/// Resolve the node: `TRACON_URL`, then `TRACON_LISTEN`, then the state
/// directory's `serve.json` (when its pid is alive), then the default.
/// The environment and the liveness check are passed in so the order can
/// be tested without touching either.
pub fn resolve(
    url_env: Option<&str>,
    listen_env: Option<&str>,
    state_dir: &Path,
    alive: impl Fn(u32) -> bool,
) -> Result<Target, String> {
    fn set(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|v| !v.is_empty())
    }
    if let Some(url) = set(url_env) {
        return Ok(Target {
            url: url.trim_end_matches('/').to_string(),
            source: Source::Url,
            warnings: Vec::new(),
        });
    }
    if let Some(listen) = set(listen_env) {
        let addr: SocketAddr = listen.parse().map_err(|_| {
            format!("TRACON_LISTEN={listen} is not an address (host:port); set TRACON_URL to the node you mean")
        })?;
        return Ok(Target {
            url: url_for(addr),
            source: Source::Listen,
            warnings: Vec::new(),
        });
    }
    let mut warnings = Vec::new();
    let path = serve_file(state_dir);
    if path.exists() {
        match read_record(&path) {
            Some(record) if alive(record.pid) => {
                return Ok(Target {
                    url: record.url.trim_end_matches('/').to_string(),
                    source: Source::ServeFile(path),
                    warnings,
                });
            }
            Some(record) => warnings.push(format!(
                "{SERVE_FILE} in {} names pid {}, which is not running; ignoring it",
                state_dir.display(),
                record.pid
            )),
            None => warnings.push(format!(
                "{} is not a serve record; ignoring it",
                path.display()
            )),
        }
    }
    Ok(Target {
        url: DEFAULT_URL.to_string(),
        source: Source::Default,
        warnings,
    })
}

/// Whether a URL's host is this machine.
pub fn is_loopback(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.host() {
        Some(url::Host::Domain(name)) => name == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// The first eight characters of a node id, as messages show it.
pub fn short_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("tracon-node-target-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn record(dir: &Path, url: &str, pid: u32) {
        let r = ServeRecord {
            url: url.into(),
            node_id: "abcd".into(),
            pid,
        };
        std::fs::write(serve_file(dir), serde_json::to_string(&r).unwrap()).unwrap();
    }

    #[test]
    fn tracon_url_wins_over_everything() {
        let d = dir("url");
        record(&d, "http://127.0.0.1:7600", 1);
        let t = resolve(
            Some("http://127.0.0.1:7000/"),
            Some("127.0.0.1:7497"),
            &d,
            |_| true,
        )
        .unwrap();
        assert_eq!(t.url, "http://127.0.0.1:7000");
        assert_eq!(t.source, Source::Url);
    }

    #[test]
    fn tracon_listen_comes_next() {
        let d = dir("listen");
        record(&d, "http://127.0.0.1:7600", 1);
        let t = resolve(None, Some("127.0.0.1:7497"), &d, |_| true).unwrap();
        assert_eq!(t.url, "http://127.0.0.1:7497");
        assert_eq!(t.source, Source::Listen);
        // An empty TRACON_URL is no TRACON_URL.
        let t = resolve(Some(""), Some("127.0.0.1:7497"), &d, |_| true).unwrap();
        assert_eq!(t.source, Source::Listen);
    }

    #[test]
    fn an_unparseable_listen_is_refused_not_skipped() {
        let d = dir("bad-listen");
        let err = resolve(None, Some("nonsense"), &d, |_| true).unwrap_err();
        assert!(err.contains("TRACON_LISTEN=nonsense"), "{err}");
    }

    #[test]
    fn a_live_serve_file_names_the_node() {
        let d = dir("live");
        record(&d, "http://127.0.0.1:7600", 42);
        let t = resolve(None, None, &d, |pid| pid == 42).unwrap();
        assert_eq!(t.url, "http://127.0.0.1:7600");
        assert_eq!(t.source, Source::ServeFile(serve_file(&d)));
        assert!(t.warnings.is_empty());
    }

    #[test]
    fn a_dead_pid_is_ignored_with_a_warning() {
        let d = dir("dead");
        record(&d, "http://127.0.0.1:7600", 42);
        let t = resolve(None, None, &d, |_| false).unwrap();
        assert_eq!(t.url, DEFAULT_URL);
        assert_eq!(t.source, Source::Default);
        assert_eq!(
            t.warnings,
            vec![format!(
                "serve.json in {} names pid 42, which is not running; ignoring it",
                d.display()
            )]
        );
    }

    #[test]
    fn nothing_named_is_the_default() {
        let d = dir("none");
        let t = resolve(None, None, &d, |_| true).unwrap();
        assert_eq!(t.url, DEFAULT_URL);
        assert!(t.warnings.is_empty());
    }

    #[test]
    fn every_interface_is_reached_on_loopback() {
        assert_eq!(
            url_for("0.0.0.0:7497".parse().unwrap()),
            "http://127.0.0.1:7497"
        );
        assert_eq!(url_for("[::]:7497".parse().unwrap()), "http://[::1]:7497");
        assert_eq!(
            url_for("192.0.2.1:7497".parse().unwrap()),
            "http://192.0.2.1:7497"
        );
    }

    #[test]
    fn the_serve_file_is_private_and_removed_only_by_its_writer() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("write");
        let guard = write_serve_file(&d, "127.0.0.1:7497".parse().unwrap(), "node-id").unwrap();
        let path = serve_file(&d);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let r = read_record(&path).unwrap();
        assert_eq!(r.url, "http://127.0.0.1:7497");
        assert_eq!(r.node_id, "node-id");
        assert_eq!(r.pid, std::process::id());
        // Ours: the live pid makes it the target.
        let t = resolve(None, None, &d, pid_alive).unwrap();
        assert_eq!(t.url, "http://127.0.0.1:7497");
        drop(guard);
        assert!(!path.exists());

        // Another node took the directory over: its file stays.
        let guard = write_serve_file(&d, "127.0.0.1:7497".parse().unwrap(), "a").unwrap();
        record(&d, "http://127.0.0.1:7600", std::process::id() + 1);
        drop(guard);
        assert!(path.exists());
    }

    #[test]
    fn this_process_is_alive() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }

    #[test]
    fn loopback_is_recognised() {
        assert!(is_loopback("http://127.0.0.1:7420"));
        assert!(is_loopback("http://localhost:7420"));
        assert!(is_loopback("http://[::1]:7420"));
        assert!(!is_loopback("https://node.example.ts.net"));
    }
}
