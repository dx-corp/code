//! Deterministic, secret-safe evidence helpers shared by Maestro CLI reports.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Return a stable SHA-256 digest for a JSON value.
///
/// Object keys are sorted recursively before serialization so the digest does
/// not depend on insertion order. Callers remain responsible for removing
/// secret-derived values before hashing.
pub fn canonical_json_sha256(value: &Value) -> String {
    let canonical = canonical_json(value);
    let bytes = serde_json::to_vec(&canonical).expect("JSON values are always serializable");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonical_json(&object[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn digest_is_independent_of_object_key_order() {
        let left = json!({"b": 2, "a": {"d": 4, "c": 3}});
        let right = json!({"a": {"c": 3, "d": 4}, "b": 2});

        assert_eq!(canonical_json_sha256(&left), canonical_json_sha256(&right));
    }

    #[test]
    fn digest_changes_with_content() {
        assert_ne!(
            canonical_json_sha256(&json!({"value": 1})),
            canonical_json_sha256(&json!({"value": 2}))
        );
    }
}
