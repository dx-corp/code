//! Cancellation-aware waits and the request terminal-drain boundary.

use super::*;

pub(super) async fn wait_for_retry_delay(
    delay: std::time::Duration,
    request_cancel: &CancellationToken,
    shutdown_token: &CancellationToken,
) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => true,
        () = request_cancel.cancelled() => false,
        () = shutdown_token.cancelled() => false,
    }
}

pub(super) async fn wait_for_codex_auth_refresh(
    host: &NativeExecutionHostHandle,
    auth_path: &std::path::Path,
    request_cancel: &CancellationToken,
    shutdown_token: &CancellationToken,
    max_wait: Duration,
) -> bool {
    let baseline = tokio::fs::read(auth_path).await.ok();
    let wait = async {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let current = tokio::fs::read(auth_path).await.ok();
            if current != baseline && host.codex_auth_is_usable(auth_path) {
                return true;
            }
        }
    };
    tokio::select! {
        result = tokio::time::timeout(max_wait, wait) => result.unwrap_or(false),
        () = request_cancel.cancelled() => false,
        () = shutdown_token.cancelled() => false,
    }
}

pub(super) enum CancellableLoad<T> {
    Loaded(T),
    RequestCancelled,
    Shutdown,
}

pub(super) async fn load_until_cancelled<F, T>(
    load: F,
    request_cancel: &CancellationToken,
    shutdown_token: &CancellationToken,
) -> CancellableLoad<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(load);
    tokio::select! {
        biased;
        () = shutdown_token.cancelled() => CancellableLoad::Shutdown,
        () = request_cancel.cancelled() => CancellableLoad::RequestCancelled,
        loaded = &mut load => CancellableLoad::Loaded(loaded),
    }
}

const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub(super) async fn run_request_with_cancellation<F>(
    request: F,
    request_cancel: &CancellationToken,
    shutdown_token: &CancellationToken,
    active_cancellation: &Arc<Mutex<ActiveCancellation>>,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    tokio::pin!(request);
    tokio::select! {
        biased;
        () = shutdown_token.cancelled() => {
            let terminal_drain_required = || {
                active_cancellation
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .terminal_drain_required
            };
            if terminal_drain_required() {
                // A mutating native tool may already have committed its
                // workspace change before bounded follow-up work (for example
                // LSP diagnostics) finishes. Keep the request alive through
                // batch cleanup so its one receipt-bearing terminal remains
                // truthful and cannot invite a duplicate retry.
                (&mut request).await
            } else {
                // Provider and legacy waits that have not crossed a mutating
                // native boundary retain bounded shutdown. Recheck after the
                // bound because polling the request may have crossed into a
                // mutating tool after shutdown won the outer selector.
                match tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, &mut request).await {
                    Ok(result) => result,
                    Err(_) if terminal_drain_required() => (&mut request).await,
                    Err(_) => Err(anyhow::anyhow!("Request cancelled")),
                }
            }
        }
        () = request_cancel.cancelled() => {
            Err(anyhow::anyhow!("Request cancelled"))
        }
        result = &mut request => result,
    }
}
