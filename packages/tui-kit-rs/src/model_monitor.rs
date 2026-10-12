//! Bounded background verification for the selected runtime model.

use std::sync::mpsc;

use crate::model_catalog::{ModelVerification, verify_model_offline};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelVerificationEvent {
    pub model: String,
    pub verification: ModelVerification,
}

impl ModelVerificationEvent {
    #[must_use]
    pub fn is_for_model(&self, current_model: &str) -> bool {
        self.model == current_model
    }
}

#[derive(Debug)]
enum MonitorCommand {
    Verify(String),
}

#[derive(Clone, Debug)]
pub struct ModelMonitor {
    tx: mpsc::SyncSender<MonitorCommand>,
}

impl ModelMonitor {
    /// Queue verification without blocking the UI. A full queue drops duplicate
    /// churn; the next confirmed model event will enqueue another check.
    pub fn verify(&self, model: impl Into<String>) {
        let _ = self.tx.try_send(MonitorCommand::Verify(model.into()));
    }
}

#[must_use]
pub fn spawn_model_monitor() -> (ModelMonitor, mpsc::Receiver<ModelVerificationEvent>) {
    let (tx, rx) = mpsc::sync_channel(8);
    let (event_tx, event_rx) = mpsc::sync_channel(8);
    std::thread::Builder::new()
        .name("maestro-model-monitor".to_owned())
        .spawn(move || {
            while let Ok(MonitorCommand::Verify(model)) = rx.recv() {
                let verification = verify_model_offline(&model);
                if event_tx
                    .send(ModelVerificationEvent {
                        model,
                        verification,
                    })
                    .is_err()
                {
                    break;
                }
                maestro_local_host::ui_wake::wake();
            }
        })
        .expect("model monitor thread should start");
    (ModelMonitor { tx }, event_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_reports_requested_model() {
        let (monitor, events) = spawn_model_monitor();
        monitor.verify("openai/gpt-4o");
        let event = events
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("monitor event");
        assert_eq!(event.model, "openai/gpt-4o");
    }

    #[test]
    fn event_rejects_stale_model() {
        let event = ModelVerificationEvent {
            model: "openai/gpt-4o".to_owned(),
            verification: verify_model_offline("openai/gpt-4o"),
        };
        assert!(event.is_for_model("openai/gpt-4o"));
        assert!(!event.is_for_model("anthropic/claude-sonnet-4-5-20250514"));
    }
}
