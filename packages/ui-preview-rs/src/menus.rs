//! A complete menu fixture family: no custom layout or keyboard handling.
use crate::registry::{Registry, Story};
use maestro_ui::{ActionPicker, Menu, PickerStatus};
pub fn register(registry: &mut Registry) -> Result<(), String> {
    for (id, label, status, query) in [
        ("menu-ready", "Choose workspace", PickerStatus::Ready, ""),
        (
            "menu-filtered",
            "Filtered workspace",
            PickerStatus::Ready,
            "docs",
        ),
        (
            "menu-empty",
            "No matching workspaces",
            PickerStatus::Ready,
            "missing",
        ),
        (
            "menu-loading",
            "Loading workspaces",
            PickerStatus::Loading("Loading workspaces…".into()),
            "",
        ),
        (
            "menu-error",
            "Workspace error",
            PickerStatus::Error("Could not load workspaces. Retry.".into()),
            "",
        ),
    ] {
        registry.add(
            Story::new(
                id,
                label,
                "products/maestro/packages/ui-preview-rs/src/menus.rs",
                move |_, frame| {
                    let mut state = ActionPicker::new(vec![
                        "Application".to_owned(),
                        "Documentation".to_owned(),
                        "docs-site".to_owned(),
                    ])
                    .searchable(String::as_str);
                    state.open();
                    state.insert_str(query);
                    state.set_status(status.clone());
                    Menu::new("Choose workspace", &mut state).render(
                        frame,
                        frame.area(),
                        maestro_presentation::palette::default_controls(),
                    );
                },
            )
            .matrix(&[(40, 20), (60, 24), (100, 30)], &[0])
            .adapter("shared-menu"),
        )?;
    }
    Ok(())
}
