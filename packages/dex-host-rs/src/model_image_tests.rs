use super::*;
use dex_loop::{
    CallId, Cursor, Event, Model as _, OutputBlock, PrincipalId, ProposedCall, ThreadId, TurnId,
};
use futures_util::StreamExt;
use std::io::{Read, Write};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6G9sAAAAASUVORK5CYII=";

fn context(output: Output) -> Context {
    let calls = vec![
        ProposedCall::new(
            CallId::new("image-call"),
            ToolName::new("codemode"),
            serde_json::json!({}),
            PrincipalId::new("alice"),
        ),
        ProposedCall::new(
            CallId::new("peer-call"),
            ToolName::new("fs.read_file"),
            serde_json::json!({"path":"peer.txt"}),
            PrincipalId::new("alice"),
        ),
    ];
    dex_loop::rehydrate(
        ThreadId {
            org: "org".into(),
            workspace: "workspace".into(),
            thread: "thread".into(),
        },
        &[
            (
                Cursor(1),
                Event::UserMessage {
                    interaction_mode: dex_loop::InteractionMode::Unspecified,
                    turn: TurnId::new("turn"),
                    message_id: None,
                    model_binding: None,
                    voice: None,
                    principal: PrincipalId::new("alice"),
                    text: "Inspect the image and peer".into(),
                    attachments: vec![],
                    client_tools: vec![],
                    authorized_tools: vec![],
                    approval_mode: dex_loop::ApprovalMode::Headless,
                },
            ),
            (
                Cursor(2),
                Event::ModelStepCompleted {
                    step: 1,
                    text: String::new(),
                    calls,
                    reasoning: None,
                    served: None,
                    timing: None,
                },
            ),
            (
                Cursor(3),
                Event::ToolFinished {
                    call: CallId::new("image-call"),
                    outcome: Outcome::Succeeded,
                    output,
                    receipt: None,
                    summary: None,
                },
            ),
            (
                Cursor(4),
                Event::ToolFinished {
                    call: CallId::new("peer-call"),
                    outcome: Outcome::Succeeded,
                    output: Output::Text("peer text".into()),
                    receipt: None,
                    summary: None,
                },
            ),
        ],
    )
}

#[tokio::test]
async fn typed_images_follow_all_peer_tool_results_on_actual_ai_model_request() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let (body_start, body_len) = loop {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "request ended before its headers");
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..index]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                break (index + 4, length);
            }
        };
        while bytes.len() < body_start + body_len {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "request ended before its body");
            bytes.extend_from_slice(&buffer[..count]);
        }
        let request: serde_json::Value =
            serde_json::from_slice(&bytes[body_start..body_start + body_len]).unwrap();
        let body = "data: {\"id\":\"done\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        request
    });
    let client = UnifiedClient::OpenAI(
        maestro_ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1"))
            .unwrap(),
    );
    let model = AiRsModel::new(client, "openai/gpt-4o", 1024);
    let ctx = context(Output::Blocks(vec![
        OutputBlock::Text {
            text: "selected image".into(),
        },
        OutputBlock::Image {
            mime_type: "image/png".into(),
            data: PNG.into(),
        },
    ]));
    let chunks = model.stream(&ctx, &[]).collect::<Vec<_>>().await;
    assert!(chunks.iter().all(Result::is_ok), "{chunks:?}");
    let request = server.join().unwrap();
    let messages = request["messages"].as_array().unwrap();
    let image_result = messages
        .iter()
        .position(|message| message["tool_call_id"] == "image-call")
        .unwrap();
    let peer_result = messages
        .iter()
        .position(|message| message["tool_call_id"] == "peer-call")
        .unwrap();
    assert_eq!(
        peer_result,
        image_result + 1,
        "peer tool results must stay contiguous"
    );
    assert_eq!(messages[image_result]["content"], "selected image");
    assert_eq!(messages[peer_result]["content"], "peer text");
    assert_eq!(messages[peer_result + 1]["role"], "user");
    assert_eq!(messages[peer_result + 1]["content"][0]["type"], "image_url");
    assert_eq!(
        messages[peer_result + 1]["content"][0]["image_url"]["url"],
        format!("data:image/png;base64,{PNG}")
    );
    assert!(
        messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .all(|message| !message["content"].to_string().contains(PNG))
    );
}

#[tokio::test]
async fn invalid_typed_media_fails_before_ai_provider_dispatch() {
    let model = AiRsModel::new(
        UnifiedClient::Scripted(maestro_ai::ScriptedClient::new("unused", vec![])),
        "unused",
        1024,
    );
    let ctx = context(Output::Blocks(vec![OutputBlock::Image {
        mime_type: "image/png".into(),
        data: "invalid base64".into(),
    }]));
    let chunks = model.stream(&ctx, &[]).collect::<Vec<_>>().await;
    assert!(
        matches!(
            chunks.as_slice(),
            [Err(ModelError {
                class: dex_loop::ErrorClass::Protocol,
                ..
            })]
        ),
        "invalid media must fail at the projection boundary: {chunks:?}"
    );
}
