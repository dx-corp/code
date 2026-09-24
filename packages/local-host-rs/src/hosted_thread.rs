//! Tenant-scoped Platform operating-thread client shared by the terminal and desktop gateway.

use crate::credential_mode::PlatformSession;
use crate::public_protocol as wire;
use anyhow::{Context, Result, bail};
use prost::Message;
use reqwest::{Client, Url};
use std::time::Duration;
use uuid::Uuid;

const SERVICE: &str = "/deixicpublic.v1.DeixicPublicService";
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

// Native-friendly view models. Only `wire` types are serialized on the public
// connection; these views do not mirror any internal protobuf field numbers.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Query {
    pub workspace_id: String,
    pub organization_id: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct GetRequest {
    pub query: Option<Query>,
    pub channel_id: String,
    pub limit: i32,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Channel {
    pub id: String,
    pub label: String,
    pub unread_count: i32,
    pub open_count: i32,
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ListChannelsRequest {
    pub query: Option<Query>,
    pub archive_filter: i32,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ListChannelsResponse {
    pub channels: Vec<Channel>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RenameThreadRequest {
    pub query: Option<Query>,
    pub channel_id: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RenameThreadResponse {
    pub channel: Option<Channel>,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ArchiveThreadRequest {
    pub query: Option<Query>,
    pub channel_id: String,
    pub archived: bool,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ArchiveThreadResponse {
    pub channel: Option<Channel>,
    pub changed: bool,
    pub replayed: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct OperatingMessage {
    pub id: String,
    pub role: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct GetResponse {
    pub channel: Option<Channel>,
    pub messages: Vec<OperatingMessage>,
    pub replay_cursor: i64,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ListRequest {
    pub query: Option<Query>,
    pub channel_id: String,
    pub after_cursor: i64,
    pub limit: i32,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Event {
    pub cursor: i64,
    pub event_id: String,
    pub turn_id: String,
    pub kind: i32,
    pub safe_text: String,
    pub request_id: String,
    pub request_type: i32,
    pub request_call_id: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ListResponse {
    pub events: Vec<Event>,
    pub next_cursor: i64,
    pub has_more: bool,
    pub reset_required: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct SubmitRequest {
    pub query: Option<Query>,
    pub channel_id: String,
    pub body: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Turn {
    pub turn_id: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct SubmitResponse {
    pub accepted_turn: Option<Turn>,
    pub replay_cursor: i64,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ThreadResponse {
    pub request_id: String,
    pub call_id: String,
    pub request_type: i32,
    pub action: i32,
    pub text: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RespondRequest {
    pub query: Option<Query>,
    pub channel_id: String,
    pub turn_id: String,
    pub response: Option<ThreadResponse>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RespondResponse {
    pub replay_cursor: i64,
}

impl From<wire::Thread> for Channel {
    fn from(value: wire::Thread) -> Self {
        Self {
            id: value.id,
            label: value.title,
            unread_count: value.unread_count,
            open_count: value.open_count,
            archived: value.archived,
        }
    }
}

impl From<wire::TaskMessage> for OperatingMessage {
    fn from(value: wire::TaskMessage) -> Self {
        let role = match value.role {
            1 => "user",
            2 => "assistant",
            3 => "system",
            _ => "unknown",
        };
        Self {
            id: value.id,
            role: role.into(),
            body: value.body,
        }
    }
}

impl From<wire::TaskEvent> for Event {
    fn from(value: wire::TaskEvent) -> Self {
        Self {
            cursor: value.cursor,
            event_id: value.id,
            turn_id: value.turn_id,
            kind: value.kind,
            safe_text: value.text,
            request_id: value.request_id,
            request_type: value.request_kind,
            request_call_id: value.call_id,
        }
    }
}

pub struct ThreadClient {
    http: Client,
    base: Url,
    session: PlatformSession,
    channel_id: String,
}

impl ThreadClient {
    pub fn new(session: PlatformSession, channel_id: String, base_url: &str) -> Result<Self> {
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

    pub fn query(&self) -> Result<Query> {
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

    fn scope(&self) -> Result<wire::Scope> {
        let query = self.query()?;
        Ok(wire::Scope {
            organization_id: query.organization_id,
            workspace_id: query.workspace_id,
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

    pub async fn get(&self) -> Result<GetResponse> {
        let result: wire::GetThreadResponse = self
            .call(
                "GetThread",
                wire::GetThreadRequest {
                    scope: Some(self.scope()?),
                    thread_id: self.channel_id.clone(),
                    limit: 50,
                    ..Default::default()
                },
            )
            .await?;
        let result = GetResponse {
            channel: result.thread.map(Into::into),
            messages: result.messages.into_iter().map(Into::into).collect(),
            replay_cursor: result.replay_cursor,
        };
        if result
            .channel
            .as_ref()
            .is_none_or(|channel| channel.id != self.channel_id)
        {
            bail!("Platform returned a different or missing thread");
        }
        Ok(result)
    }

    pub async fn list_channels(&self, archived: bool) -> Result<ListChannelsResponse> {
        let result: wire::ListThreadsResponse = self
            .call(
                "ListThreads",
                wire::ListThreadsRequest {
                    scope: Some(self.scope()?),
                    archived,
                },
            )
            .await?;
        Ok(ListChannelsResponse {
            channels: result.threads.into_iter().map(Into::into).collect(),
        })
    }

    pub async fn rename(&self, title: String) -> Result<RenameThreadResponse> {
        let result: wire::RenameThreadResponse = self
            .call(
                "RenameThread",
                wire::RenameThreadRequest {
                    scope: Some(self.scope()?),
                    thread_id: self.channel_id.clone(),
                    title,
                },
            )
            .await?;
        let result = RenameThreadResponse {
            channel: result.thread.map(Into::into),
            changed: result.changed,
        };
        if result
            .channel
            .as_ref()
            .is_none_or(|channel| channel.id != self.channel_id)
        {
            bail!("Platform returned a different or missing renamed thread");
        }
        Ok(result)
    }

    pub async fn archive(&self, archived: bool) -> Result<ArchiveThreadResponse> {
        let result: wire::ArchiveThreadResponse = self
            .call(
                "ArchiveThread",
                wire::ArchiveThreadRequest {
                    scope: Some(self.scope()?),
                    thread_id: self.channel_id.clone(),
                    archived,
                    idempotency_key: Uuid::new_v4().to_string(),
                },
            )
            .await?;
        let result = ArchiveThreadResponse {
            channel: result.thread.map(Into::into),
            changed: result.changed,
            replayed: result.replayed,
        };
        if result
            .channel
            .as_ref()
            .is_none_or(|channel| channel.id != self.channel_id || channel.archived != archived)
        {
            bail!("Platform returned a different or unmodified archive state");
        }
        Ok(result)
    }

    pub async fn events(&self, cursor: i64) -> Result<ListResponse> {
        let result: wire::ListEventsResponse = self
            .call(
                "ListEvents",
                wire::ListEventsRequest {
                    scope: Some(self.scope()?),
                    thread_id: self.channel_id.clone(),
                    after_cursor: cursor,
                    limit: 200,
                },
            )
            .await?;
        Ok(ListResponse {
            events: result.events.into_iter().map(Into::into).collect(),
            next_cursor: result.next_cursor,
            has_more: result.has_more,
            reset_required: result.reset_required,
        })
    }

    pub async fn submit(&self, body: String) -> Result<SubmitResponse> {
        if body.trim().is_empty() || body.len() > 20_000 {
            bail!("message must contain 1 to 20000 bytes");
        }
        let result: wire::SubmitTaskResponse = self
            .call(
                "SubmitTask",
                wire::SubmitTaskRequest {
                    scope: Some(self.scope()?),
                    thread_id: self.channel_id.clone(),
                    body,
                    idempotency_key: Uuid::new_v4().to_string(),
                    ..Default::default()
                },
            )
            .await?;
        let result = SubmitResponse {
            accepted_turn: result.accepted_turn.map(|turn| Turn {
                turn_id: turn.turn_id,
            }),
            replay_cursor: result.replay_cursor,
        };
        if result
            .accepted_turn
            .as_ref()
            .is_none_or(|turn| turn.turn_id.is_empty())
        {
            bail!("Platform did not return an accepted turn");
        }
        Ok(result)
    }

    pub async fn respond(
        &self,
        pending: &Event,
        action: i32,
        text: String,
    ) -> Result<RespondResponse> {
        let key = Uuid::new_v4().to_string();
        let result: wire::RespondToRequestResponse = self
            .call(
                "RespondToRequest",
                wire::RespondToRequestRequest {
                    scope: Some(self.scope()?),
                    thread_id: self.channel_id.clone(),
                    turn_id: pending.turn_id.clone(),
                    request_id: pending.request_id.clone(),
                    call_id: pending.request_call_id.clone(),
                    request_kind: pending.request_type,
                    action,
                    text,
                    idempotency_key: key,
                    ..Default::default()
                },
            )
            .await?;
        Ok(RespondResponse {
            replay_cursor: result.replay_cursor,
        })
    }
}
