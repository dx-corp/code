//! A durable claim/record ledger for local mutations, keyed by `CallId`.
//!
//! Maestro's tool registry has nothing like this today: a local mutation
//! either ran or it didn't, and a process that crashes mid-write has no
//! record of whether the write landed. This adds the `Effects` contract's
//! guarantee — a mutation is dispatched at most once per call id, even
//! across a restart — on top of one JSON file.
//!
//! Access is serialized by an in-process `tokio::sync::Mutex`, not a file
//! lock: unlike [`crate::log::LocalLog`], this ledger does not need to
//! defend against a second, concurrent process racing the same thread — a
//! local Maestro session already assumes one running process per workspace.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dex_loop::{CallId, Claim, Effects, Fenced, Outcome, Output, ProposedCall, ToolResult};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// `None` means claimed but not yet recorded — a mutation dispatched by a
/// process that has not yet appended its outcome (including one that
/// crashed between claim and record).
type Ledger = HashMap<String, Option<ToolResult>>;

#[derive(Default, Serialize, Deserialize)]
struct Persisted(Ledger);

#[derive(Clone)]
pub struct LocalEffects {
    path: Arc<PathBuf>,
    ledger: Arc<Mutex<Ledger>>,
}

impl LocalEffects {
    /// Loads any ledger already at `path` (a fresh thread has none).
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let path = path.as_ref().to_path_buf();
        let ledger = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let Persisted(ledger) =
                    serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
                ledger
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path: Arc::new(path),
            ledger: Arc::new(Mutex::new(ledger)),
        })
    }

    async fn persist(&self, ledger: &Ledger) -> Result<(), Fenced> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                Fenced::new(format!("could not create ledger directory: {error}"))
            })?;
        }
        let bytes = serde_json::to_vec(&Persisted(ledger.clone()))
            .map_err(|error| Fenced::new(format!("ledger does not encode: {error}")))?;
        // Write-then-rename: a crash mid-write leaves the previous ledger in
        // place instead of a truncated one.
        let temp = self.path.with_extension("json.tmp");
        tokio::fs::write(&temp, &bytes)
            .await
            .map_err(|error| Fenced::new(format!("could not write ledger: {error}")))?;
        tokio::fs::rename(&temp, self.path.as_path())
            .await
            .map_err(|error| Fenced::new(format!("could not commit ledger: {error}")))
    }
}

fn running() -> ToolResult {
    ToolResult {
        outcome: Outcome::Running,
        output: Output::Text("dispatched; no outcome recorded yet".into()),
        receipt: None,
    }
}

impl Effects for LocalEffects {
    async fn claim(&self, call: &ProposedCall) -> Result<Claim, Fenced> {
        let mut ledger = self.ledger.lock().await;
        match ledger.get(call.id.as_str()) {
            Some(Some(result)) => Ok(Claim::Existing(result.clone())),
            Some(None) => Ok(Claim::Existing(running())),
            None => {
                ledger.insert(call.id.as_str().to_owned(), None);
                self.persist(&ledger).await?;
                Ok(Claim::Granted)
            }
        }
    }

    async fn record(&self, call: &CallId, result: &ToolResult) -> Result<(), Fenced> {
        let mut ledger = self.ledger.lock().await;
        ledger.insert(call.as_str().to_owned(), Some(result.clone()));
        self.persist(&ledger).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_loop::{PrincipalId, ProposedCall, ToolName};
    use tempfile::TempDir;

    fn call(id: &str) -> ProposedCall {
        ProposedCall::new(
            CallId::new(id),
            ToolName::new("fs.write_file"),
            serde_json::json!({"path": "a.txt", "content": "x"}),
            PrincipalId::new("alice"),
        )
    }

    #[tokio::test]
    async fn first_claim_grants_and_second_claim_sees_running() {
        let dir = TempDir::new().expect("tempdir");
        let effects = LocalEffects::open(dir.path().join("ledger.json"))
            .await
            .expect("open");
        let call = call("c1");
        assert_eq!(effects.claim(&call).await.expect("claim"), Claim::Granted);
        let second = effects.claim(&call).await.expect("claim again");
        assert!(matches!(second, Claim::Existing(result) if result.outcome == Outcome::Running));
    }

    #[tokio::test]
    async fn recorded_outcome_is_returned_on_a_later_claim() {
        let dir = TempDir::new().expect("tempdir");
        let effects = LocalEffects::open(dir.path().join("ledger.json"))
            .await
            .expect("open");
        let call = call("c1");
        effects.claim(&call).await.expect("claim");
        effects
            .record(&call.id, &ToolResult::text("done"))
            .await
            .expect("record");
        let claim = effects.claim(&call).await.expect("claim again");
        assert_eq!(claim, Claim::Existing(ToolResult::text("done")));
    }

    #[tokio::test]
    async fn ledger_survives_a_reopen_at_the_same_path() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("ledger.json");
        let first = LocalEffects::open(&path).await.expect("open");
        let call = call("c1");
        first.claim(&call).await.expect("claim");
        first
            .record(&call.id, &ToolResult::text("done"))
            .await
            .expect("record");

        // A fresh handle, as a restarted process would create.
        let reopened = LocalEffects::open(&path).await.expect("reopen");
        let claim = reopened.claim(&call).await.expect("claim after reopen");
        assert_eq!(claim, Claim::Existing(ToolResult::text("done")));
    }

    #[tokio::test]
    async fn distinct_call_ids_are_independent() {
        let dir = TempDir::new().expect("tempdir");
        let effects = LocalEffects::open(dir.path().join("ledger.json"))
            .await
            .expect("open");
        assert_eq!(
            effects.claim(&call("a")).await.expect("claim a"),
            Claim::Granted
        );
        assert_eq!(
            effects.claim(&call("b")).await.expect("claim b"),
            Claim::Granted
        );
    }
}
