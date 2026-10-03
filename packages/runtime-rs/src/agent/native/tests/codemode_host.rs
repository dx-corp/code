//! Controlled completion timing at the actual native-host boundary.

use super::*;

impl RuntimeTestHost {
    pub(super) fn fixture_read_wave_stream<'a>(
        &'a self,
        calls: &'a [NativeReadOnlyToolCall],
        event_tx: &'a mpsc::UnboundedSender<FromAgent>,
        cancel: Option<CancellationToken>,
        completions: mpsc::UnboundedSender<(String, ToolExecution)>,
    ) -> NativeHostFuture<'a, HashMap<String, ToolExecution>> {
        if self.read_delays.is_empty() {
            return Box::pin(async move {
                let results = self.execute_read_only_wave(calls, event_tx, cancel).await;
                for (id, execution) in &results {
                    let _ = completions.send((id.clone(), execution.clone()));
                }
                results
            });
        }
        Box::pin(async move {
            use futures::StreamExt;
            self.read_only_waves
                .lock()
                .unwrap()
                .push(calls.iter().map(|call| call.call_id.clone()).collect());
            let mut pending = futures::stream::FuturesUnordered::new();
            for call in calls {
                let cancel = cancel.clone().unwrap_or_default();
                pending.push(async move {
                    self.assert_effect_pending(&call.call_id);
                    let delay = self.read_delays.get(call.args["path"].as_str().unwrap_or("")).copied().unwrap_or_default();
                    let execution = tokio::select! {
                        () = tokio::time::sleep(delay) => self.execution(&call.call_id, &call.tool_name, &call.args),
                        () = cancel.cancelled() => ToolExecution::cancelled(&call.call_id, &call.tool_name, ExecutionSource::Native, ExecutionPhase::Running),
                    };
                    (call.call_id.clone(), execution)
                });
            }
            let mut results = HashMap::new();
            while let Some((id, execution)) = pending.next().await {
                let _ = completions.send((id.clone(), execution.clone()));
                results.insert(id, execution);
            }
            results
        })
    }
    pub(super) fn execution(&self, call_id: &str, name: &str, args: &Value) -> ToolExecution {
        if name.starts_with("mcp__") {
            if let Some(result) = &self.mcp_fixture {
                return ToolExecution::from_legacy(
                    call_id,
                    name,
                    ExecutionSource::Native,
                    result.clone(),
                );
            }
        }
        let output = match name.to_ascii_lowercase().as_str() {
            "read" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let path = self.cwd.join(path);
                std::fs::read_to_string(path).unwrap_or_else(|_| "fixture read result".to_owned())
            }
            "write" | "edit" => args
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("fixture write result")
                .to_owned(),
            "bash" => {
                let command = args.get("command").and_then(Value::as_str).unwrap_or("");
                if command.trim().is_empty() || command.contains("pwd") {
                    self.cwd.display().to_string()
                } else if let Some(value) = command
                    .strip_prefix("printf ")
                    .or_else(|| command.strip_prefix("echo "))
                {
                    value.trim_matches([' ', '\'', '"']).to_owned()
                } else {
                    format!("fixture bash result: {command}")
                }
            }
            "update_goal" => args.get("status").and_then(Value::as_str).map_or_else(
                || serde_json::json!({"goal": {"status": "active"}}).to_string(),
                |status| serde_json::json!({"goal": {"status": status}}).to_string(),
            ),
            "todo" => serde_json::json!({"open": 0}).to_string(),
            _ => "fixture tool result".to_owned(),
        };
        let execution = ToolExecution::from_legacy(
            call_id,
            name,
            ExecutionSource::Native,
            ToolResult::success(output),
        );
        self.completed_tool_executions
            .fetch_add(1, Ordering::SeqCst);
        execution
    }
}
