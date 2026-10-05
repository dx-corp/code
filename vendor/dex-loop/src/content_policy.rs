//! Pure checks over authored content, independent of tools, transport and storage.

use crate::{HeadingCase, TurnContentPolicy, WritingRules};

/// Prose and citation targets have different meanings for writing rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthoredContent {
    prose: String,
    pub citation_urls: Vec<String>,
    word_count: usize,
    pub fields: Vec<AuthoredField>,
    pub is_presentation: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthoredFieldKind {
    #[default]
    Body,
    Heading,
    Bullet,
    List,
    Slide,
    Citation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthoredField {
    pub path: String,
    pub prose: String,
    pub kind: AuthoredFieldKind,
    pub items: usize,
    pub citation_urls: Vec<String>,
}

impl AuthoredContent {
    /// Preserve visible labels, but never treat a URL target as authored prose.
    fn plain_text(text: &str) -> Self {
        let mut content = Self {
            word_count: text.split_whitespace().count(),
            ..Self::default()
        };
        let mut rest = text;
        loop {
            let lower = rest.to_ascii_lowercase();
            let start = [lower.find("http://"), lower.find("https://")]
                .into_iter()
                .flatten()
                .min();
            let Some(start) = start else {
                content.prose.push_str(rest);
                break;
            };
            content.prose.push_str(&rest[..start]);
            let target = &rest[start..];
            let end = target
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, ')' | ']' | '}' | '>' | '"' | '\'')
                })
                .unwrap_or(target.len());
            let candidate = target[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
            if http_url(candidate).is_some() {
                content.citation_urls.push(candidate.into());
            }
            // Keep word boundaries around removed targets.
            content.prose.push(' ');
            rest = &target[end..];
        }
        content
    }

    pub fn from_text(text: &str) -> Self {
        let mut content = Self::plain_text(text);
        let mut bullets = 0;
        let normalized_lines = text.replace("\r\n", "\n");
        for (index, paragraph) in normalized_lines.split("\n\n").enumerate() {
            let lines: Vec<_> = paragraph.lines().collect();
            if lines
                .iter()
                .any(|line| markdown_kind(line) != AuthoredFieldKind::Body)
            {
                let mut pending: Option<(usize, AuthoredFieldKind, String)> = None;
                for (line_index, line) in lines.into_iter().enumerate() {
                    let kind = markdown_kind(line);
                    if matches!(kind, AuthoredFieldKind::Heading | AuthoredFieldKind::Bullet) {
                        if let Some((start, previous_kind, text)) = pending.take() {
                            content.fields.push(Self::field(
                                format!("response.paragraphs[{index}].lines[{start}]"),
                                previous_kind,
                                &text,
                            ));
                        }
                        bullets += usize::from(kind == AuthoredFieldKind::Bullet);
                        let prose = markdown_prose(line, kind);
                        if kind == AuthoredFieldKind::Heading {
                            content.fields.push(Self::field(
                                format!("response.paragraphs[{index}].lines[{line_index}]"),
                                kind,
                                prose,
                            ));
                        } else {
                            pending = Some((line_index, kind, prose.into()));
                        }
                    } else if let Some((_, _, text)) = &mut pending {
                        text.push('\n');
                        text.push_str(line.trim());
                    } else {
                        pending = Some((line_index, kind, line.into()));
                    }
                }
                if let Some((start, kind, text)) = pending {
                    content.fields.push(Self::field(
                        format!("response.paragraphs[{index}].lines[{start}]"),
                        kind,
                        &text,
                    ));
                }
            } else {
                content.fields.push(Self::field(
                    format!("response.paragraphs[{index}]"),
                    AuthoredFieldKind::Body,
                    paragraph,
                ));
            }
        }
        content.fields.push(AuthoredField {
            path: "response.bullets".into(),
            prose: String::new(),
            kind: AuthoredFieldKind::List,
            items: bullets,
            citation_urls: Vec::new(),
        });
        content
    }

    fn field(path: String, kind: AuthoredFieldKind, text: &str) -> AuthoredField {
        let plain = Self::plain_text(text);
        AuthoredField {
            path,
            prose: plain.prose,
            kind,
            items: 0,
            citation_urls: plain.citation_urls,
        }
    }

    /// Paths are constructed by the typed adapter, never copied from authored prose or IDs.
    pub fn push_field(&mut self, path: String, kind: AuthoredFieldKind, text: &str) {
        let plain = Self::plain_text(text);
        self.word_count += plain.word_count;
        self.prose.push_str(&plain.prose);
        self.prose.push('\n');
        self.citation_urls
            .extend(plain.citation_urls.iter().cloned());
        self.fields.push(AuthoredField {
            path,
            prose: plain.prose,
            kind,
            items: 0,
            citation_urls: plain.citation_urls,
        });
    }

    /// Aggregate checks do not duplicate prose in the artifact's term/citation projection.
    pub fn push_group(
        &mut self,
        path: String,
        kind: AuthoredFieldKind,
        prose: String,
        items: usize,
    ) {
        self.fields.push(AuthoredField {
            path,
            kind,
            prose,
            items,
            citation_urls: Vec::new(),
        });
    }

    pub fn push_citation(&mut self, path: String, url: &str) {
        self.citation_urls.push(url.into());
        self.fields.push(AuthoredField {
            path,
            prose: String::new(),
            kind: AuthoredFieldKind::Citation,
            items: 0,
            citation_urls: vec![url.into()],
        });
    }
}

fn markdown_prose(line: &str, kind: AuthoredFieldKind) -> &str {
    let line = line.trim_start();
    match kind {
        AuthoredFieldKind::Heading => line.trim_start_matches('#').trim_start(),
        AuthoredFieldKind::Bullet => {
            if let Some(text) = ordered_list_prose(line) {
                text
            } else {
                &line[2..]
            }
        }
        _ => line,
    }
}

fn ordered_list_prose(line: &str) -> Option<&str> {
    [". ", ") "].into_iter().find_map(|marker| {
        line.split_once(marker).and_then(|(prefix, text)| {
            (!prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit())).then_some(text)
        })
    })
}

fn markdown_kind(line: &str) -> AuthoredFieldKind {
    let line = line.trim_start();
    if line.starts_with('#') && line.trim_start_matches('#').starts_with(' ') {
        return AuthoredFieldKind::Heading;
    }
    if ["- ", "* ", "+ "]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        || ordered_list_prose(line).is_some()
    {
        return AuthoredFieldKind::Bullet;
    }
    AuthoredFieldKind::Body
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentPolicyScope {
    Response,
    Progress,
    Artifact,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentPolicyViolation {
    pub code: &'static str,
    pub rule_id: String,
    pub field_path: String,
    /// Safe feedback: never quotes rejected prose or URLs.
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentPolicyEvaluation {
    pub word_count: usize,
    pub violations: Vec<ContentPolicyViolation>,
}

impl TurnContentPolicy {
    pub(crate) fn has_deterministic_controls(&self) -> bool {
        !self.required_terms.is_empty()
            || !self.forbidden_terms.is_empty()
            || self.require_citations
            || !self.allowed_citation_domains.is_empty()
            || self.max_response_words > 0
            || !self.response_rules.is_empty()
            || !self.document_rules.is_empty()
            || !self.presentation_rules.is_empty()
    }
}

fn http_url(value: &str) -> Option<url::Url> {
    url::Url::parse(value)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

pub fn evaluate_content_policy(
    policy: &TurnContentPolicy,
    content: &AuthoredContent,
    scope: ContentPolicyScope,
) -> ContentPolicyEvaluation {
    let mut evaluation = ContentPolicyEvaluation {
        word_count: content.word_count,
        violations: Vec::new(),
    };
    let normalized = content.prose.to_ascii_lowercase();
    let mut reject = |code, message: &str| {
        evaluation.violations.push(ContentPolicyViolation {
            code,
            rule_id: code.into(),
            field_path: if scope == ContentPolicyScope::Artifact {
                "artifact"
            } else {
                "response"
            }
            .into(),
            message: message.into(),
        })
    };
    if scope != ContentPolicyScope::Progress {
        for term in &policy.required_terms {
            if !normalized.contains(&term.to_ascii_lowercase()) {
                reject(
                    "artifact_style_guide_required_term_missing",
                    "Content is missing a required workspace term.",
                );
            }
        }
    }
    for term in &policy.forbidden_terms {
        if !term.is_empty() && normalized.contains(&term.to_ascii_lowercase()) {
            reject(
                "artifact_style_guide_forbidden_term",
                "Content contains a prohibited workspace term.",
            );
        }
    }
    let citations: Vec<_> = content
        .citation_urls
        .iter()
        .filter_map(|value| http_url(value))
        .collect();
    if scope != ContentPolicyScope::Progress && policy.require_citations && citations.is_empty() {
        reject(
            "content_policy_citation_required",
            "Content must include a valid HTTP or HTTPS source link.",
        );
    }
    if !policy.allowed_citation_domains.is_empty()
        && citations.iter().any(|url| {
            url.host_str().is_none_or(|host| {
                !policy.allowed_citation_domains.iter().any(|allowed| {
                    host.eq_ignore_ascii_case(allowed)
                        || host
                            .to_ascii_lowercase()
                            .ends_with(&format!(".{}", allowed.to_ascii_lowercase()))
                })
            })
        })
    {
        reject(
            "content_policy_citation_domain_forbidden",
            "Content cites a domain outside the workspace allowlist.",
        );
    }
    if scope == ContentPolicyScope::Response
        && policy.max_response_words > 0
        && content.word_count > policy.max_response_words as usize
    {
        reject(
            "content_policy_response_too_long",
            "Response exceeds the workspace word limit.",
        );
    }
    let mut missing_terms = policy
        .required_terms
        .iter()
        .enumerate()
        .filter(|(_, term)| !normalized.contains(&term.to_ascii_lowercase()))
        .map(|(index, _)| index);
    let mut forbidden_matches = policy
        .forbidden_terms
        .iter()
        .enumerate()
        .filter(|(_, term)| !term.is_empty() && normalized.contains(&term.to_ascii_lowercase()));
    for violation in &mut evaluation.violations {
        if violation.code == "artifact_style_guide_required_term_missing" {
            if let Some(index) = missing_terms.next() {
                violation.rule_id = format!("required_terms[{index}]");
            }
        } else if violation.code == "artifact_style_guide_forbidden_term" {
            if let Some((index, term)) = forbidden_matches.next() {
                violation.rule_id = format!("forbidden_terms[{index}]");
                if let Some(field) = content.fields.iter().find(|field| {
                    field
                        .prose
                        .to_ascii_lowercase()
                        .contains(&term.to_ascii_lowercase())
                }) {
                    violation.field_path.clone_from(&field.path);
                }
            }
        } else if violation.code == "content_policy_citation_domain_forbidden"
            && let Some(field) = content.fields.iter().find(|field| {
                field
                    .citation_urls
                    .iter()
                    .filter_map(|url| http_url(url))
                    .any(|url| !allowed_host(policy, &url))
            })
        {
            violation.field_path.clone_from(&field.path);
        }
    }
    let (rules, scope_name) = if scope == ContentPolicyScope::Artifact {
        if content.is_presentation {
            (&policy.presentation_rules, "presentation_rules")
        } else {
            (&policy.document_rules, "document_rules")
        }
    } else {
        (&policy.response_rules, "response_rules")
    };
    evaluate_writing_rules(rules, scope_name, content, &mut evaluation.violations);
    evaluation.violations.truncate(32);
    evaluation
}

fn allowed_host(policy: &TurnContentPolicy, url: &url::Url) -> bool {
    url.host_str().is_some_and(|host| {
        policy.allowed_citation_domains.iter().any(|allowed| {
            host.eq_ignore_ascii_case(allowed)
                || host
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{}", allowed.to_ascii_lowercase()))
        })
    })
}

fn evaluate_writing_rules(
    rules: &WritingRules,
    scope: &str,
    content: &AuthoredContent,
    violations: &mut Vec<ContentPolicyViolation>,
) {
    for field in &content.fields {
        let mut check = |code: &'static str, name: &str, count: usize, limit: u32| {
            if limit > 0 && count > limit as usize {
                violations.push(ContentPolicyViolation {
                    code,
                    rule_id: format!("{scope}.{name}"),
                    field_path: field.path.clone(),
                    message: format!("{name} limit is {limit}; this field has {count}."),
                });
            }
        };
        let words = |text: &str| {
            text.split_whitespace()
                .filter(|word| {
                    *word != "#" && !word.chars().all(|c| matches!(c, '#' | '-' | '*' | '+'))
                })
                .count()
        };
        match field.kind {
            AuthoredFieldKind::Slide => check(
                "writing_rule_slide_words",
                "max_slide_words",
                words(&field.prose),
                rules.max_slide_words,
            ),
            AuthoredFieldKind::List => check(
                "writing_rule_bullet_count",
                "max_bullets",
                field.items,
                rules.max_bullets,
            ),
            AuthoredFieldKind::Citation => {}
            kind => {
                if kind == AuthoredFieldKind::Heading {
                    check(
                        "writing_rule_heading_words",
                        "max_heading_words",
                        words(&field.prose),
                        rules.max_heading_words,
                    );
                }
                if kind == AuthoredFieldKind::Bullet {
                    check(
                        "writing_rule_bullet_words",
                        "max_bullet_words",
                        words(&field.prose),
                        rules.max_bullet_words,
                    );
                }
                let normalized_lines = field.prose.replace("\r\n", "\n");
                for paragraph in normalized_lines.split("\n\n") {
                    check(
                        "writing_rule_paragraph_words",
                        "max_paragraph_words",
                        words(paragraph),
                        rules.max_paragraph_words,
                    );
                }
                if kind != AuthoredFieldKind::Heading {
                    for sentence in field.prose.split(['.', '!', '?', '\n']) {
                        check(
                            "writing_rule_sentence_words",
                            "max_sentence_words",
                            words(sentence),
                            rules.max_sentence_words,
                        );
                    }
                }
                if kind == AuthoredFieldKind::Heading
                    && !heading_case_matches(&field.prose, rules.heading_case)
                {
                    violations.push(ContentPolicyViolation {
                        code: "writing_rule_heading_case",
                        rule_id: format!("{scope}.heading_case"),
                        field_path: field.path.clone(),
                        message: "Heading does not follow the configured case convention.".into(),
                    });
                }
            }
        }
        // Bound feedback even for a large artifact with many violating fields.
        if violations.len() >= 32 {
            violations.truncate(32);
            break;
        }
    }
}

fn heading_case_matches(text: &str, case: HeadingCase) -> bool {
    let words: Vec<_> = text
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphabetic()))
        .filter(|word| !word.is_empty())
        .collect();
    words.iter().enumerate().all(|(index, word)| {
        if case == HeadingCase::Unspecified
            || word
                .chars()
                .filter(|c| c.is_alphabetic())
                .all(|c| c.is_uppercase())
        {
            return true;
        }
        let first = word.chars().next().unwrap_or(' ');
        match case {
            HeadingCase::Unspecified => true,
            HeadingCase::Sentence => {
                if index == 0 {
                    first.is_uppercase()
                } else {
                    first.is_lowercase()
                }
            }
            HeadingCase::Title => {
                if index > 0
                    && [
                        "a", "an", "and", "as", "at", "by", "for", "in", "of", "on", "or", "the",
                        "to", "with",
                    ]
                    .contains(&word.to_ascii_lowercase().as_str())
                {
                    first.is_lowercase()
                } else {
                    first.is_uppercase()
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_rules_check_markdown_heading_sentence_paragraph_and_bullets() {
        let policy: TurnContentPolicy = serde_json::from_value(serde_json::json!({
            "guide_version":0, "response_rules": {"max_sentence_words": 3, "max_heading_words": 2, "max_bullet_words": 2, "max_bullets": 1, "heading_case": "sentence"}
        })).unwrap();
        let result = evaluate_content_policy(
            &policy,
            &AuthoredContent::from_text(
                "# A Heading With Words\n\n- three bullet words\n- another item\n\nThis sentence has many words.",
            ),
            ContentPolicyScope::Response,
        );
        assert!(
            result
                .violations
                .iter()
                .any(|v| v.code == "writing_rule_sentence_words")
        );
        assert!(
            result
                .violations
                .iter()
                .any(|v| v.code == "writing_rule_heading_words")
        );
        assert!(
            result
                .violations
                .iter()
                .any(|v| v.code == "writing_rule_bullet_words")
        );
        assert!(
            result
                .violations
                .iter()
                .any(|v| v.code == "writing_rule_bullet_count")
        );
        assert!(
            result
                .violations
                .iter()
                .any(|v| v.code == "writing_rule_heading_case")
        );
    }

    #[test]
    fn markdown_markers_do_not_count_as_authored_words() {
        let policy = TurnContentPolicy {
            response_rules: WritingRules {
                max_bullet_words: 3,
                max_paragraph_words: 3,
                max_heading_words: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        for text in [
            "1. Keep it brief",
            "12. Keep it brief",
            "1) Keep it brief",
            "- Keep it brief",
            "## Keep it brief",
        ] {
            assert!(
                evaluate_content_policy(
                    &policy,
                    &AuthoredContent::from_text(text),
                    ContentPolicyScope::Response
                )
                .violations
                .is_empty(),
                "{text}"
            );
        }
        assert!(
            !evaluate_content_policy(
                &policy,
                &AuthoredContent::from_text("1. Please keep it brief"),
                ContentPolicyScope::Response
            )
            .violations
            .is_empty()
        );
    }

    #[test]
    fn markdown_continuations_preserve_paragraph_and_bullet_limits() {
        for (text, rules, expected) in [
            (
                "# Heading\none two\nthree four",
                WritingRules {
                    max_paragraph_words: 3,
                    ..Default::default()
                },
                "writing_rule_paragraph_words",
            ),
            (
                "- one\n  two three four",
                WritingRules {
                    max_bullet_words: 3,
                    ..Default::default()
                },
                "writing_rule_bullet_words",
            ),
        ] {
            let policy = TurnContentPolicy {
                response_rules: rules,
                ..Default::default()
            };
            assert!(
                evaluate_content_policy(
                    &policy,
                    &AuthoredContent::from_text(text),
                    ContentPolicyScope::Response
                )
                .violations
                .iter()
                .any(|violation| violation.code == expected)
            );
        }
    }

    #[test]
    fn ordered_parenthesis_markers_cannot_bypass_bullet_rules() {
        let policy = TurnContentPolicy {
            response_rules: WritingRules {
                max_bullet_words: 3,
                max_bullets: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let evaluation = evaluate_content_policy(
            &policy,
            &AuthoredContent::from_text("1) one two three four\n2) five"),
            ContentPolicyScope::Response,
        );
        for code in ["writing_rule_bullet_words", "writing_rule_bullet_count"] {
            assert!(
                evaluation
                    .violations
                    .iter()
                    .any(|violation| violation.code == code)
            );
        }
    }

    #[test]
    fn crlf_blank_lines_preserve_response_and_artifact_paragraph_boundaries() {
        let rules = WritingRules {
            max_paragraph_words: 3,
            ..Default::default()
        };
        let policy = TurnContentPolicy {
            response_rules: rules.clone(),
            document_rules: rules,
            ..Default::default()
        };
        let text = "one two\r\n\r\nthree four";
        let response = AuthoredContent::from_text(text);
        let mut artifact = AuthoredContent::default();
        artifact.push_field(
            "documentBlocks[0].body".into(),
            AuthoredFieldKind::Body,
            text,
        );
        for (content, scope) in [
            (response, ContentPolicyScope::Response),
            (artifact, ContentPolicyScope::Artifact),
        ] {
            assert!(
                evaluate_content_policy(&policy, &content, scope)
                    .violations
                    .is_empty()
            );
        }
    }

    #[test]
    fn url_targets_are_not_prose_but_link_labels_are() {
        let policy = TurnContentPolicy {
            required_terms: vec!["Deixic".into()],
            forbidden_terms: vec!["foster".into()],
            ..Default::default()
        };
        let missing = AuthoredContent::from_text("Read [Evidence](https://example.com/Deixic).");
        assert_eq!(
            evaluate_content_policy(&policy, &missing, ContentPolicyScope::Response).violations[0]
                .code,
            "artifact_style_guide_required_term_missing"
        );
        let good = AuthoredContent::from_text("Read [Deixic](https://example.com/foster).");
        assert!(
            evaluate_content_policy(&policy, &good, ContentPolicyScope::Response)
                .violations
                .is_empty()
        );
        let bad = AuthoredContent::from_text("[FOSTER](https://example.com/Deixic) Deixic");
        assert_eq!(
            evaluate_content_policy(&policy, &bad, ContentPolicyScope::Response).violations[0].code,
            "artifact_style_guide_forbidden_term"
        );
    }

    #[test]
    fn citation_hosts_match_only_exact_or_dot_delimited_subdomains() {
        let policy = TurnContentPolicy {
            require_citations: true,
            allowed_citation_domains: vec!["Example.com".into()],
            ..Default::default()
        };
        for link in ["https://example.com/a", "http://docs.example.com/a"] {
            assert!(
                evaluate_content_policy(
                    &policy,
                    &AuthoredContent::from_text(link),
                    ContentPolicyScope::Artifact
                )
                .violations
                .is_empty()
            );
        }
        for link in [
            "https://evilexample.com/a",
            "https://example.com.evil.invalid/a",
            "https://example.com@evil.invalid/a",
        ] {
            assert_eq!(
                evaluate_content_policy(
                    &policy,
                    &AuthoredContent::from_text(link),
                    ContentPolicyScope::Artifact
                )
                .violations[0]
                    .code,
                "content_policy_citation_domain_forbidden"
            );
        }
        assert_eq!(
            evaluate_content_policy(
                &policy,
                &AuthoredContent::from_text("No source"),
                ContentPolicyScope::Artifact
            )
            .violations[0]
                .code,
            "content_policy_citation_required"
        );
    }

    #[test]
    fn progress_does_not_require_final_terms_citations_or_response_length() {
        let policy = TurnContentPolicy {
            required_terms: vec!["Deixic".into()],
            require_citations: true,
            max_response_words: 1,
            ..Default::default()
        };
        let content = AuthoredContent::from_text("Checking the evidence.");
        assert!(
            evaluate_content_policy(&policy, &content, ContentPolicyScope::Progress)
                .violations
                .is_empty()
        );
        assert_eq!(
            evaluate_content_policy(&policy, &content, ContentPolicyScope::Response)
                .violations
                .len(),
            3
        );
        let linked = AuthoredContent::from_text("Deixic [Source](https://example.com/a)");
        assert_eq!(
            evaluate_content_policy(&policy, &linked, ContentPolicyScope::Response).word_count,
            2
        );
        assert!(
            evaluate_content_policy(&policy, &linked, ContentPolicyScope::Artifact)
                .violations
                .is_empty()
        );
    }
}
