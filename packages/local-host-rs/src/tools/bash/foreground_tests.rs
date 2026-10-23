use super::*;

#[cfg(unix)]
#[tokio::test]
async fn foreground_exit_preserves_output_despite_inherited_pipes() {
    let workspace = tempfile::tempdir().expect("workspace should be created");
    let tool = BashTool::new(workspace.path().display().to_string());
    let result = tool
        .execute(BashArgs {
            command: "sleep 30 & printf '%s' $! > descendant.pid; printf foreground; printf diagnostic >&2; exit 42".to_string(),
            timeout: Some(2_000),
            description: None,
            run_in_background: false,
            bypass_sandbox: false,
        })
        .await;
    // Always clean up the inherited-pipe holder, including on the old path.
    if let Some(pid) = std::fs::read_to_string(workspace.path().join("descendant.pid"))
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    {
        kill_process_tree_tracked(pid);
    }
    let details = BashDetails::from_json(result.details.as_ref().expect("bash details"))
        .expect("valid bash details");
    assert_eq!(
        details.exit_code, 42,
        "foreground status must survive: {result:?}"
    );
    let output = result.output;
    assert!(output.contains("foreground"), "{output}");
    assert!(output.contains("diagnostic"), "{output}");
}

#[tokio::test]
async fn streams_foreground_output_before_command_exit() {
    let workspace = tempfile::tempdir().expect("streaming workspace");
    let release = workspace.path().join("release");
    let tool = BashTool::new(workspace.path().to_string_lossy().to_string());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        tool.execute_with_cancellation_and_output(
            BashArgs {
                command: "printf first; while [ ! -f release ]; do sleep 0.01; done; printf second"
                    .to_string(),
                timeout: Some(10_000),
                description: None,
                run_in_background: false,
                bypass_sandbox: false,
            },
            None,
            Some(tx),
        )
        .await
    });

    let first_chunk = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("streaming output should arrive before the command exits")
        .expect("output channel should remain open until completion");
    assert!(first_chunk.content.contains("first"));
    assert!(
        !task.is_finished(),
        "output must arrive before command exit"
    );
    std::fs::write(release, "continue").expect("release streaming command");
    let result = task.await.expect("bash task should join");
    assert!(result.success);
    assert!(result.output.contains("firstsecond"));
}
