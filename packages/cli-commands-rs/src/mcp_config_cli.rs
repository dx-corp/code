//! Safe, atomic MCP configuration mutations for `maestro mcp`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

pub use crate::mcp_catalog::{McpCatalogEntry, catalog_entries};

pub async fn run_mcp_config(args: &[String]) -> Result<i32> {
    let command = args.first().map(String::as_str).unwrap_or("list");
    if matches!(command, "list" | "status" | "tools") {
        print_runtime_status(args).await?;
        return Ok(0);
    }
    println!("{}", apply_mcp_config_async(args).await?);
    Ok(0)
}

pub async fn apply_mcp_config_async(args: &[String]) -> Result<String> {
    apply_mcp_config_async_with_output(args, true).await
}

pub async fn apply_mcp_config_async_quiet(args: &[String]) -> Result<String> {
    apply_mcp_config_async_with_output(args, false).await
}

async fn apply_mcp_config_async_with_output(args: &[String], announce_url: bool) -> Result<String> {
    if args.first().map(String::as_str) != Some("auth") {
        return apply_mcp_config(args);
    }
    let name = required(args, 1, "server name")?;
    let cwd = std::env::current_dir()?;
    let config = crate::mcp::load_mcp_config_with_managed_connections(Some(&cwd));
    let server = config
        .get_server(name)
        .with_context(|| {
            crate::localization::cli_locale()
                .format("MCP server {0} is not configured", &[name.to_string()])
        })?
        .clone();
    if matches!(
        server.scope,
        crate::mcp::McpConfigScope::Managed | crate::mcp::McpConfigScope::Enterprise
    ) {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "managed and enterprise MCP authentication is controlled by its policy owner",
                &[]
            )
        );
    }
    let scopes = option_values(args, "--oauth-scope");
    if announce_url {
        crate::mcp::oauth_login(&server, option_value(args, "--client-id"), &scopes).await?;
    } else {
        crate::mcp::oauth_login_quiet(&server, option_value(args, "--client-id"), &scopes).await?;
    }
    let path = path_for_scope(server.scope, &cwd)?;
    set_server_field(&path, name, "authPreset", json!("oauth"))?;
    Ok(crate::localization::cli_locale().format(
        "Authenticated MCP server {0}; token stored in the OS credential store.",
        &[name.to_string()],
    ))
}

async fn print_runtime_status(args: &[String]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let executor = crate::tools::ToolExecutor::new(cwd.display().to_string());
    let statuses = executor.mcp_status().await.map_err(anyhow::Error::msg)?;
    if args.iter().any(|arg| arg == "--json") {
        println!("{}", serde_json::to_string_pretty(&statuses)?);
        return Ok(());
    }
    if statuses.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format("No MCP servers configured.", &[])
        );
        return Ok(());
    }
    let tool_filter = (args.first().map(String::as_str) == Some("tools"))
        .then(|| args.get(1).map(String::as_str))
        .flatten();
    if args.first().map(String::as_str) != Some("tools") {
        println!(
            "{}",
            crate::localization::cli_locale().format("Configured MCP servers:", &[])
        );
    }
    for status in statuses {
        if tool_filter.is_some_and(|name| name != status.name) {
            continue;
        }
        if args.first().map(String::as_str) == Some("tools") {
            println!("{}  {}", status.name, status.state.label());
            for tool in &status.tools {
                println!("  {tool}");
            }
            for tool in &status.disabled_tools {
                println!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("  {0}  disabled", std::slice::from_ref(tool))
                );
            }
            if status.tools.is_empty() && status.disabled_tools.is_empty() {
                println!(
                    "{}",
                    crate::localization::cli_locale().format("  No tools available.", &[])
                );
            }
        } else {
            println!(
                "  {:<24} {:<20} {:<7} [{:?}]  {} tools",
                status.name,
                status.state.label(),
                format!("{:?}", status.transport).to_lowercase(),
                status.scope,
                status.tools.len()
            );
            if let Some(error) = status.error {
                println!("    {error}");
            }
        }
    }
    Ok(())
}

/// Apply a configuration command and return a UI-safe status message.
pub fn apply_mcp_config(args: &[String]) -> Result<String> {
    let command = args.first().map(String::as_str).unwrap_or("list");
    let cwd = std::env::current_dir()?;
    match command {
        "list" | "status" | "tools" => Ok(
            "Runtime MCP status is available from the top-level maestro mcp command.".to_string(),
        ),
        "add" => add_unified(args, &cwd),
        "add-stdio" => {
            let name = required(args, 1, "server name")?;
            let executable = required(args, 2, "server command")?;
            let (path, scope) = target_path(args, &cwd)?;
            let (command_args, env) = stdio_options(&args[3..])?;
            if scope != "user" && !env.is_empty() {
                bail!("{}", crate::localization::cli_locale().format("environment bindings are user-scope only; project configs cannot read secrets", &[]));
            }
            reject_literal_secrets(&command_args)?;
            mutate_server(
                &path,
                name,
                Some(json!({"command": executable, "args": command_args, "env":env})),
            )?;
            Ok(crate::localization::cli_locale().format(
                "Configured stdio MCP server {0} in {1}",
                &[name.to_string(), path.display().to_string()],
            ))
        }
        "add-http" => {
            let name = required(args, 1, "server name")?;
            let url = required(args, 2, "server URL")?;
            validate_http_url(url)?;
            let mut server = json!({"transport":"http", "url":url});
            if args.iter().any(|value| value == "--bearer-token-env")
                && option_value(args, "--bearer-token-env").is_none()
            {
                bail!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("--bearer-token-env requires a variable", &[])
                );
            }
            if let Some(variable) = option_value(args, "--bearer-token-env") {
                validate_env_name(variable)?;
                server["headers"] = json!({"Authorization": format!("Bearer ${{{variable}}}")});
            }
            let (path, scope) = target_path(args, &cwd)?;
            if scope != "user" && server.get("headers").is_some() {
                bail!("{}", crate::localization::cli_locale().format("bearer-token bindings are user-scope only; project configs cannot read secrets", &[]));
            }
            mutate_server(&path, name, Some(server))?;
            Ok(crate::localization::cli_locale().format(
                "Configured HTTP MCP server {0} in {1}",
                &[name.to_string(), path.display().to_string()],
            ))
        }
        "remove" => {
            let name = required(args, 1, "server name")?;
            let (path, _) = target_path(args, &cwd)?;
            mutate_server(&path, name, None)?;
            Ok(crate::localization::cli_locale().format(
                "Removed MCP server {0} from {1}",
                &[name.to_string(), path.display().to_string()],
            ))
        }
        "enable" | "disable" => {
            let name = required(args, 1, "server name")?;
            let (path, _) = target_path(args, &cwd)?;
            set_server_field(&path, name, "enabled", json!(command == "enable"))?;
            set_server_field(&path, name, "disabled", json!(command == "disable"))?;
            Ok(crate::localization::cli_locale().format(
                if command == "enable" {
                    "Enabled MCP server {0} in {1}"
                } else {
                    "Disabled MCP server {0} in {1}"
                },
                &[name.to_string(), path.display().to_string()],
            ))
        }
        "tool" => {
            let name = required(args, 1, "server name")?;
            let tool = required(args, 2, "tool name")?;
            let setting = required(args, 3, "on or off")?;
            let enabled = match setting {
                "on" | "enable" | "enabled" => true,
                "off" | "disable" | "disabled" => false,
                _ => bail!(
                    "{}",
                    crate::localization::cli_locale().format("tool state must be on or off", &[])
                ),
            };
            let (path, _) = target_path(args, &cwd)?;
            set_tool_enabled(&path, name, tool, enabled)?;
            Ok(crate::localization::cli_locale().format(
                if enabled {
                    "Enabled tool {0} on MCP server {1}"
                } else {
                    "Disabled tool {0} on MCP server {1}"
                },
                &[tool.to_string(), name.to_string()],
            ))
        }
        "permissions" => permissions_command(&args[1..]),
        "registry" | "catalog" => registry_command(&args[1..], &cwd),
        "clear-auth" => {
            let name = required(args, 1, "server name")?;
            let removed = crate::mcp::clear_oauth(name)?;
            Ok(if removed {
                crate::localization::cli_locale().format(
                    "Cleared OAuth credentials for MCP server {0}",
                    &[name.to_string()],
                )
            } else {
                crate::localization::cli_locale().format(
                    "No OAuth credentials stored for MCP server {0}",
                    &[name.to_string()],
                )
            })
        }
        "help" | "--help" | "-h" => Ok(help_text().to_string()),
        other => bail!(
            "{}",
            crate::localization::cli_locale()
                .format("unknown mcp command: {0}", &[(other).to_string()])
        ),
    }
}

#[cfg(test)]
fn redact_server_configs(mut value: Value) -> Value {
    match &mut value {
        Value::Object(servers) => {
            for server in servers.values_mut() {
                redact_config_value(server, Some("server"));
            }
        }
        Value::Array(servers) => {
            for server in servers {
                redact_config_value(server, Some("server"));
            }
        }
        _ => {}
    }
    value
}

#[cfg(test)]
fn redact_config_value(value: &mut Value, parent_key: Option<&str>) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if matches!(parent_key, Some("env" | "headers")) || is_secret_key(key) {
                    *child = Value::String("[REDACTED]".to_string());
                } else if key.eq_ignore_ascii_case("url") {
                    if let Some(url) = child.as_str() {
                        *child = Value::String(redact_url_credentials(url));
                    }
                } else {
                    redact_config_value(child, Some(key));
                }
            }
        }
        Value::Array(values) => {
            if parent_key == Some("args") {
                redact_argument_array(values);
                return;
            }
            for child in values {
                redact_config_value(child, parent_key);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
fn redact_url_credentials(value: &str) -> String {
    let Ok(url) = url::Url::parse(value) else {
        return "[REDACTED]".to_string();
    };
    url.origin().ascii_serialization()
}

#[cfg(test)]
fn redact_argument_array(values: &mut [Value]) {
    let mut secret_value_follows = false;
    for value in values {
        let Some(argument) = value.as_str() else {
            secret_value_follows = false;
            continue;
        };
        if authorization_header_has_literal_credential(argument) {
            *value = Value::String("[REDACTED]".to_string());
            secret_value_follows = false;
            continue;
        }
        if secret_value_follows {
            if !is_plain_env_reference(argument) {
                *value = Value::String("[REDACTED]".to_string());
            }
            secret_value_follows = false;
            continue;
        }
        if let Some(inline_value) = credential_flag(argument) {
            if inline_value {
                if !argument
                    .split_once('=')
                    .is_some_and(|(_, value)| is_plain_env_reference(value))
                {
                    *value = Value::String("[REDACTED]".to_string());
                }
            } else {
                secret_value_follows = true;
            }
        }
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "authorization",
        "apikey",
        "accesstoken",
        "refreshtoken",
        "token",
        "password",
        "secret",
        "credential",
        "privatekey",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn help_text() -> &'static str {
    "Usage:\n\
         \x20 maestro mcp list [--json]\n\
         \x20 maestro mcp status [--json]\n\
         \x20 maestro mcp tools [name] [--json]\n\
         \x20 maestro mcp add <name> <url-or-command> [args...] --type stdio|http|sse [--env VAR] [--header 'Name: value'] [--no-oauth] [--scope ...]\n\
         \x20 maestro mcp add-stdio <name> <command> [args...] [--env VAR] [--scope user|project|local]\n\
         \x20 maestro mcp add-http <name> <url> [--bearer-token-env VAR] [--scope ...]\n\
         \x20 maestro mcp enable|disable <name> [--scope ...]\n\
         \x20 maestro mcp tool <name> <tool> on|off [--scope ...]\n\
         \x20 maestro mcp auth <name> [--client-id ID] [--oauth-scope SCOPE]\n\
         \x20 maestro mcp clear-auth <name>\n\
         \x20 maestro mcp registry list|add <name> [--scope ...]\n\
         \x20 maestro mcp permissions list [--json]\n\
         \x20 maestro mcp permissions revoke <server> [tool]\n\
         \x20 maestro mcp permissions clear --confirm\n\
         \x20 maestro mcp remove <name> [--scope ...]\n\n\
         Secrets must use environment references in user scope; project configs cannot read them."
}

fn registry_command(args: &[String], cwd: &Path) -> Result<String> {
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" => Ok(catalog_entries()
            .iter()
            .map(|entry| {
                format!(
                    "{:<14} {}",
                    entry.id,
                    crate::localization::cli_locale().translate(entry.description)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")),
        "add" => {
            let id = required(args, 1, "catalog server name")?;
            let entry = catalog_entries()
                .iter()
                .find(|entry| entry.id == id)
                .with_context(|| {
                    crate::localization::cli_locale()
                        .format("unknown MCP catalog entry: {0}", &[id.to_string()])
                })?;
            let (path, _) = target_path(args, cwd)?;
            insert_catalog_entry(&path, entry)?;
            Ok(crate::localization::cli_locale().format(
                "Added {0} from the MCP registry in {1}",
                &[entry.id.to_string(), path.display().to_string()],
            ))
        }
        other => bail!(
            "{}",
            crate::localization::cli_locale()
                .format("unknown MCP registry command: {0}", &[(other).to_string()])
        ),
    }
}

fn insert_catalog_entry(path: &Path, entry: &McpCatalogEntry) -> Result<()> {
    mutate_server_with_replace(path, entry.id, Some(entry.configuration()), false)
}

fn permissions_command(args: &[String]) -> Result<String> {
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" => {
            let grants = crate::mcp::list_permissions()?;
            if args.iter().any(|arg| arg == "--json") {
                return Ok(serde_json::to_string_pretty(&grants)?);
            }
            if grants.is_empty() {
                return Ok(
                    crate::localization::cli_locale().format("No persistent MCP permissions.", &[])
                );
            }
            Ok(grants
                .into_iter()
                .map(|grant| {
                    format!(
                        "{}  {}  {}  {}",
                        grant.server,
                        grant.tool,
                        &grant.fingerprint[..12],
                        grant.granted_at
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "revoke" => {
            let server = required(args, 1, "server name")?;
            if let Some(tool) = args.get(2) {
                if crate::mcp::revoke_permission(server, tool)? {
                    Ok(crate::localization::cli_locale().format(
                        "Revoked MCP permission for {0} {1}",
                        &[server.to_string(), tool.clone()],
                    ))
                } else {
                    Ok(crate::localization::cli_locale().format(
                        "No persistent MCP permission matched {0} {1}",
                        &[server.to_string(), tool.clone()],
                    ))
                }
            } else {
                let count = crate::mcp::revoke_server_permissions(server)?;
                Ok(crate::localization::cli_locale().format(
                    "Persistent MCP permissions revoked for {0}: {1}",
                    &[server.to_string(), count.to_string()],
                ))
            }
        }
        "clear" => {
            if !args.iter().any(|arg| arg == "--confirm") {
                bail!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("clearing all MCP permissions requires --confirm", &[])
                );
            }
            let count = crate::mcp::clear_permissions()?;
            Ok(crate::localization::cli_locale().format(
                "Persistent MCP permissions cleared: {0}",
                &[count.to_string()],
            ))
        }
        other => bail!(
            "{}",
            crate::localization::cli_locale().format(
                "unknown MCP permissions command: {0}",
                &[(other).to_string()]
            )
        ),
    }
}

fn add_unified(args: &[String], cwd: &Path) -> Result<String> {
    let name = required(args, 1, "server name")?;
    let target = required(args, 2, "server URL or command")?;
    let transport = option_value(args, "--type").unwrap_or_else(|| {
        if target.starts_with("http://") || target.starts_with("https://") {
            "http"
        } else {
            "stdio"
        }
    });
    match transport {
        "stdio" => {
            let (path, scope) = target_path(args, cwd)?;
            let (command_args, env) = stdio_options(&args[3..])?;
            if scope != "user" && !env.is_empty() {
                bail!("{}", crate::localization::cli_locale().format("environment bindings are user-scope only; project configs cannot read secrets", &[]));
            }
            reject_literal_secrets(&command_args)?;
            mutate_server(
                &path,
                name,
                Some(
                    json!({"transport":"stdio", "command":target, "args":command_args, "env":env}),
                ),
            )?;
            Ok(crate::localization::cli_locale().format(
                "Configured stdio MCP server {0} in {1}",
                &[name.to_string(), path.display().to_string()],
            ))
        }
        "http" | "sse" => {
            validate_http_url(target)?;
            let (path, scope) = target_path(args, cwd)?;
            let mut server = json!({"transport":transport, "url":target});
            let mut headers = http_headers(args)?;
            if let Some(variable) = option_value(args, "--bearer-token-env") {
                validate_env_name(variable)?;
                if scope != "user" {
                    bail!("{}", crate::localization::cli_locale().format("bearer-token bindings are user-scope only; project configs cannot read secrets", &[]));
                }
                headers.insert(
                    "Authorization".to_string(),
                    Value::String(format!("Bearer ${{{variable}}}")),
                );
            }
            if scope != "user"
                && headers.iter().any(|(name, value)| {
                    is_secret_key(name) || value.as_str().is_some_and(|value| value.contains("${"))
                })
            {
                bail!("{}", crate::localization::cli_locale().format("secret or environment-backed headers are user-scope only; project configs cannot read secrets", &[]));
            }
            if !headers.is_empty() {
                server["headers"] = Value::Object(headers);
            }
            if args.iter().any(|arg| arg == "--no-oauth") {
                server["authPreset"] = json!("none");
            }
            mutate_server(&path, name, Some(server))?;
            Ok(crate::localization::cli_locale().format(
                "Configured {0} MCP server {1} in {2}",
                &[
                    transport.to_string(),
                    name.to_string(),
                    path.display().to_string(),
                ],
            ))
        }
        other => bail!(
            "{}",
            crate::localization::cli_locale()
                .format("unknown MCP transport: {0}", &[(other).to_string()])
        ),
    }
}

fn required<'a>(args: &'a [String], index: usize, label: &str) -> Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .filter(|value| !value.starts_with("--"))
        .with_context(|| format!("missing {label}"))
}

fn option_value<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|pair| pair[0] == key)
        .map(|pair| pair[1].as_str())
        .or_else(|| {
            args.iter()
                .find_map(|arg| arg.strip_prefix(&format!("{key}=")))
        })
}

fn option_values(args: &[String], key: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == key {
            if let Some(value) = args.get(index + 1) {
                values.push(value.clone());
            }
            index += 2;
        } else if let Some(value) = args[index].strip_prefix(&format!("{key}=")) {
            values.push(value.to_string());
            index += 1;
        } else {
            index += 1;
        }
    }
    values
}

fn target_path(args: &[String], cwd: &Path) -> Result<(PathBuf, &'static str)> {
    let args = args.split(|value| value == "--").next().unwrap_or(args);
    if args.iter().any(|value| value == "--scope") && option_value(args, "--scope").is_none() {
        bail!(
            "{}",
            crate::localization::cli_locale().format("--scope requires a value", &[])
        );
    }
    match option_value(args, "--scope").unwrap_or("user") {
        "user" => {
            let path = crate::mcp::effective_user_config_path()
                .context("could not resolve user MCP configuration path")?;
            Ok((path, "user"))
        }
        "project" => Ok((cwd.join(".maestro").join("mcp.json"), "project")),
        "local" => Ok((cwd.join(".maestro").join("mcp.local.json"), "local")),
        other => bail!(
            "{}",
            crate::localization::cli_locale()
                .format("unknown MCP config scope: {0}", &[(other).to_string()])
        ),
    }
}

fn path_for_scope(scope: crate::mcp::McpConfigScope, cwd: &Path) -> Result<PathBuf> {
    match scope {
        crate::mcp::McpConfigScope::User => crate::mcp::effective_user_config_path()
            .context("could not resolve user MCP configuration path"),
        crate::mcp::McpConfigScope::Project => Ok(cwd.join(".maestro").join("mcp.json")),
        crate::mcp::McpConfigScope::Local => Ok(cwd.join(".maestro").join("mcp.local.json")),
        crate::mcp::McpConfigScope::Managed | crate::mcp::McpConfigScope::Enterprise => {
            bail!(
                "{}",
                crate::localization::cli_locale().format("this MCP scope is read-only", &[])
            )
        }
    }
}

fn validate_env_name(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("environment variable names must use A-Z, 0-9, and _", &[])
        );
    }
    Ok(())
}

fn validate_http_url(value: &str) -> Result<()> {
    let url = url::Url::parse(value).context("invalid HTTP MCP URL")?;
    let localhost = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && localhost) {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "HTTP MCP URLs must use HTTPS (loopback HTTP is allowed)",
                &[]
            )
        );
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("credentials may not be embedded in MCP URLs", &[])
        );
    }
    if url.query_pairs().any(|(key, _)| is_secret_key(&key)) {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "credential query parameters are not allowed; use --bearer-token-env",
                &[]
            )
        );
    }
    Ok(())
}

fn http_headers(args: &[String]) -> Result<Map<String, Value>> {
    let mut headers = Map::new();
    for raw in option_values(args, "--header") {
        let (name, value) = raw
            .split_once(':')
            .context("--header must use 'Name: value'")?;
        let name = name.trim();
        let value = value.trim();
        reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid HTTP header name: {name}"))?;
        reqwest::header::HeaderValue::from_str(value)
            .with_context(|| format!("invalid value for HTTP header {name}"))?;
        if is_secret_key(name)
            && authorization_header_has_literal_credential(&format!("{name}: {value}"))
        {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "literal header secrets are not allowed; use an environment reference",
                    &[]
                )
            );
        }
        headers.insert(name.to_string(), Value::String(value.to_string()));
    }
    Ok(headers)
}

fn stdio_options(args: &[String]) -> Result<(Vec<String>, Map<String, Value>)> {
    let mut command_args = Vec::new();
    let mut env = Map::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--" => {
                command_args.extend_from_slice(&args[index + 1..]);
                break;
            }
            "--scope" => {
                if args
                    .get(index + 1)
                    .is_none_or(|value| value.starts_with('-'))
                {
                    bail!(
                        "{}",
                        crate::localization::cli_locale().format("--scope requires a value", &[])
                    );
                }
                index += 2;
            }
            value if value.starts_with("--scope=") => index += 1,
            "--type" | "--bearer-token-env" | "--header" => {
                if args
                    .get(index + 1)
                    .is_none_or(|value| value.starts_with('-'))
                {
                    bail!("{} requires a value", args[index]);
                }
                index += 2;
            }
            value
                if value.starts_with("--type=")
                    || value.starts_with("--bearer-token-env=")
                    || value.starts_with("--header=") =>
            {
                index += 1;
            }
            "--no-oauth" => index += 1,
            "--env" => {
                let variable = args.get(index + 1).context("--env requires a variable")?;
                validate_env_name(variable)?;
                env.insert(variable.clone(), Value::String(format!("${{{variable}}}")));
                index += 2;
            }
            value if value.starts_with("--env=") => {
                let variable = value.trim_start_matches("--env=");
                validate_env_name(variable)?;
                env.insert(
                    variable.to_string(),
                    Value::String(format!("${{{variable}}}")),
                );
                index += 1;
            }
            _ => {
                command_args.push(args[index].clone());
                index += 1;
            }
        }
    }
    Ok((command_args, env))
}

fn reject_literal_secrets(args: &[String]) -> Result<()> {
    let mut secret_value_follows = false;
    for argument in args {
        if authorization_header_has_literal_credential(argument) {
            bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("literal secrets are not allowed; use --env VAR", &[])
            );
        }
        if secret_value_follows && !is_plain_env_reference(argument) {
            bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("literal secrets are not allowed; use --env VAR", &[])
            );
        }
        if let Some(inline_value) = credential_flag(argument) {
            secret_value_follows = !inline_value;
            if inline_value
                && !argument
                    .split_once('=')
                    .is_some_and(|(_, value)| is_plain_env_reference(value))
            {
                bail!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("literal secrets are not allowed; use --env VAR", &[])
                );
            }
        } else {
            secret_value_follows = false;
        }
    }
    Ok(())
}

fn authorization_header_has_literal_credential(value: &str) -> bool {
    let Some((header, value)) = value.split_once(':') else {
        return false;
    };
    let header = header.rsplit(['=', ' ']).next().unwrap_or(header).trim();
    if !is_secret_key(header) {
        return false;
    }
    let value = value.trim();
    let lower_value = value.to_ascii_lowercase();
    let credential = lower_value
        .strip_prefix("bearer ")
        .or_else(|| lower_value.strip_prefix("basic "))
        .map_or(value, |_| {
            value
                .split_once(' ')
                .map_or(value, |(_, credential)| credential)
        });
    !credential.is_empty() && !is_plain_env_reference(credential)
}

fn is_plain_env_reference(value: &str) -> bool {
    value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .is_some_and(|variable| validate_env_name(variable).is_ok())
}

fn credential_flag(argument: &str) -> Option<bool> {
    let lower = argument.to_ascii_lowercase();
    let value = lower.strip_prefix('-')?.trim_start_matches('-');
    let (name, inline_value) = value
        .split_once('=')
        .map_or((value, false), |(name, _)| (name, true));
    is_secret_key(name).then_some(inline_value)
}

fn set_server_field(path: &Path, name: &str, field: &str, value: Value) -> Result<()> {
    mutate_existing_server(path, name, |server| {
        let object = server
            .as_object_mut()
            .context("MCP server entry must be an object")?;
        object.insert(field.to_string(), value);
        Ok(())
    })
}

fn set_tool_enabled(path: &Path, name: &str, tool: &str, enabled: bool) -> Result<()> {
    mutate_existing_server(path, name, |server| {
        let object = server
            .as_object_mut()
            .context("MCP server entry must be an object")?;
        let disabled = object
            .entry("disabledTools")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .context("disabledTools must be an array")?;
        disabled.retain(|value| value.as_str() != Some(tool));
        if !enabled {
            disabled.push(Value::String(tool.to_string()));
        }
        Ok(())
    })
}

fn mutate_existing_server(
    path: &Path,
    name: &str,
    mutate: impl FnOnce(&mut Value) -> Result<()>,
) -> Result<()> {
    let mut root: Value = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON in {}", path.display()))?;
    let server = root
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.get_mut(name))
        .with_context(|| {
            format!(
                "server {name} is not editable in {}; select its owning --scope",
                path.display()
            )
        })?;
    mutate(server)?;
    crate::path_utils::atomic_private_write(path, &serde_json::to_vec_pretty(&root)?)
}

fn mutate_server(path: &Path, name: &str, value: Option<Value>) -> Result<()> {
    mutate_server_with_replace(path, name, value, true)
}

fn mutate_server_with_replace(
    path: &Path,
    name: &str,
    value: Option<Value>,
    replace: bool,
) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("server names must be alphanumeric with - or _", &[])
        );
    }
    let mut root = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .with_context(|| format!("invalid JSON in {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    let object = root
        .as_object_mut()
        .context("MCP configuration root must be an object")?;
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("mcpServers must be an object")?;
    match value {
        Some(value) => {
            if !replace && servers.contains_key(name) {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "MCP server {0} already exists; its configuration was kept",
                        &[(name).to_string()]
                    )
                );
            }
            servers.insert(name.to_string(), value);
        }
        None => {
            servers.remove(name);
        }
    }
    crate::path_utils::atomic_private_write(path, &serde_json::to_vec_pretty(&root)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn catalog_install_preserves_existing_configuration_and_rejects_invalid_json() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("mcp.json");
        let entry = catalog_entries().iter().find(|e| e.id == "linear").unwrap();
        insert_catalog_entry(&path, entry).unwrap();
        let original = fs::read(&path).unwrap();
        assert!(
            insert_catalog_entry(&path, entry)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        let config: Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(config["mcpServers"]["linear"]["transport"], "http");
        assert_eq!(
            config["mcpServers"]["linear"]["url"],
            "https://mcp.linear.app/mcp"
        );
        fs::write(&path, "{invalid").unwrap();
        assert!(insert_catalog_entry(&path, entry).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{invalid");
    }

    #[test]
    fn mutations_preserve_other_servers_and_use_env_references() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("mcp.json");
        mutate_server(&path, "one", Some(json!({"command":"one"}))).unwrap();
        mutate_server(
            &path,
            "two",
            Some(
                json!({"url":"https://example.com","headers":{"Authorization":"Bearer ${TOKEN}"}}),
            ),
        )
        .unwrap();
        mutate_server(&path, "one", None).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(value["mcpServers"].get("one").is_none());
        assert_eq!(
            value["mcpServers"]["two"]["headers"]["Authorization"],
            "Bearer ${TOKEN}"
        );
    }

    #[test]
    fn stdio_flags_are_preserved_and_secret_literals_rejected() {
        let args = vec![
            "-y".to_string(),
            "@example/server".to_string(),
            "--env".to_string(),
            "SERVICE_TOKEN".to_string(),
            "--scope".to_string(),
            "project".to_string(),
        ];
        let (command_args, env) = stdio_options(&args).unwrap();
        assert_eq!(command_args, ["-y", "@example/server"]);
        assert_eq!(env["SERVICE_TOKEN"], "${SERVICE_TOKEN}");
        assert!(reject_literal_secrets(&["--token".into(), "secret".into()]).is_err());
        assert!(reject_literal_secrets(&["--client-secret".into(), "secret".into()]).is_err());
        assert!(reject_literal_secrets(&["--access-token=secret".into()]).is_err());
        assert!(reject_literal_secrets(&["--refresh_token".into(), "secret".into()]).is_err());
        assert!(reject_literal_secrets(&["--auth-token".into(), "secret".into()]).is_err());
        assert!(reject_literal_secrets(&["--authorization=Bearer secret".into()]).is_err());
        assert!(
            reject_literal_secrets(&[
                "--header".into(),
                "Authorization: Bearer literal-secret".into()
            ])
            .is_err()
        );
        assert!(
            reject_literal_secrets(&["--header".into(), "X-API-Key: literal-secret".into()])
                .is_err()
        );
        assert!(
            reject_literal_secrets(&[
                "--header".into(),
                "Authorization: Bearer ${SERVICE_TOKEN}".into()
            ])
            .is_ok()
        );
        assert!(
            reject_literal_secrets(&["--token".into(), "${SERVICE_TOKEN:-literal-secret}".into()])
                .is_err()
        );
        assert!(
            reject_literal_secrets(&["--client-secret=${CLIENT_SECRET:-literal-secret}".into()])
                .is_err()
        );
        assert!(reject_literal_secrets(&["--token".into(), "${SERVICE_TOKEN}".into()]).is_ok());
        assert!(reject_literal_secrets(&["--access-token=${ACCESS_TOKEN}".into()]).is_ok());
        assert!(reject_literal_secrets(&["--credentials".into(), "opaque-secret".into()]).is_err());
        assert!(validate_http_url("http://localhost.evil.test/mcp").is_err());
        assert!(validate_http_url("https://example.test/mcp?token=secret").is_err());
        assert!(validate_http_url("https://example.test/mcp?access_token=secret").is_err());
        assert!(validate_http_url("https://example.test/mcp?refresh-token=secret").is_err());
        assert!(validate_http_url("https://example.test/mcp?client_secret=secret").is_err());
        assert!(validate_http_url("https://example.test/mcp?auth_token=secret").is_err());
        assert!(validate_http_url("http://127.0.0.1:3000/mcp").is_ok());
    }

    #[test]
    fn http_headers_accept_safe_values_and_reject_literal_credentials() {
        let headers = http_headers(&[
            "--header".into(),
            "X-Trace: enabled".into(),
            "--header=Authorization: Bearer ${MCP_TOKEN}".into(),
        ])
        .unwrap();
        assert_eq!(headers["X-Trace"], "enabled");
        assert_eq!(headers["Authorization"], "Bearer ${MCP_TOKEN}");
        assert!(http_headers(&["--header".into(), "Authorization: Bearer secret".into()]).is_err());
        assert!(http_headers(&["--header".into(), "bad header: value".into()]).is_err());
    }

    #[test]
    fn unified_remote_add_and_tool_toggle_persist_droid_compatible_options() {
        let temp = TempDir::new().unwrap();
        let args = vec![
            "add".into(),
            "remote".into(),
            "https://example.test/mcp".into(),
            "--type".into(),
            "http".into(),
            "--header".into(),
            "X-Trace: enabled".into(),
            "--no-oauth".into(),
            "--scope".into(),
            "project".into(),
        ];
        add_unified(&args, temp.path()).unwrap();
        let path = temp.path().join(".maestro/mcp.json");
        set_tool_enabled(&path, "remote", "write", false).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();

        assert_eq!(value["mcpServers"]["remote"]["transport"], "http");
        assert_eq!(
            value["mcpServers"]["remote"]["headers"]["X-Trace"],
            "enabled"
        );
        assert_eq!(value["mcpServers"]["remote"]["authPreset"], "none");
        assert_eq!(
            value["mcpServers"]["remote"]["disabledTools"],
            json!(["write"])
        );
    }

    #[test]
    fn stdio_delimiter_preserves_child_scope_and_env_arguments() {
        let args = vec![
            "--scope".to_string(),
            "user".to_string(),
            "--".to_string(),
            "--scope".to_string(),
            "child".to_string(),
            "--env".to_string(),
            "CHILD_VALUE".to_string(),
        ];

        let (command_args, env) = stdio_options(&args).unwrap();
        let (_, scope) = target_path(&args, Path::new("/tmp")).unwrap();

        assert_eq!(command_args, ["--scope", "child", "--env", "CHILD_VALUE"]);
        assert!(env.is_empty());
        assert_eq!(scope, "user");
    }

    #[test]
    fn list_output_redacts_loaded_credentials() {
        let listed = redact_server_configs(json!([
            {
                "name": "stdio",
                "env": {"SERVICE_TOKEN": "literal-secret", "SAFE": "${SAFE}"},
                "args": [
                    "--token", "literal-arg-secret",
                    "--client-secret=${CLIENT_SECRET}",
                    "--auth-token", "${AUTH_TOKEN:-literal-fallback}",
                    "--header", "Authorization: Bearer header-secret",
                    "--header", "X-API-Key: api-header-secret",
                    "--verbose"
                ]
            },
            {
                "name": "http",
                "headers": {
                    "Authorization": "Bearer literal-secret",
                    "X-Custom-Credential": "also-secret"
                },
                "api_key": "literal-secret",
                "credentials": "opaque-secret",
                "url": "https://user:password@example.test/mcp?access_token=url-secret&safe=ok"
            },
            {
                "name": "private-key-service",
                "command": "safe-command"
            },
            {
                "name": "managed",
                "connectionRef": "orb-team",
                "credentialRef": "secretbroker://orb/team"
            }
        ]));

        assert_eq!(listed[0]["env"]["SERVICE_TOKEN"], "[REDACTED]");
        assert_eq!(listed[0]["env"]["SAFE"], "[REDACTED]");
        assert_eq!(listed[1]["headers"]["Authorization"], "[REDACTED]");
        assert_eq!(listed[1]["headers"]["X-Custom-Credential"], "[REDACTED]");
        assert_eq!(listed[1]["api_key"], "[REDACTED]");
        assert_eq!(listed[1]["credentials"], "[REDACTED]");
        let listed_url = listed[1]["url"].as_str().unwrap();
        assert!(!listed_url.contains("password"));
        assert!(!listed_url.contains("url-secret"));
        assert!(!listed_url.contains("safe=ok"));
        assert_eq!(listed_url, "https://example.test");
        assert_eq!(listed[0]["args"][1], "[REDACTED]");
        assert_eq!(listed[0]["args"][2], "--client-secret=${CLIENT_SECRET}");
        assert_eq!(listed[0]["args"][4], "[REDACTED]");
        assert!(!listed.to_string().contains("header-secret"));
        assert!(!listed.to_string().contains("api-header-secret"));
        assert_eq!(listed[2]["name"], "private-key-service");
        assert_eq!(listed[2]["command"], "safe-command");
        assert_eq!(listed[3]["connectionRef"], "orb-team");
        assert_eq!(listed[3]["credentialRef"], "[REDACTED]");
        assert!(!listed.to_string().contains("secretbroker://orb/team"));
    }
}
