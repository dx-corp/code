use super::*;
use tokio::io::{AsyncReadExt, BufWriter};

#[tokio::test]
async fn http_frames_are_visible_before_the_writer_returns() {
    // Like tokio-rustls, BufWriter may accept a complete frame without
    // delivering it to the underlying stream until flush is polled.
    let mut socket = BufWriter::with_capacity(4096, Vec::new());
    write_response(&mut socket, 200, "application/json", b"{\"ok\":true}")
        .await
        .unwrap();
    assert!(socket.get_ref().ends_with(b"\r\n\r\n{\"ok\":true}"));

    let mut socket = BufWriter::with_capacity(4096, Vec::new());
    write_sse_headers(&mut socket).await.unwrap();
    assert!(socket.get_ref().ends_with(b"\r\n\r\n"));
    let header_len = socket.get_ref().len();
    let envelope = stream_message(
        1,
        FromAgentMessage::ResponseStart {
            response_id: "flush-test".into(),
        },
    );
    write_sse_event(&mut socket, &envelope).await.unwrap();
    let frame = &socket.get_ref()[header_len..];
    assert!(frame.starts_with(b"data: "));
    assert!(frame.ends_with(b"\n\n"));
    assert_eq!(
        &frame[6..frame.len() - 2],
        serde_json::to_vec(&envelope).unwrap()
    );
}

#[tokio::test]
async fn tls_response_survives_backpressure_before_connection_drop() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
    server_config.send_tls13_tickets = 0;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

    // A TLS record exceeds this transport capacity. The writer must wait for
    // the reader rather than dropping accepted but still-buffered ciphertext.
    let (server_io, client_io) = tokio::io::duplex(64);
    let exchange = async {
        let (server, client) = tokio::join!(
            acceptor.accept(server_io),
            connector.connect("localhost".try_into().unwrap(), client_io)
        );
        let mut server = server.unwrap();
        let mut client = client.unwrap();
        let body = vec![b'x'; 4096];
        let expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8(body.clone()).unwrap()
        );
        let send = async {
            write_response(&mut server, 200, "application/json", &body)
                .await
                .unwrap();
            drop(server);
        };
        let receive = async {
            let mut received = vec![0; expected.len()];
            client.read_exact(&mut received).await.unwrap();
            assert_eq!(received, expected.as_bytes());
        };
        tokio::join!(send, receive);
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("TLS response must drain without another application write");
}
