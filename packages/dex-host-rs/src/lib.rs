//! A local `dex_loop` host: `Log`, `Tools` and `Effects` ports backed by the
//! workspace filesystem, with no database and no HTTP server, plus a `Model`
//! adapter over `maestro-ai` (`model.rs`) and a consumer that drives one
//! turn to completion unattended (`turn.rs`, `run_local_turn`), and
//! `HostTools` (`host_tools.rs`): the `Tools` port over a Maestro
//! `NativeExecutionHost`, which offers Maestro's real tool registry to the
//! kernel and gates it the way the native actor does.
//!
//! This is the first slice of "Maestro becomes another host with a local
//! Log" (see `docs/design/maestro-on-dex-loop.md` at the repository root).
//! It proves the kernel's park/approve/resume semantics run correctly
//! against a purely local host — see `tests/turn.rs` and `turn.rs`'s own
//! tests — without touching Maestro's existing turn loop (`maestro-runtime`,
//! `maestro-local-host`), which still runs every real turn today.
//! `run_local_turn` is reached from `tui-rs` only behind
//! `MAESTRO_DEX_LOOP=1` (`dex_loop_local.rs`); the flag is unset by default.
//!
//! Placed in Maestro's own Rust workspace (`products/maestro/packages/`,
//! not `rust/crates/`) because that is where a real cutover would keep it;
//! `dex-loop` is reached by a path dependency across the workspace
//! boundary, the same shape `maestro-swarm` already uses.

mod effects;
mod host_tools;
mod host_turn;
mod lease;
mod log;
mod model;
mod observed;
mod prompt_hooks;
mod tools;
mod turn;

pub use effects::LocalEffects;
pub use host_tools::{CONFIRMATION_FIELD, HEADLESS_GATED, HostTools, USER_ASK};
pub use host_turn::{HostTurn, HostTurnRun, Park, Step, turn_dir};
pub use log::{LocalLog, LogError};
pub use model::AiRsModel;
pub use observed::{Observed, ObservedLog};
pub use prompt_hooks::{AdmittedPrompt, admit_prompt};
pub use tools::{LocalTools, READ_FILE, WRITE_FILE};
pub use turn::{LocalTurnOutcome, LocalTurnRequest, UNATTENDED_ANSWER, run_local_turn};

/// Re-exported so a consumer that only depends on this crate (e.g.
/// `tui-rs`'s `dex_loop_local.rs`) can build a `ThreadId`/`PrincipalId`/
/// `TurnId`/`Exit` without also taking a direct path dependency on
/// `dex-loop` across the same workspace boundary.
pub use dex_loop;
