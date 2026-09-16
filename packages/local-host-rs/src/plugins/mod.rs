//! Native plugin discovery foundation.
//!
//! Plugins are filesystem packages that may bundle skills, slash-command
//! templates, hooks, MCP configs, and declarative connection types:
//!
//! ```text
//! <plugin-root>/
//!   plugin.json or .plugin/plugin.json  # optional manifest
//!   skills/              # SKILL.md packages
//!   agents/              # specialized agent definitions
//!   commands/            # markdown command templates
//!   hooks/hooks.json or hooks.toml
//!   .mcp.json or mcp.json
//!   connections.json     # declarative types; no secrets
//! ```
//!
//! # Discovery order (high → low priority)
//!
//! 1. CLI/env override (reserved for a later slice)
//! 2. `.maestro/plugins/*` (project)
//! 3. `~/.maestro/plugins/*` (user)
//! 4. Legacy: `.composer/plugins/*` and `~/.composer/plugins/*`
//!
//! Name collisions prefer the higher-priority origin (project over user).
//!
//! Discovery, listing, install, trust state, and a curated marketplace catalog.

mod connection_types;
mod discovery;
mod loader;
mod manager;
mod manifest;
pub mod marketplace;
mod project_manifest;

pub use connection_types::{
    ConnectionMcpBindingDefinition, ConnectionTypeDefinition, ConnectionTypeManifest,
};
pub use discovery::{PluginOrigin, default_search_roots, search_roots_for_workspace};
pub use loader::{
    MAX_PLUGIN_FILE_BYTES, NATIVE_PLUGIN_MANIFEST_PATH, PORTABLE_PLUGIN_MANIFEST_PATH,
    PluginComponents, load_manifest, load_manifest_with_path, read_plugin_file, resolve_components,
};
pub use manager::{
    InstallPreview, InstallProvenance, PluginCapability, PluginState, PluginTrustState, install,
    install_with_provenance, package_integrity, remove, set_capability, set_enabled, update,
};
pub use manifest::PluginManifest;
pub use marketplace::{
    MarketplaceEntry, MarketplaceTier, builtin_catalog, find_entry, format_catalog, is_installed,
    resolve_install_source,
};
pub use project_manifest::{
    PROJECT_PLUGIN_MANIFEST_PATH, PluginResourceFilters, ProjectPluginDeclaration,
    ProjectPluginManifest, ProjectPluginScope, ProjectPluginSource, ResourceFilter,
};

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A plugin package discovered on disk.
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    /// Plugin name (manifest `name` or directory name).
    pub name: String,
    /// Absolute or CWD-relative plugin root directory.
    pub root: PathBuf,
    /// Discovery origin (project / user / legacy).
    pub origin: PluginOrigin,
    /// Parsed native or portable manifest when present.
    pub manifest: Option<PluginManifest>,
    /// Manifest path that supplied plugin identity and metadata.
    pub manifest_path: Option<PathBuf>,
    /// Resolved component paths (skills, agents, commands, hooks, MCP).
    pub components: PluginComponents,
    /// Managed installation metadata, when present.
    pub trust_state: Option<PluginTrustState>,
    /// Project-declared resource filters, applied beneath capability gates.
    pub resource_filters: PluginResourceFilters,
}

impl DiscoveredPlugin {
    /// Short summary of present components for `/plugins` listing.
    #[must_use]
    pub fn component_summary(&self) -> String {
        let mut parts = Vec::new();
        if self.components.skills_dir.is_some() {
            parts.push(maestro_ui::localization::tr("skills"));
        }
        if self.components.agents_dir.is_some() {
            parts.push(maestro_ui::localization::tr("agents"));
        }
        if self.components.commands_dir.is_some() {
            parts.push(maestro_ui::localization::tr("commands"));
        }
        if self.components.hooks_config.is_some() {
            parts.push(maestro_ui::localization::tr("hooks"));
        }
        if self.components.mcp_path.is_some() {
            parts.push(maestro_ui::localization::tr("mcp"));
        }
        if self.components.connections_path.is_some() {
            parts.push(maestro_ui::localization::tr("connections"));
        }
        if parts.is_empty() {
            maestro_ui::localization::tr("no components").to_string()
        } else {
            parts.join(", ")
        }
    }

    /// Detailed multi-line description for `/plugins <name>`.
    #[must_use]
    pub fn detail_report(&self) -> String {
        let mut msg = maestro_ui::localization::format(
            "## Plugin: {0}\n\n",
            std::slice::from_ref(&(self.name)),
        );
        msg.push_str(&maestro_ui::localization::format(
            "**Path:** `{0}`\n",
            &[(self.root.display()).to_string()],
        ));
        msg.push_str(&maestro_ui::localization::format(
            "**Origin:** {0}\n",
            &[(self.origin.as_str()).to_string()],
        ));

        if let Some(ref manifest) = self.manifest {
            if let Some(ref version) = manifest.version {
                msg.push_str(&maestro_ui::localization::format(
                    "**Version:** {0}\n",
                    std::slice::from_ref(version),
                ));
            }
            if let Some(ref description) = manifest.description {
                msg.push_str(&maestro_ui::localization::format(
                    "**Description:** {0}\n",
                    std::slice::from_ref(description),
                ));
            }
            let manifest_path = self
                .manifest_path
                .as_deref()
                .and_then(|path| path.strip_prefix(&self.root).ok())
                .unwrap_or_else(|| Path::new("plugin.json"));
            msg.push_str(&maestro_ui::localization::format(
                "**Manifest:** `{0}`\n",
                &[(manifest_path.display()).to_string()],
            ));
        } else {
            msg.push_str(maestro_ui::localization::tr(
                "**Manifest:** _(convention paths)_\n",
            ));
        }

        msg.push_str(maestro_ui::localization::tr("\n### Components\n\n"));
        match &self.components.skills_dir {
            Some(p) => msg.push_str(&maestro_ui::localization::format(
                "- **skills:** `{0}`\n",
                &[(p.display()).to_string()],
            )),
            None => msg.push_str(maestro_ui::localization::tr("- **skills:** _(none)_\n")),
        }
        match &self.components.agents_dir {
            Some(p) => msg.push_str(&maestro_ui::localization::format(
                "- **agents:** `{0}`\n",
                &[(p.display()).to_string()],
            )),
            None => msg.push_str(maestro_ui::localization::tr("- **agents:** _(none)_\n")),
        }
        match &self.components.commands_dir {
            Some(p) => msg.push_str(&maestro_ui::localization::format(
                "- **commands:** `{0}`\n",
                &[(p.display()).to_string()],
            )),
            None => msg.push_str(maestro_ui::localization::tr("- **commands:** _(none)_\n")),
        }
        match &self.components.hooks_config {
            Some(p) => msg.push_str(&maestro_ui::localization::format(
                "- **hooks:** `{0}`\n",
                &[(p.display()).to_string()],
            )),
            None => msg.push_str(maestro_ui::localization::tr("- **hooks:** _(none)_\n")),
        }
        match &self.components.mcp_path {
            Some(p) => msg.push_str(&maestro_ui::localization::format(
                "- **mcp:** `{0}`\n",
                &[(p.display()).to_string()],
            )),
            None => msg.push_str(maestro_ui::localization::tr("- **mcp:** _(none)_\n")),
        }
        match &self.components.connections_path {
            Some(p) => msg.push_str(&maestro_ui::localization::format(
                "- **connections:** `{0}`\n",
                &[(p.display()).to_string()],
            )),
            None => msg.push_str(maestro_ui::localization::tr(
                "- **connections:** _(none)_\n",
            )),
        }

        msg
    }
}

/// Registry of discovered plugins.
#[derive(Debug, Clone, Default)]
pub struct PluginRegistry {
    plugins: Vec<DiscoveredPlugin>,
    /// Project-scoped roots that existed but were skipped because the
    /// workspace isn't trusted (see [`Self::discover`]).
    skipped_untrusted_roots: Vec<PathBuf>,
    admission_errors: Vec<String>,
}

impl PluginRegistry {
    /// Discover plugins from default Maestro/composer roots relative to the
    /// current process working directory.
    ///
    /// Project-scoped roots (`.maestro/plugins`, legacy `.composer/plugins`)
    /// are repository-controlled: a plugin found there contributes skill
    /// directories (and, once wired, hooks/MCP configs) that run
    /// automatically. They are only searched when the workspace is marked
    /// trusted in *global* config, so a repository cannot grant itself
    /// trust (see `crate::config::workspace_trusted_in_global_config`).
    #[must_use]
    pub fn discover() -> Self {
        let workspace_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::discover_for_workspace(&workspace_dir)
    }

    /// Discover plugins for an explicit workspace while preserving the
    /// standard user roots and workspace trust gate.
    #[must_use]
    pub fn discover_for_workspace(workspace_dir: &Path) -> Self {
        let workspace_trusted = crate::config::workspace_trusted_in_global_config(workspace_dir);
        let roots = search_roots_for_workspace(
            workspace_dir,
            crate::path_utils::maestro_home_dir().as_deref(),
            crate::path_utils::legacy_composer_home_dir().as_deref(),
        );
        Self::discover_for_workspace_with_roots(workspace_dir, &roots, workspace_trusted)
    }

    fn discover_for_workspace_with_roots(
        workspace_dir: &Path,
        roots: &[(PathBuf, PluginOrigin)],
        workspace_trusted: bool,
    ) -> Self {
        let mut registry = Self::discover_gated(roots, workspace_trusted);
        if workspace_trusted {
            if let Err(error) =
                project_manifest::apply_project_manifest(workspace_dir, &mut registry.plugins)
            {
                registry
                    .plugins
                    .retain(|plugin| !plugin.origin.is_project_scoped());
                registry
                    .admission_errors
                    .push(format!("project plugin manifest rejected: {error:#}"));
            }
        }
        registry.load_ephemeral_plugins();
        registry
    }

    /// Discover from explicit roots (low → high priority order), applying
    /// the workspace-trust gate over project-scoped origins.
    ///
    /// Split out so tests can exercise both the trusted and untrusted paths
    /// deterministically without mutating the real process `$HOME`.
    #[must_use]
    fn discover_gated(roots: &[(PathBuf, PluginOrigin)], workspace_trusted: bool) -> Self {
        let skipped_untrusted_roots: Vec<PathBuf> = if workspace_trusted {
            Vec::new()
        } else {
            roots
                .iter()
                .filter(|(root, origin)| origin.is_project_scoped() && root.is_dir())
                .map(|(root, _)| root.clone())
                .collect()
        };

        let filtered: Vec<(PathBuf, PluginOrigin)> = roots
            .iter()
            .filter(|(_, origin)| workspace_trusted || !origin.is_project_scoped())
            .cloned()
            .collect();

        let mut registry = Self::discover_from(&filtered);
        registry.skipped_untrusted_roots = skipped_untrusted_roots;
        registry
    }

    /// Discover plugins from explicit roots (low → high priority order),
    /// with **no** trust gating applied.
    ///
    /// This is a low-level primitive for callers that pre-filter roots
    /// themselves (e.g. read-only CLI listing). Production code that feeds
    /// plugin components into execution must go through [`Self::discover`]
    /// instead.
    ///
    /// When multiple plugins share a name, the later (higher-priority) root wins.
    #[must_use]
    pub fn discover_from(roots: &[(PathBuf, PluginOrigin)]) -> Self {
        let mut by_name: HashMap<String, DiscoveredPlugin> = HashMap::new();

        for (root, origin) in roots {
            if !root.is_dir() {
                continue;
            }
            let state = match root
                .parent()
                .map(|parent| PluginState::load(&parent.join("plugin-state.json")))
                .transpose()
            {
                Ok(state) => state.unwrap_or_default(),
                Err(error) => {
                    eprintln!(
                        "[plugins] refusing to load {}: invalid trust state: {error}",
                        root.display()
                    );
                    continue;
                }
            };
            let Ok(entries) = fs::read_dir(root) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if let Some(mut plugin) = load_plugin_dir(&path, *origin) {
                    let key = plugin.name.to_lowercase();
                    plugin.trust_state = state.plugins.get(&key).cloned();
                    if state
                        .plugins
                        .get(&key)
                        .is_some_and(|plugin_state| !plugin_state.enabled)
                    {
                        plugin.components = PluginComponents::default();
                        by_name.insert(key, plugin);
                        continue;
                    }
                    if !state.capability_enabled(&key, PluginCapability::Skills) {
                        plugin.components.skills_dir = None;
                    }
                    if !state.capability_enabled(&key, PluginCapability::Agents) {
                        plugin.components.agents_dir = None;
                    }
                    if !state.capability_enabled(&key, PluginCapability::Commands) {
                        plugin.components.commands_dir = None;
                    }
                    if !state.capability_enabled(&key, PluginCapability::Hooks) {
                        plugin.components.hooks_config = None;
                    }
                    if !state.capability_enabled(&key, PluginCapability::Mcp) {
                        plugin.components.mcp_path = None;
                    }
                    if !state.capability_enabled(&key, PluginCapability::Connections) {
                        plugin.components.connections_path = None;
                    }
                    by_name.insert(key, plugin);
                }
            }
        }

        let mut plugins: Vec<_> = by_name.into_values().collect();
        plugins.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.root.cmp(&b.root))
        });

        Self {
            plugins,
            skipped_untrusted_roots: Vec::new(),
            admission_errors: Vec::new(),
        }
    }

    fn load_ephemeral_plugins(&mut self) {
        let Some(raw) = std::env::var_os("MAESTRO_EPHEMERAL_PLUGINS") else {
            return;
        };
        for path in std::env::split_paths(&raw) {
            match self.load_ephemeral_plugin(&path) {
                Ok(()) => {}
                Err(error) => self.admission_errors.push(format!(
                    "ephemeral plugin {} rejected: {error:#}",
                    path.display()
                )),
            }
        }
        self.plugins.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.root.cmp(&b.root))
        });
    }

    fn load_ephemeral_plugin(&mut self, path: &Path) -> anyhow::Result<()> {
        manager::validate_tree(path)?;
        let plugin = load_plugin_dir(path, PluginOrigin::Ephemeral)
            .ok_or_else(|| anyhow::anyhow!("plugin has no valid manifest or components"))?;
        let key = plugin.name.to_lowercase();
        self.plugins
            .retain(|candidate| candidate.name.to_lowercase() != key);
        self.plugins.push(plugin);
        Ok(())
    }

    /// Human-readable notice describing any project-scoped plugin roots
    /// skipped by [`Self::discover`] because the workspace isn't trusted,
    /// or `None` if none were skipped.
    #[must_use]
    pub fn untrusted_skip_notice(&self) -> Option<String> {
        if self.skipped_untrusted_roots.is_empty() {
            return None;
        }
        let paths: Vec<String> = self
            .skipped_untrusted_roots
            .iter()
            .map(|path| format!("`{}`", path.display()))
            .collect();
        Some(format!(
            "This workspace isn't trusted, so {} project plugin root(s) were not loaded: {}. \
             Set projects.\"<workspace>\".trust_level = \"trusted\" in global config \
             (~/.composer/config.toml) to enable them.",
            paths.len(),
            paths.join(", ")
        ))
    }

    /// Admission failures that left a project or one-run plugin disabled.
    #[must_use]
    pub fn admission_errors(&self) -> &[String] {
        &self.admission_errors
    }

    /// All discovered plugins (sorted by name).
    #[must_use]
    pub fn plugins(&self) -> &[DiscoveredPlugin] {
        &self.plugins
    }

    /// Look up a plugin by name (case-insensitive).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&DiscoveredPlugin> {
        let needle = name.to_lowercase();
        self.plugins
            .iter()
            .find(|p| p.name.to_lowercase() == needle)
    }

    /// Skill directories from all plugins (for SkillLoader integration).
    #[must_use]
    pub fn skill_dirs(&self) -> Vec<PathBuf> {
        self.plugins.iter().flat_map(filtered_skill_dirs).collect()
    }

    /// Agent/profile definition directories from all plugins.
    #[must_use]
    pub fn agent_dirs(&self) -> Vec<PathBuf> {
        self.plugins
            .iter()
            .filter_map(|p| p.components.agents_dir.clone())
            .collect()
    }

    /// Exact agent resources from plugins. Unfiltered plugins contribute their
    /// directory; filtered plugins contribute only admitted Markdown files.
    #[must_use]
    pub fn agent_paths(&self) -> Vec<PathBuf> {
        self.plugins
            .iter()
            .flat_map(|plugin| {
                filtered_component_paths(
                    plugin,
                    plugin.components.agents_dir.as_deref(),
                    &plugin.resource_filters.agents,
                    |path| path.extension().is_some_and(|extension| extension == "md"),
                )
            })
            .collect()
    }

    /// Command template directories from all plugins.
    #[must_use]
    pub fn command_dirs(&self) -> Vec<PathBuf> {
        self.plugins
            .iter()
            .filter_map(|p| p.components.commands_dir.clone())
            .collect()
    }

    /// Exact command resources from plugins. Unfiltered plugins contribute
    /// their directory; filtered plugins contribute only admitted files.
    #[must_use]
    pub fn command_paths(&self) -> Vec<PathBuf> {
        self.plugins
            .iter()
            .flat_map(|plugin| {
                filtered_component_paths(
                    plugin,
                    plugin.components.commands_dir.as_deref(),
                    &plugin.resource_filters.commands,
                    |path| path.is_file(),
                )
            })
            .collect()
    }

    /// MCP config file paths from all plugins.
    #[must_use]
    pub fn mcp_paths(&self) -> Vec<PathBuf> {
        self.plugins
            .iter()
            .filter_map(|p| p.components.mcp_path.clone())
            .collect()
    }

    /// Declarative connection type manifests from explicitly trusted plugins.
    #[must_use]
    pub fn connection_paths(&self) -> Vec<PathBuf> {
        self.plugins
            .iter()
            .filter_map(|p| p.components.connections_path.clone())
            .collect()
    }

    /// Hook config file paths from all plugins.
    #[must_use]
    pub fn hook_configs(&self) -> Vec<PathBuf> {
        self.plugins
            .iter()
            .filter_map(|p| p.components.hooks_config.clone())
            .collect()
    }

    /// Number of discovered plugins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether any plugins were discovered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Format a list report for `/plugins`.
    #[must_use]
    pub fn list_report(&self) -> String {
        if self.plugins.is_empty() {
            return [
                maestro_ui::localization::tr("## Plugins\n").to_string(),
                maestro_ui::localization::tr("*No plugins found*\n\n").to_string(),
                maestro_ui::localization::tr("Install plugins under:\n").to_string(),
                maestro_ui::localization::tr("- `.maestro/plugins/<name>/` (project)\n").to_string(),
                maestro_ui::localization::tr("- `~/.maestro/plugins/<name>/` (user)\n\n").to_string(),
                maestro_ui::localization::tr("Each plugin may include `plugin.json` or `.plugin/plugin.json`, `skills/`, `agents/`, `commands/`, hooks, and MCP configs.\n").to_string(),
            ]
            .concat();
        }

        let mut msg = String::from(maestro_ui::localization::tr("## Plugins\n\n"));
        for plugin in &self.plugins {
            let version = plugin
                .manifest
                .as_ref()
                .and_then(|m| m.version.as_deref())
                .unwrap_or("-");
            msg.push_str(&maestro_ui::localization::format(
                "- **{0}** ({1}) — {2} — `{3}`\n  components: {4}\n",
                &[
                    (plugin.name).clone(),
                    (plugin.origin.as_str()).to_string(),
                    (version).to_string(),
                    (plugin.root.display()).to_string(),
                    (plugin.component_summary()).clone(),
                ],
            ));
        }
        msg.push_str(&maestro_ui::localization::format(
            "\n*{0} plugin(s) discovered*\n\nUse `/plugins <name>` for details.\n",
            &[(self.plugins.len()).to_string()],
        ));
        msg
    }
}

fn load_plugin_dir(path: &Path, origin: PluginOrigin) -> Option<DiscoveredPlugin> {
    if path.join(".maestro-untrusted").exists() {
        return None;
    }
    let dir_name = path.file_name()?.to_str()?.to_string();
    // Skip hidden directories (e.g. .git under plugins root).
    if dir_name.starts_with('.') {
        return None;
    }

    let loaded_manifest = load_manifest_with_path(path);
    let manifest_path = loaded_manifest.as_ref().map(|(_, path)| path.clone());
    let manifest = loaded_manifest.map(|(manifest, _)| manifest);
    let name = manifest
        .as_ref()
        .and_then(|m| m.name.clone())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(dir_name);
    let components = resolve_components(path, manifest.as_ref());

    Some(DiscoveredPlugin {
        name,
        root: path.to_path_buf(),
        origin,
        manifest,
        manifest_path,
        components,
        trust_state: None,
        resource_filters: PluginResourceFilters::default(),
    })
}

fn filtered_component_paths(
    plugin: &DiscoveredPlugin,
    root: Option<&Path>,
    filter: &ResourceFilter,
    accepts: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let Some(root) = root else {
        return Vec::new();
    };
    if !filter.is_active() {
        return vec![root.to_path_buf()];
    }
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| accepts(path))
        .filter(|path| {
            path.strip_prefix(&plugin.root)
                .is_ok_and(|relative| filter.allows(relative))
        })
        .collect();
    paths.sort();
    paths
}

fn filtered_skill_dirs(plugin: &DiscoveredPlugin) -> Vec<PathBuf> {
    let Some(root) = plugin.components.skills_dir.as_deref() else {
        return Vec::new();
    };
    let filter = &plugin.resource_filters.skills;
    if !filter.is_active() {
        return vec![root.to_path_buf()];
    }
    let mut paths = Vec::new();
    for candidate in std::iter::once(root.to_path_buf()).chain(
        fs::read_dir(root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir()),
    ) {
        let skill_file = [candidate.join("SKILL.md"), candidate.join("skill.md")]
            .into_iter()
            .find(|path| path.is_file());
        let Some(_skill_file) = skill_file else {
            continue;
        };
        let allowed = candidate
            .strip_prefix(&plugin.root)
            .is_ok_and(|relative| filter.allows(relative));
        if allowed {
            paths.push(candidate);
        }
    }
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn make_plugin(root: &Path, name: &str, with_manifest: bool, with_skills: bool) {
        let plugin = root.join(name);
        fs::create_dir_all(&plugin).unwrap();
        if with_manifest {
            write_file(
                &plugin.join("plugin.json"),
                &format!(
                    r#"{{"name":"{name}","version":"0.1.0","description":"test plugin","skills":"skills"}}"#
                ),
            );
        }
        if with_skills {
            let skill_dir = plugin.join("skills").join("demo-skill");
            fs::create_dir_all(&skill_dir).unwrap();
            write_file(
                &skill_dir.join("SKILL.md"),
                "---\nname: demo-skill\ndescription: Demo skill for plugin tests\n---\n# Demo\n",
            );
        }
    }

    // ── Trust gate (repo-controlled `.maestro/plugins`, `.composer/plugins`) ──

    /// Regression test for the trust-gate fix: an untrusted workspace's
    /// project-scoped plugin roots must not be discovered at all, so their
    /// skills (the only currently-wired component) never load. Before the
    /// fix, `PluginRegistry::discover` had no trust check at all.
    #[test]
    fn test_untrusted_workspace_skips_project_plugin_roots() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        make_plugin(&plugins_root, "evil-plugin", true, true);

        let roots = vec![(plugins_root.clone(), PluginOrigin::Project)];
        let registry = PluginRegistry::discover_gated(&roots, false);

        assert!(
            registry.is_empty(),
            "untrusted workspace must not discover project-scoped plugins"
        );
        let notice = registry.untrusted_skip_notice().expect("notice present");
        assert!(notice.contains(&plugins_root.display().to_string()));
    }

    #[test]
    fn test_trusted_workspace_discovers_project_plugin_roots() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        make_plugin(&plugins_root, "good-plugin", true, true);

        let roots = vec![(plugins_root, PluginOrigin::Project)];
        let registry = PluginRegistry::discover_gated(&roots, true);

        assert_eq!(registry.len(), 1);
        assert!(registry.untrusted_skip_notice().is_none());
    }

    #[test]
    fn project_manifest_is_not_read_for_an_untrusted_workspace() {
        let workspace = TempDir::new().unwrap();
        let plugins_root = workspace.path().join(".maestro/plugins");
        make_plugin(&plugins_root, "project-plugin", true, true);
        write_file(
            &workspace.path().join(PROJECT_PLUGIN_MANIFEST_PATH),
            "not valid json",
        );
        let roots = vec![(plugins_root, PluginOrigin::Project)];

        let untrusted =
            PluginRegistry::discover_for_workspace_with_roots(workspace.path(), &roots, false);
        assert!(untrusted.is_empty());
        assert!(untrusted.admission_errors().is_empty());

        let trusted =
            PluginRegistry::discover_for_workspace_with_roots(workspace.path(), &roots, true);
        assert!(trusted.is_empty());
        assert_eq!(trusted.admission_errors().len(), 1);
        assert!(trusted.admission_errors()[0].contains("invalid project plugin manifest"));
    }

    #[test]
    fn test_untrusted_workspace_still_discovers_user_plugin_roots() {
        // User-scoped plugin roots (~/.maestro/plugins) aren't
        // repository-controlled, so they must not be gated on workspace trust.
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        make_plugin(&plugins_root, "user-plugin", true, true);

        let roots = vec![(plugins_root, PluginOrigin::User)];
        let registry = PluginRegistry::discover_gated(&roots, false);

        assert_eq!(registry.len(), 1);
        assert!(registry.untrusted_skip_notice().is_none());
    }

    #[test]
    fn one_run_plugin_is_ephemeral_and_overrides_installed_name() {
        let tmp = TempDir::new().unwrap();
        let installed_root = tmp.path().join("installed");
        let one_run = tmp.path().join("one-run");
        make_plugin(&installed_root, "demo", true, true);
        fs::create_dir_all(one_run.join("commands")).unwrap();
        write_file(&one_run.join("plugin.json"), r#"{"name":"demo"}"#);
        let mut registry = PluginRegistry::discover_from(&[(installed_root, PluginOrigin::User)]);

        registry.load_ephemeral_plugin(&one_run).unwrap();

        let plugin = registry.get("demo").unwrap();
        assert_eq!(plugin.origin, PluginOrigin::Ephemeral);
        assert_eq!(plugin.root, one_run);
        assert!(plugin.components.commands_dir.is_some());
        assert!(plugin.components.skills_dir.is_none());
        assert!(!tmp.path().join("plugin-state.json").exists());
    }

    #[test]
    fn discover_with_manifest_and_skills() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        make_plugin(&plugins_root, "team-tools", true, true);

        let registry = PluginRegistry::discover_from(&[(plugins_root, PluginOrigin::Project)]);
        assert_eq!(registry.len(), 1);
        let plugin = registry.get("team-tools").unwrap();
        assert!(plugin.components.skills_dir.is_some());
        assert_eq!(plugin.origin, PluginOrigin::Project);
        assert!(plugin.manifest.is_some());
        assert_eq!(registry.skill_dirs().len(), 1);
    }

    #[test]
    fn discover_portable_manifest_and_reports_its_path() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let plugin = plugins_root.join("portable");
        fs::create_dir_all(plugin.join("skills")).unwrap();
        write_file(
            &plugin.join(PORTABLE_PLUGIN_MANIFEST_PATH),
            r#"{"name":"open-plugin","version":"3.0.0"}"#,
        );

        let registry = PluginRegistry::discover_from(&[(plugins_root, PluginOrigin::User)]);
        let plugin = registry.get("open-plugin").unwrap();
        let expected_manifest_path = plugin.root.join(PORTABLE_PLUGIN_MANIFEST_PATH);
        assert_eq!(
            plugin.manifest_path.as_deref(),
            Some(expected_manifest_path.as_path())
        );
        assert!(
            plugin
                .detail_report()
                .contains("**Manifest:** `.plugin/plugin.json`")
        );
    }

    #[test]
    fn discover_missing_manifest_uses_convention() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        make_plugin(&plugins_root, "bare-plugin", false, true);
        write_file(
            &plugins_root.join("bare-plugin").join("mcp.json"),
            r#"{"mcpServers":{}}"#,
        );

        let registry = PluginRegistry::discover_from(&[(plugins_root, PluginOrigin::User)]);
        let plugin = registry.get("bare-plugin").unwrap();
        assert!(plugin.manifest.is_none());
        assert!(plugin.components.skills_dir.is_some());
        assert!(plugin.components.mcp_path.is_some());
        assert_eq!(plugin.component_summary(), "skills, mcp");
    }

    #[test]
    fn unregistered_plugin_connection_types_stay_disabled() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        make_plugin(&plugins_root, "manual-plugin", true, true);
        write_file(
            &plugins_root.join("manual-plugin").join("connections.json"),
            r#"{"schemaVersion":1,"connectionTypes":[]}"#,
        );

        let registry = PluginRegistry::discover_from(&[(plugins_root, PluginOrigin::User)]);
        let plugin = registry.get("manual-plugin").unwrap();
        assert!(plugin.components.skills_dir.is_some());
        assert!(plugin.components.connections_path.is_none());
        assert!(registry.connection_paths().is_empty());
    }

    #[test]
    fn name_collision_prefers_project_over_user() {
        let tmp = TempDir::new().unwrap();
        let user_root = tmp.path().join("user-plugins");
        let project_root = tmp.path().join("project-plugins");
        make_plugin(&user_root, "shared", true, true);
        // Project copy without skills so we can tell which won via components.
        make_plugin(&project_root, "shared", true, false);
        write_file(
            &project_root.join("shared").join("plugin.json"),
            r#"{"name":"shared","version":"9.9.9","description":"project wins"}"#,
        );

        let roots = [
            (user_root, PluginOrigin::User),
            (project_root.clone(), PluginOrigin::Project),
        ];
        let registry = PluginRegistry::discover_from(&roots);
        assert_eq!(registry.len(), 1);
        let plugin = registry.get("shared").unwrap();
        assert_eq!(plugin.origin, PluginOrigin::Project);
        assert_eq!(
            plugin.manifest.as_ref().and_then(|m| m.version.as_deref()),
            Some("9.9.9")
        );
        assert!(plugin.root.starts_with(&project_root));
        assert!(plugin.components.skills_dir.is_none());
    }

    #[test]
    fn reverse_order_still_prefers_higher_origin_when_sorted_by_insert_order() {
        // discover_from relies on walk order (low → high). If project is last it wins.
        let tmp = TempDir::new().unwrap();
        let legacy = tmp.path().join("legacy");
        let project = tmp.path().join("project");
        make_plugin(&legacy, "x", true, false);
        make_plugin(&project, "x", true, true);

        let registry = PluginRegistry::discover_from(&[
            (legacy, PluginOrigin::LegacyUser),
            (project, PluginOrigin::Project),
        ]);
        let plugin = registry.get("x").unwrap();
        assert_eq!(plugin.origin, PluginOrigin::Project);
        assert!(plugin.components.skills_dir.is_some());
    }

    #[test]
    fn empty_registry_list_report() {
        let registry = PluginRegistry::default();
        let report = registry.list_report();
        assert!(report.contains("No plugins found"));
        assert!(report.contains(".maestro/plugins"));
    }

    #[test]
    fn detail_report_includes_paths() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        make_plugin(&plugins_root, "detail-me", true, true);
        let registry = PluginRegistry::discover_from(&[(plugins_root, PluginOrigin::Project)]);
        let report = registry.get("detail-me").unwrap().detail_report();
        assert!(report.contains("## Plugin: detail-me"));
        assert!(report.contains("**Origin:** project"));
        assert!(report.contains("skills"));
    }

    #[test]
    fn skips_hidden_directories() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        make_plugin(&plugins_root, ".hidden", false, true);
        make_plugin(&plugins_root, "visible", false, true);
        let registry = PluginRegistry::discover_from(&[(plugins_root, PluginOrigin::Project)]);
        assert_eq!(registry.len(), 1);
        assert!(registry.get("visible").is_some());
        assert!(registry.get(".hidden").is_none());
    }

    #[test]
    fn skips_incomplete_untrusted_installs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("plugins");
        let plugin = root.join("incomplete");
        fs::create_dir_all(plugin.join("skills")).unwrap();
        write_file(&plugin.join(".maestro-untrusted"), "incomplete");
        let registry = PluginRegistry::discover_from(&[(root, PluginOrigin::User)]);
        assert!(registry.is_empty());
    }

    #[test]
    fn skill_command_mcp_hook_accessors() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let plugin = plugins_root.join("full");
        fs::create_dir_all(plugin.join("skills")).unwrap();
        fs::create_dir_all(plugin.join("agents")).unwrap();
        fs::create_dir_all(plugin.join("commands")).unwrap();
        write_file(&plugin.join("hooks/hooks.json"), "{}\n");
        write_file(&plugin.join(".mcp.json"), "{}\n");

        let registry = PluginRegistry::discover_from(&[(plugins_root, PluginOrigin::User)]);
        assert_eq!(registry.skill_dirs().len(), 1);
        assert_eq!(registry.agent_dirs().len(), 1);
        assert_eq!(registry.command_dirs().len(), 1);
        assert_eq!(registry.mcp_paths().len(), 1);
        assert_eq!(registry.hook_configs().len(), 1);
    }

    #[test]
    fn disabled_higher_priority_plugin_tombstones_lower_priority_copy() {
        let tmp = TempDir::new().unwrap();
        let legacy_root = tmp.path().join("legacy").join("plugins");
        let native_root = tmp.path().join("native").join("plugins");
        make_plugin(&legacy_root, "duplicate", true, false);
        make_plugin(&native_root, "duplicate", true, false);
        let mut state = manager::PluginState::default();
        state.plugins.insert(
            "duplicate".into(),
            manager::PluginTrustState {
                enabled: false,
                ..Default::default()
            },
        );
        state
            .save(&native_root.parent().unwrap().join("plugin-state.json"))
            .unwrap();

        let registry = PluginRegistry::discover_from(&[
            (legacy_root, PluginOrigin::LegacyUser),
            (native_root, PluginOrigin::User),
        ]);
        let disabled = registry.get("duplicate").unwrap();
        assert_eq!(disabled.origin, PluginOrigin::User);
        assert!(disabled.components.skills_dir.is_none());
        assert!(disabled.components.commands_dir.is_none());
        assert!(disabled.components.hooks_config.is_none());
        assert!(disabled.components.mcp_path.is_none());
    }

    #[test]
    fn workspace_style_discovery_order() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        let user_home = tmp.path().join("home").join(".maestro");
        let legacy_home = tmp.path().join("home").join(".composer");

        make_plugin(&user_home.join("plugins"), "dup", true, false);
        write_file(
            &user_home.join("plugins").join("dup").join("plugin.json"),
            r#"{"name":"dup","version":"user"}"#,
        );
        make_plugin(
            &workspace.join(".maestro").join("plugins"),
            "dup",
            true,
            false,
        );
        write_file(
            &workspace
                .join(".maestro")
                .join("plugins")
                .join("dup")
                .join("plugin.json"),
            r#"{"name":"dup","version":"project"}"#,
        );

        let roots = search_roots_for_workspace(&workspace, Some(&user_home), Some(&legacy_home));
        let registry = PluginRegistry::discover_from(&roots);
        let plugin = registry.get("dup").unwrap();
        assert_eq!(
            plugin.manifest.as_ref().and_then(|m| m.version.as_deref()),
            Some("project")
        );
        assert_eq!(plugin.origin, PluginOrigin::Project);
    }
}
