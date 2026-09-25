//! Preparation and dispatch integrity for native prompts.
use crate::{Message, RequestConfig};
use anyhow::{Result, ensure};
pub use maestro_runtime_contracts::cache_topology::{CacheTopology, CacheTransition};
use maestro_runtime_contracts::cache_topology::{PromptShape, digest};

/// Markers chosen before sealing. Indexes are canonical history positions, never the volatile tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundaryPlan {
    pub record_id: &'static str,
    pub history_indexes: Vec<usize>,
    pub mark_system: bool,
    pub mark_tools: bool,
    pub history_ttl: Option<&'static str>,
    /// GPT-5.6+ Responses explicit-only mode. False unless a breakpoint can sit before the volatile tail.
    pub openai_explicit_breakpoint: bool,
    finalized: bool,
}

impl Default for BoundaryPlan {
    fn default() -> Self {
        Self {
            record_id: "unfinalized",
            history_indexes: Vec::new(),
            mark_system: false,
            mark_tools: false,
            history_ttl: None,
            openai_explicit_breakpoint: false,
            finalized: false,
        }
    }
}

impl BoundaryPlan {
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.finalized
    }
}

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
    /// Compatible earlier checkpoint for this same route, including after A→B→A.
    /// This does not change `topology.transition`.
    prior_history_index: Option<usize>,
    prior_tools_system: bool,
    boundary: BoundaryPlan,
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
            prior_history_index: None,
            prior_tools_system: false,
            boundary: BoundaryPlan::default(),
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
            prior_history_index: None,
            prior_tools_system: false,
            boundary: BoundaryPlan::default(),
        })
    }

    /// Remember a same-route checkpoint without relabeling the provenance transition.
    /// A history rewrite keeps a tools/system prefix only when that prefix was materialized
    /// and its digests still match. A different instruction or a reordered history does not match.
    pub fn note_prior_route(
        &mut self,
        prior: &CacheTopology,
        tools_system_materialized: bool,
        messages: &[Message],
    ) -> Result<()> {
        ensure!(
            !self.boundary.finalized,
            "sealed cache boundary cannot change"
        );
        if prior.shape.namespace != self.topology.shape.namespace
            || prior.shape.model != self.topology.shape.model
        {
            return Ok(());
        }
        let same_tools = prior.shape.tools == self.topology.shape.tools;
        let same_instructions = prior.shape.instructions == self.topology.shape.instructions;
        if tools_system_materialized && same_tools && same_instructions {
            self.prior_tools_system = true;
        }
        let current: Vec<String> = messages.iter().map(digest).collect();
        if same_tools
            && same_instructions
            && prior.shape.thinking == self.topology.shape.thinking
            && !prior.shape.history.is_empty()
            && prior.shape.history.len() < current.len()
            && current.starts_with(&prior.shape.history)
        {
            let index = prior.shape.history.len() - 1;
            if self
                .prior_history_index
                .is_none_or(|existing| index < existing)
            {
                self.prior_history_index = Some(index);
            }
        }
        Ok(())
    }

    /// Fix marker policy from the sourced capability record. A second call is rejected.
    pub fn finalize_boundary(
        &mut self,
        provider: Option<&str>,
        model: &str,
        cache_system_prompt: bool,
        has_system: bool,
        has_tools: bool,
        messages_len: usize,
    ) -> Result<()> {
        ensure!(
            !self.boundary.finalized,
            "sealed cache boundary cannot change"
        );
        self.boundary = plan_boundaries(BoundaryInputs {
            provider,
            model,
            topology: &self.topology,
            cache_system_prompt,
            has_system,
            has_tools,
            messages_len,
            has_volatile_tail: self.volatile_tail.is_some(),
            previous_checkpoint: self.previous_checkpoint,
            prior_history_index: self.prior_history_index,
        });
        Ok(())
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
        if self.boundary.finalized {
            return self;
        }
        self.volatile_tail = tail.filter(|tail| !tail.trim().is_empty());
        self
    }

    #[must_use]
    pub fn boundary(&self) -> &BoundaryPlan {
        &self.boundary
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

/// `MAESTRO_EXPLICIT_CACHE_BOUNDARIES=1` opts into new explicit cache-write
/// boundaries. Unset or any other value keeps today's request bytes.
#[must_use]
pub fn explicit_cache_boundaries_enabled() -> bool {
    std::env::var("MAESTRO_EXPLICIT_CACHE_BOUNDARIES")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

/// Canonical history index after `transform_messages_for_target` drops empty
/// messages and inserts synthetic tool results. This is the same translation
/// the previous Anthropic checkpoint used.
pub(crate) fn wire_message_index(
    messages: &[crate::Message],
    canonical: usize,
    target: crate::transform::OutboundTarget,
) -> Option<usize> {
    if canonical >= messages.len() {
        return None;
    }
    crate::transform::transform_messages_for_target(&messages[..=canonical], target)
        .len()
        .checked_sub(1)
}

pub(crate) fn mark_history_indexes(body: &mut serde_json::Value, ttl: &str, indexes: &[usize]) {
    let marker = serde_json::json!({"type":"ephemeral", "ttl":ttl});
    let Some(messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    let mut planned: Vec<usize> = indexes
        .iter()
        .copied()
        .filter(|index| *index < messages.len())
        .collect();
    planned.sort_unstable();
    planned.dedup();
    for (nth, index) in planned.iter().copied().enumerate() {
        let floor = if nth == 0 {
            0
        } else {
            planned[nth - 1].saturating_add(1)
        };
        for message in messages[floor..=index].iter_mut().rev() {
            if mark_message(message, &marker) {
                break;
            }
        }
    }
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

fn take_marker(slots: &mut usize, indexes: &mut Vec<usize>, index: usize, messages_len: usize) {
    if *slots == 0 || indexes.contains(&index) || index >= messages_len {
        return;
    }
    indexes.push(index);
    *slots -= 1;
}

struct BoundaryInputs<'a> {
    provider: Option<&'a str>,
    model: &'a str,
    topology: &'a CacheTopology,
    cache_system_prompt: bool,
    has_system: bool,
    has_tools: bool,
    messages_len: usize,
    has_volatile_tail: bool,
    previous_checkpoint: Option<usize>,
    prior_history_index: Option<usize>,
}

fn plan_boundaries(input: BoundaryInputs<'_>) -> BoundaryPlan {
    let BoundaryInputs {
        provider,
        model,
        topology,
        cache_system_prompt,
        has_system,
        has_tools,
        messages_len,
        has_volatile_tail,
        previous_checkpoint,
        prior_history_index,
    } = input;
    use crate::{
        CacheBehavior, CacheBoundaryKind, allows_openai_explicit_breakpoint, cache_capability,
    };
    if topology.transition == CacheTransition::Auxiliary {
        return BoundaryPlan {
            record_id: "auxiliary",
            finalized: true,
            ..BoundaryPlan::default()
        };
    }
    let capability = cache_capability(provider, model);
    let newest = messages_len.checked_sub(1);
    let anthropic_explicit = cache_system_prompt
        && capability.behavior == CacheBehavior::Explicit
        && capability
            .boundary_kinds
            .contains(&CacheBoundaryKind::AnthropicCacheControl);
    // A history rewrite invalidates old history checkpoints. It does not stop
    // this request from writing the current tools/system prefix. Eligibility of
    // an earlier prefix is recorded on the checkpoint, not a reason to omit the marker.
    let mark_system = anthropic_explicit && has_system;
    let mark_tools = anthropic_explicit && has_tools;
    let mut slots = usize::from(capability.max_explicit_markers.unwrap_or(0));
    if mark_system {
        slots = slots.saturating_sub(1);
    }
    if mark_tools {
        slots = slots.saturating_sub(1);
    }
    let mut history_indexes = Vec::new();
    if anthropic_explicit {
        if let Some(index) = newest {
            take_marker(&mut slots, &mut history_indexes, index, messages_len);
        }
        if topology.transition != CacheTransition::HistoryRewritten {
            if let Some(index) = previous_checkpoint {
                take_marker(&mut slots, &mut history_indexes, index, messages_len);
            }
        }
        // A→B→A may reuse A's checkpoint. Append already recorded that index as
        // previous_checkpoint, and a rewrite must not pretend the old history matches.
        if !matches!(
            topology.transition,
            CacheTransition::HistoryRewritten | CacheTransition::Append
        ) {
            if let Some(index) = prior_history_index {
                take_marker(&mut slots, &mut history_indexes, index, messages_len);
            }
        }
    }
    let openai_explicit =
        has_volatile_tail && allows_openai_explicit_breakpoint(provider, model) && newest.is_some();
    if openai_explicit {
        // Not a documented provider maximum. It only stops one request from
        // asking for an unbounded number of breakpoints.
        const OPENAI_PLANNER_BREAKPOINT_BUDGET: u8 = 4;
        slots = usize::from(
            capability
                .max_explicit_markers
                .unwrap_or(OPENAI_PLANNER_BREAKPOINT_BUDGET),
        );
        history_indexes.clear();
        if let Some(index) = newest {
            take_marker(&mut slots, &mut history_indexes, index, messages_len);
        }
        if let Some(index) = previous_checkpoint {
            take_marker(&mut slots, &mut history_indexes, index, messages_len);
        }
        if topology.transition != CacheTransition::Append {
            if let Some(index) = prior_history_index {
                take_marker(&mut slots, &mut history_indexes, index, messages_len);
            }
        }
    }
    history_indexes.sort_unstable();
    BoundaryPlan {
        record_id: capability.record_id,
        history_indexes,
        mark_system,
        mark_tools,
        history_ttl: anthropic_explicit.then_some("5m"),
        openai_explicit_breakpoint: openai_explicit,
        finalized: true,
    }
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
        assert_ne!(
            auxiliary.topology().transition,
            CacheTransition::Append,
            "an auxiliary summary must not take the primary transition"
        );
    }

    fn finalize(
        prepared: &mut PreparedPrompt,
        provider: &str,
        model: &str,
        config: &RequestConfig,
        len: usize,
    ) {
        prepared
            .finalize_boundary(
                Some(provider),
                model,
                config.cache_system_prompt,
                config.system.is_some(),
                !config.tools.is_empty(),
                len,
            )
            .unwrap();
    }

    #[test]
    fn prefix_dependency_rejects_reordered_or_reinstructed_history() {
        let mut config = RequestConfig {
            cache_system_prompt: true,
            system: Some("instruction-a".into()),
            ..Default::default()
        };
        let first_messages = vec![user("document")];
        let first =
            PreparedPrompt::prepare(&first_messages, &config, "session".into(), None).unwrap();
        config.system = Some("instruction-b".into());
        let mut second = PreparedPrompt::prepare(
            &first_messages,
            &config,
            "session".into(),
            Some(first.topology()),
        )
        .unwrap();
        second
            .note_prior_route(first.topology(), true, &first_messages)
            .unwrap();
        assert_eq!(second.prior_history_index, None);
        assert!(!second.prior_tools_system);

        config.system = Some("instruction-a".into());
        let reordered = vec![user("document"), user("preface")];
        // Identical document text after a different preceding message is not the same prefix.
        let prefix = PreparedPrompt::prepare(&reordered, &config, "session".into(), None).unwrap();
        let swapped = vec![user("preface"), user("document"), user("extra")];
        let mut other =
            PreparedPrompt::prepare(&swapped, &config, "session".into(), Some(prefix.topology()))
                .unwrap();
        other
            .note_prior_route(prefix.topology(), true, &swapped)
            .unwrap();
        assert_eq!(
            other.prior_history_index, None,
            "a longer history with a different leading message is not the same prefix"
        );
        let extended = vec![user("document"), user("preface"), user("extra")];
        let mut compatible = PreparedPrompt::prepare(
            &extended,
            &config,
            "session".into(),
            Some(prefix.topology()),
        )
        .unwrap();
        compatible
            .note_prior_route(prefix.topology(), true, &extended)
            .unwrap();
        assert_eq!(compatible.prior_history_index, Some(1));
    }

    #[test]
    fn volatile_tail_preserves_an_explicit_stable_boundary() {
        let config = RequestConfig {
            model: "gpt-5.6".into(),
            ..Default::default()
        };
        let history = vec![user("stable")];
        let mut first = PreparedPrompt::prepare(&history, &config, "session".into(), None)
            .unwrap()
            .with_volatile_tail(Some("clock: 1".into()));
        finalize(&mut first, "openai", "gpt-5.6", &config, history.len());
        let mut next =
            PreparedPrompt::prepare(&history, &config, "session".into(), Some(first.topology()))
                .unwrap()
                .with_volatile_tail(Some("clock: 2".into()));
        finalize(&mut next, "openai", "gpt-5.6", &config, history.len());
        assert_eq!(next.topology().generation, first.topology().generation);
        assert_eq!(next.topology().transition, CacheTransition::Append);
        assert_eq!(next.boundary().history_indexes, vec![0]);
        assert!(next.boundary().openai_explicit_breakpoint);
        assert!(!next.boundary().history_indexes.contains(&1));
    }

    #[test]
    fn compaction_keeps_only_a_materialized_tools_prefix() {
        let config = RequestConfig {
            cache_system_prompt: true,
            system: Some("standing".into()),
            tools: std::sync::Arc::new(vec![crate::Tool::new("read", "Read")]),
            ..Default::default()
        };
        let history = vec![user("a"), user("b")];
        let first = PreparedPrompt::prepare(&history, &config, "session".into(), None).unwrap();
        let rewritten = vec![user("summary"), user("b")];
        let mut after = PreparedPrompt::prepare(
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
        after
            .note_prior_route(first.topology(), false, &rewritten)
            .unwrap();
        finalize(
            &mut after,
            "anthropic",
            "claude-sonnet-4-5",
            &config,
            rewritten.len(),
        );
        assert_eq!(after.boundary().history_indexes, vec![rewritten.len() - 1]);
        assert!(after.boundary().mark_system && after.boundary().mark_tools);
        assert!(!after.prior_tools_system);
        assert_eq!(after.prior_history_index, None);
        assert_eq!(
            after.topology().transition,
            CacheTransition::HistoryRewritten
        );

        let mut carried = PreparedPrompt::prepare(
            &rewritten,
            &config,
            "session".into(),
            Some(first.topology()),
        )
        .unwrap();
        carried
            .note_prior_route(first.topology(), true, &rewritten)
            .unwrap();
        finalize(
            &mut carried,
            "anthropic",
            "claude-sonnet-4-5",
            &config,
            rewritten.len(),
        );
        assert!(carried.boundary().mark_system);
        assert!(carried.boundary().mark_tools);
        assert_eq!(carried.prior_history_index, None);
        assert_eq!(
            carried.topology().transition,
            CacheTransition::HistoryRewritten
        );
    }

    #[test]
    fn returning_route_checkpoint_is_not_relabeled_append() {
        let config = RequestConfig::default();
        let route_a = vec![user("a1"), user("a2")];
        let mut model_a = config.clone();
        model_a.model = "route-a".into();
        model_a.cache_system_prompt = true;
        let mut first =
            PreparedPrompt::prepare(&route_a, &model_a, "session".into(), None).unwrap();
        finalize(
            &mut first,
            "anthropic",
            "claude-sonnet-4-5",
            &model_a,
            route_a.len(),
        );
        let mut model_b = model_a.clone();
        model_b.model = "route-b".into();
        let on_b =
            PreparedPrompt::prepare(&route_a, &model_b, "session".into(), Some(first.topology()))
                .unwrap();
        assert_eq!(on_b.topology().transition, CacheTransition::ModelChanged);
        let continued = vec![user("a1"), user("a2"), user("a3")];
        let mut back = PreparedPrompt::prepare(
            &continued,
            &model_a,
            "session".into(),
            Some(on_b.topology()),
        )
        .unwrap();
        assert_eq!(back.topology().transition, CacheTransition::ModelChanged);
        back.note_prior_route(first.topology(), true, &continued)
            .unwrap();
        assert_eq!(back.prior_history_index, Some(1));
        finalize(
            &mut back,
            "anthropic",
            "claude-sonnet-4-5",
            &model_a,
            continued.len(),
        );
        assert_eq!(back.topology().transition, CacheTransition::ModelChanged);
        assert!(back.boundary().history_indexes.contains(&1));
        assert!(back.boundary().history_indexes.contains(&2));
        assert_eq!(back.boundary().history_indexes.len(), 2);
    }

    #[test]
    fn mutation_of_a_sealed_boundary_fails() {
        let mut config = RequestConfig {
            model: "gpt-5.6".into(),
            ..Default::default()
        };
        let messages = vec![user("stable")];
        let mut prepared = PreparedPrompt::prepare(&messages, &config, "session".into(), None)
            .unwrap()
            .with_volatile_tail(Some("clock".into()));
        finalize(&mut prepared, "openai", "gpt-5.6", &config, messages.len());
        let prior = prepared.topology().clone();
        assert!(prepared.note_prior_route(&prior, true, &messages).is_err());
        assert!(
            prepared
                .finalize_boundary(Some("openai"), "gpt-5.6", true, true, true, 1)
                .is_err()
        );
        let tail = prepared.volatile_tail().unwrap().to_string();
        let sealed = prepared.with_volatile_tail(Some("replaced".into()));
        assert_eq!(sealed.volatile_tail(), Some(tail.as_str()));
        config.system = Some("changed policy".into());
        assert!(sealed.validate(&messages, &config).is_err());
    }

    #[test]
    fn different_namespace_does_not_inherit_a_checkpoint() {
        let messages = vec![user("same")];
        let config = RequestConfig::default();
        let tenant_a =
            PreparedPrompt::prepare(&messages, &config, "tenant-a".into(), None).unwrap();
        let mut tenant_b =
            PreparedPrompt::prepare(&messages, &config, "tenant-b".into(), None).unwrap();
        tenant_b
            .note_prior_route(tenant_a.topology(), true, &messages)
            .unwrap();
        assert_eq!(tenant_b.prior_history_index, None);
        assert!(tenant_a.validate_namespace("tenant-b").is_err());
    }
}
