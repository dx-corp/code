//! IPC (Inter-Process Communication)
//!
//! Unix socket-based communication between CLI and daemon.
//! Allows CLI commands like `stop`, `status`, and `stats` to communicate
//! with a running daemon.
//!
//! Security: Uses token-based authentication stored in a file with restricted permissions.

use crate::daemon::DaemonStats;
use serde::{Deserialize, Serialize};
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;
use tracing::{info, warn};

/// Default socket path
pub fn default_socket_path() -> PathBuf {
    dirs::runtime_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ambient-agent.sock")
}

/// Auth token file path (next to socket)
pub fn auth_token_path() -> PathBuf {
    dirs::runtime_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ambient-agent.token")
}

/// IPC request timeout
const IPC_TIMEOUT: Duration = Duration::from_secs(30);

/// IPC request from CLI to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    /// Authentication token
    pub token: String,
    /// The actual command
    pub command: IpcCommand,
}

/// IPC commands
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcCommand {
    /// Stop the daemon
    Stop,
    /// Get daemon status
    Status,
    /// Get daemon stats
    Stats,
    /// Persist in-memory learner state to disk
    FlushLearner,
    /// Pause processing
    Pause,
    /// Resume processing
    Resume,
    /// Ping to check if daemon is alive
    Ping,
}

/// IPC response from daemon to CLI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    /// Success with optional message
    Ok(Option<String>),
    /// Error with message
    Error(String),
    /// Status response
    Status(StatusResponse),
    /// Stats response
    Stats(StatsResponse),
    /// Pong response
    Pong,
    /// Authentication failed
    Unauthorized,
}

/// Status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub running: bool,
    pub status: String,
    pub uptime_secs: u64,
    pub pid: u32,
}

/// Stats information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsResponse {
    pub events_processed: u64,
    pub tasks_executed: u64,
    pub tasks_succeeded: u64,
    pub tasks_failed: u64,
    pub prs_created: u64,
    pub total_cost: f64,
    pub uptime_secs: u64,
}

impl From<DaemonStats> for StatsResponse {
    fn from(stats: DaemonStats) -> Self {
        Self {
            events_processed: stats.events_processed,
            tasks_executed: stats.tasks_executed,
            tasks_succeeded: stats.tasks_succeeded,
            tasks_failed: stats.tasks_failed,
            prs_created: stats.prs_created,
            total_cost: stats.total_cost,
            uptime_secs: stats.uptime_secs,
        }
    }
}

/// Generate a random auth token
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Constant-time token comparison to prevent timing attacks
/// This is a standalone function that can be used without an IpcServer instance
pub fn verify_token_constant_time(provided: &str, expected: &str) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    provided
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// IPC Server that runs alongside the daemon
pub struct IpcServer {
    socket_path: PathBuf,
    token_path: PathBuf,
    auth_token: String,
    listener: Option<UnixListener>,
}

impl IpcServer {
    /// Create a new IPC server
    pub fn new(socket_path: PathBuf) -> Self {
        let token_path = socket_path.with_extension("token");
        Self {
            socket_path,
            token_path,
            auth_token: generate_token(),
            listener: None,
        }
    }

    /// Get the auth token (for testing)
    pub fn token(&self) -> &str {
        &self.auth_token
    }

    /// Bind to the socket
    pub async fn bind(&mut self) -> anyhow::Result<()> {
        // Remove existing socket if present
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        // Create parent directory if needed
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write auth token to file with restricted permissions
        std::fs::write(&self.token_path, &self.auth_token)?;
        std::fs::set_permissions(&self.token_path, Permissions::from_mode(0o600))?;
        info!("Auth token written to {:?}", self.token_path);

        let listener = UnixListener::bind(&self.socket_path)?;

        // Set socket permissions to owner-only
        std::fs::set_permissions(&self.socket_path, Permissions::from_mode(0o600))?;

        info!("IPC server listening on {:?}", self.socket_path);
        self.listener = Some(listener);
        Ok(())
    }

    /// Accept a connection
    pub async fn accept(&self) -> anyhow::Result<UnixStream> {
        let listener = self
            .listener
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Server not bound"))?;
        let (stream, _) = listener.accept().await?;
        Ok(stream)
    }

    /// Verify the auth token
    pub fn verify_token(&self, token: &str) -> bool {
        // Constant-time comparison to prevent timing attacks
        if token.len() != self.auth_token.len() {
            return false;
        }
        token
            .bytes()
            .zip(self.auth_token.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }

    /// Read a request from a stream with timeout
    pub async fn read_request(stream: &mut UnixStream) -> anyhow::Result<IpcRequest> {
        let result = timeout(IPC_TIMEOUT, async {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let request: IpcRequest = serde_json::from_str(&line)?;
            Ok::<_, anyhow::Error>(request)
        })
        .await;

        match result {
            Ok(Ok(request)) => Ok(request),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("IPC request timed out"),
        }
    }

    /// Write a response to a stream with timeout
    pub async fn write_response(
        stream: &mut UnixStream,
        response: &IpcResponse,
    ) -> anyhow::Result<()> {
        let result = timeout(IPC_TIMEOUT, async {
            let json = serde_json::to_string(response)?;
            stream.write_all(json.as_bytes()).await?;
            stream.write_all(b"\n").await?;
            stream.flush().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await;

        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("IPC response timed out"),
        }
    }

    /// Clean up the socket and token files
    pub fn cleanup(&self) {
        if self.socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.socket_path) {
                warn!("Failed to remove socket file: {}", e);
            }
        }
        if self.token_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.token_path) {
                warn!("Failed to remove token file: {}", e);
            }
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// IPC Client for CLI commands
pub struct IpcClient {
    socket_path: PathBuf,
    token_path: PathBuf,
}

impl IpcClient {
    /// Create a new IPC client
    pub fn new(socket_path: PathBuf) -> Self {
        let token_path = socket_path.with_extension("token");
        Self {
            socket_path,
            token_path,
        }
    }

    /// Check if daemon is running (socket exists)
    pub fn is_daemon_running(&self) -> bool {
        self.socket_path.exists()
    }

    /// Read the auth token from file
    fn read_token(&self) -> anyhow::Result<String> {
        if !self.token_path.exists() {
            anyhow::bail!("Auth token file not found. Is the daemon running?");
        }
        let token = std::fs::read_to_string(&self.token_path)?;
        Ok(token.trim().to_string())
    }

    /// Connect to the daemon with timeout
    async fn connect(&self) -> anyhow::Result<UnixStream> {
        if !self.socket_path.exists() {
            anyhow::bail!("Daemon not running (socket not found)");
        }

        let result = timeout(
            Duration::from_secs(5),
            UnixStream::connect(&self.socket_path),
        )
        .await;

        match result {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => anyhow::bail!("Connection to daemon timed out"),
        }
    }

    /// Send a request and get a response
    pub async fn send(&self, command: IpcCommand) -> anyhow::Result<IpcResponse> {
        let token = self.read_token()?;
        let request = IpcRequest { token, command };

        let mut stream = self.connect().await?;

        // Write request with timeout
        let write_result = timeout(IPC_TIMEOUT, async {
            let json = serde_json::to_string(&request)?;
            stream.write_all(json.as_bytes()).await?;
            stream.write_all(b"\n").await?;
            stream.flush().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await;

        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => anyhow::bail!("Request write timed out"),
        }

        // Read response with timeout
        let read_result = timeout(IPC_TIMEOUT, async {
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let response: IpcResponse = serde_json::from_str(&line)?;
            Ok::<_, anyhow::Error>(response)
        })
        .await;

        match read_result {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("Response read timed out"),
        }
    }

    /// Ping the daemon
    pub async fn ping(&self) -> anyhow::Result<bool> {
        match self.send(IpcCommand::Ping).await {
            Ok(IpcResponse::Pong) => Ok(true),
            Ok(IpcResponse::Unauthorized) => {
                anyhow::bail!("Authentication failed. Token may be invalid.");
            }
            Ok(_) => Ok(false),
            Err(_) => Ok(false),
        }
    }

    /// Stop the daemon
    pub async fn stop(&self) -> anyhow::Result<()> {
        match self.send(IpcCommand::Stop).await {
            Ok(IpcResponse::Ok(_)) => Ok(()),
            Ok(IpcResponse::Unauthorized) => anyhow::bail!("Authentication failed"),
            Ok(IpcResponse::Error(e)) => anyhow::bail!(e),
            _ => anyhow::bail!("Unexpected response"),
        }
    }

    /// Get daemon status
    pub async fn status(&self) -> anyhow::Result<StatusResponse> {
        match self.send(IpcCommand::Status).await {
            Ok(IpcResponse::Status(s)) => Ok(s),
            Ok(IpcResponse::Unauthorized) => anyhow::bail!("Authentication failed"),
            Ok(IpcResponse::Error(e)) => anyhow::bail!(e),
            _ => anyhow::bail!("Unexpected response"),
        }
    }

    /// Get daemon stats
    pub async fn stats(&self) -> anyhow::Result<StatsResponse> {
        match self.send(IpcCommand::Stats).await {
            Ok(IpcResponse::Stats(s)) => Ok(s),
            Ok(IpcResponse::Unauthorized) => anyhow::bail!("Authentication failed"),
            Ok(IpcResponse::Error(e)) => anyhow::bail!(e),
            _ => anyhow::bail!("Unexpected response"),
        }
    }

    /// Flush in-memory learner state to disk
    pub async fn flush_learner(&self) -> anyhow::Result<String> {
        match self.send(IpcCommand::FlushLearner).await {
            Ok(IpcResponse::Ok(message)) => Ok(message.unwrap_or_default()),
            Ok(IpcResponse::Unauthorized) => anyhow::bail!("Authentication failed"),
            Ok(IpcResponse::Error(e)) => anyhow::bail!(e),
            _ => anyhow::bail!("Unexpected response"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_ipc_roundtrip() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("test.sock");

        // Start server
        let mut server = IpcServer::new(socket_path.clone());
        server.bind().await.unwrap();

        let server_token = server.token().to_string();

        // Spawn server handler
        let server_handle = tokio::spawn(async move {
            let mut stream = server.accept().await.unwrap();
            let request = IpcServer::read_request(&mut stream).await.unwrap();

            let response = if server.verify_token(&request.token) {
                match request.command {
                    IpcCommand::Ping => IpcResponse::Pong,
                    _ => IpcResponse::Error("Unexpected".to_string()),
                }
            } else {
                IpcResponse::Unauthorized
            };

            IpcServer::write_response(&mut stream, &response)
                .await
                .unwrap();
        });

        // Give server time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Write valid token to file for client
        std::fs::write(socket_path.with_extension("token"), &server_token).unwrap();

        // Client ping
        let client = IpcClient::new(socket_path);
        let result = client.ping().await.unwrap();
        assert!(result);

        server_handle.await.unwrap();
    }

    #[test]
    fn test_token_verification() {
        let server = IpcServer::new(PathBuf::from("/tmp/test.sock"));

        // Valid token
        assert!(server.verify_token(server.token()));

        // Invalid token
        assert!(!server.verify_token("invalid"));
        assert!(!server.verify_token(""));
    }

    #[test]
    fn generated_tokens_are_random_hex() {
        let first = generate_token();
        let second = generate_token();

        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
