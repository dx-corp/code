use super::*;

#[tokio::test]
async fn typed_bash_streaming_releases_low_volume_output_before_completion() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path().to_str().unwrap());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let args = serde_json::json!({
        "command": "touch stream-start; while [ ! -f stream-emit ]; do sleep 0.01; done; printf 'server ready'; while [ ! -f stream-finish ]; do sleep 0.01; done; printf ' done'"
    });
    let execution = executor.execute_with_receipt("bash", &args, Some(&tx), "stream-low-volume");
    tokio::pin!(execution);

    // Measure streaming after the real child starts; cold process startup is
    // separate from the existing 500ms low-volume output deadline.
    let started = async {
        while !dir.path().join("stream-start").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        result = &mut execution => panic!("Bash completed before fixture startup: {result:?}"),
        result = tokio::time::timeout(std::time::Duration::from_secs(5), started) => {
            result.expect("Bash fixture starts");
        }
    }
    std::fs::write(dir.path().join("stream-emit"), b"emit").unwrap();
    let deadline = tokio::time::sleep(std::time::Duration::from_millis(500));
    tokio::pin!(deadline);

    let first_output = loop {
        tokio::select! {
            biased;
            Some(event) = rx.recv() => {
                if let FromAgent::ToolOutput { content, .. } = event {
                    break content;
                }
            }
            result = &mut execution => panic!("Bash completed before streaming output: {result:?}"),
            () = &mut deadline => panic!("low-volume Bash output was buffered until completion"),
        }
    };
    assert!(
        first_output.contains("server ready"),
        "first output: {first_output:?}"
    );
    assert!(!dir.path().join("stream-finish").exists());
    // Completion is impossible until the observer has received streamed output.
    std::fs::write(dir.path().join("stream-finish"), b"finish").unwrap();
    let result = execution.await;
    assert!(matches!(result.outcome, ToolOutcome::Succeeded { .. }));
    let output = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            FromAgent::ToolOutput { content, .. } => Some(content),
            _ => None,
        })
        .collect::<String>();
    assert!(output.contains(" done"), "final output: {output:?}");
}
