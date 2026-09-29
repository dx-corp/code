//! Byte framing shared by providers whose SSE transport arrives in arbitrary HTTP chunks.

/// Decode only after the entire event arrives so split UTF-8 never becomes replacement text.
pub(crate) fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<String>, std::str::Utf8Error> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    let (end, delimiter_len) = match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf < crlf => (lf, 2),
        (Some(_), Some(crlf)) => (crlf, 4),
        (Some(lf), None) => (lf, 2),
        (None, Some(crlf)) => (crlf, 4),
        (None, None) => return Ok(None),
    };
    let event = std::str::from_utf8(&buffer[..end])?.to_owned();
    buffer.drain(..end + delimiter_len);
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use super::take_frame;

    #[test]
    fn unicode_frames_survive_every_two_chunk_boundary() {
        for delimiter in ["\n\n", "\r\n\r\n"] {
            let frame = format!("data: {{\"text\":\"café 🌊 東京\"}}{delimiter}");
            for split in 0..=frame.len() {
                let mut buffer = Vec::new();
                buffer.extend_from_slice(&frame.as_bytes()[..split]);
                let first = take_frame(&mut buffer).expect("valid partial frame");
                buffer.extend_from_slice(&frame.as_bytes()[split..]);
                let second = take_frame(&mut buffer).expect("valid complete frame");
                assert_eq!(
                    first.into_iter().chain(second).collect::<Vec<_>>(),
                    [frame.trim_end_matches(['\r', '\n'])]
                );
                assert!(buffer.is_empty());
            }
        }
    }

    #[test]
    fn seeded_chunk_partitions_preserve_adjacent_events() {
        let first = "event:update\r\ndata:{\"text\":\"é 🌊\"}\r\n\r\n";
        let second = "data:{\"text\":\"東京\"}\n\n";
        let wire = format!("{first}{second}");
        for seed in 0..512_u64 {
            let mut random = seed + 1;
            let mut cursor = 0;
            let mut buffer = Vec::new();
            let mut frames = Vec::new();
            while cursor < wire.len() {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let end = (cursor + 1 + (random as usize % 9)).min(wire.len());
                buffer.extend_from_slice(&wire.as_bytes()[cursor..end]);
                while let Some(frame) = take_frame(&mut buffer).expect("valid UTF-8 frame") {
                    frames.push(frame);
                }
                cursor = end;
            }
            assert_eq!(frames, [first.trim_end(), second.trim_end()]);
            assert!(buffer.is_empty());
        }
    }

    #[test]
    fn invalid_utf8_is_an_error_instead_of_replacement_text() {
        let mut buffer = b"data: {\"text\":\"\xff\"}\n\n".to_vec();
        assert!(take_frame(&mut buffer).is_err());
    }
}
