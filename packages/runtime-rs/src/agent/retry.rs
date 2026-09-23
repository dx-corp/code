//! Retry Logic with Exponential Backoff
//!
//! This module provides intelligent retry handling for transient failures in AI API calls.
//! It implements exponential backoff with jitter to prevent thundering herd problems and
//! respects rate limit headers when available.
//!
//! # Features
//!
//! - **Exponential Backoff**: Delays increase exponentially (1s, 2s, 4s, 8s, ...)
//! - **Jitter**: Random variation prevents synchronized retries across clients
//! - **Rate Limit Awareness**: Extracts and respects `Retry-After` headers
//! - **Configurable Limits**: Max retries, max delay, and budget exhaustion
//! - **Error Classification**: Distinguishes retryable, fatal, and rate-limited errors
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_runtime::agent::retry::{RetryPolicy, RetryDecision, ErrorKind};
//!
//! let mut policy = RetryPolicy::default();
//!
//! loop {
//!     match make_api_call().await {
//!         Ok(response) => break response,
//!         Err(e) => {
//!             let kind = ErrorKind::classify(&e.to_string());
//!             match policy.should_retry(kind) {
//!                 RetryDecision::Retry { delay, attempt } => {
//!                     println!("Retrying in {:?} (attempt {})", delay, attempt);
//!                     tokio::time::sleep(delay).await;
//!                 }
//!                 RetryDecision::GiveUp { reason } => {
//!                     return Err(anyhow::anyhow!("Giving up: {}", reason));
//!                 }
//!             }
//!         }
//!     }
//! }
//! ```

use std::time::Duration;

use rand::RngExt as _;

/// Configuration for retry behavior
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (not including the initial attempt)
    pub max_retries: u32,
    /// Initial delay before first retry
    pub initial_delay: Duration,
    /// Maximum delay between retries (caps exponential growth)
    pub max_delay: Duration,
    /// Multiplier for exponential backoff (typically 2.0)
    pub backoff_multiplier: f64,
    /// Jitter factor (0.0 to 1.0) - adds randomness to delay
    pub jitter_factor: f64,
    /// Whether to respect Retry-After headers when present
    pub respect_retry_after: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_mins(1),
            backoff_multiplier: 2.0,
            jitter_factor: 0.25,
            respect_retry_after: true,
        }
    }
}

impl RetryConfig {
    /// Create a bounded retry window for unattended hosted turns.
    ///
    /// The interactive default stays short, while hosted work can ride out a
    /// brief provider or network interruption without replaying the turn.
    #[must_use]
    pub fn hosted_outage() -> Self {
        Self {
            max_retries: 8,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            jitter_factor: 0.25,
            respect_retry_after: true,
        }
    }

    /// Create a config for aggressive retrying (more attempts, shorter delays)
    #[must_use]
    pub fn aggressive() -> Self {
        Self {
            max_retries: 5,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            backoff_multiplier: 1.5,
            jitter_factor: 0.2,
            respect_retry_after: true,
        }
    }

    /// Create a config for conservative retrying (fewer attempts, longer delays)
    #[must_use]
    pub fn conservative() -> Self {
        Self {
            max_retries: 2,
            initial_delay: Duration::from_secs(2),
            max_delay: Duration::from_mins(2),
            backoff_multiplier: 3.0,
            jitter_factor: 0.3,
            respect_retry_after: true,
        }
    }

    /// Create a config that never retries
    #[must_use]
    pub fn no_retry() -> Self {
        Self {
            max_retries: 0,
            ..Default::default()
        }
    }
}

/// Classification of error types for retry decisions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Transient error that may succeed on retry (e.g., network timeout, 5xx errors)
    Transient,
    /// Rate limit exceeded - should wait before retrying
    RateLimited {
        /// Suggested wait time from Retry-After header (if available)
        retry_after: Option<Duration>,
    },
    /// Context overflow - requires compaction, not retry
    ContextOverflow,
    /// Authentication/authorization failure - won't succeed on retry
    AuthFailure,
    /// Invalid request - won't succeed on retry
    InvalidRequest,
    /// Provider quota or usage window is exhausted. Immediate retries cannot
    /// succeed until the window resets.
    QuotaExceeded,
    /// Unknown error - default to not retrying
    Unknown,
}

impl ErrorKind {
    /// Classify an error message into an `ErrorKind`
    #[must_use]
    pub fn classify(error_message: &str) -> Self {
        let lower = error_message.to_lowercase();

        // Quota windows must not be classified as transient "try again"
        // because Codex usage-limit copy includes a future reset time.
        if lower.contains("usage limit")
            || lower.contains("usagelimitexceeded")
            || lower.contains("quota exceeded")
            || lower.contains("quota_exceeded")
            || lower.contains("insufficient_quota")
            || lower.contains("prepaid_credits_insufficient")
            || lower.contains("hit your usage limit")
        {
            return ErrorKind::QuotaExceeded;
        }

        // Check for rate limiting first (most specific)
        if lower.contains("rate limit")
            || lower.contains("rate_limit")
            || lower.contains("too many requests")
            || Self::contains_http_status_code(&lower, "429")
        {
            // Try to extract retry-after time from message
            let retry_after = Self::extract_retry_after(&lower);
            return ErrorKind::RateLimited { retry_after };
        }

        // Check for context overflow. One vocabulary, shared with the
        // provider clients, so a rejection the client already recognizes is
        // not classified differently here and denied its recovery. The
        // bodyless-4xx half of `is_context_overflow_error` is deliberately
        // left out: this classification compacts history, which is too
        // destructive to trigger on a status code with no message.
        if crate::ai::is_context_overflow_message(&lower) {
            return ErrorKind::ContextOverflow;
        }

        // Check for auth failures
        if lower.contains("unauthorized")
            || lower.contains("authentication")
            || lower.contains("invalid api key")
            || lower.contains("invalid_api_key")
            || Self::contains_http_status_code(&lower, "401")
            || Self::contains_http_status_code(&lower, "403")
        {
            return ErrorKind::AuthFailure;
        }

        // Check for invalid requests
        if lower.contains("invalid request")
            || lower.contains("bad request")
            || lower.contains("malformed")
            || Self::contains_http_status_code(&lower, "400")
        {
            return ErrorKind::InvalidRequest;
        }

        // Check for transient errors
        if lower.contains("empty_assistant_response")
            || lower.contains("overloaded")
            || Self::contains_http_status_code(&lower, "500")
            || Self::contains_http_status_code(&lower, "502")
            || Self::contains_http_status_code(&lower, "503")
            || Self::contains_http_status_code(&lower, "504")
            || lower.contains("service unavailable")
            || lower.contains("server error")
            || lower.contains("internal error")
            || lower.contains("temporarily")
            || lower.contains("try again")
            || lower.contains("timeout")
            || lower.contains("timed out")
            || lower.contains("connection")
            || lower.contains("network")
        {
            return ErrorKind::Transient;
        }

        ErrorKind::Unknown
    }

    /// Match a three-digit HTTP status only when it is a standalone status or
    /// is introduced by an explicit status/error/code label. Parser locations,
    /// record IDs, and larger numbers are not provider status evidence.
    fn contains_http_status_code(message: &str, code: &str) -> bool {
        debug_assert!(code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_digit()));

        message.match_indices(code).any(|(start, _)| {
            let before = &message[..start];
            let after = &message[start + code.len()..];
            if before
                .chars()
                .next_back()
                .is_some_and(|character| character.is_ascii_alphanumeric())
                || after
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_alphanumeric())
            {
                return false;
            }

            let prefix = before.trim_end_matches(|character: char| {
                character.is_ascii_whitespace()
                    || matches!(character, ':' | '=' | '(' | '[' | '{' | '"' | '\'')
            });
            if prefix.is_empty() {
                return true;
            }

            const STATUS_CONTEXTS: &[&str] = &[
                "http",
                "http error",
                "http status",
                "http status code",
                "status",
                "status code",
                "status_code",
                "response status",
                "response status code",
                "error",
                "error code",
                "code",
            ];
            STATUS_CONTEXTS.iter().any(|context| {
                prefix.strip_suffix(context).is_some_and(|leading| {
                    leading
                        .chars()
                        .next_back()
                        .is_none_or(|character| !character.is_ascii_alphanumeric())
                })
            }) || prefix
                .split_ascii_whitespace()
                .next_back()
                .is_some_and(Self::is_http_version)
        })
    }

    fn is_http_version(token: &str) -> bool {
        token.strip_prefix("http/").is_some_and(|version| {
            !version.is_empty()
                && version
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
                && version.bytes().any(|byte| byte.is_ascii_digit())
        })
    }

    /// Extract retry-after duration from error message (if present)
    fn extract_retry_after(message: &str) -> Option<Duration> {
        // Common patterns: "retry after 30 seconds", "retry-after: 30", "wait 1.5 minutes"

        // Pattern: "retry after X seconds"
        if let Some(pos) = message.find("retry after") {
            let after = &message[pos + 11..];
            if let Some(secs) = Self::parse_duration_from_start(after) {
                return Some(secs);
            }
        }

        // Pattern: "retry-after: X" or "retry_after: X"
        if let Some(pos) = message.find("retry-after") {
            let after = &message[pos + 12..];
            if let Some(secs) = Self::parse_duration_from_start(after) {
                return Some(secs);
            }
        }
        if let Some(pos) = message.find("retry_after") {
            let after = &message[pos + 12..];
            if let Some(secs) = Self::parse_duration_from_start(after) {
                return Some(secs);
            }
        }

        // Pattern: "wait X seconds/minutes"
        if let Some(pos) = message.find("wait") {
            let after = &message[pos + 4..];
            if let Some(secs) = Self::parse_duration_from_start(after) {
                return Some(secs);
            }
        }

        None
    }

    /// Parse a duration from the start of a string (e.g., "30 seconds", "1.5 minutes")
    fn parse_duration_from_start(s: &str) -> Option<Duration> {
        let s = s.trim_start_matches(|c: char| !c.is_ascii_digit());

        // Find the number
        let num_end = s
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len());
        let num_str = &s[..num_end];
        let num: f64 = num_str.parse().ok()?;

        // Check for unit
        let rest = s[num_end..].trim();
        let multiplier = if rest.starts_with("minute") || rest.starts_with("min") {
            60.0
        } else if rest.starts_with("hour") || rest.starts_with("hr") {
            3600.0
        } else {
            1.0 // default to seconds
        };

        // A provider can return a value larger than Duration can represent.
        // Keep it as an upper bound so calculate_delay applies max_delay.
        Some(Duration::try_from_secs_f64(num * multiplier).unwrap_or(Duration::MAX))
    }

    /// Check if this error kind is retryable
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, ErrorKind::Transient | ErrorKind::RateLimited { .. })
    }
}

/// Decision about whether to retry
#[derive(Debug, Clone)]
pub enum RetryDecision {
    /// Retry after the specified delay
    Retry {
        /// How long to wait before retrying
        delay: Duration,
        /// Which attempt this will be (1-indexed)
        attempt: u32,
        /// Reason for retrying (for logging/display)
        reason: String,
    },
    /// Stop retrying and give up
    GiveUp {
        /// Reason for giving up
        reason: String,
    },
}

/// Retry policy that tracks state across retry attempts
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Configuration
    config: RetryConfig,
    /// Current attempt number (0 = initial attempt, 1 = first retry, etc.)
    current_attempt: u32,
    /// Total delay accumulated across all retries
    total_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(RetryConfig::default())
    }
}

impl RetryPolicy {
    /// Create a new retry policy with the given configuration
    #[must_use]
    pub fn new(config: RetryConfig) -> Self {
        Self {
            config,
            current_attempt: 0,
            total_delay: Duration::ZERO,
        }
    }

    /// Create a retry policy with default configuration
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::default()
    }

    /// Reset the policy for a new request
    pub fn reset(&mut self) {
        self.current_attempt = 0;
        self.total_delay = Duration::ZERO;
    }

    /// Get the current attempt number (0 = initial, 1+ = retries)
    #[must_use]
    pub fn current_attempt(&self) -> u32 {
        self.current_attempt
    }

    /// Get the total delay accumulated across all retries
    #[must_use]
    pub fn total_delay(&self) -> Duration {
        self.total_delay
    }

    /// Determine whether to retry based on the error kind
    pub fn should_retry(&mut self, error_kind: ErrorKind) -> RetryDecision {
        // Check if error is retryable at all
        if !error_kind.is_retryable() {
            return RetryDecision::GiveUp {
                reason: match error_kind {
                    ErrorKind::ContextOverflow => {
                        "Context overflow - compaction required".to_string()
                    }
                    ErrorKind::AuthFailure => "Authentication failed - check API key".to_string(),
                    ErrorKind::InvalidRequest => {
                        "Invalid request - won't succeed on retry".to_string()
                    }
                    ErrorKind::QuotaExceeded => {
                        "Provider usage limit reached - not retrying".to_string()
                    }
                    ErrorKind::Unknown => "Unknown error - not retrying".to_string(),
                    _ => "Error is not retryable".to_string(),
                },
            };
        }

        // Check if we've exhausted retries
        if self.current_attempt >= self.config.max_retries {
            return RetryDecision::GiveUp {
                reason: format!("Exhausted {} retry attempts", self.config.max_retries),
            };
        }

        // Calculate delay
        let delay = self.calculate_delay(error_kind);

        // Update state
        self.current_attempt += 1;
        self.total_delay += delay;

        let reason = match error_kind {
            ErrorKind::RateLimited { .. } => "Rate limited by API".to_string(),
            ErrorKind::Transient => "Transient error, retrying".to_string(),
            _ => "Retrying".to_string(),
        };

        RetryDecision::Retry {
            delay,
            attempt: self.current_attempt,
            reason,
        }
    }

    /// Calculate the delay for the current retry attempt
    fn calculate_delay(&self, error_kind: ErrorKind) -> Duration {
        // If rate limited with retry-after, use that
        if let ErrorKind::RateLimited {
            retry_after: Some(suggested),
        } = error_kind
        {
            if self.config.respect_retry_after {
                // Still apply max_delay cap
                return suggested.min(self.config.max_delay);
            }
        }

        // Calculate exponential backoff
        let base_delay = self.config.initial_delay.as_secs_f64()
            * self
                .config
                .backoff_multiplier
                .powi(self.current_attempt as i32);

        // Apply max delay cap
        let capped_delay = base_delay.min(self.config.max_delay.as_secs_f64());

        // Apply jitter
        let jittered_delay = if self.config.jitter_factor > 0.0 {
            let mut rng = rand::rng();
            let jitter_range = capped_delay * self.config.jitter_factor;
            let jitter = rng.random_range(-jitter_range..=jitter_range);
            (capped_delay + jitter).max(0.1) // Ensure at least 100ms
        } else {
            capped_delay
        };

        Duration::from_secs_f64(jittered_delay)
    }

    /// Create a human-readable status message for the current retry state
    #[must_use]
    pub fn status_message(&self) -> String {
        if self.current_attempt == 0 {
            "Initial request".to_string()
        } else {
            format!(
                "Retry {}/{} (total wait: {:.1}s)",
                self.current_attempt,
                self.config.max_retries,
                self.total_delay.as_secs_f64()
            )
        }
    }
}

/// Helper to execute a future with retry logic
pub async fn with_retry<F, Fut, T, E>(mut policy: RetryPolicy, mut operation: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    loop {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                let error_kind = ErrorKind::classify(&e.to_string());
                match policy.should_retry(error_kind) {
                    RetryDecision::Retry { delay, .. } => {
                        tokio::time::sleep(delay).await;
                    }
                    RetryDecision::GiveUp { .. } => {
                        return Err(e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_kind_classify_rate_limit() {
        let kind = ErrorKind::classify("Error 429: Rate limit exceeded");
        assert!(matches!(kind, ErrorKind::RateLimited { .. }));

        let kind = ErrorKind::classify("Too many requests, slow down");
        assert!(matches!(kind, ErrorKind::RateLimited { .. }));
    }

    #[test]
    fn numeric_http_status_tokens_retain_their_error_classification() {
        let cases = [
            ("429", ErrorKind::RateLimited { retry_after: None }),
            (
                "HTTP status 429",
                ErrorKind::RateLimited { retry_after: None },
            ),
            (
                "HTTP/1.1 429 Too Many Requests",
                ErrorKind::RateLimited { retry_after: None },
            ),
            ("HTTP 400", ErrorKind::InvalidRequest),
            ("status code 401", ErrorKind::AuthFailure),
            ("error 403", ErrorKind::AuthFailure),
            ("HTTP 500", ErrorKind::Transient),
            ("HTTP/2 503", ErrorKind::Transient),
            ("response status 502", ErrorKind::Transient),
            ("status=503", ErrorKind::Transient),
            ("error code: 504", ErrorKind::Transient),
        ];

        for (message, expected) in cases {
            assert_eq!(
                ErrorKind::classify(message),
                expected,
                "status-bearing message must retain classification: {message}"
            );
        }
    }

    #[test]
    fn incidental_numeric_fields_are_not_http_status_codes() {
        for message in [
            "EOF while parsing a value at line 1 column 429",
            "schema field 400 is absent",
            "account 401 was not found",
            "permission record 403 is stale",
            "batch 500 was rejected",
            "row 502 is invalid",
            "job 503 is pending",
            "column 504 exceeds width",
            "embedded 1429 must not match 429",
            "token 429th is not a status",
            "HTTP 429x is not a status",
        ] {
            let kind = ErrorKind::classify(message);
            assert_eq!(
                kind,
                ErrorKind::Unknown,
                "incidental digits must not classify as an HTTP status: {message}"
            );
            assert!(
                !kind.is_retryable(),
                "incidental digits must never trigger retry: {message}"
            );
        }
    }

    #[test]
    fn test_error_kind_classify_transient() {
        let kind = ErrorKind::classify("503 Service Unavailable");
        assert_eq!(kind, ErrorKind::Transient);

        let kind = ErrorKind::classify(
            "empty_assistant_response: provider completed without text or tool calls",
        );
        assert_eq!(kind, ErrorKind::Transient);

        let kind = ErrorKind::classify("Internal server error");
        assert_eq!(kind, ErrorKind::Transient);

        let kind = ErrorKind::classify("Connection timeout");
        assert_eq!(kind, ErrorKind::Transient);

        let kind = ErrorKind::classify("Failed to send request to OpenAI API: operation timed out");
        assert_eq!(kind, ErrorKind::Transient);

        let kind = ErrorKind::classify("API is overloaded");
        assert_eq!(kind, ErrorKind::Transient);
    }

    #[test]
    fn test_error_kind_classify_context_overflow() {
        // The rejection wording of every provider the clients recognize. A
        // miss here is a session that ends instead of compacting, so each of
        // these is one provider's way of saying the same thing.
        for message in [
            // Anthropic
            "prompt is too long: 213462 tokens > 200000 maximum",
            "input length and `max_tokens` exceed context limit",
            // OpenAI
            "context_length_exceeded",
            "Your input exceeds the context window of this model",
            // OpenRouter
            "This model's maximum context length is 128000 tokens",
            // Google Gemini
            "The input token count exceeds the maximum number of tokens",
            // xAI
            "maximum prompt length is 131072 but request contains 140000 tokens",
            // Groq
            "Please reduce the length of the messages",
            // llama.cpp
            "the request exceeds the available context size",
            // LM Studio
            "is greater than the context length",
            // Generic
            "too many tokens in request",
            "token limit exceeded",
        ] {
            assert_eq!(
                ErrorKind::classify(message),
                ErrorKind::ContextOverflow,
                "{message}"
            );
        }
    }

    #[test]
    fn classify_does_not_read_a_slow_request_as_an_oversized_one() {
        // Compaction rewrites history, so a false overflow costs real context.
        // A bare "too long" used to be enough to trigger one, which made a
        // report that something took too long look like an oversized request.
        for message in [
            "the operation took too long to complete",
            "the request is taking too long",
        ] {
            assert_ne!(
                ErrorKind::classify(message),
                ErrorKind::ContextOverflow,
                "{message}"
            );
        }
        // Timeouts the transient vocabulary does name stay retryable.
        for message in ["request timed out", "504 gateway timeout"] {
            assert_eq!(
                ErrorKind::classify(message),
                ErrorKind::Transient,
                "{message}"
            );
        }
    }

    #[test]
    fn test_error_kind_classify_auth() {
        let kind = ErrorKind::classify("401 Unauthorized");
        assert_eq!(kind, ErrorKind::AuthFailure);

        let kind = ErrorKind::classify("Invalid API key provided");
        assert_eq!(kind, ErrorKind::AuthFailure);
    }

    #[test]
    fn test_error_kind_classify_invalid_request() {
        let kind = ErrorKind::classify("400 Bad Request");
        assert_eq!(kind, ErrorKind::InvalidRequest);

        let kind = ErrorKind::classify("Malformed JSON in request");
        assert_eq!(kind, ErrorKind::InvalidRequest);
    }

    #[test]
    fn test_error_kind_is_retryable() {
        assert!(ErrorKind::Transient.is_retryable());
        assert!(ErrorKind::RateLimited { retry_after: None }.is_retryable());
        assert!(!ErrorKind::ContextOverflow.is_retryable());
        assert!(!ErrorKind::AuthFailure.is_retryable());
        assert!(!ErrorKind::InvalidRequest.is_retryable());
        assert!(!ErrorKind::QuotaExceeded.is_retryable());
        assert!(!ErrorKind::Unknown.is_retryable());
    }

    #[test]
    fn insufficient_quota_is_terminal_even_with_http_429_and_retry_copy() {
        for message in [
            "HTTP 429: insufficient_quota: Check your plan and billing details, then try again.",
            "HTTP 402: prepaid_credits_insufficient: Add model credits to continue.",
        ] {
            let kind = ErrorKind::classify(message);
            assert_eq!(kind, ErrorKind::QuotaExceeded, "{message}");
            assert!(matches!(
                RetryPolicy::default().should_retry(kind),
                RetryDecision::GiveUp { .. }
            ));
        }
        assert!(matches!(
            ErrorKind::classify("HTTP 429: rate_limit_exceeded: retry after 2 seconds"),
            ErrorKind::RateLimited { .. }
        ));
    }

    #[test]
    fn usage_limit_copy_is_quota_not_transient_try_again() {
        let kind = ErrorKind::classify(
            "You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again later.",
        );
        assert_eq!(kind, ErrorKind::QuotaExceeded);
        assert!(!kind.is_retryable());

        let kind = ErrorKind::classify("Codex turn failed (usageLimitExceeded)");
        assert_eq!(kind, ErrorKind::QuotaExceeded);
    }

    #[test]
    fn test_extract_retry_after_seconds() {
        let kind = ErrorKind::classify("Rate limit exceeded, retry after 30 seconds");
        if let ErrorKind::RateLimited { retry_after } = kind {
            assert_eq!(retry_after, Some(Duration::from_secs(30)));
        } else {
            panic!("Expected RateLimited");
        }
    }

    #[test]
    fn test_extract_retry_after_minutes() {
        let kind = ErrorKind::classify("Rate limit exceeded, please wait 2 minutes");
        if let ErrorKind::RateLimited { retry_after } = kind {
            assert_eq!(retry_after, Some(Duration::from_mins(2)));
        } else {
            panic!("Expected RateLimited");
        }
    }

    #[test]
    fn oversized_retry_after_does_not_panic() {
        // 2^64 seconds is the first whole-second value Duration cannot hold.
        let error = "rate limit; retry after 18446744073709551616 seconds";
        let kind = ErrorKind::classify(error);
        let mut policy = RetryPolicy::new(RetryConfig {
            jitter_factor: 0.0,
            ..RetryConfig::default()
        });
        assert!(matches!(
            policy.should_retry(kind),
            RetryDecision::Retry { delay, .. } if delay <= Duration::from_mins(1)
        ));
    }

    #[test]
    fn generated_retry_after_magnitudes_stay_bounded() {
        for digits in 1..=400 {
            let error = format!("rate limit; retry after {} seconds", "9".repeat(digits));
            let kind = ErrorKind::classify(&error);
            let mut policy = RetryPolicy::new(RetryConfig {
                jitter_factor: 0.0,
                ..RetryConfig::default()
            });
            assert!(
                matches!(
                    policy.should_retry(kind),
                    RetryDecision::Retry { delay, attempt: 1, .. } if delay <= Duration::from_mins(1)
                ),
                "digits={digits}"
            );
        }
    }

    #[test]
    fn test_retry_policy_exhausted() {
        let config = RetryConfig {
            max_retries: 2,
            ..Default::default()
        };
        let mut policy = RetryPolicy::new(config);

        // First retry
        let decision = policy.should_retry(ErrorKind::Transient);
        assert!(matches!(decision, RetryDecision::Retry { attempt: 1, .. }));

        // Second retry
        let decision = policy.should_retry(ErrorKind::Transient);
        assert!(matches!(decision, RetryDecision::Retry { attempt: 2, .. }));

        // Should give up now
        let decision = policy.should_retry(ErrorKind::Transient);
        assert!(matches!(decision, RetryDecision::GiveUp { .. }));
    }

    #[test]
    fn test_retry_policy_non_retryable() {
        let mut policy = RetryPolicy::default();

        // Auth failure should not retry
        let decision = policy.should_retry(ErrorKind::AuthFailure);
        assert!(matches!(decision, RetryDecision::GiveUp { .. }));

        // Context overflow should not retry
        let decision = policy.should_retry(ErrorKind::ContextOverflow);
        assert!(matches!(decision, RetryDecision::GiveUp { .. }));

        let decision = policy.should_retry(ErrorKind::QuotaExceeded);
        assert!(matches!(
            decision,
            RetryDecision::GiveUp { reason } if reason.contains("usage limit")
        ));
    }

    #[test]
    fn test_retry_policy_reset() {
        let config = RetryConfig {
            max_retries: 1,
            ..Default::default()
        };
        let mut policy = RetryPolicy::new(config);

        // Use up the retry
        let _ = policy.should_retry(ErrorKind::Transient);
        let decision = policy.should_retry(ErrorKind::Transient);
        assert!(matches!(decision, RetryDecision::GiveUp { .. }));

        // Reset
        policy.reset();

        // Should be able to retry again
        let decision = policy.should_retry(ErrorKind::Transient);
        assert!(matches!(decision, RetryDecision::Retry { .. }));
    }

    #[test]
    fn test_retry_policy_respects_retry_after() {
        let config = RetryConfig {
            respect_retry_after: true,
            max_delay: Duration::from_mins(2),
            ..Default::default()
        };
        let mut policy = RetryPolicy::new(config);

        let error_kind = ErrorKind::RateLimited {
            retry_after: Some(Duration::from_secs(45)),
        };
        let decision = policy.should_retry(error_kind);

        if let RetryDecision::Retry { delay, .. } = decision {
            // Should use the suggested 45 seconds (not exponential backoff)
            assert_eq!(delay, Duration::from_secs(45));
        } else {
            panic!("Expected Retry decision");
        }
    }

    #[test]
    fn test_retry_policy_caps_retry_after() {
        let config = RetryConfig {
            respect_retry_after: true,
            max_delay: Duration::from_secs(30),
            ..Default::default()
        };
        let mut policy = RetryPolicy::new(config);

        let error_kind = ErrorKind::RateLimited {
            retry_after: Some(Duration::from_mins(2)),
        };
        let decision = policy.should_retry(error_kind);

        if let RetryDecision::Retry { delay, .. } = decision {
            // Should cap at max_delay (30s) even though retry-after said 120s
            assert_eq!(delay, Duration::from_secs(30));
        } else {
            panic!("Expected Retry decision");
        }
    }

    #[test]
    fn test_retry_policy_exponential_backoff() {
        let config = RetryConfig {
            max_retries: 5,
            initial_delay: Duration::from_secs(1),
            backoff_multiplier: 2.0,
            jitter_factor: 0.0, // No jitter for deterministic test
            max_delay: Duration::from_mins(1),
            respect_retry_after: true,
        };
        let mut policy = RetryPolicy::new(config);

        // First retry: 1s
        if let RetryDecision::Retry { delay, .. } = policy.should_retry(ErrorKind::Transient) {
            assert_eq!(delay, Duration::from_secs(1));
        }

        // Second retry: 2s
        if let RetryDecision::Retry { delay, .. } = policy.should_retry(ErrorKind::Transient) {
            assert_eq!(delay, Duration::from_secs(2));
        }

        // Third retry: 4s
        if let RetryDecision::Retry { delay, .. } = policy.should_retry(ErrorKind::Transient) {
            assert_eq!(delay, Duration::from_secs(4));
        }
    }

    #[test]
    fn test_retry_config_presets() {
        let hosted_outage = RetryConfig::hosted_outage();
        assert_eq!(hosted_outage.max_retries, 8);
        assert_eq!(hosted_outage.initial_delay, Duration::from_secs(1));
        assert_eq!(hosted_outage.max_delay, Duration::from_secs(30));

        let aggressive = RetryConfig::aggressive();
        assert_eq!(aggressive.max_retries, 5);
        assert!(aggressive.initial_delay < Duration::from_secs(1));

        let conservative = RetryConfig::conservative();
        assert_eq!(conservative.max_retries, 2);
        assert!(conservative.initial_delay >= Duration::from_secs(2));

        let no_retry = RetryConfig::no_retry();
        assert_eq!(no_retry.max_retries, 0);
    }

    #[test]
    fn hosted_outage_retry_budget_is_bounded() {
        let mut policy = RetryPolicy::new(RetryConfig::hosted_outage());
        for expected_attempt in 1..=8 {
            assert!(matches!(
                policy.should_retry(ErrorKind::Transient),
                RetryDecision::Retry { attempt, .. } if attempt == expected_attempt
            ));
        }
        assert!(matches!(
            policy.should_retry(ErrorKind::Transient),
            RetryDecision::GiveUp { .. }
        ));
    }

    #[test]
    fn mixed_failure_histories_keep_one_monotonic_retry_budget() {
        // Enumerate every five-event history over transport failure, 429,
        // context overflow, malformed request, and authentication failure.
        // The oracle tracks only retries admitted by this operation; a
        // terminal class ends the history rather than opening a new turn.
        const CLASSES: usize = 5;
        const LENGTH: u32 = 5;
        for encoded in 0..CLASSES.pow(LENGTH) {
            let mut history = encoded;
            let mut expected_retries = 0;
            let mut expected_delay = Duration::ZERO;
            let mut policy = RetryPolicy::new(RetryConfig {
                max_retries: 3,
                initial_delay: Duration::from_secs(1),
                max_delay: Duration::from_secs(30),
                jitter_factor: 0.0,
                ..RetryConfig::default()
            });
            for _ in 0..LENGTH {
                let class = history % CLASSES;
                history /= CLASSES;
                let error = match class {
                    0 => ErrorKind::Transient,
                    1 => ErrorKind::RateLimited {
                        retry_after: Some(Duration::from_secs(45)),
                    },
                    2 => ErrorKind::ContextOverflow,
                    3 => ErrorKind::InvalidRequest,
                    _ => ErrorKind::AuthFailure,
                };
                let decision = policy.should_retry(error);
                if class < 2 && expected_retries < 3 {
                    expected_retries += 1;
                    let RetryDecision::Retry { delay, attempt, .. } = decision else {
                        panic!("retryable event {class} was refused in history {encoded}");
                    };
                    assert_eq!(attempt, expected_retries, "history {encoded}");
                    assert!(delay <= Duration::from_secs(30), "history {encoded}");
                    expected_delay += delay;
                } else {
                    assert!(
                        matches!(decision, RetryDecision::GiveUp { .. }),
                        "terminal event {class} retried in history {encoded}"
                    );
                    break;
                }
                assert_eq!(
                    policy.current_attempt(),
                    expected_retries,
                    "history {encoded}"
                );
                assert_eq!(policy.total_delay(), expected_delay, "history {encoded}");
            }
        }
    }

    #[test]
    fn test_status_message() {
        let mut policy = RetryPolicy::default();
        assert_eq!(policy.status_message(), "Initial request");

        let _ = policy.should_retry(ErrorKind::Transient);
        assert!(policy.status_message().contains("Retry 1/3"));
    }

    #[test]
    fn test_no_retry_config() {
        let mut policy = RetryPolicy::new(RetryConfig::no_retry());

        // Should immediately give up
        let decision = policy.should_retry(ErrorKind::Transient);
        assert!(matches!(decision, RetryDecision::GiveUp { .. }));
    }
}
