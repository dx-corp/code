//! Explicit projection only; host adapters enforce their smaller media budgets.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputBlock {
    Text { text: String },
    Image { mime_type: String, data: String },
}

pub fn image_block(value: Value) -> Result<OutputBlock, String> {
    let (mime, data) = if let Some(url) = value
        .as_str()
        .or_else(|| value.get("image_url").and_then(Value::as_str))
    {
        let (mime, data) = url
            .strip_prefix("data:")
            .and_then(|v| v.split_once(";base64,"))
            .ok_or("image requires a base64 data URL; remote URLs are unavailable")?;
        (mime.to_owned(), data.to_owned())
    } else {
        if value
            .get("type")
            .is_some_and(|kind| kind.as_str() != Some("image"))
        {
            return Err("image requires an image block or data URL".into());
        }
        let mime = value
            .get("mimeType")
            .or_else(|| value.get("mime_type"))
            .and_then(Value::as_str)
            .ok_or("image block requires mimeType")?;
        let data = value
            .get("data")
            .and_then(Value::as_str)
            .ok_or("image block requires base64 data")?;
        (mime.to_owned(), data.to_owned())
    };
    if data.len() > 1_398_104 {
        return Err("maximum 1 MiB decoded image".into());
    }
    let decoded = STANDARD.decode(&data).map_err(|_| "invalid image base64")?;
    if decoded.len() > 1_048_576 {
        return Err("maximum 1 MiB decoded image".into());
    }
    let matches = match mime.as_str() {
        "image/png" => decoded.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => decoded.starts_with(b"\xff\xd8\xff"),
        "image/gif" => decoded.starts_with(b"GIF87a") || decoded.starts_with(b"GIF89a"),
        "image/webp" => decoded.starts_with(b"RIFF") && decoded.get(8..12) == Some(b"WEBP"),
        _ => false,
    };
    if !matches {
        return Err("image MIME type must match PNG, JPEG, GIF or WebP bytes".into());
    }
    Ok(OutputBlock::Image {
        mime_type: mime,
        data,
    })
}

#[derive(Default)]
pub(crate) struct OutputSink {
    pub blocks: Vec<OutputBlock>,
    text_bytes: usize,
    image_bytes: usize,
    image_count: usize,
}
impl OutputSink {
    pub fn text(&mut self, text: String) -> Result<(), String> {
        if self.blocks.len() >= 1024 || self.text_bytes.saturating_add(text.len()) > 65_536 {
            return Err("maximum 64 KiB script output".into());
        }
        self.text_bytes += text.len();
        self.blocks.push(OutputBlock::Text { text });
        Ok(())
    }
    pub fn image(&mut self, block: OutputBlock) -> Result<(), String> {
        let OutputBlock::Image { data, .. } = &block else {
            return Err("expected image".into());
        };
        let decoded_bytes =
            data.len() / 4 * 3 - data.bytes().rev().take_while(|b| *b == b'=').count();
        if self.blocks.len() >= 1024
            || self.image_count >= 4
            || self.image_bytes.saturating_add(decoded_bytes) > 2_097_152
        {
            return Err("maximum four images and 2 MiB decoded image output".into());
        }
        self.image_count += 1;
        self.image_bytes += decoded_bytes;
        self.blocks.push(block);
        Ok(())
    }
}
