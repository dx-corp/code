//! Narrow unary protobuf edge matching proto/deixicpublic/v1/native_code.proto.
use super::*;
#[derive(Clone, PartialEq, Message)]
pub(super) struct GetAdmissionRequest {
    #[prost(string, tag = "1")]
    pub(super) organization_id: String,
    #[prost(string, tag = "2")]
    pub(super) workspace_id: String,
    #[prost(string, tag = "3")]
    pub(super) admission_id: String,
}
#[derive(Clone, PartialEq, Message)]
struct GetAdmissionResponse {
    #[prost(message, optional, tag = "1")]
    admission: Option<Admission>,
}

pub(super) async fn read(
    http: &reqwest::Client,
    url: reqwest::Url,
    token: &str,
    request: GetAdmissionRequest,
) -> Result<Admission, String> {
    let mut response = http
        .post(url)
        .bearer_auth(token)
        .header("content-type", "application/proto")
        .header("connect-protocol-version", "1")
        .header("x-organization-id", &request.organization_id)
        .header("x-workspace-id", &request.workspace_id)
        .body(request.encode_to_vec())
        .send()
        .await
        .map_err(|_| "Hosted admission owner request failed")?;
    if !response.status().is_success() {
        return Err(format!(
            "Hosted admission owner rejected the request ({})",
            response.status().as_u16()
        ));
    }
    if response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        != Some("application/proto")
    {
        return Err("Hosted admission owner returned an invalid content type".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Hosted admission owner response failed")?
    {
        if bytes.len().saturating_add(chunk.len()) > 16 * 1024 {
            return Err("Hosted admission response exceeds its bound".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    GetAdmissionResponse::decode(bytes.as_slice())
        .ok()
        .and_then(|response| response.admission)
        .ok_or_else(|| "Hosted admission owner returned an invalid response".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn admission_http_uses_the_owner_protobuf_contract_and_tenant_headers() {
        let _guard = crate::tests::ENV_LOCK.lock().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let directory = tempfile::tempdir().unwrap();
        let descriptor = directory.path().join("descriptor");
        let expiry = chrono::Utc::now().timestamp() + 300;
        let fixture = serde_json::json!({"version":1,"organizationId":"org","workspaceId":"workspace","runnerSessionId":"runner","gateway":{"token":"gateway-token","expiresAtEpochSeconds":expiry},"toolExecution":{"baseUrl":origin,"platformBaseUrl":origin,"token":"private-token","expiresAtEpochSeconds":expiry},"refresh":null});
        std::fs::write(&descriptor, fixture.to_string()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&descriptor, std::fs::Permissions::from_mode(0o400)).unwrap();
        }
        let previous = env::var_os("MAESTRO_NATIVE_CODE_DESCRIPTOR_FILE");
        let previous_gateway = env::var_os("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE");
        env::set_var("MAESTRO_NATIVE_CODE_DESCRIPTOR_FILE", &descriptor);
        env::set_var(
            "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE",
            directory.path().join("gateway"),
        );
        let client = Client::from_env().unwrap();
        let owner = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                bytes.extend_from_slice(&buffer[..read]);
                let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let header = std::str::from_utf8(&bytes[..end])
                    .unwrap()
                    .to_ascii_lowercase();
                let length: usize = header
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if bytes.len() < end + 4 + length {
                    continue;
                }
                assert!(header.starts_with(
                    "post /deixicpublic.v1.nativecodeservice/getnativecodeturnadmission "
                ));
                for expected in [
                    "content-type: application/proto",
                    "connect-protocol-version: 1",
                    "authorization: bearer private-token",
                    "x-organization-id: org",
                    "x-workspace-id: workspace",
                ] {
                    assert!(header.contains(expected), "{header}");
                }
                let request =
                    GetAdmissionRequest::decode(&bytes[end + 4..end + 4 + length]).unwrap();
                assert_eq!(request.organization_id, "org");
                assert_eq!(request.workspace_id, "workspace");
                assert_eq!(request.admission_id, "admission");
                let payload = GetAdmissionResponse {
                    admission: Some(Admission {
                        admission_id: "admission".into(),
                        organization_id: "org".into(),
                        workspace_id: "workspace".into(),
                        runner_session_id: "runner".into(),
                        ..Default::default()
                    }),
                }
                .encode_to_vec();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",payload.len()).as_bytes()).await.unwrap();
                socket.write_all(&payload).await.unwrap();
                break;
            }
        });
        let result = client
            .admission(
                "admission",
                &AuthContext {
                    organization_id: Some("org".into()),
                    workspace_id: Some("workspace".into()),
                    ..Default::default()
                },
            )
            .await;
        match previous {
            Some(value) => env::set_var("MAESTRO_NATIVE_CODE_DESCRIPTOR_FILE", value),
            None => env::remove_var("MAESTRO_NATIVE_CODE_DESCRIPTOR_FILE"),
        }
        match previous_gateway {
            Some(value) => env::set_var("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE", value),
            None => env::remove_var("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE"),
        }
        let admission = result.unwrap();
        assert_eq!(admission.admission_id, "admission");
        owner.await.unwrap();
    }
}
