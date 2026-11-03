//! The processes the node started. They outlive any one connection: a
//! process's output is read into a bounded log as it is written, sent from
//! whatever offset the node asks for, and kept a while after sending, so a
//! node that comes back after a dropped connection carries on where it was
//! cut off; its stdin stays open, with a count of what was written into it,
//! so a resent body is not written twice.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::watch;

use super::fd::FdStream;
use super::spawner::{self, Spawner};
use crate::protocol::{ExitStatus, SpawnSpec, Spawned, WinSize, OUTPUT_AHEAD, OUTPUT_KEPT};

/// Processes whose exit the node has been told of, kept so it can be told
/// again after a reconnect; beyond this many, the oldest are forgotten.
const WAITED_KEPT: usize = 64;

pub struct Proc {
    pid: i32,
    tty: Option<OwnedFd>,
    stdin: Option<Arc<Input>>,
    stdout: Option<Arc<Output>>,
    stderr: Option<Arc<Output>>,
    exit: watch::Receiver<Option<ExitStatus>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("no process {0}")]
    NotFound(u64),
    #[error("that stream is not piped")]
    Taken,
    #[error("{0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Copy)]
pub enum Stream {
    Stdout,
    Stderr,
}

pub struct Procs {
    spawner: Spawner,
    next: AtomicU64,
    table: Mutex<HashMap<u64, Arc<Proc>>>,
    waited: Mutex<VecDeque<u64>>,
}

impl Procs {
    pub fn new(spawner: Spawner) -> Self {
        Procs {
            spawner,
            next: AtomicU64::new(1),
            table: Mutex::new(HashMap::new()),
            waited: Mutex::new(VecDeque::new()),
        }
    }

    /// Must be called inside a Tokio runtime: each output stream is read by
    /// a task of its own from the start.
    pub fn spawn(&self, spec: &SpawnSpec) -> Result<Spawned, ProcError> {
        let child = self.spawner.spawn(spec)?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let pid = child.pid;
        let proc = Arc::new(Proc {
            pid,
            tty: child.tty,
            stdin: child.stdin.map(Input::new),
            stdout: child.stdout.map(Output::pumped),
            stderr: child.stderr.map(Output::pumped),
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

    pub fn output(&self, id: u64, stream: Stream) -> Result<Arc<Output>, ProcError> {
        let proc = self.get(id)?;
        let output = match stream {
            Stream::Stdout => &proc.stdout,
            Stream::Stderr => &proc.stderr,
        };
        output.clone().ok_or(ProcError::Taken)
    }

    pub fn input(&self, id: u64) -> Result<Arc<Input>, ProcError> {
        self.get(id)?.stdin.clone().ok_or(ProcError::Taken)
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

    /// Wait for a process to exit. Its status can be asked for again, by a
    /// node that reconnected before the first answer reached it, until
    /// [`WAITED_KEPT`] later waits have pushed it out.
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
        let mut waited = self.waited.lock().unwrap();
        if !waited.contains(&id) {
            waited.push_back(id);
            if waited.len() > WAITED_KEPT {
                if let Some(oldest) = waited.pop_front() {
                    self.table.lock().unwrap().remove(&oldest);
                }
            }
        }
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

/// One output stream: everything from `base` on that the process wrote, up
/// to [`OUTPUT_KEPT`] bytes behind the furthest the node was sent and
/// [`OUTPUT_AHEAD`] bytes beyond it.
pub struct Output {
    log: Mutex<Log>,
    /// Bumped when bytes are added or the stream ends.
    grown: watch::Sender<()>,
    /// Bumped when the furthest-sent offset moves, which may make room.
    drained: watch::Sender<()>,
    /// Which attachment is current: a later one supersedes an earlier.
    attached: AtomicU64,
}

struct Log {
    base: u64,
    bytes: VecDeque<u8>,
    sent: u64,
    /// `Some(None)` at end of file, `Some(Some(why))` when reading failed.
    end: Option<Option<String>>,
}

impl Log {
    fn end_offset(&self) -> u64 {
        self.base + self.bytes.len() as u64
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("output before byte {0} is no longer kept")]
    Gone(u64),
    #[error("byte {0} has not been written yet")]
    Beyond(u64),
    #[error("reading the process's output failed: {0}")]
    Failed(String),
}

/// The most one read of the log hands out.
const CHUNK: usize = 64 * 1024;

impl Output {
    fn pumped(fd: FdStream) -> Arc<Output> {
        let output = Arc::new(Output {
            log: Mutex::new(Log {
                base: 0,
                bytes: VecDeque::new(),
                sent: 0,
                end: None,
            }),
            grown: watch::channel(()).0,
            drained: watch::channel(()).0,
            attached: AtomicU64::new(0),
        });
        tokio::spawn(pump(output.clone(), fd));
        output
    }

    /// A new reader takes over; earlier ones stop at their next chunk.
    pub fn attach(&self) -> u64 {
        self.attached.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn is_current(&self, attachment: u64) -> bool {
        self.attached.load(Ordering::SeqCst) == attachment
    }

    /// Whether `from` can be sent: neither forgotten nor not yet written.
    pub fn check(&self, from: u64) -> Result<(), OutputError> {
        let log = self.log.lock().unwrap();
        if from < log.base {
            return Err(OutputError::Gone(log.base));
        }
        if from > log.end_offset() {
            return Err(OutputError::Beyond(from));
        }
        Ok(())
    }

    /// What follows `from`, waiting for the process to write some; empty at
    /// the end of the stream.
    pub async fn read_at(&self, from: u64) -> Result<Bytes, OutputError> {
        let mut grown = self.grown.subscribe();
        loop {
            grown.borrow_and_update();
            {
                let log = self.log.lock().unwrap();
                if from < log.base {
                    return Err(OutputError::Gone(log.base));
                }
                let end = log.end_offset();
                if from > end {
                    return Err(OutputError::Beyond(from));
                }
                if from < end {
                    let start = (from - log.base) as usize;
                    let len = ((end - from) as usize).min(CHUNK);
                    let (a, b) = log.bytes.as_slices();
                    let mut chunk = Vec::with_capacity(len);
                    if start < a.len() {
                        let take = len.min(a.len() - start);
                        chunk.extend_from_slice(&a[start..start + take]);
                        chunk.extend_from_slice(&b[..len - take]);
                    } else {
                        let start = start - a.len();
                        chunk.extend_from_slice(&b[start..start + len]);
                    }
                    return Ok(Bytes::from(chunk));
                }
                match &log.end {
                    Some(None) => return Ok(Bytes::new()),
                    Some(Some(why)) => return Err(OutputError::Failed(why.clone())),
                    None => {}
                }
            }
            if grown.changed().await.is_err() {
                return Ok(Bytes::new());
            }
        }
    }

    /// Everything before `upto` has been handed to a connection: what lies
    /// more than [`OUTPUT_KEPT`] behind it is let go, and the pump may read
    /// further ahead.
    pub fn sent(&self, upto: u64) {
        let mut log = self.log.lock().unwrap();
        if upto <= log.sent {
            return;
        }
        log.sent = upto;
        let keep_from = upto.saturating_sub(OUTPUT_KEPT as u64);
        if keep_from > log.base {
            let forget = ((keep_from - log.base) as usize).min(log.bytes.len());
            log.bytes.drain(..forget);
            log.base += forget as u64;
        }
        drop(log);
        self.drained.send_replace(());
    }

    fn has_room(&self) -> bool {
        let log = self.log.lock().unwrap();
        log.end_offset().saturating_sub(log.sent) < OUTPUT_AHEAD as u64
    }

    fn append(&self, bytes: &[u8]) {
        self.log.lock().unwrap().bytes.extend(bytes);
        self.grown.send_replace(());
    }

    fn finish(&self, failure: Option<String>) {
        self.log.lock().unwrap().end = Some(failure);
        self.grown.send_replace(());
    }
}

/// Read the process's output into its log for as long as there is room.
async fn pump(output: Arc<Output>, mut fd: FdStream) {
    let mut drained = output.drained.subscribe();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        loop {
            drained.borrow_and_update();
            if output.has_room() {
                break;
            }
            if drained.changed().await.is_err() {
                return;
            }
        }
        match fd.read(&mut chunk).await {
            Ok(0) => return output.finish(None),
            Ok(n) => output.append(&chunk[..n]),
            Err(error) => return output.finish(Some(error.to_string())),
        }
    }
}

/// A process's stdin: open until the node ends it or the process closes it,
/// whatever happens to the connection carrying it.
pub struct Input {
    state: tokio::sync::Mutex<InputState>,
}

pub struct InputState {
    fd: Option<FdStream>,
    /// Bytes written into the process so far.
    pub written: u64,
    /// Why it is closed, once it is.
    pub closed: Option<String>,
}

impl Input {
    fn new(fd: FdStream) -> Arc<Input> {
        Arc::new(Input {
            state: tokio::sync::Mutex::new(InputState {
                fd: Some(fd),
                written: 0,
                closed: None,
            }),
        })
    }

    /// One writer at a time; a writer whose connection died lets go as its
    /// stream fails.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, InputState> {
        self.state.lock().await
    }
}

impl InputState {
    /// Write all of `bytes`, counting each part as it lands, so a write cut
    /// short (its future dropped) still leaves `written` true.
    pub async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let fd = self
            .fd
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?;
        let mut at = 0;
        while at < bytes.len() {
            let n = fd.write(&bytes[at..]).await?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            at += n;
            self.written += n as u64;
        }
        Ok(())
    }

    /// Close it: for a pipe, the process reads end of file.
    pub async fn close(&mut self, why: impl Into<String>) {
        if let Some(mut fd) = self.fd.take() {
            let _ = fd.shutdown().await;
        }
        self.closed.get_or_insert_with(|| why.into());
    }
}
