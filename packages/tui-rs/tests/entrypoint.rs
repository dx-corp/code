//! Table-driven integration tests for the native entrypoint's argv routing.
//!
//! `packages/maestro-rs/src/cli.rs` used to keep its own utility
//! command list and its own `--headless`/`--rpc`/`--mode=headless`/`exec`/
//! `print`/`-p` matching, independently of the routing here — with nothing
//! enforcing that the two stayed in sync. `maestro-rs` now forwards
//! everything except `serve`/`--help`/`--version` to `run_cli` unchanged, so
//! this crate's `entrypoint` module is the single place that decides
//! whether a subcommand reaches the utility handler, the headless server,
//! the exec/print bridge, or the interactive TUI. This test drives that
//! real routing code (not a re-description of it) across the full command
//! matrix so a future edit that silently drops or misroutes a command fails
//! here instead of shipping.

use maestro_tui::entrypoint::{
    AgentEntry, ClapDispatch, NATIVE_UTILITY_COMMANDS, classify_agent_entry,
    classify_clap_dispatch, native_utility_tokens,
};
use std::ffi::OsString;

fn argv(words: &[&str]) -> Vec<OsString> {
    std::iter::once("maestro-tui")
        .chain(words.iter().copied())
        .map(OsString::from)
        .collect()
}

#[test]
fn every_native_utility_command_routes_to_the_utility_handler() {
    assert_eq!(
        NATIVE_UTILITY_COMMANDS.len(),
        46,
        "this test's evidence baseline is 46 utility commands; update it deliberately \
         if the canonical table changes size"
    );

    let thread_args = argv(&["thread", "attach", "thread-id"]);
    assert_eq!(
        native_utility_tokens(&thread_args[1..]),
        Some(vec![
            "thread".to_owned(),
            "attach".to_owned(),
            "thread-id".to_owned()
        ])
    );

    assert!(NATIVE_UTILITY_COMMANDS.contains(&"experiments"));
    let experiment_args = argv(&["experiments", "off"]);
    assert_eq!(
        native_utility_tokens(&experiment_args[1..]),
        Some(vec!["experiments".to_owned(), "off".to_owned()])
    );

    assert!(NATIVE_UTILITY_COMMANDS.contains(&"stealth-models"));
    let stealth_args = argv(&["stealth-models", "status"]);
    assert_eq!(
        native_utility_tokens(&stealth_args[1..]),
        Some(vec!["stealth-models".to_owned(), "status".to_owned()])
    );

    let workflow_args = argv(&["workflow", "--help"]);
    let workflow_tokens =
        native_utility_tokens(&workflow_args[1..]).expect("workflow utility tokens");
    assert_eq!(
        workflow_tokens.first().map(String::as_str),
        Some("workflow"),
        "workflow should route to the utility handler"
    );

    for command in NATIVE_UTILITY_COMMANDS {
        let raw_args = argv(&[command, "--help"]);
        let tokens = native_utility_tokens(&raw_args[1..]).unwrap_or_else(|| {
            panic!("`{command}` should route to the utility handler, not the TUI")
        });
        assert_eq!(
            tokens.first().map(String::as_str),
            Some(command),
            "`{command}` utility tokens should start with the command itself"
        );

        // `run` is deliberately the one exception: it's only a utility
        // command when followed by one of its reconstruct subcommands (see
        // `native_utility_tokens`), so a bare `run` falls through to the
        // TUI instead. Every other utility command routes on the word
        // alone.
        if command != "run" {
            let bare_args = argv(&[command]);
            assert!(
                native_utility_tokens(&bare_args[1..]).is_some(),
                "`{command}` alone should still route to the utility handler"
            );
        }
    }
}

#[test]
fn tui_entry_cases_do_not_route_to_the_utility_handler() {
    let cases: &[&[&str]] = &[
        &[],
        &["exec", "hello"],
        &["print", "hello"],
        &["fork", "session-id"],
        &["-p", "hello"],
        &["--headless"],
        &["--rpc"],
        &["--mode=headless"],
        &["some-unknown-command"],
    ];

    for case in cases {
        let raw_args = argv(case);
        assert!(
            native_utility_tokens(&raw_args[1..]).is_none(),
            "{case:?} should not be swallowed by the utility handler"
        );
    }
}

#[test]
fn subcommand_words_route_through_the_agent_fast_paths() {
    assert_eq!(
        classify_agent_entry(&argv(&["headless"])),
        AgentEntry::HeadlessSubcommand
    );
    assert_eq!(
        classify_agent_entry(&argv(&["rpc"])),
        AgentEntry::HeadlessSubcommand
    );
    assert_eq!(
        classify_agent_entry(&argv(&["fork", "session-id"])),
        AgentEntry::ForkSubcommand
    );
    assert_eq!(
        classify_agent_entry(&argv(&["exec", "hello"])),
        AgentEntry::ExecOrPrintSubcommand("exec")
    );
    assert_eq!(
        classify_agent_entry(&argv(&["print", "hello"])),
        AgentEntry::ExecOrPrintSubcommand("print")
    );
}

#[test]
fn non_subcommand_argv_falls_through_to_clap_parsing() {
    let cases: &[&[&str]] = &[
        &[],
        &["-p", "hello"],
        &["--headless"],
        &["--rpc"],
        &["--mode=headless"],
        &["some-unknown-command"],
    ];

    for case in cases {
        assert_eq!(
            classify_agent_entry(&argv(case)),
            AgentEntry::ClapParsed,
            "{case:?} should fall through to the clap-derived Args parse"
        );
    }
}

#[test]
fn clap_flags_route_to_the_expected_agent_dispatch() {
    assert_eq!(
        classify_clap_dispatch(&argv(&["-p", "hello"])),
        ClapDispatch::Print
    );
    assert_eq!(
        classify_clap_dispatch(&argv(&["--print", "hello"])),
        ClapDispatch::Print
    );
    assert_eq!(
        classify_clap_dispatch(&argv(&["--headless"])),
        ClapDispatch::Headless
    );
    assert_eq!(
        classify_clap_dispatch(&argv(&["--rpc"])),
        ClapDispatch::Headless
    );
    assert_eq!(
        classify_clap_dispatch(&argv(&[])),
        ClapDispatch::Interactive
    );
    assert_eq!(
        classify_clap_dispatch(&argv(&["fix", "the", "bug"])),
        ClapDispatch::Interactive,
        "an unrecognized first word with no leading `-` is a prompt, not an error"
    );
    for flag in ["--help", "-h", "--version", "-V"] {
        assert_eq!(
            classify_clap_dispatch(&argv(&[flag])),
            ClapDispatch::DisplayHelpOrVersion,
            "{flag} should let clap display output without starting the interactive updater"
        );
    }

    // `--mode=headless`/`--mode headless` were only ever recognized by the
    // old `maestro-rs` classify() (see `packages/maestro-rs/src/cli.rs`),
    // whose result was discarded before this refactor — `run_cli` always
    // forwarded the original argv regardless. The `Args` clap derive has no
    // `--mode` flag, so this argv was never actually routed to headless
    // mode by the real dispatcher; it is (and remains) a parse error.
    assert_eq!(
        classify_clap_dispatch(&argv(&["--mode", "headless"])),
        ClapDispatch::ParseError
    );
    assert_eq!(
        classify_clap_dispatch(&argv(&["--mode=headless"])),
        ClapDispatch::ParseError
    );
}
