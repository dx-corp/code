use super::*;

#[test]
fn compaction_retains_image_projection_identity_without_image_bytes() {
    let messages: Vec<Message> = serde_json::from_value(serde_json::json!([
        {"role":"user", "content":[{"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":"private-image-bytes", "owner":{"call_id":"script-owner", "index":0}}}]}
    ])).unwrap();
    let previous = build_continuation_record(&messages);
    let mut next = ContinuationRecord::default();
    next.merge_previous(&previous);
    let value = serde_json::to_value(next).unwrap();
    assert_eq!(
        value["projected_tool_images"],
        serde_json::json!([{"call_id":"script-owner", "index":0}])
    );
    assert!(!value.to_string().contains("private-image-bytes"));
}
