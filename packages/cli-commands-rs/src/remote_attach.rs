//! Interactive TTY attach for `maestro remote attach`.
//!
//! Ported from `src/remote-runner/attach-client.ts`. When stdin/stdout are TTYs
//! and the caller did not request `--json` / `--print-env`, mint an attach token
//! and run a lightweight REPL over [`RemoteAgentTransport`]. Otherwise the
//! remote CLI prints env-handoff instructions.

use std::collections::HashMap;
use std::io::{self, Write};

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::headless::messages::ToolRetryDecisionAction;
use crate::headless::{
    AgentState, ClientToolResultContent, FromAgentMessage, PendingApproval, RemoteAgentTransport,
    RemoteIncoming, RemoteTransportConfig, ServerRequestType, ToAgentMessage, ToolResult,
};

const DEFAULT_CLIENT_NAME: &str = "maestro-remote-cli";
const ATTACH_TOKEN_HEADER: &str = "X-EvalOps-Runner-Attach-Token-Id";

/// Connection role used for interactive remote attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachRole {
    Viewer,
    Controller,
}

impl AttachRole {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Controller => "controller",
        }
    }

    #[must_use]
    pub fn is_controller(self) -> bool {
        matches!(self, Self::Controller)
    }
}

/// Inputs required to open an interactive remote-runner attach session.
#[derive(Debug, Clone)]
pub struct RemoteAttachInput {
    pub gateway_base_url: String,
    pub session_id: String,
    pub token_id: String,
    pub token_secret: String,
    pub role: AttachRole,
    pub client_version: Option<String>,
    pub take_control: bool,
}

/// Whether `maestro remote attach` should enter the interactive TTY REPL.
#[must_use]
pub fn should_use_interactive_remote_attach(
    json: bool,
    print_env: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
) -> bool {
    !json && !print_env && stdin_is_tty && stdout_is_tty
}

/// Build a [`RemoteTransportConfig`] for runner attach-token auth.
#[must_use]
pub fn build_remote_attach_transport_config(input: &RemoteAttachInput) -> RemoteTransportConfig {
    let mut headers = HashMap::new();
    headers.insert(ATTACH_TOKEN_HEADER.to_string(), input.token_id.clone());

    RemoteTransportConfig {
        base_url: input.gateway_base_url.trim_end_matches('/').to_string(),
        api_key: Some(input.token_secret.clone()),
        session_id: Some(input.session_id.clone()),
        client_name: DEFAULT_CLIENT_NAME.to_string(),
        client_version: input.client_version.clone(),
        role: Some(input.role.as_str().to_string()),
        take_control: input.take_control,
        // Match attach-client.ts: only server request capabilities for controllers.
        enable_client_tools: false,
        enable_command_exec: false,
        enable_file_search: false,
        enable_file_read: false,
        enable_file_watch: false,
        enable_raw_agent_events: false,
        opt_out_notifications: vec!["heartbeat".to_string()],
        headers,
        ..RemoteTransportConfig::default()
    }
}

/// Connect and run the interactive remote attach REPL until `/exit` or EOF.
pub async fn attach_to_remote_runner_session(input: RemoteAttachInput) -> Result<()> {
    let config = build_remote_attach_transport_config(&input);
    let mut transport = RemoteAgentTransport::connect(config)
        .await
        .map_err(|error| anyhow!("remote runner attach failed: {error}"))?;

    let mut state = transport.state().clone();
    let session_id = transport.session_id().to_string();
    let role = input.role;

    println!(
        "Attached to {session_id} as {}",
        if role.is_controller() {
            "controller"
        } else {
            "viewer"
        }
    );
    if role.is_controller() {
        println!(
            "Enter a prompt below. Use /status, /help, or /exit. Press Ctrl+C to interrupt an active response."
        );
    } else {
        println!("Viewer mode is read-only. Use /status or /exit.");
    }

    let mut visible_response_open = false;
    let mut response_ends_with_newline = true;
    let mut close_requested = false;

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    let result = run_repl(
        &mut transport,
        &mut state,
        &input.session_id,
        role,
        &mut lines,
        &mut visible_response_open,
        &mut response_ends_with_newline,
        &mut close_requested,
    )
    .await;

    if let Err(error) = transport.shutdown_and_wait().await {
        let _ = writeln!(io::stderr(), "remote attach disconnect: {error}");
    }

    result
}

#[allow(clippy::too_many_arguments)]
async fn run_repl(
    transport: &mut RemoteAgentTransport,
    state: &mut AgentState,
    requested_session_id: &str,
    role: AttachRole,
    lines: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
    visible_response_open: &mut bool,
    response_ends_with_newline: &mut bool,
    close_requested: &mut bool,
) -> Result<()> {
    while !*close_requested {
        // Drain any already-available events first.
        while let Some(result) = transport.try_recv_incoming() {
            handle_incoming(
                result.map_err(|error| anyhow!("remote runner stream error: {error}"))?,
                state,
                visible_response_open,
                response_ends_with_newline,
            )?;
        }

        if role.is_controller() {
            if let Some(pending) = next_pending(state) {
                handle_pending(
                    transport,
                    state,
                    role,
                    lines,
                    pending,
                    visible_response_open,
                    response_ends_with_newline,
                    close_requested,
                )
                .await?;
                continue;
            }
        }

        if state.is_responding || !state.is_ready {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    handle_sigint(
                        transport,
                        state,
                        role,
                        close_requested,
                        visible_response_open,
                        response_ends_with_newline,
                    )?;
                }
                incoming = transport.recv_incoming() => {
                    handle_incoming(
                        incoming.map_err(|error| anyhow!("remote runner stream error: {error}"))?,
                        state,
                        visible_response_open,
                        response_ends_with_newline,
                    )?;
                }
            }
            continue;
        }

        let prompt = if role.is_controller() {
            "remote> "
        } else {
            "viewer> "
        };
        print!("{prompt}");
        io::stdout().flush().ok();

        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                handle_sigint(
                    transport,
                    state,
                    role,
                    close_requested,
                    visible_response_open,
                    response_ends_with_newline,
                )?;
            }
            incoming = transport.recv_incoming() => {
                // Clear the prompt line so streamed output is readable.
                clear_prompt_line();
                handle_incoming(
                    incoming.map_err(|error| anyhow!("remote runner stream error: {error}"))?,
                    state,
                    visible_response_open,
                    response_ends_with_newline,
                )?;
            }
            line = lines.next_line() => {
                match line.context("read remote attach stdin")? {
                    None => {
                        *close_requested = true;
                    }
                    Some(raw) => {
                        let trimmed = raw.trim().to_string();
                        if handle_command(
                            transport,
                            state,
                            role,
                            requested_session_id,
                            &trimmed,
                            close_requested,
                            visible_response_open,
                            response_ends_with_newline,
                        )? {
                            continue;
                        }
                        if !role.is_controller() {
                            write_line(
                                "Viewer mode is read-only.",
                                visible_response_open,
                                response_ends_with_newline,
                            );
                            continue;
                        }
                        if trimmed.is_empty() {
                            continue;
                        }
                        send_message(
                            transport,
                            state,
                            ToAgentMessage::Prompt {
                                content: raw,
                                attachments: None,
                                managed_inference_authorization: None,
                            },
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn handle_incoming(
    incoming: RemoteIncoming,
    state: &mut AgentState,
    visible_response_open: &mut bool,
    response_ends_with_newline: &mut bool,
) -> Result<()> {
    match incoming {
        RemoteIncoming::Snapshot {
            state: snapshot, ..
        }
        | RemoteIncoming::Reset {
            state: snapshot, ..
        } => {
            *state = *snapshot;
            write_line(
                "Remote session state resynced from snapshot.",
                visible_response_open,
                response_ends_with_newline,
            );
        }
        RemoteIncoming::Message(message) => {
            // Transport already applied the message to its internal state; keep
            // our local copy in lockstep for pending-request prompts.
            state.handle_message(message.clone());
            match message {
                FromAgentMessage::ResponseStart { .. } => {
                    *visible_response_open = false;
                    *response_ends_with_newline = true;
                }
                FromAgentMessage::ResponseChunk {
                    content,
                    is_thinking,
                    ..
                } if !is_thinking => {
                    print_assistant_chunk(
                        &content,
                        visible_response_open,
                        response_ends_with_newline,
                    );
                }
                FromAgentMessage::ResponseEnd { .. } => {
                    ensure_assistant_break(visible_response_open, response_ends_with_newline);
                    *visible_response_open = false;
                }
                FromAgentMessage::TurnCompleted { .. } => {
                    ensure_assistant_break(visible_response_open, response_ends_with_newline);
                    *visible_response_open = false;
                }
                FromAgentMessage::TurnInterrupted { reason, .. } => {
                    write_line(
                        &format!("interrupted: {reason}"),
                        visible_response_open,
                        response_ends_with_newline,
                    );
                    *visible_response_open = false;
                }
                FromAgentMessage::ProviderError { kind, message } => {
                    write_line(
                        &format!("provider error ({kind:?}): {message}"),
                        visible_response_open,
                        response_ends_with_newline,
                    );
                    *visible_response_open = false;
                }
                FromAgentMessage::Status { message } => {
                    write_line(&message, visible_response_open, response_ends_with_newline);
                }
                FromAgentMessage::Error { message, .. } => {
                    write_line(
                        &format!("error: {message}"),
                        visible_response_open,
                        response_ends_with_newline,
                    );
                }
                FromAgentMessage::Compaction { summary, .. } => {
                    write_line(
                        &format!("Compacted remote history: {summary}"),
                        visible_response_open,
                        response_ends_with_newline,
                    );
                }
                _ => {}
            }
        }
        RemoteIncoming::Heartbeat => {}
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_pending(
    transport: &mut RemoteAgentTransport,
    state: &mut AgentState,
    role: AttachRole,
    lines: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
    pending: PendingKind,
    visible_response_open: &mut bool,
    response_ends_with_newline: &mut bool,
    close_requested: &mut bool,
) -> Result<()> {
    match pending {
        PendingKind::Approval(request) => {
            write_line(
                &format!("Approval required: {}", format_request_label(&request)),
                visible_response_open,
                response_ends_with_newline,
            );
            if let Some(summary) = summarize_args(&request.args) {
                write_line(&summary, visible_response_open, response_ends_with_newline);
            }
            loop {
                if *close_requested {
                    return Ok(());
                }
                let answer = read_prompt_line(
                    transport,
                    state,
                    lines,
                    "Approve? [y/N]: ",
                    visible_response_open,
                    response_ends_with_newline,
                    close_requested,
                    role,
                )
                .await?;
                if *close_requested {
                    return Ok(());
                }
                let Some(answer) = answer else {
                    continue;
                };
                let trimmed = answer.trim().to_string();
                let session_id = transport.session_id().to_string();
                if handle_command(
                    transport,
                    state,
                    role,
                    &session_id,
                    &trimmed,
                    close_requested,
                    visible_response_open,
                    response_ends_with_newline,
                )? {
                    if *close_requested {
                        return Ok(());
                    }
                    continue;
                }
                let approved = matches!(trimmed.to_ascii_lowercase().as_str(), "y" | "yes");
                send_approval_response(transport, state, &request, approved)?;
                return Ok(());
            }
        }
        PendingKind::UserInput(request) => {
            write_line(
                &format!("Input requested: {}", format_request_label(&request)),
                visible_response_open,
                response_ends_with_newline,
            );
            if let Some(summary) = summarize_args(&request.args) {
                write_line(&summary, visible_response_open, response_ends_with_newline);
            }
            loop {
                if *close_requested {
                    return Ok(());
                }
                let answer = read_prompt_line(
                    transport,
                    state,
                    lines,
                    "Reply: ",
                    visible_response_open,
                    response_ends_with_newline,
                    close_requested,
                    role,
                )
                .await?;
                if *close_requested {
                    return Ok(());
                }
                let Some(answer) = answer else {
                    continue;
                };
                let session_id = transport.session_id().to_string();
                if handle_command(
                    transport,
                    state,
                    role,
                    &session_id,
                    answer.trim(),
                    close_requested,
                    visible_response_open,
                    response_ends_with_newline,
                )? {
                    if *close_requested {
                        return Ok(());
                    }
                    continue;
                }
                send_user_input_response(transport, state, &request, &answer)?;
                return Ok(());
            }
        }
        PendingKind::ToolRetry(request) => {
            write_line(
                &format!("Tool retry requested: {}", format_request_label(&request)),
                visible_response_open,
                response_ends_with_newline,
            );
            if let Some(summary) = summarize_args(&request.args) {
                write_line(&summary, visible_response_open, response_ends_with_newline);
            }
            loop {
                if *close_requested {
                    return Ok(());
                }
                let answer = read_prompt_line(
                    transport,
                    state,
                    lines,
                    "Decision [retry/skip/abort]: ",
                    visible_response_open,
                    response_ends_with_newline,
                    close_requested,
                    role,
                )
                .await?;
                if *close_requested {
                    return Ok(());
                }
                let Some(answer) = answer else {
                    continue;
                };
                let trimmed = answer.trim().to_ascii_lowercase();
                let session_id = transport.session_id().to_string();
                if handle_command(
                    transport,
                    state,
                    role,
                    &session_id,
                    trimmed.as_str(),
                    close_requested,
                    visible_response_open,
                    response_ends_with_newline,
                )? {
                    if *close_requested {
                        return Ok(());
                    }
                    continue;
                }
                let decision = match trimmed.as_str() {
                    "retry" => ToolRetryDecisionAction::Retry,
                    "skip" => ToolRetryDecisionAction::Skip,
                    "abort" => ToolRetryDecisionAction::Abort,
                    _ => {
                        write_line(
                            "Enter retry, skip, or abort.",
                            visible_response_open,
                            response_ends_with_newline,
                        );
                        continue;
                    }
                };
                let request_id = request
                    .request_id
                    .clone()
                    .unwrap_or_else(|| request.call_id.clone());
                send_message(
                    transport,
                    state,
                    ToAgentMessage::ServerRequestResponse {
                        request_id,
                        request_type: ServerRequestType::ToolRetry,
                        approved: None,
                        result: None,
                        content: None,
                        is_error: None,
                        decision_action: Some(decision),
                        reason: None,
                    },
                )?;
                return Ok(());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_prompt_line(
    transport: &mut RemoteAgentTransport,
    state: &mut AgentState,
    lines: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
    prompt: &str,
    visible_response_open: &mut bool,
    response_ends_with_newline: &mut bool,
    close_requested: &mut bool,
    role: AttachRole,
) -> Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush().ok();
    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                handle_sigint(
                    transport,
                    state,
                    role,
                    close_requested,
                    visible_response_open,
                    response_ends_with_newline,
                )?;
                if *close_requested {
                    return Ok(None);
                }
            }
            incoming = transport.recv_incoming() => {
                clear_prompt_line();
                handle_incoming(
                    incoming.map_err(|error| anyhow!("remote runner stream error: {error}"))?,
                    state,
                    visible_response_open,
                    response_ends_with_newline,
                )?;
                // Re-print the prompt after interleaved stream output.
                print!("{prompt}");
                io::stdout().flush().ok();
            }
            line = lines.next_line() => {
                return match line.context("read remote attach stdin")? {
                    None => {
                        *close_requested = true;
                        Ok(None)
                    }
                    Some(value) => Ok(Some(value)),
                };
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_command(
    transport: &mut RemoteAgentTransport,
    state: &mut AgentState,
    role: AttachRole,
    session_id: &str,
    line: &str,
    close_requested: &mut bool,
    visible_response_open: &mut bool,
    response_ends_with_newline: &mut bool,
) -> Result<bool> {
    match line {
        "/help" => {
            write_line(
                "Commands: /status, /interrupt, /exit",
                visible_response_open,
                response_ends_with_newline,
            );
            Ok(true)
        }
        "/status" => {
            print_status(
                session_id,
                role,
                state,
                visible_response_open,
                response_ends_with_newline,
            );
            Ok(true)
        }
        "/interrupt" => {
            if !role.is_controller() {
                write_line(
                    "Viewer mode cannot interrupt the session.",
                    visible_response_open,
                    response_ends_with_newline,
                );
                return Ok(true);
            }
            if !state.is_responding {
                write_line(
                    "No active response to interrupt.",
                    visible_response_open,
                    response_ends_with_newline,
                );
                return Ok(true);
            }
            send_message(transport, state, ToAgentMessage::Interrupt)?;
            write_line(
                "Interrupt sent.",
                visible_response_open,
                response_ends_with_newline,
            );
            Ok(true)
        }
        "/exit" | "/quit" => {
            *close_requested = true;
            Ok(true)
        }
        other if other.starts_with('/') => {
            write_line(
                &format!("Unknown command: {other}"),
                visible_response_open,
                response_ends_with_newline,
            );
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn handle_sigint(
    transport: &mut RemoteAgentTransport,
    state: &mut AgentState,
    role: AttachRole,
    close_requested: &mut bool,
    visible_response_open: &mut bool,
    response_ends_with_newline: &mut bool,
) -> Result<()> {
    if role.is_controller() && state.is_responding && !*close_requested {
        send_message(transport, state, ToAgentMessage::Interrupt)?;
        write_line(
            "Interrupt sent.",
            visible_response_open,
            response_ends_with_newline,
        );
        return Ok(());
    }
    *close_requested = true;
    Ok(())
}

fn send_message(
    transport: &mut RemoteAgentTransport,
    state: &mut AgentState,
    message: ToAgentMessage,
) -> Result<()> {
    transport
        .send(message.clone())
        .map_err(|error| anyhow!("failed to send remote attach message: {error}"))?;
    state.handle_sent_message(&message);
    Ok(())
}

fn send_approval_response(
    transport: &mut RemoteAgentTransport,
    state: &mut AgentState,
    request: &PendingApproval,
    approved: bool,
) -> Result<()> {
    send_message(
        transport,
        state,
        approval_response_message(request, approved),
    )
}

fn approval_response_message(request: &PendingApproval, approved: bool) -> ToAgentMessage {
    let result = if approved {
        Some(ToolResult {
            success: true,
            output: "Approved by remote attach client".to_string(),
            error: None,
            details: None,
        })
    } else {
        Some(ToolResult {
            success: false,
            output: String::new(),
            error: Some("Denied by remote attach client".to_string()),
            details: None,
        })
    };

    if let Some(request_id) = request.request_id.as_ref() {
        ToAgentMessage::ServerRequestResponse {
            request_id: request_id.clone(),
            request_type: ServerRequestType::Approval,
            approved: Some(approved),
            result,
            content: None,
            is_error: None,
            decision_action: None,
            reason: None,
        }
    } else {
        ToAgentMessage::ToolResponse {
            call_id: request.call_id.clone(),
            tool_execution_id: request.tool_execution_id.clone(),
            approved,
            result: if approved { None } else { result },
        }
    }
}

fn send_user_input_response(
    transport: &mut RemoteAgentTransport,
    state: &mut AgentState,
    request: &PendingApproval,
    answer: &str,
) -> Result<()> {
    let content = vec![ClientToolResultContent::Text {
        text: answer.to_string(),
    }];
    if let Some(request_id) = request.request_id.as_ref() {
        send_message(
            transport,
            state,
            ToAgentMessage::ServerRequestResponse {
                request_id: request_id.clone(),
                request_type: ServerRequestType::UserInput,
                approved: None,
                result: None,
                content: Some(content),
                is_error: Some(false),
                decision_action: None,
                reason: None,
            },
        )
    } else {
        send_message(
            transport,
            state,
            ToAgentMessage::ClientToolResult {
                call_id: request.call_id.clone(),
                content,
                is_error: false,
            },
        )
    }
}

#[derive(Debug, Clone)]
enum PendingKind {
    Approval(PendingApproval),
    UserInput(PendingApproval),
    ToolRetry(PendingApproval),
}

fn next_pending(state: &AgentState) -> Option<PendingKind> {
    if let Some(request) = state.pending_approvals.first() {
        return Some(PendingKind::Approval(request.clone()));
    }
    if let Some(request) = state.pending_user_inputs.first() {
        return Some(PendingKind::UserInput(request.clone()));
    }
    if let Some(request) = state.pending_tool_retries.first() {
        return Some(PendingKind::ToolRetry(request.clone()));
    }
    None
}

fn format_request_label(request: &PendingApproval) -> String {
    request.tool.clone()
}

fn summarize_args(args: &serde_json::Value) -> Option<String> {
    if args.is_null() {
        return None;
    }
    let serialized = serde_json::to_string_pretty(args).ok()?;
    if serialized.is_empty() {
        return None;
    }
    if serialized.len() <= 400 {
        Some(serialized)
    } else {
        Some(format!("{}...", &serialized[..397]))
    }
}

fn print_status(
    session_id: &str,
    role: AttachRole,
    state: &AgentState,
    visible_response_open: &mut bool,
    response_ends_with_newline: &mut bool,
) {
    write_line(
        &format!("Remote session {session_id}"),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!("  role: {}", role.as_str()),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!("  ready: {}", if state.is_ready { "yes" } else { "no" }),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!(
            "  responding: {}",
            if state.is_responding { "yes" } else { "no" }
        ),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!("  model: {}", state.model.as_deref().unwrap_or("-")),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!("  provider: {}", state.provider.as_deref().unwrap_or("-")),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!("  cwd: {}", state.cwd.as_deref().unwrap_or("-")),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!("  git: {}", state.git_branch.as_deref().unwrap_or("-")),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!("  approvals: {}", state.pending_approvals.len()),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!("  user input: {}", state.pending_user_inputs.len()),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!("  tool retries: {}", state.pending_tool_retries.len()),
        visible_response_open,
        response_ends_with_newline,
    );
    write_line(
        &format!("  active tools: {}", state.active_tools.len()),
        visible_response_open,
        response_ends_with_newline,
    );
}

fn ensure_assistant_break(visible_response_open: &mut bool, response_ends_with_newline: &mut bool) {
    if *visible_response_open && !*response_ends_with_newline {
        println!();
        *response_ends_with_newline = true;
    }
}

/// Sanitize text that arrives over the network from the remote agent
/// transport before it reaches the attaching user's real terminal.
///
/// There is no ratatui `Buffer` in this REPL to filter it (unlike the TUI
/// chat pane), so a malicious or compromised remote peer could otherwise
/// write ANSI/OSC/CSI escape sequences straight into the terminal via
/// `Status`/`Error`/`Compaction` messages or assistant response chunks.
fn sanitize_remote_text(s: &str) -> String {
    crate::output_sanitize::sanitize_control_chars(s)
}

fn write_line(
    message: &str,
    visible_response_open: &mut bool,
    response_ends_with_newline: &mut bool,
) {
    ensure_assistant_break(visible_response_open, response_ends_with_newline);
    clear_prompt_line();
    println!("{}", sanitize_remote_text(message));
}

fn print_assistant_chunk(
    content: &str,
    visible_response_open: &mut bool,
    response_ends_with_newline: &mut bool,
) {
    if content.is_empty() {
        return;
    }
    if !*visible_response_open {
        clear_prompt_line();
        print!("assistant> ");
        *visible_response_open = true;
        *response_ends_with_newline = false;
    }
    print!("{}", sanitize_remote_text(content));
    let _ = io::stdout().flush();
    *response_ends_with_newline = content.ends_with('\n');
}

fn clear_prompt_line() {
    // Best-effort erase of the current terminal line when stdout is a TTY.
    use std::io::IsTerminal;
    if !io::stdout().is_terminal() {
        return;
    }
    print!("\r\x1b[2K");
    let _ = io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_remote_text_strips_osc_injection_from_a_malicious_peer() {
        // A minimal OSC-0 (set title) sequence a compromised/malicious remote
        // peer could send as a Status/Error/Compaction message or assistant
        // response chunk.
        let input = "before\x1b]0;evil\x07after";
        let out = sanitize_remote_text(input);
        assert_eq!(out, "before]0;evilafter");
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\x07'));
    }

    #[test]
    fn sanitize_remote_text_preserves_ordinary_text_with_newlines_and_tabs() {
        let input = "status: ok\nlatency:\t12ms";
        assert_eq!(sanitize_remote_text(input), input);
    }

    #[test]
    fn interactive_gate_requires_tty_without_json_or_print_env() {
        assert!(should_use_interactive_remote_attach(
            false, false, true, true
        ));
        assert!(!should_use_interactive_remote_attach(
            true, false, true, true
        ));
        assert!(!should_use_interactive_remote_attach(
            false, true, true, true
        ));
        assert!(!should_use_interactive_remote_attach(
            false, false, false, true
        ));
        assert!(!should_use_interactive_remote_attach(
            false, false, true, false
        ));
        assert!(!should_use_interactive_remote_attach(
            false, false, false, false
        ));
    }

    #[test]
    fn builds_attach_transport_config_with_token_headers() {
        let input = RemoteAttachInput {
            gateway_base_url: "https://runner.example/gateway/".to_string(),
            session_id: "sess-1".to_string(),
            token_id: "tok-id".to_string(),
            token_secret: "tok-secret".to_string(),
            role: AttachRole::Controller,
            client_version: Some("1.2.3".to_string()),
            take_control: true,
        };
        let config = build_remote_attach_transport_config(&input);
        assert_eq!(config.base_url, "https://runner.example/gateway");
        assert_eq!(config.api_key.as_deref(), Some("tok-secret"));
        assert_eq!(config.session_id.as_deref(), Some("sess-1"));
        assert_eq!(config.role.as_deref(), Some("controller"));
        assert_eq!(config.client_name, DEFAULT_CLIENT_NAME);
        assert_eq!(config.client_version.as_deref(), Some("1.2.3"));
        assert!(config.take_control);
        assert_eq!(config.opt_out_notifications, vec!["heartbeat".to_string()]);
        assert_eq!(
            config.headers.get(ATTACH_TOKEN_HEADER).map(String::as_str),
            Some("tok-id")
        );
        assert!(!config.enable_client_tools);
        assert!(!config.enable_command_exec);
        assert!(!config.enable_file_search);
        assert!(!config.enable_file_read);
        assert!(!config.enable_file_watch);

        let viewer = RemoteAttachInput {
            role: AttachRole::Viewer,
            take_control: false,
            ..input
        };
        let viewer_config = build_remote_attach_transport_config(&viewer);
        assert_eq!(viewer_config.role.as_deref(), Some("viewer"));
        assert!(!viewer_config.take_control);
    }

    #[test]
    fn typed_remote_terminals_close_visible_partial_responses() {
        for message in [
            FromAgentMessage::TurnInterrupted {
                response_id: "done".to_string(),
                reason: "cancelled".to_string(),
            },
            FromAgentMessage::ProviderError {
                kind: maestro_ai::ProviderStreamErrorKind::OutputTokenExhaustion,
                message: "output exhausted".to_string(),
            },
        ] {
            let mut state = AgentState::default();
            let mut visible_response_open = true;
            let mut response_ends_with_newline = false;
            handle_incoming(
                RemoteIncoming::Message(message),
                &mut state,
                &mut visible_response_open,
                &mut response_ends_with_newline,
            )
            .expect("typed terminal should render");
            assert!(!visible_response_open);
        }
    }

    #[test]
    fn governed_legacy_approval_runs_the_native_tool() {
        let request = PendingApproval {
            call_id: "call-1".to_string(),
            tool_execution_id: Some("tool-execution-1".to_string()),
            request_id: None,
            tool: "write".to_string(),
            args: serde_json::json!({}),
            started_at_ms: None,
        };

        assert!(matches!(
            approval_response_message(&request, true),
            ToAgentMessage::ToolResponse {
                tool_execution_id: Some(tool_execution_id),
                approved: true,
                result: None,
                ..
            } if tool_execution_id == "tool-execution-1"
        ));
    }
}
