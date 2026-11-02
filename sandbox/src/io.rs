//! HTTP/2 streams as byte streams, with the flow control left in place.
//!
//! A reader returns window to its peer only for bytes its consumer has
//! actually taken, and a writer sends only as much as the peer's window
//! allows. So a consumer that stops reading stops the producer at the far
//! end — down to the process writing into a pipe — with at most a window's
//! worth of bytes in flight.

use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use bytes::{Buf as _, Bytes};
use h2::{RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The most handed to one `send_data`.
const MAX_CHUNK: usize = 64 * 1024;

fn h2_to_io(error: h2::Error) -> io::Error {
    if error.is_io() {
        error.into_io().unwrap_or_else(|| io::Error::other("h2 io"))
    } else {
        io::Error::new(io::ErrorKind::ConnectionReset, error)
    }
}

/// The receiving half of an HTTP/2 stream.
pub struct H2Reader {
    stream: RecvStream,
    pending: Bytes,
    done: bool,
}

impl H2Reader {
    pub fn new(stream: RecvStream) -> Self {
        H2Reader {
            stream,
            pending: Bytes::new(),
            done: false,
        }
    }
}

impl AsyncRead for H2Reader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if !this.pending.is_empty() {
                let n = this.pending.len().min(buf.remaining());
                buf.put_slice(&this.pending[..n]);
                this.pending.advance(n);
                // Window goes back only for what was consumed.
                let _ = this.stream.flow_control().release_capacity(n);
                return Poll::Ready(Ok(()));
            }
            if this.done {
                return Poll::Ready(Ok(()));
            }
            match ready!(this.stream.poll_data(cx)) {
                Some(Ok(data)) => this.pending = data,
                Some(Err(error)) => return Poll::Ready(Err(h2_to_io(error))),
                None => this.done = true,
            }
        }
    }
}

/// The sending half of an HTTP/2 stream. Shutting it down ends the stream
/// cleanly; [`H2Writer::reset`] ends it as failed.
pub struct H2Writer {
    stream: SendStream<Bytes>,
    closed: bool,
}

impl H2Writer {
    pub fn new(stream: SendStream<Bytes>) -> Self {
        H2Writer {
            stream,
            closed: false,
        }
    }

    /// End the stream as failed, so the reader sees an error rather than a
    /// short but well-formed body.
    pub fn reset(mut self) {
        self.closed = true;
        self.stream.send_reset(h2::Reason::INTERNAL_ERROR);
    }
}

impl AsyncWrite for H2Writer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.closed {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        loop {
            let available = self.stream.capacity();
            if available > 0 {
                let n = available.min(buf.len()).min(MAX_CHUNK);
                self.stream
                    .send_data(Bytes::copy_from_slice(&buf[..n]), false)
                    .map_err(h2_to_io)?;
                return Poll::Ready(Ok(n));
            }
            self.stream.reserve_capacity(buf.len().min(MAX_CHUNK));
            match ready!(self.stream.poll_capacity(cx)) {
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Poll::Ready(Err(h2_to_io(error))),
                None => return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Sent data is with the connection, which flushes it.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.closed {
            self.closed = true;
            self.stream
                .send_data(Bytes::new(), true)
                .map_err(h2_to_io)?;
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for H2Writer {
    fn drop(&mut self) {
        // A writer dropped without a shutdown is a transfer that did not
        // finish; it must not read as one that did.
        if !self.closed {
            self.stream.send_reset(h2::Reason::CANCEL);
        }
    }
}
