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
    Serve {
        port: Option<u16>,
        parent_pid: Option<u32>,
        liveness_fd: Option<i32>,
    },
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
        let mut parent_pid = None;
        let mut liveness_fd = None;
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
            } else if argument == "--parent-pid" {
                index += 1;
                let value = strings.get(index).ok_or("--parent-pid requires a value")?;
                parent_pid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid parent pid: {value}"))?,
                );
            } else if let Some(value) = argument.strip_prefix("--parent-pid=") {
                parent_pid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid parent pid: {value}"))?,
                );
            } else if argument == "--liveness-fd" {
                index += 1;
                let value = strings.get(index).ok_or("--liveness-fd requires a value")?;
                liveness_fd = Some(
                    value
                        .parse::<i32>()
                        .map_err(|_| format!("invalid liveness fd: {value}"))?,
                );
            } else if let Some(value) = argument.strip_prefix("--liveness-fd=") {
                liveness_fd = Some(
                    value
                        .parse::<i32>()
                        .map_err(|_| format!("invalid liveness fd: {value}"))?,
                );
            } else {
                return Err(format!(
                    "`maestro serve` does not accept prompt arguments or option `{argument}`"
                ));
            }
            index += 1;
        }
        if parent_pid.is_some() && liveness_fd.is_some() {
            return Err("--parent-pid and --liveness-fd are mutually exclusive".to_owned());
        }
        if parent_pid == Some(0) {
            return Err("--parent-pid must be a positive process id".to_owned());
        }
        if liveness_fd.is_some_and(|fd| fd < 0) {
            return Err("--liveness-fd must be a non-negative file descriptor".to_owned());
        }
        return Ok(Command::Serve {
            port,
            parent_pid,
            liveness_fd,
        });
    }

    // Every other invocation (interactive TUI, `exec`/`print`/`-p`,
    // `--headless`/`--rpc`, hosted-runner, and every utility subcommand) is
    // forwarded to `maestro_tui::run_cli` with the original argv, which
    // makes the real dispatch decision.
    Ok(Command::Forward)
}
