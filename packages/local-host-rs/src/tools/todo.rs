//! Todo tool for tracking multi-step tasks.
//!
//! This module provides a persistent todo list for the agent to track
//! progress on multi-step tasks. Todos are organized by goal and stored
//! in a JSON file.
//!
//! # Features
//!
//! - Create and update todo items with status, priority, notes, and due dates
//! - Track dependencies between items (`blocked_by`)
//! - Persistent storage in `~/.composer/todos.json`
//! - Progress summary with pending/in-progress/completed counts

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::agent::ToolResult;

async fn begin_mutation_commit(cancellation: Option<&CancellationToken>) -> bool {
    match cancellation {
        Some(token) => {
            tokio::select! {
                biased;
                () = token.cancelled() => false,
                () = std::future::ready(()) => true,
            }
        }
        None => true,
    }
}

#[derive(Debug, Deserialize)]
struct TodoArgs {
    goal: String,
    #[serde(default)]
    items: Option<serde_json::Value>,
    #[serde(default)]
    updates: Option<Vec<TodoUpdateInput>>,
    #[serde(default, alias = "includeSummary")]
    include_summary: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TodoItemInput {
    #[serde(default)]
    id: Option<String>,
    content: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default, alias = "blockedBy")]
    blocked_by: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TodoUpdateInput {
    id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default, alias = "blockedBy")]
    blocked_by: Option<Vec<String>>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    remove: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct NormalizedTodo {
    id: String,
    content: String,
    status: String,
    priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    due: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "blockedBy")]
    blocked_by: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TodoRecord {
    goal: String,
    items: Vec<NormalizedTodo>,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

type TodoStore = HashMap<String, TodoRecord>;

fn store_path() -> PathBuf {
    if let Ok(path) = std::env::var("MAESTRO_TODO_FILE") {
        return PathBuf::from(path);
    }
    dirs::home_dir().map_or_else(
        || std::env::temp_dir().join("composer-todos.json"),
        |home| home.join(".composer").join("todos.json"),
    )
}

async fn load_store() -> Result<TodoStore, String> {
    let path = store_path();
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("Failed to read todo store: {e}"))?;
    let store: TodoStore = serde_json::from_str(&content).unwrap_or_else(|_| HashMap::new());
    Ok(store)
}

async fn save_store(store: &TodoStore) -> Result<(), String> {
    let path = store_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create todo dir: {e}"))?;
    }
    let data = serde_json::to_string_pretty(store)
        .map_err(|e| format!("Failed to serialize todo store: {e}"))?;
    tokio::fs::write(&path, format!("{data}\n"))
        .await
        .map_err(|e| format!("Failed to write todo store: {e}"))?;
    Ok(())
}

fn normalize_items(items: Vec<TodoItemInput>) -> Vec<NormalizedTodo> {
    items
        .into_iter()
        .map(|item| NormalizedTodo {
            id: item.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            content: item.content,
            status: item.status.unwrap_or_else(|| "pending".to_string()),
            priority: item.priority.unwrap_or_else(|| "medium".to_string()),
            notes: item.notes,
            due: item.due,
            blocked_by: item.blocked_by,
        })
        .collect()
}

fn apply_updates(
    items: &mut Vec<NormalizedTodo>,
    updates: Vec<TodoUpdateInput>,
) -> Result<(), String> {
    for update in updates {
        let idx = items
            .iter()
            .position(|item| item.id == update.id)
            .ok_or_else(|| format!("No task found with id \"{}\" for this goal", update.id))?;
        if update.remove.unwrap_or(false) {
            items.remove(idx);
            continue;
        }
        let item = &mut items[idx];
        if let Some(status) = update.status {
            item.status = status;
        }
        if let Some(priority) = update.priority {
            item.priority = priority;
        }
        if let Some(notes) = update.notes {
            item.notes = Some(notes);
        }
        if let Some(due) = update.due {
            item.due = Some(due);
        }
        if let Some(blocked) = update.blocked_by {
            item.blocked_by = Some(blocked);
        }
        if let Some(content) = update.content {
            item.content = content;
        }
    }
    Ok(())
}

fn status_symbol(status: &str) -> &str {
    match status {
        "completed" => "[x]",
        "in_progress" => "[~]",
        _ => "[ ]",
    }
}

fn format_output(goal: &str, items: &[NormalizedTodo], include_summary: bool) -> String {
    let mut lines = vec![
        "Goal".to_string(),
        "------------------------".to_string(),
        goal.to_string(),
        String::new(),
    ];

    if include_summary {
        let total = items.len();
        let pending = items.iter().filter(|i| i.status == "pending").count();
        let in_progress = items.iter().filter(|i| i.status == "in_progress").count();
        let completed = items.iter().filter(|i| i.status == "completed").count();
        lines.push("Progress".to_string());
        lines.push("------------------------".to_string());
        if total == 0 {
            lines.push("No tasks yet - add items to get started.".to_string());
        } else {
            lines.push(format!(
                "Total: {total} | Pending: {pending} | In Progress: {in_progress} | Completed: {completed}"
            ));
        }
        lines.push(String::new());
    }

    if items.is_empty() {
        lines.push("Tasks".to_string());
        lines.push("────────────────────────".to_string());
        lines.push("No tasks available.".to_string());
        return lines.join("\n");
    }

    lines.push("Tasks".to_string());
    lines.push("------------------------".to_string());
    for item in items {
        lines.push(format!(
            "{} {} (priority: {})",
            status_symbol(&item.status),
            item.content,
            item.priority
        ));
        if let Some(notes) = &item.notes {
            lines.push(format!("    notes: {notes}"));
        }
        if let Some(due) = &item.due {
            lines.push(format!("    due: {due}"));
        }
        if let Some(blocked) = &item.blocked_by {
            lines.push(format!("    blocked by: {}", blocked.join(", ")));
        }
    }
    lines.join("\n")
}

/// Count the items `format_output` rendered as not completed.
///
/// The `todo` tool returns prose, not JSON, so this reads back what
/// [`format_output`] wrote: `status_symbol` renders `[x]` for a completed
/// item and `[ ]` or `[~]` for everything else. Keeping the reader next to
/// the writer is what makes the pair testable; the test below fails if
/// either side changes alone.
#[must_use]
pub fn open_todo_count_from_output(output: &str) -> usize {
    output
        .lines()
        .filter(|line| line.starts_with("[ ] ") || line.starts_with("[~] "))
        .count()
}

pub async fn todo_with_cancellation(
    args: serde_json::Value,
    cancellation: Option<&CancellationToken>,
) -> ToolResult {
    let parsed: TodoArgs = match serde_json::from_value(args) {
        Ok(val) => val,
        Err(err) => return ToolResult::failure(format!("Invalid todo arguments: {err}")),
    };

    let mut store = match load_store().await {
        Ok(store) => store,
        Err(err) => return ToolResult::failure(err),
    };

    let include_summary = parsed.include_summary.unwrap_or(true);
    let goal = parsed.goal.clone();
    let mut record = store.remove(&goal).unwrap_or(TodoRecord {
        goal: goal.clone(),
        items: Vec::new(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    });

    if let Some(items_value) = parsed.items {
        let items_vec: Vec<TodoItemInput> = if items_value.is_string() {
            match serde_json::from_str(items_value.as_str().unwrap_or("[]")) {
                Ok(items) => items,
                Err(err) => return ToolResult::failure(format!("Invalid items JSON: {err}")),
            }
        } else {
            match serde_json::from_value(items_value) {
                Ok(items) => items,
                Err(err) => return ToolResult::failure(format!("Invalid items: {err}")),
            }
        };
        record.items = normalize_items(items_vec);
    }

    if let Some(updates) = parsed.updates {
        if let Err(err) = apply_updates(&mut record.items, updates) {
            return ToolResult::failure(err);
        }
    }

    record.updated_at = chrono::Utc::now().to_rfc3339();
    store.insert(goal.clone(), record);

    if !begin_mutation_commit(cancellation).await {
        return ToolResult::failure("todo cancelled")
            .with_details(serde_json::json!({"cancelled": true}));
    }
    if let Err(err) = save_store(&store).await {
        return ToolResult::failure(err);
    }

    let record = store.get(&goal).cloned().unwrap_or(TodoRecord {
        goal: goal.clone(),
        items: Vec::new(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    });

    let output = format_output(&goal, &record.items, include_summary);
    let details = serde_json::json!({
        "goal": goal,
        "items": record.items,
        "updatedAt": record.updated_at
    });

    ToolResult::success(output).with_details(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    static TODO_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn open_todo_count_matches_what_format_output_rendered() {
        let items = vec![
            NormalizedTodo {
                id: "1".to_string(),
                content: "read the handler".to_string(),
                status: "completed".to_string(),
                priority: "medium".to_string(),
                notes: None,
                due: None,
                blocked_by: None,
            },
            NormalizedTodo {
                id: "2".to_string(),
                content: "write the test".to_string(),
                status: "in_progress".to_string(),
                priority: "high".to_string(),
                notes: Some("notes should not be counted".to_string()),
                due: None,
                blocked_by: None,
            },
            NormalizedTodo {
                id: "3".to_string(),
                content: "land the fix".to_string(),
                status: "pending".to_string(),
                priority: "low".to_string(),
                notes: None,
                due: None,
                blocked_by: Some(vec!["2".to_string()]),
            },
        ];
        let rendered = format_output("ship it", &items, true);
        assert_eq!(open_todo_count_from_output(&rendered), 2);
    }

    #[test]
    fn open_todo_count_is_zero_for_a_finished_list() {
        let items = vec![NormalizedTodo {
            id: "1".to_string(),
            content: "done".to_string(),
            status: "completed".to_string(),
            priority: "medium".to_string(),
            notes: None,
            due: None,
            blocked_by: None,
        }];
        let rendered = format_output("ship it", &items, true);
        assert_eq!(open_todo_count_from_output(&rendered), 0);
    }

    #[test]
    fn open_todo_count_is_zero_for_an_empty_list() {
        let rendered = format_output("ship it", &[], true);
        assert_eq!(open_todo_count_from_output(&rendered), 0);
    }

    // ========================================================================
    // TodoArgs Deserialization Tests
    // ========================================================================

    #[test]
    fn test_todo_args_minimal() {
        let json = serde_json::json!({"goal": "Implement feature X"});
        let args: TodoArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.goal, "Implement feature X");
        assert!(args.items.is_none());
        assert!(args.updates.is_none());
    }

    #[test]
    fn test_todo_args_with_items() {
        let json = serde_json::json!({
            "goal": "Test goal",
            "items": [
                {"content": "Task 1"},
                {"content": "Task 2", "status": "completed"}
            ]
        });
        let args: TodoArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.goal, "Test goal");
        assert!(args.items.is_some());
    }

    #[test]
    fn test_todo_args_include_summary_alias() {
        let json = serde_json::json!({
            "goal": "Test",
            "includeSummary": false
        });
        let args: TodoArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.include_summary, Some(false));
    }

    // ========================================================================
    // TodoItemInput Deserialization Tests
    // ========================================================================

    #[test]
    fn test_todo_item_input_minimal() {
        let json = serde_json::json!({"content": "Do something"});
        let item: TodoItemInput = serde_json::from_value(json).unwrap();
        assert_eq!(item.content, "Do something");
        assert!(item.id.is_none());
        assert!(item.status.is_none());
        assert!(item.priority.is_none());
    }

    #[test]
    fn test_todo_item_input_full() {
        let json = serde_json::json!({
            "id": "task-1",
            "content": "Complete implementation",
            "status": "in_progress",
            "priority": "high",
            "notes": "Important!",
            "due": "2024-12-31",
            "blockedBy": ["task-0"]
        });
        let item: TodoItemInput = serde_json::from_value(json).unwrap();
        assert_eq!(item.id, Some("task-1".to_string()));
        assert_eq!(item.content, "Complete implementation");
        assert_eq!(item.status, Some("in_progress".to_string()));
        assert_eq!(item.priority, Some("high".to_string()));
        assert_eq!(item.notes, Some("Important!".to_string()));
        assert_eq!(item.due, Some("2024-12-31".to_string()));
        assert_eq!(item.blocked_by, Some(vec!["task-0".to_string()]));
    }

    // ========================================================================
    // TodoUpdateInput Deserialization Tests
    // ========================================================================

    #[test]
    fn test_todo_update_input_status_only() {
        let json = serde_json::json!({
            "id": "task-1",
            "status": "completed"
        });
        let update: TodoUpdateInput = serde_json::from_value(json).unwrap();
        assert_eq!(update.id, "task-1");
        assert_eq!(update.status, Some("completed".to_string()));
        assert!(update.remove.is_none());
    }

    #[test]
    fn test_todo_update_input_remove() {
        let json = serde_json::json!({
            "id": "task-to-remove",
            "remove": true
        });
        let update: TodoUpdateInput = serde_json::from_value(json).unwrap();
        assert_eq!(update.id, "task-to-remove");
        assert_eq!(update.remove, Some(true));
    }

    // ========================================================================
    // normalize_items Tests
    // ========================================================================

    #[test]
    fn test_normalize_items_empty() {
        let items: Vec<TodoItemInput> = vec![];
        let normalized = normalize_items(items);
        assert!(normalized.is_empty());
    }

    #[test]
    fn test_normalize_items_defaults() {
        let items = vec![TodoItemInput {
            id: None,
            content: "Test task".to_string(),
            status: None,
            priority: None,
            notes: None,
            due: None,
            blocked_by: None,
        }];
        let normalized = normalize_items(items);
        assert_eq!(normalized.len(), 1);
        assert!(!normalized[0].id.is_empty()); // UUID generated
        assert_eq!(normalized[0].content, "Test task");
        assert_eq!(normalized[0].status, "pending");
        assert_eq!(normalized[0].priority, "medium");
    }

    #[test]
    fn test_normalize_items_preserves_values() {
        let items = vec![TodoItemInput {
            id: Some("my-id".to_string()),
            content: "Custom task".to_string(),
            status: Some("completed".to_string()),
            priority: Some("high".to_string()),
            notes: Some("Note".to_string()),
            due: Some("2024-01-01".to_string()),
            blocked_by: Some(vec!["other".to_string()]),
        }];
        let normalized = normalize_items(items);
        assert_eq!(normalized[0].id, "my-id");
        assert_eq!(normalized[0].status, "completed");
        assert_eq!(normalized[0].priority, "high");
        assert_eq!(normalized[0].notes, Some("Note".to_string()));
    }

    // ========================================================================
    // apply_updates Tests
    // ========================================================================

    #[test]
    fn test_apply_updates_status_change() {
        let mut items = vec![NormalizedTodo {
            id: "task-1".to_string(),
            content: "Test".to_string(),
            status: "pending".to_string(),
            priority: "medium".to_string(),
            notes: None,
            due: None,
            blocked_by: None,
        }];
        let updates = vec![TodoUpdateInput {
            id: "task-1".to_string(),
            status: Some("completed".to_string()),
            priority: None,
            notes: None,
            due: None,
            blocked_by: None,
            content: None,
            remove: None,
        }];
        apply_updates(&mut items, updates).unwrap();
        assert_eq!(items[0].status, "completed");
    }

    #[test]
    fn test_apply_updates_remove() {
        let mut items = vec![
            NormalizedTodo {
                id: "task-1".to_string(),
                content: "Keep".to_string(),
                status: "pending".to_string(),
                priority: "medium".to_string(),
                notes: None,
                due: None,
                blocked_by: None,
            },
            NormalizedTodo {
                id: "task-2".to_string(),
                content: "Remove".to_string(),
                status: "pending".to_string(),
                priority: "medium".to_string(),
                notes: None,
                due: None,
                blocked_by: None,
            },
        ];
        let updates = vec![TodoUpdateInput {
            id: "task-2".to_string(),
            status: None,
            priority: None,
            notes: None,
            due: None,
            blocked_by: None,
            content: None,
            remove: Some(true),
        }];
        apply_updates(&mut items, updates).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "task-1");
    }

    #[test]
    fn test_apply_updates_not_found() {
        let mut items = vec![];
        let updates = vec![TodoUpdateInput {
            id: "nonexistent".to_string(),
            status: Some("completed".to_string()),
            priority: None,
            notes: None,
            due: None,
            blocked_by: None,
            content: None,
            remove: None,
        }];
        let result = apply_updates(&mut items, updates);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No task found"));
    }

    // ========================================================================
    // status_symbol Tests
    // ========================================================================

    #[test]
    fn test_status_symbol_completed() {
        assert_eq!(status_symbol("completed"), "[x]");
    }

    #[test]
    fn test_status_symbol_in_progress() {
        assert_eq!(status_symbol("in_progress"), "[~]");
    }

    #[test]
    fn test_status_symbol_pending() {
        assert_eq!(status_symbol("pending"), "[ ]");
    }

    #[test]
    fn test_status_symbol_unknown() {
        assert_eq!(status_symbol("unknown"), "[ ]");
    }

    // ========================================================================
    // format_output Tests
    // ========================================================================

    #[test]
    fn test_format_output_empty() {
        let output = format_output("Test Goal", &[], false);
        assert!(output.contains("Test Goal"));
        assert!(output.contains("No tasks available"));
    }

    #[test]
    fn test_format_output_with_items() {
        let items = vec![NormalizedTodo {
            id: "1".to_string(),
            content: "Do something".to_string(),
            status: "pending".to_string(),
            priority: "high".to_string(),
            notes: None,
            due: None,
            blocked_by: None,
        }];
        let output = format_output("My Goal", &items, false);
        assert!(output.contains("My Goal"));
        assert!(output.contains("[ ] Do something"));
        assert!(output.contains("priority: high"));
    }

    #[test]
    fn test_format_output_with_summary() {
        let items = vec![
            NormalizedTodo {
                id: "1".to_string(),
                content: "Task 1".to_string(),
                status: "completed".to_string(),
                priority: "medium".to_string(),
                notes: None,
                due: None,
                blocked_by: None,
            },
            NormalizedTodo {
                id: "2".to_string(),
                content: "Task 2".to_string(),
                status: "pending".to_string(),
                priority: "medium".to_string(),
                notes: None,
                due: None,
                blocked_by: None,
            },
        ];
        let output = format_output("Goal", &items, true);
        assert!(output.contains("Progress"));
        assert!(output.contains("Total: 2"));
        assert!(output.contains("Completed: 1"));
    }

    #[test]
    fn test_format_output_with_notes_and_due() {
        let items = vec![NormalizedTodo {
            id: "1".to_string(),
            content: "Important task".to_string(),
            status: "in_progress".to_string(),
            priority: "high".to_string(),
            notes: Some("Remember this".to_string()),
            due: Some("2024-12-31".to_string()),
            blocked_by: Some(vec!["other-task".to_string()]),
        }];
        let output = format_output("Goal", &items, false);
        assert!(output.contains("[~] Important task"));
        assert!(output.contains("notes: Remember this"));
        assert!(output.contains("due: 2024-12-31"));
        assert!(output.contains("blocked by: other-task"));
    }

    // ========================================================================
    // store_path Tests
    // ========================================================================

    #[test]
    fn test_store_path_default() {
        let _guard = TODO_ENV_LOCK.lock().expect("todo env lock");
        let previous = std::env::var("MAESTRO_TODO_FILE").ok();
        std::env::remove_var("MAESTRO_TODO_FILE");
        let path = store_path();
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("todos.json"),
            "Path should contain todos.json: {}",
            path_str
        );
        if let Some(previous) = previous {
            std::env::set_var("MAESTRO_TODO_FILE", previous);
        }
    }

    #[test]
    fn test_store_path_from_env() {
        let _guard = TODO_ENV_LOCK.lock().expect("todo env lock");
        let previous = std::env::var("MAESTRO_TODO_FILE").ok();
        std::env::set_var("MAESTRO_TODO_FILE", "/custom/path/todos.json");
        let path = store_path();
        assert_eq!(path, PathBuf::from("/custom/path/todos.json"));
        if let Some(previous) = previous {
            std::env::set_var("MAESTRO_TODO_FILE", previous);
        } else {
            std::env::remove_var("MAESTRO_TODO_FILE");
        }
    }
}
