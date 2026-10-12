//! Dex's product catalog. Reusable interaction mechanics live in maestro-interaction.
use maestro_interaction::{Action, ActionCatalog};
use std::sync::LazyLock;

pub use maestro_presentation::appearance::{Appearance, LOOKS};

#[derive(Clone, Copy)]
pub enum Setting {
    Personality(&'static str),
    Motion(bool),
    Tips(bool),
    Notifications(bool),
    Suggestions(bool),
    Recap(bool),
}

pub const SETTINGS: [Action<Setting>; 13] = [
    Action::new("quiet", "quiet", Setting::Personality("quiet")),
    Action::new("standard", "standard", Setting::Personality("standard")),
    Action::new(
        "expressive",
        "expressive",
        Setting::Personality("expressive"),
    ),
    Action::new("motion-on", "motion-on", Setting::Motion(true)),
    Action::new("motion-off", "motion-off", Setting::Motion(false)),
    Action::new("tips-on", "tips-on", Setting::Tips(true)),
    Action::new("tips-off", "tips-off", Setting::Tips(false)),
    Action::new(
        "notifications-on",
        "notifications-on",
        Setting::Notifications(true),
    ),
    Action::new(
        "notifications-off",
        "notifications-off",
        Setting::Notifications(false),
    ),
    Action::new(
        "suggestions-on",
        "suggestions-on",
        Setting::Suggestions(true),
    ),
    Action::new(
        "suggestions-off",
        "suggestions-off",
        Setting::Suggestions(false),
    ),
    Action::new("recap-on", "recap-on", Setting::Recap(true)),
    Action::new("recap-off", "recap-off", Setting::Recap(false)),
];

#[derive(Clone, Copy)]
pub enum Control {
    Help,
    Pet,
    Appearance,
    Recap,
    Next,
}

pub const CONTROLS: [Action<Control>; 5] = [
    Action::new("", "Dex help", Control::Help),
    Action::new("pet", "Pet Dex", Control::Pet).description("Give Dex a brief reaction"),
    Action::new("appearance", "Choose appearance", Control::Appearance),
    Action::new("recap", "Show recap", Control::Recap),
    Action::new("next", "Fill next prompt", Control::Next)
        .description("Fill the next suggested prompt without submitting it"),
];

pub fn controls() -> &'static ActionCatalog<'static, Control> {
    static CATALOG: LazyLock<ActionCatalog<'static, Control>> =
        LazyLock::new(|| ActionCatalog::new(&CONTROLS).expect("Dex controls must be unique"));
    &CATALOG
}
pub fn looks() -> &'static ActionCatalog<'static, Appearance> {
    static CATALOG: LazyLock<ActionCatalog<'static, Appearance>> =
        LazyLock::new(|| ActionCatalog::new(&LOOKS).expect("Dex appearances must be unique"));
    &CATALOG
}
pub fn settings() -> &'static ActionCatalog<'static, Setting> {
    static CATALOG: LazyLock<ActionCatalog<'static, Setting>> =
        LazyLock::new(|| ActionCatalog::new(&SETTINGS).expect("Dex settings must be unique"));
    &CATALOG
}
pub fn command_ids() -> Vec<&'static str> {
    controls()
        .actions()
        .iter()
        .map(|a| a.id)
        .chain(settings().actions().iter().map(|a| a.id))
        .chain(looks().actions().iter().map(|a| a.id))
        .filter(|id| !id.is_empty())
        .collect()
}
pub fn contains(id: &str) -> bool {
    controls().find(id).is_some() || settings().find(id).is_some() || looks().find(id).is_some()
}
pub fn help() -> String {
    [controls().help(), settings().help(), looks().help()].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dex_action_ids_are_unique_across_controls_settings_and_appearance() {
        let ids = command_ids();
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }
}
