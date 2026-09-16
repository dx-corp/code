//! Plugin discovery roots and origin priority.

use crate::path_utils::{legacy_composer_home_dir, maestro_home_dir};
use std::path::PathBuf;

/// Where a plugin was discovered from.
///
/// Higher-priority origins replace lower-priority plugins with the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginOrigin {
    /// Legacy user plugins: `~/.composer/plugins/*`
    LegacyUser = 0,
    /// Legacy project plugins: `.composer/plugins/*`
    LegacyProject = 1,
    /// User plugins: `~/.maestro/plugins/*`
    User = 2,
    /// Project plugins: `.maestro/plugins/*`
    Project = 3,
    /// Explicit one-run plugin loaded from `--plugin`.
    Ephemeral = 4,
}

impl PluginOrigin {
    /// Human-readable label for UI/diagnostics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LegacyUser => "legacy-user",
            Self::LegacyProject => "legacy-project",
            Self::User => "user",
            Self::Project => "project",
            Self::Ephemeral => "ephemeral",
        }
    }

    /// Numeric priority (higher wins on name collision).
    #[must_use]
    pub fn priority(self) -> u8 {
        self as u8
    }

    /// Returns `true` for origins rooted under the workspace (repository)
    /// rather than the user's home directory.
    ///
    /// Project-scoped roots are repository-controlled: a plugin found there
    /// can contribute skills (and, once wired, hooks/MCP configs) that run
    /// automatically. Callers must gate these on workspace trust; see
    /// `PluginRegistry::discover`.
    #[must_use]
    pub fn is_project_scoped(self) -> bool {
        matches!(self, Self::LegacyProject | Self::Project)
    }
}

/// Default discovery roots ordered low → high priority.
///
/// Discovery walks roots in this order so later entries overwrite earlier ones
/// when plugin names collide (project beats user, Maestro beats composer).
#[must_use]
pub fn default_search_roots() -> Vec<(PathBuf, PluginOrigin)> {
    let mut roots = Vec::new();

    if let Some(home) = legacy_composer_home_dir() {
        roots.push((home.join("plugins"), PluginOrigin::LegacyUser));
    }
    roots.push((
        PathBuf::from(".composer").join("plugins"),
        PluginOrigin::LegacyProject,
    ));

    if let Some(home) = maestro_home_dir() {
        roots.push((home.join("plugins"), PluginOrigin::User));
    }
    roots.push((
        PathBuf::from(".maestro").join("plugins"),
        PluginOrigin::Project,
    ));

    roots
}

/// Build absolute roots under a workspace directory (for tests / custom CWD).
#[must_use]
pub fn search_roots_for_workspace(
    workspace: &std::path::Path,
    user_home: Option<&std::path::Path>,
    legacy_home: Option<&std::path::Path>,
) -> Vec<(PathBuf, PluginOrigin)> {
    let mut roots = Vec::new();
    if let Some(home) = legacy_home {
        roots.push((home.join("plugins"), PluginOrigin::LegacyUser));
    }
    roots.push((
        workspace.join(".composer").join("plugins"),
        PluginOrigin::LegacyProject,
    ));
    if let Some(home) = user_home {
        roots.push((home.join("plugins"), PluginOrigin::User));
    }
    roots.push((
        workspace.join(".maestro").join("plugins"),
        PluginOrigin::Project,
    ));
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn origin_priority_project_beats_user() {
        assert!(PluginOrigin::Project.priority() > PluginOrigin::User.priority());
        assert!(PluginOrigin::User.priority() > PluginOrigin::LegacyProject.priority());
        assert!(PluginOrigin::LegacyProject.priority() > PluginOrigin::LegacyUser.priority());
    }

    #[test]
    fn workspace_roots_order_low_to_high() {
        let roots = search_roots_for_workspace(
            Path::new("/ws"),
            Some(Path::new("/user-home/.maestro")),
            Some(Path::new("/user-home/.composer")),
        );
        assert_eq!(roots.len(), 4);
        assert_eq!(roots[0].1, PluginOrigin::LegacyUser);
        assert_eq!(roots[1].1, PluginOrigin::LegacyProject);
        assert_eq!(roots[2].1, PluginOrigin::User);
        assert_eq!(roots[3].1, PluginOrigin::Project);
        assert_eq!(roots[3].0, PathBuf::from("/ws/.maestro/plugins"));
    }
}
