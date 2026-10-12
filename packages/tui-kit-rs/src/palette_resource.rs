//! Resource-neutral values for palette/search surfaces.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaletteResourceKind {
    Command,
    File,
    Session,
    Model,
    Theme,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteResource {
    /// Presentation metadata is never part of the resource wire contract.
    #[serde(skip)]
    pub localize_description: bool,
    pub kind: PaletteResourceKind,
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub status: Option<String>,
    pub search_terms: Vec<String>,
}

impl PaletteResource {
    #[must_use]
    pub fn new(kind: PaletteResourceKind, id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            localize_description: false,
            kind,
            id: id.into(),
            label: label.into(),
            description: None,
            status: None,
            search_terms: Vec::new(),
        }
    }

    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn localized_description(mut self, description: impl Into<String>, enabled: bool) -> Self {
        self.description = Some(description.into());
        self.localize_description = enabled;
        self
    }

    pub fn display_description(&self) -> Option<&str> {
        self.description.as_deref().map(|value| {
            if self.localize_description {
                maestro_ui::localization::tr(value)
            } else {
                value
            }
        })
    }

    #[must_use]
    pub fn search_terms(mut self, terms: impl IntoIterator<Item = String>) -> Self {
        self.search_terms = terms.into_iter().collect();
        self
    }

    #[must_use]
    pub fn status(mut self, status: impl Into<String>) -> Self {
        self.status = Some(status.into());
        self
    }

    #[must_use]
    pub fn stable_id(&self) -> String {
        format!("{}:{}", self.kind.prefix(), self.id)
    }

    #[must_use]
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.id.to_lowercase().contains(&query)
            || self.label.to_lowercase().contains(&query)
            || self
                .display_description()
                .is_some_and(|value| value.to_lowercase().contains(&query))
            || self
                .search_terms
                .iter()
                .any(|value| value.to_lowercase().contains(&query))
    }
}

impl PaletteResourceKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Command => maestro_ui::localization::tr("command"),
            Self::File => maestro_ui::localization::tr("file"),
            Self::Session => maestro_ui::localization::tr("session"),
            Self::Model => maestro_ui::localization::tr("model"),
            Self::Theme => maestro_ui::localization::tr("theme"),
        }
    }

    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Command => ">",
            Self::File => "@",
            Self::Session => "#",
            Self::Model => ":",
            Self::Theme => "%",
        }
    }
}

impl From<&crate::model_catalog::ModelInfo> for PaletteResource {
    fn from(model: &crate::model_catalog::ModelInfo) -> Self {
        Self::new(
            PaletteResourceKind::Model,
            crate::model_catalog::model_route(model),
            &model.name,
        )
        .description(&model.description)
        .search_terms([
            model.provider.clone(),
            format!("{:?}", model.capabilities.protocol),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_kind_covers_existing_modal_domains() {
        let kinds = [
            PaletteResourceKind::Command,
            PaletteResourceKind::File,
            PaletteResourceKind::Session,
            PaletteResourceKind::Model,
            PaletteResourceKind::Theme,
        ];
        assert_eq!(kinds.len(), 5);
        let resource = PaletteResource::new(PaletteResourceKind::File, "src/main.rs", "main.rs")
            .search_terms(["rust".to_owned()]);
        assert!(resource.matches("rust"));
        assert_eq!(resource.stable_id(), "@:src/main.rs");
    }

    #[test]
    fn local_model_resource_uses_qualified_route() {
        let model = crate::model_catalog::find_model("llamacpp/Qwen3.8-27B")
            .expect("local Qwen catalog row");

        assert_eq!(PaletteResource::from(&model).id, "llamacpp/Qwen3.8-27B");
    }
}

#[cfg(test)]
mod localization_regression_tests {
    use super::*;

    #[test]
    fn custom_descriptions_are_not_translated() {
        use crate::localization::{Locale, with_locale};
        with_locale(Locale::French, || {
            let custom = PaletteResource::new(PaletteResourceKind::Command, "custom", "Search")
                .description("Search");
            assert_eq!(custom.display_description(), Some("Search"));
        });
    }
}

#[cfg(test)]
mod localized_search_tests {
    use super::*;
    use crate::localization::{Locale, with_locale};
    #[test]
    fn translated_description_search_keeps_the_resource_identity() {
        let resource = PaletteResource::new(PaletteResourceKind::Command, "search", "search")
            .localized_description("Search", true);
        with_locale(Locale::French, || {
            assert_eq!(resource.display_description(), Some("Rechercher"));
            assert!(resource.matches("RECHERCHER"));
            assert_eq!(resource.stable_id(), ">:search");
        });
    }
}
