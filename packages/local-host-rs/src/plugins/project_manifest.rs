//! Trusted project plugin admission and resource filtering.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use glob::Pattern;
use serde::{Deserialize, Serialize};

use super::{
    DiscoveredPlugin, PluginCapability, PluginOrigin, package_integrity, read_plugin_file,
};

pub const PROJECT_PLUGIN_MANIFEST_PATH: &str = ".maestro/plugins.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPluginManifest {
    pub schema_version: u32,
    #[serde(default)]
    pub plugins: Vec<ProjectPluginDeclaration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPluginDeclaration {
    pub name: String,
    pub source: ProjectPluginSource,
    pub integrity: String,
    pub scope: ProjectPluginScope,
    #[serde(default)]
    pub resources: PluginResourceFilters,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ProjectPluginSource {
    Path { path: PathBuf },
    Git { url: String, commit: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectPluginScope {
    Project,
    User,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PluginResourceFilters {
    pub skills: ResourceFilter,
    pub agents: ResourceFilter,
    pub commands: ResourceFilter,
    pub hooks: ResourceFilter,
    pub mcp: ResourceFilter,
    pub connections: ResourceFilter,
}

impl PluginResourceFilters {
    fn get(&self, capability: PluginCapability) -> &ResourceFilter {
        match capability {
            PluginCapability::Skills => &self.skills,
            PluginCapability::Agents => &self.agents,
            PluginCapability::Commands => &self.commands,
            PluginCapability::Hooks => &self.hooks,
            PluginCapability::Mcp => &self.mcp,
            PluginCapability::Connections => &self.connections,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResourceFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl ResourceFilter {
    fn validate(&self) -> Result<()> {
        for pattern in self.include.iter().chain(&self.exclude) {
            if pattern.is_empty() || Path::new(pattern).is_absolute() {
                bail!("plugin resource filters must be non-empty relative patterns");
            }
            Pattern::new(pattern)
                .with_context(|| format!("invalid plugin resource filter: {pattern}"))?;
        }
        Ok(())
    }

    pub(super) fn is_active(&self) -> bool {
        !self.include.is_empty() || !self.exclude.is_empty()
    }

    pub(super) fn allows(&self, relative: &Path) -> bool {
        let relative = relative.to_string_lossy().replace('\\', "/");
        let basename = relative.rsplit('/').next().unwrap_or(&relative);
        let matches = |pattern: &str| {
            Pattern::new(pattern)
                .is_ok_and(|pattern| pattern.matches(&relative) || pattern.matches(basename))
        };
        (self.include.is_empty() || self.include.iter().any(|pattern| matches(pattern)))
            && !self.exclude.iter().any(|pattern| matches(pattern))
    }
}

pub(super) fn apply_project_manifest(
    workspace: &Path,
    plugins: &mut [DiscoveredPlugin],
) -> Result<()> {
    let path = workspace.join(PROJECT_PLUGIN_MANIFEST_PATH);
    let Some(text) = read_plugin_file(&path) else {
        if path.exists() {
            bail!("project plugin manifest is not a safe regular file");
        }
        return Ok(());
    };
    let manifest: ProjectPluginManifest =
        serde_json::from_str(&text).context("invalid project plugin manifest")?;
    if manifest.schema_version != 1 {
        bail!("unsupported project plugin manifest schema version");
    }
    let mut names = BTreeSet::new();
    for declaration in manifest.plugins {
        let key = declaration.name.to_lowercase();
        if !names.insert(key.clone()) {
            bail!("duplicate project plugin declaration: {}", declaration.name);
        }
        for capability in [
            PluginCapability::Skills,
            PluginCapability::Agents,
            PluginCapability::Commands,
            PluginCapability::Hooks,
            PluginCapability::Mcp,
            PluginCapability::Connections,
        ] {
            declaration.resources.get(capability).validate()?;
        }
        let plugin = plugins
            .iter_mut()
            .find(|plugin| plugin.name.eq_ignore_ascii_case(&declaration.name))
            .with_context(|| {
                format!(
                    "declared project plugin is not installed: {}",
                    declaration.name
                )
            })?;
        super::manager::validate_tree(&plugin.root)?;
        validate_scope(plugin, declaration.scope)?;
        validate_source(workspace, plugin, &declaration.source)?;
        let expected_integrity = declaration.integrity.to_ascii_lowercase();
        if expected_integrity.len() != 71
            || !expected_integrity.starts_with("sha256:")
            || !expected_integrity[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("plugin integrity must be sha256 followed by 64 lowercase hex digits");
        }
        let actual_integrity = package_integrity(&plugin.root)?;
        if actual_integrity != expected_integrity {
            bail!(
                "plugin integrity mismatch for {}: expected {}, found {actual_integrity}",
                declaration.name,
                declaration.integrity
            );
        }
        apply_filters(plugin, &declaration.resources);
    }
    Ok(())
}

fn validate_scope(plugin: &DiscoveredPlugin, scope: ProjectPluginScope) -> Result<()> {
    let valid = match scope {
        ProjectPluginScope::Project => plugin.origin.is_project_scoped(),
        ProjectPluginScope::User => {
            matches!(plugin.origin, PluginOrigin::User | PluginOrigin::LegacyUser)
        }
    };
    if !valid {
        bail!("plugin {} does not match its declared scope", plugin.name);
    }
    Ok(())
}

fn validate_source(
    workspace: &Path,
    plugin: &DiscoveredPlugin,
    source: &ProjectPluginSource,
) -> Result<()> {
    match source {
        ProjectPluginSource::Path { path } => {
            if path.is_absolute()
                || path.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                bail!("project plugin paths must stay within the trusted workspace");
            }
            let expected = dunce::canonicalize(workspace.join(path))?;
            let actual = dunce::canonicalize(&plugin.root)?;
            if expected != actual {
                bail!(
                    "plugin {} source path does not match its installed tree",
                    plugin.name
                );
            }
        }
        ProjectPluginSource::Git { url, commit } => {
            if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("project git plugin references must use a full 40-hex commit");
            }
            let state = plugin
                .trust_state
                .as_ref()
                .context("project git plugin is missing managed installation metadata")?;
            let installed_url = state
                .trusted_source
                .split_once('#')
                .map_or(state.trusted_source.as_str(), |(url, _)| url);
            if installed_url != url
                || state.installed_commit.as_deref() != Some(&commit.to_ascii_lowercase())
            {
                bail!(
                    "plugin {} immutable git coordinates do not match",
                    plugin.name
                );
            }
        }
    }
    Ok(())
}

fn apply_filters(plugin: &mut DiscoveredPlugin, filters: &PluginResourceFilters) {
    plugin.resource_filters = filters.clone();
    fn allowed(root: &Path, path: &Path, filter: &ResourceFilter) -> bool {
        path.strip_prefix(root)
            .is_ok_and(|relative| filter.allows(relative))
    }
    if plugin
        .components
        .skills_dir
        .as_deref()
        .is_some_and(|path| !allowed(&plugin.root, path, &filters.skills))
    {
        plugin.components.skills_dir = None;
    }
    if plugin
        .components
        .agents_dir
        .as_deref()
        .is_some_and(|path| !allowed(&plugin.root, path, &filters.agents))
    {
        plugin.components.agents_dir = None;
    }
    if plugin
        .components
        .commands_dir
        .as_deref()
        .is_some_and(|path| !allowed(&plugin.root, path, &filters.commands))
    {
        plugin.components.commands_dir = None;
    }
    if plugin
        .components
        .hooks_config
        .as_deref()
        .is_some_and(|path| !allowed(&plugin.root, path, &filters.hooks))
    {
        plugin.components.hooks_config = None;
    }
    if plugin
        .components
        .mcp_path
        .as_deref()
        .is_some_and(|path| !allowed(&plugin.root, path, &filters.mcp))
    {
        plugin.components.mcp_path = None;
    }
    if plugin
        .components
        .connections_path
        .as_deref()
        .is_some_and(|path| !allowed(&plugin.root, path, &filters.connections))
    {
        plugin.components.connections_path = None;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::plugins::{PluginRegistry, package_integrity};

    fn project_fixture() -> (TempDir, PluginRegistry, String) {
        let workspace = TempDir::new().unwrap();
        let root = workspace.path().join(".maestro/plugins/demo");
        fs::create_dir_all(root.join("skills")).unwrap();
        fs::create_dir_all(root.join("commands")).unwrap();
        fs::write(root.join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        let integrity = package_integrity(&root).unwrap();
        let registry = PluginRegistry::discover_from(&[(
            workspace.path().join(".maestro/plugins"),
            PluginOrigin::Project,
        )]);
        (workspace, registry, integrity)
    }

    #[test]
    fn trusted_project_manifest_verifies_integrity_and_applies_filters() {
        let (workspace, mut registry, integrity) = project_fixture();
        fs::write(
            workspace.path().join(PROJECT_PLUGIN_MANIFEST_PATH),
            format!(
                r#"{{"schemaVersion":1,"plugins":[{{"name":"demo","source":{{"kind":"path","path":".maestro/plugins/demo"}},"integrity":"{integrity}","scope":"project","resources":{{"commands":{{"exclude":["commands"]}}}}}}]}}"#
            ),
        )
        .unwrap();

        apply_project_manifest(workspace.path(), &mut registry.plugins).unwrap();

        let plugin = registry.get("demo").unwrap();
        assert!(plugin.components.skills_dir.is_some());
        assert!(plugin.components.commands_dir.is_none());
    }

    #[test]
    fn integrity_mismatch_is_rejected() {
        let (workspace, mut registry, _) = project_fixture();
        fs::write(
            workspace.path().join(PROJECT_PLUGIN_MANIFEST_PATH),
            r#"{"schemaVersion":1,"plugins":[{"name":"demo","source":{"kind":"path","path":".maestro/plugins/demo"},"integrity":"sha256:0000000000000000000000000000000000000000000000000000000000000000","scope":"project"}]}"#,
        )
        .unwrap();

        let error = apply_project_manifest(workspace.path(), &mut registry.plugins).unwrap_err();
        assert!(
            error.to_string().contains("integrity mismatch"),
            "{error:#}"
        );
    }

    #[test]
    fn resource_filters_narrow_individual_plugin_resources() {
        let workspace = TempDir::new().unwrap();
        let root = workspace.path().join(".maestro/plugins/demo");
        for skill in ["keep", "legacy"] {
            let dir = root.join("skills").join(skill);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {skill}\ndescription: test\n---\n"),
            )
            .unwrap();
        }
        fs::create_dir_all(root.join("agents")).unwrap();
        fs::create_dir_all(root.join("commands")).unwrap();
        fs::write(root.join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        fs::write(root.join("agents/keep.md"), "keep").unwrap();
        fs::write(root.join("agents/legacy.md"), "legacy").unwrap();
        fs::write(root.join("commands/keep.md"), "keep").unwrap();
        fs::write(root.join("commands/legacy.md"), "legacy").unwrap();
        let integrity = package_integrity(&root).unwrap();
        let mut registry = PluginRegistry::discover_from(&[(
            workspace.path().join(".maestro/plugins"),
            PluginOrigin::Project,
        )]);
        fs::write(
            workspace.path().join(PROJECT_PLUGIN_MANIFEST_PATH),
            format!(
                r#"{{"schemaVersion":1,"plugins":[{{"name":"demo","source":{{"kind":"path","path":".maestro/plugins/demo"}},"integrity":"{integrity}","scope":"project","resources":{{"skills":{{"exclude":["skills/legacy"]}},"agents":{{"exclude":["agents/legacy.md"]}},"commands":{{"exclude":["commands/legacy.md"]}}}}}}]}}"#
            ),
        )
        .unwrap();

        apply_project_manifest(workspace.path(), &mut registry.plugins).unwrap();

        assert_eq!(registry.skill_dirs(), vec![root.join("skills/keep")]);
        assert_eq!(registry.agent_paths(), vec![root.join("agents/keep.md")]);
        assert_eq!(
            registry.command_paths(),
            vec![root.join("commands/keep.md")]
        );
    }
}
