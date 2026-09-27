use super::*;
use crate::types::ProviderStreamErrorKind;
use std::time::Duration;

async fn run_attempts(cooldown_attempt: usize, cooldown_delay: Duration) -> Vec<Duration> {
    let started = tokio::time::Instant::now();
    let mut attempts = Vec::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    forward_stream_with_idle_policy(
        None::<CancellableStream>,
        || {
            attempts.push(started.elapsed());
            let (attempt_tx, attempt_rx) = mpsc::unbounded_channel();
            let stream = if attempts.len() == cooldown_attempt {
                attempt_tx
                    .send(StreamEvent::ProviderError {
                        kind: ProviderStreamErrorKind::TransientProtocol,
                        message: "API error 502 Bad Gateway".into(),
                    })
                    .unwrap();
                CancellableStream::detached(attempt_rx).with_retry_after(cooldown_delay)
            } else {
                let event = if attempts.len() <= 3 && attempts.len() < cooldown_attempt {
                    StreamEvent::ProviderError {
                        kind: ProviderStreamErrorKind::TransientProtocol,
                        message: "API error 502 Bad Gateway".into(),
                    }
                } else {
                    StreamEvent::MessageStop { stop_reason: None }
                };
                attempt_tx.send(event).unwrap();
                CancellableStream::detached(attempt_rx)
            };
            async move { Ok(stream) }
        },
        Duration::from_millis(100),
        MANAGED_GATEWAY_STREAM_MAX_RETRIES,
        tx,
    )
    .await;
    assert!(matches!(rx.try_recv(), Ok(StreamEvent::MessageStop { .. })));
    assert!(rx.try_recv().is_err());
    attempts
}

#[tokio::test(start_paused = true)]
async fn attempted_failure_that_opens_cooldown_skips_blind_retry() {
    assert_eq!(
        run_attempts(2, Duration::from_secs(29)).await,
        [
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(30)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn managed_cooldown_waits_then_gets_one_bounded_recovery_attempt() {
    assert_eq!(
        run_attempts(3, Duration::from_secs(30)).await,
        [
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(3),
            Duration::from_secs(33),
        ]
    );
}
