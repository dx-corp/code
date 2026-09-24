//! Jupyter notebook editing utilities.
//!
//! This module provides functionality for editing Jupyter notebooks (`.ipynb` files)
//! programmatically. It supports three edit modes:
//!
//! - `replace` - Replace the source content of an existing cell
//! - `insert` - Insert a new cell after a specified cell (or at the start)
//! - `delete` - Delete a cell by ID or index
//!
//! # Cell Identification
//!
//! Cells can be identified by either:
//! - `cell_id` - The unique ID stored in the cell's `id` field
//! - `cell_index` - Zero-based numeric index in the cells array
//!
//! # Notebook Creation
//!
//! If the target notebook doesn't exist and `edit_mode` is `insert` without
//! a target cell, a new notebook is created with the provided content.

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent::ToolResult;
use crate::safety::run_validators;

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
struct NotebookEditArgs {
    path: String,
    #[serde(default, alias = "cellId", alias = "cell_id")]
    cell_id: Option<String>,
    #[serde(default, alias = "cellIndex", alias = "cell_index")]
    cell_index: Option<usize>,
    #[serde(default, alias = "newSource", alias = "new_source")]
    new_source: String,
    #[serde(default, alias = "cellType", alias = "cell_type")]
    cell_type: Option<String>,
    #[serde(default, alias = "editMode", alias = "edit_mode")]
    edit_mode: Option<String>,
}

fn source_to_array(source: &str) -> Vec<Value> {
    let mut lines = source
        .split_inclusive('\n')
        .map(|line| Value::String(line.to_string()))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(Value::String(String::new()));
    }
    lines
}

fn generate_cell_id() -> String {
    let id = uuid::Uuid::new_v4().to_string();
    id.split('-').next().unwrap_or(&id).to_string()
}

fn find_cell_index(
    cells: &[Value],
    cell_id: Option<&str>,
    cell_index: Option<usize>,
) -> Result<isize, String> {
    if let Some(id) = cell_id {
        let idx = cells.iter().position(|cell| {
            cell.get("id")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s == id)
        });
        return idx
            .map(|v| v as isize)
            .ok_or_else(|| format!("Cell with ID \"{id}\" not found"));
    }
    if let Some(idx) = cell_index {
        if idx >= cells.len() {
            return Err(format!(
                "Cell index {} out of range (notebook has {} cells)",
                idx,
                cells.len()
            ));
        }
        return Ok(idx as isize);
    }
    Ok(-1)
}

fn build_cell(cell_type: &str, source: &str) -> Value {
    let mut cell = serde_json::Map::new();
    cell.insert("id".to_string(), Value::String(generate_cell_id()));
    cell.insert(
        "cell_type".to_string(),
        Value::String(cell_type.to_string()),
    );
    cell.insert("source".to_string(), Value::Array(source_to_array(source)));
    cell.insert(
        "metadata".to_string(),
        Value::Object(serde_json::Map::new()),
    );
    if cell_type == "code" {
        cell.insert("execution_count".to_string(), Value::Null);
        cell.insert("outputs".to_string(), Value::Array(Vec::new()));
    }
    Value::Object(cell)
}

pub async fn notebook_edit_with_cancellation(
    raw_args: Value,
    cwd: &str,
    cancellation: Option<&CancellationToken>,
    sandbox_policy: Option<&crate::sandbox::SandboxPolicy>,
) -> ToolResult {
    let parsed: NotebookEditArgs = match serde_json::from_value(raw_args) {
        Ok(val) => val,
        Err(err) => return ToolResult::failure(format!("Invalid notebook_edit args: {err}")),
    };

    let edit_mode = parsed.edit_mode.unwrap_or_else(|| "replace".to_string());

    let path = {
        let raw = parsed.path.trim();
        if raw.is_empty() {
            return ToolResult::failure("Missing path argument".to_string());
        }
        if std::path::Path::new(raw).is_absolute() {
            raw.to_string()
        } else {
            std::path::Path::new(cwd)
                .join(raw)
                .to_string_lossy()
                .to_string()
        }
    };

    if !path.to_lowercase().ends_with(".ipynb") {
        return ToolResult::failure(format!("File must be a Jupyter notebook (.ipynb): {path}"));
    }

    let path_buf = std::path::PathBuf::from(&path);

    // If file doesn't exist and insert without target, create new notebook
    if !path_buf.exists()
        && edit_mode == "insert"
        && parsed.cell_id.is_none()
        && parsed.cell_index.is_none()
    {
        let cell_type = parsed.cell_type.unwrap_or_else(|| "code".to_string());
        let notebook = json!({
            "cells": [build_cell(&cell_type, &parsed.new_source)],
            "metadata": {
                "kernelspec": {
                    "display_name": "Python 3",
                    "language": "python",
                    "name": "python3"
                }
            },
            "nbformat": 4,
            "nbformat_minor": 5
        });
        if !begin_mutation_commit(cancellation).await {
            return ToolResult::failure("notebook_edit cancelled")
                .with_details(json!({"cancelled": true}));
        }
        // Atomic check-and-write (see `sandbox::commit_native_write`).
        if let Err(err) = crate::sandbox::commit_native_write(
            sandbox_policy,
            std::path::Path::new(cwd),
            &path_buf,
            serde_json::to_string_pretty(&notebook).unwrap().as_bytes(),
        ) {
            return ToolResult::failure(err);
        }

        let details = json!({
            "cellIndex": 0,
            "mode": "create",
            "totalCells": 1
        });
        return ToolResult::success(format!(
            "Created new notebook with 1 {cell_type} cell: {path}"
        ))
        .with_details(details);
    }

    let content = match tokio::fs::read_to_string(&path_buf).await {
        Ok(text) => text,
        Err(err) => {
            return ToolResult::failure(format!("Failed to read notebook: {err}"));
        }
    };

    let mut notebook: Value = match serde_json::from_str(&content) {
        Ok(val) => val,
        Err(err) => return ToolResult::failure(format!("Failed to parse notebook: {err}")),
    };

    let cells = notebook
        .get_mut("cells")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| ToolResult::failure("Invalid notebook format: missing cells".to_string()));

    let cells = match cells {
        Ok(val) => val,
        Err(err) => return err,
    };

    let target_index = match find_cell_index(cells, parsed.cell_id.as_deref(), parsed.cell_index) {
        Ok(idx) => idx,
        Err(err) => return ToolResult::failure(err),
    };

    let (result_cell_id, mode) = match edit_mode.as_str() {
        "replace" => {
            if target_index < 0 {
                return ToolResult::failure(
                    "Must specify cell_id or cell_index for replace mode".to_string(),
                );
            }
            let idx = target_index as usize;
            let existing_type = cells[idx]
                .get("cell_type")
                .and_then(|v| v.as_str())
                .unwrap_or("code");
            let cell_type = parsed
                .cell_type
                .clone()
                .unwrap_or_else(|| existing_type.to_string());
            let mut new_cell = cells[idx].clone();
            if let Some(obj) = new_cell.as_object_mut() {
                obj.insert("cell_type".to_string(), Value::String(cell_type));
                obj.insert(
                    "source".to_string(),
                    Value::Array(source_to_array(&parsed.new_source)),
                );
            }
            let result_cell_id = new_cell
                .get("id")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string);
            cells[idx] = new_cell;
            (result_cell_id, "replace")
        }
        "insert" => {
            let cell_type = parsed
                .cell_type
                .clone()
                .unwrap_or_else(|| "code".to_string());
            let new_cell = build_cell(&cell_type, &parsed.new_source);
            let insert_at = if target_index < 0 {
                0
            } else {
                (target_index as usize) + 1
            };
            cells.insert(insert_at, new_cell.clone());
            let result_cell_id = new_cell
                .get("id")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string);
            (result_cell_id, "insert")
        }
        "delete" => {
            if target_index < 0 {
                return ToolResult::failure(
                    "Must specify cell_id or cell_index for delete mode".to_string(),
                );
            }
            let idx = target_index as usize;
            if idx >= cells.len() {
                return ToolResult::failure("Cell index out of range".to_string());
            }
            let removed = cells.remove(idx);
            let result_cell_id = removed
                .get("id")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string);
            (result_cell_id, "delete")
        }
        _ => {
            return ToolResult::failure(
                "Invalid edit_mode (use replace, insert, delete)".to_string(),
            );
        }
    };

    let total_cells = cells.len();

    let updated = serde_json::to_string_pretty(&notebook).unwrap_or_else(|_| content.clone());
    if !begin_mutation_commit(cancellation).await {
        return ToolResult::failure("notebook_edit cancelled")
            .with_details(json!({"cancelled": true}));
    }
    // Atomic check-and-write (see `sandbox::commit_native_write`).
    if let Err(err) = crate::sandbox::commit_native_write(
        sandbox_policy,
        std::path::Path::new(cwd),
        &path_buf,
        updated.as_bytes(),
    ) {
        return ToolResult::failure(err);
    }

    if let Err(err) = run_validators(std::slice::from_ref(&path)).await {
        return ToolResult::failure(err);
    }

    let details = json!({
        "cellIndex": if target_index < 0 { 0 } else { target_index },
        "cellId": result_cell_id,
        "mode": mode,
        "totalCells": total_cells
    });

    ToolResult::success(format!(
        "Notebook updated ({mode}). Total cells: {total_cells}"
    ))
    .with_details(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // NotebookEditArgs Deserialization Tests
    // ========================================================================

    #[test]
    fn test_args_deserialize_minimal() {
        let json = serde_json::json!({
            "path": "notebook.ipynb",
            "new_source": "print('hello')"
        });
        let args: NotebookEditArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.path, "notebook.ipynb");
        assert_eq!(args.new_source, "print('hello')");
        assert!(args.cell_id.is_none());
        assert!(args.cell_index.is_none());
        assert!(args.cell_type.is_none());
        assert!(args.edit_mode.is_none());
    }

    #[test]
    fn test_args_deserialize_snake_case_aliases() {
        let json = serde_json::json!({
            "path": "test.ipynb",
            "cell_id": "abc123",
            "new_source": "x = 1",
            "cell_type": "code",
            "edit_mode": "replace"
        });
        let args: NotebookEditArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.cell_id, Some("abc123".to_string()));
        assert_eq!(args.cell_type, Some("code".to_string()));
        assert_eq!(args.edit_mode, Some("replace".to_string()));
    }

    #[test]
    fn test_args_deserialize_camel_case_aliases() {
        let json = serde_json::json!({
            "path": "test.ipynb",
            "cellId": "xyz789",
            "cellIndex": 5,
            "newSource": "y = 2",
            "cellType": "markdown",
            "editMode": "insert"
        });
        let args: NotebookEditArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.cell_id, Some("xyz789".to_string()));
        assert_eq!(args.cell_index, Some(5));
        assert_eq!(args.new_source, "y = 2");
        assert_eq!(args.cell_type, Some("markdown".to_string()));
        assert_eq!(args.edit_mode, Some("insert".to_string()));
    }

    // ========================================================================
    // source_to_array Tests
    // ========================================================================

    #[test]
    fn test_source_to_array_single_line() {
        let result = source_to_array("print('hello')");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Value::String("print('hello')".to_string()));
    }

    #[test]
    fn test_source_to_array_multiple_lines() {
        let result = source_to_array("line1\nline2\nline3");
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], Value::String("line1\n".to_string()));
        assert_eq!(result[1], Value::String("line2\n".to_string()));
        assert_eq!(result[2], Value::String("line3".to_string()));
    }

    #[test]
    fn test_source_to_array_empty() {
        let result = source_to_array("");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Value::String(String::new()));
    }

    #[test]
    fn test_source_to_array_preserves_trailing_newline() {
        let result = source_to_array("hello\n");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Value::String("hello\n".to_string()));
    }

    // ========================================================================
    // generate_cell_id Tests
    // ========================================================================

    #[test]
    fn test_generate_cell_id_is_short() {
        let id = generate_cell_id();
        // Should be the first segment of a UUID (8 hex chars)
        assert!(id.len() <= 8, "Cell ID should be short: {}", id);
        assert!(
            !id.contains('-'),
            "Cell ID should not contain dashes: {}",
            id
        );
    }

    #[test]
    fn test_generate_cell_id_unique() {
        let id1 = generate_cell_id();
        let id2 = generate_cell_id();
        assert_ne!(id1, id2, "Generated IDs should be unique");
    }

    // ========================================================================
    // find_cell_index Tests
    // ========================================================================

    #[test]
    fn test_find_cell_index_by_id() {
        let cells = vec![
            json!({"id": "cell-a", "cell_type": "code"}),
            json!({"id": "cell-b", "cell_type": "markdown"}),
            json!({"id": "cell-c", "cell_type": "code"}),
        ];

        let result = find_cell_index(&cells, Some("cell-b"), None);
        assert_eq!(result, Ok(1));
    }

    #[test]
    fn test_find_cell_index_by_id_not_found() {
        let cells = vec![json!({"id": "cell-a", "cell_type": "code"})];

        let result = find_cell_index(&cells, Some("nonexistent"), None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_find_cell_index_by_index() {
        let cells = vec![
            json!({"id": "cell-a", "cell_type": "code"}),
            json!({"id": "cell-b", "cell_type": "code"}),
        ];

        let result = find_cell_index(&cells, None, Some(1));
        assert_eq!(result, Ok(1));
    }

    #[test]
    fn test_find_cell_index_by_index_out_of_range() {
        let cells = vec![json!({"id": "cell-a", "cell_type": "code"})];

        let result = find_cell_index(&cells, None, Some(5));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("out of range"));
    }

    #[test]
    fn test_find_cell_index_no_target() {
        let cells = vec![json!({"id": "cell-a", "cell_type": "code"})];

        let result = find_cell_index(&cells, None, None);
        assert_eq!(result, Ok(-1));
    }

    #[test]
    fn test_find_cell_index_id_takes_precedence() {
        let cells = vec![
            json!({"id": "cell-a", "cell_type": "code"}),
            json!({"id": "cell-b", "cell_type": "code"}),
        ];

        // Both ID and index provided, ID should take precedence
        let result = find_cell_index(&cells, Some("cell-b"), Some(0));
        assert_eq!(result, Ok(1)); // Found by ID "cell-b" which is at index 1
    }

    // ========================================================================
    // build_cell Tests
    // ========================================================================

    #[test]
    fn test_build_cell_code() {
        let cell = build_cell("code", "x = 1");

        assert!(cell.get("id").is_some());
        assert_eq!(cell.get("cell_type").unwrap(), "code");
        assert!(cell.get("source").unwrap().is_array());
        assert!(cell.get("metadata").unwrap().is_object());
        assert!(cell.get("execution_count").is_some());
        assert!(cell.get("outputs").unwrap().is_array());
    }

    #[test]
    fn test_build_cell_markdown() {
        let cell = build_cell("markdown", "# Header");

        assert_eq!(cell.get("cell_type").unwrap(), "markdown");
        assert!(cell.get("source").unwrap().is_array());
        // Markdown cells don't have execution_count or outputs
        assert!(cell.get("execution_count").is_none());
        assert!(cell.get("outputs").is_none());
    }

    #[test]
    fn test_build_cell_source_format() {
        let cell = build_cell("code", "line1\nline2");

        let source = cell.get("source").unwrap().as_array().unwrap();
        assert_eq!(source.len(), 2);
        assert_eq!(source[0], Value::String("line1\n".to_string()));
        assert_eq!(source[1], Value::String("line2".to_string()));
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_find_cell_index_empty_cells() {
        let cells: Vec<Value> = vec![];

        let result = find_cell_index(&cells, None, Some(0));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("out of range"));
    }

    #[test]
    fn test_source_to_array_unicode() {
        let result = source_to_array("print('こんにちは')\nprint('🚀')");
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0],
            Value::String("print('こんにちは')\n".to_string())
        );
        assert_eq!(result[1], Value::String("print('🚀')".to_string()));
    }
}
