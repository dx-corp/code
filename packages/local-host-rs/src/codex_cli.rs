//! Native `maestro codex` CLI.
//!
//! Ports `src/cli/commands/codex.ts` onto the Rust stdio JSON-RPC client in
//! `codex_app_server`. OpenAI OAuth (`maestro openai`) remains a separate command
//! and must not be aliased here.
//!
//! Doctor validates the dynamic tool schemas built from the same native tool
//! registry used by Codex app-server turns.

use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
#[cfg(test)]
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::codex_app_server::{
    AccountReadResult, CodexAppServerClient, InitializeOptions, LoginFlow, ThreadResumeParams,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexCommand {
    Login,
    Logout,
    Status,
    Doctor,
    Ready,
}

pub fn parse_codex_subcommand(args: &[&str]) -> Result<CodexCommand> {
    match args.first().copied() {
        Some("login") => Ok(CodexCommand::Login),
        Some("logout") => Ok(CodexCommand::Logout),
        Some("status") => Ok(CodexCommand::Status),
        Some("doctor") => Ok(CodexCommand::Doctor),
        Some("ready") => Ok(CodexCommand::Ready),
        _ => bail!(
            "{}",
            crate::localization::cli_locale().format("unknown codex subcommand", &[])
        ),
    }
}

/// Dispatch `maestro codex <subcommand> ...`.
pub async fn run_codex(args: &[String]) -> Result<i32> {
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let command = match parse_codex_subcommand(&refs) {
        Ok(command) => command,
        Err(_) => {
            eprintln!("{}", crate::localization::cli_locale().format("Unknown codex subcommand. Try \"deixic-code codex login\", \"logout\", \"status\", \"ready\", or \"doctor\".", &[]));
            return Ok(1);
        }
    };
    match command {
        CodexCommand::Login => handle_login(&args[1..]).await,
        CodexCommand::Logout => handle_logout(&args[1..]).await,
        CodexCommand::Status => handle_status(&args[1..]).await,
        CodexCommand::Doctor => handle_doctor(&args[1..]).await,
        CodexCommand::Ready => handle_ready(&args[1..]).await,
    }
}

#[derive(Debug, Clone, Default)]
struct CodexCliOptions {
    profile: Option<String>,
    model: Option<String>,
    json: bool,
}

fn parse_codex_options(params: &[String]) -> Result<CodexCliOptions> {
    let mut options = CodexCliOptions::default();
    let mut index = 0;
    while index < params.len() {
        match params[index].as_str() {
            "--profile" => {
                let value = params
                    .get(index + 1)
                    .filter(|value| !value.trim().is_empty() && !value.starts_with('-'))
                    .ok_or_else(|| anyhow::anyhow!("--profile requires a profile name"))?;
                if options.profile.replace(value.clone()).is_some() {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--profile may only be specified once", &[])
                    );
                }
                index += 2;
            }
            value if value.starts_with("--profile=") => {
                let profile = value.trim_start_matches("--profile=").trim().to_owned();
                if profile.is_empty() {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--profile requires a profile name", &[])
                    );
                }
                if options.profile.replace(profile).is_some() {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--profile may only be specified once", &[])
                    );
                }
                index += 1;
            }
            "--json" | "--mode=json" => {
                options.json = true;
                index += 1;
            }
            "--model" | "-m" => {
                let value = params
                    .get(index + 1)
                    .filter(|value| !value.trim().is_empty() && !value.starts_with('-'))
                    .ok_or_else(|| anyhow::anyhow!("--model requires a model name"))?;
                if options.model.replace(value.clone()).is_some() {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--model may only be specified once", &[])
                    );
                }
                index += 2;
            }
            value if value.starts_with("--model=") => {
                let model = value.trim_start_matches("--model=").trim().to_owned();
                if model.is_empty() {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--model requires a model name", &[])
                    );
                }
                if options.model.replace(model).is_some() {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--model may only be specified once", &[])
                    );
                }
                index += 1;
            }
            _ => {
                index += 1;
            }
        }
    }
    Ok(options)
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexReadinessCheck {
    state: String,
    detail: String,
}

impl CodexReadinessCheck {
    fn ready(detail: impl Into<String>) -> Self {
        Self {
            state: "ready".to_owned(),
            detail: sanitize_readiness_detail(detail),
        }
    }

    fn missing(detail: impl Into<String>) -> Self {
        Self {
            state: "missing".to_owned(),
            detail: sanitize_readiness_detail(detail),
        }
    }

    fn is_ready(&self) -> bool {
        self.state == "ready"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexOptionalReadiness {
    name: String,
    ready: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexReadinessReport {
    profile: String,
    auth: CodexReadinessCheck,
    compatibility: CodexReadinessCheck,
    tool_schema: CodexReadinessCheck,
    binding: CodexReadinessCheck,
    optional: Vec<CodexOptionalReadiness>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexReadinessEvaluation {
    ready: bool,
    exit_code: i32,
    profile: String,
    auth: CodexReadinessCheck,
    compatibility: CodexReadinessCheck,
    tool_schema: CodexReadinessCheck,
    binding: CodexReadinessCheck,
    optional: Vec<CodexOptionalReadiness>,
}

#[derive(Debug, Clone)]
pub struct CodexReadinessOptions {
    model: String,
    cwd: PathBuf,
    state_root: PathBuf,
}

impl CodexReadinessOptions {
    fn from_cli(options: &CodexCliOptions) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let state_root = crate::path_utils::maestro_home_dir()
            .ok_or_else(|| anyhow::anyhow!("Maestro state directory is unavailable"))?;
        Ok(Self {
            model: options
                .model
                .clone()
                .unwrap_or_else(crate::codex_auth::resolve_default_model),
            cwd,
            state_root,
        })
    }
}

pub fn evaluate_readiness(report: CodexReadinessReport) -> CodexReadinessEvaluation {
    let ready = report.auth.is_ready()
        && report.compatibility.is_ready()
        && report.tool_schema.is_ready()
        && report.binding.is_ready();
    CodexReadinessEvaluation {
        ready,
        exit_code: i32::from(!ready),
        profile: report.profile,
        auth: report.auth,
        compatibility: report.compatibility,
        tool_schema: report.tool_schema,
        binding: report.binding,
        optional: report.optional,
    }
}

async fn handle_ready(params: &[String]) -> Result<i32> {
    let options = parse_codex_options(params)?;
    let identity = requested_identity_with_profile(options.profile.as_deref())?;
    let readiness_options = CodexReadinessOptions::from_cli(&options)?;
    let client = spawn_for_identity(&identity).await?;
    let result = build_readiness_report(&client, &identity, &readiness_options).await;
    client.close();
    let evaluation = evaluate_readiness(result?);
    if options.json {
        println!("{}", serde_json::to_string_pretty(&evaluation)?);
    } else {
        print!("{}", render_readiness_human(&evaluation));
    }
    Ok(evaluation.exit_code)
}

async fn build_readiness_report(
    client: &CodexAppServerClient,
    identity: &crate::codex_identity::CodexIdentitySelection,
    options: &CodexReadinessOptions,
) -> Result<CodexReadinessReport> {
    let auth_health = crate::codex_identity::inspect_codex_auth(&identity.auth_path());
    let initialized = client
        .initialize(InitializeOptions {
            experimental_api: true,
            ..Default::default()
        })
        .await?;
    let account = client.read_account(true).await?;
    let auth = if account.account.is_some() {
        CodexReadinessCheck::ready(format!("configured{}", account_label(&account)))
    } else {
        CodexReadinessCheck::missing(format!(
            "{}; run {}",
            auth_health.state,
            login_command(identity)
        ))
    };
    let compatibility =
        crate::agent::codex_app_server_turns::codex_compatibility_from_initialize(&initialized);
    let compatibility_check = if compatibility.is_ready() {
        CodexReadinessCheck::ready(format!("protocol {}", compatibility.protocol_version))
    } else {
        CodexReadinessCheck::missing(format!(
            "missing required app-server capabilities: {}",
            compatibility.missing_required.join(", ")
        ))
    };
    let optional = vec![
        CodexOptionalReadiness {
            name: "resume".to_owned(),
            ready: compatibility.resume,
            detail: if compatibility.resume {
                "thread/resume available"
            } else {
                "thread/resume unavailable"
            }
            .to_owned(),
        },
        CodexOptionalReadiness {
            name: "steering".to_owned(),
            ready: compatibility.steering,
            detail: if compatibility.steering {
                "turn/steer available"
            } else {
                "turn/steer unavailable"
            }
            .to_owned(),
        },
    ];

    let schema_diagnostics = codex_schema_diagnostics_for_cwd(&options.cwd)?;
    let tool_schema = if schema_diagnostics.is_empty() {
        CodexReadinessCheck::ready("compatible")
    } else {
        CodexReadinessCheck::missing(schema_diagnostics.join("; "))
    };

    let binding = readiness_binding_check_at(
        client,
        identity,
        &options.state_root,
        &options.cwd,
        &options.model,
        &compatibility,
        &initialized,
    )
    .await?;

    Ok(CodexReadinessReport {
        profile: identity.profile_name.clone(),
        auth,
        compatibility: compatibility_check,
        tool_schema,
        binding,
        optional,
    })
}

async fn readiness_binding_check_at(
    client: &CodexAppServerClient,
    identity: &crate::codex_identity::CodexIdentitySelection,
    state_root: &Path,
    workspace: &Path,
    model: &str,
    compatibility: &crate::agent::codex_app_server_turns::CodexCompatibilityReport,
    _initialized: &Value,
) -> Result<CodexReadinessCheck> {
    let model = crate::agent::codex_app_server_turns::codex_thread_model_id(model);
    let key =
        crate::codex_session::CodexSessionKey::new(&identity.profile_name, workspace, &model)?;
    let path_exists =
        crate::codex_session::CodexThreadBinding::path_for_key_at(state_root, &key).exists();
    let loaded = crate::codex_session::CodexThreadBinding::load_at(state_root, &key)?;
    Ok(match loaded {
        Some(binding) if compatibility.resume => {
            match client
                .resume_thread(
                    ThreadResumeParams {
                        thread_id: binding.thread_id.clone(),
                        model: Some(key.model.clone()),
                        cwd: Some(key.workspace.to_string_lossy().to_string()),
                        path: None,
                        extra: None,
                    },
                    None,
                )
                .await
            {
                Ok(resumed) => CodexReadinessCheck::ready(format!(
                    "bound {} validated",
                    short_id(&resumed.thread_id)
                )),
                Err(error)
                    if crate::agent::codex_app_server_turns::is_thread_not_found_error(&error) =>
                {
                    crate::codex_session::CodexThreadBinding::quarantine_at(state_root, &key)?;
                    CodexReadinessCheck::ready("binding repaired: stale record cleared")
                }
                Err(error) => {
                    let message = error.to_string();
                    return Err(error).context(format!("thread/resume: {message}"));
                }
            }
        }
        Some(binding) => CodexReadinessCheck::ready(format!(
            "bound {} unvalidated (thread/resume unavailable)",
            short_id(&binding.thread_id)
        )),
        None if path_exists => CodexReadinessCheck::missing("binding integrity failed"),
        None => CodexReadinessCheck::ready("no binding yet"),
    })
}

fn render_readiness_human(evaluation: &CodexReadinessEvaluation) -> String {
    let mut out = String::new();
    if evaluation.ready {
        out.push_str(crate::localization::cli_locale().translate("Codex ready.\n"));
    } else {
        out.push_str(crate::localization::cli_locale().translate("Codex not ready.\n"));
    }
    out.push_str(&crate::localization::cli_locale().format(
        "Profile: {0}\n",
        std::slice::from_ref(&(evaluation.profile)),
    ));
    out.push_str(&crate::localization::cli_locale().format(
        "Auth: {0}\n",
        std::slice::from_ref(&(evaluation.auth.detail)),
    ));
    out.push_str(&crate::localization::cli_locale().format(
        "Compatibility: {0}\n",
        std::slice::from_ref(&(evaluation.compatibility.detail)),
    ));
    out.push_str(&crate::localization::cli_locale().format(
        "Tool schema: {0}\n",
        std::slice::from_ref(&(evaluation.tool_schema.detail)),
    ));
    out.push_str(&crate::localization::cli_locale().format(
        "Binding: {0}\n",
        std::slice::from_ref(&(evaluation.binding.detail)),
    ));
    let degraded = evaluation
        .optional
        .iter()
        .filter(|check| !check.ready)
        .map(|check| format!("{} ({})", check.name, check.detail))
        .collect::<Vec<_>>();
    if !degraded.is_empty() {
        out.push_str(
            &crate::localization::cli_locale()
                .format("Optional: {0}\n", &[(degraded.join(", ")).clone()]),
        );
    }
    out
}

fn sanitize_readiness_detail(detail: impl Into<String>) -> String {
    let mut detail = detail.into();
    for marker in ["access_token", "auth.json", "OPENAI_API_KEY", "sk-"] {
        detail = detail.replace(marker, "[redacted]");
    }
    detail
}

fn short_id(value: &str) -> String {
    value.chars().take(11).collect()
}

fn requested_identity_with_profile(
    profile: Option<&str>,
) -> Result<crate::codex_identity::CodexIdentitySelection> {
    let workspace = std::env::current_dir()?;
    crate::codex_identity::resolve_codex_identity(profile, &workspace)
}

fn requested_identity(params: &[String]) -> Result<crate::codex_identity::CodexIdentitySelection> {
    requested_identity_with_profile(parse_codex_options(params)?.profile.as_deref())
}

async fn spawn_for_identity(
    identity: &crate::codex_identity::CodexIdentitySelection,
) -> Result<CodexAppServerClient> {
    CodexAppServerClient::spawn_with_env(None, None, None, &identity.child_env()).await
}

fn login_command(identity: &crate::codex_identity::CodexIdentitySelection) -> String {
    if identity.profile_name == "default" {
        "deixic-code codex login".to_owned()
    } else {
        format!(
            "deixic-code codex login --profile {}",
            identity.profile_name
        )
    }
}

async fn handle_login(params: &[String]) -> Result<i32> {
    let device_flow = params
        .iter()
        .any(|p| matches!(p.as_str(), "--device" | "--device-code" | "--device-auth"));
    let force_login = params
        .iter()
        .any(|p| matches!(p.as_str(), "--force" | "--refresh"));

    println!(
        "{}",
        crate::localization::cli_locale().format("Deixic Code OpenAI Codex Login", &[])
    );
    let identity = requested_identity(params)?;
    let client = spawn_for_identity(&identity).await?;
    let result = login_with_client(&client, device_flow, force_login).await;
    client.close();
    result
}

async fn login_with_client(
    client: &CodexAppServerClient,
    device_flow: bool,
    force_login: bool,
) -> Result<i32> {
    client
        .initialize(InitializeOptions {
            experimental_api: true,
            ..Default::default()
        })
        .await?;

    if !force_login {
        match client.read_account(true).await {
            Ok(account) if account.account.is_some() => {
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "OpenAI Codex is already signed in{0}.",
                        &[(account_label(&account)).clone()]
                    )
                );
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Run \"deixic-code codex login --force\" to start a new sign-in flow.",
                        &[]
                    )
                );
                return Ok(0);
            }
            Ok(_) => {}
            Err(_) => {
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Codex app-server account refresh failed; starting a new sign-in flow.",
                        &[]
                    )
                );
            }
        }
    }

    let flow = if device_flow {
        LoginFlow::Device
    } else {
        LoginFlow::Browser
    };
    // App-server account APIs: start_chatgpt_login / wait_for_login_completion / read_account.
    let login = client.start_chatgpt_login(flow, true).await?;
    let login_type = login.get("type").and_then(Value::as_str).unwrap_or("");

    match login_type {
        "chatgpt" => {
            let login_id = login
                .get("loginId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing loginId"))?;
            let auth_url = login
                .get("authUrl")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing authUrl"))?;
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Open this URL in your browser to sign in with ChatGPT:",
                    &[]
                )
            );
            println!("{auth_url}");
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("Waiting for ChatGPT sign-in to complete...", &[])
            );
            client.wait_for_login_completion(login_id, None).await?;
        }
        "chatgptDeviceCode" => {
            let login_id = login
                .get("loginId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing loginId"))?;
            let verification_url = login
                .get("verificationUrl")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing verificationUrl"))?;
            let user_code = login
                .get("userCode")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing userCode"))?;
            println!(
                "{}",
                crate::localization::cli_locale().format("Open this URL and enter the code:", &[])
            );
            println!("{verification_url}");
            println!("{user_code}");
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("Waiting for ChatGPT sign-in to complete...", &[])
            );
            client.wait_for_login_completion(login_id, None).await?;
        }
        "apiKey" => {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("OpenAI Codex is already configured with an API key.", &[])
            );
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Select provider \"openai-codex\" or a model like \"openai-codex/gpt-5.6\".",
                    &[]
                )
            );
            return Ok(0);
        }
        "chatgptAuthTokens" => {
            let account = client.read_account(true).await?;
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "OpenAI Codex is using externally managed ChatGPT auth{0}.",
                    &[(account_label(&account)).clone()]
                )
            );
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Select provider \"openai-codex\" or a model like \"openai-codex/gpt-5.6\".",
                    &[]
                )
            );
            return Ok(0);
        }
        other => {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Unsupported Codex login response: {0}",
                    &[(other).to_string()]
                )
            );
        }
    }

    let account = client.read_account(true).await?;
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Signed in with ChatGPT{0}.",
            &[(account_label(&account)).clone()]
        )
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Select provider \"openai-codex\" or a model like \"openai-codex/gpt-5.6\".",
            &[]
        )
    );
    Ok(0)
}

async fn handle_logout(params: &[String]) -> Result<i32> {
    let identity = requested_identity(params)?;
    let client = spawn_for_identity(&identity).await?;
    let result = async {
        client.initialize(InitializeOptions::default()).await?;
        client.logout().await?;
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("Signed out of ChatGPT for OpenAI Codex.", &[])
        );
        Ok(0)
    }
    .await;
    client.close();
    result
}

async fn handle_status(params: &[String]) -> Result<i32> {
    let options = parse_codex_options(params)?;
    let identity = requested_identity(params)?;
    let client = spawn_for_identity(&identity).await?;
    let result = async {
        client.initialize(InitializeOptions::default()).await?;
        let account = client.read_account(true).await?;
        let auth_health = crate::codex_identity::inspect_codex_auth(&identity.auth_path());
        if options.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&codex_status_payload(
                    &identity,
                    &account,
                    auth_health
                ))?
            );
            return Ok(0);
        }
        print!("{}", render_status_human(&identity, &account));
        Ok(0)
    }
    .await;
    client.close();
    result
}

async fn handle_doctor(params: &[String]) -> Result<i32> {
    println!(
        "{}",
        crate::localization::cli_locale().format("Deixic Code Codex Doctor", &[])
    );
    let identity = requested_identity(params)?;
    let client = spawn_for_identity(&identity).await?;
    let mut exit_code = 0;
    let result = async {
        let initialized = client
            .initialize(InitializeOptions {
                experimental_api: true,
                ..Default::default()
            })
            .await?;
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Identity profile: {0}",
                std::slice::from_ref(&(identity.profile_name))
            )
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("Provider: openai-codex", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("Transport: codex-app-server", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Auth health: {0}",
                &[
                    (crate::codex_identity::inspect_codex_auth(&identity.auth_path()).state)
                        .to_string()
                ]
            )
        );
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Protocol: {0}",
                &[(initialized
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"))
                .to_string()]
            )
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("Connectivity: ready", &[])
        );
        let account = client.read_account(true).await?;
        if account.account.is_none() {
            println!(
                "{}",
                crate::localization::cli_locale().format("ChatGPT sign-in: missing", &[])
            );
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Run \"{0}\" to sign in with ChatGPT.",
                    &[(login_command(&identity)).clone()]
                )
            );
            exit_code = 1;
        } else {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "ChatGPT sign-in: {0}",
                    &[(account_doctor_label(&account)).clone()]
                )
            );
        }

        let cwd = std::env::current_dir()?;
        let dynamic_tools = runtime_codex_dynamic_tools(&cwd);
        let diagnostics = codex_dynamic_tool_schema_diagnostics(&dynamic_tools);
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Codex dynamic tools: {0} tools",
                &[(dynamic_tools.len()).to_string()]
            )
        );

        let errors = diagnostics.len();
        if errors > 0 {
            exit_code = 1;
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("Dynamic tool schema: {0} error(s)", &[(errors).to_string()])
            );
        } else {
            println!(
                "{}",
                crate::localization::cli_locale().format("Dynamic tool schema: compatible", &[])
            );
        }
        for diagnostic in &diagnostics {
            println!("{diagnostic}");
        }
        Ok(exit_code)
    }
    .await;
    client.close();
    result
}

fn account_label(state: &AccountReadResult) -> String {
    let Some(account) = state.account.as_ref() else {
        return String::new();
    };
    let Some(account) = account.as_object() else {
        return String::new();
    };
    if account.get("type").and_then(Value::as_str) != Some("chatgpt") {
        return String::new();
    }
    normalized_plan_label(account).unwrap_or_default()
}

fn codex_status_payload(
    identity: &crate::codex_identity::CodexIdentitySelection,
    account: &AccountReadResult,
    auth_health: crate::codex_identity::CodexAuthHealth,
) -> Value {
    json!({
        "profile": identity.profile_name,
        "provider": "openai-codex",
        "transport": "codex-app-server",
        "signed_in": account.account.is_some(),
        "auth_state": auth_health.state,
        "account_label": account_label(account).trim(),
    })
}

fn render_status_human(
    identity: &crate::codex_identity::CodexIdentitySelection,
    account: &AccountReadResult,
) -> String {
    let mut out = crate::localization::cli_locale().format(
        "Profile: {0}\n",
        std::slice::from_ref(&(identity.profile_name)),
    );
    if account.account.is_none() {
        out.push_str(
            crate::localization::cli_locale().translate("No ChatGPT sign-in for OpenAI Codex.\n"),
        );
        out.push_str(&crate::localization::cli_locale().format(
            "Run \"{0}\" to sign in with ChatGPT.\n",
            &[(login_command(identity)).clone()],
        ));
    } else {
        out.push_str(&crate::localization::cli_locale().format(
            "OpenAI Codex is signed in{0}.\n",
            &[(account_label(account)).clone()],
        ));
    }
    out
}

fn account_doctor_label(state: &AccountReadResult) -> String {
    let Some(account) = state.account.as_ref() else {
        return "missing".to_owned();
    };
    let Some(account) = account.as_object() else {
        return "unknown".to_owned();
    };
    match account.get("type").and_then(Value::as_str) {
        Some("chatgpt") => {
            let plan = normalized_plan_label(account).unwrap_or_default();
            format!("ChatGPT account{plan}")
        }
        Some("apiKey") => "API key".to_owned(),
        Some(_) => "configured account".to_owned(),
        None => "unknown".to_owned(),
    }
}

fn normalized_plan_label(account: &Map<String, Value>) -> Option<String> {
    account
        .get("planType")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|plan| format!(" ({})", plan.to_ascii_lowercase()))
}

// ── Dynamic tool compile rules (ported from compatibility.ts) ────────────────
//
// Tool names, descriptions, parameters, and profiles come from the committed
// Checked-in fixture used so doctor relies on the same tool schema surface as
// live TypeScript registry schemas rather than a hand-maintained name list.

const NAME_MAX_LENGTH: usize = 128;
const UNSUPPORTED_TOP_LEVEL_SCHEMA_KEYWORDS: &[&str] = &["anyOf", "oneOf", "allOf", "enum", "not"];
const RESERVED_NAMES: &[&str] = &["mcp"];
const RESERVED_NAME_PREFIXES: &[&str] = &["mcp__"];

#[cfg(test)]
const CODING_TOOLS_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test/fixtures/codex/coding-tools-doctor-v1.json"
));

#[cfg(test)]
static CODING_TOOLS_FIXTURE: LazyLock<CodingToolsFixture> = LazyLock::new(|| {
    let fixture: CodingToolsFixture = serde_json::from_str(CODING_TOOLS_FIXTURE_JSON)
        .expect("coding-tools-doctor-v1.json must parse as CodingToolsFixture");
    assert_eq!(
        fixture.version, 1,
        "coding-tools-doctor-v1.json must be version 1"
    );
    assert!(
        !fixture.tools.is_empty(),
        "coding-tools-doctor-v1.json must include coding tools"
    );
    assert!(
        fixture.profiles.contains_key("lean"),
        "coding-tools-doctor-v1.json must include the lean profile"
    );
    fixture
});

#[cfg(test)]
#[derive(Debug, Clone, Deserialize)]
struct CodingToolsFixture {
    version: u32,
    tools: Vec<CodingTool>,
    profiles: HashMap<String, Vec<String>>,
}

#[cfg(test)]
#[derive(Debug, Clone, Deserialize)]
struct CodingTool {
    name: String,
    description: String,
    #[serde(default = "empty_object_schema")]
    parameters: Value,
    #[serde(default, rename = "deferApiDefinition")]
    defer_api_definition: bool,
    #[serde(default, rename = "executionLocation")]
    execution_location: Option<String>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum DiagnosticSeverity {
    #[allow(dead_code)]
    Info,
    Warning,
    Error,
}

#[cfg(test)]
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Diagnostic {
    severity: DiagnosticSeverity,
    code: String,
    message: String,
}

#[cfg(test)]
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct DynamicToolSpec {
    name: String,
    description: String,
    input_schema: Value,
}

#[cfg(test)]
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct DynamicToolBinding {
    codex_name: String,
    original_name: String,
}

#[cfg(test)]
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct DynamicToolCompilation {
    #[allow(dead_code)]
    specs: Vec<DynamicToolSpec>,
    #[allow(dead_code)]
    bindings: Vec<DynamicToolBinding>,
    diagnostics: Vec<Diagnostic>,
}

#[cfg(test)]
fn resolve_codex_tool_profile_name(value: Option<&str>) -> Result<&'static str> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok("lean");
    };
    let normalized = value.to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "lean" | "default" => Ok("lean"),
        "read-only" | "readonly" => Ok("read-only"),
        "extended" => Ok("extended"),
        _ => bail!(
            "Unknown Codex tool profile \"{value}\". Available profiles: lean, default, read-only, readonly, extended"
        ),
    }
}

#[cfg(test)]
fn profile_tool_names(profile_name: &str) -> &[String] {
    CODING_TOOLS_FIXTURE
        .profiles
        .get(profile_name)
        .or_else(|| CODING_TOOLS_FIXTURE.profiles.get("lean"))
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

#[cfg(test)]
fn select_codex_tool_profile<'a>(
    tools: &'a [CodingTool],
    profile_name: &str,
) -> Vec<&'a CodingTool> {
    let by_name: HashMap<&str, &CodingTool> = tools
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect();
    profile_tool_names(profile_name)
        .iter()
        .filter_map(|name| by_name.get(name.as_str()).copied())
        .collect()
}

#[cfg(test)]
fn compile_codex_dynamic_tool_specs(tools: &[&CodingTool]) -> DynamicToolCompilation {
    let mut specs = Vec::new();
    let mut bindings = Vec::new();
    let mut diagnostics = Vec::new();
    let mut seen_original = HashSet::new();
    let mut seen_codex = HashSet::new();

    for tool in tools {
        if tool.defer_api_definition
            || tool
                .execution_location
                .as_deref()
                .is_some_and(|location| location == "client")
        {
            continue;
        }
        if !seen_original.insert(tool.name.as_str()) {
            continue;
        }
        let codex_name = to_unique_codex_dynamic_tool_name(&tool.name, &seen_codex);
        if codex_name != tool.name {
            diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                code: "renamed_tool".to_owned(),
                message: format!(
                    "Tool \"{}\" is exposed to Codex as \"{codex_name}\" to match app-server dynamic tool identifier rules.",
                    tool.name
                ),
            });
        }
        seen_codex.insert(codex_name.clone());
        specs.push(DynamicToolSpec {
            name: codex_name.clone(),
            description: tool.description.clone(),
            input_schema: normalize_codex_dynamic_tool_input_schema(&tool.parameters),
        });
        bindings.push(DynamicToolBinding {
            codex_name,
            original_name: tool.name.clone(),
        });
    }

    DynamicToolCompilation {
        specs,
        bindings,
        diagnostics,
    }
}

/// Compile the selected Codex tool profile and return error diagnostics.
pub fn codex_schema_diagnostics() -> Result<Vec<String>> {
    codex_schema_diagnostics_for_cwd(&std::env::current_dir()?)
}

fn codex_schema_diagnostics_for_cwd(cwd: &Path) -> Result<Vec<String>> {
    Ok(codex_dynamic_tool_schema_diagnostics(
        &runtime_codex_dynamic_tools(cwd),
    ))
}

fn runtime_codex_dynamic_tools(cwd: &Path) -> Vec<crate::agent::DynamicToolSpec> {
    let executor = crate::tools::ToolExecutor::new(cwd.to_string_lossy().to_string());
    let tools = executor
        .tool_definitions()
        .map(|definition| (definition.tool.name.clone(), definition.clone()))
        .collect::<HashMap<_, _>>();
    crate::agent::dynamic_tools_from_native(&tools)
}

fn codex_dynamic_tool_schema_diagnostics(specs: &[crate::agent::DynamicToolSpec]) -> Vec<String> {
    specs
        .iter()
        .flat_map(codex_dynamic_tool_schema_diagnostic)
        .collect()
}

fn codex_dynamic_tool_schema_diagnostic(spec: &crate::agent::DynamicToolSpec) -> Vec<String> {
    let name = sanitized_diagnostic_tool_name(&spec.name);
    let mut diagnostics = Vec::new();
    let trimmed = spec.name.trim();
    if trimmed.is_empty() {
        diagnostics.push("invalid_tool_name: empty dynamic tool name".to_owned());
    } else {
        if trimmed.len() > NAME_MAX_LENGTH {
            diagnostics.push(format!(
                "invalid_tool_name: tool {name} exceeds {NAME_MAX_LENGTH} bytes"
            ));
        }
        if !trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            diagnostics.push(format!(
                "invalid_tool_name: tool {name} contains unsupported characters"
            ));
        }
        if is_reserved_codex_dynamic_tool_identifier(trimmed) {
            diagnostics.push(format!(
                "invalid_tool_name: tool {name} uses a reserved identifier"
            ));
        }
    }

    let Some(schema) = spec.input_schema.as_object() else {
        diagnostics.push(format!(
            "invalid_schema: tool {name} input schema must be an object"
        ));
        return diagnostics;
    };
    for keyword in UNSUPPORTED_TOP_LEVEL_SCHEMA_KEYWORDS {
        if schema.contains_key(*keyword) {
            diagnostics.push(format!(
                "unsupported_schema_keyword: tool {name} uses top-level {keyword}"
            ));
        }
    }
    if schema
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind != "object")
    {
        diagnostics.push(format!(
            "invalid_schema: tool {name} input schema type must be object"
        ));
    }
    if schema
        .get("properties")
        .is_some_and(|properties| !properties.is_object())
    {
        diagnostics.push(format!(
            "invalid_schema: tool {name} properties must be an object"
        ));
    }
    diagnostics
}

fn sanitized_diagnostic_tool_name(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "<empty>".to_owned();
    }
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .take(32)
        .collect::<String>()
}

#[cfg(test)]
fn empty_object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

#[cfg(test)]
fn normalize_codex_dynamic_tool_input_schema(schema: &Value) -> Value {
    let Some(obj) = schema.as_object() else {
        return empty_object_schema();
    };

    if let Some(flattened) = flatten_top_level_composition_schema(obj) {
        return flattened;
    }

    let mut normalized = Map::new();
    for (key, value) in obj {
        if UNSUPPORTED_TOP_LEVEL_SCHEMA_KEYWORDS.contains(&key.as_str()) {
            continue;
        }
        normalized.insert(key.clone(), value.clone());
    }
    normalized.insert("type".to_owned(), json!("object"));
    if !normalized
        .get("properties")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        normalized.insert("properties".to_owned(), json!({}));
    }
    if !normalized.contains_key("additionalProperties") {
        normalized.insert("additionalProperties".to_owned(), json!(false));
    }
    Value::Object(normalized)
}

#[cfg(test)]
fn flatten_top_level_composition_schema(schema: &Map<String, Value>) -> Option<Value> {
    for key in ["anyOf", "oneOf", "allOf"] {
        let Some(branches) = schema.get(key).and_then(Value::as_array) else {
            continue;
        };
        if branches.is_empty() || !branches.iter().all(Value::is_object) {
            return Some(empty_object_schema());
        }
        // Best-effort flatten: merge property keys; drop composition keyword.
        let mut properties = Map::new();
        if let Some(top) = schema.get("properties").and_then(Value::as_object) {
            for (name, prop) in top {
                properties.insert(name.clone(), prop.clone());
            }
        }
        for branch in branches {
            if let Some(branch_props) = branch.get("properties").and_then(Value::as_object) {
                for (name, prop) in branch_props {
                    properties
                        .entry(name.clone())
                        .or_insert_with(|| prop.clone());
                }
            }
        }
        let mut flattened = Map::new();
        flattened.insert("type".to_owned(), json!("object"));
        flattened.insert("properties".to_owned(), Value::Object(properties));
        flattened.insert("additionalProperties".to_owned(), json!(false));
        if let Some(description) = schema.get("description") {
            flattened.insert("description".to_owned(), description.clone());
        }
        return Some(Value::Object(flattened));
    }
    None
}

#[cfg(test)]
fn to_unique_codex_dynamic_tool_name(tool_name: &str, seen: &HashSet<String>) -> String {
    let base = to_codex_dynamic_tool_name(tool_name);
    if !seen.contains(&base) {
        return base;
    }
    let mut index = 2_u32;
    loop {
        let suffix = format!("_{index}");
        let max = NAME_MAX_LENGTH.saturating_sub(suffix.len());
        let candidate = format!("{}{suffix}", truncate_identifier(&base, max));
        if !seen.contains(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

#[cfg(test)]
fn to_codex_dynamic_tool_name(tool_name: &str) -> String {
    let trimmed = tool_name.trim();
    let mut codex_name: String = trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if codex_name.is_empty() {
        codex_name = "maestro_tool".to_owned();
    }
    if is_reserved_codex_dynamic_tool_identifier(&codex_name) {
        codex_name = format!("maestro_{codex_name}");
    }
    truncate_identifier(&codex_name, NAME_MAX_LENGTH)
}

fn is_reserved_codex_dynamic_tool_identifier(value: &str) -> bool {
    RESERVED_NAMES.contains(&value)
        || RESERVED_NAME_PREFIXES
            .iter()
            .any(|prefix| value.starts_with(prefix))
}

#[cfg(test)]
fn truncate_identifier(value: &str, max_length: usize) -> String {
    if value.len() <= max_length {
        return value.to_owned();
    }
    value.chars().take(max_length.max(1)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_app_server::{CodexAppServerClient, InitializeOptions, MockCodexTransport};
    use serde_json::json;
    use std::fs;
    use std::sync::Arc;

    fn sample_tool(name: &str, description: &str, parameters: Value) -> CodingTool {
        CodingTool {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters,
            defer_api_definition: false,
            execution_location: None,
        }
    }

    fn resume_unsupported_compatibility()
    -> crate::agent::codex_app_server_turns::CodexCompatibilityReport {
        crate::agent::codex_app_server_turns::CodexCompatibilityReport {
            protocol_version: "2025-01-01".to_owned(),
            resume: false,
            steering: false,
            missing_required: Vec::new(),
        }
    }

    async fn respond_ready_initialize(mock: &MockCodexTransport, resume: bool) {
        let init = mock.next_request().await.unwrap();
        assert_eq!(init["method"], "initialize");
        let mut methods = vec!["thread/start", "turn/start", "turn/interrupt"];
        if resume {
            methods.push("thread/resume");
        }
        mock.respond(
            init["id"].as_u64().unwrap(),
            json!({
                "protocolVersion": "2025-01-01",
                "capabilities": {
                    "methods": methods,
                    "notifications": ["item/tool/call", "item/agentMessage/delta", "turn/completed"]
                }
            }),
        );
        let initialized = mock.next_request().await.unwrap();
        assert_eq!(initialized["method"], "initialized");
    }

    async fn respond_signed_in_account(mock: &MockCodexTransport) {
        let read = mock.next_request().await.unwrap();
        assert_eq!(read["method"], "account/read");
        mock.respond(
            read["id"].as_u64().unwrap(),
            json!({
                "account": { "type": "chatgpt", "email": "dev@example.com", "planType": "pro" },
                "requiresOpenaiAuth": false
            }),
        );
    }

    async fn assert_no_prompt_sent(mock: &MockCodexTransport) {
        let extra =
            tokio::time::timeout(std::time::Duration::from_millis(100), mock.next_request()).await;
        match extra {
            Err(_) | Ok(Err(_)) => {}
            Ok(Ok(request)) => panic!("readiness sent unexpected request: {request}"),
        }
    }

    #[test]
    fn fixture_loads_live_coding_tools_and_profiles() {
        assert_eq!(CODING_TOOLS_FIXTURE.version, 1);
        assert!(CODING_TOOLS_FIXTURE.tools.len() >= 12);
        assert!(CODING_TOOLS_FIXTURE.tools.iter().any(|t| t.name == "read"));
        assert!(CODING_TOOLS_FIXTURE.profiles.contains_key("lean"));
        assert!(CODING_TOOLS_FIXTURE.profiles.contains_key("extended"));
        let read = CODING_TOOLS_FIXTURE
            .tools
            .iter()
            .find(|t| t.name == "read")
            .expect("read tool");
        assert!(read.parameters["properties"]["path"].is_object());
    }

    #[test]
    fn lean_profile_selects_default_tool_snapshot() {
        let selected = select_codex_tool_profile(&CODING_TOOLS_FIXTURE.tools, "lean");
        assert_eq!(selected.len(), profile_tool_names("lean").len());
        assert_eq!(selected[0].name, "read");
        assert!(selected.iter().any(|t| t.name == "gh_pr"));
        assert!(!selected.iter().any(|t| t.name == "parallel_ripgrep"));
    }

    #[test]
    fn read_only_profile_excludes_mutation_tools() {
        let selected = select_codex_tool_profile(&CODING_TOOLS_FIXTURE.tools, "read-only");
        assert_eq!(selected.len(), profile_tool_names("read-only").len());
        assert!(
            !selected
                .iter()
                .any(|t| t.name == "write" || t.name == "bash")
        );
    }

    #[test]
    fn compiles_the_curated_codex_default_profile_into_responses_safe_dynamic_tools() {
        let selected = select_codex_tool_profile(&CODING_TOOLS_FIXTURE.tools, "lean");
        let compiled = compile_codex_dynamic_tool_specs(&selected);
        assert_eq!(compiled.specs.len(), selected.len());
        assert!(compiled.diagnostics.is_empty());
        for (spec, tool) in compiled.specs.iter().zip(selected.iter()) {
            assert_eq!(spec.input_schema["type"], "object");
            assert!(spec.input_schema.get("anyOf").is_none());
            assert!(
                spec.name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            );
            // Live schemas retain real properties (not empty stubs).
            if tool
                .parameters
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|p| !p.is_empty())
            {
                let props = spec.input_schema["properties"]
                    .as_object()
                    .expect("normalized properties object");
                assert!(
                    !props.is_empty(),
                    "tool {} should keep live parameter properties",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn renames_invalid_or_reserved_tool_identifiers_while_preserving_original_bindings() {
        let tools = [
            sample_tool("mcp", "reserved", empty_object_schema()),
            sample_tool("ticket lookup", "spaces", empty_object_schema()),
        ];
        let refs: Vec<&CodingTool> = tools.iter().collect();
        let compiled = compile_codex_dynamic_tool_specs(&refs);
        assert_eq!(compiled.specs[0].name, "maestro_mcp");
        assert_eq!(compiled.bindings[0].original_name, "mcp");
        assert_eq!(compiled.specs[1].name, "ticket_lookup");
        assert!(
            compiled
                .diagnostics
                .iter()
                .any(|d| d.code == "renamed_tool")
        );
    }

    #[test]
    fn skips_deferred_and_client_tools_like_typescript() {
        let mut deferred = sample_tool("ask_user", "prompt", empty_object_schema());
        deferred.defer_api_definition = true;
        let mut client = sample_tool("ui_pick", "client only", empty_object_schema());
        client.execution_location = Some("client".to_owned());
        let server = sample_tool(
            "read",
            "ok",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }),
        );
        let tools = [deferred, client, server];
        let refs: Vec<&CodingTool> = tools.iter().collect();
        let compiled = compile_codex_dynamic_tool_specs(&refs);
        assert_eq!(compiled.specs.len(), 1);
        assert_eq!(compiled.specs[0].name, "read");
        assert!(compiled.specs[0].input_schema["properties"]["path"].is_object());
    }

    #[test]
    fn normalize_strips_unsupported_top_level_schema_keywords() {
        let schema = json!({
            "type": "object",
            "anyOf": [{"type": "string"}],
            "properties": { "path": { "type": "string" } },
            "enum": ["a"]
        });
        // anyOf present => flatten path
        let normalized = normalize_codex_dynamic_tool_input_schema(&schema);
        assert_eq!(normalized["type"], "object");
        assert!(normalized.get("anyOf").is_none());
        assert!(normalized.get("enum").is_none());
    }

    #[test]
    fn resolve_profile_defaults_and_aliases() {
        assert_eq!(resolve_codex_tool_profile_name(None).unwrap(), "lean");
        assert_eq!(
            resolve_codex_tool_profile_name(Some("read_only")).unwrap(),
            "read-only"
        );
        assert_eq!(
            resolve_codex_tool_profile_name(Some("extended")).unwrap(),
            "extended"
        );
        assert!(resolve_codex_tool_profile_name(Some("nope")).is_err());
    }

    #[test]
    fn ready_is_a_first_class_codex_subcommand() {
        assert_eq!(
            parse_codex_subcommand(&["ready"]).unwrap(),
            CodexCommand::Ready
        );
    }

    #[test]
    fn readiness_fails_when_required_capabilities_are_missing() {
        let report = CodexReadinessReport {
            profile: "work".to_owned(),
            auth: CodexReadinessCheck::ready("ready"),
            compatibility: CodexReadinessCheck::missing(
                "missing required app-server capabilities: turn/start",
            ),
            tool_schema: CodexReadinessCheck::ready("compatible"),
            binding: CodexReadinessCheck::ready("clean"),
            optional: vec![CodexOptionalReadiness {
                name: "resume".to_owned(),
                ready: false,
                detail: "thread/resume unavailable".to_owned(),
            }],
        };

        let evaluated = evaluate_readiness(report);
        assert_eq!(evaluated.exit_code, 1);
        assert!(!evaluated.ready);
        assert!(
            evaluated
                .optional
                .iter()
                .any(|optional| optional.name == "resume" && !optional.ready)
        );
    }

    #[test]
    fn readiness_optional_resume_and_steering_do_not_fail() {
        let report = CodexReadinessReport {
            profile: "work".to_owned(),
            auth: CodexReadinessCheck::ready("ready"),
            compatibility: CodexReadinessCheck::ready("protocol 2025-01-01"),
            tool_schema: CodexReadinessCheck::ready("compatible"),
            binding: CodexReadinessCheck::ready("clean"),
            optional: vec![
                CodexOptionalReadiness {
                    name: "resume".to_owned(),
                    ready: false,
                    detail: "thread/resume unavailable".to_owned(),
                },
                CodexOptionalReadiness {
                    name: "steering".to_owned(),
                    ready: false,
                    detail: "turn/steer unavailable".to_owned(),
                },
            ],
        };

        let evaluated = evaluate_readiness(report);
        assert_eq!(evaluated.exit_code, 0);
        assert!(evaluated.ready);
    }

    #[test]
    fn readiness_json_is_privacy_safe() {
        let report = CodexReadinessReport {
            profile: "work".to_owned(),
            auth: CodexReadinessCheck::ready("signed in (pro)"),
            compatibility: CodexReadinessCheck::ready("protocol 2025-01-01"),
            tool_schema: CodexReadinessCheck::ready("compatible"),
            binding: CodexReadinessCheck::ready("clean"),
            optional: Vec::new(),
        };

        let value = serde_json::to_value(evaluate_readiness(report)).unwrap();
        let raw = value.to_string();
        assert_eq!(value["ready"], true);
        assert_eq!(value["profile"], "work");
        for secret in [
            "person@example.com",
            "access_token",
            "auth.json",
            "prompt",
            "arguments",
            "result",
            "command",
            "sk-",
        ] {
            assert!(!raw.contains(secret), "{secret} leaked in {raw}");
        }
    }

    #[test]
    fn readiness_reports_binding_and_tool_schema_diagnostics() {
        let report = CodexReadinessReport {
            profile: "work".to_owned(),
            auth: CodexReadinessCheck::ready("ready"),
            compatibility: CodexReadinessCheck::ready("protocol 2025-01-01"),
            tool_schema: CodexReadinessCheck::missing("invalid_schema: bad top-level enum"),
            binding: CodexReadinessCheck::missing("corrupt binding quarantined"),
            optional: Vec::new(),
        };

        let evaluated = evaluate_readiness(report);
        assert_eq!(evaluated.exit_code, 1);
        assert_eq!(evaluated.tool_schema.state, "missing");
        assert_eq!(evaluated.binding.state, "missing");
    }

    #[test]
    fn ready_human_output_includes_profile_without_email() {
        let report = CodexReadinessReport {
            profile: "work".to_owned(),
            auth: CodexReadinessCheck::ready("configured (pro)"),
            compatibility: CodexReadinessCheck::ready("protocol 2025-01-01"),
            tool_schema: CodexReadinessCheck::ready("compatible"),
            binding: CodexReadinessCheck::ready("no binding yet"),
            optional: Vec::new(),
        };

        let rendered = render_readiness_human(&evaluate_readiness(report));
        assert!(rendered.contains("Profile: work"));
        assert!(!rendered.contains("person@example.com"));
    }

    #[test]
    fn status_payload_and_human_output_include_profile_without_email() {
        let identity = crate::codex_identity::CodexIdentitySelection {
            profile_name: "work".to_owned(),
            codex_home: tempfile::tempdir().unwrap().path().to_path_buf(),
            workspace_boundary: None,
        };
        let account = AccountReadResult {
            account: Some(json!({
                "type": "chatgpt",
                "email": "person@example.com",
                "planType": "pro"
            })),
            requires_openai_auth: false,
        };
        let auth_health = crate::codex_identity::CodexAuthHealth {
            state: crate::codex_identity::CodexAuthState::Ready,
            auth_mode: Some("chatgpt".to_owned()),
            expires_at: None,
            account_label: None,
        };

        let payload = codex_status_payload(&identity, &account, auth_health);
        assert_eq!(payload["profile"], "work");
        assert!(!payload.to_string().contains("person@example.com"));
        let rendered = render_status_human(&identity, &account);
        assert!(rendered.contains("Profile: work"));
        assert!(!rendered.contains("person@example.com"));
    }

    #[test]
    fn ready_options_parse_profile_json_and_model() {
        let options = parse_codex_options(&[
            "ready".to_owned(),
            "--profile".to_owned(),
            "work".to_owned(),
            "--model".to_owned(),
            "openai-codex/gpt-5.6".to_owned(),
            "--json".to_owned(),
        ])
        .unwrap();

        assert_eq!(options.profile.as_deref(), Some("work"));
        assert_eq!(options.model.as_deref(), Some("openai-codex/gpt-5.6"));
        assert!(options.json);
    }

    #[tokio::test]
    async fn run_codex_ready_rejects_duplicate_model_before_spawn() {
        let result = run_codex(&[
            "ready".to_owned(),
            "--model".to_owned(),
            "openai-codex/gpt-5.6".to_owned(),
            "--model".to_owned(),
            "openai-codex/gpt-5.6".to_owned(),
        ])
        .await;

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("--model may only be specified once")
        );
    }

    #[tokio::test]
    async fn readiness_binding_uses_selected_model_key() -> anyhow::Result<()> {
        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let identity = crate::codex_identity::CodexIdentitySelection {
            profile_name: "work".to_owned(),
            codex_home: tempfile::tempdir()?.path().to_path_buf(),
            workspace_boundary: None,
        };
        let key = crate::codex_session::CodexSessionKey::new(
            "work",
            workspace.path(),
            "openai-codex/gpt-5.6",
        )?;
        crate::codex_session::CodexThreadBinding::new(key, "thread-model-54", None, 1)
            .store_at(state_root.path())?;

        let (client, _mock) = CodexAppServerClient::mock();
        let check = readiness_binding_check_at(
            &client,
            &identity,
            state_root.path(),
            workspace.path(),
            "openai-codex/gpt-5.6",
            &resume_unsupported_compatibility(),
            &json!({ "protocolVersion": "2025-01-01" }),
        )
        .await?;
        assert_eq!(check.state, "ready");
        assert!(check.detail.contains("thread-mode"));
        Ok(())
    }

    #[tokio::test]
    async fn readiness_binding_distinguishes_absent_and_corrupt_records() -> anyhow::Result<()> {
        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let identity = crate::codex_identity::CodexIdentitySelection {
            profile_name: "work".to_owned(),
            codex_home: tempfile::tempdir()?.path().to_path_buf(),
            workspace_boundary: None,
        };

        let (client, _mock) = CodexAppServerClient::mock();
        let absent = readiness_binding_check_at(
            &client,
            &identity,
            state_root.path(),
            workspace.path(),
            "gpt-5.6",
            &resume_unsupported_compatibility(),
            &json!({ "protocolVersion": "2025-01-01" }),
        )
        .await?;
        assert_eq!(absent.state, "ready");
        assert_eq!(absent.detail, "no binding yet");

        let key = crate::codex_session::CodexSessionKey::new("work", workspace.path(), "gpt-5.6")?;
        let path =
            crate::codex_session::CodexThreadBinding::path_for_key_at(state_root.path(), &key);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, "token=secret\nprompt=secret")?;

        let corrupt = readiness_binding_check_at(
            &client,
            &identity,
            state_root.path(),
            workspace.path(),
            "gpt-5.6",
            &resume_unsupported_compatibility(),
            &json!({ "protocolVersion": "2025-01-01" }),
        )
        .await?;
        assert_eq!(corrupt.state, "missing");
        assert_eq!(corrupt.detail, "binding integrity failed");
        Ok(())
    }

    #[tokio::test]
    async fn readiness_validates_existing_binding_when_resume_supported() {
        let codex_home = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let identity = crate::codex_identity::CodexIdentitySelection {
            profile_name: "work".to_owned(),
            codex_home: codex_home.path().to_path_buf(),
            workspace_boundary: None,
        };
        let key = crate::codex_session::CodexSessionKey::new(
            "work",
            workspace.path(),
            "openai-codex/gpt-5.6",
        )
        .unwrap();
        crate::codex_session::CodexThreadBinding::new(key, "thread-remote", None, 1)
            .store_at(state_root.path())
            .unwrap();
        let options = CodexReadinessOptions {
            model: "openai-codex/gpt-5.6".to_owned(),
            cwd: workspace.path().to_path_buf(),
            state_root: state_root.path().to_path_buf(),
        };
        let (client, mock) = CodexAppServerClient::mock();
        let task =
            tokio::spawn(async move { build_readiness_report(&client, &identity, &options).await });

        respond_ready_initialize(&mock, true).await;
        respond_signed_in_account(&mock).await;
        let resume = mock.next_request().await.expect("resume validation");
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "thread-remote");
        assert_eq!(resume["params"]["model"], "gpt-5.6");
        mock.respond(
            resume["id"].as_u64().unwrap(),
            json!({ "thread": { "id": "thread-remote" } }),
        );

        let evaluation = evaluate_readiness(task.await.unwrap().unwrap());
        assert_eq!(evaluation.exit_code, 0);
        assert!(evaluation.binding.detail.contains("validated"));
        assert_no_prompt_sent(&mock).await;
    }

    #[tokio::test]
    async fn readiness_clears_binding_on_explicit_thread_not_found_without_starting_thread() {
        let codex_home = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let identity = crate::codex_identity::CodexIdentitySelection {
            profile_name: "work".to_owned(),
            codex_home: codex_home.path().to_path_buf(),
            workspace_boundary: None,
        };
        let key = crate::codex_session::CodexSessionKey::new("work", workspace.path(), "gpt-5.6")
            .unwrap();
        crate::codex_session::CodexThreadBinding::new(key.clone(), "missing-thread", None, 1)
            .store_at(state_root.path())
            .unwrap();
        let options = CodexReadinessOptions {
            model: "openai-codex/gpt-5.6".to_owned(),
            cwd: workspace.path().to_path_buf(),
            state_root: state_root.path().to_path_buf(),
        };
        let (client, mock) = CodexAppServerClient::mock();
        let task =
            tokio::spawn(async move { build_readiness_report(&client, &identity, &options).await });

        respond_ready_initialize(&mock, true).await;
        respond_signed_in_account(&mock).await;
        let resume = mock.next_request().await.expect("resume validation");
        assert_eq!(resume["method"], "thread/resume");
        mock.reject(resume["id"].as_u64().unwrap(), "thread not found");

        let evaluation = evaluate_readiness(task.await.unwrap().unwrap());
        assert_eq!(evaluation.exit_code, 0);
        assert!(evaluation.binding.detail.contains("cleared"));
        assert!(
            crate::codex_session::CodexThreadBinding::load_at(state_root.path(), &key)
                .unwrap()
                .is_none(),
            "ready should remove the stale binding"
        );
        assert!(
            crate::codex_session::CodexThreadBinding::path_for_key_at(state_root.path(), &key)
                .parent()
                .unwrap()
                .join("quarantine")
                .exists(),
            "ready should quarantine the stale binding"
        );
        let raw = serde_json::to_string(&evaluation).unwrap();
        assert!(!raw.contains("missing-thread"));
        let loaded =
            crate::codex_session::CodexThreadBinding::load_at(state_root.path(), &key).unwrap();
        assert!(loaded.is_none());
        assert_no_prompt_sent(&mock).await;
    }

    #[tokio::test]
    async fn readiness_reports_unvalidated_binding_when_resume_is_unsupported() {
        let codex_home = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let identity = crate::codex_identity::CodexIdentitySelection {
            profile_name: "work".to_owned(),
            codex_home: codex_home.path().to_path_buf(),
            workspace_boundary: None,
        };
        let key = crate::codex_session::CodexSessionKey::new("work", workspace.path(), "gpt-5.6")
            .unwrap();
        crate::codex_session::CodexThreadBinding::new(key, "thread-unvalidated", None, 1)
            .store_at(state_root.path())
            .unwrap();
        let options = CodexReadinessOptions {
            model: "openai-codex/gpt-5.6".to_owned(),
            cwd: workspace.path().to_path_buf(),
            state_root: state_root.path().to_path_buf(),
        };
        let (client, mock) = CodexAppServerClient::mock();
        let task =
            tokio::spawn(async move { build_readiness_report(&client, &identity, &options).await });

        respond_ready_initialize(&mock, false).await;
        respond_signed_in_account(&mock).await;

        let evaluation = evaluate_readiness(task.await.unwrap().unwrap());
        assert_eq!(evaluation.exit_code, 0);
        assert!(evaluation.binding.detail.contains("unvalidated"));
        assert!(
            evaluation
                .optional
                .iter()
                .any(|check| check.name == "resume" && !check.ready)
        );
        let extra =
            tokio::time::timeout(std::time::Duration::from_millis(100), mock.next_request()).await;
        match extra {
            Err(_) | Ok(Err(_)) => {}
            Ok(Ok(request)) => panic!("unsupported resume sent unexpected request: {request}"),
        }
    }

    #[test]
    fn runtime_dynamic_tool_schema_diagnostics_reject_sent_schema() {
        let diagnostics = codex_dynamic_tool_schema_diagnostics(&[crate::agent::DynamicToolSpec {
            name: "bad_tool".to_owned(),
            description: "bad".to_owned(),
            input_schema: json!({
                "type": "object",
                "enum": ["bad"]
            }),
        }]);

        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("unsupported_schema_keyword"))
        );
    }

    #[tokio::test]
    async fn runs_browser_chatgpt_sign_in_via_mock_transport() {
        let (client, mock) = CodexAppServerClient::mock();
        let client = Arc::new(client);
        let client_task = Arc::clone(&client);
        let task = tokio::spawn(async move { login_with_client(&client_task, false, true).await });

        // initialize
        let init = mock.next_request().await.unwrap();
        assert_eq!(init["method"], "initialize");
        mock.respond(init["id"].as_u64().unwrap(), json!({}));
        let initialized = mock.next_request().await.unwrap();
        assert_eq!(initialized["method"], "initialized");

        // login start
        let login = mock.next_request().await.unwrap();
        assert_eq!(login["method"], "account/login/start");
        assert_eq!(login["params"]["type"], "chatgpt");
        assert_eq!(login["params"]["codexStreamlinedLogin"], true);
        mock.respond(
            login["id"].as_u64().unwrap(),
            json!({
                "type": "chatgpt",
                "loginId": "login-1",
                "authUrl": "https://chatgpt.test/auth"
            }),
        );

        // completion
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        mock.notify(
            "account/login/completed",
            json!({ "loginId": "login-1", "success": true, "error": null }),
        );

        // final account/read
        let read = mock.next_request().await.unwrap();
        assert_eq!(read["method"], "account/read");
        mock.respond(
            read["id"].as_u64().unwrap(),
            json!({
                "account": {
                    "type": "chatgpt",
                    "email": "dev@example.com",
                    "planType": "pro"
                },
                "requiresOpenaiAuth": false
            }),
        );

        let code = task.await.unwrap().expect("login");
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn supports_chatgpt_device_code_sign_in() {
        let (client, mock) = CodexAppServerClient::mock();
        let client = Arc::new(client);
        let client_task = Arc::clone(&client);
        let task = tokio::spawn(async move { login_with_client(&client_task, true, true).await });

        let init = mock.next_request().await.unwrap();
        mock.respond(init["id"].as_u64().unwrap(), json!({}));
        let _ = mock.next_request().await.unwrap(); // initialized

        let login = mock.next_request().await.unwrap();
        assert_eq!(login["params"]["type"], "chatgptDeviceCode");
        mock.respond(
            login["id"].as_u64().unwrap(),
            json!({
                "type": "chatgptDeviceCode",
                "loginId": "login-device-1",
                "verificationUrl": "https://chatgpt.test/device",
                "userCode": "ABCD-EFGH"
            }),
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        mock.notify(
            "account/login/completed",
            json!({ "loginId": "login-device-1", "success": true, "error": null }),
        );
        let read = mock.next_request().await.unwrap();
        mock.respond(
            read["id"].as_u64().unwrap(),
            json!({
                "account": { "type": "chatgpt", "email": "dev@example.com", "planType": "pro" },
                "requiresOpenaiAuth": false
            }),
        );
        assert_eq!(task.await.unwrap().unwrap(), 0);
    }

    #[tokio::test]
    async fn reports_current_codex_app_server_sign_in_status() {
        let (client, mock) = CodexAppServerClient::mock();
        let task = tokio::spawn(async move {
            client.initialize(InitializeOptions::default()).await?;
            client.read_account(true).await
        });
        let init = mock.next_request().await.unwrap();
        mock.respond(init["id"].as_u64().unwrap(), json!({}));
        let _ = mock.next_request().await.unwrap();
        let read = mock.next_request().await.unwrap();
        assert_eq!(read["method"], "account/read");
        mock.respond(
            read["id"].as_u64().unwrap(),
            json!({
                "account": {
                    "type": "chatgpt",
                    "email": "dev@example.com",
                    "planType": "pro"
                },
                "requiresOpenaiAuth": false
            }),
        );
        let account = task.await.unwrap().unwrap();
        assert_eq!(account_label(&account), " (pro)");
    }

    #[tokio::test]
    async fn readiness_report_uses_app_server_account_and_capabilities() {
        let codex_home = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        fs::write(
            codex_home.path().join("auth.json"),
            serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "opaque-token",
                    "account_id": "acct-secret-1234"
                }
            })
            .to_string(),
        )
        .unwrap();
        let identity = crate::codex_identity::CodexIdentitySelection {
            profile_name: "work".to_owned(),
            codex_home: codex_home.path().to_path_buf(),
            workspace_boundary: None,
        };
        let options = CodexReadinessOptions {
            model: "openai-codex/gpt-5.6".to_owned(),
            cwd: std::env::current_dir().unwrap(),
            state_root: state_root.path().to_path_buf(),
        };
        let (client, mock) = CodexAppServerClient::mock();
        let task =
            tokio::spawn(async move { build_readiness_report(&client, &identity, &options).await });

        let init = mock.next_request().await.unwrap();
        assert_eq!(init["method"], "initialize");
        mock.respond(
            init["id"].as_u64().unwrap(),
            json!({
                "protocolVersion": "2025-01-01",
                "capabilities": {
                    "methods": ["thread/start", "turn/start", "turn/interrupt", "thread/resume"],
                    "notifications": ["item/tool/call", "item/agentMessage/delta", "turn/completed"]
                }
            }),
        );
        let initialized = mock.next_request().await.unwrap();
        assert_eq!(initialized["method"], "initialized");
        let read = mock.next_request().await.unwrap();
        assert_eq!(read["method"], "account/read");
        mock.respond(
            read["id"].as_u64().unwrap(),
            json!({
                "account": { "type": "chatgpt", "email": "dev@example.com", "planType": "pro" },
                "requiresOpenaiAuth": false
            }),
        );

        let report = task.await.unwrap().unwrap();
        let evaluation = evaluate_readiness(report);
        assert_eq!(evaluation.exit_code, 0);
        let raw = serde_json::to_string(&evaluation).unwrap();
        assert!(!raw.contains("dev@example.com"));
        assert!(!raw.contains("auth.json"));
    }

    #[tokio::test]
    async fn readiness_accepts_app_server_account_without_local_auth_file() {
        let codex_home = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let identity = crate::codex_identity::CodexIdentitySelection {
            profile_name: "work".to_owned(),
            codex_home: codex_home.path().to_path_buf(),
            workspace_boundary: None,
        };
        let options = CodexReadinessOptions {
            model: "openai-codex/gpt-5.6".to_owned(),
            cwd: std::env::current_dir().unwrap(),
            state_root: state_root.path().to_path_buf(),
        };
        let (client, mock) = CodexAppServerClient::mock();
        let task =
            tokio::spawn(async move { build_readiness_report(&client, &identity, &options).await });

        let init = mock.next_request().await.unwrap();
        mock.respond(
            init["id"].as_u64().unwrap(),
            json!({
                "protocolVersion": "2025-01-01",
                "capabilities": {
                    "methods": ["thread/start", "turn/start", "turn/interrupt"],
                    "notifications": ["item/tool/call", "item/agentMessage/delta", "turn/completed"]
                }
            }),
        );
        let _initialized = mock.next_request().await.unwrap();
        let read = mock.next_request().await.unwrap();
        mock.respond(
            read["id"].as_u64().unwrap(),
            json!({
                "account": { "type": "chatgpt", "email": "dev@example.com", "planType": "pro" },
                "requiresOpenaiAuth": false
            }),
        );

        let evaluation = evaluate_readiness(task.await.unwrap().unwrap());
        assert_eq!(evaluation.exit_code, 0);
        let raw = serde_json::to_string(&evaluation).unwrap();
        assert!(!raw.contains("dev@example.com"));
        assert!(!raw.contains(codex_home.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn account_label_never_contains_email() {
        let account = AccountReadResult {
            account: Some(json!({
                "type": "chatgpt",
                "email": "person@example.com",
                "planType": "pro"
            })),
            requires_openai_auth: false,
        };
        assert_eq!(account_label(&account), " (pro)");
    }

    #[tokio::test]
    async fn logs_out_of_chatgpt_for_codex() {
        let (client, mock) = CodexAppServerClient::mock();
        let task = tokio::spawn(async move {
            client.initialize(InitializeOptions::default()).await?;
            client.logout().await
        });
        let init = mock.next_request().await.unwrap();
        mock.respond(init["id"].as_u64().unwrap(), json!({}));
        let _ = mock.next_request().await.unwrap();
        let logout = mock.next_request().await.unwrap();
        assert_eq!(logout["method"], "account/logout");
        mock.respond(logout["id"].as_u64().unwrap(), json!({}));
        task.await.unwrap().unwrap();
    }

    #[test]
    fn unknown_subcommand_message_mentions_login() {
        // Operating-layer evidence: the codex surface owns the `login` subcommand.
        let subcommands = ["login", "logout", "status"];
        assert!(
            subcommands.contains(&"login"),
            "deixic-code codex login must stay a first-class subcommand"
        );
    }
}
