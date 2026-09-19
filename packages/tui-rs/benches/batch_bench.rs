//! Benchmarks for batch tool execution
//!
//! Run with: cargo bench --bench batch_bench

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use maestro_tui::tools::{BatchConfig, BatchExecutor, BatchToolCall};
use serde_json::json;
use std::hint::black_box;

/// Benchmark batch executor creation
fn bench_batch_executor_creation(c: &mut Criterion) {
    c.bench_function("batch_executor_new", |b| {
        b.iter(|| {
            let executor = BatchExecutor::new(black_box("/tmp"));
            black_box(executor)
        });
    });
}

/// Benchmark batch config creation and chaining
fn bench_batch_config(c: &mut Criterion) {
    c.bench_function("batch_config_builder", |b| {
        b.iter(|| {
            let config = BatchConfig::default()
                .with_concurrency(black_box(8))
                .continue_on_error(black_box(false))
                .emit_events(black_box(true));
            black_box(config)
        });
    });
}

/// Benchmark batch tool call creation
fn bench_batch_tool_call_creation(c: &mut Criterion) {
    c.bench_function("batch_tool_call_new", |b| {
        b.iter(|| {
            let call = BatchToolCall::new(
                black_box("call-1"),
                black_box("read"),
                black_box(json!({"file_path": "/tmp/test.txt"})),
            );
            black_box(call)
        });
    });
}

/// Benchmark check_approvals with varying batch sizes
fn bench_check_approvals(c: &mut Criterion) {
    let executor = BatchExecutor::new("/tmp");

    let mut group = c.benchmark_group("check_approvals");

    for size in &[1, 5, 10, 25, 50] {
        let calls: Vec<BatchToolCall> = (0..*size)
            .map(|i| {
                if i % 2 == 0 {
                    BatchToolCall::new(
                        format!("call-{}", i),
                        "read",
                        json!({"file_path": "/tmp/test.txt"}),
                    )
                } else {
                    BatchToolCall::new(
                        format!("call-{}", i),
                        "write",
                        json!({"file_path": "/tmp/test.txt", "content": "x"}),
                    )
                }
            })
            .collect();

        group.bench_with_input(BenchmarkId::from_parameter(size), &calls, |b, calls| {
            b.iter(|| {
                let approvals = executor.check_approvals(black_box(calls));
                black_box(approvals)
            });
        });
    }

    group.finish();
}

/// Benchmark validate_calls with varying batch sizes
fn bench_validate_calls(c: &mut Criterion) {
    let executor = BatchExecutor::new("/tmp");

    let mut group = c.benchmark_group("validate_calls");

    for size in &[1, 5, 10, 25, 50] {
        let calls: Vec<BatchToolCall> = (0..*size)
            .map(|i| {
                BatchToolCall::new(
                    format!("call-{}", i),
                    "read",
                    json!({"file_path": format!("/tmp/file{}.txt", i)}),
                )
            })
            .collect();

        group.bench_with_input(BenchmarkId::from_parameter(size), &calls, |b, calls| {
            b.iter(|| {
                let errors = executor.validate_calls(black_box(calls));
                black_box(errors)
            });
        });
    }

    group.finish();
}

/// Benchmark filter_needs_approval
fn bench_filter_needs_approval(c: &mut Criterion) {
    let executor = BatchExecutor::new("/tmp");

    // Mix of read (no approval) and write (needs approval) calls
    let calls: Vec<BatchToolCall> = (0..20)
        .map(|i| {
            if i % 3 == 0 {
                BatchToolCall::new(
                    format!("call-{}", i),
                    "write",
                    json!({"file_path": "/tmp/test.txt", "content": "x"}),
                )
            } else {
                BatchToolCall::new(
                    format!("call-{}", i),
                    "read",
                    json!({"file_path": "/tmp/test.txt"}),
                )
            }
        })
        .collect();

    c.bench_function("filter_needs_approval_20", |b| {
        b.iter(|| {
            let filtered = executor.filter_needs_approval(black_box(&calls));
            black_box(filtered)
        });
    });
}

/// Benchmark empty batch execution
fn bench_execute_empty(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let executor = BatchExecutor::new("/tmp");

    c.bench_function("execute_empty_batch", |b| {
        b.iter(|| {
            rt.block_on(async {
                let results = executor.execute(black_box(vec![]), None).await;
                black_box(results)
            })
        });
    });
}

criterion_group!(
    benches,
    bench_batch_executor_creation,
    bench_batch_config,
    bench_batch_tool_call_creation,
    bench_check_approvals,
    bench_validate_calls,
    bench_filter_needs_approval,
    bench_execute_empty,
);

criterion_main!(benches);
