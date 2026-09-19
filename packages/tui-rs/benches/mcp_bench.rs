//! Benchmarks for MCP client
//!
//! Run with: cargo bench --bench mcp_bench

use criterion::{Criterion, criterion_group, criterion_main};
use maestro_tui::mcp::{
    McpClient, McpConfig, McpConfigScope, McpContent, McpRequest, McpResponse, McpServerConfig,
    McpTool, McpToolResult, McpTransport, load_mcp_config,
};
use serde_json::json;
use std::collections::HashMap;
use std::hint::black_box;

/// Benchmark MCP client creation
fn bench_mcp_client_creation(c: &mut Criterion) {
    c.bench_function("mcp_client_new", |b| {
        b.iter(|| {
            let client = McpClient::new();
            black_box(client)
        });
    });
}

/// Benchmark is_mcp_tool check
fn bench_is_mcp_tool(c: &mut Criterion) {
    c.bench_function("is_mcp_tool_true", |b| {
        b.iter(|| {
            let is_mcp = McpClient::is_mcp_tool(black_box("mcp_server_tool"));
            black_box(is_mcp)
        });
    });

    c.bench_function("is_mcp_tool_false", |b| {
        b.iter(|| {
            let is_mcp = McpClient::is_mcp_tool(black_box("bash"));
            black_box(is_mcp)
        });
    });
}

/// Benchmark McpRequest creation
fn bench_mcp_request_creation(c: &mut Criterion) {
    c.bench_function("mcp_request_new", |b| {
        b.iter(|| {
            let req = McpRequest::new(
                black_box(1),
                black_box("test/method"),
                black_box(Some(json!({"key": "value"}))),
            );
            black_box(req)
        });
    });

    c.bench_function("mcp_request_list_tools", |b| {
        b.iter(|| {
            let req = McpRequest::list_tools(black_box(1));
            black_box(req)
        });
    });

    c.bench_function("mcp_request_call_tool", |b| {
        b.iter(|| {
            let req = McpRequest::call_tool(
                black_box(1),
                black_box("my_tool"),
                black_box(json!({"arg": "value"})),
            );
            black_box(req)
        });
    });
}

/// Benchmark McpRequest serialization
fn bench_mcp_request_serialize(c: &mut Criterion) {
    let req = McpRequest::call_tool(1, "test_tool", json!({"arg1": "value1", "arg2": 42}));

    c.bench_function("mcp_request_serialize", |b| {
        b.iter(|| {
            let json = serde_json::to_string(black_box(&req)).unwrap();
            black_box(json)
        });
    });
}

/// Benchmark McpResponse deserialization
fn bench_mcp_response_deserialize(c: &mut Criterion) {
    let json_success = r#"{"jsonrpc":"2.0","id":1,"result":{"key":"value"}}"#;
    let json_error =
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid request"}}"#;

    c.bench_function("mcp_response_deserialize_success", |b| {
        b.iter(|| {
            let resp: McpResponse = serde_json::from_str(black_box(json_success)).unwrap();
            black_box(resp)
        });
    });

    c.bench_function("mcp_response_deserialize_error", |b| {
        b.iter(|| {
            let resp: McpResponse = serde_json::from_str(black_box(json_error)).unwrap();
            black_box(resp)
        });
    });
}

/// Benchmark McpServerConfig creation and validation
fn bench_mcp_server_config(c: &mut Criterion) {
    c.bench_function("mcp_server_config_stdio", |b| {
        b.iter(|| {
            let config = McpServerConfig {
                name: black_box("test-server".to_string()),
                transport: McpTransport::Stdio,
                command: Some(black_box("node".to_string())),
                args: vec![black_box("server.js".to_string())],
                env: HashMap::new(),
                cwd: None,
                url: None,
                headers: HashMap::new(),
                headers_helper: None,
                auth_preset: None,
                connection_ref: None,
                credential_ref: None,
                managed_generation: None,
                supports_parallel_tool_calls: None,
                requires_project_approval: None,
                disabled_tools: Vec::new(),
                timeout: Some(30000),
                enabled: true,
                disabled: false,
                scope: McpConfigScope::User,
            };
            black_box(config)
        });
    });

    c.bench_function("mcp_server_config_validate", |b| {
        let config = McpServerConfig {
            name: "test-server".to_string(),
            transport: McpTransport::Stdio,
            command: Some("node".to_string()),
            args: vec!["server.js".to_string()],
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: HashMap::new(),
            headers_helper: None,
            auth_preset: None,
            connection_ref: None,
            credential_ref: None,
            managed_generation: None,
            supports_parallel_tool_calls: None,
            requires_project_approval: None,
            disabled_tools: Vec::new(),
            timeout: Some(30000),
            enabled: true,
            disabled: false,
            scope: McpConfigScope::User,
        };

        b.iter(|| {
            let result = config.validate();
            black_box(result)
        });
    });
}

/// Benchmark McpTool to_tool conversion
fn bench_mcp_tool_conversion(c: &mut Criterion) {
    let mcp_tool = McpTool {
        name: "test_tool".to_string(),
        description: Some("A test tool for benchmarking".to_string()),
        input_schema: Some(json!({
            "type": "object",
            "properties": {
                "arg1": {"type": "string"},
                "arg2": {"type": "number"}
            },
            "required": ["arg1"]
        })),
        annotations: None,
    };

    c.bench_function("mcp_tool_to_tool", |b| {
        b.iter(|| {
            let tool = mcp_tool.to_tool(black_box("myserver"));
            black_box(tool)
        });
    });
}

/// Benchmark McpToolResult to_string
fn bench_mcp_tool_result(c: &mut Criterion) {
    let result = McpToolResult {
        content: vec![
            McpContent::Text {
                text: "Line 1 of output".to_string(),
            },
            McpContent::Text {
                text: "Line 2 of output".to_string(),
            },
            McpContent::Text {
                text: "Line 3 of output".to_string(),
            },
        ],
        is_error: false,
    };

    c.bench_function("mcp_tool_result_to_string", |b| {
        b.iter(|| {
            let s = result.to_string();
            black_box(s)
        });
    });
}

/// Benchmark MCP config loading (empty - no files)
fn bench_mcp_config_load(c: &mut Criterion) {
    c.bench_function("mcp_config_load_empty", |b| {
        b.iter(|| {
            let config = load_mcp_config(black_box(None));
            black_box(config)
        });
    });
}

/// Benchmark McpConfig operations
fn bench_mcp_config_operations(c: &mut Criterion) {
    let config = McpConfig::default();

    // Add some servers manually for testing
    c.bench_function("mcp_config_get_server_none", |b| {
        b.iter(|| {
            let server = config.get_server(black_box("nonexistent"));
            black_box(server)
        });
    });

    c.bench_function("mcp_config_enabled_servers", |b| {
        b.iter(|| {
            let servers: Vec<_> = config.enabled_servers().collect();
            black_box(servers)
        });
    });
}

criterion_group!(
    benches,
    bench_mcp_client_creation,
    bench_is_mcp_tool,
    bench_mcp_request_creation,
    bench_mcp_request_serialize,
    bench_mcp_response_deserialize,
    bench_mcp_server_config,
    bench_mcp_tool_conversion,
    bench_mcp_tool_result,
    bench_mcp_config_load,
    bench_mcp_config_operations,
);

criterion_main!(benches);
