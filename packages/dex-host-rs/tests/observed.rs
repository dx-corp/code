//! The same paused-consumer regression runs against the old payload channel
//! and the notification channel. It checks producer completion, queue size,
//! and accepted log content without depending on the receiver payload type.

use std::time::Duration;

use dex_loop::{Event, Log, ThreadId};
use maestro_dex_host::{LocalLog, ObservedLog};
use tempfile::TempDir;

#[tokio::test]
async fn paused_consumer_keeps_delivery_bounded_without_blocking_accepted_writes() {
    let dir = TempDir::new().expect("tempdir");
    let local = LocalLog::acquire(
        dir.path(),
        &ThreadId {
            org: "observer-org".into(),
            workspace: "observer-workspace".into(),
            thread: "observer-thread".into(),
        },
    )
    .await
    .expect("acquire log");
    let (log, observed) = ObservedLog::new(local.clone());
    let producer = tokio::spawn(async move {
        for index in 0..128 {
            log.append_text(format!("delta-{index}"))
                .await
                .expect("accept text");
        }
        log.append(&vec![Event::Interrupted; 128])
            .await
            .expect("accept event batch");
    });
    // Keep the connected receiver paused until the producer finishes.
    tokio::time::timeout(Duration::from_secs(10), producer)
        .await
        .expect("observer speed must not block writes")
        .expect("producer joined");
    assert!(
        observed.len() <= 1,
        "paused delivery must coalesce wakeups, queued {} payloads",
        observed.len()
    );
    let rows = local.read_all().await.expect("read accepted rows");
    assert_eq!(
        rows.len(),
        256,
        "bounded delivery must not lose accepted data"
    );
    for (index, (_, event)) in rows[..128].iter().enumerate() {
        assert_eq!(
            event,
            &Event::TextDelta {
                text: format!("delta-{index}")
            }
        );
    }
    assert!(
        rows[128..]
            .iter()
            .all(|(_, event)| *event == Event::Interrupted)
    );
}
