//! Private, tenant-bound native credentials supplied and refreshed by the parent.
use super::*;
use std::io::Write;

const LIMIT: usize = 64 * 1024;
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Descriptor {
    version: u32,
    pub(crate) organization_id: String,
    pub(crate) workspace_id: String,
    pub(crate) runner_session_id: String,
    gateway: GatewayCredential,
    pub(crate) tool_execution: ToolCredential,
    refresh: Option<Refresh>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GatewayCredential {
    token: String,
    expires_at_epoch_seconds: u64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ToolCredential {
    pub(crate) base_url: String,
    pub(crate) platform_base_url: String,
    pub(crate) token: String,
    expires_at_epoch_seconds: u64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Refresh {
    base_url: String,
    token: String,
}

fn valid_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 * 1024 && !value.chars().any(char::is_whitespace)
}
fn service_origin(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/"
    })
}
fn refresh_url(value: &str) -> Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(value).map_err(|_| "Native refresh owner is invalid")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("Native refresh owner must be the private loopback parent".into());
    }
    url.set_path("/credential");
    Ok(url)
}
impl Descriptor {
    fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.organization_id.is_empty()
            || self.workspace_id.is_empty()
            || self.runner_session_id.is_empty()
            || !valid_token(&self.gateway.token)
            || !valid_token(&self.tool_execution.token)
            || !service_origin(&self.tool_execution.base_url)
            || !service_origin(&self.tool_execution.platform_base_url)
        {
            return Err("Native credential descriptor is invalid".into());
        }
        if let Some(refresh) = &self.refresh {
            refresh_url(&refresh.base_url)?;
            if !valid_token(&refresh.token) {
                return Err("Native refresh credential is invalid".into());
            }
        }
        Ok(())
    }
    fn same_owner(&self, other: &Self) -> bool {
        self.organization_id == other.organization_id
            && self.workspace_id == other.workspace_id
            && self.runner_session_id == other.runner_session_id
            && self.tool_execution.base_url == other.tool_execution.base_url
            && self.tool_execution.platform_base_url == other.tool_execution.platform_base_url
    }
    fn expires_soon(&self) -> bool {
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        self.tool_execution.expires_at_epoch_seconds <= now.saturating_add(30)
            || self.gateway.expires_at_epoch_seconds <= now.saturating_add(30)
    }
}

#[derive(Clone)]
pub(crate) struct Credentials {
    path: PathBuf,
    owner: Descriptor,
}
impl Credentials {
    pub(crate) fn from_env() -> Result<Self, String> {
        let path = env::var("MAESTRO_NATIVE_CODE_DESCRIPTOR_FILE")
            .map(PathBuf::from)
            .map_err(|_| "Native governed credential descriptor is not configured")?;
        let owner = read(&path)?;
        Ok(Self { path, owner })
    }
    pub(crate) fn matches_principal(&self, auth: &AuthContext) -> bool {
        Some(self.owner.organization_id.as_str()) == auth.organization_id.as_deref()
            && Some(self.owner.workspace_id.as_str()) == auth.workspace_id.as_deref()
    }
    pub(crate) fn matches_runner(&self, runner: &str) -> bool {
        self.owner.runner_session_id == runner
    }
    pub(crate) async fn current(&self, http: &reqwest::Client) -> Result<Descriptor, String> {
        let mut current = read(&self.path)?;
        if !self.owner.same_owner(&current) {
            return Err("Native credential owner changed".into());
        }
        if current.expires_soon() {
            let refresh = current
                .refresh
                .as_ref()
                .ok_or("Native credential refresh owner is unavailable")?;
            let url = refresh_url(&refresh.base_url)?;
            let mut response = http
                .post(url)
                .bearer_auth(&refresh.token)
                .body(Vec::new())
                .send()
                .await
                .map_err(|_| "Native credential refresh failed")?;
            if !response.status().is_success() {
                return Err("Native credential refresh was rejected".into());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "Native credential refresh response failed")?
            {
                if bytes.len().saturating_add(chunk.len()) > LIMIT {
                    return Err("Native credential refresh response exceeds its bound".into());
                }
                bytes.extend(chunk);
            }
            let renewed: Descriptor = serde_json::from_slice(&bytes)
                .map_err(|_| "Native refreshed credential is invalid")?;
            renewed.validate()?;
            if !self.owner.same_owner(&renewed) || renewed.expires_soon() {
                return Err("Native refreshed credential owner or expiry is invalid".into());
            }
            write_private(&self.path, &bytes)?;
            current = renewed;
        }
        write_gateway(&current)?;
        Ok(current)
    }
}

fn read(path: &Path) -> Result<Descriptor, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "Native credential descriptor is unavailable")?;
    if !metadata.is_file() || metadata.len() > LIMIT as u64 {
        return Err("Native credential descriptor is invalid".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("Native credential descriptor is not private".into());
        }
    }
    let bytes = std::fs::read(path).map_err(|_| "Native credential descriptor is unavailable")?;
    let descriptor: Descriptor =
        serde_json::from_slice(&bytes).map_err(|_| "Native credential descriptor is invalid")?;
    descriptor.validate()?;
    Ok(descriptor)
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension(format!(
        "native-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o400);
    }
    let result = (|| {
        let mut file = options
            .open(&temp)
            .map_err(|_| "Native private credential file is unavailable")?;
        file.write_all(bytes)
            .map_err(|_| "Native private credential write failed")?;
        file.sync_all()
            .map_err(|_| "Native private credential sync failed")?;
        std::fs::rename(&temp, path).map_err(|_| "Native private credential replacement failed")
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result.map_err(str::to_owned)
}
fn write_gateway(descriptor: &Descriptor) -> Result<(), String> {
    let path = gateway_path();
    write_private(&path, descriptor.gateway.token.as_bytes())
}
pub(crate) fn gateway_path() -> PathBuf {
    env::var("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/run/sandboxwich/bootstrap/evalops-access-token"))
}

pub(crate) fn prepare_gateway() -> Result<(), String> {
    let credential = Credentials::from_env()?;
    if credential.owner.expires_soon() {
        return Err("Native bootstrap credential is already expiring".into());
    }
    write_gateway(&credential.owner)
}

pub(crate) fn validate_owner(org: &str, workspace: &str, runner: &str) -> Result<(), String> {
    let credentials = Credentials::from_env()?;
    if credentials.owner.organization_id != org
        || credentials.owner.workspace_id != workspace
        || credentials.owner.runner_session_id != runner
    {
        return Err("Native bootstrap credentials do not match the hosted launch owner".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_descriptor_accepts_canonical_cluster_service_origins_and_rejects_url_ambiguity() {
        assert!(service_origin(
            "http://tool-execution-service.evalops.svc.cluster.local:8080"
        ));
        assert!(service_origin(
            "http://platform-api-service.evalops.svc.cluster.local:8080"
        ));
        assert!(service_origin("https://platform.example"));
        for url in [
            "ftp://platform.example",
            "http://user@platform.example",
            "https://platform.example/path",
            "https://platform.example?secret=x",
            "https://platform.example#fragment",
        ] {
            assert!(!service_origin(url));
        }
    }

    #[test]
    fn refresh_origin_rejects_remote_urls_and_ambiguous_coordinates() {
        assert!(refresh_url("http://127.0.0.1:1234").is_ok());
        for url in [
            "https://example.com",
            "http://localhost:1234",
            "http://127.0.0.1:1234/other",
            "http://user@127.0.0.1:1234",
            "http://127.0.0.1:1234?token=secret",
        ] {
            assert!(refresh_url(url).is_err());
        }
    }

    #[tokio::test]
    async fn renews_through_the_authenticated_parent_and_rejects_a_changed_owner() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _guard = crate::tests::ENV_LOCK.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let previous = env::var_os("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE");
        env::set_var(
            "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE",
            temp.path().join("gateway-token"),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let now = chrono::Utc::now().timestamp() as u64;
        let fixture = serde_json::json!({"version":1,"organizationId":"org","workspaceId":"workspace","runnerSessionId":"runner","gateway":{"token":"gateway-old","expiresAtEpochSeconds":now + 5},"toolExecution":{"baseUrl":"https://tools.example.com","platformBaseUrl":"https://platform.example.com","token":"tools-old","expiresAtEpochSeconds":now + 5},"refresh":{"baseUrl":origin,"token":"parent-secret"}});
        let path = temp.path().join("descriptor");
        write_private(&path, fixture.to_string().as_bytes()).unwrap();
        let owner = read(&path).unwrap();
        let credentials = Credentials {
            path: path.clone(),
            owner: owner.clone(),
        };
        let mut fresh = fixture.clone();
        fresh["gateway"]["token"] = serde_json::json!("gateway-renewed");
        fresh["gateway"]["expiresAtEpochSeconds"] = serde_json::json!(now + 300);
        fresh["toolExecution"]["token"] = serde_json::json!("tools-renewed");
        fresh["toolExecution"]["expiresAtEpochSeconds"] = serde_json::json!(now + 300);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let count = stream.read(&mut bytes).await.unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]);
            assert!(request.starts_with("POST /credential HTTP/1.1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer parent-secret")
            );
            let body = fresh.to_string();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let renewed = credentials.current(&http).await.unwrap();
        assert!(renewed.tool_execution.token == "tools-renewed");
        assert!(std::fs::read(temp.path().join("gateway-token")).unwrap() == b"gateway-renewed");
        assert!(read(&path).unwrap().tool_execution.token == "tools-renewed");
        server.await.unwrap();
        let mut changed = fixture;
        changed["workspaceId"] = serde_json::json!("another-owner");
        write_private(&path, changed.to_string().as_bytes()).unwrap();
        assert!(credentials.current(&http).await.is_err());
        match previous {
            Some(value) => env::set_var("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE", value),
            None => env::remove_var("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE"),
        }
    }
}
