//! A sandbox's WebSocket on `proto::relay::CONNECT_PATH` as a byte stream, so
//! the relay routes and splices it exactly as it does a TCP connection.
//! Binary messages carry the bytes in order; an empty binary message, a close
//! or the end of the socket is end of stream, and shutting the writer down
//! sends an empty binary message (TCP half-close).

use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// How long a close may take to reach a client that is not reading.
const CLOSE_WITHIN: Duration = Duration::from_secs(5);

pub struct WsBytes {
    socket: WebSocket,
    pending: Bytes,
    eof: bool,
    shut: bool,
}

impl WsBytes {
    pub fn new(socket: WebSocket) -> Self {
        Self {
            socket,
            pending: Bytes::new(),
            eof: false,
            shut: false,
        }
    }

    /// End the WebSocket with a close frame and nothing else.
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(CLOSE_WITHIN, self.socket.send(Message::Close(None))).await;
    }
}

impl AsyncRead for WsBytes {
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
                this.pending = this.pending.slice(n..);
                return Poll::Ready(Ok(()));
            }
            if this.eof {
                return Poll::Ready(Ok(()));
            }
            match ready!(this.socket.poll_next_unpin(cx)) {
                Some(Ok(Message::Binary(b))) if b.is_empty() => this.eof = true,
                Some(Ok(Message::Binary(b))) => this.pending = b,
                Some(Ok(Message::Close(_))) | None => this.eof = true,
                Some(Ok(Message::Text(_))) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a text message on a byte stream",
                    )))
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Poll::Ready(Err(io::Error::other(e))),
            }
        }
    }
}

impl AsyncWrite for WsBytes {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.shut {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        ready!(self.socket.poll_ready_unpin(cx)).map_err(io::Error::other)?;
        let n = buf.len().min(proto::relay::MAX_MESSAGE);
        self.socket
            .start_send_unpin(Message::Binary(Bytes::copy_from_slice(&buf[..n])))
            .map_err(io::Error::other)?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.socket.poll_flush_unpin(cx).map_err(io::Error::other)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.shut {
            ready!(self.socket.poll_ready_unpin(cx)).map_err(io::Error::other)?;
            self.socket
                .start_send_unpin(Message::Binary(Bytes::new()))
                .map_err(io::Error::other)?;
            self.shut = true;
        }
        self.socket.poll_flush_unpin(cx).map_err(io::Error::other)
    }
}
