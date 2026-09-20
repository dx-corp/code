//! Bounded, serializable behavior contracts for deterministic UI stories.
use crate::authoring::validate_id;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
use std::collections::{BTreeMap, BTreeSet};

pub const STORY_CONTRACT_VERSION: u8 = 1;
pub const MAX_OBSERVATIONS: usize = 32;
pub const MAX_EFFECTS: usize = 32;
pub const MAX_OBSERVATION_BYTES: usize = 512;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StoryObservation {
    Text {
        name: String,
        value: String,
        hidden: bool,
    },
    Id {
        name: String,
        value: String,
    },
    Flag {
        name: String,
        value: bool,
    },
}

impl Serialize for StoryObservation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Text {
                name,
                value,
                hidden,
            } => {
                let mut state = serializer.serialize_map(Some(4))?;
                state.serialize_entry("kind", "text")?;
                state.serialize_entry("name", name)?;
                state.serialize_entry("value", if *hidden { "[hidden]" } else { value })?;
                state.serialize_entry("hidden", hidden)?;
                state.end()
            }
            Self::Id { name, value } => {
                let mut state = serializer.serialize_map(Some(3))?;
                state.serialize_entry("kind", "id")?;
                state.serialize_entry("name", name)?;
                state.serialize_entry("value", value)?;
                state.end()
            }
            Self::Flag { name, value } => {
                let mut state = serializer.serialize_map(Some(3))?;
                state.serialize_entry("kind", "flag")?;
                state.serialize_entry("name", name)?;
                state.serialize_entry("value", value)?;
                state.end()
            }
        }
    }
}

impl StoryObservation {
    #[must_use]
    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Text {
            name: name.into(),
            value: value.into(),
            hidden: false,
        }
    }
    #[must_use]
    pub fn hidden_text(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Text {
            name: name.into(),
            value: value.into(),
            hidden: true,
        }
    }
    #[must_use]
    pub fn id(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Id {
            name: name.into(),
            value: value.into(),
        }
    }
    #[must_use]
    pub fn flag(name: impl Into<String>, value: bool) -> Self {
        Self::Flag {
            name: name.into(),
            value,
        }
    }
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Text { name, .. } | Self::Id { name, .. } | Self::Flag { name, .. } => name,
        }
    }
    #[must_use]
    pub fn display_value(&self) -> String {
        match self {
            Self::Text { value, hidden, .. } => {
                if *hidden {
                    "[hidden]".into()
                } else {
                    value.clone()
                }
            }
            Self::Id { value, .. } => value.clone(),
            Self::Flag { value, .. } => value.to_string(),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        validate_id(self.name()).map_err(|_| "observation name must be a stable ID".to_owned())?;
        let value_bytes = match self {
            Self::Text { value, .. } | Self::Id { value, .. } => value.len(),
            Self::Flag { .. } => 0,
        };
        if value_bytes > MAX_OBSERVATION_BYTES {
            return Err(format!(
                "observation value exceeds {MAX_OBSERVATION_BYTES} bytes"
            ));
        }
        if let Self::Id { value, .. } = self {
            validate_id(value)
                .map_err(|_| "observation ID value must be a stable ID".to_owned())?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoryEffect {
    pub id: String,
}
impl StoryEffect {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
    pub fn validate(&self) -> Result<(), String> {
        validate_id(&self.id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StoryExpectation {
    ObservationEquals { name: String, value: String },
    EffectCount { id: String, count: u16 },
    NoUnexpectedEffects { allowed: Vec<String> },
}
impl StoryExpectation {
    #[must_use]
    pub fn observation(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self::ObservationEquals {
            name: name.into(),
            value: value.into(),
        }
    }
    #[must_use]
    pub fn effect_count(id: impl Into<String>, count: u16) -> Self {
        Self::EffectCount {
            id: id.into(),
            count,
        }
    }
    #[must_use]
    pub fn no_unexpected_effects<I, S>(allowed: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::NoUnexpectedEffects {
            allowed: allowed.into_iter().map(Into::into).collect(),
        }
    }
    fn key(&self) -> String {
        match self {
            Self::ObservationEquals { name, .. } => format!("observation:{name}"),
            Self::EffectCount { id, .. } => format!("effect:{id}"),
            Self::NoUnexpectedEffects { .. } => "effects:allowed".into(),
        }
    }
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::ObservationEquals { name, value } => {
                validate_id(name)?;
                if value.len() > MAX_OBSERVATION_BYTES {
                    return Err("expectation value is too large".into());
                }
            }
            Self::EffectCount { id, count } => {
                validate_id(id)?;
                if usize::from(*count) > MAX_EFFECTS {
                    return Err("expected effect count is too large".into());
                }
            }
            Self::NoUnexpectedEffects { allowed } => {
                if allowed.len() > MAX_EFFECTS {
                    return Err("too many allowed effects".into());
                }
                let mut seen = BTreeSet::new();
                for id in allowed {
                    validate_id(id)?;
                    if !seen.insert(id) {
                        return Err(format!("duplicate allowed effect: {id}"));
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoryContract {
    pub version: u8,
    pub story_id: String,
    pub owner: String,
    pub expectations: Vec<StoryExpectation>,
}
impl StoryContract {
    #[must_use]
    pub fn new(story_id: impl Into<String>, owner: impl Into<String>) -> Self {
        Self {
            version: STORY_CONTRACT_VERSION,
            story_id: story_id.into(),
            owner: owner.into(),
            expectations: Vec::new(),
        }
    }
    #[must_use]
    pub fn expect(mut self, expectation: StoryExpectation) -> Self {
        self.expectations.push(expectation);
        self
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.version != STORY_CONTRACT_VERSION {
            return Err(format!(
                "unsupported story contract version: {}",
                self.version
            ));
        }
        validate_id(&self.story_id)?;
        validate_id(&self.owner)
            .map_err(|_| "story contract owner must be a stable ID".to_owned())?;
        if self.expectations.len() > MAX_OBSERVATIONS + MAX_EFFECTS {
            return Err("too many story expectations".into());
        }
        let mut keys = BTreeSet::new();
        for expectation in &self.expectations {
            expectation.validate()?;
            let key = expectation.key();
            if !keys.insert(key.clone()) {
                return Err(format!("duplicate expectation: {key}"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectationResult {
    pub expectation: StoryExpectation,
    pub passed: bool,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoryStepResult {
    pub schema_version: u8,
    pub step: usize,
    pub observations: Vec<StoryObservation>,
    pub effects: Vec<StoryEffect>,
    pub expectations: Vec<ExpectationResult>,
}
impl StoryStepResult {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.expectations.iter().all(|result| result.passed)
    }
    #[must_use]
    pub fn failures(&self) -> Vec<String> {
        self.expectations
            .iter()
            .filter(|r| !r.passed)
            .map(|r| r.message.clone())
            .collect()
    }
}

pub fn evaluate_step(
    step: usize,
    observations: Vec<StoryObservation>,
    effects: Vec<StoryEffect>,
    expectations: &[StoryExpectation],
) -> Result<StoryStepResult, String> {
    if observations.len() > MAX_OBSERVATIONS {
        return Err(format!(
            "story step exceeds {MAX_OBSERVATIONS} observations"
        ));
    }
    if effects.len() > MAX_EFFECTS {
        return Err(format!("story step exceeds {MAX_EFFECTS} effects"));
    }
    let mut names = BTreeSet::new();
    for observation in &observations {
        observation.validate()?;
        if !names.insert(observation.name()) {
            return Err(format!("duplicate observation: {}", observation.name()));
        }
    }
    for effect in &effects {
        effect.validate()?;
    }
    let values: BTreeMap<_, _> = observations
        .iter()
        .map(|o| (o.name(), o.display_value()))
        .collect();
    let counts = effects
        .iter()
        .fold(BTreeMap::<&str, usize>::new(), |mut out, effect| {
            *out.entry(&effect.id).or_default() += 1;
            out
        });
    let mut results = Vec::with_capacity(expectations.len());
    for expectation in expectations {
        expectation.validate()?;
        let (passed, message) = match expectation {
            StoryExpectation::ObservationEquals { name, value } => {
                let actual = values.get(name.as_str()).map(String::as_str);
                (
                    actual == Some(value),
                    format!(
                        "expected {name} to equal {value}; observed {}",
                        actual.unwrap_or("<missing>")
                    ),
                )
            }
            StoryExpectation::EffectCount { id, count } => {
                let actual = *counts.get(id.as_str()).unwrap_or(&0);
                (
                    actual == usize::from(*count),
                    format!(
                        "expected {id} exactly {count} time{}; observed {actual}",
                        if *count == 1 { "" } else { "s" }
                    ),
                )
            }
            StoryExpectation::NoUnexpectedEffects { allowed } => {
                let unexpected: Vec<_> = effects
                    .iter()
                    .filter(|effect| !allowed.contains(&effect.id))
                    .map(|effect| effect.id.clone())
                    .collect();
                (
                    unexpected.is_empty(),
                    if unexpected.is_empty() {
                        "observed only allowed effects".into()
                    } else {
                        format!("unexpected effects: {}", unexpected.join(", "))
                    },
                )
            }
        };
        results.push(ExpectationResult {
            expectation: expectation.clone(),
            passed,
            message,
        });
    }
    Ok(StoryStepResult {
        schema_version: STORY_CONTRACT_VERSION,
        step,
        observations,
        effects,
        expectations: results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contract_rejects_duplicate_names_and_unbounded_values() {
        let duplicate = StoryContract::new("theme-selector", "maestro-tui")
            .expect(StoryExpectation::observation("state", "error"))
            .expect(StoryExpectation::observation("state", "ready"));
        assert_eq!(
            duplicate.validate().unwrap_err(),
            "duplicate expectation: observation:state"
        );
        assert!(
            StoryObservation::text("message", "x".repeat(MAX_OBSERVATION_BYTES + 1))
                .validate()
                .is_err()
        );
    }
    #[test]
    fn replay_emits_result_and_fails_wrong_effect_count() {
        let result = evaluate_step(
            1,
            vec![StoryObservation::id("state", "ready")],
            vec![StoryEffect::new("retry-themes")],
            &[StoryExpectation::effect_count("retry-themes", 1)],
        )
        .unwrap();
        assert!(result.passed());
        assert_eq!(result.step, 1);
        let failed = evaluate_step(
            1,
            vec![],
            vec![],
            &[StoryExpectation::effect_count("retry-themes", 1)],
        )
        .unwrap();
        assert_eq!(
            failed.failures(),
            ["expected retry-themes exactly 1 time; observed 0"]
        );
    }
    #[test]
    fn hidden_observations_serialize_redacted() {
        let json =
            serde_json::to_string(&StoryObservation::hidden_text("query", "secret glyph")).unwrap();
        assert!(!json.contains("secret glyph"));
        assert!(json.contains("[hidden]"));
        assert!(json.contains("\"kind\":\"text\""));
        let decoded: StoryObservation = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.display_value(), "[hidden]");
    }
}
