//! Versioned wire formats, compatibility migrations, aliases, and coverage profiles.
use crate::{
    Scene,
    adapters::AdapterManifest,
    authoring::{MenuRecipe, StorySequence, validate_dimensions, validate_id},
    contract::StoryContract,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

pub const RECIPE_SCHEMA: &str = "maestro.ui.menu-recipe";
pub const REPLAY_SCHEMA: &str = "maestro.ui.replay";
pub const CONTRACT_SCHEMA: &str = "maestro.ui.story-contract";
pub const THEME_REPLAY_SCHEMA: &str = "maestro.ui.theme-replay";
pub const RECIPE_SCHEMA_VERSION: u16 = 1;
pub const REPLAY_SCHEMA_VERSION: u16 = 1;
pub const CONTRACT_SCHEMA_VERSION: u16 = 1;
pub const THEME_REPLAY_SCHEMA_VERSION: u16 = 1;
pub const OBSERVATION_SCHEMA_VERSION: u16 = 1;
pub const CAPTURE_SCHEMA_VERSION: u16 = 1;
pub const PROFILE_SCHEMA_VERSION: u16 = 1;
const MAX_PROFILE_GEOMETRIES: usize = 16;
const MAX_PROFILE_TEXT_BYTES: usize = 240;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireEnvelope<T> {
    pub schema: String,
    pub version: u16,
    pub value: T,
}

impl<T> WireEnvelope<T> {
    #[must_use]
    pub fn new(schema: &str, version: u16, value: T) -> Self {
        Self {
            schema: schema.into(),
            version,
            value,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaError {
    code: &'static str,
    detail: String,
}

impl SchemaError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}
impl std::error::Error for SchemaError {}

fn migrate<T: DeserializeOwned>(
    value: Value,
    schema: &'static str,
    version: u16,
    unsupported: &'static str,
) -> Result<WireEnvelope<T>, SchemaError> {
    let is_envelope = value
        .as_object()
        .is_some_and(|object| object.contains_key("schema"));
    if is_envelope {
        let envelope: WireEnvelope<Value> = serde_json::from_value(value)
            .map_err(|error| SchemaError::new("invalid-wire-envelope", error.to_string()))?;
        if envelope.schema != schema {
            return Err(SchemaError::new("unexpected-schema", envelope.schema));
        }
        if envelope.version != version {
            return Err(SchemaError::new(unsupported, envelope.version.to_string()));
        }
        let decoded = serde_json::from_value(envelope.value)
            .map_err(|error| SchemaError::new("invalid-schema-value", error.to_string()))?;
        Ok(WireEnvelope::new(schema, version, decoded))
    } else {
        let raw_version = value
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| SchemaError::new("missing-schema-version", schema))?;
        if raw_version != u64::from(version) {
            return Err(SchemaError::new(unsupported, raw_version.to_string()));
        }
        let decoded = serde_json::from_value(value)
            .map_err(|error| SchemaError::new("invalid-schema-value", error.to_string()))?;
        Ok(WireEnvelope::new(schema, version, decoded))
    }
}

pub fn migrate_recipe(value: Value) -> Result<WireEnvelope<MenuRecipe>, SchemaError> {
    migrate(
        value,
        RECIPE_SCHEMA,
        RECIPE_SCHEMA_VERSION,
        "unsupported-recipe-version",
    )
}
pub fn migrate_replay(value: Value) -> Result<WireEnvelope<StorySequence>, SchemaError> {
    migrate(
        value,
        REPLAY_SCHEMA,
        REPLAY_SCHEMA_VERSION,
        "unsupported-replay-version",
    )
}
pub fn migrate_contract(value: Value) -> Result<WireEnvelope<StoryContract>, SchemaError> {
    migrate(
        value,
        CONTRACT_SCHEMA,
        CONTRACT_SCHEMA_VERSION,
        "unsupported-contract-version",
    )
}
pub fn migrate_theme_replay<T: DeserializeOwned>(
    value: Value,
) -> Result<WireEnvelope<T>, SchemaError> {
    migrate(
        value,
        THEME_REPLAY_SCHEMA,
        THEME_REPLAY_SCHEMA_VERSION,
        "unsupported-theme-replay-version",
    )
}

pub fn encode_recipe(value: MenuRecipe) -> Result<String, String> {
    serde_json::to_string_pretty(&WireEnvelope::new(
        RECIPE_SCHEMA,
        RECIPE_SCHEMA_VERSION,
        value,
    ))
    .map_err(|error| error.to_string())
}
pub fn encode_replay(value: StorySequence) -> Result<String, String> {
    serde_json::to_string_pretty(&WireEnvelope::new(
        REPLAY_SCHEMA,
        REPLAY_SCHEMA_VERSION,
        value,
    ))
    .map_err(|error| error.to_string())
}
pub fn encode_contract(value: StoryContract) -> Result<String, String> {
    serde_json::to_string_pretty(&WireEnvelope::new(
        CONTRACT_SCHEMA,
        CONTRACT_SCHEMA_VERSION,
        value,
    ))
    .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoryAlias {
    pub from: String,
    pub to: String,
    pub adapter: String,
    pub remove_in_version: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedStoryId {
    pub original: String,
    pub canonical: String,
}

#[derive(Clone, Debug, Default)]
pub struct StoryAliases {
    aliases: BTreeMap<String, StoryAlias>,
}

impl StoryAliases {
    pub fn new(
        aliases: impl IntoIterator<Item = StoryAlias>,
        canonical_stories: &BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut by_from = BTreeMap::new();
        for alias in aliases {
            validate_id(&alias.from)?;
            validate_id(&alias.to)?;
            validate_id(&alias.adapter)?;
            if alias.remove_in_version == 0 {
                return Err(format!("alias {} needs a removal version", alias.from));
            }
            if canonical_stories.contains_key(&alias.from) {
                return Err(format!(
                    "story alias shadows canonical story: {}",
                    alias.from
                ));
            }
            if by_from.insert(alias.from.clone(), alias).is_some() {
                return Err("duplicate story alias".into());
            }
        }
        let graph = Self { aliases: by_from };
        for id in graph.aliases.keys() {
            let mut seen = BTreeSet::new();
            let mut current = id.as_str();
            let adapter = graph.aliases[id].adapter.as_str();
            while let Some(alias) = graph.aliases.get(current) {
                if !seen.insert(current.to_owned()) {
                    return Err(format!("story alias cycle: {id}"));
                }
                if alias.adapter != adapter {
                    return Err(format!("story alias crosses adapters: {id}"));
                }
                current = &alias.to;
            }
            let target_adapter = canonical_stories
                .get(current)
                .ok_or_else(|| format!("story alias target does not exist: {current}"))?;
            if target_adapter != adapter {
                return Err(format!("story alias crosses adapters: {id}"));
            }
        }
        Ok(graph)
    }

    #[must_use]
    pub fn resolve(&self, id: &str) -> ResolvedStoryId {
        let mut canonical = id;
        while let Some(alias) = self.aliases.get(canonical) {
            canonical = &alias.to;
        }
        ResolvedStoryId {
            original: id.into(),
            canonical: canonical.into(),
        }
    }

    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.aliases.contains_key(id)
    }

    pub fn validate_for_version(&self, current_version: u16) -> Result<(), String> {
        if let Some(alias) = self
            .aliases
            .values()
            .find(|alias| alias.remove_in_version <= current_version)
        {
            return Err(format!("expired story alias: {}", alias.from));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalProfile {
    pub color: String,
    pub unicode: bool,
    pub motion: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageProfile {
    pub id: String,
    pub version: u16,
    pub terminal: TerminalProfile,
    pub geometries: Vec<(u16, u16)>,
    pub purpose: String,
    pub owner: String,
    pub runtime_budget_ms: u64,
}

impl CoverageProfile {
    pub fn validate(&self) -> Result<(), String> {
        validate_id(&self.id)?;
        validate_id(&self.owner)
            .map_err(|_| "coverage profile owner must be a stable ID".to_owned())?;
        if self.version != PROFILE_SCHEMA_VERSION {
            return Err("unsupported coverage profile version".into());
        }
        if self.geometries.is_empty() || self.geometries.len() > MAX_PROFILE_GEOMETRIES {
            return Err(format!(
                "coverage profile needs 1..{MAX_PROFILE_GEOMETRIES} geometries"
            ));
        }
        let mut unique = BTreeSet::new();
        for &(width, height) in &self.geometries {
            validate_dimensions(width, height)?;
            if !unique.insert((width, height)) {
                return Err("duplicate coverage profile geometry".into());
            }
        }
        if self.purpose.trim().is_empty() || self.purpose.len() > MAX_PROFILE_TEXT_BYTES {
            return Err("coverage profile purpose must be 1..240 bytes".into());
        }
        if self.terminal.color != "truecolor" && self.terminal.color != "ansi256" {
            return Err("unsupported terminal color profile".into());
        }
        if self.runtime_budget_ms == 0 || self.runtime_budget_ms > 300_000 {
            return Err("coverage profile runtime budget must be 1..300000 ms".into());
        }
        Ok(())
    }

    pub fn select<'a>(
        &self,
        manifest: &AdapterManifest,
        scenes: &'a [Scene],
    ) -> Result<Vec<&'a Scene>, String> {
        self.validate()?;
        if !manifest.profiles.contains(&self.id) {
            return Err(format!(
                "adapter {} does not support profile {}",
                manifest.id, self.id
            ));
        }
        let geometries: BTreeSet<_> = self.geometries.iter().copied().collect();
        Ok(scenes
            .iter()
            .filter(|scene| geometries.contains(&(scene.width, scene.height)))
            .collect())
    }
}

#[must_use]
pub fn builtin_profiles() -> [CoverageProfile; 2] {
    [
        CoverageProfile {
            id: "pr-v1".into(),
            version: PROFILE_SCHEMA_VERSION,
            terminal: TerminalProfile {
                color: "truecolor".into(),
                unicode: true,
                motion: false,
            },
            geometries: vec![
                (28, 14),
                (30, 14),
                (40, 20),
                (40, 24),
                (60, 20),
                (60, 24),
                (72, 22),
            ],
            purpose: "Fast deterministic pull request evidence".into(),
            owner: "maestro-ui".into(),
            runtime_budget_ms: 30_000,
        },
        CoverageProfile {
            id: "scheduled-v1".into(),
            version: PROFILE_SCHEMA_VERSION,
            terminal: TerminalProfile {
                color: "truecolor".into(),
                unicode: true,
                motion: true,
            },
            geometries: vec![
                (28, 14),
                (30, 14),
                (32, 12),
                (40, 20),
                (40, 24),
                (60, 20),
                (60, 24),
                (60, 32),
                (72, 22),
                (80, 30),
                (100, 30),
                (100, 40),
            ],
            purpose: "Broad scheduled UI contract coverage".into(),
            owner: "maestro-ui".into(),
            runtime_budget_ms: 120_000,
        },
    ]
}

pub fn builtin_profile(id: &str) -> Result<CoverageProfile, String> {
    builtin_profiles()
        .into_iter()
        .find(|profile| profile.id == id)
        .ok_or_else(|| format!("unknown coverage profile: {id}"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RendererIdentity {
    pub id: String,
    pub version: String,
    pub source_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureMetadata {
    pub capture_version: u16,
    pub story_contract_version: u16,
    pub replay_version: u16,
    pub observation_version: u16,
    pub profile_version: u16,
    pub profile: ProfileIdentity,
    pub renderer: RendererIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileIdentity {
    pub id: String,
    pub version: u16,
}

impl CaptureMetadata {
    #[must_use]
    pub fn current() -> Self {
        Self {
            capture_version: CAPTURE_SCHEMA_VERSION,
            story_contract_version: CONTRACT_SCHEMA_VERSION,
            replay_version: REPLAY_SCHEMA_VERSION,
            observation_version: OBSERVATION_SCHEMA_VERSION,
            profile_version: PROFILE_SCHEMA_VERSION,
            profile: ProfileIdentity {
                id: "unprofiled".into(),
                version: PROFILE_SCHEMA_VERSION,
            },
            renderer: RendererIdentity {
                id: "maestro-ui-preview".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                source_digest: env!("MAESTRO_PREVIEW_SOURCE_DIGEST").into(),
            },
        }
    }

    #[must_use]
    pub fn for_profile(profile: &CoverageProfile) -> Self {
        let mut metadata = Self::current();
        metadata.profile = ProfileIdentity {
            id: profile.id.clone(),
            version: profile.version,
        };
        metadata
    }
}
