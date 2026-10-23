//! Fullscreen image presentation from admitted tool pixels. No path or URL is opened here.
//!
//! Ratatui owns text and reserves image rows. Graphics are cleared before its diff, then
//! painted after it, inside the same synchronized update. Cropping happens before encoding,
//! so terminal protocols cannot paint into the composer or over a modal.

use std::collections::VecDeque;
use std::io::{self, Cursor, IsTerminal, Write};
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageReader, RgbaImage, imageops::FilterType};
use maestro_runtime_contracts::tool_operation::CodeModeImage;
use ratatui::{buffer::Buffer, layout::Rect, style::Style};

const MAX_ENCODED_BYTES: usize = 14 * 1024 * 1024;
const MAX_ENTRIES: usize = 128;
const MAX_DECODED: usize = 8;
const MAX_IMAGES_PER_CALL: usize = 4;
const MAX_COLUMNS: u16 = 64;
const MAX_ROWS: u16 = 12;
const CELL_WIDTH: u32 = 8;
const CELL_HEIGHT: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageProtocol {
    Kitty,
    Sixel,
    Iterm,
    None,
}

impl ImageProtocol {
    fn detect() -> Self {
        if !io::stdout().is_terminal() || std::env::var_os("TMUX").is_some() {
            return Self::None;
        }
        let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
        let term = std::env::var("TERM").unwrap_or_default();
        match (program.as_str(), term.as_str()) {
            ("kitty" | "ghostty", _) | (_, "xterm-kitty" | "xterm-ghostty") => Self::Kitty,
            ("iTerm.app" | "WezTerm", _) => Self::Iterm,
            _ if term.contains("sixel") => Self::Sixel,
            _ => Self::None,
        }
    }
}

#[derive(Debug)]
struct Preview {
    call_id: String,
    id: u32,
    pixels: Option<Arc<RgbaImage>>,
    fallback: &'static str,
}

impl Preview {
    fn size(&self, width: u16, cell: (u32, u32)) -> (u16, u16) {
        let Some(image) = &self.pixels else {
            return (0, 0);
        };
        let max_width = u32::from(width.saturating_sub(2).min(MAX_COLUMNS)) * cell.0;
        let max_height = u32::from(MAX_ROWS) * cell.1;
        if max_width == 0 {
            return (0, 0);
        }
        let scale = (f64::from(max_width) / f64::from(image.width()))
            .min(f64::from(max_height) / f64::from(image.height()))
            .min(1.0);
        let columns = ((f64::from(image.width()) * scale).ceil() as u32).div_ceil(cell.0);
        let rows = ((f64::from(image.height()) * scale).ceil() as u32).div_ceil(cell.1);
        (columns.max(1) as u16, rows.max(1) as u16)
    }
}

#[derive(Clone, Debug)]
struct Placement {
    id: u32,
    pixels: Arc<RgbaImage>,
    area: Rect,
    full_rows: u16,
    skip_rows: u16,
    cell: (u32, u32),
}

#[derive(Debug)]
struct Encoded {
    id: u32,
    width: u16,
    height: u16,
    full_rows: u16,
    skip_rows: u16,
    data: Vec<u8>,
}

/// Session-owned, bounded image cache and graphics lifecycle.
#[derive(Debug)]
pub struct TranscriptImages {
    protocol: ImageProtocol,
    entries: VecDeque<Preview>,
    pending: Vec<Placement>,
    painted: Vec<Placement>,
    encoded: VecDeque<Encoded>,
    next_id: u32,
    revision: u64,
    cell: (u32, u32),
    terminal_size: Option<(u16, u16)>,
}

impl Default for TranscriptImages {
    fn default() -> Self {
        let mut images = Self::with_protocol(ImageProtocol::detect());
        images.refresh_cell_size();
        images
    }
}

impl TranscriptImages {
    fn with_protocol(protocol: ImageProtocol) -> Self {
        // IDs share the terminal with graphics left by other applications. Avoid
        // the low IDs commonly used by painters and only ever delete our own IDs.
        let seed = *uuid::Uuid::new_v4().as_bytes();
        Self {
            protocol,
            entries: VecDeque::new(),
            pending: Vec::new(),
            painted: Vec::new(),
            encoded: VecDeque::new(),
            next_id: u32::from_be_bytes([seed[0], seed[1], seed[2], seed[3]]).max(1),
            revision: 0,
            cell: (CELL_WIDTH, CELL_HEIGHT),
            terminal_size: None,
        }
    }

    /// Changes when cache eviction or new results change transcript row counts.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Native viewed images carry an explicit pixel projection in result details.
    /// A path without pixels is presentation metadata only, never a read grant.
    pub fn record_tool_result(
        &mut self,
        call_id: &str,
        result: &maestro_runtime_contracts::ToolResult,
    ) {
        if !result.success {
            return;
        }
        let Some(details) = &result.details else {
            return;
        };
        self.record_details(call_id, details);
    }

    /// Restore recent admitted image pixels, with bounded decode work on cold resume.
    pub fn restore(&mut self, session: &crate::session::ParsedSession) {
        self.reset();
        let recent: Vec<_> = session
            .messages
            .iter()
            .rev()
            .filter_map(|message| match message {
                crate::session::AppMessage::ToolResult {
                    tool_call_id,
                    details: Some(details),
                    is_error: false,
                    ..
                } if details.get("image_preview").is_some()
                    || details
                        .get("mime_type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|mime| mime.starts_with("image/")) =>
                {
                    Some((tool_call_id, details))
                }
                _ => None,
            })
            .take(MAX_DECODED)
            .collect();
        for (call_id, details) in recent.into_iter().rev() {
            self.record_details(call_id, details);
        }
    }

    fn record_details(&mut self, call_id: &str, details: &serde_json::Value) {
        if let Some(value) = details.get("image_preview") {
            if value
                .get("data")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|data| data.len() > MAX_ENCODED_BYTES)
            {
                self.record_unavailable(call_id);
                return;
            }
            if let Ok(image) = serde_json::from_value::<CodeModeImage>(value.clone()) {
                self.record_images(call_id, &[image]);
            } else {
                self.record_unavailable(call_id);
            }
        } else if details
            .get("mime_type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|mime| mime.starts_with("image/"))
        {
            self.record_unavailable(call_id);
        }
    }

    /// Add media explicitly emitted by a completed tool, never inferred from prose.
    pub fn record_images(&mut self, call_id: &str, images: &[CodeModeImage]) {
        self.entries.retain(|entry| entry.call_id != call_id);
        for image in images.iter().take(MAX_IMAGES_PER_CALL) {
            let decoded = if self.protocol == ImageProtocol::None {
                Err("preview unavailable in this terminal")
            } else {
                decode(image)
            };
            let (pixels, fallback) = match decoded {
                Ok(pixels) => (Some(Arc::new(pixels)), ""),
                Err(reason) => (None, reason),
            };
            self.entries.push_back(Preview {
                call_id: call_id.into(),
                id: self.next_id,
                pixels,
                fallback,
            });
            self.next_id = self.next_id.wrapping_add(1).max(1);
        }
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
        let mut decoded = 0;
        for entry in self.entries.iter_mut().rev() {
            if entry.pixels.is_some() {
                decoded += 1;
                if decoded > MAX_DECODED {
                    entry.pixels = None;
                    entry.fallback = "preview evicted from memory";
                }
            }
        }
        self.encoded.clear();
        self.revision = self.revision.wrapping_add(1);
    }

    /// A typed path-only/remote image keeps a textual fallback; this does not read it.
    pub fn record_unavailable(&mut self, call_id: &str) {
        self.record_images(
            call_id,
            &[CodeModeImage {
                mime_type: String::new(),
                data: String::new(),
            }],
        );
    }

    /// Drop session pixels, preserving the pending deletion of already displayed graphics.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.encoded.clear();
        self.pending.clear();
        self.revision = self.revision.wrapping_add(1);
    }

    /// Prepare a synchronized frame. Kitty placements can survive unrelated text redraws.
    pub fn prepare_frame(
        &mut self,
        writer: &mut impl Write,
        overlay_visible: bool,
    ) -> io::Result<()> {
        let size = crossterm::terminal::size().ok();
        let resized = size != self.terminal_size;
        self.terminal_size = size;
        if overlay_visible || resized || self.protocol != ImageProtocol::Kitty {
            self.clear_graphics(writer)?;
        }
        self.begin_frame();
        Ok(())
    }

    /// Start collecting placements for a new frame.
    pub fn begin_frame(&mut self) {
        self.pending.clear();
        self.refresh_cell_size();
    }

    fn refresh_cell_size(&mut self) {
        let measured = crossterm::terminal::window_size().ok().and_then(|size| {
            if size.columns == 0 || size.rows == 0 {
                return None;
            }
            let cell = (
                u32::from(size.width / size.columns),
                u32::from(size.height / size.rows),
            );
            (cell.0 > 0 && cell.0 <= 32 && cell.1 > 0 && cell.1 <= 64).then_some(cell)
        });
        // Sixel uses physical pixels, so it cannot safely clip without cell metrics.
        if measured.is_none() && self.protocol == ImageProtocol::Sixel {
            self.protocol = ImageProtocol::None;
            self.entries.iter_mut().for_each(|entry| {
                entry.pixels = None;
                entry.fallback = "terminal pixel size unavailable";
            });
            self.revision = self.revision.wrapping_add(1);
        }
        if let Some(cell) = measured.filter(|cell| *cell != self.cell) {
            self.cell = cell;
            self.encoded.clear();
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Reserved rows, including one accessible caption per image.
    pub fn height(&self, call_id: &str, width: u16) -> u16 {
        self.entries
            .iter()
            .filter(|entry| entry.call_id == call_id)
            .map(|entry| 1 + entry.size(width, self.cell).1)
            .sum()
    }

    /// Render the visible part of image slots after a message's ordinary content.
    /// `offset` and `skip` are rows in the complete message, before viewport clipping.
    pub fn render(
        &mut self,
        call_id: &str,
        width: u16,
        offset: usize,
        skip: usize,
        viewport: Rect,
        buffer: &mut Buffer,
    ) {
        let mut top = offset;
        for entry in self.entries.iter().filter(|entry| entry.call_id == call_id) {
            let (columns, rows) = entry.size(width, self.cell);
            if top >= skip && top - skip < usize::from(viewport.height) {
                let label = if entry.pixels.is_some() {
                    "Image preview".to_string()
                } else {
                    format!("Image: {}", entry.fallback)
                };
                buffer.set_stringn(
                    viewport.x + 2,
                    viewport.y + (top - skip) as u16,
                    label,
                    usize::from(width.saturating_sub(2)),
                    Style::default(),
                );
            }
            top += 1;
            let visible_top = top.max(skip);
            let visible_bottom = (top + usize::from(rows)).min(skip + usize::from(viewport.height));
            if let Some(pixels) = entry
                .pixels
                .as_ref()
                .filter(|_| visible_top < visible_bottom)
            {
                self.pending.push(Placement {
                    id: entry.id,
                    pixels: pixels.clone(),
                    area: Rect::new(
                        viewport.x + 2,
                        viewport.y + (visible_top - skip) as u16,
                        columns,
                        (visible_bottom - visible_top) as u16,
                    ),
                    full_rows: rows,
                    skip_rows: (visible_top - top) as u16,
                    cell: self.cell,
                });
            }
            top += usize::from(rows);
        }
    }

    /// Erase only this renderer's graphics before Ratatui paints text/overlays.
    pub fn clear_graphics(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.painted.is_empty() {
            return Ok(());
        }
        writer.write_all(b"\x1b7")?;
        for placement in &self.painted {
            let id = placement.id;
            let area = placement.area;
            if self.protocol == ImageProtocol::Kitty {
                write!(writer, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\")?;
            } else {
                for row in area.y..area.bottom() {
                    write!(
                        writer,
                        "\x1b[{};{}H\x1b[{}X",
                        row + 1,
                        area.x + 1,
                        area.width
                    )?;
                }
            }
        }
        writer.write_all(b"\x1b8")?;
        self.painted.clear();
        Ok(())
    }

    /// Paint current clipped placements after text, unless any overlay covers the transcript.
    pub fn paint(&mut self, writer: &mut impl Write, overlay_visible: bool) -> io::Result<()> {
        if overlay_visible {
            return Ok(());
        }
        if self.pending.is_empty() && self.painted.is_empty() {
            return Ok(());
        }
        writer.write_all(b"\x1b7")?;
        if self.protocol == ImageProtocol::Kitty {
            for old in &self.painted {
                if !self.pending.iter().any(|new| same_placement(old, new)) {
                    write!(writer, "\x1b_Ga=d,d=I,i={},q=2;\x1b\\", old.id)?;
                }
            }
        }
        for placement in self.pending.iter().take(MAX_DECODED) {
            if self.protocol == ImageProtocol::Kitty
                && self
                    .painted
                    .iter()
                    .any(|old| same_placement(old, placement))
            {
                continue;
            }
            let position = self.encoded.iter().position(|encoded| {
                encoded.id == placement.id
                    && encoded.width == placement.area.width
                    && encoded.height == placement.area.height
                    && encoded.full_rows == placement.full_rows
                    && encoded.skip_rows == placement.skip_rows
            });
            let index = match position {
                Some(index) => index,
                None => {
                    if self.encoded.len() >= MAX_DECODED {
                        self.encoded.pop_front();
                    }
                    self.encoded.push_back(Encoded {
                        id: placement.id,
                        width: placement.area.width,
                        height: placement.area.height,
                        full_rows: placement.full_rows,
                        skip_rows: placement.skip_rows,
                        data: encode(self.protocol, placement)?,
                    });
                    self.encoded.len() - 1
                }
            };
            write!(
                writer,
                "\x1b[{};{}H",
                placement.area.y + 1,
                placement.area.x + 1
            )?;
            writer.write_all(&self.encoded[index].data)?;
        }
        self.painted = self.pending.iter().take(MAX_DECODED).cloned().collect();
        writer.write_all(b"\x1b8")
    }
}

fn same_placement(left: &Placement, right: &Placement) -> bool {
    left.id == right.id
        && left.area == right.area
        && left.full_rows == right.full_rows
        && left.skip_rows == right.skip_rows
        && left.cell == right.cell
}

fn decode(source: &CodeModeImage) -> Result<RgbaImage, &'static str> {
    if source.data.starts_with("http:") || source.data.starts_with("https:") {
        return Err("remote image; open the tool result");
    }
    if source.data.len() > MAX_ENCODED_BYTES {
        return Err("image exceeds preview size limit");
    }
    if !matches!(
        source.mime_type.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
    ) {
        return Err("image data unavailable or unsupported");
    }
    let bytes = STANDARD
        .decode(&source.data)
        .map_err(|_| "invalid image data")?;
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "unsupported image format")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|_| "image cannot be decoded within preview limits")?;
    Ok(decoded
        .thumbnail(
            u32::from(MAX_COLUMNS) * CELL_WIDTH,
            u32::from(MAX_ROWS) * CELL_HEIGHT,
        )
        .to_rgba8())
}

fn encode(protocol: ImageProtocol, placement: &Placement) -> io::Result<Vec<u8>> {
    let width = u32::from(placement.area.width) * placement.cell.0;
    let full_height = u32::from(placement.full_rows) * placement.cell.1;
    let scale = (f64::from(width) / f64::from(placement.pixels.width()))
        .min(f64::from(full_height) / f64::from(placement.pixels.height()));
    let raster_width = (f64::from(placement.pixels.width()) * scale)
        .round()
        .max(1.0) as u32;
    let raster_height = (f64::from(placement.pixels.height()) * scale)
        .round()
        .max(1.0) as u32;
    let raster = image::imageops::resize(
        placement.pixels.as_ref(),
        raster_width,
        raster_height,
        FilterType::Triangle,
    );
    let mut scaled = RgbaImage::new(width, full_height);
    image::imageops::overlay(
        &mut scaled,
        &raster,
        i64::from((width - raster_width) / 2),
        i64::from((full_height - raster_height) / 2),
    );
    let pixels = image::imageops::crop_imm(
        &scaled,
        0,
        u32::from(placement.skip_rows) * placement.cell.1,
        width,
        u32::from(placement.area.height) * placement.cell.1,
    )
    .to_image();
    let mut output = Vec::new();
    match protocol {
        ImageProtocol::Kitty => {
            let payload = STANDARD.encode(pixels.as_raw());
            let chunks = payload.as_bytes().chunks(4096);
            let count = chunks.len();
            for (index, chunk) in chunks.enumerate() {
                let more = u8::from(index + 1 < count);
                if index == 0 {
                    write!(
                        output,
                        "\x1b_Ga=T,f=32,t=d,s={},v={},i={},c={},r={},C=1,q=2,m={more};",
                        pixels.width(),
                        pixels.height(),
                        placement.id,
                        placement.area.width,
                        placement.area.height
                    )?;
                } else {
                    write!(output, "\x1b_Gm={more};")?;
                }
                output.extend_from_slice(chunk);
                output.extend_from_slice(b"\x1b\\");
            }
        }
        ImageProtocol::Iterm => {
            let mut png = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(pixels)
                .write_to(&mut png, image::ImageFormat::Png)
                .map_err(io::Error::other)?;
            write!(
                output,
                "\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=0;doNotMoveCursor=1:{}\x07",
                placement.area.width,
                placement.area.height,
                STANDARD.encode(png.into_inner())
            )?;
        }
        ImageProtocol::Sixel => encode_sixel(&pixels, &mut output)?,
        ImageProtocol::None => {}
    }
    Ok(output)
}

fn encode_sixel(pixels: &RgbaImage, output: &mut Vec<u8>) -> io::Result<()> {
    write!(
        output,
        "\x1bP0;1;0q\"1;1;{};{}",
        pixels.width(),
        pixels.height()
    )?;
    for color in 0..64 {
        write!(
            output,
            "#{color};2;{};{};{}",
            ((color >> 4) & 3) * 100 / 3,
            ((color >> 2) & 3) * 100 / 3,
            (color & 3) * 100 / 3
        )?;
    }
    for top in (0..pixels.height()).step_by(6) {
        let mut rows = vec![vec![b'?'; pixels.width() as usize]; 64];
        for dy in 0..6.min(pixels.height() - top) {
            for x in 0..pixels.width() {
                let pixel = pixels.get_pixel(x, top + dy);
                if pixel[3] >= 128 {
                    let color = ((pixel[0] >> 6) << 4) | ((pixel[1] >> 6) << 2) | (pixel[2] >> 6);
                    rows[color as usize][x as usize] += 1 << dy;
                }
            }
        }
        for (color, row) in rows.iter().enumerate() {
            if row.iter().all(|byte| *byte == b'?') {
                continue;
            }
            write!(output, "#{color}")?;
            output.extend_from_slice(row);
            output.push(b'$');
        }
        output.push(b'-');
    }
    output.extend_from_slice(b"\x1b\\");
    Ok(())
}

#[cfg(test)]
mod tests;
