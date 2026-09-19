//! Credential Store - Secure In-Memory Credential Vault
//!
//! This module provides secure storage for credentials (API keys, tokens, etc.)
//! that are detected during agent execution. Instead of blocking tool calls
//! containing credentials, we:
//!
//! 1. Detect the credential in tool arguments
//! 2. Store it securely in memory with a unique reference ID
//! 3. Replace the raw credential with a reference token
//! 4. Resolve references back to real values at execution time
//!
//! This approach allows users to provide test API keys without triggering
//! "credential leaked" errors, while still maintaining security by keeping
//! raw credentials out of the conversation context.
//!
//! # Reference Format
//!
//! Credentials are replaced with: `{{CRED:type:id}}`
//! - `type`: The credential type (e.g., "api_key", "token")
//! - `id`: A unique identifier for retrieval
//!
//! # Example
//!
//! ```rust
//! use maestro_runtime::agent::credential_store::{CredentialStore, CredentialType};
//!
//! let mut store = CredentialStore::new();
//!
//! // Store a credential and get a reference
//! let reference = store.store("sk-ant-abc123", CredentialType::ApiKey);
//! // Returns something like: "{{CRED:api_key:a1b2c3d4e5f6}}"
//!
//! // Resolve a reference back to the real value
//! let value = store.resolve(&reference);
//! assert_eq!(value, Some("sk-ant-abc123".to_string()));
//!
//! // Resolve all references in a string
//! let cmd = format!("curl -H 'Authorization: Bearer {}'", reference);
//! let resolved = store.resolve_all(&cmd);
//! // Returns: "curl -H 'Authorization: Bearer sk-ant-abc123'"
//! ```
//!
//! # Thread Safety
//!
//! The `CredentialStore` is not thread-safe by itself. For concurrent access,
//! wrap it in a `Mutex` or `RwLock`.
//!
//! # Security Notes
//!
//! - Credentials are stored in memory only (not persisted to disk)
//! - Each session has its own credential store
//! - References are opaque and don't reveal credential content
//! - Store is cleared when dropped

use rand::RngExt;
use regex::{Captures, Regex};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fmt::Write;
use std::sync::{Arc, LazyLock, Mutex};
use zeroize::{Zeroize, Zeroizing};

/// Credential types that can be stored
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialType {
    /// API key (OpenAI, Anthropic, etc.)
    ApiKey,
    /// Bearer token
    Token,
    /// Password
    Password,
    /// Generic secret
    Secret,
    /// Private key (RSA, SSH, etc.)
    PrivateKey,
    /// Connection string (database, etc.)
    ConnectionString,
    /// Unknown credential type
    Unknown,
}

impl CredentialType {
    /// Convert to string representation for reference format
    fn as_str(&self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Token => "token",
            Self::Password => "password",
            Self::Secret => "secret",
            Self::PrivateKey => "private_key",
            Self::ConnectionString => "connection_string",
            Self::Unknown => "unknown",
        }
    }

    /// Parse from string representation
    #[allow(dead_code)]
    fn from_str(s: &str) -> Self {
        match s {
            "api_key" => Self::ApiKey,
            "token" => Self::Token,
            "password" => Self::Password,
            "secret" => Self::Secret,
            "private_key" => Self::PrivateKey,
            "connection_string" => Self::ConnectionString,
            _ => Self::Unknown,
        }
    }
}

/// Stored credential metadata
#[derive(Clone)]
struct StoredCredential {
    /// The actual credential value
    value: Zeroizing<String>,
    /// Type of credential
    cred_type: CredentialType,
    /// How many times it's been resolved
    resolve_count: u32,
}

impl fmt::Debug for StoredCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredCredential")
            .field("cred_type", &self.cred_type)
            .field("resolve_count", &self.resolve_count)
            .finish_non_exhaustive()
    }
}

/// Reference pattern for matching credential references in strings
/// Matches: {{CRED:type:id}}
static REFERENCE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\{\{CRED:(api_key|token|password|secret|private_key|connection_string|unknown):([a-f0-9]{12})\}\}",
    )
    .expect("Invalid regex pattern")
});

const REFERENCE_PREFIX: &str = "{{CRED:";
const REFERENCE_SUFFIX: &str = "}}";

fn credential_reference_like_ranges(input: &str) -> Vec<(usize, usize)> {
    credential_reference_like_ranges_with_scan_count(input).0
}

fn credential_reference_like_ranges_with_scan_count(input: &str) -> (Vec<(usize, usize)>, usize) {
    let mut ranges = Vec::new();
    let mut search_from = 0;
    let mut scanned_bytes = 0;

    while let Some(relative_start) = input[search_from..].find(REFERENCE_PREFIX) {
        scanned_bytes += relative_start + REFERENCE_PREFIX.len();
        let start = search_from + relative_start;
        let payload_start = start + REFERENCE_PREFIX.len();
        let remainder = &input[payload_start..];
        let (boundary, boundary_scan_bytes) = next_reference_boundary(remainder);
        scanned_bytes += boundary_scan_bytes;

        let end = match boundary {
            Some(ReferenceBoundary::Closing(closing)) => {
                payload_start + closing + REFERENCE_SUFFIX.len()
            }
            Some(ReferenceBoundary::NestedTemplate(nested)) => {
                unclosed_reference_end(input, payload_start, payload_start + nested)
            }
            None => unclosed_reference_end(input, payload_start, input.len()),
        };

        ranges.push((start, end));
        search_from = end;
    }
    scanned_bytes += input.len().saturating_sub(search_from);

    (ranges, scanned_bytes)
}

enum ReferenceBoundary {
    Closing(usize),
    NestedTemplate(usize),
}

fn next_reference_boundary(input: &str) -> (Option<ReferenceBoundary>, usize) {
    let mut scanned_bytes = 0;
    let mut nested_template_depth = 0;
    let mut crossed_line_boundary = false;
    let mut multiline_nested_start = None;
    let mut awaiting_outer_close = false;
    let mut outer_close_line_breaks = 0;
    let mut skip_until = 0;
    for (offset, character) in input.char_indices() {
        scanned_bytes += character.len_utf8();
        if offset < skip_until {
            continue;
        }
        let remainder = &input[offset..];
        if character == '\n' || character == '\r' {
            let follows_carriage_return =
                character == '\n' && input.as_bytes().get(offset.saturating_sub(1)) == Some(&b'\r');
            if awaiting_outer_close && !follows_carriage_return {
                outer_close_line_breaks += 1;
                if outer_close_line_breaks > 1 {
                    let mut next_offset = offset + character.len_utf8();
                    if character == '\r' && input[next_offset..].starts_with('\n') {
                        next_offset += '\n'.len_utf8();
                    }
                    let closer_follows = input[next_offset..]
                        .trim_start_matches([' ', '\t'])
                        .starts_with(REFERENCE_SUFFIX);
                    if !closer_follows {
                        return (
                            Some(ReferenceBoundary::NestedTemplate(
                                multiline_nested_start.expect("awaiting nested template boundary"),
                            )),
                            scanned_bytes,
                        );
                    }
                }
            }
            crossed_line_boundary = true;
        }
        if character == '{' && remainder.starts_with("{{") {
            if remainder.starts_with(REFERENCE_PREFIX) {
                return (
                    Some(ReferenceBoundary::NestedTemplate(offset)),
                    scanned_bytes,
                );
            }
            if crossed_line_boundary && multiline_nested_start.is_none() {
                multiline_nested_start = Some(offset);
            }
            skip_until = offset + "{{".len();
            nested_template_depth += 1;
            awaiting_outer_close = false;
            outer_close_line_breaks = 0;
            continue;
        }
        if character == '}' && remainder.starts_with(REFERENCE_SUFFIX) {
            if nested_template_depth == 0 {
                return (Some(ReferenceBoundary::Closing(offset)), scanned_bytes);
            }
            nested_template_depth -= 1;
            skip_until = offset + REFERENCE_SUFFIX.len();
            awaiting_outer_close = nested_template_depth == 0 && multiline_nested_start.is_some();
            outer_close_line_breaks = 0;
        }
    }
    (
        multiline_nested_start.map(ReferenceBoundary::NestedTemplate),
        scanned_bytes,
    )
}

fn unclosed_reference_end(input: &str, payload_start: usize, limit: usize) -> usize {
    let candidate = &input[payload_start..limit];
    let relative_end = candidate
        .char_indices()
        .find_map(|(offset, character)| {
            (character.is_whitespace() || matches!(character, '\'' | '"' | ',' | ';' | '}' | ']'))
                .then_some(offset)
        })
        .unwrap_or(candidate.len());
    payload_start + relative_end
}

fn redact_credential_references(input: &str, preserve_references: bool) -> String {
    let ranges = credential_reference_like_ranges(input);
    if ranges.is_empty() {
        return input.to_string();
    }

    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    for (start, end) in ranges {
        output.push_str(&input[cursor..start]);
        let candidate = &input[start..end];
        let is_canonical = REFERENCE_PATTERN
            .find(candidate)
            .is_some_and(|matched| matched.start() == 0 && matched.end() == candidate.len());
        if preserve_references && is_canonical {
            output.push_str(candidate);
        } else {
            output.push_str("[REDACTED:credential_reference:portable-export]");
        }
        cursor = end;
    }
    output.push_str(&input[cursor..]);
    output
}

#[derive(Debug, Clone, Copy)]
enum ReplaceKind {
    Full,
    Bearer,
    Basic,
    Authorization,
    KeyValue,
    UriUserInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringRedactionContext {
    Shell,
    Opaque,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellQuote {
    Single,
    AnsiCSingle,
    Double,
    Backtick,
}

#[derive(Debug)]
struct CredentialPattern {
    regex: Regex,
    kind: CredentialType,
    replace: ReplaceKind,
}

static CREDENTIAL_PATTERNS: LazyLock<Vec<CredentialPattern>> = LazyLock::new(|| {
    let pem_begin = [
        "-----BEGIN ",
        "(?:RSA |EC |DSA |OPENSSH )?PRIVATE",
        " KEY-----",
    ]
    .concat();
    let pem_end = [
        "-----END ",
        "(?:RSA |EC |DSA |OPENSSH )?PRIVATE",
        " KEY-----",
    ]
    .concat();
    let pgp_block = ["PGP", " PRIVATE", " KEY", " BLOCK"].concat();

    vec![
        CredentialPattern {
            regex: Regex::new(
                r#"(?i)(password|passwd|pwd|credential|credentials)(['"]?\s*[:=]\s*['"]?)([^\s'",;}{}\[\]]+)"#,
            )
            .expect("Invalid regex pattern"),
            kind: CredentialType::Password,
            replace: ReplaceKind::KeyValue,
        },
        CredentialPattern {
            regex: Regex::new(r"(?i)([a-z][a-z0-9+.-]*://[^:/\s@]+:)([^@\s]+)(@)")
                .expect("Invalid regex pattern"),
            kind: CredentialType::Password,
            replace: ReplaceKind::UriUserInfo,
        },
        CredentialPattern {
            regex: Regex::new(
                r#"(?i)(api[_-]?key|apikey|api[_-]?token|token|secret)(['"\s:=]+)([A-Za-z0-9_-]{20,})"#,
            )
            .expect("Invalid regex pattern"),
            kind: CredentialType::ApiKey,
            replace: ReplaceKind::KeyValue,
        },
        CredentialPattern {
            regex: Regex::new(
                r#"(?i)(aws[_-]?secret[_-]?(?:access[_-]?)?key|secret[_-]?key)(['"\s:=]+)([A-Za-z0-9/+=]{40})"#,
            )
            .expect("Invalid regex pattern"),
            kind: CredentialType::Secret,
            replace: ReplaceKind::KeyValue,
        },
        CredentialPattern {
            regex: Regex::new(
                r#"(?i)(token|access[_-]?token|refresh[_-]?token|auth[_-]?token)(['"]?\s*[:=]\s*['"]?)([^\s'",;}{}\[\]]+)"#,
            )
            .expect("Invalid regex pattern"),
            kind: CredentialType::Token,
            replace: ReplaceKind::KeyValue,
        },
        CredentialPattern {
            regex: Regex::new(r"(?i)Bearer\s+([A-Za-z0-9._~+/\-]+=*)")
                .expect("Invalid regex pattern"),
            kind: CredentialType::Token,
            replace: ReplaceKind::Bearer,
        },
        CredentialPattern {
            regex: Regex::new(
                r"(?i)(\bAuthorization\s*:\s*Basic\s+)([A-Za-z0-9+/]+={0,2})",
            )
                .expect("Invalid regex pattern"),
            kind: CredentialType::Password,
            replace: ReplaceKind::Basic,
        },
        CredentialPattern {
            regex: Regex::new(
                r#"(?i)(\bAuthorization\s*:\s*[A-Za-z][A-Za-z0-9+.-]*\s+)((?:[A-Za-z0-9!#$%&'*+.^_`|~-]+\s*=\s*(?:"(?:\\.|[^"\\])*"|[^\s,'"]+))(?:\s*,\s*[A-Za-z0-9!#$%&'*+.^_`|~-]+\s*=\s*(?:"(?:\\.|[^"\\])*"|[^\s,'"]+))*|[A-Za-z0-9._~+/\-]+=*)"#,
            )
                .expect("Invalid regex pattern"),
            kind: CredentialType::Token,
            replace: ReplaceKind::Authorization,
        },
        CredentialPattern {
            regex: Regex::new(r"(?:sk|pk)_(?:live|test)_[A-Za-z0-9]{20,}")
                .expect("Invalid regex pattern"),
            kind: CredentialType::ApiKey,
            replace: ReplaceKind::Full,
        },
        CredentialPattern {
            regex: Regex::new(r"sk-ant-[A-Za-z0-9_-]{20,}").expect("Invalid regex pattern"),
            kind: CredentialType::ApiKey,
            replace: ReplaceKind::Full,
        },
        CredentialPattern {
            regex: Regex::new(r"sk-[A-Za-z0-9]{20,}").expect("Invalid regex pattern"),
            kind: CredentialType::ApiKey,
            replace: ReplaceKind::Full,
        },
        CredentialPattern {
            regex: Regex::new(r"gh[pousr]_[A-Za-z0-9]{36,}").expect("Invalid regex pattern"),
            kind: CredentialType::ApiKey,
            replace: ReplaceKind::Full,
        },
        CredentialPattern {
            regex: Regex::new(r"xox[baprs]-[A-Za-z0-9-]{10,}")
                .expect("Invalid regex pattern"),
            kind: CredentialType::ApiKey,
            replace: ReplaceKind::Full,
        },
        CredentialPattern {
            regex: Regex::new(r"AKIA[A-Z0-9]{16}").expect("Invalid regex pattern"),
            kind: CredentialType::ApiKey,
            replace: ReplaceKind::Full,
        },
        CredentialPattern {
            regex: Regex::new(r"AIza[0-9A-Za-z_-]{35}").expect("Invalid regex pattern"),
            kind: CredentialType::ApiKey,
            replace: ReplaceKind::Full,
        },
        CredentialPattern {
            regex: Regex::new(r"ya29\.[A-Za-z0-9_-]{20,}").expect("Invalid regex pattern"),
            kind: CredentialType::Token,
            replace: ReplaceKind::Full,
        },
        CredentialPattern {
            regex: Regex::new(r"eyJ[a-zA-Z0-9_-]*\.[a-zA-Z0-9_-]*\.[a-zA-Z0-9_-]*")
                .expect("Invalid regex pattern"),
            kind: CredentialType::Token,
            replace: ReplaceKind::Full,
        },
        CredentialPattern {
            regex: Regex::new(
                &format!(r"(?s){pem_begin}.*?{pem_end}"),
            )
            .expect("Invalid regex pattern"),
            kind: CredentialType::PrivateKey,
            replace: ReplaceKind::Full,
        },
        CredentialPattern {
            regex: Regex::new(
                &format!(r"(?s)-----BEGIN {pgp_block}-----.*?-----END {pgp_block}-----"),
            )
            .expect("Invalid regex pattern"),
            kind: CredentialType::PrivateKey,
            replace: ReplaceKind::Full,
        },
    ]
});

/// Detect likely credentials in source being published, without returning a
/// value or retaining it in the vault. Source code needs narrower assignment
/// matching than transcript redaction to avoid treating variable names as keys.
#[must_use]
pub fn publication_secret_kind(line: &str) -> Option<CredentialType> {
    static PRIVATE_HEADER: LazyLock<Regex> = LazyLock::new(|| {
        let header = [
            "-----BEGIN ",
            "(?:(?:RSA |EC |DSA |OPENSSH )?PRIVATE",
            " KEY|PGP PRIVATE",
            " KEY BLOCK)-----",
        ]
        .concat();
        Regex::new(&header).expect("valid private key header")
    });
    if PRIVATE_HEADER.is_match(line) {
        return Some(CredentialType::PrivateKey);
    }
    static EXTRA_PREFIXES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:github_pat_[A-Za-z0-9_]{40,}|npm_[A-Za-z0-9]{36,}|glpat-[A-Za-z0-9_-]{20,}|sk-proj-[A-Za-z0-9_-]{40,}|AGE-SECRET-KEY-1[A-Z0-9]{40,})")
            .expect("valid publication prefixes")
    });
    if EXTRA_PREFIXES.is_match(line) {
        return Some(CredentialType::Secret);
    }
    // Keep transcript streaming patterns unchanged. Publication also checks
    // short hexadecimal API-key assignments, which need no decimal digit.
    static HEX_KEY_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)(api[_-]?key|apikey|secret)(['"\s:=]+)([A-Za-z0-9_-]+)"#)
            .expect("valid hexadecimal key assignment")
    });
    if HEX_KEY_ASSIGNMENT.captures_iter(line).any(|capture| {
        capture.get(3).is_some_and(|value| {
            value.as_str().len() >= 16
                && value.as_str().bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    }) {
        return Some(CredentialType::ApiKey);
    }
    for pattern in CREDENTIAL_PATTERNS.iter() {
        for captures in pattern.regex.captures_iter(line) {
            let index = match pattern.replace {
                ReplaceKind::Full => return Some(pattern.kind),
                ReplaceKind::KeyValue => 3,
                ReplaceKind::Bearer => 1,
                ReplaceKind::Basic | ReplaceKind::Authorization | ReplaceKind::UriUserInfo => 2,
            };
            let Some(value) = captures.get(index).map(|value| value.as_str()) else {
                continue;
            };
            if value.starts_with('$') || value.starts_with("op://") || value.starts_with("{{") {
                continue;
            }
            // Credential assignments can contain hex keys made only of
            // letters; they need neither decimal digits nor base64 entropy.
            if value.len() >= 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Some(pattern.kind);
            }
            // Known provider formats above do not need this heuristic. For
            // generic assignments require a substantial, mixed candidate.
            if value.len() < 16 || !value.bytes().any(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let mut counts = [0usize; 256];
            for byte in value.bytes() {
                counts[usize::from(byte)] += 1;
            }
            let entropy = counts
                .iter()
                .filter(|count| **count > 0)
                .map(|count| {
                    let probability = *count as f64 / value.len() as f64;
                    -probability * probability.log2()
                })
                .sum::<f64>();
            if entropy >= 3.5 {
                return Some(pattern.kind);
            }
        }
    }
    None
}

/// Generate a short unique ID for credential references
fn generate_id() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 6] = rng.random();
    let mut hex_string = String::with_capacity(12);
    for byte in bytes {
        write!(hex_string, "{:02x}", byte).expect("Writing to string should never fail");
    }
    hex_string
}

fn random_fingerprint_key() -> [u8; 32] {
    rand::rng().random()
}

/// Credential Store - manages secure credential storage
pub struct CredentialStore {
    /// Credentials indexed by ID
    credentials: HashMap<String, StoredCredential>,
    /// Reverse lookup: keyed digest -> reference. Raw credential values are never duplicated.
    value_to_ref: HashMap<[u8; 32], String>,
    /// Per-store key for reverse lookup fingerprints.
    fingerprint_key: Zeroizing<[u8; 32]>,
}

/// Shared, session-scoped credential vault.
///
/// Clones share the same in-memory credentials while independent vaults remain
/// isolated. Inject this into every component that needs to vault or resolve
/// credentials for a session.
#[derive(Clone)]
pub struct CredentialVault(Arc<Mutex<CredentialVaultState>>);

struct CredentialVaultState {
    store: CredentialStore,
    generation: u64,
    initial_references: HashSet<String>,
}

impl fmt::Debug for CredentialVault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .store
            .fmt(formatter)
    }
}

impl Default for CredentialVault {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialVault {
    /// Create a new empty, independently scoped credential vault.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(CredentialVaultState {
            store: CredentialStore::new(),
            generation: 0,
            initial_references: HashSet::new(),
        })))
    }

    /// Fork this vault into an independently owned child scope.
    ///
    /// The child keeps the current opaque references and their values, but a
    /// later clear or store operation in either vault cannot affect the other.
    pub fn fork(&self) -> Self {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self(Arc::new(Mutex::new(CredentialVaultState {
            initial_references: state.store.references(),
            store: state.store.fork(),
            generation: state.generation,
        })))
    }

    /// Import child-created credentials and return a mapping from child
    /// references to references usable by this scope. Credentials that were
    /// present when the child was forked are deliberately excluded so a
    /// parent clear cannot be undone by a child finishing later. Import only
    /// occurs while the parent execution generation is still current. The
    /// generation check is performed while holding the parent lock so a
    /// concurrent clear cannot be followed by a stale child repopulating the
    /// newly reset vault.
    pub fn absorb_child_credentials_at_generation(
        &self,
        child: &Self,
        expected_generation: u64,
    ) -> HashMap<String, String> {
        if Arc::ptr_eq(&self.0, &child.0) {
            return HashMap::new();
        }

        let child_credentials = {
            let child_state = child
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            child_state
                .store
                .credentials
                .iter()
                .filter_map(|(id, credential)| {
                    let child_reference =
                        format!("{{{{CRED:{}:{id}}}}}", credential.cred_type.as_str());
                    (!child_state.initial_references.contains(&child_reference)).then(|| {
                        (
                            child_reference,
                            credential.value.to_string(),
                            credential.cred_type,
                        )
                    })
                })
                .collect::<Vec<_>>()
        };

        let mut parent_state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if parent_state.generation != expected_generation {
            return HashMap::new();
        }
        child_credentials
            .into_iter()
            .map(|(child_reference, value, cred_type)| {
                let parent_reference = parent_state.store.store(&value, cred_type);
                (child_reference, parent_reference)
            })
            .collect()
    }

    /// Rewrite canonical credential references using a child-to-parent map.
    pub fn translate_references(input: &str, mappings: &HashMap<String, String>) -> String {
        if mappings.is_empty() {
            return input.to_string();
        }
        let replacements = REFERENCE_PATTERN
            .captures_iter(input)
            .filter_map(|captures| {
                let full_match = captures.get(0)?;
                mappings
                    .get(full_match.as_str())
                    .map(|replacement| (full_match.start(), full_match.end(), replacement.clone()))
            })
            .collect::<Vec<_>>();
        let mut translated = input.to_string();
        for (start, end, replacement) in replacements.into_iter().rev() {
            translated.replace_range(start..end, &replacement);
        }
        translated
    }

    /// Re-key every credential reference in `input` for another vault.
    ///
    /// This preserves the reference boundary when a parent sends a later
    /// control message to a child whose vault was forked before the referenced
    /// credential existed.
    pub fn rekey_references_to(&self, target: &Self, input: &str) -> Result<String, String> {
        if Arc::ptr_eq(&self.0, &target.0) {
            return Ok(input.to_string());
        }
        let replacements = {
            let mut source = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut replacements = Vec::new();
            for captures in REFERENCE_PATTERN.captures_iter(input) {
                let whole = captures
                    .get(0)
                    .expect("credential reference match must include the whole token");
                let kind = captures
                    .get(1)
                    .map(|value| CredentialType::from_str(value.as_str()))
                    .unwrap_or(CredentialType::Unknown);
                let id = captures
                    .get(2)
                    .expect("credential reference match must include an id")
                    .as_str();
                let credential = source.store.credentials.get_mut(id).ok_or_else(|| {
                    "credential reference is unavailable in the current parent session".to_string()
                })?;
                credential.resolve_count = credential.resolve_count.saturating_add(1);
                replacements.push((
                    whole.start(),
                    whole.end(),
                    credential.value.to_string(),
                    kind,
                ));
            }
            replacements
        };
        let mut translated = input.to_string();
        for (start, end, value, kind) in replacements.into_iter().rev() {
            translated.replace_range(start..end, &target.store(&value, kind));
        }
        Ok(translated)
    }

    /// Store a credential and return its opaque reference.
    pub fn store(&self, value: &str, cred_type: CredentialType) -> String {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .store
            .store(value, cred_type)
    }

    /// Capture the current session generation for an execution.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }

    /// Resolve all credential references in a string.
    pub fn resolve_all(&self, input: &str) -> String {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .store
            .resolve_all(input)
    }

    /// Resolve all credential references in a JSON value.
    pub fn resolve_in_json(&self, value: &serde_json::Value) -> serde_json::Value {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .store
            .resolve_in_json(value)
    }

    /// Check whether a value contains a canonical credential reference.
    #[must_use]
    pub fn has_references(input: &str) -> bool {
        CredentialStore::has_references(input)
    }

    /// Vault credentials in a plain text value.
    pub fn vault_in_text(&self, value: &str) -> String {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vault_credentials_in_string(&mut state.store, value)
    }

    /// Vault credentials only when the execution still belongs to this session.
    ///
    /// Stale output is redacted rather than stored so a cleared vault cannot be
    /// repopulated by an execution that began under an earlier generation.
    pub fn vault_in_text_at_generation(&self, generation: u64, value: &str) -> String {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.generation == generation {
            vault_credentials_in_string(&mut state.store, value)
        } else {
            redact_credentials_in_string(value)
        }
    }

    /// Vault credentials in a JSON value.
    pub fn vault_in_json(&self, value: &serde_json::Value) -> serde_json::Value {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vault_credentials_in_json(&mut state.store, value)
    }

    /// Vault JSON credentials only if the execution generation remains active.
    pub fn vault_in_json_at_generation(
        &self,
        generation: u64,
        value: &serde_json::Value,
    ) -> serde_json::Value {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.generation == generation {
            vault_credentials_in_json(&mut state.store, value)
        } else {
            redact_credentials_in_json(value)
        }
    }

    /// Clear and zeroize all credentials in this vault.
    pub fn clear(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.store.clear();
        state.generation = state.generation.wrapping_add(1);
        state.initial_references.clear();
    }

    /// Get credential statistics for this vault.
    #[must_use]
    pub fn stats(&self) -> CredentialStats {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .store
            .stats()
    }
}

impl fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialStore")
            .field("credential_count", &self.credentials.len())
            .finish()
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore {
    /// Create a new empty credential store
    #[must_use]
    pub fn new() -> Self {
        Self {
            credentials: HashMap::new(),
            value_to_ref: HashMap::new(),
            fingerprint_key: Zeroizing::new(random_fingerprint_key()),
        }
    }

    fn fork(&self) -> Self {
        Self {
            credentials: self.credentials.clone(),
            value_to_ref: self.value_to_ref.clone(),
            fingerprint_key: self.fingerprint_key.clone(),
        }
    }

    fn references(&self) -> HashSet<String> {
        self.credentials
            .iter()
            .map(|(id, credential)| format!("{{{{CRED:{}:{id}}}}}", credential.cred_type.as_str()))
            .collect()
    }

    fn fingerprint(&self, value: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(*self.fingerprint_key);
        hasher.update(value.as_bytes());
        hasher.finalize().into()
    }

    /// Store a credential and return a reference token
    ///
    /// If the same credential value is stored multiple times, the same
    /// reference is returned (deduplication).
    ///
    /// # Arguments
    ///
    /// * `value` - The raw credential value
    /// * `cred_type` - The type of credential
    ///
    /// # Returns
    ///
    /// A reference token like `{{CRED:api_key:a1b2c3d4e5f6}}`
    pub fn store(&mut self, value: &str, cred_type: CredentialType) -> String {
        // Check if we already have this value stored
        let fingerprint = self.fingerprint(value);
        if let Some(existing_ref) = self.value_to_ref.get(&fingerprint) {
            return existing_ref.clone();
        }

        // Generate a new reference
        let id = generate_id();
        let reference = format!("{{{{CRED:{}:{}}}}}", cred_type.as_str(), id);

        // Store the credential
        self.credentials.insert(
            id,
            StoredCredential {
                value: Zeroizing::new(value.to_string()),
                cred_type,
                resolve_count: 0,
            },
        );
        self.value_to_ref.insert(fingerprint, reference.clone());

        reference
    }

    /// Resolve a single reference token to its original value
    ///
    /// # Arguments
    ///
    /// * `reference` - A reference token like `{{CRED:api_key:a1b2c3d4e5f6}}`
    ///
    /// # Returns
    ///
    /// The original credential value, or None if not found
    pub fn resolve(&mut self, reference: &str) -> Option<String> {
        let caps = REFERENCE_PATTERN.captures(reference)?;
        let id = caps.get(2)?.as_str();

        let credential = self.credentials.get_mut(id)?;
        credential.resolve_count += 1;
        Some(credential.value.to_string())
    }

    /// Resolve all credential references in a string
    ///
    /// # Arguments
    ///
    /// * `input` - String potentially containing credential references
    ///
    /// # Returns
    ///
    /// String with all references replaced with actual values
    pub fn resolve_all(&mut self, input: &str) -> String {
        let mut result = input.to_string();

        // Find all matches and resolve them
        // We need to collect matches first to avoid borrow issues
        let matches: Vec<_> = REFERENCE_PATTERN
            .captures_iter(input)
            .filter_map(|caps| {
                let full_match = caps.get(0)?.as_str().to_string();
                let id = caps.get(2)?.as_str().to_string();
                Some((full_match, id))
            })
            .collect();

        for (full_match, id) in matches {
            if let Some(credential) = self.credentials.get_mut(&id) {
                credential.resolve_count += 1;
                result = result.replace(&full_match, &credential.value);
            }
        }

        result
    }

    /// Recursively resolve all credential references in a JSON value
    ///
    /// # Arguments
    ///
    /// * `value` - JSON value potentially containing credential references
    ///
    /// # Returns
    ///
    /// New JSON value with all references resolved
    pub fn resolve_in_json(&mut self, value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => serde_json::Value::String(self.resolve_all(s)),
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(|v| self.resolve_in_json(v)).collect())
            }
            serde_json::Value::Object(map) => {
                let new_map: serde_json::Map<String, serde_json::Value> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), self.resolve_in_json(v)))
                    .collect();
                serde_json::Value::Object(new_map)
            }
            // Other types pass through unchanged
            _ => value.clone(),
        }
    }

    /// Check if a string contains any credential references
    #[must_use]
    pub fn has_references(input: &str) -> bool {
        REFERENCE_PATTERN.is_match(input)
    }

    /// Get the number of stored credentials
    #[must_use]
    pub fn len(&self) -> usize {
        self.credentials.len()
    }

    /// Check if the store is empty
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.credentials.is_empty()
    }

    fn vault_known_values(&self, input: &str, protected_ranges: &[(usize, usize)]) -> String {
        let mut replacements = self
            .credentials
            .iter()
            .filter_map(|(id, credential)| {
                let value = credential.value.to_string();
                (!value.is_empty()).then(|| {
                    let reference = format!("{{{{CRED:{}:{id}}}}}", credential.cred_type.as_str());
                    (value, reference)
                })
            })
            .collect::<Vec<_>>();
        replacements.sort_by(
            |(left_value, left_reference), (right_value, right_reference)| {
                right_value
                    .len()
                    .cmp(&left_value.len())
                    .then_with(|| left_value.cmp(right_value))
                    .then_with(|| left_reference.cmp(right_reference))
            },
        );

        let replace_segment = |segment: &str| {
            let mut output = String::with_capacity(segment.len());
            let mut cursor = 0;
            while cursor < segment.len() {
                if let Some((value, reference)) = replacements
                    .iter()
                    .find(|(value, _)| segment[cursor..].starts_with(value))
                {
                    output.push_str(reference);
                    cursor += value.len();
                } else {
                    let character = segment[cursor..]
                        .chars()
                        .next()
                        .expect("cursor remains on a UTF-8 character boundary");
                    output.push(character);
                    cursor += character.len_utf8();
                }
            }
            output
        };
        let mut ranges = REFERENCE_PATTERN
            .find_iter(input)
            .map(|reference| (reference.start(), reference.end()))
            .collect::<Vec<_>>();
        ranges.extend_from_slice(protected_ranges);
        ranges.sort_unstable_by_key(|(start, end)| (*start, *end));
        let mut merged_ranges = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            if start >= end {
                continue;
            }
            if let Some((_, previous_end)) = merged_ranges.last_mut() {
                if start <= *previous_end {
                    *previous_end = (*previous_end).max(end);
                    continue;
                }
            }
            merged_ranges.push((start, end));
        }
        let ranges = merged_ranges;
        if ranges.is_empty() {
            return replace_segment(input);
        }

        let mut output = String::with_capacity(input.len());
        let mut cursor = 0;
        for (start, end) in ranges {
            output.push_str(&replace_segment(&input[cursor..start]));
            output.push_str(&input[start..end]);
            cursor = end;
        }
        output.push_str(&replace_segment(&input[cursor..]));
        output
    }

    /// Clear all stored credentials
    pub fn clear(&mut self) {
        for credential in self.credentials.values_mut() {
            credential.value.zeroize();
        }
        self.credentials.clear();
        self.value_to_ref.clear();
        self.fingerprint_key.zeroize();
        self.fingerprint_key = Zeroizing::new(random_fingerprint_key());
    }

    /// Get statistics about stored credentials
    #[must_use]
    pub fn stats(&self) -> CredentialStats {
        let mut types: HashMap<CredentialType, usize> = HashMap::new();
        let mut total_resolves: u32 = 0;

        for cred in self.credentials.values() {
            *types.entry(cred.cred_type).or_insert(0) += 1;
            total_resolves += cred.resolve_count;
        }

        CredentialStats {
            count: self.credentials.len(),
            types,
            total_resolves,
        }
    }
}

fn vault_credentials_in_string(store: &mut CredentialStore, input: &str) -> String {
    // Protect full pattern matches while replacing already-known values. This
    // keeps a known value such as `word` from corrupting the `password` label
    // before the original segment is scanned for a newly discovered secret.
    let protected_ranges = CREDENTIAL_PATTERNS
        .iter()
        .flat_map(|pattern| {
            pattern
                .regex
                .find_iter(input)
                .map(|matched| (matched.start(), matched.end()))
        })
        .collect::<Vec<_>>();
    let mut output = store.vault_known_values(input, &protected_ranges);

    let ordered_patterns = CREDENTIAL_PATTERNS
        .iter()
        .filter(|pattern| {
            matches!(
                pattern.replace,
                ReplaceKind::Basic | ReplaceKind::Authorization
            )
        })
        .chain(CREDENTIAL_PATTERNS.iter().filter(|pattern| {
            !matches!(
                pattern.replace,
                ReplaceKind::Basic | ReplaceKind::Authorization
            )
        }));
    for pattern in ordered_patterns {
        output = vault_pattern_preserving_references(store, &output, pattern);
    }

    output
}

fn vault_pattern_preserving_references(
    store: &mut CredentialStore,
    input: &str,
    pattern: &CredentialPattern,
) -> String {
    let mut shadow = input.as_bytes().to_vec();
    let reference_ranges: Vec<_> = REFERENCE_PATTERN
        .find_iter(input)
        .map(|reference| (reference.start(), reference.end()))
        .collect();
    for &(start, end) in &reference_ranges {
        shadow[start..end].fill(b'{');
    }
    let shadow = std::str::from_utf8(&shadow).expect("credential references are ASCII");

    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut reference_cursor = 0;
    for captures in pattern.regex.captures_iter(shadow) {
        let Some(full_match) = captures.get(0) else {
            continue;
        };
        while reference_cursor < reference_ranges.len()
            && reference_ranges[reference_cursor].1 <= full_match.start()
        {
            reference_cursor += 1;
        }
        let secret_capture = match pattern.replace {
            ReplaceKind::Full => 0,
            ReplaceKind::Bearer => 1,
            ReplaceKind::Basic | ReplaceKind::Authorization => 2,
            ReplaceKind::KeyValue => 3,
            ReplaceKind::UriUserInfo => 2,
        };
        let Some(secret) = captures.get(secret_capture) else {
            continue;
        };
        let secret_overlaps_reference = reference_cursor < reference_ranges.len()
            && reference_ranges[reference_cursor].0 < secret.end()
            && reference_ranges[reference_cursor].1 > secret.start();
        if secret_overlaps_reference {
            output.push_str(&input[cursor..secret.start()]);
            let mut segment_cursor = secret.start();
            let mut overlap_cursor = reference_cursor;
            while overlap_cursor < reference_ranges.len()
                && reference_ranges[overlap_cursor].0 < secret.end()
            {
                let (start, end) = reference_ranges[overlap_cursor];
                if end > secret.start() {
                    if segment_cursor < start {
                        output.push_str(&store.store(&input[segment_cursor..start], pattern.kind));
                    }
                    output.push_str(&input[start..end]);
                    segment_cursor = end;
                }
                overlap_cursor += 1;
            }
            if segment_cursor < secret.end() {
                output.push_str(&store.store(&input[segment_cursor..secret.end()], pattern.kind));
            }
            output.push_str(&input[secret.end()..full_match.end()]);
            cursor = full_match.end();
            continue;
        }
        output.push_str(&input[cursor..full_match.start()]);
        match pattern.replace {
            ReplaceKind::Full => {
                output
                    .push_str(&store.store(capture_from_input(input, &captures, 0), pattern.kind));
            }
            ReplaceKind::Bearer => {
                let reference = store.store(capture_from_input(input, &captures, 1), pattern.kind);
                output.push_str("Bearer ");
                output.push_str(&reference);
            }
            ReplaceKind::Basic | ReplaceKind::Authorization => {
                let reference = store.store(capture_from_input(input, &captures, 2), pattern.kind);
                output.push_str(capture_from_input(input, &captures, 1));
                output.push_str(&reference);
            }
            ReplaceKind::KeyValue => {
                let reference = store.store(capture_from_input(input, &captures, 3), pattern.kind);
                output.push_str(capture_from_input(input, &captures, 1));
                output.push_str(capture_from_input(input, &captures, 2));
                output.push_str(&reference);
            }
            ReplaceKind::UriUserInfo => {
                let reference = store.store(capture_from_input(input, &captures, 2), pattern.kind);
                output.push_str(capture_from_input(input, &captures, 1));
                output.push_str(&reference);
                output.push_str(capture_from_input(input, &captures, 3));
            }
        }
        cursor = full_match.end();
    }
    output.push_str(&input[cursor..]);
    output
}

fn capture_from_input<'a>(input: &'a str, captures: &Captures<'_>, index: usize) -> &'a str {
    captures
        .get(index)
        .map_or("", |capture| &input[capture.start()..capture.end()])
}

fn vault_credentials_in_json(
    store: &mut CredentialStore,
    value: &serde_json::Value,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(value) => {
            serde_json::Value::String(vault_credentials_in_string(store, value))
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| vault_credentials_in_json(store, value))
                .collect(),
        ),
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), vault_credentials_in_json(store, value)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// Replace credentials in an arbitrary JSON value with stable, non-reversible masks.
///
/// Portable session exports use this instead of the in-memory credential vault because
/// exported files must never contain references that require process-local state.
#[must_use]
pub fn redact_credentials_in_json(value: &serde_json::Value) -> serde_json::Value {
    redact_credentials_in_json_with_mode(value, false, true, StringRedactionContext::Opaque)
}

/// Replace raw credentials while retaining valid references to the active in-memory vault.
///
/// Hosted transcripts remain attached to their live session vault, unlike portable exports.
#[must_use]
pub fn redact_credentials_in_json_preserving_references(
    value: &serde_json::Value,
) -> serde_json::Value {
    redact_credentials_in_json_with_mode(value, true, false, StringRedactionContext::Opaque)
}

/// Redact tool arguments while preserving shell syntax only where the tool contract proves it.
///
/// An arbitrary tool may call an opaque payload `command`, so the field name alone is not
/// sufficient evidence that shell tokenization applies.
#[must_use]
pub fn redact_tool_arguments_preserving_references(
    tool: &str,
    value: &serde_json::Value,
) -> serde_json::Value {
    let serde_json::Value::Object(values) = value else {
        return redact_credentials_in_json_preserving_references(value);
    };
    let is_bash = tool.eq_ignore_ascii_case("bash");
    serde_json::Value::Object(
        values
            .iter()
            .map(|(key, value)| {
                let context = if is_bash && key == "command" && value.is_string() {
                    StringRedactionContext::Shell
                } else {
                    StringRedactionContext::Opaque
                };
                (
                    key.clone(),
                    redact_credentials_in_json_with_mode(value, true, false, context),
                )
            })
            .collect(),
    )
}

fn redact_credentials_in_json_with_mode(
    value: &serde_json::Value,
    preserve_references: bool,
    redact_bare_bearer: bool,
    context: StringRedactionContext,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(value) => {
            serde_json::Value::String(redact_credentials_in_string_with_mode(
                value,
                preserve_references,
                redact_bare_bearer,
                context,
            ))
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| {
                    redact_credentials_in_json_with_mode(
                        value,
                        preserve_references,
                        redact_bare_bearer,
                        StringRedactionContext::Opaque,
                    )
                })
                .collect(),
        ),
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        redact_credentials_in_json_with_mode(
                            value,
                            preserve_references,
                            redact_bare_bearer,
                            StringRedactionContext::Opaque,
                        ),
                    )
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn redact_credentials_in_string(input: &str) -> String {
    redact_credentials_in_string_with_mode(input, false, true, StringRedactionContext::Opaque)
}

fn redact_credentials_in_string_with_mode(
    input: &str,
    preserve_references: bool,
    redact_bare_bearer: bool,
    context: StringRedactionContext,
) -> String {
    let mut output = input.to_string();
    for pattern in CREDENTIAL_PATTERNS.iter() {
        if matches!(pattern.replace, ReplaceKind::Bearer) && !redact_bare_bearer {
            continue;
        }
        output = redact_pattern(&output, pattern, preserve_references, context);
    }
    redact_credential_references(&output, preserve_references)
}

fn redact_pattern(
    input: &str,
    pattern: &CredentialPattern,
    preserve_references: bool,
    context: StringRedactionContext,
) -> String {
    redact_pattern_with_reference_checks(input, pattern, preserve_references, context).0
}

fn redact_pattern_with_reference_checks(
    input: &str,
    pattern: &CredentialPattern,
    preserve_references: bool,
    context: StringRedactionContext,
) -> (String, usize, usize, usize) {
    let mut shadow = input.as_bytes().to_vec();
    let reference_ranges = credential_reference_like_ranges(input);
    for &(start, end) in &reference_ranges {
        shadow[start..end].fill(b'A');
    }
    let shadow = std::str::from_utf8(&shadow).expect("credential references are ASCII");
    let mask = format!("[REDACTED:{}:portable-export]", pattern.kind.as_str());
    let secret_capture = match pattern.replace {
        ReplaceKind::Full => 0,
        ReplaceKind::Bearer => 1,
        ReplaceKind::Basic | ReplaceKind::Authorization => 2,
        ReplaceKind::KeyValue => 3,
        ReplaceKind::UriUserInfo => 2,
    };

    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut reference_cursor = 0;
    let mut reference_checks = 0;
    let mut quote_scan_cursor = 0;
    let mut quote_scan_bytes = 0;
    let mut credential_scan_bytes = 0;
    let mut quote = None;
    let mut preceding_backslashes = 0;
    let mut previous_unescaped_dollar = false;
    for captures in pattern.regex.captures_iter(shadow) {
        let Some(secret) = captures.get(secret_capture) else {
            continue;
        };
        if secret.start() < cursor {
            continue;
        }
        let initial_quote = if matches!(context, StringRedactionContext::Shell) {
            quote_scan_bytes += secret.start().saturating_sub(quote_scan_cursor);
            advance_shell_quote_state(
                input,
                &mut quote_scan_cursor,
                secret.start(),
                &mut quote,
                &mut preceding_backslashes,
                &mut previous_unescaped_dollar,
            );
            quote
        } else {
            None
        };
        let candidate_end = match (pattern.replace, context) {
            (ReplaceKind::KeyValue, StringRedactionContext::Shell) => {
                shell_credential_end(input, secret.start(), initial_quote)
            }
            (ReplaceKind::KeyValue, StringRedactionContext::Opaque) => input.len(),
            _ => secret.end(),
        };
        if matches!(
            (pattern.replace, context),
            (ReplaceKind::KeyValue, StringRedactionContext::Shell)
        ) {
            credential_scan_bytes += candidate_end.saturating_sub(secret.start());
        }
        let (secret_start, secret_end, checks) = expand_reference_overlapping_secret(
            input,
            &reference_ranges,
            &mut reference_cursor,
            secret.start(),
            candidate_end,
            pattern.replace,
        );
        reference_checks += checks;
        if secret_end <= cursor {
            continue;
        }
        let secret_start = secret_start.max(cursor);
        output.push_str(&input[cursor..secret_start]);
        if matches!(
            (pattern.replace, context),
            (ReplaceKind::KeyValue, StringRedactionContext::Shell)
        ) {
            output.push_str(&redact_shell_credential(
                &input[secret_start..secret_end],
                initial_quote,
                &mask,
                preserve_references,
            ));
        } else {
            output.push_str(&redact_secret(
                &input[secret_start..secret_end],
                &mask,
                preserve_references,
            ));
        }
        cursor = secret_end;
    }
    output.push_str(&input[cursor..]);
    (
        output,
        reference_checks,
        quote_scan_bytes,
        credential_scan_bytes,
    )
}

fn advance_shell_quote_state(
    input: &str,
    cursor: &mut usize,
    end: usize,
    quote: &mut Option<ShellQuote>,
    preceding_backslashes: &mut usize,
    previous_unescaped_dollar: &mut bool,
) {
    for character in input[*cursor..end].chars() {
        let escaped = *preceding_backslashes % 2 == 1;
        if is_shell_syntax_quote(*quote, character, escaped) {
            *quote = if quote.is_some() {
                None
            } else {
                Some(opened_shell_quote(character, *previous_unescaped_dollar))
            };
        }
        // Bash consumes adjacent unescaped dollars as `$$` expansions. Only an
        // unmatched final dollar can introduce an ANSI-C `$'...'` quote.
        *previous_unescaped_dollar =
            character == '$' && !escaped && quote.is_none() && !*previous_unescaped_dollar;
        if character == '\\' {
            *preceding_backslashes += 1;
        } else {
            *preceding_backslashes = 0;
        }
    }
    *cursor = end;
}

fn opened_shell_quote(character: char, previous_unescaped_dollar: bool) -> ShellQuote {
    match character {
        '\'' if previous_unescaped_dollar => ShellQuote::AnsiCSingle,
        '\'' => ShellQuote::Single,
        '"' => ShellQuote::Double,
        '`' => ShellQuote::Backtick,
        _ => unreachable!("only shell quote characters can open quote state"),
    }
}

fn is_shell_syntax_quote(quote: Option<ShellQuote>, character: char, escaped: bool) -> bool {
    match quote {
        Some(ShellQuote::Single) => character == '\'',
        Some(ShellQuote::AnsiCSingle) => character == '\'' && !escaped,
        Some(ShellQuote::Double) => character == '"' && !escaped,
        Some(ShellQuote::Backtick) => character == '`' && !escaped,
        None => matches!(character, '\'' | '"' | '`') && !escaped,
    }
}

fn shell_credential_end(input: &str, start: usize, initial_quote: Option<ShellQuote>) -> usize {
    let mut quote = initial_quote;
    let mut preceding_backslashes = 0;
    let mut parenthesis_depth = 0;
    let mut previous_unescaped_dollar = false;
    for (offset, character) in input[start..].char_indices() {
        let escaped = preceding_backslashes % 2 == 1;
        if quote.is_none() && !escaped {
            if character == '('
                && (parenthesis_depth > 0 || offset == 0 || previous_unescaped_dollar)
            {
                parenthesis_depth += 1;
            } else if character == ')' && parenthesis_depth > 0 {
                parenthesis_depth -= 1;
            }
        }
        if quote.is_none()
            && !escaped
            && parenthesis_depth == 0
            && (character.is_whitespace()
                || matches!(character, ';' | '|' | '&' | '<' | '>' | '(' | ')'))
        {
            return start + offset;
        }
        if is_shell_syntax_quote(quote, character, escaped) {
            quote = if quote.is_some() {
                None
            } else {
                Some(opened_shell_quote(character, previous_unescaped_dollar))
            };
        }
        previous_unescaped_dollar =
            character == '$' && !escaped && quote.is_none() && !previous_unescaped_dollar;
        if character == '\\' {
            preceding_backslashes += 1;
        } else {
            preceding_backslashes = 0;
        }
    }
    input.len()
}

fn redact_shell_credential(
    input: &str,
    initial_quote: Option<ShellQuote>,
    mask: &str,
    preserve_references: bool,
) -> String {
    let mut output = String::with_capacity(input.len());
    let mut quote = initial_quote;
    let mut cursor = 0;
    let mut preceding_backslashes = 0;
    let mut previous_unescaped_dollar = false;

    for (offset, character) in input.char_indices() {
        let escaped = preceding_backslashes % 2 == 1;
        let is_syntax_quote = is_shell_syntax_quote(quote, character, escaped);
        if is_syntax_quote {
            if offset > cursor {
                output.push_str(&redact_secret(
                    &input[cursor..offset],
                    mask,
                    preserve_references,
                ));
            }
            output.push(character);
            cursor = offset + character.len_utf8();
            quote = if quote.is_some() {
                None
            } else {
                Some(opened_shell_quote(character, previous_unescaped_dollar))
            };
        }
        previous_unescaped_dollar =
            character == '$' && !escaped && quote.is_none() && !previous_unescaped_dollar;
        if character == '\\' {
            preceding_backslashes += 1;
        } else {
            preceding_backslashes = 0;
        }
    }
    if cursor < input.len() {
        output.push_str(&redact_secret(&input[cursor..], mask, preserve_references));
    }
    output
}

fn expand_reference_overlapping_secret(
    input: &str,
    reference_ranges: &[(usize, usize)],
    reference_cursor: &mut usize,
    start: usize,
    end: usize,
    replace: ReplaceKind,
) -> (usize, usize, usize) {
    let mut expanded_start = start;
    let mut expanded_end = end;
    let mut overlaps_reference = false;
    let mut reference_checks = 0;

    while *reference_cursor < reference_ranges.len()
        && reference_ranges[*reference_cursor].1 <= start
    {
        reference_checks += 1;
        *reference_cursor += 1;
    }

    let mut scan = *reference_cursor;
    while scan < reference_ranges.len() {
        reference_checks += 1;
        let (reference_start, reference_end) = reference_ranges[scan];
        if reference_start >= end {
            break;
        }
        if start < reference_end && end > reference_start {
            overlaps_reference = true;
            expanded_start = expanded_start.min(reference_start);
            expanded_end = expanded_end.max(reference_end);
        }
        scan += 1;
    }

    if overlaps_reference {
        loop {
            expanded_end += input[expanded_end..]
                .char_indices()
                .take_while(|(_, character)| credential_value_continues(*character, replace))
                .map(|(_, character)| character.len_utf8())
                .sum::<usize>();

            while scan < reference_ranges.len()
                && reference_ranges[scan].0 < expanded_end
                && reference_ranges[scan].1 <= expanded_end
            {
                reference_checks += 1;
                scan += 1;
            }
            if scan >= reference_ranges.len() {
                break;
            }
            reference_checks += 1;
            let (reference_start, reference_end) = reference_ranges[scan];
            if reference_start != expanded_end {
                break;
            }
            expanded_end = reference_end;
            scan += 1;
        }
    }

    *reference_cursor = (*reference_cursor).max(scan);
    (expanded_start, expanded_end, reference_checks)
}

fn credential_value_continues(character: char, replace: ReplaceKind) -> bool {
    match replace {
        ReplaceKind::Full => false,
        ReplaceKind::Bearer => {
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | '~' | '+' | '/' | '-' | '=')
        }
        ReplaceKind::Basic => {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '/' | '=')
        }
        ReplaceKind::Authorization => {
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | '~' | '+' | '/' | '-' | '=')
        }
        ReplaceKind::KeyValue => {
            !character.is_whitespace()
                && !matches!(character, '\'' | '"' | ',' | ';' | '}' | '{' | '[' | ']')
        }
        ReplaceKind::UriUserInfo => character != '@' && !character.is_whitespace(),
    }
}

fn redact_secret(input: &str, mask: &str, preserve_references: bool) -> String {
    if !preserve_references {
        return mask.to_string();
    }

    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    for reference in REFERENCE_PATTERN.find_iter(input) {
        if reference.start() > cursor {
            output.push_str(mask);
        }
        output.push_str(reference.as_str());
        cursor = reference.end();
    }
    if cursor < input.len() {
        output.push_str(mask);
    }
    output
}

/// Statistics about stored credentials
#[derive(Debug, Clone)]
pub struct CredentialStats {
    /// Total number of stored credentials
    pub count: usize,
    /// Count by credential type
    pub types: HashMap<CredentialType, usize>,
    /// Total number of times credentials have been resolved
    pub total_resolves: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn publication_detection_covers_hexadecimal_credentials_without_digits() {
        let value = ["ebfacefa", "cedeafbe"].concat();
        assert!(publication_secret_kind(&format!("api_key = \"{value}\"")).is_some());
        assert!(
            publication_secret_kind(&format!("password = \"{}\"", value.to_uppercase())).is_some()
        );
        assert!(publication_secret_kind(&format!("checksum = \"{value}\"")).is_none());
        assert!(publication_secret_kind("api_key = feedface").is_none());
    }

    #[test]
    fn publication_detection_uses_formats_and_entropy_without_echoing_values() {
        let token = ["ghp_", "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7cF0iL3oR6"].concat();
        assert!(publication_secret_kind(&token).is_some());
        assert!(publication_secret_kind("password = service_configuration").is_none());
        assert!(publication_secret_kind("token = $SERVICE_TOKEN").is_none());
        assert!(publication_secret_kind("password = op://vault/item/field").is_none());
        let assignment = format!("password = \"{}\"", "aB3dE6gH9jK2mN5pQ8sT");
        assert!(publication_secret_kind(&assignment).is_some());
    }

    #[test]
    fn test_store_and_resolve() {
        let mut store = CredentialStore::new();
        let reference = store.store("sk-ant-test123", CredentialType::ApiKey);

        assert!(reference.starts_with("{{CRED:api_key:"));
        assert!(reference.ends_with("}}"));

        let resolved = store.resolve(&reference);
        assert_eq!(resolved, Some("sk-ant-test123".to_string()));
    }

    #[test]
    fn test_deduplication() {
        let mut store = CredentialStore::new();
        let ref1 = store.store("secret", CredentialType::Secret);
        let ref2 = store.store("secret", CredentialType::Secret);

        assert_eq!(ref1, ref2);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn cloned_vaults_share_credentials_but_separate_vaults_do_not() {
        let vault = CredentialVault::new();
        let clone = vault.clone();
        let isolated = CredentialVault::new();
        let secret = "credential-isolation-secret";
        let reference = vault.store(secret, CredentialType::Secret);

        assert_eq!(clone.resolve_all(&reference), secret);
        assert_eq!(isolated.resolve_all(&reference), reference);
    }

    #[test]
    fn forked_vaults_survive_parent_clear_independently() {
        let parent = CredentialVault::new();
        let secret = "child-survives-parent-reset";
        let reference = parent.store(secret, CredentialType::Secret);
        let child = parent.fork();

        parent.clear();
        assert_eq!(child.resolve_all(&reference), secret);

        let child_reference = child.store("child-only-secret", CredentialType::Secret);
        assert_eq!(parent.resolve_all(&child_reference), child_reference);
    }

    #[test]
    fn absorb_child_credentials_translates_only_new_references() {
        let parent = CredentialVault::new();
        let inherited_reference = parent.store("inherited-secret", CredentialType::Secret);
        let child = parent.fork();
        let child_reference = child.store("discovered-secret", CredentialType::Token);
        let generation = parent.generation();

        let mappings = parent.absorb_child_credentials_at_generation(&child, generation);

        assert!(!mappings.contains_key(&inherited_reference));
        let parent_reference = mappings
            .get(&child_reference)
            .expect("new child reference should transfer");
        assert_ne!(parent_reference, &child_reference);
        assert_eq!(parent.resolve_all(parent_reference), "discovered-secret");
        assert_eq!(
            CredentialVault::translate_references(&format!("value={child_reference}"), &mappings),
            format!("value={parent_reference}")
        );
    }

    #[test]
    fn absorb_child_credentials_skips_stale_parent_generation() {
        let parent = CredentialVault::new();
        let child = parent.fork();
        let child_reference = child.store("stale-child-secret", CredentialType::Secret);
        let generation = parent.generation();

        parent.clear();
        let mappings = parent.absorb_child_credentials_at_generation(&child, generation);

        assert!(mappings.is_empty());
        assert_eq!(parent.stats().count, 0);
        assert_eq!(parent.resolve_all(&child_reference), child_reference);
    }

    #[test]
    fn vault_replaces_arbitrary_values_already_registered_in_the_store() {
        let vault = CredentialVault::new();
        let reference = vault.store("arbitrary-password", CredentialType::Password);

        let vaulted = vault.vault_in_json(&json!({
            "prompt": "Use arbitrary-password in the child"
        }));
        assert_eq!(vaulted["prompt"], format!("Use {reference} in the child"));
        assert_eq!(
            vault.resolve_in_json(&vaulted),
            json!({"prompt": "Use arbitrary-password in the child"})
        );
    }

    #[test]
    fn vault_does_not_rewrite_known_values_inside_existing_references() {
        let vault = CredentialVault::new();
        let reference = vault.store("password", CredentialType::Password);

        let vaulted = vault.vault_in_json(&json!({
            "prompt": format!("Use the existing reference {reference}")
        }));
        assert_eq!(
            vaulted["prompt"],
            format!("Use the existing reference {reference}")
        );
    }

    #[test]
    fn vault_overlapping_values_does_not_rewrite_generated_references() {
        let vault = CredentialVault::new();
        let password_reference = vault.store("password", CredentialType::Password);
        vault.store("word", CredentialType::Secret);

        let vaulted = vault.vault_in_text("password");
        assert_eq!(vaulted, password_reference);
        assert_eq!(vault.resolve_all(&vaulted), "password");
    }

    #[test]
    fn vault_scans_new_credentials_before_known_value_replacement() {
        let vault = CredentialVault::new();
        vault.store("word", CredentialType::Secret);
        let input = "password=new-secret";

        let vaulted = vault.vault_in_text(input);

        assert!(!vaulted.contains("new-secret"));
        assert!(vaulted.contains("{{CRED:password:"));
        assert_eq!(vault.resolve_all(&vaulted), input);
    }

    #[test]
    fn clear_drops_references_and_does_not_expose_secret_in_debug_output() {
        let mut store = CredentialStore::new();
        let secret = "credential-that-must-not-appear-in-debug-output";
        let reference = store.store(secret, CredentialType::Secret);

        assert!(!format!("{store:?}").contains(secret));
        store.clear();

        assert!(store.is_empty());
        assert_eq!(store.resolve(&reference), None);
    }

    #[test]
    fn test_resolve_all() {
        let mut store = CredentialStore::new();
        let ref1 = store.store("key1", CredentialType::ApiKey);
        let ref2 = store.store("key2", CredentialType::Token);

        let input = format!("Use {} and {} in command", ref1, ref2);
        let resolved = store.resolve_all(&input);

        assert_eq!(resolved, "Use key1 and key2 in command");
    }

    #[test]
    fn test_resolve_in_json() {
        let mut store = CredentialStore::new();
        let reference = store.store("my-secret", CredentialType::Secret);

        let input = json!({
            "command": format!("echo {}", reference),
            "nested": {
                "key": reference.clone()
            },
            "array": [reference.clone(), "other"],
            "number": 42
        });

        let resolved = store.resolve_in_json(&input);

        assert_eq!(resolved["command"], "echo my-secret");
        assert_eq!(resolved["nested"]["key"], "my-secret");
        assert_eq!(resolved["array"][0], "my-secret");
        assert_eq!(resolved["array"][1], "other");
        assert_eq!(resolved["number"], 42);
    }

    #[test]
    fn test_vault_credentials_in_json_roundtrip() {
        let vault = CredentialVault::new();
        let sample_key = ["sk", "-", "abc123def456ghi789jkl012mno345pqr678"].join("");
        let payload = json!({
            "header": format!("Authorization: Bearer {}", sample_key),
        });

        let vaulted = vault.vault_in_json(&payload);
        let header = vaulted.get("header").and_then(|v| v.as_str()).unwrap_or("");

        assert!(header.contains("{{CRED:"));
        assert!(!header.contains("abc123def456"));

        let resolved = vault.resolve_in_json(&vaulted);
        assert_eq!(resolved, payload);
    }

    #[test]
    fn vault_roundtrip_resolves_sigv4_authorization_without_nested_references() {
        let vault = CredentialVault::new();
        let authorization = concat!(
            "Authorization: AWS4-HMAC-SHA256 ",
            "Credential=AKIAIOSFODNN7EXAMPLE/20260729/us-east-1/s3/aws4_request, ",
            "SignedHeaders=host;x-amz-date, ",
            "Signature=sigv4-signature-secret"
        );
        let payload = json!({ "header": authorization });

        let vaulted = vault.vault_in_json(&payload);
        let vaulted_header = vaulted["header"].as_str().unwrap_or("");
        assert!(
            vaulted_header.contains("{{CRED:"),
            "authorization should be vaulted"
        );

        let resolved = vault.resolve_in_json(&vaulted);
        let resolved_header = resolved["header"].as_str().unwrap_or("");
        assert_eq!(resolved, payload);
        assert!(
            !resolved_header.contains("{{CRED:"),
            "roundtrip left a nested credential reference: {resolved_header}"
        );
    }

    #[test]
    fn vault_preserves_existing_reference_inside_structured_authorization() {
        let vault = CredentialVault::new();
        let reference = vault.store("nonce-secret", CredentialType::Token);
        let payload = json!({
            "header": format!("Authorization: Digest nonce=\"{reference}\""),
        });

        let vaulted = vault.vault_in_json(&payload);
        let vaulted_header = vaulted["header"].as_str().unwrap_or("");
        assert!(
            vaulted_header.contains(&reference),
            "vaulting replaced the existing credential reference"
        );
        assert!(!vaulted_header.contains("nonce=\""));

        let resolved = vault.resolve_in_json(&vaulted);
        let resolved_header = resolved["header"].as_str().unwrap_or("");
        assert_eq!(
            resolved_header,
            "Authorization: Digest nonce=\"nonce-secret\""
        );
        assert!(
            !resolved_header.contains(REFERENCE_PREFIX),
            "roundtrip left an unresolved nested reference: {resolved_header}"
        );
    }

    #[test]
    fn vault_segments_mixed_structured_authorization_around_existing_reference() {
        let vault = CredentialVault::new();
        let reference = vault.store("nonce-secret", CredentialType::Token);
        let payload = json!({
            "header": format!(
                "Authorization: Digest nonce=\"{reference}\", response=\"response-secret\""
            ),
        });

        let vaulted = vault.vault_in_json(&payload);
        let vaulted_header = vaulted["header"].as_str().unwrap_or("");
        assert!(
            vaulted_header.contains(&reference),
            "vaulting replaced the existing credential reference"
        );
        assert!(
            !vaulted_header.contains("response-secret"),
            "mixed authorization left raw credential material"
        );

        let resolved = vault.resolve_in_json(&vaulted);
        let resolved_header = resolved["header"].as_str().unwrap_or("");
        assert_eq!(
            resolved_header,
            "Authorization: Digest nonce=\"nonce-secret\", response=\"response-secret\""
        );
        assert!(
            !resolved_header.contains(REFERENCE_PREFIX),
            "roundtrip left an unresolved nested reference: {resolved_header}"
        );
    }

    #[test]
    fn portable_redaction_masks_nested_credentials_without_vault_references() {
        let api_key = ["sk-ant-", "abcdefghijklmnopqrstuvwxyz123456"].concat();
        let payload = json!({
            "message": format!("apiKey={api_key}"),
            "nested": [format!("Bearer {api_key}")],
        });

        let redacted = redact_credentials_in_json(&payload);
        let serialized = serde_json::to_string(&redacted).unwrap();

        assert!(!serialized.contains(&api_key));
        assert!(serialized.contains("[REDACTED:api_key:portable-export]"));
        assert!(!serialized.contains("{{CRED:"));
    }

    #[test]
    fn portable_redaction_masks_uri_userinfo_and_generic_assignments() {
        let payload = json!({
            "database": "DATABASE_URL=postgres://alice:db-password@db.example/app",
            "password": "password = plain-secret",
            "credential": "credential: service-secret",
            "tokens": "token=plain-token access_token=access-secret refresh-token:refresh-secret authToken=auth-secret",
        });

        let redacted = redact_credentials_in_json(&payload);
        let serialized = serde_json::to_string(&redacted).unwrap();

        for secret in [
            "db-password",
            "plain-secret",
            "service-secret",
            "plain-token",
            "access-secret",
            "refresh-secret",
            "auth-secret",
        ] {
            assert!(!serialized.contains(secret));
        }
        assert!(serialized.contains("postgres://alice:[REDACTED:password:portable-export]@"));
    }

    #[test]
    fn portable_redaction_masks_full_bearer_alphabet() {
        let payload = json!({
            "header": "Authorization: Bearer abc/remaining+secret~==",
        });

        let redacted = redact_credentials_in_json(&payload);
        let header = redacted["header"].as_str().unwrap_or("");
        assert!(!header.contains("remaining+secret"));
        assert!(header.contains("Bearer [REDACTED:token:portable-export]"));
    }

    #[test]
    fn portable_redaction_masks_basic_authorization_credentials() {
        let payload = json!({
            "header": "Authorization: Basic dXNlcjpwYXNz",
            "negotiate": "Authorization: Negotiate TlRMTVNTUAAB",
            "digest": "Authorization: Digest username=\"alice\", nonce=\"nonce-secret\", response=\"response-secret\"",
            "sigv4": "Authorization: AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20260729/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=sigv4-signature-secret",
            "noncredential": "echo NotBasic dXNlcjpwYXNz",
            "ordinary_prose": "rg 'Basic authentication' packages",
            "authorization_prose": "documentation says Authorization matters",
        });

        let redacted = redact_credentials_in_json(&payload);
        let header = redacted["header"].as_str().unwrap_or("");
        assert!(!header.contains("dXNlcjpwYXNz"));
        assert!(header.contains("Basic [REDACTED:password:portable-export]"));
        let negotiate = redacted["negotiate"].as_str().unwrap_or("");
        assert!(!negotiate.contains("TlRMTVNTUAAB"));
        assert!(negotiate.contains("Negotiate [REDACTED:token:portable-export]"));
        let digest = redacted["digest"].as_str().unwrap_or("");
        assert!(!digest.contains("nonce-secret"));
        assert!(!digest.contains("response-secret"));
        assert!(digest.contains("Digest [REDACTED:token:portable-export]"));
        let sigv4 = redacted["sigv4"].as_str().unwrap_or("");
        assert!(!sigv4.contains("20260729/us-east-1/s3/aws4_request"));
        assert!(!sigv4.contains("sigv4-signature-secret"));
        assert!(sigv4.contains("AWS4-HMAC-SHA256 [REDACTED:token:portable-export]"));
        assert_eq!(
            redacted["noncredential"].as_str(),
            Some("echo NotBasic dXNlcjpwYXNz")
        );
        assert_eq!(
            redacted["ordinary_prose"].as_str(),
            Some("rg 'Basic authentication' packages")
        );
        assert_eq!(
            redacted["authorization_prose"].as_str(),
            Some("documentation says Authorization matters")
        );
    }

    #[test]
    fn hosted_redaction_requires_authorization_context_for_bearer_prose() {
        let payload = json!({
            "header": "Authorization: Bearer abc/remaining+secret~==",
            "prose": "rg 'Bearer authentication' packages",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        assert_eq!(
            redacted["prose"].as_str(),
            Some("rg 'Bearer authentication' packages")
        );
        let header = redacted["header"].as_str().unwrap_or("");
        assert!(!header.contains("remaining+secret"));
        assert!(header.contains("Bearer [REDACTED:token:portable-export]"));
    }

    #[test]
    fn hosted_redaction_masks_complete_quoted_credential_assignments() {
        for command in [
            "curl --data 'password=abc;remaining-secret' example.test",
            "curl --data 'user=x&password=abc;embedded-remaining-secret' example.test",
            "curl --data password=\"abc;remaining-secret\" example.test",
            "curl --data password=\"abc\\\"remaining-secret\" example.test",
            "curl --data password='abc'raw-password-secret example.test",
            "curl --data password=abc'remaining-secret' example.test",
            "curl --data password=a'b'c\"d\"e example.test",
            "curl --data password=abc,remaining-secret example.test",
        ] {
            let redacted = redact_tool_arguments_preserving_references(
                "bash",
                &json!({
                    "command": command,
                }),
            );
            let command = redacted["command"].as_str().unwrap_or("");
            assert!(
                !command.contains("remaining-secret")
                    && !command.contains("raw-password-secret")
                    && !command.contains("a'b'c"),
                "credential segment leaked from {command}"
            );
            assert!(command.contains("[REDACTED:password:portable-export]"));
            assert!(command.contains("example.test"));
        }

        let array_assignment = redact_tool_arguments_preserving_references(
            "bash",
            &json!({
                "command": "password=(array-secret other-secret); password=$(printf dynamic-secret); password=`printf legacy-secret`; printf '%s' 'foo\\'; password=abc; echo next-command",
            }),
        );
        let command = array_assignment["command"].as_str().unwrap_or("");
        assert!(
            !command.contains("array-secret") && !command.contains("other-secret"),
            "array credential leaked from {command}"
        );
        assert!(!command.contains("dynamic-secret"));
        assert!(!command.contains("legacy-secret"));
        assert!(command.contains("[REDACTED:password:portable-export]"));
        assert!(command.contains("; echo next-command"));

        let shell_control = redact_tool_arguments_preserving_references(
            "bash",
            &json!({
                "command": "curl --data 'password=abc'raw-password-secret; echo next-command",
            }),
        );
        let command = shell_control["command"].as_str().unwrap_or("");
        assert!(!command.contains("raw-password-secret"));
        assert!(command.contains("; echo next-command"));
        assert_eq!(command.matches('\'').count(), 2);

        let opaque_payload = redact_credentials_in_json_preserving_references(&json!({
            "body": "password=abc;remaining-secret",
            "note": "password=abc; echo next-command",
        }));
        let opaque = opaque_payload["body"].as_str().unwrap_or("");
        assert!(!opaque.contains("remaining-secret"));
        assert_eq!(opaque, "password=[REDACTED:password:portable-export]");
        let note = opaque_payload["note"].as_str().unwrap_or("");
        assert!(!note.contains("next-command"));
        assert_eq!(note, "password=[REDACTED:password:portable-export]");
    }

    #[test]
    fn hosted_redaction_preserves_ansi_c_quoted_text_and_later_shell_control() {
        let redacted = redact_tool_arguments_preserving_references(
            "bash",
            &json!({
                "command": r"printf '%s' $'foo\'bar'; password=abc; echo ok",
            }),
        );

        assert_eq!(
            redacted["command"].as_str(),
            Some(r"printf '%s' $'foo\'bar'; password=[REDACTED:password:portable-export]; echo ok")
        );
    }

    #[test]
    fn hosted_redaction_does_not_treat_consumed_dollar_as_ansi_c_quote_prefix() {
        let redacted = redact_tool_arguments_preserving_references(
            "bash",
            &json!({
                "command": r"printf '<%s>' $$'foo\'bar' password=abc;trailing\'; echo ok",
            }),
        );

        let command = redacted["command"].as_str().unwrap_or("");
        assert!(
            !command.contains("trailing"),
            "the suffix inside the ordinary single-quoted credential token must be redacted"
        );
        assert!(command.contains("; echo ok"));
    }

    #[test]
    fn hosted_redaction_does_not_infer_shell_syntax_from_a_custom_command_field() {
        let redacted = redact_tool_arguments_preserving_references(
            "custom.mcp",
            &json!({
                "command": "password=abc;remaining-secret",
            }),
        );
        assert_eq!(
            redacted["command"].as_str(),
            Some("password=[REDACTED:password:portable-export]")
        );

        let bash = redact_tool_arguments_preserving_references(
            "bash",
            &json!({
                "body": "password=abc;remaining-secret",
            }),
        );
        assert_eq!(
            bash["body"].as_str(),
            Some("password=[REDACTED:password:portable-export]")
        );
    }

    #[test]
    fn portable_redaction_masks_ambiguous_credential_shaped_text() {
        let payload = json!({
            "password_source": "password: String",
            "token_source": "token: Option<String>",
        });

        let redacted = redact_credentials_in_json(&payload);
        let password_source = redacted["password_source"].as_str().unwrap_or("");
        let token_source = redacted["token_source"].as_str().unwrap_or("");
        assert!(!password_source.contains("password: String"));
        assert!(!token_source.contains("token: Option<String>"));
        assert!(password_source.contains("[REDACTED:password:portable-export]"));
        assert!(token_source.contains("[REDACTED:token:portable-export]"));
    }

    #[test]
    fn portable_redaction_does_not_treat_uppercase_secrets_as_types() {
        let payload = json!({
            "password_source": "password: TOPSECRETVALUE",
            "token_source": "token: ABCDEFGHIJKLMNOPQRST",
        });

        let redacted = redact_credentials_in_json(&payload);
        let password_source = redacted["password_source"].as_str().unwrap_or("");
        let token_source = redacted["token_source"].as_str().unwrap_or("");
        assert!(!password_source.contains("TOPSECRETVALUE"));
        assert!(!token_source.contains("ABCDEFGHIJKLMNOPQRST"));
        assert!(password_source.contains("[REDACTED:password:portable-export]"));
        assert!(token_source.contains("[REDACTED:api_key:portable-export]"));
    }

    #[test]
    fn portable_redaction_masks_process_local_vault_references() {
        let reference = "{{CRED:token:abcdef012345}}";
        let payload = json!({
            "standalone": reference,
            "command": format!("Authorization: Bearer {reference}"),
        });

        let redacted = redact_credentials_in_json(&payload);
        let serialized = serde_json::to_string(&redacted).unwrap();

        assert!(!serialized.contains("{{CRED:"));
        assert!(serialized.contains("[REDACTED:"));
    }

    #[test]
    fn hosted_redaction_does_not_let_unclosed_reference_hide_later_bearer_secret() {
        let payload = json!({
            "command": "echo '{{CRED:'; curl -H 'Authorization: Bearer real-secret'",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");

        assert!(!command.contains("real-secret"));
        assert!(command.contains("[REDACTED:token:portable-export]"));
    }

    #[test]
    fn hosted_redaction_masks_closed_malformed_reference_containing_api_key() {
        let payload = json!({
            "command": "echo {{CRED:sk-ant-abcdefghijklmnopqrstuvwxyz123456}}",
            "delimited": "{{CRED:password:abc,remaining-secret}}",
            "whitespace": "{{CRED:password:abc remaining-secret}} after",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");
        let delimited = redacted["delimited"].as_str().unwrap_or("");
        let whitespace = redacted["whitespace"].as_str().unwrap_or("");

        assert!(!command.contains("sk-ant-abcdefghijklmnopqrstuvwxyz123456"));
        assert!(!command.contains("{{CRED:"));
        assert!(command.contains("[REDACTED:credential_reference:portable-export]"));
        assert!(!delimited.contains("remaining-secret"));
        assert!(!delimited.contains("{{CRED:"));
        assert_eq!(delimited, "[REDACTED:credential_reference:portable-export]");
        assert!(!whitespace.contains("remaining-secret"));
        assert!(!whitespace.contains("{{CRED:"));
        assert_eq!(
            whitespace,
            "[REDACTED:credential_reference:portable-export] after"
        );
    }

    #[test]
    fn hosted_redaction_masks_closed_multiline_malformed_reference() {
        let payload = json!({
            "multiline": "{{CRED:password:abc\nremaining-secret}}",
            "multiline_with_spaces": "{{CRED:password:abc\nremaining secret\nmore-secret}}",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let multiline = redacted["multiline"].as_str().unwrap_or("");
        let multiline_with_spaces = redacted["multiline_with_spaces"].as_str().unwrap_or("");

        assert!(!multiline.contains("{{CRED:"));
        assert!(
            !multiline.contains("remaining-secret"),
            "malformed reference suffix leaked: {multiline}"
        );
        assert!(
            !multiline_with_spaces.contains("remaining secret")
                && !multiline_with_spaces.contains("more-secret"),
            "multiline malformed reference suffix leaked: {multiline_with_spaces}"
        );
        assert_eq!(multiline, "[REDACTED:credential_reference:portable-export]");
        assert_eq!(
            multiline_with_spaces,
            "[REDACTED:credential_reference:portable-export]"
        );
    }

    #[test]
    fn hosted_redaction_does_not_merge_unclosed_marker_with_later_reference() {
        let valid_reference = "{{CRED:token:abcdef012345}}";
        let payload = json!({
            "command": format!(
                "echo '{{{{CRED:'\nkeep-this-output\n{valid_reference}"
            ),
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");

        assert!(command.contains("keep-this-output"));
        assert!(command.contains(valid_reference));
    }

    #[test]
    fn hosted_redaction_does_not_claim_an_unrelated_template_closer() {
        let payload = json!({
            "command": "{{CRED:broken\nkeep-this-output\n{{ value }}",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");

        assert!(!command.contains("{{CRED:broken"));
        assert!(command.contains("keep-this-output"));
        assert!(command.contains("{{ value }}"));
        assert_eq!(
            command,
            "[REDACTED:credential_reference:portable-export]\nkeep-this-output\n{{ value }}"
        );
    }

    #[test]
    fn hosted_redaction_masks_same_line_nested_braces_inside_malformed_reference() {
        let payload = json!({
            "command": "{{CRED:password:abc{{x}}remaining-secret}}",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");

        assert!(!command.contains(REFERENCE_PREFIX));
        assert!(!command.contains("remaining-secret"));
        assert!(!command.contains("{{x}}"));
        assert_eq!(command, "[REDACTED:credential_reference:portable-export]");
    }

    #[test]
    fn hosted_redaction_masks_cross_line_nested_braces_with_outer_close() {
        let payload = json!({
            "command": "{{CRED:password:abc\n{{x}}remaining-secret}}",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");

        assert!(!command.contains(REFERENCE_PREFIX));
        assert!(!command.contains("remaining-secret"));
        assert!(!command.contains("{{x}}"));
        assert_eq!(command, "[REDACTED:credential_reference:portable-export]");
    }

    #[test]
    fn hosted_redaction_masks_nested_braces_before_multiline_outer_close() {
        let payload = json!({
            "command": "{{CRED:password:abc\n{{x}}\nremaining-secret}}",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");

        assert!(!command.contains(REFERENCE_PREFIX));
        assert!(!command.contains("remaining-secret"));
        assert!(!command.contains("{{x}}"));
        assert_eq!(command, "[REDACTED:credential_reference:portable-export]");
    }

    #[test]
    fn hosted_redaction_accepts_outer_closer_on_its_own_line() {
        for command in [
            "{{CRED:password:abc\n{{x}}\nremaining-secret\n}}",
            "{{CRED:password:abc\r\n{{x}}\r\nremaining-secret\r\n\t}}",
        ] {
            let payload = json!({ "command": command });

            let redacted = redact_credentials_in_json_preserving_references(&payload);
            let command = redacted["command"].as_str().unwrap_or("");

            assert!(!command.contains(REFERENCE_PREFIX));
            assert!(!command.contains("remaining-secret"));
            assert!(!command.contains("{{x}}"));
            assert_eq!(command, "[REDACTED:credential_reference:portable-export]");
        }
    }

    #[test]
    fn hosted_redaction_masks_nested_braces_across_lone_carriage_returns() {
        let payload = json!({
            "command": "{{CRED:password:abc\r{{x}}\rremaining-secret}}",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");

        assert!(!command.contains(REFERENCE_PREFIX));
        assert!(!command.contains("remaining-secret"));
        assert!(!command.contains("{{x}}"));
        assert_eq!(command, "[REDACTED:credential_reference:portable-export]");
    }

    #[test]
    fn hosted_redaction_bounds_lone_carriage_returns_before_unrelated_closer() {
        let payload = json!({
            "command": "{{CRED:broken\r{{x}}\rkeep-this-output\r{{ value }}}",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");

        assert!(!command.contains("{{CRED:broken"));
        assert!(command.contains("{{x}}"));
        assert!(command.contains("keep-this-output"));
        assert!(command.contains("{{ value }}}"));
    }

    #[test]
    fn hosted_redaction_does_not_reuse_overlapping_template_closer() {
        let payload = json!({
            "command": "{{CRED:broken\nkeep-this-output\n{{ value }}}",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");

        assert!(!command.contains("{{CRED:broken"));
        assert!(command.contains("keep-this-output"));
        assert!(command.contains("{{ value }}}"));
        assert_eq!(
            command,
            "[REDACTED:credential_reference:portable-export]\nkeep-this-output\n{{ value }}}"
        );
    }

    #[test]
    fn credential_reference_scanner_is_linear_for_repeated_unclosed_prefixes() {
        let input = REFERENCE_PREFIX.repeat(2_000);

        let (ranges, scanned_bytes) = credential_reference_like_ranges_with_scan_count(&input);

        assert_eq!(ranges.len(), 2_000);
        assert!(
            scanned_bytes <= input.len() * 2,
            "scanner revisited bytes: scanned {scanned_bytes} for {} input bytes",
            input.len()
        );
    }

    #[test]
    fn credential_reference_scanner_is_linear_for_unrelated_template_closers() {
        let entry_count = 2_000;
        let mut input = String::new();
        for _ in 0..entry_count {
            input.push_str("{{CRED:broken\nkeep-this-output\n{{ value }}\n");
        }

        let (ranges, scanned_bytes) = credential_reference_like_ranges_with_scan_count(&input);

        assert_eq!(ranges.len(), entry_count);
        assert!(
            scanned_bytes <= input.len() * 2,
            "scanner revisited bytes: scanned {scanned_bytes} for {} input bytes",
            input.len()
        );
    }

    #[test]
    fn credential_reference_scanner_is_linear_for_same_line_nested_templates() {
        let entry_count = 2_000;
        let mut input = String::new();
        for _ in 0..entry_count {
            input.push_str("{{CRED:password:abc{{x}}remaining-secret}}\n");
        }

        let (ranges, scanned_bytes) = credential_reference_like_ranges_with_scan_count(&input);

        assert_eq!(ranges.len(), entry_count);
        assert!(
            scanned_bytes <= input.len() * 2,
            "scanner revisited bytes: scanned {scanned_bytes} for {} input bytes",
            input.len()
        );
    }

    #[test]
    fn hosted_redaction_scans_reference_ranges_and_quote_state_linearly() {
        let entry_count = 2_000;
        let mut input = String::new();
        for index in 0..entry_count {
            let id = format!("{index:012x}");
            writeln!(
                &mut input,
                "password={{{{CRED:token:{id}}}}}raw-secret-{index}"
            )
            .unwrap();
        }
        let password_pattern = CREDENTIAL_PATTERNS
            .iter()
            .find(|pattern| {
                pattern.kind == CredentialType::Password
                    && matches!(pattern.replace, ReplaceKind::KeyValue)
            })
            .expect("password assignment pattern");

        let (redacted, reference_checks, quote_scan_bytes, credential_scan_bytes) =
            redact_pattern_with_reference_checks(
                &input,
                password_pattern,
                true,
                StringRedactionContext::Shell,
            );

        assert_eq!(
            redacted.matches("{{CRED:token:").count(),
            entry_count,
            "canonical references must be preserved"
        );
        assert!(!redacted.contains("raw-secret-"));
        assert!(
            reference_checks <= entry_count * 3,
            "each ordered reference should be examined at most once plus overlap and adjacency look-ahead per secret; got {reference_checks}"
        );
        assert!(
            quote_scan_bytes <= input.len(),
            "shell quote state must advance monotonically; scanned {quote_scan_bytes} bytes for {} bytes of input",
            input.len()
        );
        assert!(
            credential_scan_bytes <= input.len(),
            "credential boundaries must cover disjoint ranges; scanned {credential_scan_bytes} bytes for {} bytes of input",
            input.len()
        );
    }

    #[test]
    fn hosted_redaction_skips_credential_captures_already_covered_by_a_quoted_value() {
        let entry_count = 2_000;
        let body = (0..entry_count)
            .map(|index| format!("password=secret-{index}"))
            .collect::<Vec<_>>()
            .join("&");
        let input = format!("curl --data '{body}' example.test");
        let password_pattern = CREDENTIAL_PATTERNS
            .iter()
            .find(|pattern| {
                pattern.kind == CredentialType::Password
                    && matches!(pattern.replace, ReplaceKind::KeyValue)
            })
            .expect("password assignment pattern");

        let (redacted, _, quote_scan_bytes, credential_scan_bytes) =
            redact_pattern_with_reference_checks(
                &input,
                password_pattern,
                true,
                StringRedactionContext::Shell,
            );

        assert!(!redacted.contains("secret-"));
        assert!(redacted.contains("example.test"));
        assert!(
            quote_scan_bytes <= input.len(),
            "shell quote state must advance monotonically; scanned {quote_scan_bytes} bytes for {} bytes of input",
            input.len()
        );
        assert!(
            credential_scan_bytes <= input.len(),
            "covered captures must not rescan the same quoted suffix; scanned {credential_scan_bytes} bytes for {} bytes of input",
            input.len()
        );
    }

    #[test]
    fn hosted_redaction_preserves_vaulted_references_and_masks_surrounding_raw_text() {
        let reference = "{{CRED:token:abcdef012345}}";
        let payload = json!({
            "command": format!(
                "curl -H 'Authorization: Bearer {reference}' example.test password=raw-secret"
            ),
            "adjacent_bearer": format!("Authorization: Bearer {reference}raw-bearer-secret"),
            "adjacent_password": format!("password={reference}raw-password-secret"),
            "malformed_id": "password={{CRED:token:abc123}}raw-malformed-id-secret",
            "malformed_type":
                "password={{CRED:not_a_credential:abcdef012345}}raw-malformed-type-secret",
            "malformed_shape": "password={{CRED:token:abc:123}}raw-malformed-shape-secret",
            "aws_adjacent":
                "AWS_SECRET_KEY={{CRED:token:abcdef012345}}rawsecretlongenough",
            "aws_malformed_unicode":
                "AWS_SECRET_KEY={{CRED:token:éééééééééééééééé}}rawunicodesecret",
            "aws_multiple_references":
                "AWS_SECRET_KEY={{CRED:token:abcdef012345}}abcdefghijklmn{{CRED:password:fedcba543210}}rawmultisecret",
            "malformed_nested": "password={{CRED:token:{abc}}}rawnestedsecret",
            "malformed_unclosed": "password={{CRED:token:abcdefrawunclosedsecret",
        });

        let redacted = redact_credentials_in_json_preserving_references(&payload);
        let command = redacted["command"].as_str().unwrap_or("");
        assert!(command.contains(reference));
        assert!(!command.contains("raw-secret"));
        assert!(command.contains("[REDACTED:password:portable-export]"));
        let adjacent_bearer = redacted["adjacent_bearer"].as_str().unwrap_or("");
        assert!(adjacent_bearer.contains(reference));
        assert!(!adjacent_bearer.contains("raw-bearer-secret"));
        assert!(adjacent_bearer.contains("[REDACTED:token:portable-export]"));
        let adjacent_password = redacted["adjacent_password"].as_str().unwrap_or("");
        assert!(adjacent_password.contains(reference));
        assert!(!adjacent_password.contains("raw-password-secret"));
        assert!(adjacent_password.contains("[REDACTED:password:portable-export]"));
        let malformed_id = redacted["malformed_id"].as_str().unwrap_or("");
        assert!(!malformed_id.contains("{{CRED:"));
        assert!(!malformed_id.contains("raw-malformed-id-secret"));
        assert!(malformed_id.contains("[REDACTED:password:portable-export]"));
        let malformed_type = redacted["malformed_type"].as_str().unwrap_or("");
        assert!(!malformed_type.contains("{{CRED:"));
        assert!(!malformed_type.contains("raw-malformed-type-secret"));
        assert!(malformed_type.contains("[REDACTED:password:portable-export]"));
        let malformed_shape = redacted["malformed_shape"].as_str().unwrap_or("");
        assert!(!malformed_shape.contains("{{CRED:"));
        assert!(!malformed_shape.contains("raw-malformed-shape-secret"));
        assert!(malformed_shape.contains("[REDACTED:password:portable-export]"));
        let aws_adjacent = redacted["aws_adjacent"].as_str().unwrap_or("");
        assert!(aws_adjacent.contains(reference));
        assert!(!aws_adjacent.contains("rawsecretlongenough"));
        assert!(aws_adjacent.contains("[REDACTED:secret:portable-export]"));
        let aws_malformed_unicode = redacted["aws_malformed_unicode"].as_str().unwrap_or("");
        assert!(!aws_malformed_unicode.contains("{{CRED:"));
        assert!(!aws_malformed_unicode.contains("rawunicodesecret"));
        assert!(aws_malformed_unicode.contains("[REDACTED:secret:portable-export]"));
        let aws_multiple_references = redacted["aws_multiple_references"].as_str().unwrap_or("");
        assert!(aws_multiple_references.contains("{{CRED:token:abcdef012345}}"));
        assert!(aws_multiple_references.contains("{{CRED:password:fedcba543210}}"));
        assert!(!aws_multiple_references.contains("abcdefghijklmn"));
        assert!(!aws_multiple_references.contains("rawmultisecret"));
        assert!(aws_multiple_references.contains("[REDACTED:secret:portable-export]"));
        let malformed_nested = redacted["malformed_nested"].as_str().unwrap_or("");
        assert!(!malformed_nested.contains("{{CRED:"));
        assert!(!malformed_nested.contains("rawnestedsecret"));
        assert!(malformed_nested.contains("[REDACTED:password:portable-export]"));
        let malformed_unclosed = redacted["malformed_unclosed"].as_str().unwrap_or("");
        assert!(!malformed_unclosed.contains("{{CRED:"));
        assert!(!malformed_unclosed.contains("rawunclosedsecret"));
        assert!(malformed_unclosed.contains("[REDACTED:password:portable-export]"));
    }

    #[test]
    fn vault_roundtrips_uri_userinfo_and_generic_assignments() {
        let vault = CredentialVault::new();
        let payload = json!({
            "command": "connect postgres://alice:db-password@db.example/app password=plain-secret",
        });

        let vaulted = vault.vault_in_json(&payload);
        let serialized = serde_json::to_string(&vaulted).unwrap();
        assert!(!serialized.contains("db-password"));
        assert!(!serialized.contains("plain-secret"));
        assert_eq!(vault.resolve_in_json(&vaulted), payload);
    }

    #[test]
    fn test_vaults_private_key_blocks() {
        let vault = CredentialVault::new();
        let rsa_label = ["RSA", "PRIVATE", "KEY"].join(" ");
        let private_key = [
            "-----BEGIN ",
            rsa_label.as_str(),
            "-----\n",
            "MIIEpAIBAAKCAQEA...",
            "\n-----END RSA PRIVATE KEY-----",
        ]
        .join("");
        let payload = json!({
            "key": private_key,
        });

        let vaulted = vault.vault_in_json(&payload);
        let value = vaulted.get("key").and_then(|v| v.as_str()).unwrap_or("");

        assert!(value.contains("{{CRED:"));
        assert!(!value.contains("MIIEpAIB"));
        assert!(!value.contains("END RSA PRIVATE KEY"));
    }

    #[test]
    fn test_vaults_jwt_and_aws_secret() {
        let vault = CredentialVault::new();
        let aws_secret = ["wJalrXUtnFEMI/K7MDENG", "/bPxRfiCY", "EXAMPLEKEY"].join("");
        let jwt = [
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIn0",
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        ]
        .join(".");
        let payload = json!({
            "header": format!("AWS_SECRET_ACCESS_KEY={aws_secret}"),
            "token": jwt,
        });

        let vaulted = vault.vault_in_json(&payload);
        let header = vaulted.get("header").and_then(|v| v.as_str()).unwrap_or("");
        let token = vaulted.get("token").and_then(|v| v.as_str()).unwrap_or("");

        assert!(header.contains("{{CRED:"));
        assert!(!header.contains("EXAMPLEKEY"));
        assert!(token.contains("{{CRED:"));
        assert!(!token.contains("SflKxwRJSMeKKF2Q"));
    }

    #[test]
    fn test_has_references() {
        assert!(CredentialStore::has_references(
            "test {{CRED:api_key:abcdef012345}} end"
        ));
        assert!(!CredentialStore::has_references("no references here"));
        assert!(!CredentialStore::has_references(
            "test {{CRED:api_key:abc123}} end"
        ));
    }

    #[test]
    fn test_unknown_reference() {
        let mut store = CredentialStore::new();
        let result = store.resolve("{{CRED:api_key:nonexistent}}");
        assert_eq!(result, None);
    }

    #[test]
    fn test_clear() {
        let mut store = CredentialStore::new();
        store.store("secret1", CredentialType::Secret);
        store.store("secret2", CredentialType::Secret);

        assert_eq!(store.len(), 2);

        store.clear();

        assert!(store.is_empty());
    }

    #[test]
    fn test_stats() {
        let mut store = CredentialStore::new();
        let ref1 = store.store("key1", CredentialType::ApiKey);
        store.store("key2", CredentialType::ApiKey);
        store.store("token1", CredentialType::Token);

        // Resolve a few times
        store.resolve(&ref1);
        store.resolve(&ref1);

        let stats = store.stats();
        assert_eq!(stats.count, 3);
        assert_eq!(stats.types.get(&CredentialType::ApiKey), Some(&2));
        assert_eq!(stats.types.get(&CredentialType::Token), Some(&1));
        assert_eq!(stats.total_resolves, 2);
    }

    #[test]
    fn test_credential_type_roundtrip() {
        let types = [
            CredentialType::ApiKey,
            CredentialType::Token,
            CredentialType::Password,
            CredentialType::Secret,
            CredentialType::PrivateKey,
            CredentialType::ConnectionString,
            CredentialType::Unknown,
        ];

        for cred_type in types {
            let s = cred_type.as_str();
            let parsed = CredentialType::from_str(s);
            assert_eq!(parsed, cred_type);
        }
    }
}
