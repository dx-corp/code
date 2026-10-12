//! Bounded, graded transcript subscriptions for remote and protocol clients.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptGrade {
    Off,
    Turn,
    Block,
    #[default]
    Delta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptLevel {
    Turn,
    Block,
    Delta,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptEvent {
    pub sequence: u64,
    pub level: TranscriptLevel,
    pub kind: String,
    pub payload: Value,
}

impl TranscriptGrade {
    #[must_use]
    pub fn includes(self, level: TranscriptLevel) -> bool {
        match self {
            Self::Off => false,
            Self::Turn => level == TranscriptLevel::Turn,
            Self::Block => level != TranscriptLevel::Delta,
            Self::Delta => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptJournal {
    next_sequence: u64,
    capacity: usize,
    events: VecDeque<TranscriptEvent>,
}

impl TranscriptJournal {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            next_sequence: 1,
            capacity: capacity.max(1),
            events: VecDeque::new(),
        }
    }

    pub fn push(&mut self, level: TranscriptLevel, kind: impl Into<String>, payload: Value) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.events.push_back(TranscriptEvent {
            sequence,
            level,
            kind: kind.into(),
            payload: redact(payload),
        });
        while self.events.len() > self.capacity {
            self.events.pop_front();
        }
        sequence
    }

    #[must_use]
    pub fn after(&self, cursor: u64, grade: TranscriptGrade) -> Vec<TranscriptEvent> {
        self.events
            .iter()
            .filter(|event| event.sequence > cursor && grade.includes(event.level))
            .cloned()
            .collect()
    }
}

fn redact(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            for (key, child) in map {
                if is_credential_key(key) {
                    *child = Value::String("[REDACTED]".to_string());
                } else {
                    *child = redact(child.take());
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                *child = redact(child.take());
            }
        }
        _ => {}
    }
    value
}

fn is_credential_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    if [
        "authorization",
        "apikey",
        "clientsecret",
        "credential",
        "secret",
        "password",
        "privatekey",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
    {
        return true;
    }
    if [
        "databaseurl",
        "dburl",
        "redisurl",
        "mongodburl",
        "mongourl",
        "postgresurl",
        "postgresqlurl",
        "mysqlurl",
        "amqpurl",
    ]
    .contains(&normalized.as_str())
    {
        return true;
    }
    normalized.ends_with("token")
        && ![
            "maxtoken",
            "inputtoken",
            "outputtoken",
            "totaltoken",
            "cachedtoken",
            "tokencount",
        ]
        .iter()
        .any(|metric| normalized.contains(metric))
}

pub fn redact_agent_message(
    message: crate::headless::messages::FromAgentMessage,
) -> crate::headless::messages::FromAgentMessage {
    let Ok(value) = serde_json::to_value(&message) else {
        return message;
    };
    let Ok(mut redacted_message) = serde_json::from_value(redact(value)) else {
        return message;
    };
    match &mut redacted_message {
        crate::headless::messages::FromAgentMessage::ToolCall { tool, args, .. }
        | crate::headless::messages::FromAgentMessage::ClientToolRequest { tool, args, .. }
        | crate::headless::messages::FromAgentMessage::ServerRequest { tool, args, .. } => {
            *args = crate::agent::credential_store::redact_tool_arguments_preserving_references(
                tool, args,
            );
        }
        crate::headless::messages::FromAgentMessage::ToolOutput { content, .. }
        | crate::headless::messages::FromAgentMessage::UtilityCommandOutput { content, .. }
        | crate::headless::messages::FromAgentMessage::UtilityFileReadResult { content, .. } => {
            redact_output_content(content);
        }
        _ => {}
    }
    redacted_message
}

pub fn agent_message_for_controller(
    message: crate::headless::messages::FromAgentMessage,
) -> crate::headless::messages::FromAgentMessage {
    if matches!(
        &message,
        crate::headless::messages::FromAgentMessage::ClientToolRequest { .. }
            | crate::headless::messages::FromAgentMessage::ServerRequest {
                request_type: crate::headless::messages::ServerRequestType::ClientTool,
                ..
            }
    ) {
        message
    } else {
        redact_agent_message(message)
    }
}

fn redact_output_content(content: &mut String) {
    if let Ok(value) = serde_json::from_str::<Value>(content) {
        let redacted_value = redact(value.clone());
        if redacted_value != value {
            *content =
                serde_json::to_string(&redacted_value).unwrap_or_else(|_| "[REDACTED]".to_string());
        } else if value_contains_credential_marker(&value) {
            *content = "[REDACTED]".to_string();
        }
    } else if contains_credential_marker(content) {
        *content = "[REDACTED]".to_string();
    }
}

fn value_contains_credential_marker(value: &Value) -> bool {
    match value {
        Value::String(content) => contains_credential_marker(content),
        Value::Array(values) => values.iter().any(value_contains_credential_marker),
        Value::Object(map) => map.values().any(value_contains_credential_marker),
        _ => false,
    }
}

fn contains_credential_marker(content: &str) -> bool {
    content.lines().any(|line| {
        line_contains_private_key_marker(line)
            || line_contains_credential(line)
            || line_contains_redactable_json(line)
    })
}

fn line_contains_private_key_marker(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("-----BEGIN ") && line.ends_with("PRIVATE KEY-----")
}

fn line_contains_redactable_json(line: &str) -> bool {
    line.char_indices()
        .filter(|(_, character)| matches!(character, '{' | '['))
        .any(|(index, _)| {
            serde_json::from_str::<Value>(line[index..].trim())
                .is_ok_and(|value| redact(value.clone()) != value)
        })
}

fn line_contains_credential(line: &str) -> bool {
    let line = line.trim();
    let line = line.strip_prefix("export ").unwrap_or(line);
    if let Some((key, value)) = line.split_once('=') {
        return credential_assignment_key(key).is_some() && !value.trim().is_empty();
    }
    if let Some((key, value)) = line.split_once(':') {
        return credential_assignment_key(key).is_some()
            && looks_like_colon_credential_value(value);
    }
    false
}

fn credential_assignment_key(value: &str) -> Option<&str> {
    let value = value.trim().trim_matches(['"', '\'']);
    (!value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        && is_credential_key(value))
    .then_some(value)
}

fn looks_like_colon_credential_value(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let type_like = [
        "string", "str", "&str", "bool", "usize", "isize", "u8", "u16", "u32", "u64", "u128", "i8",
        "i16", "i32", "i64", "i128", "f32", "f64",
    ]
    .contains(&value)
        || value.starts_with(['&', '[', '('])
        || (value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_uppercase())
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || "<>&[](),_: ".contains(character)
            }));
    !type_like
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn grades_filter_and_replay_from_cursor() {
        let mut journal = TranscriptJournal::new(3);
        journal.push(TranscriptLevel::Turn, "turn", json!({"text":"a"}));
        let cursor = journal.push(TranscriptLevel::Block, "block", json!({"text":"b"}));
        journal.push(TranscriptLevel::Delta, "delta", json!({"text":"c"}));
        assert_eq!(journal.after(cursor, TranscriptGrade::Turn).len(), 0);
        assert_eq!(journal.after(0, TranscriptGrade::Block).len(), 2);
        assert_eq!(journal.after(cursor, TranscriptGrade::Delta).len(), 1);
    }

    #[test]
    fn redacts_credentials_before_journaling() {
        let mut journal = TranscriptJournal::new(2);
        journal.push(
            TranscriptLevel::Block,
            "tool",
            json!({"nested":{"token":"sensitive","result":"ok"}}),
        );
        let event = &journal.after(0, TranscriptGrade::Delta)[0];
        assert_eq!(event.payload["nested"]["token"], "[REDACTED]");
    }

    #[test]
    fn redacts_structured_provider_credential_aliases() {
        assert_eq!(
            redact(json!({
                "credential": "first-secret",
                "provider_credentials": "second-secret"
            })),
            json!({
                "credential": "[REDACTED]",
                "provider_credentials": "[REDACTED]"
            })
        );
    }

    #[test]
    fn redacts_credential_markers_inside_json_scalars_and_arrays() {
        let mut scalar = r#""access_token=scalar-secret""#.to_string();
        redact_output_content(&mut scalar);
        assert_eq!(scalar, "[REDACTED]");

        let mut array = r#"["access_token=array-secret"]"#.to_string();
        redact_output_content(&mut array);
        assert_eq!(array, "[REDACTED]");
    }

    #[test]
    fn redacts_credentials_embedded_in_tool_argument_strings() {
        use crate::headless::messages::FromAgentMessage;

        let call = redact_agent_message(FromAgentMessage::ToolCall {
            call_id: "call".into(),
            tool_execution_id: None,
            tool: "bash".into(),
            args: serde_json::json!({
                "command": "curl -H 'Authorization: Bearer abc/remaining+secret~==' example.test; curl --data password=abc,comma-tail-secret; password=(array-secret other-secret); password=$(printf dynamic-secret); password=`printf legacy-secret`; printf '%s' 'foo\\'; password=abc; echo ok",
                "basic": "curl -H 'Authorization: Basic dG9vbDp0b29sLXNlY3JldA==' example.test",
                "negotiate": "curl -H 'Authorization: Negotiate TlRMTVNTUAAB' example.test",
                "digest": "curl -H 'Authorization: Digest username=\"alice\", nonce=\"nonce-secret\", response=\"response-secret\"' example.test",
                "sigv4": "curl -H 'Authorization: AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20260729/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=sigv4-signature-secret' example.test",
                "quoted_password": "curl --data 'password=abc;remaining-secret' example.test",
                "embedded_quoted_password": "curl --data 'user=x&password=abc;embedded-tail-secret' example.test",
                "quoted_to_unquoted_password": "curl --data password='abc'raw-password-secret example.test",
                "unquoted_to_quoted_password": "curl --data password=abc'remaining-secret' example.test",
                "repeated_password_segments": "curl --data password=a'b'c\"d\"e example.test",
                "source": "password: String\ntoken: Option<String>",
                "vaulted": "curl -H 'Authorization: Bearer {{CRED:token:abcdef012345}}' example.test",
                "vaulted_adjacent": "password={{CRED:token:abcdef012345}}raw-adjacent-secret",
                "malformed_closed": "{{CRED:sk-ant-abcdefghijklmnopqrstuvwxyz123456}}",
                "malformed_delimited": "{{CRED:password:abc,delimiter-tail-secret}}",
                "malformed_whitespace":
                    "{{CRED:password:abc whitespace-tail-secret}} preserved-tail"
            }),
            requires_approval: false,
        });
        let serialized = serde_json::to_string(&call).unwrap();
        assert!(!serialized.contains("remaining+secret"));
        assert!(!serialized.contains("dG9vbDp0b29sLXNlY3JldA"));
        assert!(!serialized.contains("TlRMTVNTUAAB"));
        assert!(!serialized.contains("nonce-secret"));
        assert!(!serialized.contains("response-secret"));
        assert!(!serialized.contains("20260729/us-east-1/s3/aws4_request"));
        assert!(!serialized.contains("sigv4-signature-secret"));
        assert!(!serialized.contains("abc;remaining-secret"));
        assert!(!serialized.contains("remaining-secret"));
        assert!(!serialized.contains("embedded-tail-secret"));
        assert!(!serialized.contains("raw-password-secret"));
        assert!(!serialized.contains("a'b'c"));
        assert!(!serialized.contains("\"d\"e"));
        assert!(!serialized.contains("comma-tail-secret"));
        assert!(!serialized.contains("array-secret"));
        assert!(!serialized.contains("other-secret"));
        assert!(!serialized.contains("dynamic-secret"));
        assert!(!serialized.contains("legacy-secret"));
        assert!(serialized.contains("echo ok"), "{serialized}");
        assert!(serialized.contains("curl -H"));
        assert!(serialized.contains("[REDACTED:token:portable-export]"));
        assert!(!serialized.contains("password: String"));
        assert!(!serialized.contains("token: Option<String>"));
        assert!(serialized.contains("[REDACTED:password:portable-export]"));
        assert!(serialized.contains("[REDACTED:token:portable-export]"));
        assert!(serialized.contains("Bearer {{CRED:token:abcdef012345}}"));
        assert!(!serialized.contains("raw-adjacent-secret"));
        assert!(
            serialized.contains("{{CRED:token:abcdef012345}}[REDACTED:password:portable-export]")
        );
        assert!(!serialized.contains("sk-ant-abcdefghijklmnopqrstuvwxyz123456"));
        assert!(!serialized.contains("{{CRED:sk-ant-"));
        assert!(!serialized.contains("delimiter-tail-secret"));
        assert!(!serialized.contains("whitespace-tail-secret"));
        assert!(!serialized.contains("{{CRED:password:abc "));
        assert!(serialized.contains("preserved-tail"));
        assert!(serialized.contains("[REDACTED:credential_reference:portable-export]"));

        let ordinary_command = redact_agent_message(FromAgentMessage::ToolCall {
            call_id: "call".into(),
            tool_execution_id: None,
            tool: "bash".into(),
            args: serde_json::json!({"command": "rg authorization packages"}),
            requires_approval: false,
        });
        assert_eq!(
            serde_json::to_value(ordinary_command).unwrap()["args"]["command"],
            "rg authorization packages"
        );
    }

    #[test]
    fn redacts_credentials_in_all_argument_bearing_agent_messages() {
        use crate::headless::messages::{FromAgentMessage, ServerRequestType};

        let client_request = redact_agent_message(FromAgentMessage::ClientToolRequest {
            call_id: "client-call".into(),
            tool_execution_id: None,
            tool: "bash".into(),
            args: serde_json::json!({
                "command": "curl -H 'Authorization: Bearer client/inline+secret~==' example.test",
                "basic": "curl -H 'Authorization: Basic Y2xpZW50OmNsaWVudC1zZWNyZXQ=' example.test"
            }),
        });
        let server_request = redact_agent_message(FromAgentMessage::ServerRequest {
            request_id: "approval".into(),
            request_type: ServerRequestType::Approval,
            call_id: "server-call".into(),
            tool_execution_id: None,
            tool: "bash".into(),
            args: serde_json::json!({
                "command": "curl -H 'Authorization: Bearer server/inline+secret~==' example.test",
                "basic": "curl -H 'Authorization: Basic c2VydmVyOnNlcnZlci1zZWNyZXQ=' example.test"
            }),
            reason: "approval required".into(),
            started_at_ms: None,
        });

        let client = serde_json::to_string(&client_request).unwrap();
        assert!(!client.contains("client/inline+secret"));
        assert!(!client.contains("Y2xpZW50OmNsaWVudC1zZWNyZXQ"));
        assert!(client.contains("[REDACTED:token:portable-export]"));
        assert!(client.contains("[REDACTED:password:portable-export]"));
        let server = serde_json::to_string(&server_request).unwrap();
        assert!(!server.contains("server/inline+secret"));
        assert!(!server.contains("c2VydmVyOnNlcnZlci1zZWNyZXQ"));
        assert!(server.contains("[REDACTED:token:portable-export]"));
        assert!(server.contains("[REDACTED:password:portable-export]"));
    }

    #[test]
    fn redacts_structured_tool_args_and_credential_shaped_output() {
        use crate::headless::messages::{FromAgentMessage, UtilityCommandStream};

        let call = redact_agent_message(FromAgentMessage::ToolCall {
            call_id: "call".into(),
            tool_execution_id: None,
            tool: "http".into(),
            args: serde_json::json!({
                "headers": {
                    "authorization": "Bearer secret",
                    "X-API-Key": "prefixed-secret"
                }
            }),
            requires_approval: false,
        });
        let output = redact_agent_message(FromAgentMessage::ToolOutput {
            call_id: "call".into(),
            content: "access_token=secret".into(),
        });
        assert!(
            !serde_json::to_string(&call)
                .unwrap()
                .contains("Bearer secret")
        );
        assert!(
            !serde_json::to_string(&call)
                .unwrap()
                .contains("prefixed-secret")
        );

        let noncredential = redact_agent_message(FromAgentMessage::ToolCall {
            call_id: "call".into(),
            tool_execution_id: None,
            tool: "generate".into(),
            args: serde_json::json!({"max_tokens": 1000}),
            requires_approval: false,
        });
        assert_eq!(
            serde_json::to_value(noncredential).unwrap()["args"]["max_tokens"],
            1000
        );
        assert!(
            !serde_json::to_string(&output)
                .unwrap()
                .contains("access_token")
        );

        let utility_output = redact_agent_message(FromAgentMessage::UtilityCommandOutput {
            command_id: "command".into(),
            stream: UtilityCommandStream::Stderr,
            content: "OPENAI_API_KEY=secret".into(),
        });
        assert!(
            !serde_json::to_string(&utility_output)
                .unwrap()
                .contains("OPENAI_API_KEY")
        );

        let file_read = redact_agent_message(FromAgentMessage::UtilityFileReadResult {
            read_id: "read".into(),
            path: "/workspace/.env".into(),
            relative_path: ".env".into(),
            cwd: "/workspace".into(),
            content: "SERVICE_TOKEN=file-secret".into(),
            start_line: 1,
            end_line: 1,
            total_lines: 1,
            truncated: false,
        });
        assert!(
            !serde_json::to_string(&file_read)
                .unwrap()
                .contains("file-secret")
        );

        let prose = redact_agent_message(FromAgentMessage::ToolOutput {
            call_id: "call".into(),
            content: "search results for authorization middleware".into(),
        });
        assert!(
            serde_json::to_string(&prose)
                .unwrap()
                .contains("authorization middleware")
        );

        let typed_source = redact_agent_message(FromAgentMessage::ToolOutput {
            call_id: "call".into(),
            content: "token: Option<String>\npassword: String\nlet token = parser.next();".into(),
        });
        let typed_source = serde_json::to_string(&typed_source).unwrap();
        assert!(typed_source.contains("Option<String>"));
        assert!(typed_source.contains("let token = parser.next();"));

        let connection_url = redact_agent_message(FromAgentMessage::UtilityFileReadResult {
            read_id: "read".into(),
            path: "/workspace/.env".into(),
            relative_path: ".env".into(),
            cwd: "/workspace".into(),
            content: "DATABASE_URL=postgres://alice:s3cr3t@db/internal".into(),
            start_line: 1,
            end_line: 1,
            total_lines: 1,
            truncated: false,
        });
        assert!(
            !serde_json::to_string(&connection_url)
                .unwrap()
                .contains("s3cr3t")
        );

        let compact_json = redact_agent_message(FromAgentMessage::ToolOutput {
            call_id: "call".into(),
            content: r#"{"api_key":"literal-secret","result":"ok"}"#.into(),
        });
        let compact_json = serde_json::to_string(&compact_json).unwrap();
        assert!(!compact_json.contains("literal-secret"));
        assert!(compact_json.contains("[REDACTED]"));

        let prefixed_json = redact_agent_message(FromAgentMessage::ToolOutput {
            call_id: "call".into(),
            content: r#"stdout: {"api_key":"prefixed-secret","result":"ok"}"#.into(),
        });
        let prefixed_json = serde_json::to_string(&prefixed_json).unwrap();
        assert!(!prefixed_json.contains("prefixed-secret"));
        assert!(prefixed_json.contains("[REDACTED]"));

        let private_key = redact_agent_message(FromAgentMessage::ToolOutput {
            call_id: "call".into(),
            content: "-----BEGIN PRIVATE KEY-----\nprivate-key-material\n-----END PRIVATE KEY-----"
                .into(),
        });
        let private_key = serde_json::to_string(&private_key).unwrap();
        assert!(!private_key.contains("private-key-material"));
        assert!(private_key.contains("[REDACTED]"));
    }
}
