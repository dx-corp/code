//! Model selector modal
//!
//! Provides a UI for selecting AI models.

use crossterm::event::KeyCode;
use maestro_ui::{ActionPicker, KeyHint, Modal, ModalSize, PickerOptions, PickerStatus};

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::ListItem,
};

pub use crate::model_catalog::{ModelInfo, ModelVerification, available_models};

/// Maximum number of models in the focused slice shown before the
/// "show all models" affordance expands the full catalog.
const FOCUSED_SLICE_LIMIT: usize = 8;

/// A route restored from state can be valid for the provider while absent from
/// the bundled/runtime catalog. Keep that row distinguishable from sourced
/// catalog metadata so the picker can show the active choice safely.
const ACTIVE_ROUTE_SOURCE: &str = "active-route";

/// Current coding models promoted in discovery, independently of saved runtime
/// defaults. IDs are checked against the bundled models.dev snapshot in tests.
const PREFERRED_MODELS: &[(&str, &str)] = &[
    ("openai", "gpt-6-astra"),
    ("anthropic", "claude-fable-5-1"),
    ("openai", "gpt-5.6"),
    ("openai", "gpt-5.6-sol"),
    ("openai", "gpt-5.6-terra"),
    ("openai", "gpt-5.6-luna"),
];

fn discovery_priority(model: &ModelInfo) -> usize {
    let routed = model.provider == "openrouter";
    let (provider, id) = if routed {
        let Some((provider, id)) = model.id.split_once('/') else {
            return PREFERRED_MODELS.len() * 2;
        };
        // OpenRouter uses a dotted release suffix for the same Fable model.
        (
            provider,
            if id == "claude-fable-5.1" {
                "claude-fable-5-1"
            } else {
                id
            },
        )
    } else {
        (model.provider.as_str(), model.id.as_str())
    };
    PREFERRED_MODELS
        .iter()
        .position(|&(preferred_provider, preferred_id)| {
            provider == preferred_provider && id == preferred_id
        })
        .map_or(PREFERRED_MODELS.len() * 2, |rank| {
            rank + if routed { PREFERRED_MODELS.len() } else { 0 }
        })
}

/// Return the route that must be passed to the native agent for a catalog
/// selection. Google and Vertex share Gemini ids, so both rows need an
/// explicit provider qualifier to keep Enter and Ctrl+D selections distinct;
/// other providers retain the historical bare-id behavior.
#[must_use]
pub(crate) fn selection_model_id(model: &ModelInfo) -> String {
    crate::model_catalog::model_route(model)
}

pub(crate) fn canonical_current_route(model_id: &str, models: &[ModelInfo]) -> Option<String> {
    let (provider, bare_id) = model_id
        .split_once('/')
        .map_or((None, model_id), |(provider, model)| {
            (Some(provider), model)
        });
    if let Some(provider) = provider {
        if let Some(descriptor) = crate::ai::ProviderRegistry::descriptor(provider) {
            if let Some(model) = models
                .iter()
                .find(|model| model.id == bare_id && model.provider == descriptor.id)
            {
                return Some(selection_model_id(model));
            }
            if descriptor.id == "openai-codex" {
                if let Some(model) = models
                    .iter()
                    .find(|model| model.id == bare_id && model.provider == "openai")
                {
                    return Some(selection_model_id(model));
                }
            }
            if descriptor.id != "openrouter" {
                if let Some(model) = models
                    .iter()
                    .find(|model| model.provider == "openrouter" && model.id == model_id)
                {
                    return Some(selection_model_id(model));
                }
            }
            return Some(format!("{}/{}", descriptor.id, bare_id));
        }
        if let Some(model) = models
            .iter()
            .find(|model| model.provider == "openrouter" && model.id == model_id)
        {
            return Some(selection_model_id(model));
        }
        return None;
    } else if let Some(model) = models.iter().find(|model| model.id == bare_id) {
        return Some(selection_model_id(model));
    }
    None
}

fn model_matches_current(
    model: &ModelInfo,
    raw_current: &str,
    canonical_current: Option<&str>,
) -> bool {
    let route = selection_model_id(model);
    let provider_route = format!("{}/{}", model.provider, model.id);
    canonical_current == Some(route.as_str())
        || canonical_current == Some(provider_route.as_str())
        || (canonical_current.is_none() && raw_current == model.id && model.provider != "vertex-ai")
}

fn current_model_route(model_id: &str, models: &[ModelInfo]) -> Option<String> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }
    if let Some((_, bare_id)) = model_id.split_once('/') {
        if bare_id.trim().is_empty() {
            return None;
        }
    }
    if let Some(route) = canonical_current_route(model_id, models) {
        return Some(route);
    }

    let descriptor = crate::ai::ProviderRegistry::resolve_descriptor(model_id).ok()?;
    if let Some((_, bare_id)) = model_id.split_once('/') {
        Some(format!("{}/{}", descriptor.id, bare_id.trim()))
    } else {
        Some(model_id.to_owned())
    }
}

fn model_row_identity(model: &ModelInfo, route: Option<&str>) -> String {
    if model.verification.source == ACTIVE_ROUTE_SOURCE {
        format!("active/{}", route.unwrap_or(model.id.as_str()))
    } else {
        format!("{}/{}", model.provider, model.id)
    }
}

fn uncatalogued_model(route: &str) -> Option<ModelInfo> {
    let inspection = crate::model_catalog::inspect_model(route).ok()?;
    if inspection.catalog.is_some() {
        return None;
    }
    let descriptor = crate::ai::ProviderRegistry::resolve_descriptor(route).ok()?;
    let model_id = route.split_once('/').map_or(route, |(_, id)| id).trim();
    if model_id.is_empty() {
        return None;
    }
    let protocol = inspection
        .resolved
        .request_capabilities
        .as_ref()
        .map(|request| match request.protocol {
            crate::ai::OpenAiWireProtocol::OpenAiChat => {
                crate::model_catalog::ModelProtocol::OpenAiChat
            }
            crate::ai::OpenAiWireProtocol::OpenAiResponses => {
                crate::model_catalog::ModelProtocol::OpenAiResponses
            }
        })
        .unwrap_or_else(|| match descriptor.protocol {
            crate::ai::ProviderProtocol::Anthropic => {
                crate::model_catalog::ModelProtocol::Anthropic
            }
            crate::ai::ProviderProtocol::Google | crate::ai::ProviderProtocol::VertexAi => {
                crate::model_catalog::ModelProtocol::Google
            }
            crate::ai::ProviderProtocol::Codex => {
                crate::model_catalog::ModelProtocol::CodexAppServer
            }
            // The shared catalog protocol enum has no separate variants for
            // managed and compatible provider families. Their capabilities
            // remain unknown on this row, so this value is never presented as
            // a support claim to the user.
            crate::ai::ProviderProtocol::OpenAi
            | crate::ai::ProviderProtocol::OpenAiCompatible
            | crate::ai::ProviderProtocol::AzureOpenAi
            | crate::ai::ProviderProtocol::Bedrock
            | crate::ai::ProviderProtocol::Managed => {
                crate::model_catalog::ModelProtocol::OpenAiChat
            }
        });

    Some(ModelInfo {
        id: model_id.to_owned(),
        name: model_id.to_owned(),
        provider: descriptor.id.to_owned(),
        description: maestro_ui::localization::format(
            "Active route {0}; capabilities are not in the catalog",
            &[(route).to_string()],
        ),
        capabilities: crate::model_catalog::ModelCapabilities {
            protocol,
            tools: false,
            vision: false,
            reasoning: false,
            streaming: false,
            context_tokens: 0,
            output_tokens: None,
        },
        verification: ModelVerification {
            state: crate::model_catalog::VerificationState::Unknown,
            source: ACTIVE_ROUTE_SOURCE.to_owned(),
            detail: Some(
                maestro_ui::localization::tr("Capabilities are not in the catalog").to_owned(),
            ),
        },
    })
}

#[derive(Clone)]
struct ModelRow {
    // Provider-qualified catalog identity is distinct from the execution route.
    id: String,
    model_index: Option<usize>,
    /// Explicit execution route for an active row that is not cataloged.
    route: Option<String>,
}

/// Model selector modal state
pub struct ModelSelector {
    /// Stable catalog snapshot used as the base for replacing discovery batches.
    catalog_models: Vec<ModelInfo>,
    /// Available models
    models: Vec<ModelInfo>,
    /// Latest applied local discovery generation.
    discovery_generation: u64,
    /// Shared transient query, selection, and scrolling over ordered result rows.
    picker: ActionPicker<ModelRow>,
    /// Product-owned focused/search result ordering.
    filtered: Vec<usize>,
    /// Current model ID (for highlighting)
    current_model: Option<String>,
    /// Whether the full catalog is shown instead of the focused slice
    show_all: bool,
    /// Whether a "show all N models" affordance row follows the filtered rows
    show_all_affordance: bool,
    /// When false, `show` keeps an injected fixture catalog (tests).
    reload_live_catalog: bool,
    /// User-global, versioned consent; project configuration cannot grant it.
    stealth_models_enabled: bool,
}

impl Default for ModelSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelSelector {
    /// Create a new model selector
    #[must_use]
    pub fn new() -> Self {
        let models = available_models();
        let filtered: Vec<usize> = (0..models.len()).collect();
        Self {
            catalog_models: models.clone(),
            models,
            discovery_generation: 0,
            picker: ActionPicker::new(Vec::new())
                .identified_by(|row: &ModelRow| row.id.as_str())
                .expect("empty model rows are unique")
                // Product filtering orders the focused slice and special row below.
                .matching(|_, _| true),
            filtered,
            current_model: None,
            show_all: false,
            show_all_affordance: false,
            reload_live_catalog: true,
            stealth_models_enabled: maestro_local_host::stealth_models::permits_current(),
        }
    }

    #[cfg(test)]
    fn with_models(models: Vec<ModelInfo>) -> Self {
        let mut selector = Self::new();
        selector.catalog_models = models.clone();
        selector.models = models;
        selector.reload_live_catalog = false;
        selector.stealth_models_enabled = false;
        selector.filter();
        selector
    }

    #[cfg(test)]
    fn set_stealth_models_enabled_for_test(&mut self, enabled: bool) {
        self.stealth_models_enabled = enabled;
        self.filter();
    }

    /// Set the current model (for highlighting)
    pub fn set_current_model(&mut self, model_id: Option<String>) {
        self.current_model = model_id;
        if self.picker.is_open() {
            self.filter();
        }
    }

    /// Apply verification to the matching catalog entry.
    pub fn set_verification(
        &mut self,
        model_id: &str,
        mut verification: ModelVerification,
    ) -> bool {
        let Some(catalog_model) = crate::model_catalog::find_model(model_id) else {
            return false;
        };
        let Some(model) = self
            .models
            .iter_mut()
            .find(|model| model.id == catalog_model.id && model.provider == catalog_model.provider)
        else {
            return false;
        };
        // A registry-only auth check is weaker than a successful live local
        // discovery. Preserve the live source so the discovery label and active
        // row retention continue to reflect the strongest evidence.
        if model.verification.source == "local-runtime"
            && verification.state == crate::model_catalog::VerificationState::Verified
            && verification.source == "provider-registry"
        {
            if model.verification.state != crate::model_catalog::VerificationState::Verified {
                return false;
            }
            verification.source.clone_from(&model.verification.source);
            if verification.detail.is_none() {
                verification.detail.clone_from(&model.verification.detail);
            }
        }
        if model.verification == verification {
            return false;
        }
        model.verification = verification;
        if let Some(base) = self
            .catalog_models
            .iter_mut()
            .find(|model| model.id == catalog_model.id && model.provider == catalog_model.provider)
        {
            base.verification = model.verification.clone();
        }
        true
    }

    /// Replace the complete discovered-model snapshot. Older batches are
    /// ignored, catalog duplicates are replaced in place, and the selected
    /// route remains stable when it still exists.
    pub fn replace_discovered_models(
        &mut self,
        generation: u64,
        discovered: Vec<ModelInfo>,
    ) -> bool {
        if generation <= self.discovery_generation {
            return false;
        }
        let active_discovered_model = self.current_model.as_deref().and_then(|current| {
            let current_route = canonical_current_route(current, &self.models)?;
            self.models
                .iter()
                .find(|model| {
                    model.verification.source == "local-runtime"
                        && selection_model_id(model) == current_route
                })
                .cloned()
        });
        self.discovery_generation = generation;
        self.models = self.catalog_models.clone();
        for discovered_model in discovered {
            if let Some(existing) = self.models.iter_mut().find(|model| {
                model.provider == discovered_model.provider && model.id == discovered_model.id
            }) {
                *existing = discovered_model;
            } else {
                self.models.push(discovered_model);
            }
        }
        if let Some(mut active_model) = active_discovered_model {
            let active_is_still_discovered = self.models.iter().any(|model| {
                model.provider == active_model.provider
                    && model.id == active_model.id
                    && model.verification.source == "local-runtime"
            });
            if !active_is_still_discovered {
                active_model.verification.state =
                    crate::model_catalog::VerificationState::Unavailable;
                let unavailable_detail = maestro_ui::localization::tr(
                    "Not reported by the local runtime on the latest refresh",
                );
                match active_model.verification.detail.as_mut() {
                    Some(detail) if !detail.contains(unavailable_detail) => {
                        detail.push_str("; ");
                        detail.push_str(unavailable_detail);
                    }
                    Some(_) => {}
                    None => {
                        active_model.verification.detail = Some(unavailable_detail.to_owned());
                    }
                }
                if let Some(catalog_row) = self.models.iter_mut().find(|model| {
                    model.provider == active_model.provider && model.id == active_model.id
                }) {
                    *catalog_row = active_model;
                } else {
                    self.models.push(active_model);
                }
            }
        }
        self.filter();
        true
    }

    /// Show the modal
    pub fn show(&mut self) {
        if self.reload_live_catalog {
            self.stealth_models_enabled = maestro_local_host::stealth_models::permits_current();
        }
        self.reload_catalog_from_cache();
        self.picker.open();
        self.show_all = false;
        self.filter();
        // Keep the active choice selected even when newer recommendations lead
        // discovery. Opening and confirming must not silently change models.
        let current = self.current_model.as_deref();
        let canonical = current.and_then(|id| canonical_current_route(id, &self.models));
        let active = self.filtered.iter().copied().find(|&index| {
            current.is_some_and(|id| {
                model_matches_current(&self.models[index], id, canonical.as_deref())
            })
        });
        if let Some(index) = active.or_else(|| self.filtered.first().copied()) {
            let model = &self.models[index];
            let route = (model.verification.source == ACTIVE_ROUTE_SOURCE)
                .then_some(current.unwrap_or_default());
            self.picker.select_id(&model_row_identity(model, route));
        }
    }

    /// Pick up a completed background catalog refresh without dropping local
    /// discovery rows. `available_models` also schedules the next refresh.
    fn reload_catalog_from_cache(&mut self) {
        if !self.reload_live_catalog {
            return;
        }
        let catalog = available_models();
        if catalog == self.catalog_models {
            return;
        }
        let discovered: Vec<ModelInfo> = self
            .models
            .iter()
            .filter(|model| model.verification.source == "local-runtime")
            .cloned()
            .collect();
        self.catalog_models = catalog;
        self.models = self.catalog_models.clone();
        for discovered_model in discovered {
            if let Some(existing) = self.models.iter_mut().find(|model| {
                model.provider == discovered_model.provider && model.id == discovered_model.id
            }) {
                *existing = discovered_model;
            } else {
                self.models.push(discovered_model);
            }
        }
    }

    /// Hide the modal
    pub fn hide(&mut self) {
        self.picker.close();
    }

    /// Check if visible
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.picker.is_open()
    }

    /// Edit through the shared Unicode cursor path, then apply product ordering.
    pub fn insert_char(&mut self, c: char) {
        self.picker.handle_key(KeyCode::Char(c), false);
        self.filter();
    }
    pub fn insert_str(&mut self, text: &str) {
        self.picker.insert_str(text);
        self.filter();
    }
    pub fn backspace(&mut self) {
        self.picker.handle_key(KeyCode::Backspace, false);
        self.filter();
    }
    pub fn move_left(&mut self) {
        self.picker.handle_key(KeyCode::Left, false);
    }
    pub fn move_right(&mut self) {
        self.picker.handle_key(KeyCode::Right, false);
    }
    pub fn move_up(&mut self) {
        self.picker.handle_key(KeyCode::Up, false);
    }
    pub fn move_down(&mut self) {
        self.picker.handle_key(KeyCode::Down, false);
    }

    /// Whether the selection is on the "show all models" affordance row.
    #[must_use]
    pub fn selected_show_all(&self) -> bool {
        self.picker
            .selected()
            .is_some_and(|row| row.model_index.is_none())
    }

    /// Toggle between the focused slice and the full catalog.
    pub fn toggle_show_all(&mut self) {
        self.show_all = !self.show_all;
        self.filter();
    }

    /// Get the selected model
    #[must_use]
    pub fn selected_model(&self) -> Option<&ModelInfo> {
        self.picker
            .selected()
            .and_then(|row| row.model_index)
            .and_then(|idx| self.models.get(idx))
    }

    /// Get the selected model route, preserving provider identity for shared
    /// Google/Vertex model ids and retaining an active uncatalogued route.
    /// Used by both Enter and Ctrl+D paths.
    #[must_use]
    pub fn selected_model_id(&self) -> Option<String> {
        self.picker.selected().and_then(|row| {
            row.model_index
                .and_then(|idx| self.models.get(idx))
                .map(|model| {
                    row.route
                        .clone()
                        .unwrap_or_else(|| selection_model_id(model))
                })
        })
    }

    /// Confirm selection and return the model ID. Confirming the "show all
    /// models" affordance row expands the full catalog instead of closing.
    pub fn confirm(&mut self) -> Option<String> {
        if self.selected_show_all() {
            self.toggle_show_all();
            return None;
        }
        let id = self.selected_model_id();
        self.hide();
        id
    }

    /// Filter models based on query
    fn filter(&mut self) {
        self.ensure_active_route_row();
        let query = self.picker.query().to_lowercase();
        let current_route = self.current_model.as_deref();
        let mut full: Vec<usize> = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                if !self.model_visible(m) {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                m.id.to_lowercase().contains(&query)
                    || m.name.to_lowercase().contains(&query)
                    || m.provider.to_lowercase().contains(&query)
                    || format!("{}/{}", m.provider, m.id)
                        .to_lowercase()
                        .contains(&query)
                    || (m.verification.source == ACTIVE_ROUTE_SOURCE
                        && current_route.is_some_and(|route| route.to_lowercase().contains(&query)))
                    || model_status_summary(m).to_lowercase().contains(&query)
                    || capability_summary(m).to_lowercase().contains(&query)
                    || crate::palette_resource::PaletteResource::from(*m).matches(&query)
            })
            .map(|(i, _)| i)
            .collect();
        full.sort_by_key(|&index| discovery_priority(&self.models[index]));

        // With an empty query show the focused slice plus a "show all"
        // affordance; any search or an explicit expansion lists everything.
        if query.is_empty() && !self.show_all {
            self.filtered = self.focused_slice();
            self.show_all_affordance = full.len() > self.filtered.len();
        } else {
            self.filtered = full;
            self.show_all_affordance = false;
        }

        let mut rows: Vec<_> = self
            .filtered
            .iter()
            .map(|&index| {
                let model = &self.models[index];
                let route = (model.verification.source == ACTIVE_ROUTE_SOURCE)
                    .then(|| current_route.map(str::to_owned))
                    .flatten();
                ModelRow {
                    id: model_row_identity(model, route.as_deref()),
                    model_index: Some(index),
                    route,
                }
            })
            .collect();
        if self.show_all_affordance {
            rows.push(ModelRow {
                id: "show-all".into(),
                model_index: None,
                route: None,
            });
        }
        self.picker.set_status(PickerStatus::Ready);
        if let Err(error) = self.picker.replace_items(rows) {
            self.picker
                .set_status(PickerStatus::Error(maestro_ui::localization::format(
                    "Could not update models: {0}",
                    &[(error).to_string()],
                )));
        }
    }

    /// Retain a valid active route when the catalog has no metadata for it.
    /// The provider registry validates the route; capabilities stay unknown.
    fn ensure_active_route_row(&mut self) {
        let Some(current) = self.current_model.as_deref() else {
            self.models
                .retain(|model| model.verification.source != ACTIVE_ROUTE_SOURCE);
            return;
        };
        let canonical = canonical_current_route(current, &self.models);
        let catalog_match = self.models.iter().any(|model| {
            model.verification.source != ACTIVE_ROUTE_SOURCE
                && model_matches_current(model, current, canonical.as_deref())
        });
        if !catalog_match
            && self.models.iter().any(|model| {
                model.verification.source == ACTIVE_ROUTE_SOURCE
                    && model_matches_current(model, current, canonical.as_deref())
            })
        {
            return;
        }

        self.models
            .retain(|model| model.verification.source != ACTIVE_ROUTE_SOURCE);
        let Some(route) = current_model_route(current, &self.models) else {
            return;
        };
        if let Some(model) = uncatalogued_model(&route) {
            self.models.push(model);
        }
    }

    /// Preferred models, current model, local discoveries, then provider defaults.
    /// Recommendations, the active row, and discoveries survive the normal cap.
    fn focused_slice(&self) -> Vec<usize> {
        let mut slice: Vec<usize> = Vec::new();
        for &(provider, id) in PREFERRED_MODELS {
            if let Some(index) = self.models.iter().position(|model| {
                self.model_visible(model) && model.provider == provider && model.id == id
            }) {
                slice.push(index);
            }
        }
        if let Some(current) = &self.current_model {
            let canonical_current = canonical_current_route(current, &self.models);
            if let Some(idx) = self.models.iter().position(|model| {
                self.model_visible(model)
                    && model_matches_current(model, current, canonical_current.as_deref())
            }) {
                if !slice.contains(&idx) {
                    slice.push(idx);
                }
            }
        }
        for (idx, model) in self.models.iter().enumerate() {
            if self.model_visible(model)
                && model.verification.source == "local-runtime"
                && !slice.contains(&idx)
            {
                slice.push(idx);
            }
        }
        let retained_rows = slice.len();
        for provider in crate::model_catalog::MODEL_SELECTOR_PROVIDERS {
            let Some(default_id) = crate::model_catalog::default_model_for_provider(provider)
            else {
                continue;
            };
            if let Some(idx) = self.models.iter().position(|model| {
                self.model_visible(model) && model.id == default_id && model.provider == *provider
            }) {
                if !slice.contains(&idx) {
                    slice.push(idx);
                }
            }
        }
        slice.truncate(FOCUSED_SLICE_LIMIT.max(retained_rows));
        slice
    }

    fn model_visible(&self, model: &ModelInfo) -> bool {
        self.stealth_models_enabled
            || !maestro_local_host::stealth_models::is_openrouter_stealth_model(
                &model.provider,
                &model.id,
            )
    }

    pub(crate) fn cycle_routes(&self) -> Vec<String> {
        self.focused_slice()
            .into_iter()
            .map(|index| {
                let model = &self.models[index];
                if model.verification.source == ACTIVE_ROUTE_SOURCE {
                    self.current_model
                        .clone()
                        .unwrap_or_else(|| selection_model_id(model))
                } else {
                    selection_model_id(model)
                }
            })
            .collect()
    }

    /// Render the modal
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.picker.is_open() {
            return;
        }

        let theme = crate::themes::current_ui_theme();
        let inner = Modal::sized(
            maestro_ui::localization::tr("Select Model"),
            ModalSize::Standard,
        )
        .theme(theme)
        .render(frame, area);

        // Model list
        let canonical_current = self
            .current_model
            .as_deref()
            .and_then(|current| canonical_current_route(current, &self.models));
        let models = &self.models;
        let current_model = &self.current_model;
        let visible_model_count = self
            .models
            .iter()
            .filter(|model| self.model_visible(model))
            .count();
        self.picker.render(
            frame,
            inner,
            theme,
            PickerOptions {
                placeholder: maestro_ui::localization::tr("Type to filter models..."),
                empty: maestro_ui::localization::tr("No matching models"),
                hints: Some(&[
                    KeyHint::new("Enter", maestro_ui::localization::tr("select")),
                    KeyHint::new("Esc", maestro_ui::localization::tr("cancel")),
                    KeyHint::new("Tab", "all"),
                    KeyHint::new("Ctrl+D", "default"),
                ]),
                ..PickerOptions::default()
            },
            |row| {
                let Some(index) = row.model_index else {
                    return ListItem::new(Line::from(Span::styled(
                        maestro_ui::localization::format(
                            "… show all {0} models (Tab)",
                            &[(visible_model_count).to_string()],
                        ),
                        Style::default().fg(theme.focus),
                    )));
                };
                let model = &models[index];
                let is_current = current_model.as_deref().is_some_and(|current| {
                    model_matches_current(model, current, canonical_current.as_deref())
                });

                let mut spans = vec![
                    Span::styled(&model.name, Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!(" ({}) ", model_route_summary(model)),
                        theme.muted_style(),
                    ),
                ];

                if is_current {
                    spans.push(Span::styled("*", Style::default().fg(theme.success)));
                }

                // Keep availability and capabilities off the name line so long
                // model/provider names cannot hide them at standard terminal widths.
                let status = Line::from(Span::styled(
                    format!(
                        "  {} · {}",
                        model_status_summary(model),
                        format_context_window(model.capabilities.context_tokens)
                    ),
                    theme.muted_style(),
                ));
                let capabilities = Line::from(Span::styled(
                    format!("  {}", capability_summary(model)),
                    theme.muted_style(),
                ));

                ListItem::new(vec![Line::from(spans), status, capabilities])
            },
        );
    }
}

/// Compact context window label: `1M ctx` for exact millions, `200k ctx`
/// otherwise.
fn format_context_window(context_tokens: u32) -> String {
    if context_tokens == 0 {
        maestro_ui::localization::tr("unknown ctx").to_owned()
    } else if context_tokens >= 1_000_000 && context_tokens.is_multiple_of(1_000_000) {
        maestro_ui::localization::format("{0}M ctx", &[(context_tokens / 1_000_000).to_string()])
    } else {
        maestro_ui::localization::format("{0}k ctx", &[(context_tokens / 1000).to_string()])
    }
}

fn model_route_summary(model: &ModelInfo) -> String {
    if let Some(managed) = crate::model_catalog::MANAGED_MODELS
        .iter()
        .find(|entry| model.provider == "evalops" && entry.id == model.id)
    {
        let provider = match managed.provider {
            "fireworks" => "Fireworks",
            "google" => "Google",
            other => other,
        };
        return format!(
            "{provider} · {}",
            maestro_ui::localization::tr("Model credits")
        );
    }
    model.provider.clone()
}

fn model_status_summary(model: &ModelInfo) -> &'static str {
    if maestro_local_host::stealth_models::is_openrouter_stealth_model(&model.provider, &model.id) {
        return "Stealth · experimental";
    }
    use crate::model_catalog::VerificationState;
    match (model.verification.source.as_str(), model.verification.state) {
        ("local-runtime", VerificationState::Verified) => "Local · detected",
        ("local-runtime", VerificationState::Unavailable) => "Local · unavailable",
        ("local-runtime", _) => "Local · unknown",
        (ACTIVE_ROUTE_SOURCE, VerificationState::Unknown) => "Active · uncataloged",
        (_, VerificationState::Unavailable) => "Catalog · unavailable",
        // Provider authentication alone does not prove model availability.
        _ => maestro_ui::localization::tr("Catalog · availability unchecked"),
    }
}

fn capability_summary(model: &ModelInfo) -> String {
    let unknown = model
        .verification
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("not in the catalog"));
    if unknown {
        return maestro_ui::localization::tr("Capabilities: unknown (not in catalog)").to_owned();
    }
    let supported = |value| if value { "yes" } else { "no" };
    maestro_ui::localization::format(
        "Tools: {0} · Images: {1} · Reasoning: {2}",
        &[
            (supported(model.capabilities.tools)).to_string(),
            (supported(model.capabilities.vision)).to_string(),
            (supported(model.capabilities.reasoning)).to_string(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stealth_models_are_hidden_until_consent_and_keep_a_warning_badge() {
        use ratatui::{Terminal, backend::TestBackend};
        let models = vec![
            test_model("openai/gpt-5.6", "openrouter"),
            test_model("stealth/union-alphax", "OpenRouter"),
        ];
        let mut selector = ModelSelector::with_models(models);
        selector.set_current_model(Some("OpenRouter/stealth/union-alphax".to_owned()));
        selector.show();
        selector.insert_str("stealth");
        assert!(selector.filtered.is_empty());
        assert!(
            selector
                .cycle_routes()
                .iter()
                .all(|route| !route.contains("/stealth/"))
        );

        selector.set_stealth_models_enabled_for_test(true);
        selector.show();
        selector.insert_str("stealth");
        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("OpenRouter/stealth/union-alphax")
        );
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|frame| selector.render(frame, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Stealth · experimental"), "{text}");
    }

    #[test]
    fn managed_model_picker_shows_funding_and_upstream_without_changing_route() {
        use ratatui::{Terminal, backend::TestBackend};
        for (id, provider) in [
            ("gemini-3.8-flash", "Google"),
            ("accounts/fireworks/models/qwen3p8-max", "Fireworks"),
        ] {
            let mut selector = ModelSelector::new();
            selector.show();
            let route = format!("evalops/{id}");
            selector.insert_str(&route);
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| selector.render(frame, frame.area()))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(
                text.contains(&format!("{provider} · Model credits")),
                "{text}"
            );
            assert!(text.contains("availability unchecked"), "{text}");
            assert_eq!(selector.confirm(), Some(route));
        }
    }

    #[test]
    fn managed_model_search_selects_exact_gateway_routes() {
        for model in crate::model_catalog::MANAGED_MODELS {
            let mut selector = ModelSelector::new();
            selector.set_current_model(Some(
                crate::credential_mode::DEFAULT_MANAGED_MODEL.to_owned(),
            ));
            selector.show();
            assert_eq!(
                selector.confirm().as_deref(),
                Some(crate::credential_mode::DEFAULT_MANAGED_MODEL)
            );
            selector.show();
            selector.insert_str(&format!("evalops/{}", model.id));
            assert_eq!(selector.confirm(), Some(format!("evalops/{}", model.id)));
        }
    }

    #[test]
    fn model_selector_shared_picker_renders_empty_query_result_and_help() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut selector = ModelSelector::new();
        selector.show();
        selector.insert_str("no-such-result-zzz");
        let before = selector.picker.query().to_owned();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| selector.render(frame, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("No matching models"));
        assert!(text.contains("Ctrl+D default"));
        assert_eq!(selector.picker.query(), before);
    }

    #[test]
    fn test_model_selector_creation() {
        let selector = ModelSelector::new();
        assert!(!selector.is_visible());
        assert!(!selector.models.is_empty());
    }

    #[test]
    fn test_model_selector_show_hide() {
        let mut selector = ModelSelector::new();
        selector.show();
        assert!(selector.is_visible());
        selector.hide();
        assert!(!selector.is_visible());
    }

    #[test]
    fn test_model_selector_insert_str() {
        let mut selector = ModelSelector::new();
        selector.show();
        selector.insert_str("claude");
        assert_eq!(selector.picker.query(), "claude");
        selector.move_left();
        selector.insert_str("-");
        assert_eq!(selector.picker.query(), "claud-e");
    }

    #[test]
    fn test_model_selector_filter() {
        let mut selector = ModelSelector::new();
        selector.show();

        // Filter for Claude
        selector.insert_char('c');
        selector.insert_char('l');
        selector.insert_char('a');
        selector.insert_char('u');
        selector.insert_char('d');
        selector.insert_char('e');

        // Matches any catalog field the filter searches, including OpenRouter
        // vendor ids such as `anthropic/claude-sonnet-4.5`.
        assert!(!selector.filtered.is_empty());
        for &idx in &selector.filtered {
            let model = &selector.models[idx];
            let haystack = format!(
                "{} {} {}",
                model.id.to_lowercase(),
                model.name.to_lowercase(),
                model.provider.to_lowercase()
            );
            assert!(
                haystack.contains("claude")
                    || model_status_summary(model)
                        .to_lowercase()
                        .contains("claude")
                    || crate::palette_resource::PaletteResource::from(model).matches("claude"),
                "filtered model {} / {} must match query claude",
                model.provider,
                model.id
            );
        }
    }

    #[test]
    fn test_model_selector_navigation() {
        let mut selector = ModelSelector::new();
        selector.show();

        assert_eq!(
            selector.selected_model_id(),
            Some(selection_model_id(&selector.models[selector.filtered[0]]))
        );
        selector.move_down();
        assert_eq!(
            selector.selected_model_id(),
            Some(selection_model_id(&selector.models[selector.filtered[1]]))
        );
        selector.move_up();
        assert_eq!(
            selector.selected_model_id(),
            Some(selection_model_id(&selector.models[selector.filtered[0]]))
        );
    }

    #[test]
    fn test_model_selector_confirm() {
        let mut selector = ModelSelector::new();
        selector.show();

        let model_id = selector.confirm();
        assert!(model_id.is_some());
        assert!(!selector.is_visible());
    }

    #[test]
    fn duplicate_google_and_vertex_rows_keep_provider_qualified_selection() {
        let mut selector = ModelSelector::with_models(vec![
            test_model("gemini-2.5-pro", "google"),
            test_model("gemini-2.5-pro", "vertex-ai"),
        ]);
        selector.show();

        assert_eq!(
            selector.confirm().as_deref(),
            Some("google/gemini-2.5-pro"),
            "Enter must preserve the Google route"
        );

        selector.show();
        selector.move_down();
        assert_eq!(
            selector.confirm().as_deref(),
            Some("vertex-ai/gemini-2.5-pro"),
            "Enter must preserve the Vertex route"
        );
    }

    #[test]
    fn ctrl_d_selection_route_preserves_vertex_provider_for_persistence() {
        let mut selector = ModelSelector::with_models(vec![
            test_model("gemini-2.5-pro", "google"),
            test_model("gemini-2.5-pro", "vertex-ai"),
        ]);
        selector.show();

        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("google/gemini-2.5-pro")
        );
        selector.move_down();
        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("vertex-ai/gemini-2.5-pro")
        );
    }

    #[test]
    fn selection_route_preserves_llamacpp_provider() {
        let model = crate::model_catalog::find_model("llamacpp/Qwen3.8-27B")
            .expect("local Qwen catalog row");

        assert_eq!(selection_model_id(&model), "llamacpp/Qwen3.8-27B");
    }

    #[test]
    fn selection_routes_preserve_every_local_provider() {
        for provider in ["llamacpp", "lmstudio", "ollama"] {
            let model = test_model("local-model", provider);
            assert_eq!(
                selection_model_id(&model),
                format!("{provider}/local-model")
            );
        }
    }

    #[test]
    fn focused_slice_canonicalizes_current_google_and_vertex_aliases() {
        for (current, expected_provider) in [
            ("vertex-ai/gemini-2.5-pro", "vertex-ai"),
            ("vertex/gemini-2.5-pro", "vertex-ai"),
            ("google/gemini-2.5-pro", "google"),
            ("gemini/gemini-2.5-pro", "google"),
            ("gemini-2.5-pro", "google"),
        ] {
            let mut selector = ModelSelector::with_models(vec![
                test_model("gemini-2.5-pro", "google"),
                test_model("gemini-2.5-pro", "vertex-ai"),
            ]);
            selector.set_current_model(Some(current.to_owned()));
            selector.show();

            let first = selector.filtered.first().map(|&idx| &selector.models[idx]);
            assert_eq!(
                first.map(|model| model.provider.as_str()),
                Some(expected_provider),
                "current route {current} must lead its canonical provider row"
            );
        }
    }

    #[test]
    fn uncatalogued_active_route_is_retained_and_searchable_as_unknown() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        selector.set_current_model(Some("openai/future-custom-model".to_owned()));
        selector.show();

        let active = selector.selected_model().expect("active route row");
        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("openai/future-custom-model")
        );
        assert_eq!(active.provider, "openai");
        assert_eq!(active.id, "future-custom-model");
        assert_eq!(
            active.verification.state,
            crate::model_catalog::VerificationState::Unknown
        );
        assert_eq!(active.verification.source, ACTIVE_ROUTE_SOURCE);
        assert_eq!(active.capabilities.context_tokens, 0);
        assert_eq!(model_status_summary(active), "Active · uncataloged");
        assert_eq!(
            capability_summary(active),
            "Capabilities: unknown (not in catalog)"
        );

        selector.insert_str("openai/future-custom-model");
        assert_eq!(selector.filtered.len(), 1);
        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("openai/future-custom-model")
        );
    }

    #[test]
    fn verification_updates_matching_catalog_model_only() {
        let mut selector = ModelSelector::new();
        let verification = ModelVerification {
            state: crate::model_catalog::VerificationState::Verified,
            source: "test".to_owned(),
            detail: None,
        };
        assert!(selector.set_verification("openai/gpt-4o", verification.clone()));
        assert_eq!(
            selector
                .models
                .iter()
                .find(|model| model.id == "gpt-4o")
                .map(|model| &model.verification),
            Some(&verification)
        );
        assert!(!selector.set_verification("anthropic/gpt-4o", verification));
    }

    #[test]
    fn verification_updates_vertex_row_without_touching_google_row() {
        let mut selector = ModelSelector::new();
        let verification = ModelVerification {
            state: crate::model_catalog::VerificationState::Verified,
            source: "test".to_owned(),
            detail: None,
        };
        assert!(selector.set_verification("vertex/gemini-2.5-pro", verification.clone()));
        assert_eq!(
            selector
                .models
                .iter()
                .find(|model| model.provider == "vertex-ai" && model.id == "gemini-2.5-pro")
                .map(|model| &model.verification),
            Some(&verification)
        );
        assert_ne!(
            selector
                .models
                .iter()
                .find(|model| model.provider == "google" && model.id == "gemini-2.5-pro")
                .map(|model| &model.verification),
            Some(&verification)
        );
    }

    #[test]
    fn offline_registry_verification_does_not_replace_live_local_evidence() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        let mut discovered = test_model("active-local", "llamacpp");
        discovered.verification = ModelVerification {
            state: crate::model_catalog::VerificationState::Verified,
            source: "local-runtime".to_owned(),
            detail: Some("live discovery".to_owned()),
        };
        assert!(selector.replace_discovered_models(1, vec![discovered]));

        let changed = selector.set_verification(
            "llamacpp/active-local",
            ModelVerification {
                state: crate::model_catalog::VerificationState::Verified,
                source: "provider-registry".to_owned(),
                detail: None,
            },
        );

        assert!(!changed);
        let model = selector
            .models
            .iter()
            .find(|model| selection_model_id(model) == "llamacpp/active-local")
            .expect("discovered model");
        assert_eq!(model.verification.source, "local-runtime");
        assert_eq!(model.verification.detail.as_deref(), Some("live discovery"));
    }

    #[test]
    fn offline_registry_verification_does_not_revive_unavailable_local_evidence() {
        let mut selector = ModelSelector::new();
        let mut active = crate::model_catalog::find_model("llamacpp/Qwen3.8-27B")
            .expect("built-in local Qwen row");
        active.verification = ModelVerification {
            state: crate::model_catalog::VerificationState::Verified,
            source: "local-runtime".to_owned(),
            detail: Some("live discovery".to_owned()),
        };
        assert!(selector.replace_discovered_models(1, vec![active]));
        selector.set_current_model(Some("llamacpp/Qwen3.8-27B".to_owned()));
        assert!(selector.replace_discovered_models(2, vec![]));

        assert!(!selector.set_verification(
            "llamacpp/Qwen3.8-27B",
            ModelVerification {
                state: crate::model_catalog::VerificationState::Verified,
                source: "provider-registry".to_owned(),
                detail: None,
            },
        ));

        let retained = selector
            .models
            .iter()
            .find(|model| model.provider == "llamacpp" && model.id == "Qwen3.8-27B")
            .expect("retained local Qwen row");
        assert_eq!(
            retained.verification.state,
            crate::model_catalog::VerificationState::Unavailable
        );
        assert_eq!(retained.verification.source, "local-runtime");
    }

    fn test_model(id: &str, provider: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_owned(),
            name: id.to_owned(),
            provider: provider.to_owned(),
            description: format!("{id} description"),
            capabilities: crate::model_catalog::ModelCapabilities {
                protocol: crate::model_catalog::ModelProtocol::OpenAiChat,
                tools: true,
                vision: false,
                reasoning: false,
                streaming: true,
                context_tokens: 200_000,
                output_tokens: None,
            },
            verification: ModelVerification::catalog(),
        }
    }

    /// Provider defaults plus filler models. OpenAI's catalog default is also
    /// a preferred discovery row; tests that pin slice order keep a non-default
    /// OpenAI id here and add `gpt-5.6` only where that default is under test.
    fn slice_catalog() -> Vec<ModelInfo> {
        let mut models = vec![
            test_model("claude-sonnet-4-6", "anthropic"),
            test_model("gemini-2.5-pro", "google"),
            test_model("gemini-2.5-pro", "vertex-ai"),
            test_model("gpt-5.5", "openai"),
            test_model("grok-4.5", "xai"),
        ];
        for index in 0..12 {
            models.push(test_model(&format!("filler-{index}"), "openai"));
        }
        models
    }

    #[test]
    fn focused_slice_shows_current_and_provider_defaults() {
        let mut models = slice_catalog();
        models.push(test_model("gpt-5.6", "openai"));
        let mut selector = ModelSelector::with_models(models);
        selector.set_current_model(Some("grok-4.5".to_owned()));
        selector.show();

        let ids: Vec<&str> = selector
            .filtered
            .iter()
            .map(|&idx| selector.models[idx].id.as_str())
            .collect();
        assert_eq!(
            ids[0], "gpt-5.6",
            "preferred openai default leads the slice"
        );
        assert!(
            ids.contains(&"grok-4.5"),
            "current model stays in the slice"
        );
        for default in ["claude-sonnet-4-6", "gemini-2.5-pro", "gpt-5.6"] {
            assert!(ids.contains(&default), "slice must include {default}");
        }
        assert!(
            !ids.contains(&"gpt-5.5"),
            "retired openai default is not a focused-slice row"
        );
        let default_providers: Vec<&str> = selector
            .filtered
            .iter()
            .map(|&idx| selector.models[idx].provider.as_str())
            .collect();
        assert!(default_providers.contains(&"vertex-ai"));
        assert!(ids.len() <= FOCUSED_SLICE_LIMIT);
        assert!(!ids.iter().any(|id| id.starts_with("filler-")));
        assert!(selector.show_all_affordance);
    }

    #[test]
    fn newer_models_lead_discovery_without_replacing_the_active_choice() {
        let mut models = vec![test_model("gpt-4o", "openai")];
        models.extend(
            PREFERRED_MODELS
                .iter()
                .rev()
                .map(|&(provider, id)| test_model(id, provider)),
        );
        let mut selector = ModelSelector::with_models(models);
        let discovered = (0..10)
            .map(|index| {
                let mut model = test_model(&format!("local-{index}"), "ollama");
                model.verification.source = "local-runtime".to_owned();
                model
            })
            .collect();
        selector.replace_discovered_models(1, discovered);
        selector.set_current_model(Some("gpt-4o".to_owned()));
        selector.show();

        let ids: Vec<_> = selector
            .filtered
            .iter()
            .map(|&index| selector.models[index].id.as_str())
            .collect();
        assert_eq!(
            &ids[..6],
            &[
                "gpt-6-astra",
                "claude-fable-5-1",
                "gpt-5.6",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
            ]
        );
        assert_eq!(ids.iter().filter(|id| id.starts_with("local-")).count(), 10);
        assert_eq!(selector.confirm().as_deref(), Some("gpt-4o"));

        selector.show();
        selector.toggle_show_all();
        assert_eq!(selector.models[selector.filtered[0]].id, "gpt-6-astra");
        selector.insert_str("gpt");
        assert_eq!(selector.models[selector.filtered[0]].id, "gpt-6-astra");
        assert_eq!(
            selector.models[*selector.filtered.last().unwrap()].id,
            "gpt-4o"
        );
    }

    #[test]
    fn preferred_models_exist_in_the_bundled_catalog() {
        let bundled: serde_json::Value = serde_json::from_str(include_str!(
            "../../../local-host-rs/src/model_catalog_data.json"
        ))
        .unwrap();
        for &(provider, id) in PREFERRED_MODELS {
            assert!(
                bundled["models"].as_array().unwrap().iter().any(|model| {
                    model["provider"].as_str() == Some(provider) && model["id"].as_str() == Some(id)
                }),
                "preferred model {provider}/{id} must have sourced catalog metadata"
            );
        }
    }

    #[test]
    fn provider_filtered_search_prioritizes_current_openrouter_routes() {
        let mut selector = ModelSelector::with_models(vec![
            test_model("openai/gpt-4o", "openrouter"),
            test_model("openai/gpt-5.6-sol", "openrouter"),
            test_model("anthropic/claude-fable-5.1", "openrouter"),
            test_model("openai/gpt-6-astra", "openrouter"),
            test_model("gpt-6-astra", "openai"),
        ]);
        selector.show();
        selector.insert_str("openrouter");
        let ids: Vec<_> = selector
            .filtered
            .iter()
            .map(|&index| selector.models[index].id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "openai/gpt-6-astra",
                "anthropic/claude-fable-5.1",
                "openai/gpt-5.6-sol",
                "openai/gpt-4o"
            ]
        );
    }

    #[test]
    fn focused_slice_keeps_current_model_ahead_of_discovered_models() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        let discovered = (0..10)
            .map(|index| {
                let mut model = test_model(&format!("local-{index}"), "ollama");
                model.verification = ModelVerification {
                    state: crate::model_catalog::VerificationState::Verified,
                    source: "local-runtime".to_owned(),
                    detail: None,
                };
                model
            })
            .collect();
        assert!(selector.replace_discovered_models(1, discovered));
        selector.set_current_model(Some("grok-4.5".to_owned()));
        selector.show();

        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("grok-4.5"),
            "opening and confirming the selector must preserve the active route"
        );
        assert_eq!(selector.filtered.len(), 11);
    }

    #[test]
    fn show_all_toggle_expands_and_collapses_full_catalog() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        selector.show();
        let total = selector.models.len();
        assert!(selector.filtered.len() < total);

        selector.toggle_show_all();
        assert_eq!(selector.filtered.len(), total);
        assert!(!selector.show_all_affordance);

        selector.toggle_show_all();
        assert!(selector.filtered.len() < total);
        assert!(selector.show_all_affordance);
    }

    #[test]
    fn confirm_on_show_all_row_expands_instead_of_closing() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        selector.show();
        while !selector.selected_show_all() {
            selector.move_down();
        }

        assert!(selector.confirm().is_none());
        assert!(selector.is_visible(), "expansion keeps the modal open");
        assert_eq!(selector.filtered.len(), selector.models.len());
        assert!(!selector.show_all_affordance);
    }

    #[test]
    fn search_bypasses_slice_and_hides_affordance() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        selector.show();
        selector.insert_str("filler");

        assert!(!selector.show_all_affordance);
        assert_eq!(selector.filtered.len(), 12);
        for &idx in &selector.filtered {
            assert!(selector.models[idx].id.contains("filler"));
        }
    }

    #[test]
    fn search_matches_rendered_local_verification_status() {
        for (state, query, expected) in [
            (
                crate::model_catalog::VerificationState::Verified,
                "detected",
                "detected-local",
            ),
            (
                crate::model_catalog::VerificationState::Unavailable,
                "unavailable",
                "unavailable-local",
            ),
        ] {
            let mut matching = test_model(expected, "llamacpp");
            matching.verification = ModelVerification {
                state,
                source: "local-runtime".to_owned(),
                detail: None,
            };
            let mut other = test_model("other-local", "ollama");
            other.verification = ModelVerification {
                state: crate::model_catalog::VerificationState::Unknown,
                source: "local-runtime".to_owned(),
                detail: None,
            };
            let mut selector = ModelSelector::with_models(vec![matching, other]);
            selector.show();
            selector.insert_str(query);

            assert_eq!(selector.filtered.len(), 1, "status query {query}");
            assert_eq!(
                selector.selected_model_id().as_deref(),
                Some(format!("llamacpp/{expected}").as_str())
            );
        }
    }

    #[test]
    fn context_window_labels_preserve_known_and_unknown_limits() {
        assert_eq!(format_context_window(1_050_000), "1050k ctx");
        assert_eq!(format_context_window(1_000_000), "1M ctx");
        assert_eq!(format_context_window(131_072), "131k ctx");
        assert_eq!(format_context_window(0), "unknown ctx");
    }

    #[test]
    fn picker_at_80_columns_keeps_status_and_capabilities_visible() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut known = test_model("a-long-model-name-with-a-provider-and-version", "openai");
        known.verification = ModelVerification {
            state: crate::model_catalog::VerificationState::Verified,
            source: "provider-registry".to_owned(),
            detail: None,
        };
        let mut discovered = test_model("local-model", "llamacpp");
        discovered.verification = ModelVerification {
            state: crate::model_catalog::VerificationState::Verified,
            source: "local-runtime".to_owned(),
            detail: Some("Capabilities are not in the catalog".to_owned()),
        };
        discovered.capabilities.context_tokens = 0;
        let mut selector = ModelSelector::with_models(vec![known, discovered]);
        selector.show();
        selector.toggle_show_all();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| selector.render(frame, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Catalog · availability unchecked · 200k ctx"));
        assert!(text.contains("Tools: yes · Images: no · Reasoning: no"));
        assert!(text.contains("Local · detected · unknown ctx"));
        assert!(text.contains("Capabilities: unknown (not in catalog)"));
        assert!(!text.contains("ready"));
        assert!(!text.contains("T1"));
        assert!(!text.contains("?T0"));
    }

    #[test]
    fn discovered_models_are_deduplicated_and_lead_the_focused_slice() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        let mut first = test_model("Qwen3.8-27B", "llamacpp");
        first.verification = ModelVerification {
            state: crate::model_catalog::VerificationState::Verified,
            source: "local-runtime".to_owned(),
            detail: None,
        };
        selector.models.push(test_model("Qwen3.8-27B", "llamacpp"));
        let mut second = test_model("qwen3.6:27b", "ollama");
        second.verification = first.verification.clone();

        assert!(selector.replace_discovered_models(1, vec![first, second]));
        selector.show();

        let focused = selector
            .filtered
            .iter()
            .map(|&idx| (&selector.models[idx].provider, &selector.models[idx].id))
            .collect::<Vec<_>>();
        assert_eq!(focused[0].0, "llamacpp");
        assert_eq!(focused[1].0, "ollama");
        assert_eq!(
            selector
                .models
                .iter()
                .filter(|model| model.provider == "llamacpp" && model.id == "Qwen3.8-27B")
                .count(),
            1
        );
    }

    #[test]
    fn stale_discovery_batches_do_not_reorder_the_active_selection() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        selector.show();
        selector.move_down();
        let selected = selector.selected_model_id();

        assert!(selector.replace_discovered_models(2, vec![]));
        assert!(!selector.replace_discovered_models(1, vec![test_model("stale", "ollama")]));
        assert_eq!(selector.selected_model_id(), selected);
    }

    #[test]
    fn discovery_refresh_resets_selection_when_the_selected_route_disappears() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        let mut first = test_model("first-local", "llamacpp");
        first.verification.source = "local-runtime".to_owned();
        let mut second = test_model("second-local", "ollama");
        second.verification.source = "local-runtime".to_owned();
        assert!(selector.replace_discovered_models(1, vec![first.clone(), second]));
        selector.show();
        selector.move_down();
        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("ollama/second-local")
        );

        assert!(selector.replace_discovered_models(2, vec![first]));

        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("llamacpp/first-local")
        );
        assert_eq!(
            selector.selected_model_id(),
            Some(selection_model_id(&selector.models[selector.filtered[0]]))
        );
    }

    #[test]
    fn discovery_refresh_preserves_show_all_affordance_selection() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        selector.show();
        while !selector.selected_show_all() {
            selector.move_down();
        }
        let previous_count = selector.filtered.len();

        let mut discovered = test_model("new-local", "ollama");
        discovered.verification.source = "local-runtime".to_owned();
        assert!(selector.replace_discovered_models(1, vec![discovered]));

        assert!(selector.selected_show_all());
        assert_ne!(selector.filtered.len(), previous_count);
        assert!(selector.selected_model_id().is_none());
    }

    #[test]
    fn discovery_refresh_retains_an_active_missing_local_route_as_unavailable() {
        let mut selector = ModelSelector::with_models(slice_catalog());
        let mut active = test_model("active-local", "llamacpp");
        active.verification = ModelVerification {
            state: crate::model_catalog::VerificationState::Verified,
            source: "local-runtime".to_owned(),
            detail: Some("Capabilities are not in the catalog".to_owned()),
        };
        assert!(selector.replace_discovered_models(1, vec![active]));
        selector.set_current_model(Some("llamacpp/active-local".to_owned()));
        selector.show();

        assert!(selector.replace_discovered_models(2, vec![]));

        let retained = selector
            .models
            .iter()
            .find(|model| selection_model_id(model) == "llamacpp/active-local")
            .expect("active local model remains visible");
        assert_eq!(
            retained.verification.state,
            crate::model_catalog::VerificationState::Unavailable
        );
        let status = model_status_summary(retained);
        assert!(status.starts_with("Local · unavailable"));
        assert!(!status.contains("Local · detected"));
        assert_eq!(
            capability_summary(retained),
            "Capabilities: unknown (not in catalog)"
        );
        let detail = retained.verification.detail.as_deref().expect("detail");
        assert!(detail.contains("Capabilities are not in the catalog"));
        assert!(detail.contains("Not reported by the local runtime"));
        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("llamacpp/active-local")
        );
    }

    #[test]
    fn discovery_refresh_replaces_matching_catalog_row_with_unavailable_active_model() {
        let mut selector = ModelSelector::new();
        let mut active = crate::model_catalog::find_model("llamacpp/Qwen3.8-27B")
            .expect("built-in local Qwen row");
        active.verification = ModelVerification {
            state: crate::model_catalog::VerificationState::Verified,
            source: "local-runtime".to_owned(),
            detail: Some("Capabilities from the built-in catalog".to_owned()),
        };
        assert!(selector.replace_discovered_models(1, vec![active]));
        selector.set_current_model(Some("llamacpp/Qwen3.8-27B".to_owned()));
        selector.show();

        assert!(selector.replace_discovered_models(2, vec![]));

        let matching = selector
            .models
            .iter()
            .filter(|model| model.provider == "llamacpp" && model.id == "Qwen3.8-27B")
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            1,
            "retention must not duplicate the catalog row"
        );
        assert_eq!(
            matching[0].verification.state,
            crate::model_catalog::VerificationState::Unavailable
        );
        assert_eq!(matching[0].verification.source, "local-runtime");
        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("llamacpp/Qwen3.8-27B")
        );
    }

    #[test]
    fn capability_labels_distinguish_unsupported_from_unknown() {
        let mut model = test_model("local-model", "llamacpp");
        model.capabilities.tools = false;
        assert_eq!(
            capability_summary(&model),
            "Tools: no · Images: no · Reasoning: no"
        );
        model.verification.detail = Some("Capabilities are not in the catalog".to_owned());
        assert_eq!(
            capability_summary(&model),
            "Capabilities: unknown (not in catalog)"
        );
        model.verification.detail = None;
        model.capabilities.tools = true;
        model.capabilities.vision = true;
        model.capabilities.reasoning = true;
        assert_eq!(
            capability_summary(&model),
            "Tools: yes · Images: yes · Reasoning: yes"
        );
    }

    #[test]
    fn filtering_keeps_the_selected_route_when_earlier_rows_disappear() {
        let mut selector = ModelSelector::with_models(vec![
            test_model("alpha", "ollama"),
            test_model("beta-common", "ollama"),
            test_model("gamma-common", "ollama"),
        ]);
        selector.show();
        selector.toggle_show_all();
        selector.move_down();
        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("ollama/beta-common")
        );
        selector.insert_str("common");
        assert_eq!(
            selector.selected_model_id().as_deref(),
            Some("ollama/beta-common")
        );
    }
}
