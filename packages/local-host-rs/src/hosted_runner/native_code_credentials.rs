//! The native child refreshes through its parent; workload keys stay in parent.
use super::config::{HostedRunnerRendezvousConfig, HostedRunnerWorkloadIdentityConfig};
use super::*;
use prost::Message;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

// Wire types for proto/remoterunner/v1/remoterunner.proto. This standalone
// native package intentionally depends only on the two narrow owner messages.
#[derive(Clone, PartialEq, Message)]
struct RefreshRequest {
    #[prost(string, tag = "1")]
    organization_id: String,
    #[prost(string, tag = "2")]
    workspace_id: String,
    #[prost(string, tag = "3")]
    runner_session_id: String,
}
#[derive(Clone, PartialEq, Message)]
struct RefreshResponse {
    #[prost(string, tag = "1")]
    organization_id: String,
    #[prost(string, tag = "2")]
    workspace_id: String,
    #[prost(string, tag = "3")]
    runner_session_id: String,
    #[prost(string, tag = "4")]
    physical_sandbox_id: String,
    #[prost(uint64, tag = "5")]
    placement_generation: u64,
    #[prost(uint64, tag = "6")]
    resident_generation: u64,
    #[prost(string, tag = "7")]
    tool_execution_token: String,
    #[prost(int64, tag = "8")]
    tool_execution_expires_at_epoch_seconds: i64,
    #[prost(string, tag = "9")]
    gateway_token: String,
    #[prost(int64, tag = "10")]
    gateway_expires_at_epoch_seconds: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Descriptor {
    version: u32,
    organization_id: String,
    workspace_id: String,
    runner_session_id: String,
    gateway: Credential,
    tool_execution: ToolCredential,
    refresh: Option<RefreshBroker>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Credential {
    token: String,
    expires_at_epoch_seconds: i64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolCredential {
    base_url: String,
    platform_base_url: String,
    token: String,
    expires_at_epoch_seconds: i64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RefreshBroker {
    base_url: String,
    token: String,
}

pub(super) async fn start(
    shared: &SharedRunner,
    client_identity: Option<&workload_identity::ReloadableClientIdentity>,
    rendezvous: Option<&HostedRunnerRendezvousConfig>,
    workload: &HostedRunnerWorkloadIdentityConfig,
    shutdown: CancellationToken,
) -> io::Result<()> {
    if std::env::var("MAESTRO_NATIVE_CODE_ENABLED").as_deref() != Ok("1") {
        return Ok(());
    }
    let path = std::env::var_os("MAESTRO_NATIVE_CODE_DESCRIPTOR_FILE")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("native credential descriptor missing"))?;
    let mut descriptor = read_descriptor(&path).await?;
    if descriptor.version != 1
        || descriptor.organization_id != workload.organization_id
        || descriptor.workspace_id != workload.workspace_id
        || descriptor.runner_session_id != shared.config.runner_session_id
    {
        return Err(io::Error::other(
            "native credential descriptor owner mismatch",
        ));
    }
    let identity = client_identity
        .cloned()
        .ok_or_else(|| io::Error::other("native credential refresh mTLS identity missing"))?;
    let rendezvous = rendezvous
        .cloned()
        .ok_or_else(|| io::Error::other("native credential refresh endpoint missing"))?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let token = Uuid::new_v4().to_string() + &Uuid::new_v4().to_string();
    descriptor.refresh = Some(RefreshBroker {
        base_url: format!("http://{}", listener.local_addr()?),
        token: token.clone(),
    });
    write_descriptor(&path, &descriptor).await?;
    let workload = workload.clone();
    let shared = shared.clone();
    tokio::spawn(async move {
        let owner = Arc::new(tokio::sync::Mutex::new(descriptor));
        loop {
            let accepted = tokio::select! { ()=shutdown.cancelled()=>break, accepted=listener.accept()=>accepted };
            let Ok((mut socket, _)) = accepted else { break };
            let owner = owner.clone();
            let path = path.clone();
            let token = token.clone();
            let identity = identity.clone();
            let rendezvous = rendezvous.clone();
            let workload = workload.clone();
            let shared = shared.clone();
            tokio::spawn(async move {
                let result = tokio::time::timeout(Duration::from_secs(25), async {
                    let request = read_request(&mut socket)
                        .await?
                        .ok_or_else(|| io::Error::other("native refresh request missing"))?;
                    if request.method != "POST"
                        || request.path != "/credential"
                        || request.raw_query.is_some()
                        || !request.body.is_empty()
                        || request.headers.get("authorization") != Some(&format!("Bearer {token}"))
                    {
                        return write_response(
                            &mut socket,
                            403,
                            "application/json",
                            b"{\"error\":\"native credential refresh denied\"}",
                        )
                        .await;
                    }
                    // The model cannot refresh credentials after drain revokes mutations.
                    shared
                        .ensure_mutation_allowed()
                        .map_err(|_| io::Error::other("native runtime not active"))?;
                    let mut descriptor = owner.lock().await;
                    let refreshed = refresh(&identity, &rendezvous, &workload, &descriptor).await?;
                    apply_refresh(&mut descriptor, &workload, refreshed)?;
                    write_descriptor(&path, &descriptor).await?;
                    let bytes = serde_json::to_vec(&*descriptor).map_err(io::Error::other)?;
                    write_response(&mut socket, 200, "application/json", &bytes).await
                })
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    let _ = write_response(
                        &mut socket,
                        503,
                        "application/json",
                        b"{\"error\":\"native credential owner unavailable\"}",
                    )
                    .await;
                }
            });
        }
    });
    Ok(())
}

async fn refresh(
    identity: &workload_identity::ReloadableClientIdentity,
    rendezvous: &HostedRunnerRendezvousConfig,
    workload: &HostedRunnerWorkloadIdentityConfig,
    descriptor: &Descriptor,
) -> io::Result<RefreshResponse> {
    let (tls, revoked, _) = identity
        .snapshot(chrono::Utc::now())
        .await
        .ok_or_else(|| io::Error::other("native workload identity expired"))?;
    let tcp = TcpStream::connect(&rendezvous.endpoint).await?;
    let name = rustls::pki_types::ServerName::try_from(rendezvous.server_name.clone())
        .map_err(io::Error::other)?;
    let mut socket = TlsConnector::from(tls).connect(name, tcp).await?;
    let body = RefreshRequest {
        organization_id: workload.organization_id.clone(),
        workspace_id: workload.workspace_id.clone(),
        runner_session_id: descriptor.runner_session_id.clone(),
    }
    .encode_to_vec();
    tokio::select! {
        ()=revoked.cancelled()=>Err(io::Error::other("native workload identity revoked")),
        result=async {
            socket.write_all(format!("POST /remoterunner.v1.RemoteRunnerService/RefreshNativeCodeCredential HTTP/1.1\r\nHost: {}\r\nContent-Type: application/proto\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",rendezvous.server_name,body.len()).as_bytes()).await?;
            socket.write_all(&body).await?;socket.flush().await?;
            read_refresh(&mut BufReader::new(socket)).await
        }=>result
    }
}

async fn read_refresh<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> io::Result<RefreshResponse> {
    let mut line = String::new();
    (&mut *reader).take(8193).read_line(&mut line).await?;
    if line.trim() != "HTTP/1.1 200 OK" {
        return Err(io::Error::other("native credential owner refused request"));
    }
    let mut length = None;
    let mut content_type = false;
    let mut total = line.len();
    loop {
        line.clear();
        (&mut *reader).take(8193).read_line(&mut line).await?;
        total += line.len();
        if total > 8192 || line.is_empty() {
            return Err(io::Error::other(
                "native credential response headers invalid",
            ));
        }
        if line == "\r\n" {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("native credential response header invalid"))?;
        match name.to_ascii_lowercase().as_str() {
            "content-length" if length.is_none() => {
                length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
            }
            "content-type" => content_type = value.trim() == "application/proto",
            "content-length" | "transfer-encoding" => {
                return Err(io::Error::other("native credential response body invalid"));
            }
            _ => {}
        }
    }
    let length = length
        .filter(|v| *v <= 64 * 1024)
        .ok_or_else(|| io::Error::other("native credential response too large"))?;
    if !content_type {
        return Err(io::Error::other(
            "native credential response is not protobuf",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    RefreshResponse::decode(&*bytes).map_err(io::Error::other)
}

fn apply_refresh(
    descriptor: &mut Descriptor,
    workload: &HostedRunnerWorkloadIdentityConfig,
    response: RefreshResponse,
) -> io::Result<()> {
    let now = chrono::Utc::now().timestamp();
    let credential = |token: &str, expiry: i64| {
        !token.is_empty()
            && token.len() <= 16 * 1024
            && !token.chars().any(char::is_control)
            && expiry > now + 10
            && expiry <= now + 300
    };
    if response.organization_id != descriptor.organization_id
        || response.workspace_id != descriptor.workspace_id
        || response.runner_session_id != descriptor.runner_session_id
        || response.physical_sandbox_id != workload.sandbox_id.to_string()
        || response.placement_generation != workload.placement_generation
        || response.resident_generation == 0
        || !credential(
            &response.tool_execution_token,
            response.tool_execution_expires_at_epoch_seconds,
        )
        || !credential(
            &response.gateway_token,
            response.gateway_expires_at_epoch_seconds,
        )
    {
        return Err(io::Error::other(
            "native credential response owner mismatch",
        ));
    }
    descriptor.gateway = Credential {
        token: response.gateway_token,
        expires_at_epoch_seconds: response.gateway_expires_at_epoch_seconds,
    };
    descriptor.tool_execution.token = response.tool_execution_token;
    descriptor.tool_execution.expires_at_epoch_seconds =
        response.tool_execution_expires_at_epoch_seconds;
    Ok(())
}

async fn read_descriptor(path: &Path) -> io::Result<Descriptor> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "native descriptor permissions are not private",
            ));
        }
    }
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(io::Error::other("native descriptor file invalid"));
    }
    serde_json::from_slice(&tokio::fs::read(path).await?).map_err(io::Error::other)
}
async fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = path.with_extension(format!("{}-tmp", Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o400);
    let mut file = options.open(&temp).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temp, path).await
}
async fn write_descriptor(path: &Path, descriptor: &Descriptor) -> io::Result<()> {
    write_private(
        &path.with_file_name("evalops-access-token"),
        descriptor.gateway.token.as_bytes(),
    )
    .await?;
    write_private(
        path,
        &serde_json::to_vec(descriptor).map_err(io::Error::other)?,
    )
    .await
}

#[cfg(test)]
mod tests;
