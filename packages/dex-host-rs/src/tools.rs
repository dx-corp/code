//! The `Tools` port over the workspace filesystem.
//!
//! Two tools, standing in for `local-host-rs`'s much larger registry
//! (`packages/local-host-rs/src/tools/`): one read (`fs.read_file`) and one
//! mutation in the `Approval` governance class (`fs.write_file`), which the
//! headless host allows without a prompt. A real cutover ports the
//! rest of that registry the same way — each tool becomes a `ToolSpec` plus
//! a `run` arm, and `NativeHost::requires_approval`'s per-call decision
//! becomes this module's `policy` — not a rewrite of the tools themselves.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dex_loop::{
    CancellationToken, Context, ExecutorKind, GovernanceClass, PrincipalId, ProposedCall, ThreadId,
    ToolName, ToolResult, ToolSpec, Tools, Verdict,
};
use serde_json::Value;

pub const READ_FILE: &str = "fs.read_file";
pub const WRITE_FILE: &str = "fs.write_file";

fn catalog() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            description: "Read a text file from the workspace.".into(),
            name: ToolName::new(READ_FILE),
            label: "Read a workspace file".into(),
            schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the workspace root",
                    },
                },
            }),
            read_only: true,
            core: true,
            governance: GovernanceClass::Plain,
            executor: ExecutorKind::InProcess,
        },
        ToolSpec {
            description: "Write text content to a file in the workspace.".into(),
            name: ToolName::new(WRITE_FILE),
            label: "Write a workspace file".into(),
            schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path", "content"],
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the workspace root",
                    },
                    "content": {"type": "string"},
                },
            }),
            read_only: false,
            core: true,
            governance: GovernanceClass::Approval,
            executor: ExecutorKind::InProcess,
        },
    ]
}

/// Confines `relative` to `root`: rejects an absolute path and any `..`
/// component instead of relying on `canonicalize`, which requires the
/// target to already exist and so cannot guard a write to a new file.
fn resolve_within(root: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.trim().is_empty() {
        return Err("invalid call: args.path must not be empty".to_owned());
    }
    let candidate = Path::new(relative);
    let mut resolved = root.to_path_buf();
    for component in candidate.components() {
        match component {
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(format!("invalid call: {relative:?} must not contain .."));
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!(
                    "invalid call: {relative:?} must be relative to the workspace root"
                ));
            }
        }
    }
    Ok(resolved)
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("invalid call: args.{key} must be a string"))
}

async fn read_file(root: &Path, args: &Value) -> ToolResult {
    let path = match string_arg(args, "path") {
        Ok(path) => path,
        Err(reason) => return ToolResult::error(reason),
    };
    let resolved = match resolve_within(root, path) {
        Ok(resolved) => resolved,
        Err(reason) => return ToolResult::error(reason),
    };
    match tokio::fs::read_to_string(&resolved).await {
        Ok(contents) => ToolResult::text(contents),
        Err(error) => ToolResult::error(format!("could not read {path}: {error}")),
    }
}

async fn write_file(root: &Path, args: &Value) -> ToolResult {
    let path = match string_arg(args, "path") {
        Ok(path) => path,
        Err(reason) => return ToolResult::error(reason),
    };
    let content = match string_arg(args, "content") {
        Ok(content) => content,
        Err(reason) => return ToolResult::error(reason),
    };
    let resolved = match resolve_within(root, path) {
        Ok(resolved) => resolved,
        Err(reason) => return ToolResult::error(reason),
    };
    if let Some(parent) = resolved.parent() {
        if let Err(error) = tokio::fs::create_dir_all(parent).await {
            return ToolResult::error(format!("could not create {}: {error}", parent.display()));
        }
    }
    match tokio::fs::write(&resolved, content).await {
        Ok(()) => ToolResult::text(format!("wrote {} bytes to {path}", content.len())),
        Err(error) => ToolResult::error(format!("could not write {path}: {error}")),
    }
}

/// The `Tools` port over the workspace filesystem at `root`.
#[derive(Clone)]
pub struct LocalTools {
    root: Arc<PathBuf>,
    catalog: Arc<[ToolSpec]>,
}

impl LocalTools {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: Arc::new(root.as_ref().to_path_buf()),
            catalog: Arc::from(catalog()),
        }
    }
}

impl Tools for LocalTools {
    fn catalog(&self) -> &[ToolSpec] {
        &self.catalog
    }

    async fn search(&self, _principal: &PrincipalId, query: &str) -> Vec<ToolName> {
        let query = query.to_ascii_lowercase();
        if query.trim().is_empty() {
            return Vec::new();
        }
        self.catalog
            .iter()
            .filter(|spec| spec.label.to_ascii_lowercase().contains(&query))
            .map(|spec| spec.name.clone())
            .collect()
    }

    async fn policy(&self, _ctx: &Context, call: &ProposedCall) -> Verdict {
        // Maestro turns are headless: no human approves a tool call, so this
        // host never returns `NeedsApproval`. An `Approval`-class tool (the
        // mutation `fs.write_file`) is allowed outright; its
        // `ToolStarted`/`ToolFinished` pair in the log is the audit record.
        // A hard deny would be `Verdict::Deny`; no local tool has one today.
        let _ = call;
        Verdict::Allow
    }

    async fn run(
        &self,
        _thread: &ThreadId,
        call: &ProposedCall,
        _cancel: &CancellationToken,
    ) -> ToolResult {
        match call.tool.as_str() {
            READ_FILE => read_file(&self.root, &call.args).await,
            WRITE_FILE => write_file(&self.root, &call.args).await,
            other => ToolResult::error(format!("unknown local tool: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_loop::Outcome;
    use tempfile::TempDir;

    fn thread() -> ThreadId {
        ThreadId {
            org: "org-1".into(),
            workspace: "ws-1".into(),
            thread: "thread-1".into(),
        }
    }

    fn call(tool: &str, args: Value) -> ProposedCall {
        ProposedCall::new(
            dex_loop::CallId::new("c1"),
            ToolName::new(tool),
            args,
            PrincipalId::new("alice"),
        )
    }

    #[tokio::test]
    async fn read_file_returns_contents() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "hello").expect("seed file");
        let tools = LocalTools::new(dir.path());
        let cancel = CancellationToken::new();
        let result = tools
            .run(
                &thread(),
                &call(READ_FILE, serde_json::json!({"path": "a.txt"})),
                &cancel,
            )
            .await;
        assert_eq!(result, ToolResult::text("hello"));
    }

    #[tokio::test]
    async fn read_file_rejects_path_traversal() {
        let dir = TempDir::new().expect("tempdir");
        let tools = LocalTools::new(dir.path());
        let cancel = CancellationToken::new();
        let result = tools
            .run(
                &thread(),
                &call(READ_FILE, serde_json::json!({"path": "../escape.txt"})),
                &cancel,
            )
            .await;
        assert_eq!(result.outcome, Outcome::Failed);
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories_and_writes_content() {
        let dir = TempDir::new().expect("tempdir");
        let tools = LocalTools::new(dir.path());
        let cancel = CancellationToken::new();
        let result = tools
            .run(
                &thread(),
                &call(
                    WRITE_FILE,
                    serde_json::json!({"path": "nested/b.txt", "content": "world"}),
                ),
                &cancel,
            )
            .await;
        assert_eq!(result.outcome, Outcome::Succeeded);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("nested/b.txt")).expect("read back"),
            "world"
        );
    }

    #[tokio::test]
    async fn every_tool_is_allowed_because_turns_are_headless() {
        let tools = LocalTools::new(".");
        let read_verdict = tools
            .policy(
                &fake_ctx(),
                &call(READ_FILE, serde_json::json!({"path": "a"})),
            )
            .await;
        assert_eq!(read_verdict, Verdict::Allow);
        let write_verdict = tools
            .policy(
                &fake_ctx(),
                &call(WRITE_FILE, serde_json::json!({"path": "a", "content": "x"})),
            )
            .await;
        assert_eq!(write_verdict, Verdict::Allow);
    }

    /// `Tools::policy` here does not read `ctx`, so an empty rehydrated
    /// context is enough to exercise it without a real thread history.
    fn fake_ctx() -> Context {
        dex_loop::rehydrate(thread(), &[])
    }
}
