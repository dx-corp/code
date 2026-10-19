//! Thread-local untrusted scratch values, partitioned by accepted principal.
use crate::{CallId, Outcome, PrincipalId};
use agent_codemode::{Store, StoreWrites};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Scratch {
    values: BTreeMap<PrincipalId, Store>,
    prepared: BTreeMap<CallId, (PrincipalId, StoreWrites)>,
}
impl Scratch {
    pub fn discard_pending(&mut self) {
        self.prepared.clear();
    }
    pub fn read(&self, principal: &PrincipalId) -> Store {
        self.values.get(principal).cloned().unwrap_or_default()
    }
    pub fn validate(&self, principal: &PrincipalId, writes: &StoreWrites) -> Result<(), String> {
        let mut next = self.values.clone();
        writes.apply(next.entry(principal.clone()).or_default())?;
        next.retain(|_, values| !values.is_empty());
        if next.values().map(Store::len).sum::<usize>() > 128
            || serde_json::to_vec(&next).map_err(|e| e.to_string())?.len() > 256 * 1024
        {
            return Err(
                "thread scratch capacity exceeded; delete unused values before storing more".into(),
            );
        }
        Ok(())
    }
    pub fn validate_preparation(
        &self,
        principal: &PrincipalId,
        writes: &StoreWrites,
    ) -> Result<(), String> {
        self.validate(principal, writes)?;
        let pending_bytes = self
            .prepared
            .values()
            .map(|(_, writes)| serde_json::to_vec(writes).map_or(256 * 1024, |bytes| bytes.len()))
            .sum::<usize>();
        let write_bytes = serde_json::to_vec(writes).map_err(|e| e.to_string())?.len();
        if self.prepared.len() >= 128 || pending_bytes.saturating_add(write_bytes) > 256 * 1024 {
            return Err("pending thread scratch capacity exceeded; finish prior scripts or reduce stored values".into());
        }
        Ok(())
    }
    pub fn prepare(&mut self, parent: &CallId, principal: &PrincipalId, writes: &StoreWrites) {
        if self.validate_preparation(principal, writes).is_ok() {
            self.prepared
                .entry(parent.clone())
                .or_insert_with(|| (principal.clone(), writes.clone()));
        }
    }
    pub fn finish(&mut self, parent: &CallId, outcome: Outcome) {
        if let Some((principal, writes)) = self.prepared.remove(parent)
            && outcome == Outcome::Succeeded
            && self.validate(&principal, &writes).is_ok()
        {
            let values = self.values.entry(principal.clone()).or_default();
            if writes.apply(values).is_ok() && values.is_empty() {
                self.values.remove(&principal);
            }
        }
    }
}
