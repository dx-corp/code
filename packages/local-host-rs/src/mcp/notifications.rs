//! Bounded, non-blocking notification retention shared by stdio and HTTP.
//! List invalidations are coalesced separately so diagnostic floods cannot hide
//! a capability refresh. The response reader must never wait for a UI consumer.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::protocol::McpNotification;

const MAX_EVENTS: usize = 256;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const LIST_METHODS: [&str; 3] = [
    "notifications/tools/list_changed",
    "notifications/resources/list_changed",
    "notifications/prompts/list_changed",
];
pub(super) const MAX_POLL_NOTIFICATIONS: usize = MAX_EVENTS + LIST_METHODS.len();

#[derive(Debug, Default)]
struct Pending {
    invalidated: [bool; 3],
    next_invalidation: usize,
    events: VecDeque<(McpNotification, usize)>,
    bytes: usize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct NotificationQueue(Arc<Mutex<Pending>>);

pub(super) fn notification_channel() -> (NotificationQueue, NotificationQueue) {
    let queue = NotificationQueue::default();
    (queue.clone(), queue)
}

impl NotificationQueue {
    /// Return false for an oversized diagnostic. Keep the newest diagnostics
    /// when the consumer is absent or slower than the server.
    pub(super) fn send(&self, notification: McpNotification) -> bool {
        if let Some(index) = LIST_METHODS
            .iter()
            .position(|method| *method == notification.method)
        {
            self.0.lock().unwrap().invalidated[index] = true;
            crate::ui_wake::wake();
            return true;
        }
        let Ok(serialized) = serde_json::to_vec(&notification) else {
            return false;
        };
        let bytes = serialized.len();
        if bytes > MAX_EVENT_BYTES {
            return false;
        }
        let mut pending = self.0.lock().unwrap();
        while pending.events.len() >= MAX_EVENTS || pending.bytes + bytes > MAX_BYTES {
            if let Some((_, old_bytes)) = pending.events.pop_front() {
                pending.bytes -= old_bytes;
            }
        }
        pending.bytes += bytes;
        pending.events.push_back((notification, bytes));
        drop(pending);
        crate::ui_wake::wake();
        true
    }

    pub(super) fn try_recv(&mut self) -> Result<McpNotification, ()> {
        let mut pending = self.0.lock().unwrap();
        if let Some(index) = (0..LIST_METHODS.len())
            .map(|offset| (pending.next_invalidation + offset) % LIST_METHODS.len())
            .find(|index| pending.invalidated[*index])
        {
            pending.invalidated[index] = false;
            pending.next_invalidation = (index + 1) % LIST_METHODS.len();
            return Ok(McpNotification {
                jsonrpc: "2.0".into(),
                method: LIST_METHODS[index].into(),
                params: None,
            });
        }
        let (notification, bytes) = pending.events.pop_front().ok_or(())?;
        pending.bytes -= bytes;
        Ok(notification)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn notification(method: &str, size: usize) -> McpNotification {
        McpNotification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params: Some(serde_json::json!({"message":"x".repeat(size)})),
        }
    }

    #[test]
    fn headless_notification_flood_is_bounded_and_keeps_invalidations() {
        let (sender, mut receiver) = notification_channel();
        for index in 0..10_000 {
            sender.send(notification(LIST_METHODS[index % 3], 0));
            sender.send(notification("notifications/message", 4096));
        }
        let pending = receiver.0.lock().unwrap();
        assert!(pending.events.len() <= MAX_EVENTS);
        assert!(pending.bytes <= MAX_BYTES);
        drop(pending);
        for method in LIST_METHODS {
            assert_eq!(receiver.try_recv().unwrap().method, method);
        }
        let mut count = 0;
        while receiver.try_recv().is_ok() {
            count += 1;
        }
        assert!(count > 0 && count <= MAX_EVENTS);
        assert_eq!(receiver.0.lock().unwrap().bytes, 0);
    }

    #[test]
    fn repeated_tools_invalidations_do_not_starve_other_lists() {
        let (sender, mut receiver) = notification_channel();
        for method in LIST_METHODS {
            sender.send(notification(method, 0));
        }
        for expected in LIST_METHODS {
            assert_eq!(receiver.try_recv().unwrap().method, expected);
            sender.send(notification(LIST_METHODS[0], 0));
        }
    }

    #[test]
    fn oversized_events_do_not_displace_capability_invalidations() {
        let (sender, mut receiver) = notification_channel();
        assert!(sender.send(notification(LIST_METHODS[0], MAX_BYTES * 2)));
        assert!(!sender.send(notification("notifications/message", MAX_BYTES * 2)));
        assert_eq!(receiver.try_recv().unwrap().method, LIST_METHODS[0]);
        assert!(receiver.try_recv().is_err());
    }
}
