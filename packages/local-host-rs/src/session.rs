//! Compatibility facade for local Maestro session persistence.
//!
//! The JSONL store lives in `maestro-session`; this module keeps the historical
//! `maestro_tui::session` imports and supplies shared local tool-output cleanup
//! to each manager.

use std::ops::{Deref, DerefMut};

pub(crate) use maestro_session::SessionLock;
pub use maestro_session::{
    AppMessage, Attachment, AttachmentExtract, BranchSummaryEntry, CompactionEntry, ContentBlock,
    CustomEntry, CustomMessageEntry, ImageSource, LabelEntry, MessageContent, MessageEntry,
    ModelChange, ModelMetadata, SessionEntry, SessionHeader, SessionMeta, SessionStats,
    SideQuestionEntry, ThinkingLevel, ThinkingLevelChange, TokenCost, TokenUsage, ToolInfo,
};
pub use maestro_session::{
    BranchId, BranchManager, BranchMetadata, BranchPoint, BranchSummary, MessageId,
};
pub use maestro_session::{ExportFormat, ExportOptions, SessionExporter, export_session_file};
pub use maestro_session::{ForkedSession, fork_session_file};
pub use maestro_session::{
    IndexedSession, SessionIndexEntry, collect_sessions, default_index_path,
};
pub use maestro_session::{
    LifecycleAgentNoteEntry, LifecycleNotificationEntry, ParsedSession, SessionReadError,
    SessionReader,
};
pub use maestro_session::{PreparedSessionAdoption, SessionInfo};
pub use maestro_session::{
    SessionWriter, generate_session_filename, sanitize_path_for_dirname, sessions_dir,
};
pub use maestro_session::{append_selective_summary_checkpoint, selective_summary_usage_entry};

/// TUI-owned wrapper around the shared persistence manager.
pub struct SessionManager(maestro_session::SessionManager);

impl SessionManager {
    /// Create a manager using the standard per-working-directory session path.
    pub fn new(cwd: impl Into<String>) -> Self {
        Self(
            maestro_session::SessionManager::new(cwd)
                .with_auxiliary_cleanup(crate::tool_output::remove_model_tool_spill_dir),
        )
    }

    /// Create a manager rooted at an explicit sessions directory.
    pub fn with_sessions_dir(
        cwd: impl Into<String>,
        sessions_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self(
            maestro_session::SessionManager::with_sessions_dir(cwd, sessions_dir)
                .with_auxiliary_cleanup(crate::tool_output::remove_model_tool_spill_dir),
        )
    }
}

impl Deref for SessionManager {
    type Target = maestro_session::SessionManager;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SessionManager {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
