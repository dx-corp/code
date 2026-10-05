//! The shared dex-loop recovery scenarios over the actual filesystem ports.
use dex_loop::{
    Cursor, Event, ThreadId,
    testing::{RecoveryHost, RecoveryScenario, tenant_isolation},
};
use maestro_dex_host::{LocalEffects, LocalLog};
use std::{path::PathBuf, sync::Arc};
use tempfile::TempDir;

struct LocalHost {
    root: Arc<TempDir>,
    ledger_path: PathBuf,
    thread: ThreadId,
    log: LocalLog,
    effects: LocalEffects,
}

impl LocalHost {
    async fn new() -> Self {
        let root = Arc::new(TempDir::new().expect("temp directory"));
        let thread = ThreadId {
            org: "recovery-org".into(),
            workspace: "recovery-workspace".into(),
            thread: "recovery-thread".into(),
        };
        Self::scoped(root, thread).await
    }

    async fn scoped(root: Arc<TempDir>, thread: ThreadId) -> Self {
        // LocalEffects is bound by the composition root to this thread's
        // state directory; it does not interpret tenant IDs itself.
        let ledger_path = root
            .path()
            .join(&thread.org)
            .join(&thread.workspace)
            .join(&thread.thread)
            .join("effects.json");
        let log = LocalLog::acquire(root.path(), &thread)
            .await
            .expect("acquire");
        // This is a per-thread ledger, as in the production local host. It is
        // deliberately separate from the log and survives a new generation.
        let effects = LocalEffects::open(&ledger_path).await.expect("open ledger");
        Self {
            root,
            ledger_path,
            thread,
            log,
            effects,
        }
    }
}

impl RecoveryHost for LocalHost {
    type Log = LocalLog;
    type Effects = LocalEffects;
    fn thread(&self) -> ThreadId {
        self.thread.clone()
    }
    fn log(&self) -> LocalLog {
        self.log.clone()
    }
    fn effects(&self) -> LocalEffects {
        self.effects.clone()
    }
    async fn events(&self) -> Vec<(Cursor, Event)> {
        self.log.read_all().await.expect("read durable events")
    }
    async fn restart(&mut self) {
        self.log = LocalLog::acquire(self.root.path(), &self.thread)
            .await
            .expect("successor generation");
        self.effects = LocalEffects::open(&self.ledger_path)
            .await
            .expect("reopen ledger");
    }
}

macro_rules! recovery_test {
    ($name:ident, $scenario:ident) => {
        #[tokio::test]
        async fn $name() {
            RecoveryScenario::$scenario
                .run(&mut LocalHost::new().await)
                .await;
        }
    };
}
recovery_test!(recovery_lost_lease, LostLease);
recovery_test!(recovery_claim_without_result, ClaimWithoutResult);
recovery_test!(recovery_recorded_result_replay, RecordedResultReplay);
recovery_test!(recovery_duplicate_claim, DuplicateClaim);
recovery_test!(recovery_cancellation, Cancellation);

#[tokio::test]
async fn recovery_tenant_isolation() {
    for field in ["organization", "workspace"] {
        let mut left = LocalHost::new().await;
        let mut scope = left.thread.clone();
        if field == "organization" {
            scope.org = "foreign-org".into();
        } else {
            scope.workspace = "foreign-workspace".into();
        }
        let mut right = LocalHost::scoped(left.root.clone(), scope).await;
        tenant_isolation(&mut left, &mut right).await;
    }
}
