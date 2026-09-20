//! Adapter-local story registration. Studio edits only the bounded macro list.
use crate::registry::Registry;

macro_rules! registered_stories {
    ($($story:ident),* $(,)?) => {
        $(pub mod $story;)*
        pub fn register(registry: &mut Registry) -> Result<(), String> {
            $($story::register(registry)?;)*
            Ok(())
        }
    };
}

registered_stories! {
// maestro-ui-stories:start
    shared_menu,
// maestro-ui-stories:end
}
