//! A `Log` that also reports what it writes, so a host can project a turn
//! to its surface while the engine is still running.
//!
//! Every write goes to the wrapped log first. A single coalesced wakeup tells
//! the host to read accepted rows from that log; payloads never queue behind a
//! slow surface, and observer speed never blocks a log write.

use dex_loop::{Cursor, Event, Fenced, Log};
use tokio::sync::mpsc;

/// One accepted write.
#[derive(Clone, Debug, PartialEq)]
pub enum Observed {
    /// Customer-visible model text, as the engine streamed it.
    Text(String),
    Event(Cursor, Box<Event>),
}

/// Wraps `L`, waking the observer after an accepted write.
///
/// The receiver carries notifications, not events. The host must catch up
/// from its log even when several writes coalesce into one notification.
#[derive(Clone)]
pub struct ObservedLog<L> {
    inner: L,
    observer: mpsc::Sender<()>,
}

impl<L: Log> ObservedLog<L> {
    pub fn new(inner: L) -> (Self, mpsc::Receiver<()>) {
        let (observer, observed) = mpsc::channel(1);
        (Self { inner, observer }, observed)
    }
}

impl<L: Log> Log for ObservedLog<L> {
    async fn append(&self, events: &[Event]) -> Result<Vec<Cursor>, Fenced> {
        let cursors = self.inner.append(events).await?;
        if !cursors.is_empty() {
            // Full means a wakeup is already pending; closed means nobody
            // is watching. Neither changes the accepted write.
            let _ = self.observer.try_send(());
        }
        Ok(cursors)
    }

    async fn append_text(&self, text: String) -> Result<(), Fenced> {
        self.inner.append_text(text).await?;
        let _ = self.observer.try_send(());
        Ok(())
    }

    async fn control_since(&self, after: Cursor) -> Result<Vec<(Cursor, Event)>, Fenced> {
        self.inner.control_since(after).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalLog;
    use dex_loop::ThreadId;
    use tempfile::TempDir;

    fn thread() -> ThreadId {
        ThreadId {
            org: "org".into(),
            workspace: "workspace".into(),
            thread: "thread".into(),
        }
    }

    #[tokio::test]
    async fn refused_writes_do_not_wake_the_observer() {
        let dir = TempDir::new().expect("tempdir");
        let old = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("first lease");
        let (log, observed) = ObservedLog::new(old);
        let _replacement = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("replacement");
        assert!(log.append(&[Event::Interrupted]).await.is_err());
        assert!(log.append_text("refused text".into()).await.is_err());
        assert!(observed.is_empty());
    }

    #[tokio::test]
    async fn disconnected_observer_does_not_change_text_or_event_persistence() {
        let dir = TempDir::new().expect("tempdir");
        let local = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("lease");
        let (log, observed) = ObservedLog::new(local.clone());
        drop(observed);
        log.append_text("accepted text".into()).await.expect("text");
        log.append(&[Event::Interrupted]).await.expect("event");
        assert_eq!(
            local.read_all().await.expect("read"),
            vec![
                (
                    Cursor(1),
                    Event::TextDelta {
                        text: "accepted text".into()
                    }
                ),
                (Cursor(2), Event::Interrupted),
            ]
        );
    }
}
