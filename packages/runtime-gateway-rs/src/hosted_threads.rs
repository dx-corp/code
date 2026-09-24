//! Tenant-bound desktop access to Platform-owned operating threads.

use super::*;
use maestro_local_host::credential_mode::{
    PlatformSession, verified_current_identity_session, verified_desktop_identity_session,
};
use maestro_local_host::hosted_thread::ThreadClient;
use serde::de::DeserializeOwned;

const ROOT: &str = "/api/hosted-threads";
const PREFIX: &str = "/api/hosted-threads/";
const MAX_HOSTED_BODY_BYTES: usize = 70 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum Operation {
    List,
    Rename,
    Archive,
    Snapshot,
    Events,
    Message,
    Response,
}

#[derive(Deserialize)]
struct MessageBody {
    body: String,
}

#[derive(Deserialize)]
struct RenameBody {
    title: String,
}

#[derive(Deserialize)]
struct ArchiveBody {
    archived: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseBody {
    cursor: String,
    request_id: String,
    action: String,
    #[serde(default)]
    text: String,
}

pub(crate) fn is_hosted_thread_endpoint(head: &RequestHead) -> bool {
    head.path == ROOT || head.path.starts_with(PREFIX)
}

fn valid_thread_id(id: &str) -> bool {
    let Some(suffix) = id.strip_prefix("thread:") else {
        return false;
    };
    !suffix.is_empty()
        && suffix.len() <= 249
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn parse_route(path: &str) -> Option<(String, Operation)> {
    let suffix = path.strip_prefix(PREFIX)?;
    let (encoded_id, operation) = match suffix.split_once('/') {
        Some((id, "rename")) => (id, Operation::Rename),
        Some((id, "archive")) => (id, Operation::Archive),
        Some((id, "events")) => (id, Operation::Events),
        Some((id, "messages")) => (id, Operation::Message),
        Some((id, "responses")) => (id, Operation::Response),
        Some(_) => return None,
        None => (suffix, Operation::Snapshot),
    };
    let raw_id = percent_decode_component(encoded_id);
    let id = raw_id.strip_prefix("thread:").unwrap_or(&raw_id);
    let channel_id = format!("thread:{id}");
    if !valid_thread_id(&channel_id) {
        return None;
    }
    Some((channel_id, operation))
}

fn private_json(status: u16, value: &impl Serialize) -> Vec<u8> {
    let body = serde_json::to_vec(value).expect("hosted thread response is JSON serializable");
    response_with_no_store(status, "application/json", &body)
}

fn error(status: u16, message: &str) -> Vec<u8> {
    private_json(status, &serde_json::json!({ "error": message }))
}

async fn parse_body<T: DeserializeOwned>(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
) -> Result<T, Vec<u8>> {
    let body = read_request_body_with_limit(stream, initial, head, MAX_HOSTED_BODY_BYTES)
        .await
        .map_err(|_| error(400, "invalid request body"))?;
    serde_json::from_slice(&body).map_err(|_| error(400, "invalid JSON request"))
}

fn session_matches_auth(auth: &AuthContext, session: &PlatformSession) -> bool {
    let subject_matches = auth
        .subject
        .as_deref()
        .is_none_or(|subject| session.user_id.as_deref() == Some(subject));
    let org_matches = auth
        .organization_id
        .as_deref()
        .is_none_or(|org| org == session.organization_id);
    let workspace_matches = auth
        .workspace_id
        .as_deref()
        .is_none_or(|workspace| session.workspace_id.as_deref() == Some(workspace));
    subject_matches && org_matches && workspace_matches
}

fn desktop_credential(
    head: &RequestHead,
    auth: &AuthContext,
    loopback: bool,
) -> Result<Option<(String, String, String)>, Vec<u8>> {
    let token = head.headers.get("x-maestro-identity-token");
    let org = head.headers.get("x-maestro-identity-organization");
    let workspace = head.headers.get("x-maestro-identity-workspace");
    if token.is_none() && org.is_none() && workspace.is_none() {
        return Ok(None);
    }
    if !loopback || auth.source != AuthSource::StaticGatewayKey {
        return Err(error(
            403,
            "desktop credential requires local gateway authority",
        ));
    }
    match (token, org, workspace) {
        (Some(token), Some(org), Some(workspace))
            if !token.trim().is_empty()
                && !org.trim().is_empty()
                && !workspace.trim().is_empty() =>
        {
            Ok(Some((token.clone(), org.clone(), workspace.clone())))
        }
        _ => Err(error(401, "desktop credential is incomplete")),
    }
}

fn response_action(action: &str, text: &str) -> Option<i32> {
    if text.len() > 65_536 {
        return None;
    }
    match action {
        "approve" => Some(1),
        "deny" => Some(2),
        "answer" if !text.trim().is_empty() => Some(3),
        "retry" => Some(4),
        "skip" => Some(5),
        "abort" => Some(6),
        _ => None,
    }
}

pub(crate) async fn handle_hosted_thread_endpoint(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: RequestHead,
    state: &AppState,
) -> Vec<u8> {
    if let Err(response) = validate_csrf(&head, &state.config) {
        return response;
    }
    let auth = match authorized_context(&head, &state.config) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let route = if head.path == ROOT {
        Some((String::new(), Operation::List))
    } else {
        parse_route(&head.path)
    };
    let Some((channel_id, operation)) = route else {
        return error(404, "hosted thread route not found");
    };
    let valid_method = matches!(
        (&operation, head.method.as_str()),
        (
            Operation::List | Operation::Snapshot | Operation::Events,
            "GET"
        ) | (
            Operation::Rename | Operation::Archive | Operation::Message | Operation::Response,
            "POST"
        )
    );
    if !valid_method {
        return error(405, "method not allowed");
    }

    // Electron main can supply its live keychain token only over the private
    // loopback gateway key. Otherwise, use the independent CLI credential.
    let desktop = match desktop_credential(&head, &auth, state.config.listen_host_is_loopback()) {
        Ok(credential) => credential,
        Err(response) => return response,
    };
    let session = match tokio::task::spawn_blocking(move || match desktop {
        Some((token, org, workspace)) => {
            verified_desktop_identity_session(&token, &org, &workspace)
        }
        None => verified_current_identity_session(),
    })
    .await
    {
        Ok(Ok(session)) => session,
        _ => return error(401, "managed sign-in required"),
    };
    if !session_matches_auth(&auth, &session) {
        return error(403, "caller tenant differs from managed sign-in");
    }
    let Some(base_url) = maestro_local_host::managed_setup::platform_base_url() else {
        return error(503, "Platform URL unavailable");
    };
    let client = match ThreadClient::new(session, channel_id, &base_url) {
        Ok(client) => client,
        Err(_) => return error(503, "Platform configuration unavailable"),
    };

    match operation {
        Operation::List => {
            let archived = match head.query.get("archived").map(String::as_str) {
                None | Some("false") => false,
                Some("true") => true,
                _ => return error(400, "invalid archive filter"),
            };
            match client.list_channels(archived).await {
                Ok(result) => {
                    let threads: Vec<_> = result
                        .channels
                        .iter()
                        .filter(|channel| {
                            channel.archived == archived && valid_thread_id(&channel.id)
                        })
                        .collect();
                    private_json(
                        200,
                        &serde_json::json!({
                            "threads": threads.iter().take(200).map(|channel| serde_json::json!({
                                "id": channel.id,
                                "label": channel.label,
                                "unreadCount": channel.unread_count,
                                "openCount": channel.open_count,
                            })).collect::<Vec<_>>(),
                            "truncated": threads.len() > 200,
                        }),
                    )
                }
                Err(_) => error(503, "Platform threads unavailable"),
            }
        }
        Operation::Rename => {
            let request: RenameBody = match parse_body(stream, initial, &head).await {
                Ok(request) => request,
                Err(response) => return response,
            };
            let title = request.title.trim();
            if title.is_empty() || title.chars().count() > 80 {
                return error(400, "title must contain 1 to 80 characters");
            }
            match client.rename(title.to_owned()).await {
                Ok(result) => {
                    let channel = result.channel.expect("rename validates channel identity");
                    private_json(
                        200,
                        &serde_json::json!({
                            "id": channel.id,
                            "label": channel.label,
                            "changed": result.changed,
                        }),
                    )
                }
                Err(_) => error(503, "Platform thread rename was not accepted"),
            }
        }
        Operation::Archive => {
            let request: ArchiveBody = match parse_body(stream, initial, &head).await {
                Ok(request) => request,
                Err(response) => return response,
            };
            match client.archive(request.archived).await {
                Ok(result) => {
                    let channel = result.channel.expect("archive validates channel identity");
                    private_json(
                        200,
                        &serde_json::json!({
                            "id": channel.id,
                            "archived": channel.archived,
                            "changed": result.changed,
                        }),
                    )
                }
                Err(_) => error(503, "Platform thread archive was not accepted"),
            }
        }
        Operation::Snapshot => match client.get().await {
            Ok(snapshot) => {
                let archived = snapshot
                    .channel
                    .as_ref()
                    .is_some_and(|channel| channel.archived);
                private_json(
                    200,
                    &serde_json::json!({
                        "channelId": snapshot.channel.map(|channel| channel.id),
                        "archived": archived,
                        "messages": snapshot.messages.iter().map(|message| serde_json::json!({
                            "id": message.id, "role": message.role, "body": message.body
                        })).collect::<Vec<_>>(),
                        "replayCursor": snapshot.replay_cursor.to_string()
                    }),
                )
            }
            Err(_) => error(503, "Platform thread unavailable"),
        },
        Operation::Events => {
            let cursor = match head.query.get("cursor").map(String::as_str) {
                None => 0,
                Some(value) => match value.parse::<i64>() {
                    Ok(cursor) if cursor >= 0 => cursor,
                    _ => return error(400, "invalid event cursor"),
                },
            };
            match client.events(cursor).await {
                Ok(page) if page.next_cursor >= cursor || page.reset_required => private_json(
                    200,
                    &serde_json::json!({
                        "events": page.events.iter().map(|event| serde_json::json!({
                            "cursor": event.cursor.to_string(),
                            "eventId": event.event_id,
                            "turnId": event.turn_id,
                            "kind": event.kind,
                            "safeText": event.safe_text,
                            "requestId": event.request_id,
                        })).collect::<Vec<_>>(),
                        "nextCursor": page.next_cursor.to_string(),
                        "hasMore": page.has_more,
                        "resetRequired": page.reset_required,
                    }),
                ),
                Err(_) => error(503, "Platform events unavailable"),
                Ok(_) => error(503, "Platform event cursor moved backwards"),
            }
        }
        Operation::Message => {
            let request: MessageBody = match parse_body(stream, initial, &head).await {
                Ok(request) => request,
                Err(response) => return response,
            };
            match client.submit(request.body).await {
                Ok(result) => private_json(
                    202,
                    &serde_json::json!({
                        "turnId": result.accepted_turn.map(|turn| turn.turn_id),
                        "replayCursor": result.replay_cursor.to_string(),
                    }),
                ),
                Err(_) => error(503, "Platform message was not accepted"),
            }
        }
        Operation::Response => {
            let request: ResponseBody = match parse_body(stream, initial, &head).await {
                Ok(request) => request,
                Err(response) => return response,
            };
            let Some(action) = response_action(&request.action, &request.text) else {
                return error(400, "invalid response action");
            };
            let cursor = match request.cursor.parse::<i64>() {
                Ok(cursor) if cursor > 0 => cursor,
                _ => return error(400, "invalid request cursor"),
            };
            if request.request_id.is_empty() {
                return error(400, "request cursor and id required");
            }
            // Resolve identity from Platform immediately before responding.
            // A renderer may name a request, but cannot supply its type, call id,
            // or turn id to the owner mutation.
            let page = match client.events(cursor - 1).await {
                Ok(page) if !page.reset_required => page,
                _ => return error(409, "request must be refreshed"),
            };
            let Some(event) = page.events.iter().find(|event| {
                event.cursor == cursor
                    && event.request_id == request.request_id
                    && matches!(event.kind, 4 | 5 | 14 | 15)
            }) else {
                return error(409, "request must be refreshed");
            };
            match client.respond(event, action, request.text).await {
                Ok(result) => private_json(
                    200,
                    &serde_json::json!({ "replayCursor": result.replay_cursor.to_string() }),
                ),
                Err(_) => error(503, "Platform response was not accepted"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_encoded_path_escape_and_unrecognized_actions() {
        assert!(parse_route("/api/hosted-threads/thread%3Aone%2Ftwo").is_none());
        assert!(parse_route("/api/hosted-threads/thread%3Aone/events/extra").is_none());
        assert_eq!(response_action("answer", " "), None);
        assert_eq!(response_action("approve", ""), Some(1));
    }

    #[test]
    fn routes_are_tenant_scoped_thread_ids() {
        assert_eq!(
            parse_route("/api/hosted-threads/thread%3Aabc_1/events"),
            Some(("thread:abc_1".to_string(), Operation::Events))
        );
        assert_eq!(
            parse_route("/api/hosted-threads/thread%3Aabc_1/rename"),
            Some(("thread:abc_1".to_string(), Operation::Rename))
        );
        assert_eq!(
            parse_route("/api/hosted-threads/thread%3Aabc_1/archive"),
            Some(("thread:abc_1".to_string(), Operation::Archive))
        );
        assert!(valid_thread_id("thread:abc_1"));
        assert!(!valid_thread_id("thread:../escape"));
        assert!(!valid_thread_id("other:abc"));
        assert!(is_hosted_thread_endpoint(&RequestHead {
            method: "GET".into(),
            path: ROOT.into(),
            query: Default::default(),
            headers: Default::default(),
        }));
    }

    #[test]
    fn hosted_identity_cannot_cross_caller_subject_or_tenant() {
        let session = PlatformSession {
            access_token: "secret".into(),
            organization_id: "org-a".into(),
            workspace_id: Some("ws-a".into()),
            provider_ref: Value::Null,
            email: None,
            user_id: Some("user-a".into()),
        };
        let mut auth = AuthContext {
            subject: Some("user-a".into()),
            organization_id: Some("org-a".into()),
            workspace_id: Some("ws-a".into()),
            ..AuthContext::default()
        };
        assert!(session_matches_auth(&auth, &session));
        auth.subject = Some("user-b".into());
        assert!(!session_matches_auth(&auth, &session));
        auth.subject = Some("user-a".into());
        auth.workspace_id = Some("ws-b".into());
        assert!(!session_matches_auth(&auth, &session));
    }

    #[test]
    fn desktop_credential_requires_private_loopback_key_and_all_fields() {
        let mut head = RequestHead {
            method: "GET".into(),
            path: "/api/hosted-threads/thread%3Aone".into(),
            query: Default::default(),
            headers: Default::default(),
        };
        head.headers
            .insert("x-maestro-identity-token".into(), "token".into());
        head.headers
            .insert("x-maestro-identity-organization".into(), "org".into());
        head.headers
            .insert("x-maestro-identity-workspace".into(), "workspace".into());
        let static_key = AuthContext {
            source: AuthSource::StaticGatewayKey,
            ..AuthContext::default()
        };
        assert!(
            desktop_credential(&head, &static_key, true)
                .unwrap()
                .is_some()
        );
        assert!(desktop_credential(&head, &static_key, false).is_err());
        let jwt = AuthContext {
            source: AuthSource::IdentityJwt,
            ..AuthContext::default()
        };
        assert!(desktop_credential(&head, &jwt, true).is_err());
        head.headers.remove("x-maestro-identity-workspace");
        assert!(desktop_credential(&head, &static_key, true).is_err());
    }
}
