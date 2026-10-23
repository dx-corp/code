//! Application-Level Message Types
//!
//! This module provides a richer message type system for the application layer.
//! `AppMessage` can represent standard messages as well as custom types like
//! bash command executions, which get transformed to standard API messages
//! before being sent to the LLM.
//!
//! # Architecture
//!
//! The pattern separates concerns:
//! - **App layer**: Uses `AppMessage` with rich metadata (exit codes, truncation info, etc.)
//! - **API layer**: Uses standard `Message` types required by LLM providers
//!
//! The `transform_to_api_messages` function bridges these layers.
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_tui::ai::app_message::{AppMessage, BashExecution, transform_to_api_messages};
//!
//! let app_messages = vec![
//!     AppMessage::User { content: "Run ls -la".to_string() },
//!     AppMessage::BashExecution(BashExecution {
//!         command: "ls -la".to_string(),
//!         output: "file1.txt\nfile2.txt".to_string(),
//!         exit_code: 0,
//!         ..Default::default()
//!     }),
//! ];
//!
//! let api_messages = transform_to_api_messages(&app_messages);
//! // api_messages contains standard Message types ready for the API
//! ```

use super::types::{ContentBlock, Message, MessageContent, Role};
use serde::{Deserialize, Serialize};

/// Application-level message that can represent various message types.
///
/// This enum extends the standard message types with application-specific
/// variants that carry richer metadata. All variants can be transformed
/// to standard `Message` types for API communication.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppMessage {
    /// Standard user text message
    User {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },

    /// Standard assistant text response
    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },

    /// Assistant message with structured content blocks
    AssistantBlocks {
        blocks: Vec<ContentBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },

    /// Tool result from the user side
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },

    /// Bash command execution with rich metadata
    BashExecution(BashExecution),

    /// System message (not typically sent to API but stored in session)
    System {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },

    /// Context summary from compaction
    ContextSummary {
        summary: String,
        compacted_count: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },
}

/// Bash command execution details.
///
/// This struct captures rich information about a bash command execution
/// that would be lost in a simple text representation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BashExecution {
    /// The command that was executed
    pub command: String,

    /// The output (stdout + stderr combined)
    pub output: String,

    /// Exit code of the command (0 = success)
    pub exit_code: i32,

    /// Whether the command was cancelled by the user
    #[serde(default)]
    pub cancelled: bool,

    /// Whether the output was truncated
    #[serde(default)]
    pub truncated: bool,

    /// Path to full output file if truncated
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,

    /// Execution duration in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,

    /// Timestamp of execution
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,

    /// Working directory where command was executed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl BashExecution {
    /// Create a new bash execution record
    pub fn new(command: impl Into<String>, output: impl Into<String>, exit_code: i32) -> Self {
        Self {
            command: command.into(),
            output: output.into(),
            exit_code,
            ..Default::default()
        }
    }

    /// Mark as truncated with optional full output path
    #[must_use]
    pub fn with_truncation(mut self, full_output_path: Option<String>) -> Self {
        self.truncated = true;
        self.full_output_path = full_output_path;
        self
    }

    /// Add execution duration
    #[must_use]
    pub fn with_duration(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    /// Check if the command succeeded (exit code 0)
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0 && !self.cancelled
    }

    /// Format as a user message for the LLM
    fn to_user_content(&self) -> String {
        let mut parts = Vec::new();

        // Command
        parts.push(format!("$ {}", self.command));

        // Output
        if !self.output.is_empty() {
            parts.push(self.output.clone());
        }

        // Status line
        let mut status_parts = Vec::new();

        if self.cancelled {
            status_parts.push("(cancelled)".to_string());
        } else if self.exit_code != 0 {
            status_parts.push(format!("(exit code: {})", self.exit_code));
        }

        if self.truncated {
            if let Some(ref path) = self.full_output_path {
                status_parts.push(format!("(truncated, full output: {path})"));
            } else {
                status_parts.push("(truncated)".to_string());
            }
        }

        if !status_parts.is_empty() {
            parts.push(status_parts.join(" "));
        }

        parts.join("\n")
    }
}

// Type guards for AppMessage variants

impl AppMessage {
    /// Check if this is a user message
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, AppMessage::User { .. })
    }

    /// Check if this is an assistant message
    #[must_use]
    pub fn is_assistant(&self) -> bool {
        matches!(
            self,
            AppMessage::Assistant { .. } | AppMessage::AssistantBlocks { .. }
        )
    }

    /// Check if this is a bash execution
    #[must_use]
    pub fn is_bash_execution(&self) -> bool {
        matches!(self, AppMessage::BashExecution(_))
    }

    /// Check if this is a tool result
    #[must_use]
    pub fn is_tool_result(&self) -> bool {
        matches!(self, AppMessage::ToolResult { .. })
    }

    /// Check if this is a system message
    #[must_use]
    pub fn is_system(&self) -> bool {
        matches!(self, AppMessage::System { .. })
    }

    /// Check if this is a context summary
    #[must_use]
    pub fn is_context_summary(&self) -> bool {
        matches!(self, AppMessage::ContextSummary { .. })
    }

    /// Get the timestamp if available
    #[must_use]
    pub fn timestamp(&self) -> Option<u64> {
        match self {
            AppMessage::User { timestamp, .. } => *timestamp,
            AppMessage::Assistant { timestamp, .. } => *timestamp,
            AppMessage::AssistantBlocks { timestamp, .. } => *timestamp,
            AppMessage::ToolResult { timestamp, .. } => *timestamp,
            AppMessage::BashExecution(bash) => bash.timestamp,
            AppMessage::System { timestamp, .. } => *timestamp,
            AppMessage::ContextSummary { timestamp, .. } => *timestamp,
        }
    }

    /// Convert to standard API message
    ///
    /// Returns None for message types that shouldn't be sent to the API
    /// (like System messages).
    #[must_use]
    pub fn to_api_message(&self) -> Option<Message> {
        match self {
            AppMessage::User { content, .. } => Some(Message {
                role: Role::User,
                content: MessageContent::Text(content.clone()),
            }),

            AppMessage::Assistant { content, .. } => Some(Message {
                role: Role::Assistant,
                content: MessageContent::Text(content.clone()),
            }),

            AppMessage::AssistantBlocks { blocks, .. } => Some(Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(blocks.clone()),
            }),

            AppMessage::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => Some(Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                }]),
            }),

            AppMessage::BashExecution(bash) => Some(Message {
                role: Role::User,
                content: MessageContent::Text(bash.to_user_content()),
            }),

            AppMessage::System { .. } => {
                // System messages are handled separately (as system prompt)
                None
            }

            AppMessage::ContextSummary { summary, .. } => Some(Message {
                role: Role::User,
                content: MessageContent::Text(format!(
                    "<context_summary>\n{summary}\n</context_summary>\n\nPlease continue from where we left off."
                )),
            }),
        }
    }
}

/// Transform a slice of `AppMessages` to API-compatible Messages.
///
/// Filters out message types that shouldn't be sent to the API (like System messages)
/// and converts custom types to their API representation.
///
/// # Arguments
///
/// * `app_messages` - The application-level messages to transform
///
/// # Returns
///
/// A vector of standard Message types ready for the API
#[must_use]
pub fn transform_to_api_messages(app_messages: &[AppMessage]) -> Vec<Message> {
    app_messages
        .iter()
        .filter_map(AppMessage::to_api_message)
        .collect()
}

/// Convert standard Messages to `AppMessages`.
///
/// This is useful when loading messages from an API response or legacy format.
#[must_use]
pub fn from_api_messages(messages: &[Message]) -> Vec<AppMessage> {
    messages
        .iter()
        .map(|m| match m.role {
            Role::User => match &m.content {
                MessageContent::Text(text) => AppMessage::User {
                    content: text.clone(),
                    timestamp: None,
                },
                MessageContent::Blocks(blocks) => {
                    // Check if it's a tool result
                    if let Some(ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    }) = blocks.first()
                    {
                        AppMessage::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: *is_error,
                            timestamp: None,
                        }
                    } else {
                        // Convert blocks to text
                        let text: String = blocks
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text }
                                | ContentBlock::AttributedText { text, .. } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        AppMessage::User {
                            content: text,
                            timestamp: None,
                        }
                    }
                }
            },
            Role::Assistant => match &m.content {
                MessageContent::Text(text) => AppMessage::Assistant {
                    content: text.clone(),
                    timestamp: None,
                },
                MessageContent::Blocks(blocks) => AppMessage::AssistantBlocks {
                    blocks: blocks.clone(),
                    timestamp: None,
                },
            },
            Role::System => {
                let content = match &m.content {
                    MessageContent::Text(text) => text.clone(),
                    MessageContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text }
                            | ContentBlock::AttributedText { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                AppMessage::System {
                    content,
                    timestamp: None,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bash_execution_to_user_content() {
        let bash = BashExecution::new("ls -la", "file1.txt\nfile2.txt", 0);
        let content = bash.to_user_content();

        assert!(content.contains("$ ls -la"));
        assert!(content.contains("file1.txt"));
        assert!(!content.contains("exit code")); // Success doesn't show exit code
    }

    #[test]
    fn test_bash_execution_with_error() {
        let bash = BashExecution::new("cat nonexistent", "No such file", 1);
        let content = bash.to_user_content();

        assert!(content.contains("exit code: 1"));
    }

    #[test]
    fn test_bash_execution_truncated() {
        let bash = BashExecution::new("cat large_file", "...", 0)
            .with_truncation(Some("/tmp/output.log".to_string()));

        let content = bash.to_user_content();
        assert!(content.contains("truncated"));
        assert!(content.contains("/tmp/output.log"));
    }

    #[test]
    fn test_bash_execution_cancelled() {
        let mut bash = BashExecution::new("sleep 1000", "", 130);
        bash.cancelled = true;

        let content = bash.to_user_content();
        assert!(content.contains("cancelled"));
    }

    #[test]
    fn test_transform_to_api_messages() {
        let app_messages = vec![
            AppMessage::User {
                content: "Hello".to_string(),
                timestamp: None,
            },
            AppMessage::BashExecution(BashExecution::new("echo hi", "hi", 0)),
            AppMessage::System {
                content: "System prompt".to_string(),
                timestamp: None,
            },
            AppMessage::Assistant {
                content: "Response".to_string(),
                timestamp: None,
            },
        ];

        let api_messages = transform_to_api_messages(&app_messages);

        // System message should be filtered out
        assert_eq!(api_messages.len(), 3);
        assert_eq!(api_messages[0].role, Role::User);
        assert_eq!(api_messages[1].role, Role::User); // Bash execution becomes user message
        assert_eq!(api_messages[2].role, Role::Assistant);
    }

    #[test]
    fn test_app_message_type_guards() {
        let user = AppMessage::User {
            content: "Hi".to_string(),
            timestamp: None,
        };
        assert!(user.is_user());
        assert!(!user.is_assistant());

        let bash = AppMessage::BashExecution(BashExecution::default());
        assert!(bash.is_bash_execution());
        assert!(!bash.is_user());
    }

    #[test]
    fn test_from_api_messages() {
        let api_messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("Hello".to_string()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("Hi there!".to_string()),
            },
        ];

        let app_messages = from_api_messages(&api_messages);

        assert_eq!(app_messages.len(), 2);
        assert!(app_messages[0].is_user());
        assert!(app_messages[1].is_assistant());
    }

    #[test]
    fn test_context_summary_to_api() {
        let summary = AppMessage::ContextSummary {
            summary: "Previous discussion about files".to_string(),
            compacted_count: 10,
            timestamp: None,
        };

        let api = summary.to_api_message().unwrap();
        assert_eq!(api.role, Role::User);
        assert!(api.content.as_text().unwrap().contains("context_summary"));
    }

    #[test]
    fn test_tool_result_to_api() {
        let tool_result = AppMessage::ToolResult {
            tool_use_id: "call_123".to_string(),
            content: "file contents".to_string(),
            is_error: Some(false),
            timestamp: None,
        };

        let api = tool_result.to_api_message().unwrap();
        assert_eq!(api.role, Role::User);

        if let MessageContent::Blocks(blocks) = api.content {
            assert!(matches!(blocks[0], ContentBlock::ToolResult { .. }));
        } else {
            panic!("Expected blocks");
        }
    }

    #[test]
    fn test_bash_execution_succeeded() {
        let success = BashExecution::new("echo hi", "hi", 0);
        assert!(success.succeeded());

        let failed = BashExecution::new("exit 1", "", 1);
        assert!(!failed.succeeded());

        let mut cancelled = BashExecution::new("sleep", "", 0);
        cancelled.cancelled = true;
        assert!(!cancelled.succeeded());
    }
}
