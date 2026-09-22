use super::*;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
struct DispatchFixture {
    #[serde(rename = "modelTiers")]
    model_tiers: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(rename = "modeDispatch")]
    mode_dispatch: BTreeMap<String, DispatchFixtureMode>,
}

#[derive(Debug, Deserialize)]
struct DispatchFixtureMode {
    #[serde(rename = "primaryTier")]
    primary_tier: String,
    #[serde(rename = "reasoningEffort")]
    reasoning_effort: String,
    #[serde(rename = "fallbackSubagent")]
    fallback_subagent: String,
    subagents: BTreeMap<String, DispatchFixtureRule>,
}

#[derive(Debug, Deserialize)]
struct DispatchFixtureRule {
    model: String,
    #[serde(rename = "reasoningEffort")]
    reasoning_effort: String,
}

fn parse_agent_mode(value: &str) -> AgentMode {
    match value {
        "smart" => AgentMode::Smart,
        "rush" => AgentMode::Rush,
        "free" => AgentMode::Free,
        "custom" => AgentMode::Custom,
        "frontier" => AgentMode::Frontier,
        "replay" => AgentMode::Replay,
        _ => panic!("unexpected mode {value}"),
    }
}

fn parse_model_provider(value: &str) -> ModelProvider {
    match value {
        "anthropic" => ModelProvider::Anthropic,
        "openai" => ModelProvider::OpenAi,
        "openai-codex" => ModelProvider::OpenAiCodex,
        "google" => ModelProvider::Google,
        _ => panic!("unexpected model provider {value}"),
    }
}

fn parse_model_tier(value: &str) -> ModelTier {
    match value {
        "opus" => ModelTier::Opus,
        "sonnet" => ModelTier::Sonnet,
        "haiku" => ModelTier::Haiku,
        _ => panic!("unexpected model tier {value}"),
    }
}

fn parse_reasoning_effort(value: &str) -> ReasoningEffort {
    match value {
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::XHigh,
        _ => panic!("unexpected reasoning effort {value}"),
    }
}

fn parse_subagent_type(value: &str) -> SubagentType {
    match value {
        "explorer" => SubagentType::Explorer,
        "planner" => SubagentType::Planner,
        "coder" => SubagentType::Coder,
        "reviewer" => SubagentType::Reviewer,
        "test-runner" => SubagentType::TestRunner,
        "researcher" => SubagentType::Researcher,
        "browser-qa" => SubagentType::BrowserQa,
        "minimal" => SubagentType::Minimal,
        "custom" => SubagentType::Custom,
        _ => panic!("unexpected subagent type {value}"),
    }
}

fn split_explicit_model(model: &str) -> Option<(ModelProvider, &str)> {
    let (provider, model_id) = model.split_once('/')?;
    Some((parse_model_provider(provider), model_id))
}

#[test]
fn test_task_can_start() {
    let mut completed = HashSet::new();
    let task = SwarmTask::new("task-1", "Test Task").with_dependencies(vec!["dep-1".into()]);

    assert!(!task.can_start(&completed));

    completed.insert("dep-1".into());
    assert!(task.can_start(&completed));
}

#[test]
fn test_task_priority_order() {
    assert!(TaskPriority::Low < TaskPriority::Normal);
    assert!(TaskPriority::Normal < TaskPriority::High);
    assert!(TaskPriority::High < TaskPriority::Critical);
}

#[test]
fn test_plan_ready_tasks() {
    let plan = SwarmPlan::new("Test Plan").with_tasks(vec![
        SwarmTask::new("task-1", "First"),
        SwarmTask::new("task-2", "Second").with_dependencies(vec!["task-1".into()]),
        SwarmTask::new("task-3", "Third"),
    ]);

    let completed = HashSet::new();
    let ready = plan.ready_tasks(&completed);
    assert_eq!(ready.len(), 2); // task-1 and task-3 (task-2 blocked by dep)

    let mut completed = HashSet::new();
    completed.insert("task-1".into());
    let ready = plan.ready_tasks(&completed);
    // task-2's dep is now met, task-3 has no deps, but task-1 is already in completed
    // ready_tasks returns tasks that can_start (Pending status + deps met)
    // Since task-1 is completed but still Pending status, it would still be returned
    // Let's fix by checking the actual logic: can_start checks status == Pending
    assert_eq!(ready.len(), 3); // All tasks still have Pending status, deps met for all
}

#[test]
fn test_swarm_state_progress() {
    let plan = SwarmPlan::new("Test").with_tasks(vec![
        SwarmTask::new("1", "One"),
        SwarmTask::new("2", "Two"),
        SwarmTask::new("3", "Three"),
    ]);

    let mut state = SwarmState::new(plan, SwarmConfig::default());
    assert_eq!(state.progress(), (0, 3));

    state.completed_tasks.insert("1".into());
    assert_eq!(state.progress(), (1, 3));

    state.failed_tasks.insert("2".into());
    assert_eq!(state.progress(), (2, 3));
}

// ========== SwarmStatus Tests ==========

#[test]
fn test_swarm_status_default() {
    assert_eq!(SwarmStatus::default(), SwarmStatus::Initializing);
}

#[test]
fn test_swarm_status_serialization() {
    let status = SwarmStatus::Running;
    let json = serde_json::to_string(&status).unwrap();
    assert_eq!(json, "\"running\"");

    let status: SwarmStatus = serde_json::from_str("\"completed\"").unwrap();
    assert_eq!(status, SwarmStatus::Completed);
}

// ========== TaskStatus Tests ==========

#[test]
fn test_task_status_default() {
    assert_eq!(TaskStatus::default(), TaskStatus::Pending);
}

#[test]
fn test_task_status_variants() {
    assert_ne!(TaskStatus::Pending, TaskStatus::Running);
    assert_ne!(TaskStatus::Running, TaskStatus::Completed);
    assert_ne!(TaskStatus::Completed, TaskStatus::Failed);
    assert_ne!(TaskStatus::Failed, TaskStatus::Skipped);
}

// ========== TaskPriority Tests ==========

#[test]
fn test_task_priority_default() {
    assert_eq!(TaskPriority::default(), TaskPriority::Normal);
}

#[test]
fn test_task_priority_serialization() {
    let priority = TaskPriority::High;
    let json = serde_json::to_string(&priority).unwrap();
    assert_eq!(json, "\"high\"");

    let priority: TaskPriority = serde_json::from_str("\"critical\"").unwrap();
    assert_eq!(priority, TaskPriority::Critical);
}

// ========== SwarmTask Tests ==========

#[test]
fn test_swarm_task_new() {
    let task = SwarmTask::new("task-1", "Test Task");
    assert_eq!(task.id, "task-1");
    assert_eq!(task.title, "Test Task");
    assert!(task.description.is_empty());
    assert_eq!(task.priority, TaskPriority::Normal);
    assert_eq!(task.status, TaskStatus::Pending);
    assert!(task.dependencies.is_empty());
    assert_eq!(task.complexity, 1);
    assert!(task.assigned_agent.is_none());
    assert!(task.result.is_none());
    assert!(task.files.is_empty());
    assert!(task.tags.is_empty());
}

#[test]
fn test_swarm_task_builder() {
    let task = SwarmTask::new("task-1", "Test")
        .with_description("A test task")
        .with_dependencies(vec!["dep-1".into(), "dep-2".into()])
        .with_priority(TaskPriority::High)
        .with_complexity(7);

    assert_eq!(task.description, "A test task");
    assert_eq!(task.dependencies.len(), 2);
    assert_eq!(task.priority, TaskPriority::High);
    assert_eq!(task.complexity, 7);
}

#[test]
fn test_swarm_task_complexity_clamped() {
    let task1 = SwarmTask::new("1", "T").with_complexity(0);
    assert_eq!(task1.complexity, 1);

    let task2 = SwarmTask::new("2", "T").with_complexity(15);
    assert_eq!(task2.complexity, 10);

    let task3 = SwarmTask::new("3", "T").with_complexity(5);
    assert_eq!(task3.complexity, 5);
}

#[test]
fn test_swarm_task_can_start_no_deps() {
    let task = SwarmTask::new("task-1", "Test");
    let completed = HashSet::new();
    assert!(task.can_start(&completed));
}

#[test]
fn test_swarm_task_can_start_with_status() {
    let mut task = SwarmTask::new("task-1", "Test");
    let completed = HashSet::new();

    assert!(task.can_start(&completed));

    task.status = TaskStatus::Running;
    assert!(!task.can_start(&completed));

    task.status = TaskStatus::Completed;
    assert!(!task.can_start(&completed));
}

// ========== TaskResult Tests ==========

#[test]
fn test_task_result_success() {
    let result = TaskResult {
        success: true,
        output: "Task completed successfully".to_string(),
        files_modified: vec!["src/main.rs".to_string()],
        duration_ms: 1500,
        error: None,
    };

    assert!(result.success);
    assert!(result.error.is_none());
    assert_eq!(result.files_modified.len(), 1);
}

#[test]
fn test_task_result_failure() {
    let result = TaskResult {
        success: false,
        output: String::new(),
        files_modified: Vec::new(),
        duration_ms: 500,
        error: Some("Failed to compile".to_string()),
    };

    assert!(!result.success);
    assert!(result.error.is_some());
}

// ========== SwarmPlan Tests ==========

#[test]
fn test_swarm_plan_default() {
    let plan = SwarmPlan::default();
    assert_eq!(plan.title, "Untitled Plan");
    assert!(plan.goal.is_empty());
    assert!(plan.tasks.is_empty());
    assert_eq!(plan.max_concurrency, 3);
    assert!(!plan.continue_on_failure);
}

#[test]
fn test_swarm_plan_builder() {
    let plan = SwarmPlan::new("My Plan")
        .with_goal("Complete the project")
        .with_max_concurrency(5)
        .with_tasks(vec![
            SwarmTask::new("1", "First"),
            SwarmTask::new("2", "Second"),
        ]);

    assert_eq!(plan.title, "My Plan");
    assert_eq!(plan.goal, "Complete the project");
    assert_eq!(plan.max_concurrency, 5);
    assert_eq!(plan.tasks.len(), 2);
}

#[test]
fn test_swarm_plan_max_concurrency_minimum() {
    let plan = SwarmPlan::new("Test").with_max_concurrency(0);
    assert_eq!(plan.max_concurrency, 1);
}

#[test]
fn test_swarm_plan_get_task() {
    let plan = SwarmPlan::new("Test").with_tasks(vec![
        SwarmTask::new("task-1", "First"),
        SwarmTask::new("task-2", "Second"),
    ]);

    assert!(plan.get_task("task-1").is_some());
    assert!(plan.get_task("task-2").is_some());
    assert!(plan.get_task("nonexistent").is_none());
}

#[test]
fn test_swarm_plan_get_task_mut() {
    let mut plan = SwarmPlan::new("Test").with_tasks(vec![SwarmTask::new("task-1", "First")]);

    {
        let task = plan.get_task_mut("task-1").unwrap();
        task.status = TaskStatus::Running;
    }

    assert_eq!(plan.get_task("task-1").unwrap().status, TaskStatus::Running);
}

// ========== SwarmConfig Tests ==========

#[test]
fn test_swarm_config_default() {
    let config = SwarmConfig::default();
    assert_eq!(config.max_concurrency, 3);
    assert!(!config.continue_on_failure);
    assert_eq!(config.task_timeout_ms, Some(300_000));
    assert!(config.model.is_none());
    assert!(config.system_prompt.is_none());
    assert!(config.mode.is_none());
    assert!(config.model_provider.is_none());
    assert!(config.subagent_type.is_none());
    assert!(config.reasoning_effort.is_none());
}

#[test]
fn test_smart_coder_subagent_dispatch_uses_openai_codex_model() {
    let dispatch = resolve_subagent_dispatch(
        AgentMode::Smart,
        SubagentType::Coder,
        ModelProvider::Anthropic,
    );

    assert_eq!(dispatch.mode, AgentMode::Smart);
    assert_eq!(dispatch.subagent_type, SubagentType::Coder);
    assert_eq!(dispatch.provider, ModelProvider::OpenAiCodex);
    assert_eq!(dispatch.model, "gpt-5.6");
    assert_eq!(dispatch.reasoning_effort, ReasoningEffort::Medium);
    assert_eq!(dispatch.source, DispatchSource::Mode);
}

#[test]
fn test_custom_researcher_subagent_dispatch_falls_back_to_mode_tier() {
    let dispatch = resolve_subagent_dispatch(
        AgentMode::Custom,
        SubagentType::Researcher,
        ModelProvider::Google,
    );

    assert_eq!(dispatch.mode, AgentMode::Custom);
    assert_eq!(dispatch.subagent_type, SubagentType::Researcher);
    assert_eq!(dispatch.provider, ModelProvider::Google);
    // Derived, not a literal: this test pinned "gemini-2.0-flash-exp" and so
    // kept passing after that model was retired.
    assert_eq!(
        dispatch.model,
        model_for_tier(ModelTier::Sonnet, ModelProvider::Google)
    );
    assert_eq!(dispatch.model_tier, Some(ModelTier::Sonnet));
    assert_eq!(dispatch.reasoning_effort, ReasoningEffort::Medium);
    assert_eq!(dispatch.source, DispatchSource::Fallback);
}

#[test]
fn test_test_runner_subagent_dispatch_falls_back_to_mode_tier() {
    let dispatch = resolve_subagent_dispatch(
        AgentMode::Smart,
        SubagentType::TestRunner,
        ModelProvider::Anthropic,
    );

    assert_eq!(dispatch.mode, AgentMode::Smart);
    assert_eq!(dispatch.subagent_type, SubagentType::TestRunner);
    assert_eq!(dispatch.provider, ModelProvider::Anthropic);
    assert_eq!(dispatch.model_tier, Some(ModelTier::Opus));
    assert_eq!(dispatch.reasoning_effort, ReasoningEffort::Medium);
    assert_eq!(dispatch.source, DispatchSource::Fallback);
}

#[test]
fn test_subagent_dispatch_matches_protocol_fixture() {
    let fixture: DispatchFixture = serde_json::from_str(include_str!(
        "../../../../docs/protocols/codex-subagent-dispatch-table-v1.json"
    ))
    .expect("dispatch fixture should parse");

    for (tier_id, providers) in &fixture.model_tiers {
        let tier = parse_model_tier(tier_id);
        for (provider_id, expected_model) in providers {
            assert_eq!(
                model_for_tier(tier, parse_model_provider(provider_id)),
                expected_model
            );
        }
    }

    for (mode_id, mode_fixture) in &fixture.mode_dispatch {
        let mode = parse_agent_mode(mode_id);
        assert_eq!(
            mode_primary_tier(mode),
            parse_model_tier(&mode_fixture.primary_tier)
        );
        assert_eq!(
            mode_reasoning_effort(mode),
            parse_reasoning_effort(&mode_fixture.reasoning_effort)
        );

        for (subagent_id, rule) in &mode_fixture.subagents {
            let subagent_type = parse_subagent_type(subagent_id);
            let dispatch = resolve_subagent_dispatch(mode, subagent_type, ModelProvider::Anthropic);
            assert_eq!(dispatch.source, DispatchSource::Mode);
            assert_eq!(
                dispatch.reasoning_effort,
                parse_reasoning_effort(&rule.reasoning_effort)
            );

            if let Some((provider, model)) = split_explicit_model(&rule.model) {
                assert_eq!(dispatch.provider, provider);
                assert_eq!(dispatch.model, model);
                assert_eq!(dispatch.model_tier, None);
            } else {
                let tier = parse_model_tier(&rule.model);
                assert_eq!(dispatch.provider, ModelProvider::Anthropic);
                assert_eq!(dispatch.model_tier, Some(tier));
                assert_eq!(
                    dispatch.model,
                    model_for_tier(tier, ModelProvider::Anthropic)
                );
            }
        }

        let fallback = resolve_subagent_dispatch(
            mode,
            parse_subagent_type(&mode_fixture.fallback_subagent),
            ModelProvider::Google,
        );
        let fallback_tier = parse_model_tier(&mode_fixture.primary_tier);
        assert_eq!(fallback.source, DispatchSource::Fallback);
        assert_eq!(fallback.provider, ModelProvider::Google);
        assert_eq!(fallback.model_tier, Some(fallback_tier));
        assert_eq!(
            fallback.model,
            model_for_tier(fallback_tier, ModelProvider::Google)
        );
        assert_eq!(
            fallback.reasoning_effort,
            parse_reasoning_effort(&mode_fixture.reasoning_effort)
        );
    }
}

// ========== SwarmEvent Tests ==========

#[test]
fn test_swarm_event_started() {
    let event = SwarmEvent::Started {
        plan_title: "Test Plan".to_string(),
        total_tasks: 5,
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"started\""));
    assert!(json.contains("\"plan_title\":\"Test Plan\""));
    assert!(json.contains("\"total_tasks\":5"));
}

#[test]
fn test_swarm_event_task_started() {
    let event = SwarmEvent::TaskStarted {
        task_id: "task-1".to_string(),
        task_title: "First Task".to_string(),
        agent_id: "agent-123".to_string(),
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"task_started\""));
}

#[test]
fn test_swarm_event_completed() {
    let event = SwarmEvent::Completed {
        successful: 8,
        failed: 1,
        skipped: 1,
        duration_ms: 60_000,
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"completed\""));
    assert!(json.contains("\"successful\":8"));
    assert!(json.contains("\"failed\":1"));
}

#[test]
fn test_swarm_event_cancelled() {
    let event = SwarmEvent::Cancelled {
        reason: "User requested".to_string(),
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"cancelled\""));
}

// ========== SwarmState Tests ==========

#[test]
fn test_swarm_state_new() {
    let plan = SwarmPlan::new("Test");
    let config = SwarmConfig::default();
    let state = SwarmState::new(plan, config);

    assert_eq!(state.status, SwarmStatus::Initializing);
    assert!(state.completed_tasks.is_empty());
    assert!(state.failed_tasks.is_empty());
    assert!(state.running_tasks.is_empty());
    assert!(state.started_at.is_none());
    assert!(state.events.is_empty());
}

#[test]
fn test_swarm_state_is_done() {
    let plan = SwarmPlan::new("Test");
    let config = SwarmConfig::default();
    let mut state = SwarmState::new(plan, config);

    assert!(!state.is_done());

    state.status = SwarmStatus::Running;
    assert!(!state.is_done());

    state.status = SwarmStatus::Completed;
    assert!(state.is_done());

    state.status = SwarmStatus::Cancelled;
    assert!(state.is_done());

    state.status = SwarmStatus::Failed;
    assert!(state.is_done());
}

#[test]
fn test_swarm_state_can_start_more() {
    let plan = SwarmPlan::new("Test");
    let config = SwarmConfig {
        max_concurrency: 2,
        ..Default::default()
    };
    let mut state = SwarmState::new(plan, config);
    state.status = SwarmStatus::Running;

    assert!(state.can_start_more());

    state
        .running_tasks
        .insert("task-1".into(), "agent-1".into());
    assert!(state.can_start_more());

    state
        .running_tasks
        .insert("task-2".into(), "agent-2".into());
    assert!(!state.can_start_more()); // At max concurrency
}

#[test]
fn test_swarm_state_can_start_more_when_done() {
    let plan = SwarmPlan::new("Test");
    let config = SwarmConfig::default();
    let mut state = SwarmState::new(plan, config);

    state.status = SwarmStatus::Completed;
    assert!(!state.can_start_more());

    state.status = SwarmStatus::Cancelled;
    assert!(!state.can_start_more());
}

#[test]
fn test_swarm_state_can_start_more_not_running() {
    let plan = SwarmPlan::new("Test");
    let config = SwarmConfig::default();
    let mut state = SwarmState::new(plan, config);

    state.status = SwarmStatus::Initializing;
    assert!(!state.can_start_more());

    state.status = SwarmStatus::Planning;
    assert!(!state.can_start_more());
}

// ========== Additional SwarmStatus Tests ==========

#[test]
fn test_swarm_status_all_variants() {
    let statuses = [
        SwarmStatus::Initializing,
        SwarmStatus::Planning,
        SwarmStatus::Running,
        SwarmStatus::Completed,
        SwarmStatus::Cancelled,
        SwarmStatus::Failed,
    ];
    assert_eq!(statuses.len(), 6);
    // All distinct
    for i in 0..statuses.len() {
        for j in (i + 1)..statuses.len() {
            assert_ne!(statuses[i], statuses[j]);
        }
    }
}

#[test]
fn test_swarm_status_serialization_all() {
    assert_eq!(
        serde_json::to_string(&SwarmStatus::Initializing).unwrap(),
        "\"initializing\""
    );
    assert_eq!(
        serde_json::to_string(&SwarmStatus::Planning).unwrap(),
        "\"planning\""
    );
    assert_eq!(
        serde_json::to_string(&SwarmStatus::Running).unwrap(),
        "\"running\""
    );
    assert_eq!(
        serde_json::to_string(&SwarmStatus::Completed).unwrap(),
        "\"completed\""
    );
    assert_eq!(
        serde_json::to_string(&SwarmStatus::Cancelled).unwrap(),
        "\"cancelled\""
    );
    assert_eq!(
        serde_json::to_string(&SwarmStatus::Failed).unwrap(),
        "\"failed\""
    );
}

#[test]
fn test_swarm_status_deserialization() {
    let status: SwarmStatus = serde_json::from_str("\"initializing\"").unwrap();
    assert_eq!(status, SwarmStatus::Initializing);

    let status: SwarmStatus = serde_json::from_str("\"planning\"").unwrap();
    assert_eq!(status, SwarmStatus::Planning);
}

#[test]
fn test_swarm_status_copy_trait() {
    let status = SwarmStatus::Running;
    let copied = status;
    assert_eq!(status, copied);
}

// ========== Additional TaskStatus Tests ==========

#[test]
fn test_task_status_all_variants() {
    let statuses = [
        TaskStatus::Pending,
        TaskStatus::Blocked,
        TaskStatus::Running,
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Skipped,
    ];
    assert_eq!(statuses.len(), 6);
}

#[test]
fn test_task_status_serialization_all() {
    assert_eq!(
        serde_json::to_string(&TaskStatus::Pending).unwrap(),
        "\"pending\""
    );
    assert_eq!(
        serde_json::to_string(&TaskStatus::Blocked).unwrap(),
        "\"blocked\""
    );
    assert_eq!(
        serde_json::to_string(&TaskStatus::Running).unwrap(),
        "\"running\""
    );
    assert_eq!(
        serde_json::to_string(&TaskStatus::Completed).unwrap(),
        "\"completed\""
    );
    assert_eq!(
        serde_json::to_string(&TaskStatus::Failed).unwrap(),
        "\"failed\""
    );
    assert_eq!(
        serde_json::to_string(&TaskStatus::Skipped).unwrap(),
        "\"skipped\""
    );
}

#[test]
fn test_task_status_copy_trait() {
    let status = TaskStatus::Running;
    let copied = status;
    assert_eq!(status, copied);
}

// ========== Additional TaskPriority Tests ==========

#[test]
fn test_task_priority_all_values() {
    assert_eq!(TaskPriority::Low as u8, 0);
    assert_eq!(TaskPriority::Normal as u8, 1);
    assert_eq!(TaskPriority::High as u8, 2);
    assert_eq!(TaskPriority::Critical as u8, 3);
}

#[test]
fn test_task_priority_serialization_all() {
    assert_eq!(
        serde_json::to_string(&TaskPriority::Low).unwrap(),
        "\"low\""
    );
    assert_eq!(
        serde_json::to_string(&TaskPriority::Normal).unwrap(),
        "\"normal\""
    );
    assert_eq!(
        serde_json::to_string(&TaskPriority::High).unwrap(),
        "\"high\""
    );
    assert_eq!(
        serde_json::to_string(&TaskPriority::Critical).unwrap(),
        "\"critical\""
    );
}

#[test]
fn test_task_priority_ordering() {
    let mut priorities = vec![
        TaskPriority::High,
        TaskPriority::Low,
        TaskPriority::Critical,
        TaskPriority::Normal,
    ];
    priorities.sort();
    assert_eq!(
        priorities,
        vec![
            TaskPriority::Low,
            TaskPriority::Normal,
            TaskPriority::High,
            TaskPriority::Critical
        ]
    );
}

// ========== Additional SwarmTask Tests ==========

#[test]
fn test_swarm_task_multiple_dependencies() {
    let task = SwarmTask::new("task-5", "Fifth Task").with_dependencies(vec![
        "1".into(),
        "2".into(),
        "3".into(),
        "4".into(),
    ]);

    assert_eq!(task.dependencies.len(), 4);

    let mut completed = HashSet::new();
    completed.insert("1".to_string());
    completed.insert("2".to_string());
    completed.insert("3".to_string());
    assert!(!task.can_start(&completed)); // Missing "4"

    completed.insert("4".to_string());
    assert!(task.can_start(&completed));
}

#[test]
fn test_swarm_task_with_files() {
    let mut task = SwarmTask::new("task-1", "Test");
    task.files = vec!["src/main.rs".into(), "src/lib.rs".into()];
    assert_eq!(task.files.len(), 2);
}

#[test]
fn test_swarm_task_with_tags() {
    let mut task = SwarmTask::new("task-1", "Test");
    task.tags = vec!["frontend".into(), "critical".into(), "v1".into()];
    assert_eq!(task.tags.len(), 3);
}

#[test]
fn test_swarm_task_clone() {
    let task = SwarmTask::new("task-1", "Test")
        .with_description("Description")
        .with_priority(TaskPriority::High)
        .with_dependencies(vec!["dep-1".into()])
        .with_complexity(8);

    let cloned = task.clone();
    assert_eq!(cloned.id, task.id);
    assert_eq!(cloned.title, task.title);
    assert_eq!(cloned.description, task.description);
    assert_eq!(cloned.priority, task.priority);
    assert_eq!(cloned.dependencies, task.dependencies);
    assert_eq!(cloned.complexity, task.complexity);
}

#[test]
fn test_swarm_task_serialization_roundtrip() {
    let mut task = SwarmTask::new("task-1", "Test Task")
        .with_description("A test task")
        .with_priority(TaskPriority::Critical)
        .with_dependencies(vec!["dep-1".into()])
        .with_complexity(5);

    task.assigned_agent = Some("agent-1".to_string());
    task.files = vec!["file1.rs".into()];
    task.tags = vec!["test".into()];

    let json = serde_json::to_string(&task).unwrap();
    let deserialized: SwarmTask = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.id, task.id);
    assert_eq!(deserialized.title, task.title);
    assert_eq!(deserialized.description, task.description);
    assert_eq!(deserialized.priority, task.priority);
    assert_eq!(deserialized.status, task.status);
    assert_eq!(deserialized.dependencies, task.dependencies);
    assert_eq!(deserialized.complexity, task.complexity);
    assert_eq!(deserialized.assigned_agent, task.assigned_agent);
    assert_eq!(deserialized.files, task.files);
    assert_eq!(deserialized.tags, task.tags);
}

#[test]
fn test_swarm_task_blocked_status_cannot_start() {
    let mut task = SwarmTask::new("task-1", "Test");
    task.status = TaskStatus::Blocked;
    assert!(!task.can_start(&HashSet::new()));
}

// ========== Additional TaskResult Tests ==========

#[test]
fn test_task_result_serialization() {
    let result = TaskResult {
        success: true,
        output: "Done".to_string(),
        files_modified: vec!["a.rs".into(), "b.rs".into()],
        duration_ms: 1000,
        error: None,
    };

    let json = serde_json::to_string(&result).unwrap();
    let deserialized: TaskResult = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.success, result.success);
    assert_eq!(deserialized.output, result.output);
    assert_eq!(deserialized.files_modified, result.files_modified);
    assert_eq!(deserialized.duration_ms, result.duration_ms);
    assert_eq!(deserialized.error, result.error);
}

#[test]
fn test_task_result_clone() {
    let result = TaskResult {
        success: false,
        output: "Error".to_string(),
        files_modified: vec![],
        duration_ms: 100,
        error: Some("Compile error".to_string()),
    };

    let cloned = result.clone();
    assert_eq!(cloned.success, result.success);
    assert_eq!(cloned.output, result.output);
    assert_eq!(cloned.error, result.error);
}

// ========== Additional SwarmPlan Tests ==========

#[test]
fn test_swarm_plan_ready_tasks_all_completed() {
    let plan = SwarmPlan::new("Test").with_tasks(vec![
        SwarmTask::new("task-1", "First"),
        SwarmTask::new("task-2", "Second"),
    ]);

    let mut completed = HashSet::new();
    completed.insert("task-1".to_string());
    completed.insert("task-2".to_string());

    // All tasks completed, but they still have Pending status
    // so can_start will return true for tasks with met dependencies
    let ready = plan.ready_tasks(&completed);
    assert_eq!(ready.len(), 2);
}

#[test]
fn test_swarm_plan_chained_dependencies() {
    let plan = SwarmPlan::new("Test").with_tasks(vec![
        SwarmTask::new("1", "First"),
        SwarmTask::new("2", "Second").with_dependencies(vec!["1".into()]),
        SwarmTask::new("3", "Third").with_dependencies(vec!["2".into()]),
        SwarmTask::new("4", "Fourth").with_dependencies(vec!["3".into()]),
    ]);

    let completed = HashSet::new();
    let ready = plan.ready_tasks(&completed);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "1");
}

#[test]
fn test_swarm_plan_serialization() {
    let plan = SwarmPlan::new("Test Plan")
        .with_goal("Complete project")
        .with_max_concurrency(5)
        .with_tasks(vec![
            SwarmTask::new("1", "First"),
            SwarmTask::new("2", "Second"),
        ]);

    let json = serde_json::to_string(&plan).unwrap();
    let deserialized: SwarmPlan = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.title, plan.title);
    assert_eq!(deserialized.goal, plan.goal);
    assert_eq!(deserialized.max_concurrency, plan.max_concurrency);
    assert_eq!(deserialized.tasks.len(), plan.tasks.len());
}

#[test]
fn test_swarm_plan_clone() {
    let plan = SwarmPlan::new("Test")
        .with_goal("Goal")
        .with_tasks(vec![SwarmTask::new("1", "Task 1")]);

    let cloned = plan.clone();
    assert_eq!(cloned.title, plan.title);
    assert_eq!(cloned.goal, plan.goal);
    assert_eq!(cloned.tasks.len(), plan.tasks.len());
}

// ========== Additional SwarmConfig Tests ==========

#[test]
fn test_swarm_config_custom() {
    let config = SwarmConfig {
        max_concurrency: 10,
        continue_on_failure: true,
        task_timeout_ms: Some(600_000),
        model: Some("claude-3-opus".to_string()),
        system_prompt: Some("Custom prompt".to_string()),
        ..Default::default()
    };

    assert_eq!(config.max_concurrency, 10);
    assert!(config.continue_on_failure);
    assert_eq!(config.task_timeout_ms, Some(600_000));
    assert_eq!(config.model, Some("claude-3-opus".to_string()));
    assert_eq!(config.system_prompt, Some("Custom prompt".to_string()));
}

#[test]
fn test_swarm_config_serialization() {
    let config = SwarmConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: SwarmConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.max_concurrency, config.max_concurrency);
    assert_eq!(deserialized.continue_on_failure, config.continue_on_failure);
    assert_eq!(deserialized.task_timeout_ms, config.task_timeout_ms);
}

#[test]
fn test_swarm_config_clone() {
    let config = SwarmConfig {
        max_concurrency: 5,
        continue_on_failure: true,
        task_timeout_ms: Some(100_000),
        model: Some("test-model".to_string()),
        system_prompt: Some("test".to_string()),
        ..Default::default()
    };

    let cloned = config.clone();
    assert_eq!(cloned.max_concurrency, config.max_concurrency);
    assert_eq!(cloned.continue_on_failure, config.continue_on_failure);
    assert_eq!(cloned.model, config.model);
}

// ========== Additional SwarmEvent Tests ==========

#[test]
fn test_swarm_event_task_progress() {
    let event = SwarmEvent::TaskProgress {
        task_id: "task-1".to_string(),
        message: "Working on it...".to_string(),
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"task_progress\""));
    assert!(json.contains("\"task_id\":\"task-1\""));
    assert!(json.contains("\"message\":\"Working on it...\""));
}

#[test]
fn test_swarm_event_task_completed() {
    let event = SwarmEvent::TaskCompleted {
        task_id: "task-1".to_string(),
        result: TaskResult {
            success: true,
            output: "Done".to_string(),
            files_modified: vec!["main.rs".to_string()],
            duration_ms: 5000,
            error: None,
        },
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"task_completed\""));
}

#[test]
fn test_swarm_event_task_failed() {
    let event = SwarmEvent::TaskFailed {
        task_id: "task-1".to_string(),
        error: "Compilation failed".to_string(),
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"task_failed\""));
    assert!(json.contains("\"error\":\"Compilation failed\""));
}

#[test]
fn test_swarm_event_failed() {
    let event = SwarmEvent::Failed {
        error: "Fatal error occurred".to_string(),
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"failed\""));
}

#[test]
fn test_swarm_event_clone() {
    let event = SwarmEvent::Started {
        plan_title: "Test".to_string(),
        total_tasks: 10,
    };

    let cloned = event.clone();
    match (event, cloned) {
        (
            SwarmEvent::Started {
                plan_title: a,
                total_tasks: b,
            },
            SwarmEvent::Started {
                plan_title: c,
                total_tasks: d,
            },
        ) => {
            assert_eq!(a, c);
            assert_eq!(b, d);
        }
        _ => panic!("Clone mismatch"),
    }
}

// ========== Additional SwarmState Tests ==========

#[test]
fn test_swarm_state_default() {
    let state = SwarmState::default();
    assert_eq!(state.status, SwarmStatus::Initializing);
    assert!(state.completed_tasks.is_empty());
    assert!(state.failed_tasks.is_empty());
    assert!(state.running_tasks.is_empty());
}

#[test]
fn test_swarm_state_progress_with_running() {
    let plan = SwarmPlan::new("Test").with_tasks(vec![
        SwarmTask::new("1", "One"),
        SwarmTask::new("2", "Two"),
        SwarmTask::new("3", "Three"),
        SwarmTask::new("4", "Four"),
    ]);

    let mut state = SwarmState::new(plan, SwarmConfig::default());
    state.completed_tasks.insert("1".to_string());
    state.failed_tasks.insert("2".to_string());
    state
        .running_tasks
        .insert("3".to_string(), "agent-1".to_string());

    // Progress counts completed + failed, not running
    let (done, total) = state.progress();
    assert_eq!(done, 2);
    assert_eq!(total, 4);
}

#[test]
fn test_swarm_state_events_tracking() {
    let plan = SwarmPlan::new("Test");
    let config = SwarmConfig::default();
    let mut state = SwarmState::new(plan, config);

    state.events.push(SwarmEvent::Started {
        plan_title: "Test".to_string(),
        total_tasks: 5,
    });
    state.events.push(SwarmEvent::TaskStarted {
        task_id: "1".to_string(),
        task_title: "First".to_string(),
        agent_id: "agent-1".to_string(),
    });

    assert_eq!(state.events.len(), 2);
}

#[test]
fn test_swarm_state_started_at() {
    let plan = SwarmPlan::new("Test");
    let config = SwarmConfig::default();
    let mut state = SwarmState::new(plan, config);

    assert!(state.started_at.is_none());

    state.started_at = Some(1_234_567_890);
    assert_eq!(state.started_at, Some(1_234_567_890));
}

// ========== Debug Trait Tests ==========

#[test]
fn test_swarm_status_debug() {
    let debug = format!("{:?}", SwarmStatus::Running);
    assert!(debug.contains("Running"));
}

#[test]
fn test_task_status_debug() {
    let debug = format!("{:?}", TaskStatus::Pending);
    assert!(debug.contains("Pending"));
}

#[test]
fn test_task_priority_debug() {
    let debug = format!("{:?}", TaskPriority::High);
    assert!(debug.contains("High"));
}

#[test]
fn test_swarm_task_debug() {
    let task = SwarmTask::new("task-1", "Test");
    let debug = format!("{:?}", task);
    assert!(debug.contains("task-1"));
    assert!(debug.contains("Test"));
}

#[test]
fn test_task_result_debug() {
    let result = TaskResult {
        success: true,
        output: "Done".to_string(),
        files_modified: vec![],
        duration_ms: 100,
        error: None,
    };
    let debug = format!("{:?}", result);
    assert!(debug.contains("success"));
}

#[test]
fn test_swarm_plan_debug() {
    let plan = SwarmPlan::new("Test");
    let debug = format!("{:?}", plan);
    assert!(debug.contains("Test"));
}

#[test]
fn test_swarm_config_debug() {
    let config = SwarmConfig::default();
    let debug = format!("{:?}", config);
    assert!(debug.contains("max_concurrency"));
}

#[test]
fn test_swarm_event_debug() {
    let event = SwarmEvent::Started {
        plan_title: "Test".to_string(),
        total_tasks: 5,
    };
    let debug = format!("{:?}", event);
    assert!(debug.contains("Started"));
}

#[test]
fn test_swarm_state_debug() {
    let state = SwarmState::default();
    let debug = format!("{:?}", state);
    assert!(debug.contains("SwarmState"));
}

// ========== Edge Cases ==========

#[test]
fn test_swarm_task_empty_id_and_title() {
    let task = SwarmTask::new("", "");
    assert!(task.id.is_empty());
    assert!(task.title.is_empty());
}

#[test]
fn test_swarm_task_unicode() {
    let task = SwarmTask::new("タスク-1", "日本語タスク").with_description("説明文");
    assert_eq!(task.id, "タスク-1");
    assert_eq!(task.title, "日本語タスク");
    assert_eq!(task.description, "説明文");
}

#[test]
fn test_swarm_plan_empty_tasks() {
    let plan = SwarmPlan::new("Empty Plan");
    assert!(plan.tasks.is_empty());

    let ready = plan.ready_tasks(&HashSet::new());
    assert!(ready.is_empty());
}

#[test]
fn test_swarm_state_progress_empty_plan() {
    let plan = SwarmPlan::new("Empty");
    let state = SwarmState::new(plan, SwarmConfig::default());

    let (done, total) = state.progress();
    assert_eq!(done, 0);
    assert_eq!(total, 0);
}

#[test]
fn test_task_result_empty_output() {
    let result = TaskResult {
        success: true,
        output: String::new(),
        files_modified: vec![],
        duration_ms: 0,
        error: None,
    };
    assert!(result.output.is_empty());
    assert_eq!(result.duration_ms, 0);
}

#[test]
fn test_swarm_config_no_timeout() {
    let config = SwarmConfig {
        task_timeout_ms: None,
        ..Default::default()
    };
    assert!(config.task_timeout_ms.is_none());
}

// ============================================================
// Circular Dependencies Tests
// ============================================================

#[test]
fn test_circular_dependency_self_reference() {
    // A task depending on itself
    let task = SwarmTask::new("task-1", "Self Reference").with_dependencies(vec!["task-1".into()]);

    // can_start will never return true because task-1 can't be completed without starting
    let mut completed = HashSet::new();
    assert!(!task.can_start(&completed));

    // Even if we artificially mark it complete, can_start returns true
    // (no validation that this is a self-reference)
    completed.insert("task-1".into());
    assert!(task.can_start(&completed));
}

#[test]
fn test_circular_dependency_two_tasks() {
    // A -> B and B -> A
    let task_a = SwarmTask::new("A", "Task A").with_dependencies(vec!["B".into()]);
    let task_b = SwarmTask::new("B", "Task B").with_dependencies(vec!["A".into()]);

    let completed = HashSet::new();
    // Neither can start
    assert!(!task_a.can_start(&completed));
    assert!(!task_b.can_start(&completed));
}

#[test]
fn test_circular_dependency_chain() {
    // A -> B -> C -> A
    let task_a = SwarmTask::new("A", "Task A").with_dependencies(vec!["C".into()]);
    let task_b = SwarmTask::new("B", "Task B").with_dependencies(vec!["A".into()]);
    let task_c = SwarmTask::new("C", "Task C").with_dependencies(vec!["B".into()]);

    let plan = SwarmPlan::new("Circular").with_tasks(vec![task_a, task_b, task_c]);

    let completed = HashSet::new();
    // No tasks can start
    let ready = plan.ready_tasks(&completed);
    assert!(ready.is_empty());
}

#[test]
fn test_plan_with_mixed_circular_and_valid() {
    // Valid: D (no deps)
    // Circular: A -> B -> C -> A
    let task_a = SwarmTask::new("A", "Task A").with_dependencies(vec!["C".into()]);
    let task_b = SwarmTask::new("B", "Task B").with_dependencies(vec!["A".into()]);
    let task_c = SwarmTask::new("C", "Task C").with_dependencies(vec!["B".into()]);
    let task_d = SwarmTask::new("D", "Task D"); // No dependencies

    let plan = SwarmPlan::new("Mixed").with_tasks(vec![task_a, task_b, task_c, task_d]);

    let completed = HashSet::new();
    let ready = plan.ready_tasks(&completed);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "D");
}

#[test]
fn test_dependency_on_nonexistent_task() {
    // Task depends on a task that doesn't exist in the plan
    let task = SwarmTask::new("task-1", "Test").with_dependencies(vec!["nonexistent".into()]);

    let plan = SwarmPlan::new("Test").with_tasks(vec![task]);

    let completed = HashSet::new();
    let ready = plan.ready_tasks(&completed);
    // Task can't start because "nonexistent" isn't completed
    assert!(ready.is_empty());
}

// ============================================================
// Cycle Detection Tests (find_cycle, validate_dependencies, is_acyclic)
// ============================================================

#[test]
fn test_find_cycle_no_cycle() {
    // Linear chain: A -> B -> C (no cycle)
    let task_a = SwarmTask::new("A", "Task A");
    let task_b = SwarmTask::new("B", "Task B").with_dependencies(vec!["A".into()]);
    let task_c = SwarmTask::new("C", "Task C").with_dependencies(vec!["B".into()]);

    let plan = SwarmPlan::new("Linear").with_tasks(vec![task_a, task_b, task_c]);

    assert!(plan.find_cycle().is_none());
    assert!(plan.is_acyclic());
}

#[test]
fn test_find_cycle_self_reference() {
    let task = SwarmTask::new("A", "Task A").with_dependencies(vec!["A".into()]);
    let plan = SwarmPlan::new("Self").with_tasks(vec![task]);

    let cycle = plan.find_cycle();
    assert!(cycle.is_some());
    let cycle = cycle.unwrap();
    assert!(cycle.contains(&"A".to_string()));
}

#[test]
fn test_find_cycle_two_node() {
    // A -> B -> A
    let task_a = SwarmTask::new("A", "Task A").with_dependencies(vec!["B".into()]);
    let task_b = SwarmTask::new("B", "Task B").with_dependencies(vec!["A".into()]);

    let plan = SwarmPlan::new("Two").with_tasks(vec![task_a, task_b]);

    let cycle = plan.find_cycle();
    assert!(cycle.is_some());
    assert!(!plan.is_acyclic());
}

#[test]
fn test_find_cycle_three_node() {
    // A -> B -> C -> A
    let task_a = SwarmTask::new("A", "Task A").with_dependencies(vec!["C".into()]);
    let task_b = SwarmTask::new("B", "Task B").with_dependencies(vec!["A".into()]);
    let task_c = SwarmTask::new("C", "Task C").with_dependencies(vec!["B".into()]);

    let plan = SwarmPlan::new("Three").with_tasks(vec![task_a, task_b, task_c]);

    let cycle = plan.find_cycle();
    assert!(cycle.is_some());
    let cycle = cycle.unwrap();
    // Cycle should include all three
    assert!(cycle.len() >= 3);
}

#[test]
fn test_find_cycle_with_independent_tasks() {
    // Cycle: A -> B -> A
    // Independent: C, D
    let task_a = SwarmTask::new("A", "Task A").with_dependencies(vec!["B".into()]);
    let task_b = SwarmTask::new("B", "Task B").with_dependencies(vec!["A".into()]);
    let task_c = SwarmTask::new("C", "Task C");
    let task_d = SwarmTask::new("D", "Task D").with_dependencies(vec!["C".into()]);

    let plan = SwarmPlan::new("Mixed").with_tasks(vec![task_a, task_b, task_c, task_d]);

    // Should still detect the cycle
    assert!(plan.find_cycle().is_some());
    assert!(!plan.is_acyclic());
}

#[test]
fn test_find_cycle_diamond_no_cycle() {
    // Diamond pattern (no cycle):
    //     A
    //    / \
    //   B   C
    //    \ /
    //     D
    let task_a = SwarmTask::new("A", "Task A");
    let task_b = SwarmTask::new("B", "Task B").with_dependencies(vec!["A".into()]);
    let task_c = SwarmTask::new("C", "Task C").with_dependencies(vec!["A".into()]);
    let task_d = SwarmTask::new("D", "Task D").with_dependencies(vec!["B".into(), "C".into()]);

    let plan = SwarmPlan::new("Diamond").with_tasks(vec![task_a, task_b, task_c, task_d]);

    assert!(plan.find_cycle().is_none());
    assert!(plan.is_acyclic());
}

#[test]
fn test_find_cycle_long_chain() {
    // Long chain with cycle at the end: 1 -> 2 -> ... -> 10 -> 1
    let mut tasks = Vec::new();
    for i in 1..=10 {
        let deps = if i == 1 {
            vec!["10".to_string()]
        } else {
            vec![format!("{}", i - 1)]
        };
        tasks.push(SwarmTask::new(format!("{}", i), format!("Task {}", i)).with_dependencies(deps));
    }

    let plan = SwarmPlan::new("Long").with_tasks(tasks);

    assert!(plan.find_cycle().is_some());
}

#[test]
fn test_validate_dependencies_valid() {
    let task_a = SwarmTask::new("A", "Task A");
    let task_b = SwarmTask::new("B", "Task B").with_dependencies(vec!["A".into()]);

    let plan = SwarmPlan::new("Valid").with_tasks(vec![task_a, task_b]);

    assert!(plan.validate_dependencies().is_ok());
}

#[test]
fn test_validate_dependencies_cycle_error() {
    let task_a = SwarmTask::new("A", "Task A").with_dependencies(vec!["B".into()]);
    let task_b = SwarmTask::new("B", "Task B").with_dependencies(vec!["A".into()]);

    let plan = SwarmPlan::new("Cycle").with_tasks(vec![task_a, task_b]);

    let result = plan.validate_dependencies();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Circular dependency"));
}

#[test]
fn test_validate_dependencies_missing_dep_error() {
    let task = SwarmTask::new("A", "Task A").with_dependencies(vec!["nonexistent".into()]);

    let plan = SwarmPlan::new("Missing").with_tasks(vec![task]);

    let result = plan.validate_dependencies();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("non-existent task"));
}

#[test]
fn test_validate_dependencies_self_dep_error() {
    let task = SwarmTask::new("A", "Task A").with_dependencies(vec!["A".into()]);

    let plan = SwarmPlan::new("Self").with_tasks(vec![task]);

    let result = plan.validate_dependencies();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("depends on itself"));
}

#[test]
fn test_validate_dependencies_empty_plan() {
    let plan = SwarmPlan::new("Empty");
    assert!(plan.validate_dependencies().is_ok());
}

#[test]
fn test_is_acyclic_empty_plan() {
    let plan = SwarmPlan::new("Empty");
    assert!(plan.is_acyclic());
}

#[test]
fn test_find_cycle_performance_many_tasks() {
    // Create 100 tasks in a valid DAG (linear chain)
    let tasks: Vec<SwarmTask> = (0..100)
        .map(|i| {
            if i == 0 {
                SwarmTask::new(format!("task-{}", i), format!("Task {}", i))
            } else {
                SwarmTask::new(format!("task-{}", i), format!("Task {}", i))
                    .with_dependencies(vec![format!("task-{}", i - 1)])
            }
        })
        .collect();

    let plan = SwarmPlan::new("Large").with_tasks(tasks);

    // Should complete quickly and find no cycle
    assert!(plan.find_cycle().is_none());
    assert!(plan.is_acyclic());
}

// ============================================================
// Boundary Value Tests
// ============================================================

#[test]
fn test_complexity_boundary_zero() {
    let task = SwarmTask::new("task-1", "Test").with_complexity(0);
    assert_eq!(task.complexity, 1); // Clamped to 1
}

#[test]
fn test_complexity_boundary_max() {
    let task = SwarmTask::new("task-1", "Test").with_complexity(u8::MAX);
    assert_eq!(task.complexity, 10); // Clamped to 10
}

#[test]
fn test_complexity_boundary_exact() {
    let task1 = SwarmTask::new("1", "T").with_complexity(1);
    assert_eq!(task1.complexity, 1);

    let task10 = SwarmTask::new("10", "T").with_complexity(10);
    assert_eq!(task10.complexity, 10);
}

#[test]
fn test_max_concurrency_boundary_zero() {
    let plan = SwarmPlan::new("Test").with_max_concurrency(0);
    assert_eq!(plan.max_concurrency, 1); // Minimum 1
}

#[test]
fn test_max_concurrency_boundary_max() {
    let plan = SwarmPlan::new("Test").with_max_concurrency(usize::MAX);
    assert_eq!(plan.max_concurrency, usize::MAX);
}

#[test]
fn test_task_result_duration_max() {
    let result = TaskResult {
        success: true,
        output: String::new(),
        files_modified: vec![],
        duration_ms: u64::MAX,
        error: None,
    };
    assert_eq!(result.duration_ms, u64::MAX);
}

#[test]
fn test_swarm_config_max_values() {
    let config = SwarmConfig {
        max_concurrency: usize::MAX,
        continue_on_failure: true,
        task_timeout_ms: Some(u64::MAX),
        model: None,
        system_prompt: None,
        ..Default::default()
    };

    let json = serde_json::to_string(&config).unwrap();
    let deserialized: SwarmConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.max_concurrency, usize::MAX);
    assert_eq!(deserialized.task_timeout_ms, Some(u64::MAX));
}

#[test]
fn test_swarm_state_can_start_more_at_exact_limit() {
    let plan = SwarmPlan::new("Test");
    let config = SwarmConfig {
        max_concurrency: 3,
        ..Default::default()
    };
    let mut state = SwarmState::new(plan, config);
    state.status = SwarmStatus::Running;

    // Add exactly max_concurrency tasks
    state.running_tasks.insert("1".into(), "a1".into());
    state.running_tasks.insert("2".into(), "a2".into());
    state.running_tasks.insert("3".into(), "a3".into());

    assert!(!state.can_start_more());
}

// ============================================================
// Many Tasks Tests
// ============================================================

#[test]
fn test_plan_with_many_tasks() {
    let tasks: Vec<SwarmTask> = (0..1000)
        .map(|i| SwarmTask::new(format!("task-{}", i), format!("Task {}", i)))
        .collect();

    let plan = SwarmPlan::new("Large Plan").with_tasks(tasks);
    assert_eq!(plan.tasks.len(), 1000);

    let ready = plan.ready_tasks(&HashSet::new());
    assert_eq!(ready.len(), 1000);
}

#[test]
fn test_task_with_many_dependencies() {
    let deps: Vec<String> = (0..100).map(|i| format!("dep-{}", i)).collect();
    let task = SwarmTask::new("task-1", "Task 1").with_dependencies(deps.clone());

    assert_eq!(task.dependencies.len(), 100);

    // Can't start with no deps completed
    let completed = HashSet::new();
    assert!(!task.can_start(&completed));

    // Can't start with some deps completed
    let mut partial: HashSet<String> = (0..50).map(|i| format!("dep-{}", i)).collect();
    assert!(!task.can_start(&partial));

    // Can start with all deps completed
    for i in 50..100 {
        partial.insert(format!("dep-{}", i));
    }
    assert!(task.can_start(&partial));
}

#[test]
fn test_task_with_many_files() {
    let mut task = SwarmTask::new("task-1", "Test");
    task.files = (0..1000).map(|i| format!("file-{}.rs", i)).collect();
    assert_eq!(task.files.len(), 1000);
}

#[test]
fn test_task_with_many_tags() {
    let mut task = SwarmTask::new("task-1", "Test");
    task.tags = (0..100).map(|i| format!("tag-{}", i)).collect();
    assert_eq!(task.tags.len(), 100);
}

// ============================================================
// State Consistency Tests
// ============================================================

#[test]
fn test_state_progress_consistency() {
    let plan = SwarmPlan::new("Test").with_tasks(vec![
        SwarmTask::new("1", "One"),
        SwarmTask::new("2", "Two"),
        SwarmTask::new("3", "Three"),
    ]);

    let mut state = SwarmState::new(plan, SwarmConfig::default());

    // Progress should always be <= total
    for i in 0..4 {
        let (done, total) = state.progress();
        assert!(done <= total);
        assert_eq!(total, 3);

        if i < 3 {
            state.completed_tasks.insert(format!("{}", i + 1));
        }
    }
}

#[test]
fn test_state_cannot_double_complete() {
    let plan = SwarmPlan::new("Test").with_tasks(vec![SwarmTask::new("1", "One")]);
    let mut state = SwarmState::new(plan, SwarmConfig::default());

    state.completed_tasks.insert("1".into());
    state.completed_tasks.insert("1".into()); // Insert same again

    // HashSet deduplicates
    assert_eq!(state.completed_tasks.len(), 1);
}

#[test]
fn test_state_task_in_both_completed_and_failed() {
    let plan = SwarmPlan::new("Test").with_tasks(vec![SwarmTask::new("1", "One")]);
    let mut state = SwarmState::new(plan, SwarmConfig::default());

    // Shouldn't happen but test the behavior
    state.completed_tasks.insert("1".into());
    state.failed_tasks.insert("1".into());

    // Progress counts both
    let (done, total) = state.progress();
    assert_eq!(done, 2); // This is probably a bug but documenting behavior
    assert_eq!(total, 1);
}

// ============================================================
// Event Serialization Tests
// ============================================================

#[test]
fn test_swarm_event_deserialization_all_types() {
    let events = vec![
        r#"{"type":"started","plan_title":"Test","total_tasks":5}"#,
        r#"{"type":"task_started","task_id":"1","task_title":"T","agent_id":"a"}"#,
        r#"{"type":"task_progress","task_id":"1","message":"Working"}"#,
        r#"{"type":"task_failed","task_id":"1","error":"Error"}"#,
        r#"{"type":"cancelled","reason":"User requested"}"#,
        r#"{"type":"failed","error":"Fatal"}"#,
    ];

    for json in events {
        let _event: SwarmEvent = serde_json::from_str(json).unwrap();
    }
}

#[test]
fn test_swarm_event_task_completed_deserialization() {
    let json = r#"{
        "type": "task_completed",
        "task_id": "task-1",
        "result": {
            "success": true,
            "output": "Done",
            "files_modified": ["a.rs"],
            "duration_ms": 1000,
            "error": null
        }
    }"#;

    let event: SwarmEvent = serde_json::from_str(json).unwrap();
    match event {
        SwarmEvent::TaskCompleted { task_id, result } => {
            assert_eq!(task_id, "task-1");
            assert!(result.success);
        }
        _ => panic!("Wrong event type"),
    }
}

#[test]
fn test_swarm_event_completed_deserialization() {
    let json = r#"{
        "type": "completed",
        "successful": 8,
        "failed": 1,
        "skipped": 2,
        "duration_ms": 60000
    }"#;

    let event: SwarmEvent = serde_json::from_str(json).unwrap();
    match event {
        SwarmEvent::Completed {
            successful,
            failed,
            skipped,
            duration_ms,
        } => {
            assert_eq!(successful, 8);
            assert_eq!(failed, 1);
            assert_eq!(skipped, 2);
            assert_eq!(duration_ms, 60_000);
        }
        _ => panic!("Wrong event type"),
    }
}

#[test]
fn test_swarm_event_completed_default_skipped() {
    let json = r#"{
        "type": "completed",
        "successful": 2,
        "failed": 0,
        "duration_ms": 500
    }"#;

    let event: SwarmEvent = serde_json::from_str(json).unwrap();
    match event {
        SwarmEvent::Completed {
            successful,
            failed,
            skipped,
            duration_ms,
        } => {
            assert_eq!(successful, 2);
            assert_eq!(failed, 0);
            assert_eq!(skipped, 0);
            assert_eq!(duration_ms, 500);
        }
        _ => panic!("Wrong event type"),
    }
}

// ============================================================
// Special Characters Tests
// ============================================================

#[test]
fn test_task_id_special_characters() {
    let task = SwarmTask::new("task/with:special-chars_v1.0", "Test");
    assert_eq!(task.id, "task/with:special-chars_v1.0");

    let json = serde_json::to_string(&task).unwrap();
    let deserialized: SwarmTask = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.id, task.id);
}

#[test]
fn test_task_description_multiline() {
    let task = SwarmTask::new("task-1", "Test")
        .with_description("Line 1\nLine 2\nLine 3\n\nWith blank line");

    let json = serde_json::to_string(&task).unwrap();
    let deserialized: SwarmTask = serde_json::from_str(&json).unwrap();
    assert!(deserialized.description.contains('\n'));
}

#[test]
fn test_task_result_error_with_quotes() {
    let result = TaskResult {
        success: false,
        output: String::new(),
        files_modified: vec![],
        duration_ms: 0,
        error: Some("Error: \"file not found\"".to_string()),
    };

    let json = serde_json::to_string(&result).unwrap();
    let deserialized: TaskResult = serde_json::from_str(&json).unwrap();
    assert!(deserialized.error.unwrap().contains("\"file not found\""));
}

// ============================================================
// Plan Operations Tests
// ============================================================

#[test]
fn test_plan_get_task_mut_modify_status() {
    let mut plan = SwarmPlan::new("Test").with_tasks(vec![
        SwarmTask::new("1", "First"),
        SwarmTask::new("2", "Second"),
    ]);

    // Modify status
    if let Some(task) = plan.get_task_mut("1") {
        task.status = TaskStatus::Running;
    }

    // Verify change
    assert_eq!(plan.get_task("1").unwrap().status, TaskStatus::Running);
    assert_eq!(plan.get_task("2").unwrap().status, TaskStatus::Pending);
}

#[test]
fn test_plan_get_task_mut_assign_agent() {
    let mut plan = SwarmPlan::new("Test").with_tasks(vec![SwarmTask::new("1", "First")]);

    if let Some(task) = plan.get_task_mut("1") {
        task.assigned_agent = Some("agent-123".to_string());
        task.status = TaskStatus::Running;
    }

    let task = plan.get_task("1").unwrap();
    assert_eq!(task.assigned_agent, Some("agent-123".to_string()));
}

#[test]
fn test_plan_get_task_mut_set_result() {
    let mut plan = SwarmPlan::new("Test").with_tasks(vec![SwarmTask::new("1", "First")]);

    if let Some(task) = plan.get_task_mut("1") {
        task.result = Some(TaskResult {
            success: true,
            output: "Done".to_string(),
            files_modified: vec!["main.rs".to_string()],
            duration_ms: 1000,
            error: None,
        });
        task.status = TaskStatus::Completed;
    }

    let task = plan.get_task("1").unwrap();
    assert!(task.result.is_some());
    assert!(task.result.as_ref().unwrap().success);
}

// ============================================================
// Continue on Failure Tests
// ============================================================

#[test]
fn test_swarm_config_continue_on_failure_default_false() {
    let config = SwarmConfig::default();
    assert!(!config.continue_on_failure);
}

#[test]
fn test_swarm_plan_continue_on_failure() {
    let plan = SwarmPlan {
        title: "Test".to_string(),
        goal: String::new(),
        tasks: vec![],
        max_concurrency: 3,
        continue_on_failure: true,
    };
    assert!(plan.continue_on_failure);
}

#[test]
fn every_tier_model_exists_in_the_bundled_catalog() {
    // The Google tiers pointed at gemini-2.0-flash-thinking-exp,
    // gemini-2.0-flash-exp and gemini-2.0-flash-lite-exp: retired 2.0
    // experimental previews absent from the catalog, so every Google-provider
    // subagent dispatch named a model that no longer exists. Nothing checked
    // these ids against the catalog, so they went stale silently.
    let catalog: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../local-host-rs/src/model_catalog_data.json"
    )))
    .expect("bundled catalog parses");
    let ids: std::collections::HashSet<&str> = catalog["models"]
        .as_array()
        .expect("models array")
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect();

    let mut missing = Vec::new();
    for tier in [ModelTier::Opus, ModelTier::Sonnet, ModelTier::Haiku] {
        for provider in [
            ModelProvider::Anthropic,
            ModelProvider::OpenAi,
            ModelProvider::Google,
        ] {
            let model = model_for_tier(tier, provider);
            if !ids.contains(model) {
                missing.push(format!("{tier:?}/{provider:?} -> {model}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "tier models absent from the bundled catalog: {missing:?}"
    );
}
