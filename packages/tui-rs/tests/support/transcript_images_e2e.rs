use super::*;
use base64::Engine as _;

const SYNC_END: &[u8] = b"\x1b[?2026l";
const KITTY_PAINT: &[u8] = b"\x1b_Ga=T,f=32,t=d";

fn complete_graphics_frame(
    raw: &[u8],
    needle: &[u8],
    after: usize,
    expected_columns: Option<u16>,
) -> bool {
    let Some(marker) = raw
        .windows(needle.len())
        .rposition(|bytes| bytes == needle)
        .filter(|position| *position >= after)
    else {
        return false;
    };
    let Some(sync_end) = raw
        .windows(SYNC_END.len())
        .rposition(|bytes| bytes == SYNC_END)
    else {
        return false;
    };
    if sync_end <= marker {
        return false;
    }
    if needle == KITTY_PAINT {
        if let Some(expected_columns) = expected_columns {
            let Some(header_end) = raw[marker..].iter().position(|byte| *byte == b';') else {
                return false;
            };
            let header = String::from_utf8_lossy(&raw[marker..marker + header_end]);
            let columns = header.split(',').find_map(|field| {
                field
                    .strip_prefix("c=")
                    .and_then(|value| value.parse::<u16>().ok())
            });
            if columns != Some(expected_columns) {
                return false;
            }
        }
        // This fixture is multi-chunk. Its final Kitty packet must be complete,
        // not merely the a=T header observed while the PTY reader is streaming.
        let final_chunk = b"\x1b_Gm=0;";
        let Some(final_start) = raw
            .windows(final_chunk.len())
            .rposition(|bytes| bytes == final_chunk)
            .filter(|position| *position > marker)
        else {
            return false;
        };
        let Some(final_end) = raw[final_start..]
            .windows(2)
            .position(|bytes| bytes == b"\x1b\\")
        else {
            return false;
        };
        if final_start + final_end + 2 > sync_end {
            return false;
        }
    }
    true
}

fn wait_raw(session: &PtySession, needle: &[u8], after: usize, columns: Option<u16>) {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let complete = {
            let capture = session
                .output
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            capture.completed_current_text().is_some()
                && complete_graphics_frame(capture.raw_bytes(), needle, after, columns)
        };
        if complete {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "missing complete graphics frame {:?}; screen: {}",
            String::from_utf8_lossy(needle),
            session.screen_text()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn transcript_images_evidence_waits_for_final_packet_and_sync_end() {
    let mut bytes = KITTY_PAINT.to_vec();
    bytes.extend_from_slice(b",c=40,r=12,m=1;AAAA\x1b\\");
    assert!(!complete_graphics_frame(&bytes, KITTY_PAINT, 0, Some(40)));
    bytes.extend_from_slice(b"\x1b_Gm=0;BBBB");
    assert!(!complete_graphics_frame(&bytes, KITTY_PAINT, 0, Some(40)));
    bytes.extend_from_slice(b"\x1b\\");
    assert!(!complete_graphics_frame(&bytes, KITTY_PAINT, 0, Some(40)));
    bytes.extend_from_slice(SYNC_END);
    assert!(complete_graphics_frame(&bytes, KITTY_PAINT, 0, Some(40)));
    assert!(!complete_graphics_frame(
        &bytes,
        KITTY_PAINT,
        bytes.len(),
        None
    ));
    // An already queued wide paint is complete, but does not acknowledge resize.
    assert!(!complete_graphics_frame(&bytes, KITTY_PAINT, 0, Some(37)));
    bytes.extend_from_slice(b"\x1b_Ga=T,f=32,t=d,c=37,r=12,m=1;CCCC\x1b\\\x1b_Gm=0;DDDD\x1b\\");
    bytes.extend_from_slice(SYNC_END);
    assert!(complete_graphics_frame(&bytes, KITTY_PAINT, 0, Some(37)));
}

fn save_graphics(session: &PtySession, name: &str) {
    if let Some(directory) = std::env::var_os("MAESTRO_PTY_EVIDENCE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let capture = session
            .output
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let raw = capture.raw_bytes();
        let end = raw
            .windows(SYNC_END.len())
            .rposition(|bytes| bytes == SYNC_END)
            .expect("completed synchronized frame")
            + SYNC_END.len();
        std::fs::write(directory.join(format!("{name}.ansi")), &raw[..end]).unwrap();
        std::fs::write(
            directory.join(format!("{name}.txt")),
            capture.current_text(),
        )
        .unwrap();
    }
}

fn assert_graphics_above_composer(session: &PtySession, terminal_rows: u16) {
    let capture = session.output.lock().unwrap();
    let raw = String::from_utf8_lossy(capture.raw_bytes());
    let image_start = raw.rfind("\x1b_Ga=T").expect("native image command");
    let cursor = raw[..image_start].rsplit("\x1b[").next().unwrap();
    let top: u16 = cursor.split(';').next().unwrap().parse().unwrap();
    let header = raw[image_start..].split(';').next().unwrap();
    let rows: u16 = header
        .split(',')
        .find_map(|part| part.strip_prefix("r="))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        top + rows <= terminal_rows - 2,
        "image at {top} with {rows} rows crosses composer"
    );
}

#[test]
fn pty_transcript_images_resume_clip_resize_and_clear_for_overlay() {
    let _serial = PTY_TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let workdir = tempfile::tempdir().unwrap();
    let path = write_fork_fixture(workdir.path(), "image-preview");
    let mut pixels = image::RgbaImage::new(320, 192);
    for (x, y, pixel) in pixels.enumerate_pixels_mut() {
        *pixel = image::Rgba(if (x / 40 + y / 32).is_multiple_of(2) {
            [28, 190, 230, 255]
        } else {
            [182, 74, 225, 255]
        });
    }
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(pixels)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let image = serde_json::json!({ "mime_type": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(png.into_inner()) });
    let assistant = serde_json::json!({"type":"message","timestamp":"2026-07-29T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"Transcript image fixture"},{"type":"toolCall","id":"view-fixture","name":"read_image","arguments":{"file_path":"/missing/admitted-image.png"}}],"timestamp":2}});
    let tool = serde_json::json!({"type":"message","timestamp":"2026-07-29T00:00:03Z","message":{"role":"toolResult","toolCallId":"view-fixture","toolName":"read_image","content":"Image loaded: admitted fixture pixels","details":{"mime_type":"image/png","path":"/missing/admitted-image.png","image_preview":image},"isError":false,"timestamp":3}});
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{assistant}\n{tool}").unwrap();
    let mock = MockOpenAiServer::start(vec![]);
    let mut session = PtySession::launch(
        &mock,
        workdir.path(),
        PtyLaunch {
            executable: std::env::var_os("CARGO_BIN_EXE_maestro-tui").unwrap(),
            args: &[
                "--model",
                "gpt-4o",
                "--api-key",
                "pty-e2e-key",
                "--resume-session",
                "image-preview",
            ],
            extra_env: &[("TERM", "xterm-kitty"), ("TERM_PROGRAM", "kitty")],
            removed_env: &["TMUX"],
            columns: 120,
        },
    );
    session.output.lock().unwrap().capture_raw();
    session.wait_for_text("Image preview", READY_TIMEOUT);
    wait_raw(&session, KITTY_PAINT, 0, Some(40));
    assert_graphics_above_composer(&session, 36);
    assert_eq!(
        mock.request_count(),
        0,
        "resume preview must not need inference"
    );
    save_graphics(&session, "transcript-image-visible");

    let before_resize = {
        let mut capture = session.output.lock().unwrap();
        let checkpoint = capture.raw_bytes().len();
        // Resize the emulated screen before the child can issue its new DSR.
        capture.resize(20, 40);
        session
            ._master
            .resize(PtySize {
                rows: 20,
                cols: 40,
                pixel_width: 320,
                pixel_height: 320,
            })
            .unwrap();
        checkpoint
    };
    // A pre-resize frame can still be in flight after the PTY size changes.
    // Wait for the narrow transcript width (40 minus scrollbar and image indent),
    // then independently assert its vertical clipping below.
    wait_raw(&session, KITTY_PAINT, before_resize, Some(37));
    assert_graphics_above_composer(&session, 20);
    save_graphics(&session, "transcript-image-clipped");

    let before_overlay = session.output.lock().unwrap().raw_bytes().len();
    session.send_bytes_until(b"\x0b", "Commands", TURN_TIMEOUT);
    wait_raw(&session, b"\x1b_Ga=d,d=I", before_overlay, None);
    let overlay = session.output.lock().unwrap().raw_bytes().to_vec();
    let deletion = overlay
        .windows(b"\x1b_Ga=d,d=I".len())
        .rposition(|part| part == b"\x1b_Ga=d,d=I")
        .unwrap();
    assert!(
        !overlay[deletion..]
            .windows(b"\x1b_Ga=T".len())
            .any(|part| part == b"\x1b_Ga=T"),
        "modal must not repaint images over its text"
    );
    save_graphics(&session, "transcript-image-overlay");
    session.shutdown();
}
