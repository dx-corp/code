//! Provider-only conversation projection for aged tool observations.

use super::*;

/// Keep verbose observations from the current and immediately previous user
/// turn in full. Older results remain in durable history and can be recovered
/// through `recall_output`.
pub(super) const OBSERVATION_FULL_TURNS: usize = 2;

#[derive(Clone)]
struct ObservationCall {
    tool_name: String,
    context_effect: Option<NativeContextEffect>,
    turn: usize,
    sequence: usize,
    successful: bool,
}

#[derive(Clone)]
struct FrontierPoint {
    call_id: String,
    turn: usize,
    sequence: usize,
}

/// Build a provider-only projection at a user-turn boundary. Durable session
/// history and current-turn prompt-cache prefixes retain the original output.
pub(super) fn project_observation_history(
    messages: &Arc<Vec<Message>>,
    keep_recent_turns: usize,
    recall_available: bool,
    context_effect: impl Fn(&str, &Value) -> Option<NativeContextEffect>,
) -> Arc<Vec<Message>> {
    if !recall_available {
        return Arc::clone(messages);
    }
    let mut turn = 0usize;
    let mut calls = HashMap::<String, ObservationCall>::new();
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
                    let context_effect = context_effect(name, input).filter(|effect| {
                        let resource_key = match effect {
                            NativeContextEffect::Observe { resource_key }
                            | NativeContextEffect::Mutate { resource_key } => resource_key,
                        };
                        !resource_key.trim().is_empty()
                    });
                    calls.insert(
                        id.clone(),
                        ObservationCall {
                            tool_name,
                            context_effect,
                            turn,
                            sequence,
                            successful: false,
                        },
                    );
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } if !is_error.unwrap_or(false) => {
                    if let Some(call) = calls.get_mut(tool_use_id) {
                        call.successful = true;
                    }
                }
                _ => {}
            }
        }
    }

    let mut latest_observation = HashMap::<String, FrontierPoint>::new();
    let mut latest_mutation = HashMap::<String, FrontierPoint>::new();
    for (call_id, call) in &calls {
        if call.turn >= turn || !call.successful {
            continue;
        }
        let Some(effect) = &call.context_effect else {
            continue;
        };
        let (resource_key, frontier) = match effect {
            NativeContextEffect::Observe { resource_key } => {
                (resource_key, &mut latest_observation)
            }
            NativeContextEffect::Mutate { resource_key } => (resource_key, &mut latest_mutation),
        };
        let candidate = FrontierPoint {
            call_id: call_id.clone(),
            turn: call.turn,
            sequence: call.sequence,
        };
        if frontier.get(resource_key).is_none_or(|current| {
            (candidate.turn, candidate.sequence) > (current.turn, current.sequence)
        }) {
            frontier.insert(resource_key.clone(), candidate);
        }
    }

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
            if call.turn >= turn {
                continue;
            }

            let observation_resource = match &call.context_effect {
                Some(NativeContextEffect::Observe { resource_key }) => Some(resource_key),
                _ => None,
            };
            let invalidated_by = observation_resource.and_then(|resource_key| {
                latest_mutation
                    .get(resource_key)
                    .filter(|latest| (latest.turn, latest.sequence) > (call.turn, call.sequence))
            });
            let superseded_by = observation_resource.and_then(|resource_key| {
                latest_observation.get(resource_key).filter(|latest| {
                    (latest.turn, latest.sequence) > (call.turn, call.sequence)
                        && latest.call_id != *tool_use_id
                })
            });
            let age = turn.saturating_sub(call.turn);
            if let Some(latest) = invalidated_by {
                *content = format!(
                    "[Earlier observation invalidated by successful mutation `{}` at a later user-turn boundary. Use `recall_output` with id `{tool_use_id}` to retrieve it.]",
                    latest.call_id
                );
                changed = true;
            } else if let Some(latest) = superseded_by {
                *content = format!(
                    "[Earlier observation superseded by `{}` at a later user-turn boundary. Use `recall_output` with id `{tool_use_id}` to retrieve it.]",
                    latest.call_id
                );
                changed = true;
            } else if matches!(call.tool_name.as_str(), "read" | "grep" | "bash")
                && age >= keep_recent_turns
            {
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
