//! Explicit owner-authorized watch controls also serve native Codex transports,
//! which do not register the embedded runner's gateway-handled tool schemas.
use super::*;

pub(crate) async fn handle(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
    auth: &AuthContext,
    session: &str,
) -> Vec<u8> {
    let Some(mut scope) = crate::pull_request_watch::capture_action_scope(
        state,
        auth,
        Some(session),
        "list_pull_request_watches",
        &Value::Null,
    )
    .await
    else {
        return json_response(404, &serde_json::json!({"error":"Session not found"}));
    };
    let (tool, args) = match head.method.as_str() {
        "GET" => ("list_pull_request_watches", serde_json::json!({})),
        "POST" | "DELETE" => {
            if let Err(response) = validate_csrf(head, &state.config) {
                return response;
            }
            let body = match crate::http::read_request_body_with_limit(stream, initial, head, 4096)
                .await
            {
                Ok(body) => body,
                Err(error) => return json_response(400, &serde_json::json!({"error":error})),
            };
            let args: Value = match serde_json::from_slice(&body) {
                Ok(Value::Object(args)) if args.len() == 1 && args.contains_key("url") => {
                    Value::Object(args)
                }
                _ => {
                    return json_response(
                        400,
                        &serde_json::json!({"error":"Expected an object containing only url"}),
                    );
                }
            };
            (
                if head.method == "POST" {
                    "watch_pull_request"
                } else {
                    "stop_pull_request_watch"
                },
                args,
            )
        }
        _ => return json_response(405, &serde_json::json!({"error":"Method not allowed"})),
    };
    scope.capture_stop_target(state, tool, &args).await;
    let result = crate::pull_request_watch::handle_scoped_tool(
        state,
        auth,
        Some(session),
        tool,
        &args,
        scope.clone(),
    )
    .await;
    if !result.success {
        return json_response(
            400,
            &serde_json::json!({"error":result.error.unwrap_or(result.output)}),
        );
    }
    let projection = if head.method == "GET" {
        result
    } else {
        crate::pull_request_watch::handle_scoped_tool(
            state,
            auth,
            Some(session),
            "list_pull_request_watches",
            &serde_json::json!({}),
            scope,
        )
        .await
    };
    if !projection.success {
        return json_response(404, &serde_json::json!({"error":"Session not found"}));
    }
    match serde_json::from_str::<Value>(&projection.output) {
        Ok(value) => json_response(200, &value),
        Err(_) => json_response(
            500,
            &serde_json::json!({"error":"Invalid watch projection"}),
        ),
    }
}
