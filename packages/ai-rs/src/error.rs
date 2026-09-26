//! Provider API error body summarization.
//!
//! Provider error responses are JSON envelopes whose only actionable content
//! is a nested `message` string:
//!
//! - Anthropic: `{"type":"error","error":{"type":"invalid_request_error","message":"..."}}`
//! - OpenAI: `{"error":{"message":"...","type":"invalid_request_error","code":"..."}}`
//! - Google: `{"error":{"code":400,"message":"...","status":"INVALID_ARGUMENT"}}`
//!
//! Dumping the raw body into the UI truncates the envelope's opening braces
//! and hides the message, so we extract the message (and the error kind)
//! whenever the body parses as JSON.

use serde_json::Value;

/// Summarize a provider error response body for display.
///
/// Returns `"<kind>: <message>"` (or just `<message>`) when the body is a
/// JSON error envelope carrying a message. Otherwise returns the raw body
/// with whitespace runs collapsed so it renders as one wrapped paragraph.
#[must_use]
pub fn summarize_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "(empty response body)".to_string();
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if let Some(summary) = extract_error_message(&value) {
            return summary;
        }
    }
    collapse_whitespace(trimmed)
}

/// Pull `error.message` (plus `error.type`/`status`/`code`) out of a parsed
/// error envelope. Also accepts a top-level `message` and the
/// `"error": "..."` string shorthand some gateways use.
///
/// A governance denial's error envelope may additionally carry `decision_id`
/// and `reason_codes` (or `reasons`) fields alongside `message`: see
/// `governance_denial_detail`. Both are optional and are appended to the
/// message when present, so their absence leaves the summary unchanged.
fn extract_error_message(value: &Value) -> Option<String> {
    let error = value.get("error").unwrap_or(value);
    let detail = governance_denial_detail(error);
    if let Some(message) = error.as_str() {
        let message = message.trim();
        return (!message.is_empty()).then(|| format!("{message}{detail}"));
    }
    let message = error.get("message").and_then(Value::as_str)?.trim();
    if message.is_empty() {
        return None;
    }
    let kind = error
        .get("type")
        .or_else(|| error.get("status"))
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .map(str::to_string)
        .or_else(|| error.get("code").map(ToString::to_string));
    Some(match kind {
        Some(kind) => format!("{kind}: {message}{detail}"),
        None => format!("{message}{detail}"),
    })
}

/// Render a gateway's optional governance-decision metadata for display.
///
/// A governance denial's 403 body may carry `decision_id` (a string) and
/// `reason_codes` (an array of strings; some gateways instead use `reasons`)
/// alongside the usual `code`/`message`. Both fields are optional -- render
/// as `" (decision <id>; reasons: <codes>)"`, dropping whichever half is
/// absent, or `""` when neither is present, so a body without this metadata
/// (every provider's error shape, and any gateway response predating it)
/// summarizes exactly as before.
fn governance_denial_detail(error: &Value) -> String {
    let decision_id = error
        .get("decision_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let reasons = error
        .get("reason_codes")
        .or_else(|| error.get("reasons"))
        .and_then(Value::as_array)
        .map(|codes| {
            codes
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|codes| !codes.is_empty());

    match (decision_id, reasons) {
        (Some(id), Some(codes)) => format!(" (decision {id}; reasons: {codes})"),
        (Some(id), None) => format!(" (decision {id})"),
        (None, Some(codes)) => format!(" (reasons: {codes})"),
        (None, None) => String::new(),
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_anthropic_error_message_and_type() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.0: all messages must have non-empty content"}}"#;
        assert_eq!(
            summarize_error_body(body),
            "invalid_request_error: messages.0: all messages must have non-empty content"
        );
    }

    #[test]
    fn extracts_openai_error_message_and_type() {
        let body = r#"{"error":{"message":"Invalid model: gpt-9000","type":"invalid_request_error","param":"model","code":"model_not_found"}}"#;
        assert_eq!(
            summarize_error_body(body),
            "invalid_request_error: Invalid model: gpt-9000"
        );
    }

    #[test]
    fn extracts_google_error_message_and_status() {
        let body =
            r#"{"error":{"code":400,"message":"API key not valid.","status":"INVALID_ARGUMENT"}}"#;
        assert_eq!(
            summarize_error_body(body),
            "INVALID_ARGUMENT: API key not valid."
        );
    }

    #[test]
    fn falls_back_to_numeric_code_when_no_kind_string() {
        let body = r#"{"error":{"code":429,"message":"quota exceeded"}}"#;
        assert_eq!(summarize_error_body(body), "429: quota exceeded");
    }

    #[test]
    fn accepts_top_level_message() {
        let body = r#"{"message":"rate limited, retry after 30s"}"#;
        assert_eq!(summarize_error_body(body), "rate limited, retry after 30s");
    }

    #[test]
    fn accepts_error_string_shorthand() {
        let body = r#"{"error":"upstream timeout"}"#;
        assert_eq!(summarize_error_body(body), "upstream timeout");
    }

    #[test]
    fn falls_back_to_collapsed_raw_body_for_non_json() {
        let body = "<html>\n  <body>Bad Gateway</body>\n</html>";
        assert_eq!(
            summarize_error_body(body),
            "<html> <body>Bad Gateway</body> </html>"
        );
    }

    #[test]
    fn falls_back_to_collapsed_raw_body_for_json_without_message() {
        let body = "{\n  \"unexpected\": true\n}";
        assert_eq!(summarize_error_body(body), "{ \"unexpected\": true }");
    }

    #[test]
    fn handles_empty_body() {
        assert_eq!(summarize_error_body(""), "(empty response body)");
        assert_eq!(summarize_error_body("  \n "), "(empty response body)");
    }

    #[test]
    fn governance_denial_appends_decision_id_and_reason_codes() {
        let body = r#"{"message":"governance denied model content","decision_id":"dec_01HZX","reason_codes":["policy.blocked_topic","policy.pii"]}"#;
        assert_eq!(
            summarize_error_body(body),
            "governance denied model content \
             (decision dec_01HZX; reasons: policy.blocked_topic, policy.pii)"
        );
    }

    #[test]
    fn governance_denial_accepts_reasons_alias() {
        let body = r#"{"message":"governance denied model content","decision_id":"dec_02","reasons":["policy.blocked_topic"]}"#;
        assert_eq!(
            summarize_error_body(body),
            "governance denied model content (decision dec_02; reasons: policy.blocked_topic)"
        );
    }

    #[test]
    fn governance_denial_tolerates_a_decision_id_without_reason_codes() {
        let body = r#"{"message":"governance denied model content","decision_id":"dec_03"}"#;
        assert_eq!(
            summarize_error_body(body),
            "governance denied model content (decision dec_03)"
        );
    }

    #[test]
    fn governance_denial_tolerates_reason_codes_without_a_decision_id() {
        let body = r#"{"message":"governance denied model content","reason_codes":["policy.blocked_topic"]}"#;
        assert_eq!(
            summarize_error_body(body),
            "governance denied model content (reasons: policy.blocked_topic)"
        );
    }

    #[test]
    fn governance_denial_fields_absent_leave_the_message_unchanged() {
        let body = r#"{"type":"governance_denied","message":"governance denied model content"}"#;
        assert_eq!(
            summarize_error_body(body),
            "governance_denied: governance denied model content"
        );
    }

    #[test]
    fn governance_denial_detail_survives_the_error_wrapper_shorthand() {
        // The real gateway body nests the code/message/decision fields
        // directly under a top-level `error` object, mirroring the other
        // providers' envelopes this module already handles.
        let body = r#"{"error":{"message":"governance denied model content","decision_id":"dec_04","reason_codes":["policy.blocked_topic"]}}"#;
        assert_eq!(
            summarize_error_body(body),
            "governance denied model content (decision dec_04; reasons: policy.blocked_topic)"
        );
    }
}
