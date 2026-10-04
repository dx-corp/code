//! Pure checks over authored content, independent of tools, transport and storage.

use crate::TurnContentPolicy;

/// Prose and citation targets have different meanings for writing rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthoredContent {
    pub prose: String,
    pub citation_urls: Vec<String>,
    word_count: usize,
}

impl AuthoredContent {
    /// Preserve visible labels, but never treat a URL target as authored prose.
    pub fn from_text(text: &str) -> Self {
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
    /// Safe feedback: never quotes rejected prose or URLs.
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentPolicyEvaluation {
    pub word_count: usize,
    pub violations: Vec<ContentPolicyViolation>,
}

impl TurnContentPolicy {
    pub fn has_deterministic_controls(&self) -> bool {
        !self.required_terms.is_empty()
            || !self.forbidden_terms.is_empty()
            || self.require_citations
            || !self.allowed_citation_domains.is_empty()
            || self.max_response_words > 0
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
    evaluation
}

#[cfg(test)]
mod tests {
    use super::*;

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
