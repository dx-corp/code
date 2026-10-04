//! Orderly shutdown for the interactive TUI on an externally delivered
//! process signal: SIGINT, SIGTERM, SIGHUP on Unix; the Ctrl+C, console
//! close, and system-shutdown console events on Windows.
//!
//! # Why this exists
//!
//! Nothing on the interactive path (`entrypoint::run_agent` ->
//! `App::run()`) previously registered any of these. A supervisor stop, a
//! bare `kill <pid>`, a container shutdown, or closing the terminal
//! (SIGHUP) hit the OS default disposition: immediate termination, no
//! unwind, no `Drop`, no panic hook. Two things depend on an orderly exit
//! actually running:
//!
//! - `tools::process_registry` tracks background processes (bash
//!   `run_in_background`, `tools::background_tasks`) for cleanup on exit;
//!   its only callers are the panic hook and the path after `app.run()`
//!   returns *normally* (`entrypoint.rs`). A bare signal skips both, so
//!   those children are orphaned permanently.
//! - `session::writer::SessionWriter` buffers up to
//!   `DEFAULT_BATCH_SIZE - 1` entries and flushes on `Drop`. A bare signal
//!   never runs that `Drop`, so `--resume` silently drops the most recent
//!   turns with no error shown.
//!
//! # Ctrl+C-as-keypress is untouched by design
//!
//! While the TUI is running, `crossterm::terminal::enable_raw_mode` clears
//! the tty's `ISIG` termios flag, so the terminal driver stops turning a
//! Ctrl+C *keystroke* into a `SIGINT` at all -- it only ever arrives as the
//! byte `0x03` on the input event stream, handled entirely in-app as
//! "cancel the current turn". The listeners in this module only resolve for
//! signals actually delivered by the OS (`kill(2)`, a supervisor, a
//! disconnecting controlling terminal): raw mode means the keyboard never
//! produces one of those while the app has focus of the tty. So installing
//! these listeners cannot turn an in-app Ctrl+C keypress into a process
//! exit. Outside of raw mode (startup, `init_fallback`, a shell that never
//! entered raw mode), a Ctrl+C now exits through the orderly path below
//! instead of the OS default -- a strict improvement, not a UX change to
//! the in-app cancel behavior.
//!
//! # Composition with PR #3094 (signal-safe crash handler)
//!
//! #3094 installs an async-signal-safe `SIGSEGV`/`SIGBUS` handler that
//! restores the terminal from *signal-handler context* (precomputed
//! escapes, no heap allocation, no async runtime) because a hard crash can
//! occur at any point, including inside the allocator or the runtime. That
//! is a fundamentally different mechanism from this module: SIGINT/TERM/HUP
//! are not crashes, we are already inside the tokio runtime when they
//! arrive, and normal async code (allocation, `.await`, `eprintln!`) is
//! safe to run in response to them. This module does not touch, duplicate,
//! or need `crash_handler.rs`; the two are complementary (hard-crash vs.
//! orderly-termination) and can be reviewed/merged independently.
//!
//! # Shutdown sequence
//!
//! 1. Register all signal streams before constructing `App`, then race
//!    `App::run` against the monitor receiver with
//!    `tokio::select!`.
//! 2. If the signal wins, arm the second-signal escape hatch (see
//!    "Re-entrancy" below), then drop the pinned `app.run()` future. That
//!    is ordinary `Future` cancellation of the interactive loop, not panic
//!    unwinding, so it happens the same way whether or not this crate's
//!    `panic = "abort"` release profile is in effect. The future only
//!    borrows `App` (`run` takes `&mut self`), so dropping it does not yet
//!    drop the App itself.
//! 3. Call and await `App::signal_shutdown_teardown()`: cancel the per-tool
//!    cancellation tokens, await their task handles so foreground child
//!    processes are killed and reaped, and emit the notifier teardown the
//!    normal exit path runs (clear tab progress, restore the saved terminal
//!    title).
//! 4. On a blocking thread (`tokio::task::spawn_blocking`, so the sync
//!    work cannot starve the escape-hatch task on this worker): drop the
//!    App, which cascades into `SessionWriter::drop` and flushes buffered
//!    entries (`session/writer.rs`); call `cleanup_background_processes()`
//!    (the process registry is a free-standing global
//!    `std::sync::LazyLock<RwLock<..>>` in `tools::process_registry`, not
//!    owned by `App`); and call `terminal::restore()` (also a free
//!    function on a process-global tty handle, `terminal::setup::TTY`, so
//!    it is safe regardless of what state `App::run` left the terminal in).
//! 5. Return `Ok(signal.exit_code())` -- the conventional `128 + signo` --
//!    so the caller's existing exit-code and worktree-teardown plumbing in
//!    `run_cli` still runs exactly as it does for a normal exit. Shutdown
//!    by signal does not skip worktree cleanup.
//!
//! # Re-entrancy
//!
//! Step 4 above is synchronous once the race resolves, so it runs on a
//! blocking thread rather than an async worker. If any of it were ever to
//! hang (a wedged child process, a blocked write to a dead tty), an
//! impatient operator sending a second signal needs an escape hatch that
//! does not depend on this task being polled again. [`ShutdownMonitor`]
//! owns the registered streams on a detached task created before `App`
//! construction. After forwarding the first signal it keeps waiting for a
//! second and can force-exit even if app construction or cleanup is stuck.
//! If the TUI returns normally, dropping the monitor receiver leaves that
//! task alive through the caller's synchronous worktree cleanup; a signal
//! during that interval restores the terminal and exits immediately.
//!
//! # Cancellation safety
//!
//! Racing `app.run()` inside `select!` means whatever the interactive loop
//! was in the middle of (an in-flight model request, a pending tool
//! execution, a partially read keystroke) is abandoned mid-flight when the
//! signal wins -- the same "no further forward progress" outcome as a hard
//! kill, just reached through a controlled Rust drop instead of `SIGKILL`.
//! The durability guarantee this module adds is narrower and specific:
//! `SessionWriter::flush` only ever writes whole, already-serialized
//! entries, so cancellation cannot produce a torn write to the session
//! file. Any background tokio tasks the app may have spawned and detached
//! (not tracked by `tools::process_registry`, e.g. a streaming response
//! task) are not explicitly joined or aborted here; they are torn down by
//! the process exit that follows shortly after `run_cli` receives this
//! function's return value, the same way they already are on the existing
//! normal-completion path.

use crate::tools::cleanup_background_processes;
use tokio::sync::{mpsc, oneshot};

/// A signal that should trigger an orderly shutdown of the interactive TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShutdownSignal {
    Interrupt,
    Terminate,
    Hangup,
    /// Windows Ctrl+C console event.
    #[cfg(windows)]
    CtrlC,
    /// Windows Ctrl+Break console event.
    #[cfg(windows)]
    CtrlBreak,
    /// Windows console-close event (user closed the window / clicked the X).
    #[cfg(windows)]
    CtrlClose,
    /// Windows logoff/shutdown console event.
    #[cfg(windows)]
    CtrlShutdown,
}

impl ShutdownSignal {
    /// The conventional `128 + signo` shell exit code for the signal that
    /// caused the process to stop. Windows console events have no POSIX
    /// signal number; `130` (as if a `SIGINT` had been raised) matches what
    /// most cross-platform CLIs already report for a Ctrl+C exit.
    pub(super) fn exit_code(self) -> i32 {
        match self {
            ShutdownSignal::Interrupt => 128 + 2,
            ShutdownSignal::Terminate => 128 + 15,
            ShutdownSignal::Hangup => 128 + 1,
            #[cfg(windows)]
            ShutdownSignal::CtrlC
            | ShutdownSignal::CtrlBreak
            | ShutdownSignal::CtrlClose
            | ShutdownSignal::CtrlShutdown => 130,
        }
    }

    /// Human-readable label for the shutdown notice printed to stderr.
    fn label(self) -> &'static str {
        match self {
            ShutdownSignal::Interrupt => "SIGINT",
            ShutdownSignal::Terminate => "SIGTERM",
            ShutdownSignal::Hangup => "SIGHUP",
            #[cfg(windows)]
            ShutdownSignal::CtrlC => "Ctrl+C",
            #[cfg(windows)]
            ShutdownSignal::CtrlBreak => "Ctrl+Break",
            #[cfg(windows)]
            ShutdownSignal::CtrlClose => "console close",
            #[cfg(windows)]
            ShutdownSignal::CtrlShutdown => "system shutdown",
        }
    }
}

/// Process-lifetime signal monitor. The registered streams live on a detached
/// task so Tokio's process-wide signal disposition always has an active
/// receiver, including during synchronous app construction and worktree cleanup.
///
/// The task backing this struct is intentionally never told about normal
/// (no-signal) completion: it stays blocked waiting for a first real signal
/// for as long as the process lives, which is exactly what lets a signal
/// arriving after `run_with_shutdown` returns -- e.g. during the caller's
/// synchronous worktree cleanup in `run_cli` -- still force an exit. That
/// happens because `ShutdownMonitor` (and its `receiver` half of the mpsc
/// channel below) is dropped when `run_with_shutdown`/`run_agent` return; a
/// subsequent signal then fails to forward (`sender.send` errors on a
/// dropped receiver) and `force_exit_after_normal_completion` runs. In
/// ordinary operation this leaked task and its live signal registrations are
/// reaped by the unconditional `std::process::exit` in `run_cli`, not by any
/// cooperative shutdown of the task itself.
pub(super) struct ShutdownMonitor {
    receiver: mpsc::UnboundedReceiver<ShutdownEvent>,
    #[cfg(windows)]
    close_cleanup: windows_close::CleanupSignal,
}

struct ShutdownEvent {
    signal: ShutdownSignal,
    acknowledged: oneshot::Sender<()>,
}

impl ShutdownMonitor {
    /// Register signal streams synchronously before terminal initialization.
    #[cfg(unix)]
    pub(super) fn register() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let (sender, receiver) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let first = tokio::select! {
                biased;
                _ = terminate.recv() => ShutdownSignal::Terminate,
                _ = interrupt.recv() => ShutdownSignal::Interrupt,
                _ = hangup.recv() => ShutdownSignal::Hangup,
            };
            let (acknowledged, acknowledgement) = oneshot::channel();
            if let Err(error) = sender.send(ShutdownEvent {
                signal: first,
                acknowledged,
            }) {
                force_exit_after_normal_completion(error.0.signal);
            }

            tokio::select! {
                biased;
                ack = acknowledgement => {
                    if ack.is_err() {
                        // The `ShutdownEvent` (and its `acknowledged`
                        // sender) was dropped without ever being handled:
                        // `run_with_shutdown` already resolved via its
                        // normal-completion branch and returned, dropping
                        // `shutdown`'s receiver, before this event was
                        // consumed. Treat it exactly like a signal arriving
                        // after normal completion (which is genuinely what
                        // happened -- this signal was never actually acted
                        // on) instead of silently falling through to "wait
                        // for a second signal", which would otherwise make
                        // this first signal a no-op and require an
                        // impatient operator to send a second one to force
                        // an exit through e.g. a wedged
                        // `WorktreeSession::finish()`.
                        force_exit_after_normal_completion(first);
                    }
                }
                _ = terminate.recv() => {
                    force_exit_during_shutdown(ShutdownSignal::Terminate);
                }
                _ = interrupt.recv() => {
                    force_exit_during_shutdown(ShutdownSignal::Interrupt);
                }
                _ = hangup.recv() => {
                    force_exit_during_shutdown(ShutdownSignal::Hangup);
                }
            }
            let second = tokio::select! {
                biased;
                _ = terminate.recv() => ShutdownSignal::Terminate,
                _ = interrupt.recv() => ShutdownSignal::Interrupt,
                _ = hangup.recv() => ShutdownSignal::Hangup,
            };
            force_exit_during_shutdown(second);
        });

        Ok(Self { receiver })
    }

    /// Register Windows console-event streams before terminal initialization.
    #[cfg(windows)]
    pub(super) fn register() -> std::io::Result<Self> {
        use tokio::signal::windows::{ctrl_break, ctrl_c};

        let mut ctrl_c = ctrl_c()?;
        let mut ctrl_break = ctrl_break()?;
        let (sender, receiver) = mpsc::unbounded_channel();
        let close_cleanup = windows_close::register(sender.clone())?;

        tokio::spawn(async move {
            let first = tokio::select! {
                biased;
                _ = ctrl_c.recv() => ShutdownSignal::CtrlC,
                _ = ctrl_break.recv() => ShutdownSignal::CtrlBreak,
            };
            let (acknowledged, acknowledgement) = oneshot::channel();
            if let Err(error) = sender.send(ShutdownEvent {
                signal: first,
                acknowledged,
            }) {
                force_exit_after_normal_completion(error.0.signal);
            }

            tokio::select! {
                biased;
                ack = acknowledgement => {
                    // See the identical comment in the Unix `register()`
                    // above: an `Err` here means this event was dropped
                    // unhandled, not genuinely acknowledged.
                    if ack.is_err() {
                        force_exit_after_normal_completion(first);
                    }
                }
                _ = ctrl_c.recv() => {
                    force_exit_during_shutdown(ShutdownSignal::CtrlC);
                }
                _ = ctrl_break.recv() => {
                    force_exit_during_shutdown(ShutdownSignal::CtrlBreak);
                }
            }
            let second = tokio::select! {
                biased;
                _ = ctrl_c.recv() => ShutdownSignal::CtrlC,
                _ = ctrl_break.recv() => ShutdownSignal::CtrlBreak,
            };
            force_exit_during_shutdown(second);
        });

        Ok(Self {
            receiver,
            close_cleanup,
        })
    }

    pub(super) async fn recv(&mut self) -> ShutdownSignal {
        // The monitor task backing `sender` never returns: every path
        // through it ends in `force_exit_during_shutdown`/
        // `force_exit_after_normal_completion` (both `-> !`, i.e.
        // `std::process::exit`), or it blocks forever waiting for the next
        // signal. So `sender` is dropped only if that task panics, in which
        // case this process's entire signal-handling story is already
        // broken and panicking here (surfacing loudly) is preferable to
        // silently never observing another shutdown signal.
        let event = self
            .receiver
            .recv()
            .await
            .expect("registered shutdown monitor task ended unexpectedly");
        let _ = event.acknowledged.send(());
        event.signal
    }

    #[cfg(windows)]
    pub(super) fn complete_platform_cleanup(&self) {
        self.close_cleanup.complete();
    }

    #[cfg(not(windows))]
    pub(super) fn complete_platform_cleanup(&self) {}
}

/// Windows closes a console process after `CTRL_CLOSE_EVENT` /
/// `CTRL_SHUTDOWN_EVENT` handlers return. Tokio's async streams acknowledge
/// those callbacks immediately, which lets the OS terminate the process before
/// App/session/process cleanup completes. Keep a native handler blocked on a
/// Win32 event while a dedicated bridge thread forwards the event into the
/// ordinary async shutdown path; cleanup releases the handler explicitly.
#[cfg(windows)]
mod windows_close {
    use super::{ShutdownEvent, ShutdownSignal};
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use tokio::sync::{mpsc, oneshot};
    use windows_sys::Win32::System::Console::{
        CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, INFINITE, SetEvent, WaitForSingleObject,
    };

    const NO_EVENT: u32 = u32::MAX;
    // Windows normally gives console-close handlers only a short grace
    // period. Stay below that platform-owned ceiling so the callback returns
    // cleanly even if application cleanup itself wedges.
    const CLOSE_CLEANUP_TIMEOUT_MS: u32 = 4_000;

    static RECEIVED_EVENT: AtomicU32 = AtomicU32::new(NO_EVENT);
    static SIGNAL_EVENT: AtomicUsize = AtomicUsize::new(0);
    static CLEANUP_EVENT: AtomicUsize = AtomicUsize::new(0);

    pub(super) struct CleanupSignal;

    impl CleanupSignal {
        pub(super) fn complete(&self) {
            let event =
                CLEANUP_EVENT.load(Ordering::Acquire) as windows_sys::Win32::Foundation::HANDLE;
            if !event.is_null() {
                // SAFETY: `register` creates this process-lifetime event and
                // never closes it while the console handler may still run.
                unsafe {
                    SetEvent(event);
                }
            }
        }
    }

    pub(super) fn register(
        sender: mpsc::UnboundedSender<ShutdownEvent>,
    ) -> std::io::Result<CleanupSignal> {
        if SIGNAL_EVENT.load(Ordering::Acquire) != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "Windows console-close handler is already registered",
            ));
        }

        // SAFETY: null security/name pointers request unnamed process-local
        // events. The signal event auto-resets; cleanup remains signaled once
        // orderly teardown has completed.
        let signal_event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if signal_event.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let cleanup_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if cleanup_event.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        SIGNAL_EVENT.store(signal_event as usize, Ordering::Release);
        CLEANUP_EVENT.store(cleanup_event as usize, Ordering::Release);

        let signal_event_address = signal_event as usize;
        std::thread::Builder::new()
            .name("maestro-console-close".to_string())
            .spawn(move || {
                loop {
                    let signal_event =
                        signal_event_address as windows_sys::Win32::Foundation::HANDLE;
                    // SAFETY: the process-lifetime event remains valid until the
                    // process exits.
                    unsafe {
                        WaitForSingleObject(signal_event, INFINITE);
                    }
                    let signal = match RECEIVED_EVENT.swap(NO_EVENT, Ordering::AcqRel) {
                        CTRL_CLOSE_EVENT => ShutdownSignal::CtrlClose,
                        CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => ShutdownSignal::CtrlShutdown,
                        _ => continue,
                    };
                    let (acknowledged, _acknowledgement) = oneshot::channel();
                    if sender
                        .send(ShutdownEvent {
                            signal,
                            acknowledged,
                        })
                        .is_err()
                    {
                        force_exit(signal);
                    }
                }
            })
            .map_err(|error| {
                std::io::Error::other(format!(
                    "failed to spawn Windows console-close bridge: {error}"
                ))
            })?;

        // SAFETY: the handler uses only atomics and process-lifetime Win32
        // event handles. Registering it last ensures it receives close and
        // shutdown events before Tokio's immediate-return handler.
        if unsafe { SetConsoleCtrlHandler(Some(console_close_handler), 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(CleanupSignal)
    }

    unsafe extern "system" fn console_close_handler(event: u32) -> i32 {
        if !matches!(
            event,
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
        ) {
            return 0;
        }

        RECEIVED_EVENT.store(event, Ordering::Release);
        let signal_event =
            SIGNAL_EVENT.load(Ordering::Acquire) as windows_sys::Win32::Foundation::HANDLE;
        let cleanup_event =
            CLEANUP_EVENT.load(Ordering::Acquire) as windows_sys::Win32::Foundation::HANDLE;
        if signal_event.is_null() || cleanup_event.is_null() {
            return 0;
        }

        // SAFETY: both handles are valid process-lifetime events. Blocking
        // here is required: Windows is allowed to terminate the process as
        // soon as this callback returns.
        if unsafe { SetEvent(signal_event) } == 0 {
            return 0;
        }
        unsafe {
            WaitForSingleObject(cleanup_event, CLOSE_CLEANUP_TIMEOUT_MS);
        }
        1
    }

    fn force_exit(signal: ShutdownSignal) -> ! {
        std::process::exit(signal.exit_code());
    }
}

/// Run synchronous app construction off the async worker while continuing to
/// receive shutdown signals. The constructor cannot be cancelled once a
/// blocking worker starts it, so a signal returns immediately to the caller
/// instead of waiting on it -- but it hands back the still-running
/// [`tokio::task::JoinHandle`] rather than abandoning it, because the
/// constructor (`App::new_with_initial_prompt` -> `terminal::init`) performs
/// synchronous, unsynchronized terminal setup (raw mode, bracketed paste,
/// mouse capture) *before* publishing the global TTY handle terminal
/// restore reads. A caller that calls `terminal::restore()` without first
/// awaiting this handle races that setup: the abandoned thread can
/// re-enable modes after the restore call returns, and nothing runs a
/// second restore afterward before the process exits. Callers must await
/// the returned handle before restoring the terminal; a wedged constructor
/// is still bounded by `ShutdownMonitor`'s second-signal force-exit
/// watchdog, which is unaffected by this await since it runs on a
/// different, already-detached task.
pub(super) async fn construct_while_monitoring<T, F>(
    shutdown: &mut ShutdownMonitor,
    constructor: F,
) -> anyhow::Result<Result<T, (ShutdownSignal, tokio::task::JoinHandle<anyhow::Result<T>>)>>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let mut construction = tokio::task::spawn_blocking(constructor);
    tokio::select! {
        result = &mut construction => {
            let constructed = result
                .map_err(|error| anyhow::anyhow!("interactive app construction task failed: {error}"))??;
            Ok(Ok(constructed))
        }
        signal = shutdown.recv() => Ok(Err((signal, construction))),
    }
}

/// Force an immediate exit with no I/O of any kind first.
///
/// Deliberately does *not* `eprintln!` a diagnostic before exiting, unlike
/// the rest of this module: `eprintln!` takes stderr's process-global lock
/// and can block on a real write syscall (a full pipe, a wedged terminal,
/// ...). This function exists specifically as the unconditional last-resort
/// escape hatch for an impatient operator's repeated signal -- if it can
/// block on I/O, it is not actually unconditional, defeating the one thing
/// it is for. Diagnostics for the *first* signal (the common, non-escape-
/// hatch path) still run in `run_with_shutdown`.
fn force_exit_after_normal_completion(signal: ShutdownSignal) -> ! {
    std::process::exit(signal.exit_code());
}

/// See [`force_exit_after_normal_completion`]: same reasoning, no logging
/// before the unconditional exit.
fn force_exit_during_shutdown(signal: ShutdownSignal) -> ! {
    std::process::exit(signal.exit_code());
}

/// Run the interactive app's main loop, racing it against an externally
/// delivered shutdown signal so the loop is torn down in an orderly way
/// instead of hitting the OS default disposition. See the module docs for
/// the full design.
///
/// On a normal return, this behaves exactly like `app.run().await` -- the
/// caller's own `cleanup_background_processes()` safety-net call and
/// exit-code plumbing in `entrypoint::run_agent` still run unchanged.
pub(super) async fn run_with_shutdown(
    mut app: crate::App,
    mut shutdown: ShutdownMonitor,
) -> anyhow::Result<i32> {
    // Box and pin the run future so the signal branch controls *when* it is
    // dropped: with `app.run()` inline, `select!` would drop it before the
    // winning branch body runs, blocking in `SessionWriter::drop` before
    // the second-signal escape hatch below is armed.
    let mut run = Box::pin(app.run());
    tokio::select! {
        biased;

        signal = shutdown.recv() => {
            // Deliberately not `eprintln!`'d here: this arm runs on the
            // async worker, and `eprintln!` takes stderr's process-global
            // lock and performs a real (possibly blocking) write syscall --
            // on a full pipe or wedged terminal that would stall this
            // worker before `drop(run)` below even releases `app`, with
            // nothing left on a single-worker runtime to poll the repeat-
            // signal monitor this escape hatch depends on. Logged instead
            // from inside the `spawn_blocking` block below, alongside the
            // other diagnostics already moved there for the same reason.
            let signal_label = signal.label();

            // Dropping the pinned run future cancels the interactive loop
            // mid-flight (see "Cancellation safety" in the module docs) and
            // releases its `&mut app` borrow so the teardown below can run.
            drop(run);

            // Cancel in-flight tool executions and await them (the async
            // part, which must stay on this worker) and get back the
            // terminal notifier-teardown sequences (tab progress, saved
            // title) to write -- but not written yet; see below.
            let (shutdown_seqs, disable_theme_reporting) =
                app.signal_shutdown_teardown().await;

            // The remaining teardown blocks synchronously: `drop(app)`
            // cascades into `SessionWriter::drop` -> flush,
            // `cleanup_background_processes` kills and reaps children,
            // `write_terminal_sequences` and `terminal::restore` both write
            // to the tty. Run all of it on a blocking thread so it cannot
            // starve the escape hatch above (or any other task) on this
            // worker -- including `write_terminal_sequences`, which takes
            // the global TTY mutex and can block on a real (possibly
            // wedged) terminal write; on a single-worker runtime (the
            // Tokio default on a one-vCPU machine) that would otherwise
            // starve the very monitor task this escape hatch depends on.
            tokio::task::spawn_blocking(move || {
                // Flush session state and reap background processes before
                // any terminal/stderr write that can block on backpressure.
                drop(app);
                let killed = cleanup_background_processes();

                eprintln!("[shutdown] received {signal_label}, shutting down...");
                if disable_theme_reporting {
                    let _ = crate::terminal::disable_theme_reporting();
                }
                crate::App::write_terminal_sequences(&shutdown_seqs);
                if killed > 0 {
                    eprintln!("[shutdown] cleaned up {killed} background process(es)");
                }

                if let Err(error) = crate::terminal::restore() {
                    eprintln!("[shutdown] failed to restore terminal: {error}");
                }
            })
            .await
            .map_err(|error| anyhow::anyhow!("shutdown cleanup task failed: {error}"))?;

            shutdown.complete_platform_cleanup();
            Ok(signal.exit_code())
        }

        // Normal completion: `run` only borrowed `app`, and `run_inner`'s
        // own exit path (notifier teardown, terminal restore via `run`)
        // already ran, so the app is simply dropped on return -- flushing
        // the session writer exactly as it did before this wrapper existed.
        // `shutdown` is dropped along with this function's locals on
        // return; see `ShutdownMonitor`'s doc comment for why leaving its
        // background task running (rather than telling it about this
        // normal completion) is what lets a signal arriving during the
        // caller's subsequent worktree cleanup still force an exit.
        result = &mut run => {
            drop(run);
            let code = result?;
            let target = app.resume_target.take();
            if target.is_some() {
                app.stop_agent_for_resume().await;
            }
            tokio::task::spawn_blocking(move || drop(app)).await?;
            if code == 0 {
                if let Some((cwd, session_id)) = target {
                    let executable = std::env::current_exe()?;
                    #[cfg(unix)]
                    {
                        // Replace the process so the old terminal input reader
                        // cannot compete with the resumed TUI for keystrokes.
                        use std::os::unix::process::CommandExt;
                        let error = tokio::task::spawn_blocking(move || {
                            std::process::Command::new(executable)
                                .current_dir(cwd)
                                .arg("--resume-session")
                                .arg(session_id)
                                .exec()
                        }).await?;
                        return Err(error.into());
                    }
                    #[cfg(not(unix))]
                    {
                        let status = tokio::process::Command::new(executable)
                            .current_dir(cwd)
                            .arg("--resume-session")
                            .arg(session_id)
                            .status()
                            .await?;
                        return Ok(status.code().unwrap_or(1));
                    }
                }
            }
            Ok(code)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    static SIGNAL_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn exit_codes_follow_128_plus_signo() {
        assert_eq!(ShutdownSignal::Hangup.exit_code(), 129);
        assert_eq!(ShutdownSignal::Interrupt.exit_code(), 130);
        assert_eq!(ShutdownSignal::Terminate.exit_code(), 143);
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(ShutdownSignal::Interrupt.label(), "SIGINT");
        assert_eq!(ShutdownSignal::Terminate.label(), "SIGTERM");
        assert_eq!(ShutdownSignal::Hangup.label(), "SIGHUP");
    }

    /// Exercises real OS signal delivery end to end: self-sends a `SIGTERM`
    /// with `libc::kill` and asserts the pre-registered monitor actually
    /// resolves via `tokio::signal::unix`'s registration, not just that the
    /// pure `exit_code`/`label` mappings are correct.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_monitor_resolves_on_real_sigterm() {
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let mut monitor = ShutdownMonitor::register().expect("register shutdown monitor");

        // SAFETY: signaling our own process with a handled, non-fatal
        // signal whose streams were synchronously registered above.
        let pid = std::process::id() as i32;
        let result = unsafe { libc::kill(pid, libc::SIGTERM) };
        assert_eq!(
            result,
            0,
            "self-kill(SIGTERM) failed: {}",
            std::io::Error::last_os_error()
        );

        let signal = tokio::time::timeout(std::time::Duration::from_secs(5), monitor.recv())
            .await
            .expect("shutdown monitor did not resolve within 5s of a real SIGTERM");

        assert_eq!(signal, ShutdownSignal::Terminate);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blocking_constructor_does_not_starve_first_signal() {
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let mut monitor = ShutdownMonitor::register().expect("register shutdown monitor");
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            // SAFETY: the monitor registered a Tokio SIGTERM handler above.
            assert_eq!(
                unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) },
                0
            );
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            construct_while_monitoring(&mut monitor, move || {
                release_rx.recv().expect("release blocked constructor");
                Ok(())
            }),
        )
        .await
        .expect("signal handling was starved by blocking construction")
        .expect("construction race should not fail");

        let (signal, construction) = result.expect_err("signal should win the construction race");
        assert_eq!(signal, ShutdownSignal::Terminate);

        // The abandoned constructor is still running; releasing it and
        // awaiting the handle exercises exactly what a real caller must do
        // before calling `terminal::restore()` (see the function's doc
        // comment) -- confirms the handle is live and actually completes.
        release_tx.send(()).expect("release constructor worker");
        tokio::time::timeout(std::time::Duration::from_secs(5), construction)
            .await
            .expect("abandoned constructor did not finish after being released")
            .expect("abandoned constructor task should not panic")
            .expect("abandoned constructor should return Ok(())");
    }

    /// A repeated signal must remain an escape hatch even before the first
    /// signal is acknowledged, which is the state the monitor remains in
    /// while synchronous App construction is still running.
    #[cfg(unix)]
    #[test]
    fn shutdown_monitor_forces_exit_on_repeat_before_acknowledgement() {
        const CHILD_ENV: &str = "MAESTRO_TEST_REPEAT_SIGNAL_BEFORE_ACK";

        if std::env::var_os(CHILD_ENV).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build signal-test runtime");
            runtime.block_on(async {
                let _monitor = ShutdownMonitor::register().expect("register shutdown monitor");
                let pid = std::process::id() as i32;

                // SAFETY: both signals have registered Tokio handlers. The
                // first is intentionally never acknowledged; the second must
                // make the detached monitor call force_exit_during_shutdown.
                assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
                std::future::pending::<()>().await;
            });
            unreachable!("repeat signal should force process exit");
        }

        let test_name = "entrypoint::shutdown_signal::tests::shutdown_monitor_forces_exit_on_repeat_before_acknowledgement";
        let mut child = std::process::Command::new(
            std::env::current_exe().expect("resolve current test executable"),
        )
        .arg(test_name)
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .spawn()
        .expect("spawn repeat-signal test child");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll repeat-signal test child") {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().expect("kill wedged repeat-signal test child");
                panic!("repeat signal was not observed before acknowledgement");
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };

        assert_eq!(status.code(), Some(ShutdownSignal::Terminate.exit_code()));
    }

    /// Covers the escape hatch described on [`ShutdownMonitor`]: once the
    /// monitor (and its `receiver` half of the mpsc channel) is dropped --
    /// exactly what happens when `run_with_shutdown`/`run_agent` return
    /// after a completely normal exit, i.e. no signal was ever received --
    /// a signal arriving afterward (e.g. during the caller's synchronous
    /// worktree cleanup) must still force an exit rather than being
    /// silently lost. Runs in a child process because the escape hatch
    /// itself calls `std::process::exit`.
    #[cfg(unix)]
    #[test]
    fn shutdown_monitor_forces_exit_on_signal_after_drop() {
        const CHILD_ENV: &str = "MAESTRO_TEST_SIGNAL_AFTER_MONITOR_DROP";

        if std::env::var_os(CHILD_ENV).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build signal-test runtime");
            runtime.block_on(async {
                let monitor = ShutdownMonitor::register().expect("register shutdown monitor");
                let pid = std::process::id() as i32;

                // Simulate `run_with_shutdown` returning normally: the
                // monitor (and its mpsc receiver) is dropped without ever
                // observing a signal, while its detached task keeps running.
                drop(monitor);
                // Give the background task's `tokio::spawn` a chance to be
                // scheduled so the registered signal streams are live and
                // it is parked waiting on them before we signal.
                tokio::task::yield_now().await;

                // SAFETY: signaling our own process with a handled,
                // non-fatal signal whose streams were registered above,
                // before the monitor (but not its background task) was
                // dropped.
                assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
                std::future::pending::<()>().await;
            });
            unreachable!("a signal after the monitor is dropped should force process exit");
        }

        let test_name =
            "entrypoint::shutdown_signal::tests::shutdown_monitor_forces_exit_on_signal_after_drop";
        let mut child = std::process::Command::new(
            std::env::current_exe().expect("resolve current test executable"),
        )
        .arg(test_name)
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .spawn()
        .expect("spawn signal-after-drop test child");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll signal-after-drop test child") {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child
                    .kill()
                    .expect("kill wedged signal-after-drop test child");
                panic!("signal after monitor drop was not observed");
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };

        assert_eq!(status.code(), Some(ShutdownSignal::Terminate.exit_code()));
    }

    /// End-to-end rehearsal of the real shutdown sequence without
    /// constructing a full `App` (out of scope to touch `app.rs` here):
    /// a stand-in "interactive loop" future owns a real `SessionManager`
    /// with buffered, unflushed entries, and a real child process is
    /// registered in the *actual* global `process_registry`. A real
    /// `SIGTERM` is self-sent; this asserts the buffered entry lands on
    /// disk (via the cancellation drop cascade) and its Child owner reaps it
    /// -- the two failure modes this regression protects.
    ///
    /// The registry is process-global and tests run in parallel, so the
    /// termination step uses `process_registry::cleanup_one(child_pid)`
    /// (the per-PID variant of the production `cleanup_all` path) rather
    /// than draining the whole registry: a full drain could kill another
    /// concurrently-running test's registered child and make that test's
    /// assertions fail nondeterministically.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_signal_flushes_session_and_reaps_background_process() {
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let mut monitor = ShutdownMonitor::register().expect("register shutdown monitor");
        let dir = tempfile::TempDir::new().expect("tempdir");
        let mut manager = crate::session::SessionManager::with_sessions_dir(
            "/tmp/shutdown-signal-test",
            dir.path(),
        );
        manager
            .start_session(crate::session::SessionHeader {
                version: Some(2),
                id: "shutdown-signal-test".to_string(),
                timestamp: "2024-01-15T10:30:00Z".to_string(),
                cwd: "/tmp/shutdown-signal-test".to_string(),
                model: "test-model".to_string(),
                subject: None,
                model_metadata: None,
                thinking_level: Default::default(),
                system_prompt: None,
                prompt_metadata: None,
                prompt_context_manifest: None,
                unified_context_manifest: None,
                tools: vec![],
                branched_from: None,
                parent_session: None,
            })
            .expect("start_session");
        // Well under `SessionWriter`'s batch size (25): nothing auto-flushes,
        // so the only way this reaches disk is the drop-on-cancel cascade.
        manager
            .save_attachment_extract("att-1", "unflushed turn that must survive SIGTERM")
            .expect("write buffered entry");
        let session_path = manager
            .current_session_path()
            .expect("session path after start_session");

        // A real background child, registered exactly the way the bash
        // tool's `run_in_background` does.
        let mut child = tokio::process::Command::new("sleep")
            .arg("300")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn background child");
        let child_pid = child.id().expect("PID of newly spawned background child");
        crate::tools::process_registry::register(child_pid);

        /// Stands in for `App::run(self)`: owns the manager and suspends
        /// forever, the same way the real interactive loop blocks on
        /// terminal/network I/O until it is cancelled.
        async fn fake_interactive_loop(_manager: crate::session::SessionManager) -> ! {
            std::future::pending().await
        }

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            // SAFETY: signaling our own process with a handled, non-fatal
            // signal that the monitor above registered before any app state.
            let result = unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
            assert_eq!(result, 0, "self-kill(SIGTERM) failed");
        });

        let shutdown_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let terminated = tokio::time::timeout_at(shutdown_deadline, async {
            tokio::select! {
                biased;
                signal = monitor.recv() => {
                    assert_eq!(signal, ShutdownSignal::Terminate);
                    crate::tools::process_registry::cleanup_one(child_pid)
                }
                () = fake_interactive_loop(manager) => unreachable!("stand-in loop never completes"),
            }
        })
        .await
        .expect("shutdown race did not resolve within 5s of a real SIGTERM");

        assert!(
            terminated,
            "expected the registered child to be cleaned up by cleanup_one"
        );

        // The registry requests termination; the Child owner must await and
        // reap it, as the production Bash watcher does. A fixed sleep followed
        // by try_wait races process exit under concurrent workspace load.
        let status = tokio::time::timeout_at(shutdown_deadline, child.wait())
            .await
            .expect("background child was not reaped within the shutdown deadline")
            .expect("wait for background child");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "cleanup_one must terminate the registered background child"
        );

        // The buffered entry must be on disk: this only happens because
        // `fake_interactive_loop`'s future (which owned `manager`) was
        // dropped by `select!` when the signal branch won, cascading into
        // `SessionWriter::drop` -> `flush`.
        let contents = std::fs::read_to_string(&session_path).expect("read session file");
        assert!(
            contents.contains("unflushed turn that must survive SIGTERM"),
            "buffered session entry was not flushed to {}:\n{contents}",
            session_path.display()
        );
    }
}
