//! Bound foreground pipe collection after the owning process exits.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::oneshot;
use tokio::time::{Instant, Sleep};

pub(super) const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);
const FINAL_DRAIN_BYTES: usize = 1024 * 1024;

/// Only used for actual local child pipes; remote transports retain their own EOF semantics.
pub(super) struct ExitBoundedReader<R> {
    reader: R,
    exited: oneshot::Receiver<Instant>,
    deadline: Option<Pin<Box<Sleep>>>,
    remaining: usize,
}

impl<R> ExitBoundedReader<R> {
    pub(super) fn new(reader: R, exited: oneshot::Receiver<Instant>) -> Self {
        Self {
            reader,
            exited,
            deadline: None,
            remaining: FINAL_DRAIN_BYTES,
        }
    }
}

trait ReadyPipe: AsyncRead + Unpin {
    fn read_ready(&mut self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> io::Result<()>;
}

#[cfg(unix)]
impl<R: AsyncRead + Unpin + std::os::fd::AsRawFd> ReadyPipe for R {
    fn read_ready(&mut self, _cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> io::Result<()> {
        // Tokio child pipes are nonblocking. Read the pipe itself after expiry:
        // readiness registration or cooperative task budgeting must not hide bytes
        // the foreground process already wrote before its exit was observed.
        let bytes = buf.initialize_unfilled();
        // SAFETY: self owns this live descriptor and bytes is writable for its
        // entire supplied length. read does not retain either pointer or descriptor.
        let read = unsafe { libc::read(self.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        if read >= 0 {
            buf.advance(read as usize);
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(not(unix))]
impl<R: AsyncRead + Unpin> ReadyPipe for R {
    fn read_ready(&mut self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> io::Result<()> {
        match Pin::new(self).poll_read(cx, buf) {
            Poll::Ready(result) => result,
            Poll::Pending => Ok(()),
        }
    }
}

impl<R: ReadyPipe> AsyncRead for ExitBoundedReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.deadline.is_none() {
            match Pin::new(&mut this.exited).poll(cx) {
                Poll::Ready(Ok(deadline)) => {
                    this.deadline = Some(Box::pin(tokio::time::sleep_until(deadline)));
                }
                Poll::Ready(Err(_)) => return Poll::Ready(Ok(())),
                Poll::Pending => {}
            }
        }
        let expired = this
            .deadline
            .as_mut()
            .is_some_and(|deadline| deadline.as_mut().poll(cx).is_ready());
        if !expired {
            return Pin::new(&mut this.reader).poll_read(cx, buf);
        }
        if this.remaining == 0 {
            return Poll::Ready(Ok(()));
        }
        let limit = this.remaining.min(buf.remaining());
        let mut ready = ReadBuf::new(&mut buf.initialize_unfilled()[..limit]);
        if let Err(error) = this.reader.read_ready(cx, &mut ready) {
            return Poll::Ready(Err(error));
        }
        let read = ready.filled().len();
        buf.advance(read);
        this.remaining -= read;
        Poll::Ready(Ok(()))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, RawFd};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    struct UnscheduledReader(UnixStream);

    impl AsRawFd for UnscheduledReader {
        fn as_raw_fd(&self) -> RawFd {
            self.0.as_raw_fd()
        }
    }

    impl AsyncRead for UnscheduledReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn ready_bytes_survive_expired_deadline_and_unscheduled_reader() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"foreground output").await.unwrap();
        let (exited, receiver) = oneshot::channel();
        exited.send(Instant::now() - OUTPUT_DRAIN_GRACE).unwrap();
        let mut reader = ExitBoundedReader::new(UnscheduledReader(reader), receiver);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"foreground output");
        // The writer is still alive: collecting must not require EOF.
        drop(writer);
    }

    #[tokio::test]
    async fn expired_deadline_bounds_a_still_readable_pipe() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"abcdef").await.unwrap();
        let (exited, receiver) = oneshot::channel();
        exited.send(Instant::now() - OUTPUT_DRAIN_GRACE).unwrap();
        let mut reader = ExitBoundedReader::new(UnscheduledReader(reader), receiver);
        reader.remaining = 3;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"abc");
        drop(writer);
    }

    #[tokio::test(start_paused = true)]
    async fn retains_bytes_written_during_post_exit_grace_without_waiting_for_eof() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let reader = UnixStream::from_std(reader).unwrap();
        let (exited, receiver) = oneshot::channel();
        exited.send(Instant::now() + OUTPUT_DRAIN_GRACE).unwrap();
        let collect = tokio::spawn(async move {
            let mut reader = ExitBoundedReader::new(reader, receiver);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(20)).await;
        std::io::Write::write_all(&mut writer, b"post-exit diagnostics").unwrap();
        tokio::time::advance(OUTPUT_DRAIN_GRACE).await;
        assert_eq!(collect.await.unwrap(), b"post-exit diagnostics");
        // The inherited writer remains open beyond the entire drain interval.
        drop(writer);
    }
}
