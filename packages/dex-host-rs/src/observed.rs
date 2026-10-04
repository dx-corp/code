//! A `Log` that also reports what it writes, so a host can project a turn
//! to its surface while the engine is still running.
//!
//! Every write goes to the wrapped log first; only a write the log accepted
//! is reported, so the surface never shows what the log refused.

use dex_loop::{Cursor, Event, Fenced, Log};
use tokio::sync::mpsc;

/// One accepted write.
#[derive(Clone, Debug, PartialEq)]
pub enum Observed {
    /// Customer-visible model text, as the engine streamed it.
    Text(String),
    Event(Cursor, Box<Event>),
}

/// Wraps `L`, sending each accepted write to the observer.
#[derive(Clone)]
pub struct ObservedLog<L> {
    inner: L,
    observer: mpsc::UnboundedSender<Observed>,
}

impl<L: Log> ObservedLog<L> {
    pub fn new(inner: L) -> (Self, mpsc::UnboundedReceiver<Observed>) {
        let (observer, observed) = mpsc::unbounded_channel();
        (Self { inner, observer }, observed)
    }
}

impl<L: Log> Log for ObservedLog<L> {
    async fn append(&self, events: &[Event]) -> Result<Vec<Cursor>, Fenced> {
        let cursors = self.inner.append(events).await?;
        for (cursor, event) in cursors.iter().zip(events) {
            // A closed observer only means nobody is watching any more.
            let _ = self
                .observer
                .send(Observed::Event(*cursor, Box::new(event.clone())));
        }
        Ok(cursors)
    }

    async fn append_text(&self, text: String) -> Result<(), Fenced> {
        self.inner.append_text(text.clone()).await?;
        let _ = self.observer.send(Observed::Text(text));
        Ok(())
    }

    async fn control_since(&self, after: Cursor) -> Result<Vec<(Cursor, Event)>, Fenced> {
        self.inner.control_since(after).await
    }
}
