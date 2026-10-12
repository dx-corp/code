//! Desktop Notifications
//!
//! Sends system notifications when tasks complete.
//! Supports macOS, Linux, and Windows notification systems.
//!
//! # Terminal State Notifications
//!
//! The [`TerminalStateNotifier`] drives richer terminal-native notifications:
//!
//! - **Tab progress bar (OSC 9;4)**: indeterminate state while a turn runs,
//!   cleared on completion. Supported by iTerm2, `WezTerm`, and ConEmu
//!   (detected via `TERM_PROGRAM`, overridable via `[tui] tab_progress`).
//! - **Terminal title (OSC 0)**: shows working/idle state. The original title
//!   is saved via the xterm title stack (`CSI 22;2 t`) on session start and
//!   restored (`CSI 23;2 t`) on exit.
//! - **Focus-gated desktop notifications**: when the terminal reports
//!   focus-in/focus-out events (crossterm `FocusGained`/`FocusLost`), desktop
//!   notifications are suppressed while the terminal window is focused.

use std::path::PathBuf;

use crate::config::TabProgressMode;

/// Configuration for desktop notifications.
#[derive(Debug, Clone, Default)]
pub struct NotificationConfig {
    /// Whether notifications are enabled.
    pub enabled: bool,
    /// Whether terminal bell is enabled.
    pub terminal_bell: bool,
    /// Custom sound file path (optional).
    pub sound_file: Option<PathBuf>,
}

/// Events that can trigger notifications.
#[derive(Debug, Clone)]
pub enum NotificationEvent {
    /// Session started.
    SessionStart,
    /// Turn/response completed.
    TurnComplete,
    /// Error occurred.
    Error(String),
}

/// Payload for a notification.
#[derive(Debug, Clone)]
pub struct NotificationPayload {
    /// Title of the notification.
    pub title: String,
    /// Body text.
    pub body: String,
    /// Optional sound.
    pub sound: Option<String>,
}

/// Load notification configuration.
#[must_use]
pub fn load_config() -> NotificationConfig {
    NotificationConfig::default()
}

/// Check if notifications are enabled.
#[must_use]
pub fn is_enabled() -> bool {
    false
}

/// Check if terminal notifications are enabled.
#[must_use]
pub fn is_terminal_enabled() -> bool {
    false
}

/// Send a desktop notification.
pub fn send_notification(_payload: NotificationPayload) {
    // Stub - would use notify-rust or similar
}

/// Send a terminal notification (bell).
pub fn send_terminal_notification() {
    print!("\x07"); // Terminal bell
}

/// Notify session start.
pub fn notify_session_start() {
    // Stub
}

/// Notify turn complete.
pub fn notify_turn_complete() {
    // The App owns the opt-in/focus/deduplication gate for Dex notifications.
}

/// A notification names an observed event, never transcript text or inferred success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DexAttention {
    Finished,
    Failed,
    NeedsInput,
}

impl DexAttention {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Finished => "Dex finished your request.",
            Self::Failed => "Dex hit a problem. Return to your task for details.",
            Self::NeedsInput => "Dex needs your answer. Return to your task to continue.",
        }
    }
}

/// Send an opt-in, focus-gated native notification without blocking the TUI.
/// Arguments are fixed strings; workspace/model output is never executable text.
pub fn notify_dex_attention(event: DexAttention) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        let (program, args) = notification_command(event);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
        match result {
            Ok(Ok(status)) if status.success() => {}
            result => tracing::warn!(?result, "Dex attention notification was not delivered"),
        }
    });
}

fn notification_command(event: DexAttention) -> (&'static str, Vec<String>) {
    if cfg!(target_os = "macos") {
        (
            "osascript",
            vec![
                "-e".into(),
                format!(
                    "display notification \"{}\" with title \"Dex · Deixic Code\"",
                    event.message()
                ),
            ],
        )
    } else if cfg!(target_os = "windows") {
        // Static XML and script; event messages contain no XML/script delimiters.
        (
            "powershell.exe",
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                format!(
                    "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null; $xml = New-Object Windows.Data.Xml.Dom.XmlDocument; $xml.LoadXml('<toast><visual><binding template=\"ToastText02\"><text id=\"1\">Dex</text><text id=\"2\">{}</text></binding></visual></toast>'); [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Deixic Code').Show([Windows.UI.Notifications.ToastNotification]::new($xml))",
                    event.message()
                ),
            ],
        )
    } else {
        (
            "notify-send",
            vec![
                "--app-name=Deixic Code".into(),
                "--".into(),
                "Dex".into(),
                event.message().into(),
            ],
        )
    }
}

/// Notify error.
pub fn notify_error(_msg: &str) {
    // Stub
}

// ─────────────────────────────────────────────────────────────────────────────
// TERMINAL STATE NOTIFICATIONS (OSC 9;4 PROGRESS, OSC 0 TITLE, FOCUS GATING)
// ─────────────────────────────────────────────────────────────────────────────

/// OSC sequence terminator: ST (String Terminator). Used for OSC 9;4 progress
/// sequences, which ConEmu documents with ST; iTerm2 and `WezTerm` accept
/// both BEL and ST.
const ST: &str = "\x1b\\";

/// OSC sequence terminator: BEL. Used for OSC 0 title sequences, matching the
/// existing OSC 9 notification style in this crate.
const BEL: &str = "\x07";

/// xterm window-title stack push: saves the current window title so it can be
/// restored on exit.
pub const TITLE_STACK_PUSH: &str = "\x1b[22;2t";

/// xterm window-title stack pop: restores the title saved by
/// [`TITLE_STACK_PUSH`].
pub const TITLE_STACK_POP: &str = "\x1b[23;2t";

/// Title shown while the session is idle.
const IDLE_TITLE: &str = "maestro";

/// Title shown while a turn is running.
const WORKING_TITLE: &str = "maestro - working";

/// Check whether `TERM_PROGRAM` identifies a terminal known to support the
/// OSC 9;4 tab progress sequence (iTerm2, `WezTerm`, ConEmu).
#[must_use]
pub fn term_supports_tab_progress(term_program: Option<&str>) -> bool {
    match term_program {
        Some(program) => {
            let program = program.to_ascii_lowercase();
            program.contains("iterm") || program.contains("wezterm") || program.contains("conemu")
        }
        None => false,
    }
}

/// Resolve the configured tab progress mode against the detected terminal.
#[must_use]
pub fn tab_progress_enabled(mode: TabProgressMode, term_program: Option<&str>) -> bool {
    match mode {
        TabProgressMode::Always => true,
        TabProgressMode::Never => false,
        TabProgressMode::Auto => term_supports_tab_progress(term_program),
    }
}

/// OSC 9;4 sequence: indeterminate progress (busy) state.
#[must_use]
pub fn osc_progress_indeterminate() -> String {
    format!("\x1b]9;4;3;0{ST}")
}

/// OSC 9;4 sequence: clear the progress state.
#[must_use]
pub fn osc_progress_clear() -> String {
    format!("\x1b]9;4;0;0{ST}")
}

/// Strip control characters so user-influenced text cannot break out of an
/// OSC sequence.
///
/// `pub(crate)` because [`crate::hyperlink::format_link`] and
/// [`crate::ansi_commands::PostNotification`] reuse it for the same reason:
/// any text interpolated into an OSC payload needs this, not just the
/// title-setting sequence it was first written for.
pub fn sanitize_osc_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// OSC 0 sequence: set the terminal window (and tab) title.
#[must_use]
pub fn osc_set_title(title: &str) -> String {
    format!("\x1b]0;{}{BEL}", sanitize_osc_text(title))
}

/// Terminal focus state, driven by crossterm focus-in/focus-out events.
///
/// Until the first focus event arrives the focus state is unknown; in that
/// case the gate never suppresses notifications, so terminals that do not
/// report focus events keep their notifications.
#[derive(Debug, Clone, Copy, Default)]
pub struct FocusGate {
    focused: Option<bool>,
}

impl FocusGate {
    /// Record a focus event (`true` = focus gained, `false` = focus lost).
    pub fn record(&mut self, gained: bool) {
        self.focused = Some(gained);
    }

    /// Whether the terminal has reported at least one focus event.
    #[must_use]
    pub fn reports_focus(&self) -> bool {
        self.focused.is_some()
    }

    /// Whether desktop notifications should be suppressed right now: only
    /// when the terminal reports focus events and currently has focus.
    #[must_use]
    pub fn suppress_desktop(&self) -> bool {
        self.focused == Some(true)
    }
}

/// Stateful emitter for terminal-native turn notifications.
///
/// The methods are pure: they return the escape sequences to write to the
/// terminal, so callers control the actual output and tests can assert the
/// exact bytes per state transition.
#[derive(Debug, Clone)]
pub struct TerminalStateNotifier {
    tab_progress: bool,
    title_updates: bool,
    focus_gating: bool,
    busy: bool,
    focus_gate: FocusGate,
}

impl TerminalStateNotifier {
    /// Create a notifier with resolved feature flags.
    #[must_use]
    pub fn new(tab_progress: bool, title_updates: bool, focus_gating: bool) -> Self {
        Self {
            tab_progress,
            title_updates,
            focus_gating,
            busy: false,
            focus_gate: FocusGate::default(),
        }
    }

    /// Resolve from `[tui]` config options and the detected `TERM_PROGRAM`.
    #[must_use]
    pub fn from_config(
        tab_progress_mode: Option<TabProgressMode>,
        title_updates: Option<bool>,
        focus_gating: Option<bool>,
        term_program: Option<&str>,
    ) -> Self {
        Self::new(
            tab_progress_enabled(tab_progress_mode.unwrap_or_default(), term_program),
            title_updates.unwrap_or(true),
            focus_gating.unwrap_or(true),
        )
    }

    /// Record a terminal focus event for desktop-notification gating.
    pub fn record_focus(&mut self, gained: bool) {
        self.focus_gate.record(gained);
    }

    /// Whether a desktop notification should be sent right now.
    #[must_use]
    pub fn should_send_desktop_notification(&self) -> bool {
        !(self.focus_gating && self.focus_gate.suppress_desktop())
    }

    /// Sequences to emit when the session starts: save the original title and
    /// show the idle state.
    #[must_use]
    pub fn session_started(&self) -> Vec<String> {
        let mut seqs = Vec::new();
        if self.title_updates {
            seqs.push(TITLE_STACK_PUSH.to_string());
            seqs.push(osc_set_title(IDLE_TITLE));
        }
        seqs
    }

    /// Sequences to emit when a turn starts: indeterminate progress plus the
    /// working title. Idempotent while already busy.
    pub fn turn_started(&mut self) -> Vec<String> {
        if self.busy {
            return Vec::new();
        }
        self.busy = true;
        let mut seqs = Vec::new();
        if self.tab_progress {
            seqs.push(osc_progress_indeterminate());
        }
        if self.title_updates {
            seqs.push(osc_set_title(WORKING_TITLE));
        }
        seqs
    }

    /// Sequences to emit when a turn finishes: clear progress and restore the
    /// idle title. Idempotent while already idle.
    pub fn turn_finished(&mut self) -> Vec<String> {
        if !self.busy {
            return Vec::new();
        }
        self.busy = false;
        let mut seqs = Vec::new();
        if self.tab_progress {
            seqs.push(osc_progress_clear());
        }
        if self.title_updates {
            seqs.push(osc_set_title(IDLE_TITLE));
        }
        seqs
    }

    /// Sequences to emit when the session ends: clear progress and restore
    /// the original terminal title saved by [`Self::session_started`].
    pub fn session_ended(&mut self) -> Vec<String> {
        self.busy = false;
        let mut seqs = Vec::new();
        if self.tab_progress {
            seqs.push(osc_progress_clear());
        }
        if self.title_updates {
            seqs.push(TITLE_STACK_POP.to_string());
        }
        seqs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let config = NotificationConfig::default();
        assert!(!config.enabled);
    }

    // ── Tab progress support detection ─────────────────────────────────────

    #[test]
    fn tab_progress_support_by_term_program() {
        assert!(term_supports_tab_progress(Some("iTerm.app")));
        assert!(term_supports_tab_progress(Some("WezTerm")));
        assert!(term_supports_tab_progress(Some("ConEmu")));
        assert!(!term_supports_tab_progress(Some("vscode")));
        assert!(!term_supports_tab_progress(Some("Apple_Terminal")));
        assert!(!term_supports_tab_progress(Some("tmux")));
        assert!(!term_supports_tab_progress(None));
    }

    #[test]
    fn tab_progress_mode_gating() {
        // Auto defers to terminal detection.
        assert!(tab_progress_enabled(
            TabProgressMode::Auto,
            Some("iTerm.app")
        ));
        assert!(!tab_progress_enabled(
            TabProgressMode::Auto,
            Some("Apple_Terminal")
        ));
        // Always overrides detection.
        assert!(tab_progress_enabled(
            TabProgressMode::Always,
            Some("Apple_Terminal")
        ));
        assert!(tab_progress_enabled(TabProgressMode::Always, None));
        // Never overrides detection.
        assert!(!tab_progress_enabled(
            TabProgressMode::Never,
            Some("iTerm.app")
        ));
    }

    // ── Sequence emission ──────────────────────────────────────────────────

    #[test]
    fn turn_start_emits_progress_and_title() {
        let mut notifier = TerminalStateNotifier::new(true, true, true);
        let seqs = notifier.turn_started();
        assert_eq!(
            seqs,
            vec![
                "\x1b]9;4;3;0\x1b\\".to_string(),
                "\x1b]0;maestro - working\x07".to_string(),
            ]
        );
    }

    #[test]
    fn turn_finish_clears_progress_and_restores_idle_title() {
        let mut notifier = TerminalStateNotifier::new(true, true, true);
        notifier.turn_started();
        let seqs = notifier.turn_finished();
        assert_eq!(
            seqs,
            vec![
                "\x1b]9;4;0;0\x1b\\".to_string(),
                "\x1b]0;maestro\x07".to_string(),
            ]
        );
    }

    #[test]
    fn turn_transitions_are_idempotent() {
        let mut notifier = TerminalStateNotifier::new(true, true, true);
        assert!(notifier.turn_finished().is_empty());
        assert!(!notifier.turn_started().is_empty());
        assert!(notifier.turn_started().is_empty());
        assert!(!notifier.turn_finished().is_empty());
        assert!(notifier.turn_finished().is_empty());
    }

    #[test]
    fn disabled_features_emit_nothing() {
        let mut notifier = TerminalStateNotifier::new(false, false, true);
        assert!(notifier.session_started().is_empty());
        assert!(notifier.turn_started().is_empty());
        assert!(notifier.turn_finished().is_empty());
        assert!(notifier.session_ended().is_empty());
    }

    // ── Title save/restore ─────────────────────────────────────────────────

    #[test]
    fn session_start_saves_title_and_sets_idle() {
        let notifier = TerminalStateNotifier::new(true, true, true);
        let seqs = notifier.session_started();
        assert_eq!(
            seqs,
            vec![
                TITLE_STACK_PUSH.to_string(),
                "\x1b]0;maestro\x07".to_string(),
            ]
        );
    }

    #[test]
    fn session_end_restores_title_and_clears_progress() {
        let mut notifier = TerminalStateNotifier::new(true, true, true);
        notifier.turn_started();
        let seqs = notifier.session_ended();
        assert_eq!(
            seqs,
            vec![
                "\x1b]9;4;0;0\x1b\\".to_string(),
                TITLE_STACK_POP.to_string(),
            ]
        );
    }

    #[test]
    fn title_text_is_sanitized() {
        assert_eq!(osc_set_title("plain"), "\x1b]0;plain\x07");
        // Control characters (ESC, BEL, newline) must not break the sequence.
        assert_eq!(osc_set_title("evil\x1b]0;x\x07\n"), "\x1b]0;evil]0;x\x07");
    }

    // ── Focus gating ───────────────────────────────────────────────────────

    #[test]
    fn focus_gate_suppresses_only_when_focused() {
        let mut gate = FocusGate::default();
        // Unknown focus state (terminal may not report focus): never suppress.
        assert!(!gate.reports_focus());
        assert!(!gate.suppress_desktop());

        gate.record(false);
        assert!(gate.reports_focus());
        assert!(!gate.suppress_desktop());

        gate.record(true);
        assert!(gate.suppress_desktop());

        gate.record(false);
        assert!(!gate.suppress_desktop());
    }

    #[test]
    fn desktop_notification_gating_respects_config() {
        let mut notifier = TerminalStateNotifier::new(true, true, true);
        // No focus events seen yet: notifications allowed.
        assert!(notifier.should_send_desktop_notification());

        notifier.record_focus(true);
        assert!(!notifier.should_send_desktop_notification());

        notifier.record_focus(false);
        assert!(notifier.should_send_desktop_notification());

        // With gating disabled in config, focus never suppresses.
        let mut ungated = TerminalStateNotifier::new(true, true, false);
        ungated.record_focus(true);
        assert!(ungated.should_send_desktop_notification());
    }

    // ── Config resolution ──────────────────────────────────────────────────

    #[test]
    fn from_config_defaults() {
        // Auto mode on an unsupported terminal disables progress; title and
        // focus gating default to enabled.
        let notifier = TerminalStateNotifier::from_config(None, None, None, Some("Apple_Terminal"));
        assert!(!notifier.tab_progress);
        assert!(notifier.title_updates);
        assert!(notifier.focus_gating);

        let notifier = TerminalStateNotifier::from_config(None, None, None, Some("WezTerm"));
        assert!(notifier.tab_progress);
    }

    #[test]
    fn from_config_honors_overrides() {
        let notifier = TerminalStateNotifier::from_config(
            Some(TabProgressMode::Always),
            Some(false),
            Some(false),
            Some("Apple_Terminal"),
        );
        assert!(notifier.tab_progress);
        assert!(!notifier.title_updates);
        assert!(!notifier.focus_gating);
    }
}

#[cfg(test)]
mod dex_attention_tests {
    use super::*;

    #[test]
    fn native_notifications_use_fixed_event_text_without_transcript_interpolation() {
        for event in [
            DexAttention::Finished,
            DexAttention::Failed,
            DexAttention::NeedsInput,
        ] {
            let (program, args) = notification_command(event);
            assert!(!program.is_empty());
            assert!(args.iter().any(|arg| arg.contains(event.message())));
            assert!(!event.message().contains('"'));
            assert!(!event.message().contains('\''));
            assert!(!event.message().contains('<'));
        }
        assert!(
            DexAttention::NeedsInput
                .message()
                .contains("needs your answer")
        );
        assert!(!DexAttention::Finished.message().contains("tests passed"));
    }
}
