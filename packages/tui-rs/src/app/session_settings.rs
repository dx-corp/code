//! Persist conversation choices without replaying permission grants.
use super::*;
use crate::session::{SessionApprovalMode, SessionQueueMode, SessionSettings};
use crate::state::ApprovalMode;

fn saved_approval(mode: ApprovalMode) -> SessionApprovalMode {
    match mode {
        ApprovalMode::Yolo => SessionApprovalMode::Yolo,
        ApprovalMode::Selective => SessionApprovalMode::Selective,
        ApprovalMode::Safe => SessionApprovalMode::Safe,
    }
}

fn restored_approval(saved: SessionApprovalMode, current: ApprovalMode) -> ApprovalMode {
    // A transcript may request more confirmation, but cannot waive a check
    // required by this process. Sandbox/managed authorization stays untouched.
    match (saved, current) {
        (SessionApprovalMode::Safe, _) | (_, ApprovalMode::Safe) => ApprovalMode::Safe,
        (SessionApprovalMode::Selective, _) | (_, ApprovalMode::Selective) => {
            ApprovalMode::Selective
        }
        (SessionApprovalMode::Yolo, ApprovalMode::Yolo) => ApprovalMode::Yolo,
    }
}

fn saved_queue(mode: crate::state::QueueMode) -> SessionQueueMode {
    match mode {
        crate::state::QueueMode::All => SessionQueueMode::All,
        crate::state::QueueMode::One => SessionQueueMode::One,
    }
}

fn restored_queue(mode: SessionQueueMode) -> crate::state::QueueMode {
    match mode {
        SessionQueueMode::All => crate::state::QueueMode::All,
        SessionQueueMode::One => crate::state::QueueMode::One,
    }
}

impl App {
    /// Used only for an explicit launch flag; global defaults do not override
    /// session-owned choices. Consumed after a successful startup adoption.
    pub(crate) fn set_startup_session_model_override(&mut self, model: Option<String>) {
        self.startup_session_model_override = model;
    }

    pub(crate) fn take_startup_session_model_override(&mut self) -> Option<String> {
        self.startup_session_model_override.take()
    }

    /// A launch override applies only to the requested startup resume. Preserve
    /// it across a workspace relaunch, but never carry it into later UI choices.
    pub(crate) fn finish_startup_session_model_override(&mut self) {
        if self.resume_target.is_none() {
            self.startup_session_model_override = None;
        }
    }

    pub(super) fn record_session_settings(&mut self) -> bool {
        let Some(path) = self.session_manager.current_session_path() else {
            return true;
        };
        if self.session_manager.writer().is_none() {
            return true;
        }
        let settings = SessionSettings {
            version: 1,
            model: policy_model_id(&self.current_model),
            thinking_level: self.current_thinking_level,
            approval_mode: saved_approval(self.state.approval_mode),
            steering_mode: saved_queue(self.state.steering_mode),
            follow_up_mode: saved_queue(self.state.follow_up_mode),
        };
        if self
            .persisted_session_settings
            .as_ref()
            .is_some_and(|(saved_path, saved)| saved_path == &path && saved == &settings)
        {
            return true;
        }
        let entry = match settings.clone().into_entry() {
            Ok(entry) => entry,
            Err(error) => {
                self.state.error = Some(format!("Could not persist session settings: {error}"));
                return false;
            }
        };
        if !self.write_session_entry(entry) || !self.flush_session() {
            return false;
        }
        self.persisted_session_settings = Some((path, settings));
        true
    }

    pub(super) fn restore_session_preferences(&mut self, settings: Option<&SessionSettings>) {
        let Some(settings) = settings else { return };
        self.state.approval_mode =
            restored_approval(settings.approval_mode, self.state.approval_mode);
        self.state.interaction_mode = match self.state.approval_mode {
            ApprovalMode::Yolo => crate::state::InteractionMode::AlwaysApprove,
            ApprovalMode::Safe | ApprovalMode::Selective => crate::state::InteractionMode::Normal,
        };
        self.sync_agent_approval_mode();
        self.state.steering_mode = restored_queue(settings.steering_mode);
        self.state.follow_up_mode = restored_queue(settings.follow_up_mode);
        if let Some(agent) = &self.native_agent {
            let _ = agent.set_steering_mode(self.state.steering_mode);
            let _ = agent.set_follow_up_mode(self.state.follow_up_mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_permissions_can_only_narrow_the_current_authority() {
        assert_eq!(
            restored_approval(SessionApprovalMode::Yolo, ApprovalMode::Selective),
            ApprovalMode::Selective
        );
        assert_eq!(
            restored_approval(SessionApprovalMode::Yolo, ApprovalMode::Safe),
            ApprovalMode::Safe
        );
        assert_eq!(
            restored_approval(SessionApprovalMode::Selective, ApprovalMode::Safe),
            ApprovalMode::Safe
        );
        assert_eq!(
            restored_approval(SessionApprovalMode::Safe, ApprovalMode::Yolo),
            ApprovalMode::Safe
        );
        assert_eq!(
            restored_approval(SessionApprovalMode::Yolo, ApprovalMode::Yolo),
            ApprovalMode::Yolo
        );
    }
}
