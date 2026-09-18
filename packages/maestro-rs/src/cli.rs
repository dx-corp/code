use std::ffi::OsString;

/// The commands this binary decides directly (help/version text, the
/// in-process control plane). Everything else is forwarded verbatim to
/// `maestro_tui::run_cli`, which owns the real argv-to-target routing
/// (TUI vs utility handler vs headless/exec/print) via the canonical
/// command table in `packages/tui-rs/src/entrypoint.rs`. `classify` used to
/// re-derive that routing here too (a second, independently maintained copy
/// of the utility command list and the headless/exec/print flag matching)
/// even though the result was discarded by `main`'s dispatch, which always
/// forwarded the original argv regardless of which `Agent`/`HostedRunner`/
/// `Utility` variant was produced. Collapsing those into one `Forward`
/// variant removes that dead computation and the duplicated table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Serve { port: Option<u16> },
    Forward,
    Help,
    Version,
}

pub fn classify<I, S>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let strings = args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    let first = strings.first().map(|value| value.as_ref());

    if first.is_none() {
        return Ok(Command::Forward);
    }
    if matches!(first, Some("--version" | "-V" | "-v")) {
        return Ok(Command::Version);
    }
    if matches!(
        first,
        Some("--help" | "-h" | "--help-hidden" | "--help-all")
    ) {
        return Ok(Command::Help);
    }
    if first == Some("serve") {
        let mut port = None;
        let mut index = 1;
        while index < strings.len() {
            let argument = strings[index].as_ref();
            if argument == "--port" {
                index += 1;
                let value = strings.get(index).ok_or("--port requires a value")?;
                port = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| format!("invalid serve port: {value}"))?,
                );
            } else if let Some(value) = argument.strip_prefix("--port=") {
                port = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| format!("invalid serve port: {value}"))?,
                );
            } else {
                return Err(format!(
                    "`maestro serve` does not accept prompt arguments or option `{argument}`"
                ));
            }
            index += 1;
        }
        return Ok(Command::Serve { port });
    }

    // Every other invocation (interactive TUI, `exec`/`print`/`-p`,
    // `--headless`/`--rpc`, hosted-runner, and every utility subcommand) is
    // forwarded to `maestro_tui::run_cli` with the original argv, which
    // makes the real dispatch decision.
    Ok(Command::Forward)
}
