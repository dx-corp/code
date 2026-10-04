use super::*;

fn fixture() -> (
    Descriptor,
    HostedRunnerWorkloadIdentityConfig,
    RefreshResponse,
) {
    let now = chrono::Utc::now().timestamp();
    let sandbox = Uuid::new_v4();
    let descriptor = Descriptor {
        version: 1,
        organization_id: "org".into(),
        workspace_id: "ws".into(),
        runner_session_id: "runner".into(),
        gateway: Credential {
            token: "old-gateway".into(),
            expires_at_epoch_seconds: now + 30,
        },
        tool_execution: ToolCredential {
            base_url: "http://tool-executor.evalops.svc.cluster.local:8080".into(),
            platform_base_url: "http://platform-api.evalops.svc.cluster.local:8080".into(),
            token: "old-tool".into(),
            expires_at_epoch_seconds: now + 30,
        },
        refresh: None,
    };
    let workload = HostedRunnerWorkloadIdentityConfig {
        kubernetes_token_file: "unused".into(),
        identity_tls_ca_file: "unused".into(),
        identity_exchange_url: "https://identity.example/exchange".parse().unwrap(),
        organization_id: "org".into(),
        workspace_id: "ws".into(),
        sandbox_id: sandbox,
        placement_generation: 7,
    };
    let response = RefreshResponse {
        organization_id: "org".into(),
        workspace_id: "ws".into(),
        runner_session_id: "runner".into(),
        physical_sandbox_id: sandbox.to_string(),
        placement_generation: 7,
        resident_generation: 2,
        tool_execution_token: "new-tool".into(),
        tool_execution_expires_at_epoch_seconds: now + 290,
        gateway_token: "new-gateway".into(),
        gateway_expires_at_epoch_seconds: now + 290,
    };
    (descriptor, workload, response)
}
#[test]
fn native_code_refresh_rejects_cross_owner_stale_runtime_and_invalid_expiry() {
    let (descriptor, workload, response) = fixture();
    let mut valid = descriptor.clone();
    apply_refresh(&mut valid, &workload, response.clone()).unwrap();
    assert_eq!(valid.tool_execution.token, "new-tool");
    for invalid in [
        RefreshResponse {
            workspace_id: "other".into(),
            ..response.clone()
        },
        RefreshResponse {
            runner_session_id: "other".into(),
            ..response.clone()
        },
        RefreshResponse {
            placement_generation: 8,
            ..response.clone()
        },
        RefreshResponse {
            resident_generation: 0,
            ..response.clone()
        },
        RefreshResponse {
            tool_execution_expires_at_epoch_seconds: 0,
            ..response.clone()
        },
        RefreshResponse {
            physical_sandbox_id: Uuid::new_v4().to_string(),
            ..response.clone()
        },
    ] {
        assert!(apply_refresh(&mut descriptor.clone(), &workload, invalid).is_err());
    }
}
#[tokio::test]
async fn native_code_refresh_keeps_descriptor_and_gateway_in_private_files() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native-code-descriptor.json");
    let (descriptor, _, _) = fixture();
    write_descriptor(&path, &descriptor).await.unwrap();
    assert_eq!(
        read_descriptor(&path).await.unwrap().tool_execution.token,
        "old-tool"
    );
    assert_eq!(
        tokio::fs::read_to_string(path.with_file_name("evalops-access-token"))
            .await
            .unwrap(),
        "old-gateway"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(&path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
    }
}
#[tokio::test]
async fn native_code_refresh_reads_binary_response_with_bounded_headers_and_body() {
    let (_, _, response) = fixture();
    let bytes = response.encode_to_vec();
    let mut wire = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nContent-Length: {}\r\n\r\n",
        bytes.len()
    )
    .into_bytes();
    wire.extend(bytes);
    assert_eq!(
        read_refresh(&mut BufReader::new(&wire[..]))
            .await
            .unwrap()
            .tool_execution_token,
        "new-tool"
    );
    let bad =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nContent-Length: 65537\r\n\r\n";
    assert!(read_refresh(&mut BufReader::new(&bad[..])).await.is_err());
}
