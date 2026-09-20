//! ThemeSelector adapter-local story registration.
use maestro_ui_preview::review::Capture;

macro_rules! generated_stories {
    () => {
        fn generated_captures(_captures: &mut [Capture]) -> Result<(), String> {
            Ok(())
        }
    };
    ($first:ident $(, $story:ident)* $(,)?) => {
        pub mod $first;
        $(pub mod $story;)*
        fn generated_captures(captures: &mut Vec<Capture>) -> Result<(), String> {
            captures.extend($first::captures()?);
            $(captures.extend($story::captures()?);)*
            Ok(())
        }
    };
}

generated_stories! {
// maestro-ui-stories:start
// maestro-ui-stories:end
}

pub fn captures_selected(selected: Option<&str>) -> Result<Vec<Capture>, String> {
    let mut captures = super::theme_selector_story::captures_selected(selected)?;
    generated_captures(&mut captures)?;
    captures.retain(|capture| selected.is_none_or(|id| capture.scene.id == id));
    Ok(captures)
}
