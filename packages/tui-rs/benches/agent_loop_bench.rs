use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use maestro_tui::agent::{FromAgent, NativeAgent, NativeAgentConfig};
use maestro_tui::ai::{ScriptedBlock, ScriptedClient, ScriptedResponse, StopReason, UnifiedClient};
use maestro_tui::state::ApprovalMode;
use tokio::runtime::Runtime;

const TURN_COUNT: usize = 32;
const TOOL_TURN_COUNT: usize = 32;
const MULTI_TOOL_TURN_COUNT: usize = 16;
const LONG_HISTORY_TURN_COUNT: usize = 96;

async fn run_scripted_session(
    responses: Vec<ScriptedResponse>,
    prompts: Vec<String>,
    expected_completions: usize,
    expected_tool_ends: usize,
) {
    let client = UnifiedClient::Scripted(ScriptedClient::new("maestro-replay-v1", responses));
    let config = NativeAgentConfig {
        model: "maestro-replay-v1".to_string(),
        cwd: benchmark_cwd(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = NativeAgent::new_with_test_client(
        std::sync::Arc::new(maestro_local_host::agent::tool_host::BuiltInToolHost),
        config,
        client,
    )
    .expect("agent");

    for prompt in prompts {
        agent.prompt(prompt, vec![]).await.expect("prompt");
    }

    let mut completed = 0;
    let mut tool_ends = 0;
    let mut failed_tool_ids = Vec::new();
    while let Some(event) = events.recv().await {
        if let FromAgent::ToolEnd {
            call_id, success, ..
        } = &event
        {
            tool_ends += 1;
            if !success {
                failed_tool_ids.push(call_id.clone());
            }
        }
        if matches!(
            &event,
            FromAgent::ResponseEnd { response_id, .. } if response_id == "done"
        ) {
            completed += 1;
            if completed == expected_completions {
                break;
            }
        }
    }
    assert_eq!(completed, expected_completions);
    assert_eq!(tool_ends, expected_tool_ends);
    assert!(
        failed_tool_ids.is_empty(),
        "scripted tool fixture must dispatch successfully: {failed_tool_ids:?}"
    );
    agent.shutdown().await;
}

async fn run_scripted_turns(turn_count: usize) {
    let responses = (0..turn_count)
        .map(|_| ScriptedResponse {
            blocks: vec![ScriptedBlock::Text("Completed this step.".to_string())],
            stop_reason: StopReason::EndTurn,
            error: None,
        })
        .collect();
    let prompts = (0..turn_count)
        .map(|index| format!("Continue benchmark turn {index}."))
        .collect();
    run_scripted_session(responses, prompts, turn_count, 0).await;
}

const TOOL_KIND_COUNT: usize = 8;

fn benchmark_cwd() -> String {
    env!("CARGO_MANIFEST_DIR").to_string()
}

fn scripted_tool_call(index: usize, cwd: &str, id_prefix: &str) -> ScriptedBlock {
    let variant = (index / TOOL_KIND_COUNT) % 4;
    let path = match variant {
        0 => cwd.to_string(),
        1 => format!("{cwd}/src"),
        2 => format!("{cwd}/benches"),
        _ => format!("{cwd}/src/agent"),
    };
    let cargo_toml = format!("{cwd}/Cargo.toml");
    let pattern = ["workspace", "members", "resolver", "edition"][variant];
    let id = format!("{id_prefix}-{index}");

    match index % TOOL_KIND_COUNT {
        0 => ScriptedBlock::ToolUse {
            id,
            name: "list".to_string(),
            input: serde_json::json!({"path": path}),
        },
        1 => ScriptedBlock::ToolUse {
            id,
            name: "find".to_string(),
            input: serde_json::json!({
                "pattern": "Cargo.toml",
                "path": path,
                "limit": variant + 1
            }),
        },
        2 => ScriptedBlock::ToolUse {
            id,
            name: "search".to_string(),
            input: serde_json::json!({
                "pattern": pattern,
                "paths": [cargo_toml],
                "literal": true,
                "maxResults": variant + 1
            }),
        },
        3 => ScriptedBlock::ToolUse {
            id,
            name: "parallel_ripgrep".to_string(),
            input: serde_json::json!({
                "patterns": [pattern],
                "paths": [cargo_toml],
                "maxResults": variant + 1
            }),
        },
        4 => ScriptedBlock::ToolUse {
            id,
            name: "read".to_string(),
            input: serde_json::json!({
                "path": cargo_toml,
                "offset": variant + 1,
                "limit": 1
            }),
        },
        5 => ScriptedBlock::ToolUse {
            id,
            name: "glob".to_string(),
            input: serde_json::json!({"pattern": "Cargo.toml", "path": path}),
        },
        6 => ScriptedBlock::ToolUse {
            id,
            name: "grep".to_string(),
            input: serde_json::json!({"pattern": pattern, "path": cargo_toml}),
        },
        _ => ScriptedBlock::ToolUse {
            id,
            name: "status".to_string(),
            input: serde_json::json!({
                "branchSummary": variant < 2,
                "includeIgnored": variant % 2 == 1
            }),
        },
    }
}

async fn run_scripted_tool_turns(turn_count: usize) {
    let cwd = benchmark_cwd();
    let mut responses = (0..turn_count)
        .map(|index| ScriptedResponse {
            blocks: vec![scripted_tool_call(index, &cwd, "tool")],
            stop_reason: StopReason::ToolUse,
            error: None,
        })
        .collect::<Vec<_>>();
    responses.push(ScriptedResponse {
        blocks: vec![ScriptedBlock::Text("Completed the tool loop.".to_string())],
        stop_reason: StopReason::EndTurn,
        error: None,
    });
    run_scripted_session(
        responses,
        vec!["Run the scripted tool loop.".to_string()],
        1,
        turn_count,
    )
    .await;
}

async fn run_scripted_multi_tool_turns(turn_count: usize) {
    let cwd = benchmark_cwd();
    let mut responses = (0..turn_count)
        .map(|index| ScriptedResponse {
            blocks: vec![
                scripted_tool_call(index * 2, &cwd, "multi-tool-a"),
                scripted_tool_call(index * 2 + 1, &cwd, "multi-tool-b"),
            ],
            stop_reason: StopReason::ToolUse,
            error: None,
        })
        .collect::<Vec<_>>();
    responses.push(ScriptedResponse {
        blocks: vec![ScriptedBlock::Text(
            "Completed the multi-tool loop.".to_string(),
        )],
        stop_reason: StopReason::EndTurn,
        error: None,
    });
    run_scripted_session(
        responses,
        vec!["Run the scripted multi-tool loop.".to_string()],
        1,
        turn_count * 2,
    )
    .await;
}

async fn run_scripted_long_history(turn_count: usize) {
    let response_text = "Completed a long-history step. ".repeat(12);
    let responses = (0..turn_count)
        .map(|_| ScriptedResponse {
            blocks: vec![ScriptedBlock::Text(response_text.clone())],
            stop_reason: StopReason::EndTurn,
            error: None,
        })
        .collect();
    let prompt_suffix = " prior context".repeat(24);
    let prompts = (0..turn_count)
        .map(|index| format!("Continue long-history turn {index}.{prompt_suffix}"))
        .collect();
    run_scripted_session(responses, prompts, turn_count, 0).await;
}

fn bench_agent_loop(c: &mut Criterion) {
    let runtime = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("agent_loop");
    group.bench_function("scripted_32_turns", |b| {
        b.iter(|| runtime.block_on(run_scripted_turns(black_box(TURN_COUNT))));
    });
    group.bench_function("scripted_32_tool_turns", |b| {
        b.iter(|| runtime.block_on(run_scripted_tool_turns(black_box(TOOL_TURN_COUNT))));
    });
    group.bench_function("scripted_16_multi_tool_turns", |b| {
        b.iter(|| {
            runtime.block_on(run_scripted_multi_tool_turns(black_box(
                MULTI_TOOL_TURN_COUNT,
            )));
        });
    });
    group.bench_function("scripted_96_long_history_turns", |b| {
        b.iter(|| {
            runtime.block_on(run_scripted_long_history(black_box(
                LONG_HISTORY_TURN_COUNT,
            )));
        });
    });
    group.finish();
}

criterion_group!(benches, bench_agent_loop);
criterion_main!(benches);
