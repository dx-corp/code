//! Identity contract for a tool call that outlives the process that made it.
//!
//! A session transcript stores each tool call as a name plus arguments
//! (`session::entries::ContentBlock::ToolCall`). Dispatch then resolves that
//! name against whatever is configured *now*
//! (`tools/registry/execute.rs`, the `mcp__` branch). For MCP tools the set of
//! configured servers is user- and repository-controlled and can change
//! between the save and the resume, so a call recorded against one server's
//! `read_file` can execute a different server's `read_file` after a reload,
//! with the approval the user granted to the first one.
//!
//! The contract is persisted next to the call, recomputed from the live
//! registry at dispatch, and a mismatch produces a refusal result naming the
//! expected and actual identity instead of executing anything.
//!
//! The digest is deliberately scoped to the tool name plus its parameter
//! surface (property names and required names). Servers regenerate
//! descriptions for benign reasons; a changed parameter set is a changed tool.

use std::sync::{Mutex, OnceLock};

use serde_json::Value;

pub use maestro_runtime::{ToolCallContract, mcp_server_id, schema_digest, validate_identity};
/// Contracts restored from a resumed transcript, keyed by tool name.
///
/// A resumed transcript replays into the model, which re-emits its tool calls
/// under fresh call ids, so the recorded call id cannot be the lookup key. The
/// tool name is what dispatch resolves, and the tool name is what must still
/// mean the same thing.
///
/// Process-global because resume happens in the TUI event loop and the check happens inside the tool
/// executor, and there is no owned channel between them.
static RESTORED_CONTRACTS: OnceLock<Mutex<Vec<ToolCallContract>>> = OnceLock::new();

fn store() -> &'static Mutex<Vec<ToolCallContract>> {
    RESTORED_CONTRACTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Replace the restored contract set. Called when a session is resumed.
pub fn restore_pending_contracts(contracts: Vec<ToolCallContract>) {
    if let Ok(mut guard) = store().lock() {
        *guard = contracts;
    }
}

/// Drop every restored contract. Called when a session is cleared.
pub fn clear_pending_contracts() {
    restore_pending_contracts(Vec::new());
}

/// Forget the restored contract for `tool_name`.
///
/// Called after a refusal so the model's next attempt is judged against the
/// current definition and its own approval, instead of being refused forever.
pub fn drop_contract(tool_name: &str) {
    if let Ok(mut guard) = store().lock() {
        guard.retain(|contract| contract.tool_name != tool_name);
    }
}

/// The most recently recorded contract for `tool_name`, if the current session
/// was resumed from a transcript that used it.
#[must_use]
pub fn recorded_contract(tool_name: &str) -> Option<ToolCallContract> {
    let guard = store().lock().ok()?;
    guard
        .iter()
        .rev()
        .find(|contract| contract.tool_name == tool_name)
        .cloned()
}

/// Identity of every tool the executor can currently dispatch, keyed by the
/// model-facing tool name.
///
/// Published by `ToolExecutor` whenever it synchronizes MCP servers, and read
/// by the session recorder so a call is stamped with the identity that was
/// live when the model issued it.
static LIVE_IDENTITIES: OnceLock<Mutex<std::collections::HashMap<String, String>>> =
    OnceLock::new();

fn live_store() -> &'static Mutex<std::collections::HashMap<String, String>> {
    LIVE_IDENTITIES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Replace the published live identities with the current tool set.
pub fn publish_live_identities<'a, I>(tools: I)
where
    I: IntoIterator<Item = (&'a str, Option<&'a Value>)>,
{
    let map: std::collections::HashMap<String, String> = tools
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema_digest(name, schema)))
        .collect();
    if let Ok(mut guard) = live_store().lock() {
        *guard = map;
    }
}

/// Contract to persist next to a tool call the model just issued.
///
/// `None` when the tool's identity was never published, which is the case for
/// built-in tools compiled into this binary: their schemas cannot drift
/// between a save and a resume, so there is nothing to pin.
#[must_use]
pub fn stamp(call_id: &str, tool_name: &str) -> Option<ToolCallContract> {
    let digest = live_store().lock().ok()?.get(tool_name).cloned()?;
    Some(ToolCallContract {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        schema_digest: digest,
        server_id: mcp_server_id(tool_name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published identities and the restored contracts are process
    /// globals, so tests that write them must not run concurrently.
    static GLOBAL_STATE_GUARD: Mutex<()> = Mutex::new(());

    fn lock_global_state() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn schema(properties: &[&str], required: &[&str]) -> Value {
        let mut map = serde_json::Map::new();
        for name in properties {
            map.insert((*name).to_string(), serde_json::json!({"type": "string"}));
        }
        serde_json::json!({
            "type": "object",
            "properties": map,
            "required": required,
        })
    }

    #[test]
    fn digest_ignores_description_churn() {
        let a = serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "a path"}},
            "required": ["path"],
        });
        let b = serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "the path to read, as of 12:04"}},
            "required": ["path"],
        });
        assert_eq!(
            schema_digest("mcp__fs__read", Some(&a)),
            schema_digest("mcp__fs__read", Some(&b))
        );
    }

    #[test]
    fn digest_changes_when_a_parameter_appears() {
        let before = schema(&["path"], &["path"]);
        let after = schema(&["path", "exfiltrate_to"], &["path"]);
        assert_ne!(
            schema_digest("mcp__fs__read", Some(&before)),
            schema_digest("mcp__fs__read", Some(&after))
        );
    }

    #[test]
    fn digest_changes_when_a_parameter_becomes_required() {
        let before = schema(&["path", "mode"], &["path"]);
        let after = schema(&["path", "mode"], &["path", "mode"]);
        assert_ne!(
            schema_digest("mcp__fs__read", Some(&before)),
            schema_digest("mcp__fs__read", Some(&after))
        );
    }

    #[test]
    fn server_id_comes_from_the_dispatch_name() {
        assert_eq!(mcp_server_id("mcp__fs__read"), Some("fs".to_string()));
        assert_eq!(mcp_server_id("read"), None);
        assert_eq!(mcp_server_id("mcp__fs"), None);
    }

    #[test]
    fn validate_identity_accepts_an_unchanged_tool() {
        let s = schema(&["path"], &["path"]);
        let recorded = ToolCallContract::record("call-1", "mcp__fs__read", Some(&s));
        let live = ToolCallContract::record("call-9", "mcp__fs__read", Some(&s));
        assert!(validate_identity(&recorded, Some(&live)).is_ok());
    }

    #[test]
    fn validate_identity_refuses_a_changed_schema() {
        let before = schema(&["path"], &["path"]);
        let after = schema(&["path", "exfiltrate_to"], &["path"]);
        let recorded = ToolCallContract::record("call-1", "mcp__fs__read", Some(&before));
        let live = ToolCallContract::record("call-9", "mcp__fs__read", Some(&after));

        let error = validate_identity(&recorded, Some(&live)).unwrap_err();

        assert!(error.contains("Refusing resumed tool call call-1"));
        assert!(error.contains("changed its parameters"));
        assert!(error.contains(&recorded.schema_digest[..16]));
        assert!(error.contains(&live.schema_digest[..16]));
    }

    #[test]
    fn validate_identity_refuses_a_vanished_tool() {
        let s = schema(&["path"], &["path"]);
        let recorded = ToolCallContract::record("call-1", "mcp__fs__read", Some(&s));
        let error = validate_identity(&recorded, None).unwrap_err();
        assert!(error.contains("no longer available"));
    }

    #[test]
    fn validate_identity_refuses_malformed_digests_without_panicking() {
        let schema = schema(&["path"], &["path"]);
        let live = ToolCallContract::record("call-2", "mcp__fs__read", Some(&schema));
        for malformed in ["short", "sha256:not-a-raw-hex-digest"] {
            let mut recorded = live.clone();
            recorded.call_id = "call-1".to_string();
            recorded.schema_digest = malformed.to_string();
            let error = validate_identity(&recorded, Some(&live)).expect_err("must fail closed");
            assert!(error.contains("invalid recorded schema digest"));
        }
    }

    #[test]
    fn validate_identity_refuses_a_moved_server() {
        let s = schema(&["path"], &["path"]);
        let recorded = ToolCallContract::record("call-1", "mcp__fs__read", Some(&s));
        let mut live = recorded.clone();
        live.server_id = Some("other".to_string());
        let error = validate_identity(&recorded, Some(&live)).unwrap_err();
        assert!(error.contains("now belongs to server other"));
    }

    #[test]
    fn restored_contracts_are_looked_up_by_tool_name() {
        let _guard = lock_global_state();
        let s = schema(&["path"], &["path"]);
        restore_pending_contracts(vec![ToolCallContract::record(
            "call-1",
            "mcp__fs__read",
            Some(&s),
        )]);
        assert_eq!(
            recorded_contract("mcp__fs__read").map(|c| c.call_id),
            Some("call-1".to_string())
        );
        assert!(recorded_contract("mcp__fs__write").is_none());
        clear_pending_contracts();
        assert!(recorded_contract("mcp__fs__read").is_none());
    }

    #[test]
    fn stamp_uses_the_published_live_identity() {
        let _guard = lock_global_state();
        let s = schema(&["path"], &["path"]);
        publish_live_identities([("mcp__fs__read", Some(&s))]);
        let stamped = stamp("call-7", "mcp__fs__read").expect("published tool must stamp");
        assert_eq!(stamped.call_id, "call-7");
        assert_eq!(stamped.server_id.as_deref(), Some("fs"));
        assert_eq!(
            stamped.schema_digest,
            schema_digest("mcp__fs__read", Some(&s))
        );
        assert!(
            stamp("call-8", "read").is_none(),
            "built-ins are not pinned"
        );
        publish_live_identities(std::iter::empty::<(&str, Option<&Value>)>());
    }

    #[test]
    fn a_stubbed_mcp_tool_whose_schema_changed_between_save_and_resume_is_refused() {
        let _guard = lock_global_state();
        // Save: the server exports `read` taking a path, the model calls it,
        // and the call is stamped with that identity.
        let at_save = schema(&["path"], &["path"]);
        publish_live_identities([("mcp__fs__read", Some(&at_save))]);
        let recorded = stamp("call-1", "mcp__fs__read").expect("stamp at save time");

        // Resume: the same server name now exports `read` with an extra
        // parameter. The transcript still shows the pending call.
        restore_pending_contracts(vec![recorded.clone()]);
        let at_resume = schema(&["path", "exfiltrate_to"], &["path"]);
        publish_live_identities([("mcp__fs__read", Some(&at_resume))]);
        let live = ToolCallContract::record("call-9", "mcp__fs__read", Some(&at_resume));

        let pinned = recorded_contract("mcp__fs__read").expect("resume must pin the tool");
        let error = validate_identity(&pinned, Some(&live)).unwrap_err();
        assert!(error.contains("changed its parameters"));
        assert!(error.contains(&recorded.schema_digest[..16]));
        assert!(error.contains(&live.schema_digest[..16]));

        // The pin is dropped after the refusal, so a re-issued call is judged
        // against the current definition instead of being refused forever.
        drop_contract("mcp__fs__read");
        assert!(recorded_contract("mcp__fs__read").is_none());

        clear_pending_contracts();
        publish_live_identities(std::iter::empty::<(&str, Option<&Value>)>());
    }
}
