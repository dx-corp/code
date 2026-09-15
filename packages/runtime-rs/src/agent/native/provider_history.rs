//! Provider-only conversation projection for aged tool observations.

use super::*;

/// Keep verbose observations from the current and immediately previous user
/// turn in full. Older results remain in durable history and can be recovered
/// through `recall_output`.
pub(super) const OBSERVATION_FULL_TURNS: usize = 2;

#[derive(Clone)]
struct ObservationCall {
    tool_name: String,
    read_path: Option<String>,
    turn: usize,
    sequence: usize,
}

/// Build a provider-only projection at a user-turn boundary. Durable session
/// history and current-turn prompt-cache prefixes retain the original output.
pub(super) fn project_observation_history(
    messages: &Arc<Vec<Message>>,
    keep_recent_turns: usize,
    recall_available: bool,
) -> Arc<Vec<Message>> {
    if !recall_available {
        return Arc::clone(messages);
    }
    let mut turn = 0usize;
    let mut calls = HashMap::<String, ObservationCall>::new();
    let mut successful_results = HashSet::<String>::new();
    let mut sequence = 0usize;
    for message in messages.iter() {
        if message.role == Role::User && !is_tool_result_only_user_message(message) {
            turn = turn.saturating_add(1);
        }
        let MessageContent::Blocks(blocks) = &message.content else {
            continue;
        };
        for block in blocks {
            match block {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    sequence = sequence.saturating_add(1);
                    let tool_name = name.to_ascii_lowercase();
                    let read_path = (tool_name == "read")
                        .then(|| {
                            input
                                .get("path")
                                .or_else(|| input.get("file_path"))
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .flatten();
                    calls.insert(
                        id.clone(),
                        ObservationCall {
                            tool_name,
                            read_path,
                            turn,
                            sequence,
                        },
                    );
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } if !is_error.unwrap_or(false) => {
                    successful_results.insert(tool_use_id.clone());
                }
                _ => {}
            }
        }
    }

    let latest_completed_read = calls
        .iter()
        .filter(|(id, call)| {
            call.tool_name == "read"
                && call.read_path.is_some()
                && call.turn < turn
                && successful_results.contains(*id)
        })
        .fold(
            HashMap::<&str, (usize, usize, &str)>::new(),
            |mut latest, (id, call)| {
                let path = call.read_path.as_deref().expect("filtered read path");
                let candidate = (call.turn, call.sequence, id.as_str());
                if latest
                    .get(path)
                    .is_none_or(|current| (candidate.0, candidate.1) > (current.0, current.1))
                {
                    latest.insert(path, candidate);
                }
                latest
            },
        );

    let mut projected = messages.as_ref().clone();
    let mut changed = false;
    for message in &mut projected {
        let MessageContent::Blocks(blocks) = &mut message.content else {
            continue;
        };
        for block in blocks {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = block
            else {
                continue;
            };
            if is_error.unwrap_or(false) {
                continue;
            }
            let Some(call) = calls.get(tool_use_id) else {
                continue;
            };
            if !matches!(call.tool_name.as_str(), "read" | "grep" | "bash") {
                continue;
            }

            let superseded_by = call.read_path.as_deref().and_then(|path| {
                latest_completed_read
                    .get(path)
                    .filter(|(latest_turn, latest_sequence, latest_id)| {
                        (*latest_turn, *latest_sequence) > (call.turn, call.sequence)
                            && *latest_id != tool_use_id
                    })
                    .map(|(_, _, latest_id)| *latest_id)
            });
            let age = turn.saturating_sub(call.turn);
            if let Some(latest_id) = superseded_by {
                *content = format!(
                    "[Earlier read result superseded by `{latest_id}` at a later user-turn boundary. Use `recall_output` with id `{tool_use_id}` to retrieve it.]"
                );
                changed = true;
            } else if age >= keep_recent_turns {
                *content = format!(
                    "[Earlier {} result omitted after {age} user turns. Use `recall_output` with id `{tool_use_id}` to retrieve it.]",
                    call.tool_name
                );
                changed = true;
            }
        }
    }

    if changed {
        Arc::new(projected)
    } else {
        Arc::clone(messages)
    }
}
