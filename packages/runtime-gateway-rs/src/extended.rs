//! Native implementations for the remaining web control surfaces.

use super::*;

#[derive(Debug, Default)]
pub(crate) struct ExtendedApiState {
    pub(crate) automations: Option<crate::automations::AutomationStore>,
    workspaces: HashMap<String, Value>,
    traces: HashMap<String, Value>,
    headless_sessions: HashMap<String, Value>,
    router_decisions: Vec<Value>,
    router_metrics: Vec<Value>,
    router_overrides: HashMap<String, Value>,
    values: HashMap<String, Value>,
}

pub(crate) fn is_extended_endpoint(head: &RequestHead) -> bool {
    if head.method == "POST" && head.path == "/.well-known/evalops/remote-runner/drain" {
        return true;
    }
    extended_api_matches_path(&head.path)
}

// Path-only membership test for the extended API surface, independent of HTTP
// method. `runtime_tenant_resource_path` (auth.rs) uses this to bind every
// extended path to a tenant principal; keep it in sync with the routes handled
// by `handle_extended_endpoint`.
pub(crate) fn extended_api_matches_path(path: &str) -> bool {
    if !path.starts_with("/api/") {
        return false;
    }
    if matches!(
        path,
        "/api/a2a/cockpit"
            | "/api/automations"
            | "/api/automations/preview"
            | "/api/automations/magic-docs"
            | "/api/branch"
            | "/api/bridge/status"
            | "/api/compliance/controls"
            | "/api/compliance/generate-report"
            | "/api/composer"
            | "/api/cost"
            | "/api/diagnostics"
            | "/api/fleet"
            | "/api/guardian/status"
            | "/api/guardian/run"
            | "/api/guardian/config"
            | "/api/headless/connections"
            | "/api/headless/sessions"
            | "/api/intelligent-router/decisions"
            | "/api/intelligent-router/metrics"
            | "/api/intelligent-router/overrides"
            | "/api/lsp"
            | "/api/mcp"
            | "/api/memory"
            | "/api/mode"
            | "/api/ollama"
            | "/api/package"
            | "/api/preview"
            | "/api/prompt-suggestion"
            | "/api/queue"
            | "/api/quota"
            | "/api/traces"
            | "/api/ui"
            | "/api/usage/analytics"
            | "/api/workflow"
            | "/api/workspace-configs"
            | "/api/zen"
            | "/api/chat/approval"
            | "/api/chat/client-tool-result"
            | "/api/chat/tool-retry"
            | "/api/run/event"
            | "/api/policy/validate"
            | "/api/admin/enterprise-policy/status"
            | "/api/admin/enterprise-policy/refresh"
            | "/api/admin/enterprise-policy/publish"
            | "/api/admin/enterprise-policy/audit"
            | "/api/admin/cleanup"
            | "/api/admin/warm-caches"
            | "/api/attribution/record-outcome"
    ) {
        return true;
    }
    [
        "/api/automations/",
        "/api/attribution/roi/",
        "/api/compliance/evidence/",
        "/api/headless/sessions/",
        "/api/intelligent-router/overrides/",
        "/api/traces/",
        "/api/usage/analytics/",
        "/api/workspace-configs/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
}
fn managed_policy_response(status: maestro_local_host::safety::ManagedPolicyStatus) -> Vec<u8> {
    let response_status = if status.configured && !status.valid {
        409
    } else {
        200
    };
    json_response(
        response_status,
        &serde_json::json!({ "managedPolicy": status }),
    )
}

fn managed_policy_actor(head: &RequestHead, config: &Config) -> Option<String> {
    let auth = auth_context(head, config)?;
    Some(auth.actor_label())
}

fn managed_policy_audit_event(
    action: &str,
    actor: Option<String>,
    outcome: &str,
    metadata: Option<maestro_local_host::safety::ManagedPolicyMetadata>,
    reason: Option<String>,
) -> maestro_local_host::safety::ManagedPolicyAuditEvent {
    maestro_local_host::safety::ManagedPolicyAuditEvent {
        event_id: format!("managed-policy-{}", now_millis()),
        action: action.to_string(),
        actor,
        recorded_at: now_millis(),
        outcome: outcome.to_string(),
        metadata,
        reason,
    }
}

pub(crate) async fn handle_extended_endpoint(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: RequestHead,
    state: &AppState,
) -> Vec<u8> {
    if let Err(response) = validate_csrf(&head, &state.config) {
        return response;
    }
    if head.path.starts_with("/api/") || head.path == "/.well-known/evalops/remote-runner/drain" {
        // `authorize_extended` applies the tenant-bound write authorization for
        // the whole surface and additionally requires the administrator role for
        // `/api/admin/*` and MCP-config mutations.
        if let Err(response) = authorize_extended(&head, &state.config) {
            return response;
        }
    }
    let body = if matches!(head.method.as_str(), "POST" | "PUT" | "PATCH") {
        match read_request_body(stream, initial, &head).await {
            Ok(body) if body.is_empty() => Value::Object(Map::new()),
            Ok(body) => match serde_json::from_slice(&body) {
                Ok(body) => body,
                Err(error) => {
                    return json_response(
                        400,
                        &serde_json::json!({ "error": format!("invalid JSON request: {error}") }),
                    );
                }
            },
            Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
        }
    } else {
        Value::Object(Map::new())
    };

    if head.path == "/api/mcp" {
        let _mcp_config_guard = state.extended_api.lock().await;
        return match head.method.as_str() {
            "GET" => {
                let config = maestro_local_host::mcp::load_mcp_config_with_managed_connections(
                    Some(&state.config.cwd),
                );
                json_response(
                    200,
                    &serde_json::json!({ "servers": config.servers, "authPresets": [] }),
                )
            }
            "POST" => {
                let action = head.query.get("action").map(String::as_str).unwrap_or("");
                match mutate_mcp_config(&state.config.cwd, action, &body).await {
                    Ok(result) => json_response(200, &result),
                    Err((status, error)) => {
                        json_response(status, &serde_json::json!({ "error": error }))
                    }
                }
            }
            _ => json_response(405, &serde_json::json!({ "error": "Method not allowed" })),
        };
    }

    let mut api = state.extended_api.lock().await;
    match (head.method.as_str(), head.path.as_str()) {
        ("GET", "/api/admin/enterprise-policy/status") => {
            managed_policy_response(maestro_local_host::safety::managed_policy_status())
        }
        ("POST", "/api/admin/enterprise-policy/refresh") => {
            managed_policy_response(maestro_local_host::safety::refresh_managed_policy())
        }
        ("POST", "/api/admin/enterprise-policy/publish") => {
            let envelope = match body
                .get("envelope")
                .cloned()
                .ok_or_else(|| "publish request must include an envelope".to_string())
                .and_then(|value| {
                    serde_json::from_value::<maestro_local_host::safety::ManagedPolicyEnvelope>(
                        value,
                    )
                    .map_err(|error| format!("invalid managed policy envelope: {error}"))
                }) {
                Ok(envelope) => envelope,
                Err(error) => {
                    return json_response(400, &serde_json::json!({ "error": error }));
                }
            };
            let actor = managed_policy_actor(&head, &state.config);
            match maestro_local_host::safety::publish_managed_policy(envelope) {
                Ok(result) => {
                    let event = managed_policy_audit_event(
                        "publish",
                        actor,
                        "accepted",
                        result.status.metadata.clone(),
                        result.status.error.clone(),
                    );
                    if let Err(error) =
                        maestro_local_host::safety::record_managed_policy_audit(event)
                    {
                        return json_response(
                            500,
                            &serde_json::json!({ "error": format!("managed policy published but audit failed: {error}") }),
                        );
                    }
                    json_response(
                        200,
                        &serde_json::json!({
                            "published": result.published,
                            "managedPolicy": result.status,
                        }),
                    )
                }
                Err(error) => {
                    let event = managed_policy_audit_event(
                        "publish",
                        actor,
                        "rejected",
                        None,
                        Some(error.clone()),
                    );
                    if let Err(audit_error) =
                        maestro_local_host::safety::record_managed_policy_audit(event)
                    {
                        return json_response(
                            500,
                            &serde_json::json!({ "error": format!("managed policy rejected and audit failed: {audit_error}") }),
                        );
                    }
                    json_response(
                        409,
                        &serde_json::json!({ "published": false, "error": error }),
                    )
                }
            }
        }
        ("GET", "/api/admin/enterprise-policy/audit") => {
            let limit = head
                .query
                .get("limit")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(50)
                .min(100);
            match maestro_local_host::safety::managed_policy_audit(limit) {
                Ok(events) => {
                    json_response(200, &serde_json::json!({ "managedPolicyAudit": events }))
                }
                Err(error) => json_response(500, &serde_json::json!({ "error": error })),
            }
        }
        ("POST", "/.well-known/evalops/remote-runner/drain") => {
            json_response(200, &serde_json::json!({ "ok": true, "draining": true }))
        }
        ("GET", "/api/a2a/cockpit") => json_response(
            200,
            &serde_json::json!({ "agents": [], "tasks": [], "runtime": "rust-control-plane" }),
        ),
        ("GET", "/api/bridge/status") => json_response(
            200,
            &serde_json::json!({ "connected": true, "runtime": "rust-control-plane" }),
        ),
        ("GET", "/api/fleet") => {
            json_response(200, &serde_json::json!({ "agents": [], "summary": {} }))
        }
        ("GET", "/api/guardian/status") => json_response(
            200,
            &serde_json::json!({ "enabled": true, "status": "ready", "findings": [] }),
        ),
        ("POST", "/api/guardian/run" | "/api/guardian/config") => {
            json_response(200, &serde_json::json!({ "success": true, "result": body }))
        }
        ("GET", "/api/automations") => {
            let Some(store) = api.automations.as_mut() else {
                return json_response(
                    503,
                    &serde_json::json!({ "error": "automation store unavailable" }),
                );
            };
            match store.list_definitions() {
                Ok(automations) => {
                    json_response(200, &serde_json::json!({ "automations": automations }))
                }
                Err(error) => {
                    json_response(500, &serde_json::json!({ "error": error.to_string() }))
                }
            }
        }
        ("POST", "/api/automations") => {
            let Some(store) = api.automations.as_mut() else {
                return json_response(
                    503,
                    &serde_json::json!({ "error": "automation store unavailable" }),
                );
            };
            match store.upsert(None, &body, now_millis()) {
                Ok(value) => json_response(200, &value),
                Err(error) => {
                    json_response(400, &serde_json::json!({ "error": error.to_string() }))
                }
            }
        }
        ("POST", "/api/automations/preview") => {
            let Some(store) = api.automations.as_ref() else {
                return json_response(
                    503,
                    &serde_json::json!({ "error": "automation store unavailable" }),
                );
            };
            match store.preview(&body, now_millis()) {
                Ok(value) => json_response(200, &value),
                Err(error) => {
                    json_response(400, &serde_json::json!({ "error": error.to_string() }))
                }
            }
        }
        ("GET", "/api/automations/magic-docs") => json_response(
            200,
            &serde_json::json!({
                "schema": crate::automations::AUTOMATION_SCHEMA,
                "documents": [{
                    "schedule": "intervalSeconds (1..86400); cron and remote endpoints are not accepted",
                    "execution": "native_tool_free_turn",
                    "durability": ["atomic_state", "lease", "idempotency", "retry", "signed_receipt"]
                }]
            }),
        ),
        ("GET", path) if automation_runs_path(path).is_some() => {
            let id = automation_runs_path(path).expect("matched automation runs path");
            let Some(store) = api.automations.as_mut() else {
                return json_response(
                    503,
                    &serde_json::json!({ "error": "automation store unavailable" }),
                );
            };
            match store.list_runs(id) {
                Ok(runs) => json_response(
                    200,
                    &serde_json::json!({ "automationId": id, "runs": runs }),
                ),
                Err(error) => {
                    json_response(500, &serde_json::json!({ "error": error.to_string() }))
                }
            }
        }
        ("GET", path) if automation_id_path(path).is_some() => {
            let id = automation_id_path(path).expect("matched automation path");
            let Some(store) = api.automations.as_mut() else {
                return json_response(
                    503,
                    &serde_json::json!({ "error": "automation store unavailable" }),
                );
            };
            match store.get_definition(id) {
                Ok(Some(definition)) => json_response(200, &definition),
                Ok(None) => {
                    json_response(404, &serde_json::json!({ "error": "automation not found" }))
                }
                Err(error) => {
                    json_response(500, &serde_json::json!({ "error": error.to_string() }))
                }
            }
        }
        (method @ ("PATCH" | "DELETE" | "POST"), path) if automation_path(path).is_some() => {
            let (id, action) = automation_path(path).expect("matched automation path");
            let Some(store) = api.automations.as_mut() else {
                return json_response(
                    503,
                    &serde_json::json!({ "error": "automation store unavailable" }),
                );
            };
            if method == "DELETE" {
                return match store.delete(id) {
                    Ok(removed) => json_response(200, &serde_json::json!({ "success": removed })),
                    Err(error) => {
                        json_response(500, &serde_json::json!({ "error": error.to_string() }))
                    }
                };
            }
            if action == Some("run") {
                let fallback_model = {
                    let model = state.selected_model.lock().await;
                    format!("{}/{}", model.provider, model.id)
                };
                let idempotency_key = body.get("idempotencyKey").and_then(Value::as_str);
                let owner = format!("manual-{}", now_millis());
                return match store.claim_manual(
                    id,
                    idempotency_key,
                    &owner,
                    &fallback_model,
                    now_millis(),
                ) {
                    Ok(crate::automations::RunClaim::Claimed(claim)) => {
                        let run_id = claim.run_id.clone();
                        crate::automations::spawn_claimed(
                            state.extended_api.clone(),
                            state.config.cwd.clone(),
                            claim,
                        );
                        json_response(
                            202,
                            &serde_json::json!({
                                "accepted": true,
                                "idempotent": false,
                                "automationId": id,
                                "runId": run_id,
                                "status": "running"
                            }),
                        )
                    }
                    Ok(crate::automations::RunClaim::Existing(run)) => json_response(
                        200,
                        &serde_json::json!({
                            "accepted": true,
                            "idempotent": true,
                            "automationId": id,
                            "runId": run.run_id,
                            "status": run.status,
                            "run": run
                        }),
                    ),
                    Err(error) => {
                        let status = if error.to_string().contains("not found") {
                            404
                        } else {
                            400
                        };
                        json_response(status, &serde_json::json!({ "error": error.to_string() }))
                    }
                };
            }
            match store.upsert(Some(id), &body, now_millis()) {
                Ok(updated) => json_response(200, &updated),
                Err(error) => {
                    json_response(400, &serde_json::json!({ "error": error.to_string() }))
                }
            }
        }
        ("GET", "/api/workspace-configs") => json_response(
            200,
            &serde_json::json!({ "workspaceConfigs": api.workspaces.values().cloned().collect::<Vec<_>>() }),
        ),
        ("POST", "/api/workspace-configs") => {
            let id = body
                .get("workspaceId")
                .or_else(|| body.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("workspace-{}", now_millis()));
            let value = merge_object(body, serde_json::json!({ "workspaceId": id }));
            api.workspaces.insert(id, value.clone());
            json_response(200, &value)
        }
        (method @ ("GET" | "PUT" | "DELETE"), path)
            if path.strip_prefix("/api/workspace-configs/").is_some() =>
        {
            let id = path.trim_start_matches("/api/workspace-configs/");
            match method {
                "GET" => json_response(
                    200,
                    api.workspaces
                        .get(id)
                        .unwrap_or(&serde_json::json!({ "workspaceId": id })),
                ),
                "PUT" => {
                    let value = merge_object(body, serde_json::json!({ "workspaceId": id }));
                    api.workspaces.insert(id.to_string(), value.clone());
                    json_response(200, &value)
                }
                _ => json_response(
                    200,
                    &serde_json::json!({ "success": api.workspaces.remove(id).is_some() }),
                ),
            }
        }
        ("GET", "/api/traces") => json_response(
            200,
            &serde_json::json!({ "traces": api.traces.values().cloned().collect::<Vec<_>>() }),
        ),
        ("POST", "/api/traces") => {
            let id = body
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("trace-{}", now_millis()));
            let value = merge_object(
                body,
                serde_json::json!({ "id": id, "timestamp": now_rfc3339() }),
            );
            api.traces.insert(id, value.clone());
            json_response(200, &value)
        }
        ("GET", path) if path.starts_with("/api/traces/") => {
            let id = path.trim_start_matches("/api/traces/");
            json_response(
                200,
                api.traces
                    .get(id)
                    .unwrap_or(&serde_json::json!({ "id": id })),
            )
        }
        ("GET", "/api/intelligent-router/decisions") => json_response(
            200,
            &serde_json::json!({ "decisions": api.router_decisions }),
        ),
        ("POST", "/api/intelligent-router/decisions") => {
            api.router_decisions.push(body.clone());
            json_response(200, &body)
        }
        ("GET", "/api/intelligent-router/metrics") => {
            json_response(200, &serde_json::json!({ "metrics": api.router_metrics }))
        }
        ("POST", "/api/intelligent-router/metrics") => {
            api.router_metrics.push(body.clone());
            json_response(200, &body)
        }
        ("GET", "/api/intelligent-router/overrides") => json_response(
            200,
            &serde_json::json!({ "overrides": api.router_overrides }),
        ),
        ("POST", "/api/intelligent-router/overrides") => {
            let task_type = body
                .get("taskType")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_string();
            api.router_overrides.insert(task_type, body.clone());
            json_response(200, &body)
        }
        ("DELETE", path) if path.starts_with("/api/intelligent-router/overrides/") => {
            let task_type = path.trim_start_matches("/api/intelligent-router/overrides/");
            json_response(
                200,
                &serde_json::json!({ "success": api.router_overrides.remove(task_type).is_some() }),
            )
        }
        ("POST", "/api/headless/connections") => json_response(
            200,
            &serde_json::json!({ "connectionId": format!("connection-{}", now_millis()), "protocolVersion": maestro_local_host::headless::HEADLESS_PROTOCOL_VERSION }),
        ),
        ("POST", "/api/headless/sessions") => {
            let id = format!("headless-{}", now_millis());
            let value = merge_object(
                body,
                serde_json::json!({ "id": id, "status": "ready", "createdAt": now_rfc3339() }),
            );
            api.headless_sessions.insert(id, value.clone());
            json_response(200, &value)
        }
        ("GET", path)
            if path.starts_with("/api/headless/sessions/") && path.ends_with("/events") =>
        {
            response_with_extra_headers(
                200,
                "text/event-stream",
                b"event: ready\ndata: {\"type\":\"ready\"}\n\n",
                "Cache-Control: no-cache\r\n",
            )
        }
        ("GET", path) if path.starts_with("/api/headless/sessions/") => {
            let id = path.trim_start_matches("/api/headless/sessions/");
            json_response(
                200,
                api.headless_sessions
                    .get(id)
                    .unwrap_or(&serde_json::json!({ "id": id, "status": "ready" })),
            )
        }
        ("POST", path) if path.starts_with("/api/headless/sessions/") => {
            let tail = path.trim_start_matches("/api/headless/sessions/");
            let (id, action) = tail.split_once('/').unwrap_or((tail, "messages"));
            if matches!(action, "disconnect") {
                api.headless_sessions.remove(id);
            }
            json_response(
                200,
                &serde_json::json!({ "success": true, "sessionId": id, "action": action }),
            )
        }
        ("GET", "/api/usage/analytics") => json_response(
            200,
            &serde_json::json!({ "period": "all", "usage": usage_snapshot(&state.config.usage_file_path).await }),
        ),
        ("GET", path) if path.starts_with("/api/usage/analytics/") => json_response(
            200,
            &serde_json::json!({ "period": path.trim_start_matches("/api/usage/analytics/"), "usage": usage_snapshot(&state.config.usage_file_path).await }),
        ),
        ("GET", "/api/compliance/controls") => {
            json_response(200, &serde_json::json!({ "controls": [] }))
        }
        ("GET", path) if path.starts_with("/api/compliance/evidence/") => json_response(
            200,
            &serde_json::json!({ "controlId": path.rsplit('/').next(), "evidence": [] }),
        ),
        ("POST", "/api/compliance/generate-report") => json_response(
            200,
            &serde_json::json!({ "generated": true, "report": body, "generatedAt": now_rfc3339() }),
        ),
        ("POST", "/api/attribution/record-outcome") => {
            json_response(200, &serde_json::json!({ "success": true }))
        }
        ("GET", path) if path.starts_with("/api/attribution/roi/") => json_response(
            200,
            &serde_json::json!({ "agentId": path.rsplit('/').next(), "roi": 0, "outcomes": [] }),
        ),
        ("GET", "/api/lsp") => json_response(
            200,
            &serde_json::json!({ "enabled": maestro_local_host::lsp::is_lsp_enabled(), "servers": [], "diagnostics": [] }),
        ),
        ("POST", "/api/lsp") => json_response(
            200,
            &serde_json::json!({ "success": true, "request": body }),
        ),
        ("GET", "/api/workflow") => workflow_dashboard_response(&state.config.cwd),
        ("POST", "/api/workflow") => workflow_mutation_response(&state.config.cwd, &body),
        ("GET", path) if is_generic_get(path) => generic_get(path, &head, &api),
        ("POST", path) if is_generic_post(path) => {
            api.values.insert(path.to_string(), body.clone());
            json_response(200, &serde_json::json!({ "success": true, "result": body }))
        }
        _ => json_response(404, &serde_json::json!({ "error": "Not found" })),
    }
}

fn workflow_dashboard_response(cwd: &Path) -> Vec<u8> {
    let store = maestro_local_host::workflow_runtime::WorkflowStore::for_workspace(cwd);
    match (store.list(), store.dashboard()) {
        (Ok(items), Ok(groups)) => json_response(
            200,
            &serde_json::json!({
                "runtime": "rust-workflow-ledger",
                "items": items,
                "groups": groups,
            }),
        ),
        (Err(error), _) | (_, Err(error)) => {
            json_response(500, &serde_json::json!({ "error": error }))
        }
    }
}

fn workflow_mutation_response(cwd: &Path, body: &Value) -> Vec<u8> {
    use maestro_local_host::workflow_runtime::{WorkflowRun, WorkflowSpec, WorkflowStore};

    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("start");
    let store = WorkflowStore::for_workspace(cwd);
    let result = match action {
        "start" => body
            .get("spec")
            .cloned()
            .ok_or_else(|| "workflow start requires spec".to_string())
            .and_then(|value| {
                serde_json::from_value::<WorkflowSpec>(value)
                    .map_err(|error| format!("invalid workflow spec: {error}"))
            })
            .and_then(|spec| {
                WorkflowRun::start(
                    spec,
                    body.get("args")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                )
            }),
        "pause" | "resume" | "stop" | "record_usage" => {
            let id = body
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("workflow {action} requires id"));
            id.and_then(|id| store.get(id)).and_then(|mut run| {
                let mutation = match action {
                    "pause" => run.pause(),
                    "resume" => {
                        let expected_spec_sha = body
                            .get("expectedSpecSha")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                "workflow resume requires expectedSpecSha".to_string()
                            })?;
                        let args = body.get("args").ok_or_else(|| {
                            "workflow resume requires the original args".to_string()
                        })?;
                        run.resume(expected_spec_sha, args)
                    }
                    "stop" => run.stop(
                        body.get("reason")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    ),
                    "record_usage" => {
                        let new_agents = body
                            .get("newAgents")
                            .and_then(Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok())
                            .unwrap_or(0);
                        let active_agents = body
                            .get("activeAgents")
                            .and_then(Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok())
                            .unwrap_or(run.active_agents);
                        let tokens = body.get("tokens").and_then(Value::as_u64).unwrap_or(0);
                        run.record_usage(new_agents, active_agents, tokens)
                    }
                    _ => unreachable!(),
                };
                if let Err(error) = mutation {
                    if run.status == maestro_local_host::workflow_runtime::WorkflowRunStatus::Failed
                    {
                        store.append(&run)?;
                    }
                    return Err(error);
                }
                Ok(run)
            })
        }
        other => Err(format!("unknown workflow action: {other}")),
    };

    match result {
        Ok(run) => match store.append(&run) {
            Ok(()) => json_response(200, &serde_json::json!({ "success": true, "run": run })),
            Err(error) => json_response(500, &serde_json::json!({ "error": error })),
        },
        Err(error) => json_response(
            409,
            &serde_json::json!({ "success": false, "error": error }),
        ),
    }
}

async fn mutate_mcp_config(cwd: &Path, action: &str, body: &Value) -> Result<Value, (u16, String)> {
    let scope = writable_mcp_scope(body.get("scope").and_then(Value::as_str))?;
    let path = writable_mcp_config_path(cwd, scope)?;
    let mut document = read_mcp_config_document(&path).await?;

    match action {
        "add-server" | "update-server" => {
            let mut server_value = body
                .get("server")
                .cloned()
                .ok_or_else(|| (400, "MCP server is required".to_string()))?;
            if let Some(server) = server_value.as_object_mut() {
                server.retain(|_, value| !value.is_null());
            }
            let mut server: maestro_local_host::mcp::McpServerConfig =
                serde_json::from_value(server_value)
                    .map_err(|error| (400, format!("invalid MCP server: {error}")))?;
            server.scope = scope;
            server.validate().map_err(|error| (400, error))?;

            let original_name = if action == "update-server" {
                body.get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| (400, "MCP server name is required".to_string()))?
            } else {
                server.name.as_str()
            };
            if original_name != server.name && has_mcp_server_entry(&document, &server.name)? {
                return Err((409, format!("MCP server already exists: {}", server.name)));
            }
            let existed = remove_mcp_server_entry(&mut document, original_name)?;
            if action == "add-server" && existed {
                return Err((409, format!("MCP server already exists: {original_name}")));
            }
            if action == "update-server" && !existed {
                return Err((404, format!("MCP server not found: {original_name}")));
            }
            mcp_servers_array(&mut document)?.push(
                serde_json::to_value(&server)
                    .map_err(|error| (500, format!("failed to serialize MCP server: {error}")))?,
            );
            write_mcp_config_document(&path, &document).await?;
            Ok(serde_json::json!({
                "name": server.name,
                "scope": mcp_scope_name(scope),
                "path": path,
                "server": server
            }))
        }
        "remove-server" => {
            let name = body
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| (400, "MCP server name is required".to_string()))?;
            if !remove_mcp_server_entry(&mut document, name)? {
                return Err((404, format!("MCP server not found: {name}")));
            }
            write_mcp_config_document(&path, &document).await?;
            let fallback =
                maestro_local_host::mcp::load_mcp_config_with_managed_connections(Some(cwd))
                    .get_server(name)
                    .map(|server| {
                        serde_json::json!({
                            "name": server.name,
                            "scope": mcp_scope_name(server.scope)
                        })
                    });
            Ok(serde_json::json!({
                "name": name,
                "scope": mcp_scope_name(scope),
                "path": path,
                "fallback": fallback
            }))
        }
        _ => Err((400, format!("unsupported MCP action: {action}"))),
    }
}

fn writable_mcp_scope(
    scope: Option<&str>,
) -> Result<maestro_local_host::mcp::McpConfigScope, (u16, String)> {
    match scope.unwrap_or("local") {
        "local" => Ok(maestro_local_host::mcp::McpConfigScope::Local),
        "project" => Ok(maestro_local_host::mcp::McpConfigScope::Project),
        "user" => Ok(maestro_local_host::mcp::McpConfigScope::User),
        value => Err((400, format!("invalid writable MCP scope: {value}"))),
    }
}

fn mcp_scope_name(scope: maestro_local_host::mcp::McpConfigScope) -> &'static str {
    match scope {
        maestro_local_host::mcp::McpConfigScope::Managed => "managed",
        maestro_local_host::mcp::McpConfigScope::Local => "local",
        maestro_local_host::mcp::McpConfigScope::Project => "project",
        maestro_local_host::mcp::McpConfigScope::User => "user",
        maestro_local_host::mcp::McpConfigScope::Enterprise => "enterprise",
    }
}

fn writable_mcp_config_path(
    cwd: &Path,
    scope: maestro_local_host::mcp::McpConfigScope,
) -> Result<PathBuf, (u16, String)> {
    match scope {
        maestro_local_host::mcp::McpConfigScope::Managed => {
            Err((400, "managed MCP config is read-only".to_string()))
        }
        maestro_local_host::mcp::McpConfigScope::Local => {
            Ok(cwd.join(".composer").join("mcp.local.json"))
        }
        maestro_local_host::mcp::McpConfigScope::Project => {
            Ok(cwd.join(".composer").join("mcp.json"))
        }
        maestro_local_host::mcp::McpConfigScope::User => {
            if let Some(path) = trimmed_env("MAESTRO_USER_MCP_PATH") {
                let path = PathBuf::from(path);
                return Ok(if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                });
            }
            let home = trimmed_env("MAESTRO_HOME")
                .map(PathBuf::from)
                .or_else(|| trimmed_env("HOME").map(|home| PathBuf::from(home).join(".maestro")))
                .ok_or_else(|| (500, "cannot resolve user MCP config path".to_string()))?;
            Ok(home.join("mcp.json"))
        }
        maestro_local_host::mcp::McpConfigScope::Enterprise => {
            Err((400, "enterprise MCP config is read-only".to_string()))
        }
    }
}

async fn read_mcp_config_document(path: &Path) -> Result<Value, (u16, String)> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let document: Value = serde_json::from_slice(&bytes).map_err(|error| {
                (
                    400,
                    format!("invalid MCP config {}: {error}", path.display()),
                )
            })?;
            if document.is_object() {
                Ok(document)
            } else {
                Err((
                    400,
                    format!("MCP config must be an object: {}", path.display()),
                ))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::json!({ "servers": [] }))
        }
        Err(error) => Err((
            500,
            format!("failed to read MCP config {}: {error}", path.display()),
        )),
    }
}

fn mcp_servers_array(document: &mut Value) -> Result<&mut Vec<Value>, (u16, String)> {
    let object = document
        .as_object_mut()
        .ok_or_else(|| (400, "MCP config must be an object".to_string()))?;
    let servers = object
        .entry("servers".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    servers
        .as_array_mut()
        .ok_or_else(|| (400, "MCP config servers must be an array".to_string()))
}

fn remove_mcp_server_entry(document: &mut Value, name: &str) -> Result<bool, (u16, String)> {
    let object = document
        .as_object_mut()
        .ok_or_else(|| (400, "MCP config must be an object".to_string()))?;
    let mut removed = false;
    if let Some(servers) = object.get_mut("servers") {
        let servers = servers
            .as_array_mut()
            .ok_or_else(|| (400, "MCP config servers must be an array".to_string()))?;
        let original_len = servers.len();
        servers.retain(|server| server.get("name").and_then(Value::as_str) != Some(name));
        removed |= servers.len() != original_len;
    }
    if let Some(servers) = object.get_mut("mcpServers") {
        let servers = servers
            .as_object_mut()
            .ok_or_else(|| (400, "MCP config mcpServers must be an object".to_string()))?;
        removed |= servers.remove(name).is_some();
    }
    Ok(removed)
}

fn has_mcp_server_entry(document: &Value, name: &str) -> Result<bool, (u16, String)> {
    let object = document
        .as_object()
        .ok_or_else(|| (400, "MCP config must be an object".to_string()))?;
    let in_servers = match object.get("servers") {
        Some(servers) => servers
            .as_array()
            .ok_or_else(|| (400, "MCP config servers must be an array".to_string()))?
            .iter()
            .any(|server| server.get("name").and_then(Value::as_str) == Some(name)),
        None => false,
    };
    let in_server_map = match object.get("mcpServers") {
        Some(servers) => servers
            .as_object()
            .ok_or_else(|| (400, "MCP config mcpServers must be an object".to_string()))?
            .contains_key(name),
        None => false,
    };
    Ok(in_servers || in_server_map)
}

async fn write_mcp_config_document(path: &Path, document: &Value) -> Result<(), (u16, String)> {
    let parent = path
        .parent()
        .ok_or_else(|| (500, format!("invalid MCP config path: {}", path.display())))?;
    tokio::fs::create_dir_all(parent).await.map_err(|error| {
        (
            500,
            format!("failed to create {}: {error}", parent.display()),
        )
    })?;
    let bytes = serde_json::to_vec_pretty(document)
        .map_err(|error| (500, format!("failed to serialize MCP config: {error}")))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mcp.json");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", now_millis()));
    tokio::fs::write(&temporary, bytes).await.map_err(|error| {
        (
            500,
            format!("failed to write {}: {error}", temporary.display()),
        )
    })?;
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err((
            500,
            format!("failed to replace {}: {error}", path.display()),
        ));
    }
    Ok(())
}

fn automation_path(path: &str) -> Option<(&str, Option<&str>)> {
    let tail = path.strip_prefix("/api/automations/")?;
    if let Some(id) = tail.strip_suffix("/run") {
        return (!id.is_empty() && !id.contains('/')).then_some((id, Some("run")));
    }
    (!tail.is_empty() && !tail.contains('/')).then_some((tail, None))
}

fn automation_runs_path(path: &str) -> Option<&str> {
    let tail = path.strip_prefix("/api/automations/")?;
    let id = tail.strip_suffix("/runs")?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

fn automation_id_path(path: &str) -> Option<&str> {
    let tail = path.strip_prefix("/api/automations/")?;
    (!tail.is_empty() && !tail.contains('/')).then_some(tail)
}

fn merge_object(mut primary: Value, fallback: Value) -> Value {
    let primary = primary.as_object_mut().cloned().unwrap_or_default();
    let mut fallback = fallback.as_object().cloned().unwrap_or_default();
    fallback.extend(primary);
    Value::Object(fallback)
}

fn is_generic_get(path: &str) -> bool {
    matches!(
        path,
        "/api/branch"
            | "/api/composer"
            | "/api/cost"
            | "/api/diagnostics"
            | "/api/memory"
            | "/api/mode"
            | "/api/ollama"
            | "/api/package"
            | "/api/preview"
            | "/api/queue"
            | "/api/quota"
            | "/api/ui"
            | "/api/zen"
    )
}

fn is_generic_post(path: &str) -> bool {
    matches!(
        path,
        "/api/branch"
            | "/api/chat/approval"
            | "/api/chat/client-tool-result"
            | "/api/chat/tool-retry"
            | "/api/run/event"
            | "/api/composer"
            | "/api/cost"
            | "/api/memory"
            | "/api/mode"
            | "/api/ollama"
            | "/api/package"
            | "/api/policy/validate"
            | "/api/prompt-suggestion"
            | "/api/queue"
            | "/api/quota"
            | "/api/ui"
            | "/api/zen"
            | "/api/admin/cleanup"
            | "/api/admin/warm-caches"
    )
}

fn generic_get(path: &str, head: &RequestHead, api: &ExtendedApiState) -> Vec<u8> {
    let resource = path.trim_start_matches("/api/");
    let action = head.query.get("action").cloned();
    let stored = api.values.get(path).cloned();
    json_response(
        200,
        &serde_json::json!({
            "resource": resource,
            "action": action,
            "runtime": "rust-control-plane",
            "items": [],
            "status": "ready",
            "value": stored,
        }),
    )
}

#[cfg(test)]
mod mcp_mutation_tests {
    use super::*;

    async fn test_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "maestro-mcp-mutation-{}-{}",
            std::process::id(),
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::create_dir_all(&root).await.expect("temp root");
        root
    }

    #[tokio::test]
    async fn add_update_and_remove_mcp_server_persists_contract_shape() {
        let root = test_root().await;
        let added = mutate_mcp_config(
            &root,
            "add-server",
            &serde_json::json!({
                "scope": "local",
                "server": { "name": "docs", "transport": "http", "url": "https://example.com/mcp" }
            }),
        )
        .await
        .expect("add server");
        assert_eq!(added["name"], "docs");
        assert_eq!(added["scope"], "local");
        assert_eq!(added["server"]["transport"], "http");

        let updated = mutate_mcp_config(
            &root,
            "update-server",
            &serde_json::json!({
                "name": "docs",
                "scope": "local",
                "server": { "name": "docs", "transport": "sse", "url": "https://example.com/events" }
            }),
        )
        .await
        .expect("update server");
        assert_eq!(updated["server"]["transport"], "sse");

        let removed = mutate_mcp_config(
            &root,
            "remove-server",
            &serde_json::json!({ "name": "docs", "scope": "local" }),
        )
        .await
        .expect("remove server");
        assert_eq!(removed["name"], "docs");
        assert!(removed["fallback"].is_null());

        let path = root.join(".composer/mcp.local.json");
        let persisted: Value =
            serde_json::from_slice(&tokio::fs::read(path).await.expect("persisted MCP config"))
                .expect("valid persisted MCP JSON");
        assert_eq!(persisted["servers"], serde_json::json!([]));
        tokio::fs::remove_dir_all(root)
            .await
            .expect("remove temp root");
    }

    #[tokio::test]
    async fn mcp_mutation_rejects_invalid_or_duplicate_servers() {
        let root = test_root().await;
        let request = serde_json::json!({
            "server": { "name": "docs", "transport": "http", "url": "https://example.com/mcp" }
        });
        mutate_mcp_config(&root, "add-server", &request)
            .await
            .expect("first add");
        assert_eq!(
            mutate_mcp_config(&root, "add-server", &request)
                .await
                .expect_err("duplicate add")
                .0,
            409
        );
        assert_eq!(
            mutate_mcp_config(
                &root,
                "add-server",
                &serde_json::json!({ "server": { "name": "bad", "transport": "http" } }),
            )
            .await
            .expect_err("invalid server")
            .0,
            400
        );
        tokio::fs::remove_dir_all(root)
            .await
            .expect("remove temp root");
    }

    #[test]
    fn automation_paths_keep_definition_mutations_and_run_distinct() {
        assert_eq!(
            automation_path("/api/automations/nightly"),
            Some(("nightly", None))
        );
        assert_eq!(
            automation_path("/api/automations/nightly/run"),
            Some(("nightly", Some("run")))
        );
        assert_eq!(automation_path("/api/automations/nightly/runs"), None);
        assert_eq!(automation_path("/api/automations/a/b"), None);
    }
}
