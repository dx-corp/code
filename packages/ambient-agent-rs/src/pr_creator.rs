//! PR Creator
//!
//! Handles git operations and creates pull requests on GitHub.
//! Creates branches, commits changes, pushes, and opens PRs.

use crate::types::*;
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, error, info};

/// Configuration for PR creation
#[derive(Debug, Clone)]
pub struct PrCreatorConfig {
    /// GitHub personal access token
    pub token: String,
    /// GitHub API base URL
    pub api_base_url: String,
    /// Git author name
    pub author_name: String,
    /// Git author email
    pub author_email: String,
    /// Request timeout in seconds
    pub request_timeout_secs: u64,
    /// Command timeout in seconds
    pub command_timeout_secs: u64,
}

impl Default for PrCreatorConfig {
    fn default() -> Self {
        Self {
            token: String::new(),
            api_base_url: "https://api.github.com".to_string(),
            author_name: "Ambient Agent".to_string(),
            author_email: "ambient-agent@example.com".to_string(),
            request_timeout_secs: 30,
            command_timeout_secs: 60,
        }
    }
}

/// Result of PR creation
#[derive(Debug, Clone)]
pub struct PrCreationResult {
    pub success: bool,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub branch_name: String,
    pub error: Option<String>,
}

/// Metadata rendered into Maestro-authored git commit trailers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaestroAuthorshipMetadata {
    pub maestro_version: String,
    pub model_identifier: String,
    pub prompt_id: String,
    pub approvals_id: String,
}

/// GitHub PR creation response
#[derive(Debug, Deserialize)]
struct GitHubPrResponse {
    number: u64,
    html_url: String,
}

/// PR Creator handles git operations and GitHub PR creation
pub struct PrCreator {
    config: PrCreatorConfig,
    client: Client,
}

impl PrCreator {
    fn format_pr_body(body: &str) -> String {
        format!(
            "{}\n\n---\n*Created by [Ambient Agent](https://github.com/evalops/maestro)*",
            body
        )
    }

    /// Create a new PR creator
    pub fn new(config: PrCreatorConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .user_agent("ambient-agent/0.1")
            .build()
            .unwrap_or_else(|_| Client::new());

        Self { config, client }
    }

    /// Create a PR for the given changes
    #[allow(clippy::too_many_arguments)]
    pub async fn create_pr(
        &self,
        repo_path: &Path,
        repo_name: &str,
        base_branch: &str,
        title: &str,
        body: &str,
        changes: &[FileChange],
        source_event: &NormalizedEvent,
        authorship: &MaestroAuthorshipMetadata,
    ) -> PrCreationResult {
        // Generate branch name
        let branch_name = self.generate_branch_name(source_event);

        // Step 1: Create and checkout branch
        if let Err(e) = self
            .create_branch(repo_path, &branch_name, base_branch)
            .await
        {
            return PrCreationResult {
                success: false,
                pr_number: None,
                pr_url: None,
                branch_name,
                error: Some(format!("Failed to create branch: {}", e)),
            };
        }

        // Step 2: Write file changes to disk
        if let Err(e) = self.write_changes_to_disk(repo_path, changes).await {
            let _ = self
                .cleanup_branch(repo_path, &branch_name, base_branch)
                .await;
            return PrCreationResult {
                success: false,
                pr_number: None,
                pr_url: None,
                branch_name,
                error: Some(format!("Failed to write changes to disk: {}", e)),
            };
        }

        // Step 3: Stage and commit changes
        if let Err(e) = self
            .commit_changes(repo_path, changes, title, authorship)
            .await
        {
            // Try to cleanup
            let _ = self
                .cleanup_branch(repo_path, &branch_name, base_branch)
                .await;
            return PrCreationResult {
                success: false,
                pr_number: None,
                pr_url: None,
                branch_name,
                error: Some(format!("Failed to commit changes: {}", e)),
            };
        }

        // Step 4: Push branch
        if let Err(e) = self.push_branch(repo_path, &branch_name).await {
            let _ = self
                .cleanup_branch(repo_path, &branch_name, base_branch)
                .await;
            return PrCreationResult {
                success: false,
                pr_number: None,
                pr_url: None,
                branch_name,
                error: Some(format!("Failed to push branch: {}", e)),
            };
        }

        // Step 5: Create PR via GitHub API
        match self
            .create_github_pr(repo_name, &branch_name, base_branch, title, body)
            .await
        {
            Ok((number, url)) => {
                info!("Created PR #{} at {}", number, url);
                PrCreationResult {
                    success: true,
                    pr_number: Some(number),
                    pr_url: Some(url),
                    branch_name,
                    error: None,
                }
            }
            Err(e) => {
                error!("Failed to create PR: {}", e);
                // Clean up local branch on PR creation failure
                let _ = self
                    .cleanup_branch(repo_path, &branch_name, base_branch)
                    .await;
                PrCreationResult {
                    success: false,
                    pr_number: None,
                    pr_url: None,
                    branch_name,
                    error: Some(format!("Failed to create PR: {}", e)),
                }
            }
        }
    }

    /// Generate a branch name from the event
    fn generate_branch_name(&self, event: &NormalizedEvent) -> String {
        let timestamp = Utc::now().format("%Y%m%d%H%M%S");
        let sanitized_title = event
            .title
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == ' ')
            .take(30)
            .collect::<String>()
            .trim()
            .to_lowercase()
            .replace(' ', "-");

        format!("ambient/{}-{}", sanitized_title, timestamp)
    }

    /// Create and checkout a new branch
    async fn create_branch(
        &self,
        repo_path: &Path,
        branch_name: &str,
        base_branch: &str,
    ) -> anyhow::Result<()> {
        // Fetch latest from remote
        self.run_git_command(repo_path, &["fetch", "origin", base_branch])
            .await?;

        // Create branch from origin/base
        self.run_git_command(
            repo_path,
            &[
                "checkout",
                "-b",
                branch_name,
                &format!("origin/{}", base_branch),
            ],
        )
        .await?;

        info!("Created branch {} from {}", branch_name, base_branch);
        Ok(())
    }

    /// Write file changes to disk before staging
    async fn write_changes_to_disk(
        &self,
        repo_path: &Path,
        changes: &[FileChange],
    ) -> anyhow::Result<()> {
        use tokio::fs;

        for change in changes {
            let file_path = repo_path.join(&change.file);

            match change.change_type {
                ChangeType::Delete => {
                    // Delete file if it exists
                    if file_path.exists() {
                        fs::remove_file(&file_path).await?;
                        debug!("Deleted file: {}", change.file);
                    }
                }
                ChangeType::Create | ChangeType::Modify => {
                    // Ensure parent directory exists
                    if let Some(parent) = file_path.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    // Write content to file
                    if let Some(content) = &change.content {
                        fs::write(&file_path, content).await?;
                        debug!("Wrote file: {}", change.file);
                    } else {
                        anyhow::bail!("No content provided for file: {}", change.file);
                    }
                }
                ChangeType::Rename => {
                    // Delete old file if old_path is specified
                    if let Some(old_path) = &change.old_path {
                        let old_file_path = repo_path.join(old_path);
                        if old_file_path.exists() {
                            fs::remove_file(&old_file_path).await?;
                            debug!("Deleted old file for rename: {}", old_path);
                        }
                    }
                    // Write new file
                    if let Some(parent) = file_path.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    if let Some(content) = &change.content {
                        fs::write(&file_path, content).await?;
                        debug!(
                            "Wrote renamed file: {} -> {}",
                            change.old_path.as_deref().unwrap_or("?"),
                            change.file
                        );
                    }
                }
            }
        }

        info!("Wrote {} file changes to disk", changes.len());
        Ok(())
    }

    /// Stage and commit changes
    async fn commit_changes(
        &self,
        repo_path: &Path,
        changes: &[FileChange],
        message: &str,
        authorship: &MaestroAuthorshipMetadata,
    ) -> anyhow::Result<()> {
        // Stage all changed files
        for change in changes {
            match change.change_type {
                ChangeType::Delete => {
                    self.run_git_command(repo_path, &["rm", "--force", &change.file])
                        .await
                        .ok(); // Ignore errors for delete
                }
                ChangeType::Rename => {
                    // Remove old file from git if old_path is specified
                    if let Some(old_path) = &change.old_path {
                        self.run_git_command(repo_path, &["rm", "--force", old_path])
                            .await
                            .ok(); // Ignore errors if old file not tracked
                    }
                    // Add new file
                    self.run_git_command(repo_path, &["add", &change.file])
                        .await?;
                }
                _ => {
                    self.run_git_command(repo_path, &["add", &change.file])
                        .await?;
                }
            }
        }

        // Check if there are staged changes
        let status = self
            .run_git_command(repo_path, &["status", "--porcelain"])
            .await?;
        if status.trim().is_empty() {
            anyhow::bail!("No changes to commit");
        }

        // Commit with configured author
        let author = format!("{} <{}>", self.config.author_name, self.config.author_email);
        let commit_message = Self::commit_message_with_authorship(message, authorship)?;
        self.run_git_command(
            repo_path,
            &["commit", "-m", &commit_message, "--author", &author],
        )
        .await?;

        info!("Committed {} changes", changes.len());
        Ok(())
    }

    /// Push branch to remote
    async fn push_branch(&self, repo_path: &Path, branch_name: &str) -> anyhow::Result<()> {
        self.run_git_command(repo_path, &["push", "-u", "origin", branch_name])
            .await?;

        info!("Pushed branch {}", branch_name);
        Ok(())
    }

    /// Create PR via GitHub API
    async fn create_github_pr(
        &self,
        repo_name: &str,
        head_branch: &str,
        base_branch: &str,
        title: &str,
        body: &str,
    ) -> anyhow::Result<(u64, String)> {
        let url = format!("{}/repos/{}/pulls", self.config.api_base_url, repo_name);

        let pr_body = Self::format_pr_body(body);

        let payload = serde_json::json!({
            "title": title,
            "body": pr_body,
            "head": head_branch,
            "base": base_branch,
            "draft": false
        });

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API error {}: {}", status, body);
        }

        let pr: GitHubPrResponse = response.json().await?;
        Ok((pr.number, pr.html_url))
    }

    /// Cleanup branch on failure
    async fn cleanup_branch(
        &self,
        repo_path: &Path,
        branch_name: &str,
        base_branch: &str,
    ) -> anyhow::Result<()> {
        // Checkout origin/base_branch (detached HEAD) since local base may not exist
        let origin_base = format!("origin/{}", base_branch);
        let _ = self
            .run_git_command(repo_path, &["checkout", &origin_base])
            .await;

        // Delete the failed branch
        let _ = self
            .run_git_command(repo_path, &["branch", "-D", branch_name])
            .await;

        debug!("Cleaned up branch {}", branch_name);
        Ok(())
    }

    /// Run a git command with timeout
    async fn run_git_command(&self, repo_path: &Path, args: &[&str]) -> anyhow::Result<String> {
        let timeout_duration = Duration::from_secs(self.config.command_timeout_secs);

        let output_result = tokio::time::timeout(
            timeout_duration,
            Command::new("git")
                .args(args)
                .current_dir(repo_path)
                .output(),
        )
        .await;

        let output = match output_result {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => anyhow::bail!("Git command failed: {}", e),
            Err(_) => anyhow::bail!(
                "Git command timed out after {} seconds",
                self.config.command_timeout_secs
            ),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Git command failed: {}", stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Add a comment to a PR
    pub async fn add_pr_comment(
        &self,
        repo_name: &str,
        pr_number: u64,
        comment: &str,
    ) -> anyhow::Result<()> {
        let url = format!(
            "{}/repos/{}/issues/{}/comments",
            self.config.api_base_url, repo_name, pr_number
        );

        let payload = serde_json::json!({
            "body": comment
        });

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API error {}: {}", status, body);
        }

        Ok(())
    }

    /// Close a PR
    pub async fn close_pr(&self, repo_name: &str, pr_number: u64) -> anyhow::Result<()> {
        let url = format!(
            "{}/repos/{}/pulls/{}",
            self.config.api_base_url, repo_name, pr_number
        );

        let payload = serde_json::json!({
            "state": "closed"
        });

        let response = self
            .client
            .patch(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API error {}: {}", status, body);
        }

        Ok(())
    }

    /// Add labels to a PR
    pub async fn add_labels(
        &self,
        repo_name: &str,
        pr_number: u64,
        labels: &[String],
    ) -> anyhow::Result<()> {
        let url = format!(
            "{}/repos/{}/issues/{}/labels",
            self.config.api_base_url, repo_name, pr_number
        );

        let payload = serde_json::json!({
            "labels": labels
        });

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API error {}: {}", status, body);
        }

        Ok(())
    }

    /// Request reviewers for a PR
    pub async fn request_reviewers(
        &self,
        repo_name: &str,
        pr_number: u64,
        reviewers: &[String],
    ) -> anyhow::Result<()> {
        let url = format!(
            "{}/repos/{}/pulls/{}/requested_reviewers",
            self.config.api_base_url, repo_name, pr_number
        );

        let payload = serde_json::json!({
            "reviewers": reviewers
        });

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API error {}: {}", status, body);
        }

        Ok(())
    }
}

impl PrCreator {
    pub(crate) fn build_authorship_metadata(
        source_event: &NormalizedEvent,
        model_identifier: &str,
    ) -> anyhow::Result<MaestroAuthorshipMetadata> {
        let prompt_id = non_empty_or_error("Maestro-Prompt-Id", &source_event.id)?;
        let model_identifier = non_empty_or_error("model identifier", model_identifier)?;
        let approvals_id = approvals_id_from_event(source_event)
            .unwrap_or_else(|| format!("missing:approval-follow-up-required:{prompt_id}"));

        Ok(MaestroAuthorshipMetadata {
            maestro_version: env!("CARGO_PKG_VERSION").to_string(),
            model_identifier: model_identifier.to_string(),
            prompt_id: prompt_id.to_string(),
            approvals_id,
        })
    }

    pub(crate) fn commit_message_with_authorship(
        message: &str,
        authorship: &MaestroAuthorshipMetadata,
    ) -> anyhow::Result<String> {
        let message = non_empty_or_error("commit message", message)?;
        let maestro_version = non_empty_or_error("Maestro-Version", &authorship.maestro_version)?;
        let model_identifier =
            non_empty_or_error("model identifier", &authorship.model_identifier)?;
        let prompt_id = non_empty_or_error("Maestro-Prompt-Id", &authorship.prompt_id)?;
        let approvals_id = non_empty_or_error("Maestro-Approvals-Id", &authorship.approvals_id)?;

        Ok(format!(
            "{message}\n\nCo-Authored-By: Maestro <maestro@evalops.dev>\nMaestro-Version: {maestro_version} / {model_identifier}\nMaestro-Prompt-Id: {prompt_id}\nMaestro-Approvals-Id: {approvals_id}"
        ))
    }
}

fn non_empty_or_error<'a>(field: &str, value: &'a str) -> anyhow::Result<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("{field} is required for Maestro commit authorship trailers");
    }
    Ok(trimmed)
}

fn approvals_id_from_event(event: &NormalizedEvent) -> Option<String> {
    for key in ["approvals_id", "approval_id", "maestro_approvals_id"] {
        if let Some(value) = event
            .payload
            .extra
            .get(key)
            .and_then(|value| value.as_str())
        {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    fn event_with_extra(extra: HashMap<String, serde_json::Value>) -> NormalizedEvent {
        let repo = Repository {
            owner: "evalops".to_string(),
            name: "platform".to_string(),
            full_name: "example/platform".to_string(),
            default_branch: "main".to_string(),
            path: "/tmp/platform".to_string(),
            url: "https://github.com/example/platform".to_string(),
            config: None,
            agent_md: None,
            test_coverage: None,
            codeowners: vec![],
        };
        NormalizedEvent {
            id: "prompt_123".to_string(),
            source: WatcherType::GitHubWebhook,
            event_type: EventType::Issue,
            repo: repo.clone(),
            repository: repo.full_name.clone(),
            priority: 5,
            title: "Ship provenance".to_string(),
            body: None,
            labels: vec![],
            context: EventContext {
                repo,
                history: vec![],
                related: vec![],
            },
            payload: EventPayload {
                extra,
                ..Default::default()
            },
            created_at: Utc::now(),
            processed_at: None,
            status: EventStatus::Pending,
            flags: EventFlags::default(),
        }
    }

    #[test]
    fn renders_maestro_commit_trailers() {
        let metadata = MaestroAuthorshipMetadata {
            maestro_version: "0.1.0".to_string(),
            model_identifier: "anthropic/claude-sonnet".to_string(),
            prompt_id: "prompt_123".to_string(),
            approvals_id: "approval_456".to_string(),
        };

        let message =
            PrCreator::commit_message_with_authorship("feat: add provenance", &metadata).unwrap();

        assert_eq!(
            message,
            "feat: add provenance\n\nCo-Authored-By: Maestro <maestro@evalops.dev>\nMaestro-Version: 0.1.0 / anthropic/claude-sonnet\nMaestro-Prompt-Id: prompt_123\nMaestro-Approvals-Id: approval_456"
        );
    }

    #[test]
    fn fails_loudly_when_required_authorship_fields_are_missing() {
        let metadata = MaestroAuthorshipMetadata {
            maestro_version: "0.1.0".to_string(),
            model_identifier: "".to_string(),
            prompt_id: "prompt_123".to_string(),
            approvals_id: "approval_456".to_string(),
        };

        let err = PrCreator::commit_message_with_authorship("feat: add provenance", &metadata)
            .expect_err("missing model should fail");

        assert!(
            err.to_string().contains("model identifier is required"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn builds_explicit_follow_up_approval_trailer_when_missing() {
        let metadata =
            PrCreator::build_authorship_metadata(&event_with_extra(HashMap::new()), "gpt-5.6")
                .unwrap();

        assert_eq!(metadata.prompt_id, "prompt_123");
        assert_eq!(metadata.model_identifier, "gpt-5.6");
        assert_eq!(
            metadata.approvals_id,
            "missing:approval-follow-up-required:prompt_123"
        );
    }

    #[test]
    fn fallback_approval_trailer_uses_trimmed_prompt_id() {
        let mut event = event_with_extra(HashMap::new());
        event.id = " prompt_123 ".to_string();
        let metadata = PrCreator::build_authorship_metadata(&event, "gpt-5.6").unwrap();

        assert_eq!(metadata.prompt_id, "prompt_123");
        assert_eq!(
            metadata.approvals_id,
            "missing:approval-follow-up-required:prompt_123"
        );
    }

    #[test]
    fn uses_approval_identifier_from_event_payload() {
        let mut extra = HashMap::new();
        extra.insert(
            "approvals_id".to_string(),
            serde_json::Value::String("approval_789".to_string()),
        );

        let metadata =
            PrCreator::build_authorship_metadata(&event_with_extra(extra), "gpt-5.6").unwrap();

        assert_eq!(metadata.approvals_id, "approval_789");
    }
    #[test]
    fn test_generate_branch_name() {
        let config = PrCreatorConfig::default();
        let creator = PrCreator::new(config);

        let event = NormalizedEvent {
            id: "test".to_string(),
            source: WatcherType::GitHubPoll,
            event_type: EventType::Issue,
            repo: Repository {
                owner: "test".to_string(),
                name: "repo".to_string(),
                full_name: "test/repo".to_string(),
                default_branch: "main".to_string(),
                path: "/tmp/test".to_string(),
                url: "https://github.com/test/repo".to_string(),
                config: None,
                agent_md: None,
                test_coverage: None,
                codeowners: vec![],
            },
            repository: "test/repo".to_string(),
            priority: 50,
            title: "Fix the bug in login!".to_string(),
            body: None,
            labels: vec![],
            context: EventContext {
                repo: Repository {
                    owner: "test".to_string(),
                    name: "repo".to_string(),
                    full_name: "test/repo".to_string(),
                    default_branch: "main".to_string(),
                    path: "/tmp/test".to_string(),
                    url: "https://github.com/test/repo".to_string(),
                    config: None,
                    agent_md: None,
                    test_coverage: None,
                    codeowners: vec![],
                },
                history: vec![],
                related: vec![],
            },
            payload: EventPayload::default(),
            created_at: Utc::now(),
            processed_at: None,
            status: EventStatus::Pending,
            flags: EventFlags::default(),
        };

        let branch_name = creator.generate_branch_name(&event);
        assert!(branch_name.starts_with("ambient/fix-the-bug-in-login"));
    }

    #[test]
    fn test_format_pr_body_uses_maestro_repo_link() {
        let body = PrCreator::format_pr_body("Implemented fixes");
        assert!(body.contains("Created by [Ambient Agent]"));
        assert!(body.contains("https://github.com/evalops/maestro"));
        assert!(!body.contains("https://github.com/evalops/composer"));
    }
}
