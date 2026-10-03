use super::{FromAgent, NativeAgent, NativeAgentConfig};
use crate::state::ApprovalMode;
use serde_json::{Value, json};
use std::time::Duration;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6G9sAAAAASUVORK5CYII=";

#[test]
fn codemode_images_reach_app_server_and_semantic_checkpoint() {
    if std::env::var("MAESTRO_CODEMODE_IMAGE_FIXTURE").as_deref() == Ok("1") {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            crate::credential_mode::install_test_identity_env();
            let workspace = std::env::var("MAESTRO_CODEMODE_IMAGE_WORKSPACE").unwrap();
            let config = NativeAgentConfig {
                model: "openai-codex/gpt-5.5".into(),
                cwd: workspace,
                approval_mode: ApprovalMode::Yolo,
                ..Default::default()
            };
            let (agent, mut events) = NativeAgent::new(config).unwrap();
            agent.prompt("Emit the image".into(), vec![]).await.unwrap();
            let mut snapshots = Vec::new();
            tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(event) = events.recv().await {
                    match event {
                        FromAgent::ConversationSnapshot { messages, .. } => {
                            snapshots.push(messages);
                        }
                        FromAgent::TurnCompleted { .. } => break,
                        FromAgent::Error {
                            message,
                            fatal: true,
                            ..
                        } => panic!("{message}"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("app-server script completes on the default test stack");
            agent.shutdown().await;
            assert!(
                snapshots
                    .iter()
                    .any(|snapshot| serde_json::to_string(snapshot).unwrap().contains(PNG)),
                "explicit image must reach semantic checkpoint: {snapshots:?}"
            );
        });
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("server.js");
    let reply = root.path().join("reply.json");
    let catalog = root.path().join("catalog.json");
    let code = format!("image({{mime_type:'image/png',data:'{PNG}'}}); text('one pixel');");
    let js = format!(
        r"const fs=require('fs'); const rl=require('readline').createInterface({{input:process.stdin}});
function send(x){{process.stdout.write(JSON.stringify(x)+'\n')}}
rl.on('line',line=>{{const x=JSON.parse(line);
if(!x.method){{if(x.id==='image-rpc'){{fs.writeFileSync({reply},JSON.stringify(x.result));send({{method:'item/agentMessage/delta',params:{{turnId:'turn',delta:'Done'}}}});send({{method:'turn/completed',params:{{turnId:'turn'}}}})}}return}}
if(x.method==='initialize')send({{id:x.id,result:{{protocolVersion:'2025-01-01',capabilities:{{}}}}}});
else if(x.method==='model/list')send({{id:x.id,result:{{data:[{{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{{reasoningEffort:'medium'}}]}}],nextCursor:null}}}});
else if(x.method==='thread/start'){{fs.writeFileSync({catalog},JSON.stringify(x.params));send({{id:x.id,result:{{thread:{{id:'thread'}}}}}})}}
else if(x.method==='turn/start'){{send({{id:x.id,result:{{turn:{{id:'turn'}}}}}});setTimeout(()=>send({{id:'image-rpc',method:'item/tool/call',params:{{threadId:'thread',turnId:'turn',tool:'codemode',callId:'owned-image',arguments:{{code:{code}}}}}}}),10)}}
else if(x.id!==undefined)send({{id:x.id,result:{{}}}});
}});",
        reply = json!(reply.display().to_string()),
        catalog = json!(catalog.display().to_string()),
        code = json!(code)
    );
    std::fs::write(&script, js).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(
            "agent::codemode_codex_tests::codemode_images_reach_app_server_and_semantic_checkpoint",
        )
        .arg("--exact")
        .arg("--nocapture")
        .env("MAESTRO_CODEMODE_IMAGE_FIXTURE", "1")
        .env("MAESTRO_CODEMODE_IMAGE_WORKSPACE", root.path())
        .env("MAESTRO_HOME", root.path().join("maestro-home"))
        .env("MAESTRO_OAUTH_STORAGE_MODE", "file")
        .env("MAESTRO_DISABLE_KEYCHAIN", "1")
        .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
        .env("OPENAI_CODEX_TOKEN", "fixture-token")
        .env(
            "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
            json!([script.display().to_string()]).to_string(),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: Value = serde_json::from_slice(&std::fs::read(reply).unwrap()).unwrap();
    assert_eq!(reply["success"], true);
    assert_eq!(reply["contentItems"][1]["type"], "inputImage");
    assert_eq!(
        reply["contentItems"][1]["imageUrl"],
        format!("data:image/png;base64,{PNG}")
    );
    assert!(
        !reply["contentItems"][0]["text"]
            .as_str()
            .unwrap()
            .contains(PNG)
    );
    let catalog: Value = serde_json::from_slice(&std::fs::read(catalog).unwrap()).unwrap();
    assert!(
        catalog["dynamicTools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "codemode"
                && tool["description"].as_str().unwrap().contains("Promise<"))
    );
}
