//! Swarm Mode Types
//!
//! Defines the core types for multi-agent task orchestration.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::recovery::{DispatchId, IndeterminateTask};

/// Unique identifier for a swarm task
pub type TaskId = String;

/// Unique identifier for a swarm agent
pub type AgentId = String;

/// Status of a swarm operation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmStatus {
    /// Swarm is initializing
    #[default]
    Initializing,
    /// Swarm is parsing the plan
    Planning,
    /// Swarm is actively executing tasks
    Running,
    /// Swarm has completed all tasks
    Completed,
    /// Swarm was cancelled
    Cancelled,
    /// Swarm failed due to errors
    Failed,
}

/// Status of an individual task
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task is waiting to be executed
    #[default]
    Pending,
    /// Task is blocked by dependencies
    Blocked,
    /// Task is currently running
    Running,
    /// Task completed successfully
    Completed,
    /// Task failed
    Failed,
    /// Task was skipped (e.g., dependency failed)
    Skipped,
}

/// Priority level for tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskPriority {
    Low = 0,
    #[default]
    Normal = 1,
    High = 2,
    Critical = 3,
}

/// A single task in the swarm
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmTask {
    /// Unique identifier
    pub id: TaskId,
    /// Human-readable title
    pub title: String,
    /// Detailed description/instructions
    pub description: String,
    /// Task priority
    pub priority: TaskPriority,
    /// Current status
    pub status: TaskStatus,
    /// IDs of tasks this depends on
    pub dependencies: Vec<TaskId>,
    /// Estimated complexity (1-10)
    pub complexity: u8,
    /// Assigned agent ID (if running)
    pub assigned_agent: Option<AgentId>,
    /// Result output (if completed)
    pub result: Option<TaskResult>,
    /// Files involved in this task
    pub files: Vec<String>,
    /// Tags for categorization
    pub tags: Vec<String>,
}

impl SwarmTask {
    /// Create a new task with the given ID and title
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            priority: TaskPriority::default(),
            status: TaskStatus::default(),
            dependencies: Vec::new(),
            complexity: 1,
            assigned_agent: None,
            result: None,
            files: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// Set the task description
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Add dependencies
    #[must_use]
    pub fn with_dependencies(mut self, deps: Vec<TaskId>) -> Self {
        self.dependencies = deps;
        self
    }

    /// Set priority
    #[must_use]
    pub fn with_priority(mut self, priority: TaskPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Set complexity
    #[must_use]
    pub fn with_complexity(mut self, complexity: u8) -> Self {
        self.complexity = complexity.clamp(1, 10);
        self
    }

    /// Check if task can be started (all dependencies complete)
    #[must_use]
    pub fn can_start(&self, completed_tasks: &HashSet<TaskId>) -> bool {
        self.status == TaskStatus::Pending
            && self
                .dependencies
                .iter()
                .all(|d| completed_tasks.contains(d))
    }
}

/// Result of a completed task
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResult {
    /// Was the task successful
    pub success: bool,
    /// Output/summary from the task
    pub output: String,
    /// Files modified
    pub files_modified: Vec<String>,
    /// Duration in milliseconds
    pub duration_ms: u64,
    /// Error message if failed
    pub error: Option<String>,
}

/// Swarm execution plan
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmPlan {
    /// Plan title
    pub title: String,
    /// Overall goal/description
    pub goal: String,
    /// All tasks in the plan
    pub tasks: Vec<SwarmTask>,
    /// Maximum concurrent agents
    pub max_concurrency: usize,
    /// Whether to continue on task failure
    pub continue_on_failure: bool,
}

impl Default for SwarmPlan {
    fn default() -> Self {
        Self {
            title: "Untitled Plan".to_string(),
            goal: String::new(),
            tasks: Vec::new(),
            max_concurrency: 3,
            continue_on_failure: false,
        }
    }
}

impl SwarmPlan {
    /// Create a new plan with a title
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Default::default()
        }
    }

    /// Set the goal
    pub fn with_goal(mut self, goal: impl Into<String>) -> Self {
        self.goal = goal.into();
        self
    }

    /// Add tasks
    #[must_use]
    pub fn with_tasks(mut self, tasks: Vec<SwarmTask>) -> Self {
        self.tasks = tasks;
        self
    }

    /// Set max concurrency
    #[must_use]
    pub fn with_max_concurrency(mut self, n: usize) -> Self {
        self.max_concurrency = n.max(1);
        self
    }

    /// Get tasks that are ready to run
    #[must_use]
    pub fn ready_tasks(&self, completed: &HashSet<TaskId>) -> Vec<&SwarmTask> {
        self.tasks
            .iter()
            .filter(|t| t.can_start(completed))
            .collect()
    }

    /// Get task by ID
    #[must_use]
    pub fn get_task(&self, id: &str) -> Option<&SwarmTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Get mutable task by ID
    pub fn get_task_mut(&mut self, id: &str) -> Option<&mut SwarmTask> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// Check if the dependency graph has any cycles.
    /// Returns Some(cycle) with the task IDs forming a cycle, or None if acyclic.
    #[must_use]
    pub fn find_cycle(&self) -> Option<Vec<TaskId>> {
        // Build adjacency map: task_id -> dependencies
        let task_ids: HashSet<_> = self.tasks.iter().map(|t| t.id.as_str()).collect();

        // Track visit state: 0 = unvisited, 1 = in current path, 2 = completed
        let mut state: HashMap<&str, u8> = HashMap::new();
        let mut path: Vec<&str> = Vec::new();

        fn dfs<'a>(
            task_id: &'a str,
            tasks: &'a [SwarmTask],
            task_ids: &HashSet<&str>,
            state: &mut HashMap<&'a str, u8>,
            path: &mut Vec<&'a str>,
        ) -> Option<Vec<String>> {
            match state.get(task_id) {
                Some(2) => return None, // Already fully processed
                Some(1) => {
                    // Found cycle - extract it from path
                    let cycle_start = path.iter().position(|&id| id == task_id).unwrap();
                    let mut cycle: Vec<String> = path[cycle_start..]
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect();
                    cycle.push(task_id.to_string());
                    return Some(cycle);
                }
                _ => {}
            }

            state.insert(task_id, 1); // Mark as in current path
            path.push(task_id);

            // Find task and check its dependencies
            if let Some(task) = tasks.iter().find(|t| t.id == task_id) {
                for dep_id in &task.dependencies {
                    // Only follow dependencies that exist in the plan
                    if task_ids.contains(dep_id.as_str()) {
                        if let Some(cycle) = dfs(dep_id, tasks, task_ids, state, path) {
                            return Some(cycle);
                        }
                    }
                }
            }

            path.pop();
            state.insert(task_id, 2); // Mark as completed
            None
        }

        // Check each task as a starting point
        for task in &self.tasks {
            if let Some(cycle) = dfs(&task.id, &self.tasks, &task_ids, &mut state, &mut path) {
                return Some(cycle);
            }
        }

        None
    }

    /// Check if the plan has valid dependencies (no cycles, no missing deps).
    /// Returns Ok(()) if valid, or Err with description of the problem.
    pub fn validate_dependencies(&self) -> Result<(), String> {
        // Check for self-dependencies first (more specific error message)
        for task in &self.tasks {
            if task.dependencies.contains(&task.id) {
                return Err(format!("Task '{}' depends on itself", task.id));
            }
        }

        // Check for missing dependencies
        let task_ids: HashSet<_> = self.tasks.iter().map(|t| t.id.as_str()).collect();
        for task in &self.tasks {
            for dep_id in &task.dependencies {
                if !task_ids.contains(dep_id.as_str()) {
                    return Err(format!(
                        "Task '{}' depends on non-existent task '{}'",
                        task.id, dep_id
                    ));
                }
            }
        }

        // Check for cycles (multi-node cycles)
        if let Some(cycle) = self.find_cycle() {
            return Err(format!(
                "Circular dependency detected: {}",
                cycle.join(" -> ")
            ));
        }

        Ok(())
    }

    /// Returns true if the dependency graph is acyclic (valid DAG)
    #[must_use]
    pub fn is_acyclic(&self) -> bool {
        self.find_cycle().is_none()
    }
}

/// Agent modes used to resolve subagent dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Low,
    Medium,
    High,
    Ultra,
    Smart,
    Rush,
    Free,
    Custom,
    Frontier,
    Replay,
}

/// Logical model tiers that map to provider-specific model IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Opus,
    Sonnet,
    Haiku,
}

/// Supported model providers for mode and subagent dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelProvider {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openai-codex")]
    OpenAiCodex,
    Google,
}

/// Reasoning budget hint for child agents that support it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

/// Where a resolved subagent dispatch came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DispatchSource {
    Mode,
    Fallback,
}

/// Available subagent roles shared with the TypeScript runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentType {
    Explorer,
    Planner,
    Coder,
    Reviewer,
    #[serde(rename = "test-runner")]
    TestRunner,
    Researcher,
    #[serde(rename = "browser-qa")]
    BrowserQa,
    Minimal,
    Custom,
}

/// Fully resolved routing for one subagent invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSubagentDispatch {
    pub mode: AgentMode,
    pub subagent_type: SubagentType,
    pub provider: ModelProvider,
    pub model: String,
    pub model_tier: Option<ModelTier>,
    pub reasoning_effort: ReasoningEffort,
    pub source: DispatchSource,
}

enum DispatchModelRef {
    Tier(ModelTier),
    Explicit {
        provider: Option<ModelProvider>,
        model: &'static str,
    },
}

struct SubagentDispatchRule {
    model: DispatchModelRef,
    reasoning_effort: ReasoningEffort,
}

impl SubagentDispatchRule {
    fn tier(model: ModelTier, reasoning_effort: ReasoningEffort) -> Self {
        Self {
            model: DispatchModelRef::Tier(model),
            reasoning_effort,
        }
    }

    fn explicit(
        provider: ModelProvider,
        model: &'static str,
        reasoning_effort: ReasoningEffort,
    ) -> Self {
        Self {
            model: DispatchModelRef::Explicit {
                provider: Some(provider),
                model,
            },
            reasoning_effort,
        }
    }
}

pub fn model_for_tier(tier: ModelTier, provider: ModelProvider) -> &'static str {
    match (tier, provider) {
        (ModelTier::Opus, ModelProvider::Anthropic) => "claude-opus-5-5",
        (ModelTier::Opus, ModelProvider::OpenAi) => "gpt-5.2",
        (ModelTier::Opus, ModelProvider::OpenAiCodex) => "gpt-5.6",
        (ModelTier::Opus, ModelProvider::Google) => "gemini-3.1-pro-preview",
        (ModelTier::Sonnet, ModelProvider::Anthropic) => "claude-sonnet-5",
        (ModelTier::Sonnet, ModelProvider::OpenAi) => "gpt-4o",
        (ModelTier::Sonnet, ModelProvider::OpenAiCodex) => "gpt-5.6",
        (ModelTier::Sonnet, ModelProvider::Google) => "gemini-flash-latest",
        (ModelTier::Haiku, ModelProvider::Anthropic) => "claude-haiku-4-5-20251001",
        (ModelTier::Haiku, ModelProvider::OpenAi) => "gpt-4o-mini",
        (ModelTier::Haiku, ModelProvider::OpenAiCodex) => "gpt-5.6",
        (ModelTier::Haiku, ModelProvider::Google) => "gemini-flash-lite-latest",
    }
}

fn mode_primary_tier(mode: AgentMode) -> ModelTier {
    match mode {
        AgentMode::Low | AgentMode::Free | AgentMode::Replay => ModelTier::Haiku,
        AgentMode::Medium
        | AgentMode::High
        | AgentMode::Ultra
        | AgentMode::Smart
        | AgentMode::Frontier => ModelTier::Opus,
        AgentMode::Rush | AgentMode::Custom => ModelTier::Sonnet,
    }
}

fn mode_reasoning_effort(mode: AgentMode) -> ReasoningEffort {
    match mode {
        AgentMode::Medium | AgentMode::Smart | AgentMode::Custom => ReasoningEffort::Medium,
        AgentMode::High | AgentMode::Ultra => ReasoningEffort::XHigh,
        AgentMode::Frontier => ReasoningEffort::High,
        AgentMode::Low | AgentMode::Rush | AgentMode::Free | AgentMode::Replay => {
            ReasoningEffort::Low
        }
    }
}

fn subagent_dispatch_rule(
    mode: AgentMode,
    subagent_type: SubagentType,
) -> Option<SubagentDispatchRule> {
    let rule = match mode {
        AgentMode::Smart => match subagent_type {
            SubagentType::Explorer | SubagentType::Minimal => {
                SubagentDispatchRule::tier(ModelTier::Haiku, ReasoningEffort::Low)
            }
            SubagentType::Planner => {
                SubagentDispatchRule::tier(ModelTier::Opus, ReasoningEffort::High)
            }
            SubagentType::Coder => SubagentDispatchRule::explicit(
                ModelProvider::OpenAiCodex,
                "gpt-5.6",
                ReasoningEffort::Medium,
            ),
            SubagentType::Reviewer | SubagentType::Researcher | SubagentType::BrowserQa => {
                SubagentDispatchRule::tier(ModelTier::Sonnet, ReasoningEffort::Medium)
            }
            SubagentType::TestRunner | SubagentType::Custom => return None,
        },
        AgentMode::Rush => match subagent_type {
            SubagentType::Explorer
            | SubagentType::Reviewer
            | SubagentType::Researcher
            | SubagentType::Minimal => {
                SubagentDispatchRule::tier(ModelTier::Haiku, ReasoningEffort::Low)
            }
            SubagentType::Planner | SubagentType::Coder | SubagentType::BrowserQa => {
                SubagentDispatchRule::tier(ModelTier::Sonnet, ReasoningEffort::Low)
            }
            SubagentType::TestRunner | SubagentType::Custom => return None,
        },
        AgentMode::Free | AgentMode::Replay => match subagent_type {
            SubagentType::TestRunner | SubagentType::Custom => return None,
            _ => SubagentDispatchRule::tier(ModelTier::Haiku, ReasoningEffort::Low),
        },
        AgentMode::Frontier => match subagent_type {
            SubagentType::Explorer | SubagentType::Researcher => {
                SubagentDispatchRule::tier(ModelTier::Sonnet, ReasoningEffort::Medium)
            }
            SubagentType::Planner => {
                SubagentDispatchRule::tier(ModelTier::Opus, ReasoningEffort::XHigh)
            }
            SubagentType::Coder => SubagentDispatchRule::explicit(
                ModelProvider::OpenAiCodex,
                "gpt-5.6",
                ReasoningEffort::High,
            ),
            SubagentType::Reviewer => SubagentDispatchRule::explicit(
                ModelProvider::OpenAiCodex,
                "gpt-5.6",
                ReasoningEffort::Medium,
            ),
            SubagentType::BrowserQa => {
                SubagentDispatchRule::tier(ModelTier::Sonnet, ReasoningEffort::High)
            }
            SubagentType::Minimal => {
                SubagentDispatchRule::tier(ModelTier::Haiku, ReasoningEffort::Low)
            }
            SubagentType::TestRunner | SubagentType::Custom => return None,
        },
        AgentMode::Low
        | AgentMode::Medium
        | AgentMode::High
        | AgentMode::Ultra
        | AgentMode::Custom => return None,
    };

    Some(rule)
}

/// Resolve the concrete model/provider a subagent should use in a mode.
#[must_use]
pub fn resolve_subagent_dispatch(
    mode: AgentMode,
    subagent_type: SubagentType,
    provider: ModelProvider,
) -> ResolvedSubagentDispatch {
    if let Some(rule) = subagent_dispatch_rule(mode, subagent_type) {
        return match rule.model {
            DispatchModelRef::Tier(tier) => ResolvedSubagentDispatch {
                mode,
                subagent_type,
                provider,
                model: model_for_tier(tier, provider).to_string(),
                model_tier: Some(tier),
                reasoning_effort: rule.reasoning_effort,
                source: DispatchSource::Mode,
            },
            DispatchModelRef::Explicit {
                provider: explicit_provider,
                model,
            } => ResolvedSubagentDispatch {
                mode,
                subagent_type,
                provider: explicit_provider.unwrap_or(provider),
                model: model.to_string(),
                model_tier: None,
                reasoning_effort: rule.reasoning_effort,
                source: DispatchSource::Mode,
            },
        };
    }

    let tier = mode_primary_tier(mode);
    ResolvedSubagentDispatch {
        mode,
        subagent_type,
        provider,
        model: model_for_tier(tier, provider).to_string(),
        model_tier: Some(tier),
        reasoning_effort: mode_reasoning_effort(mode),
        source: DispatchSource::Fallback,
    }
}

/// Configuration for swarm execution
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmConfig {
    /// Maximum number of concurrent agents
    pub max_concurrency: usize,
    /// Continue executing if a task fails
    pub continue_on_failure: bool,
    /// Timeout for individual tasks (ms)
    pub task_timeout_ms: Option<u64>,
    /// Model to use for agents
    pub model: Option<String>,
    /// System prompt override for agents
    pub system_prompt: Option<String>,
    /// Agent mode used to resolve subagent model dispatch
    #[serde(default)]
    pub mode: Option<AgentMode>,
    /// Parent model provider used when dispatch falls back to a model tier
    #[serde(default)]
    pub model_provider: Option<ModelProvider>,
    /// Default subagent type for teammate tasks
    #[serde(default)]
    pub subagent_type: Option<SubagentType>,
    /// Default reasoning hint for teammate tasks
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 3,
            continue_on_failure: false,
            task_timeout_ms: Some(300_000), // 5 minutes
            model: None,
            system_prompt: None,
            mode: None,
            model_provider: None,
            subagent_type: None,
            reasoning_effort: None,
        }
    }
}

/// Event emitted during swarm execution
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SwarmEvent {
    /// Swarm started
    Started {
        plan_title: String,
        total_tasks: usize,
    },
    /// Task started
    TaskStarted {
        task_id: TaskId,
        task_title: String,
        agent_id: AgentId,
    },
    /// Task progress update
    TaskProgress { task_id: TaskId, message: String },
    /// Task completed
    TaskCompleted { task_id: TaskId, result: TaskResult },
    /// Task failed
    TaskFailed { task_id: TaskId, error: String },
    /// Swarm completed
    Completed {
        successful: usize,
        failed: usize,
        #[serde(default)]
        skipped: usize,
        duration_ms: u64,
    },
    /// Swarm cancelled
    Cancelled { reason: String },
    /// Swarm failed
    Failed { error: String },
}

/// State of the swarm execution
#[derive(Debug, Clone, Default)]
pub struct SwarmState {
    /// Current status
    pub status: SwarmStatus,
    /// The execution plan
    pub plan: SwarmPlan,
    /// Configuration
    pub config: SwarmConfig,
    /// Completed task IDs
    pub completed_tasks: HashSet<TaskId>,
    /// Accepted successful results keyed by task ID, kept in snapshot order.
    pub completed_results: BTreeMap<TaskId, TaskResult>,
    /// Failed task IDs
    pub failed_tasks: HashSet<TaskId>,
    /// Accepted failed results keyed by task ID, kept in snapshot order.
    pub failed_results: BTreeMap<TaskId, TaskResult>,
    /// Currently running tasks (`task_id` -> `agent_id`)
    pub running_tasks: HashMap<TaskId, AgentId>,
    /// Dispatch identity for every currently running task.
    pub running_dispatches: HashMap<TaskId, DispatchId>,
    /// Tasks whose interrupted effects remain unresolved after owner recovery.
    pub indeterminate_tasks: HashMap<TaskId, IndeterminateTask>,
    /// Monotonic state transition revision used by owner checkpoints.
    pub revision: u64,
    /// Lifetime task budget used by dynamic expansion and recovery.
    pub task_budget: usize,
    /// Cached graph digest reused by immutable checkpoints.
    pub(crate) graph_digest: String,
    /// Start time (unix timestamp ms)
    pub started_at: Option<u64>,
    /// Events emitted
    pub events: Vec<SwarmEvent>,
}

impl SwarmState {
    /// Create new state with a plan and config
    #[must_use]
    pub fn new(plan: SwarmPlan, config: SwarmConfig) -> Self {
        let task_budget = plan.tasks.len();
        Self {
            status: SwarmStatus::Initializing,
            plan,
            config,
            task_budget,
            ..Default::default()
        }
    }

    /// Get progress as (completed, total)
    #[must_use]
    pub fn progress(&self) -> (usize, usize) {
        let completed = self.completed_tasks.len() + self.failed_tasks.len();
        let total = self.plan.tasks.len();
        (completed, total)
    }

    /// Check if swarm is done
    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(
            self.status,
            SwarmStatus::Completed | SwarmStatus::Cancelled | SwarmStatus::Failed
        )
    }

    /// Check if can start more tasks
    #[must_use]
    pub fn can_start_more(&self) -> bool {
        self.running_tasks.len() < self.config.max_concurrency
            && !self.is_done()
            && self.status == SwarmStatus::Running
    }
}

#[cfg(test)]
mod tests;
