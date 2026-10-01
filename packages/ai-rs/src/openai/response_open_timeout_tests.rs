//! Response-header deadlines preserve delayed-body streaming.
use super::send_with_response_open_timeout;

// Real loopback I/O must finish without Tokio advancing a paused clock to
// the open deadline just because the socket is briefly idle. A runnable
// task keeps time under the test's explicit control until it is aborted.
fn prevent_network_clock_auto_advance() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    })
}

#[tokio::test(start_paused = true)]
async fn managed_gateway_response_open_timeout_stops_stalled_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let clock = prevent_network_clock_auto_advance();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock gateway");
    let address = listener.local_addr().expect("mock gateway address");
    let (accepted, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept gateway request");
        let mut request = [0_u8; 4096];
        assert!(
            stream
                .read(&mut request)
                .await
                .expect("read gateway request")
                > 0
        );
        accepted.send(()).expect("request readiness");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
    });

    let request = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{address}/responses"));
    let client = tokio::spawn(send_with_response_open_timeout(
        request,
        Some(std::time::Duration::from_millis(25)),
        "managed gateway",
    ));
    received.await.expect("request reached gateway");
    assert!(
        !client.is_finished(),
        "headers are still pending before the deadline"
    );
    tokio::time::advance(std::time::Duration::from_millis(25)).await;
    let error = client
        .await
        .unwrap()
        .expect_err("stalled response opening must time out");

    assert!(
        error
            .to_string()
            .contains("managed gateway response headers timed out"),
        "unexpected error: {error:#}"
    );
    tokio::time::advance(std::time::Duration::from_millis(125)).await;
    server.await.expect("mock gateway server");
    clock.abort();
}

#[tokio::test(start_paused = true)]
async fn managed_gateway_response_open_timeout_does_not_cover_stream_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let clock = prevent_network_clock_auto_advance();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock gateway");
    let address = listener.local_addr().expect("mock gateway address");
    let (headers_written, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept gateway request");
        let mut request = [0_u8; 4096];
        assert!(
            stream
                .read(&mut request)
                .await
                .expect("read gateway request")
                > 0
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
            .await
            .expect("write response headers");
        stream.flush().await.expect("flush response headers");
        headers_written.send(()).expect("header readiness");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        stream
            .write_all(b"hello")
            .await
            .expect("write delayed body");
    });

    let request = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{address}/responses"));
    let client = tokio::spawn(send_with_response_open_timeout(
        request,
        Some(std::time::Duration::from_millis(50)),
        "managed gateway",
    ));
    received.await.expect("gateway wrote headers");
    let response = client
        .await
        .unwrap()
        .expect("response headers should arrive within the open timeout");
    let body = response.text();
    tokio::pin!(body);
    assert!(futures::poll!(&mut body).is_pending());
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    assert!(
        futures::poll!(&mut body).is_pending(),
        "body must outlive the 50 ms open timeout"
    );
    tokio::time::advance(std::time::Duration::from_millis(99)).await;
    assert!(
        futures::poll!(&mut body).is_pending(),
        "body remains pending until its 150 ms delay"
    );
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    let body = body
        .await
        .expect("body may continue past the response-open timeout");

    assert_eq!(body, "hello");
    server.await.expect("mock gateway server");
    clock.abort();
}
