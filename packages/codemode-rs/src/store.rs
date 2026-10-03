//! Bounded, untrusted script scratch values. Hosts own durable commit and scope.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub type Store = BTreeMap<String, Value>;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StoreWrites {
    pub set: Store,
    pub delete: Vec<String>,
}

impl StoreWrites {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.delete.is_empty()
    }
    pub fn apply(&self, current: &mut Store) -> Result<(), String> {
        let mut next = current.clone();
        for key in &self.delete {
            next.remove(key);
        }
        next.extend(self.set.clone());
        validate_store(&next)?;
        *current = next;
        Ok(())
    }
    pub(crate) fn between(before: &Store, after: &Store) -> Self {
        Self {
            set: after
                .iter()
                .filter(|(k, v)| before.get(*k) != Some(*v))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            delete: before
                .keys()
                .filter(|k| !after.contains_key(*k))
                .cloned()
                .collect(),
        }
    }
}

pub fn validate_store(store: &Store) -> Result<(), String> {
    if store.len() > 128 {
        return Err("maximum 128 store keys".into());
    }
    for (key, value) in store {
        if key.is_empty() || key.len() > 256 {
            return Err("store key must contain 1 to 256 bytes".into());
        }
        if serde_json::to_vec(value).map_err(|e| e.to_string())?.len() > 65_536 {
            return Err("maximum 64 KiB per store value".into());
        }
    }
    if serde_json::to_vec(store).map_err(|e| e.to_string())?.len() > 262_144 {
        return Err("maximum 256 KiB store".into());
    }
    Ok(())
}
