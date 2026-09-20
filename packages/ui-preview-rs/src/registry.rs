//! Register a renderer once; derive catalog, captures, and source links from it.
use crate::{
    Scene, StoryFilter,
    authoring::{REPLAY_STEP_MS, StoryInput, StorySequence, validate_dimensions, validate_id},
    contract::{StoryContract, StoryEffect, StoryObservation, evaluate_step},
    review::{self, Capture},
    schema::{
        CAPTURE_SCHEMA_VERSION, CoverageProfile, ProfileIdentity, ResolvedStoryId, StoryAlias,
        StoryAliases,
    },
};
use ratatui::{Frame, Terminal, backend::TestBackend, buffer::Buffer};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoverageState {
    Declared,
    Visited,
    AssertedPassed,
    AssertedFailed,
    Skipped,
    Unavailable,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CoverageSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileIdentity>,
    pub declared: Vec<String>,
    pub states: BTreeMap<String, CoverageState>,
}
impl CoverageSummary {
    pub fn declare(&mut self, id: impl Into<String>) {
        let id = id.into();
        if !self.declared.contains(&id) {
            self.declared.push(id.clone());
        }
        self.states.insert(id, CoverageState::Declared);
    }
    pub fn record(&mut self, id: impl Into<String>, state: CoverageState) {
        self.states.insert(id.into(), state);
    }
    #[must_use]
    pub fn state(&self, id: &str) -> Option<CoverageState> {
        self.states.get(id).copied()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ContributionDiagnostic {
    pub story: String,
    pub adapter: String,
    pub owner: String,
    pub status: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct StoryInspection {
    pub id: String,
    pub label: String,
    pub source: String,
    pub adapter: String,
    pub cases: Vec<(u16, u16, u64)>,
    pub input_count: usize,
    pub assertion_count: usize,
    pub contract_status: &'static str,
}

type Renderer = Box<dyn Fn(&Scene) -> Result<Buffer, String>>;
type Observer = Box<
    dyn Fn(
        &Scene,
        &[StoryInput],
        &Buffer,
    ) -> Result<(Vec<StoryObservation>, Vec<StoryEffect>), String>,
>;
/// One named fixture family. The closure captures typed, caller-owned mock data.
pub struct Story {
    id: String,
    label: String,
    source: String,
    cases: Vec<(u16, u16, u64)>,
    renderer: Renderer,
    sequence: Option<StorySequence>,
    contract: Option<StoryContract>,
    observer: Option<Observer>,
    adapter: String,
}
impl Story {
    pub fn new(
        id: &str,
        label: &str,
        source: &str,
        render: impl Fn(&Scene, &mut Frame<'_>) + 'static,
    ) -> Self {
        Self::buffer(id, label, source, move |scene| {
            let mut terminal = Terminal::new(TestBackend::new(scene.width, scene.height))
                .map_err(|e| e.to_string())?;
            terminal
                .draw(|f| render(scene, f))
                .map_err(|e| e.to_string())?;
            Ok(terminal.backend().buffer().clone())
        })
    }
    pub fn buffer(
        id: &str,
        label: &str,
        source: &str,
        render: impl Fn(&Scene) -> Result<Buffer, String> + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            source: source.into(),
            cases: vec![(60, 24, 0)],
            renderer: Box::new(render),
            sequence: None,
            contract: None,
            observer: None,
            adapter: "unowned".into(),
        }
    }
    /// Build deterministic frames by replaying each prefix through the same
    /// controller and renderer used by the host application.
    pub fn replay(
        sequence: StorySequence,
        source: &str,
        render: impl Fn(&Scene, &[StoryInput]) -> Result<Buffer, String> + 'static,
    ) -> Result<Self, String> {
        let cases = sequence.cases()?;
        let replay = sequence.clone();
        Ok(Self {
            id: sequence.id.clone(),
            label: sequence.label.clone(),
            source: source.into(),
            cases,
            renderer: Box::new(move |scene| {
                if scene.time_ms % REPLAY_STEP_MS != 0 {
                    return Err("replay time is not an interaction step".into());
                }
                let step = usize::try_from(scene.time_ms / REPLAY_STEP_MS)
                    .map_err(|_| "invalid replay step")?;
                if step > replay.inputs.len() {
                    return Err("replay step exceeds input sequence".into());
                }
                let expected = replay.cases()?[step];
                if (scene.width, scene.height, scene.time_ms) != expected {
                    return Err("scene does not match replay dimensions".into());
                }
                render(scene, &replay.inputs[..step])
            }),
            sequence: Some(sequence),
            contract: None,
            observer: None,
            adapter: "unowned".into(),
        })
    }
    /// Cross product of sizes and fixed timestamps; no wall clock or sleeps.
    pub fn matrix(mut self, sizes: &[(u16, u16)], times: &[u64]) -> Self {
        self.cases = sizes
            .iter()
            .flat_map(|&(w, h)| times.iter().map(move |&t| (w, h, t)))
            .collect();
        self
    }
    #[must_use]
    pub fn adapter(mut self, adapter: &str) -> Self {
        self.adapter = adapter.into();
        self
    }
    /// Attach bounded behavior observations to the same renderer that produces
    /// visual captures. The evaluator is effect-free and receives replay inputs.
    pub fn contract(
        mut self,
        contract: StoryContract,
        observe: impl Fn(
            &Scene,
            &[StoryInput],
            &Buffer,
        ) -> Result<(Vec<StoryObservation>, Vec<StoryEffect>), String>
        + 'static,
    ) -> Result<Self, String> {
        contract.validate()?;
        if contract.story_id != self.id {
            return Err("story contract ID must match story ID".into());
        }
        self.contract = Some(contract);
        self.observer = Some(Box::new(observe));
        Ok(self)
    }
}
#[derive(Default)]
pub struct Registry {
    stories: BTreeMap<String, Story>,
    aliases: StoryAliases,
}
impl Registry {
    pub fn inspect(&self, id: &str) -> Result<StoryInspection, String> {
        let resolved = self.aliases.resolve(id);
        let story = self
            .stories
            .get(&resolved.canonical)
            .ok_or_else(|| format!("unknown story: {id}"))?;
        let assertion_count = story
            .contract
            .as_ref()
            .map_or(0, |contract| contract.expectations.len());
        Ok(StoryInspection {
            id: story.id.clone(),
            label: story.label.clone(),
            source: story.source.clone(),
            adapter: story.adapter.clone(),
            cases: story.cases.clone(),
            input_count: story
                .sequence
                .as_ref()
                .map_or(0, |sequence| sequence.inputs.len()),
            assertion_count,
            contract_status: if assertion_count == 0 {
                "behavior-not-asserted"
            } else {
                "asserted"
            },
        })
    }
    pub fn add(&mut self, story: Story) -> Result<(), String> {
        validate_id(&story.id)?;
        if let Some(sequence) = &story.sequence {
            if sequence.cases()? != story.cases {
                return Err("replay cases must be derived from its interaction sequence".into());
            }
        }
        if self.stories.contains_key(&story.id) {
            return Err(format!("duplicate story: {}", story.id));
        }
        if story.cases.is_empty()
            || story
                .cases
                .iter()
                .any(|&(w, h, t)| validate_dimensions(w, h).is_err() || t > 86_400_000)
        {
            return Err("story needs bounded dimensions and timestamps".into());
        }
        let mut cases = story.cases.clone();
        cases.sort_unstable();
        cases.dedup();
        if cases.len() != story.cases.len() {
            return Err("duplicate story case".into());
        }
        self.stories.insert(story.id.clone(), story);
        Ok(())
    }
    pub fn sequences(&self) -> Vec<StorySequence> {
        self.stories
            .values()
            .filter_map(|story| story.sequence.clone())
            .collect()
    }
    pub fn coverage_summary(&self) -> Result<CoverageSummary, String> {
        let captures = self.results()?;
        let mut summary = CoverageSummary::default();
        for id in self.stories.keys() {
            summary.declare(id);
        }
        for capture in captures {
            let state = match capture.semantic {
                Some(result) if result.passed() => CoverageState::AssertedPassed,
                Some(_) => CoverageState::AssertedFailed,
                None => CoverageState::Visited,
            };
            summary.record(capture.scene.id, state);
        }
        Ok(summary)
    }

    pub fn coverage_for_profile(
        &self,
        adapters: &crate::adapters::AdapterRegistry,
        profile: &CoverageProfile,
    ) -> Result<CoverageSummary, String> {
        profile.validate()?;
        let mut summary = CoverageSummary {
            profile: Some(ProfileIdentity {
                id: profile.id.clone(),
                version: profile.version,
            }),
            ..CoverageSummary::default()
        };
        for id in self.stories.keys() {
            summary.declare(id);
        }
        for story in self.stories.values() {
            let manifest = adapters.get(&story.adapter).map_err(|_| {
                format!(
                    "orphan story {} references adapter {}",
                    story.id, story.adapter
                )
            })?;
            if !manifest.profiles.contains(&profile.id) {
                summary.record(&story.id, CoverageState::Unavailable);
                continue;
            }
            let has_profile_case = story
                .cases
                .iter()
                .any(|(width, height, _)| profile.geometries.contains(&(*width, *height)));
            if !has_profile_case {
                summary.record(&story.id, CoverageState::Skipped);
                continue;
            }
            let captures = self.results_for(std::iter::once(story), None, None, Some(profile))?;
            let state = if captures
                .iter()
                .filter_map(|capture| capture.semantic.as_ref())
                .any(|result| !result.passed())
            {
                CoverageState::AssertedFailed
            } else if captures.iter().any(|capture| capture.semantic.is_some()) {
                CoverageState::AssertedPassed
            } else {
                CoverageState::Visited
            };
            summary.record(&story.id, state);
        }
        Ok(summary)
    }

    pub fn contribution_diagnostics(
        &self,
        adapters: &crate::adapters::AdapterRegistry,
    ) -> Result<Vec<ContributionDiagnostic>, String> {
        self.stories
            .values()
            .map(|story| {
                let adapter = adapters.get(&story.adapter).map_err(|_| {
                    format!(
                        "orphan story {} references adapter {}",
                        story.id, story.adapter
                    )
                })?;
                if adapter.owner.is_empty() {
                    return Err(format!("ownerless adapter: {}", adapter.id));
                }
                if let Some(profile) = adapter
                    .profiles
                    .iter()
                    .find(|profile| !matches!(profile.as_str(), "pr-v1" | "scheduled-v1"))
                {
                    return Err(format!("unsupported coverage profile: {profile}"));
                }
                match &adapter.lifecycle {
                    crate::adapters::StoryLifecycle::Deprecated { replacement } => {
                        adapters.get(replacement).map_err(|_| {
                            format!(
                                "deprecated adapter {} has missing replacement {replacement}",
                                adapter.id
                            )
                        })?;
                    }
                    crate::adapters::StoryLifecycle::Retired { reason } => {
                        return Err(format!(
                            "story {} uses retired adapter {}: {reason}",
                            story.id, adapter.id
                        ));
                    }
                    crate::adapters::StoryLifecycle::Active => {}
                }
                Ok(ContributionDiagnostic {
                    story: story.id.clone(),
                    adapter: adapter.id.clone(),
                    owner: adapter.owner.clone(),
                    status: "owned",
                })
            })
            .collect()
    }
    pub fn add_aliases(
        &mut self,
        aliases: impl IntoIterator<Item = StoryAlias>,
    ) -> Result<(), String> {
        let stories = self
            .stories
            .iter()
            .map(|(id, story)| (id.clone(), story.adapter.clone()))
            .collect();
        let aliases = StoryAliases::new(aliases, &stories)?;
        aliases.validate_for_version(CAPTURE_SCHEMA_VERSION)?;
        self.aliases = aliases;
        Ok(())
    }
    /// Bridge existing catalogs without changing their IDs or capture matrix.
    pub fn import(
        &mut self,
        scenes: Vec<Scene>,
        source: &str,
        render: fn(&Scene) -> Result<Buffer, String>,
    ) -> Result<(), String> {
        let mut groups: BTreeMap<String, Story> = BTreeMap::new();
        for scene in scenes {
            let story = groups.entry(scene.id.clone()).or_insert_with(|| {
                let mut story =
                    Story::buffer(&scene.id, &scene.label, source, render).adapter("legacy");
                story.cases.clear();
                story
            });
            story.cases.push((scene.width, scene.height, scene.time_ms));
        }
        for story in groups.into_values() {
            self.add(story)?;
        }
        Ok(())
    }
    pub fn scenes(&self) -> Vec<Scene> {
        self.stories
            .values()
            .flat_map(|s| {
                s.cases.iter().map(|&(width, height, time_ms)| Scene {
                    id: s.id.clone(),
                    label: s.label.clone(),
                    width,
                    height,
                    time_ms,
                })
            })
            .collect()
    }
    pub fn render(&self, scene: &Scene) -> Result<Buffer, String> {
        let story = self.stories.get(&scene.id).ok_or("unknown story")?;
        if !(8..=240).contains(&scene.width)
            || !(3..=100).contains(&scene.height)
            || scene.time_ms > 86_400_000
        {
            return Err("unbounded scene".into());
        }
        (story.renderer)(scene)
    }
    pub fn captures(&self) -> Result<Vec<Capture>, String> {
        self.results()
    }
    /// Render visual and semantic evidence from one immutable story case.
    pub fn results(&self) -> Result<Vec<Capture>, String> {
        self.results_for(self.stories.values(), None, None, None)
    }
    pub fn results_for_profile(
        &self,
        adapters: &crate::adapters::AdapterRegistry,
        profile: &CoverageProfile,
    ) -> Result<Vec<Capture>, String> {
        profile.validate()?;
        let mut stories = Vec::new();
        for story in self.stories.values() {
            let adapter = adapters.get(&story.adapter).map_err(|_| {
                format!(
                    "orphan story {} references adapter {}",
                    story.id, story.adapter
                )
            })?;
            if adapter.profiles.contains(&profile.id) {
                stories.push(story);
            }
        }
        self.results_for(stories, None, None, Some(profile))
    }
    fn results_for<'a>(
        &self,
        stories: impl IntoIterator<Item = &'a Story>,
        filter: Option<StoryFilter>,
        resolved_story: Option<ResolvedStoryId>,
        profile: Option<&CoverageProfile>,
    ) -> Result<Vec<Capture>, String> {
        stories
            .into_iter()
            .flat_map(|story| {
                story
                    .cases
                    .iter()
                    .filter(move |&&(width, height, _)| {
                        profile.is_none_or(|profile| profile.geometries.contains(&(width, height)))
                    })
                    .map(move |&(width, height, time_ms)| {
                        (
                            story,
                            Scene {
                                id: story.id.clone(),
                                label: story.label.clone(),
                                width,
                                height,
                                time_ms,
                            },
                        )
                    })
            })
            .map(|(story, scene)| {
                let buffer = (story.renderer)(&scene)?;
                let mut capture = review::from_buffer(scene.clone(), &buffer)?;
                if let Some(profile) = profile {
                    capture.metadata = crate::schema::CaptureMetadata::for_profile(profile);
                }
                capture.source = story.source.clone();
                capture.filter = filter.clone();
                capture.resolved_story = resolved_story.clone();
                if let (Some(contract), Some(observer)) = (&story.contract, &story.observer) {
                    let step = usize::try_from(scene.time_ms / REPLAY_STEP_MS)
                        .map_err(|_| "invalid replay step")?;
                    let inputs = story.sequence.as_ref().map_or(&[][..], |sequence| {
                        &sequence.inputs[..step.min(sequence.inputs.len())]
                    });
                    let (observations, effects) = observer(&scene, inputs, &buffer)?;
                    let expectations = if story
                        .sequence
                        .as_ref()
                        .is_none_or(|sequence| step == sequence.inputs.len())
                    {
                        &contract.expectations[..]
                    } else {
                        &[][..]
                    };
                    capture.semantic =
                        Some(evaluate_step(step, observations, effects, expectations)?);
                }
                Ok(capture)
            })
            .collect()
    }

    #[must_use]
    pub fn story_ids(&self) -> Vec<String> {
        self.stories.keys().cloned().collect()
    }

    pub fn filtered(&self, filter: StoryFilter) -> Result<FilteredRegistry<'_>, String> {
        let resolved_story = match &filter {
            StoryFilter::Story(id) => Some(self.aliases.resolve(id)),
            StoryFilter::Adapter(_) => None,
        };
        let stories: Vec<_> = self
            .stories
            .values()
            .filter(|story| match &filter {
                StoryFilter::Story(_) => resolved_story
                    .as_ref()
                    .is_some_and(|resolved| story.id == resolved.canonical),
                StoryFilter::Adapter(id) => &story.adapter == id,
            })
            .collect();
        if stories.is_empty() {
            return Err(match &filter {
                StoryFilter::Story(id) => format!("unknown story: {id}"),
                StoryFilter::Adapter(id) => format!("unknown adapter or empty adapter: {id}"),
            });
        }
        Ok(FilteredRegistry {
            registry: self,
            filter,
            stories,
            resolved_story,
        })
    }
}

pub struct FilteredRegistry<'a> {
    registry: &'a Registry,
    filter: StoryFilter,
    stories: Vec<&'a Story>,
    resolved_story: Option<ResolvedStoryId>,
}
impl FilteredRegistry<'_> {
    #[must_use]
    pub fn len(&self) -> usize {
        self.stories.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stories.is_empty()
    }
    #[must_use]
    pub fn story_ids(&self) -> Vec<String> {
        self.stories.iter().map(|story| story.id.clone()).collect()
    }
    pub fn scenes(&self) -> Vec<Scene> {
        self.stories
            .iter()
            .flat_map(|story| {
                story
                    .cases
                    .iter()
                    .map(move |&(width, height, time_ms)| Scene {
                        id: story.id.clone(),
                        label: story.label.clone(),
                        width,
                        height,
                        time_ms,
                    })
            })
            .collect()
    }
    pub fn sequences(&self) -> Vec<StorySequence> {
        self.stories
            .iter()
            .filter_map(|story| story.sequence.clone())
            .collect()
    }
    pub fn captures(&self) -> Result<Vec<Capture>, String> {
        self.registry.results_for(
            self.stories.iter().copied(),
            Some(self.filter.clone()),
            self.resolved_story.clone(),
            None,
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn story() -> Story {
        Story::new("menu", "Menu", "example.rs", |_, f| {
            f.render_widget(ratatui::widgets::Paragraph::new("fixture"), f.area());
        })
        .matrix(&[(40, 20), (80, 30)], &[0, 80])
    }
    #[test]
    fn coverage_distinguishes_declared_visited_asserted_and_terminal_states() {
        let mut summary = CoverageSummary::default();
        summary.declare("scheduled-only");
        summary.record("unicode", CoverageState::Visited);
        summary.record("error", CoverageState::AssertedPassed);
        summary.record("broken", CoverageState::AssertedFailed);
        summary.record("skipped", CoverageState::Skipped);
        summary.record("unavailable", CoverageState::Unavailable);
        assert_eq!(summary.state("error"), Some(CoverageState::AssertedPassed));
        assert_eq!(summary.state("unicode"), Some(CoverageState::Visited));
        assert_eq!(
            summary.state("scheduled-only"),
            Some(CoverageState::Declared)
        );
        assert_eq!(summary.states.len(), 6);
    }

    #[test]
    fn ownership_diagnostics_reject_orphan_stories() {
        let mut registry = Registry::default();
        registry.add(story().adapter("missing-adapter")).unwrap();
        assert!(
            registry
                .contribution_diagnostics(&crate::adapters::AdapterRegistry::builtins().unwrap())
                .unwrap_err()
                .contains("orphan story")
        );
    }

    #[test]
    fn contribution_validation_enforces_owner_and_lifecycle() {
        let mut registry = Registry::default();
        registry.add(story().adapter("shared-menu")).unwrap();
        let manifest = || {
            crate::adapters::AdapterManifest::active(
                "shared-menu",
                "maestro-ui",
                "fixtures",
                "registration.rs",
                crate::adapters::StoryTemplate::MenuRecipe,
            )
        };
        let mut ownerless = manifest();
        ownerless.owner.clear();
        assert!(
            registry
                .contribution_diagnostics(
                    &crate::adapters::AdapterRegistry::new([ownerless]).unwrap()
                )
                .unwrap_err()
                .contains("ownerless")
        );
        let mut deprecated = manifest();
        deprecated.lifecycle = crate::adapters::StoryLifecycle::Deprecated {
            replacement: "missing".into(),
        };
        assert!(
            registry
                .contribution_diagnostics(
                    &crate::adapters::AdapterRegistry::new([deprecated]).unwrap()
                )
                .unwrap_err()
                .contains("missing replacement")
        );
        let mut unsupported = manifest();
        unsupported.profiles = vec!["unbounded".into()];
        assert!(
            registry
                .contribution_diagnostics(
                    &crate::adapters::AdapterRegistry::new([unsupported]).unwrap()
                )
                .unwrap_err()
                .contains("unsupported coverage profile")
        );
        let mut retired = manifest();
        retired.lifecycle = crate::adapters::StoryLifecycle::Retired {
            reason: "superseded".into(),
        };
        assert!(
            registry
                .contribution_diagnostics(
                    &crate::adapters::AdapterRegistry::new([retired]).unwrap()
                )
                .unwrap_err()
                .contains("retired adapter")
        );
    }

    #[test]
    fn profile_evaluation_produces_skipped_and_unavailable_states() {
        let mut registry = Registry::default();
        registry.add(story().adapter("shared-menu")).unwrap();
        let manifest = crate::adapters::AdapterManifest::active(
            "shared-menu",
            "maestro-ui",
            "fixtures",
            "registration.rs",
            crate::adapters::StoryTemplate::MenuRecipe,
        );
        let mut profile = crate::schema::builtin_profiles()[0].clone();
        profile.geometries = vec![(60, 20)];
        let adapters = crate::adapters::AdapterRegistry::new([manifest.clone()]).unwrap();
        assert_eq!(
            registry
                .coverage_for_profile(&adapters, &profile)
                .unwrap()
                .state("menu"),
            Some(CoverageState::Skipped)
        );
        assert_eq!(
            registry
                .coverage_for_profile(&adapters, &profile)
                .unwrap()
                .declared,
            vec!["menu"]
        );
        let scheduled = crate::schema::builtin_profiles()[1].clone();
        let mut unsupported = manifest.clone();
        unsupported.profiles = vec!["pr-v1".into()];
        let adapters = crate::adapters::AdapterRegistry::new([unsupported]).unwrap();
        assert_eq!(
            registry
                .coverage_for_profile(&adapters, &scheduled)
                .unwrap()
                .state("menu"),
            Some(CoverageState::Unavailable)
        );
    }
    #[test]
    fn registry_derives_every_case_and_rejects_collisions() {
        let mut registry = Registry::default();
        registry.add(story()).unwrap();
        assert!(registry.add(story()).is_err());
        let captures = registry.captures().unwrap();
        assert_eq!(captures.len(), 4);
        assert!(captures.iter().all(|c| c.source == "example.rs"));
        assert_eq!(
            review::json(&captures).unwrap(),
            review::json(&registry.captures().unwrap()).unwrap()
        );
    }

    #[test]
    fn replay_uses_prefixes_and_rejects_mismatched_dimensions() {
        let sequence = StorySequence::new("menu-replay", "Menu replay", 40, 10).inputs([
            StoryInput::Text { text: "猫".into() },
            StoryInput::Resize {
                width: 20,
                height: 8,
            },
        ]);
        let story = Story::replay(sequence.clone(), "example.rs", |scene, inputs| {
            let mut buffer =
                Buffer::empty(ratatui::layout::Rect::new(0, 0, scene.width, scene.height));
            buffer.set_string(
                0,
                0,
                inputs.len().to_string(),
                ratatui::style::Style::default(),
            );
            Ok(buffer)
        })
        .unwrap();
        let mut registry = Registry::default();
        registry.add(story).unwrap();
        assert_eq!(registry.sequences(), vec![sequence]);
        let captures = registry.captures().unwrap();
        assert_eq!(captures.len(), 3);
        assert_eq!(captures[0].cells[0].text, "0");
        assert_eq!(captures[2].scene.width, 20);
        let mut invalid = captures[2].scene.clone();
        invalid.width = 21;
        assert!(registry.render(&invalid).is_err());
    }

    #[test]
    fn replay_matrix_cannot_replace_sequence_derived_cases() {
        let sequence = StorySequence::new("matrix-replay", "Matrix replay", 40, 10);
        let story = Story::replay(sequence, "example.rs", |scene, _| {
            Ok(Buffer::empty(ratatui::layout::Rect::new(
                0,
                0,
                scene.width,
                scene.height,
            )))
        })
        .unwrap()
        .matrix(&[(80, 20)], &[0]);
        assert!(Registry::default().add(story).is_err());
    }

    #[test]
    fn filters_select_registered_story_or_adapter_and_reject_unknown_values() {
        let mut registry = Registry::default();
        registry.add(story().adapter("shared-menu")).unwrap();
        registry
            .add(Story::new("other", "Other", "other.rs", |_, _| {}).adapter("other-adapter"))
            .unwrap();
        let selected = registry
            .filtered(StoryFilter::Story("menu".into()))
            .unwrap();
        assert_eq!(selected.story_ids(), ["menu"]);
        assert_eq!(
            selected.captures().unwrap()[0].filter,
            Some(StoryFilter::Story("menu".into()))
        );
        assert_eq!(
            registry
                .filtered(StoryFilter::Adapter("shared-menu".into()))
                .unwrap()
                .len(),
            1
        );
        assert!(
            registry
                .filtered(StoryFilter::Story("missing".into()))
                .is_err()
        );
    }

    #[test]
    fn registry_rejects_aliases_expired_for_the_current_capture_schema() {
        let mut registry = Registry::default();
        registry.add(story().adapter("shared-menu")).unwrap();
        let alias = |remove_in_version| StoryAlias {
            from: "old-menu".into(),
            to: "menu".into(),
            adapter: "shared-menu".into(),
            remove_in_version,
        };
        assert!(
            registry
                .add_aliases([alias(CAPTURE_SCHEMA_VERSION)])
                .is_err()
        );
        registry
            .add_aliases([alias(CAPTURE_SCHEMA_VERSION + 1)])
            .unwrap();
    }

    #[test]
    fn contract_results_share_the_capture_render_and_assert_final_prefix() {
        let sequence = StorySequence::new("asserted-menu", "Asserted menu", 40, 10)
            .inputs([StoryInput::Retry]);
        let story = Story::replay(sequence, "example.rs", |scene, _| {
            Ok(Buffer::empty(ratatui::layout::Rect::new(
                0,
                0,
                scene.width,
                scene.height,
            )))
        })
        .unwrap()
        .contract(
            StoryContract::new("asserted-menu", "maestro-ui")
                .expect(crate::contract::StoryExpectation::effect_count("retry", 1)),
            |_, inputs, _| {
                Ok((
                    vec![StoryObservation::id("state", "ready")],
                    inputs
                        .iter()
                        .filter(|input| matches!(input, StoryInput::Retry))
                        .map(|_| StoryEffect::new("retry"))
                        .collect(),
                ))
            },
        )
        .unwrap();
        let mut registry = Registry::default();
        registry.add(story).unwrap();
        let results = registry.results().unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].semantic.as_ref().unwrap().passed());
        assert!(
            results[0]
                .semantic
                .as_ref()
                .unwrap()
                .expectations
                .is_empty(),
            "final-state expectations do not apply to earlier prefixes"
        );
        assert!(results[1].semantic.as_ref().unwrap().passed());
        assert_eq!(
            results[1].semantic.as_ref().unwrap().expectations.len(),
            1,
            "the final prefix evaluates the contract"
        );
        assert_eq!(
            results[1].semantic.as_ref().unwrap().effects,
            [StoryEffect::new("retry")]
        );
    }
}
