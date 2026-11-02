//! What carries the TLS connection: a TCP connection, or a unix socket the
//! node mounts into the sandbox.
//!
//! A unix socket is the carrier for a container with no network at all. Under
//! SELinux a confined container (`container_t`) may not connect to a socket
//! the node created with its own label, so the node creates the listener with
//! the container's: [`bind_unix_labelled`] sets the label the kernel gives the
//! next socket this thread creates, and [`label_path`] labels the directory
//! the socket lives in so the container can reach it. Both are needed, and
//! both are no-ops on a host without SELinux.

use std::io;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};

/// Any byte stream a TLS connection can run over.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}

pub type BoxedIo = Box<dyn Io>;

/// Where the sandbox dials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dial {
    /// `host:port`.
    Tcp(String),
    Unix(PathBuf),
}

impl Dial {
    pub async fn connect(&self) -> io::Result<BoxedIo> {
        match self {
            Dial::Tcp(addr) => {
                let stream = TcpStream::connect(addr).await?;
                stream.set_nodelay(true)?;
                Ok(Box::new(stream))
            }
            Dial::Unix(path) => Ok(Box::new(UnixStream::connect(path).await?)),
        }
    }
}

impl std::fmt::Display for Dial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dial::Tcp(addr) => write!(f, "tcp {addr}"),
            Dial::Unix(path) => write!(f, "unix {}", path.display()),
        }
    }
}

/// The node's end: whichever listener the provider's carrier needs.
#[derive(Debug)]
pub enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Listener {
    pub async fn accept(&self) -> io::Result<BoxedIo> {
        match self {
            Listener::Tcp(listener) => {
                let (stream, _) = listener.accept().await?;
                stream.set_nodelay(true)?;
                Ok(Box::new(stream))
            }
            Listener::Unix(listener) => {
                let (stream, _) = listener.accept().await?;
                Ok(Box::new(stream))
            }
        }
    }

    /// What a sandbox should dial to reach this listener from the node's own
    /// network namespace. A unix socket mounted elsewhere in the sandbox is
    /// dialled at its mounted path instead.
    pub fn dial(&self) -> io::Result<Dial> {
        match self {
            Listener::Tcp(listener) => Ok(Dial::Tcp(listener.local_addr()?.to_string())),
            Listener::Unix(listener) => listener
                .local_addr()?
                .as_pathname()
                .map(|path| Dial::Unix(path.to_path_buf()))
                .ok_or_else(|| io::Error::other("unnamed unix listener")),
        }
    }
}

/// Whether the host runs SELinux at all.
pub fn selinux_enabled() -> bool {
    cfg!(target_os = "linux") && Path::new("/sys/fs/selinux/enforce").exists()
}

/// Bind a unix listener at `path` whose socket carries `context` (for a
/// Podman container, `system_u:system_r:container_t:s0`), so a container
/// confined to that type may connect to it. Without SELinux, or with no
/// context, it is a plain bind. `path` must not exist.
///
/// Must be called inside a Tokio runtime.
pub fn bind_unix_labelled(path: &Path, context: Option<&str>) -> io::Result<UnixListener> {
    let listener = match context {
        Some(context) if selinux_enabled() => bind_with_sockcreate(path, context)?,
        _ => std::os::unix::net::UnixListener::bind(path)?,
    };
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener)
}

#[cfg(target_os = "linux")]
fn bind_with_sockcreate(
    path: &Path,
    context: &str,
) -> io::Result<std::os::unix::net::UnixListener> {
    if context.is_empty() || context.contains('\0') || context.contains('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SELinux context must be one non-empty line",
        ));
    }
    // The attribute is per thread and applies to every socket that thread
    // creates until it is cleared, so the set, the bind and the reset happen
    // here with nothing in between that could yield to another task.
    set_sockcreate(Some(context))?;
    let bound = std::os::unix::net::UnixListener::bind(path);
    let reset = set_sockcreate(None);
    let listener = bound?;
    reset?;
    Ok(listener)
}

#[cfg(not(target_os = "linux"))]
fn bind_with_sockcreate(
    path: &Path,
    _context: &str,
) -> io::Result<std::os::unix::net::UnixListener> {
    std::os::unix::net::UnixListener::bind(path)
}

#[cfg(target_os = "linux")]
fn set_sockcreate(context: Option<&str>) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/thread-self/attr/sockcreate")?;
    let bytes = context.unwrap_or("").as_bytes();
    // A zero-length write is how the attribute is cleared, so it goes to the
    // kernel as one, not through a writer that may skip empty writes.
    let written = unsafe { libc::write(file.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The SELinux context sockets created by this thread currently get; `None`
/// for the default. For tests and diagnostics.
#[cfg(target_os = "linux")]
pub fn current_sockcreate() -> io::Result<Option<String>> {
    let raw = std::fs::read("/proc/thread-self/attr/sockcreate")?;
    let text = String::from_utf8_lossy(&raw)
        .trim_end_matches(['\0', '\n'])
        .to_string();
    Ok((!text.is_empty()).then_some(text))
}

/// Label a file or directory (not following a link at its leaf) with
/// `context`, e.g. `system_u:object_r:container_file_t:s0` for the directory
/// holding a socket a container must reach. A no-op without SELinux.
pub fn label_path(path: &Path, context: &str) -> io::Result<()> {
    if !selinux_enabled() {
        return Ok(());
    }
    label_path_linux(path, context)
}

#[cfg(target_os = "linux")]
fn label_path_linux(path: &Path, context: &str) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let value = std::ffi::CString::new(context)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "context contains NUL"))?;
    let bytes = value.as_bytes_with_nul();
    let done = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            c"security.selinux".as_ptr(),
            bytes.as_ptr().cast(),
            bytes.len(),
            0,
        )
    };
    if done != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn label_path_linux(_path: &Path, _context: &str) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn labelled_bind_leaves_the_thread_attribute_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sock");
        // The process's own context is always one it may create sockets
        // with; on a host without SELinux the context is ignored.
        let context = std::fs::read_to_string("/proc/self/attr/current")
            .ok()
            .map(|c| c.trim_end_matches(['\0', '\n']).to_string())
            .filter(|c| !c.is_empty() && c != "kernel" && c != "unconfined");
        let listener = bind_unix_labelled(&path, context.as_deref()).unwrap();
        #[cfg(target_os = "linux")]
        if selinux_enabled() {
            assert_eq!(current_sockcreate().unwrap(), None);
        }
        let listener = Listener::Unix(listener);
        let dial = listener.dial().unwrap();
        assert_eq!(dial, Dial::Unix(path.clone()));
        let _client = dial.connect().await.unwrap();
        let _server = listener.accept().await.unwrap();
        assert!(
            bind_unix_labelled(&path, None).is_err(),
            "path already bound"
        );
    }

    #[test]
    fn a_context_must_be_one_line() {
        if !selinux_enabled() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        assert!(bind_unix_labelled(&dir.path().join("s"), Some("a\nb")).is_err());
    }
}
