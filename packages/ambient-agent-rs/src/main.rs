//! Ambient Agent CLI
//!
//! Command-line interface for the always-on GitHub agent.

use ambient_agent::{
    daemon::{DaemonBuilder, DaemonCommand},
    ipc::{IpcClient, default_socket_path},
    types::*,
};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

/// CLI-specific config that gets converted to/from AmbientConfig
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CliConfig {
    #[serde(default)]
    repos: Vec<CliRepoConfig>,
    #[serde(default)]
    thresholds: CliThresholds,
    #[serde(default)]
    github_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CliRepoConfig {
    name: String,
    #[serde(default)]
    watchers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CliThresholds {
    #[serde(default = "default_auto_execute")]
    auto_execute: f64,
    #[serde(default = "default_ask_human")]
    ask_human: f64,
}

fn default_auto_execute() -> f64 {
    0.8
}
fn default_ask_human() -> f64 {
    0.5
}

impl From<CliConfig> for AmbientConfig {
    fn from(cli: CliConfig) -> Self {
        AmbientConfig {
            enabled: true,
            auto_triggers: vec![],
            thresholds: Thresholds {
                auto_execute: cli.thresholds.auto_execute,
                ask_human: cli.thresholds.ask_human,
                skip: 0.0,
            },
            limits: Limits::default(),
            capabilities: Capabilities::default(),
            schedule: ScheduleConfig::default(),
            notify: NotifyConfig::default(),
            learning: LearningConfig::default(),
        }
    }
}

#[derive(Parser)]
#[command(name = "ambient")]
#[command(about = "Always-on GitHub agent that watches repos and ships code", long_about = None)]
struct Cli {
    /// Config file path
    #[arg(short, long, default_value = "ambient.yaml")]
    config: PathBuf,

    /// Data directory for persistence
    #[arg(short, long)]
    data_dir: Option<PathBuf>,

    /// Log level
    #[arg(short, long, default_value = "info")]
    log_level: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the daemon
    Start {
        /// Run in foreground (don't daemonize)
        #[arg(short, long)]
        foreground: bool,
    },

    /// Stop the running daemon
    Stop,

    /// Show daemon status
    Status,

    /// Show statistics
    Stats,

    /// Flush in-memory learner state to disk
    Flush,

    /// Watch a repository
    Watch {
        /// Repository in owner/repo format
        repo: String,
    },

    /// Unwatch a repository
    Unwatch {
        /// Repository in owner/repo format
        repo: String,
    },

    /// List watched repositories
    List,

    /// Initialize configuration
    Init {
        /// Force overwrite existing config
        #[arg(short, long)]
        force: bool,
    },

    /// Validate configuration
    Validate,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();

    // Setup logging
    let log_level = match cli.log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    match cli.command {
        Commands::Start { foreground } => cmd_start(&cli.config, cli.data_dir, foreground).await,
        Commands::Stop => cmd_stop().await,
        Commands::Status => cmd_status().await,
        Commands::Stats => cmd_stats().await,
        Commands::Flush => cmd_flush().await,
        Commands::Watch { repo } => cmd_watch(&cli.config, &repo).await,
        Commands::Unwatch { repo } => cmd_unwatch(&cli.config, &repo).await,
        Commands::List => cmd_list(&cli.config).await,
        Commands::Init { force } => cmd_init(&cli.config, force).await,
        Commands::Validate => cmd_validate(&cli.config).await,
    }
}

async fn cmd_start(
    config_path: &PathBuf,
    data_dir: Option<PathBuf>,
    _foreground: bool,
) -> anyhow::Result<()> {
    info!("Starting Ambient Agent");

    let cli_config = load_config(config_path).await?;
    let config: AmbientConfig = cli_config.into();

    let data_dir = data_dir.unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ambient-agent")
    });

    let mut daemon = DaemonBuilder::new()
        .config(config)
        .data_dir(data_dir)
        .build()?;

    // Setup signal handlers
    let cmd_tx = daemon.get_command_sender();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("Received shutdown signal");
        let _ = cmd_tx.send(DaemonCommand::Shutdown).await;
    });

    daemon.run().await?;

    info!("Ambient Agent stopped");
    Ok(())
}

async fn cmd_stop() -> anyhow::Result<()> {
    let client = IpcClient::new(default_socket_path());

    if !client.is_daemon_running() {
        println!("Daemon is not running");
        return Ok(());
    }

    println!("Stopping daemon...");
    match client.stop().await {
        Ok(()) => {
            println!("Daemon stopped");
            Ok(())
        }
        Err(e) => {
            println!("Failed to stop daemon: {}", e);
            Err(e)
        }
    }
}

async fn cmd_status() -> anyhow::Result<()> {
    let client = IpcClient::new(default_socket_path());

    if !client.is_daemon_running() {
        println!("Daemon status: Not running");
        println!("  Run 'ambient start' to start the daemon.");
        return Ok(());
    }

    match client.status().await {
        Ok(status) => {
            println!("Daemon status: {}", status.status);
            println!("  Running: {}", status.running);
            println!("  PID: {}", status.pid);
            println!("  Uptime: {}s", status.uptime_secs);
            Ok(())
        }
        Err(e) => {
            println!("Failed to get status: {}", e);
            Err(e)
        }
    }
}

async fn cmd_stats() -> anyhow::Result<()> {
    let client = IpcClient::new(default_socket_path());

    if !client.is_daemon_running() {
        println!("Daemon is not running");
        return Ok(());
    }

    match client.stats().await {
        Ok(stats) => {
            let success_rate = if stats.tasks_executed > 0 {
                (stats.tasks_succeeded as f64 / stats.tasks_executed as f64) * 100.0
            } else {
                0.0
            };

            println!("Statistics:");
            println!("  Uptime: {}s", stats.uptime_secs);
            println!("  Events processed: {}", stats.events_processed);
            println!("  Tasks executed: {}", stats.tasks_executed);
            println!("    - Succeeded: {}", stats.tasks_succeeded);
            println!("    - Failed: {}", stats.tasks_failed);
            println!("  Success rate: {:.1}%", success_rate);
            println!("  PRs created: {}", stats.prs_created);
            println!("  Total cost: ${:.4}", stats.total_cost);
            Ok(())
        }
        Err(e) => {
            println!("Failed to get stats: {}", e);
            Err(e)
        }
    }
}

async fn cmd_flush() -> anyhow::Result<()> {
    let client = IpcClient::new(default_socket_path());

    if !client.is_daemon_running() {
        anyhow::bail!("Daemon is not running");
    }

    match client.flush_learner().await {
        Ok(learner_path) => {
            if learner_path.is_empty() {
                println!("Learner state flushed");
            } else {
                println!("Learner state flushed: {}", learner_path);
            }
            Ok(())
        }
        Err(e) => {
            println!("Failed to flush learner state: {}", e);
            Err(e)
        }
    }
}

async fn cmd_watch(config_path: &PathBuf, repo: &str) -> anyhow::Result<()> {
    let mut config = load_config(config_path).await?;

    // Check if already watching
    if config.repos.iter().any(|r| r.name == repo) {
        println!("Already watching: {}", repo);
        return Ok(());
    }

    config.repos.push(CliRepoConfig {
        name: repo.to_string(),
        watchers: vec!["issues".to_string(), "pull_requests".to_string()],
    });

    save_config(config_path, &config).await?;
    println!("Now watching: {}", repo);
    Ok(())
}

async fn cmd_unwatch(config_path: &PathBuf, repo: &str) -> anyhow::Result<()> {
    let mut config = load_config(config_path).await?;

    let original_len = config.repos.len();
    config.repos.retain(|r| r.name != repo);

    if config.repos.len() == original_len {
        println!("Not watching: {}", repo);
        return Ok(());
    }

    save_config(config_path, &config).await?;
    println!("Stopped watching: {}", repo);
    Ok(())
}

async fn cmd_list(config_path: &PathBuf) -> anyhow::Result<()> {
    let config = load_config(config_path).await?;

    if config.repos.is_empty() {
        println!("No repositories being watched");
        return Ok(());
    }

    println!("Watched repositories:");
    for repo in &config.repos {
        println!("  {} ({})", repo.name, repo.watchers.join(", "));
    }

    Ok(())
}

async fn cmd_init(config_path: &PathBuf, force: bool) -> anyhow::Result<()> {
    if config_path.exists() && !force {
        anyhow::bail!(
            "Config file already exists: {}. Use --force to overwrite.",
            config_path.display()
        );
    }

    let config = CliConfig::default();
    save_config(config_path, &config).await?;

    println!("Created config file: {}", config_path.display());
    println!();
    println!("Next steps:");
    println!(
        "  1. Edit {} to add your GitHub token",
        config_path.display()
    );
    println!("  2. Run 'ambient watch owner/repo' to add repositories");
    println!("  3. Run 'ambient start' to begin watching");

    Ok(())
}

async fn cmd_validate(config_path: &PathBuf) -> anyhow::Result<()> {
    let config = load_config(config_path).await?;

    let mut errors = vec![];
    let mut warnings = vec![];

    // Check required fields
    if config.github_token.is_none() {
        errors.push("github_token is not set");
    }

    if config.repos.is_empty() {
        warnings.push("No repositories configured");
    }

    // Check thresholds
    if config.thresholds.auto_execute < 0.0 || config.thresholds.auto_execute > 1.0 {
        errors.push("auto_execute must be between 0 and 1");
    }

    if config.thresholds.ask_human < 0.0 || config.thresholds.ask_human > 1.0 {
        errors.push("ask_human must be between 0 and 1");
    }

    // Report results
    if !errors.is_empty() {
        println!("Errors:");
        for e in &errors {
            println!("  - {}", e);
        }
    }

    if !warnings.is_empty() {
        println!("Warnings:");
        for w in &warnings {
            println!("  - {}", w);
        }
    }

    if errors.is_empty() && warnings.is_empty() {
        println!("Configuration is valid");
    }

    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("Configuration has {} error(s)", errors.len())
    }
}

async fn load_config(path: &PathBuf) -> anyhow::Result<CliConfig> {
    if !path.exists() {
        anyhow::bail!(
            "Config file not found: {}. Run 'ambient init' to create one.",
            path.display()
        );
    }

    let content = tokio::fs::read_to_string(path).await?;
    let config: CliConfig = serde_yaml::from_str(&content)?;
    Ok(config)
}

async fn save_config(path: &PathBuf, config: &CliConfig) -> anyhow::Result<()> {
    let yaml = serde_yaml::to_string(config)?;
    tokio::fs::write(path, yaml).await?;
    Ok(())
}
