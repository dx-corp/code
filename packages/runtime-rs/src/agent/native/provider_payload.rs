//! Vault data fields without rewriting protocol tags or continuation bytes.

use super::*;

pub(super) fn vault_history(messages: &[Message], vault: &CredentialVault) -> Result<Vec<Message>> {
    let mut safe = messages.to_vec();
    for message in &mut safe {
        match &mut message.content {
            MessageContent::Text(text) => *text = vault.vault_in_text(text),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => *text = vault.vault_in_text(text),
                        ContentBlock::Thinking { thinking, .. } => {
                            *thinking = vault.vault_in_text(thinking);
                        }
                        ContentBlock::ToolUse { input, .. } => *input = vault.vault_in_json(input),
                        ContentBlock::ToolResult { content, .. } => {
                            *content = vault.vault_in_text(content);
                        }
                        ContentBlock::Image {
                            source: ImageSource::Url { url },
                        } => {
                            *url = vault.vault_in_text(url);
                        }
                        ContentBlock::Image {
                            source: ImageSource::Base64 { owner, .. },
                        } => *owner = None,
                    }
                }
            }
        }
    }
    Ok(safe)
}

pub(super) fn attest_history(
    messages: &[Message],
    vault: &CredentialVault,
) -> Result<CredentialAttestation> {
    let attestation = vault.attest_provider_text("").map_err(anyhow::Error::msg)?;
    let text = |value: &str| check(attestation, vault.attest_provider_text(value));
    let identifier = |value: &str| check(attestation, vault.attest_provider_identifier(value));
    for message in messages {
        match &message.content {
            MessageContent::Text(value) => text(value)?,
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text: value } => text(value)?,
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } => {
                            text(thinking)?;
                            if let Some(signature) = signature {
                                identifier(signature)?;
                            }
                        }
                        ContentBlock::ToolUse {
                            id,
                            name,
                            input,
                            gemini_context,
                        } => {
                            identifier(id)?;
                            identifier(name)?;
                            check(attestation, vault.attest_provider_json(input))?;
                            if let Some(context) = gemini_context {
                                identifier(&context.native_name)?;
                                if let Some(id) = &context.native_id {
                                    identifier(id)?;
                                }
                                if let Some(signature) = &context.thought_signature {
                                    identifier(signature)?;
                                }
                            }
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            identifier(tool_use_id)?;
                            text(content)?;
                        }
                        ContentBlock::Image { source } => match source {
                            ImageSource::Base64 {
                                media_type, data, ..
                            } => {
                                identifier(media_type)?;
                                identifier(data)?;
                            }
                            ImageSource::Url { url } => text(url)?,
                        },
                    }
                }
            }
        }
    }
    Ok(attestation)
}

fn check(
    expected: CredentialAttestation,
    actual: std::result::Result<CredentialAttestation, &'static str>,
) -> Result<()> {
    anyhow::ensure!(
        expected == actual.map_err(anyhow::Error::msg)?,
        "credential vault changed during provider request preparation"
    );
    Ok(())
}

pub(super) fn schema_protocol_keyword(key: &str) -> bool {
    matches!(
        key,
        "type"
            | "required"
            | "enum"
            | "const"
            | "format"
            | "pattern"
            | "$ref"
            | "$schema"
            | "$id"
            | "$anchor"
    )
}

pub(super) fn attest_protocol_json(value: &Value, vault: &CredentialVault) -> Result<()> {
    match value {
        Value::String(text) => {
            vault
                .attest_provider_identifier(text)
                .map_err(anyhow::Error::msg)?;
        }
        Value::Array(values) => {
            for value in values {
                attest_protocol_json(value, vault)?;
            }
        }
        Value::Object(entries) => {
            for (key, value) in entries {
                vault
                    .attest_provider_identifier(key)
                    .map_err(anyhow::Error::msg)?;
                attest_protocol_json(value, vault)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn attest_schema(value: &Value, vault: &CredentialVault) -> Result<()> {
    match value {
        Value::String(text) => {
            vault
                .attest_provider_text(text)
                .map_err(anyhow::Error::msg)?;
        }
        Value::Array(values) => {
            for value in values {
                attest_schema(value, vault)?;
            }
        }
        Value::Object(entries) => {
            for (key, value) in entries {
                vault
                    .attest_provider_identifier(key)
                    .map_err(anyhow::Error::msg)?;
                if schema_protocol_keyword(key) {
                    attest_protocol_json(value, vault)?;
                } else {
                    attest_schema(value, vault)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn attest_tools(
    tools: &[Tool],
    vault: &CredentialVault,
) -> Result<CredentialAttestation> {
    let attestation = vault.attest_provider_text("").map_err(anyhow::Error::msg)?;
    for tool in tools {
        check(attestation, vault.attest_provider_identifier(&tool.name))?;
        check(attestation, vault.attest_provider_text(&tool.description))?;
        attest_schema(&tool.input_schema, vault)?;
    }
    anyhow::ensure!(
        vault.has_attestation(attestation),
        "credential vault changed during provider request preparation"
    );
    Ok(attestation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_password_cannot_rewrite_roles_tags_schema_or_continuation_bytes() {
        let vault = CredentialVault::new();
        let reference = vault.store("a", crate::agent::CredentialType::Password);
        let history = Arc::new(vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Thinking {
                        thinking: "a".into(),
                        signature: Some("dataaaaSignature".into()),
                    },
                    ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"path":"a"}),
                        gemini_context: Some(crate::ai::GeminiToolContext {
                            native_name: "bash".into(),
                            native_id: Some("call_1".into()),
                            thought_signature: Some("dataaaaSignature".into()),
                        }),
                    },
                    ContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/png".into(),
                            data: "aaaa".into(),
                            owner: None,
                        },
                    },
                ]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "a".into(),
                    is_error: Some(false),
                }]),
            },
            Message {
                role: Role::System,
                content: MessageContent::Text("a".into()),
            },
        ]);
        let original = serde_json::to_value(history.as_ref()).unwrap();
        let config = RequestConfig {
            system: Some("a".into()),
            tools: Arc::new(vec![Tool {
                name: "bash".into(),
                description: "a".into(),
                input_schema: serde_json::json!({"type":"object", "additionalProperties":false,
                    "required":["path"], "properties":{"path":{"type":"array", "items":{"type":"string"}}}}),
                output_schema: None,
                schema_enforcement: crate::ai::ToolSchemaEnforcement::Require,
            }]),
            ..RequestConfig::default()
        };
        let expected_schema = config.tools[0].input_schema.clone();
        let safe = ProviderSafeRequest::prepare(&history, config, &vault).unwrap();
        safe.ensure_current(&vault).unwrap();
        let wire = serde_json::to_value(safe.messages.as_ref()).unwrap();
        assert_eq!(wire[0]["role"], "assistant");
        assert_eq!(wire[1]["role"], "user");
        assert_eq!(wire[2]["role"], "system");
        assert_eq!(wire[0]["content"][0]["type"], "thinking");
        assert_eq!(wire[0]["content"][0]["thinking"], reference);
        assert_eq!(wire[0]["content"][0]["signature"], "dataaaaSignature");
        assert_eq!(wire[0]["content"][1]["name"], "bash");
        assert_eq!(wire[0]["content"][1]["id"], "call_1");
        assert_eq!(wire[0]["content"][1]["input"]["path"], reference);
        assert_eq!(
            wire[0]["content"][1]["gemini_context"],
            original[0]["content"][1]["gemini_context"]
        );
        assert_eq!(wire[0]["content"][2], original[0]["content"][2]);
        assert_eq!(wire[1]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(wire[1]["content"][0]["content"], reference);
        assert_eq!(wire[2]["content"], reference);
        assert_eq!(safe.config.system.as_deref(), Some(reference.as_str()));
        assert_eq!(safe.config.tools[0].description, reference);
        assert_eq!(safe.config.tools[0].input_schema, expected_schema);
        assert_eq!(
            safe.config.tools[0].schema_enforcement,
            crate::ai::ToolSchemaEnforcement::Require
        );
        assert_eq!(serde_json::to_value(history.as_ref()).unwrap(), original);
        let _: Vec<Message> =
            serde_json::from_value(wire).expect("typed history still round trips");
    }

    #[test]
    fn protocol_identifiers_still_reject_actual_credentials_and_forged_references() {
        let vault = CredentialVault::new();
        vault.store("a", crate::agent::CredentialType::Password);
        for identifier in ["a", "prefix_a_suffix", "{{CRED|password|0123456789ab}}"] {
            assert!(
                vault.attest_provider_identifier(identifier).is_err(),
                "{identifier}"
            );
        }
        assert!(
            vault.attest_provider_text("bash").is_err(),
            "free content stays strict"
        );
        assert!(
            vault
                .attest_provider_json(&serde_json::json!({"a":"safe"}))
                .is_err()
        );
        let longer = "vaulted-token-1234567890";
        vault.store(longer, crate::agent::CredentialType::Token);
        assert!(
            vault
                .attest_provider_identifier(&format!("prefix{longer}suffix"))
                .is_err()
        );
        assert!(
            vault
                .attest_provider_json(&serde_json::json!({"nested":{"value":longer}}))
                .is_err()
        );
    }
}
