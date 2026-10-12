//! Shared named-peer message submission used by the CLI and TUI handoff path.

use anyhow::Result;
use serde_json::{Map, Value, json};

use super::client::{A2AServiceConfig, A2ATask, SendMessageInput, send_message, wait_for_task};
use super::ledger::{
    PeerMessageIntentInput, RecordTaskStartInput, record_peer_message_intent, record_task_start,
    update_task_in_ledger,
};
use super::registry::{ResolvePeerOptions, resolve_peer};

pub const DEFAULT_PEER_MESSAGE_WAIT_MS: u64 = 300_000;
pub const DEFAULT_PEER_MESSAGE_INTERVAL_MS: u64 = 5_000;

#[derive(Debug, Clone)]
pub struct PeerMessageInput {
    pub peer: Option<String>,
    pub text: String,
    pub request_kind: String,
    pub ledger_kind: String,
    pub metadata: Map<String, Value>,
    pub registry_path: Option<String>,
    pub tasks_path: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PendingPeerMessage {
    pub peer: String,
    pub config: A2AServiceConfig,
    pub task: A2ATask,
    pub tasks_path: Option<String>,
    pub ledger_warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompletedPeerMessage {
    pub task: A2ATask,
    pub ledger_warning: Option<String>,
}

/// A reference to immutable bytes already owned by hosted Computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputerHandoffPackageReference {
    pub package_id: String,
    pub package_digest: String,
    pub target_thread_id: String,
}

pub async fn start_peer_message(input: PeerMessageInput) -> Result<PendingPeerMessage> {
    let peer = resolve_peer(
        input.peer.as_deref(),
        ResolvePeerOptions {
            registry_path: input.registry_path,
            timeout_ms: input.timeout_ms,
            token: None,
            max_attempts: None,
        },
    )?;
    let mut metadata = input.metadata;
    metadata.insert("requestKind".into(), json!(input.request_kind));
    metadata.insert("relayPeer".into(), json!(peer.name));
    let proposed_message_id = format!("maestro-a2a-message-{}", uuid::Uuid::new_v4());
    let proposed_context_id = peer_context_id(&peer.config);
    let intent = record_peer_message_intent(PeerMessageIntentInput {
        path: input.tasks_path.as_deref(),
        peer: &peer.name,
        peer_display_name: peer.entry.display_name.as_deref(),
        text: &input.text,
        message_id: &proposed_message_id,
        context_id: &proposed_context_id,
        kind: &input.ledger_kind,
        metadata: Value::Object(metadata.clone()),
    })?;
    let message_id = intent.message_id;
    let context_id = intent.context_id;

    let sent = send_message(
        &peer.config,
        SendMessageInput {
            text: input.text.clone(),
            message_id: message_id.clone(),
            context_id: Some(context_id.clone()),
            task_id: None,
            metadata: Some(Value::Object(metadata.clone())),
            return_immediately: true,
        },
    )
    .await?;

    let ledger_warning = record_task_start(RecordTaskStartInput {
        path: input.tasks_path.as_deref(),
        peer: &peer.name,
        peer_display_name: peer.entry.display_name.as_deref(),
        task: &sent.task,
        text: &input.text,
        message_id: Some(&message_id),
        context_id: Some(&context_id),
        kind: &input.ledger_kind,
        metadata: Some(Value::Object(metadata)),
    })
    .err()
    .map(|error| format!("could not record sent task locally: {error:#}"));

    Ok(PendingPeerMessage {
        peer: peer.name,
        config: peer.config,
        task: sent.task,
        tasks_path: input.tasks_path,
        ledger_warning,
    })
}

pub async fn wait_for_peer_message(
    pending: &PendingPeerMessage,
    max_wait_ms: u64,
    interval_ms: u64,
) -> Result<CompletedPeerMessage> {
    let task = wait_for_task(&pending.config, &pending.task.id, max_wait_ms, interval_ms).await?;
    let ledger_warning = update_task_in_ledger(pending.tasks_path.as_deref(), &pending.peer, &task)
        .err()
        .map(|error| format!("could not sync sent task result locally: {error:#}"));
    Ok(CompletedPeerMessage {
        task,
        ledger_warning,
    })
}

pub async fn start_handoff(
    peer: Option<String>,
    text: String,
    package: Option<&ComputerHandoffPackageReference>,
) -> Result<PendingPeerMessage> {
    let (text, metadata) = prepare_handoff(text, package);
    start_peer_message(PeerMessageInput {
        peer,
        text,
        request_kind: "maestro-peer-handoff".into(),
        ledger_kind: "handoff".into(),
        metadata,
        registry_path: None,
        tasks_path: None,
        timeout_ms: None,
    })
    .await
}

fn prepare_handoff(
    text: String,
    package: Option<&ComputerHandoffPackageReference>,
) -> (String, Map<String, Value>) {
    let mut metadata = Map::new();
    let text = if let Some(package) = package {
        metadata.insert(
            "computerHandoffPackage".into(),
            json!({
                "schemaVersion": "evalops.maestro.computer-handoff-reference.v1",
                "packageId": package.package_id,
                "packageDigest": package.package_digest,
                "targetThreadId": package.target_thread_id,
            }),
        );
        append_computer_handoff_instruction(&text, package)
    } else {
        text
    };
    (text, metadata)
}

fn append_computer_handoff_instruction(
    text: &str,
    package: &ComputerHandoffPackageReference,
) -> String {
    format!(
        "{text}\n\nMaestro attached immutable context from hosted Computer. Before acting, read package `{}` for target thread `{}` through this machine's managed Computer connection (`maestro computer handoff read {} {} --json`) and confirm its manifest digest is `{}`.",
        package.package_id,
        package.target_thread_id,
        package.target_thread_id,
        package.package_id,
        package.package_digest,
    )
}

pub(super) fn peer_context_id(config: &A2AServiceConfig) -> String {
    config
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("maestro-a2a-context-{}", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::a2a_cli::{extract_task_text, load_task_ledger};

    #[test]
    fn computer_package_instruction_and_metadata_are_explicit() {
        let package = ComputerHandoffPackageReference {
            package_id: "package-1".into(),
            package_digest: "digest-1".into(),
            target_thread_id: "thread-2".into(),
        };
        let (prompt, metadata) = prepare_handoff("continue".into(), Some(&package));
        assert!(prompt.starts_with("continue\n\n"));
        assert!(prompt.contains("maestro computer handoff read thread-2 package-1 --json"));
        assert!(prompt.contains("digest-1"));
        assert_eq!(
            metadata["computerHandoffPackage"],
            json!({
                "schemaVersion": "evalops.maestro.computer-handoff-reference.v1",
                "packageId": "package-1",
                "packageDigest": "digest-1",
                "targetThreadId": "thread-2",
            })
        );
    }

    #[tokio::test]
    async fn peer_message_submits_waits_and_updates_the_shared_ledger() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let temp = tempdir().unwrap();
        let registry_path = temp.path().join("peers.json");
        let tasks_path = temp.path().join("tasks.json");
        let server_tasks_path = tasks_path.clone();
        let server = tokio::spawn(async move {
            let (mut send_stream, _) = listener.accept().await.unwrap();
            let send_request = read_request(&mut send_stream).await;
            assert!(send_request.starts_with("POST /message:send "));
            let send_body = request_json_body(&send_request);
            assert_eq!(send_body["message"]["contextId"], "chief-session");
            assert_eq!(
                send_body["message"]["metadata"]["requestKind"],
                "maestro-peer-handoff"
            );
            let intent = load_task_ledger(Some(server_tasks_path.to_str().unwrap())).unwrap();
            assert_eq!(intent.tasks.len(), 1);
            assert_eq!(intent.tasks[0].state, "LOCAL_DISPATCH_INTENT");
            assert_eq!(
                intent.tasks[0].message_id.as_deref(),
                send_body["message"]["messageId"].as_str()
            );
            write_json_response(
                &mut send_stream,
                &json!({
                    "task": {
                        "id": "task-1",
                        "contextId": "chief-session",
                        "status": {"state": "SUBMITTED"}
                    }
                }),
            )
            .await;

            let (mut task_stream, _) = listener.accept().await.unwrap();
            let task_request = read_request(&mut task_stream).await;
            assert!(task_request.starts_with("GET /tasks/task-1 "));
            write_json_response(
                &mut task_stream,
                &json!({
                    "id": "task-1",
                    "contextId": "chief-session",
                    "status": {
                        "state": "COMPLETED",
                        "message": {
                            "role": "ROLE_AGENT",
                            "parts": [{"text": "release queue is clear"}]
                        }
                    }
                }),
            )
            .await;
        });

        std::fs::write(
            &registry_path,
            serde_json::to_vec(&json!({
                "defaultPeer": "chief",
                "peers": {
                    "chief": {
                        "url": base_url,
                        "displayName": "Chief",
                        "sessionId": "chief-session",
                        "timeoutMs": 2_000
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let pending = start_peer_message(PeerMessageInput {
            peer: None,
            text: "check the release queue".into(),
            request_kind: "maestro-peer-handoff".into(),
            ledger_kind: "handoff".into(),
            metadata: Map::new(),
            registry_path: Some(registry_path.display().to_string()),
            tasks_path: Some(tasks_path.display().to_string()),
            timeout_ms: Some(2_000),
        })
        .await
        .unwrap();
        assert_eq!(pending.task.id, "task-1");
        assert!(pending.ledger_warning.is_none());

        let completed = wait_for_peer_message(&pending, 2_000, 100).await.unwrap();
        assert_eq!(
            extract_task_text(&completed.task).as_deref(),
            Some("release queue is clear")
        );
        assert!(completed.ledger_warning.is_none());
        server.await.unwrap();

        let ledger = load_task_ledger(Some(tasks_path.to_str().unwrap())).unwrap();
        assert_eq!(ledger.tasks.len(), 1);
        assert_eq!(ledger.tasks[0].kind, "handoff");
        assert_eq!(ledger.tasks[0].peer, "chief");
        assert_eq!(ledger.tasks[0].state, "COMPLETED");
        assert_eq!(
            ledger.tasks[0].response_text.as_deref(),
            Some("release queue is clear")
        );
    }

    #[tokio::test]
    async fn identical_peer_messages_use_distinct_dispatch_identities() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut first_stream, _) = listener.accept().await.unwrap();
            let first_request = read_request(&mut first_stream).await;
            let first_body = request_json_body(&first_request);
            write_json_response(&mut first_stream, &json!({})).await;

            let (mut retry_stream, _) = listener.accept().await.unwrap();
            let retry_request = read_request(&mut retry_stream).await;
            let retry_body = request_json_body(&retry_request);
            assert_ne!(
                retry_body["message"]["messageId"],
                first_body["message"]["messageId"]
            );
            assert_eq!(
                retry_body["message"]["contextId"],
                first_body["message"]["contextId"]
            );
            write_json_response(
                &mut retry_stream,
                &json!({
                    "task": {
                        "id": "task-recovered",
                        "contextId": "chief-session",
                        "status": {"state": "SUBMITTED"}
                    }
                }),
            )
            .await;
        });

        let temp = tempdir().unwrap();
        let registry_path = temp.path().join("peers.json");
        let tasks_path = temp.path().join("tasks.json");
        std::fs::write(
            &registry_path,
            serde_json::to_vec(&json!({
                "defaultPeer": "chief",
                "maxAttempts": 1,
                "peers": {
                    "chief": {
                        "url": base_url,
                        "sessionId": "chief-session",
                        "timeoutMs": 2_000
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let input = PeerMessageInput {
            peer: None,
            text: "reconcile this handoff".into(),
            request_kind: "maestro-peer-handoff".into(),
            ledger_kind: "handoff".into(),
            metadata: Map::new(),
            registry_path: Some(registry_path.display().to_string()),
            tasks_path: Some(tasks_path.display().to_string()),
            timeout_ms: Some(2_000),
        };

        start_peer_message(input.clone())
            .await
            .expect_err("the accepted response is missing its task");
        let pending = start_peer_message(input).await.unwrap();
        assert_eq!(pending.task.id, "task-recovered");
        server.await.unwrap();

        let ledger = load_task_ledger(Some(tasks_path.to_str().unwrap())).unwrap();
        assert_eq!(ledger.tasks.len(), 2);
        let unresolved = ledger
            .tasks
            .iter()
            .find(|entry| entry.state == "LOCAL_DISPATCH_INTENT")
            .expect("the first failed send should retain its own intent");
        let submitted = ledger
            .tasks
            .iter()
            .find(|entry| entry.task_id == "task-recovered")
            .expect("the second send should own the accepted task");
        assert_eq!(submitted.state, "SUBMITTED");
        assert_ne!(unresolved.message_id, submitted.message_id);
    }

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..count]);
            let Some(header_end) = find_header_end(&bytes) else {
                continue;
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }

    fn find_header_end(bytes: &[u8]) -> Option<usize> {
        bytes.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn request_json_body(request: &str) -> Value {
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        serde_json::from_str(body).unwrap()
    }

    async fn write_json_response(stream: &mut TcpStream, body: &Value) {
        let body = serde_json::to_vec(body).unwrap();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
        stream.shutdown().await.unwrap();
    }
}
