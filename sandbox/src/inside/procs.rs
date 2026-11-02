//! The processes the node started on this connection.

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use super::fd::FdStream;
use super::spawner::{self, Spawner};
use crate::protocol::{ExitStatus, SpawnSpec, Spawned, WinSize};

pub struct Proc {
    pid: i32,
    tty: Option<OwnedFd>,
    stdin: Mutex<Option<FdStream>>,
    stdout: Mutex<Option<FdStream>>,
    stderr: Mutex<Option<FdStream>>,
    exit: watch::Receiver<Option<ExitStatus>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("no process {0}")]
    NotFound(u64),
    #[error("that stream is not available (not piped, or already attached)")]
    Taken,
    #[error("{0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Copy)]
pub enum Stream {
    Stdin,
    Stdout,
    Stderr,
}

pub struct Procs {
    spawner: Spawner,
    next: AtomicU64,
    table: Mutex<HashMap<u64, Arc<Proc>>>,
}

impl Procs {
    pub fn new(spawner: Spawner) -> Self {
        Procs {
            spawner,
            next: AtomicU64::new(1),
            table: Mutex::new(HashMap::new()),
        }
    }

    pub fn spawn(&self, spec: &SpawnSpec) -> Result<Spawned, ProcError> {
        let child = self.spawner.spawn(spec)?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let pid = child.pid;
        let proc = Arc::new(Proc {
            pid,
            tty: child.tty,
            stdin: Mutex::new(child.stdin),
            stdout: Mutex::new(child.stdout),
            stderr: Mutex::new(child.stderr),
            exit: child.exit,
        });
        self.table.lock().unwrap().insert(id, proc);
        Ok(Spawned {
            id,
            pid: pid as u32,
        })
    }

    fn get(&self, id: u64) -> Result<Arc<Proc>, ProcError> {
        self.table
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(ProcError::NotFound(id))
    }

    /// Hand over one of a process's streams; each is handed over once.
    pub fn take(&self, id: u64, stream: Stream) -> Result<FdStream, ProcError> {
        let proc = self.get(id)?;
        let slot = match stream {
            Stream::Stdin => &proc.stdin,
            Stream::Stdout => &proc.stdout,
            Stream::Stderr => &proc.stderr,
        };
        let taken = slot.lock().unwrap().take();
        taken.ok_or(ProcError::Taken)
    }

    pub fn signal(&self, id: u64, signal: i32, group: bool) -> Result<(), ProcError> {
        let proc = self.get(id)?;
        Ok(self.spawner.signal(proc.pid, signal, group)?)
    }

    pub fn resize(&self, id: u64, size: WinSize) -> Result<(), ProcError> {
        let proc = self.get(id)?;
        let tty = proc.tty.as_ref().ok_or_else(|| {
            ProcError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not running on a terminal",
            ))
        })?;
        Ok(spawner::resize(tty, size)?)
    }

    /// Wait for a process to exit. Once its status has been delivered the
    /// process is forgotten, with any stream nobody attached: attach before
    /// waiting.
    pub async fn wait(&self, id: u64) -> Result<ExitStatus, ProcError> {
        let proc = self.get(id)?;
        let mut exit = proc.exit.clone();
        let status = loop {
            if let Some(status) = *exit.borrow_and_update() {
                break status;
            }
            if exit.changed().await.is_err() {
                return Err(ProcError::Io(io::Error::other("reaper stopped")));
            }
        };
        self.table.lock().unwrap().remove(&id);
        Ok(status)
    }

    /// End every process still running: its group gets SIGTERM, then SIGKILL
    /// after `grace`.
    pub async fn terminate_all(&self, grace: Duration) {
        let procs: Vec<Arc<Proc>> = self.table.lock().unwrap().drain().map(|(_, p)| p).collect();
        let running: Vec<_> = procs.iter().filter(|p| p.exit.borrow().is_none()).collect();
        if running.is_empty() {
            return;
        }
        for proc in &running {
            let _ = self.spawner.signal(proc.pid, libc::SIGTERM, true);
            let _ = self.spawner.signal(proc.pid, libc::SIGTERM, false);
        }
        let all_exited = async {
            for proc in &running {
                let mut exit = proc.exit.clone();
                while exit.borrow_and_update().is_none() {
                    if exit.changed().await.is_err() {
                        break;
                    }
                }
            }
        };
        if tokio::time::timeout(grace, all_exited).await.is_err() {
            for proc in &running {
                let _ = self.spawner.signal(proc.pid, libc::SIGKILL, true);
                let _ = self.spawner.signal(proc.pid, libc::SIGKILL, false);
            }
        }
    }

    /// Deliver `signal` to every process still running, and its group.
    pub fn forward(&self, signal: i32) {
        let procs: Vec<Arc<Proc>> = self.table.lock().unwrap().values().cloned().collect();
        for proc in procs {
            if proc.exit.borrow().is_none() {
                let _ = self.spawner.signal(proc.pid, signal, true);
            }
        }
    }
}
