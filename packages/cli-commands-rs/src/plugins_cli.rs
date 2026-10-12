//! Native `maestro plugins` / `maestro plugin` CLI.
//!
//! Surfaces plugin discovery, installation, marketplace, and capability
//! controls for operator use without entering the interactive TUI (`/plugins`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::path_utils::{legacy_composer_home_dir, maestro_home_dir};
use crate::plugins::{DiscoveredPlugin, PluginRegistry, search_roots_for_workspace};

#[derive(Debug, Default)]
struct PluginArgs {
    command: Option<String>,
    positionals: Vec<String>,
    json: bool,
    workspace: Option<PathBuf>,
    trust: bool,
    help: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginListEntry {
    name: String,
    origin: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    components: Vec<&'static str>,
    has_manifest: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginListReport {
    plugins: Vec<PluginListEntry>,
    count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginComponentPaths {
    #[serde(skip_serializing_if = "Option::is_none")]
    skills: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agents: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commands: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hooks: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    connections: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginInfoReport {
    name: String,
    origin: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    has_manifest: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_path: Option<String>,
    components: PluginComponentPaths,
}

/// Dispatch `maestro plugins|plugin <subcommand> ...`.
pub fn run_plugins(args: &[String]) -> Result<i32> {
    let parsed = parse_args(args)?;
    if parsed.help || parsed.command.as_deref() == Some("help") {
        print_help();
        return Ok(0);
    }

    let registry = discover_registry(parsed.workspace.as_deref())?;
    let command = parsed.command.as_deref().unwrap_or("list");

    match command {
        "list" | "ls" => run_list(&registry, parsed.json),
        "info" | "show" => {
            let name = parsed
                .positionals
                .first()
                .map(String::as_str)
                .filter(|value| !value.is_empty());
            let Some(name) = name else {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("Usage: maestro plugins info <plugin-name>", &[])
                );
                return Ok(1);
            };
            run_info(&registry, name, parsed.json)
        }
        "install" => {
            let source = parsed
                .positionals
                .first()
                .context("Usage: maestro plugins install <path-or-git-url> [--trust]")?;
            let home = maestro_home_dir().context("could not resolve ~/.maestro")?;
            let preview = crate::plugins::install(
                source,
                &home.join("plugins"),
                &home.join("plugin-state.json"),
                parsed.trust,
            )?;
            println!("{}", serde_json::to_string_pretty(&preview)?);
            Ok(0)
        }
        "remove" | "uninstall" => {
            let name = parsed
                .positionals
                .first()
                .context("Usage: maestro plugins remove <name>")?;
            let home = maestro_home_dir().context("could not resolve ~/.maestro")?;
            crate::plugins::remove(name, &home.join("plugins"), &home.join("plugin-state.json"))?;
            println!("Removed {name}");
            Ok(0)
        }
        "update" => {
            let home = maestro_home_dir().context("could not resolve ~/.maestro")?;
            let updates = crate::plugins::update(
                parsed.positionals.first().map(String::as_str),
                &home.join("plugins"),
                &home.join("plugin-state.json"),
                parsed.trust,
            )?;
            if parsed.json {
                println!("{}", serde_json::to_string_pretty(&updates)?);
            } else if updates.is_empty() {
                println!("No managed plugins to update");
            } else {
                for update in updates {
                    println!("Updated {} from {}", update.name, update.source);
                }
            }
            Ok(0)
        }
        "marketplace" | "market" | "catalog" => {
            run_marketplace(&parsed.positionals, parsed.trust, parsed.json)
        }
        "enable" | "disable" => {
            let name = parsed
                .positionals
                .first()
                .context("Usage: maestro plugins enable|disable <name>")?;
            let state_path = state_path_for_plugin(&registry, name)?;
            crate::plugins::set_enabled(&state_path, name, command == "enable")?;
            println!("{name}: {command}d");
            Ok(0)
        }
        "capability" => {
            let name = parsed.positionals.first().context(
                "Usage: maestro plugins capability <name> <skills|agents|commands|hooks|mcp|connections> <on|off>",
            )?;
            let capability = parsed
                .positionals
                .get(1)
                .context("missing plugin capability")
                .and_then(|value| crate::plugins::PluginCapability::parse(value))?;
            let enabled = match parsed.positionals.get(2).map(String::as_str) {
                Some("on" | "enable" | "enabled") => true,
                Some("off" | "disable" | "disabled") => false,
                _ => bail!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("capability state must be on or off", &[])
                ),
            };
            let state_path = state_path_for_plugin(&registry, name)?;
            crate::plugins::set_capability(&state_path, name, capability, enabled)?;
            println!(
                "{name} {capability:?}: {}",
                if enabled { "on" } else { "off" }
            );
            Ok(0)
        }
        // Bare plugin name after `maestro plugins <name>` → info lookup.
        other if parsed.positionals.is_empty() && !other.starts_with('-') => {
            run_info(&registry, other, parsed.json)
        }
        other => {
            eprintln!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unknown plugins subcommand: {0}", &[(other).to_string()])
            );
            eprintln!(
                "{}",
                crate::localization::cli_locale()
                    .format("Try: maestro plugins list|info <name>", &[])
            );
            Ok(1)
        }
    }
}

fn state_path_for_plugin(registry: &PluginRegistry, name: &str) -> Result<PathBuf> {
    let plugin = registry
        .get(name)
        .with_context(|| format!("plugin not found: {name}"))?;
    let base = plugin
        .root
        .parent()
        .and_then(Path::parent)
        .with_context(|| format!("plugin path has no state root: {}", plugin.root.display()))?;
    Ok(base.join("plugin-state.json"))
}

fn parse_args(args: &[String]) -> Result<PluginArgs> {
    let mut parsed = PluginArgs::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--json" => parsed.json = true,
            "--trust" => parsed.trust = true,
            "--help" | "-h" => parsed.help = true,
            "--workspace" | "--cwd" => {
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                    .ok_or_else(|| anyhow::anyhow!("{arg} requires a value"))?;
                parsed.workspace = Some(PathBuf::from(value));
                index += 1;
            }
            value if value.starts_with("--workspace=") || value.starts_with("--cwd=") => {
                let value = value
                    .split_once('=')
                    .map(|(_, rest)| rest)
                    .filter(|rest| !rest.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("{arg} requires a value"))?;
                parsed.workspace = Some(PathBuf::from(value));
            }
            value if value.starts_with('-') => bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Unknown maestro plugins option: {0}",
                    &[(value).to_string()]
                )
            ),
            value if parsed.command.is_none() => parsed.command = Some(value.to_owned()),
            value => parsed.positionals.push(value.to_owned()),
        }
        index += 1;
    }
    Ok(parsed)
}

fn print_help() {
    println!("{}", crate::localization::cli_locale().format("maestro plugins [list|info|marketplace] [name] [options]\n\nCommands:\nlist                   List discovered plugins (default)\ninfo <name>            Show one plugin's path, origin, and components\ninstall <path|git-url> Install a plugin; git URLs require --trust\nremove <name>          Atomically remove a managed plugin\nupdate [name]          Refresh one or all managed plugins; explicit pins stay pinned\nmarketplace [list]     List curated catalog (id, tier, source)\nmarketplace install <id>  Install catalog entry; non-official needs --trust\nenable|disable <name>  Toggle the whole plugin\ncapability <name> <skills|agents|commands|hooks|mcp|connections> <on|off>\n<name>                 Alias for info <name>\n\nOptions:\n--json                 Emit machine-readable JSON\n--trust                Explicitly trust and execute remote plugin code\n--workspace <path>     Discover relative to this workspace (default: cwd)\n--help, -h             Show this help\n\nDiscovery roots (high wins on name collision):\n.maestro/plugins/<name>/   project\n~/.maestro/plugins/<name>/ user\n.composer/plugins/<name>/  legacy project\n~/.composer/plugins/<name>/ legacy user\n\nUse top-level `--plugin <path>` to load a local plugin for one process without persisting state. Installed plugin code and each capability remain independently disableable.", &[]));
}

fn run_marketplace(positionals: &[String], trust: bool, json: bool) -> Result<i32> {
    let sub = positionals.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => {
            let catalog = crate::plugins::builtin_catalog();
            let registry = discover_registry(None)?;
            let installed: std::collections::HashSet<String> =
                registry.plugins().iter().map(|p| p.name.clone()).collect();
            if json {
                let rows: Vec<serde_json::Value> = catalog
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "id": e.id,
                            "displayName": e.display_name,
                            "tier": e.tier.as_str(),
                            "description": e.description,
                            "source": e.source,
                            "homepage": e.homepage,
                            "installed": crate::plugins::is_installed(e, &installed),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!("{}", crate::plugins::format_catalog(&catalog, &installed));
            }
            Ok(0)
        }
        "install" => {
            let id = positionals
                .get(1)
                .map(String::as_str)
                .filter(|s| !s.is_empty())
                .context("Usage: maestro plugins marketplace install <id> [--trust]")?;
            let catalog = crate::plugins::builtin_catalog();
            let entry = crate::plugins::find_entry(&catalog, id).with_context(|| {
                format!(
                    "marketplace entry '{id}' not found; try `maestro plugins marketplace list`"
                )
            })?;
            if entry.tier.requires_explicit_trust() && !trust {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "entry '{0}' ({1}) requires --trust for install",
                        &[(entry.id).clone(), (entry.tier.as_str()).to_string()]
                    )
                );
            }
            let source = crate::plugins::resolve_install_source(entry)?;
            let home = maestro_home_dir().context("could not resolve ~/.maestro")?;
            let preview = crate::plugins::install_with_provenance(
                &source,
                &home.join("plugins"),
                &home.join("plugin-state.json"),
                trust || !entry.tier.requires_explicit_trust(),
                Some(crate::plugins::InstallProvenance {
                    marketplace_id: Some(entry.id.clone()),
                    marketplace_tier: Some(entry.tier.as_str().to_string()),
                }),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&preview)?);
            } else {
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Installed {0} from {1} (marketplace id={2}, tier={3}, capabilities: {4})",
                        &[
                            (preview.name).clone(),
                            (preview.source).clone(),
                            (entry.id).clone(),
                            (entry.tier.as_str()).to_string(),
                            format!("{:?}", preview.capabilities)
                        ]
                    )
                );
            }
            Ok(0)
        }
        other => {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Unknown marketplace subcommand: {0}",
                    &[(other).to_string()]
                )
            );
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Usage: maestro plugins marketplace [list|install <id>] [--trust]",
                    &[]
                )
            );
            Ok(1)
        }
    }
}

/// Discover for the read-only `maestro plugins` CLI.
///
/// Intentionally uses the ungated [`PluginRegistry::discover_from`], not
/// [`PluginRegistry::discover`]: this command only lists metadata (name,
/// origin, component paths) for an operator to inspect, it never wires a
/// plugin's skills/hooks/MCP into execution, so showing project-scoped
/// plugins here even for an untrusted workspace is a debugging aid, not a
/// trust bypass. Any path that actually loads plugin components for
/// execution (the interactive TUI's `PluginRegistry::discover()`) must stay
/// gated on workspace trust.
fn discover_registry(workspace: Option<&Path>) -> Result<PluginRegistry> {
    let cwd = match workspace {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let roots = search_roots_for_workspace(
        &cwd,
        maestro_home_dir().as_deref(),
        legacy_composer_home_dir().as_deref(),
    );
    Ok(PluginRegistry::discover_from(&roots))
}

fn run_list(registry: &PluginRegistry, json: bool) -> Result<i32> {
    if json {
        let report = PluginListReport {
            plugins: registry.plugins().iter().map(list_entry).collect(),
            count: registry.len(),
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(0);
    }

    if registry.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format("No plugins found", &[])
        );
        println!();
        println!(
            "{}",
            crate::localization::cli_locale().format("Install plugins under:", &[])
        );
        println!("  .maestro/plugins/<name>/   (project)");
        println!("  ~/.maestro/plugins/<name>/ (user)");
        println!();
        println!("{}", crate::localization::cli_locale().format("Each plugin may include plugin.json or .plugin/plugin.json, skills/, agents/, commands/, hooks, MCP configs, and declarative connection types.", &[]));
        return Ok(0);
    }

    println!(
        "{}",
        crate::localization::cli_locale().format("Plugins", &[])
    );
    println!();
    for plugin in registry.plugins() {
        let version = plugin
            .manifest
            .as_ref()
            .and_then(|m| m.version.as_deref())
            .unwrap_or("-");
        println!(
            "- {} ({}) — {} — {}",
            plugin.name,
            plugin.origin.as_str(),
            version,
            plugin.root.display()
        );
        println!("  components: {}", plugin.component_summary());
    }
    println!();
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "{0} plugin(s) discovered. Use `maestro plugins info <name>` for details.",
            &[(registry.len()).to_string()]
        )
    );
    Ok(0)
}

fn run_info(registry: &PluginRegistry, name: &str, json: bool) -> Result<i32> {
    let Some(plugin) = registry.get(name) else {
        eprintln!(
            "{}",
            crate::localization::cli_locale()
                .format("Plugin not found: {0}", &[(name).to_string()])
        );
        if registry.is_empty() {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "No plugins discovered. Install under .maestro/plugins/<name>/.",
                    &[]
                )
            );
        } else {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format("Known plugins:", &[])
            );
            for p in registry.plugins() {
                eprintln!("  - {}", p.name);
            }
        }
        return Ok(1);
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&info_report(plugin))?);
        return Ok(0);
    }

    print!("{}", plugin.detail_report());
    Ok(0)
}

fn list_entry(plugin: &DiscoveredPlugin) -> PluginListEntry {
    PluginListEntry {
        name: plugin.name.clone(),
        origin: plugin.origin.as_str().to_string(),
        path: plugin.root.display().to_string(),
        version: plugin.manifest.as_ref().and_then(|m| m.version.clone()),
        description: plugin.manifest.as_ref().and_then(|m| m.description.clone()),
        components: component_labels(plugin),
        has_manifest: plugin.manifest.is_some(),
        manifest_path: plugin
            .manifest_path
            .as_ref()
            .map(|path| path.display().to_string()),
    }
}

fn info_report(plugin: &DiscoveredPlugin) -> PluginInfoReport {
    PluginInfoReport {
        name: plugin.name.clone(),
        origin: plugin.origin.as_str().to_string(),
        path: plugin.root.display().to_string(),
        version: plugin.manifest.as_ref().and_then(|m| m.version.clone()),
        description: plugin.manifest.as_ref().and_then(|m| m.description.clone()),
        has_manifest: plugin.manifest.is_some(),
        manifest_path: plugin
            .manifest_path
            .as_ref()
            .map(|path| path.display().to_string()),
        components: PluginComponentPaths {
            skills: plugin
                .components
                .skills_dir
                .as_ref()
                .map(|p| p.display().to_string()),
            agents: plugin
                .components
                .agents_dir
                .as_ref()
                .map(|p| p.display().to_string()),
            commands: plugin
                .components
                .commands_dir
                .as_ref()
                .map(|p| p.display().to_string()),
            hooks: plugin
                .components
                .hooks_config
                .as_ref()
                .map(|p| p.display().to_string()),
            mcp: plugin
                .components
                .mcp_path
                .as_ref()
                .map(|p| p.display().to_string()),
            connections: plugin
                .components
                .connections_path
                .as_ref()
                .map(|p| p.display().to_string()),
        },
    }
}

fn component_labels(plugin: &DiscoveredPlugin) -> Vec<&'static str> {
    let mut parts = Vec::new();
    if plugin.components.skills_dir.is_some() {
        parts.push("skills");
    }
    if plugin.components.agents_dir.is_some() {
        parts.push("agents");
    }
    if plugin.components.commands_dir.is_some() {
        parts.push("commands");
    }
    if plugin.components.hooks_config.is_some() {
        parts.push("hooks");
    }
    if plugin.components.mcp_path.is_some() {
        parts.push("mcp");
    }
    if plugin.components.connections_path.is_some() {
        parts.push("connections");
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::PluginOrigin;
    use std::fs;
    use tempfile::TempDir;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn make_plugin(root: &Path, name: &str) {
        let plugin = root.join(name);
        fs::create_dir_all(plugin.join("skills")).unwrap();
        write_file(
            &plugin.join("plugin.json"),
            &format!(r#"{{"name":"{name}","version":"1.2.3","description":"cli test plugin"}}"#),
        );
        write_file(
            &plugin.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill for CLI tests\n---\n# Demo\n",
        );
    }

    #[test]
    fn parse_list_json_and_workspace() {
        let args = [
            "list".into(),
            "--json".into(),
            "--workspace".into(),
            "/tmp/ws".into(),
        ]
        .to_vec();
        let parsed = parse_args(&args).unwrap();
        assert_eq!(parsed.command.as_deref(), Some("list"));
        assert!(parsed.json);
        assert_eq!(parsed.workspace.as_deref(), Some(Path::new("/tmp/ws")));
    }

    #[test]
    fn list_and_info_against_workspace() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join(".maestro").join("plugins");
        make_plugin(&plugins_root, "team-tools");

        let list_args = [
            "list".into(),
            "--json".into(),
            "--workspace".into(),
            tmp.path().display().to_string(),
        ]
        .to_vec();
        assert_eq!(run_plugins(&list_args).unwrap(), 0);

        let info_args = [
            "info".into(),
            "team-tools".into(),
            "--json".into(),
            format!("--workspace={}", tmp.path().display()),
        ]
        .to_vec();
        assert_eq!(run_plugins(&info_args).unwrap(), 0);

        let missing = [
            "info".into(),
            "missing-plugin".into(),
            "--workspace".into(),
            tmp.path().display().to_string(),
        ]
        .to_vec();
        assert_eq!(run_plugins(&missing).unwrap(), 1);
    }

    #[test]
    fn bare_name_acts_as_info() {
        let tmp = TempDir::new().unwrap();
        make_plugin(&tmp.path().join(".maestro").join("plugins"), "solo");
        let args = [
            "solo".into(),
            "--json".into(),
            "--workspace".into(),
            tmp.path().display().to_string(),
        ]
        .to_vec();
        assert_eq!(run_plugins(&args).unwrap(), 0);
    }

    #[test]
    fn help_exits_cleanly() {
        assert_eq!(run_plugins(&["--help".into()]).unwrap(), 0);
        assert_eq!(run_plugins(&["help".into()]).unwrap(), 0);
    }

    #[test]
    fn list_entry_includes_components() {
        let plugin = DiscoveredPlugin {
            name: "x".into(),
            root: PathBuf::from("/p/x"),
            origin: PluginOrigin::Project,
            manifest: None,
            manifest_path: None,
            components: crate::plugins::PluginComponents {
                skills_dir: Some(PathBuf::from("/p/x/skills")),
                agents_dir: Some(PathBuf::from("/p/x/agents")),
                commands_dir: None,
                hooks_config: None,
                mcp_path: Some(PathBuf::from("/p/x/mcp.json")),
                connections_path: None,
            },
            trust_state: None,
            resource_filters: crate::plugins::PluginResourceFilters::default(),
        };
        let entry = list_entry(&plugin);
        assert_eq!(entry.components, vec!["skills", "agents", "mcp"]);
        assert!(!entry.has_manifest);
        assert!(entry.manifest_path.is_none());
        let report = info_report(&plugin);
        assert_eq!(report.components.agents.as_deref(), Some("/p/x/agents"));
    }

    #[test]
    fn state_path_tracks_the_discovered_plugin_origin() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("workspace/.maestro/plugins");
        make_plugin(&root, "shadowed");
        let registry = PluginRegistry::discover_from(&[(root, PluginOrigin::Project)]);

        assert_eq!(
            state_path_for_plugin(&registry, "shadowed").unwrap(),
            temp.path().join("workspace/.maestro/plugin-state.json")
        );
    }
}
