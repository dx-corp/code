use super::*;

#[derive(Debug)]
pub(super) struct BlockedPromptRuntimeExecutor {
    pub(super) calls: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) started: Arc<AtomicBool>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

// Release blocked fake work before a failing fixture tears down its runtime.
pub(super) struct BlockedPromptRuntimeExecutorReleaseGuard(
    pub(super) Arc<BlockedPromptRuntimeExecutor>,
);

impl Drop for BlockedPromptRuntimeExecutorReleaseGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

impl BlockedPromptRuntimeExecutor {
    pub(super) fn new() -> Self {
        Self {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            started: Arc::new(AtomicBool::new(false)),
            release: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    pub(super) fn release(&self) {
        let (released, wake) = &*self.release;
        *released.lock().expect("release state") = true;
        wake.notify_all();
    }
}

impl HostedRunnerHeadlessMessageExecutor for BlockedPromptRuntimeExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        if matches!(message, ToAgentMessage::Prompt { .. }) || is_control_response_message(&message)
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.store(true, Ordering::SeqCst);
            let (released, wake) = &*self.release;
            let mut released = released.lock().expect("release state");
            while !*released {
                released = wake.wait(released).expect("release state");
            }
            return Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
                vec![FromAgentMessage::ResponseStart {
                    response_id: "response-blocked".to_string(),
                }],
                "released blocked prompt",
            ));
        }
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "ignored by blocked prompt executor",
        ))
    }
}

#[tokio::test]
async fn blocked_prompt_executor_cleanup_preserves_an_early_fixture_failure() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(BlockedPromptRuntimeExecutor::new());
    let context = executor_context_with_response_key(workspace.path(), "cleanup-regression");
    let worker_executor = executor.clone();
    let worker = tokio::task::spawn_blocking(move || {
        worker_executor.execute(
            &context,
            ToAgentMessage::ClientToolResult {
                call_id: "cleanup-regression".to_string(),
                content: Vec::new(),
                is_error: false,
            },
        )
    });
    let started = tokio::time::timeout(Duration::from_secs(1), async {
        while !executor.started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let failure = std::panic::catch_unwind(|| {
        let _release_guard = BlockedPromptRuntimeExecutorReleaseGuard(executor.clone());
        panic!("intentional fixture assertion failure before explicit release");
    });
    let completion = tokio::time::timeout(Duration::from_secs(1), worker).await;
    // Keep the regression itself safe when cleanup is broken.
    executor.release();
    assert!(started.is_ok(), "blocked fixture should start");
    assert!(
        failure.is_err(),
        "the original fixture failure must remain visible"
    );
    completion
        .expect("panic cleanup must release the blocked fixture")
        .expect("blocked fixture worker should not panic")
        .expect("blocked fixture should retain its normal response");
}
