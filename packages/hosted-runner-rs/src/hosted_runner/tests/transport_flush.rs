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
async fn tls_response_drains_backpressure_and_closes_cleanly() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

    // Keep room for the default TLS session tickets during the handshake,
    // but less than a response record so the writer encounters backpressure.
    let (server_io, client_io) = tokio::io::duplex(2048);
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
            let mut received = Vec::new();
            client
                .read_to_end(&mut received)
                .await
                .expect("response must close TLS with close_notify");
            assert_eq!(received, expected.as_bytes());
        };
        tokio::join!(send, receive);
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("TLS response must drain without another application write");
}

#[tokio::test]
async fn closed_broadcast_finishes_sse_but_live_broadcast_stays_open() {
    let workspace = tempfile::tempdir().unwrap();
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let (sender, receiver) = broadcast::channel(4);
    let (mut server, mut client) = tokio::io::duplex(4096);
    let writer = tokio::spawn(async move {
        write_sse_stream(
            &mut server,
            Vec::new(),
            receiver,
            Box::new(shared),
            Box::new(TranscriptStreamFilter::new(
                crate::transcript::TranscriptGrade::default(),
                0,
            )),
            None,
        )
        .await
        .unwrap();
    });

    let mut headers = vec![0; 256];
    let read = client.read(&mut headers).await.unwrap();
    assert!(String::from_utf8_lossy(&headers[..read]).contains("text/event-stream"));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), client.read(&mut headers))
            .await
            .is_err(),
        "a live broadcaster must not close its SSE response"
    );

    drop(sender);
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut rest))
        .await
        .expect("closed broadcaster must finish the SSE response")
        .unwrap();
    writer.await.unwrap();
}

fn test_controller_authorization(shared: &SharedRunner) -> ControllerStreamAuthorization {
    let mut state = shared.state.lock().unwrap();
    upsert_connection(
        &mut state,
        ConnectionUpsert {
            connection_id: "conn_replay".to_string(),
            connection_capability: None,
            connection_capability_required: true,
            role: ConnectionRole::Controller,
            client_protocol_version: None,
            client_info: None,
            capabilities: None,
            opt_out_notifications: vec![],
            take_control: false,
        },
    )
    .unwrap();
    let connection_capability = state
        .connections
        .get("conn_replay")
        .unwrap()
        .connection_capability
        .clone();
    state.subscriptions.insert(
        "sub_replay".to_string(),
        SubscriptionRecord {
            connection_id: "conn_replay".to_string(),
            connection_capability,
            authority_mode: ConnectionAuthorityMode::Capability,
            role: ConnectionRole::Controller,
            attached: true,
        },
    );
    ControllerStreamAuthorization {
        connection_id: "conn_replay".to_string(),
        subscription_id: "sub_replay".to_string(),
        cancellation: state.controller_stream_cancellation.clone(),
    }
}

#[tokio::test]
async fn revoked_controller_finishes_an_idle_sse_without_waiting_for_an_event() {
    let workspace = tempfile::tempdir().unwrap();
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let authorization = test_controller_authorization(&shared);
    let (sender, receiver) = broadcast::channel(4);
    let (mut server, mut client) = tokio::io::duplex(4096);
    let writer_shared = shared.clone();
    let writer = tokio::spawn(async move {
        write_sse_stream(
            &mut server,
            Vec::new(),
            receiver,
            Box::new(writer_shared),
            Box::new(TranscriptStreamFilter::new(
                crate::transcript::TranscriptGrade::default(),
                0,
            )),
            Some(authorization),
        )
        .await
        .unwrap();
    });

    let mut headers = vec![0; 256];
    let read = client.read(&mut headers).await.unwrap();
    assert!(String::from_utf8_lossy(&headers[..read]).contains("text/event-stream"));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), client.read(&mut headers))
            .await
            .is_err(),
        "an authorized idle stream must remain open"
    );
    {
        let mut state = shared.state.lock().unwrap();
        revoke_controller_streams(&mut state);
    }
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut rest))
        .await
        .expect("revocation must close the idle SSE response")
        .unwrap();
    writer.await.unwrap();
    drop(sender);
}

#[tokio::test]
async fn revoked_controller_closes_a_backpressured_replay() {
    let workspace = tempfile::tempdir().unwrap();
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let authorization = test_controller_authorization(&shared);
    let (sender, receiver) = broadcast::channel(4);
    let (mut server, mut client) = tokio::io::duplex(1024);
    let marker = "unwritten-final-marker";
    let replay = vec![StreamEnvelope::Message {
        cursor: 1,
        message: Box::new(FromAgentMessage::ClientToolRequest {
            call_id: "backpressured-replay".to_string(),
            tool_execution_id: None,
            tool: "bash".to_string(),
            args: json!({"padding": format!("{}{}", "x".repeat(64 * 1024), marker)}),
        }),
    }];
    let writer_shared = shared.clone();
    let writer = tokio::spawn(async move {
        write_sse_stream(
            &mut server,
            replay,
            receiver,
            Box::new(writer_shared),
            Box::new(TranscriptStreamFilter::new(
                crate::transcript::TranscriptGrade::Delta,
                0,
            )),
            Some(authorization),
        )
        .await
    });

    let mut first = vec![0; 1024];
    let read = client.read(&mut first).await.unwrap();
    assert!(String::from_utf8_lossy(&first[..read]).contains("text/event-stream"));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !writer.is_finished(),
        "large replay should still be blocked"
    );
    {
        let mut state = shared.state.lock().unwrap();
        revoke_controller_streams(&mut state);
    }
    tokio::time::timeout(Duration::from_secs(2), writer)
        .await
        .expect("revoked backpressured replay must stop")
        .unwrap()
        .unwrap();
    let mut rest = Vec::new();
    client.read_to_end(&mut rest).await.unwrap();
    first.truncate(read);
    first.extend(rest);
    assert!(!String::from_utf8_lossy(&first).contains(marker));
    drop(sender);
}

#[tokio::test]
async fn tls_sse_close_sends_close_notify() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let (server_io, client_io) = tokio::io::duplex(2048);
    let exchange = async {
        let (server, client) = tokio::join!(
            acceptor.accept(server_io),
            connector.connect("localhost".try_into().unwrap(), client_io)
        );
        let mut server = server.unwrap();
        let mut client = client.unwrap();
        let send = async {
            write_sse_headers(&mut server).await.unwrap();
            close_sse_stream(&mut server).await.unwrap();
            drop(server);
        };
        let receive = async {
            let mut received = Vec::new();
            client
                .read_to_end(&mut received)
                .await
                .expect("SSE response must close TLS with close_notify");
            assert!(String::from_utf8_lossy(&received).contains("text/event-stream"));
        };
        tokio::join!(send, receive);
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("TLS SSE close must complete");
}

#[tokio::test(start_paused = true)]
async fn sse_close_bounds_a_stalled_flush() {
    struct StalledFlush {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl AsyncWrite for StalledFlush {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            self.bytes.extend_from_slice(bytes);
            std::task::Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            self.flushes += 1;
            if self.flushes == 1 {
                std::task::Poll::Ready(Ok(()))
            } else {
                std::task::Poll::Pending
            }
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            panic!("shutdown must not begin before a stalled flush completes")
        }
    }

    let mut socket = StalledFlush {
        bytes: Vec::new(),
        flushes: 0,
    };
    write_sse_headers(&mut socket).await.unwrap();
    let error = close_sse_stream(&mut socket)
        .await
        .expect_err("stalled flush must not pin an SSE task");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(socket.bytes.ends_with(b"\r\n\r\n"));
}

#[tokio::test(start_paused = true)]
async fn response_close_has_a_deadline_after_the_frame_is_flushed() {
    struct StalledClose(Vec<u8>);

    impl AsyncWrite for StalledClose {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            self.0.extend_from_slice(bytes);
            std::task::Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    let mut socket = StalledClose(Vec::new());
    let error = write_response(&mut socket, 200, "application/json", b"{}")
        .await
        .expect_err("a peer that stops reading must not pin the response task");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(socket.0.ends_with(b"\r\n\r\n{}"));
}
