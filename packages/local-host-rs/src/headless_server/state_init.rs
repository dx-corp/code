//! Initial state for the selected headless execution host.

use super::*;

impl HeadlessState {
    pub(super) fn new(
        tool_host: Arc<dyn crate::agent::tool_host::ToolHost>,
        model_override: Option<String>,
    ) -> Self {
        let model = resolve_headless_model(model_override, &std::env::vars().collect());
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let system_prompt = format!(
            "You are Deixic Code, an AI coding assistant. Working directory: {cwd}. Be concise and use tools when helpful."
        );
        let session_id = env_session_id();
        Self {
            tool_host,
            model_capabilities: crate::agent::catalog_model_capabilities(&model),
            model,
            cwd,
            system_prompt: system_prompt.clone(),
            thinking_enabled: false,
            thinking_budget: 10_000,
            credential_vault: CredentialVault::new(),
            history: None,
            init_applied: false,
            governed_grant: None,
            controller_binding_sha256: None,
            controller_binding: None,
            workspace_capabilities:
                crate::headless::workspace_capabilities::WorkspaceCapabilityActivation::new(
                    system_prompt.clone(),
                ),
            next_prompt_queue_id: 1,
            ready_emitted: false,
            meta: Arc::new(Mutex::new(RuntimeMeta {
                session_id,
                approval_mode: None,
                tool_execution_ids: HashMap::new(),
                decided_tool_execution_ids: HashSet::new(),
                pending_tool_calls: HashSet::new(),
                client_tool_bindings: HashMap::new(),
                pending_client_tools: HashMap::new(),
                emitted_client_tool_terminals: HashSet::new(),
                queued_workspace_receipts: HashMap::new(),
                conversation_snapshot: None,
                process_budget: None,
                turn_active: false,
                transcript_grade: crate::transcript::TranscriptGrade::Delta,
                response_chunks: Vec::new(),
                semantic_text: SemanticTextRelease::default(),
                semantic_response_id: None,
                semantic_started_at: None,
                semantic_raw_first_ms: None,
                semantic_first_released_ms: None,
                semantic_published_chunks: 0,
                semantic_standalone_releases: 0,
                semantic_forced_flushes: 0,
                semantic_peak_held_bytes: 0,
                semantic_total_hold_ms: 0,
                managed_gateway_receipt: None,
                prompt_experiment: None,
                receipt_tasks: Arc::new(Mutex::new(Vec::new())),
            })),
            agent: None,
            tool_tx: None,
            event_task: None,
            utility_commands: HashMap::new(),
            file_watches: HashMap::new(),
        }
    }
}
