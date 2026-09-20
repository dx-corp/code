//! Shared Menu and ActionPicker stories owned by maestro-ui.
use crate::registry::Registry;

pub fn register(registry: &mut Registry) -> Result<(), String> {
    crate::menus::register(registry)
}
