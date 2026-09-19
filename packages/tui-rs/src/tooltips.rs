//! Random tooltips for user education
//!
//! Displays helpful tips to users, loaded from tooltips.txt

use rand::RngExt;

const RAW_TOOLTIPS: &str = include_str!("../tooltips.txt");

static TOOLTIPS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    RAW_TOOLTIPS
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
});

/// Get a random tooltip
pub fn random_tooltip() -> Option<&'static str> {
    if TOOLTIPS.is_empty() {
        return None;
    }
    let mut rng = rand::rng();
    let idx = rng.random_range(0..TOOLTIPS.len());
    TOOLTIPS.get(idx).copied()
}

/// Get all tooltips
#[must_use]
pub fn all_tooltips() -> &'static [&'static str] {
    &TOOLTIPS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltips_load() {
        assert!(!TOOLTIPS.is_empty(), "Should have at least one tooltip");
    }

    #[test]
    fn random_tooltip_returns_some() {
        assert!(random_tooltip().is_some());
    }

    #[test]
    fn no_comment_lines() {
        for tip in TOOLTIPS.iter() {
            assert!(!tip.starts_with('#'), "Comments should be filtered out");
        }
    }

    #[test]
    fn tooltips_use_maestro_branding() {
        for tip in TOOLTIPS.iter() {
            assert!(
                !tip.contains("Composer"),
                "Tooltip should not use stale Composer branding: {tip}"
            );
            assert!(
                !tip.contains("composer --resume"),
                "Tooltip should not use stale composer command name: {tip}"
            );
        }
    }
}
