//! Tenant-scoped Platform operating-thread client shared by the terminal and desktop gateway.

use crate::credential_mode::PlatformSession;
use anyhow::{Context, Result, bail};
use prost::Message;
use reqwest::{Client, Url};
use std::time::Duration;
use uuid::Uuid;

const SERVICE: &str = "/deixic.v1.DeixicService";
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

// A narrow wire projection of console.v1. Field numbers and kinds mirror the
// canonical protobuf contract; unknown fields are ignored by prost.
#[derive(Clone, PartialEq, Message)]
pub struct Query {
    #[prost(string, tag = "1")]
    pub workspace_id: String,
    #[prost(string, tag = "13")]
    pub organization_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetRequest {
    #[prost(message, optional, tag = "1")]
    pub query: Option<Query>,
    #[prost(string, tag = "2")]
    pub channel_id: String,
    #[prost(int32, tag = "3")]
    pub limit: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Channel {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub label: String,
    #[prost(int32, tag = "5")]
    pub unread_count: i32,
    #[prost(int32, tag = "6")]
    pub open_count: i32,
    #[prost(bool, tag = "11")]
    pub archived: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ListChannelsRequest {
    #[prost(message, optional, tag = "1")]
    pub query: Option<Query>,
    #[prost(int32, tag = "2")]
    pub archive_filter: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ListChannelsResponse {
    #[prost(message, repeated, tag = "1")]
    pub channels: Vec<Channel>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RenameThreadRequest {
    #[prost(message, optional, tag = "1")]
    pub query: Option<Query>,
    #[prost(string, tag = "2")]
    pub channel_id: String,
    #[prost(string, tag = "3")]
    pub title: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct RenameThreadResponse {
    #[prost(message, optional, tag = "1")]
    pub channel: Option<Channel>,
    #[prost(bool, tag = "2")]
    pub changed: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ArchiveThreadRequest {
    #[prost(message, optional, tag = "1")]
    pub query: Option<Query>,
    #[prost(string, tag = "2")]
    pub channel_id: String,
    #[prost(bool, tag = "3")]
    pub archived: bool,
    #[prost(string, tag = "4")]
    pub idempotency_key: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ArchiveThreadResponse {
    #[prost(message, optional, tag = "1")]
    pub channel: Option<Channel>,
    #[prost(bool, tag = "2")]
    pub changed: bool,
    #[prost(bool, tag = "3")]
    pub replayed: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct OperatingMessage {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "3")]
    pub role: String,
    #[prost(string, tag = "5")]
    pub body: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetResponse {
    #[prost(message, optional, tag = "1")]
    pub channel: Option<Channel>,
    #[prost(message, repeated, tag = "2")]
    pub messages: Vec<OperatingMessage>,
    #[prost(int64, tag = "7")]
    pub replay_cursor: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct ListRequest {
    #[prost(message, optional, tag = "1")]
    pub query: Option<Query>,
    #[prost(string, tag = "2")]
    pub channel_id: String,
    #[prost(int64, tag = "3")]
    pub after_cursor: i64,
    #[prost(int32, tag = "4")]
    pub limit: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Event {
    #[prost(int64, tag = "1")]
    pub cursor: i64,
    #[prost(string, tag = "2")]
    pub event_id: String,
    #[prost(string, tag = "3")]
    pub turn_id: String,
    #[prost(int32, tag = "4")]
    pub kind: i32,
    #[prost(string, tag = "5")]
    pub safe_text: String,
    #[prost(string, tag = "9")]
    pub request_id: String,
    #[prost(int32, tag = "10")]
    pub request_type: i32,
    #[prost(string, tag = "12")]
    pub request_call_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ListResponse {
    #[prost(message, repeated, tag = "1")]
    pub events: Vec<Event>,
    #[prost(int64, tag = "2")]
    pub next_cursor: i64,
    #[prost(bool, tag = "3")]
    pub has_more: bool,
    #[prost(bool, tag = "4")]
    pub reset_required: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubmitRequest {
    #[prost(message, optional, tag = "1")]
    pub query: Option<Query>,
    #[prost(string, tag = "2")]
    pub channel_id: String,
    #[prost(string, tag = "3")]
    pub body: String,
    #[prost(string, tag = "4")]
    pub idempotency_key: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Turn {
    #[prost(string, tag = "1")]
    pub turn_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubmitResponse {
    #[prost(message, optional, tag = "6")]
    pub accepted_turn: Option<Turn>,
    #[prost(int64, tag = "7")]
    pub replay_cursor: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct ThreadResponse {
    #[prost(string, tag = "1")]
    pub request_id: String,
    #[prost(string, tag = "2")]
    pub call_id: String,
    #[prost(int32, tag = "3")]
    pub request_type: i32,
    #[prost(int32, tag = "4")]
    pub action: i32,
    #[prost(string, tag = "5")]
    pub text: String,
    #[prost(string, tag = "7")]
    pub idempotency_key: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct RespondRequest {
    #[prost(message, optional, tag = "1")]
    pub query: Option<Query>,
    #[prost(string, tag = "2")]
    pub channel_id: String,
    #[prost(string, tag = "3")]
    pub turn_id: String,
    #[prost(message, optional, tag = "4")]
    pub response: Option<ThreadResponse>,
    #[prost(string, tag = "5")]
    pub idempotency_key: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct RespondResponse {
    #[prost(int64, tag = "5")]
    pub replay_cursor: i64,
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

    pub async fn list_channels(&self, archived: bool) -> Result<ListChannelsResponse> {
        self.call(
            "ListOperatingChannels",
            ListChannelsRequest {
                query: Some(self.query()?),
                archive_filter: if archived { 2 } else { 1 },
            },
        )
        .await
    }

    pub async fn rename(&self, title: String) -> Result<RenameThreadResponse> {
        let result: RenameThreadResponse = self
            .call(
                "RenameOperatingThread",
                RenameThreadRequest {
                    query: Some(self.query()?),
                    channel_id: self.channel_id.clone(),
                    title,
                },
            )
            .await?;
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
        let result: ArchiveThreadResponse = self
            .call(
                "ArchiveOperatingThread",
                ArchiveThreadRequest {
                    query: Some(self.query()?),
                    channel_id: self.channel_id.clone(),
                    archived,
                    idempotency_key: Uuid::new_v4().to_string(),
                },
            )
            .await?;
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

    pub async fn submit(&self, body: String) -> Result<SubmitResponse> {
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

    pub async fn respond(
        &self,
        pending: &Event,
        action: i32,
        text: String,
    ) -> Result<RespondResponse> {
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
