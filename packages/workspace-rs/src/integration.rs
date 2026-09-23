//! Conflict-aware Git integration for dynamic Maestro workflow children.
//!
//! A child is created from an explicit base revision and works in its own Git
//! worktree.  Integration is performed in another temporary worktree, so a
//! merge conflict can never leave the user's checkout with conflict markers.
//! The resulting commit is installed with a compare-and-swap update of two
//! owned refs: the workflow aggregate and the contribution receipt ref.  The
//! ref pair is the recovery truth; the JSON receipt is an immutable, convenient
//! cache for callers and inspection tools.
//!
//! This module deliberately reports Git-level conflicts only.  Whether two
//! independently valid changes are semantically compatible belongs to the
//! workflow verifier that runs against [`IntegrationReceipt::result_sha`].

use crate::worktree::WorktreeSession;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

const RECEIPT_SCHEMA_VERSION: u8 = 1;
const GIT_PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
// Readers run on separate threads and can be delayed after Git exits on a busy host.
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const ZERO_OID_SHA1: &str = "0000000000000000000000000000000000000000";
const ZERO_OID_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Errors returned by the Git integration boundary.
#[derive(Debug, Error)]
pub enum IntegrationError {
    /// The request contains an unsafe or otherwise unusable identifier.
    #[error("invalid integration request: {0}")]
    InvalidRequest(String),

    /// A path is outside its repository or contains a path traversal.
    #[error("path escapes repository: {path}")]
    PathEscape { path: PathBuf },

    /// A path traverses a symlink and therefore cannot be safely scoped.
    #[error("path traverses a symlink: {path}")]
    SymlinkPath { path: PathBuf },

    /// The source or aggregate worktree contains local changes.
    #[error("worktree {path} has uncommitted or untracked changes: {status}")]
    DirtyWorktree { path: PathBuf, status: String },

    /// The source worktree is not at the revision it claims to contribute.
    #[error("source revision changed: expected {expected}, found {actual}")]
    SourceRevisionChanged { expected: String, actual: String },

    /// The requested base is no longer an ancestor of the child contribution.
    #[error("source {source_sha} is not based on {expected_base}")]
    BaseChanged {
        expected_base: String,
        source_sha: String,
    },

    /// The serialized aggregate moved since the caller read it.
    #[error("aggregate revision changed: expected {expected}, found {actual}")]
    AggregateMoved { expected: String, actual: String },

    /// Git could not apply the contribution without textual conflicts.
    #[error("textual merge conflict in: {paths:?}")]
    TextualConflict { paths: Vec<String> },

    /// The source commit is already an ancestor of the aggregate.
    #[error("source commit {source_sha} is already integrated")]
    DuplicateCommit { source_sha: String },

    /// The contribution has no tree change relative to the aggregate.
    #[error("source contribution has an empty diff")]
    EmptyDiff,

    /// A contribution id was already bound to a different result.
    #[error("contribution key is already bound to result {existing}")]
    ContributionKeyReuse { existing: String },

    /// A receipt exists without the Git ref that makes it recoverable.
    #[error("receipt exists without its contribution ref: {path}")]
    OrphanedReceipt { path: PathBuf },

    /// A receipt or its commit metadata is malformed or inconsistent.
    #[error("invalid integration receipt: {0}")]
    Receipt(String),

    /// An owned ref was replaced with a symbolic ref and must not be followed.
    #[error("owned ref {reference} is symbolic ({target})")]
    OwnedRefSymbolic { reference: String, target: String },

    /// A contribution ref without its aggregate ref cannot be repaired safely.
    #[error("contribution ref {contribution_ref} exists without its aggregate ref")]
    MissingAggregateRef { contribution_ref: String },

    /// An atomic receipt write failed.
    #[error("could not persist integration receipt {path}: {reason}")]
    ReceiptWrite { path: PathBuf, reason: String },

    /// A Git operation could not be started or completed.
    #[error("git command `{command}` failed ({status:?}): {stderr}")]
    GitCommand {
        command: String,
        status: Option<i32>,
        stderr: String,
    },

    /// A temporary worktree operation failed.
    #[error("temporary integration worktree failed: {0}")]
    Worktree(String),

    /// The coordinator was poisoned by a panic in another caller.
    #[error("integration coordinator lock was poisoned")]
    LockPoisoned,
}

/// A child contribution to be integrated into one workflow aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationRequest {
    /// Stable parent workflow identity. It scopes the aggregate and receipt refs.
    pub workflow_id: String,
    /// Stable child contribution identity.
    pub contribution_id: String,
    /// Child worktree containing the committed contribution.
    pub source_worktree: PathBuf,
    /// Full immutable object ID for the source worktree `HEAD`.
    pub source_revision: String,
    /// Full immutable object ID from which the child was created.
    pub base_revision: String,
    /// A clean checkout in the same repository. It is used for scope and
    /// cleanliness checks; integration never writes to this checkout.
    pub aggregate_worktree: PathBuf,
    /// Full immutable object ID for the aggregate revision observed by the
    /// caller.
    pub aggregate_revision: String,
    /// Optional declared write scope. Actual changed paths must be contained in
    /// this set when it was supplied with [`Self::with_write_scope`]. An
    /// explicitly empty scope denies all changed paths; leaving the scope
    /// unspecified preserves the legacy unrestricted behavior.
    pub expected_write_paths: Vec<PathBuf>,
    write_scope_declared: bool,
}

impl IntegrationRequest {
    /// Construct a request from the source and aggregate worktrees.
    pub fn new(
        workflow_id: impl Into<String>,
        contribution_id: impl Into<String>,
        source_worktree: impl Into<PathBuf>,
        source_revision: impl Into<String>,
        base_revision: impl Into<String>,
        aggregate_worktree: impl Into<PathBuf>,
        aggregate_revision: impl Into<String>,
    ) -> Self {
        Self {
            workflow_id: workflow_id.into(),
            contribution_id: contribution_id.into(),
            source_worktree: source_worktree.into(),
            source_revision: source_revision.into(),
            base_revision: base_revision.into(),
            aggregate_worktree: aggregate_worktree.into(),
            aggregate_revision: aggregate_revision.into(),
            expected_write_paths: Vec::new(),
            write_scope_declared: false,
        }
    }

    /// Restrict integration to paths declared by the child.
    #[must_use]
    pub fn with_expected_write_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.expected_write_paths = paths.into_iter().map(Into::into).collect();
        self.write_scope_declared = true;
        self
    }

    /// Alias for callers that describe the field as a write scope.
    #[must_use]
    pub fn with_write_scope<I, P>(self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.with_expected_write_paths(paths)
    }
}

/// The durable Git-level result of integrating one child contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct IntegrationReceipt {
    /// Receipt schema version for future migrations.
    pub schema_version: u8,
    /// Parent workflow identity.
    pub workflow_id: String,
    /// Child contribution identity.
    pub contribution_id: String,
    /// Full source contribution commit SHA.
    pub source_sha: String,
    /// Full child base commit SHA.
    pub base_sha: String,
    /// Full aggregate commit SHA before this contribution.
    pub previous_aggregate_sha: String,
    /// Full result commit SHA to which verification must bind.
    pub result_sha: String,
    /// Paths changed by the integrated contribution.
    pub changed_paths: Vec<String>,
    /// Owned ref that identifies the aggregate revision.
    pub aggregate_ref: String,
    /// Owned ref that makes this contribution idempotent and recoverable.
    pub contribution_ref: String,
    /// Stable status for a successful Git integration.
    pub status: IntegrationStatus,
}

/// State recorded in a successful integration receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStatus {
    /// The contribution is installed in the workflow aggregate ref.
    Integrated,
}

/// Result returned by [`IntegrationCoordinator::integrate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationResult {
    /// Immutable source/base/result evidence.
    pub receipt: IntegrationReceipt,
    /// True when the result was recovered or replayed from existing refs.
    pub replayed: bool,
}

#[derive(Debug, Clone)]
struct OwnedRefs {
    aggregate: String,
    contribution: String,
    child: String,
}

/// Serializes integration attempts and owns the Git/ref/receipt boundary for
/// one repository.
#[derive(Clone, Debug)]
pub struct IntegrationCoordinator {
    repo_root: PathBuf,
    receipt_dir: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl IntegrationCoordinator {
    /// Create a coordinator with an explicit receipt directory.
    pub fn new(
        repo_root: impl AsRef<Path>,
        receipt_dir: impl AsRef<Path>,
    ) -> Result<Self, IntegrationError> {
        let repo_root = repository_root(repo_root.as_ref())?;
        let receipt_dir = absolute_path(receipt_dir.as_ref())?;
        Ok(Self {
            repo_root,
            receipt_dir,
            lock: Arc::new(Mutex::new(())),
        })
    }

    /// Create a coordinator using a receipt cache under the repository's
    /// common Git directory.
    pub fn default_at(repo_root: impl AsRef<Path>) -> Result<Self, IntegrationError> {
        let repo_root = repository_root(repo_root.as_ref())?;
        let receipt_dir = common_git_dir(&repo_root)?
            .join("maestro")
            .join("integrations");
        Self::new(repo_root, receipt_dir)
    }

    /// Repository root shared by all child and aggregate worktrees.
    #[must_use]
    pub fn repository_root(&self) -> &Path {
        &self.repo_root
    }

    /// Path where the immutable JSON receipt for a contribution is stored.
    pub fn receipt_path(
        &self,
        workflow_id: &str,
        contribution_id: &str,
    ) -> Result<PathBuf, IntegrationError> {
        validate_identifier(workflow_id, "workflow_id")?;
        validate_identifier(contribution_id, "contribution_id")?;
        Ok(self
            .receipt_dir
            .join(encode_component(workflow_id))
            .join(format!("{}.json", encode_component(contribution_id))))
    }

    /// Owned aggregate ref used for one workflow.
    pub fn aggregate_ref(&self, workflow_id: &str) -> Result<String, IntegrationError> {
        validate_identifier(workflow_id, "workflow_id")?;
        Ok(format!(
            "refs/maestro/workflows/{}/aggregate",
            encode_component(workflow_id)
        ))
    }

    /// Owned contribution ref used for one workflow child.
    pub fn contribution_ref(
        &self,
        workflow_id: &str,
        contribution_id: &str,
    ) -> Result<String, IntegrationError> {
        validate_identifier(workflow_id, "workflow_id")?;
        validate_identifier(contribution_id, "contribution_id")?;
        Ok(format!(
            "refs/maestro/workflows/{}/contributions/{}",
            encode_component(workflow_id),
            encode_component(contribution_id)
        ))
    }

    /// Owned child marker used to validate deterministic worktree reuse.
    pub fn child_ref(
        &self,
        workflow_id: &str,
        contribution_id: &str,
    ) -> Result<String, IntegrationError> {
        validate_identifier(workflow_id, "workflow_id")?;
        validate_identifier(contribution_id, "contribution_id")?;
        Ok(format!(
            "refs/maestro/workflows/{}/children/{}",
            encode_component(workflow_id),
            encode_component(contribution_id)
        ))
    }

    /// Create an isolated child worktree pinned to `base_revision`.
    pub fn create_child_worktree(
        &self,
        workflow_id: &str,
        contribution_id: &str,
        base_revision: &str,
    ) -> Result<WorktreeSession, IntegrationError> {
        validate_identifier(workflow_id, "workflow_id")?;
        validate_identifier(contribution_id, "contribution_id")?;
        let base_sha = resolve_commit(&self.repo_root, base_revision)?;
        let branch = format!(
            "maestro-child-{}-{}",
            branch_component(workflow_id),
            branch_component(contribution_id)
        );
        let child_ref = self.child_ref(workflow_id, contribution_id)?;
        let owner_base = read_ref(&self.repo_root, &child_ref)?;
        if let Some(owner_base) = owner_base.as_deref() {
            if owner_base != base_sha {
                return Err(IntegrationError::ContributionKeyReuse {
                    existing: owner_base.to_string(),
                });
            }
            let session = match WorktreeSession::reopen_in_at(&self.repo_root, &branch, &base_sha)
            {
                Ok(session) => session,
                Err(reopen_error) => WorktreeSession::create_in_at(
                    &self.repo_root,
                    &branch,
                    &base_sha,
                )
                .map_err(|create_error| {
                    IntegrationError::Worktree(format!(
                        "owned child recovery failed ({reopen_error}); creating a replacement also failed: {create_error}"
                    ))
                })?,
            };
            let head = resolve_commit(session.path(), "HEAD")?;
            if !is_ancestor(&self.repo_root, &base_sha, &head)? {
                return Err(IntegrationError::Worktree(format!(
                    "existing child worktree {} no longer descends from base {}",
                    session.path().display(),
                    base_sha
                )));
            }
            ensure_clean(session.path())?;
            return Ok(session);
        } else {
            let zero = repository_zero_oid(&self.repo_root)?;
            update_refs_atomic(
                &self.repo_root,
                &[RefUpdate {
                    name: &child_ref,
                    new: &base_sha,
                    old: &zero,
                }],
            )?;
        }

        WorktreeSession::create_in_at(&self.repo_root, &branch, &base_sha)
            .map_err(|error| IntegrationError::Worktree(format!("{error:#}")))
    }

    /// Integrate one clean child commit into the serialized workflow aggregate.
    ///
    /// The aggregate checkout is inspected but never reset, merged, or written
    /// by this method.  Callers that own a managed checkout can explicitly
    /// fast-forward it after verification using the returned `result_sha`.
    pub fn integrate(
        &self,
        request: IntegrationRequest,
    ) -> Result<IntegrationResult, IntegrationError> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned)?;
        self.integrate_locked(request)
    }

    fn integrate_locked(
        &self,
        request: IntegrationRequest,
    ) -> Result<IntegrationResult, IntegrationError> {
        validate_object_id(&request.source_revision, "source_revision")?;
        validate_object_id(&request.base_revision, "base_revision")?;
        validate_object_id(&request.aggregate_revision, "aggregate_revision")?;
        let refs = OwnedRefs {
            aggregate: self.aggregate_ref(&request.workflow_id)?,
            contribution: self.contribution_ref(&request.workflow_id, &request.contribution_id)?,
            child: self.child_ref(&request.workflow_id, &request.contribution_id)?,
        };
        let receipt_path = self.receipt_path(&request.workflow_id, &request.contribution_id)?;

        validate_worktree_path(&request.aggregate_worktree)?;
        ensure_same_repository(&self.repo_root, &request.aggregate_worktree)?;
        ensure_clean(&request.aggregate_worktree)?;

        let expected_aggregate_sha = resolve_commit(&self.repo_root, &request.aggregate_revision)?;
        let contribution_sha = read_ref(&self.repo_root, &refs.contribution)?;
        let aggregate_sha = read_ref(&self.repo_root, &refs.aggregate)?;
        let child_marker = read_ref(&self.repo_root, &refs.child)?;

        if let Some(contribution_sha) = contribution_sha {
            return self.recover_existing(
                &request,
                &refs,
                &receipt_path,
                &expected_aggregate_sha,
                aggregate_sha,
                &contribution_sha,
            );
        }
        if receipt_path.exists() {
            return Err(IntegrationError::OrphanedReceipt { path: receipt_path });
        }
        if let Some(actual) = aggregate_sha.as_deref() {
            if actual != expected_aggregate_sha {
                return Err(IntegrationError::AggregateMoved {
                    expected: expected_aggregate_sha,
                    actual: actual.to_string(),
                });
            }
        }

        validate_worktree_path(&request.source_worktree)?;
        ensure_same_repository(&self.repo_root, &request.source_worktree)?;
        ensure_clean(&request.source_worktree)?;
        let source_sha = resolve_commit(&self.repo_root, &request.source_revision)?;
        let source_head = resolve_commit(&request.source_worktree, "HEAD")?;
        if source_head != source_sha {
            return Err(IntegrationError::SourceRevisionChanged {
                expected: source_sha,
                actual: source_head,
            });
        }
        let base_sha = resolve_commit(&self.repo_root, &request.base_revision)?;
        if let Some(marker) = child_marker.as_deref() {
            if marker != base_sha {
                return Err(IntegrationError::BaseChanged {
                    expected_base: base_sha,
                    source_sha,
                });
            }
        }
        if !is_ancestor(&self.repo_root, &base_sha, &source_sha)? {
            return Err(IntegrationError::BaseChanged {
                expected_base: base_sha,
                source_sha,
            });
        }
        if is_ancestor(&self.repo_root, &source_sha, &expected_aggregate_sha)? {
            return Err(IntegrationError::DuplicateCommit { source_sha });
        }

        let changed_paths = changed_paths_between(&self.repo_root, &base_sha, &source_sha)?;
        if changed_paths.is_empty() {
            return Err(IntegrationError::EmptyDiff);
        }
        validate_declared_scope(
            request.write_scope_declared,
            &request.expected_write_paths,
            &changed_paths,
        )?;
        validate_changed_paths(
            &request.source_worktree,
            &request.aggregate_worktree,
            &changed_paths,
        )?;

        let previous_aggregate_sha = expected_aggregate_sha.clone();
        let result_sha =
            self.build_candidate(&request, &base_sha, &source_sha, &previous_aggregate_sha)?;
        let final_changed_paths = self.validate_candidate(
            &request,
            &base_sha,
            &source_sha,
            &previous_aggregate_sha,
            &result_sha,
        )?;
        validate_declared_scope(
            request.write_scope_declared,
            &request.expected_write_paths,
            &final_changed_paths,
        )?;
        validate_changed_paths(
            &request.source_worktree,
            &request.aggregate_worktree,
            &final_changed_paths,
        )?;
        validate_changed_tree_paths(
            &self.repo_root,
            &previous_aggregate_sha,
            &source_sha,
            &result_sha,
            &final_changed_paths,
        )?;
        let zero = repository_zero_oid(&self.repo_root)?;
        let old_aggregate = aggregate_sha.unwrap_or_else(|| zero.clone());
        let mut updates = vec![
            RefUpdate {
                name: &refs.aggregate,
                new: &result_sha,
                old: &old_aggregate,
            },
            RefUpdate {
                name: &refs.contribution,
                new: &result_sha,
                old: &zero,
            },
        ];
        if let Some(child_marker) = child_marker.as_deref() {
            updates.push(RefUpdate {
                name: &refs.child,
                new: &zero,
                old: child_marker,
            });
        }
        if let Err(error) = update_refs_atomic(&self.repo_root, &updates) {
            return self.classify_install_failure(
                &request,
                &refs,
                &receipt_path,
                &expected_aggregate_sha,
                error,
            );
        }

        let receipt = make_receipt(
            &request,
            &refs,
            &base_sha,
            &source_sha,
            &previous_aggregate_sha,
            &result_sha,
            final_changed_paths,
        );
        persist_receipt(&receipt_path, &receipt)?;
        Ok(IntegrationResult {
            receipt,
            replayed: false,
        })
    }

    fn build_candidate(
        &self,
        request: &IntegrationRequest,
        base_sha: &str,
        source_sha: &str,
        previous_aggregate_sha: &str,
    ) -> Result<String, IntegrationError> {
        let path = temporary_worktree_path(&request.workflow_id, &request.contribution_id)?;
        let hooks_path = temporary_hooks_path(&request.workflow_id, &request.contribution_id);
        fs::create_dir(&hooks_path).map_err(|error| {
            IntegrationError::Worktree(format!(
                "create isolated hooks directory {}: {error}",
                hooks_path.display()
            ))
        })?;
        let path_arg = path.clone().into_os_string();
        let add = match run_git_allow_failure_with_hooks(
            &self.repo_root,
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--detach"),
                path_arg,
                OsString::from(previous_aggregate_sha),
            ],
            &hooks_path,
        ) {
            Ok(output) => output,
            Err(error) => {
                let _ = fs::remove_dir_all(&hooks_path);
                return Err(error);
            }
        };
        if !add.status.success() {
            let _ = fs::remove_dir_all(&hooks_path);
            return Err(git_failure("git worktree add", &add));
        }
        let mut temporary = TemporaryWorktree::new(self.repo_root.clone(), path, hooks_path);

        let merge = run_git_allow_failure_with_hooks(
            temporary.path(),
            vec![
                OsString::from("merge"),
                OsString::from("--no-commit"),
                OsString::from("--no-ff"),
                OsString::from("--no-edit"),
                OsString::from(source_sha),
            ],
            temporary.hooks_path(),
        )?;
        if !merge.status.success() {
            let conflicts = unmerged_paths(temporary.path())?;
            let _ = run_git_allow_failure_with_hooks(
                temporary.path(),
                vec![OsString::from("merge"), OsString::from("--abort")],
                temporary.hooks_path(),
            );
            temporary.remove_best_effort();
            if !conflicts.is_empty() {
                return Err(IntegrationError::TextualConflict { paths: conflicts });
            }
            return Err(git_failure("git merge", &merge));
        }

        let cached = run_git_allow_failure_with_hooks(
            temporary.path(),
            vec![
                OsString::from("diff"),
                OsString::from("--cached"),
                OsString::from("--quiet"),
                OsString::from("--"),
            ],
            temporary.hooks_path(),
        )?;
        if cached.status.success() {
            temporary.remove_best_effort();
            return Err(IntegrationError::EmptyDiff);
        }
        if cached.status.code() != Some(1) {
            temporary.remove_best_effort();
            return Err(git_failure("git diff --cached --quiet", &cached));
        }

        let message = format!(
            "Maestro workflow integration\n\nMaestro-Workflow: {}\nMaestro-Contribution: {}\nMaestro-Source: {source_sha}\nMaestro-Base: {base_sha}\nMaestro-Previous-Aggregate: {previous_aggregate_sha}\n",
            request.workflow_id, request.contribution_id
        );
        let commit = run_git_allow_failure_with_hooks(
            temporary.path(),
            vec![
                OsString::from("-c"),
                OsString::from("user.name=Maestro Integration"),
                OsString::from("-c"),
                OsString::from("user.email=maestro-integration@localhost"),
                OsString::from("commit"),
                OsString::from("-m"),
                OsString::from(message),
            ],
            temporary.hooks_path(),
        )?;
        if !commit.status.success() {
            temporary.remove_best_effort();
            return Err(git_failure("git commit", &commit));
        }
        let result_sha = resolve_commit(temporary.path(), "HEAD")?;
        self.validate_candidate(
            request,
            base_sha,
            source_sha,
            previous_aggregate_sha,
            &result_sha,
        )?;
        temporary.remove()?;
        Ok(result_sha)
    }

    fn validate_candidate(
        &self,
        request: &IntegrationRequest,
        base_sha: &str,
        source_sha: &str,
        previous_aggregate_sha: &str,
        result_sha: &str,
    ) -> Result<Vec<PathBuf>, IntegrationError> {
        let metadata = read_integration_metadata(&self.repo_root, result_sha)?;
        if metadata.workflow_id != request.workflow_id {
            return Err(IntegrationError::Receipt(format!(
                "result belongs to workflow {}",
                metadata.workflow_id
            )));
        }
        if metadata.contribution_id != request.contribution_id {
            return Err(IntegrationError::Receipt(format!(
                "result belongs to contribution {}",
                metadata.contribution_id
            )));
        }
        if metadata.source_sha != source_sha {
            return Err(IntegrationError::Receipt(format!(
                "result source trailer does not match source commit: expected {}, found {}",
                source_sha, metadata.source_sha
            )));
        }
        if metadata.base_sha != base_sha {
            return Err(IntegrationError::Receipt(format!(
                "result base trailer does not match child base: expected {}, found {}",
                base_sha, metadata.base_sha
            )));
        }
        if metadata.previous_aggregate_sha != previous_aggregate_sha {
            return Err(IntegrationError::Receipt(format!(
                "result aggregate trailer does not match previous aggregate: expected {}, found {}",
                previous_aggregate_sha, metadata.previous_aggregate_sha
            )));
        }
        if !is_ancestor(&self.repo_root, base_sha, source_sha)? {
            return Err(IntegrationError::BaseChanged {
                expected_base: base_sha.to_string(),
                source_sha: source_sha.to_string(),
            });
        }
        if is_ancestor(&self.repo_root, source_sha, previous_aggregate_sha)? {
            return Err(IntegrationError::DuplicateCommit {
                source_sha: source_sha.to_string(),
            });
        }
        let source_parents = commit_parents(&self.repo_root, source_sha)?;
        if source_parents.is_empty()
            || !is_first_parent_ancestor(&self.repo_root, base_sha, source_sha)?
        {
            return Err(IntegrationError::BaseChanged {
                expected_base: base_sha.to_string(),
                source_sha: source_sha.to_string(),
            });
        }
        resolve_tree(&self.repo_root, source_sha)?;
        if changed_paths_between(&self.repo_root, base_sha, source_sha)?.is_empty() {
            return Err(IntegrationError::EmptyDiff);
        }
        let result_parents = commit_parents(&self.repo_root, result_sha)?;
        if result_parents.len() != 2
            || result_parents[0] != previous_aggregate_sha
            || result_parents[1] != source_sha
        {
            return Err(IntegrationError::Receipt(format!(
                "result {} must have aggregate {} and source {} as its parents",
                result_sha, previous_aggregate_sha, source_sha
            )));
        }
        let expected_tree =
            merge_tree_in_scratch_worktree(&self.repo_root, previous_aggregate_sha, source_sha)?;
        let result_tree = resolve_tree(&self.repo_root, result_sha)?;
        if result_tree != expected_tree {
            return Err(IntegrationError::Receipt(format!(
                "result {} tree {} does not match merge tree {}",
                result_sha, result_tree, expected_tree
            )));
        }
        let final_changed_paths =
            changed_paths_between(&self.repo_root, previous_aggregate_sha, result_sha)?;
        if final_changed_paths.is_empty() {
            return Err(IntegrationError::EmptyDiff);
        }
        Ok(final_changed_paths)
    }

    fn recover_existing(
        &self,
        request: &IntegrationRequest,
        refs: &OwnedRefs,
        receipt_path: &Path,
        expected_aggregate_sha: &str,
        aggregate_sha: Option<String>,
        contribution_sha: &str,
    ) -> Result<IntegrationResult, IntegrationError> {
        if aggregate_sha.is_none() {
            return Err(IntegrationError::MissingAggregateRef {
                contribution_ref: refs.contribution.clone(),
            });
        }
        let requested_source = resolve_commit(&self.repo_root, &request.source_revision)?;
        let requested_base = resolve_commit(&self.repo_root, &request.base_revision)?;
        let child_marker = read_ref(&self.repo_root, &refs.child)?;
        if let Some(marker) = child_marker.as_deref() {
            if marker != requested_base {
                return Err(IntegrationError::BaseChanged {
                    expected_base: requested_base,
                    source_sha: requested_source,
                });
            }
        }
        let final_changed_paths = self.validate_candidate(
            request,
            &requested_base,
            &requested_source,
            expected_aggregate_sha,
            contribution_sha,
        )?;
        validate_declared_scope(
            request.write_scope_declared,
            &request.expected_write_paths,
            &final_changed_paths,
        )?;
        validate_changed_paths_in_root(&request.aggregate_worktree, &final_changed_paths)?;
        validate_changed_tree_paths(
            &self.repo_root,
            expected_aggregate_sha,
            &requested_source,
            contribution_sha,
            &final_changed_paths,
        )?;

        let aggregate_now = match aggregate_sha {
            Some(aggregate_now) => aggregate_now,
            None => {
                return Err(IntegrationError::MissingAggregateRef {
                    contribution_ref: refs.contribution.clone(),
                });
            }
        };
        if aggregate_now == expected_aggregate_sha {
            let zero = repository_zero_oid(&self.repo_root)?;
            let mut updates = vec![
                RefUpdate {
                    name: &refs.aggregate,
                    new: contribution_sha,
                    old: expected_aggregate_sha,
                },
                RefUpdate {
                    name: &refs.contribution,
                    new: contribution_sha,
                    old: contribution_sha,
                },
            ];
            if let Some(child_marker) = child_marker.as_deref() {
                updates.push(RefUpdate {
                    name: &refs.child,
                    new: &zero,
                    old: child_marker,
                });
            }
            update_refs_atomic(&self.repo_root, &updates).map_err(|error| {
                let actual = read_ref(&self.repo_root, &refs.aggregate)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| expected_aggregate_sha.to_string());
                if actual != expected_aggregate_sha {
                    IntegrationError::AggregateMoved {
                        expected: expected_aggregate_sha.to_string(),
                        actual,
                    }
                } else {
                    error
                }
            })?;
        } else if aggregate_now != contribution_sha
            && !is_ancestor(&self.repo_root, contribution_sha, &aggregate_now)?
        {
            return Err(IntegrationError::AggregateMoved {
                expected: expected_aggregate_sha.to_string(),
                actual: aggregate_now,
            });
        } else {
            let zero = repository_zero_oid(&self.repo_root)?;
            let mut updates = vec![
                RefUpdate {
                    name: &refs.aggregate,
                    new: &aggregate_now,
                    old: &aggregate_now,
                },
                RefUpdate {
                    name: &refs.contribution,
                    new: contribution_sha,
                    old: contribution_sha,
                },
            ];
            if let Some(child_marker) = child_marker.as_deref() {
                updates.push(RefUpdate {
                    name: &refs.child,
                    new: &zero,
                    old: child_marker,
                });
            }
            update_refs_atomic(&self.repo_root, &updates)?;
        }

        let receipt = make_receipt(
            request,
            refs,
            &requested_base,
            &requested_source,
            expected_aggregate_sha,
            contribution_sha,
            final_changed_paths,
        );
        if receipt_path.exists() {
            let existing = load_receipt(receipt_path)?;
            validate_receipt_matches(&existing, &receipt)?;
            Ok(IntegrationResult {
                receipt: existing,
                replayed: true,
            })
        } else {
            persist_receipt(receipt_path, &receipt)?;
            Ok(IntegrationResult {
                receipt,
                replayed: true,
            })
        }
    }

    fn classify_install_failure(
        &self,
        request: &IntegrationRequest,
        refs: &OwnedRefs,
        receipt_path: &Path,
        expected_aggregate_sha: &str,
        error: IntegrationError,
    ) -> Result<IntegrationResult, IntegrationError> {
        let contribution = read_ref(&self.repo_root, &refs.contribution)?;
        let aggregate = read_ref(&self.repo_root, &refs.aggregate)?;
        if let Some(contribution) = contribution {
            return self.recover_existing(
                request,
                refs,
                receipt_path,
                expected_aggregate_sha,
                aggregate,
                &contribution,
            );
        }
        if let Some(actual) = aggregate {
            if actual != expected_aggregate_sha {
                return Err(IntegrationError::AggregateMoved {
                    expected: expected_aggregate_sha.to_string(),
                    actual,
                });
            }
        }
        Err(error)
    }
}

#[derive(Debug)]
struct RefUpdate<'a> {
    name: &'a str,
    new: &'a str,
    old: &'a str,
}

#[derive(Debug)]
struct IntegrationMetadata {
    workflow_id: String,
    contribution_id: String,
    source_sha: String,
    base_sha: String,
    previous_aggregate_sha: String,
}

fn make_receipt(
    request: &IntegrationRequest,
    refs: &OwnedRefs,
    base_sha: &str,
    source_sha: &str,
    previous_aggregate_sha: &str,
    result_sha: &str,
    changed_paths: Vec<PathBuf>,
) -> IntegrationReceipt {
    let mut paths = changed_paths
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    IntegrationReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        workflow_id: request.workflow_id.clone(),
        contribution_id: request.contribution_id.clone(),
        source_sha: source_sha.to_string(),
        base_sha: base_sha.to_string(),
        previous_aggregate_sha: previous_aggregate_sha.to_string(),
        result_sha: result_sha.to_string(),
        changed_paths: paths,
        aggregate_ref: refs.aggregate.clone(),
        contribution_ref: refs.contribution.clone(),
        status: IntegrationStatus::Integrated,
    }
}

fn validate_receipt_matches(
    actual: &IntegrationReceipt,
    expected: &IntegrationReceipt,
) -> Result<(), IntegrationError> {
    if actual != expected {
        return Err(IntegrationError::Receipt(
            "existing receipt does not match immutable Git metadata".to_string(),
        ));
    }
    Ok(())
}

fn persist_receipt(path: &Path, receipt: &IntegrationReceipt) -> Result<(), IntegrationError> {
    let bytes =
        serde_json::to_vec_pretty(receipt).map_err(|error| IntegrationError::ReceiptWrite {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    if path.exists() {
        let existing = load_receipt(path)?;
        validate_receipt_matches(&existing, receipt)?;
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| IntegrationError::ReceiptWrite {
            path: path.to_path_buf(),
            reason: "receipt path has no parent directory".to_string(),
        })?;
    fs::create_dir_all(parent).map_err(|error| IntegrationError::ReceiptWrite {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("receipt"),
        std::process::id(),
        nonce
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|error| IntegrationError::ReceiptWrite {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| IntegrationError::ReceiptWrite {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    match fs::hard_link(&tmp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&tmp);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&tmp);
            let existing = load_receipt(path)?;
            validate_receipt_matches(&existing, receipt)
        }
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(IntegrationError::ReceiptWrite {
                path: path.to_path_buf(),
                reason: error.to_string(),
            })
        }
    }
}

fn load_receipt(path: &Path) -> Result<IntegrationReceipt, IntegrationError> {
    let bytes = fs::read(path)
        .map_err(|error| IntegrationError::Receipt(format!("read {}: {error}", path.display())))?;
    let receipt: IntegrationReceipt = serde_json::from_slice(&bytes)
        .map_err(|error| IntegrationError::Receipt(format!("parse {}: {error}", path.display())))?;
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION {
        return Err(IntegrationError::Receipt(format!(
            "unsupported schema version {}",
            receipt.schema_version
        )));
    }
    Ok(receipt)
}

fn read_integration_metadata(
    repo_root: &Path,
    result_sha: &str,
) -> Result<IntegrationMetadata, IntegrationError> {
    let body = run_git(
        repo_root,
        vec![
            OsString::from("show"),
            OsString::from("-s"),
            OsString::from("--format=%B"),
            OsString::from(result_sha),
        ],
    )?;
    let text = String::from_utf8_lossy(&body.stdout);
    let value = |key: &str| -> Result<String, IntegrationError> {
        let values = text
            .lines()
            .filter_map(|line| line.strip_prefix(key).map(str::trim))
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        match values.as_slice() {
            [value] => Ok((*value).to_owned()),
            [] => Err(IntegrationError::Receipt(format!("missing {key} trailer"))),
            _ => Err(IntegrationError::Receipt(format!(
                "duplicate {key} trailer"
            ))),
        }
    };
    Ok(IntegrationMetadata {
        workflow_id: value("Maestro-Workflow:")?,
        contribution_id: value("Maestro-Contribution:")?,
        source_sha: value("Maestro-Source:")?,
        base_sha: value("Maestro-Base:")?,
        previous_aggregate_sha: value("Maestro-Previous-Aggregate:")?,
    })
}

fn commit_parents(repo_root: &Path, revision: &str) -> Result<Vec<String>, IntegrationError> {
    let output = run_git(
        repo_root,
        vec![
            OsString::from("rev-list"),
            OsString::from("--parents"),
            OsString::from("-n"),
            OsString::from("1"),
            OsString::from(revision),
        ],
    )?;
    let output_text = String::from_utf8_lossy(&output.stdout);
    let mut fields = output_text.split_whitespace();
    let _commit = fields.next();
    Ok(fields.map(ToOwned::to_owned).collect())
}

fn resolve_tree(repo_root: &Path, revision: &str) -> Result<String, IntegrationError> {
    let tree = format!("{revision}^{{tree}}");
    let output = run_git(
        repo_root,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--end-of-options"),
            OsString::from(tree),
        ],
    )?;
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if tree.is_empty() {
        return Err(IntegrationError::Receipt(format!(
            "Git returned an empty tree for result {revision}"
        )));
    }
    Ok(tree)
}

/// Reproduce the merge tree without relying on `git merge-tree --write-tree`.
///
/// The latter was introduced in Git 2.38, while the hosted Linux image may
/// provide an older system Git.  A detached scratch worktree gives older Git
/// versions the same merge semantics as `IntegrationCoordinator::build_candidate`, without
/// touching the caller's checkout or index.  The scratch worktree and its
/// private hooks directory are cleaned up on success and cleanup is attempted
/// on error by [`TemporaryWorktree`]; integration accepts the result only
/// after cleanup succeeds.
fn merge_tree_in_scratch_worktree(
    repo_root: &Path,
    aggregate_sha: &str,
    source_sha: &str,
) -> Result<String, IntegrationError> {
    let key = format!(
        "{:016x}",
        stable_hash(format!("{aggregate_sha}\0{source_sha}").as_bytes())
    );
    let path = temporary_worktree_path("merge-tree", &key)?;
    let hooks_path = temporary_hooks_path("merge-tree", &key);
    fs::create_dir(&hooks_path).map_err(|error| {
        IntegrationError::Worktree(format!(
            "create isolated hooks directory {}: {error}",
            hooks_path.display()
        ))
    })?;

    let add = match run_git_allow_failure_with_hooks(
        repo_root,
        vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("--detach"),
            path.clone().into_os_string(),
            OsString::from(aggregate_sha),
        ],
        &hooks_path,
    ) {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_dir_all(&hooks_path);
            return Err(error);
        }
    };
    if !add.status.success() {
        let _ = fs::remove_dir_all(&hooks_path);
        return Err(git_failure("git worktree add", &add));
    }
    let mut temporary = TemporaryWorktree::new(repo_root.to_path_buf(), path, hooks_path);

    let merge = run_git_allow_failure_with_hooks(
        temporary.path(),
        vec![
            OsString::from("merge"),
            OsString::from("--no-commit"),
            OsString::from("--no-ff"),
            OsString::from("--no-edit"),
            OsString::from(source_sha),
        ],
        temporary.hooks_path(),
    )?;
    if !merge.status.success() {
        let conflicts = unmerged_paths(temporary.path())?;
        let _ = run_git_allow_failure_with_hooks(
            temporary.path(),
            vec![OsString::from("merge"), OsString::from("--abort")],
            temporary.hooks_path(),
        );
        temporary.remove_best_effort();
        if !conflicts.is_empty() {
            return Err(IntegrationError::TextualConflict { paths: conflicts });
        }
        return Err(git_failure("git merge", &merge));
    }

    let tree = run_git_allow_failure_with_hooks(
        temporary.path(),
        vec![OsString::from("write-tree")],
        temporary.hooks_path(),
    )?;
    if !tree.status.success() {
        temporary.remove_best_effort();
        return Err(git_failure("git write-tree", &tree));
    }
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_owned();
    if tree.is_empty() {
        temporary.remove_best_effort();
        return Err(IntegrationError::Receipt(format!(
            "merge of aggregate {aggregate_sha} and source {source_sha} returned no tree"
        )));
    }
    temporary.remove()?;
    Ok(tree)
}

fn validate_changed_tree_paths(
    repo_root: &Path,
    aggregate_sha: &str,
    source_sha: &str,
    result_sha: &str,
    changed_paths: &[PathBuf],
) -> Result<(), IntegrationError> {
    for path in changed_paths {
        validate_tree_path(repo_root, aggregate_sha, path)?;
        validate_tree_path(repo_root, source_sha, path)?;
        validate_tree_path(repo_root, result_sha, path)?;
    }
    Ok(())
}

fn validate_tree_path(
    repo_root: &Path,
    revision: &str,
    path: &Path,
) -> Result<(), IntegrationError> {
    validate_relative_path(path)?;
    let mut prefix = PathBuf::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        prefix.push(name);
        let output = run_git_allow_failure(
            repo_root,
            vec![
                OsString::from("--literal-pathspecs"),
                OsString::from("ls-tree"),
                OsString::from("-z"),
                OsString::from("--full-tree"),
                OsString::from(revision),
                OsString::from("--"),
                prefix.clone().into_os_string(),
            ],
        )?;
        if !output.status.success() {
            return Err(git_failure("git ls-tree", &output));
        }
        let Some(entry) = output.stdout.split(|byte| *byte == 0).find(|entry| {
            !entry.is_empty()
                && entry
                    .splitn(2, |byte| *byte == b'\t')
                    .nth(1)
                    .is_some_and(|entry_path| entry_path == prefix.as_os_str().as_encoded_bytes())
        }) else {
            continue;
        };
        let mode = entry.split(|byte| *byte == b' ').next().unwrap_or_default();
        if mode == b"120000" {
            return Err(IntegrationError::SymlinkPath {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn changed_paths_between(
    repo_root: &Path,
    from: &str,
    to: &str,
) -> Result<Vec<PathBuf>, IntegrationError> {
    let output = run_git(
        repo_root,
        vec![
            OsString::from("diff"),
            OsString::from("--no-ext-diff"),
            OsString::from("--no-textconv"),
            OsString::from("--name-only"),
            OsString::from("-z"),
            OsString::from("--no-renames"),
            OsString::from(from),
            OsString::from(to),
            OsString::from("--"),
        ],
    )?;
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let text = std::str::from_utf8(path).map_err(|_| {
                IntegrationError::InvalidRequest(
                    "changed path is not valid UTF-8 and cannot be represented in a receipt"
                        .to_string(),
                )
            })?;
            let path = PathBuf::from(text);
            validate_relative_path(&path)?;
            Ok(path)
        })
        .collect()
}

fn unmerged_paths(path: &Path) -> Result<Vec<String>, IntegrationError> {
    let output = run_git(
        path,
        vec![
            OsString::from("diff"),
            OsString::from("--no-ext-diff"),
            OsString::from("--no-textconv"),
            OsString::from("--name-only"),
            OsString::from("-z"),
            OsString::from("--diff-filter=U"),
            OsString::from("--"),
        ],
    )?;
    let mut paths = Vec::new();
    for bytes in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|p| !p.is_empty())
    {
        let path = std::str::from_utf8(bytes).map_err(|_| {
            IntegrationError::Receipt("conflict path is not valid UTF-8".to_string())
        })?;
        paths.push(path.to_string());
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn validate_declared_scope(
    declared_scope: bool,
    declared: &[PathBuf],
    changed: &[PathBuf],
) -> Result<(), IntegrationError> {
    if !declared_scope {
        return Ok(());
    }
    if declared.is_empty() {
        return Err(IntegrationError::InvalidRequest(
            "write scope denies all changed paths".to_string(),
        ));
    }
    let declared = declared
        .iter()
        .map(|path| {
            validate_relative_path(path)?;
            Ok(normalize_scope_path(path))
        })
        .collect::<Result<BTreeSet<_>, IntegrationError>>()?;
    if let Some(path) = changed.iter().find(|path| {
        !declared.iter().any(|scope| {
            scope.as_os_str().is_empty()
                || path.as_path() == scope.as_path()
                || path.starts_with(scope)
        })
    }) {
        return Err(IntegrationError::InvalidRequest(format!(
            "changed path `{}` is outside the declared write scope",
            path.display()
        )));
    }
    Ok(())
}

fn normalize_scope_path(path: &Path) -> PathBuf {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            Component::CurDir => None,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect()
}

fn validate_changed_paths(
    source_root: &Path,
    aggregate_root: &Path,
    changed: &[PathBuf],
) -> Result<(), IntegrationError> {
    validate_changed_paths_in_root(source_root, changed)?;
    validate_changed_paths_in_root(aggregate_root, changed)?;
    Ok(())
}

fn validate_changed_paths_in_root(
    root: &Path,
    changed: &[PathBuf],
) -> Result<(), IntegrationError> {
    for path in changed {
        validate_relative_path(path)?;
        validate_path_components(root, path)?;
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), IntegrationError> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || path.components().next().is_none()
    {
        return Err(IntegrationError::PathEscape {
            path: path.to_path_buf(),
        });
    }
    if path.to_string_lossy().contains('\0') {
        return Err(IntegrationError::PathEscape {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_path_components(root: &Path, relative: &Path) -> Result<(), IntegrationError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            if metadata.file_type().is_symlink() {
                return Err(IntegrationError::SymlinkPath {
                    path: relative.to_path_buf(),
                });
            }
        }
    }
    Ok(())
}

fn validate_worktree_path(path: &Path) -> Result<(), IntegrationError> {
    let metadata = fs::metadata(path).map_err(|error| IntegrationError::PathEscape {
        path: PathBuf::from(format!("{} ({error})", path.display())),
    })?;
    if !metadata.is_dir() {
        return Err(IntegrationError::PathEscape {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn ensure_clean(path: &Path) -> Result<(), IntegrationError> {
    let output = run_git(
        path,
        vec![
            OsString::from("status"),
            OsString::from("--porcelain=v1"),
            OsString::from("--untracked-files=all"),
        ],
    )?;
    if output.stdout.is_empty() {
        return Ok(());
    }
    Err(IntegrationError::DirtyWorktree {
        path: path.to_path_buf(),
        status: String::from_utf8_lossy(&output.stdout).trim().to_string(),
    })
}

fn ensure_same_repository(repo_root: &Path, other: &Path) -> Result<(), IntegrationError> {
    let expected = common_git_dir(repo_root)?;
    let actual = common_git_dir(other)?;
    if expected != actual {
        return Err(IntegrationError::InvalidRequest(format!(
            "{} belongs to a different Git repository than {}",
            other.display(),
            repo_root.display()
        )));
    }
    Ok(())
}

fn repository_root(path: &Path) -> Result<PathBuf, IntegrationError> {
    let output = run_git_allow_failure(
        path,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--show-toplevel"),
        ],
    )?;
    if !output.status.success() {
        return Err(git_failure("git rev-parse --show-toplevel", &output));
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return Err(IntegrationError::InvalidRequest(format!(
            "git returned an empty repository root for {}",
            path.display()
        )));
    }
    dunce::canonicalize(root).map_err(|error| {
        IntegrationError::InvalidRequest(format!(
            "canonicalize repository root {}: {error}",
            path.display()
        ))
    })
}

fn common_git_dir(path: &Path) -> Result<PathBuf, IntegrationError> {
    let output = run_git_allow_failure(
        path,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--git-common-dir"),
        ],
    )?;
    if !output.status.success() {
        return Err(git_failure("git rev-parse --git-common-dir", &output));
    }
    let raw = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let absolute = if raw.is_absolute() {
        raw
    } else {
        path.join(raw)
    };
    dunce::canonicalize(&absolute).map_err(|error| {
        IntegrationError::InvalidRequest(format!(
            "canonicalize Git common directory {}: {error}",
            absolute.display()
        ))
    })
}

fn absolute_path(path: &Path) -> Result<PathBuf, IntegrationError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| {
            IntegrationError::InvalidRequest(format!("resolve path {}: {error}", path.display()))
        })
}

fn resolve_commit(repo: &Path, revision: &str) -> Result<String, IntegrationError> {
    if revision.trim().is_empty() || revision.contains(['\n', '\r', '\0']) {
        return Err(IntegrationError::InvalidRequest(format!(
            "invalid revision `{revision}`"
        )));
    }
    let revspec = format!("{revision}^{{commit}}");
    let output = run_git_allow_failure(
        repo,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--end-of-options"),
            OsString::from(revspec),
        ],
    )?;
    if !output.status.success() {
        return Err(IntegrationError::InvalidRequest(format!(
            "cannot resolve revision `{revision}`: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if resolved.is_empty() {
        return Err(IntegrationError::InvalidRequest(format!(
            "Git returned an empty revision for `{revision}`"
        )));
    }
    Ok(resolved)
}

fn read_ref(repo: &Path, reference: &str) -> Result<Option<String>, IntegrationError> {
    reject_symbolic_ref(repo, reference)?;
    let output = run_git_allow_failure(
        repo,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--quiet"),
            OsString::from("--end-of-options"),
            OsString::from(reference),
        ],
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn reject_symbolic_ref(repo: &Path, reference: &str) -> Result<(), IntegrationError> {
    let output = run_git_allow_failure(
        repo,
        vec![
            OsString::from("symbolic-ref"),
            OsString::from("--quiet"),
            OsString::from("--"),
            OsString::from(reference),
        ],
    )?;
    if output.status.success() {
        let target = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(IntegrationError::OwnedRefSymbolic {
            reference: reference.to_string(),
            target,
        });
    }
    if output.status.code() == Some(1) {
        Ok(())
    } else {
        Err(git_failure("git symbolic-ref --quiet", &output))
    }
}

fn repository_zero_oid(repo: &Path) -> Result<String, IntegrationError> {
    let output = run_git_allow_failure(
        repo,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--show-object-format"),
        ],
    )?;
    if !output.status.success() {
        return Ok(ZERO_OID_SHA1.to_string());
    }
    let format = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if format == "sha256" {
        ZERO_OID_SHA256.to_string()
    } else {
        ZERO_OID_SHA1.to_string()
    })
}

fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool, IntegrationError> {
    let output = run_git_allow_failure(
        repo,
        vec![
            OsString::from("merge-base"),
            OsString::from("--is-ancestor"),
            OsString::from(ancestor),
            OsString::from(descendant),
        ],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure("git merge-base --is-ancestor", &output)),
    }
}

fn is_first_parent_ancestor(
    repo: &Path,
    ancestor: &str,
    descendant: &str,
) -> Result<bool, IntegrationError> {
    let output = run_git(
        repo,
        vec![
            OsString::from("rev-list"),
            OsString::from("--first-parent"),
            OsString::from(descendant),
        ],
    )?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|revision| revision.trim() == ancestor))
}

fn update_refs_atomic(repo: &Path, updates: &[RefUpdate<'_>]) -> Result<(), IntegrationError> {
    for update in updates {
        reject_symbolic_ref(repo, update.name)?;
    }
    let mut input = String::from("start\n");
    for update in updates {
        input.push_str("update ");
        input.push_str(update.name);
        input.push(' ');
        input.push_str(update.new);
        input.push(' ');
        input.push_str(update.old);
        input.push('\n');
    }
    input.push_str("prepare\ncommit\n");
    let mut command = Command::new("git");
    command
        .args(["update-ref", "--no-deref", "--stdin"])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child =
        spawn_owned_command(&mut command).map_err(|error| IntegrationError::GitCommand {
            command: "git update-ref --no-deref --stdin".to_string(),
            status: None,
            stderr: error.to_string(),
        })?;
    let output =
        wait_for_child_with_input(child, "git update-ref --no-deref --stdin", input.as_bytes())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure("git update-ref --no-deref --stdin", &output))
    }
}

fn run_git(repo: &Path, args: Vec<OsString>) -> Result<Output, IntegrationError> {
    let output = run_git_allow_failure(repo, args.clone())?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(git_failure(&render_command(&args), &output))
    }
}

fn run_git_allow_failure(repo: &Path, args: Vec<OsString>) -> Result<Output, IntegrationError> {
    let command_text = render_command(&args);
    let mut command = Command::new("git");
    command
        .args(&args)
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child =
        spawn_owned_command(&mut command).map_err(|error| IntegrationError::GitCommand {
            command: command_text.clone(),
            status: None,
            stderr: error.to_string(),
        })?;
    wait_for_child(child, &command_text)
}

fn run_git_allow_failure_with_hooks(
    repo: &Path,
    args: Vec<OsString>,
    hooks_path: &Path,
) -> Result<Output, IntegrationError> {
    let mut configured = vec![
        OsString::from("-c"),
        OsString::from(format!("core.hooksPath={}", hooks_path.display())),
    ];
    configured.extend(args);
    run_git_allow_failure(repo, configured)
}

fn wait_for_child(child: Child, command: &str) -> Result<Output, IntegrationError> {
    wait_for_child_with_input_inner(child, command, None)
}

fn wait_for_child_with_input(
    child: Child,
    command: &str,
    input: &[u8],
) -> Result<Output, IntegrationError> {
    wait_for_child_with_input_inner(child, command, Some(input))
}

fn wait_for_child_with_input_inner(
    mut child: Child,
    command: &str,
    input: Option<&[u8]>,
) -> Result<Output, IntegrationError> {
    let stdout = child.stdout.take().map(drain_pipe);
    let stderr = child.stderr.take().map(drain_pipe);
    let stdin_result = if let Some(input) = input {
        let (sender, receiver) = mpsc::channel();
        match child.stdin.take() {
            Some(mut stdin) => {
                let input = input.to_vec();
                thread::spawn(move || {
                    let result = stdin.write_all(&input);
                    drop(stdin);
                    let _ = sender.send(result);
                });
            }
            None => {
                let _ = sender.send(Err(io::Error::other("Git did not expose stdin")));
            }
        }
        Some(receiver)
    } else {
        drop(child.stdin.take());
        None
    };

    let deadline = Instant::now() + GIT_PROCESS_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                kill_owned_process(&mut child);
                break child.wait().map_err(|error| IntegrationError::GitCommand {
                    command: command.to_string(),
                    status: None,
                    stderr: format!("timed out and could not reap child: {error}"),
                })?;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                kill_owned_process(&mut child);
                let _ = child.wait();
                let _ = join_pipe(stdout, "stdout", command);
                let _ = join_pipe(stderr, "stderr", command);
                if let Some(receiver) = stdin_result {
                    let _ = receiver.recv_timeout(PIPE_DRAIN_TIMEOUT);
                }
                return Err(IntegrationError::GitCommand {
                    command: command.to_string(),
                    status: None,
                    stderr: format!("could not wait for child: {error}"),
                });
            }
        }
    };

    let stdout = match join_pipe(stdout, "stdout", command) {
        Ok(stdout) => stdout,
        Err(error) => {
            kill_owned_process(&mut child);
            let _ = child.wait();
            return Err(error);
        }
    };
    let stderr = match join_pipe(stderr, "stderr", command) {
        Ok(stderr) => stderr,
        Err(error) => {
            kill_owned_process(&mut child);
            let _ = child.wait();
            return Err(error);
        }
    };
    let stderr_text = String::from_utf8_lossy(&stderr).trim().to_string();
    if timed_out {
        if let Some(receiver) = stdin_result {
            let _ = receiver.recv_timeout(PIPE_DRAIN_TIMEOUT);
        }
        return Err(IntegrationError::GitCommand {
            command: command.to_string(),
            status: status.code(),
            stderr: if stderr_text.is_empty() {
                format!(
                    "process exceeded {}s deadline",
                    GIT_PROCESS_TIMEOUT.as_secs()
                )
            } else {
                format!(
                    "process exceeded {}s deadline: {stderr_text}",
                    GIT_PROCESS_TIMEOUT.as_secs()
                )
            },
        });
    }
    if let Some(receiver) = stdin_result {
        match receiver.recv_timeout(PIPE_DRAIN_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                kill_owned_process(&mut child);
                let _ = child.wait();
                return Err(IntegrationError::GitCommand {
                    command: command.to_string(),
                    status: status.code(),
                    stderr: error.to_string(),
                });
            }
            Err(error) => {
                kill_owned_process(&mut child);
                let _ = child.wait();
                return Err(IntegrationError::GitCommand {
                    command: command.to_string(),
                    status: status.code(),
                    stderr: format!("could not finish writing Git stdin: {error}"),
                });
            }
        }
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn join_pipe(
    pipe: Option<mpsc::Receiver<(Vec<u8>, io::Result<usize>)>>,
    name: &str,
    command: &str,
) -> Result<Vec<u8>, IntegrationError> {
    let Some(pipe) = pipe else {
        return Ok(Vec::new());
    };
    let (bytes, result) =
        pipe.recv_timeout(PIPE_DRAIN_TIMEOUT)
            .map_err(|error| IntegrationError::GitCommand {
                command: command.to_string(),
                status: None,
                stderr: format!("could not drain {name} within deadline: {error}"),
            })?;
    result.map_err(|error| IntegrationError::GitCommand {
        command: command.to_string(),
        status: None,
        stderr: format!("could not drain {name}: {error}"),
    })?;
    Ok(bytes)
}

fn drain_pipe<R>(mut pipe: R) -> mpsc::Receiver<(Vec<u8>, io::Result<usize>)>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.read_to_end(&mut bytes);
        let _ = sender.send((bytes, result));
    });
    receiver
}

fn spawn_owned_command(command: &mut Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // Keep Git and hooks/drivers in a private process group so a timed
        // out integration cannot leave a helper holding our output pipes.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    command.spawn()
}

fn kill_owned_process(child: &mut Child) {
    #[cfg(unix)]
    {
        let process_group = -(child.id() as libc::pid_t);
        // The direct child is reaped below even when process-group killing is
        // unavailable or races with process exit.
        unsafe {
            let _ = libc::kill(process_group, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn git_failure(command: &str, output: &Output) -> IntegrationError {
    IntegrationError::GitCommand {
        command: command.to_string(),
        status: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

fn render_command(args: &[OsString]) -> String {
    std::iter::once("git".to_string())
        .chain(args.iter().map(|arg| arg.to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_identifier(value: &str, name: &str) -> Result<(), IntegrationError> {
    if value.is_empty() || value.len() > 96 || value.contains(['\n', '\r', '\0']) {
        return Err(IntegrationError::InvalidRequest(format!(
            "{name} must be 1-96 bytes and contain no line breaks"
        )));
    }
    Ok(())
}

fn validate_object_id(value: &str, name: &str) -> Result<(), IntegrationError> {
    let valid_length = matches!(value.len(), 40 | 64);
    if !valid_length || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(IntegrationError::InvalidRequest(format!(
            "{name} must be a full immutable Git object ID"
        )));
    }
    Ok(())
}

fn encode_component(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn branch_component(value: &str) -> String {
    let encoded = encode_component(value);
    if encoded.len() <= 48 {
        encoded
    } else {
        format!("{}-{:016x}", &encoded[..32], stable_hash(value.as_bytes()))
    }
}

fn stable_hash(value: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn temporary_worktree_path(
    workflow_id: &str,
    contribution_id: &str,
) -> Result<PathBuf, IntegrationError> {
    let mut path = std::env::temp_dir().join(format!(
        "maestro-integration-{}-{}-{}-{}",
        branch_component(workflow_id),
        branch_component(contribution_id),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    if path.exists() {
        path.push(format!(
            "retry-{}",
            stable_hash(path.to_string_lossy().as_bytes())
        ));
    }
    Ok(path)
}

fn temporary_hooks_path(workflow_id: &str, contribution_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "maestro-integration-hooks-{}-{}-{}-{}",
        branch_component(workflow_id),
        branch_component(contribution_id),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ))
}

struct TemporaryWorktree {
    repo_root: PathBuf,
    path: PathBuf,
    hooks_path: PathBuf,
    removed: bool,
}

impl TemporaryWorktree {
    fn new(repo_root: PathBuf, path: PathBuf, hooks_path: PathBuf) -> Self {
        Self {
            repo_root,
            path,
            hooks_path,
            removed: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn hooks_path(&self) -> &Path {
        &self.hooks_path
    }

    fn remove(&mut self) -> Result<(), IntegrationError> {
        if self.removed {
            return Ok(());
        }
        let output = run_git_allow_failure_with_hooks(
            &self.repo_root,
            vec![
                OsString::from("worktree"),
                OsString::from("remove"),
                OsString::from("--force"),
                self.path.clone().into_os_string(),
            ],
            &self.hooks_path,
        )?;
        if !output.status.success() {
            return Err(IntegrationError::Worktree(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        self.removed = true;
        let _ = fs::remove_dir_all(&self.hooks_path);
        Ok(())
    }

    fn remove_best_effort(&mut self) {
        let _ = self.remove();
    }
}

impl Drop for TemporaryWorktree {
    fn drop(&mut self) {
        self.remove_best_effort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    struct RepoFixture {
        _root: TempDir,
        repo: PathBuf,
        aggregate: PathBuf,
        receipts: PathBuf,
    }

    impl RepoFixture {
        fn new(name: &str) -> Self {
            let root = tempfile::Builder::new()
                .prefix(&format!("maestro-integration-{name}-"))
                .tempdir()
                .expect("temporary root should be created");
            let repo = root.path().join("repo");
            fs::create_dir_all(&repo).expect("repo should be created");
            git(&repo, &["init", "--quiet"]);
            git(&repo, &["config", "user.email", "test@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            fs::write(repo.join("README.md"), "base\n").expect("base should be written");
            git(&repo, &["add", "README.md"]);
            git(&repo, &["commit", "--quiet", "-m", "base"]);
            let aggregate = root.path().join("aggregate");
            git(
                &repo,
                &["worktree", "add", "--quiet", &aggregate.to_string_lossy()],
            );
            let receipts = root.path().join("receipts");
            Self {
                _root: root,
                repo,
                aggregate,
                receipts,
            }
        }

        fn coordinator(&self) -> IntegrationCoordinator {
            IntegrationCoordinator::new(&self.repo, &self.receipts).expect("coordinator")
        }

        fn head(&self) -> String {
            rev(&self.repo, "HEAD")
        }

        fn child(&self, workflow: &str, contribution: &str) -> WorktreeSession {
            self.coordinator()
                .create_child_worktree(workflow, contribution, &self.head())
                .expect("child worktree")
        }
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn rev(repo: &Path, revision: &str) -> String {
        let output = Command::new("git")
            .args(["rev-parse", revision])
            .current_dir(repo)
            .output()
            .expect("git should run");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit_child(child: &WorktreeSession, file: &str, contents: &str, message: &str) {
        let path = child.path().join(file);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("child file parent should be created");
        }
        fs::write(path, contents).expect("child file should be written");
        git(child.path(), &["add", file]);
        git(child.path(), &["commit", "--quiet", "-m", message]);
    }

    #[test]
    fn child_worktree_is_pinned_to_requested_base() {
        let fixture = RepoFixture::new("pinned");
        let base = fixture.head();
        let child = fixture
            .coordinator()
            .create_child_worktree("workflow", "child", &base)
            .expect("child should be created");
        assert_eq!(child.initial_head(), base);
        assert_eq!(rev(child.path(), "HEAD"), base);
        child.abort();
    }

    #[test]
    fn owned_child_worktree_reopens_and_preserves_dirty_resume_state() {
        let fixture = RepoFixture::new("child-reuse");
        let base = fixture.head();
        let coordinator = fixture.coordinator();
        let first = coordinator
            .create_child_worktree("workflow", "child", &base)
            .expect("child should be created");
        let path = first.path().to_path_buf();
        fs::write(path.join("resume.txt"), "unfinished\n").expect("resume state should be written");
        let resumed = coordinator.create_child_worktree("workflow", "child", &base);
        assert!(matches!(
            &resumed,
            Err(IntegrationError::DirtyWorktree { path: dirty, .. }) if *dirty == path
        ));
        assert!(path.join("resume.txt").is_file());
        fs::remove_file(path.join("resume.txt")).expect("resume state should be removed");
        first.abort();

        let reopened_after_cleanup = coordinator
            .create_child_worktree("workflow", "child", &base)
            .expect("owned marker should permit a clean recreation");
        assert_eq!(reopened_after_cleanup.initial_head(), base);
        assert_eq!(reopened_after_cleanup.path(), path);
        reopened_after_cleanup.abort();
    }

    #[test]
    fn owned_child_recovery_reattaches_missing_path_without_deleting_branch() {
        let fixture = RepoFixture::new("child-reattach");
        let base = fixture.head();
        let coordinator = fixture.coordinator();
        let child = coordinator
            .create_child_worktree("workflow", "child", &base)
            .expect("child worktree");
        let path = child.path().to_path_buf();
        let path_arg = path.to_string_lossy().into_owned();
        git(&fixture.repo, &["worktree", "remove", "--force", &path_arg]);
        assert!(!path.exists());

        let resumed = coordinator
            .create_child_worktree("workflow", "child", &base)
            .expect("owned child should be reattached");
        assert_eq!(resumed.initial_head(), base);
        assert_eq!(rev(resumed.path(), "HEAD"), base);
        resumed.abort();
    }

    #[test]
    fn integrates_with_owned_refs_and_immutable_receipt() {
        let fixture = RepoFixture::new("integrate");
        let base = fixture.head();
        let aggregate_head_before = rev(&fixture.aggregate, "HEAD");
        let aggregate_status_before = git_stdout(
            &fixture.aggregate,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        );
        let child = fixture.child("workflow", "child");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let result = coordinator
            .integrate(IntegrationRequest::new(
                "workflow",
                "child",
                child.path(),
                &source,
                &base,
                &fixture.aggregate,
                &base,
            ))
            .expect("integration should succeed");
        assert!(!result.replayed);
        assert_eq!(result.receipt.source_sha, source);
        assert_eq!(result.receipt.base_sha, base);
        assert_ne!(result.receipt.result_sha, base);
        assert!(
            fixture
                .receipts
                .join(encode_component("workflow"))
                .join(format!("{}.json", encode_component("child")))
                .is_file()
        );
        assert_eq!(
            rev(
                &fixture.repo,
                &coordinator.aggregate_ref("workflow").expect("ref")
            ),
            result.receipt.result_sha
        );
        assert_eq!(
            rev(&fixture.aggregate, "HEAD"),
            aggregate_head_before,
            "user checkout is untouched"
        );
        assert_eq!(
            git_stdout(
                &fixture.aggregate,
                &["status", "--porcelain=v1", "--untracked-files=all"]
            ),
            aggregate_status_before,
            "user checkout index and worktree are untouched"
        );
        assert!(
            read_ref(
                &fixture.repo,
                &coordinator
                    .child_ref("workflow", "child")
                    .expect("child ref")
            )
            .expect("child ref read")
            .is_none(),
            "successful integration should release the child ownership marker"
        );
        child.abort();
    }

    #[test]
    fn replay_is_idempotent_and_duplicate_source_is_explicit() {
        let fixture = RepoFixture::new("replay");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let request = IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &base,
        );
        let first = coordinator
            .integrate(request.clone())
            .expect("first integration");
        let second = coordinator.integrate(request).expect("replay integration");
        assert!(second.replayed);
        assert_eq!(first.receipt, second.receipt);
        let other = coordinator.integrate(IntegrationRequest::new(
            "workflow",
            "other",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &first.receipt.result_sha,
        ));
        assert!(
            matches!(&other, Err(IntegrationError::DuplicateCommit { .. })),
            "unexpected result: {other:?}"
        );
        child.abort();
    }

    #[test]
    fn replay_survives_child_worktree_cleanup() {
        let fixture = RepoFixture::new("replay-after-child-cleanup");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let child_path = child.path().to_path_buf();
        let coordinator = fixture.coordinator();
        let request = IntegrationRequest::new(
            "workflow",
            "child",
            &child_path,
            &source,
            &base,
            &fixture.aggregate,
            &base,
        );
        let first = coordinator
            .integrate(request.clone())
            .expect("first integration");
        child.abort();
        assert!(
            !child_path.exists(),
            "the child worktree should be cleaned up"
        );

        let replay = coordinator
            .integrate(request)
            .expect("replay should use owned Git refs after cleanup");
        assert!(replay.replayed);
        assert_eq!(replay.receipt, first.receipt);
    }

    #[test]
    fn integration_rejects_mutable_revision_inputs() {
        let fixture = RepoFixture::new("immutable-revisions");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let cases = [
            ("HEAD", base.as_str(), base.as_str(), "source_revision"),
            (source.as_str(), "HEAD", base.as_str(), "base_revision"),
            (source.as_str(), base.as_str(), "HEAD", "aggregate_revision"),
        ];
        for (source_revision, base_revision, aggregate_revision, field) in cases {
            let result = coordinator.integrate(IntegrationRequest::new(
                "workflow",
                format!("child-{field}"),
                child.path(),
                source_revision,
                base_revision,
                &fixture.aggregate,
                aggregate_revision,
            ));
            assert!(
                matches!(&result, Err(IntegrationError::InvalidRequest(message)) if message.contains(field)),
                "mutable {field} should be rejected: {result:?}"
            );
        }
        child.abort();
    }

    #[test]
    fn replay_revalidates_the_current_write_scope() {
        let fixture = RepoFixture::new("replay-scope");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "secret.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let request = IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &base,
        );
        coordinator
            .integrate(request.clone())
            .expect("first integration");
        let replay = coordinator.integrate(request.with_write_scope(["safe"]));
        assert!(matches!(
            replay,
            Err(IntegrationError::InvalidRequest(message))
                if message.contains("secret.txt")
        ));
        child.abort();
    }

    #[test]
    fn textual_conflict_does_not_touch_aggregate() {
        let fixture = RepoFixture::new("conflict");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "README.md", "child\n", "child conflict");
        let source = rev(child.path(), "HEAD");
        fs::write(fixture.aggregate.join("README.md"), "aggregate\n")
            .expect("aggregate change should be written");
        git(&fixture.aggregate, &["add", "README.md"]);
        git(
            &fixture.aggregate,
            &["commit", "--quiet", "-m", "aggregate"],
        );
        let aggregate = rev(&fixture.aggregate, "HEAD");
        let aggregate_status_before = git_stdout(
            &fixture.aggregate,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        );
        let result = fixture.coordinator().integrate(IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &aggregate,
        ));
        assert!(matches!(
            result,
            Err(IntegrationError::TextualConflict { .. })
        ));
        assert!(
            read_ref(
                &fixture.repo,
                &fixture
                    .coordinator()
                    .aggregate_ref("workflow")
                    .expect("ref")
            )
            .expect("ref read")
            .is_none()
        );
        assert_eq!(rev(&fixture.aggregate, "HEAD"), aggregate);
        assert_eq!(
            git_stdout(
                &fixture.aggregate,
                &["status", "--porcelain=v1", "--untracked-files=all"]
            ),
            aggregate_status_before,
            "conflict preview leaves the user checkout index and worktree untouched"
        );
        child.abort();
    }

    #[test]
    fn aggregate_move_is_a_cas_conflict() {
        let fixture = RepoFixture::new("aggregate-move");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let aggregate_ref = coordinator.aggregate_ref("workflow").expect("ref");
        git(&fixture.repo, &["update-ref", &aggregate_ref, &base]);
        fs::write(fixture.aggregate.join("aggregate.txt"), "moved\n")
            .expect("aggregate change should be written");
        git(&fixture.aggregate, &["add", "aggregate.txt"]);
        git(
            &fixture.aggregate,
            &["commit", "--quiet", "-m", "aggregate moved"],
        );
        let moved = rev(&fixture.aggregate, "HEAD");
        git(&fixture.repo, &["update-ref", &aggregate_ref, &moved]);
        let result = coordinator.integrate(IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &base,
        ));
        assert!(
            matches!(&result, Err(IntegrationError::AggregateMoved { actual, .. }) if actual == &moved),
            "unexpected result: {result:?}"
        );
        child.abort();
    }

    #[test]
    fn symbolic_owned_ref_cannot_move_user_branch() {
        let fixture = RepoFixture::new("symbolic-ref");
        let base = fixture.head();
        git(&fixture.repo, &["branch", "user-branch", &base]);
        let child = fixture.child("workflow", "child");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let aggregate_ref = coordinator
            .aggregate_ref("workflow")
            .expect("aggregate ref");
        git(
            &fixture.repo,
            &["symbolic-ref", &aggregate_ref, "refs/heads/user-branch"],
        );
        let result = coordinator.integrate(IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &base,
        ));
        assert!(matches!(
            result,
            Err(IntegrationError::OwnedRefSymbolic { reference, .. })
                if reference == aggregate_ref
        ));
        assert_eq!(rev(&fixture.repo, "refs/heads/user-branch"), base);
        child.abort();
    }

    #[test]
    fn dirty_aggregate_and_unsafe_scope_fail_closed() {
        let fixture = RepoFixture::new("dirty");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        fs::write(fixture.aggregate.join("scratch.txt"), "local\n")
            .expect("untracked file should be written");
        let dirty = fixture.coordinator().integrate(IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &base,
        ));
        assert!(matches!(dirty, Err(IntegrationError::DirtyWorktree { .. })));
        let _ = fs::remove_file(fixture.aggregate.join("scratch.txt"));
        let unsafe_scope = fixture.coordinator().integrate(
            IntegrationRequest::new(
                "workflow",
                "child",
                child.path(),
                &source,
                &base,
                &fixture.aggregate,
                &base,
            )
            .with_expected_write_paths(["../outside"]),
        );
        assert!(matches!(
            unsafe_scope,
            Err(IntegrationError::PathEscape { .. })
        ));
        child.abort();
    }

    #[test]
    fn recovery_rejects_missing_aggregate_ref() {
        let fixture = RepoFixture::new("missing-aggregate-ref");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let request = IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &base,
        );
        let first = coordinator
            .integrate(request.clone())
            .expect("first integration");
        let aggregate_ref = coordinator
            .aggregate_ref("workflow")
            .expect("aggregate ref");
        git(&fixture.repo, &["update-ref", "-d", &aggregate_ref]);
        let recovered = coordinator.integrate(request);
        assert!(matches!(
            recovered,
            Err(IntegrationError::MissingAggregateRef { contribution_ref })
                if contribution_ref == coordinator.contribution_ref("workflow", "child").expect("contribution ref")
        ));
        assert_eq!(
            rev(
                &fixture.repo,
                &coordinator
                    .contribution_ref("workflow", "child")
                    .expect("contribution ref")
            ),
            first.receipt.result_sha
        );
        child.abort();
    }

    #[test]
    fn receipt_paths_bind_to_the_final_merge_tree() {
        let fixture = RepoFixture::new("final-paths");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        fs::write(fixture.aggregate.join("shared.txt"), "same\n")
            .expect("aggregate change should be written");
        git(&fixture.aggregate, &["add", "shared.txt"]);
        git(
            &fixture.aggregate,
            &["commit", "--quiet", "-m", "aggregate"],
        );
        let aggregate = rev(&fixture.aggregate, "HEAD");
        commit_child(&child, "shared.txt", "same\n", "shared");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let request = IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &aggregate,
        );
        let first = coordinator
            .integrate(request.clone())
            .expect("integration should succeed");
        assert_eq!(first.receipt.changed_paths, vec!["child.txt".to_string()]);
        let replay = coordinator
            .integrate(request)
            .expect("replay should succeed");
        assert_eq!(replay.receipt, first.receipt);
        child.abort();
    }

    #[test]
    fn forged_recovery_result_tree_is_rejected() {
        let fixture = RepoFixture::new("forged-result");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let aggregate_ref = coordinator
            .aggregate_ref("workflow")
            .expect("aggregate ref");
        let contribution_ref = coordinator
            .contribution_ref("workflow", "child")
            .expect("contribution ref");
        let tree = git_stdout(&fixture.repo, &["rev-parse", &format!("{base}^{{tree}}")]);
        let message = format!(
            "Maestro workflow integration\n\nMaestro-Workflow: workflow\nMaestro-Contribution: child\nMaestro-Source: {source}\nMaestro-Base: {base}\nMaestro-Previous-Aggregate: {base}\n"
        );
        let forged = {
            let output = Command::new("git")
                .args([
                    "commit-tree",
                    &tree,
                    "-p",
                    &base,
                    "-p",
                    &source,
                    "-m",
                    &message,
                ])
                .current_dir(&fixture.repo)
                .output()
                .expect("git commit-tree should run");
            assert!(
                output.status.success(),
                "git commit-tree failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&fixture.repo, &["update-ref", &aggregate_ref, &forged]);
        git(&fixture.repo, &["update-ref", &contribution_ref, &forged]);
        let result = coordinator.integrate(IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &base,
        ));
        assert!(matches!(
            result,
            Err(IntegrationError::Receipt(message))
                if message.contains("does not match merge tree")
        ));
        assert_eq!(rev(&fixture.repo, &aggregate_ref), forged);
        child.abort();
    }

    #[test]
    fn directory_scope_allows_descendants_and_empty_scope_denies() {
        assert!(
            validate_declared_scope(
                true,
                &[PathBuf::from(".")],
                &[PathBuf::from("src/nested.txt")]
            )
            .is_ok()
        );
        let fixture = RepoFixture::new("scope");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "src/nested.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let request = IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &base,
        );

        let denied = coordinator
            .integrate(request.clone().with_write_scope(Vec::<&str>::new()))
            .expect_err("an explicit empty write scope must deny changes");
        assert!(matches!(
            denied,
            IntegrationError::InvalidRequest(message)
                if message == "write scope denies all changed paths"
        ));
        assert!(
            read_ref(
                &fixture.repo,
                &coordinator
                    .aggregate_ref("workflow")
                    .expect("aggregate ref")
            )
            .expect("aggregate ref read")
            .is_none()
        );

        let integrated = coordinator
            .integrate(request.with_write_scope(["src"]))
            .expect("a directory scope should include descendants");
        assert_eq!(
            integrated.receipt.changed_paths,
            vec!["src/nested.txt".to_string()]
        );
        child.abort();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_changed_path_fails_closed() {
        let fixture = RepoFixture::new("symlink");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        fs::write(child.path().join("target.txt"), "target\n")
            .expect("symlink target should be written");
        std::os::unix::fs::symlink("target.txt", child.path().join("link.txt"))
            .expect("symlink should be created");
        git(child.path(), &["add", "--all"]);
        git(child.path(), &["commit", "--quiet", "-m", "child symlink"]);
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let result = coordinator.integrate(IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &base,
        ));
        assert!(matches!(
            result,
            Err(IntegrationError::SymlinkPath { path }) if path == Path::new("link.txt")
        ));
        assert!(
            read_ref(
                &fixture.repo,
                &coordinator
                    .aggregate_ref("workflow")
                    .expect("aggregate ref")
            )
            .expect("aggregate ref read")
            .is_none()
        );
        child.abort();
    }

    #[cfg(unix)]
    #[test]
    fn replay_rejects_committed_tab_named_symlink_after_child_cleanup() {
        let fixture = RepoFixture::new("tab-symlink-replay");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        let tab_named_link = "link\tname";
        fs::write(child.path().join("target.txt"), "target\n")
            .expect("symlink target should be written");
        std::os::unix::fs::symlink("target.txt", child.path().join(tab_named_link))
            .expect("tab-named symlink should be created");
        git(child.path(), &["add", "--all"]);
        git(
            child.path(),
            &["commit", "--quiet", "-m", "child tab symlink"],
        );
        let source = rev(child.path(), "HEAD");
        let coordinator = fixture.coordinator();
        let aggregate_ref = coordinator
            .aggregate_ref("workflow")
            .expect("aggregate ref");
        let contribution_ref = coordinator
            .contribution_ref("workflow", "child")
            .expect("contribution ref");
        let worktrees_before = git_stdout(&fixture.repo, &["worktree", "list", "--porcelain"]);
        let merge_tree = merge_tree_in_scratch_worktree(&fixture.repo, &base, &source)
            .expect("merge tree should be returned");
        let worktrees_after = git_stdout(&fixture.repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees_after, worktrees_before,
            "merge preview should remove its scratch worktree"
        );
        let message = format!(
            "Maestro workflow integration\n\nMaestro-Workflow: workflow\nMaestro-Contribution: child\nMaestro-Source: {source}\nMaestro-Base: {base}\nMaestro-Previous-Aggregate: {base}\n"
        );
        let output = Command::new("git")
            .args([
                "commit-tree",
                &merge_tree,
                "-p",
                &base,
                "-p",
                &source,
                "-m",
                &message,
            ])
            .current_dir(&fixture.repo)
            .output()
            .expect("git commit-tree should run");
        assert!(
            output.status.success(),
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let forged = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        git(&fixture.repo, &["update-ref", &aggregate_ref, &forged]);
        git(&fixture.repo, &["update-ref", &contribution_ref, &forged]);

        let child_path = child.path().to_path_buf();
        child.abort();
        assert!(!child_path.exists(), "child worktree should be cleaned up");

        let result = coordinator.integrate(IntegrationRequest::new(
            "workflow",
            "child",
            child_path,
            &source,
            &base,
            &fixture.aggregate,
            &base,
        ));
        assert!(
            matches!(&result, Err(IntegrationError::SymlinkPath { path }) if path == Path::new(tab_named_link)),
            "tab-named committed symlink should be rejected during replay: {result:?}"
        );
    }

    #[test]
    fn merge_preview_conflict_cleans_scratch_worktree() {
        let fixture = RepoFixture::new("merge-preview-conflict");
        let child = fixture.child("workflow", "child");
        commit_child(&child, "README.md", "child\n", "child conflict");
        fs::write(fixture.aggregate.join("README.md"), "aggregate\n")
            .expect("aggregate change should be written");
        git(&fixture.aggregate, &["add", "README.md"]);
        git(
            &fixture.aggregate,
            &["commit", "--quiet", "-m", "aggregate"],
        );
        let aggregate = rev(&fixture.aggregate, "HEAD");
        let source = rev(child.path(), "HEAD");
        let aggregate_status_before = git_stdout(
            &fixture.aggregate,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        );
        let worktrees_before = git_stdout(&fixture.repo, &["worktree", "list", "--porcelain"]);

        let result = merge_tree_in_scratch_worktree(&fixture.repo, &aggregate, &source);

        assert!(matches!(
            result,
            Err(IntegrationError::TextualConflict { .. })
        ));
        let worktrees_after = git_stdout(&fixture.repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees_after, worktrees_before,
            "conflict preview should remove its scratch worktree"
        );
        assert_eq!(rev(&fixture.aggregate, "HEAD"), aggregate);
        assert_eq!(
            git_stdout(
                &fixture.aggregate,
                &["status", "--porcelain=v1", "--untracked-files=all"]
            ),
            aggregate_status_before,
            "conflict preview should leave the user checkout index and worktree untouched"
        );
        child.abort();
    }

    #[test]
    fn receipt_write_failure_recovers_from_git_refs() {
        let fixture = RepoFixture::new("receipt-recovery");
        let base = fixture.head();
        let child = fixture.child("workflow", "child");
        commit_child(&child, "child.txt", "child\n", "child");
        let source = rev(child.path(), "HEAD");
        fs::create_dir_all(&fixture.receipts).expect("receipt parent should be created");
        let bad_receipt_path = fixture.receipts.join("file");
        fs::write(&bad_receipt_path, "not a directory").expect("bad receipt path");
        let bad = IntegrationCoordinator::new(&fixture.repo, &bad_receipt_path)
            .expect("coordinator should not write at construction");
        let request = IntegrationRequest::new(
            "workflow",
            "child",
            child.path(),
            &source,
            &base,
            &fixture.aggregate,
            &base,
        );
        let first = bad.integrate(request.clone());
        assert!(matches!(first, Err(IntegrationError::ReceiptWrite { .. })));
        let recovered = fixture
            .coordinator()
            .integrate(request)
            .expect("recovery should work");
        assert!(recovered.replayed);
        assert_eq!(recovered.receipt.source_sha, source);
        child.abort();
    }
}
