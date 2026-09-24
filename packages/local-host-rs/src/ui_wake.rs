//! Process-wide wake hook for the Deixic Code terminal event loop.
//!
//! Producers that enqueue work from another crate, a sync thread, or a
//! file-watcher callback cannot hold the TUI's [`tokio::sync::Notify`].
//! They call [`wake`] after the enqueue. The TUI installs one hook for the
//! lifetime of its main loop; other processes leave the hook empty.

use std::sync::{Arc, Mutex};

static HOOK: Mutex<Option<Arc<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Install or clear the UI wake hook. The TUI sets this while its loop runs.
pub fn set_hook(hook: Option<Arc<dyn Fn() + Send + Sync>>) {
    *HOOK.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

/// Wake the installed UI loop, if any. Safe to call when no hook is set.
pub fn wake() {
    let hook = HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}
