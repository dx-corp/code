//! Native `maestro config` command.
//!
//! Covers high-traffic configuration inspection and mutation without booting
//! the TypeScript agent runtime:
//! - `path` / `list` / `get` / `set` against Maestro TOML settings
//! - `show` / `validate` for provider JSON configs + TOML settings
//! - `init` / `local` for project provider bootstrap helpers
//!
//! Residual gaps vs the old TypeScript command:
//! - Full model-registry/factory merge inspection (Factory fallback providers)
//! - Themed badge rendering from the TypeScript style system
//! - Trust-aware project reference scanning uses a best-effort trusted default

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value as JsonValue, json};
use toml::Value as TomlValue;

use crate::local_models::{local_metadata_url, local_runtime_endpoints};
use crate::path_utils::{env_path, maestro_home_dir};
use crate::skill_cli::write_atomic;

const CONFIG_SCHEMA: &str = "https://composer-cli.dev/config.schema.json";
const DEFAULT_GATEWAY: &str = "https://llm-gateway.evalops.dev/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    User,
    Project,
    Local,
}

impl Scope {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "user" | "global" | "home" => Some(Self::User),
            "project" => Some(Self::Project),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct ProviderPreset {
    id: &'static str,
    name: &'static str,
    api: &'static str,
    default_model: &'static str,
    base_url: Option<&'static str>,
    requires_api_key: bool,
    api_key_env: Option<&'static str>,
    note: Option<&'static str>,
    context_window: Option<u64>,
    max_tokens: Option<u64>,
    managed: bool,
}

pub async fn run_config(args: &[String]) -> Result<i32> {
    let subcommand = args.first().map(String::as_str).unwrap_or("help");
    match subcommand {
        "help" | "--help" | "-h" => {
            println!("{}", config_help());
            Ok(0)
        }
        "deployment" => run_deployment(&args[1..]),
        "path" | "paths" => run_path(&args[1..]),
        "list" | "ls" => run_list(&args[1..]),
        "get" => run_get(&args[1..]),
        "set" => run_set(&args[1..]),
        "show" | "status" => run_show(&args[1..]),
        "validate" => run_validate(&args[1..]),
        "init" => run_init(&args[1..]),
        "local" => run_local(&args[1..]).await,
        other => {
            eprintln!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unknown config subcommand: {0}", &[(other).to_string()])
            );
            eprintln!(
                "{}",
                crate::localization::cli_locale().format("\nAvailable commands:", &[])
            );
            println!("{}", config_help());
            Ok(1)
        }
    }
}

fn config_help() -> &'static str {
    "Usage: deixic-code config <command> [options]

Commands:
  path                         Show Maestro config file locations
  list [--scope user|project|local]
                               List keys in a TOML settings file
  get <key> [--scope ...]      Read a dotted TOML key (e.g. model, history.persistence)
  set <key> <value> [--scope ...]
                               Write a dotted TOML key
  show | status                Inspect sources and print a secret-safe generation digest
  deployment --json            Inspect the signed disconnected profile without network requests
  validate                     Validate provider JSON + TOML config files
  init [--preset <id>] [--force]
                               Create project .maestro/config.json
  local [--check] [--provider lmstudio|ollama|llamacpp] [--scope project|user]
                               Manage local LM Studio / Ollama / llama.cpp providers

Options:
  --json                       Machine-readable output where supported
  --help, -h                   Show this help"
}

fn run_deployment(args: &[String]) -> Result<i32> {
    anyhow::ensure!(
        args == ["--json"],
        "Usage: deixic-code config deployment --json"
    );
    let contract = maestro_local_host::safety::disconnected_deployment_contract()
        .map_err(anyhow::Error::msg)?;
    println!("{}", serde_json::to_string_pretty(&contract)?);
    Ok(0)
}

fn run_path(args: &[String]) -> Result<i32> {
    let json = args.iter().any(|arg| arg == "--json");
    let entries = config_path_entries()?;
    if json {
        let payload: Vec<_> = entries
            .iter()
            .map(|(label, path, exists)| {
                json!({
                    "label": label,
                    "path": path.display().to_string(),
                    "exists": exists,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(0);
    }

    println!(
        "{}",
        crate::localization::cli_locale().format("Configuration paths", &[])
    );
    println!("{}", "─".repeat(40));
    for (label, path, exists) in entries {
        let status = if exists { "present" } else { "missing" };
        println!("  [{status:<7}] {label:<22} {}", display_path(&path));
    }
    Ok(0)
}

fn run_list(args: &[String]) -> Result<i32> {
    let (scope, json) = parse_scope_json(args, Scope::User)?;
    let path = settings_path(scope)?;
    let value = load_toml(&path).unwrap_or_else(|| TomlValue::Table(toml::map::Map::new()));
    let mut keys = Vec::new();
    flatten_keys("", &value, &mut keys);
    keys.sort();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "path": path.display().to_string(),
                "scope": scope_name(scope),
                "keys": keys,
            }))?
        );
        return Ok(0);
    }

    println!(
        "{}",
        crate::localization::cli_locale().format("Settings: {0}", &[(display_path(&path)).clone()])
    );
    if keys.is_empty() {
        println!("  (empty)");
    } else {
        for key in keys {
            println!("  {key}");
        }
    }
    Ok(0)
}

fn run_get(args: &[String]) -> Result<i32> {
    let mut key = None;
    let mut scope = Scope::User;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--scope" => {
                let value = args
                    .get(index + 1)
                    .context("deixic-code config get --scope requires user|project|local")?;
                scope = Scope::parse(value)
                    .with_context(|| format!("unknown config scope: {value}"))?;
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--help" | "-h" => {
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Usage: deixic-code config get <key> [--scope user|project|local] [--json]",
                        &[]
                    )
                );
                return Ok(0);
            }
            arg if arg.starts_with('-') => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unknown option: {0}", &[(arg).to_string()])
            ),
            arg => {
                if key.is_some() {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("deixic-code config get accepts a single key", &[])
                    );
                }
                key = Some(arg.to_owned());
                index += 1;
            }
        }
    }
    let Some(key) = key else {
        eprintln!(
            "{}",
            crate::localization::cli_locale()
                .format("Key required. Usage: deixic-code config get <key>", &[])
        );
        return Ok(1);
    };

    let path = settings_path(scope)?;
    let value = load_toml(&path).unwrap_or_else(|| TomlValue::Table(toml::map::Map::new()));
    match get_dotted(&value, &key) {
        Some(found) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "path": path.display().to_string(),
                        "key": key,
                        "value": toml_to_json(&found),
                    }))?
                );
            } else {
                println!("{}", format_toml_value(&found));
            }
            Ok(0)
        }
        None => {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Key not set: {0} (in {1})",
                    &[(key).clone(), (display_path(&path)).clone()]
                )
            );
            Ok(1)
        }
    }
}

fn run_set(args: &[String]) -> Result<i32> {
    let mut key = None;
    let mut value_raw = None;
    let mut scope = Scope::User;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--scope" => {
                let value = args
                    .get(index + 1)
                    .context("deixic-code config set --scope requires user|project|local")?;
                scope = Scope::parse(value)
                    .with_context(|| format!("unknown config scope: {value}"))?;
                index += 2;
            }
            "--help" | "-h" => {
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Usage: deixic-code config set <key> <value> [--scope user|project|local]",
                        &[]
                    )
                );
                return Ok(0);
            }
            arg if arg.starts_with('-') => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unknown option: {0}", &[(arg).to_string()])
            ),
            arg => {
                if key.is_none() {
                    key = Some(arg.to_owned());
                } else if value_raw.is_none() {
                    value_raw = Some(arg.to_owned());
                } else {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("deixic-code config set accepts a single key and value", &[])
                    );
                }
                index += 1;
            }
        }
    }
    let Some(key) = key else {
        eprintln!(
            "{}",
            crate::localization::cli_locale().format(
                "Key required. Usage: deixic-code config set <key> <value>",
                &[]
            )
        );
        return Ok(1);
    };
    let Some(value_raw) = value_raw else {
        eprintln!(
            "{}",
            crate::localization::cli_locale().format(
                "Value required. Usage: deixic-code config set <key> <value>",
                &[]
            )
        );
        return Ok(1);
    };

    let path = settings_path(scope)?;
    let mut root = load_toml_for_update(&path)?;
    let parsed = parse_cli_value(&value_raw);
    set_dotted(&mut root, &key, parsed)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let rendered = toml::to_string_pretty(&root).context("failed to serialize config.toml")?;
    write_atomic(&path, &rendered)?;
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Set {0} in {1}",
            &[(key).clone(), (display_path(&path)).clone()]
        )
    );
    Ok(0)
}

/// Persist the default model in the user-scope `config.toml`, following the
/// same load → set → atomic-write flow as `maestro config set`. Returns the
/// path that was written.
pub fn persist_user_model_default(model_id: &str) -> Result<PathBuf> {
    let path = settings_path(Scope::User)?;
    persist_model_default_to(&path, model_id)?;
    Ok(path)
}

/// Persist the selected model and provider in user-scope `config.toml`.
pub fn persist_user_model_and_provider(model_id: &str, provider: &str) -> Result<PathBuf> {
    let path = settings_path(Scope::User)?;
    let mut root = load_toml_for_update(&path)?;
    set_dotted(&mut root, "model", TomlValue::String(model_id.to_owned()))?;
    set_dotted(
        &mut root,
        "model_provider",
        TomlValue::String(provider.to_owned()),
    )?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let rendered = toml::to_string_pretty(&root).context("failed to serialize config.toml")?;
    write_atomic(&path, &rendered)?;
    Ok(path)
}

fn persist_model_default_to(path: &Path, model_id: &str) -> Result<()> {
    let mut root = load_toml_for_update(path)?;
    set_dotted(&mut root, "model", TomlValue::String(model_id.to_owned()))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let rendered = toml::to_string_pretty(&root).context("failed to serialize config.toml")?;
    write_atomic(path, &rendered)
}

fn run_show(args: &[String]) -> Result<i32> {
    let json = args.iter().any(|arg| arg == "--json");
    let inspection = inspect_config()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&inspection)?);
        return Ok(0);
    }

    println!(
        "{}",
        crate::localization::cli_locale().format("Configuration Inspection", &[])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Generation: {0}",
            &[inspection["generation"]
                .as_str()
                .unwrap_or("unavailable")
                .to_string()]
        )
    );
    println!();
    println!(
        "{}",
        crate::localization::cli_locale().format("Config Sources", &[])
    );
    for source in &inspection["sources"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let path = source["path"].as_str().unwrap_or("");
        let exists = source["exists"].as_bool().unwrap_or(false);
        let status = if exists { "present" } else { "missing" };
        let mark = if source["active"].as_bool().unwrap_or(false) {
            "•"
        } else {
            " "
        };
        println!("  {mark} [{status}] {}", display_path(Path::new(path)));
    }
    println!();

    let providers = inspection["providers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if providers.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format("No providers configured", &[])
        );
        println!();
    } else {
        println!("Providers ({})", providers.len());
        for provider in providers {
            let id = provider["id"].as_str().unwrap_or("?");
            let name = provider["name"].as_str().unwrap_or("");
            let model_count = provider["modelCount"].as_u64().unwrap_or(0);
            let enabled = if provider["enabled"].as_bool().unwrap_or(true) {
                "enabled"
            } else {
                "disabled"
            };
            let key = provider["apiKeySource"]
                .as_str()
                .unwrap_or("API key missing");
            let base = provider["baseUrl"].as_str().unwrap_or("(auto-generated)");
            println!("  {id} ({model_count} models) · {key} · {enabled}");
            if !name.is_empty() {
                println!("     {name}");
            }
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("     Base URL: {0}", &[(base).to_string()])
            );
            if let Some(models) = provider["models"].as_array() {
                let shown = models.iter().take(3);
                for model in shown {
                    let mid = model["id"].as_str().unwrap_or("?");
                    println!("       • {mid}");
                }
                if models.len() > 3 {
                    println!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("       ... and {0} more", &[(models.len() - 3).to_string()])
                    );
                }
            }
            println!();
        }
    }

    if let Some(settings) = inspection.get("settings") {
        println!(
            "{}",
            crate::localization::cli_locale().format("Effective Settings (TOML)", &[])
        );
        if let Some(obj) = settings.as_object() {
            for (key, value) in obj {
                println!("  {key}: {value}");
            }
        }
        println!();
    }

    if let Some(refs) = inspection["fileReferences"].as_array() {
        if !refs.is_empty() {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("File References ({0})", &[(refs.len()).to_string()])
            );
            for file_ref in refs {
                let path = file_ref["path"].as_str().unwrap_or("");
                let exists = file_ref["exists"].as_bool().unwrap_or(false);
                let status = if exists { "present" } else { "missing" };
                let size = file_ref
                    .get("size")
                    .and_then(JsonValue::as_u64)
                    .map(|bytes| format!(" ({})", format_bytes(bytes)))
                    .unwrap_or_default();
                println!("  [{status}] {}{size}", display_path(Path::new(path)));
            }
            println!();
        }
    }

    if let Some(vars) = inspection["envVars"].as_array() {
        if !vars.is_empty() {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("Environment Variables ({0})", &[(vars.len()).to_string()])
            );
            for env_var in vars {
                let name = env_var["name"].as_str().unwrap_or("?");
                let set = env_var["set"].as_bool().unwrap_or(false);
                let status = if set { "set" } else { "missing" };
                let value = env_var["maskedValue"].as_str().unwrap_or("(not set)");
                println!("  [{status}] {name}: {value}");
            }
            println!();
        }
    }

    Ok(0)
}

fn run_validate(args: &[String]) -> Result<i32> {
    let json = args.iter().any(|arg| arg == "--json");
    let result = validate_config()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(i32::from(!result["valid"].as_bool().unwrap_or(false)));
    }

    println!(
        "{}",
        crate::localization::cli_locale().format("Validating Configuration", &[])
    );
    println!();
    if let Some(files) = result["summary"]["configFiles"].as_array() {
        if !files.is_empty() {
            println!(
                "{}",
                crate::localization::cli_locale().format("Config Files:", &[])
            );
            for file in files {
                if let Some(path) = file.as_str() {
                    println!("  • {}", display_path(Path::new(path)));
                }
            }
            println!();
        }
    }

    if let Some(errors) = result["errors"].as_array() {
        if !errors.is_empty() {
            println!("[ERROR] Errors");
            for error in errors {
                if let Some(message) = error.as_str() {
                    println!("  • {message}");
                }
            }
            println!();
        }
    }

    if let Some(warnings) = result["warnings"].as_array() {
        if !warnings.is_empty() {
            println!("[WARN] Warnings");
            for warning in warnings {
                if let Some(message) = warning.as_str() {
                    println!("  • {message}");
                }
            }
            println!();
        }
    }

    let summary = &result["summary"];
    println!(
        "{}",
        crate::localization::cli_locale().format("Summary:", &[])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "  • Providers: {0}",
            &[(summary["providers"].as_u64().unwrap_or(0)).to_string()]
        )
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "  • Models: {0}",
            &[(summary["models"].as_u64().unwrap_or(0)).to_string()]
        )
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "  • File References: {0}",
            &[(summary["fileReferences"]
                .as_array()
                .map(|values| values.len())
                .unwrap_or(0))
            .to_string()]
        )
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "  • Environment Variables: {0}",
            &[(summary["envVars"]
                .as_array()
                .map(|values| values.len())
                .unwrap_or(0))
            .to_string()]
        )
    );
    println!();

    if result["valid"].as_bool().unwrap_or(false) {
        println!(
            "{}",
            crate::localization::cli_locale().format("Configuration is valid", &[])
        );
        Ok(0)
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format("Configuration has errors", &[])
        );
        Ok(1)
    }
}

fn run_init(args: &[String]) -> Result<i32> {
    let mut preset_id = None;
    let mut force = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--preset" | "-p" => {
                let value = args
                    .get(index + 1)
                    .context("deixic-code config init --preset requires an id")?;
                preset_id = Some(value.to_owned());
                index += 2;
            }
            "--force" | "-f" | "-y" => {
                force = true;
                index += 1;
            }
            "--help" | "-h" => {
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Usage: deixic-code config init [--preset <id>] [--force]",
                        &[]
                    )
                );
                println!(
                    "{}",
                    crate::localization::cli_locale().format("\nPresets:", &[])
                );
                for preset in provider_presets() {
                    let note = preset.note.unwrap_or("");
                    if note.is_empty() {
                        println!("  {:<22} {}", preset.id, preset.name);
                    } else {
                        println!("  {:<22} {} — {note}", preset.id, preset.name);
                    }
                }
                return Ok(0);
            }
            other => bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Unknown option for deixic-code config init: {0}",
                    &[(other).to_string()]
                )
            ),
        }
    }

    println!(
        "{}",
        crate::localization::cli_locale().format("Initialize Deixic Code Configuration", &[])
    );
    let cwd = env::current_dir().context("failed to resolve current directory")?;
    let config_dir = cwd.join(".maestro");
    let config_path = config_dir.join("config.json");
    let prompts_dir = config_dir.join("prompts");

    if config_path.exists()
        && !force
        && !prompt_yes_no(
            &format!(
                "Config already exists at {}. Overwrite? (y/N): ",
                config_path.display()
            ),
            false,
        )?
    {
        println!(
            "{}",
            crate::localization::cli_locale().format("\nCancelled.", &[])
        );
        return Ok(0);
    }

    let presets = provider_presets();
    let preset = if let Some(id) = preset_id.as_deref() {
        match presets
            .iter()
            .find(|preset| preset.id.eq_ignore_ascii_case(id))
        {
            Some(preset) => {
                println!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("\nUsing preset: {0}", &[(preset.name).to_string()])
                );
                preset.clone()
            }
            None => {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Unknown preset \"{0}\", falling back to menu selection.",
                        &[(id).to_string()]
                    )
                );
                select_preset_interactively(&presets)?
            }
        }
    } else {
        select_preset_interactively(&presets)?
    };

    let mut use_env = false;
    let mut api_key_env = None;
    let mut api_key = None;
    if preset.requires_api_key {
        if stdin_is_tty() {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("\n2. How would you like to provide your API key?", &[])
            );
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("  1) Environment variable (recommended)", &[])
            );
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("  2) Direct in config (not recommended)", &[])
            );
            let choice = prompt_line("\nChoice (1-2): ")?;
            use_env = choice.trim() != "2";
            if use_env {
                let env_name = preset
                    .api_key_env
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{}_API_KEY", preset.id.to_ascii_uppercase()));
                api_key_env = Some(env_name.clone());
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "\nUsing environment variable: {0}",
                        std::slice::from_ref(&(env_name))
                    )
                );
            } else {
                let key = prompt_line("\nEnter API key: ")?;
                api_key = Some(key.trim().to_owned());
            }
        } else {
            // Non-interactive default: environment variable reference.
            let env_name = preset
                .api_key_env
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{}_API_KEY", preset.id.to_ascii_uppercase()));
            api_key_env = Some(env_name.clone());
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "\nUsing environment variable: {0}",
                    std::slice::from_ref(&(env_name))
                )
            );
            use_env = true;
        }
    } else if preset.managed {
        println!("{}", crate::localization::cli_locale().format("\nManaged gateway preset does not use a local API key. Run deixic-code evalops login after setup.", &[]));
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "\nLocal providers do not require API keys. Skipping step.",
                &[]
            )
        );
    }

    let create_prompts = if stdin_is_tty() {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "\n3. Would you like to use file references for prompts?",
                &[]
            )
        );
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "  This creates a prompts/ folder for better organization.",
                &[]
            )
        );
        prompt_yes_no("\nUse file references? (Y/n): ", true)?
    } else {
        // Non-interactive default: create prompts for a useful starter project.
        true
    };

    fs::create_dir_all(&config_dir)?;
    if create_prompts {
        fs::create_dir_all(&prompts_dir)?;
    }

    let mut provider = Map::new();
    provider.insert("id".into(), JsonValue::String(preset.id.to_owned()));
    provider.insert("name".into(), JsonValue::String(preset.name.to_owned()));
    if let Some(base_url) = resolved_base_url(&preset) {
        provider.insert("baseUrl".into(), JsonValue::String(base_url));
    }
    provider.insert("api".into(), JsonValue::String(preset.api.to_owned()));
    if let Some(env_name) = &api_key_env {
        provider.insert("apiKeyEnv".into(), JsonValue::String(env_name.clone()));
    }
    if let Some(key) = &api_key {
        provider.insert("apiKey".into(), JsonValue::String(key.clone()));
    }
    provider.insert(
        "models".into(),
        json!([{
            "id": preset.default_model,
            "name": if create_prompts { "{file:./prompts/system.md}" } else { "Default assistant" },
            "contextWindow": preset_context_window(&preset),
            "maxTokens": preset_max_tokens(&preset),
        }]),
    );

    let config = json!({
        "$schema": CONFIG_SCHEMA,
        "providers": [provider],
    });
    write_atomic(
        &config_path,
        &format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "\nCreated config {0}",
            &[(config_path.display()).to_string()]
        )
    );

    if create_prompts {
        let system_prompt_path = prompts_dir.join("system.md");
        write_atomic(&system_prompt_path, DEFAULT_SYSTEM_PROMPT)?;
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Created prompt {0}",
                &[(system_prompt_path.display()).to_string()]
            )
        );
    }

    if use_env {
        if let Some(env_name) = api_key_env {
            let env_example = cwd.join(".env.example");
            let addition = if env_example.exists() {
                format!("\n# Added by deixic-code init\n{env_name}=your-api-key-here\n")
            } else {
                format!("# Deixic Code Configuration\n{env_name}=your-api-key-here\n")
            };
            if env_example.exists() {
                let mut existing = fs::read_to_string(&env_example)?;
                existing.push_str(&addition);
                write_atomic(&env_example, &existing)?;
            } else {
                write_atomic(&env_example, &addition)?;
            }
            println!("Updated .env.example");
        }
    }

    println!(
        "{}",
        crate::localization::cli_locale().format("\nConfiguration initialized successfully!", &[])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format("Next steps:", &[])
    );
    if let Some(env_name) = provider
        .get("apiKeyEnv")
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
    {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "  1. Set {0} in your environment",
                std::slice::from_ref(&(env_name))
            )
        );
    }
    if create_prompts {
        println!("  2. Edit .maestro/prompts/system.md");
    }
    println!(
        "{}",
        crate::localization::cli_locale().format("  3. Run: deixic-code models list", &[])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format("  4. Start using: maestro \"your prompt\"", &[])
    );
    Ok(0)
}

async fn run_local(args: &[String]) -> Result<i32> {
    let mut check_only = false;
    let mut provider = None;
    let mut scope = Scope::Project;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--check" => {
                check_only = true;
                index += 1;
            }
            "--provider" => {
                let value = args.get(index + 1).context(
                    "deixic-code config local --provider requires lmstudio|ollama|llamacpp",
                )?;
                provider = Some(value.to_owned());
                index += 2;
            }
            "--scope" => {
                let value = args
                    .get(index + 1)
                    .context("deixic-code config local --scope requires project|user")?;
                scope = match value.as_str() {
                    "project" | "1" => Scope::Project,
                    "user" | "home" | "global" | "2" => Scope::User,
                    other => bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("unknown local scope: {0}", &[(other).to_string()])
                    ),
                };
                index += 2;
            }
            "--help" | "-h" => {
                println!("{}", crate::localization::cli_locale().format("Usage: deixic-code config local [--check] [--provider lmstudio|ollama|llamacpp] [--scope project|user]", &[]));
                return Ok(0);
            }
            other => bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Unknown option for deixic-code config local: {0}",
                    &[(other).to_string()]
                )
            ),
        }
    }

    println!(
        "{}",
        crate::localization::cli_locale().format("Local provider helper", &[])
    );
    // Non-interactive default when only --check or no TTY: check endpoints.
    if (check_only || provider.is_none() && !stdin_is_tty()) && provider.is_none() {
        check_only = true;
    }

    if check_only {
        let process_env = env::vars().collect();
        for (name, base_url) in local_endpoint_checks(&process_env) {
            println!("{}", check_local_endpoint(name, &base_url).await);
        }
        return Ok(0);
    }

    let provider_flag = provider.as_deref();
    let template_key = if let Some(provider_name) = provider_flag {
        match provider_name.to_ascii_lowercase().as_str() {
            "lmstudio" | "1" => "lmstudio",
            "ollama" | "2" => "ollama",
            "llamacpp" | "llama.cpp" | "llama-cpp" | "3" => "llamacpp",
            other => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unknown local provider: {0}", &[(other).to_string()])
            ),
        }
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format("  1) Add LM Studio provider", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("  2) Add Ollama provider", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("  3) Add llama.cpp provider (Qwen3.8-27B)", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("  4) Check local endpoints", &[])
        );
        println!("  5) Cancel");
        let choice = prompt_line("\nChoice (1-5): ")?;
        match choice.trim() {
            "4" => {
                let process_env = env::vars().collect();
                for (name, base_url) in local_endpoint_checks(&process_env) {
                    println!("{}", check_local_endpoint(name, &base_url).await);
                }
                return Ok(0);
            }
            "5" => {
                println!(
                    "{}",
                    crate::localization::cli_locale().format("\nCancelled.", &[])
                );
                return Ok(0);
            }
            "2" => "ollama",
            "3" => "llamacpp",
            _ => "lmstudio",
        }
    };

    let template = local_provider_template(template_key)?;
    let interactive_local = provider_flag.is_none();
    if interactive_local {
        println!(
            "{}",
            crate::localization::cli_locale().format("\nSave provider to:", &[])
        );
        println!("  1) Project (.maestro/local.json)");
        println!("  2) Home (~/.maestro/local.json)");
        let choice = prompt_line("\nChoice (1-2): ")?;
        scope = if choice.trim() == "2" {
            Scope::User
        } else {
            Scope::Project
        };
    }

    let target_dir = match scope {
        Scope::User => maestro_home_dir().context("Maestro home is unavailable")?,
        Scope::Project | Scope::Local => env::current_dir()?.join(".maestro"),
    };
    fs::create_dir_all(&target_dir)?;
    let local_path = target_dir.join("local.json");
    let mut config = load_local_config(&local_path)?;

    let customizable_local = stdin_is_tty() && interactive_local && template.id != "llamacpp";
    let provider_id = if customizable_local {
        let answer = prompt_line(&format!("\nProvider id ({}): ", template.id))?;
        if answer.trim().is_empty() {
            template.id.to_owned()
        } else {
            answer.trim().to_owned()
        }
    } else {
        template.id.to_owned()
    };
    let provider_name = if customizable_local {
        let answer = prompt_line(&format!("Provider name ({}): ", template.name))?;
        if answer.trim().is_empty() {
            template.name.to_owned()
        } else {
            answer.trim().to_owned()
        }
    } else {
        template.name.to_owned()
    };
    let base_url = if template.id == "llamacpp" {
        llamacpp_base_url(&env::vars().collect(), template.base_url)
    } else if customizable_local {
        let answer = prompt_line(&format!("Base URL ({}): ", template.base_url))?;
        if answer.trim().is_empty() {
            template.base_url.to_owned()
        } else {
            answer.trim().to_owned()
        }
    } else {
        template.base_url.to_owned()
    };
    let model_id = if customizable_local {
        let answer = prompt_line(&format!("Model id ({}): ", template.model_id))?;
        if answer.trim().is_empty() {
            template.model_id.to_owned()
        } else {
            answer.trim().to_owned()
        }
    } else {
        template.model_id.to_owned()
    };
    let model_name = if customizable_local {
        let answer = prompt_line(&format!("Model name ({}): ", template.model_name))?;
        if answer.trim().is_empty() {
            template.model_name.to_owned()
        } else {
            answer.trim().to_owned()
        }
    } else {
        template.model_name.to_owned()
    };
    let context_window = if customizable_local {
        let answer = prompt_line(&format!("Context window ({}): ", template.context_window))?;
        answer
            .trim()
            .parse::<u64>()
            .unwrap_or(template.context_window)
    } else {
        template.context_window
    };
    let max_tokens = if customizable_local {
        let answer = prompt_line(&format!("Max output tokens ({}): ", template.max_tokens))?;
        answer.trim().parse::<u64>().unwrap_or(template.max_tokens)
    } else {
        template.max_tokens
    };

    upsert_local_provider(
        &mut config,
        template.api,
        LocalProviderOverrides {
            id: provider_id,
            name: provider_name,
            base_url,
            model_id,
            model_name,
            context_window,
            max_tokens,
        },
    );
    write_atomic(
        &local_path,
        &format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "\nUpdated local config {0}",
            &[(local_path.display()).to_string()]
        )
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Reload your models (/model) after starting the local runtime to use the new provider.",
            &[]
        )
    );
    if template.id == "llamacpp" {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Set LLAMA_CPP_BASE_URL before starting Deixic Code to override {0}.",
                &[(template.base_url).to_string()]
            )
        );
    }
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Tip: run `deixic-code config local --check` to check connectivity.",
            &[]
        )
    );
    Ok(0)
}

// ─────────────────────────────────────────────────────────────
// Config path + inspection helpers
// ─────────────────────────────────────────────────────────────

fn config_path_entries() -> Result<Vec<(String, PathBuf, bool)>> {
    let home = maestro_home_dir().context("Maestro home is unavailable")?;
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut entries = vec![
        (
            "user config.json".into(),
            home.join("config.json"),
            home.join("config.json").exists(),
        ),
        (
            "user local.json".into(),
            home.join("local.json"),
            home.join("local.json").exists(),
        ),
        (
            "user config.toml".into(),
            home.join("config.toml"),
            home.join("config.toml").exists(),
        ),
        (
            "project config.json".into(),
            cwd.join(".maestro/config.json"),
            cwd.join(".maestro/config.json").exists(),
        ),
        (
            "project local.json".into(),
            cwd.join(".maestro/local.json"),
            cwd.join(".maestro/local.json").exists(),
        ),
        (
            "project config.toml".into(),
            cwd.join(".maestro/config.toml"),
            cwd.join(".maestro/config.toml").exists(),
        ),
        (
            "project config.local.toml".into(),
            cwd.join(".maestro/config.local.toml"),
            cwd.join(".maestro/config.local.toml").exists(),
        ),
    ];

    let legacy = home.join("models.json");
    if legacy.exists() {
        entries.push(("legacy models.json".into(), legacy.clone(), true));
    }
    if let Some(path) = env_path("MAESTRO_MODELS_FILE") {
        let exists = path.exists();
        entries.push(("MAESTRO_MODELS_FILE".into(), path, exists));
    }
    if let Some(path) = env_path("MAESTRO_CONFIG") {
        let exists = path.exists();
        entries.push(("MAESTRO_CONFIG".into(), path, exists));
    }
    Ok(entries)
}

fn provider_json_paths() -> Result<Vec<PathBuf>> {
    let home = maestro_home_dir().context("Maestro home is unavailable")?;
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut paths = vec![home.join("config.json"), home.join("local.json")];
    for candidate in [
        cwd.join(".maestro/config.json"),
        cwd.join(".maestro/local.json"),
    ] {
        if candidate.exists() {
            paths.push(candidate);
        }
    }
    let legacy = home.join("models.json");
    if legacy.exists() {
        paths.push(legacy);
    }
    if let Some(path) = env_path("MAESTRO_MODELS_FILE") {
        paths.push(path);
    }
    if let Some(path) = env_path("MAESTRO_CONFIG") {
        paths.push(path);
    }
    Ok(paths)
}

fn inspect_config() -> Result<JsonValue> {
    let path_entries = config_path_entries()?;
    let mut sources = Vec::new();
    for (label, path, exists) in &path_entries {
        sources.push(json!({
            "label": label,
            "path": path.display().to_string(),
            "exists": exists,
            "active": *exists,
            "loaded": *exists,
        }));
    }

    let mut providers_by_id: BTreeMap<String, JsonValue> = BTreeMap::new();
    let mut file_references = Vec::new();
    let mut env_vars: BTreeSet<String> = BTreeSet::new();

    for path in provider_json_paths()? {
        if !path.exists() {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        collect_file_and_env_refs(&raw, &path, &mut file_references, &mut env_vars);

        match parse_jsonish(&raw) {
            Ok(JsonValue::Object(map)) => {
                if let Some(JsonValue::Array(providers)) = map.get("providers") {
                    for provider in providers {
                        if let Some(id) = provider.get("id").and_then(JsonValue::as_str) {
                            if let Some(env_name) =
                                provider.get("apiKeyEnv").and_then(JsonValue::as_str)
                            {
                                env_vars.insert(env_name.to_owned());
                            }
                            providers_by_id.insert(id.to_owned(), summarize_provider(provider));
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }

    let providers: Vec<_> = providers_by_id.into_values().collect();
    let env_var_rows: Vec<_> = env_vars
        .into_iter()
        .map(|name| {
            let value = env::var(&name).ok();
            let set = value.is_some();
            let masked = value.as_deref().map(mask_secret);
            json!({
                "name": name,
                "set": set,
                "maskedValue": masked,
            })
        })
        .collect();

    let settings = effective_settings_summary();

    let mut inspection = json!({
        "schemaVersion": "evalops.maestro.config-inspection.v2",
        "sources": sources,
        "providers": providers,
        "fileReferences": file_references,
        "envVars": env_var_rows,
        "settings": settings,
    });
    let generation = config_generation_sha256(&inspection);
    inspection["generation"] = JsonValue::String(generation);
    Ok(inspection)
}

fn config_generation_sha256(inspection: &JsonValue) -> String {
    let mut generation_input = inspection.clone();
    if let Some(object) = generation_input.as_object_mut() {
        object.remove("generation");
    }
    if let Some(env_vars) = generation_input
        .get_mut("envVars")
        .and_then(JsonValue::as_array_mut)
    {
        for env_var in env_vars {
            if let Some(object) = env_var.as_object_mut() {
                object.remove("maskedValue");
            }
        }
    }
    crate::evidence::canonical_json_sha256(&generation_input)
}

fn validate_config() -> Result<JsonValue> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut config_files = Vec::new();
    let mut providers = 0_u64;
    let mut models = 0_u64;
    let mut file_references = Vec::new();
    let mut env_vars = Vec::new();

    for path in provider_json_paths()? {
        if !path.exists() {
            continue;
        }
        config_files.push(path.display().to_string());
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) => {
                errors.push(format!("Failed to read {}: {error}", path.display()));
                continue;
            }
        };

        let mut refs = Vec::new();
        let mut vars = BTreeSet::new();
        collect_file_and_env_refs(&raw, &path, &mut refs, &mut vars);
        for file_ref in &refs {
            let ref_path = file_ref["path"].as_str().unwrap_or("");
            file_references.push(ref_path.to_owned());
            if !file_ref["exists"].as_bool().unwrap_or(false) {
                errors.push(format!("File reference not found: {ref_path}"));
            }
        }
        for var in vars {
            env_vars.push(var.clone());
            if env::var(&var).is_err() {
                warnings.push(format!("Environment variable not set: {var}"));
            }
        }

        match parse_jsonish(&raw) {
            Ok(JsonValue::Object(map)) => {
                if let Some(JsonValue::Array(list)) = map.get("providers") {
                    providers += list.len() as u64;
                    for provider in list {
                        let model_count = provider
                            .get("models")
                            .and_then(JsonValue::as_array)
                            .map(|models| models.len() as u64)
                            .unwrap_or(0);
                        models += model_count;
                        let id = provider
                            .get("id")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("(missing-id)");
                        let has_override =
                            provider.get("baseUrl").is_some() || provider.get("headers").is_some();
                        if model_count == 0 && !has_override {
                            warnings.push(format!(
                                "Provider \"{id}\" has no models and no override settings (baseUrl/headers); entry has no effect."
                            ));
                        }
                    }
                }
            }
            Ok(_) => errors.push(format!(
                "Failed to parse {}: root value must be an object",
                path.display()
            )),
            Err(error) => errors.push(format!("Failed to parse {}: {error}", path.display())),
        }
    }

    // Validate TOML settings files when present.
    if let Ok(home) = maestro_home_dir().context("home") {
        let toml_path = home.join("config.toml");
        if toml_path.exists() {
            config_files.push(toml_path.display().to_string());
            if let Err(error) = fs::read_to_string(&toml_path).and_then(|raw| {
                toml::from_str::<TomlValue>(&raw)
                    .map(|_| ())
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
            }) {
                errors.push(format!("Failed to parse {}: {error}", toml_path.display()));
            }
        }
    }
    if let Ok(cwd) = env::current_dir() {
        for rel in [".maestro/config.toml", ".maestro/config.local.toml"] {
            let path = cwd.join(rel);
            if !path.exists() {
                continue;
            }
            config_files.push(path.display().to_string());
            if let Err(error) = fs::read_to_string(&path).and_then(|raw| {
                toml::from_str::<TomlValue>(&raw)
                    .map(|_| ())
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
            }) {
                errors.push(format!("Failed to parse {}: {error}", path.display()));
            }
        }
    }

    if config_files.is_empty() {
        warnings.push("No config files found".into());
    }

    let valid = errors.is_empty();
    Ok(json!({
        "valid": valid,
        "errors": errors,
        "warnings": warnings,
        "summary": {
            "configFiles": config_files,
            "providers": providers,
            "models": models,
            "fileReferences": file_references,
            "envVars": env_vars,
        }
    }))
}

fn summarize_provider(provider: &JsonValue) -> JsonValue {
    let models = provider
        .get("models")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    let api_key_source =
        if let Some(env_name) = provider.get("apiKeyEnv").and_then(JsonValue::as_str) {
            Some(format!("env:{env_name}"))
        } else if provider.get("apiKey").and_then(JsonValue::as_str).is_some() {
            Some("direct (hardcoded)".to_owned())
        } else {
            None
        };
    let base_url = provider
        .get("baseUrl")
        .and_then(JsonValue::as_str)
        .unwrap_or("(auto-generated)");
    let model_rows: Vec<_> = models
        .iter()
        .map(|model| {
            json!({
                "id": model.get("id").and_then(JsonValue::as_str).unwrap_or("?"),
                "name": model.get("name").and_then(JsonValue::as_str).unwrap_or(""),
                "reasoning": model.get("reasoning").and_then(JsonValue::as_bool),
                "input": model.get("input").cloned(),
            })
        })
        .collect();

    json!({
        "id": provider.get("id").and_then(JsonValue::as_str).unwrap_or("?"),
        "name": provider.get("name").and_then(JsonValue::as_str).unwrap_or(""),
        "baseUrl": base_url,
        "enabled": provider.get("enabled").and_then(JsonValue::as_bool).unwrap_or(true),
        "apiKeySource": api_key_source,
        "options": provider.get("options").cloned(),
        "modelCount": models.len(),
        "models": model_rows,
    })
}

fn collect_file_and_env_refs(
    raw: &str,
    config_path: &Path,
    file_references: &mut Vec<JsonValue>,
    env_vars: &mut BTreeSet<String>,
) {
    let file_re = regex::Regex::new(r"\{file:([^}]+)\}").expect("file ref regex");
    let env_re = regex::Regex::new(r"\{env:([^}]+)\}").expect("env ref regex");
    for caps in file_re.captures_iter(raw) {
        let matched = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let expanded = expand_path_ref(matched, config_path);
        let exists = expanded.exists();
        let size = if exists {
            fs::metadata(&expanded).ok().map(|meta| meta.len())
        } else {
            None
        };
        file_references.push(json!({
            "path": expanded.display().to_string(),
            "exists": exists,
            "size": size,
        }));
    }
    for caps in env_re.captures_iter(raw) {
        if let Some(name) = caps.get(1).map(|m| m.as_str()) {
            if !name.is_empty() {
                env_vars.insert(name.to_owned());
            }
        }
    }
}

fn expand_path_ref(raw: &str, config_path: &Path) -> PathBuf {
    let trimmed = raw.trim();
    let path = if let Some(rest) = trimmed.strip_prefix("~/") {
        dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(trimmed))
    } else if trimmed == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"))
    } else {
        PathBuf::from(trimmed)
    };
    if path.is_absolute() {
        path
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    }
}

fn effective_settings_summary() -> JsonValue {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Prefer ~/.maestro/config.toml (current product home), fall back to legacy loader paths.
    let mut settings = BTreeMap::new();
    if let Some(home) = maestro_home_dir() {
        if let Some(value) = load_toml(&home.join("config.toml")) {
            merge_settings_summary(&mut settings, &value);
        }
    }
    if let Some(value) = load_toml(&cwd.join(".maestro/config.toml")) {
        merge_settings_summary(&mut settings, &value);
    }
    if let Some(value) = load_toml(&cwd.join(".maestro/config.local.toml")) {
        merge_settings_summary(&mut settings, &value);
    }
    // Also surface values from the legacy .composer TOML loader when present.
    let legacy = crate::config::load_config(&cwd, env::var("MAESTRO_PROFILE").ok().as_deref());
    if let Some(model) = legacy.model {
        settings.insert("model".into(), JsonValue::String(model));
    }
    if let Some(provider) = legacy.model_provider {
        settings.insert("model_provider".into(), JsonValue::String(provider));
    }
    if let Some(policy) = legacy.approval_policy {
        settings.insert(
            "approval_policy".into(),
            JsonValue::String(format!("{policy:?}").to_ascii_lowercase()),
        );
    }
    if let Some(sandbox) = legacy.sandbox_mode {
        settings.insert(
            "sandbox_mode".into(),
            JsonValue::String(format!("{sandbox:?}")),
        );
    }
    JsonValue::Object(settings.into_iter().collect())
}

fn merge_settings_summary(target: &mut BTreeMap<String, JsonValue>, value: &TomlValue) {
    let Some(table) = value.as_table() else {
        return;
    };
    for key in [
        "model",
        "model_provider",
        "approval_policy",
        "sandbox_mode",
        "profile",
    ] {
        if let Some(entry) = table.get(key) {
            target.insert(key.to_owned(), toml_to_json(entry));
        }
    }
}

// ─────────────────────────────────────────────────────────────
// TOML get/set helpers
// ─────────────────────────────────────────────────────────────

fn settings_path(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::User => Ok(maestro_home_dir()
            .context("Maestro home is unavailable")?
            .join("config.toml")),
        Scope::Project => Ok(env::current_dir()?.join(".maestro/config.toml")),
        Scope::Local => Ok(env::current_dir()?.join(".maestro/config.local.toml")),
    }
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "user",
        Scope::Project => "project",
        Scope::Local => "local",
    }
}

fn parse_scope_json(args: &[String], default: Scope) -> Result<(Scope, bool)> {
    let mut scope = default;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--scope" => {
                let value = args
                    .get(index + 1)
                    .context("--scope requires user|project|local")?;
                scope = Scope::parse(value)
                    .with_context(|| format!("unknown config scope: {value}"))?;
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--help" | "-h" => {}
            other if other.starts_with('-') => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unknown option: {0}", &[(other).to_string()])
            ),
            other => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unexpected argument: {0}", &[(other).to_string()])
            ),
        }
    }
    Ok((scope, json))
}

/// Only an absent config may start empty. Read/parse failures must preserve it.
fn load_toml_for_update(path: &Path) -> Result<TomlValue> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(TomlValue::Table(toml::map::Map::new()));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    // Parser diagnostics can contain config values. Keep those out of the TUI.
    toml::from_str::<TomlValue>(&raw)
        .map_err(|_| anyhow::anyhow!("invalid TOML in {}; file left unchanged", path.display()))
}

fn load_toml(path: &Path) -> Option<TomlValue> {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| toml::from_str::<TomlValue>(&raw).ok())
}

fn flatten_keys(prefix: &str, value: &TomlValue, out: &mut Vec<String>) {
    match value {
        TomlValue::Table(table) => {
            if table.is_empty() && !prefix.is_empty() {
                out.push(prefix.to_owned());
            }
            for (key, child) in table {
                let next = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_keys(&next, child, out);
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.push(prefix.to_owned());
            }
        }
    }
}

fn get_dotted(value: &TomlValue, key: &str) -> Option<TomlValue> {
    let mut current = value;
    for part in key.split('.') {
        current = current.as_table()?.get(part)?;
    }
    Some(current.clone())
}

fn set_dotted(root: &mut TomlValue, key: &str, value: TomlValue) -> Result<()> {
    if key.trim().is_empty() || key.split('.').any(|part| part.is_empty()) {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("invalid config key: {0}", &[(key).to_string()])
        );
    }
    if !matches!(root, TomlValue::Table(_)) {
        *root = TomlValue::Table(toml::map::Map::new());
    }
    let parts: Vec<&str> = key.split('.').collect();
    let mut cursor = root.as_table_mut().context("config root must be a table")?;
    for part in &parts[..parts.len() - 1] {
        let entry = cursor
            .entry(part.to_string())
            .or_insert_with(|| TomlValue::Table(toml::map::Map::new()));
        if !matches!(entry, TomlValue::Table(_)) {
            *entry = TomlValue::Table(toml::map::Map::new());
        }
        cursor = entry
            .as_table_mut()
            .context("failed to navigate config table")?;
    }
    cursor.insert(parts[parts.len() - 1].to_owned(), value);
    Ok(())
}

fn parse_cli_value(raw: &str) -> TomlValue {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("true") {
        return TomlValue::Boolean(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return TomlValue::Boolean(false);
    }
    if let Ok(number) = trimmed.parse::<i64>() {
        return TomlValue::Integer(number);
    }
    if let Ok(number) = trimmed.parse::<f64>() {
        return TomlValue::Float(number);
    }
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return TomlValue::String(trimmed[1..trimmed.len() - 1].to_owned());
    }
    // Try TOML literal forms (arrays/tables/inline).
    let wrapped = format!("value = {trimmed}");
    if let Ok(table) = wrapped.parse::<toml::Table>() {
        if let Some(value) = table.get("value") {
            return value.clone();
        }
    }
    TomlValue::String(trimmed.to_owned())
}

fn format_toml_value(value: &TomlValue) -> String {
    match value {
        TomlValue::String(text) => text.clone(),
        TomlValue::Integer(number) => number.to_string(),
        TomlValue::Float(number) => number.to_string(),
        TomlValue::Boolean(flag) => flag.to_string(),
        TomlValue::Datetime(datetime) => datetime.to_string(),
        TomlValue::Array(_) | TomlValue::Table(_) => {
            toml::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        }
    }
}

fn toml_to_json(value: &TomlValue) -> JsonValue {
    match value {
        TomlValue::String(text) => JsonValue::String(text.clone()),
        TomlValue::Integer(number) => json!(number),
        TomlValue::Float(number) => json!(number),
        TomlValue::Boolean(flag) => JsonValue::Bool(*flag),
        TomlValue::Datetime(datetime) => JsonValue::String(datetime.to_string()),
        TomlValue::Array(items) => JsonValue::Array(items.iter().map(toml_to_json).collect()),
        TomlValue::Table(table) => {
            let mut map = Map::new();
            for (key, child) in table {
                map.insert(key.clone(), toml_to_json(child));
            }
            JsonValue::Object(map)
        }
    }
}

// ─────────────────────────────────────────────────────────────
// Provider presets + local templates
// ─────────────────────────────────────────────────────────────

/// The limits written into a generated config for `preset`'s default model.
///
/// The catalog wins over the preset's own numbers whenever it describes the
/// model, because the preset numbers were typed by hand and drifted: the
/// `openai` preset declared a 200,000-token window for `gpt-4o-mini`, whose
/// real window is 128,000, so a generated config invited requests past the
/// model's limit. `google-gemini` declared 200,000 for `gemini-2.5-flash` at
/// 1,048,576, and `openrouter` declared an 8,192-token output ceiling for
/// `openai/o4-mini` at 100,000.
///
/// `find_model` is used rather than the bundled snapshot directly because this
/// runs once in an interactive `config` flow, not on a request path, and it
/// picks up a locally discovered model the snapshot does not carry.
fn preset_catalog_limits(preset: &ProviderPreset) -> Option<(u64, Option<u64>)> {
    let model = crate::model_catalog::find_model(preset.default_model)?;
    let context = u64::from(model.capabilities.context_tokens);
    (context > 0).then(|| (context, model.capabilities.output_tokens.map(u64::from)))
}

fn preset_context_window(preset: &ProviderPreset) -> u64 {
    preset_catalog_limits(preset)
        .map(|(context, _)| context)
        .or(preset.context_window)
        .unwrap_or(200_000)
}

fn preset_max_tokens(preset: &ProviderPreset) -> u64 {
    preset_catalog_limits(preset)
        .and_then(|(_, output)| output)
        .or(preset.max_tokens)
        .unwrap_or(8192)
}

fn provider_presets() -> Vec<ProviderPreset> {
    let mut presets = vec![
        ProviderPreset {
            id: "anthropic",
            name: "Anthropic (Claude)",
            api: "anthropic-messages",
            default_model: maestro_runtime_contracts::DefaultModel::AnthropicFlagship.id(),
            base_url: Some("https://api.anthropic.com"),
            requires_api_key: true,
            api_key_env: Some("ANTHROPIC_API_KEY"),
            note: None,
            context_window: Some(1_000_000),
            max_tokens: Some(128_000),
            managed: false,
        },
        ProviderPreset {
            id: "openai",
            name: "OpenAI (Responses)",
            api: "openai-responses",
            default_model: "gpt-4o-mini",
            base_url: Some("https://api.openai.com/v1"),
            requires_api_key: true,
            api_key_env: Some("OPENAI_API_KEY"),
            note: None,
            context_window: None,
            max_tokens: None,
            managed: false,
        },
        ProviderPreset {
            id: "groq",
            name: "Groq",
            api: "openai-completions",
            default_model: "llama-3.3-70b-versatile",
            base_url: Some("https://api.groq.com/openai/v1"),
            requires_api_key: true,
            api_key_env: Some("GROQ_API_KEY"),
            note: None,
            context_window: None,
            max_tokens: None,
            managed: false,
        },
        ProviderPreset {
            id: "openrouter",
            name: "OpenRouter",
            api: "openai-completions",
            default_model: "openai/o4-mini",
            base_url: Some("https://openrouter.ai/api/v1"),
            requires_api_key: true,
            api_key_env: Some("OPENROUTER_API_KEY"),
            note: Some("Supports many upstreams; accepts OpenAI-compatible keys"),
            context_window: None,
            max_tokens: None,
            managed: false,
        },
    ];

    // Managed gateway presets (always listed; gateway base from env when set).
    for (id, name, api, default_model, note) in [
        (
            "evalops",
            "EvalOps Managed Gateway (OpenAI Responses)",
            "openai-responses",
            "gpt-4o-mini",
            "Requires /login evalops and routes managed OpenAI responses through the gateway",
        ),
        (
            "evalops-anthropic",
            "EvalOps Managed Gateway (Anthropic Messages)",
            "anthropic-messages",
            "claude-sonnet-4-5",
            "Requires /login evalops and routes managed Anthropic messages through the gateway",
        ),
        (
            "evalops-google",
            "EvalOps Managed Gateway (Google Gemini)",
            "openai-completions",
            "gemini-2.5-pro",
            "Requires /login evalops and routes managed Google Gemini chat completions through the gateway",
        ),
    ] {
        presets.push(ProviderPreset {
            id,
            name,
            api,
            default_model,
            base_url: None,
            requires_api_key: false,
            api_key_env: None,
            note: Some(note),
            context_window: None,
            max_tokens: None,
            managed: true,
        });
    }

    presets.extend([
        ProviderPreset {
            id: "google-gemini",
            name: "Google Gemini API",
            api: "google-generative-ai",
            default_model: "gemini-2.5-flash",
            base_url: Some("https://generativelanguage.googleapis.com/v1beta"),
            requires_api_key: true,
            api_key_env: Some("GEMINI_API_KEY"),
            note: None,
            context_window: None,
            max_tokens: None,
            managed: false,
        },
        ProviderPreset {
            id: "deepseek",
            name: "DeepSeek",
            api: "openai-completions",
            default_model: "deepseek-chat",
            base_url: Some("https://api.deepseek.com/v1"),
            requires_api_key: true,
            api_key_env: Some("DEEPSEEK_API_KEY"),
            note: None,
            context_window: Some(131_072),
            max_tokens: Some(8192),
            managed: false,
        },
        ProviderPreset {
            id: "moonshot",
            name: "Moonshot AI (Kimi)",
            api: "openai-completions",
            default_model: "kimi-k2.6",
            base_url: Some("https://api.moonshot.ai/v1"),
            requires_api_key: true,
            api_key_env: Some("MOONSHOT_API_KEY"),
            note: Some("International endpoint; KIMI_API_KEY is also accepted."),
            context_window: Some(262_144),
            max_tokens: Some(16_384),
            managed: false,
        },
        ProviderPreset {
            id: "lmstudio",
            name: "LM Studio (local)",
            api: "openai-responses",
            default_model: "lmstudio/gemma-3n",
            base_url: Some("http://127.0.0.1:1234/v1"),
            requires_api_key: false,
            api_key_env: None,
            note: None,
            context_window: None,
            max_tokens: None,
            managed: false,
        },
        ProviderPreset {
            id: "ollama",
            name: "Ollama (local)",
            api: "openai-responses",
            default_model: "ollama/llama3.2",
            base_url: Some("http://localhost:11434/v1"),
            requires_api_key: false,
            api_key_env: None,
            note: None,
            context_window: None,
            max_tokens: None,
            managed: false,
        },
    ]);
    presets
}

fn resolved_base_url(preset: &ProviderPreset) -> Option<String> {
    if preset.managed {
        let gateway = env::var("MAESTRO_LLM_GATEWAY_URL")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_GATEWAY.to_owned());
        return Some(gateway);
    }
    preset.base_url.map(str::to_owned)
}

fn select_preset_interactively(presets: &[ProviderPreset]) -> Result<ProviderPreset> {
    println!(
        "{}",
        crate::localization::cli_locale().format("\n1. Choose your provider", &[])
    );
    for (index, preset) in presets.iter().enumerate() {
        match preset.note {
            Some(note) => println!("  {}) {} — {note}", index + 1, preset.name),
            None => println!("  {}) {}", index + 1, preset.name),
        }
    }
    let choice = prompt_line(&format!("\nProvider (1-{}): ", presets.len()))?;
    let index = choice
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|value| value.checked_sub(1))
        .filter(|value| *value < presets.len())
        .unwrap_or(0);
    Ok(presets[index].clone())
}

struct LocalProviderTemplate {
    id: &'static str,
    name: &'static str,
    base_url: &'static str,
    api: &'static str,
    model_id: &'static str,
    model_name: &'static str,
    context_window: u64,
    max_tokens: u64,
}

fn local_provider_template(key: &str) -> Result<LocalProviderTemplate> {
    match key {
        "lmstudio" => Ok(LocalProviderTemplate {
            id: "lmstudio",
            name: "LM Studio (local)",
            base_url: "http://127.0.0.1:1234/v1",
            api: "openai-responses",
            model_id: "lmstudio/gemma-3n",
            model_name: "Gemma 3n (local)",
            context_window: 200_000,
            max_tokens: 8192,
        }),
        "ollama" => Ok(LocalProviderTemplate {
            id: "ollama",
            name: "Ollama (local)",
            base_url: "http://localhost:11434/v1",
            api: "openai-responses",
            model_id: "ollama/llama3.1",
            model_name: "Llama 3.1 (local)",
            context_window: 128_000,
            max_tokens: 8192,
        }),
        "llamacpp" => Ok(LocalProviderTemplate {
            id: "llamacpp",
            name: "llama.cpp (local)",
            base_url: "http://127.0.0.1:8080/v1",
            api: "openai-completions",
            model_id: "llamacpp/Qwen3.8-27B",
            model_name: "Qwen3.8-27B (local)",
            context_window: 262_144,
            max_tokens: 32_768,
        }),
        other => bail!(
            "{}",
            crate::localization::cli_locale().format(
                "unknown local provider template: {0}",
                &[(other).to_string()]
            )
        ),
    }
}

fn llamacpp_base_url(env: &BTreeMap<String, String>, default: &str) -> String {
    let env: HashMap<String, String> = env.clone().into_iter().collect();
    local_runtime_endpoints(&env)
        .into_iter()
        .find(|endpoint| endpoint.provider == "llamacpp")
        .map_or_else(|| default.to_owned(), |endpoint| endpoint.base_url)
}

fn local_endpoint_checks(env: &BTreeMap<String, String>) -> Vec<(&'static str, String)> {
    let env: HashMap<String, String> = env.clone().into_iter().collect();
    local_runtime_endpoints(&env)
        .into_iter()
        .map(|endpoint| (endpoint.display_name, endpoint.base_url))
        .collect()
}

struct LocalProviderOverrides {
    id: String,
    name: String,
    base_url: String,
    model_id: String,
    model_name: String,
    context_window: u64,
    max_tokens: u64,
}

fn load_local_config(path: &Path) -> Result<JsonValue> {
    if !path.exists() {
        return Ok(json!({
            "$schema": CONFIG_SCHEMA,
            "providers": [],
        }));
    }
    let raw = fs::read_to_string(path)?;
    match parse_jsonish(&raw)? {
        JsonValue::Object(map) => Ok(JsonValue::Object(map)),
        _ => Ok(json!({
            "$schema": CONFIG_SCHEMA,
            "providers": [],
        })),
    }
}

fn upsert_local_provider(config: &mut JsonValue, api: &str, overrides: LocalProviderOverrides) {
    let root = config.as_object_mut().expect("local config object");
    root.entry("$schema")
        .or_insert_with(|| JsonValue::String(CONFIG_SCHEMA.to_owned()));
    let providers = root
        .entry("providers")
        .or_insert_with(|| JsonValue::Array(Vec::new()));
    let list = providers.as_array_mut().expect("providers array");
    let provider_id = overrides.id.clone();
    let entry = json!({
        "id": overrides.id,
        "name": overrides.name,
        "api": api,
        "baseUrl": overrides.base_url,
        "models": [{
            "id": overrides.model_id,
            "name": overrides.model_name,
            "contextWindow": overrides.context_window,
            "maxTokens": overrides.max_tokens,
            "input": ["text"],
        }],
    });
    if let Some(existing) = list.iter_mut().find(|provider| {
        provider.get("id").and_then(JsonValue::as_str) == Some(provider_id.as_str())
    }) {
        *existing = entry;
    } else {
        list.push(entry);
    }
}

async fn check_local_endpoint(name: &str, base_url: &str) -> String {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(error) => return format!("[{name}] client error: {error}"),
    };
    let url = match local_metadata_url(base_url) {
        Ok(url) => url,
        Err(error) => return format!("[{name}] invalid url: {error}"),
    };
    let url = match reqwest::Url::parse(&url) {
        Ok(url) => url,
        Err(_) => return format!("[{name}] invalid url"),
    };
    let redacted_url = {
        let mut redacted = url.clone();
        let _ = redacted.set_username("");
        let _ = redacted.set_password(None);
        redacted.set_query(None);
        redacted.set_fragment(None);
        redacted.to_string()
    };
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() => {
            format!(
                "[{name}] ok · responded with {}",
                response.status().as_u16()
            )
        }
        Ok(response) => format!("[{name}] warn · HTTP {}", response.status().as_u16()),
        Err(error) => format!("[{name}] error · {} · {redacted_url}", error.without_url()),
    }
}

// ─────────────────────────────────────────────────────────────
// Generic helpers
// ─────────────────────────────────────────────────────────────

fn parse_jsonish(raw: &str) -> Result<JsonValue> {
    match serde_json::from_str(raw) {
        Ok(value) => Ok(value),
        Err(first) => {
            let stripped = strip_json_comments(raw);
            serde_json::from_str(&stripped).map_err(|_| anyhow::anyhow!(first))
        }
    }
}

fn strip_json_comments(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                output.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                for next in chars.by_ref() {
                    if next == '\n' {
                        output.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => output.push(ch),
        }
    }
    output
}

fn display_path(path: &Path) -> String {
    let rendered = path.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let home_str = home.display().to_string();
        if rendered == home_str {
            return "~".into();
        }
        let prefix = format!("{home_str}{}", std::path::MAIN_SEPARATOR);
        if let Some(rest) = rendered.strip_prefix(&prefix) {
            return format!("~/{rest}");
        }
        // Also handle forward-slash normalization.
        let normalized = rendered.replace('\\', "/");
        let home_norm = home_str.replace('\\', "/");
        if let Some(rest) = normalized.strip_prefix(&(home_norm + "/")) {
            return format!("~/{rest}");
        }
    }
    rendered
}

fn mask_secret(value: &str) -> String {
    if value.len() > 8 {
        format!("{}{}", &value[..4], "•".repeat(8))
    } else {
        "••••••••".into()
    }
}

fn format_bytes(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut index = 0_usize;
    let mut value = bytes as f64;
    while value >= 1024.0 && index + 1 < units.len() {
        value /= 1024.0;
        index += 1;
    }
    format!("{value:.1} {}", units[index])
}

fn stdin_is_tty() -> bool {
    // Unit tests must stay non-interactive even when the test runner allocates
    // a pseudo-terminal (as GitHub Actions does for this workspace).
    if cfg!(test) {
        return false;
    }
    io::IsTerminal::is_terminal(&io::stdin())
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line)
}

fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    if !stdin_is_tty() {
        return Ok(default_yes);
    }
    let answer = prompt_line(prompt)?;
    let trimmed = answer.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(trimmed.as_str(), "y" | "yes"))
}

const DEFAULT_SYSTEM_PROMPT: &str = r"# System Prompt

You are a helpful AI coding assistant.

## Guidelines

- Write clean, maintainable code
- Follow best practices
- Provide clear explanations
- Test your suggestions

## Style

- Be concise but thorough
- Use examples when helpful
- Ask clarifying questions when needed
";

#[cfg(test)]
mod tests {

    #[test]
    fn provider_presets_take_their_limits_from_the_catalog() {
        // Every preset whose default model the catalog describes must publish
        // the catalog's numbers. These three disagreed: openai declared a
        // 200,000-token window for gpt-4o-mini (real 128,000), which invited
        // requests past the model's limit; google-gemini declared 200,000 for
        // gemini-2.5-flash (real 1,048,576); openrouter declared an 8,192
        // output ceiling for openai/o4-mini (real 100,000).
        let mut checked = 0;
        for preset in provider_presets() {
            let Some(model) = crate::model_catalog::find_model(preset.default_model) else {
                continue;
            };
            if model.capabilities.context_tokens == 0 {
                continue;
            }
            checked += 1;
            assert_eq!(
                preset_context_window(&preset),
                u64::from(model.capabilities.context_tokens),
                "{} contextWindow",
                preset.id
            );
            if let Some(output) = model.capabilities.output_tokens {
                assert_eq!(
                    preset_max_tokens(&preset),
                    u64::from(output),
                    "{} maxTokens",
                    preset.id
                );
            }
        }
        assert!(
            checked >= 4,
            "expected several presets to resolve through the catalog, saw {checked}"
        );
    }

    #[test]
    fn uncatalogued_preset_models_keep_their_declared_limits() {
        // deepseek-chat and kimi-k2.6 are not in the bundled snapshot, so the
        // hand-declared numbers remain the answer for them.
        let presets = provider_presets();
        let deepseek = presets
            .iter()
            .find(|preset| preset.id == "deepseek")
            .expect("deepseek preset");
        if crate::model_catalog::find_model(deepseek.default_model).is_none() {
            assert_eq!(preset_context_window(deepseek), 131_072);
            assert_eq!(preset_max_tokens(deepseek), 8192);
        }
    }
    use super::*;
    use tempfile::TempDir;

    fn env_lock() -> tokio::sync::OwnedMutexGuard<()> {
        crate::config::test_process_env_lock()
    }

    fn restore_env(name: &str, value: Option<String>) {
        match value {
            Some(value) => env::set_var(name, value),
            None => env::remove_var(name),
        }
    }

    #[test]
    fn deployment_inspection_rejects_mutation_and_probe_flags() {
        for args in [
            vec![],
            vec!["--live"],
            vec!["--json", "--live"],
            vec!["--json", "--set"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(
                run_deployment(&args)
                    .unwrap_err()
                    .to_string()
                    .contains("Usage:")
            );
        }
    }

    #[test]
    fn parse_cli_value_handles_scalars_and_strings() {
        assert_eq!(parse_cli_value("true"), TomlValue::Boolean(true));
        assert_eq!(parse_cli_value("42"), TomlValue::Integer(42));
        assert_eq!(parse_cli_value("gpt-5"), TomlValue::String("gpt-5".into()));
        assert_eq!(
            parse_cli_value("\"quoted\""),
            TomlValue::String("quoted".into())
        );
    }

    #[test]
    fn dotted_get_set_round_trips() {
        let mut root = TomlValue::Table(toml::map::Map::new());
        set_dotted(
            &mut root,
            "history.persistence",
            TomlValue::String("save-all".into()),
        )
        .unwrap();
        set_dotted(&mut root, "model", TomlValue::String("gpt-5".into())).unwrap();
        assert_eq!(
            get_dotted(&root, "history.persistence"),
            Some(TomlValue::String("save-all".into()))
        );
        assert_eq!(
            get_dotted(&root, "model"),
            Some(TomlValue::String("gpt-5".into()))
        );
        let mut keys = Vec::new();
        flatten_keys("", &root, &mut keys);
        keys.sort();
        assert_eq!(
            keys,
            vec!["history.persistence".to_owned(), "model".to_owned()]
        );
    }

    #[test]
    fn config_file_loaders_parse_documents_without_process_state() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.toml");
        for document in [
            "",
            "theme = \"dark\"\n",
            "[history]\npersistence = \"save-all\"\n",
        ] {
            fs::write(&path, document).unwrap();
            let expected: TomlValue = toml::from_str(document).unwrap();
            assert_eq!(load_toml(&path), Some(expected.clone()));
            assert_eq!(load_toml_for_update(&path).unwrap(), expected);
        }
        let invalid = "secret_fixture = \"do-not-echo\"\n[broken\n";
        fs::write(&path, invalid).unwrap();
        assert!(load_toml(&path).is_none());
        let error = load_toml_for_update(&path).unwrap_err();
        assert!(!format!("{error:#}").contains("do-not-echo"));
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
    }

    #[test]
    fn config_updates_preserve_invalid_or_unreadable_settings() {
        if crate::config::test_reexec_for_process_isolation() {
            return;
        }
        let temp = TempDir::new().unwrap();
        env::set_var("MAESTRO_HOME", temp.path());
        let path = temp.path().join("config.toml");
        let original = "theme = \"dark\"\nsecret_fixture = \"do-not-echo\"\n[broken\n";
        for update in 0..3 {
            fs::write(&path, original).unwrap();
            let error = match update {
                0 => persist_user_model_default("gpt-5.6").map(|_| ()),
                1 => persist_user_model_and_provider("gpt-5.6", "openai").map(|_| ()),
                _ => run_set(&["model".into(), "gpt-5.6".into()]).map(|_| ()),
            }
            .unwrap_err();
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
            assert!(!format!("{error:#}").contains("do-not-echo"));
        }
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(persist_user_model_default("gpt-5.6").is_err());
        assert!(path.is_dir());
        fs::remove_dir(&path).unwrap();
        persist_user_model_default("gpt-5.6").unwrap();
        assert_eq!(load_toml(&path).unwrap()["model"].as_str(), Some("gpt-5.6"));
    }

    #[test]
    fn persist_user_model_default_round_trips_and_preserves_keys() {
        let _guard = env_lock();
        let temp = TempDir::new().unwrap();
        let previous = env::var("MAESTRO_HOME").ok();
        env::set_var("MAESTRO_HOME", temp.path());

        fs::write(temp.path().join("config.toml"), "theme = \"dark\"\n").unwrap();
        let path = persist_user_model_default("gpt-5.5").unwrap();
        assert_eq!(path, temp.path().join("config.toml"));

        let root = load_toml(&path).unwrap();
        assert_eq!(
            get_dotted(&root, "model"),
            Some(TomlValue::String("gpt-5.5".into()))
        );
        assert_eq!(
            get_dotted(&root, "theme"),
            Some(TomlValue::String("dark".into())),
            "existing keys must survive a default-model update"
        );

        persist_user_model_default("claude-sonnet-4-6").unwrap();
        let root = load_toml(&path).unwrap();
        assert_eq!(
            get_dotted(&root, "model"),
            Some(TomlValue::String("claude-sonnet-4-6".into()))
        );

        restore_env("MAESTRO_HOME", previous);
    }

    #[test]
    fn persist_user_model_and_provider_writes_both_keys() {
        let _guard = env_lock();
        let temp = TempDir::new().unwrap();
        let previous = env::var("MAESTRO_HOME").ok();
        env::set_var("MAESTRO_HOME", temp.path());
        persist_user_model_and_provider("openai/o4-mini", "openrouter").unwrap();
        let root = load_toml(&temp.path().join("config.toml")).unwrap();
        assert_eq!(
            get_dotted(&root, "model"),
            Some(TomlValue::String("openai/o4-mini".into()))
        );
        assert_eq!(
            get_dotted(&root, "model_provider"),
            Some(TomlValue::String("openrouter".into()))
        );
        restore_env("MAESTRO_HOME", previous);
    }

    #[test]
    fn effective_settings_summary_uses_maestro_home_not_builtin_defaults() {
        if crate::config::test_reexec_for_cwd_isolation() {
            return;
        }
        let _guard = env_lock();
        let maestro_home = TempDir::new().unwrap();
        let user_home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let previous_maestro_home = env::var("MAESTRO_HOME").ok();
        let previous_home = env::var("HOME").ok();
        let previous_cwd = env::current_dir().ok();
        env::set_var("MAESTRO_HOME", maestro_home.path());
        env::set_var("HOME", user_home.path());
        env::set_current_dir(workspace.path()).unwrap();
        fs::write(
            maestro_home.path().join("config.toml"),
            "model = \"openai/o4-mini\"\nmodel_provider = \"openrouter\"\n",
        )
        .unwrap();
        crate::config::clear_config_cache();
        let settings = effective_settings_summary();
        crate::config::clear_config_cache();
        restore_env("MAESTRO_HOME", previous_maestro_home);
        restore_env("HOME", previous_home);
        if let Some(cwd) = previous_cwd {
            let _ = env::set_current_dir(cwd);
        }
        assert_eq!(settings["model"], "openai/o4-mini");
        assert_eq!(settings["model_provider"], "openrouter");
    }

    #[test]
    fn config_get_set_list_against_temp_home() {
        let _guard = env_lock();
        let temp = TempDir::new().unwrap();
        let previous = env::var("MAESTRO_HOME").ok();
        env::set_var("MAESTRO_HOME", temp.path());

        let set_code = run_set(&[
            "model".into(),
            "claude-sonnet-4-5".into(),
            "--scope".into(),
            "user".into(),
        ])
        .unwrap();
        assert_eq!(set_code, 0);

        let get_code = run_get(&["model".into(), "--scope".into(), "user".into()]).unwrap();
        assert_eq!(get_code, 0);

        let list_code = run_list(&["--scope".into(), "user".into(), "--json".into()]).unwrap();
        assert_eq!(list_code, 0);

        let content = fs::read_to_string(temp.path().join("config.toml")).unwrap();
        assert!(content.contains("claude-sonnet-4-5"));

        restore_env("MAESTRO_HOME", previous);
    }

    #[test]
    fn validate_detects_missing_file_reference() {
        let _guard = env_lock();
        let temp = TempDir::new().unwrap();
        let previous = env::var("MAESTRO_HOME").ok();
        env::set_var("MAESTRO_HOME", temp.path());

        let config = json!({
            "providers": [{
                "id": "local",
                "name": "Local",
                "api": "openai-responses",
                "models": [{
                    "id": "local/model",
                    "name": "{file:./missing.md}",
                    "contextWindow": 1000,
                    "maxTokens": 100
                }]
            }]
        });
        fs::write(
            temp.path().join("config.json"),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();

        let result = validate_config().unwrap();
        assert_eq!(result["valid"], false);
        let errors = result["errors"].as_array().unwrap();
        assert!(errors.iter().any(|error| {
            error
                .as_str()
                .is_some_and(|message| message.contains("File reference not found"))
        }));

        restore_env("MAESTRO_HOME", previous);
    }

    #[test]
    fn init_preset_writes_project_config() {
        if crate::config::test_reexec_for_cwd_isolation() {
            return;
        }
        let _guard = env_lock();
        let temp = TempDir::new().unwrap();
        let previous_cwd = env::current_dir().unwrap();
        let previous_home = env::var("MAESTRO_HOME").ok();
        env::set_var("MAESTRO_HOME", temp.path().join("home"));
        env::set_current_dir(temp.path()).unwrap();

        // Non-interactive: force overwrite defaults and skip prompts via non-TTY defaults.
        let code = run_init(&["--preset".into(), "openai".into(), "--force".into()]).unwrap();
        assert_eq!(code, 0);
        let written = fs::read_to_string(temp.path().join(".maestro/config.json")).unwrap();
        assert!(written.contains("openai"));
        assert!(written.contains("gpt-4o-mini"));

        env::set_current_dir(previous_cwd).unwrap();
        restore_env("MAESTRO_HOME", previous_home);
    }

    #[test]
    fn strip_json_comments_allows_trailing_comments() {
        let raw = r#"{
            // comment
            "providers": []
        }"#;
        let value = parse_jsonish(raw).unwrap();
        assert!(value.get("providers").is_some());
    }

    #[tokio::test]
    async fn local_check_returns_success_exit() {
        let code = run_local(&["--check".into()]).await.unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn local_endpoint_errors_redact_override_url_secrets() {
        let output = check_local_endpoint(
            "llama.cpp",
            "http://user:password@127.0.0.1:9/v1?token=query-secret#fragment",
        )
        .await;

        assert!(output.starts_with("[llama.cpp] error ·"));
        assert!(output.contains("http://127.0.0.1:9/v1/models"));
        for secret in ["user", "password", "query-secret", "fragment"] {
            assert!(!output.contains(secret), "leaked {secret}: {output}");
        }
    }

    #[test]
    fn llamacpp_local_template_targets_qwen38() {
        let template = local_provider_template("llamacpp").unwrap();

        assert_eq!(template.id, "llamacpp");
        assert_eq!(template.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(template.api, "openai-completions");
        assert_eq!(template.model_id, "llamacpp/Qwen3.8-27B");
        assert_eq!(template.context_window, 262_144);
    }

    #[test]
    fn local_metadata_url_preserves_openai_prefix() {
        assert_eq!(
            local_metadata_url("http://127.0.0.1:8080/v1").unwrap(),
            "http://127.0.0.1:8080/v1/models"
        );
    }

    #[test]
    fn llamacpp_setup_uses_the_registry_base_url_override() {
        let env = BTreeMap::from([(
            "LLAMA_CPP_BASE_URL".to_owned(),
            "http://127.0.0.1:9090/v1".to_owned(),
        )]);

        assert_eq!(
            llamacpp_base_url(&env, "http://127.0.0.1:8080/v1"),
            "http://127.0.0.1:9090/v1"
        );
    }

    #[test]
    fn local_endpoint_checks_use_all_registry_overrides() {
        let env = BTreeMap::from([
            (
                "LLAMA_CPP_BASE_URL".to_owned(),
                "http://127.0.0.1:9090/v1".to_owned(),
            ),
            (
                "LM_STUDIO_BASE_URL".to_owned(),
                "http://127.0.0.1:9123/v1".to_owned(),
            ),
            (
                "OLLAMA_BASE_URL".to_owned(),
                "http://127.0.0.1:9114/v1".to_owned(),
            ),
        ]);
        let checks = local_endpoint_checks(&env);

        assert_eq!(
            checks,
            vec![
                ("llama.cpp", "http://127.0.0.1:9090/v1".to_owned()),
                ("LM Studio", "http://127.0.0.1:9123/v1".to_owned()),
                ("Ollama", "http://127.0.0.1:9114/v1".to_owned()),
            ]
        );
    }

    #[test]
    fn config_generation_excludes_secret_derived_masked_values() {
        let first = json!({
            "schemaVersion": "evalops.maestro.config-inspection.v2",
            "envVars": [{"name": "MODEL_API_KEY", "set": true, "maskedValue": "ab…yz"}],
            "settings": {"model": "openai/gpt-5.5"}
        });
        let second = json!({
            "schemaVersion": "evalops.maestro.config-inspection.v2",
            "envVars": [{"name": "MODEL_API_KEY", "set": true, "maskedValue": "cd…wx"}],
            "settings": {"model": "openai/gpt-5.5"}
        });

        assert_eq!(
            config_generation_sha256(&first),
            config_generation_sha256(&second)
        );
    }

    #[test]
    fn config_generation_changes_with_effective_settings() {
        let first = json!({"envVars": [], "settings": {"model": "model-a"}});
        let second = json!({"envVars": [], "settings": {"model": "model-b"}});

        assert_ne!(
            config_generation_sha256(&first),
            config_generation_sha256(&second)
        );
    }
}
