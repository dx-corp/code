//! Descriptive prompt sources follow exact bytes; they confer no execution authority.
use super::*;

impl NativeAgentRunner {
    pub(super) fn request_system_parts(
        &self,
        rendered: Option<&str>,
        context: Option<&str>,
    ) -> Vec<ContentBlock> {
        let Some(rendered) = rendered else {
            return Vec::new();
        };
        describe_system_parts(
            self.config.system_prompt.as_deref().unwrap_or_default(),
            &self.config.system_prompt_parts,
            rendered,
            context,
            &self.credential_vault,
        )
    }
}

fn describe_system_parts(
    base: &str,
    declared_parts: &[ContentBlock],
    rendered: &str,
    context: Option<&str>,
    vault: &CredentialVault,
) -> Vec<ContentBlock> {
    let mut parts = declared_parts.to_vec();
    let described = render_parts(&parts);
    if described != base || parts.iter().any(|part| part.text_content().is_none()) {
        parts = if base.is_empty() {
            Vec::new()
        } else {
            unknown_parts(base)
        };
    }
    let mut prefix = base.to_owned();
    if let Some(context) = context.filter(|text| !text.trim().is_empty()) {
        if prefix.is_empty() {
            parts.clear();
        } else {
            prefix.push_str("\n\n");
        }
        prefix.push_str(context);
        parts.push(ContentBlock::attributed_text(
            context,
            maestro_ai::ContentSource::Hook,
            Some("prompt_context".into()),
        ));
    }
    let safe_prefix = vault.vault_in_text(&prefix);
    if rendered != safe_prefix {
        let suffix = if safe_prefix.is_empty() {
            Some(rendered)
        } else {
            rendered
                .strip_prefix(&safe_prefix)
                .and_then(|suffix| suffix.strip_prefix("\n\n"))
        };
        let Some(suffix) = suffix.filter(|suffix| !suffix.is_empty()) else {
            return unknown_parts(rendered);
        };
        if safe_prefix.is_empty() {
            parts.clear();
        }
        parts.push(ContentBlock::attributed_text(
            suffix,
            maestro_ai::ContentSource::Harness,
            Some("runtime.model_and_policy".into()),
        ));
    }
    vault_system_parts(&mut parts, vault);
    if render_parts(&parts) != rendered {
        return unknown_parts(rendered);
    }
    parts
}

fn render_parts(parts: &[ContentBlock]) -> String {
    parts
        .iter()
        .filter_map(ContentBlock::text_content)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn unknown_parts(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::attributed_text(
        text,
        maestro_ai::ContentSource::Unknown,
        None,
    )]
}

pub(super) fn vault_system_parts(parts: &mut [ContentBlock], vault: &CredentialVault) {
    for part in parts {
        match part {
            ContentBlock::Text { text } => *text = vault.vault_in_text(text),
            ContentBlock::AttributedText { text, provenance } => {
                vault_attributed_text(text, provenance, vault);
            }
            _ => {}
        }
    }
}

pub(super) fn vault_attributed_text(
    text: &mut String,
    provenance: &mut maestro_ai::ContentProvenance,
    vault: &CredentialVault,
) {
    *text = vault.vault_in_text(text);
    if let Some(origin) = &mut provenance.origin {
        *origin = vault.vault_in_text(origin);
    }
    for origin in &mut provenance.derived_from {
        if let Some(value) = &mut origin.origin {
            *value = vault.vault_in_text(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_ai::ContentSource;

    fn sources(parts: &[ContentBlock]) -> Vec<ContentSource> {
        parts
            .iter()
            .map(|part| match part {
                ContentBlock::AttributedText { provenance, .. } => provenance.source,
                _ => panic!("attributed part expected"),
            })
            .collect()
    }

    #[test]
    fn provenance_exact_base_and_context_keep_their_sources_without_a_runtime_suffix() {
        let vault = CredentialVault::new();
        let declared = vec![
            ContentBlock::attributed_text("policy", ContentSource::Developer, None),
            ContentBlock::attributed_text(
                "repository",
                ContentSource::Repository,
                Some("AGENTS.md".into()),
            ),
        ];
        let base = "policy\n\nrepository";
        let parts = describe_system_parts(base, &declared, base, None, &vault);
        assert_eq!(
            sources(&parts),
            vec![ContentSource::Developer, ContentSource::Repository]
        );
        assert_eq!(
            serde_json::to_value(&parts).unwrap(),
            serde_json::to_value(&declared).unwrap()
        );
        let rendered = format!("{base}\n\nhook context");
        let parts = describe_system_parts(base, &declared, &rendered, Some("hook context"), &vault);
        assert_eq!(
            sources(&parts),
            vec![
                ContentSource::Developer,
                ContentSource::Repository,
                ContentSource::Hook
            ]
        );
        assert_eq!(render_parts(&parts), rendered);
        let parts = describe_system_parts("", &[], "hook context", Some("hook context"), &vault);
        assert_eq!(sources(&parts), vec![ContentSource::Hook]);
        assert_eq!(render_parts(&parts), "hook context");
    }

    #[test]
    fn provenance_preparation_vaults_unsealed_parts_and_enforces_an_existing_seal() {
        let vault = CredentialVault::new();
        vault.store(
            "source-private-secret",
            crate::agent::CredentialType::Password,
        );
        let text = "policy source-private-secret";
        let config = RequestConfig {
            system: Some(text.into()),
            system_parts: vec![ContentBlock::attributed_text(
                text,
                ContentSource::Developer,
                None,
            )],
            ..RequestConfig::default()
        };
        let messages = Arc::new(Vec::new());
        let sanitized = ProviderSafeRequest::prepare(&messages, config, &vault).unwrap();
        assert!(
            !sanitized
                .config
                .system
                .as_deref()
                .unwrap()
                .contains("source-private-secret")
        );
        sanitized.config.validate_system_parts().unwrap();
        let mut config = sanitized.config;
        config.cache_topology = Some(
            maestro_ai::cache_topology::PreparedPrompt::prepare(
                &messages,
                &config,
                "session".into(),
                None,
            )
            .unwrap(),
        );
        ProviderSafeRequest::prepare(&messages, config.clone(), &vault).unwrap();
        let ContentBlock::AttributedText { provenance, .. } = &mut config.system_parts[0] else {
            panic!("typed part")
        };
        provenance.source = ContentSource::User;
        assert!(ProviderSafeRequest::prepare(&messages, config, &vault).is_err());
    }

    #[test]
    fn provenance_only_labels_an_actual_runtime_suffix_and_rejects_mismatched_bytes() {
        let vault = CredentialVault::new();
        let declared = vec![ContentBlock::attributed_text(
            "policy",
            ContentSource::Developer,
            None,
        )];
        let rendered = "policy\n\nruntime policy";
        let parts = describe_system_parts("policy", &declared, rendered, None, &vault);
        assert_eq!(
            sources(&parts),
            vec![ContentSource::Developer, ContentSource::Harness]
        );
        assert_eq!(render_parts(&parts), rendered);
        for changed in ["different policy", "policy\n\n"] {
            let parts = describe_system_parts("policy", &declared, changed, None, &vault);
            assert_eq!(sources(&parts), vec![ContentSource::Unknown]);
            assert_eq!(render_parts(&parts), changed);
        }
    }
}
