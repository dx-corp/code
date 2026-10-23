#[tokio::test]
async fn disconnected_endpoint_refuses_redirects() {
    use std::io::{Read, Write};
    let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    destination.set_nonblocking(true).unwrap();
    let origin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_url = format!("http://{}", origin.local_addr().unwrap());
    let location = format!("http://{}", destination.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = origin.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "request ended before HTTP headers");
            request.extend_from_slice(&buffer[..count]);
            assert!(request.len() <= 16 * 1024, "request headers too large");
        }
        write!(stream, "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let client =
        super::OpenAiClient::with_disconnected_endpoint("fixture", &origin_url, "ollama").unwrap();
    let response = client.client.get(&origin_url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        destination.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        client.request_model_name("ollama/customer-model"),
        "customer-model"
    );
    server.join().unwrap();
}
