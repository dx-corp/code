//! Background command dispatch keeps the existing owner bound to host scope.
use super::*;

impl ToolExecutor {
    #[cfg(test)]
    pub(crate) fn with_test_code_authority(mut self) -> Self {
        self.code_authority = Some(crate::code_authority::CodeToolAuthority::for_test(vec![]));
        self
    }

    #[cfg(test)]
    pub(crate) fn with_test_code_authority_decisions(
        mut self,
        decisions: Vec<Result<crate::code_authority::CodeAuthorityDecision, String>>,
    ) -> Self {
        self.code_authority = Some(crate::code_authority::CodeToolAuthority::for_test(
            decisions,
        ));
        self
    }

    /// The host binds access; model arguments cannot choose their task owner.
    pub fn with_background_task_access(
        mut self,
        access: background_tasks::BackgroundTaskAccess,
    ) -> Self {
        self.background_task_access = access;
        self
    }

    pub(super) async fn execute_background_task(&self, args: &Value) -> ToolResult {
        let scope = match &self.background_task_access {
            background_tasks::BackgroundTaskAccess::Legacy => None,
            background_tasks::BackgroundTaskAccess::Scoped(scope) => Some(scope),
            background_tasks::BackgroundTaskAccess::Denied => {
                return ToolResult::failure(
                    "Background tasks require an authorized session binding",
                );
            }
        };
        if let Some(scope) = scope {
            if let Some(id) = args.get("taskId").and_then(Value::as_str) {
                if !background_tasks::task_belongs_to_scope(id, scope) {
                    return ToolResult::failure("Task not found".to_string());
                }
            }
        }
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        match action {
            "start" => {
                let command = match args.get("command").and_then(|v| v.as_str()) {
                    Some(cmd) => cmd.to_string(),
                    None => {
                        return ToolResult::failure("command required for start".to_string());
                    }
                };
                let requested_cwd = args.get("cwd").and_then(|v| v.as_str());
                let cwd = match requested_cwd {
                    Some(raw) if !raw.trim().is_empty() => {
                        let raw = raw.trim();
                        // Resolve relative to the session workspace
                        // (not whatever directory the Maestro process
                        // itself happens to be running from), so the
                        // sandbox check below and the spawned
                        // process see the same path.
                        if std::path::Path::new(raw).is_absolute() {
                            raw.to_string()
                        } else {
                            std::path::Path::new(&self.cwd)
                                .join(raw)
                                .to_string_lossy()
                                .to_string()
                        }
                    }
                    _ => self.cwd.clone(),
                };
                // `background_tasks::start` passes `cwd` straight
                // through as the sandbox spawn cwd, and the sandbox
                // policy automatically treats a spawn's cwd as a
                // writable root (see `get_writable_roots_with_cwd`).
                // A model-supplied cwd must not be allowed to expand
                // the writable footprint beyond what the workspace
                // sandbox already grants -- otherwise `background_tasks
                // { cwd: "$HOME" }` silently makes the whole home
                // directory writable under a policy advertised as
                // workspace-write.
                if let Some(policy) = &self.sandbox_policy {
                    if !policy.allows_write_to(
                        std::path::Path::new(&self.cwd),
                        std::path::Path::new(&cwd),
                    ) {
                        return ToolResult::failure(format!(
                            "background_tasks cwd '{cwd}' is outside the sandbox's \
                                     writable roots; omit cwd to use the workspace or pick a \
                                     directory the sandbox already allows"
                        ));
                    }
                }
                let shell = args
                    .get("shell")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let env = args.get("env").and_then(|v| v.as_object()).map(|map| {
                    map.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect::<std::collections::HashMap<_, _>>()
                });
                match background_tasks::start_owned(
                    command,
                    cwd,
                    self.cwd.clone(),
                    shell,
                    env,
                    self.sandbox_policy.clone(),
                    scope.cloned(),
                )
                .await
                {
                    Ok(task) => {
                        let details = serde_json::json!({
                            "id": task.id,
                            "pid": task.pid,
                            "status": "running",
                            "logPath": task.log_path
                        });
                        ToolResult::success(format!("Started task {}", task.id))
                            .with_details(details)
                    }
                    Err(err) => ToolResult::failure(err),
                }
            }
            "stop" => {
                let id = match args.get("taskId").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => {
                        return ToolResult::failure("taskId required for stop".to_string());
                    }
                };
                match background_tasks::stop(id) {
                    Ok(task) => ToolResult::success(format!("Stopped task {}", task.id)),
                    Err(err) => ToolResult::failure(err),
                }
            }
            "logs" => {
                let id = match args.get("taskId").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => {
                        return ToolResult::failure("taskId required for logs".to_string());
                    }
                };
                let lines = args
                    .get("lines")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(40) as usize;
                match background_tasks::logs(id, lines) {
                    Ok(logs) => ToolResult::success(logs),
                    Err(err) => ToolResult::failure(err),
                }
            }
            "waitForRotation" | "wait_for_rotation" => {
                let id = match args.get("taskId").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => {
                        return ToolResult::failure(
                            "taskId required for waitForRotation".to_string(),
                        );
                    }
                };
                // Default 0 = non-blocking snapshot (do not stall the turn).
                let timeout_ms = args
                    .get("timeoutMs")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                match background_tasks::wait_for_rotation(id, Duration::from_millis(timeout_ms))
                    .await
                {
                    Ok(info) => {
                        let rotated_at = info
                            .rotated_at
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .ok()
                            .map(|duration| duration.as_millis() as u64);
                        let details = serde_json::json!({
                            "logPath": info.log_path.to_string_lossy(),
                            "archivePath": info.archive_path.to_string_lossy(),
                            "rotatedAt": rotated_at
                        });
                        ToolResult::success(format!("Log rotated for task {}", id))
                            .with_details(details)
                    }
                    Err(err) => ToolResult::failure(err),
                }
            }
            _ => {
                let tasks = match scope {
                    Some(scope) => match background_tasks::list_scoped(scope) {
                        Ok(tasks) => tasks,
                        Err(error) => return ToolResult::failure(error),
                    },
                    None => background_tasks::list(),
                };
                let summary = tasks
                    .iter()
                    .map(|t| {
                        let mut line = format!("{} {:?} {}", t.id, t.status, t.command);
                        if t.log_write_failed {
                            if let Some(reason) = &t.log_write_error {
                                let reason = reason.replace(['\n', '\r'], " ");
                                line.push_str(&format!(" [log write failed: {reason}]"));
                            } else {
                                line.push_str(" [log write failed]");
                            }
                        }
                        line
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let details = serde_json::json!({ "count": tasks.len() });
                ToolResult::success(if summary.is_empty() {
                    "No background tasks".to_string()
                } else {
                    summary
                })
                .with_details(details)
            }
        }
    }
}
