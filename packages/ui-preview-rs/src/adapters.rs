//! Compiled ownership manifests and transaction-like story promotion.
use crate::{
    authoring::{MenuRecipe, validate_id},
    schema::migrate_recipe,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    fs::OpenOptions,
    io::Write,
    path::{Component, Path, PathBuf},
};

pub const REGISTRATION_START: &str = "// maestro-ui-stories:start";
pub const REGISTRATION_END: &str = "// maestro-ui-stories:end";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoryTemplate {
    MenuRecipe,
    ThemeSelector,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoryKind {
    Static,
    Replay,
    Menu,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterVerifier {
    UiPreview,
    OnboardingPreview,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum StoryLifecycle {
    Active,
    Deprecated { replacement: String },
    Retired { reason: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterManifest {
    pub id: String,
    pub owner: String,
    pub fixture_dir: PathBuf,
    pub registration_file: PathBuf,
    pub template: StoryTemplate,
    pub story_kinds: Vec<StoryKind>,
    pub profiles: Vec<String>,
    pub lifecycle: StoryLifecycle,
    pub verifier: AdapterVerifier,
    pub authoring: bool,
}
impl AdapterManifest {
    #[must_use]
    pub fn active(
        id: &str,
        owner: &str,
        fixture_dir: impl Into<PathBuf>,
        registration_file: impl Into<PathBuf>,
        template: StoryTemplate,
    ) -> Self {
        Self {
            id: id.into(),
            owner: owner.into(),
            fixture_dir: fixture_dir.into(),
            registration_file: registration_file.into(),
            template,
            story_kinds: vec![StoryKind::Static, StoryKind::Replay],
            profiles: vec!["pr-v1".into(), "scheduled-v1".into()],
            lifecycle: StoryLifecycle::Active,
            verifier: AdapterVerifier::UiPreview,
            authoring: true,
        }
    }
    pub fn validate(&self, repo_root: &Path) -> Result<(), String> {
        validate_id(&self.id)?;
        validate_id(&self.owner).map_err(|_| "adapter owner must be a stable ID".to_owned())?;
        validate_repo_path(repo_root, &self.fixture_dir, "adapter fixture path")?;
        validate_repo_path(
            repo_root,
            &self.registration_file,
            "adapter registration path",
        )?;
        if self.story_kinds.is_empty() {
            return Err("adapter needs a story kind".into());
        }
        if self.profiles.is_empty() {
            return Err("adapter needs a coverage profile".into());
        }
        for profile in &self.profiles {
            validate_id(profile)?;
            if !matches!(profile.as_str(), "pr-v1" | "scheduled-v1") {
                return Err(format!("unsupported coverage profile: {profile}"));
            }
        }
        match &self.lifecycle {
            StoryLifecycle::Deprecated { replacement } => validate_id(replacement)?,
            StoryLifecycle::Retired { reason } if reason.trim().is_empty() => {
                return Err("retired adapter needs a reason".into());
            }
            StoryLifecycle::Active | StoryLifecycle::Retired { .. } => {}
        }
        Ok(())
    }
}

fn validate_repo_path(root: &Path, path: &Path, label: &str) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!("{label} escapes repository"));
    }
    let canonical_root =
        dunce::canonicalize(root).map_err(|error| format!("canonicalize repository: {error}"))?;
    let mut existing = root.join(path);
    while !existing.exists() {
        if !existing.pop() || existing == root.parent().unwrap_or(root) {
            return Err(format!("{label} escapes repository"));
        }
    }
    let canonical_existing =
        dunce::canonicalize(&existing).map_err(|error| format!("canonicalize {label}: {error}"))?;
    if !canonical_existing.starts_with(&canonical_root) {
        return Err(format!("{label} escapes repository"));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct AdapterRegistry {
    manifests: BTreeMap<String, AdapterManifest>,
}
impl AdapterRegistry {
    pub fn new(manifests: impl IntoIterator<Item = AdapterManifest>) -> Result<Self, String> {
        let mut by_id = BTreeMap::new();
        for manifest in manifests {
            let id = manifest.id.clone();
            if by_id.insert(id.clone(), manifest).is_some() {
                return Err(format!("duplicate adapter: {id}"));
            }
        }
        Ok(Self { manifests: by_id })
    }
    pub fn builtins() -> Result<Self, String> {
        Self::new([
            AdapterManifest {
                authoring: false,
                profiles: vec!["scheduled-v1".into()],
                ..AdapterManifest::active(
                    "legacy",
                    "maestro-presentation",
                    "products/maestro/packages/ui-preview-rs/src",
                    "products/maestro/packages/ui-preview-rs/src/lib.rs",
                    StoryTemplate::ThemeSelector,
                )
            },
            AdapterManifest {
                story_kinds: vec![StoryKind::Static, StoryKind::Replay, StoryKind::Menu],
                ..AdapterManifest::active(
                    "shared-menu",
                    "maestro-ui",
                    "products/maestro/packages/ui-preview-rs/src/stories",
                    "products/maestro/packages/ui-preview-rs/src/stories/mod.rs",
                    StoryTemplate::MenuRecipe,
                )
            },
            AdapterManifest {
                story_kinds: vec![StoryKind::Static, StoryKind::Replay],
                verifier: AdapterVerifier::OnboardingPreview,
                ..AdapterManifest::active(
                    "theme-selector",
                    "maestro-tui",
                    "products/maestro/packages/tui-rs/examples/support/ui_stories",
                    "products/maestro/packages/tui-rs/examples/support/ui_stories.rs",
                    StoryTemplate::ThemeSelector,
                )
            },
        ])
    }
    pub fn get(&self, id: &str) -> Result<&AdapterManifest, String> {
        self.manifests
            .get(id)
            .ok_or_else(|| format!("unknown adapter: {id}"))
    }
    #[must_use]
    pub fn manifests(&self) -> Vec<&AdapterManifest> {
        self.manifests.values().collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PromotionPlan {
    pub adapter: String,
    pub story_id: String,
    pub fixture: PathBuf,
    pub registration: PathBuf,
    pub writes: Vec<PathBuf>,
}

pub fn plan_new(
    root: &Path,
    adapter: &AdapterManifest,
    story_id: &str,
) -> Result<PromotionPlan, String> {
    if !adapter.authoring {
        return Err(format!("adapter {} does not support authoring", adapter.id));
    }
    adapter.validate(root)?;
    validate_id(story_id)?;
    let fixture = adapter
        .fixture_dir
        .join(format!("{}.rs", story_id.replace('-', "_")));
    validate_repo_path(root, &fixture, "story fixture path")?;
    Ok(PromotionPlan {
        adapter: adapter.id.clone(),
        story_id: story_id.into(),
        fixture: fixture.clone(),
        registration: adapter.registration_file.clone(),
        writes: vec![fixture, adapter.registration_file.clone()],
    })
}

fn template_source(adapter: &AdapterManifest, story_id: &str) -> Result<String, String> {
    match adapter.template {
        StoryTemplate::MenuRecipe => {
            let mut recipe = MenuRecipe::starter();
            recipe.id = story_id.into();
            recipe.label = story_id.replace('-', " ");
            recipe.fixture_source_for_adapter(&adapter.id)
        }
        StoryTemplate::ThemeSelector => Ok(format!(
            r#"//! Generated ThemeSelector fixture using the production controller and renderer.
use maestro_tui::components::ThemeSelector;
use maestro_ui_preview::{{Scene, review::{{self, Capture}}}};

pub fn captures() -> Result<Vec<Capture>, String> {{
    let mut selector = ThemeSelector::with_themes(vec!["auto".into(), "dark".into(), "light".into()])
        .map_err(|error| error.to_string())?;
    selector.show();
    let scene = Scene {{ id: {story_id:?}.into(), label: {story_id:?}.replace('-', " "), width: 72, height: 22, time_ms: 0 }};
    let mut capture = review::capture(scene, |frame| selector.render(frame, frame.area()))?;
    capture.source = file!().into();
    Ok(vec![capture])
}}
"#
        )),
    }
}

fn registration_with_story(before: &str, module: &str) -> Result<String, String> {
    let start = before
        .find(REGISTRATION_START)
        .ok_or("registration file is missing start marker")?;
    let end = before
        .find(REGISTRATION_END)
        .ok_or("registration file is missing end marker")?;
    if start >= end {
        return Err("registration markers are out of order".into());
    }
    let body_start = start + REGISTRATION_START.len();
    let body = &before[body_start..end];
    let line = format!("\n    {module},\n");
    if body
        .lines()
        .any(|existing| existing.trim() == format!("{module},"))
    {
        return Err(format!("story is already registered: {module}"));
    }
    Ok(format!(
        "{}{}{}{}",
        &before[..body_start],
        body,
        line,
        &before[end..]
    ))
}

fn registration_without_story(current: &str, module: &str) -> Result<String, String> {
    let owned = format!("    {module},");
    let mut removed = false;
    let mut out = String::with_capacity(current.len());
    for line in current.lines() {
        if !removed && line == owned {
            removed = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if removed {
        Ok(out)
    } else {
        Err(format!(
            "story registration disappeared during rollback: {module}"
        ))
    }
}

fn create_new_file(path: &Path, source: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(source.as_bytes())
        .map_err(|error| error.to_string())
}

fn write_registration_if_unchanged(path: &Path, before: &str, after: &str) -> Result<(), String> {
    let current = fs::read_to_string(path).map_err(|error| error.to_string())?;
    if current != before {
        return Err("registration changed concurrently".into());
    }
    fs::write(path, after).map_err(|error| error.to_string())
}

pub fn create_story(
    root: &Path,
    adapter: &AdapterManifest,
    story_id: &str,
    check: bool,
) -> Result<PromotionPlan, String> {
    let plan = plan_new(root, adapter, story_id)?;
    let fixture = root.join(&plan.fixture);
    let registration = root.join(&plan.registration);
    if fixture.exists() {
        return Err(format!(
            "story fixture already exists: {}",
            plan.fixture.display()
        ));
    }
    let before = fs::read_to_string(&registration)
        .map_err(|error| format!("read {}: {error}", plan.registration.display()))?;
    let module = story_id.replace('-', "_");
    let after = registration_with_story(&before, &module)?;
    let source = template_source(adapter, story_id)?;
    if check {
        return Ok(plan);
    }
    if let Some(parent) = fixture.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    create_new_file(&fixture, &source)?;
    if let Err(error) = write_registration_if_unchanged(&registration, &before, &after) {
        let _ = fs::remove_file(&fixture);
        return Err(error.to_string());
    }
    Ok(plan)
}

pub fn promote_recipe(
    root: &Path,
    adapter: &AdapterManifest,
    recipe_path: &Path,
    check: bool,
    verify: impl FnOnce(&PromotionPlan) -> Result<(), String>,
) -> Result<PromotionPlan, String> {
    validate_repo_path(root, recipe_path, "recipe path")?;
    let bytes = fs::read(root.join(recipe_path)).map_err(|error| error.to_string())?;
    let value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let recipe: MenuRecipe = migrate_recipe(value)
        .map_err(|error| error.to_string())?
        .value;
    recipe.validate()?;
    let plan = plan_new(root, adapter, &recipe.id)?;
    let fixture = root.join(&plan.fixture);
    let registration = root.join(&plan.registration);
    if fixture.exists() {
        return Err(format!(
            "story fixture already exists: {}",
            plan.fixture.display()
        ));
    }
    let before = fs::read_to_string(&registration).map_err(|error| error.to_string())?;
    let after = registration_with_story(&before, &recipe.id.replace('-', "_"))?;
    let source = recipe.fixture_source_for_adapter(&adapter.id)?;
    if check {
        return Ok(plan);
    }
    if let Some(parent) = fixture.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    create_new_file(&fixture, &source)?;
    if let Err(error) = write_registration_if_unchanged(&registration, &before, &after) {
        let _ = fs::remove_file(&fixture);
        return Err(error.to_string());
    }
    if let Err(error) = verify(&plan) {
        let _ = fs::remove_file(&fixture);
        let current = fs::read_to_string(&registration)
            .map_err(|restore| format!("{error}; read for restore failed: {restore}"))?;
        let restored = registration_without_story(&current, &recipe.id.replace('-', "_"))
            .map_err(|restore| format!("{error}; restore failed: {restore}"))?;
        fs::write(&registration, restored)
            .map_err(|restore| format!("{error}; restore failed: {restore}"))?;
        return Err(error);
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "maestro-adapter-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("fixtures")).unwrap();
        fs::write(
            root.join("register.rs"),
            format!("before\n{REGISTRATION_START}\n{REGISTRATION_END}\nafter\n"),
        )
        .unwrap();
        root
    }
    fn fixture_adapter(id: &str) -> AdapterManifest {
        AdapterManifest::active(
            id,
            "maestro-ui",
            "fixtures",
            "register.rs",
            StoryTemplate::MenuRecipe,
        )
    }
    #[test]
    fn adapter_registry_rejects_ambiguous_and_escaping_paths() {
        assert_eq!(
            AdapterRegistry::new([fixture_adapter("menu"), fixture_adapter("menu")]).unwrap_err(),
            "duplicate adapter: menu"
        );
        let mut invalid = fixture_adapter("menu");
        invalid.fixture_dir = PathBuf::from("../outside");
        assert_eq!(
            invalid.validate(Path::new("repo")).unwrap_err(),
            "adapter fixture path escapes repository"
        );
    }
    #[test]
    fn create_is_dry_runnable_and_never_overwrites() {
        let root = temp_root();
        let adapter = fixture_adapter("menu");
        let plan = create_story(&root, &adapter, "workspace-picker", true).unwrap();
        assert!(!root.join(&plan.fixture).exists());
        create_story(&root, &adapter, "workspace-picker", false).unwrap();
        assert!(root.join(&plan.fixture).exists());
        assert!(
            fs::read_to_string(root.join("register.rs"))
                .unwrap()
                .contains("workspace_picker,")
        );
        assert!(
            fs::read_to_string(root.join(&plan.fixture))
                .unwrap()
                .contains("pub fn register(registry: &mut Registry)")
        );
        assert!(create_story(&root, &adapter, "workspace-picker", false).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn promotion_restores_owned_writes_when_verification_fails() {
        let root = temp_root();
        let adapter = fixture_adapter("menu");
        let recipe = MenuRecipe::starter();
        let recipe_path = PathBuf::from("recipe.json");
        fs::write(
            root.join(&recipe_path),
            serde_json::to_vec(&recipe).unwrap(),
        )
        .unwrap();
        let registration = root.join("register.rs");
        let error = promote_recipe(&root, &adapter, &recipe_path, false, |_| {
            let mut current = fs::read_to_string(&registration).unwrap();
            current.push_str("concurrent unrelated edit\n");
            fs::write(&registration, current).unwrap();
            Err("focused verification failed".into())
        })
        .unwrap_err();
        assert_eq!(error, "focused verification failed");
        assert!(!root.join("fixtures/new_menu.rs").exists());
        let after = fs::read_to_string(root.join("register.rs")).unwrap();
        assert!(after.contains("before\n"));
        assert!(after.contains("after\n"));
        assert!(after.ends_with("concurrent unrelated edit\n"));
        assert!(!after.contains("new_menu,"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn promotion_validates_before_mutation_and_preserves_unrelated_registration() {
        let root = temp_root();
        let adapter = fixture_adapter("menu");
        let recipe_path = PathBuf::from("recipe.json");
        let before = fs::read_to_string(root.join("register.rs")).unwrap();
        fs::write(root.join(&recipe_path), b"{\"version\":99}").unwrap();
        assert!(promote_recipe(&root, &adapter, &recipe_path, false, |_| Ok(())).is_err());
        assert_eq!(
            fs::read_to_string(root.join("register.rs")).unwrap(),
            before
        );
        assert!(!root.join("fixtures/new_menu.rs").exists());

        let recipe = MenuRecipe::starter();
        fs::write(
            root.join(&recipe_path),
            serde_json::to_vec(&recipe).unwrap(),
        )
        .unwrap();
        let plan = promote_recipe(&root, &adapter, &recipe_path, false, |_| Ok(())).unwrap();
        assert!(root.join(plan.fixture).exists());
        let after = fs::read_to_string(root.join("register.rs")).unwrap();
        assert!(after.starts_with("before\n"));
        assert!(after.ends_with("after\n"));
        assert!(after.contains("new_menu,"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn profiles_are_unique_and_bounded() {
        let registry = AdapterRegistry::builtins().unwrap();
        let ids: std::collections::BTreeSet<_> = registry
            .manifests()
            .iter()
            .map(|manifest| manifest.id.as_str())
            .collect();
        assert_eq!(
            ids,
            std::collections::BTreeSet::from(["legacy", "shared-menu", "theme-selector"])
        );
        assert!(!registry.get("legacy").unwrap().authoring);
        assert!(
            plan_new(Path::new("."), registry.get("legacy").unwrap(), "new-story")
                .unwrap_err()
                .contains("does not support authoring")
        );
        let root = temp_root();
        let mut invalid = fixture_adapter("menu");
        invalid.profiles = vec!["unknown-profile".into()];
        assert!(invalid.validate(&root).unwrap_err().contains("unsupported"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typed_verifier_and_theme_template_are_adapter_owned() {
        let registry = AdapterRegistry::builtins().unwrap();
        let theme = registry.get("theme-selector").unwrap();
        assert_eq!(theme.verifier, AdapterVerifier::OnboardingPreview);
        assert_eq!(
            theme.fixture_dir,
            PathBuf::from("products/maestro/packages/tui-rs/examples/support/ui_stories")
        );
        let source = template_source(theme, "workspace-picker").unwrap();
        assert!(source.contains("ThemeSelector::with_themes"));
        assert!(source.contains("pub fn captures()"));
    }

    #[test]
    fn create_new_file_closes_the_overwrite_race() {
        let root = temp_root();
        let path = root.join("fixtures/existing.rs");
        fs::write(&path, "owned by another writer").unwrap();
        assert!(create_new_file(&path, "replacement").is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "owned by another writer"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn conditional_registration_write_refuses_concurrent_edits() {
        let root = temp_root();
        let path = root.join("register.rs");
        let before = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("{before}concurrent\n")).unwrap();
        assert_eq!(
            write_registration_if_unchanged(&path, &before, "replacement").unwrap_err(),
            "registration changed concurrently"
        );
        assert!(fs::read_to_string(&path).unwrap().ends_with("concurrent\n"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn canonical_path_validation_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let root = temp_root();
        let outside = std::env::temp_dir().join(format!("maestro-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("linked")).unwrap();
        let mut adapter = fixture_adapter("menu");
        adapter.fixture_dir = PathBuf::from("linked");
        assert_eq!(
            adapter.validate(&root).unwrap_err(),
            "adapter fixture path escapes repository"
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}
