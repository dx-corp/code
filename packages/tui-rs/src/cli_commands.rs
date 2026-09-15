//! Lightweight non-agent CLI helpers (sessions, cost, modes, status, hooks).
//!
//! These replace TypeScript `maestro cost|sessions|modes|status|hooks` entrypoints
//! so the Node agent bootstrap is not required.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::session::{ExportFormat, ExportOptions, SessionManager};
use crate::session_transfer::{
    PortableFormat, SecureSessionExportOptions, SecureSessionImportOptions,
    export_portable_session, export_secure_portable_session, import_portable_session_with_options,
};

const ANTHROPIC_OAUTH_REMOVED_MESSAGE: &str = "Anthropic OAuth login has been removed. Set ANTHROPIC_API_KEY to use Anthropic models, or run `deixic-code codex login` for the default Codex flow.";

/// Dispatch a top-level CLI helper subcommand.
///
/// `args` is the token stream after the program name, e.g.
/// `["sessions", "list"]` or `["cost", "today"]`.
pub async fn run_cli_command(args: &[String]) -> Result<i32> {
    let Some(cmd) = args.first().map(String::as_str) else {
        bail!(
            "{}",
            crate::localization::cli_locale().format("missing command", &[])
        );
    };

    match cmd {
        "sessions" => run_sessions(&args[1..]),
        "search" => crate::search_cli::run_search(&args[1..]),
        "cost" => run_cost(&args[1..]),
        "stats" => run_stats(&args[1..]),
        "models" => run_models(&args[1..]),
        "diagnostics" => maestro_local_host::diagnostics::run_cli(&args[1..]),
        "doctor" => crate::doctor::run_doctor(&args[1..]).await,
        "setup" => crate::setup_cli::run_setup(&args[1..]).await,
        "status" if args.get(1).is_some_and(|arg| is_help(arg)) => {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("Usage: {0}", &["deixic-code status".to_string()])
            );
            Ok(0)
        }
        "status" => run_status(),
        "hooks" => run_hooks(&args[1..]),
        "export" if args.get(1).is_some_and(|arg| is_help(arg)) => {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Usage: {0}",
                    &["deixic-code export <session-id> [output-path] [--format f]".to_string()]
                )
            );
            Ok(0)
        }
        "export" => {
            // `maestro export <id> [path] [--format json|md|html|txt|jsonl]`
            run_sessions_export(&args[1..])
        }
        "import" if args.get(1).is_some_and(|arg| is_help(arg)) => {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Usage: {0}",
                    &["deixic-code import <file.jsonl|file.json>".to_string()]
                )
            );
            Ok(0)
        }
        "import" => run_sessions_import(&args[1..]),
        "import-claude" if args.get(1).is_some_and(|arg| is_help(arg)) => {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Usage: {0}",
                    &["deixic-code import-claude [--dry-run]".to_string()]
                )
            );
            Ok(0)
        }
        "import-claude" => crate::import_claude_cli::run_import_claude(&args[1..]),
        "skill" => crate::skill_cli::run_skill(&args[1..]).await,
        "update" => crate::update_cli::run_update(&args[1..]).await,
        "modes" => crate::mode_cli::run_modes(&args[1..]).await,
        "memory" => crate::memory_cli::run_memory(&args[1..]).await,
        "mission" => crate::mission_cli::run_mission(&args[1..]).await,
        "init" => crate::init_cli::run_init(&args[1..]).await,
        "login" if args.get(1).is_some_and(|arg| is_help(arg)) => {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("Usage: {0}", &["maestro login".to_string()])
            );
            Ok(0)
        }
        "login" => crate::evalops_cli::run_evalops(&["login".to_owned()]).await,
        "evalops" => crate::evalops_cli::run_evalops(&args[1..]).await,
        "openai" => crate::openai_cli::run_openai(&args[1..]).await,
        "computer" | "orb" => crate::orb_cli::run_orb(&args[1..]).await,
        "config" => crate::config_cli::run_config(&args[1..]).await,
        "operating-plane" => crate::operating_plane_cli::run_operating_plane(&args[1..]).await,
        "painter" => crate::painter_cli::run_painter(&args[1..]),
        "anthropic" => {
            eprintln!("{ANTHROPIC_OAUTH_REMOVED_MESSAGE}");
            Ok(1)
        }
        "remote" => crate::remote_cli::run_remote(&args[1..]).await,
        "value" => crate::value_cli::run_value(&args[1..]).await,
        "scenario" => crate::scenario_cli::run_scenario(&args[1..]).await,
        "codex" => crate::codex_cli::run_codex(&args[1..]).await,
        "context" => crate::context_cli::run_context(&args[1..]).await,
        "run" => crate::run_cli::run_run(&args[1..]).await,
        "workflow" => crate::workflow_cli::run_workflow(&args[1..]).await,
        "a2a" => crate::a2a_cli::run_a2a(&args[1..]).await,
        "acp" => crate::acp_cli::run_acp(&args[1..]).await,
        "mcp" => crate::mcp_config_cli::run_mcp_config(&args[1..]).await,
        "plugins" | "plugin" => crate::plugins_cli::run_plugins(&args[1..]),
        "connections" => crate::connections_cli::run_connections(&args[1..]),
        other => bail!(
            "{}",
            crate::localization::cli_locale().format("unknown command: {0}", &[other.to_string()])
        ),
    }
}

fn is_help(arg: &str) -> bool {
    matches!(arg, "help" | "--help" | "-h")
}

fn run_sessions(args: &[String]) -> Result<i32> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => run_sessions_list(&args[1..]),
        "path" => run_sessions_path(),
        "export" => run_sessions_export(&args[1..]),
        "import" => run_sessions_import(&args[1..]),
        "help" | "--help" | "-h" => {
            println!("{}", crate::localization::cli_locale().format("Usage: {0}", &["deixic-code sessions [list [N]|path|export <id> [out] [--format f]|import <file> [secure key flags]]".to_string()]));
            Ok(0)
        }
        other => {
            eprintln!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unknown sessions subcommand: {0}", &[other.to_string()])
            );
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Try: {0}",
                    &["deixic-code sessions list|export|import|path".to_string()]
                )
            );
            Ok(1)
        }
    }
}

fn run_sessions_list(args: &[String]) -> Result<i32> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let manager = SessionManager::new(cwd.to_string_lossy().to_string());
    let limit = args
        .first()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(20);
    let sessions = manager
        .recent_sessions(limit)
        .context("Failed to list sessions")?;
    if sessions.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("No sessions found for {0}", &[format!("{}", cwd.display())])
        );
        return Ok(0);
    }
    println!(
        "{:<12}  {:<20}  {:<24}  {}",
        "ID",
        crate::localization::cli_locale().translate("Model"),
        crate::localization::cli_locale().translate("Modified"),
        crate::localization::cli_locale().translate("Title")
    );
    for s in sessions {
        let id_short = if s.id.len() > 10 {
            format!("{}…", &s.id[..8])
        } else {
            s.id.clone()
        };
        let modified = s
            .modified
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| {
                let secs = d.as_secs();
                if secs < 3600 {
                    format!("{}m ago", secs / 60)
                } else if secs < 86400 {
                    format!("{}h ago", secs / 3600)
                } else {
                    format!("{}d ago", secs / 86400)
                }
            })
            .unwrap_or_else(|| s.timestamp.clone());
        let title = s.title();
        let title = if title.len() > 48 {
            format!("{}…", &title[..45])
        } else {
            title
        };
        println!(
            "{id_short:<12}  {:<20}  {modified:<24}  {title}",
            truncate(&s.model, 20)
        );
    }
    Ok(0)
}

fn run_sessions_path() -> Result<i32> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let manager = SessionManager::new(cwd.to_string_lossy().to_string());
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("Working directory: {0}", &[cwd.display().to_string()])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "sessions dir: {0}",
            &[format!("{}", manager.sessions_dir().display())]
        )
    );
    if let Ok(sessions) = manager.recent_sessions(1) {
        if let Some(s) = sessions.first() {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("Latest: {0}", &[s.path.display().to_string()])
            );
        }
    }
    Ok(0)
}

fn run_sessions_export(args: &[String]) -> Result<i32> {
    let mut session_id: Option<String> = None;
    let mut output: Option<PathBuf> = None;
    let mut format = ExportFormat::Json;
    let mut format_name = "json".to_string();
    let mut redact_secrets = false;
    let mut encryption_key_file: Option<PathBuf> = None;
    let mut signing_key_file: Option<PathBuf> = None;
    let mut recipient_key_id: Option<String> = None;
    let mut signing_key_id: Option<String> = None;
    let mut i = 0usize;
    while i < args.len() {
        let a = &args[i];
        if a == "--format" || a == "-f" {
            i += 1;
            let Some(f) = args.get(i) else {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "--format requires a value (json|secure-json|md|html|txt|jsonl|markdown)",
                        &[]
                    )
                );
            };
            if f.eq_ignore_ascii_case("secure-json") {
                format = ExportFormat::Json;
            } else {
                format = parse_export_format(f)?;
            }
            format_name = f.to_ascii_lowercase();
        } else if let Some(rest) = a.strip_prefix("--format=") {
            if rest.eq_ignore_ascii_case("secure-json") {
                format = ExportFormat::Json;
            } else {
                format = parse_export_format(rest)?;
            }
            format_name = rest.to_ascii_lowercase();
        } else if a == "--redact-secrets" {
            redact_secrets = true;
        } else if a == "--encryption-key-file" || a.starts_with("--encryption-key-file=") {
            encryption_key_file = Some(PathBuf::from(take_arg_value(
                args,
                &mut i,
                a,
                "--encryption-key-file",
            )?));
        } else if a == "--signing-key-file" || a.starts_with("--signing-key-file=") {
            signing_key_file = Some(PathBuf::from(take_arg_value(
                args,
                &mut i,
                a,
                "--signing-key-file",
            )?));
        } else if a == "--recipient-key-id" || a.starts_with("--recipient-key-id=") {
            recipient_key_id = Some(take_arg_value(args, &mut i, a, "--recipient-key-id")?);
        } else if a == "--signing-key-id" || a.starts_with("--signing-key-id=") {
            signing_key_id = Some(take_arg_value(args, &mut i, a, "--signing-key-id")?);
        } else if a.starts_with('-') {
            bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("unknown export flag: {0}", std::slice::from_ref(a))
            );
        } else if session_id.is_none() {
            session_id = Some(a.clone());
        } else if output.is_none() {
            output = Some(PathBuf::from(a));
        }
        i += 1;
    }

    let Some(id) = session_id else {
        eprintln!("{}", crate::localization::cli_locale().format("Usage: {0}", &["deixic-code sessions export <session-id> [output-path] [--format json|secure-json|md|html|txt|jsonl]".to_string()]));
        return Ok(2);
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let manager = SessionManager::new(cwd.to_string_lossy().to_string());
    if format_name == "secure-json" {
        let options = SecureSessionExportOptions {
            encryption_key_file: encryption_key_file
                .context("secure export requires --encryption-key-file")?,
            signing_key_file: signing_key_file
                .context("secure export requires --signing-key-file")?,
            recipient_key_id: recipient_key_id
                .context("secure export requires --recipient-key-id")?,
            signing_key_id: signing_key_id.context("secure export requires --signing-key-id")?,
        };
        let out_path = export_secure_portable_session(&manager, &id, output.as_deref(), &options)?;
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Exported redacted session {0} to {1} (secure-json).",
                &[id.clone(), format!("{}", out_path.display())]
            )
        );
        return Ok(0);
    }
    if encryption_key_file.is_some()
        || signing_key_file.is_some()
        || recipient_key_id.is_some()
        || signing_key_id.is_some()
    {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("secure session key flags require --format secure-json", &[])
        );
    }
    if matches!(format_name.as_str(), "json" | "jsonl") {
        let portable_format = if format_name == "jsonl" {
            PortableFormat::Jsonl
        } else {
            PortableFormat::Json
        };
        let out_path = export_portable_session(
            &manager,
            &id,
            output.as_deref(),
            portable_format,
            redact_secrets,
        )?;
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Exported session {0} to {1} ({2}).",
                &[
                    id.clone(),
                    format!("{}", out_path.display()),
                    format_name.clone()
                ]
            )
        );
        return Ok(0);
    }
    if redact_secrets {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "--redact-secrets is supported for json and jsonl exports",
                &[]
            )
        );
    }
    let session = manager
        .load_session(&id)
        .with_context(|| format!("Session not found: {id}"))?;

    let out_path = output.unwrap_or_else(|| {
        let ext = if matches!(format, ExportFormat::Json) {
            "json".to_string()
        } else {
            format.extension().to_string()
        };
        PathBuf::from(format!("session-{}.{}", &id[..id.len().min(8)], ext))
    });

    let options = ExportOptions {
        format,
        ..ExportOptions::default()
    };
    let content = {
        let exporter = crate::session::SessionExporter::from_session(&session, options);
        exporter.export_to_string()
    };

    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&out_path, content)
        .with_context(|| format!("write export to {}", out_path.display()))?;
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Exported session {0} to {1} ({2}).",
            &[
                session.header.id.clone(),
                format!("{}", out_path.display()),
                format.extension().to_string()
            ]
        )
    );
    Ok(0)
}

fn parse_export_format(s: &str) -> Result<ExportFormat> {
    match s.to_ascii_lowercase().as_str() {
        "json" => Ok(ExportFormat::Json),
        "md" | "markdown" => Ok(ExportFormat::Markdown),
        "html" | "htm" => Ok(ExportFormat::Html),
        "txt" | "text" | "plain" => Ok(ExportFormat::PlainText),
        "jsonl" => Ok(ExportFormat::Json), // handled specially above for raw copy
        other => {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "unsupported export format: {0} (use json|secure-json|md|html|txt|jsonl)",
                    &[other.to_string()]
                )
            )
        }
    }
}

fn run_sessions_import(args: &[String]) -> Result<i32> {
    let mut source: Option<PathBuf> = None;
    let mut encryption_key_file: Option<PathBuf> = None;
    let mut verify_key_file: Option<PathBuf> = None;
    let mut expected_recipient_key_id: Option<String> = None;
    let mut i = 0usize;
    while i < args.len() {
        let a = &args[i];
        if a == "--encryption-key-file" || a.starts_with("--encryption-key-file=") {
            encryption_key_file = Some(PathBuf::from(take_arg_value(
                args,
                &mut i,
                a,
                "--encryption-key-file",
            )?));
        } else if a == "--verify-key-file" || a.starts_with("--verify-key-file=") {
            verify_key_file = Some(PathBuf::from(take_arg_value(
                args,
                &mut i,
                a,
                "--verify-key-file",
            )?));
        } else if a == "--recipient-key-id" || a.starts_with("--recipient-key-id=") {
            expected_recipient_key_id =
                Some(take_arg_value(args, &mut i, a, "--recipient-key-id")?);
        } else if a.starts_with('-') {
            bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("unknown import flag: {0}", std::slice::from_ref(a))
            );
        } else if source.is_none() {
            source = Some(PathBuf::from(a));
        } else {
            bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("unexpected import argument: {0}", std::slice::from_ref(a))
            );
        }
        i += 1;
    }
    let Some(source) = source else {
        eprintln!("{}", crate::localization::cli_locale().format("Usage: {0}", &["deixic-code sessions import <file.jsonl|file.json> [--encryption-key-file path --verify-key-file path [--recipient-key-id id]]".to_string()]));
        return Ok(2);
    };
    let secure_options = match (encryption_key_file, verify_key_file) {
        (None, None) => None,
        (Some(encryption_key_file), Some(verify_key_file)) => Some(SecureSessionImportOptions {
            encryption_key_file,
            verify_key_file,
            expected_recipient_key_id,
        }),
        _ => bail!(
            "{}",
            crate::localization::cli_locale()
                .format("secure session import requires both key files", &[])
        ),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let manager = SessionManager::new(cwd.to_string_lossy().to_string());
    let imported =
        import_portable_session_with_options(&manager, &source, secure_options.as_ref())?;
    if imported.imported_count > 1 {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Imported {0} sessions from {1}. Active session: {2}.",
                &[
                    format!("{}", imported.imported_count),
                    format!("{}", source.display()),
                    imported.session_id.clone()
                ]
            )
        );
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Imported session {0} from {1}.",
                &[imported.session_id.clone(), format!("{}", source.display())]
            )
        );
    }
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Stored at {0}",
            &[format!("{}", imported.session_file.display())]
        )
    );
    Ok(0)
}

fn take_arg_value(args: &[String], index: &mut usize, arg: &str, name: &str) -> Result<String> {
    if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
        if value.is_empty() {
            bail!("{name} requires a value");
        }
        return Ok(value.to_string());
    }
    *index += 1;
    let value = args
        .get(*index)
        .cloned()
        .with_context(|| format!("{name} requires a value"))?;
    if value.is_empty() {
        bail!("{name} requires a value");
    }
    Ok(value)
}

fn run_cost(args: &[String]) -> Result<i32> {
    let period = args.first().map(String::as_str).unwrap_or("today");
    match period {
        "clear" => run_cost_clear(args),
        "breakdown" => run_cost_breakdown(),
        "help" | "--help" | "-h" => {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Usage: {0}",
                    &["deixic-code cost [today|week|month|all|breakdown|clear]".to_string()]
                )
            );
            Ok(0)
        }
        "today" | "yesterday" | "week" | "7d" | "month" | "30d" | "all" | "total" => {
            run_cost_summary(period)
        }
        other => {
            eprintln!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unknown cost subcommand: {0}", &[other.to_string()])
            );
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Try: {0}",
                    &[
                        "deixic-code cost today|yesterday|week|month|all|breakdown|clear"
                            .to_string()
                    ]
                )
            );
            Ok(1)
        }
    }
}

fn run_cost_summary(period: &str) -> Result<i32> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let manager = SessionManager::new(cwd.to_string_lossy().to_string());
    let sessions = manager
        .recent_sessions(200)
        .context("Failed to load sessions for cost summary")?;

    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut total_cost = 0.0f64;
    let mut session_count = 0usize;

    for s in &sessions {
        session_count += 1;
        input_tokens = input_tokens.saturating_add(s.stats.total_input_tokens);
        output_tokens = output_tokens.saturating_add(s.stats.total_output_tokens);
        total_cost += s.stats.total_cost;
    }

    let label = match period {
        "today" => "Recent sessions (local, last 200)",
        "yesterday" => "Recent sessions (local, last 200)",
        "week" | "7d" => "Recent sessions (local, last 200)",
        "month" | "30d" => "Recent sessions (local, last 200)",
        "all" | "total" => "Recent sessions (local, last 200)",
        other => other,
    };

    println!(
        "{}",
        crate::localization::cli_locale().format("Deixic Code cost — {0}", &[label.to_string()])
    );
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("  Sessions scanned: {0}", &[format!("{}", session_count)])
    );
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("  Input tokens:     {0}", &[format!("{}", input_tokens)])
    );
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("  Output tokens:    {0}", &[format!("{}", output_tokens)])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "  Total tokens:     {0}",
            &[format!("{}", input_tokens + output_tokens)]
        )
    );
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("  Estimated cost (USD): {0}", &[format!("{total_cost:.4}")])
    );
    println!();
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Note: native cost reads local session stats (not the legacy TS usage DB).",
            &[]
        )
    );
    Ok(0)
}

fn run_cost_breakdown() -> Result<i32> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let manager = SessionManager::new(cwd.to_string_lossy().to_string());
    let sessions = manager
        .recent_sessions(200)
        .context("Failed to load sessions for cost breakdown")?;

    let mut by_model: BTreeMap<String, (usize, u64, u64, f64)> = BTreeMap::new();
    for s in &sessions {
        let entry = by_model.entry(s.model.clone()).or_insert((0, 0, 0, 0.0));
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(s.stats.total_input_tokens);
        entry.2 = entry.2.saturating_add(s.stats.total_output_tokens);
        entry.3 += s.stats.total_cost;
    }

    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Deixic Code cost breakdown (by model, last 200 sessions)",
            &[]
        )
    );
    if by_model.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format("  No session usage found.", &[])
        );
        return Ok(0);
    }
    println!(
        "{:<32} {:>8} {:>12} {:>12} {:>12}",
        "MODEL", "SESSIONS", "INPUT", "OUTPUT", "COST"
    );
    for (model, (n, inn, out, cost)) in by_model {
        println!(
            "{:<32} {:>8} {:>12} {:>12} {:>12.4}",
            truncate(&model, 32),
            n,
            inn,
            out,
            cost
        );
    }
    Ok(0)
}

fn run_cost_clear(args: &[String]) -> Result<i32> {
    let force = args.iter().any(|a| a == "--yes" || a == "-y" || a == "yes");
    if !force {
        eprint!(
            "{}",
            crate::localization::cli_locale().format("Clear local usage.json files? [y/N] ", &[])
        );
        let _ = io::stderr().flush();
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let answer = line.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!(
                "{}",
                crate::localization::cli_locale().format("Aborted.", &[])
            );
            return Ok(0);
        }
    }

    let mut removed = 0usize;
    for path in usage_file_candidates() {
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("Removed {0}", &[format!("{}", path.display())])
            );
            removed += 1;
        }
    }
    if removed == 0 {
        println!(
            "{}",
            crate::localization::cli_locale().format("No usage files found to clear.", &[])
        );
    } else {
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("Cleared {0} usage file(s).", &[format!("{}", removed)])
        );
    }
    Ok(0)
}

fn usage_file_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(p) = crate::path_utils::env_path("MAESTRO_USAGE_FILE") {
        paths.push(p);
    }
    if let Some(home) = crate::path_utils::maestro_home_dir() {
        paths.push(home.join("usage.json"));
    }
    if let Some(legacy) = crate::path_utils::legacy_composer_home_dir() {
        paths.push(legacy.join("usage.json"));
    }
    crate::path_utils::dedupe_paths(paths)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageEntry {
    timestamp: i64,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default, alias = "tokensInput")]
    tokens_input: u64,
    #[serde(default, alias = "tokensOutput")]
    tokens_output: u64,
    #[serde(default, alias = "tokensCacheRead")]
    tokens_cache_read: Option<u64>,
    #[serde(default, alias = "tokensCacheWrite")]
    tokens_cache_write: Option<u64>,
    #[serde(default)]
    cost: f64,
}

fn load_usage_entries() -> Vec<UsageEntry> {
    for path in usage_file_candidates() {
        if !path.exists() {
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Vec<UsageEntry>>(&raw) {
                Ok(entries) => return entries,
                Err(err) => {
                    eprintln!(
                        "{}",
                        crate::localization::cli_locale().format(
                            "warning: failed to parse {0}: {1}",
                            &[format!("{}", path.display()), format!("{}", err)]
                        )
                    );
                }
            },
            Err(err) => {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "warning: failed to read {0}: {1}",
                        &[format!("{}", path.display()), format!("{}", err)]
                    )
                );
            }
        }
    }
    Vec::new()
}

fn period_range_ms(period: &str) -> (Option<i64>, &'static str) {
    let now = chrono::Utc::now().timestamp_millis();
    let day = 86_400_000i64;
    match period {
        "today" => {
            // Local midnight approximation via UTC midnight is good enough for CLI.
            let secs = now / 1000;
            let midnight = (secs - (secs % 86_400)) * 1000;
            (Some(midnight), "Today")
        }
        "yesterday" => {
            let secs = now / 1000;
            let midnight = (secs - (secs % 86_400)) * 1000;
            (Some(midnight - day), "Yesterday")
        }
        "week" | "7d" => (Some(now - 7 * day), "Last 7 Days"),
        "month" | "30d" => (Some(now - 30 * day), "Last 30 Days"),
        "all" | "total" => (None, "All Time"),
        _ => (Some(now - 7 * day), "Last 7 Days"),
    }
}

fn run_stats(args: &[String]) -> Result<i32> {
    let mut period = "week";
    let mut format = "text";
    let mut session_id: Option<&str> = None;
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--json" => format = "json",
            "--csv" => format = "csv",
            "--format" | "-f" => {
                i += 1;
                if let Some(f) = args.get(i) {
                    format = f.as_str();
                }
            }
            s if s.starts_with("--format=") => {
                format = s.trim_start_matches("--format=");
            }
            "--session" | "-s" => {
                i += 1;
                session_id = args.get(i).map(String::as_str);
            }
            s if s.starts_with("--session=") => {
                session_id = Some(s.trim_start_matches("--session="));
            }
            "help" | "--help" | "-h" => {
                println!("{}", crate::localization::cli_locale().format("Usage: {0}", &["deixic-code stats [today|yesterday|week|month|all] [--json|--csv] [--session <id>]".to_string()]));
                return Ok(0);
            }
            "today" | "yesterday" | "week" | "7d" | "month" | "30d" | "all" | "total" => {
                period = a;
            }
            _ => {}
        }
        i += 1;
    }

    let (since, label) = period_range_ms(period);
    let mut entries = load_usage_entries();
    if let Some(since) = since {
        entries.retain(|e| e.timestamp >= since);
        if period == "yesterday" {
            let end = since + 86_400_000;
            entries.retain(|e| e.timestamp < end);
        }
    }
    if let Some(sid) = session_id {
        entries.retain(|e| e.session_id.as_deref() == Some(sid));
    }

    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(0);
    }
    if format == "csv" {
        println!("timestamp,sessionId,provider,model,tokensInput,tokensOutput,cost");
        for e in &entries {
            println!(
                "{},{},{},{},{},{},{:.6}",
                e.timestamp,
                e.session_id.as_deref().unwrap_or(""),
                e.provider,
                e.model,
                e.tokens_input,
                e.tokens_output,
                e.cost
            );
        }
        return Ok(0);
    }

    let mut total_cost = 0.0f64;
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut by_provider: BTreeMap<String, (usize, u64, f64)> = BTreeMap::new();
    let mut by_model: BTreeMap<String, (usize, u64, f64)> = BTreeMap::new();
    for e in &entries {
        total_cost += e.cost;
        total_in = total_in.saturating_add(e.tokens_input);
        total_out = total_out.saturating_add(e.tokens_output);
        let p = by_provider.entry(e.provider.clone()).or_insert((0, 0, 0.0));
        p.0 += 1;
        p.1 = p.1.saturating_add(e.tokens_input + e.tokens_output);
        p.2 += e.cost;
        let m = by_model.entry(e.model.clone()).or_insert((0, 0, 0.0));
        m.0 += 1;
        m.1 = m.1.saturating_add(e.tokens_input + e.tokens_output);
        m.2 += e.cost;
    }

    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Deixic Code stats — {0} (native usage.json)",
            &[label.to_string()]
        )
    );
    if entries.is_empty() {
        // Fall back to session rollups when no usage DB.
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "  No usage.json entries; showing local session rollup instead.",
                &[]
            )
        );
        return run_cost_summary(if period == "week" { "all" } else { period });
    }
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("  Requests:      {0}", &[format!("{}", entries.len())])
    );
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("  Input tokens:  {0}", &[format!("{}", total_in)])
    );
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("  Output tokens: {0}", &[format!("{}", total_out)])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "  Total tokens:  {0}",
            &[format!("{}", total_in + total_out)]
        )
    );
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("  Estimated cost (USD): {0}", &[format!("{total_cost:.4}")])
    );
    if !by_provider.is_empty() {
        println!();
        println!(
            "{}",
            crate::localization::cli_locale().format("  By provider:", &[])
        );
        for (name, (n, tok, cost)) in by_provider {
            println!(
                "    {:<16} req={n:<5} tok={tok:<10} cost=${cost:.4}",
                truncate(&name, 16)
            );
        }
    }
    if !by_model.is_empty() {
        println!();
        println!(
            "{}",
            crate::localization::cli_locale().format("  By model:", &[])
        );
        for (name, (n, tok, cost)) in by_model {
            println!(
                "    {:<28} req={n:<5} tok={tok:<10} cost=${cost:.4}",
                truncate(&name, 28)
            );
        }
    }
    Ok(0)
}

fn run_models(args: &[String]) -> Result<i32> {
    if args.first().is_some_and(|arg| arg == "inspect") {
        let model = args
            .get(1)
            .filter(|arg| !arg.starts_with('-'))
            .context("Usage: deixic-code models inspect <model-id> [--json]")?;
        let inspection = crate::model_catalog::inspect_model(model)?;
        if args.iter().any(|arg| arg == "--json") {
            println!("{}", serde_json::to_string_pretty(&inspection)?);
        } else {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("Model: {0}", std::slice::from_ref(&inspection.id))
            );
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Provider: {0}",
                    std::slice::from_ref(&inspection.resolved.provider)
                )
            );
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Protocol: {0}",
                    std::slice::from_ref(&inspection.resolved.protocol)
                )
            );
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Authentication: {0} ({1})",
                    &[
                        (if inspection.resolved.auth_configured {
                            "configured"
                        } else {
                            "not configured"
                        })
                        .to_string(),
                        inspection.sources["auth"].clone()
                    ]
                )
            );
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Base URL: {0} ({1})",
                    &[
                        inspection
                            .resolved
                            .base_url
                            .as_deref()
                            .unwrap_or("(none)")
                            .to_string(),
                        inspection.sources["baseUrl"].clone()
                    ]
                )
            );
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("Catalog: {0}", &[inspection.sources["catalog"].clone()])
            );
        }
        return Ok(0);
    }
    let mut sub = "list";
    let mut provider_filter: Option<String> = None;
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "list" | "ls" => sub = "list",
            "providers" => sub = "providers",
            "help" | "--help" | "-h" => {
                println!(
                    "Usage: deixic-code models [list|providers] [--provider <name>]\n\
                     \x20      deixic-code models inspect <model-id> [--json]"
                );
                return Ok(0);
            }
            "--provider" | "-p" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    provider_filter = Some(v.clone());
                }
            }
            s if s.starts_with("--provider=") => {
                provider_filter = Some(s.trim_start_matches("--provider=").to_string());
            }
            other if !other.starts_with('-') && provider_filter.is_none() => {
                // bare provider name: `maestro models openai`
                provider_filter = Some(other.to_string());
            }
            _ => {}
        }
        i += 1;
    }

    let models = crate::components::available_models();
    let filtered: Vec<_> = models
        .into_iter()
        .filter(|m| {
            provider_filter.as_ref().is_none_or(|p| {
                m.provider.eq_ignore_ascii_case(p)
                    || m.id.to_lowercase().contains(&p.to_lowercase())
            })
        })
        .collect();

    if sub == "providers" {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for m in &filtered {
            *counts.entry(m.provider.clone()).or_default() += 1;
        }
        println!(
            "{}",
            crate::localization::cli_locale().format("Providers (native catalog)", &[])
        );
        if counts.is_empty() {
            println!(
                "  ({})",
                crate::localization::cli_locale().translate("none")
            );
            return Ok(0);
        }
        for (p, n) in counts {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("  {0} — models: {1}", &[format!("{p:<16}"), n.to_string()])
            );
        }
        return Ok(0);
    }

    println!(
        "{}",
        crate::localization::cli_locale().format("Registered models (native catalog)", &[])
    );
    if let Some(p) = &provider_filter {
        println!(
            "{}",
            crate::localization::cli_locale().format("  Filter: {0}", std::slice::from_ref(p))
        );
    }
    if filtered.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format("  No models matched.", &[])
        );
        return Ok(1);
    }
    let mut by_provider: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for m in filtered {
        by_provider.entry(m.provider.clone()).or_default().push(m);
    }
    for (provider, entries) in by_provider {
        println!();
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "  {0} — models: {1}",
                &[provider.clone(), entries.len().to_string()]
            )
        );
        for m in entries {
            println!("  • {:<28}  {}", m.id, m.description);
            println!(
                "    {} | {:?} | tools={} vision={} reasoning={} streaming={} context={} | verification={:?}",
                m.name,
                m.capabilities.protocol,
                m.capabilities.tools,
                m.capabilities.vision,
                m.capabilities.reasoning,
                m.capabilities.streaming,
                m.capabilities.context_tokens,
                m.verification.state,
            );
        }
    }
    Ok(0)
}

fn run_status() -> Result<i32> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let version = env!("CARGO_PKG_VERSION");
    println!(
        "{}",
        crate::localization::cli_locale().format("Deixic Code status (native)", &[])
    );
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("  Binary:     maestro {0}", &[version.to_string()])
    );
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("  Cwd:        {0}", &[format!("{}", cwd.display())])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "  Git:        {0}",
            &[crate::git::current_branch(&cwd)
                .unwrap_or_else(|| "(not a repo)".into())
                .clone()]
        )
    );
    if let Some(home) = crate::path_utils::maestro_home_dir() {
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("  Home:       {0}", &[format!("{}", home.display())])
        );
    }
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "  Model env:  {0}",
            &[std::env::var("MAESTRO_MODEL")
                .unwrap_or_else(|_| "(default)".into())
                .clone()]
        )
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "  Plan mode:  {0}",
            &[(if crate::safety::is_plan_mode() {
                "on"
            } else {
                "off"
            })
            .to_string()]
        )
    );
    Ok(0)
}

const HOOKS_IMPORT_USAGE: &str = "Usage: deixic-code hooks import --from-claude-code [path] [--out <file>]\n\nReads a Claude Code settings.json and writes the equivalent Deixic Code hook\nconfig. With no path, reads .claude/settings.json in the working directory,\nthen ~/.claude/settings.json.\n\nExits non-zero and lists every entry that has no Deixic Code equivalent. Nothing\nis written in that case, so a partial import is never mistaken for a\ncomplete one.";

/// Locate the Claude Code settings file to import.
fn claude_code_settings_path(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(PathBuf::from(path));
    }
    let local = std::env::current_dir()
        .ok()?
        .join(".claude")
        .join("settings.json");
    if local.is_file() {
        return Some(local);
    }
    let global = dirs::home_dir()?.join(".claude").join("settings.json");
    global.is_file().then_some(global)
}

fn run_hooks_import(args: &[String]) -> Result<i32> {
    if args.iter().any(|arg| is_help(arg)) {
        println!("{HOOKS_IMPORT_USAGE}");
        return Ok(0);
    }
    if !args.iter().any(|arg| arg == "--from-claude-code") {
        eprintln!("{HOOKS_IMPORT_USAGE}");
        return Ok(1);
    }

    let mut explicit_source = None;
    let mut out_path = None;
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        match argument {
            "--from-claude-code" => {}
            "--out" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!(
                        "{}",
                        crate::localization::cli_locale().format("--out requires a path", &[])
                    );
                    return Ok(1);
                };
                out_path = Some(PathBuf::from(value));
            }
            other if other.starts_with("--") => {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Unknown option for `deixic-code hooks import`: {0}",
                        &[other.to_string()]
                    )
                );
                return Ok(1);
            }
            other => explicit_source = Some(other.to_string()),
        }
        index += 1;
    }

    let Some(source) = claude_code_settings_path(explicit_source.as_deref()) else {
        eprintln!("{}", crate::localization::cli_locale().format("No Claude Code settings found. Looked for .claude/settings.json here and ~/.claude/settings.json.", &[]));
        return Ok(1);
    };
    let text = match std::fs::read_to_string(&source) {
        Ok(text) => text,
        Err(error) => {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Failed to read {0}: {1}",
                    &[format!("{}", source.display()), format!("{}", error)]
                )
            );
            return Ok(1);
        }
    };
    let outcome = match crate::hooks::import_claude_code_hooks(&text) {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Failed to parse {0}: {1}",
                    &[format!("{}", source.display()), format!("{}", error)]
                )
            );
            return Ok(1);
        }
    };

    if outcome.has_unmappable() {
        eprintln!(
            "{}",
            crate::localization::cli_locale().format(
                "{0} of {1} entries in {2} have no Deixic Code equivalent:",
                &[
                    format!("{}", outcome.unmappable.len()),
                    format!("{}", outcome.unmappable.len() + outcome.hooks.len()),
                    format!("{}", source.display())
                ]
            )
        );
        for entry in &outcome.unmappable {
            eprintln!("  - {entry}");
        }
        eprintln!(
            "{}",
            crate::localization::cli_locale().format(
                "Nothing was written. Remove or rewrite those entries and run the import again.",
                &[]
            )
        );
        return Ok(1);
    }

    let rendered = crate::hooks::render_maestro_hooks_json(&outcome);
    match out_path {
        Some(path) => {
            if let Err(error) = std::fs::write(&path, &rendered) {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Failed to write {0}: {1}",
                        &[format!("{}", path.display()), format!("{}", error)]
                    )
                );
                return Ok(1);
            }
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Imported {0} hook(s) from {1} into {2}",
                    &[
                        format!("{}", outcome.hooks.len()),
                        format!("{}", source.display()),
                        format!("{}", path.display())
                    ]
                )
            );
        }
        None => print!("{rendered}"),
    }
    Ok(0)
}

fn run_hooks(args: &[String]) -> Result<i32> {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    if sub == "import" {
        return run_hooks_import(&args[1..]);
    }
    if sub != "status" && sub != "list" {
        eprintln!(
            "{}",
            crate::localization::cli_locale()
                .format("Unknown hooks subcommand: {0}", &[sub.to_string()])
        );
        eprintln!(
            "{}",
            crate::localization::cli_locale()
                .format("Try: {0}", &["deixic-code hooks status".to_string()])
        );
        eprintln!("     deixic-code hooks import --from-claude-code [path]");
        return Ok(1);
    }

    println!(
        "{}",
        crate::localization::cli_locale().format("Hook status (native summary)", &[])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "  Runtime:    native TUI hooks (Lua/WASM/native + optional Node bridge)",
            &[]
        )
    );
    println!("{}", crate::localization::cli_locale().format("  Config:     ~/.maestro/hooks.toml and project hooks (see packages/tui-rs hooks docs)", &[]));
    if std::env::var("MAESTRO_HOOKS_DISABLED").ok().as_deref() == Some("1") {
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("  State:      disabled (MAESTRO_HOOKS_DISABLED=1)", &[])
        );
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format("  State:      enabled (default)", &[])
        );
    }
    println!();
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Inspect hooks from the interactive TUI (/hooks) for live concurrency stats.",
            &[]
        )
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Import a Claude Code config with `deixic-code hooks import --from-claude-code`.",
            &[]
        )
    );
    Ok(0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[tokio::test]
    async fn native_utility_help_exits_cleanly() {
        for args in [
            argv(&["status", "--help"]),
            argv(&["export", "--help"]),
            argv(&["import", "--help"]),
            argv(&["import-claude", "--help"]),
            argv(&["update", "--help"]),
            argv(&["plugins", "--help"]),
            argv(&["plugin", "--help"]),
        ] {
            assert_eq!(run_cli_command(&args).await.expect("help command"), 0);
        }
    }

    #[tokio::test]
    async fn unknown_cost_subcommand_is_rejected() {
        assert_eq!(
            run_cli_command(&argv(&["cost", "nonsense"]))
                .await
                .expect("cost command"),
            1
        );
    }

    #[tokio::test]
    async fn anthropic_command_reports_removed_oauth_flow() {
        assert!(ANTHROPIC_OAUTH_REMOVED_MESSAGE.contains("ANTHROPIC_API_KEY"));
        assert!(ANTHROPIC_OAUTH_REMOVED_MESSAGE.contains("deixic-code codex login"));
        assert_eq!(
            run_cli_command(&argv(&["anthropic", "status"]))
                .await
                .expect("anthropic command"),
            1
        );
    }

    #[test]
    fn secure_transfer_flags_accept_equals_and_next_token_values() {
        let args = argv(&["--recipient-key-id=recipient-v1"]);
        let mut index = 0;
        assert_eq!(
            take_arg_value(&args, &mut index, &args[0], "--recipient-key-id")
                .expect("equals value"),
            "recipient-v1"
        );

        let args = argv(&["--verify-key-file", "/tmp/verify-key"]);
        let mut index = 0;
        assert_eq!(
            take_arg_value(&args, &mut index, &args[0], "--verify-key-file")
                .expect("next-token value"),
            "/tmp/verify-key"
        );

        let args = argv(&["--encryption-key-file"]);
        let mut index = 0;
        assert!(take_arg_value(&args, &mut index, &args[0], "--encryption-key-file").is_err());
    }

    #[test]
    fn hooks_import_writes_mapped_config_and_refuses_unmappable_entries() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        fs::write(
            &settings,
            r#"{
              "hooks": {
                "PreToolUse": [
                  {"matcher": "Bash", "hooks": [{"type": "command", "command": "./guard.sh"}]},
                  {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "./fmt.sh"}]}
                ],
                "UserPromptSubmit": [
                  {"hooks": [{"type": "command", "command": "./context.sh"}]}
                ],
                "Stop": [
                  {"hooks": [{"type": "command", "command": "./done.sh"}]}
                ],
                "PostToolUse": [
                  {"matcher": "Nonsense", "hooks": [{"type": "command", "command": "./never.sh"}]}
                ]
              }
            }"#,
        )
        .unwrap();
        let out = dir.path().join("hooks.json");

        // Two entries cannot be mapped, so the command fails and writes
        // nothing rather than reporting a partial import as a success.
        assert_eq!(
            run_hooks(&argv(&[
                "import",
                "--from-claude-code",
                settings.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ]))
            .unwrap(),
            1
        );
        assert!(!out.exists(), "a refused import must not write output");

        // With the two unmappable entries removed, the remaining three map.
        fs::write(
            &settings,
            r#"{
              "hooks": {
                "PreToolUse": [
                  {"matcher": "Bash", "hooks": [{"type": "command", "command": "./guard.sh"}]},
                  {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "./fmt.sh"}]}
                ],
                "UserPromptSubmit": [
                  {"hooks": [{"type": "command", "command": "./context.sh"}]}
                ]
              }
            }"#,
        )
        .unwrap();
        assert_eq!(
            run_hooks(&argv(&[
                "import",
                "--from-claude-code",
                settings.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ]))
            .unwrap(),
            0
        );
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(written["version"], 1);
        assert_eq!(written["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(
            written["hooks"]["UserPromptSubmit"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn hooks_import_requires_the_source_flag() {
        assert_eq!(run_hooks(&argv(&["import"])).unwrap(), 1);
        assert_eq!(run_hooks(&argv(&["import", "--help"])).unwrap(), 0);
    }
}
