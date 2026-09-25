//! Deterministic browser fixture: one upstream token stream, two release policies.

use maestro_local_host::semantic_text::{FlushReason, SemanticTextRelease};
use serde_json::json;

fn main() {
    let input = concat!(
        "## Risk by area\n\nThe largest risk is delayed evidence.\n\n",
        "**Next steps**\n\n- Check the run receipt.\n- Verify the rollout.\n\n",
        "### Comparison\n\n| Area | Risk |\n| --- | --- |\n| API | Medium |\n| UI | Low |\n\n",
        "### Example\n\n```rust\nfn main() { println!(\"ready\"); }\n```\n",
    );
    let mut policy = SemanticTextRelease::default();
    let mut legacy = String::new();
    let mut semantic = String::new();
    let mut frames = Vec::new();
    let mut holds = Vec::new();
    let mut title_holds = Vec::new();
    let mut released_chunks = 0;
    let mut first_visible_ms = None;
    let mut forced_flushes = 0;
    let mut legacy_title_frames = 0;
    let mut semantic_title_frames = 0;
    let rate_ms = 64;
    for (index, chunk) in input.as_bytes().chunks(16).enumerate() {
        let chunk = std::str::from_utf8(chunk).expect("ASCII fixture");
        let at_ms = (index as u64 + 1) * rate_ms;
        legacy.push_str(chunk);
        for release in policy.push(chunk, at_ms) {
            holds.push(release.held_ms);
            if release.semantic_unit {
                title_holds.push(release.held_ms);
            }
            forced_flushes += u64::from(release.forced.is_some());
            first_visible_ms.get_or_insert(at_ms);
            released_chunks += 1;
            semantic.push_str(&release.text);
        }
        legacy_title_frames += u64::from(ends_in_standalone_title(&legacy));
        semantic_title_frames += u64::from(ends_in_standalone_title(&semantic));
        frames.push(json!({"atMs": at_ms, "legacy": legacy, "semantic": semantic}));
    }
    let end_ms = (frames.len() as u64 + 1) * rate_ms;
    for release in policy.flush(FlushReason::Finalization, end_ms) {
        holds.push(release.held_ms);
        if release.semantic_unit {
            title_holds.push(release.held_ms);
        }
        forced_flushes += u64::from(release.forced.is_some());
        first_visible_ms.get_or_insert(end_ms);
        released_chunks += 1;
        semantic.push_str(&release.text);
    }
    assert_eq!(legacy, input);
    assert_eq!(semantic, input);
    frames.push(json!({"atMs": end_ms, "legacy": legacy, "semantic": semantic}));
    holds.sort_unstable();
    title_holds.sort_unstable();
    let measured = json!({
        "inputRateCharsPerSecond": 250,
        "upstreamChunkBytes": 16,
        "upstreamIntervalMs": rate_ms,
        "upstreamChunks": input.len().div_ceil(16),
        "semanticPublishedChunks": released_chunks,
        "semanticMedianHoldMs": holds.get(holds.len() / 2).copied().unwrap_or(0),
        "semanticMaxHoldMs": holds.last().copied().unwrap_or(0),
        "titleMedianHoldMs": title_holds.get(title_holds.len() / 2).copied().unwrap_or(0),
        "titleMaxHoldMs": title_holds.last().copied().unwrap_or(0),
        "transportFirstTextMs": rate_ms,
        "firstVisibleSemanticContentMs": first_visible_ms,
        "peakBytesHeld": policy.peak_held_bytes(),
        "forcedFlushes": forced_flushes,
        "legacyStandaloneTitleFrames": legacy_title_frames,
        "semanticStandaloneTitleFrames": semantic_title_frames,
    });
    let fixture = json!({"measured": measured, "frames": frames});
    println!("{}", serde_json::to_string_pretty(&fixture).unwrap());
}

fn ends_in_standalone_title(body: &str) -> bool {
    let last = body.trim_end().lines().last().unwrap_or_default();
    let value = last.trim();
    value.starts_with("# ")
        || value.starts_with("## ")
        || value.starts_with("### ")
        || value.starts_with("**") && (value.ends_with("**") || value.ends_with("**:"))
}
