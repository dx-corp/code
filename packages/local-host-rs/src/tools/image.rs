//! Image Tool for Vision-Capable Models
//!
//! This module provides tools for working with images in the agent:
//!
//! - **`read_image`**: Read an image file and encode it for vision APIs
//! - **screenshot**: Capture a screenshot of the screen or a window
//!
//! # Supported Formats
//!
//! - PNG (image/png)
//! - JPEG (image/jpeg)
//! - GIF (image/gif)
//! - WebP (image/webp)
//!
//! # Usage
//!
//! Images are returned as base64-encoded data URIs that can be included
//! in message content for vision-capable models like Claude 3.5 Sonnet,
//! GPT-4 Vision, and Gemini Pro Vision.
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_local_host::tools::image::{ImageTool, ReadImageArgs};
//!
//! let tool = ImageTool::new();
//! let result = tool.read_image(ReadImageArgs {
//!     file_path: "/path/to/image.png".to_string(),
//!     max_dimension: Some(1024),
//! }).await;
//!
//! if result.success {
//!     // result.output contains base64-encoded image data
//! }
//! ```

use std::io::Cursor;
use std::path::Path;
use std::time::Instant;

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{GenericImageView, ImageFormat, imageops::FilterType};
use serde::{Deserialize, Serialize};

use super::details::ImageDetails;
use crate::agent::ToolResult;
use crate::ai::Tool;

// Persist only a small projection of already admitted bytes. Presentation must
// not reopen a path, which may be remote or change after the approved tool read.
fn with_preview(details: ImageDetails, data: &[u8]) -> serde_json::Value {
    let mut details = details.to_json();
    if let Some(preview) = bounded_preview(data) {
        details["image_preview"] = serde_json::json!({
            "mime_type": "image/png",
            "data": STANDARD.encode(preview),
        });
    }
    details
}

fn bounded_preview(data: &[u8]) -> Option<Vec<u8>> {
    const MAX_PREVIEW_PNG_BYTES: usize = 512 * 1024;
    if data.len() > MAX_IMAGE_SIZE {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    // The bounded raster also bounds PNG encoder work and temporary allocation.
    let thumbnail = if decoded.width() <= 512 && decoded.height() <= 192 {
        decoded.to_rgba8()
    } else {
        decoded.thumbnail(512, 192).to_rgba8()
    };
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(thumbnail)
        .write_to(&mut encoded, ImageFormat::Png)
        .ok()?;
    let encoded = encoded.into_inner();
    (encoded.len() <= MAX_PREVIEW_PNG_BYTES).then_some(encoded)
}

/// Maximum file size for images (10MB)
const MAX_IMAGE_SIZE: usize = 10 * 1024 * 1024;

/// Default max dimension for image resizing
const DEFAULT_MAX_DIMENSION: u32 = 2048;

/// Arguments for reading an image file
#[derive(Debug, Serialize, Deserialize)]
pub struct ReadImageArgs {
    /// Path to the image file
    pub file_path: String,
    /// Maximum dimension (width or height) for resizing
    /// Images larger than this will be scaled down
    #[serde(default)]
    pub max_dimension: Option<u32>,
}

/// Arguments for capturing a screenshot
#[derive(Debug, Serialize, Deserialize)]
pub struct ScreenshotArgs {
    /// Optional window title to capture (if not specified, captures full screen)
    #[serde(default)]
    pub window_title: Option<String>,
    /// Optional delay in milliseconds before capture
    #[serde(default)]
    pub delay_ms: Option<u64>,
}

/// Image tool for reading images and capturing screenshots
pub struct ImageTool;

impl ImageTool {
    /// Create a new `ImageTool` instance
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Get the tool definition for `read_image`
    #[must_use]
    pub fn read_image_definition() -> Tool {
        Tool::new(
            "read_image",
            "Read an IMAGE file (PNG, JPEG, GIF, WebP only) and encode it for vision analysis. DO NOT use for text files like .txt, .md, .rs, .json - use the 'read' tool instead. Returns base64-encoded image data.",
        )
        .with_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the image file to read"
                },
                "max_dimension": {
                    "type": "integer",
                    "description": "Optional maximum dimension (width or height) for resizing. Default 2048."
                }
            },
            "required": ["file_path"]
        }))
    }

    /// Get the tool definition for screenshot
    #[must_use]
    pub fn screenshot_definition() -> Tool {
        Tool::new(
            "screenshot",
            "Capture a screenshot of the screen or a specific window. Returns base64-encoded PNG image.",
        )
        .with_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "window_title": {
                    "type": "string",
                    "description": "Optional window title to capture. If not specified, captures the entire screen."
                },
                "delay_ms": {
                    "type": "integer",
                    "description": "Optional delay in milliseconds before capturing. Useful for capturing menus or tooltips."
                }
            },
            "required": []
        }))
    }

    /// Read an image file and return base64-encoded data
    pub async fn read_image(&self, args: ReadImageArgs) -> ToolResult {
        let start_time = Instant::now();
        let path = Path::new(&args.file_path);

        // Check if file exists
        if !path.exists() {
            let details = ImageDetails::from_file(&args.file_path)
                .with_duration(start_time.elapsed().as_millis() as u64);
            return ToolResult::failure(format!("Image file not found: {}", args.file_path))
                .with_details(details.to_json());
        }

        // Determine MIME type from extension (case-insensitive)
        let extension = path.extension().and_then(|e| e.to_str());
        let extension_lower = extension.map(str::to_ascii_lowercase);
        let mime_type = match extension_lower.as_deref() {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("bmp") => "image/bmp",
            Some("svg") => "image/svg+xml",
            Some(ext) => {
                let details = ImageDetails::from_file(&args.file_path)
                    .with_duration(start_time.elapsed().as_millis() as u64);
                return ToolResult::failure(format!(
                    "Not an image file: .{ext} is not a supported image format. Use the 'read' tool for text files (.txt, .md, .rs, .json, etc). Supported image formats: PNG, JPEG, GIF, WebP, BMP, SVG"
                )).with_details(details.to_json());
            }
            None => {
                let details = ImageDetails::from_file(&args.file_path)
                    .with_duration(start_time.elapsed().as_millis() as u64);
                return ToolResult::failure("Could not determine image format from extension")
                    .with_details(details.to_json());
            }
        };

        // Read the file
        let data = match tokio::fs::read(path).await {
            Ok(data) => data,
            Err(e) => {
                let details = ImageDetails::from_file(&args.file_path)
                    .with_mime_type(mime_type)
                    .with_duration(start_time.elapsed().as_millis() as u64);
                return ToolResult::failure(format!("Failed to read image: {e}"))
                    .with_details(details.to_json());
            }
        };

        // Check file size
        if data.len() > MAX_IMAGE_SIZE {
            let details = ImageDetails::from_file(&args.file_path)
                .with_mime_type(mime_type)
                .with_size(data.len() as u64)
                .with_duration(start_time.elapsed().as_millis() as u64);
            return ToolResult::failure(format!(
                "Image too large: {} bytes (max {} bytes)",
                data.len(),
                MAX_IMAGE_SIZE
            ))
            .with_details(details.to_json());
        }

        let mut mime_type = mime_type.to_string();
        let mut data = data;

        // Get image dimensions info if possible
        let mut dimensions = get_image_dimensions(&data);
        let mut original_dimensions = dimensions;
        let mut resized = false;
        let max_dim = args.max_dimension.unwrap_or(DEFAULT_MAX_DIMENSION);

        // Resize raster images when they exceed max dimension
        if matches!(
            mime_type.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
        ) {
            if let Ok(img) = image::load_from_memory(&data) {
                let (width, height) = img.dimensions();
                original_dimensions = Some((width, height));
                dimensions = Some((width, height));

                if width > max_dim || height > max_dim {
                    let scale = max_dim as f32 / width.max(height) as f32;
                    let new_width = ((width as f32 * scale).round() as u32).max(1);
                    let new_height = ((height as f32 * scale).round() as u32).max(1);
                    let resized_img = img.resize(new_width, new_height, FilterType::Triangle);

                    let mut buffer = Vec::new();
                    let mut encoded = false;
                    let format = match mime_type.as_str() {
                        "image/png" => Some(ImageFormat::Png),
                        "image/jpeg" => Some(ImageFormat::Jpeg),
                        "image/gif" => Some(ImageFormat::Gif),
                        "image/webp" => Some(ImageFormat::WebP),
                        "image/bmp" => Some(ImageFormat::Bmp),
                        _ => None,
                    };

                    if let Some(format) = format {
                        if resized_img
                            .write_to(&mut Cursor::new(&mut buffer), format)
                            .is_ok()
                        {
                            encoded = true;
                        }
                    }

                    if !encoded {
                        buffer.clear();
                        if resized_img
                            .write_to(&mut Cursor::new(&mut buffer), ImageFormat::Png)
                            .is_ok()
                        {
                            encoded = true;
                            mime_type = "image/png".to_string();
                        }
                    }

                    if encoded {
                        data = buffer;
                        dimensions = Some((new_width, new_height));
                        resized = true;
                    }
                }
            }
        }

        let dim_info = match (original_dimensions, dimensions) {
            (Some((ow, oh)), Some((w, h))) if resized => {
                format!(" ({ow}x{oh} -> {w}x{h}, max {max_dim})")
            }
            (Some((w, h)), _) => {
                if w > max_dim || h > max_dim {
                    format!(" ({w}x{h}, exceeds max {max_dim})")
                } else {
                    format!(" ({w}x{h})")
                }
            }
            _ => String::new(),
        };

        // Encode to base64 (after optional resize)
        let base64_data = STANDARD.encode(&data);

        // Format as data URI for easy consumption
        let data_uri = format!("data:{mime_type};base64,{base64_data}");

        // Return structured output
        let output = serde_json::json!({
            "mime_type": mime_type.as_str(),
            "size_bytes": data.len(),
            "dimensions": dimensions.map(|(w, h)| format!("{w}x{h}")),
            "original_dimensions": if resized {
                original_dimensions.map(|(w, h)| format!("{w}x{h}"))
            } else {
                None
            },
            "resized": resized,
            "base64_length": base64_data.len(),
            "data_uri": data_uri,
        });

        // Build ImageDetails
        let mut details = ImageDetails::from_file(&args.file_path)
            .with_mime_type(mime_type.clone())
            .with_size(data.len() as u64)
            .with_base64_length(base64_data.len())
            .with_duration(start_time.elapsed().as_millis() as u64);
        if let Some((w, h)) = dimensions {
            details = details.with_dimensions(w, h);
        }

        ToolResult::success(format!(
            "Image loaded: {} ({} bytes){}\n\n{}",
            args.file_path,
            data.len(),
            dim_info,
            serde_json::to_string_pretty(&output).unwrap_or_default()
        ))
        .with_details(with_preview(details, &data))
    }

    /// Capture a screenshot
    pub async fn screenshot(&self, args: ScreenshotArgs) -> ToolResult {
        let start_time = Instant::now();

        // Apply delay if specified
        if let Some(delay) = args.delay_ms {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }

        // Try platform-specific screenshot tools
        let result = if cfg!(target_os = "linux") {
            capture_screenshot_linux(args.window_title.as_deref()).await
        } else if cfg!(target_os = "macos") {
            capture_screenshot_macos(args.window_title.as_deref()).await
        } else if cfg!(target_os = "windows") {
            capture_screenshot_windows(args.window_title.as_deref()).await
        } else {
            Err("Screenshot not supported on this platform".to_string())
        };

        match result {
            Ok((data, mime_type)) => {
                let base64_data = STANDARD.encode(&data);
                let dimensions = get_image_dimensions(&data);
                let data_uri = format!("data:{mime_type};base64,{base64_data}");

                let output = serde_json::json!({
                    "mime_type": mime_type,
                    "size_bytes": data.len(),
                    "dimensions": dimensions.map(|(w, h)| format!("{w}x{h}")),
                    "base64_length": base64_data.len(),
                    "data_uri": data_uri,
                });

                // Build ImageDetails for screenshot
                let mut details = ImageDetails::screenshot()
                    .with_mime_type(mime_type)
                    .with_size(data.len() as u64)
                    .with_base64_length(base64_data.len())
                    .with_duration(start_time.elapsed().as_millis() as u64);
                if let Some((w, h)) = dimensions {
                    details = details.with_dimensions(w, h);
                }

                ToolResult::success(format!(
                    "Screenshot captured ({} bytes)\n\n{}",
                    data.len(),
                    serde_json::to_string_pretty(&output).unwrap_or_default()
                ))
                .with_details(with_preview(details, &data))
            }
            Err(e) => {
                let details = ImageDetails::screenshot()
                    .with_duration(start_time.elapsed().as_millis() as u64);
                ToolResult::failure(format!("Screenshot failed: {e}"))
                    .with_details(details.to_json())
            }
        }
    }
}

impl Default for ImageTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Get image dimensions from raw data (PNG/JPEG header parsing)
fn get_image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // PNG signature: 89 50 4E 47 0D 0A 1A 0A
    if data.len() >= 24 && data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        // PNG IHDR chunk starts at byte 8, dimensions at 16-23
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((width, height));
    }

    // JPEG signature: FF D8 FF
    if data.len() >= 2 && data.starts_with(&[0xFF, 0xD8]) {
        // JPEG dimensions are in SOF0/SOF2 markers, more complex to parse
        // For now, return None for JPEG (could be improved)
        return None;
    }

    // GIF signature: GIF87a or GIF89a
    if data.len() >= 10 && (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        let width = u32::from(u16::from_le_bytes([data[6], data[7]]));
        let height = u32::from(u16::from_le_bytes([data[8], data[9]]));
        return Some((width, height));
    }

    None
}

/// Run a screenshot helper without blocking the async runtime.
///
/// `kill_on_drop` ensures that cancelling the screenshot future also
/// terminates an interactive helper such as macOS `screencapture -w`.
async fn run_screenshot_command(
    program: &str,
    args: &[&str],
) -> Result<std::process::Output, std::io::Error> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).kill_on_drop(true);
    command.output().await
}

fn screenshot_temp_path() -> Result<(tempfile::TempDir, std::path::PathBuf), String> {
    let temp_dir = tempfile::Builder::new()
        .prefix("composer_screenshot_")
        .tempdir()
        .map_err(|error| format!("Failed to create screenshot path: {error}"))?;
    let temp_path = temp_dir.path().join("screenshot.png");
    Ok((temp_dir, temp_path))
}

/// Capture screenshot on Linux using scrot or gnome-screenshot
async fn capture_screenshot_linux(
    window_title: Option<&str>,
) -> Result<(Vec<u8>, &'static str), String> {
    let (_temp_dir, temp_path) = screenshot_temp_path()?;
    let temp_str = temp_path.to_string_lossy().to_string();

    // Try different screenshot tools
    let tools = [
        ("scrot", vec!["-o", &temp_str]),
        ("gnome-screenshot", vec!["-f", &temp_str]),
        ("maim", vec![&temp_str]),
        ("import", vec!["-window", "root", &temp_str]), // ImageMagick
    ];

    let mut last_error = String::new();

    for (tool, mut args) in tools {
        // Add window selection if specified
        if let Some(_title) = window_title {
            if tool == "scrot" {
                args = vec!["-u", "-o", &temp_str]; // Focused window
            }
        }

        match run_screenshot_command(tool, &args).await {
            Ok(output) if output.status.success() => {
                // Read the captured image
                match tokio::fs::read(&temp_path).await {
                    Ok(data) => {
                        return Ok((data, "image/png"));
                    }
                    Err(e) => {
                        last_error = format!("Failed to read screenshot: {e}");
                    }
                }
            }
            Ok(output) => {
                last_error = format!(
                    "{} failed: {}",
                    tool,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(_) => {
                // Tool not found, try next
                continue;
            }
        }
    }

    Err(format!(
        "No screenshot tool available. Install scrot, gnome-screenshot, or maim. Last error: {last_error}"
    ))
}

/// Capture screenshot on macOS using screencapture
async fn capture_screenshot_macos(
    window_title: Option<&str>,
) -> Result<(Vec<u8>, &'static str), String> {
    let (_temp_dir, temp_path) = screenshot_temp_path()?;
    let temp_str = temp_path.to_string_lossy().to_string();

    let mut args = vec!["-x"]; // Silent mode

    if window_title.is_some() {
        args.push("-w"); // Interactive window selection
    }

    args.push(&temp_str);

    match run_screenshot_command("screencapture", &args).await {
        Ok(output) if output.status.success() => match tokio::fs::read(&temp_path).await {
            Ok(data) => Ok((data, "image/png")),
            Err(e) => Err(format!("Failed to read screenshot: {e}")),
        },
        Ok(output) => Err(format!(
            "screencapture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
        Err(e) => Err(format!("Failed to run screencapture: {e}")),
    }
}

/// Capture screenshot on Windows using `PowerShell`
async fn capture_screenshot_windows(
    _window_title: Option<&str>,
) -> Result<(Vec<u8>, &'static str), String> {
    let (_temp_dir, temp_path) = screenshot_temp_path()?;
    let temp_str = temp_path.to_string_lossy().to_string();

    // PowerShell script to capture screen
    let script = format!(
        r"
        Add-Type -AssemblyName System.Windows.Forms
        $screen = [System.Windows.Forms.Screen]::PrimaryScreen
        $bitmap = New-Object System.Drawing.Bitmap($screen.Bounds.Width, $screen.Bounds.Height)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.CopyFromScreen($screen.Bounds.Location, [System.Drawing.Point]::Empty, $screen.Bounds.Size)
        $bitmap.Save('{}')
        $graphics.Dispose()
        $bitmap.Dispose()
        ",
        temp_str.replace('\\', "\\\\").replace('\'', "''")
    );

    match run_screenshot_command("powershell", &["-Command", &script]).await {
        Ok(output) if output.status.success() => match tokio::fs::read(&temp_path).await {
            Ok(data) => Ok((data, "image/png")),
            Err(e) => Err(format!("Failed to read screenshot: {e}")),
        },
        Ok(output) => Err(format!(
            "PowerShell screenshot failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
        Err(e) => Err(format!("Failed to run PowerShell: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_image_returns_admitted_preview_pixels_without_a_second_file_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preview.png");
        image::DynamicImage::new_rgb8(16, 16).save(&path).unwrap();
        let result = ImageTool::new()
            .read_image(ReadImageArgs {
                file_path: path.display().to_string(),
                max_dimension: None,
            })
            .await;
        assert!(result.success);
        std::fs::remove_file(path).unwrap();
        let preview = result.details.unwrap()["image_preview"].clone();
        let preview: maestro_runtime_contracts::tool_operation::CodeModeImage =
            serde_json::from_value(preview).unwrap();
        assert_eq!(preview.mime_type, "image/png");
        let pixels = STANDARD.decode(preview.data).unwrap();
        assert_eq!(
            image::load_from_memory(&pixels).unwrap().dimensions(),
            (16, 16)
        );
    }

    #[tokio::test]
    async fn persisted_preview_is_bounded_and_survives_source_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.png");
        let pixels = image::RgbImage::from_fn(1024, 768, |x, y| {
            image::Rgb([
                (x * 73 + y * 31) as u8,
                (x * 29 + y * 97) as u8,
                (x ^ y) as u8,
            ])
        });
        pixels.save(&path).unwrap();
        let admitted = std::fs::read(&path).unwrap();
        let result = ImageTool::new()
            .read_image(ReadImageArgs {
                file_path: path.display().to_string(),
                max_dimension: None,
            })
            .await;
        assert!(result.success);
        let persisted = serde_json::to_vec(&result).unwrap();
        std::fs::remove_file(&path).unwrap();
        let restored: ToolResult = serde_json::from_slice(&persisted).unwrap();
        let output: serde_json::Value =
            serde_json::from_str(restored.output.split_once("\n\n").unwrap().1).unwrap();
        let full_uri = output["data_uri"].as_str().unwrap();
        assert_eq!(
            STANDARD
                .decode(full_uri.split_once(',').unwrap().1)
                .unwrap(),
            admitted
        );
        let preview = &restored.details.unwrap()["image_preview"];
        assert_eq!(preview["mime_type"], "image/png");
        let encoded = preview["data"].as_str().unwrap();
        let bytes = STANDARD.decode(encoded).unwrap();
        assert!(bytes.len() <= 512 * 1024);
        assert!(bytes.len() < admitted.len());
        let dimensions = image::load_from_memory(&bytes).unwrap().dimensions();
        assert_eq!(dimensions, (256, 192));
    }

    #[test]
    fn screenshot_preview_uses_the_same_bounded_admitted_bytes_projection() {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1600, 900)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        let details = with_preview(
            ImageDetails::screenshot().with_mime_type("image/png"),
            bytes.get_ref(),
        );
        let preview = STANDARD
            .decode(details["image_preview"]["data"].as_str().unwrap())
            .unwrap();
        let (width, height) = image::load_from_memory(&preview).unwrap().dimensions();
        assert!(width <= 512 && height <= 192);
        assert!(preview.len() <= 512 * 1024);
    }

    #[test]
    fn preview_omits_unsupported_oversized_and_excessive_dimension_sources() {
        for bytes in [b"<svg/>".to_vec(), vec![0; 10 * 1024 * 1024 + 1]] {
            let details = with_preview(
                ImageDetails::screenshot().with_mime_type("image/png"),
                &bytes,
            );
            assert!(details.get("image_preview").is_none());
        }
        let mut oversized = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8193, 1)
            .write_to(&mut oversized, ImageFormat::Png)
            .unwrap();
        let details = with_preview(
            ImageDetails::screenshot().with_mime_type("image/png"),
            oversized.get_ref(),
        );
        assert!(details.get("image_preview").is_none());
    }

    #[test]
    fn screenshot_temp_path_is_nonexistent_and_guarded() {
        let (temp_dir, temp_path) = screenshot_temp_path().expect("temporary path");
        let directory = temp_dir.path().to_path_buf();

        assert!(
            !temp_path.exists(),
            "the screenshot tool creates the output"
        );
        drop(temp_dir);
        assert!(
            !directory.exists(),
            "dropping the guard cleans up the directory"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn screenshot_command_can_be_cancelled_without_blocking_the_runtime() {
        use std::time::Duration;

        let pid_path = std::env::temp_dir().join(format!(
            "maestro_screenshot_command_test_{}.pid",
            std::process::id()
        ));
        let command = format!("echo $$ > {}; exec sleep 2", pid_path.display());

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            run_screenshot_command("sh", &["-c", &command]),
        )
        .await;
        assert!(
            result.is_err(),
            "the command should be cancelled by timeout"
        );

        let pid = tokio::fs::read_to_string(&pid_path)
            .await
            .expect("the child should record its pid");
        let pid = pid.trim();
        let mut alive = true;
        for _ in 0..20 {
            alive = std::process::Command::new("kill")
                .args(["-0", pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let _ = tokio::fs::remove_file(&pid_path).await;
        assert!(
            !alive,
            "cancelling the future must terminate the child process"
        );
    }

    #[test]
    fn test_read_image_definition() {
        let tool = ImageTool::read_image_definition();
        assert_eq!(tool.name, "read_image");
        assert!(tool.description.contains("image"));
    }

    #[test]
    fn test_screenshot_definition() {
        let tool = ImageTool::screenshot_definition();
        assert_eq!(tool.name, "screenshot");
        assert!(tool.description.contains("screenshot"));
    }

    #[test]
    fn test_get_png_dimensions() {
        // PNG header with 100x200 dimensions
        let png_header: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // IHDR
            0x00, 0x00, 0x00, 0x64, // width = 100
            0x00, 0x00, 0x00, 0xC8, // height = 200
        ];

        let dims = get_image_dimensions(&png_header);
        assert_eq!(dims, Some((100, 200)));
    }

    #[test]
    fn test_get_gif_dimensions() {
        // GIF header with 320x240 dimensions
        let gif_header: Vec<u8> = vec![
            b'G', b'I', b'F', b'8', b'9', b'a', // GIF89a
            0x40, 0x01, // width = 320 (little endian)
            0xF0, 0x00, // height = 240 (little endian)
        ];

        let dims = get_image_dimensions(&gif_header);
        assert_eq!(dims, Some((320, 240)));
    }

    #[test]
    fn test_get_dimensions_unknown_format() {
        let random_data = vec![0x00, 0x01, 0x02, 0x03];
        let dims = get_image_dimensions(&random_data);
        assert_eq!(dims, None);
    }

    #[tokio::test]
    async fn test_read_image_not_found() {
        let tool = ImageTool::new();
        let result = tool
            .read_image(ReadImageArgs {
                file_path: "/nonexistent/image.png".to_string(),
                max_dimension: None,
            })
            .await;

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_read_image_unsupported_format() {
        let tool = ImageTool::new();
        // Create a temp file with unsupported extension
        let temp_dir = std::env::temp_dir();
        let temp_path = temp_dir.join("test_image.xyz");
        tokio::fs::write(&temp_path, b"test").await.unwrap();

        let result = tool
            .read_image(ReadImageArgs {
                file_path: temp_path.to_string_lossy().to_string(),
                max_dimension: None,
            })
            .await;

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Not an image file"));

        let _ = tokio::fs::remove_file(&temp_path).await;
    }
}
