use serde_json::{Map, Value};
use url::Url;

fn get_string_arg(args: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(value) = args.get(*key) else {
            continue;
        };
        if let Some(text) = value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return Some(text.to_string());
        }
        if let Some(array) = value.as_array() {
            if let Some(text) = array
                .iter()
                .find_map(|item| item.as_str().map(str::trim).filter(|text| !text.is_empty()))
            {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_label(value: &str, max: usize) -> String {
    let normalized = normalize_whitespace(value);
    let char_count = normalized.chars().count();
    if char_count <= max {
        return normalized;
    }

    let truncated = normalized
        .chars()
        .take(max.saturating_sub(3))
        .collect::<String>()
        .trim_end()
        .to_string();
    format!("{truncated}...")
}

fn quote_label(value: &str, max: usize) -> String {
    format!("\"{}\"", truncate_label(value, max))
}

fn short_url_label(raw: &str) -> String {
    if let Ok(parsed) = Url::parse(raw) {
        let path = if parsed.path() == "/" {
            ""
        } else {
            parsed.path()
        };
        if let Some(host) = parsed.host_str() {
            return truncate_label(&format!("{host}{path}"), 40);
        }
    }

    truncate_label(raw, 40)
}

fn short_path_label(raw: &str) -> String {
    let normalized = raw.trim().replace('\\', "/");
    if normalized.is_empty() {
        return maestro_ui::localization::tr("file").to_string();
    }
    if normalized.contains("://") {
        return short_url_label(&normalized);
    }
    if normalized == "." || normalized == ".." {
        return normalized;
    }

    let is_directory = normalized.ends_with('/');
    let trimmed = normalized.trim_end_matches('/');
    let parts = trimmed
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let leaf = parts.last().copied().unwrap_or(&normalized);
    format!(
        "{}{}",
        truncate_label(leaf, 32),
        if is_directory { "/" } else { "" }
    )
}

fn replace_tool_separators(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_was_space = false;

    for ch in value.chars() {
        if matches!(ch, '.' | '_' | '-') || ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }

    out.trim().to_string()
}

fn humanize_tool_name(tool_name: &str) -> String {
    let trimmed = tool_name.trim();
    if trimmed.is_empty() {
        return maestro_ui::localization::tr("tool").to_string();
    }

    let mcp_parts = trimmed
        .split("__")
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if trimmed.starts_with("mcp__") && mcp_parts.len() >= 3 {
        return replace_tool_separators(&mcp_parts[2..].join(" "));
    }

    replace_tool_separators(trimmed)
}

fn sentence_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.collect::<String>()),
        None => String::new(),
    }
}

fn summarize_known_tool(
    tool_name: &str,
    args: &Map<String, Value>,
    completed: bool,
) -> Option<String> {
    let tense = |past: &'static str, present: &'static str| {
        maestro_ui::localization::tr(if completed { past } else { present })
    };
    let normalized = tool_name.trim().to_lowercase();
    let file_path = get_string_arg(
        args,
        &[
            "file_path",
            "filePath",
            "path",
            "target_path",
            "targetPath",
            "filename",
        ],
    );
    let directory = get_string_arg(args, &["directory", "dir", "cwd"]);
    let pattern = get_string_arg(args, &["pattern", "query", "search", "regex"]);
    let command = get_string_arg(args, &["command", "cmd", "script"]);
    let url = get_string_arg(args, &["url", "uri"]);

    match normalized.as_str() {
        "read" => Some(maestro_ui::localization::format(
            "Read {0}",
            &[short_path_label(
                file_path
                    .as_deref()
                    .unwrap_or(maestro_ui::localization::tr("file")),
            )
            .clone()],
        )),
        "write" | "append" | "create_file" | "createfile" => Some(format!(
            "{} {}",
            tense("Wrote", "Write"),
            short_path_label(
                file_path
                    .as_deref()
                    .unwrap_or(maestro_ui::localization::tr("file"))
            )
        )),
        "edit" | "multi_edit" | "str_replace_based_edit" | "apply_patch" => Some(format!(
            "{} {}",
            tense("Edited", "Edit"),
            short_path_label(
                file_path
                    .as_deref()
                    .unwrap_or(maestro_ui::localization::tr("file"))
            )
        )),
        "delete" | "remove" | "unlink" => Some(format!(
            "{} {}",
            tense("Deleted", "Delete"),
            short_path_label(
                file_path
                    .as_deref()
                    .unwrap_or(maestro_ui::localization::tr("file"))
            )
        )),
        "list" | "ls" => Some(format!(
            "{} {}",
            tense("Listed", "List"),
            short_path_label(
                directory
                    .as_deref()
                    .or(file_path.as_deref())
                    .unwrap_or(maestro_ui::localization::tr("directory"))
            )
        )),
        "glob" => Some(if let Some(pattern) = pattern {
            format!(
                "{} {}",
                tense("Matched", "Match"),
                quote_label(&pattern, 32)
            )
        } else {
            format!(
                "{} {}",
                tense("Scanned", "Scan"),
                short_path_label(
                    directory
                        .as_deref()
                        .unwrap_or(maestro_ui::localization::tr("workspace"))
                )
            )
        }),
        "grep" | "search" | "search_files" => Some(if let Some(pattern) = pattern {
            maestro_ui::localization::format(
                if completed {
                    "Searched for {0}"
                } else {
                    "Search for {0}"
                },
                &[quote_label(&pattern, 32)],
            )
        } else {
            tense("Searched files", "Search files").to_string()
        }),
        "bash" | "shell" | "exec_command" => Some(if let Some(command) = command {
            format!("{} {}", tense("Ran", "Run"), truncate_label(&command, 52))
        } else {
            tense("Ran command", "Run command").to_string()
        }),
        "webfetch" | "fetch" | "open" => Some(format!(
            "{} {}",
            tense("Fetched", "Fetch"),
            short_url_label(
                url.as_deref()
                    .or(file_path.as_deref())
                    .unwrap_or(maestro_ui::localization::tr("resource"))
            )
        )),
        "websearch" | "search_query" => Some(if let Some(pattern) = pattern {
            maestro_ui::localization::format(
                "{0} web for {1}",
                &[
                    (tense("Searched", "Search")).to_string(),
                    (quote_label(&pattern, 32)).clone(),
                ],
            )
        } else {
            tense("Searched web", "Search web").to_string()
        }),
        "todo" => Some(maestro_ui::localization::format(
            "{0} task list",
            &[(tense("Updated", "Update")).to_string()],
        )),
        "batch" => {
            let calls = args
                .get("tool_uses")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
                .max(
                    args.get("calls")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len),
                );

            Some(if calls > 0 {
                maestro_ui::localization::format(
                    match (completed, calls == 1) {
                        (true, true) => "Ran {0} tool call",
                        (true, false) => "Ran {0} tool calls",
                        (false, true) => "Run {0} tool call",
                        (false, false) => "Run {0} tool calls",
                    },
                    &[calls.to_string()],
                )
            } else {
                maestro_ui::localization::format(
                    "{0} tool batch",
                    &[(tense("Ran", "Run")).to_string()],
                )
            })
        }
        "background_tasks" => {
            let action = get_string_arg(args, &["action"]);
            Some(match action.as_deref() {
                Some("start") => maestro_ui::localization::format(
                    "{0} background task",
                    &[(tense("Started", "Start")).to_string()],
                ),
                Some("stop") => maestro_ui::localization::format(
                    "{0} background task",
                    &[(tense("Stopped", "Stop")).to_string()],
                ),
                Some("logs") => maestro_ui::localization::format(
                    "{0} background logs",
                    &[(tense("Viewed", "View")).to_string()],
                ),
                Some("list") => maestro_ui::localization::format(
                    "{0} background tasks",
                    &[(tense("Listed", "List")).to_string()],
                ),
                _ => maestro_ui::localization::format(
                    "{0} background tasks",
                    &[(tense("Checked", "Check")).to_string()],
                ),
            })
        }
        _ => None,
    }
}

#[must_use]
pub fn summarize_tool_use(tool_name: &str, args: &Value) -> String {
    if let Some(object) = args.as_object() {
        if let Some(known) = summarize_known_tool(tool_name, object, true) {
            return sentence_case(&known);
        }
    }

    sentence_case(&maestro_ui::localization::format(
        "Ran {0}",
        &[truncate_label(&humanize_tool_name(tool_name), 40).clone()],
    ))
}

/// Describe an action without claiming it has already executed.
#[must_use]
pub fn summarize_tool_intent(tool_name: &str, args: &Value) -> String {
    if let Some(object) = args.as_object() {
        if let Some(known) = summarize_known_tool(tool_name, object, false) {
            return sentence_case(&known);
        }
    }
    maestro_ui::localization::format(
        "Run {0}",
        &[truncate_label(&humanize_tool_name(tool_name), 40).clone()],
    )
}

#[cfg(test)]
mod tests {
    use super::{summarize_tool_intent, summarize_tool_use};
    use serde_json::json;

    #[test]
    fn intent_does_not_claim_pending_actions_have_executed() {
        for (tool, args, intent, completed) in [
            (
                "bash",
                json!({"command":"printf hello"}),
                "Run printf hello",
                "Ran printf hello",
            ),
            (
                "write",
                json!({"path":"README.md"}),
                "Write README.md",
                "Wrote README.md",
            ),
            (
                "delete",
                json!({"path":"old.txt"}),
                "Delete old.txt",
                "Deleted old.txt",
            ),
        ] {
            assert_eq!(summarize_tool_intent(tool, &args), intent);
            assert_eq!(summarize_tool_use(tool, &args), completed);
        }
    }

    #[test]
    fn summarizes_file_reads_by_basename() {
        assert_eq!(
            summarize_tool_use(
                "read",
                &json!({
                    "file_path": "/Users/jonathanhaas/Documents/Projects/maestro/package.json"
                }),
            ),
            "Read package.json"
        );
    }

    #[test]
    fn summarizes_bash_commands_with_the_command_text() {
        assert_eq!(
            summarize_tool_use(
                "bash",
                &json!({
                    "command": "npm test -- --runInBand"
                }),
            ),
            "Ran npm test -- --runInBand"
        );
    }

    #[test]
    fn summarizes_search_style_tools_with_quoted_patterns() {
        assert_eq!(
            summarize_tool_use(
                "grep",
                &json!({
                    "pattern": "TODO: tighten this logic"
                }),
            ),
            "Searched for \"TODO: tighten this logic\""
        );
    }

    #[test]
    fn summarizes_web_search_queries() {
        assert_eq!(
            summarize_tool_use(
                "search_query",
                &json!({
                    "query": "maestro nx monorepo"
                }),
            ),
            "Searched web for \"maestro nx monorepo\""
        );
    }

    #[test]
    fn falls_back_to_a_humanized_tool_name() {
        assert_eq!(
            summarize_tool_use("mcp__github__search_issues", &json!({})),
            "Ran search issues"
        );
    }
}
