//! Hosted runner: the single-session HTTP surface, rendezvous carrier and CLI
//! startup that Platform-managed Maestro runtimes execute.
//!
//! Split out of `maestro-local-host` so that edits here recompile this crate
//! instead of the whole local host.

#[cfg(test)]
pub(crate) use maestro_ai as ai;
#[cfg(test)]
pub(crate) use maestro_local_host::credential_mode;
pub(crate) use maestro_local_host::{
    agent, fs_atomic, git, headless, headless_server, safety, transcript,
};

pub mod hosted_runner;
pub mod hosted_runner_cli;
pub mod hosted_runner_conformance;

/// Environment switch that lets the spawned native child accept the loopback
/// Identity fixture, now that `maestro-local-host` is not compiled as a test build.
#[cfg(test)]
pub(crate) const ID_AUTH: &str = "MAESTRO_TEST_IDENTITY_AUTHORITY";
