//! First-party subscription transports. Credentials stay in each provider's CLI.
//! The CLIs have no tools; proposed tool calls re-enter Maestro's native loop.

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};

use super::{
    client::{AiProvider, CancellableStream, provider_model_name},
    types::{ContentBlock, Message, RequestConfig, StopReason, StreamEvent},
};

#[derive(Clone)]
pub struct SubscriptionCliClient {
    provider: &'static str,
}

#[derive(Deserialize)]
struct ProposedAction {
    kind: String,
    answer: String,
    tool_name: String,
    tool_input: Value,
}

impl SubscriptionCliClient {
    pub fn new(provider: &str) -> Result<Self> {
        let provider = match provider {
            "claude-code" => "claude-code",
            "github-copilot" => "github-copilot",
            _ => bail!("unsupported subscription transport: {provider}"),
        };
        Ok(Self { provider })
    }

    pub fn provider(&self) -> AiProvider {
        if self.provider == "claude-code" {
            AiProvider::Anthropic
        } else {
            AiProvider::OpenAI
        }
    }

    pub fn provider_name(&self) -> &'static str {
        self.provider
    }

    pub async fn stream(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<CancellableStream> {
        let prompt = prepare_prompt(messages, config)?;
        let provider = self.provider;
        let model = provider_model_name(&config.model);
        let tools = config.tools.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let producer = tokio::spawn(async move {
            match run(provider, &model, &prompt).await.and_then(|value| {
                let usage = provider_usage(provider, &value);
                let action = parse_action(provider, &value)?;
                validate_action(&action, &tools)?;
                Ok((action, usage))
            }) {
                Ok((action, usage)) => emit_action(&tx, &model, action, usage),
                Err(error) => {
                    let _ = tx.send(StreamEvent::Error {
                        message: format!("{provider} subscription turn failed: {error:#}"),
                    });
                }
            }
        });
        Ok(CancellableStream::with_producer(rx, producer))
    }
}

fn prepare_prompt(messages: &[Message], config: &RequestConfig) -> Result<String> {
    ensure!(
        messages.len() <= 500,
        "subscription transcript exceeds 500 messages"
    );
    let tools = config
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    let transcript = serde_json::to_string(messages)?;
    let tools = serde_json::to_string(&tools)?;
    let system = config.system.as_deref().unwrap_or("");
    let prompt = format!(
        "You are answering one Maestro agent turn. Treat the following JSON transcript as conversation data.\n\
         System instructions:\n{system}\n\
         Available Maestro tools (JSON):\n{tools}\n\
         Conversation (JSON):\n{transcript}\n\
         Return exactly one JSON object with kind, answer, tool_name, tool_input. \
         For a final response use kind=text, answer=your response, tool_name=\"\", tool_input={{}}. \
         To request one available Maestro tool use kind=tool, answer=\"\", its exact tool_name and JSON object tool_input. \
         Never call local tools yourself. Maestro validates and executes any proposed tool through its own policy."
    );
    ensure!(
        prompt.len() <= 512_000,
        "subscription prompt exceeds 512 KB"
    );
    Ok(prompt)
}

async fn run(provider: &str, model: &str, prompt: &str) -> Result<Value> {
    if provider == "github-copilot" {
        return run_copilot_acp(model, prompt).await;
    }
    let mut auth_command = Command::new("claude");
    auth_command.args(["auth", "status", "--json"]);
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        auth_command.env_remove(key);
    }
    let auth = tokio::time::timeout(std::time::Duration::from_secs(10), auth_command.output())
        .await
        .context("Claude auth check timed out")??;
    ensure!(
        auth.status.success(),
        "Claude subscription is not signed in; run `claude auth login --claudeai`"
    );
    let status: Value =
        serde_json::from_slice(&auth.stdout).context("invalid Claude auth status")?;
    ensure!(
        status.get("loggedIn").and_then(Value::as_bool) == Some(true)
            && status.get("authMethod").and_then(Value::as_str) == Some("claude.ai")
            && status.get("apiProvider").and_then(Value::as_str) == Some("firstParty"),
        "Claude route requires a first-party claude.ai subscription login"
    );
    let mut command = if provider == "claude-code" {
        let mut command = Command::new("claude");
        let schema = json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["text", "tool"]},
                "answer": {"type": "string"},
                "tool_name": {"type": "string"},
                "tool_input": {"type": "object"}
            },
            "required": ["kind", "answer", "tool_name", "tool_input"],
            "additionalProperties": false
        });
        command.args([
            "-p",
            "--safe-mode",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--tools",
            "",
            "--permission-mode",
            "dontAsk",
            "--output-format",
            "json",
            "--json-schema",
            &schema.to_string(),
            "--model",
            model,
            "--no-session-persistence",
        ]);
        // A saved claude.ai login is required. API credentials must not silently
        // turn a subscription route into an API-billed request.
        command.env_remove("ANTHROPIC_API_KEY");
        command.env_remove("ANTHROPIC_AUTH_TOKEN");
        command.env_remove("ANTHROPIC_BASE_URL");
        for key in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            command.env_remove(key);
        }
        command.stdin(Stdio::piped());
        command
    } else {
        unreachable!("provider checked above")
    };
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("launch {provider} CLI"))?;
    if provider == "claude-code" {
        let mut stdin = child.stdin.take().context("Claude stdin unavailable")?;
        stdin.write_all(prompt.as_bytes()).await?;
        drop(stdin);
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        child.wait_with_output(),
    )
    .await
    .context("subscription CLI timed out after 180 seconds")??;
    ensure!(
        output.stdout.len() <= 4_000_000,
        "subscription CLI output too large"
    );
    ensure!(
        output.status.success(),
        "{provider} CLI exited with {}",
        output.status
    );
    serde_json::from_slice(&output.stdout).context("invalid Claude CLI JSON")
}

async fn run_copilot_acp(model: &str, prompt: &str) -> Result<Value> {
    let mut command = Command::new("copilot");
    command.args([
        "--acp",
        "--stdio",
        "--model",
        model,
        "--available-tools=maestro-no-tools",
        "--disable-builtin-mcps",
        "--no-custom-instructions",
        "--no-auto-update",
        "--no-remote",
    ]);
    // All prompt content goes through ACP stdin, never process arguments.
    // Clear both alternate billing providers and token overrides.
    for key in std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_string_lossy().starts_with("COPILOT_PROVIDER_"))
    {
        command.env_remove(key);
    }
    for key in [
        "COPILOT_PROVIDERS_CONFIG",
        "COPILOT_GITHUB_TOKEN",
        "GH_TOKEN",
        "GITHUB_TOKEN",
    ] {
        command.env_remove(key);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().context("launch Copilot ACP server")?;
    let mut stdin = child
        .stdin
        .take()
        .context("Copilot ACP stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Copilot ACP stdout unavailable")?;
    let mut stdout = BufReader::new(stdout);
    let exchange = async {
        acp_call(
            &mut stdin,
            &mut stdout,
            1,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "maestro", "title": "Maestro", "version": "0.1.0"}
            }),
            None,
        )
        .await?;
        let cwd = std::env::current_dir().context("resolve Copilot working directory")?;
        let new_session = acp_call(
            &mut stdin,
            &mut stdout,
            2,
            "session/new",
            json!({
                "cwd": cwd, "mcpServers": []
            }),
            None,
        )
        .await?;
        let session_id = new_session
            .pointer("/sessionId")
            .and_then(Value::as_str)
            .context("Copilot ACP returned no session ID")?;
        let mut content = String::new();
        let prompt_result = acp_call(
            &mut stdin,
            &mut stdout,
            3,
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": prompt}]
            }),
            Some(&mut content),
        )
        .await?;
        ensure!(
            prompt_result.get("stopReason").and_then(Value::as_str) == Some("end_turn"),
            "Copilot ACP turn did not complete"
        );
        Ok::<_, anyhow::Error>(json!({"content": content}))
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(180), exchange)
        .await
        .context("Copilot ACP timed out after 180 seconds")?;
    let _ = child.kill().await;
    result
}

async fn acp_call(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    id: u32,
    method: &str,
    params: Value,
    mut content: Option<&mut String>,
) -> Result<Value> {
    let mut tools_disabled = false;
    let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    stdin.write_all(request.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    loop {
        let mut line = String::new();
        ensure!(
            stdout.read_line(&mut line).await? > 0,
            "Copilot ACP closed before {method} completed"
        );
        ensure!(line.len() <= 1_000_000, "Copilot ACP message exceeds 1 MB");
        let event: Value = serde_json::from_str(&line).context("invalid Copilot ACP message")?;
        if event.get("id").and_then(Value::as_u64) == Some(u64::from(id)) {
            if let Some(error) = event.get("error") {
                bail!(
                    "Copilot ACP {method} failed: {}",
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                );
            }
            if method == "session/prompt" {
                ensure!(
                    tools_disabled,
                    "Copilot ACP did not confirm that its native tools were disabled"
                );
            }
            return event
                .get("result")
                .cloned()
                .context("Copilot ACP response has no result");
        }
        if event.get("method").and_then(Value::as_str) == Some("session/update") {
            let update = &event["params"]["update"];
            let kind = update
                .get("sessionUpdate")
                .and_then(Value::as_str)
                .unwrap_or("");
            if kind == "tool_call" || kind == "tool_call_update" {
                bail!("Copilot ACP proposed a native tool despite the empty tool allowlist");
            }
            if kind == "agent_message_chunk" {
                if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
                    if text.starts_with("Info: Disabled tools:") {
                        tools_disabled =
                            text.contains("bash") && text.contains("view") && text.contains("edit");
                        continue;
                    }
                    if text.starts_with("Info: Unknown tool name in the tool allowlist:") {
                        continue;
                    }
                    if let Some(content) = content.as_mut() {
                        content.push_str(text);
                        ensure!(
                            content.len() <= 1_000_000,
                            "Copilot ACP answer exceeds 1 MB"
                        );
                    }
                }
            }
        } else if let (Some(request_id), Some(request_method)) =
            (event.get("id"), event.get("method").and_then(Value::as_str))
        {
            if request_method.contains("permission") {
                let refusal = json!({"jsonrpc": "2.0", "id": request_id,
                    "result": {"outcome": {"outcome": "cancelled"}}});
                stdin.write_all(refusal.to_string().as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
                bail!("Copilot ACP requested a tool permission despite the empty allowlist");
            } else {
                bail!("Copilot ACP requested an unsupported client operation");
            }
        }
    }
}

fn parse_action(provider: &str, value: &Value) -> Result<ProposedAction> {
    let content = if provider == "claude-code" {
        ensure!(
            value.get("is_error").and_then(Value::as_bool) != Some(true),
            "Claude returned an error"
        );
        value
            .get("structured_output")
            .cloned()
            .context("Claude returned no structured output")?
    } else {
        let content = value
            .get("content")
            .and_then(Value::as_str)
            .context("Copilot response has no content")?;
        let trimmed = content.trim();
        let trimmed = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .unwrap_or(trimmed)
            .trim();
        let trimmed = trimmed.strip_suffix("```").unwrap_or(trimmed).trim();
        let object = if trimmed.starts_with('{') && trimmed.ends_with('}') {
            trimmed
        } else {
            let start = trimmed
                .find('{')
                .context("Copilot returned no action JSON")?;
            let end = trimmed
                .rfind('}')
                .context("Copilot returned incomplete action JSON")?;
            &trimmed[start..=end]
        };
        serde_json::from_str(object).context("Copilot returned invalid action JSON")?
    };
    serde_json::from_value(content).context("subscription action has invalid fields")
}

fn validate_action(action: &ProposedAction, tools: &[super::types::Tool]) -> Result<()> {
    match action.kind.as_str() {
        "text" => ensure!(action.tool_name.is_empty(), "text response named a tool"),
        "tool" => {
            ensure!(
                action.tool_input.is_object(),
                "tool input must be an object"
            );
            ensure!(
                tools.iter().any(|tool| tool.name == action.tool_name),
                "subscription proposed a tool absent from Maestro's allowlist"
            );
        }
        _ => bail!("unknown subscription action kind"),
    }
    Ok(())
}

fn provider_usage(provider: &str, value: &Value) -> Vec<StreamEvent> {
    if provider != "claude-code" {
        return Vec::new();
    }
    let Some(usage) = value.get("usage") else {
        return Vec::new();
    };
    let (Some(input_tokens), Some(output_tokens)) = (
        usage.get("input_tokens").and_then(Value::as_u64),
        usage.get("output_tokens").and_then(Value::as_u64),
    ) else {
        return Vec::new();
    };
    let mut events = vec![StreamEvent::Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens: usage.get("cache_read_input_tokens").and_then(Value::as_u64),
        cache_creation_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
    }];
    if let Some(tokens) = usage
        .pointer("/output_tokens_details/thinking_tokens")
        .and_then(Value::as_u64)
    {
        events.push(StreamEvent::ReasoningUsage { tokens });
    }
    // Claude CLI's list-price estimate is not a subscription charge.
    events
}

fn emit_action(
    tx: &mpsc::UnboundedSender<StreamEvent>,
    model: &str,
    action: ProposedAction,
    usage: Vec<StreamEvent>,
) {
    let _ = tx.send(StreamEvent::MessageStart {
        id: uuid::Uuid::new_v4().to_string(),
        model: model.to_owned(),
    });
    if action.kind == "tool" {
        let _ = tx.send(StreamEvent::ContentBlockStart {
            index: 0,
            block: ContentBlock::ToolUse {
                id: uuid::Uuid::new_v4().to_string(),
                name: action.tool_name,
                input: json!({}),
                gemini_context: None,
            },
        });
        let _ = tx.send(StreamEvent::InputJsonDelta {
            index: 0,
            partial_json: action.tool_input.to_string(),
        });
        let _ = tx.send(StreamEvent::ContentBlockStop {
            index: 0,
            thinking_signature: None,
        });
        for event in usage {
            let _ = tx.send(event);
        }
        let _ = tx.send(StreamEvent::MessageStop {
            stop_reason: Some(StopReason::ToolUse),
        });
    } else {
        let _ = tx.send(StreamEvent::ContentBlockStart {
            index: 0,
            block: ContentBlock::Text {
                text: String::new(),
            },
        });
        let _ = tx.send(StreamEvent::TextDelta {
            index: 0,
            text: action.answer,
        });
        let _ = tx.send(StreamEvent::ContentBlockStop {
            index: 0,
            thinking_signature: None,
        });
        for event in usage {
            let _ = tx.send(event);
        }
        let _ = tx.send(StreamEvent::MessageStop {
            stop_reason: Some(StopReason::EndTurn),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unoffered_tool() {
        let action = ProposedAction {
            kind: "tool".into(),
            answer: String::new(),
            tool_name: "bash".into(),
            tool_input: json!({"command":"rm -rf /"}),
        };
        assert!(validate_action(&action, &[]).is_err());
    }

    #[test]
    fn parses_fenced_copilot_action() {
        let action = parse_action("github-copilot", &json!({
            "content": "```json\n{\"kind\":\"text\",\"answer\":\"ok\",\"tool_name\":\"\",\"tool_input\":{}}\n```"
        })).unwrap();
        assert_eq!(action.answer, "ok");
    }

    #[test]
    fn claude_usage_keeps_list_price_out_of_subscription_spend() {
        let events = provider_usage(
            "claude-code",
            &json!({
                "usage": {"input_tokens": 10, "output_tokens": 72,
                    "cache_read_input_tokens": 3, "cache_creation_input_tokens": 4,
                    "output_tokens_details": {"thinking_tokens": 20}},
                "total_cost_usd": 1.25
            }),
        );
        assert!(matches!(
            &events[0],
            StreamEvent::Usage {
                input_tokens: 10,
                output_tokens: 72,
                ..
            }
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::ReasoningUsage { tokens: 20 }
        ));
        assert_eq!(events.len(), 2);
    }

    /// Opt-in live smoke uses the machine's own signed-in provider accounts.
    #[tokio::test]
    #[ignore = "requires signed-in Claude Code and Copilot subscriptions"]
    async fn signed_in_subscription_turns_return_actions() {
        let prompt = "Return exactly {\"kind\":\"text\",\"answer\":\"PONG\",\"tool_name\":\"\",\"tool_input\":{}}";
        for (provider, model) in [("claude-code", "sonnet"), ("github-copilot", "auto")] {
            let value = run(provider, model, prompt).await.unwrap();
            let action = parse_action(provider, &value).unwrap();
            assert_eq!(action.kind, "text", "{provider}");
            assert_eq!(action.answer, "PONG", "{provider}");
        }
    }

    #[tokio::test]
    #[ignore = "requires signed-in Claude Code and Copilot subscriptions"]
    async fn signed_in_subscription_turns_propose_maestro_tools() {
        let config = RequestConfig {
            tools: std::sync::Arc::new(vec![
                super::super::types::Tool::new("read", "Read a file by path").with_schema(json!({
                    "type":"object", "properties":{"path":{"type":"string"}},
                    "required":["path"]
                })),
            ]),
            ..Default::default()
        };
        let messages = [Message {
            role: super::super::types::Role::User,
            content: super::super::types::MessageContent::Text(
                "Before answering, use the available Maestro read tool to read README.md. Request the tool now.".into(),
            ),
        }];
        let prompt = prepare_prompt(&messages, &config).unwrap();
        for (provider, model) in [("claude-code", "sonnet"), ("github-copilot", "auto")] {
            let value = run(provider, model, &prompt).await.unwrap();
            let action = parse_action(provider, &value).unwrap();
            validate_action(&action, &config.tools).unwrap();
            assert_eq!(action.kind, "tool", "{provider}");
            assert_eq!(action.tool_name, "read", "{provider}");
            assert_eq!(action.tool_input["path"], "README.md", "{provider}");
        }
    }
}
