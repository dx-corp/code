//! Attach the native terminal to a Platform-owned Dex operating thread.
//!
//! The wire projections below retain only the fields this client reads. Their
//! field numbers come from proto/console/v1/console.proto. Unknown fields stay
//! opaque; Platform remains the sole owner of execution and approval state.

use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use maestro_local_host::credential_mode::{PlatformSession, verified_current_identity_session};
use prost::Message;
use reqwest::{Client, Url};
use serde::Serialize;
use tokio::time::sleep;
use uuid::Uuid;

const SERVICE: &str = "/deixic.v1.DeixicService";
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const FOLLOW_LIMIT: Duration = Duration::from_secs(120);
const USAGE: &str = "Usage: deixic-code thread attach <thread-id> [--message <text>] [--json] [--base-url <url>]\n\
The thread id is a Deixic thread id (thread:...). A managed Deixic login is required.\n\
Interactive commands: /quit, /respond <request-id> <approve|deny|answer|retry|skip|abort> [text].";

// A narrow wire projection of console.v1. Field numbers and kinds mirror the
// canonical protobuf contract; unknown fields are ignored by prost.
#[derive(Clone, PartialEq, Message)]
struct Query {
    #[prost(string, tag = "1")]
    workspace_id: String,
    #[prost(string, tag = "13")]
    organization_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct GetRequest {
    #[prost(message, optional, tag = "1")]
    query: Option<Query>,
    #[prost(string, tag = "2")]
    channel_id: String,
    #[prost(int32, tag = "3")]
    limit: i32,
}

#[derive(Clone, PartialEq, Message)]
struct Channel {
    #[prost(string, tag = "1")]
    id: String,
}

#[derive(Clone, PartialEq, Message)]
struct OperatingMessage {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "3")]
    role: String,
    #[prost(string, tag = "5")]
    body: String,
}

#[derive(Clone, PartialEq, Message)]
struct GetResponse {
    #[prost(message, optional, tag = "1")]
    channel: Option<Channel>,
    #[prost(message, repeated, tag = "2")]
    messages: Vec<OperatingMessage>,
    #[prost(int64, tag = "7")]
    replay_cursor: i64,
}

#[derive(Clone, PartialEq, Message)]
struct ListRequest {
    #[prost(message, optional, tag = "1")]
    query: Option<Query>,
    #[prost(string, tag = "2")]
    channel_id: String,
    #[prost(int64, tag = "3")]
    after_cursor: i64,
    #[prost(int32, tag = "4")]
    limit: i32,
}

#[derive(Clone, PartialEq, Message)]
struct Event {
    #[prost(int64, tag = "1")]
    cursor: i64,
    #[prost(string, tag = "2")]
    event_id: String,
    #[prost(string, tag = "3")]
    turn_id: String,
    #[prost(int32, tag = "4")]
    kind: i32,
    #[prost(string, tag = "5")]
    safe_text: String,
    #[prost(string, tag = "9")]
    request_id: String,
    #[prost(int32, tag = "10")]
    request_type: i32,
    #[prost(string, tag = "12")]
    request_call_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct ListResponse {
    #[prost(message, repeated, tag = "1")]
    events: Vec<Event>,
    #[prost(int64, tag = "2")]
    next_cursor: i64,
    #[prost(bool, tag = "3")]
    has_more: bool,
    #[prost(bool, tag = "4")]
    reset_required: bool,
}

#[derive(Clone, PartialEq, Message)]
struct SubmitRequest {
    #[prost(message, optional, tag = "1")]
    query: Option<Query>,
    #[prost(string, tag = "2")]
    channel_id: String,
    #[prost(string, tag = "3")]
    body: String,
    #[prost(string, tag = "4")]
    idempotency_key: String,
}

#[derive(Clone, PartialEq, Message)]
struct Turn {
    #[prost(string, tag = "1")]
    turn_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct SubmitResponse {
    #[prost(message, optional, tag = "6")]
    accepted_turn: Option<Turn>,
    #[prost(int64, tag = "7")]
    replay_cursor: i64,
}

#[derive(Clone, PartialEq, Message)]
struct ThreadResponse {
    #[prost(string, tag = "1")]
    request_id: String,
    #[prost(string, tag = "2")]
    call_id: String,
    #[prost(int32, tag = "3")]
    request_type: i32,
    #[prost(int32, tag = "4")]
    action: i32,
    #[prost(string, tag = "5")]
    text: String,
    #[prost(string, tag = "7")]
    idempotency_key: String,
}

#[derive(Clone, PartialEq, Message)]
struct RespondRequest {
    #[prost(message, optional, tag = "1")]
    query: Option<Query>,
    #[prost(string, tag = "2")]
    channel_id: String,
    #[prost(string, tag = "3")]
    turn_id: String,
    #[prost(message, optional, tag = "4")]
    response: Option<ThreadResponse>,
    #[prost(string, tag = "5")]
    idempotency_key: String,
}

#[derive(Clone, PartialEq, Message)]
struct RespondResponse {
    #[prost(int64, tag = "5")]
    replay_cursor: i64,
}

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

struct ThreadClient {
    http: Client,
    base: Url,
    session: PlatformSession,
    channel_id: String,
}

impl ThreadClient {
    fn new(session: PlatformSession, channel_id: String, base_url: &str) -> Result<Self> {
        let base = Url::parse(base_url).context("invalid Platform URL")?;
        if !matches!(base.scheme(), "https" | "http")
            || base.host_str().is_none()
            || (base.scheme() == "http"
                && !matches!(base.host_str(), Some("127.0.0.1" | "localhost" | "::1")))
        {
            bail!("Platform URL must use HTTPS or loopback HTTP");
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            base,
            session,
            channel_id,
        })
    }

    fn query(&self) -> Result<Query> {
        let workspace_id = self
            .session
            .workspace_id
            .as_ref()
            .filter(|id| !id.trim().is_empty())
            .context("managed login must select a workspace")?
            .clone();
        Ok(Query {
            workspace_id,
            organization_id: self.session.organization_id.clone(),
        })
    }

    async fn call<Req: Message, Resp: Message + Default>(
        &self,
        method: &str,
        request: Req,
    ) -> Result<Resp> {
        let url = self.base.join(&format!("{SERVICE}/{method}"))?;
        let workspace = self.query()?.workspace_id;
        let mut response = self
            .http
            .post(url)
            .bearer_auth(&self.session.access_token)
            .header("X-Organization-ID", &self.session.organization_id)
            .header("X-Workspace-ID", workspace)
            .header("Connect-Protocol-Version", "1")
            .header("Content-Type", "application/proto")
            .header("Accept", "application/proto")
            .body(request.encode_to_vec())
            .send()
            .await
            .context("Platform request failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Platform {method} returned HTTP {status}");
        }
        if response
            .content_length()
            .is_some_and(|size| size as usize > MAX_RESPONSE_BYTES)
        {
            bail!("Platform response exceeds limit");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                bail!("Platform response exceeds limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        Resp::decode(bytes.as_slice()).context("invalid Platform protobuf response")
    }

    async fn get(&self) -> Result<GetResponse> {
        let result: GetResponse = self
            .call(
                "GetOperatingThread",
                GetRequest {
                    query: Some(self.query()?),
                    channel_id: self.channel_id.clone(),
                    limit: 50,
                },
            )
            .await?;
        if result
            .channel
            .as_ref()
            .is_none_or(|channel| channel.id != self.channel_id)
        {
            bail!("Platform returned a different or missing thread");
        }
        Ok(result)
    }

    async fn events(&self, cursor: i64) -> Result<ListResponse> {
        self.call(
            "ListOperatingThreadEvents",
            ListRequest {
                query: Some(self.query()?),
                channel_id: self.channel_id.clone(),
                after_cursor: cursor,
                limit: 200,
            },
        )
        .await
    }

    async fn submit(&self, body: String) -> Result<SubmitResponse> {
        if body.trim().is_empty() || body.len() > 20_000 {
            bail!("message must contain 1 to 20000 bytes");
        }
        let result: SubmitResponse = self
            .call(
                "SubmitOperatingMessage",
                SubmitRequest {
                    query: Some(self.query()?),
                    channel_id: self.channel_id.clone(),
                    body,
                    idempotency_key: Uuid::new_v4().to_string(),
                },
            )
            .await?;
        if result
            .accepted_turn
            .as_ref()
            .is_none_or(|turn| turn.turn_id.is_empty())
        {
            bail!("Platform did not return an accepted turn");
        }
        Ok(result)
    }

    async fn respond(&self, pending: &Event, action: i32, text: String) -> Result<RespondResponse> {
        let key = Uuid::new_v4().to_string();
        self.call(
            "RespondOperatingThread",
            RespondRequest {
                query: Some(self.query()?),
                channel_id: self.channel_id.clone(),
                turn_id: pending.turn_id.clone(),
                response: Some(ThreadResponse {
                    request_id: pending.request_id.clone(),
                    call_id: pending.request_call_id.clone(),
                    request_type: pending.request_type,
                    action,
                    text,
                    idempotency_key: key.clone(),
                }),
                idempotency_key: key,
            },
        )
        .await
    }
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
                println!("Turn {turn_id} is still running. Reattach to continue watching.")
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
                if !event.request_id.is_empty() && matches!(event.kind, 4 | 5 | 14 | 15) {
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
                if event.turn_id == turn_id && matches!(event.kind, 4 | 5 | 14 | 15) {
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
            if !event.request_id.is_empty() && matches!(event.kind, 4 | 5 | 14 | 15) {
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
    async fn typed_owner_calls_keep_tenant_and_request_identity() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for method in [
                "GetOperatingThread",
                "SubmitOperatingMessage",
                "ListOperatingThreadEvents",
                "RespondOperatingThread",
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
                            "/deixic.v1.deixicservice/{}",
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
                    "GetOperatingThread" => {
                        let request = GetRequest::decode(body).unwrap();
                        assert_eq!(request.query.unwrap().organization_id, "org-a");
                        assert_eq!(request.channel_id, "thread:one");
                        GetResponse {
                            channel: Some(Channel {
                                id: "thread:one".into(),
                            }),
                            messages: vec![OperatingMessage {
                                id: "msg-1".into(),
                                role: "user".into(),
                                body: "prior".into(),
                            }],
                            replay_cursor: 9,
                        }
                        .encode_to_vec()
                    }
                    "SubmitOperatingMessage" => {
                        let request = SubmitRequest::decode(body).unwrap();
                        assert_eq!(request.query.unwrap().workspace_id, "ws-a");
                        assert_eq!(request.channel_id, "thread:one");
                        assert_eq!(request.body, "continue");
                        assert!(!request.idempotency_key.is_empty());
                        SubmitResponse {
                            accepted_turn: Some(Turn {
                                turn_id: "turn-2".into(),
                            }),
                            replay_cursor: 10,
                        }
                        .encode_to_vec()
                    }
                    "ListOperatingThreadEvents" => {
                        let request = ListRequest::decode(body).unwrap();
                        assert_eq!(request.after_cursor, 9);
                        ListResponse {
                            events: vec![Event {
                                cursor: 10,
                                event_id: "event-10".into(),
                                turn_id: "turn-2".into(),
                                kind: 4,
                                safe_text: "Review the action".into(),
                                request_id: "request-1".into(),
                                request_type: 1,
                                request_call_id: "call-1".into(),
                            }],
                            next_cursor: 10,
                            has_more: false,
                            reset_required: false,
                        }
                        .encode_to_vec()
                    }
                    "RespondOperatingThread" => {
                        let request = RespondRequest::decode(body).unwrap();
                        assert_eq!(request.query.unwrap().organization_id, "org-a");
                        assert_eq!(request.turn_id, "turn-2");
                        let response = request.response.unwrap();
                        assert_eq!(response.request_id, "request-1");
                        assert_eq!(response.action, 2);
                        assert_eq!(response.idempotency_key, request.idempotency_key);
                        RespondResponse { replay_cursor: 11 }.encode_to_vec()
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
