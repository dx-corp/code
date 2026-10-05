use super::{ExternalToolSchemaPolicy, FromAgent, NativeAgent, NativeAgentConfig, ToolDefinition};
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
                external_tool_schema_policy: ExternalToolSchemaPolicy::Deferred,
                ..Default::default()
            };
            let external: Vec<ToolDefinition> = (0..1000).map(|index| ToolDefinition {
                tool: maestro_ai::Tool::new(format!("client_capability_{index:04}"), "Caller-owned test capability")
                    .with_schema(json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"]})),
                requires_approval: true,
            }).collect();
            let replacement = vec![external[0].clone()];
            let (agent, mut events) = NativeAgent::new_with_tools(config, external).unwrap();
            let mut snapshots = Vec::new();
            let mut caller_executions = 0;
            for (index, prompt) in ["Emit the image", "Emit again with updated tools"].into_iter().enumerate() {
                if index == 1 {
                    agent.replace_governed_tools(
                        ["tool_search", "codemode", "ask_user", "websearch"].into_iter().map(str::to_owned).collect(),
                        replacement.clone(),
                    ).unwrap();
                }
                agent.prompt(prompt.into(), vec![]).await.unwrap();
            // This fixture now admits 1,000 tools and exercises two turns.
            // Keep protocol assertions intact while allowing shared CI load.
            tokio::time::timeout(Duration::from_secs(30), async {
                while let Some(event) = events.recv().await {
                    match event {
                        FromAgent::ToolCall { call_id, tool, .. } if tool == "client_capability_0000" => {
                            caller_executions += 1;
                            agent.tool_response_sender().send((call_id, true, Some(crate::agent::ToolResult::success("caller result")), maestro_runtime_contracts::ExecutionSource::RemoteClient, None)).unwrap();
                        }
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
            }
            agent.shutdown().await;
            assert_eq!(caller_executions, 2, "inactive direct calls must never reach the caller");
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
    let denied = root.path().join("denied.json");
    let registrations = root.path().join("registrations.jsonl");
    let history = root.path().join("history.json");
    let code = format!(
        "text(await tools.tool_search({{names:['ask_user']}})); const matches=searchTools('client_capability_0000'); text(matches); const schema=getToolSchema('client_capability_0000'); text(JSON.parse(schema.json).inputSchema.required); text(await tools.client_capability_0000({{id:'one'}})); image({{mime_type:'image/png',data:'{PNG}'}}); text('one pixel');"
    );
    let js = format!(
        r"const fs=require('fs'); const rl=require('readline').createInterface({{input:process.stdin}});
function send(x){{process.stdout.write(JSON.stringify(x)+'\n')}}
rl.on('line',line=>{{const x=JSON.parse(line);
if(!x.method){{if(x.id==='image-rpc'){{fs.writeFileSync({reply},JSON.stringify(x.result));send({{id:'denied-rpc',method:'item/tool/call',params:{{threadId:'thread',turnId:'turn',tool:'client_capability_0000',callId:'inactive-direct',arguments:{{id:'two'}}}}}})}}else if(x.id==='denied-rpc'){{fs.writeFileSync({denied},JSON.stringify(x.result));send({{method:'item/agentMessage/delta',params:{{turnId:'turn',delta:'Done'}}}});send({{method:'turn/completed',params:{{turnId:'turn'}}}})}}return}}
if(x.method==='initialize')send({{id:x.id,result:{{protocolVersion:'2025-01-01',capabilities:{{}}}}}});
else if(x.method==='model/list')send({{id:x.id,result:{{data:[{{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{{reasoningEffort:'medium'}}]}}],nextCursor:null}}}});
else if(x.method==='thread/start'){{fs.writeFileSync({catalog},JSON.stringify(x.params));fs.appendFileSync({registrations},JSON.stringify(x.params)+'\n');send({{id:x.id,result:{{thread:{{id:'thread'}}}}}})}}
else if(x.method==='thread/inject_items'){{fs.writeFileSync({history},JSON.stringify(x.params));send({{id:x.id,result:{{}}}})}}
else if(x.method==='turn/start'){{send({{id:x.id,result:{{turn:{{id:'turn'}}}}}});setTimeout(()=>send({{id:'image-rpc',method:'item/tool/call',params:{{threadId:'thread',turnId:'turn',tool:'codemode',callId:'owned-image',arguments:{{code:{code}}}}}}}),10)}}
else if(x.id!==undefined)send({{id:x.id,result:{{}}}});
}});",
        reply = json!(reply.display().to_string()),
        catalog = json!(catalog.display().to_string()),
        denied = json!(denied.display().to_string()),
        registrations = json!(registrations.display().to_string()),
        history = json!(history.display().to_string()),
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
        .env("MAESTRO_TOOL_PROFILE", "review")
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
    let denied: Value = serde_json::from_slice(&std::fs::read(denied).unwrap()).unwrap();
    assert_eq!(denied["success"], false, "{denied}");
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
                && tool["description"]
                    .as_str()
                    .unwrap()
                    .contains("getToolSchema"))
    );
    let registrations = std::fs::read_to_string(registrations)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        registrations.len(),
        2,
        "changed direct projection must replace the in-process session"
    );
    assert!(
        !registrations[0]["dynamicTools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "websearch")
    );
    assert!(
        registrations[1]["dynamicTools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "websearch")
    );
    let history = std::fs::read_to_string(history).unwrap();
    assert!(
        history.contains("Emit the image") && history.contains("caller result"),
        "{history}"
    );
    let dynamic = catalog["dynamicTools"].as_array().unwrap();
    assert!(dynamic.iter().any(|tool| tool["name"] == "ask_user"));
    assert!(!dynamic.iter().any(|tool| {
        tool["name"]
            .as_str()
            .unwrap()
            .starts_with("client_capability_")
    }));
    assert!(serde_json::to_vec(dynamic).unwrap().len() < 65_536);
    let content = reply["contentItems"][0]["text"].as_str().unwrap();
    assert!(content.contains("caller result"));
    assert!(content.contains("call directly; already registered"));
    assert!(content.contains("id"));
}
