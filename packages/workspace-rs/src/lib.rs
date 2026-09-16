//! Workspace file discovery, search, and Git worktree operations for Maestro.

pub mod files;
pub mod git;
pub mod integration;
pub mod symbols;
pub mod worktree;

pub use integration::{
    IntegrationCoordinator, IntegrationError, IntegrationReceipt, IntegrationRequest,
    IntegrationResult, IntegrationStatus,
};
