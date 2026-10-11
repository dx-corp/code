//! Single integration-test binary for `dex-loop`.
//!
//! Cargo compiles and links one binary per `tests/*.rs` file. Folding the files
//! into modules of one target keeps every test while paying codegen and link
//! cost once; filters still work with a module prefix, for example
//! `cargo test -p dex-loop --test suite action_confirmation::`.

mod support;

#[path = "action_confirmation.rs"]
mod action_confirmation;

#[path = "authorized_tools.rs"]
mod authorized_tools;

#[path = "budget_contract.rs"]
mod budget_contract;

#[path = "client_tool_replay.rs"]
mod client_tool_replay;

#[path = "codemode.rs"]
mod codemode;

#[path = "codemode_qualification.rs"]
mod codemode_qualification;

#[path = "codemode_workflows.rs"]
mod codemode_workflows;

#[path = "content_policy.rs"]
mod content_policy;

#[path = "grc_context.rs"]
mod grc_context;

#[path = "interaction_mode.rs"]
mod interaction_mode;

#[path = "loop_recovery.rs"]
mod loop_recovery;

#[path = "model_binding.rs"]
mod model_binding;

#[path = "prefetch.rs"]
mod prefetch;

#[path = "prefetch_authority.rs"]
mod prefetch_authority;

#[path = "read_wave_progress.rs"]
mod read_wave_progress;

#[path = "scenarios.rs"]
mod scenarios;

#[path = "sim.rs"]
mod sim;

#[path = "storage_contract.rs"]
mod storage_contract;

#[path = "tool_deadline.rs"]
mod tool_deadline;

#[path = "wall_budget.rs"]
mod wall_budget;
