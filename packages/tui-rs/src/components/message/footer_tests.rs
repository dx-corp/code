use super::*;

#[cfg(test)]
mod dex_notice_layout_tests {
    use super::*;

    #[test]
    fn quiet_notice_does_not_overwrite_welcome_identity() {
        let state = crate::state::AppState::new();
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        ChatView::new(&state)
            .with_dex_presentation(
                crate::components::dex_companion::DexPersonality::Quiet,
                false,
            )
            .with_dex_delight(
                Default::default(),
                Some("Welcome back. Your answer is needed"),
                None,
                None,
            )
            .render(area, &mut buffer);
        let lines: Vec<String> = (0..area.height)
            .map(|y| (0..area.width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let notice = lines
            .iter()
            .position(|line| line.contains("Welcome back. Your answer is needed"))
            .unwrap();
        let status = lines
            .iter()
            .position(|line| line.contains(crate::components::deixic_logo::PRODUCT_TITLE))
            .unwrap();
        assert_ne!(notice, status);
    }
}

#[cfg(test)]
mod worker_badge_tests {
    use super::*;
    #[test]
    fn worker_activity_is_visible_in_the_footer() {
        let area = Rect::new(0, 0, 100, 1);
        let mut buffer = Buffer::empty(area);
        StatusBarWidget::new(None, None, None, None)
            .with_worker_badge(Some("↗ 2 running · 1 need input /workers"))
            .render(area, &mut buffer);
        let text: String = (0..100).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(text.contains("↗ 2 running"), "{text}");
        assert!(text.contains("/workers"), "{text}");
    }
}

#[cfg(test)]
mod transparent_theme_regression {
    use super::*;
    #[test]
    fn transparent_high_contrast_uses_selected_palette() {
        let theme = crate::themes::high_contrast_theme();
        assert!(theme.canvas_style().bg.is_none());
        let actual = conversation_theme_for(&theme);
        let expected = theme.ui_theme();
        assert_eq!(actual.text, expected.text);
        assert_eq!(actual.muted, expected.muted);
        assert_eq!(actual.focus, expected.focus);
        assert_eq!(
            semantic_color_for_theme(&theme, "md_link", Color::Blue),
            theme.get_color("md_link").unwrap()
        );
        assert_eq!(
            conversation_theme_for(&crate::themes::dark_theme()).text,
            crate::themes::dark_theme().ui_theme().text
        );
    }
}
