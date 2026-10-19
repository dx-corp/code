//! Host limits are narrower than the sandbox limits and apply again on replay.
use agent_codemode::OutputBlock;

pub fn validate_codemode_blocks(blocks: &[OutputBlock]) -> Result<(), String> {
    let mut text_bytes = 0usize;
    let mut image_bytes = 0usize;
    let mut images = 0usize;
    for block in blocks {
        match block {
            OutputBlock::Text { text } => text_bytes = text_bytes.saturating_add(text.len()),
            OutputBlock::Image { data, .. } => {
                images += 1;
                image_bytes = image_bytes.saturating_add(data.len());
                if images > 4 || image_bytes > 32 * 1024 {
                    return Err("selected images exceed Dex's 32 KiB base64/four-image capacity; select fewer or smaller images".into());
                }
                agent_codemode::image_block(
                    serde_json::to_value(block).map_err(|e| e.to_string())?,
                )?;
            }
        }
    }
    if text_bytes > 64 * 1024 {
        return Err(
            "script text exceeds Dex's 64 KiB capacity; return a smaller projection".into(),
        );
    }
    Ok(())
}
