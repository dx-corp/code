//! Preparation and dispatch integrity for native prompts.
use crate::{Message, RequestConfig};
use anyhow::{Result, ensure};
pub use maestro_runtime_contracts::cache_topology::{CacheTopology, CacheTransition};
use maestro_runtime_contracts::cache_topology::{PromptShape, digest};

#[derive(Clone, Debug)]
pub struct PreparedPrompt {
    topology: CacheTopology,
    affinity: Option<String>,
    volatile_tail: Option<String>,
    /// Index of the last history message the previous primary request sent.
    /// Providers only look back a bounded number of blocks from each explicit
    /// cache marker, so a long tool loop appended since the last request can
    /// push the previous cache write out of reach of the newest marker. A
    /// second marker at this index pins the prefix the previous request
    /// already paid to write.
    previous_checkpoint: Option<usize>,
}

impl PreparedPrompt {
    pub fn prepare(
        messages: &[Message],
        config: &RequestConfig,
        namespace: String,
        previous: Option<&CacheTopology>,
    ) -> Result<Self> {
        let affinity = std::env::var("MAESTRO_OPENROUTER_PROMPT_CACHE_KEY")
            .ok()
            .filter(|key| !key.is_empty() && key.len() <= 256);
        let topology = CacheTopology::prepare(
            shape(messages, config, namespace, affinity.as_deref()),
            previous,
        )
        .map_err(anyhow::Error::msg)?;
        let previous_checkpoint = previous
            .filter(|_| topology.transition == CacheTransition::Append)
            .map(|previous| previous.shape.history.len())
            .filter(|len| *len > 0 && *len < messages.len())
            .map(|len| len - 1);
        Ok(Self {
            topology,
            affinity,
            volatile_tail: None,
            previous_checkpoint,
        })
    }
    /// Standalone summaries carry no explicit cache hints and never advance the primary checkpoint.
    pub fn auxiliary(
        messages: &[Message],
        config: &RequestConfig,
        namespace: String,
    ) -> Result<Self> {
        ensure!(
            !config.cache_system_prompt,
            "auxiliary request must disable explicit cache markers"
        );
        let mut topology = CacheTopology::prepare(shape(messages, config, namespace, None), None)
            .map_err(anyhow::Error::msg)?;
        topology.transition = CacheTransition::Auxiliary;
        Ok(Self {
            topology,
            affinity: None,
            volatile_tail: None,
            previous_checkpoint: None,
        })
    }

    /// Index of the message that closed the previous primary request, when the
    /// current request appends to that history.
    pub fn previous_checkpoint(&self) -> Option<usize> {
        self.previous_checkpoint
    }
    /// The tail is owned by the prepared request, so dispatch cannot read a
    /// newer clock, plan, listing, voice setting, or custom instruction. It is
    /// serialized after history, outside the reusable prefix identity.
    pub fn with_volatile_tail(mut self, tail: Option<String>) -> Self {
        self.volatile_tail = tail.filter(|tail| !tail.trim().is_empty());
        self
    }

    pub fn volatile_tail(&self) -> Option<&str> {
        self.volatile_tail.as_deref()
    }

    pub(crate) fn append_volatile_tail(&self, body: &mut serde_json::Value) {
        let Some(tail) = &self.volatile_tail else {
            return;
        };
        let field = if body.get("input").is_some() {
            "input"
        } else {
            "messages"
        };
        if let Some(messages) = body[field].as_array_mut() {
            messages.push(serde_json::json!({"role":"user", "content": tail}));
        }
    }

    pub(crate) fn affinity(&self) -> Option<&str> {
        self.affinity.as_deref()
    }
    pub fn topology(&self) -> &CacheTopology {
        &self.topology
    }
    pub fn validate(&self, messages: &[Message], config: &RequestConfig) -> Result<()> {
        self.topology
            .validate(&shape(
                messages,
                config,
                self.topology.shape.namespace.clone(),
                self.affinity.as_deref(),
            ))
            .map_err(anyhow::Error::msg)
    }
    pub fn validate_namespace(&self, namespace: &str) -> Result<()> {
        ensure!(
            self.topology.shape.namespace == namespace,
            "cache topology restore scope mismatch"
        );
        Ok(())
    }
}

fn shape(
    messages: &[Message],
    config: &RequestConfig,
    namespace: String,
    affinity: Option<&str>,
) -> PromptShape {
    PromptShape {
        namespace,
        model: digest(&config.model),
        instructions: digest(&config.system),
        tools: digest(config.tools.as_ref()),
        thinking: digest(&config.thinking),
        cache_policy: digest(&(config.cache_system_prompt, affinity)),
        history: messages.iter().map(digest).collect(),
    }
}

pub(crate) fn messages_with_volatile_tail<'a>(
    messages: &'a [Message],
    config: &RequestConfig,
) -> std::borrow::Cow<'a, [Message]> {
    let Some(tail) = config
        .cache_topology
        .as_ref()
        .and_then(PreparedPrompt::volatile_tail)
    else {
        return std::borrow::Cow::Borrowed(messages);
    };
    let mut request = messages.to_vec();
    request.push(Message {
        role: crate::Role::User,
        content: crate::MessageContent::text(tail),
    });
    std::borrow::Cow::Owned(request)
}

/// Shape each primary checkpoint, including the first request after compaction.
/// Call before appending the volatile tail and before attesting provider bytes.
///
/// Marks the newest stable message and, when `previous_checkpoint` names an
/// earlier message, that message too, so the walking cache always has a marker
/// at the boundary the previous request wrote.
pub(crate) fn mark_stable_history(
    body: &mut serde_json::Value,
    ttl: &str,
    previous_checkpoint: Option<usize>,
) {
    let marker = serde_json::json!({"type":"ephemeral", "ttl":ttl});
    let Some(messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    let newest = messages
        .iter_mut()
        .rposition(|message| mark_message(message, &marker));
    if let Some(index) = previous_checkpoint.filter(|index| Some(*index) < newest) {
        for message in messages[..=index].iter_mut().rev() {
            if mark_message(message, &marker) {
                break;
            }
        }
    }
}

/// Attach `marker` to the last cacheable block of one message; returns whether
/// a block was marked.
fn mark_message(message: &mut serde_json::Value, marker: &serde_json::Value) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
        *content = serde_json::json!([{"type":"text", "text":text, "cache_control":marker}]);
        return true;
    }
    if let Some(block) = content.as_array_mut().and_then(|blocks| {
        blocks.iter_mut().rev().find(|block| {
            !matches!(
                block.get("type").and_then(serde_json::Value::as_str),
                Some("thinking" | "redacted_thinking")
            ) && block.is_object()
        })
    }) {
        block["cache_control"] = marker.clone();
        return true;
    }
    false
}

pub(crate) fn validate_prepared(messages: &[Message], config: &RequestConfig) -> Result<()> {
    if let Some(prepared) = &config.cache_topology {
        prepared.validate(messages, config)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compacted_checkpoint_is_marked_before_the_volatile_tail() {
        let config = RequestConfig::default();
        let old = vec![Message {
            role: crate::Role::User,
            content: crate::MessageContent::text("old"),
        }];
        let first = PreparedPrompt::prepare(&old, &config, "session".into(), None).unwrap();
        let checkpoint = vec![Message {
            role: crate::Role::User,
            content: crate::MessageContent::text("checkpoint"),
        }];
        let next = PreparedPrompt::prepare(
            &checkpoint,
            &config,
            "session".into(),
            Some(first.topology()),
        )
        .unwrap()
        .with_volatile_tail(Some("clock: now".into()));
        assert_eq!(next.topology().generation, 2);
        let mut body = serde_json::json!({"messages":[{"role":"user","content":"checkpoint"}]});
        assert_eq!(next.previous_checkpoint(), None);
        mark_stable_history(&mut body, "1h", next.previous_checkpoint());
        next.append_volatile_tail(&mut body);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
        assert_eq!(body["messages"][1]["content"], "clock: now");
        assert!(body["messages"][1].get("cache_control").is_none());
    }

    fn user(text: &str) -> Message {
        Message {
            role: crate::Role::User,
            content: crate::MessageContent::text(text),
        }
    }

    fn marked(body: &serde_json::Value) -> Vec<usize> {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
            })
            .map(|(index, _)| index)
            .collect()
    }

    #[test]
    fn appended_history_keeps_the_previous_checkpoint_marked() {
        let config = RequestConfig::default();
        let first_history = vec![user("a"), user("b")];
        let first =
            PreparedPrompt::prepare(&first_history, &config, "session".into(), None).unwrap();
        assert_eq!(first.previous_checkpoint(), None);

        let appended = vec![user("a"), user("b"), user("c"), user("d")];
        let next =
            PreparedPrompt::prepare(&appended, &config, "session".into(), Some(first.topology()))
                .unwrap();
        assert_eq!(next.topology().transition, CacheTransition::Append);
        assert_eq!(next.previous_checkpoint(), Some(1));

        let mut body = serde_json::json!({"messages":[
            {"role":"user","content":"a"},
            {"role":"user","content":"b"},
            {"role":"assistant","content":[{"type":"thinking","thinking":"..."},{"type":"text","text":"c"}]},
            {"role":"user","content":"d"},
        ]});
        mark_stable_history(&mut body, "5m", next.previous_checkpoint());
        assert_eq!(marked(&body), vec![1, 3]);
        assert!(
            body["messages"][2]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn unchanged_or_rewritten_history_marks_only_the_newest_message() {
        let config = RequestConfig::default();
        let history = vec![user("a"), user("b")];
        let first = PreparedPrompt::prepare(&history, &config, "session".into(), None).unwrap();
        let same =
            PreparedPrompt::prepare(&history, &config, "session".into(), Some(first.topology()))
                .unwrap();
        assert_eq!(same.previous_checkpoint(), None);

        let rewritten = vec![user("summary"), user("b"), user("c")];
        let after = PreparedPrompt::prepare(
            &rewritten,
            &config,
            "session".into(),
            Some(first.topology()),
        )
        .unwrap();
        assert_eq!(
            after.topology().transition,
            CacheTransition::HistoryRewritten
        );
        assert_eq!(after.previous_checkpoint(), None);

        let mut body = serde_json::json!({"messages":[
            {"role":"user","content":"a"},
            {"role":"user","content":"b"},
        ]});
        mark_stable_history(&mut body, "5m", Some(1));
        assert_eq!(marked(&body), vec![1]);
    }

    #[test]
    fn volatile_tail_changes_wire_bytes_without_rewriting_the_prefix() {
        let config = RequestConfig::default();
        let first = PreparedPrompt::prepare(&[], &config, "session".into(), None)
            .unwrap()
            .with_volatile_tail(Some("clock: 1; plan: a".into()));
        let next = PreparedPrompt::prepare(&[], &config, "session".into(), Some(first.topology()))
            .unwrap()
            .with_volatile_tail(Some("clock: 2; plan: b".into()));
        assert_eq!(next.topology().generation, first.topology().generation);
        let mut before = serde_json::json!({"messages":[]});
        let mut after = before.clone();
        first.append_volatile_tail(&mut before);
        next.append_volatile_tail(&mut after);
        assert_ne!(before, after);
        next.validate(&[], &config).unwrap();
        let mut mutated = config.clone();
        mutated.system = Some("changed standing instruction".into());
        assert!(next.validate(&[], &mutated).is_err());
    }

    #[test]
    fn cache_topology_rejects_post_preparation_changes() {
        let messages = vec![Message {
            role: crate::Role::User,
            content: crate::MessageContent::text("hello"),
        }];
        let mut config = RequestConfig::default();
        config.cache_topology =
            Some(PreparedPrompt::prepare(&messages, &config, "local".into(), None).unwrap());
        validate_prepared(&messages, &config).unwrap();
        config.system = Some("late injection".into());
        assert!(validate_prepared(&messages, &config).is_err());
        config.system = None;
        assert!(
            validate_prepared(
                &[Message {
                    role: crate::Role::User,
                    content: crate::MessageContent::text("changed")
                }],
                &config
            )
            .is_err()
        );
        assert!(
            config
                .cache_topology
                .as_ref()
                .unwrap()
                .validate_namespace("other-tenant")
                .is_err()
        );
    }
    #[test]
    fn cache_topology_auxiliary_is_separate_from_primary() {
        let messages = vec![];
        let config = RequestConfig::default();
        let primary = PreparedPrompt::prepare(&messages, &config, "local".into(), None).unwrap();
        let auxiliary = PreparedPrompt::auxiliary(&messages, &config, "local".into()).unwrap();
        assert_eq!(auxiliary.topology().transition, CacheTransition::Auxiliary);
        assert!(auxiliary.affinity().is_none());
        auxiliary.validate(&messages, &config).unwrap();
        let next =
            PreparedPrompt::prepare(&messages, &config, "local".into(), Some(primary.topology()))
                .unwrap();
        assert_eq!(next.topology().generation, primary.topology().generation);
    }
}
