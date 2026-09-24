//! Attach the native terminal to a Platform-owned Dex operating thread.
//!
//! The shared native client uses the generated public application protocol.
//! Platform owns execution and approvals.

use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
#[cfg(test)]
use maestro_local_host::credential_mode::PlatformSession;
use maestro_local_host::credential_mode::verified_current_identity_session;
use maestro_local_host::hosted_thread::{Event, ThreadClient};
#[cfg(test)]
use maestro_local_host::public_protocol as wire;
#[cfg(test)]
use prost::Message;
use serde::Serialize;
use tokio::time::sleep;

const FOLLOW_LIMIT: Duration = Duration::from_mins(2);
const USAGE: &str = "Usage: deixic-code thread attach <thread-id> [--message <text>] [--json] [--base-url <url>]\n\
The thread id is a Deixic thread id (thread:...). A managed Deixic login is required.\n\
Interactive commands: /quit, /respond <request-id> <approve|deny|answer|retry|skip|abort> [text].";

#[derive(Debug)]
struct Options {
    channel_id: String,
    message: Option<String>,
    json: bool,
    base_url: Option<String>,
}

fn parse(args: &[String]) -> Result<Option<Options>> {
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return Ok(None);
    }
    if args.first().map(String::as_str) != Some("attach") {
        bail!("unknown thread command; {USAGE}");
    }
    let mut id = None;
    let mut message = None;
    let mut json = false;
    let mut base_url = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--message" | "--base-url" => {
                let value = args.get(i + 1).context("option value required")?.clone();
                if args[i] == "--message" {
                    message = Some(value);
                } else {
                    base_url = Some(value);
                }
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            value if value.starts_with('-') => bail!("unknown option: {value}"),
            value if id.is_none() => {
                id = Some(value.to_owned());
                i += 1;
            }
            _ => bail!("only one thread id is allowed"),
        }
    }
    let id: String = id.context("thread id required")?;
    let channel_id = if id.starts_with("thread:") {
        id
    } else {
        format!("thread:{id}")
    };
    if channel_id.len() > 256
        || channel_id == "thread:"
        || channel_id.chars().any(char::is_whitespace)
    {
        bail!("invalid Deixic thread id");
    }
    if message
        .as_ref()
        .is_some_and(|body| body.trim().is_empty() || body.len() > 20_000)
    {
        bail!("message must contain 1 to 20000 bytes");
    }
    if json && message.is_none() {
        bail!("--json requires --message");
    }
    Ok(Some(Options {
        channel_id,
        message,
        json,
        base_url,
    }))
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Output<'a> {
    Message {
        role: &'a str,
        body: &'a str,
    },
    Event {
        cursor: i64,
        turn_id: &'a str,
        kind: i32,
        text: &'a str,
        request_id: &'a str,
    },
    Accepted {
        turn_id: &'a str,
    },
    Waiting {
        turn_id: &'a str,
    },
}

fn emit(output: Output<'_>, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        match output {
            Output::Message { role, body } => println!("{role}: {body}"),
            Output::Event {
                kind,
                text,
                request_id,
                ..
            } => {
                if !text.is_empty() {
                    println!("{text}");
                }
                if !request_id.is_empty() {
                    println!(
                        "Request {request_id} (kind {kind}); use /respond after reviewing it."
                    );
                }
            }
            Output::Accepted { turn_id } => println!("Accepted turn {turn_id}"),
            Output::Waiting { turn_id } => {
                println!("Turn {turn_id} is still running. Reattach to continue watching.");
            }
        }
    }
    Ok(())
}

async fn follow(
    client: &ThreadClient,
    mut cursor: i64,
    turn_id: &str,
    json: bool,
    pending: &mut HashMap<String, Event>,
    known_messages: &mut HashSet<String>,
) -> Result<i64> {
    let deadline = Instant::now() + FOLLOW_LIMIT;
    loop {
        let page = client.events(cursor).await?;
        if page.reset_required {
            let snapshot = client.get().await?;
            cursor = snapshot.replay_cursor;
            for message in &snapshot.messages {
                if known_messages.insert(message.id.clone()) {
                    emit(
                        Output::Message {
                            role: &message.role,
                            body: &message.body,
                        },
                        json,
                    )?;
                }
            }
        } else {
            if page.next_cursor < cursor {
                bail!("Platform event cursor moved backwards");
            }
            let before = cursor;
            for event in &page.events {
                if event.cursor <= cursor {
                    continue;
                }
                cursor = event.cursor;
                if !event.request_id.is_empty() && matches!(event.kind, 4 | 5 | 10 | 11) {
                    pending.insert(event.request_id.clone(), event.clone());
                }
                emit(
                    Output::Event {
                        cursor: event.cursor,
                        turn_id: &event.turn_id,
                        kind: event.kind,
                        text: &event.safe_text,
                        request_id: &event.request_id,
                    },
                    json,
                )?;
                if event.turn_id == turn_id && matches!(event.kind, 7..=9) {
                    pending.retain(|_, request| request.turn_id != turn_id);
                    let snapshot = client.get().await?;
                    for message in &snapshot.messages {
                        if known_messages.insert(message.id.clone()) {
                            emit(
                                Output::Message {
                                    role: &message.role,
                                    body: &message.body,
                                },
                                json,
                            )?;
                        }
                    }
                    return Ok(cursor);
                }
                if event.turn_id == turn_id && matches!(event.kind, 4 | 5 | 10 | 11) {
                    return Ok(cursor);
                }
            }
            cursor = cursor.max(page.next_cursor);
            if page.has_more {
                if cursor == before {
                    bail!("Platform event page made no progress");
                }
                continue;
            }
        }
        if Instant::now() >= deadline {
            emit(Output::Waiting { turn_id }, json)?;
            return Ok(cursor);
        }
        sleep(Duration::from_secs(1)).await;
    }
}

fn parse_response(line: &str) -> Result<(&str, i32, String)> {
    let mut parts = line.splitn(4, ' ');
    if parts.next() != Some("/respond") {
        bail!("not a response command");
    }
    let request_id = parts.next().context("request id required")?;
    let action = match parts.next().context("action required")? {
        "approve" => 1,
        "deny" => 2,
        "answer" => 3,
        "retry" => 4,
        "skip" => 5,
        "abort" => 6,
        other => bail!("unknown action: {other}"),
    };
    let text = parts.next().unwrap_or("").to_owned();
    if action == 3 && text.trim().is_empty() {
        bail!("answer text required");
    }
    if text.len() > 65_536 {
        bail!("response text exceeds 65536 bytes");
    }
    Ok((request_id, action, text))
}

pub async fn run_thread(args: &[String]) -> Result<i32> {
    let Some(options) = parse(args)? else {
        println!("{USAGE}");
        return Ok(0);
    };
    maestro_local_host::safety::require_vendor_network()?;
    let session = verified_current_identity_session()?;
    let base_url = options
        .base_url
        .or_else(maestro_local_host::managed_setup::platform_base_url)
        .context("Platform URL unavailable")?;
    let client = ThreadClient::new(session, options.channel_id, &base_url)?;
    let snapshot = client.get().await?;
    let mut known_messages = HashSet::new();
    for message in &snapshot.messages {
        known_messages.insert(message.id.clone());
        emit(
            Output::Message {
                role: &message.role,
                body: &message.body,
            },
            options.json,
        )?;
    }
    let mut pending = HashMap::new();
    if let Some(body) = options.message {
        let submitted = client.submit(body).await?;
        let turn = submitted.accepted_turn.context("accepted turn missing")?;
        emit(
            Output::Accepted {
                turn_id: &turn.turn_id,
            },
            options.json,
        )?;
        follow(
            &client,
            snapshot.replay_cursor,
            &turn.turn_id,
            options.json,
            &mut pending,
            &mut known_messages,
        )
        .await?;
        return Ok(0);
    }
    if !io::stdin().is_terminal() || options.json {
        bail!("interactive attach requires a terminal; pass --message for one-shot use");
    }
    // Recover recent owner-issued requests when attaching to a turn that was
    // already waiting before this terminal opened. Platform still validates
    // request identity and current generation when a response is submitted.
    let mut history_cursor = snapshot.replay_cursor.saturating_sub(200);
    for _ in 0..10 {
        let page = client.events(history_cursor).await?;
        if page.reset_required {
            break;
        }
        for event in page.events {
            if !event.request_id.is_empty() && matches!(event.kind, 4 | 5 | 10 | 11) {
                pending.insert(event.request_id.clone(), event.clone());
            }
            if matches!(event.kind, 7..=9) {
                pending.retain(|_, request| request.turn_id != event.turn_id);
            }
        }
        if !page.has_more || page.next_cursor <= history_cursor {
            break;
        }
        history_cursor = page.next_cursor;
    }
    for event in pending.values() {
        println!(
            "Waiting request {} for turn {}: {}",
            event.request_id, event.turn_id, event.safe_text
        );
    }
    let mut cursor = snapshot.replay_cursor;
    loop {
        print!("deixic> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "/quit" | "/exit") {
            break;
        }
        if line.starts_with("/respond ") {
            let (request_id, action, text) = parse_response(line)?;
            let event = pending
                .get(request_id)
                .context("unknown pending request id")?;
            let turn_id = event.turn_id.clone();
            let response = client.respond(event, action, text).await?;
            pending.remove(request_id);
            cursor = follow(
                &client,
                cursor.min(response.replay_cursor),
                &turn_id,
                false,
                &mut pending,
                &mut known_messages,
            )
            .await?;
            continue;
        }
        if line.starts_with('/') {
            eprintln!("{USAGE}");
            continue;
        }
        let submitted = client.submit(line.to_owned()).await?;
        let turn = submitted.accepted_turn.context("accepted turn missing")?;
        emit(
            Output::Accepted {
                turn_id: &turn.turn_id,
            },
            false,
        )?;
        cursor = follow(
            &client,
            cursor,
            &turn.turn_id,
            false,
            &mut pending,
            &mut known_messages,
        )
        .await?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn parse_thread_attach() {
        let options = parse(&[
            "attach".into(),
            "abc".into(),
            "--message".into(),
            "hello".into(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(options.channel_id, "thread:abc");
        assert_eq!(options.message.as_deref(), Some("hello"));
        assert!(parse(&["attach".into(), "thread:".into()]).is_err());
        assert!(parse(&["attach".into(), "a".into(), "b".into()]).is_err());
    }

    #[test]
    fn response_requires_explicit_action() {
        assert_eq!(parse_response("/respond req-1 deny").unwrap().1, 2);
        assert!(parse_response("/respond req-1 answer").is_err());
        assert!(parse_response("/respond req-1 yes").is_err());
    }

    #[test]
    fn refuses_missing_tenant_and_plaintext_remote_origin() {
        let session = PlatformSession {
            access_token: "token".into(),
            organization_id: "org-a".into(),
            workspace_id: None,
            provider_ref: serde_json::Value::Null,
            email: None,
            user_id: Some("user-a".into()),
        };
        assert!(
            ThreadClient::new(session.clone(), "thread:one".into(), "http://example.com").is_err()
        );
        let client =
            ThreadClient::new(session, "thread:one".into(), "https://app.deixic.com").unwrap();
        assert!(client.query().is_err());
    }

    #[tokio::test]
    async fn public_calls_keep_tenant_and_request_identity() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for method in [
                "GetThread",
                "ListThreads",
                "RenameThread",
                "ArchiveThread",
                "SubmitTask",
                "ListEvents",
                "RespondToRequest",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let body_offset;
                let body_length;
                loop {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "request closed before body");
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        assert!(headers.contains(&format!(
                            "/deixicpublic.v1.deixicpublicservice/{}",
                            method.to_ascii_lowercase()
                        )));
                        assert!(headers.contains("authorization: bearer scoped-token"));
                        assert!(headers.contains("x-organization-id: org-a"));
                        assert!(headers.contains("x-workspace-id: ws-a"));
                        assert!(headers.contains("content-type: application/proto"));
                        body_length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap()
                            .parse::<usize>()
                            .unwrap();
                        body_offset = end + 4;
                        break;
                    }
                }
                while bytes.len() - body_offset < body_length {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let body = &bytes[body_offset..body_offset + body_length];
                let result = match method {
                    "ListThreads" => {
                        let request = wire::ListThreadsRequest::decode(body).unwrap();
                        assert_eq!(request.scope.unwrap().workspace_id, "ws-a");
                        assert!(!request.archived);
                        wire::ListThreadsResponse {
                            threads: vec![wire::Thread {
                                id: "thread:one".into(),
                                title: "Prior title".into(),
                                unread_count: 2,
                                ..Default::default()
                            }],
                        }
                        .encode_to_vec()
                    }
                    "RenameThread" => {
                        let request = wire::RenameThreadRequest::decode(body).unwrap();
                        assert_eq!(request.thread_id, "thread:one");
                        assert_eq!(request.title, "New title");
                        wire::RenameThreadResponse {
                            thread: Some(wire::Thread {
                                id: "thread:one".into(),
                                title: "New title".into(),
                                ..Default::default()
                            }),
                            changed: true,
                        }
                        .encode_to_vec()
                    }
                    "ArchiveThread" => {
                        let request = wire::ArchiveThreadRequest::decode(body).unwrap();
                        assert_eq!(request.thread_id, "thread:one");
                        assert!(request.archived);
                        assert!(!request.idempotency_key.is_empty());
                        wire::ArchiveThreadResponse {
                            thread: Some(wire::Thread {
                                id: "thread:one".into(),
                                archived: true,
                                ..Default::default()
                            }),
                            changed: true,
                            ..Default::default()
                        }
                        .encode_to_vec()
                    }
                    "GetThread" => {
                        let request = wire::GetThreadRequest::decode(body).unwrap();
                        assert_eq!(request.scope.unwrap().organization_id, "org-a");
                        assert_eq!(request.thread_id, "thread:one");
                        wire::GetThreadResponse {
                            thread: Some(wire::Thread {
                                id: "thread:one".into(),
                                ..Default::default()
                            }),
                            messages: vec![wire::TaskMessage {
                                id: "msg-1".into(),
                                role: wire::MessageRole::User as i32,
                                body: "prior".into(),
                                ..Default::default()
                            }],
                            replay_cursor: 9,
                            ..Default::default()
                        }
                        .encode_to_vec()
                    }
                    "SubmitTask" => {
                        let request = wire::SubmitTaskRequest::decode(body).unwrap();
                        assert_eq!(request.scope.unwrap().workspace_id, "ws-a");
                        assert_eq!(request.thread_id, "thread:one");
                        assert_eq!(request.body, "continue");
                        assert!(!request.idempotency_key.is_empty());
                        wire::SubmitTaskResponse {
                            accepted_turn: Some(wire::TaskTurn {
                                turn_id: "turn-2".into(),
                                ..Default::default()
                            }),
                            replay_cursor: 10,
                            ..Default::default()
                        }
                        .encode_to_vec()
                    }
                    "ListEvents" => {
                        let request = wire::ListEventsRequest::decode(body).unwrap();
                        assert_eq!(request.after_cursor, 9);
                        wire::ListEventsResponse {
                            events: vec![wire::TaskEvent {
                                cursor: 10,
                                id: "event-10".into(),
                                turn_id: "turn-2".into(),
                                kind: 4,
                                text: "Review the action".into(),
                                request_id: "request-1".into(),
                                request_kind: 1,
                                call_id: "call-1".into(),
                                ..Default::default()
                            }],
                            next_cursor: 10,
                            has_more: false,
                            reset_required: false,
                            ..Default::default()
                        }
                        .encode_to_vec()
                    }
                    "RespondToRequest" => {
                        let request = wire::RespondToRequestRequest::decode(body).unwrap();
                        assert_eq!(request.scope.unwrap().organization_id, "org-a");
                        assert_eq!(request.turn_id, "turn-2");
                        assert_eq!(request.request_id, "request-1");
                        assert_eq!(request.action, 2);
                        assert!(!request.idempotency_key.is_empty());
                        wire::RespondToRequestResponse {
                            replay_cursor: 11,
                            ..Default::default()
                        }
                        .encode_to_vec()
                    }
                    _ => unreachable!(),
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    result.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(&result).await.unwrap();
            }
        });

        let client = ThreadClient::new(
            PlatformSession {
                access_token: "scoped-token".into(),
                organization_id: "org-a".into(),
                workspace_id: Some("ws-a".into()),
                provider_ref: serde_json::Value::Null,
                email: None,
                user_id: Some("user-a".into()),
            },
            "thread:one".into(),
            &base,
        )
        .unwrap();
        let snapshot = client.get().await.unwrap();
        assert_eq!(snapshot.messages[0].body, "prior");
        let listed = client.list_channels(false).await.unwrap();
        assert_eq!(listed.channels[0].unread_count, 2);
        let renamed = client.rename("New title".into()).await.unwrap();
        assert_eq!(renamed.channel.unwrap().label, "New title");
        let archived = client.archive(true).await.unwrap();
        assert!(archived.channel.unwrap().archived);
        let submitted = client.submit("continue".into()).await.unwrap();
        assert_eq!(submitted.accepted_turn.unwrap().turn_id, "turn-2");
        let page = client.events(9).await.unwrap();
        assert_eq!(page.events[0].safe_text, "Review the action");
        assert_eq!(
            client
                .respond(&page.events[0], 2, String::new())
                .await
                .unwrap()
                .replay_cursor,
            11
        );
        server.await.unwrap();
    }
}
