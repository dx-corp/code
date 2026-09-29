//! Keeping the model's context bounded.

use std::future::Future;

use crate::context::{Context, Entry, Message};
use crate::event::Cursor;

/// A planned compaction: history up to and including `covers_to` becomes
/// `summary`. The engine appends it as `Event::Compaction`, so rehydration
/// applies it the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compaction {
    pub covers_to: Cursor,
    pub summary: String,
}

/// Decides, before each model call, whether to compact.
pub trait Compactor: Send + Sync {
    fn plan(&self, ctx: &Context) -> impl Future<Output = Option<Compaction>> + Send;
}

/// Never compacts.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCompaction;

impl Compactor for NoCompaction {
    async fn plan(&self, _ctx: &Context) -> Option<Compaction> {
        None
    }
}

/// Writes the summary for a prefix of history (usually one model call).
pub trait Summarize: Send + Sync {
    /// `None` skips this compaction; the engine continues uncompacted.
    fn summarize(&self, entries: &[Entry]) -> impl Future<Output = Option<String>> + Send;
}

/// Compacts when history exceeds `max_bytes`, keeping at least the last
/// `keep_recent` entries verbatim.
#[derive(Clone, Debug)]
pub struct Threshold<S> {
    max_bytes: usize,
    keep_recent: usize,
    summarizer: S,
}

impl<S: Summarize> Threshold<S> {
    pub fn new(max_bytes: usize, keep_recent: usize, summarizer: S) -> Self {
        Self {
            max_bytes,
            keep_recent,
            summarizer,
        }
    }
}

impl<S: Summarize> Compactor for Threshold<S> {
    async fn plan(&self, ctx: &Context) -> Option<Compaction> {
        let history = ctx.history();
        let size: usize = history.iter().map(|entry| entry.message.size()).sum();
        if size <= self.max_bytes {
            return None;
        }
        let cut = cut_point(history, self.keep_recent)?;
        let summary = self.summarizer.summarize(&history[..cut]).await?;
        Some(Compaction {
            covers_to: history[cut - 1].cursor,
            summary,
        })
    }
}

/// The latest index `cut` with at least `keep_recent` entries after it where
/// history can be split: the kept part must not start with a tool result
/// (it belongs to the assistant message before it), and the cut must fall
/// between two cursors so the compaction event can name it.
fn cut_point(history: &[Entry], keep_recent: usize) -> Option<usize> {
    let latest = history.len().checked_sub(keep_recent)?;
    (1..=latest).rev().find(|&cut| {
        let first_kept = history.get(cut);
        let only_summary = cut == 1 && matches!(history[0].message, Message::Summary { .. });
        let splits_cursor = first_kept.is_some_and(|kept| kept.cursor == history[cut - 1].cursor);
        let orphans_result =
            first_kept.is_some_and(|kept| matches!(kept.message, Message::Tool { .. }));
        !only_summary && !splits_cursor && !orphans_result
    })
}
