//! Clipboard integration
//!
//! This module provides cross-platform clipboard access for copy/paste operations in the TUI.
//! It uses the `arboard` crate which supports X11, Wayland, macOS, and Windows.
//!
//! # Feature Flag
//!
//! Clipboard support is gated behind the `clipboard` feature flag. When the feature is
//! disabled, all clipboard operations return `ClipboardError::FeatureDisabled`.
//!
//! This allows the crate to compile on platforms without clipboard support or in
//! environments where clipboard access is not needed.
//!
//! # Platform Support
//!
//! - **Linux/Unix**: Supports both X11 and Wayland via `arboard`
//! - **macOS**: Uses NSPasteboard APIs
//! - **Windows**: Uses Win32 clipboard APIs
//! - **Headless/SSH**: Gracefully handles clipboard unavailability
//!
//! # External Crates
//!
//! - **arboard**: Cross-platform clipboard library with support for text, images, and HTML
//! - **thiserror**: Used for deriving the `Error` trait on `ClipboardError`
//!
//! # Conditional Compilation
//!
//! This module uses `#[cfg(feature = "clipboard")]` extensively to provide stub
//! implementations when the feature is disabled, allowing the code to compile without
//! pulling in platform-specific clipboard dependencies.

#[cfg(feature = "clipboard")]
use arboard::Clipboard;

/// Result type alias for clipboard operations.
///
/// Convenience alias for `Result<T, ClipboardError>` used throughout this module.
pub type ClipboardResult<T> = Result<T, ClipboardError>;

/// Errors that can occur during clipboard operations.
///
/// These errors cover both feature-level issues (clipboard feature not enabled) and
/// runtime issues (clipboard access denied, clipboard server not running, etc.).
#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("Clipboard not available: {0}")]
    NotAvailable(String),

    #[error("Failed to access clipboard: {0}")]
    AccessFailed(String),

    #[error("Clipboard feature not enabled")]
    FeatureDisabled,
}

/// Clipboard manager for copy/paste operations.
///
/// This struct wraps the `arboard::Clipboard` instance and provides a simplified API
/// for text clipboard operations. It handles the feature flag conditionally, providing
/// stub implementations when clipboard support is disabled.
///
/// # Example
///
/// ```no_run
/// use maestro_tui_kit::clipboard::ClipboardManager;
///
/// let mut clipboard = ClipboardManager::new();
/// if clipboard.is_available() {
///     clipboard.copy("Hello, clipboard!").ok();
///     if let Ok(text) = clipboard.paste() {
///         println!("Pasted: {}", text);
///     }
/// }
/// ```
pub struct ClipboardManager {
    #[cfg(feature = "clipboard")]
    clipboard: Option<Clipboard>,
}

impl Default for ClipboardManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardManager {
    /// Create a new clipboard manager
    #[must_use]
    pub fn new() -> Self {
        #[cfg(feature = "clipboard")]
        {
            let clipboard = Clipboard::new().ok();
            Self { clipboard }
        }

        #[cfg(not(feature = "clipboard"))]
        {
            Self {}
        }
    }

    /// Check if clipboard is available
    #[must_use]
    pub fn is_available(&self) -> bool {
        #[cfg(feature = "clipboard")]
        {
            self.clipboard.is_some()
        }

        #[cfg(not(feature = "clipboard"))]
        {
            false
        }
    }

    /// Copy text to clipboard
    pub fn copy(&mut self, text: &str) -> ClipboardResult<()> {
        #[cfg(feature = "clipboard")]
        {
            let clipboard = self
                .clipboard
                .as_mut()
                .ok_or_else(|| ClipboardError::NotAvailable("Clipboard not initialized".into()))?;

            clipboard
                .set_text(text)
                .map_err(|e| ClipboardError::AccessFailed(e.to_string()))
        }

        #[cfg(not(feature = "clipboard"))]
        {
            let _ = text;
            Err(ClipboardError::FeatureDisabled)
        }
    }

    /// Paste text from clipboard
    pub fn paste(&mut self) -> ClipboardResult<String> {
        #[cfg(feature = "clipboard")]
        {
            let clipboard = self
                .clipboard
                .as_mut()
                .ok_or_else(|| ClipboardError::NotAvailable("Clipboard not initialized".into()))?;

            clipboard
                .get_text()
                .map_err(|e| ClipboardError::AccessFailed(e.to_string()))
        }

        #[cfg(not(feature = "clipboard"))]
        {
            Err(ClipboardError::FeatureDisabled)
        }
    }

    /// Copy code block content (strips leading/trailing whitespace)
    pub fn copy_code_block(&mut self, code: &str) -> ClipboardResult<()> {
        let trimmed = code.trim();
        self.copy(trimmed)
    }

    /// Copy message content
    pub fn copy_message(&mut self, content: &str) -> ClipboardResult<()> {
        self.copy(content)
    }

    /// Paste an image from the system clipboard into a temporary PNG file.
    ///
    /// Returns the absolute path of the written file. Used by Ctrl+Y when the
    /// clipboard holds image data instead of text (Kimi-style multimodal paste).
    pub fn paste_image_to_temp_file(&mut self) -> ClipboardResult<std::path::PathBuf> {
        #[cfg(feature = "clipboard")]
        {
            use std::io::Write;

            let clipboard = self
                .clipboard
                .as_mut()
                .ok_or_else(|| ClipboardError::NotAvailable("Clipboard not initialized".into()))?;

            let image = clipboard
                .get_image()
                .map_err(|e| ClipboardError::AccessFailed(e.to_string()))?;

            let width = u32::try_from(image.width)
                .map_err(|_| ClipboardError::AccessFailed("image width does not fit u32".into()))?;
            let height = u32::try_from(image.height).map_err(|_| {
                ClipboardError::AccessFailed("image height does not fit u32".into())
            })?;

            let rgba = image::RgbaImage::from_raw(width, height, image.bytes.into_owned())
                .ok_or_else(|| {
                    ClipboardError::AccessFailed(
                        "clipboard image bytes do not match width×height×4".into(),
                    )
                })?;

            let dir = crate::path_utils::maestro_home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("clipboard");
            std::fs::create_dir_all(&dir)
                .map_err(|e| ClipboardError::AccessFailed(format!("create clipboard dir: {e}")))?;

            let path = dir.join(format!(
                "paste-{}.png",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ));

            let mut file = std::fs::File::create(&path).map_err(|e| {
                ClipboardError::AccessFailed(format!("create {}: {e}", path.display()))
            })?;
            let dyn_img = image::DynamicImage::ImageRgba8(rgba);
            dyn_img
                .write_to(&mut file, image::ImageFormat::Png)
                .map_err(|e| ClipboardError::AccessFailed(format!("encode png: {e}")))?;
            file.flush()
                .map_err(|e| ClipboardError::AccessFailed(format!("flush png: {e}")))?;
            Ok(path)
        }

        #[cfg(not(feature = "clipboard"))]
        {
            Err(ClipboardError::FeatureDisabled)
        }
    }
}

/// Check if clipboard feature is compiled in.
///
/// Returns `true` if the crate was built with the `clipboard` feature enabled.
/// This can be used to conditionally enable clipboard-related UI elements.
///
/// This is a const function and can be used in const contexts.
#[must_use]
pub const fn is_feature_enabled() -> bool {
    cfg!(feature = "clipboard")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clipboard_manager_creation() {
        let manager = ClipboardManager::new();
        // Just ensure no panic on creation
        let _ = manager.is_available();
    }

    #[test]
    fn test_feature_enabled_flag() {
        // This should compile regardless of feature flag
        let enabled = is_feature_enabled();
        #[cfg(feature = "clipboard")]
        assert!(enabled);
        #[cfg(not(feature = "clipboard"))]
        assert!(!enabled);
    }

    // Note: Clipboard tests that actually access the system clipboard are marked
    // as ignored by default because they can crash in headless environments (CI).
    // Run them manually with: cargo test --features clipboard -- --ignored

    #[cfg(feature = "clipboard")]
    #[test]
    #[ignore = "Requires display environment for clipboard access"]
    fn test_copy_paste_roundtrip() {
        let mut manager = ClipboardManager::new();
        if manager.is_available() {
            let test_text = "Hello, clipboard!";
            assert!(manager.copy(test_text).is_ok());
            let result = manager.paste();
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), test_text);
        }
    }

    #[cfg(feature = "clipboard")]
    #[test]
    #[ignore = "Requires display environment for clipboard access"]
    fn test_copy_code_block() {
        let mut manager = ClipboardManager::new();
        if manager.is_available() {
            let code = "\n  fn main() {}\n  ";
            assert!(manager.copy_code_block(code).is_ok());
            let result = manager.paste();
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), "fn main() {}");
        }
    }
}
