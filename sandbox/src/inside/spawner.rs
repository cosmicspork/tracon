//! Starting processes and reaping every child, ours or adopted.
//!
//! The sandbox is its namespace's init or, under another init, a child
//! subreaper, so orphans of anything it started are reparented to it and it
//! must reap them. It does that with `waitpid(-1)`, which would also take the
//! exit status of a child some other part of the process was waiting for; so
//! every child is started here and nothing else waits. A child's status goes
//! to the watch channel registered for its pid; an orphan's is discarded.
//!
//! One lock covers starting a child, registering it and reaping. Holding it
//! while starting means the reaper cannot take a child's status before it is
//! registered (and cannot take the status of one whose exec failed, which
//! the standard library reaps itself while reporting the error); holding it
//! while signalling means a pid the reaper has released cannot be signalled
//! after the kernel has given it to someone else.

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::fd::FdStream;
use crate::protocol::{ExitStatus, SpawnSpec, WinSize};

/// Environment variables the sandbox reads at start and children never see.
pub const BOOTSTRAP_PREFIX: &str = "TRACON_SANDBOX_";

#[derive(Clone, Default)]
pub struct Spawner {
    children: Arc<Mutex<HashMap<i32, watch::Sender<Option<ExitStatus>>>>>,
}

pub struct Child {
    pub pid: i32,
    pub exit: watch::Receiver<Option<ExitStatus>>,
    pub stdin: Option<FdStream>,
    pub stdout: Option<FdStream>,
    pub stderr: Option<FdStream>,
    /// The terminal master, kept for resizing.
    pub tty: Option<OwnedFd>,
}

impl Spawner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn(&self, spec: &SpawnSpec) -> io::Result<Child> {
        let (program, args) = spec
            .argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
        let mut command = Command::new(program);
        command.args(args);
        if spec.env_clear {
            command.env_clear();
        }
        command.envs(&spec.env);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        if let Some(gid) = spec.gid {
            command.gid(gid);
        }
        if let Some(uid) = spec.uid {
            command.uid(uid);
        }
        let mut tty = None;
        match spec.tty {
            Some(size) => {
                let (master, slave) = open_pty(size)?;
                command
                    .stdin(Stdio::from(slave.try_clone()?))
                    .stdout(Stdio::from(slave.try_clone()?))
                    .stderr(Stdio::from(slave));
                // A session of its own with the terminal as its controlling
                // one, so job control and ^C behave as in a login shell.
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() < 0 {
                            return Err(io::Error::last_os_error());
                        }
                        #[allow(clippy::useless_conversion)]
                        if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                tty = Some(master);
            }
            None => {
                let pipe = |wanted| {
                    if wanted {
                        Stdio::piped()
                    } else {
                        Stdio::null()
                    }
                };
                command
                    .stdin(pipe(spec.stdin))
                    .stdout(pipe(spec.stdout))
                    .stderr(pipe(spec.stderr));
                // Its own process group, so a signal or teardown reaches
                // whatever it starts as well.
                command.process_group(0);
            }
        }

        let mut children = self.children.lock().unwrap();
        let mut child = command.spawn()?;
        let pid = child.id() as i32;
        let (sender, exit) = watch::channel(None);
        children.insert(pid, sender);
        drop(children);

        let (stdin, stdout, stderr) = match tty {
            Some(ref master) => (
                Some(FdStream::new(master.try_clone()?, true)?),
                Some(FdStream::new(master.try_clone()?, true)?),
                None,
            ),
            None => (
                child
                    .stdin
                    .take()
                    .map(|s| FdStream::new(OwnedFd::from(s), false))
                    .transpose()?,
                child
                    .stdout
                    .take()
                    .map(|s| FdStream::new(OwnedFd::from(s), false))
                    .transpose()?,
                child
                    .stderr
                    .take()
                    .map(|s| FdStream::new(OwnedFd::from(s), false))
                    .transpose()?,
            ),
        };
        Ok(Child {
            pid,
            exit,
            stdin,
            stdout,
            stderr,
            tty,
        })
    }

    /// Collect every child that has exited.
    pub fn reap(&self) {
        let mut children = self.children.lock().unwrap();
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            if let Some(sender) = children.remove(&pid) {
                let _ = sender.send(Some(ExitStatus::from_raw(status)));
            } else {
                tracing::debug!(pid, "reaped an adopted process");
            }
        }
    }

    /// Reap on every SIGCHLD, for as long as the runtime runs. A tick covers a
    /// child that exited before the handler was installed.
    pub fn start_reaper(&self) -> io::Result<tokio::task::JoinHandle<()>> {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigchld = signal(SignalKind::child())?;
        let spawner = self.clone();
        Ok(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = sigchld.recv() => {}
                    _ = tick.tick() => {}
                }
                spawner.reap();
            }
        }))
    }

    /// Signal a child, or the process group it leads. A child already reaped
    /// is refused, since its pid may now be someone else's. A group is
    /// signalled even after its leader exited: what it started may still run,
    /// and the kernel does not reuse a pid that names a live group.
    pub fn signal(&self, pid: i32, signal: i32, group: bool) -> io::Result<()> {
        if !(1..=64).contains(&signal) || pid <= 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad signal"));
        }
        let children = self.children.lock().unwrap();
        let done = if group {
            unsafe { libc::kill(-pid, signal) }
        } else if children.contains_key(&pid) {
            unsafe { libc::kill(pid, signal) }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the process has exited",
            ));
        };
        drop(children);
        if done != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The pids of every live child this spawner started.
    pub fn live(&self) -> Vec<i32> {
        self.children.lock().unwrap().keys().copied().collect()
    }
}

fn open_pty(size: WinSize) -> io::Result<(OwnedFd, OwnedFd)> {
    use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};
    #[cfg(target_os = "linux")]
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    // macOS's posix_openpt takes no O_CLOEXEC; set it right after instead.
    #[cfg(not(target_os = "linux"))]
    let master = {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
        master
    };
    grantpt(&master)?;
    unlockpt(&master)?;
    let name = ptsname(&master, Vec::new())?;
    let slave = rustix::fs::open(
        name.as_c_str(),
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    resize(&master, size)?;
    Ok((master, slave))
}

pub fn resize(master: &OwnedFd, size: WinSize) -> io::Result<()> {
    rustix::termios::tcsetwinsize(
        master,
        rustix::termios::Winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )?;
    Ok(())
}

/// Drop every bootstrap variable from this process's environment, so no
/// child inherits the token. Call before any thread starts.
pub fn scrub_environment() {
    let names: Vec<_> = std::env::vars_os()
        .filter_map(|(name, _)| {
            name.to_str()
                .filter(|name| name.starts_with(BOOTSTRAP_PREFIX))
                .map(str::to_string)
        })
        .collect();
    for name in names {
        std::env::remove_var(name);
    }
}
