//! Lightweight UI preferences persisted under `~/.maestro/ui.json`.
//!
//! Stores presentation preferences. Load failures fall back to
//! defaults so a corrupt file never blocks TUI startup.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::OutputDetail;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPrefs {
    /// Display locale; unknown future locales fall back without losing other preferences.
    #[serde(default)]
    pub display_language: Option<String>,
    /// Suppresses automatic first-run setup; never proves runtime readiness.
    #[serde(default)]
    pub onboarding_seen: bool,
    /// First-launch artwork was shown. Separate from setup completion so quitting
    /// preparation does not replay it or suppress account setup. No authority.
    #[serde(default)]
    pub boot_seen: bool,
    /// The user's choice for structured onboarding information collection.
    #[serde(default)]
    pub onboarding_share_diagnostics: Option<bool>,
    /// Conversation output detail. Missing values retain compact tool previews.
    #[serde(default)]
    pub output_detail: Option<String>,
    #[serde(default)]
    pub footer_style: Option<String>,
    #[serde(default)]
    pub dex_personality: Option<String>,
    #[serde(default)]
    pub animations: Option<bool>,
    #[serde(default)]
    pub dex_accessory: maestro_presentation::dex_delight::DexAccessory,
    #[serde(default)]
    pub dex_accent: maestro_presentation::dex_delight::DexAccent,
    #[serde(default)]
    pub dex_tips_dismissed: bool,
    #[serde(default)]
    pub dex_notifications: bool,
    #[serde(default)]
    pub dex_suggestions_disabled: bool,
    #[serde(default)]
    pub dex_recap_disabled: bool,
    /// Show wall-clock timestamps beside conversation headings. Off by default.
    #[serde(default)]
    pub timestamps: Option<bool>,
}

impl UiPrefs {
    pub fn locale(&self) -> crate::localization::Locale {
        self.display_language
            .as_deref()
            .and_then(crate::localization::Locale::parse)
            .unwrap_or_default()
    }

    /// Return the persisted output detail, using the legacy default if absent or unknown.
    pub fn output_detail(&self) -> OutputDetail {
        self.output_detail
            .as_deref()
            .and_then(OutputDetail::parse)
            .unwrap_or_default()
    }

    /// Set output detail without replacing unrelated preferences.
    pub fn set_output_detail(&mut self, detail: OutputDetail) {
        self.output_detail = Some(detail.as_str().to_string());
    }

    pub fn load_default() -> Self {
        load_from_path(&default_path()).unwrap_or_default()
    }

    pub fn save_default(&self) -> Result<()> {
        save_to_path(self, &default_path())
    }

    pub fn dex_personality(
        &self,
    ) -> maestro_presentation::components::dex_companion::DexPersonality {
        use maestro_presentation::components::dex_companion::DexPersonality;
        match self.dex_personality.as_deref() {
            Some("quiet") => DexPersonality::Quiet,
            Some("expressive") => DexPersonality::Expressive,
            Some("standard") => DexPersonality::Standard,
            _ => DexPersonality::Quiet,
        }
    }

    pub fn footer_style(&self) -> FooterStyle {
        self.footer_style
            .as_deref()
            .and_then(FooterStyle::parse)
            .unwrap_or_default()
    }

    pub fn set_footer_style(&mut self, style: FooterStyle) {
        self.footer_style = Some(style.as_str().to_string());
    }
}

fn default_path() -> PathBuf {
    crate::path_utils::maestro_home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ui.json")
}

fn load_from_path(path: &Path) -> Result<UiPrefs> {
    if !path.exists() {
        return Ok(UiPrefs::default());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let prefs: UiPrefs = serde_json::from_str(&raw).context("parse ui.json")?;
    Ok(prefs)
}

fn save_to_path(prefs: &UiPrefs, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let raw = serde_json::to_string_pretty(prefs).context("serialize ui prefs")?;
    crate::fs_atomic::write_atomic(path, raw.as_bytes()).context("write ui.json")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn display_language_roundtrips_and_unknown_language_keeps_other_preferences() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ui.json");
        let mut prefs = UiPrefs::default();
        prefs.set_footer_style(FooterStyle::Solo);
        for locale in crate::localization::Locale::ALL {
            prefs.display_language = Some(locale.code().into());
            save_to_path(&prefs, &path).unwrap();
            let loaded = load_from_path(&path).unwrap();
            assert_eq!(loaded.locale(), locale);
            assert_eq!(loaded.footer_style(), FooterStyle::Solo);
        }
        prefs.display_language = Some("future".into());
        save_to_path(&prefs, &path).unwrap();
        let loaded = load_from_path(&path).unwrap();
        assert_eq!(loaded.locale(), crate::localization::Locale::English);
        assert_eq!(loaded.footer_style(), FooterStyle::Solo);
    }

    #[test]
    fn output_detail_roundtrip_preserves_other_preferences() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ui.json");
        let mut prefs = UiPrefs::default();
        prefs.set_footer_style(FooterStyle::Solo);
        prefs.timestamps = Some(true);
        for detail in [
            OutputDetail::Summary,
            OutputDetail::Compact,
            OutputDetail::Expanded,
        ] {
            prefs.set_output_detail(detail);
            save_to_path(&prefs, &path).unwrap();
            let loaded = load_from_path(&path).unwrap();
            assert_eq!(loaded.output_detail(), detail);
            assert_eq!(loaded.footer_style(), FooterStyle::Solo);
            assert_eq!(loaded.timestamps, Some(true));
        }
    }

    #[test]
    fn output_detail_missing_or_unknown_defaults_to_legacy_compact() {
        for raw in [
            r#"{"footerStyle":"solo"}"#,
            r#"{"outputDetail":"future"}"#,
            r#"{"outputDetail":null}"#,
        ] {
            let prefs: UiPrefs = serde_json::from_str(raw).unwrap();
            assert_eq!(prefs.output_detail(), OutputDetail::Compact);
        }
        assert_eq!(UiPrefs::default().output_detail(), OutputDetail::Compact);
    }

    #[test]
    fn roundtrip_footer_style() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ui.json");
        let mut prefs = UiPrefs::default();
        prefs.set_footer_style(FooterStyle::Solo);
        save_to_path(&prefs, &path).unwrap();
        let loaded = load_from_path(&path).unwrap();
        assert_eq!(loaded.footer_style(), FooterStyle::Solo);
    }
    #[test]
    fn dex_preferences_persist_without_replacing_footer() {
        use maestro_presentation::components::dex_companion::DexPersonality;
        let dir = tempdir().unwrap();
        let path = dir.path().join("ui.json");
        for (name, personality) in [
            ("quiet", DexPersonality::Quiet),
            ("standard", DexPersonality::Standard),
            ("expressive", DexPersonality::Expressive),
        ] {
            let mut prefs = UiPrefs::default();
            prefs.set_footer_style(FooterStyle::Solo);
            prefs.dex_personality = Some(name.to_owned());
            prefs.animations = Some(false);
            save_to_path(&prefs, &path).unwrap();
            let loaded = load_from_path(&path).unwrap();
            assert_eq!(loaded.dex_personality(), personality);
            assert_eq!(loaded.animations, Some(false));
            assert_eq!(loaded.footer_style(), FooterStyle::Solo);
        }
        let legacy: UiPrefs = serde_json::from_str(r#"{"footerStyle":"solo"}"#).unwrap();
        assert_eq!(legacy.dex_personality(), DexPersonality::Quiet);
        assert_eq!(legacy.animations, None);
    }
    #[test]
    fn dex_cosmetics_and_controls_roundtrip_with_legacy_defaults() {
        use maestro_presentation::dex_delight::{DexAccent, DexAccessory};
        let dir = tempdir().unwrap();
        let path = dir.path().join("ui.json");
        let prefs = UiPrefs {
            dex_accessory: DexAccessory::Beanie,
            dex_accent: DexAccent::Mint,
            dex_tips_dismissed: true,
            dex_notifications: true,
            dex_suggestions_disabled: true,
            dex_recap_disabled: true,
            ..Default::default()
        };
        save_to_path(&prefs, &path).unwrap();
        let loaded = load_from_path(&path).unwrap();
        assert_eq!(loaded.dex_accessory, DexAccessory::Beanie);
        assert_eq!(loaded.dex_accent, DexAccent::Mint);
        assert!(loaded.dex_notifications && loaded.dex_tips_dismissed);
        assert!(loaded.dex_suggestions_disabled && loaded.dex_recap_disabled);
        for accessory in [
            DexAccessory::Sprout,
            DexAccessory::CatEars,
            DexAccessory::Crown,
            DexAccessory::Bow,
        ] {
            let customized = UiPrefs {
                dex_accessory: accessory,
                ..prefs.clone()
            };
            save_to_path(&customized, &path).unwrap();
            assert_eq!(load_from_path(&path).unwrap().dex_accessory, accessory);
        }
        let old: UiPrefs = serde_json::from_str(r#"{"footerStyle":"solo"}"#).unwrap();
        assert_eq!(old.dex_accessory, DexAccessory::None);
        assert!(!old.dex_notifications);
    }
    #[test]
    fn onboarding_display_and_sharing_preferences_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ui.json");
        let mut prefs = UiPrefs::default();
        assert!(!prefs.onboarding_seen);
        assert_eq!(prefs.onboarding_share_diagnostics, None);
        assert!(!prefs.boot_seen);
        prefs.boot_seen = true;
        prefs.onboarding_seen = true;
        prefs.onboarding_share_diagnostics = Some(false);
        save_to_path(&prefs, &path).unwrap();
        let loaded = load_from_path(&path).unwrap();
        assert!(loaded.onboarding_seen);
        assert!(loaded.boot_seen);
        assert_eq!(loaded.onboarding_share_diagnostics, Some(false));
    }
}

/// Status bar density presets (Kimi-inspired `/footer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FooterStyle {
    /// Model, cwd, git, badges, shortcuts.
    #[default]
    Rich,
    /// Compact: model + goal + key badges only.
    Solo,
    /// Hide most chrome; keep alerts and pending approvals.
    History,
    /// Status bar empty (zen-adjacent; zen mode still separate).
    Clear,
}

impl FooterStyle {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "rich" | "full" | "default" => Some(Self::Rich),
            "solo" | "compact" | "min" => Some(Self::Solo),
            "history" | "hist" => Some(Self::History),
            "clear" | "off" | "none" | "hidden" => Some(Self::Clear),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rich => "rich",
            Self::Solo => "solo",
            Self::History => "history",
            Self::Clear => "clear",
        }
    }
}
