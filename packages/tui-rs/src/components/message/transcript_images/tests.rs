use super::*;

fn image() -> CodeModeImage {
    let image = image::DynamicImage::new_rgb8(320, 320);
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    CodeModeImage {
        mime_type: "image/png".into(),
        data: STANDARD.encode(bytes.into_inner()),
    }
}

#[test]
fn transcript_images_clip_pixels_to_visible_rows() {
    let mut images = TranscriptImages::with_protocol(ImageProtocol::Kitty);
    images.record_images("call", &[image()]);
    let height = images.height("call", 40);
    assert!(height > 2);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 3));
    images.render("call", 40, 0, 2, Rect::new(0, 0, 40, 3), &mut buffer);
    assert_eq!(images.pending.len(), 1);
    assert_eq!(images.pending[0].area.height, 3);
    assert_eq!(images.pending[0].skip_rows, 1);
    let mut terminal = Vec::new();
    images.paint(&mut terminal, false).unwrap();
    let output = String::from_utf8(terminal).unwrap();
    assert!(output.contains("r=3"));
    assert!(output.contains("t=d"));
    assert!(!output.contains("t=f"));
}

#[test]
fn transcript_images_overlay_and_session_reset_remove_owned_graphics() {
    let mut images = TranscriptImages::with_protocol(ImageProtocol::Kitty);
    images.record_images("call", &[image()]);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 20));
    images.render("call", 40, 0, 0, buffer.area, &mut buffer);
    let mut terminal = Vec::new();
    images.paint(&mut terminal, false).unwrap();
    assert!(!terminal.is_empty());
    terminal.clear();
    images.clear_graphics(&mut terminal).unwrap();
    assert!(String::from_utf8_lossy(&terminal).contains("a=d,d=I"));
    assert!(!String::from_utf8_lossy(&terminal).contains("d=A"));
    terminal.clear();
    images.paint(&mut terminal, true).unwrap();
    assert!(terminal.is_empty());
    images.reset();
    assert_eq!(images.height("call", 40), 0);
}

#[test]
fn transcript_images_kitty_retains_redraws_and_replaces_resized_placements() {
    let mut images = TranscriptImages::with_protocol(ImageProtocol::Kitty);
    images.record_images("call", &[image()]);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 20));
    images.render("call", 40, 0, 0, buffer.area, &mut buffer);
    images.paint(&mut Vec::new(), false).unwrap();
    let mut unchanged = Vec::new();
    images.paint(&mut unchanged, false).unwrap();
    assert!(!String::from_utf8_lossy(&unchanged).contains("_G"));
    images.pending.clear();
    images.render("call", 10, 0, 0, buffer.area, &mut buffer);
    let mut resized = Vec::new();
    images.paint(&mut resized, false).unwrap();
    let resized = String::from_utf8_lossy(&resized);
    assert!(resized.contains("a=d,d=I"));
    assert!(resized.contains("a=T"));
    images.pending.clear();
    let mut scrolled_out = Vec::new();
    images.paint(&mut scrolled_out, false).unwrap();
    assert!(String::from_utf8_lossy(&scrolled_out).contains("a=d,d=I"));
}

#[test]
fn transcript_images_invalid_oversized_and_remote_data_fall_back() {
    let mut images = TranscriptImages::with_protocol(ImageProtocol::Kitty);
    for (index, data) in [
        "https://example.com/image.png".into(),
        "not base64".into(),
        "x".repeat(MAX_ENCODED_BYTES + 1),
    ]
    .into_iter()
    .enumerate()
    {
        let call = index.to_string();
        images.record_images(
            &call,
            &[CodeModeImage {
                mime_type: "image/png".into(),
                data,
            }],
        );
        assert_eq!(images.height(&call, 40), 1);
    }
    assert!(images.entries.iter().all(|entry| entry.pixels.is_none()));
}

#[test]
fn transcript_images_never_open_paths_or_prose_urls() {
    let mut images = TranscriptImages::with_protocol(ImageProtocol::Kitty);
    let result = maestro_runtime_contracts::ToolResult::success("![image](/private/secret.png)")
        .with_details(serde_json::json!({"path": "/private/secret.png", "mime_type": "image/png"}));
    images.record_tool_result("call", &result);
    assert_eq!(images.height("call", 40), 1);
    assert!(images.entries[0].pixels.is_none());
    let unrelated =
        maestro_runtime_contracts::ToolResult::success("![image](https://example.com/x.png)");
    images.record_tool_result("prose", &unrelated);
    assert_eq!(images.height("prose", 40), 0);
}

#[test]
fn transcript_images_cache_is_bounded_and_unsupported_terminal_uses_text() {
    let mut images = TranscriptImages::with_protocol(ImageProtocol::Kitty);
    for index in 0..MAX_ENTRIES + 10 {
        images.record_images(&index.to_string(), &[image()]);
    }
    assert_eq!(images.entries.len(), MAX_ENTRIES);
    assert!(
        images
            .entries
            .iter()
            .filter(|entry| entry.pixels.is_some())
            .count()
            <= MAX_DECODED
    );
    let mut unsupported = TranscriptImages::with_protocol(ImageProtocol::None);
    unsupported.record_images("call", &[image()]);
    assert_eq!(unsupported.height("call", 40), 1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 2));
    unsupported.render("call", 40, 0, 0, buffer.area, &mut buffer);
    assert!(unsupported.pending.is_empty());
}

#[test]
fn transcript_images_sixel_and_iterm_preserve_cursor_and_bound_pixels() {
    for protocol in [ImageProtocol::Sixel, ImageProtocol::Iterm] {
        let mut images = TranscriptImages::with_protocol(protocol);
        images.record_images("call", &[image()]);
        let mut buffer = Buffer::empty(Rect::new(2, 3, 40, 20));
        images.render("call", 40, 0, 0, buffer.area, &mut buffer);
        let mut output = Vec::new();
        images.paint(&mut output, false).unwrap();
        assert!(output.starts_with(b"\x1b7"));
        assert!(output.ends_with(b"\x1b8"));
        assert!(output.len() < 1024 * 1024);
    }
}

#[test]
fn transcript_images_preserve_pixel_aspect_inside_character_cells() {
    let placement = Placement {
        id: 1,
        pixels: Arc::new(RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]))),
        area: Rect::new(0, 0, 1, 1),
        full_rows: 1,
        skip_rows: 0,
        cell: (8, 16),
    };
    let output = String::from_utf8(encode(ImageProtocol::Kitty, &placement).unwrap()).unwrap();
    let payload = output
        .split("\x1b_G")
        .skip(1)
        .map(|chunk| chunk.split_once(';').unwrap().1.trim_end_matches("\x1b\\"))
        .collect::<String>();
    let pixels = STANDARD.decode(payload).unwrap();
    assert_eq!(pixels.len(), 8 * 16 * 4);
    assert_eq!(
        pixels.chunks_exact(4).filter(|pixel| pixel[3] != 0).count(),
        8 * 8,
        "square source stays square with transparent padding"
    );
}
