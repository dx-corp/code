//! Configuration File Watcher
//!
//! Provides hot-reload functionality for configuration files.
//! When enabled with the `hot-reload` feature, watches config files and notifies when changes occur.
//!
//! # Features
//!
//! - Watches multiple config files simultaneously
//! - Debounces rapid changes to avoid excessive reloads
//! - Non-blocking async notifications
//! - Graceful handling of file deletions and creations
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_tui::config_watcher::{ConfigWatcher, ConfigWatcherBuilder};
//!
//! let mut watcher = ConfigWatcherBuilder::new()
//!     .watch_composer_configs()
//!     .build()?;
//!
//! // In your event loop:
//! while let Some(event) = watcher.poll() {
//!     match event {
//!         ConfigEvent::Changed(path) => reload_config(&path),
//!         ConfigEvent::Created(path) => load_new_config(&path),
//!         ConfigEvent::Deleted(path) => handle_config_deleted(&path),
//!         ConfigEvent::Error(msg) => log_error(&msg),
//!     }
//! }
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// Events emitted by the config watcher
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigEvent {
    /// A watched file was modified
    Changed(PathBuf),
    /// A watched file was created
    Created(PathBuf),
    /// A watched file was deleted
    Deleted(PathBuf),
    /// An error occurred while watching
    Error(String),
}

impl ConfigEvent {
    /// Get the path associated with this event (if any)
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            ConfigEvent::Changed(p) | ConfigEvent::Created(p) | ConfigEvent::Deleted(p) => Some(p),
            ConfigEvent::Error(_) => None,
        }
    }

    /// Check if this is an error event
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, ConfigEvent::Error(_))
    }
}

/// Debounce state for a single file
struct DebounceState {
    last_event: Instant,
    pending_event: Option<ConfigEvent>,
}

/// Configuration file watcher with debouncing
///
/// Watches configuration files for changes and emits events when
/// files are modified, created, or deleted. Events are debounced
/// to prevent rapid-fire notifications from editor save operations.
pub struct ConfigWatcher {
    /// Paths being watched
    watched_paths: HashSet<PathBuf>,
    /// Event receiver (from notify)
    event_rx: Receiver<ConfigEvent>,
    /// Event sender (for notify callback)
    #[allow(dead_code)]
    event_tx: Sender<ConfigEvent>,
    /// Debounce duration (default 500ms)
    debounce_duration: Duration,
    /// Debounce state per path
    debounce_states: std::collections::HashMap<PathBuf, DebounceState>,
    /// Native file watcher (when notify feature is enabled)
    #[cfg(feature = "hot-reload")]
    _watcher: Option<notify::RecommendedWatcher>,
}

impl ConfigWatcher {
    /// Create a new config watcher
    pub fn new() -> std::io::Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();

        Ok(Self {
            watched_paths: HashSet::new(),
            event_rx,
            event_tx,
            debounce_duration: Duration::from_millis(500),
            debounce_states: std::collections::HashMap::new(),
            #[cfg(feature = "hot-reload")]
            _watcher: None,
        })
    }

    /// Set the debounce duration
    #[must_use]
    pub fn with_debounce(mut self, duration: Duration) -> Self {
        self.debounce_duration = duration;
        self
    }

    /// Start watching a configuration file
    #[cfg(feature = "hot-reload")]
    pub fn watch(&mut self, path: impl AsRef<Path>) -> std::io::Result<()> {
        use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};

        let path = path.as_ref().to_path_buf();
        let canonical = dunce::canonicalize(&path).unwrap_or_else(|_| path.clone());

        if self.watched_paths.contains(&canonical) {
            return Ok(()); // Already watching
        }

        // Create watcher if not exists
        if self._watcher.is_none() {
            let tx = self.event_tx.clone();
            let watcher = RecommendedWatcher::new(
                move |res: Result<notify::Event, notify::Error>| {
                    if let Ok(event) = res {
                        for path in event.paths {
                            let config_event = match event.kind {
                                notify::EventKind::Create(_) => ConfigEvent::Created(path),
                                notify::EventKind::Modify(_) => ConfigEvent::Changed(path),
                                notify::EventKind::Remove(_) => ConfigEvent::Deleted(path),
                                _ => continue,
                            };
                            let _ = tx.send(config_event);
                            maestro_local_host::ui_wake::wake();
                        }
                    }
                },
                Config::default(),
            )
            .map_err(std::io::Error::other)?;

            self._watcher = Some(watcher);
        }

        // Watch the file's parent directory (to catch file creation)
        if let Some(parent) = canonical.parent() {
            if let Some(ref mut watcher) = self._watcher {
                watcher
                    .watch(parent, RecursiveMode::NonRecursive)
                    .map_err(std::io::Error::other)?;
            }
        }

        self.watched_paths.insert(canonical);
        Ok(())
    }

    /// Start watching a configuration file (no-op without hot-reload feature)
    #[cfg(not(feature = "hot-reload"))]
    pub fn watch(&mut self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref().to_path_buf();
        self.watched_paths.insert(path);
        Ok(())
    }

    /// Stop watching a file
    pub fn unwatch(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref().to_path_buf();
        #[cfg(feature = "hot-reload")]
        let path = dunce::canonicalize(&path).unwrap_or(path);
        self.watched_paths.remove(&path);
        self.debounce_states.remove(&path);
    }

    /// Poll for configuration change events
    ///
    /// Returns the next debounced event, or None if no events are pending.
    /// This is non-blocking.
    pub fn poll(&mut self) -> Option<ConfigEvent> {
        // Process any pending events from the channel
        while let Ok(event) = self.event_rx.try_recv() {
            let path = match &event {
                ConfigEvent::Changed(p) | ConfigEvent::Created(p) | ConfigEvent::Deleted(p) => {
                    p.clone()
                }
                ConfigEvent::Error(_) => return Some(event),
            };

            // Only process events for watched paths
            if !self.is_watched(&path) {
                continue;
            }

            // Update debounce state
            let state = self.debounce_states.entry(path).or_insert(DebounceState {
                last_event: Instant::now(),
                pending_event: None,
            });

            state.last_event = Instant::now();
            state.pending_event = Some(event);
        }

        // Check for debounced events ready to emit
        let now = Instant::now();
        let mut emit_path = None;

        for (path, state) in &self.debounce_states {
            if state.pending_event.is_some()
                && now.duration_since(state.last_event) >= self.debounce_duration
            {
                emit_path = Some(path.clone());
                break;
            }
        }

        if let Some(path) = emit_path {
            if let Some(state) = self.debounce_states.get_mut(&path) {
                return state.pending_event.take();
            }
        }

        None
    }

    /// Check if a path is being watched
    fn is_watched(&self, path: &Path) -> bool {
        // Check exact match
        if self.watched_paths.contains(path) {
            return true;
        }

        // Check if it's a watched file by name (handles temp files from editors)
        if let Some(file_name) = path.file_name() {
            for watched in &self.watched_paths {
                if watched.file_name() == Some(file_name) {
                    return true;
                }
            }
        }

        false
    }

    /// Get the list of watched paths
    pub fn watched_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.watched_paths.iter()
    }

    /// Check if any paths are being watched
    #[must_use]
    pub fn is_watching(&self) -> bool {
        !self.watched_paths.is_empty()
    }

    /// A debounced change is waiting out its quiet period.
    #[must_use]
    pub fn has_pending_debounce(&self) -> bool {
        self.debounce_states
            .values()
            .any(|state| state.pending_event.is_some())
    }

    /// Clear all pending events
    pub fn clear_pending(&mut self) {
        while self.event_rx.try_recv().is_ok() {}
        self.debounce_states.clear();
    }

    /// Get count of watched paths
    #[must_use]
    pub fn watch_count(&self) -> usize {
        self.watched_paths.len()
    }
}

impl Default for ConfigWatcher {
    fn default() -> Self {
        Self::new().unwrap_or_else(|err| {
            let (event_tx, event_rx) = mpsc::channel();
            let _ = event_tx.send(ConfigEvent::Error(format!(
                "Failed to initialize config watcher: {err}"
            )));
            maestro_local_host::ui_wake::wake();
            Self {
                watched_paths: HashSet::new(),
                event_rx,
                event_tx,
                debounce_duration: Duration::from_millis(500),
                debounce_states: std::collections::HashMap::new(),
                #[cfg(feature = "hot-reload")]
                _watcher: None,
            }
        })
    }
}

/// Builder for `ConfigWatcher` with common config file paths
pub struct ConfigWatcherBuilder {
    paths: Vec<PathBuf>,
    debounce: Duration,
}

impl ConfigWatcherBuilder {
    /// Create a new builder
    #[must_use]
    pub fn new() -> Self {
        Self {
            paths: Vec::new(),
            debounce: Duration::from_millis(500),
        }
    }

    /// Add a path to watch
    pub fn watch(mut self, path: impl Into<PathBuf>) -> Self {
        self.paths.push(path.into());
        self
    }

    /// Add standard composer config paths
    ///
    /// Watches:
    /// - ~/.composer/config.toml
    /// - ~/.composer/mcp.json
    /// - ~/.composer/hooks.lua
    /// - .composer/config.toml
    /// - .composer/mcp.json
    /// - .composer/mcp.local.json
    /// - .composer/hooks.lua
    /// - AGENT.md
    /// - CLAUDE.md
    #[must_use]
    pub fn watch_composer_configs(mut self) -> Self {
        // User config
        if let Some(home) = dirs::home_dir() {
            self.paths.push(home.join(".composer").join("config.toml"));
            self.paths.push(home.join(".composer").join("mcp.json"));
            self.paths.push(home.join(".composer").join("hooks.lua"));
        }

        // Project config
        self.paths.push(PathBuf::from(".composer/config.toml"));
        self.paths.push(PathBuf::from(".composer/mcp.json"));
        self.paths.push(PathBuf::from(".composer/mcp.local.json"));
        self.paths.push(PathBuf::from(".composer/hooks.lua"));

        // Agent/Claude files
        self.paths.push(PathBuf::from("AGENT.md"));
        self.paths.push(PathBuf::from("CLAUDE.md"));

        self
    }

    /// Set debounce duration
    #[must_use]
    pub fn debounce(mut self, duration: Duration) -> Self {
        self.debounce = duration;
        self
    }

    /// Build the watcher
    pub fn build(self) -> std::io::Result<ConfigWatcher> {
        let mut watcher = ConfigWatcher::new()?.with_debounce(self.debounce);

        for path in self.paths {
            // Only watch files that exist or whose parent exists
            if path.exists() || path.parent().is_some_and(std::path::Path::exists) {
                let _ = watcher.watch(&path);
            }
        }

        Ok(watcher)
    }
}

impl Default for ConfigWatcherBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_config_watcher_creation() {
        let watcher = ConfigWatcher::new();
        assert!(watcher.is_ok());
    }

    #[test]
    fn test_config_watcher_watch() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "test = true").unwrap();

        let mut watcher = ConfigWatcher::new().unwrap();
        let result = watcher.watch(&config_path);
        assert!(result.is_ok());
        assert!(watcher.is_watching());
        assert_eq!(watcher.watch_count(), 1);
    }

    #[test]
    fn test_config_watcher_unwatch() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "test = true").unwrap();

        let mut watcher = ConfigWatcher::new().unwrap();
        watcher.watch(&config_path).unwrap();
        watcher.unwatch(&config_path);

        // Path should be removed
        assert_eq!(watcher.watched_paths().count(), 0);
    }

    #[test]
    fn test_config_watcher_builder() {
        let builder = ConfigWatcherBuilder::new().debounce(Duration::from_millis(100));

        let watcher = builder.build();
        assert!(watcher.is_ok());
    }

    #[test]
    fn test_config_event_equality() {
        let path = PathBuf::from("/test/config.toml");
        let event1 = ConfigEvent::Changed(path.clone());
        let event2 = ConfigEvent::Changed(path.clone());
        let event3 = ConfigEvent::Created(path);

        assert_eq!(event1, event2);
        assert_ne!(event1, event3);
    }

    #[test]
    fn test_config_event_path() {
        let path = PathBuf::from("/test/config.toml");
        let event = ConfigEvent::Changed(path.clone());
        assert_eq!(event.path(), Some(path.as_path()));

        let error = ConfigEvent::Error("test error".to_string());
        assert_eq!(error.path(), None);
    }

    #[test]
    fn test_config_event_is_error() {
        let path = PathBuf::from("/test/config.toml");
        assert!(!ConfigEvent::Changed(path.clone()).is_error());
        assert!(!ConfigEvent::Created(path.clone()).is_error());
        assert!(!ConfigEvent::Deleted(path).is_error());
        assert!(ConfigEvent::Error("test".to_string()).is_error());
    }

    #[test]
    fn test_poll_no_events() {
        let mut watcher = ConfigWatcher::new().unwrap();
        assert!(watcher.poll().is_none());
    }

    #[test]
    fn test_builder_watch_composer_configs() {
        let builder = ConfigWatcherBuilder::new().watch_composer_configs();
        assert!(!builder.paths.is_empty());
    }
}
