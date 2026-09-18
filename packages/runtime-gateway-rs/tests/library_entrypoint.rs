use maestro_runtime_gateway::{RuntimeGatewayConfig, serve_listener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("write request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("read response");
    response
}

#[tokio::test]
async fn library_server_serves_health() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local address");
    let config = RuntimeGatewayConfig::test_default();
    let server = tokio::spawn(serve_listener(listener, config));

    let health = get(addr, "/healthz").await;
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");

    server.abort();
}
