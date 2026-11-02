//! A pipe end or a pseudo-terminal master as an async byte stream.

use std::io;
use std::os::fd::{AsFd as _, OwnedFd};
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct FdStream {
    fd: Option<AsyncFd<OwnedFd>>,
    /// A terminal master reads EIO once the last slave is closed, which is
    /// its end of file.
    eio_is_eof: bool,
}

impl FdStream {
    pub fn new(fd: OwnedFd, eio_is_eof: bool) -> io::Result<Self> {
        let flags = rustix::fs::fcntl_getfl(&fd)?;
        rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)?;
        Ok(FdStream {
            fd: Some(AsyncFd::new(fd)?),
            eio_is_eof,
        })
    }

    fn fd(&self) -> io::Result<&AsyncFd<OwnedFd>> {
        self.fd
            .as_ref()
            .ok_or_else(|| io::ErrorKind::BrokenPipe.into())
    }
}

impl AsyncRead for FdStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let eio_is_eof = self.eio_is_eof;
        let fd = self.fd()?;
        loop {
            let mut guard = ready!(fd.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|inner| {
                rustix::io::read(inner.get_ref().as_fd(), &mut *unfilled).map_err(io::Error::from)
            }) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) if eio_is_eof && error.raw_os_error() == Some(libc::EIO) => {
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for FdStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let fd = self.fd()?;
        loop {
            let mut guard = ready!(fd.poll_write_ready(cx))?;
            match guard.try_io(|inner| {
                rustix::io::write(inner.get_ref().as_fd(), buf).map_err(io::Error::from)
            }) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// Closes the descriptor: for a pipe, the reader sees end of file.
    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.fd = None;
        Poll::Ready(Ok(()))
    }
}
