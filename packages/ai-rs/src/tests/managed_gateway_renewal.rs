use super::*;
use crate::managed_authorization::{ManagedAuthorizationProvider, ManagedAuthorizationRenewal};
use maestro_runtime_contracts::{ManagedGatewayCredential, ManagedInferenceAuthorization};
use std::{
    future::Future,
    io::Write,
    net::TcpListener,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct RenewingController(AtomicUsize);

impl ManagedAuthorizationProvider for RenewingController {
    fn renew(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ManagedAuthorizationRenewal>> + Send + '_>>
    {
        Box::pin(async move {
            let call = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            Ok(ManagedAuthorizationRenewal {
                authorization: ManagedInferenceAuthorization::new(managed_authorization_fixture(
                    "lineage-receipt",
                )),
                gateway_credential: Some(ManagedGatewayCredential::new(
                    format!("fresh-bearer-{call}"),
                    now + 60,
                )),
            })
        })
    }
}

fn two_request_gateway() -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let body = MANAGED_COMPLETED_SSE.to_string();
    let receipts = managed_receipt_headers();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            tx.send(read_complete_http_request(&mut stream)).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n", body.len()).unwrap();
            for (name, value) in &receipts {
                write!(stream, "{name}: {value}\r\n").unwrap();
            }
            write!(stream, "\r\n{body}").unwrap();
        }
    });
    (format!("http://{address}/v1"), rx)
}

#[tokio::test]
async fn hosted_managed_inference_renews_gateway_bearer_on_first_and_later_calls() {
    let controller = Arc::new(RenewingController(AtomicUsize::new(0)));
    let (base_url, request_rx) = two_request_gateway();
    let mut client = OpenAiClient::with_base_url("expired-bootstrap", base_url)
        .unwrap()
        .with_route_provider("openai")
        .with_managed_gateway_scope(
            "org_123",
            "workspace_456",
            serde_json::json!({
                "provider": "openai", "environment": "production", "credential_name": "default"
            }),
        )
        .unwrap();
    client.set_managed_request_lineage(Some("lineage-receipt".into()));
    client.set_managed_inference_authorization(Some(managed_authorization_fixture(
        "lineage-receipt",
    )));
    client.set_managed_authorization_provider(controller.clone());
    for call in 1..=2 {
        let stream = client.stream(&[], &RequestConfig::default()).await.unwrap();
        let events = collect_stream_events(stream).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::ProviderError { .. })),
            "{events:?}"
        );
        let request = request_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            captured_request_headers(&request)["authorization"],
            format!("Bearer fresh-bearer-{call}")
        );
        assert!(!request.contains("expired-bootstrap"));
        assert!(
            !captured_request_body(&request)
                .to_string()
                .contains("fresh-bearer")
        );
    }
    assert_eq!(controller.0.load(Ordering::SeqCst), 2);
}
