//! TUI Components
//!
//! This module contains the core UI components for the Maestro terminal interface,
//! built using the Ratatui library. Each component is a custom widget that implements
//! the Widget trait to render terminal UI elements.
//!
//! # Architecture Overview
//!
//! Ratatui follows a declarative UI model where widgets are stateless rendering functions
//! that take a rendering area (Rect) and a buffer (Buffer) and draw to it. State is managed
//! separately from rendering.
//!
//! ## The Widget Trait
//!
//! All custom components implement the `ratatui::widgets::Widget` trait:
//!
//! ```rust,ignore
//! trait Widget {
//!     fn render(self, area: Rect, buf: &mut Buffer);
//! }
//! ```
//!
//! - `area`: The rectangular area (x, y, width, height) where the widget should render
//! - `buf`: The terminal buffer where styled text is written cell-by-cell
//!
//! ## Stateful vs Stateless Widgets
//!
//! - **Stateless widgets**: Take references to data and render it. The widget struct
//!   itself doesn't hold state, only references needed for rendering (e.g., MessageWidget,
//!   StatusBarWidget).
//! - **Stateful widgets**: Maintain their own state across renders (e.g., TextArea,
//!   CommandPalette, ApprovalController). These typically have separate state and
//!   rendering components.
//!
//! ## Layout and Rect
//!
//! The `Rect` type represents a rectangular area in the terminal:
//!
//! ```rust,ignore
//! pub struct Rect {
//!     pub x: u16,      // Column offset
//!     pub y: u16,      // Row offset
//!     pub width: u16,  // Width in columns
//!     pub height: u16, // Height in rows
//! }
//! ```
//!
//! Layouts are created using `ratatui::layout::Layout` to split areas into sub-regions:
//!
//! ```rust,ignore
//! let chunks = Layout::vertical([
//!     Constraint::Length(3),  // Fixed height
//!     Constraint::Min(0),     // Take remaining space
//!     Constraint::Length(1),  // Footer
//! ]).split(area);
//! ```
//!
//! ## Styled Text
//!
//! Text is styled using `Span` and `Line` types:
//!
//! - `Span<'a>`: A string with a single style
//! - `Line<'a>`: A collection of spans forming a single line
//! - `Text<'a>`: A collection of lines
//!
//! Example:
//!
//! ```rust,ignore
//! let line = Line::from(vec![
//!     Span::styled("Error: ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
//!     Span::raw("File not found"),
//! ]);
//! ```
//!
//! ## Component Categories
//!
//! ### Message Display
//! - `ChatView`: The main scrollable message list
//! - `MessageWidget`: Individual message renderer with markdown support
//! - `ToolCallWidget`: Tool execution display with expand/collapse
//!
//! ### Input Components
//! - `TextArea`: Multi-line text input with cursor tracking
//! - `ChatInputWidget`: The main chat input box with in-box prompt and busy states
//!
//! ### Modal Dialogs
//! - `ApprovalModal`: Tool approval confirmation dialog
//! - `BatchedApprovalModal`: Batch approval dialog for parallel tool calls
//! - `CommandPalette`: Fuzzy command search modal
//! - `FileSearchModal`: File picker modal
//! - `ModelSelector`: Model selection modal
//! - `RewindPicker`: File checkpoint restore picker modal
//! - `SessionSwitcher`: Session list modal
//! - `SetupModal`: First-run EvalOps Identity and optional local API key modal
//! - `ThemeSelector`: Theme picker modal
//!
//! ### Utility Components
//! - `StatusBarWidget`: Bottom status bar with model/git info
//! - `BoxWidget`: Bordered container widget
//! - `render_scrollbar`: Scrollbar rendering helper
//!
//! ## Keyboard Event Handling
//!
//! Keyboard events are handled separately from rendering. Components that respond to
//! keyboard input typically expose methods like:
//!
//! ```rust,ignore
//! impl CommandPalette {
//!     pub fn insert_char(&mut self, c: char) { ... }
//!     pub fn backspace(&mut self) { ... }
//!     pub fn move_up(&mut self) { ... }
//! }
//! ```
//!
//! These methods update the component's state, and the next render cycle will reflect
//! the changes. This separation of input handling and rendering is a core principle
//! of immediate-mode UI architectures.

pub mod activity;
mod approval;
mod command_palette;
mod config_selector;
mod context_indicator;
mod deixic_logo;
mod detail_view;
pub mod dex_companion;
mod file_search;
mod input;
mod layout;
mod mcp_manager;
mod message;
pub mod startup;
pub(crate) use message::should_render_message;
pub(crate) mod message_layout;
pub(crate) mod model_selector;
mod operations;
mod rate_limit;
mod rewind_picker;
mod scroll;
mod session_switcher;
mod setup_modal;
mod shortcuts_help;
mod status_indicator;
mod text;
pub mod textarea;
mod theme_selector;
mod thinking_indicator;
mod welcome;

pub use crate::model_catalog::{ModelInfo, available_models};
pub use approval::{
    ApprovalController, ApprovalDecision, ApprovalModal, ApprovalModalKind, ApprovalRequest,
    BatchedApprovalModal, approval_modal_kind,
};
pub use command_palette::CommandPalette;
pub use config_selector::{ConfigCategory, ConfigChangeEvent, ConfigOption, ConfigSelector};
pub use context_indicator::{ContextIndicator, ContextIndicatorBuilder, UsageLevel};
pub use deixic_logo::{
    LOGO_COMPACT, LOGO_FULL, LOGO_MICRO, LOGO_TINY, logo_line_count, logo_visual_width, pick_logo,
    render_welcome, welcome_content_lines,
};
pub use detail_view::DetailView;
pub use file_search::FileSearchModal;
pub use input::{EditorWidget, InputWidget};
pub use layout::{BoxWidget, column_layout, row_layout};
pub use mcp_manager::McpManager;
pub use message::{
    ChatInputWidget, ChatInputWidgetOptions, ChatView, MessageWidget, StatusBarWidget,
    ToolCallWidget,
};
pub(crate) use message::{PendingInputPreview, calculate_input_height, composer_editor_width};
pub use model_selector::ModelSelector;
pub use operations::{OperationRow, OperationsModal, ReceiptSummary, project_session};
pub use rate_limit::{
    CreditsDisplay, RateLimitDisplay, RateLimitState, RateLimitTracker, RateLimitWindow,
    format_duration_compact, format_elapsed,
};
pub use rewind_picker::RewindPicker;
pub use scroll::render_scrollbar;
pub use session_switcher::SessionSwitcher;
pub use setup_modal::{SetupAdvance, SetupModal, SetupPage, SetupPresentation, SetupProvider};
pub use shortcuts_help::{Shortcut, ShortcutCategory, ShortcutsHelp, ShortcutsHelpBuilder};
pub use status_indicator::{StatusIndicator, StatusIndicatorBuilder};
pub use text::{StyledTextWidget, TextWidget};
pub use theme_selector::ThemeSelector;
pub use thinking_indicator::{
    ThinkingDisplayMode, ThinkingIndicator, ThinkingIndicatorBuilder, ThinkingPhase, ThinkingState,
};
pub use welcome::{OnboardingFlow, OnboardingStep, SplashScreen, WelcomeScreen};

#[cfg(test)]
pub(crate) mod theme_test;
