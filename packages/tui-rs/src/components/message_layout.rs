use std::sync::Arc;

use crate::state::{Message, MessageKind, MessageRole};

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolCallLayoutKey {
    call_id: String,
    tool: String,
    output_len: usize,
    parent_call_id: Option<String>,
    duration_ms: Option<u64>,
    status: crate::state::ToolCallStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MessageLayoutKey {
    id: String,
    role: MessageRole,
    kind: MessageKind,
    content_len: usize,
    thinking_len: usize,
    streaming: bool,
    thinking_expanded: bool,
    tool_calls: Vec<ToolCallLayoutKey>,
}

impl MessageLayoutKey {
    fn from_message(message: &Message) -> Self {
        Self {
            id: message.id.clone(),
            role: message.role,
            kind: message.kind,
            content_len: message.content.len(),
            thinking_len: message.thinking.len(),
            streaming: message.streaming,
            thinking_expanded: message.thinking_expanded,
            tool_calls: message
                .tool_calls
                .iter()
                .map(|tool_call| ToolCallLayoutKey {
                    call_id: tool_call.call_id.clone(),
                    tool: tool_call.tool.clone(),
                    output_len: tool_call.output.len(),
                    parent_call_id: tool_call.parent_call_id.clone(),
                    duration_ms: tool_call.duration_ms,
                    status: tool_call.status,
                })
                .collect(),
        }
    }

    fn matches_message(&self, message: &Message) -> bool {
        self.id == message.id
            && self.role == message.role
            && self.kind == message.kind
            && self.content_len == message.content.len()
            && self.thinking_len == message.thinking.len()
            && self.streaming == message.streaming
            && self.thinking_expanded == message.thinking_expanded
            && self.tool_calls.len() == message.tool_calls.len()
            && self
                .tool_calls
                .iter()
                .zip(&message.tool_calls)
                .all(|(cached, current)| {
                    cached.call_id == current.call_id
                        && cached.tool == current.tool
                        && cached.output_len == current.output.len()
                        && cached.parent_call_id == current.parent_call_id
                        && cached.duration_ms == current.duration_ms
                        && cached.status == current.status
                })
    }
}

#[derive(Clone, Debug)]
struct CachedEntry {
    key: MessageLayoutKey,
    height: usize,
    previous_role_kind: Option<(MessageRole, MessageKind)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MessageLayout {
    heights: Arc<[usize]>,
    cumulative_bottoms: Arc<[usize]>,
}

impl Default for MessageLayout {
    fn default() -> Self {
        Self {
            heights: Arc::from(Vec::<usize>::new()),
            cumulative_bottoms: Arc::from(Vec::<usize>::new()),
        }
    }
}

impl MessageLayout {
    pub(crate) fn heights(&self) -> &[usize] {
        &self.heights
    }

    pub(crate) fn total_height(&self) -> usize {
        self.cumulative_bottoms.last().copied().unwrap_or_default()
    }

    pub(crate) fn first_visible(&self, window_top: usize) -> usize {
        self.cumulative_bottoms
            .partition_point(|bottom| *bottom <= window_top)
    }
}

#[derive(Debug, Default)]
pub(crate) struct MessageLayoutCache {
    width: Option<u16>,
    settings_key: u64,
    entries: Vec<CachedEntry>,
    layout: MessageLayout,
    #[cfg(test)]
    measurements: usize,
}

impl MessageLayoutCache {
    pub(crate) fn prepare_messages<F>(
        &mut self,
        width: u16,
        settings_key: u64,
        messages: &[&Message],
        mut measure: F,
    ) -> MessageLayout
    where
        F: FnMut(usize) -> usize,
    {
        let mut layout_dirty = self.width != Some(width) || self.settings_key != settings_key;
        if layout_dirty {
            self.entries.clear();
            self.width = Some(width);
            self.settings_key = settings_key;
        }

        for (index, message) in messages.iter().enumerate() {
            let previous_role_kind = index
                .checked_sub(1)
                .map(|i| (messages[i].role, messages[i].kind));
            if self.entries.get(index).is_some_and(|entry| {
                entry.key.matches_message(message) && entry.previous_role_kind == previous_role_kind
            }) {
                continue;
            }

            #[cfg(test)]
            {
                self.measurements += 1;
            }
            let entry = CachedEntry {
                key: MessageLayoutKey::from_message(message),
                height: measure(index),
                previous_role_kind,
            };
            layout_dirty = true;
            if let Some(cached) = self.entries.get_mut(index) {
                *cached = entry;
            } else {
                self.entries.push(entry);
            }
        }
        let previous_len = self.entries.len();
        self.entries.truncate(messages.len());
        layout_dirty |= previous_len != self.entries.len();

        if layout_dirty {
            let mut total = 0usize;
            let mut heights = Vec::with_capacity(self.entries.len());
            let mut cumulative_bottoms = Vec::with_capacity(self.entries.len());
            for entry in &self.entries {
                heights.push(entry.height);
                total = total.saturating_add(entry.height);
                cumulative_bottoms.push(total);
            }
            self.layout = MessageLayout {
                heights: Arc::from(heights),
                cumulative_bottoms: Arc::from(cumulative_bottoms),
            };
        }

        self.layout.clone()
    }

    #[cfg(test)]
    pub(crate) const fn measurements(&self) -> usize {
        self.measurements
    }
}

#[cfg(test)]
mod tests {
    use super::MessageLayoutCache;
    use crate::state::{Message, MessageKind, MessageRole, ToolCallState, ToolCallStatus};
    use std::time::SystemTime;

    fn message(id: &str) -> Message {
        Message {
            id: id.to_string(),
            role: MessageRole::Assistant,
            kind: MessageKind::Regular,
            content: String::new(),
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
            thinking_expanded: false,
        }
    }

    fn tool_message() -> Message {
        let mut message = message("tool-message");
        message.tool_calls.push(ToolCallState {
            parent_call_id: None,
            duration_ms: None,
            call_id: "call-one".to_string(),
            tool: "read".to_string(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            output: String::new(),
        });
        message
    }

    fn messages(ids: &[&str]) -> Vec<Message> {
        ids.iter().map(|id| message(id)).collect()
    }

    fn refs(messages: &[Message]) -> Vec<&Message> {
        messages.iter().collect()
    }

    #[test]
    fn message_prepare_reuses_unchanged_signatures_and_detects_same_length_id_changes() {
        let mut cache = MessageLayoutCache::default();
        let mut messages = [message("one")];
        let refs = messages.iter().collect::<Vec<_>>();
        cache.prepare_messages(80, 0, &refs, |_| 10);

        messages[0].id = "two".to_string();
        let refs = messages.iter().collect::<Vec<_>>();
        let mut measurements = 0;
        cache.prepare_messages(80, 0, &refs, |_| {
            measurements += 1;
            20
        });

        assert_eq!(measurements, 1);
    }

    #[test]
    fn message_prepare_detects_same_length_tool_identity_changes() {
        let mut cache = MessageLayoutCache::default();
        let mut messages = vec![tool_message()];
        let message_refs = refs(&messages);
        cache.prepare_messages(80, 0, &message_refs, |_| 10);

        messages[0].tool_calls[0].call_id = "call-two".to_string();
        messages[0].tool_calls[0].tool = "write".to_string();
        let message_refs = refs(&messages);
        let mut measurements = 0;
        cache.prepare_messages(80, 0, &message_refs, |_| {
            measurements += 1;
            20
        });

        assert_eq!(measurements, 1);
    }

    #[test]
    fn reuses_unchanged_heights_and_remeasures_one_changed_entry() {
        let mut cache = MessageLayoutCache::default();
        let messages = messages(&["one", "two", "three"]);
        let message_refs = refs(&messages);
        let mut measurements = 0;
        let initial = cache.prepare_messages(80, 0, &message_refs, |_| {
            measurements += 1;
            10
        });
        assert_eq!(initial.heights(), &[10, 10, 10]);
        assert_eq!(measurements, 3);

        cache.prepare_messages(80, 0, &message_refs, |_| {
            measurements += 1;
            99
        });
        assert_eq!(measurements, 3);

        let mut updated_messages = messages.clone();
        updated_messages[1].content.push('x');
        let updated_refs = refs(&updated_messages);
        let updated = cache.prepare_messages(80, 0, &updated_refs, |index| {
            measurements += 1;
            20 + index
        });
        assert_eq!(updated.heights(), &[10, 21, 10]);
        assert_eq!(measurements, 4);
    }

    #[test]
    fn unchanged_prepare_reuses_layout_vectors() {
        let mut cache = MessageLayoutCache::default();
        let messages = messages(&["one", "two", "three"]);
        let message_refs = refs(&messages);
        let first = cache.prepare_messages(80, 0, &message_refs, |_| 10);
        let first_heights = first.heights().as_ptr();
        let first_cumulative = first.cumulative_bottoms.as_ptr();

        let second = cache.prepare_messages(80, 0, &message_refs, |_| 99);

        assert_eq!(second.heights(), &[10, 10, 10]);
        assert_eq!(second.heights().as_ptr(), first_heights);
        assert_eq!(second.cumulative_bottoms.as_ptr(), first_cumulative);
    }

    #[test]
    fn append_measures_only_the_new_suffix() {
        let mut cache = MessageLayoutCache::default();
        let mut messages = messages(&["one", "two"]);
        let initial_refs = refs(&messages);
        cache.prepare_messages(80, 0, &initial_refs, |_| 5);

        messages.extend([message("three"), message("four")]);
        let message_refs = refs(&messages);

        let mut measured = Vec::new();
        let layout = cache.prepare_messages(80, 0, &message_refs, |index| {
            measured.push(index);
            index + 1
        });

        assert_eq!(measured, vec![2, 3]);
        assert_eq!(layout.heights(), &[5, 5, 3, 4]);
    }

    #[test]
    fn width_and_settings_changes_invalidate_all_heights() {
        let mut cache = MessageLayoutCache::default();
        let messages = messages(&["one", "two"]);
        let message_refs = refs(&messages);
        cache.prepare_messages(80, 0, &message_refs, |_| 5);

        let mut width_measurements = 0;
        cache.prepare_messages(81, 0, &message_refs, |_| {
            width_measurements += 1;
            6
        });
        assert_eq!(width_measurements, 2);

        let mut settings_measurements = 0;
        cache.prepare_messages(81, 1, &message_refs, |_| {
            settings_measurements += 1;
            7
        });
        assert_eq!(settings_measurements, 2);
    }

    #[test]
    fn cumulative_bottoms_find_the_first_visible_entry() {
        let mut cache = MessageLayoutCache::default();
        let messages = messages(&["one", "two", "three"]);
        let message_refs = refs(&messages);
        let layout = cache.prepare_messages(80, 0, &message_refs, |index| [3, 5, 7][index]);

        assert_eq!(layout.total_height(), 15);
        assert_eq!(layout.first_visible(0), 0);
        assert_eq!(layout.first_visible(2), 0);
        assert_eq!(layout.first_visible(3), 1);
        assert_eq!(layout.first_visible(7), 1);
        assert_eq!(layout.first_visible(8), 2);
        assert_eq!(layout.first_visible(15), 3);
    }

    #[test]
    fn totals_larger_than_u16_do_not_overflow() {
        let mut cache = MessageLayoutCache::default();
        let messages = (0..1_000)
            .map(|index| message(&format!("message-{index}")))
            .collect::<Vec<_>>();
        let message_refs = refs(&messages);
        let layout = cache.prepare_messages(80, 0, &message_refs, |_| 100);

        assert_eq!(layout.total_height(), 100_000);
        assert_eq!(layout.first_visible(99_950), 999);
    }
}
