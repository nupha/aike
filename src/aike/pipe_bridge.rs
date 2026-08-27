// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Bridge blocking `std::io` pipe halves (as returned by nanosandbox's
//! detached spawn) into tokio's `AsyncRead`/`AsyncWrite`, so sandboxed
//! children can flow through the same StdioHandle plumbing as regular
//! spawns. One worker thread per pipe; tokio's bounded channels carry the
//! waker wiring, so pending polls are woken exactly when data or capacity
//! arrives — no polling loops.

use std::{
    io,
    pin::Pin,
    task::{ready, Context, Poll},
    thread,
};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};

/// Wraps a blocking reader: a worker thread pumps chunks into a bounded
/// channel; `poll_read` serves buffered bytes without ever blocking.
///
/// The underlying `File` is held via `Arc` so it stays alive across the
/// spawned reader thread — the thread itself keeps an `Arc<File>` clone
/// so the fd is never closed while reading.
pub struct BlockingReadAdapter {
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    #[allow(dead_code)]
    file: std::sync::Arc<std::sync::Mutex<std::fs::File>>,
}

pub fn bridge_reader(r: std::fs::File) -> BlockingReadAdapter {
    use std::sync::{Arc, Mutex};
    let (tx, rx) = mpsc::channel(16);
    let file = Arc::new(Mutex::new(r));
    let file_clone = Arc::clone(&file);
    thread::spawn(move || {
        use std::io::Read;
        loop {
            let mut chunk = vec![0u8; 16 * 1024];
            let n = file_clone.lock().unwrap().read(&mut chunk);
            match n {
                Ok(0) => break,
                Ok(n) => {
                    chunk.truncate(n);
                    if tx.blocking_send(Ok(chunk)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });
    BlockingReadAdapter { rx, buf: Vec::new(), pos: 0, eof: false, file }
}

impl AsyncRead for BlockingReadAdapter {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            // Serve buffered bytes first.
            if this.pos < this.buf.len() {
                let n = (this.buf.len() - this.pos).min(buf.remaining());
                buf.put_slice(&this.buf[this.pos..this.pos + n]);
                this.pos += n;
                return Poll::Ready(Ok(()));
            }

            if this.eof {
                return Poll::Ready(Ok(())); // EOF
            }

            match ready!(this.rx.poll_recv(cx)) {
                Some(Ok(chunk)) => {
                    this.buf = chunk;
                    this.pos = 0;
                }
                Some(Err(e)) => return Poll::Ready(Err(e)),
                None => {
                    this.eof = true;
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

/// Wraps a blocking writer: `poll_write` hands buffers to a worker thread
/// over a bounded channel; full channel ⇒ Pending with proper waker, giving
/// natural backpressure. `poll_shutdown` closes the pipe (child sees EOF).
pub struct BlockingWriteAdapter {
    tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    join: Option<thread::JoinHandle<()>>,
    fd: i32,
}

#[cfg(unix)]
pub fn bridge_writer(w: impl io::Write + Send + std::os::fd::AsRawFd + 'static) -> BlockingWriteAdapter {
    let fd = w.as_raw_fd();
    bridge_writer_impl(w, fd)
}

#[cfg(not(unix))]
pub fn bridge_writer(w: impl io::Write + Send + 'static) -> BlockingWriteAdapter {
    bridge_writer_impl(w, -1)
}

fn bridge_writer_impl(w: impl io::Write + Send + 'static, fd: i32) -> BlockingWriteAdapter {
    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let join = thread::spawn(move || {
        let mut w = w;
        let mut rx = rx;
        while let Some(data) = rx.blocking_recv() {
            if w.write_all(&data).is_err() {
                break;
            }
            let _ = w.flush();
        }
    });
    BlockingWriteAdapter { tx: Some(tx), join: Some(join), fd }
}

#[cfg(unix)]
impl std::os::fd::AsRawFd for BlockingWriteAdapter {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.fd
    }
}

impl BlockingWriteAdapter {
    fn poll_send(&mut self, cx: &mut Context<'_>, data: Vec<u8>) -> Poll<io::Result<()>> {
        let Some(tx) = &self.tx else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "writer closed",
            )));
        };
        match tx.send(data) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(_) => {
                self.tx = None;
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "writer thread died",
                )))
            }
        }
    }
}

impl AsyncWrite for BlockingWriteAdapter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let len = buf.len();
        self.get_mut().poll_send(cx, buf.to_vec())?;
        Poll::Ready(Ok(len)) // accepted into the ordered queue
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(())) // single worker preserves ordering
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        drop(this.tx.take()); // close pipe → child sees EOF
        if let Some(join) = this.join.take() {
            let _ = join.join();
        }
        Poll::Ready(Ok(()))
    }
}
