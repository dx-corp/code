//! Executor
//!
//! Executes tasks by calling LLMs and applying file changes.
//! Handles API calls, response parsing, and file operations.

use crate::cascader::RoutingResult;
#[cfg(test)]
use crate::file_permission::FilePermissionRule;
use crate::file_permission::{
    FilePermissionDecision, FilePermissionEvaluation, FilePermissionPolicy,
};
use crate::prompt::{PromptBuilder, PromptBundle, PromptFileContext};
use crate::types::*;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

/// Static regex patterns to avoid recompilation in hot path
static FILE_CHANGE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)<file_change>\s*<action>(\w+)</action>\s*<path>([^<]+)</path>(?:\s*<content>(.*?)</content>)?\s*</file_change>"
    ).unwrap()
});

static MARKDOWN_FILE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```(?:\w+)?\n// File: ([^\n]+)\n(.*?)```").unwrap());

/// Allowed test commands (whitelist for security)
static ALLOWED_TEST_COMMANDS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        "npm", "yarn", "pnpm", "bun", "cargo", "go", "pytest", "python", "ruby", "rspec", "bundle",
        "make", "gradle", "mvn",
    ]
});

/// Configuration for the executor
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    pub api_key: String,
    pub api_base_url: String,
    pub api_provider: LlmApiProvider,
    pub max_tokens: u32,
    pub temperature: f64,
    pub run_tests: bool,
    pub test_command: Option<String>,
    pub working_dir: String,
    pub request_timeout_secs: u64,
    pub test_timeout_secs: u64,
    pub max_retries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmApiProvider {
    AnthropicMessages,
    OpenRouterChatCompletions,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_base_url: "https://openrouter.ai/api/v1".to_string(),
            api_provider: LlmApiProvider::OpenRouterChatCompletions,
            max_tokens: 4096,
            temperature: 0.0,
            run_tests: true,
            test_command: None,
            working_dir: ".".to_string(),
            request_timeout_secs: 300, // 5 minutes for LLM calls
            test_timeout_secs: 120,    // 2 minutes for tests
            max_retries: 3,
        }
    }
}

impl ExecutorConfig {
    pub fn from_env(working_dir: impl Into<String>) -> Self {
        let working_dir = working_dir.into();
        let api_provider = match std::env::var("MAESTRO_AMBIENT_LLM_API").ok().as_deref() {
            Some("anthropic") | Some("anthropic-messages") => LlmApiProvider::AnthropicMessages,
            _ => LlmApiProvider::OpenRouterChatCompletions,
        };

        match api_provider {
            LlmApiProvider::OpenRouterChatCompletions => Self {
                api_key: std::env::var("OPENROUTER_API_KEY")
                    .or_else(|_| std::env::var("MAESTRO_OPENROUTER_API_KEY"))
                    .unwrap_or_default(),
                api_base_url: std::env::var("OPENROUTER_BASE_URL")
                    .or_else(|_| std::env::var("MAESTRO_OPENROUTER_BASE_URL"))
                    .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string()),
                api_provider,
                working_dir,
                ..Default::default()
            },
            LlmApiProvider::AnthropicMessages => Self {
                api_key: std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
                api_base_url: std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string()),
                api_provider,
                working_dir,
                ..Default::default()
            },
        }
    }
}

/// Executor handles LLM calls and file operations
pub struct Executor {
    config: ExecutorConfig,
    client: Client,
    file_permission_policy: FilePermissionPolicy,
}

/// Anthropic API request
#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    messages: Vec<Message>,
    system: String,
}

#[derive(Debug, Serialize)]
struct OpenRouterChatRequest {
    model: String,
    max_tokens: u32,
    temperature: f64,
    messages: Vec<Message>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterChatResponse {
    choices: Vec<OpenRouterChoice>,
    usage: Option<OpenRouterUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterChoice {
    message: OpenRouterMessage,
}

#[derive(Debug, Deserialize)]
struct OpenRouterMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

/// Anthropic API response
#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    #[allow(dead_code)] // Required for deserialization but not read directly
    content_type: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
}

/// Parsed file change from LLM response
#[derive(Debug, Clone)]
struct ParsedChange {
    file_path: String,
    action: String, // "create", "modify", "delete"
    content: Option<String>,
}

impl Executor {
    /// Create a new executor with configured HTTP client
    pub fn new(config: ExecutorConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .pool_max_idle_per_host(5)
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            config,
            client,
            file_permission_policy: FilePermissionPolicy::default_write_policy(),
        }
    }

    /// Override file write permissions for executor-applied changes.
    ///
    /// The executor is currently non-interactive, so Ask and Deny decisions both
    /// stop writes. Keeping Ask distinct gives callers a stable hook for future
    /// approval plumbing without changing this execution path.
    pub fn with_file_permission_policy(mut self, policy: FilePermissionPolicy) -> Self {
        self.file_permission_policy = policy;
        self
    }

    /// Execute a task plan using the routed model
    pub async fn execute(&self, plan: &TaskPlan, routing: &RoutingResult) -> ExecutionResult {
        let mut logs = vec![];
        logs.push(format!("Executing: {}", plan.summary));
        logs.push(format!(
            "Using model: {} ({})",
            routing.model, routing.tier.name
        ));

        // Build the prompt
        let prompts = self.build_prompts(plan).await;
        logs.push("Built prompts".to_string());

        // Call the LLM with retries
        let response = match self
            .call_llm_with_retry(&routing.model, &prompts.system, &prompts.user)
            .await
        {
            Ok(resp) => {
                logs.push(format!(
                    "LLM response received ({} input, {} output tokens)",
                    resp.usage.input_tokens, resp.usage.output_tokens
                ));
                resp
            }
            Err(e) => {
                error!("LLM call failed after retries: {}", e);
                return ExecutionResult {
                    status: ExecutionStatus::Failed,
                    changes: vec![],
                    test_results: vec![],
                    error: Some(format!("LLM call failed: {}", e)),
                    logs,
                };
            }
        };

        // Parse the response
        let llm_output = response
            .content
            .iter()
            .filter_map(|c| c.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n");

        let parsed_changes = match self.parse_response(&llm_output) {
            Ok(changes) => changes,
            Err(e) => {
                error!("Failed to parse LLM response: {}", e);
                return ExecutionResult {
                    status: ExecutionStatus::Failed,
                    changes: vec![],
                    test_results: vec![],
                    error: Some(format!("Parse error: {}", e)),
                    logs,
                };
            }
        };
        logs.push(format!("Parsed {} file changes", parsed_changes.len()));

        if parsed_changes.is_empty() {
            warn!("No file changes parsed from LLM response");
            return ExecutionResult {
                status: ExecutionStatus::Partial,
                changes: vec![],
                test_results: vec![],
                error: Some("No file changes found in LLM response".to_string()),
                logs,
            };
        }

        // Apply the changes
        let mut changes = vec![];
        let mut errors = vec![];

        for parsed in &parsed_changes {
            match self.apply_change(parsed).await {
                Ok(change) => {
                    logs.push(format!("Applied: {} {}", parsed.action, parsed.file_path));
                    changes.push(change);
                }
                Err(e) => {
                    let err_msg = format!(
                        "Failed to apply {} {}: {}",
                        parsed.action, parsed.file_path, e
                    );
                    error!("{}", err_msg);
                    errors.push(err_msg.clone());
                    logs.push(err_msg);
                }
            }
        }

        // Run tests if configured
        let test_results = if self.config.run_tests {
            match self.run_tests().await {
                Ok(results) => {
                    logs.push(format!("Ran {} tests", results.len()));
                    results
                }
                Err(e) => {
                    logs.push(format!("Test execution failed: {}", e));
                    vec![]
                }
            }
        } else {
            vec![]
        };

        // Determine status
        let status = if !errors.is_empty() {
            ExecutionStatus::Partial
        } else if test_results.iter().any(|t| !t.passed) {
            ExecutionStatus::Failed
        } else {
            ExecutionStatus::Success
        };

        ExecutionResult {
            status,
            changes,
            test_results,
            error: if errors.is_empty() {
                None
            } else {
                Some(errors.join("; "))
            },
            logs,
        }
    }

    /// Build system and user prompts for the LLM.
    async fn build_prompts(&self, plan: &TaskPlan) -> PromptBundle {
        let prompt_builder = PromptBuilder::new();
        let mut file_contexts = Vec::with_capacity(plan.files.len());

        for file in &plan.files {
            if let Ok(content) = self.read_file_context(file).await {
                file_contexts.push(PromptFileContext::new(file, content));
            }
        }

        prompt_builder.build(plan, &file_contexts)
    }

    /// Read file content for prompt context.
    async fn read_file_context(&self, path: &str) -> anyhow::Result<String> {
        // Validate path before reading
        let full_path = self.validate_path(path)?;
        Ok(fs::read_to_string(&full_path).await?)
    }

    /// Validate that a path is safe and within the working directory
    fn validate_path(&self, path: &str) -> anyhow::Result<PathBuf> {
        // Reject absolute paths
        if path.starts_with('/') || path.starts_with('\\') {
            anyhow::bail!("Absolute paths are not allowed: {}", path);
        }

        // Reject paths with .. components
        if path.contains("..") {
            anyhow::bail!("Path traversal detected: {}", path);
        }

        // Reject paths with null bytes
        if path.contains('\0') {
            anyhow::bail!("Invalid path (contains null byte): {}", path);
        }

        let working_dir = dunce::canonicalize(Path::new(&self.config.working_dir))
            .unwrap_or_else(|_| PathBuf::from(&self.config.working_dir));

        let full_path = working_dir.join(path);

        // For existing files, verify they're within working dir
        if full_path.exists() {
            let canonical = dunce::canonicalize(&full_path)?;
            if !canonical.starts_with(&working_dir) {
                anyhow::bail!(
                    "Path escapes working directory: {} -> {}",
                    path,
                    canonical.display()
                );
            }
            return Ok(canonical);
        }

        // For new files, verify the path components don't escape
        // by checking each component. Split on both / and \ for cross-platform safety.
        let normalized: PathBuf = path
            .split(['/', '\\'])
            .filter(|c| !c.is_empty() && *c != ".")
            .collect();
        let final_path = working_dir.join(&normalized);

        // Double-check that the final path is still within working_dir
        // by comparing path prefixes
        let final_str = final_path.to_string_lossy();
        let working_str = working_dir.to_string_lossy();
        if !final_str.starts_with(working_str.as_ref()) {
            anyhow::bail!("Path escapes working directory: {}", path);
        }

        Ok(final_path)
    }

    /// Evaluate write permission for an executor-applied file path.
    pub fn evaluate_file_permission(&self, path: &str) -> FilePermissionEvaluation {
        self.file_permission_policy.evaluate(path)
    }

    /// Whether the executor has an API key configured for LLM calls.
    pub fn has_api_key(&self) -> bool {
        !self.config.api_key.is_empty()
    }

    /// Call the configured LLM with retry and return the raw text content.
    ///
    /// Used by auxiliary judges (e.g. the goal evaluator) that need a plain
    /// completion rather than executor file changes.
    pub async fn chat_completion_text(
        &self,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> anyhow::Result<String> {
        let response = self
            .call_llm_with_retry(model, system_prompt, user_prompt)
            .await?;
        Ok(response
            .content
            .iter()
            .filter_map(|c| c.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Call the LLM with retry logic and exponential backoff
    async fn call_llm_with_retry(
        &self,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> anyhow::Result<AnthropicResponse> {
        let mut last_error = None;
        let mut delay = Duration::from_secs(1);

        for attempt in 0..self.config.max_retries {
            if attempt > 0 {
                info!(
                    "Retrying LLM call (attempt {}/{})",
                    attempt + 1,
                    self.config.max_retries
                );
                tokio::time::sleep(delay).await;
                delay *= 2; // Exponential backoff
            }

            match self.call_llm(model, system_prompt, user_prompt).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    let err_str = e.to_string();
                    // Don't retry on client errors (4xx) except rate limits (429)
                    if err_str.contains("API error 4") && !err_str.contains("429") {
                        return Err(e);
                    }
                    warn!("LLM call attempt {} failed: {}", attempt + 1, e);
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("LLM call failed after retries")))
    }

    /// Call the configured LLM API
    async fn call_llm(
        &self,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> anyhow::Result<AnthropicResponse> {
        match self.config.api_provider {
            LlmApiProvider::AnthropicMessages => {
                self.call_anthropic_messages(model, system_prompt, user_prompt)
                    .await
            }
            LlmApiProvider::OpenRouterChatCompletions => {
                self.call_openrouter_chat_completions(model, system_prompt, user_prompt)
                    .await
            }
        }
    }

    /// Call the Anthropic Messages API
    async fn call_anthropic_messages(
        &self,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> anyhow::Result<AnthropicResponse> {
        let request = AnthropicRequest {
            model: model.to_string(),
            max_tokens: self.config.max_tokens,
            temperature: supports_anthropic_temperature(model).then_some(self.config.temperature),
            system: system_prompt.to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: user_prompt.to_string(),
            }],
        };

        let response = self
            .client
            .post(format!("{}/messages", self.config.api_base_url))
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("API error {}: {}", status, body);
        }

        let result: AnthropicResponse = response.json().await?;
        Ok(result)
    }

    /// Call the OpenRouter OpenAI-compatible chat completions API.
    async fn call_openrouter_chat_completions(
        &self,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> anyhow::Result<AnthropicResponse> {
        let request = OpenRouterChatRequest {
            model: model.to_string(),
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: system_prompt.to_string(),
                },
                Message {
                    role: "user".to_string(),
                    content: user_prompt.to_string(),
                },
            ],
        };

        let response = self
            .client
            .post(format!("{}/chat/completions", self.config.api_base_url))
            .bearer_auth(&self.config.api_key)
            .header("content-type", "application/json")
            .header("HTTP-Referer", "https://orb.deixic.com")
            .header("X-OpenRouter-Title", "EvalOps Maestro Ambient Agent")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("API error {}: {}", status, body);
        }

        let result: OpenRouterChatResponse = response.json().await?;
        let text = result
            .choices
            .first()
            .and_then(|choice| choice.message.content.clone())
            .unwrap_or_default();
        let usage = result.usage.unwrap_or(OpenRouterUsage {
            prompt_tokens: Some(0),
            completion_tokens: Some(0),
        });

        Ok(AnthropicResponse {
            content: vec![ContentBlock {
                content_type: "text".to_string(),
                text: Some(text),
            }],
            usage: Usage {
                input_tokens: usage.prompt_tokens.unwrap_or(0),
                output_tokens: usage.completion_tokens.unwrap_or(0),
            },
        })
    }

    /// Parse LLM response to extract file changes with validation
    fn parse_response(&self, response: &str) -> anyhow::Result<Vec<ParsedChange>> {
        let mut changes = vec![];
        let valid_actions = ["create", "modify", "delete"];

        // Parse <file_change> blocks using static pattern
        for cap in FILE_CHANGE_PATTERN.captures_iter(response) {
            let action = cap
                .get(1)
                .map(|m| m.as_str().to_lowercase())
                .unwrap_or_default();
            let file_path = cap
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            let content = cap.get(3).map(|m| m.as_str().trim().to_string());

            // Validate action
            if !valid_actions.contains(&action.as_str()) {
                warn!(
                    "Invalid action '{}' for path '{}', skipping",
                    action, file_path
                );
                continue;
            }

            if !file_path.is_empty() {
                changes.push(ParsedChange {
                    file_path,
                    action,
                    content,
                });
            }
        }

        // Fallback: try to parse markdown code blocks with file paths using static pattern
        if changes.is_empty() {
            debug!("No <file_change> blocks found, trying markdown fallback");

            for cap in MARKDOWN_FILE_PATTERN.captures_iter(response) {
                let file_path = cap
                    .get(1)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default();
                let content = cap.get(2).map(|m| m.as_str().to_string());

                if !file_path.is_empty() {
                    changes.push(ParsedChange {
                        file_path,
                        action: "modify".to_string(),
                        content,
                    });
                }
            }
        }

        Ok(changes)
    }

    /// Apply a parsed change to the filesystem with security checks
    async fn apply_change(&self, change: &ParsedChange) -> anyhow::Result<FileChange> {
        // Security: Check write permission before touching the filesystem.
        let file_permission = self.evaluate_file_permission(&change.file_path);
        if file_permission.decision.is_blocking() {
            let decision = match file_permission.decision {
                FilePermissionDecision::Ask => "requires approval",
                FilePermissionDecision::Deny => "is denied",
                FilePermissionDecision::Allow => "is allowed",
            };
            let pattern = file_permission
                .matched_pattern
                .as_deref()
                .map(|pattern| format!(" matched by '{pattern}'"))
                .unwrap_or_default();
            let reason = file_permission
                .reason
                .as_deref()
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default();

            anyhow::bail!(
                "Cannot modify {}: file permission {}{}{}",
                change.file_path,
                decision,
                pattern,
                reason
            );
        }

        // Security: Validate path is within working directory
        let full_path = self.validate_path(&change.file_path)?;

        match change.action.as_str() {
            "create" => {
                // Ensure parent directory exists
                if let Some(parent) = full_path.parent() {
                    fs::create_dir_all(parent).await?;
                }

                let content = change
                    .content
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Create action requires content"))?;

                fs::write(&full_path, content).await?;

                let additions = content.lines().count() as u32;

                Ok(FileChange {
                    file: change.file_path.clone(),
                    change_type: ChangeType::Create,
                    content: Some(content.clone()),
                    old_path: None,
                    additions,
                    deletions: 0,
                })
            }
            "modify" => {
                let content = change
                    .content
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Modify action requires content"))?;

                // Count old lines for diff stats
                let old_lines = if full_path.exists() {
                    fs::read_to_string(&full_path).await?.lines().count() as u32
                } else {
                    0
                };

                fs::write(&full_path, content).await?;

                let new_lines = content.lines().count() as u32;

                Ok(FileChange {
                    file: change.file_path.clone(),
                    change_type: ChangeType::Modify,
                    content: Some(content.clone()),
                    old_path: None,
                    additions: new_lines,
                    deletions: old_lines,
                })
            }
            "delete" => {
                let old_lines = if full_path.exists() {
                    let content = fs::read_to_string(&full_path).await?;
                    fs::remove_file(&full_path).await?;
                    content.lines().count() as u32
                } else {
                    0
                };

                Ok(FileChange {
                    file: change.file_path.clone(),
                    change_type: ChangeType::Delete,
                    content: None,
                    old_path: None,
                    additions: 0,
                    deletions: old_lines,
                })
            }
            _ => anyhow::bail!("Unknown action: {}", change.action),
        }
    }

    /// Run tests and return results with timeout
    async fn run_tests(&self) -> anyhow::Result<Vec<TestResult>> {
        let test_cmd = match &self.config.test_command {
            Some(cmd) => cmd.clone(),
            None => return Ok(vec![]), // No test command configured
        };

        let parts: Vec<&str> = test_cmd.split_whitespace().collect();

        if parts.is_empty() {
            return Ok(vec![]);
        }

        // Security: Validate test command against whitelist using basename only
        let cmd = parts[0];
        let cmd_basename = Path::new(cmd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(cmd);
        if !ALLOWED_TEST_COMMANDS.contains(&cmd_basename) {
            anyhow::bail!(
                "Test command '{}' not in allowed list. Allowed: {:?}",
                cmd,
                *ALLOWED_TEST_COMMANDS
            );
        }

        // Run with timeout
        let test_timeout = Duration::from_secs(self.config.test_timeout_secs);

        let output_result = timeout(
            test_timeout,
            Command::new(parts[0])
                .args(&parts[1..])
                .current_dir(&self.config.working_dir)
                .kill_on_drop(true)
                .output(),
        )
        .await;

        let output = match output_result {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => anyhow::bail!("Failed to execute test command: {}", e),
            Err(_) => anyhow::bail!(
                "Test command timed out after {} seconds",
                self.config.test_timeout_secs
            ),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Simple test result - real implementation would parse test output
        let passed = output.status.success();

        Ok(vec![TestResult {
            name: "test-suite".to_string(),
            passed,
            duration_ms: 0,
            error: if passed {
                None
            } else {
                Some(format!("{}\n{}", stdout, stderr))
            },
        }])
    }
}

/// Whether the direct Anthropic route accepts `temperature` for `model`.
///
/// Delegates to the one rule in `maestro-ai`. This crate kept its own copy,
/// which excluded only the Opus 4 family, so the frontier default
/// (`claude-opus-5-5`) and the Sonnet 5 / Fable 5 tiers were sent
/// `temperature: 0.0`; models.dev lists all of them as rejecting it.
fn supports_anthropic_temperature(model: &str) -> bool {
    let model = model.strip_prefix("~anthropic/").unwrap_or(model);
    maestro_ai::anthropic_request_capabilities(Some("anthropic"), model).temperature
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    #[test]
    fn test_parse_file_change_blocks() {
        let executor = Executor::new(ExecutorConfig::default());

        let response = r#"
Here's the implementation:

<file_change>
<action>create</action>
<path>src/utils/helper.rs</path>
<content>
pub fn greet(name: &str) -> String {
    format!("Hello, {}!", name)
}
</content>
</file_change>

<file_change>
<action>modify</action>
<path>src/main.rs</path>
<content>
mod utils;

fn main() {
    println!("{}", utils::helper::greet("World"));
}
</content>
</file_change>
"#;

        let changes = executor.parse_response(response).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].action, "create");
        assert_eq!(changes[0].file_path, "src/utils/helper.rs");
        assert_eq!(changes[1].action, "modify");
        assert_eq!(changes[1].file_path, "src/main.rs");
    }

    #[test]
    fn test_path_validation_rejects_traversal() {
        let executor = Executor::new(ExecutorConfig::default());

        // Should reject path traversal
        assert!(executor.validate_path("../../../etc/passwd").is_err());
        assert!(executor.validate_path("foo/../../../bar").is_err());

        // Should reject absolute paths
        assert!(executor.validate_path("/etc/passwd").is_err());
        assert!(executor.validate_path("/tmp/evil").is_err());

        // Should accept normal relative paths
        assert!(executor.validate_path("src/main.rs").is_ok());
        assert!(executor.validate_path("lib/utils/helper.rs").is_ok());
    }

    #[test]
    fn test_protected_paths() {
        let executor = Executor::new(ExecutorConfig::default());

        for path in [
            ".git/config",
            ".env",
            ".env.local",
            "config/secrets.json",
            "credentials.yaml",
            "server.key",
            "node_modules/package/index.js",
        ] {
            assert!(
                executor
                    .evaluate_file_permission(path)
                    .decision
                    .is_blocking(),
                "{path} should be blocked"
            );
        }

        for path in ["src/main.rs", "lib/utils.ts"] {
            assert_eq!(
                executor.evaluate_file_permission(path).decision,
                FilePermissionDecision::Allow,
                "{path} should be allowed"
            );
        }
    }

    #[test]
    fn test_file_permission_policy_override_uses_last_match() {
        let executor = Executor::new(ExecutorConfig::default()).with_file_permission_policy(
            FilePermissionPolicy::new(vec![
                FilePermissionRule::new("*", FilePermissionDecision::Allow),
                FilePermissionRule::new("src/generated/**", FilePermissionDecision::Ask),
                FilePermissionRule::new("src/generated/fixtures/**", FilePermissionDecision::Allow),
            ]),
        );

        let generated = executor.evaluate_file_permission("src/generated/client.rs");
        let fixture = executor.evaluate_file_permission("src/generated/fixtures/client.rs");

        assert_eq!(generated.decision, FilePermissionDecision::Ask);
        assert!(generated.decision.is_blocking());
        assert_eq!(fixture.decision, FilePermissionDecision::Allow);
        assert!(!fixture.decision.is_blocking());
    }

    #[test]
    fn anthropic_temperature_support_matches_opus_4_family() {
        for model in [
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5-1",
            "anthropic/claude-opus-5.5",
            "~anthropic/claude-sonnet-5",
        ] {
            assert!(!supports_anthropic_temperature(model), "{model}");
        }
        assert!(!supports_anthropic_temperature("claude-opus-4"));
        assert!(!supports_anthropic_temperature("claude-opus-4-7"));
        assert!(!supports_anthropic_temperature("claude-opus-4.7"));
        assert!(!supports_anthropic_temperature("claude-opus-4-1-20250805"));
        assert!(!supports_anthropic_temperature("anthropic/claude-opus-4-7"));
        assert!(!supports_anthropic_temperature(
            "~anthropic/claude-opus-4-7"
        ));
        assert!(!supports_anthropic_temperature("claude-opus-latest"));
        assert!(supports_anthropic_temperature("claude-3-opus-20240229"));
        assert!(supports_anthropic_temperature("claude-sonnet-4-5"));
        assert!(supports_anthropic_temperature("claude-haiku-3-5"));
    }

    #[tokio::test]
    async fn call_llm_omits_temperature_for_anthropic_opus_4_7() {
        let (api_base_url, request_rx) = spawn_anthropic_messages_fixture();
        let executor = Executor::new(ExecutorConfig {
            api_key: "ant-test-key".to_string(),
            api_base_url,
            api_provider: LlmApiProvider::AnthropicMessages,
            temperature: 0.7,
            max_retries: 1,
            ..ExecutorConfig::default()
        });

        let response = executor
            .call_llm("claude-opus-4-7", "system prompt", "user prompt")
            .await
            .unwrap();

        let request = request_rx.recv().unwrap();
        let lowercase_request = request.to_ascii_lowercase();
        assert!(request.starts_with("POST /messages HTTP/1.1"));
        assert!(lowercase_request.contains("x-api-key: ant-test-key"));
        assert!(request.contains("\"model\":\"claude-opus-4-7\""));
        assert!(request.contains("\"system\":\"system prompt\""));
        assert!(request.contains("\"role\":\"user\""));
        assert!(request.contains("\"content\":\"user prompt\""));
        assert!(!request.contains("\"temperature\""));
        assert_eq!(response.usage.input_tokens, 3);
        assert_eq!(response.usage.output_tokens, 2);
        assert_eq!(response.content[0].text.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn call_llm_keeps_temperature_for_anthropic_sonnet() {
        let (api_base_url, request_rx) = spawn_anthropic_messages_fixture();
        let executor = Executor::new(ExecutorConfig {
            api_key: "ant-test-key".to_string(),
            api_base_url,
            api_provider: LlmApiProvider::AnthropicMessages,
            temperature: 0.7,
            max_retries: 1,
            ..ExecutorConfig::default()
        });

        let response = executor
            .call_llm("claude-sonnet-4-5", "system prompt", "user prompt")
            .await
            .unwrap();

        let request = request_rx.recv().unwrap();
        let lowercase_request = request.to_ascii_lowercase();
        assert!(request.starts_with("POST /messages HTTP/1.1"));
        assert!(lowercase_request.contains("x-api-key: ant-test-key"));
        assert!(request.contains("\"model\":\"claude-sonnet-4-5\""));
        assert!(request.contains("\"temperature\":0.7"));
        assert_eq!(response.usage.input_tokens, 3);
        assert_eq!(response.usage.output_tokens, 2);
    }

    #[tokio::test]
    async fn call_llm_uses_openrouter_chat_completions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            request_tx.send(request).unwrap();
            let body = "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"<file_change><action>modify</action><path>README.md</path><content>ok</content></file_change>\"}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let executor = Executor::new(ExecutorConfig {
            api_key: "or-test-key".to_string(),
            api_base_url: format!("http://{}", addr),
            api_provider: LlmApiProvider::OpenRouterChatCompletions,
            max_retries: 1,
            ..ExecutorConfig::default()
        });

        let response = executor
            .call_llm(
                "~anthropic/claude-opus-latest",
                "system prompt",
                "user prompt",
            )
            .await
            .unwrap();

        let request = request_rx.recv().unwrap();
        assert!(request.starts_with("POST /chat/completions HTTP/1.1"));
        assert!(request.contains("authorization: Bearer or-test-key"));
        assert!(request.contains("\"model\":\"~anthropic/claude-opus-latest\""));
        assert!(request.contains("\"role\":\"system\""));
        assert!(request.contains("\"content\":\"system prompt\""));
        assert!(request.contains("\"role\":\"user\""));
        assert!(request.contains("\"content\":\"user prompt\""));
        assert_eq!(response.usage.input_tokens, 11);
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(
            response.content[0].text.as_deref(),
            Some(
                "<file_change><action>modify</action><path>README.md</path><content>ok</content></file_change>"
            )
        );
    }

    fn spawn_anthropic_messages_fixture() -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            request_tx.send(request).unwrap();
            let body = "{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        (format!("http://{}", addr), request_rx)
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let Some(header_end) = find_header_end(&buffer) else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buffer.len() >= header_end + 4 + content_length {
                break;
            }
        }
        String::from_utf8(buffer).unwrap()
    }

    fn find_header_end(buffer: &[u8]) -> Option<usize> {
        buffer.windows(4).position(|window| window == b"\r\n\r\n")
    }
}
