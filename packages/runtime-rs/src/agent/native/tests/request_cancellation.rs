//! Cancellation and shutdown retain truthful terminal outcomes.

use super::*;

#[tokio::test]
async fn retry_backoff_is_interrupted_by_request_cancellation() {
    let cancel_token = CancellationToken::new();
    let waiter_token = cancel_token.clone();
    let shutdown_token = CancellationToken::new();
    let waiter = tokio::spawn(async move {
        wait_for_retry_delay(
            std::time::Duration::from_mins(1),
            &waiter_token,
            &shutdown_token,
        )
        .await
    });

    tokio::task::yield_now().await;
    cancel_token.cancel();
    let completed_delay = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .expect("cancellation must interrupt the retry backoff")
        .expect("retry waiter task must not panic");
    assert!(!completed_delay, "cancelled backoff must not begin a retry");
}

#[tokio::test]
async fn shutdown_awaits_terminal_outcome_beyond_the_old_drain_cutoff() {
    let request_cancel = CancellationToken::new();
    let shutdown_token = CancellationToken::new();
    let shutdown = shutdown_token.clone();

    let request = tokio::spawn(async move {
        let active_cancellation = Arc::new(Mutex::new(ActiveCancellation {
            terminal_drain_required: true,
            ..ActiveCancellation::default()
        }));
        run_request_with_cancellation(
            async {
                tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
                Ok(())
            },
            &request_cancel,
            &shutdown_token,
            &active_cancellation,
        )
        .await
    });
    tokio::task::yield_now().await;
    shutdown.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), request)
        .await
        .expect("shutdown should await the active terminal outcome")
        .expect("request task should not panic");
    assert!(
        result.is_ok(),
        "shutdown must not replace a late successful terminal with cancellation"
    );
}

#[tokio::test]
async fn shutdown_preempts_and_drops_attachment_loading() {
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let request_cancel = CancellationToken::new();
    let shutdown_token = CancellationToken::new();
    let shutdown = shutdown_token.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let entered_by_load = Arc::clone(&entered);
    let dropped = Arc::new(AtomicBool::new(false));
    let dropped_by_load = Arc::clone(&dropped);

    let loading = tokio::spawn(async move {
        load_until_cancelled(
            async move {
                let _drop_flag = DropFlag(dropped_by_load);
                entered_by_load.notify_one();
                std::future::pending::<Vec<ContentBlock>>().await
            },
            &request_cancel,
            &shutdown_token,
        )
        .await
    });

    entered.notified().await;
    shutdown.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_millis(100), loading)
        .await
        .expect("shutdown should preempt attachment loading")
        .expect("attachment loading task should not panic");
    assert!(matches!(result, CancellableLoad::Shutdown));
    assert!(
        dropped.load(Ordering::SeqCst),
        "shutdown must drop the in-flight attachment future"
    );
}
