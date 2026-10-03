use super::*;

pub(crate) fn validate_client_tool_names(chat: &ChatRequest) -> Result<(), String> {
    if let Some(tool) = chat.tools.iter().find(|tool| {
        is_session_messaging_tool(&tool.name.to_ascii_lowercase())
            || crate::pull_request_watch::is_tool(&tool.name.to_ascii_lowercase())
    }) {
        return Err(format!(
            "client tool name `{}` is reserved by the gateway",
            tool.name
        ));
    }
    Ok(())
}

pub(crate) fn client_tool_definitions(
    chat: &ChatRequest,
) -> (Vec<ToolDefinition>, HashSet<String>) {
    let names = chat
        .tools
        .iter()
        .map(|tool| tool.name.to_lowercase())
        .collect::<HashSet<_>>();
    let definitions = chat
        .tools
        .iter()
        .map(|tool| ToolDefinition {
            tool: Tool::new(&tool.name, &tool.description).with_schema(tool.parameters.clone()),
            requires_approval: true,
        })
        .collect();
    (definitions, names)
}

pub(crate) fn native_chat_terminal_status(event: &FromAgent) -> Option<Result<(), String>> {
    match event {
        FromAgent::TurnCompleted { .. } => Some(Ok(())),
        FromAgent::TurnInterrupted { reason, .. } => Some(Err(reason.clone())),
        FromAgent::ProviderError { kind, message } => {
            Some(Err(format!("provider failure ({kind:?}): {message}")))
        }
        _ => None,
    }
}

pub(crate) fn native_chat_acknowledges_peer_messages(event: &FromAgent) -> bool {
    matches!(native_chat_terminal_status(event), Some(Ok(())))
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientToolDefinition {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}
