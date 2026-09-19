//! Benchmarks for the hook system
//!
//! Run with: cargo bench --all-features

use criterion::{Criterion, criterion_group, criterion_main};
use maestro_tui::hooks::{
    HookRegistry, HookResult, IntegratedHookSystem, PreToolUseHook, PreToolUseInput, SafetyHook,
};
use std::hint::black_box;
use std::sync::Arc;

/// Benchmark hook registry creation
fn bench_registry_creation(c: &mut Criterion) {
    c.bench_function("registry_creation", |b| {
        b.iter(|| {
            let registry = HookRegistry::new();
            black_box(registry)
        });
    });
}

/// Benchmark registering a hook
fn bench_hook_registration(c: &mut Criterion) {
    c.bench_function("hook_registration", |b| {
        b.iter(|| {
            let mut registry = HookRegistry::new();
            registry.register_pre_tool_use(Arc::new(SafetyHook));
            black_box(registry)
        });
    });
}

/// Benchmark pre-tool-use hook execution (no hooks)
fn bench_pre_tool_use_empty(c: &mut Criterion) {
    let registry = HookRegistry::new();
    let input = PreToolUseInput {
        hook_event_name: "PreToolUse".to_string(),
        cwd: "/tmp".to_string(),
        session_id: None,
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        tool_name: "Bash".to_string(),
        tool_call_id: "1".to_string(),
        tool_input: serde_json::json!({"command": "ls"}),
    };

    c.bench_function("pre_tool_use_empty", |b| {
        b.iter(|| {
            let result = registry.execute_pre_tool_use(black_box(&input));
            black_box(result)
        });
    });
}

/// Benchmark pre-tool-use hook execution (with safety hook)
fn bench_pre_tool_use_safety(c: &mut Criterion) {
    let mut registry = HookRegistry::new();
    registry.register_pre_tool_use(Arc::new(SafetyHook));

    let input = PreToolUseInput {
        hook_event_name: "PreToolUse".to_string(),
        cwd: "/tmp".to_string(),
        session_id: None,
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        tool_name: "Bash".to_string(),
        tool_call_id: "1".to_string(),
        tool_input: serde_json::json!({"command": "ls -la"}),
    };

    c.bench_function("pre_tool_use_safety", |b| {
        b.iter(|| {
            let result = registry.execute_pre_tool_use(black_box(&input));
            black_box(result)
        });
    });
}

/// Benchmark pre-tool-use with dangerous command (blocked)
fn bench_pre_tool_use_blocked(c: &mut Criterion) {
    let mut registry = HookRegistry::new();
    registry.register_pre_tool_use(Arc::new(SafetyHook));

    let input = PreToolUseInput {
        hook_event_name: "PreToolUse".to_string(),
        cwd: "/tmp".to_string(),
        session_id: None,
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        tool_name: "Bash".to_string(),
        tool_call_id: "1".to_string(),
        tool_input: serde_json::json!({"command": "rm -rf /"}),
    };

    c.bench_function("pre_tool_use_blocked", |b| {
        b.iter(|| {
            let result = registry.execute_pre_tool_use(black_box(&input));
            black_box(result)
        });
    });
}

/// Custom hook for benchmarking
struct BenchmarkHook;

impl PreToolUseHook for BenchmarkHook {
    fn on_pre_tool_use(&self, input: &PreToolUseInput) -> HookResult {
        // Simple check that doesn't do much
        if input.tool_name == "Blocked" {
            HookResult::Block {
                reason: "blocked".to_string(),
            }
        } else {
            HookResult::Continue
        }
    }
}

/// Benchmark with multiple hooks
fn bench_pre_tool_use_multiple_hooks(c: &mut Criterion) {
    let mut registry = HookRegistry::new();
    // Register 5 hooks
    for _ in 0..5 {
        registry.register_pre_tool_use(Arc::new(BenchmarkHook));
    }

    let input = PreToolUseInput {
        hook_event_name: "PreToolUse".to_string(),
        cwd: "/tmp".to_string(),
        session_id: None,
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        tool_name: "Bash".to_string(),
        tool_call_id: "1".to_string(),
        tool_input: serde_json::json!({}),
    };

    c.bench_function("pre_tool_use_5_hooks", |b| {
        b.iter(|| {
            let result = registry.execute_pre_tool_use(black_box(&input));
            black_box(result)
        });
    });
}

/// Benchmark IntegratedHookSystem creation
fn bench_integrated_system_creation(c: &mut Criterion) {
    c.bench_function("integrated_system_new", |b| {
        b.iter(|| {
            let system = IntegratedHookSystem::new(black_box("/tmp"));
            black_box(system)
        });
    });
}

/// Benchmark IntegratedHookSystem execute_pre_tool_use
fn bench_integrated_system_pre_tool_use(c: &mut Criterion) {
    let mut system = IntegratedHookSystem::new("/tmp");

    c.bench_function("integrated_pre_tool_use", |b| {
        b.iter(|| {
            let result = system.execute_pre_tool_use(
                black_box("Bash"),
                black_box("1"),
                black_box(&serde_json::json!({"command": "ls"})),
            );
            black_box(result)
        });
    });
}

/// Benchmark IntegratedHookSystem execute_post_tool_use
fn bench_integrated_system_post_tool_use(c: &mut Criterion) {
    let mut system = IntegratedHookSystem::new("/tmp");

    c.bench_function("integrated_post_tool_use", |b| {
        b.iter(|| {
            let result = system.execute_post_tool_use(
                black_box("Bash"),
                black_box("1"),
                black_box(&serde_json::json!({"command": "ls"})),
                black_box("output"),
                black_box(false),
                black_box(0),
            );
            black_box(result)
        });
    });
}

/// Benchmark session lifecycle
fn bench_session_lifecycle(c: &mut Criterion) {
    c.bench_function("session_lifecycle", |b| {
        b.iter(|| {
            let mut system = IntegratedHookSystem::new("/tmp");
            system.on_session_start(black_box("bench"));
            system.increment_turn();
            system.increment_turn();
            system.on_session_end(black_box("complete"));
            black_box(system)
        });
    });
}

/// Benchmark metrics tracking
fn bench_metrics(c: &mut Criterion) {
    let mut system = IntegratedHookSystem::new("/tmp");
    // Execute some hooks to populate metrics
    for _ in 0..10 {
        system.execute_pre_tool_use("Bash", "1", &serde_json::json!({}));
    }

    c.bench_function("get_metrics", |b| {
        b.iter(|| {
            let metrics = system.metrics();
            black_box(metrics)
        });
    });
}

/// Benchmark has_hooks check
fn bench_has_hooks(c: &mut Criterion) {
    use maestro_tui::hooks::HookEventType;

    let mut registry = HookRegistry::new();
    registry.register_pre_tool_use(Arc::new(SafetyHook));

    c.bench_function("has_hooks_check", |b| {
        b.iter(|| {
            let has = registry.has_hooks(black_box(HookEventType::PreToolUse));
            black_box(has)
        });
    });
}

criterion_group!(
    benches,
    bench_registry_creation,
    bench_hook_registration,
    bench_pre_tool_use_empty,
    bench_pre_tool_use_safety,
    bench_pre_tool_use_blocked,
    bench_pre_tool_use_multiple_hooks,
    bench_integrated_system_creation,
    bench_integrated_system_pre_tool_use,
    bench_integrated_system_post_tool_use,
    bench_session_lifecycle,
    bench_metrics,
    bench_has_hooks,
);

criterion_main!(benches);
