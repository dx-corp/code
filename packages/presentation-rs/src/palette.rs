//! The default native control palette, also supplied to component previews.
use maestro_ui::UiTheme;
use ratatui::style::Color;
pub fn default_controls() -> UiTheme {
    conversation()
}

/// The brand palette shared by the composer, transcript, and controls.
pub fn conversation() -> UiTheme {
    use crate::shimmer::{DEIXIC_ACCENT, DEIXIC_BORDER, DEIXIC_MUTED, DEIXIC_SURFACE, DEIXIC_TEXT};
    let color = |(r, g, b)| Color::Rgb(r, g, b);
    UiTheme {
        panel: Some(Color::Rgb(0x23, 0x21, 0x32)),
        selection: Some(Color::Rgb(0x32, 0x2e, 0x40)),
        surface: color(DEIXIC_SURFACE),
        text: color(DEIXIC_TEXT),
        muted: color(DEIXIC_MUTED),
        border: color(DEIXIC_BORDER),
        focus: color(DEIXIC_ACCENT),
        success: Color::Rgb(0x92, 0xc8, 0xb2),
        attention: Color::Rgb(0xe0, 0xbc, 0x87),
        error: Color::Rgb(0xea, 0xa2, 0x9b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_controls_and_conversation_share_the_brand_palette() {
        let controls = default_controls();
        assert_eq!(controls, conversation());
        assert_eq!(controls.surface, Color::Rgb(0x17, 0x16, 0x24));
        assert_eq!(controls.focus, Color::Rgb(0xab, 0xa0, 0xff));
    }
}
