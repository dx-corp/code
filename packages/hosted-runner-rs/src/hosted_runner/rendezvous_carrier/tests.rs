use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::headless::{AsyncFrameReader, AsyncFrameWriter};
use crate::hosted_runner::rendezvous_protocol::{
    CommandAuthority, HostToRunnerFrame, MAX_IN_FLIGHT_REQUESTS, RendezvousAccepted,
    RendezvousIdentity, RendezvousLifecycle, RendezvousMode, RendezvousNonce, RendezvousRequest,
    RunnerToHostFrame,
};

use super::{
    ACTIVATION_CAS_DURATION_METRIC, CONNECT_DURATION_METRIC, QUEUE_COUNTER_METRIC,
    RECONNECT_COUNTER_METRIC, RendezvousCarrier, RendezvousCarrierConfig, RendezvousMetricSink,
    RendezvousQueueError, bounded_rendezvous_queue,
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Observation {
    metric: &'static str,
    phase: Option<&'static str>,
    outcome: &'static str,
}

#[derive(Default)]
struct RecordingMetrics(Mutex<Vec<Observation>>);

impl RendezvousMetricSink for RecordingMetrics {
    fn observe_connect_duration(
        &self,
        phase: &'static str,
        outcome: &'static str,
        _duration: Duration,
    ) {
        self.0.lock().unwrap().push(Observation {
            metric: CONNECT_DURATION_METRIC,
            phase: Some(phase),
            outcome,
        });
    }

    fn observe_activation_cas_duration(&self, outcome: &'static str, _duration: Duration) {
        self.0.lock().unwrap().push(Observation {
            metric: ACTIVATION_CAS_DURATION_METRIC,
            phase: None,
            outcome,
        });
    }

    fn increment_counter(&self, metric: &'static str, outcome: &'static str) {
        self.0.lock().unwrap().push(Observation {
            metric,
            phase: None,
            outcome,
        });
    }
}

fn lifecycle(mode: RendezvousMode) -> RendezvousLifecycle {
    let mut lifecycle = RendezvousLifecycle::new(
        mode,
        RendezvousIdentity {
            organization_id: "org-123".into(),
            workspace_id: "workspace-123".into(),
            sandbox_id: Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap(),
            placement_generation: 7,
            runner_session_id: "runner-session-1".into(),
        },
        RendezvousNonce::parse("proof-0123456789abcdef0123456789abcdef").unwrap(),
    );
    lifecycle.set_runtime_ready(true);
    lifecycle
}

fn mtls_configs() -> (Arc<rustls::ClientConfig>, Arc<rustls::ServerConfig>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let ca_certificate = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let mut server_params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let server_certificate = server_params.signed_by(&server_key, &issuer).unwrap();

    let mut client_params = CertificateParams::default();
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let client_certificate = client_params.signed_by(&client_key, &issuer).unwrap();

    let ca_der = CertificateDer::from(ca_certificate.der().to_vec());
    let mut server_roots = rustls::RootCertStore::empty();
    server_roots.add(ca_der.clone()).unwrap();
    let client_verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(server_roots),
        provider.clone(),
    )
    .build()
    .unwrap();
    let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(
            vec![
                CertificateDer::from(server_certificate.der().to_vec()),
                ca_der.clone(),
            ],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
        )
        .unwrap();

    let mut client_roots = rustls::RootCertStore::empty();
    client_roots.add(ca_der.clone()).unwrap();
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(client_roots)
        .with_client_auth_cert(
            vec![
                CertificateDer::from(client_certificate.der().to_vec()),
                ca_der,
            ],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client_key.serialize_der())),
        )
        .unwrap();
    (Arc::new(client_config), Arc::new(server_config))
}

#[tokio::test]
async fn mtls_carrier_opens_accepts_and_receives_first_request() {
    let (client_tls, server_tls) = mtls_configs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let activation_id = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let tls = TlsAcceptor::from(server_tls).accept(socket).await.unwrap();
        let (read, write) = tokio::io::split(tls);
        let mut reader = AsyncFrameReader::new(read);
        let mut writer = AsyncFrameWriter::new(write);
        let open: RunnerToHostFrame = reader.read_message().await.unwrap();
        let RunnerToHostFrame::Open(open) = open else {
            panic!("first frame was not open");
        };
        assert_eq!(open.activation_id, activation_id);
        writer
            .write_message(&HostToRunnerFrame::Accepted(RendezvousAccepted {
                activation_id,
                outbound_commands_enabled: true,
                replay_from_sequence: 1,
            }))
            .await
            .unwrap();
        writer
            .write_message(&HostToRunnerFrame::Request(RendezvousRequest {
                activation_id,
                sequence: 1,
                idempotency_key: "command-1".into(),
                payload: serde_json::json!({"method": "ping"}),
            }))
            .await
            .unwrap();
    });
    let metrics = Arc::new(RecordingMetrics::default());
    let carrier = RendezvousCarrier::new(
        RendezvousCarrierConfig {
            endpoint,
            server_name: ServerName::try_from("localhost").unwrap().to_owned(),
            tls_config: client_tls,
            identity_cancellation: CancellationToken::new(),
            phase_timeout: Duration::from_secs(2),
        },
        metrics.clone(),
    );
    let mut lifecycle = lifecycle(RendezvousMode::Outbound);

    let mut connection = carrier
        .connect(&mut lifecycle, activation_id)
        .await
        .expect("mTLS rendezvous acceptance");
    let frame = connection.recv().await.expect("first logical request");

    assert!(matches!(frame, HostToRunnerFrame::Request(request) if request.sequence == 1));
    assert_eq!(
        lifecycle.command_authority(),
        Some(CommandAuthority::Outbound)
    );
    assert_eq!(
        &metrics.0.lock().unwrap()[..4],
        ["tcp", "tls", "open", "accepted"].map(|phase| Observation {
            metric: CONNECT_DURATION_METRIC,
            phase: Some(phase),
            outcome: "success",
        })
    );
    assert_eq!(
        metrics.0.lock().unwrap()[4],
        Observation {
            metric: ACTIVATION_CAS_DURATION_METRIC,
            phase: None,
            outcome: "success",
        }
    );
    server.await.unwrap();
}

#[tokio::test]
async fn legacy_shadow_acceptance_keeps_inbound_authoritative() {
    let (client_tls, server_tls) = mtls_configs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let activation_id = Uuid::new_v4();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let tls = TlsAcceptor::from(server_tls).accept(socket).await.unwrap();
        let (read, write) = tokio::io::split(tls);
        let mut reader = AsyncFrameReader::new(read);
        let mut writer = AsyncFrameWriter::new(write);
        let _: RunnerToHostFrame = reader.read_message().await.unwrap();
        writer
            .write_message(&serde_json::json!({
                "type": "accepted",
                "activation_id": activation_id,
                "replay_from_sequence": 1,
                "n_minus_one_extension": "ignored"
            }))
            .await
            .unwrap();
    });
    let metrics = Arc::new(RecordingMetrics::default());
    let carrier = RendezvousCarrier::new(
        RendezvousCarrierConfig {
            endpoint,
            server_name: ServerName::try_from("localhost").unwrap().to_owned(),
            tls_config: client_tls,
            identity_cancellation: CancellationToken::new(),
            phase_timeout: Duration::from_secs(2),
        },
        metrics,
    );
    let mut lifecycle = lifecycle(RendezvousMode::OutboundShadow);

    carrier
        .connect(&mut lifecycle, activation_id)
        .await
        .expect("N-1 shadow acceptance");

    assert_eq!(
        lifecycle.command_authority(),
        Some(CommandAuthority::Inbound)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn inbound_mode_never_dials_the_outbound_carrier() {
    let (client_tls, _) = mtls_configs();
    let metrics = Arc::new(RecordingMetrics::default());
    let carrier = RendezvousCarrier::new(
        RendezvousCarrierConfig {
            endpoint: "127.0.0.1:9".parse().unwrap(),
            server_name: ServerName::try_from("localhost").unwrap().to_owned(),
            tls_config: client_tls,
            identity_cancellation: CancellationToken::new(),
            phase_timeout: Duration::from_millis(20),
        },
        metrics.clone(),
    );
    let mut lifecycle = lifecycle(RendezvousMode::Inbound);

    let error = carrier
        .connect(&mut lifecycle, Uuid::new_v4())
        .await
        .err()
        .expect("inbound mode must reject before TCP");

    assert!(matches!(
        error,
        super::RendezvousCarrierError::Lifecycle(
            crate::hosted_runner::rendezvous_protocol::RendezvousLifecycleError::OutboundDisabled
        )
    ));
    assert!(metrics.0.lock().unwrap().is_empty());
    assert_eq!(
        lifecycle.command_authority(),
        Some(CommandAuthority::Inbound)
    );
}

#[tokio::test]
async fn legacy_acceptance_cannot_grant_outbound_authority() {
    let (client_tls, server_tls) = mtls_configs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let activation_id = Uuid::new_v4();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let tls = TlsAcceptor::from(server_tls).accept(socket).await.unwrap();
        let (read, write) = tokio::io::split(tls);
        let mut reader = AsyncFrameReader::new(read);
        let mut writer = AsyncFrameWriter::new(write);
        let _: RunnerToHostFrame = reader.read_message().await.unwrap();
        writer
            .write_message(&serde_json::json!({
                "type": "accepted",
                "activation_id": activation_id,
                "replay_from_sequence": 1
            }))
            .await
            .unwrap();
    });
    let metrics = Arc::new(RecordingMetrics::default());
    let carrier = RendezvousCarrier::new(
        RendezvousCarrierConfig {
            endpoint,
            server_name: ServerName::try_from("localhost").unwrap().to_owned(),
            tls_config: client_tls,
            identity_cancellation: CancellationToken::new(),
            phase_timeout: Duration::from_secs(2),
        },
        metrics,
    );
    let mut lifecycle = lifecycle(RendezvousMode::Outbound);

    carrier
        .connect(&mut lifecycle, activation_id)
        .await
        .err()
        .expect("N-1 acceptance lacks an outbound authority grant");

    assert_eq!(lifecycle.command_authority(), None);
    assert_eq!(lifecycle.active_activation_id(), None);
    server.await.unwrap();
}

#[tokio::test]
async fn reconnect_rotates_activation_and_resumes_after_applied_sequence() {
    let (client_tls, server_tls) = mtls_configs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let first_activation = Uuid::new_v4();
    let second_activation = Uuid::new_v4();
    let server = tokio::spawn(async move {
        let acceptor = TlsAcceptor::from(server_tls);
        let mut opens = Vec::new();
        for (activation_id, replay_from_sequence) in
            [(first_activation, 1_u64), (second_activation, 2_u64)]
        {
            let (socket, _) = listener.accept().await.unwrap();
            let tls = acceptor.accept(socket).await.unwrap();
            let (read, write) = tokio::io::split(tls);
            let mut reader = AsyncFrameReader::new(read);
            let mut writer = AsyncFrameWriter::new(write);
            let open: RunnerToHostFrame = reader.read_message().await.unwrap();
            let RunnerToHostFrame::Open(open) = open else {
                panic!("first frame was not open");
            };
            opens.push(open);
            writer
                .write_message(&HostToRunnerFrame::Accepted(RendezvousAccepted {
                    activation_id,
                    outbound_commands_enabled: true,
                    replay_from_sequence,
                }))
                .await
                .unwrap();
            tokio::spawn(async move {
                let _connection = (reader, writer);
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
        }
        opens
    });
    let metrics = Arc::new(RecordingMetrics::default());
    let carrier = RendezvousCarrier::new(
        RendezvousCarrierConfig {
            endpoint,
            server_name: ServerName::try_from("localhost").unwrap().to_owned(),
            tls_config: client_tls,
            identity_cancellation: CancellationToken::new(),
            phase_timeout: Duration::from_secs(2),
        },
        metrics.clone(),
    );
    let mut lifecycle = lifecycle(RendezvousMode::Outbound);
    let mut first = carrier
        .connect(&mut lifecycle, first_activation)
        .await
        .unwrap();
    lifecycle.record_applied_request(1, "command-1").unwrap();
    let mut second = carrier
        .connect(&mut lifecycle, second_activation)
        .await
        .expect("rotated reconnect");

    let opens = server.await.unwrap();
    assert_eq!(opens[0].resume_after_sequence, None);
    assert_eq!(opens[1].resume_after_sequence, Some(1));
    assert_eq!(lifecycle.active_activation_id(), Some(second_activation));
    let old_connection = tokio::time::timeout(Duration::from_millis(100), first.recv()).await;
    assert!(
        matches!(
            old_connection,
            Ok(Err(super::RendezvousCarrierError::ActivationRevoked))
        ),
        "rotation must revoke the previous carrier immediately"
    );
    {
        let observations = metrics.0.lock().unwrap();
        assert!(observations.contains(&Observation {
            metric: RECONNECT_COUNTER_METRIC,
            phase: None,
            outcome: "attempt",
        }));
        assert!(observations.contains(&Observation {
            metric: RECONNECT_COUNTER_METRIC,
            phase: None,
            outcome: "success",
        }));
    }

    assert!(lifecycle.set_runtime_ready(false).is_some());
    carrier.revoke_active();
    assert!(matches!(
        second.recv().await,
        Err(super::RendezvousCarrierError::ActivationRevoked)
    ));
    assert_eq!(lifecycle.command_authority(), None);
}

#[tokio::test]
async fn queue_is_bounded_and_counts_enqueue_dequeue_and_drop() {
    let metrics = Arc::new(RecordingMetrics::default());
    let (sender, mut receiver) = bounded_rendezvous_queue(metrics.clone());
    for sequence in 1..=u64::from(MAX_IN_FLIGHT_REQUESTS) {
        sender.try_send(sequence).unwrap();
    }

    assert_eq!(sender.try_send(129), Err(RendezvousQueueError::Full));
    assert_eq!(receiver.recv().await, Some(1));

    let observations = metrics.0.lock().unwrap();
    assert_eq!(
        observations
            .iter()
            .filter(|observation| observation.metric == QUEUE_COUNTER_METRIC
                && observation.outcome == "enqueued")
            .count(),
        usize::from(MAX_IN_FLIGHT_REQUESTS)
    );
    assert!(observations.contains(&Observation {
        metric: QUEUE_COUNTER_METRIC,
        phase: None,
        outcome: "dropped_full",
    }));
    assert!(observations.contains(&Observation {
        metric: QUEUE_COUNTER_METRIC,
        phase: None,
        outcome: "dequeued",
    }));
}
