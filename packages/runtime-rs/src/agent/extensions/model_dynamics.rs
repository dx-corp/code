//! Suggest extra reasoning only for a repeated, typed local repair failure.
use super::{AgentExtension, ToolResultContext, ToolResultPayload};
use crate::{
    agent::FromAgent,
    agent::model_dynamics::{BoostStatus, DynamicsState},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

pub(crate) struct ModelDynamicsExtension {
    state: Arc<Mutex<DynamicsState>>,
    events: mpsc::UnboundedSender<FromAgent>,
    turn: String,
    repairs: HashMap<String, usize>,
    calls: HashSet<String>,
}

impl ModelDynamicsExtension {
    pub fn new(state: Arc<Mutex<DynamicsState>>, events: mpsc::UnboundedSender<FromAgent>) -> Self {
        Self {
            state,
            events,
            turn: String::new(),
            repairs: HashMap::new(),
            calls: HashSet::new(),
        }
    }
}

impl AgentExtension for ModelDynamicsExtension {
    fn allows_independent_script_results(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "model-dynamics"
    }

    fn on_tool_result(&mut self, cx: &ToolResultContext, _: &mut ToolResultPayload) {
        if self.turn != cx.turn_id {
            self.turn.clone_from(&cx.turn_id);
            self.repairs.clear();
            self.calls.clear();
        }
        // Generic errors (including provider-native failures) cannot distinguish
        // a reasoning problem from permissions, outages, or missing credentials.
        let Some(edit) = &cx.edit else { return };
        if !self.calls.insert(cx.call_id.clone()) {
            return;
        }
        if !cx.is_error {
            self.repairs.remove(&edit.path);
            if !self.repairs.values().any(|count| *count >= 2) {
                let mut state = self.state.lock().expect("model dynamics mutex");
                if state.status == BoostStatus::Suggested {
                    state.status = BoostStatus::Idle;
                    let _ = self.events.send(FromAgent::BoostChanged {
                        status: state.status,
                        thinking: None,
                    });
                }
            }
            return;
        }
        if !edit.text_not_found {
            return;
        }
        let count = self.repairs.entry(edit.path.clone()).or_default();
        *count += 1;
        let mut state = self.state.lock().expect("model dynamics mutex");
        if *count >= 2 && state.available && !state.used && state.status == BoostStatus::Idle {
            state.status = BoostStatus::Suggested;
            let _ = self.events.send(FromAgent::BoostChanged {
                status: state.status,
                thinking: None,
            });
            let _ = self.events.send(FromAgent::Status {
                message: "Repeated edits could not match the file. /boost adds reasoning once for this task.".into(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::extensions::{ExtensionRegistry, NativeToolResultContext};
    fn result(call: &str, turn: &str, path: Option<&str>, failed: bool) -> ToolResultContext {
        ToolResultContext {
            turn_id: turn.into(),
            call_id: call.into(),
            tool_name: "edit".into(),
            args_hash: "{}".into(),
            args: serde_json::json!({}),
            is_error: failed,
            duration_ms: 1,
            edit: path.map(|path| super::super::LocalEditResult {
                text_not_found: failed,
                path: path.into(),
            }),
        }
    }
    #[test]
    fn repeated_local_repair_suggests_once_across_reads_without_changing_results() {
        let state = Arc::new(Mutex::new(DynamicsState {
            available: true,
            ..Default::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut registry = ExtensionRegistry::with_default_tenants();
        registry.register(Box::new(ModelDynamicsExtension::new(state.clone(), tx)));
        for cx in [
            result("1", "a", Some("file"), true),
            result("read", "a", None, false),
            result("1", "a", Some("file"), true),
        ] {
            let mut payload = ToolResultPayload {
                content: "unchanged".into(),
                is_error: cx.is_error,
            };
            registry.on_tool_result(&cx, &mut payload);
            assert_eq!(payload.content, "unchanged");
        }
        assert!(rx.try_recv().is_err());
        for call in ["2", "3"] {
            registry.on_tool_result(
                &result(call, "a", Some("file"), true),
                &mut ToolResultPayload {
                    content: "unchanged".into(),
                    is_error: true,
                },
            );
        }
        assert_eq!(state.lock().unwrap().status, BoostStatus::Suggested);
        assert!(matches!(
            rx.try_recv().unwrap(),
            FromAgent::BoostChanged {
                status: BoostStatus::Suggested,
                ..
            }
        ));
        assert!(matches!(rx.try_recv().unwrap(), FromAgent::Status { .. }));
        assert!(rx.try_recv().is_err());
        registry.on_tool_result(
            &result("fixed", "a", Some("file"), false),
            &mut ToolResultPayload {
                content: "fixed".into(),
                is_error: false,
            },
        );
        assert_eq!(state.lock().unwrap().status, BoostStatus::Idle);
        assert!(matches!(
            rx.try_recv().unwrap(),
            FromAgent::BoostChanged {
                status: BoostStatus::Idle,
                ..
            }
        ));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn unknown_errors_native_failures_other_files_success_and_new_tasks_do_not_suggest() {
        let state = Arc::new(Mutex::new(DynamicsState {
            available: true,
            ..Default::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut extension = ModelDynamicsExtension::new(state.clone(), tx);
        for n in 0..5 {
            extension.on_native_tool_result(&NativeToolResultContext {
                turn_id: "a".into(),
                call_id: n.to_string(),
                success: false,
            });
            extension.on_tool_result(
                &result(&n.to_string(), "a", None, true),
                &mut ToolResultPayload {
                    content: "oldText not found".into(),
                    is_error: true,
                },
            );
        }
        for cx in [
            result("1", "a", Some("file"), true),
            result("2", "a", Some("other"), true),
            result("3", "a", Some("file"), false),
            result("4", "a", Some("file"), true),
            result("5", "b", Some("file"), true),
        ] {
            extension.on_tool_result(
                &cx,
                &mut ToolResultPayload {
                    content: "unchanged".into(),
                    is_error: cx.is_error,
                },
            );
        }
        state.lock().unwrap().used = true;
        extension.on_tool_result(
            &result("6", "b", Some("file"), true),
            &mut ToolResultPayload {
                content: "unchanged".into(),
                is_error: true,
            },
        );
        assert!(rx.try_recv().is_err());
    }
}
