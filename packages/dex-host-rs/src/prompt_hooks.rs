//! Prompt admission through a Maestro host's hooks, before a dex-loop turn
//! starts.
//!
//! The native actor runs `UserPromptSubmit` and then `PreMessage` on every
//! prompt: either may block it, rewrite it (a string, or an object with
//! `message`/`prompt` and `attachments`), or inject context that joins the
//! system prompt. A kernel turn runs the same two hooks the same way, so a
//! user's hooks keep working when their turns move onto the kernel.

use maestro_runtime::agent::native_host::{NativeExecutionHostHandle, NativeHookResult};
use serde_json::Value;

/// A prompt the host's hooks admitted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AdmittedPrompt {
    pub prompt: String,
    pub attachments: Vec<String>,
    /// Context the hooks injected, for the system prompt.
    pub context: Option<String>,
}

fn rewrite(admitted: &mut AdmittedPrompt, new_input: Value) {
    match new_input {
        Value::String(text) => admitted.prompt = text,
        Value::Object(map) => {
            if let Some(Value::String(text)) = map.get("message").or_else(|| map.get("prompt")) {
                admitted.prompt = text.clone();
            }
            if let Some(Value::Array(items)) = map.get("attachments") {
                admitted.attachments = items
                    .iter()
                    .map(|item| match item {
                        Value::String(value) => value.clone(),
                        other => other.to_string(),
                    })
                    .collect();
            }
        }
        _ => {}
    }
}

fn inject(admitted: &mut AdmittedPrompt, context: String) {
    if context.trim().is_empty() {
        return;
    }
    match &mut admitted.context {
        Some(existing) => {
            existing.push('\n');
            existing.push_str(&context);
        }
        None => admitted.context = Some(context),
    }
}

/// Runs `UserPromptSubmit` then `PreMessage`. `Err` carries the message the
/// native actor reports for a blocked prompt.
pub async fn admit_prompt(
    host: &NativeExecutionHostHandle,
    prompt: String,
    attachments: Vec<String>,
    model: &str,
) -> Result<AdmittedPrompt, String> {
    let mut admitted = AdmittedPrompt {
        prompt,
        attachments,
        context: None,
    };
    let count = u32::try_from(admitted.attachments.len()).unwrap_or(u32::MAX);
    match host.hook_user_prompt_submit(&admitted.prompt, count).await {
        NativeHookResult::Block { reason } => {
            return Err(format!("Prompt blocked by hook: {reason}"));
        }
        NativeHookResult::ModifyInput { new_input } => rewrite(&mut admitted, new_input),
        NativeHookResult::InjectContext { context } => inject(&mut admitted, context),
        NativeHookResult::Continue => {}
    }
    match host
        .hook_pre_message(&admitted.prompt, &admitted.attachments, Some(model))
        .await
    {
        NativeHookResult::Block { reason } => {
            return Err(format!("Message blocked by hook: {reason}"));
        }
        NativeHookResult::ModifyInput { new_input } => rewrite(&mut admitted, new_input),
        NativeHookResult::InjectContext { context } => inject(&mut admitted, context),
        NativeHookResult::Continue => {}
    }
    Ok(admitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_follow_the_native_actor() {
        let mut admitted = AdmittedPrompt {
            prompt: "a".into(),
            attachments: vec!["x".into()],
            context: None,
        };
        rewrite(&mut admitted, Value::String("b".into()));
        assert_eq!(admitted.prompt, "b");
        rewrite(
            &mut admitted,
            serde_json::json!({"prompt": "c", "attachments": ["y", 1]}),
        );
        assert_eq!(admitted.prompt, "c");
        assert_eq!(admitted.attachments, vec!["y".to_owned(), "1".to_owned()]);
        inject(&mut admitted, "one".into());
        inject(&mut admitted, "  ".into());
        inject(&mut admitted, "two".into());
        assert_eq!(admitted.context.as_deref(), Some("one\ntwo"));
    }
}
