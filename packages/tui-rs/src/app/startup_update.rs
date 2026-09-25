//! Release notice for installed interactive launches, off the first-frame path.
//!
//! `App::run_inner` paints the first frame, then spawns the bounded check
//! from `update_cli::startup_update_notice`. The result travels through a
//! oneshot the main loop drains with `try_recv`, and the task signals the
//! shared `LoopWake` so an idle loop wakes for it. Nothing on the startup path
//! awaits the check; quit aborts it.
//!
//! The opt-in `MAESTRO_AUTO_UPDATE=apply` path (check, install, restart before
//! terminal setup) stays in `update_cli::run_startup_update` and is not
//! touched here.

use std::future::Future;

use tokio::sync::oneshot;

use super::App;
use crate::loop_wake::LoopWake;
use crate::update_cli::StartupUpdateNotice;

pub(super) struct StartupUpdateCheck {
    notice_rx: Option<oneshot::Receiver<StartupUpdateNotice>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl StartupUpdateCheck {
    /// No check in flight. `App::new` starts here; `run_inner` spawns later.
    pub(super) fn idle() -> Self {
        Self {
            notice_rx: None,
            task: None,
        }
    }

    /// Run `check` on its own task. The task signals `wake` when it ends,
    /// with or without a notice, so the loop's idle wait returns for it.
    pub(super) fn spawn(
        check: impl Future<Output = Option<StartupUpdateNotice>> + Send + 'static,
        wake: LoopWake,
    ) -> Self {
        let (notice_tx, notice_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            if let Some(notice) = check.await {
                let _ = notice_tx.send(notice);
            }
            wake.signal();
        });
        Self {
            notice_rx: Some(notice_rx),
            task: Some(task),
        }
    }

    /// Non-blocking. Returns the notice once, then never again.
    pub(super) fn try_take(&mut self) -> Option<StartupUpdateNotice> {
        let notice_rx = self.notice_rx.as_mut()?;
        match notice_rx.try_recv() {
            Ok(notice) => {
                self.notice_rx = None;
                Some(notice)
            }
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                self.notice_rx = None;
                None
            }
        }
    }

    /// Abort an in-flight check and drop any undelivered notice.
    pub(super) fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.notice_rx = None;
    }

    #[cfg(test)]
    pub(super) fn is_pending(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }
}

impl App {
    /// Spawn the production check. Called once, after the first frame.
    pub(super) fn spawn_startup_update_check(&mut self) {
        self.spawn_startup_update_check_with(crate::update_cli::startup_update_notice());
    }

    /// Spawn `check` in place of the production check. Returns immediately.
    pub(super) fn spawn_startup_update_check_with(
        &mut self,
        check: impl Future<Output = Option<StartupUpdateNotice>> + Send + 'static,
    ) {
        self.startup_update_check.cancel();
        self.startup_update_check = StartupUpdateCheck::spawn(check, self.loop_wake.clone());
    }

    /// Drain a finished check into the transcript. Non-blocking.
    pub(super) fn poll_startup_update_notice(&mut self) -> bool {
        let Some(notice) = self.startup_update_check.try_take() else {
            return false;
        };
        let message = self.state.locale.format(
            "Deixic Code {0} is available (current {1}); run `deixic-code update`.",
            &[notice.latest, notice.current],
        );
        self.state.add_system_message(message);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::app::tests::new_test_app;
    use crate::loop_wake::{LoopWakeCause, await_loop_wake};

    fn notice(latest: &str, current: &str) -> StartupUpdateNotice {
        StartupUpdateNotice {
            latest: latest.to_owned(),
            current: current.to_owned(),
        }
    }

    /// A check that never resolves must not hold the first frame, the poll,
    /// or the app's ability to continue. This is the startup contract.
    #[tokio::test]
    async fn first_frame_renders_while_the_startup_update_check_is_pending() {
        let mut app = new_test_app();
        app.spawn_startup_update_check_with(std::future::pending());

        app.render()
            .expect("first frame renders with the release check still pending");
        assert!(
            !app.poll_startup_update_notice(),
            "a pending check must not produce a notice"
        );
        assert!(
            app.startup_update_check.is_pending(),
            "the check is still running after the frame"
        );
        assert!(
            app.state
                .messages
                .iter()
                .all(|message| !message.content.contains("is available")),
            "no notice reaches the transcript before the check completes"
        );

        app.startup_update_check.cancel();
        assert!(
            !app.startup_update_check.is_pending(),
            "cancel drops the task handle"
        );
    }

    /// Wait through the loop's own idle wait until the check task has ended.
    /// Returns whether a producer signal (the task's `wake.signal()`) was
    /// observed on the way.
    async fn wait_for_check_to_end(app: &App) -> bool {
        let mut producer_wake = false;
        for _ in 0..400 {
            let cause = await_loop_wake(
                Duration::from_millis(25),
                &app.loop_wake,
                std::future::pending::<()>(),
            )
            .await;
            producer_wake |= cause == LoopWakeCause::Producer;
            if !app.startup_update_check.is_pending() {
                return producer_wake;
            }
        }
        panic!("startup update check did not end");
    }

    #[tokio::test]
    async fn a_finished_check_wakes_the_loop_and_posts_one_transcript_notice() {
        let mut app = new_test_app();
        app.spawn_startup_update_check_with(async { Some(notice("9.9.9", "1.0.0")) });

        assert!(
            wait_for_check_to_end(&app).await,
            "the check signals the loop wake"
        );

        assert!(app.poll_startup_update_notice());
        let last = app.state.messages.last().expect("notice message");
        assert!(
            last.content
                .contains("Deixic Code 9.9.9 is available (current 1.0.0)"),
            "unexpected notice: {}",
            last.content
        );
        assert!(
            !app.poll_startup_update_notice(),
            "the notice is delivered once"
        );
    }

    #[tokio::test]
    async fn a_check_with_no_newer_release_posts_nothing() {
        let mut app = new_test_app();
        let before = app.state.messages.len();
        app.spawn_startup_update_check_with(async { None });

        assert!(wait_for_check_to_end(&app).await);

        assert!(!app.poll_startup_update_notice());
        assert_eq!(app.state.messages.len(), before);
        assert!(!app.poll_startup_update_notice());
    }

    /// Set when the check future is dropped, which is what abort does to a
    /// task that never resolves.
    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn cancel_aborts_a_check_that_never_resolves() {
        use std::sync::atomic::Ordering;

        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = DropFlag(std::sync::Arc::clone(&dropped));
        let mut check = StartupUpdateCheck::spawn(
            async move {
                let _flag = flag;
                std::future::pending::<Option<StartupUpdateNotice>>().await
            },
            LoopWake::new(),
        );
        tokio::task::yield_now().await;
        assert!(check.is_pending());
        assert!(!dropped.load(Ordering::SeqCst));

        check.cancel();
        for _ in 0..100 {
            if dropped.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "cancel must drop the pending check future"
        );
        assert!(check.try_take().is_none());
        assert!(!check.is_pending());
    }
}
