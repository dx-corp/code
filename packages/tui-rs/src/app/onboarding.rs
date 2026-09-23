//! Coordinates the setup presentation with observed readiness and content-free analytics.
use super::*;
use crate::components::SetupPage;
use crate::doctor::CheckStatus;
use crate::onboarding_checks::{OnboardingCheck, OnboardingReadiness};
use crate::telemetry::{
    OnboardingCheckId, OnboardingCheckResult, OnboardingCollectionStatus, OnboardingEvent,
    OnboardingStage,
};

pub(super) struct OnboardingSession {
    attempt_id: uuid::Uuid,
    started: Instant,
    transition: Instant,
    attempts: u32,
    check_scope: Option<crate::telemetry::TelemetryIdentityScope>,
    checks: Option<tokio::task::JoinHandle<OnboardingReadiness>>,
    collection_tx: mpsc::UnboundedSender<OnboardingCollectionStatus>,
    collection_rx: mpsc::UnboundedReceiver<OnboardingCollectionStatus>,
}

impl Default for OnboardingSession {
    fn default() -> Self {
        let (collection_tx, collection_rx) = mpsc::unbounded_channel();
        Self {
            attempt_id: uuid::Uuid::new_v4(),
            started: Instant::now(),
            transition: Instant::now(),
            attempts: 0,
            check_scope: None,
            checks: None,
            collection_tx,
            collection_rx,
        }
    }
}

impl Drop for OnboardingSession {
    fn drop(&mut self) {
        if let Some(task) = self.checks.take() {
            task.abort();
        }
    }
}

impl OnboardingSession {
    pub(super) fn frame(&self) -> u64 {
        (self.transition.elapsed().as_millis() / 80) as u64
    }
}

impl App {
    /// Local preparation only. Account/model checks retain their explicit consent step.
    pub(super) async fn first_run_boot(&mut self) -> Result<()> {
        if self.ui_prefs.boot_seen || self.ui_prefs.onboarding_seen || self.initial_prompt.is_some()
        {
            return Ok(());
        }
        let snapshot = match crate::init_cli::load_evalops_snapshot() {
            Ok(snapshot) => snapshot,
            Err(_) => return Ok(()), // Unknown account state is not a first-time account.
        };
        let env = std::env::vars().collect();
        let has_identity = snapshot.is_some()
            || crate::credential_mode::platform_session_from(snapshot.as_ref(), &env).is_some();
        if !boot_eligible(
            self.ui_prefs.boot_seen,
            self.ui_prefs.onboarding_seen,
            has_identity,
            self.initial_prompt.is_some(),
        ) {
            return Ok(());
        }
        self.ui_prefs.boot_seen = true;
        if self.ui_prefs.save_default().is_err() {
            self.state.add_system_message(
                self.state
                    .locale
                    .translate("Could not save the startup display preference.")
                    .into(),
            );
        }
        let started = Instant::now();
        let animate = self
            .ui_prefs
            .animations
            .unwrap_or(self.configured_animations);
        // Bound time on the identity screen, not the work: the existing scan continues
        // and its receiver is polled by the normal event loop after skip or timeout.
        while self.workspace_scan_rx.is_some() && started.elapsed() < Duration::from_millis(1500) {
            self.poll_workspace_scan();
            if self.workspace_scan_rx.is_none() {
                break;
            }
            let tick = animate.then_some((started.elapsed().as_millis() / 80) as u64);
            self.terminal.draw(|frame| {
                crate::components::startup::render_startup(
                    frame,
                    frame.area(),
                    crate::themes::current_ui_theme(),
                    tick,
                );
            })?;
            if let Some(event) = self.poll_terminal_event(Duration::from_millis(30))? {
                match event {
                    AppTerminalEvent::Key(key) if should_handle_key_event(key.kind) => {
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(CrosstermModifiers::CONTROL)
                        {
                            self.should_quit = true;
                            break;
                        }
                        if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                            break;
                        }
                    }
                    AppTerminalEvent::Resize { width, height } => {
                        self.handle_resize(width, height)?;
                    }
                    AppTerminalEvent::ThemeReportingStatus(setting) => {
                        self.state.theme_reporting_available = setting.is_available();
                        if setting == uncurses::ansi::mode::ModeSetting::Reset {
                            let _ = terminal::enable_theme_reporting();
                        }
                    }
                    event => {
                        self.apply_terminal_theme_event(&event);
                    }
                }
            }
            tokio::task::yield_now().await;
        }
        if !self.should_quit && self.active_modal == ActiveModal::None {
            self.open_onboarding();
        }
        Ok(())
    }

    pub(super) fn open_onboarding(&mut self) {
        // Replacing the presentation session cancels an old probe and discards its callbacks.
        self.onboarding = OnboardingSession::default();
        self.setup_modal.show();
        self.setup_modal
            .set_share_diagnostics(self.ui_prefs.onboarding_share_diagnostics.unwrap_or(true));
        self.setup_modal
            .set_connection_available(default_model_credentials_ready());
        self.active_modal = ActiveModal::Setup;
    }

    pub(super) fn record_onboarding(&mut self, stage: OnboardingStage) {
        if !self.setup_modal.share_diagnostics() {
            self.setup_modal
                .set_collection_status(OnboardingCollectionStatus::Disabled);
            return;
        }
        let checks = self.setup_modal.checks().map_or_else(Vec::new, |report| {
            report.checks.iter().map(analytics_check).collect()
        });
        let event = OnboardingEvent::new(
            self.onboarding.attempt_id,
            stage,
            self.setup_modal.profile().clone(),
            self.onboarding
                .started
                .elapsed()
                .as_millis()
                .min(86_400_000) as u64,
            checks,
            self.onboarding.attempts.saturating_sub(1).min(1_000),
        );
        let tx = self.onboarding.collection_tx.clone();
        let origin = if matches!(
            stage,
            OnboardingStage::ChecksCompleted | OnboardingStage::Completed
        ) {
            self.onboarding.check_scope.clone()
        } else {
            crate::telemetry::onboarding_identity_scope()
        };
        tokio::spawn(async move {
            let status = crate::telemetry::record_onboarding_event(event, origin).await;
            let _ = tx.send(status);
        });
    }

    pub(super) fn start_onboarding_checks(&mut self) {
        if self.onboarding.checks.is_some() || self.state.busy {
            self.setup_modal.set_status(
                self.state
                    .locale
                    .translate("Finish the current request before running setup checks."),
            );
            return;
        }
        let model = if self.current_model.is_empty() {
            self.state.model.clone()
        } else {
            Some(self.current_model.clone())
        };
        let cwd = std::env::current_dir().unwrap_or_default();
        self.onboarding.check_scope = crate::telemetry::onboarding_identity_scope();
        self.onboarding.attempts = self.onboarding.attempts.saturating_add(1);
        self.onboarding.transition = Instant::now();
        self.setup_modal.set_checking();
        self.onboarding.checks = Some(tokio::spawn(async move {
            crate::onboarding_checks::run_checks(model.as_deref(), &cwd).await
        }));
    }

    pub(super) fn poll_onboarding(&mut self) -> bool {
        let mut changed = false;
        while let Ok(status) = self.onboarding.collection_rx.try_recv() {
            self.setup_modal.set_collection_status(status);
            changed = true;
        }
        // A finished JoinHandle can be polled without blocking the terminal thread.
        if self
            .onboarding
            .checks
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            use futures::FutureExt;
            if let Some(task) = self.onboarding.checks.take() {
                let mut report = task.now_or_never().and_then(Result::ok).unwrap_or_else(|| {
                    OnboardingReadiness {
                        ready: false,
                        elapsed_ms: 0,
                        checks: vec![OnboardingCheck {
                            id: OnboardingCheckId::ModelProbe,
                            status: CheckStatus::Fail,
                            summary: self
                                .state
                                .locale
                                .translate("Setup checks ended without a result.")
                                .to_string(),
                            repair: Some(
                                self.state
                                    .locale
                                    .translate("Retry the checks. If this repeats, open /feedback.")
                                    .to_string(),
                            ),
                        }],
                    }
                });
                invalidate_changed_scope(
                    &mut report,
                    self.onboarding.check_scope.as_ref(),
                    crate::telemetry::onboarding_identity_scope().as_ref(),
                );
                self.setup_modal.set_check_results(report);
                self.onboarding.transition = Instant::now();
                self.record_onboarding(OnboardingStage::ChecksCompleted);
                changed = true;
            }
        }
        changed
    }

    pub(super) fn onboarding_animation_active(&self) -> bool {
        self.active_modal == ActiveModal::Setup
            && self
                .ui_prefs
                .animations
                .unwrap_or(self.configured_animations)
            && (self.setup_modal.page() == SetupPage::Welcome
                || self.setup_modal.page() == SetupPage::Checking
                || (self.setup_modal.checks().is_some_and(|report| report.ready)
                    && self.onboarding.transition.elapsed() < Duration::from_millis(800)))
    }

    pub(super) fn close_onboarding(&mut self, completed: bool) {
        if completed && self.onboarding.check_scope != crate::telemetry::onboarding_identity_scope()
        {
            self.setup_modal.set_connection_ready();
            self.setup_modal.set_status(
                self.state.locale.translate(
                    "The selected account changed. Run the checks again before finishing.",
                ),
            );
            return;
        }
        let completed = completed && self.setup_modal.checks().is_some_and(|report| report.ready);
        self.record_onboarding(if completed {
            OnboardingStage::Completed
        } else {
            OnboardingStage::Dismissed
        });
        if let Some(task) = self.onboarding.checks.take() {
            task.abort();
        }
        self.setup_login_rx = None;
        self.setup_login_url_rx = None;
        if let Some(task) = self.setup_login_task.take() {
            task.abort();
        }
        self.ui_prefs.onboarding_seen = true;
        self.ui_prefs.onboarding_share_diagnostics = Some(self.setup_modal.share_diagnostics());
        if self.ui_prefs.save_default().is_err() {
            self.state.add_system_message(
                self.state
                    .locale
                    .translate("Could not save the onboarding display preference.")
                    .to_string(),
            );
        }
        self.setup_modal.hide();
        self.active_modal = ActiveModal::None;
        if completed && self.native_agent.is_none() {
            self.pending_agent_spawn = true;
        }
    }
}

fn boot_eligible(seen: bool, setup_seen: bool, identity: bool, prompt: bool) -> bool {
    !seen && !setup_seen && !identity && !prompt
}

fn invalidate_changed_scope(
    report: &mut OnboardingReadiness,
    checked: Option<&crate::telemetry::TelemetryIdentityScope>,
    current: Option<&crate::telemetry::TelemetryIdentityScope>,
) {
    if checked == current {
        return;
    }
    report.ready = false;
    report
        .checks
        .retain(|check| check.id != OnboardingCheckId::Identity);
    report.checks.push(OnboardingCheck {
        id: OnboardingCheckId::Identity,
        status: CheckStatus::Fail,
        summary: maestro_ui::localization::tr("The selected account changed during verification.")
            .into(),
        repair: Some(
            maestro_ui::localization::tr("Reconnect and run the checks for the selected account.")
                .into(),
        ),
    });
}

fn analytics_check(check: &OnboardingCheck) -> OnboardingCheckResult {
    OnboardingCheckResult {
        id: check.id,
        status: check.status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_is_only_for_first_interactive_launch_without_identity() {
        assert!(boot_eligible(false, false, false, false));
        for args in [
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
        ] {
            assert!(!boot_eligible(args.0, args.1, args.2, args.3));
        }
    }

    #[test]
    fn collection_omits_local_details() {
        let check = OnboardingCheck {
            id: OnboardingCheckId::ModelProbe,
            status: CheckStatus::Fail,
            summary: "private local detail".into(),
            repair: Some("private repair text".into()),
        };
        let encoded = serde_json::to_string(&analytics_check(&check)).unwrap();
        assert!(!encoded.contains("private"));
        assert!(encoded.contains("model_probe"));
    }
    #[test]
    fn stale_completion_cannot_verify_another_account() {
        let origin = crate::telemetry::TelemetryIdentityScope::new("org", Some("a")).unwrap();
        let switched = crate::telemetry::TelemetryIdentityScope::new("org", Some("b")).unwrap();
        let mut report = OnboardingReadiness {
            ready: true,
            elapsed_ms: 1,
            checks: vec![],
        };
        invalidate_changed_scope(&mut report, Some(&origin), Some(&origin));
        assert!(report.ready);
        invalidate_changed_scope(&mut report, Some(&origin), Some(&switched));
        assert!(!report.ready);
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].id, OnboardingCheckId::Identity);
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
    }

    #[tokio::test]
    async fn replacing_walkthrough_cancels_pending_checks_and_discards_callbacks() {
        let mut old = OnboardingSession::default();
        let old_attempt = old.attempt_id;
        let tx = old.collection_tx.clone();
        let task = tokio::spawn(std::future::pending::<OnboardingReadiness>());
        let abort = task.abort_handle();
        old.checks = Some(task);
        drop(old);
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
        assert!(tx.send(OnboardingCollectionStatus::Queued).is_err());
        let mut reopened = OnboardingSession::default();
        assert_ne!(reopened.attempt_id, old_attempt);
        assert!(reopened.checks.is_none());
        assert!(reopened.collection_rx.try_recv().is_err());
    }
}
