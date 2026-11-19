//! A directory that work off the runtime's threads fills, and that goes when
//! the one awaiting that work gives up.
//!
//! Dropping a future that awaits a blocking task does not stop the task: an
//! unpack goes on reading its archive and writing the directory, and then
//! returns to nobody, who would have removed it. A [`Staging`] dropped before
//! it is kept stops what reads through [`Staging::reader`] (so the stream it
//! reads is dropped, which ends the transfer), and the directory is removed
//! once the last of its blocking work ends, never while it is still being
//! written. What it holds ([`Staging::hold`]), such as a lock, is released
//! only then too, so nothing that waits on it starts beside the work its
//! owner abandoned.

use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// A staging directory, removed unless kept. See the module docs.
pub struct Staging {
    path: PathBuf,
    shared: Arc<Shared>,
    /// Kept or already removed: dropping it does nothing more.
    settled: bool,
}

struct Shared {
    stop: CancellationToken,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Blocking work still running on the directory.
    busy: usize,
    /// Its owner dropped it unkept: the last work to end removes it.
    abandoned: bool,
    held: Vec<Box<dyn Send>>,
}

fn in_flight_paths() -> &'static Mutex<HashSet<PathBuf>> {
    static PATHS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    PATHS.get_or_init(Default::default)
}

/// Whether a [`Staging`] in this process owns `path` now: a sweep leaves it
/// to its owner.
pub fn in_flight(path: &Path) -> bool {
    in_flight_paths().lock().unwrap().contains(path)
}

/// Remove `path` and forget it. What it held is released after.
fn remove(path: &Path, held: Vec<Box<dyn Send>>) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            if let Err(error) = std::fs::remove_dir_all(path) {
                tracing::warn!(path = %path.display(), %error, "could not remove a staging directory");
            }
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
    in_flight_paths().lock().unwrap().remove(path);
    drop(held);
}

impl Staging {
    /// Make the directory `path` (its parent must exist) and own it.
    pub fn create(path: PathBuf) -> io::Result<Self> {
        std::fs::create_dir(&path)?;
        Ok(Self::own(path))
    }

    /// Own the directory at `path`, which was just made for this.
    pub fn own(path: PathBuf) -> Self {
        in_flight_paths().lock().unwrap().insert(path.clone());
        Staging {
            path,
            shared: Arc::new(Shared {
                stop: CancellationToken::new(),
                state: Mutex::new(State::default()),
            }),
            settled: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Set once its owner has given up, for blocking work to look at before
    /// a step that must not happen after that (a commit).
    pub fn stopped(&self) -> CancellationToken {
        self.shared.stop.clone()
    }

    /// Release `held` only once the directory is settled: kept, or removed
    /// after the last of its blocking work.
    pub fn hold(&self, held: impl Send + 'static) {
        self.shared.state.lock().unwrap().held.push(Box::new(held));
    }

    /// `reader`, which fails every read once its owner gives up, waking a
    /// read that waits.
    pub fn reader<R>(&self, reader: R) -> Stoppable<R> {
        Stoppable {
            inner: reader,
            stop: self.shared.stop.clone(),
            stopped: Box::pin(self.shared.stop.clone().cancelled_owned()),
        }
    }

    /// Run `work` on the directory off the runtime's threads. Dropping the
    /// future leaves `work` running; the directory then goes when it ends.
    pub async fn blocking<T, F>(&self, work: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.shared.state.lock().unwrap().busy += 1;
        let busy = Busy {
            path: self.path.clone(),
            shared: self.shared.clone(),
        };
        let joined = tokio::task::spawn_blocking(move || {
            let _busy = busy;
            work()
        })
        .await;
        match joined {
            Ok(done) => done,
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }

    /// Keep what is at its path (or nothing, once work moved it away), and
    /// release what it holds.
    pub fn keep(mut self) -> PathBuf {
        self.settled = true;
        in_flight_paths().lock().unwrap().remove(&self.path);
        std::mem::take(&mut self.path)
    }

    /// Remove it now, once no work of its runs.
    pub async fn discard(mut self) {
        self.settled = true;
        self.shared.stop.cancel();
        let path = std::mem::take(&mut self.path);
        let held = std::mem::take(&mut self.shared.state.lock().unwrap().held);
        // Off the runtime's threads; a caller that gives up meanwhile leaves
        // it running to the end.
        let _ = tokio::task::spawn_blocking(move || remove(&path, held)).await;
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.shared.stop.cancel();
        let held = {
            let mut state = self.shared.state.lock().unwrap();
            state.abandoned = true;
            if state.busy > 0 {
                // The last work to end removes it.
                return;
            }
            std::mem::take(&mut state.held)
        };
        let path = std::mem::take(&mut self.path);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(move || remove(&path, held));
            }
            Err(_) => remove(&path, held),
        }
    }
}

/// One piece of blocking work running on a [`Staging`], until it ends.
struct Busy {
    path: PathBuf,
    shared: Arc<Shared>,
}

impl Drop for Busy {
    fn drop(&mut self) {
        let held = {
            let mut state = self.shared.state.lock().unwrap();
            state.busy -= 1;
            if state.busy > 0 || !state.abandoned {
                return;
            }
            std::mem::take(&mut state.held)
        };
        remove(&self.path, held);
    }
}

/// A reader that fails once its [`Staging`]'s owner gives up.
pub struct Stoppable<R> {
    inner: R,
    stop: CancellationToken,
    stopped: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Stoppable<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if this.stop.is_cancelled() || this.stopped.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::Error::other("the transfer was abandoned")));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}
