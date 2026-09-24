//! Scheduling for the interactive terminal loop.
//!
//! A quiescent TUI blocks until terminal input, a producer signal, or a slow
//! safety tick. Busy frames, queued agent output, and timer-driven UI keep
//! the previous short poll.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, mpsc, oneshot};

/// Safety drain when no terminal input and no producer signal arrives.
/// Matches the hosted runner's notified maintenance interval.
pub(crate) const IDLE_SAFETY_TICK: Duration = Duration::from_secs(5);
pub(crate) const SHORT_MAINTENANCE_POLL: Duration = Duration::from_millis(100);
pub(crate) const BUSY_FRAME_POLL: Duration = Duration::from_millis(33);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TerminalPollInput {
    pub(crate) agent_activity: bool,
    pub(crate) busy: bool,
    pub(crate) pending_redraw: bool,
    /// Animation, an outstanding theme query, a debounce, or a live source
    /// that cannot signal the loop itself.
    pub(crate) short_cadence: bool,
}

impl TerminalPollInput {
    #[cfg(test)]
    pub(crate) fn idle() -> Self {
        Self {
            agent_activity: false,
            busy: false,
            pending_redraw: false,
            short_cadence: false,
        }
    }
}

pub(crate) fn terminal_poll_timeout(input: TerminalPollInput) -> Duration {
    // Busy frames stay at 33 ms even when a redraw is already queued.
    // Agent output still skips the wait, matching the previous contract.
    if input.agent_activity {
        Duration::ZERO
    } else if input.busy {
        BUSY_FRAME_POLL
    } else if input.pending_redraw {
        Duration::ZERO
    } else if input.short_cadence {
        SHORT_MAINTENANCE_POLL
    } else {
        IDLE_SAFETY_TICK
    }
}

/// Shared wake for every in-process producer the main loop drains.
///
/// `notify` wakes the crossterm `select`. `terminal_waker` interrupts a
/// blocking uncurses `EventSource::poll` from another thread.
#[derive(Clone)]
pub(crate) struct LoopWake {
    notify: Arc<Notify>,
    terminal_waker: Arc<Mutex<Option<uncurses::event::Waker>>>,
}

impl std::fmt::Debug for LoopWake {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("LoopWake").finish_non_exhaustive()
    }
}

impl LoopWake {
    pub(crate) fn new() -> Self {
        Self {
            notify: Arc::new(Notify::new()),
            terminal_waker: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn signal(&self) {
        self.notify.notify_one();
        if let Ok(guard) = self.terminal_waker.lock() {
            if let Some(waker) = guard.as_ref() {
                let _ = waker.wake();
            }
        }
    }

    pub(crate) fn install_terminal_waker(&self, waker: uncurses::event::Waker) {
        if let Ok(mut guard) = self.terminal_waker.lock() {
            *guard = Some(waker);
        }
    }

    fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoopWakeCause<T> {
    Terminal(T),
    Producer,
    Tick,
}

/// Wait for terminal input, a [`LoopWake`] signal, or `timeout`.
///
/// A zero timeout still prefers terminal input that is already queued, then
/// returns. That is the non-blocking poll the busy and agent-activity paths
/// already used.
pub(crate) async fn await_loop_wake<T>(
    timeout: Duration,
    wake: &LoopWake,
    tty: impl Future<Output = T>,
) -> LoopWakeCause<T> {
    tokio::pin!(tty);
    if timeout.is_zero() {
        tokio::select! {
            biased;
            value = &mut tty => return LoopWakeCause::Terminal(value),
            () = std::future::ready(()) => return LoopWakeCause::Tick,
        }
    }
    let notified = wake.notified();
    tokio::pin!(notified);
    tokio::select! {
        biased;
        value = &mut tty => LoopWakeCause::Terminal(value),
        () = &mut notified => LoopWakeCause::Producer,
        () = tokio::time::sleep(timeout) => LoopWakeCause::Tick,
    }
}

/// Forward a Tokio channel onto a receiver the loop can `try_recv`, and signal
/// the loop after each item. Runtime agent sends already wake this task.
pub(crate) fn forward_unbounded<T: Send + 'static>(
    mut rx: mpsc::UnboundedReceiver<T>,
    wake: LoopWake,
) -> mpsc::UnboundedReceiver<T> {
    let (tx, forwarded) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            if tx.send(item).is_err() {
                break;
            }
            wake.signal();
        }
        wake.signal();
    });
    forwarded
}

pub(crate) fn forward_oneshot<T: Send + 'static>(
    rx: oneshot::Receiver<T>,
    wake: LoopWake,
) -> oneshot::Receiver<T> {
    let (tx, forwarded) = oneshot::channel();
    tokio::spawn(async move {
        if let Ok(value) = rx.await {
            let _ = tx.send(value);
        }
        wake.signal();
    });
    forwarded
}

/// Counted stand-in for one idle main-loop wait. Tests drive it under
/// `tokio::time::pause`. The crossterm path uses [`await_loop_wake`] with the
/// same timeout decision.
#[cfg(test)]
pub(crate) async fn run_notified_terminal_pump(
    stop: tokio::sync::watch::Receiver<bool>,
    wake: &LoopWake,
    wakes: &std::sync::atomic::AtomicUsize,
    input: impl Fn() -> TerminalPollInput,
) {
    use std::sync::atomic::Ordering;

    loop {
        if *stop.borrow() {
            break;
        }
        let timeout = terminal_poll_timeout(input());
        let _cause = await_loop_wake(timeout, wake, std::future::pending::<()>()).await;
        if *stop.borrow() {
            break;
        }
        wakes.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    async fn pump_for(
        input: impl Fn() -> TerminalPollInput + Send + 'static,
    ) -> (
        LoopWake,
        std::sync::Arc<AtomicUsize>,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let wake = LoopWake::new();
        let wakes = std::sync::Arc::new(AtomicUsize::new(0));
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let pump_wake = wake.clone();
        let pump_wakes = std::sync::Arc::clone(&wakes);
        let pump = tokio::spawn(async move {
            run_notified_terminal_pump(stop_rx, &pump_wake, &pump_wakes, input).await;
        });
        tokio::task::yield_now().await;
        (wake, wakes, stop_tx, pump)
    }

    async fn stop_pump(
        wake: &LoopWake,
        stop_tx: tokio::sync::watch::Sender<bool>,
        pump: tokio::task::JoinHandle<()>,
    ) {
        let _ = stop_tx.send(true);
        wake.signal();
        pump.await.expect("terminal pump");
    }

    #[tokio::test(start_paused = true)]
    async fn notified_terminal_loop_has_a_bounded_idle_wake_rate() {
        let (wake, wakes, stop_tx, pump) = pump_for(TerminalPollInput::idle).await;
        let initial = wakes.load(Ordering::Relaxed);
        assert_eq!(
            initial, 0,
            "an idle loop must not wake before the first tick"
        );

        // Advance in small steps so Tokio cannot collapse a missed interval
        // into one callback. This measures the work a real idle minute schedules.
        for _ in 0..600 {
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
        }
        let idle_wakes = wakes.load(Ordering::Relaxed) - initial;
        assert!(
            idle_wakes <= 12,
            "idle terminal loop woke {idle_wakes} times in one minute"
        );

        wake.signal();
        tokio::task::yield_now().await;
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            initial + idle_wakes + 1,
            "a producer signal must wake the loop without waiting for the safety tick"
        );
        stop_pump(&wake, stop_tx, pump).await;
    }

    #[tokio::test(start_paused = true)]
    async fn short_cadence_keeps_the_100ms_maintenance_poll() {
        let (wake, wakes, stop_tx, pump) = pump_for(|| TerminalPollInput {
            short_cadence: true,
            ..TerminalPollInput::idle()
        })
        .await;
        let initial = wakes.load(Ordering::Relaxed);
        for _ in 0..10 {
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(wakes.load(Ordering::Relaxed), initial + 10);
        stop_pump(&wake, stop_tx, pump).await;
    }

    #[tokio::test(start_paused = true)]
    async fn busy_frames_keep_the_33ms_poll() {
        let (wake, wakes, stop_tx, pump) = pump_for(|| TerminalPollInput {
            busy: true,
            ..TerminalPollInput::idle()
        })
        .await;
        let initial = wakes.load(Ordering::Relaxed);
        for _ in 0..10 {
            tokio::time::advance(BUSY_FRAME_POLL).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(wakes.load(Ordering::Relaxed), initial + 10);
        stop_pump(&wake, stop_tx, pump).await;
    }

    #[tokio::test(start_paused = true)]
    async fn agent_activity_does_not_wait() {
        assert_eq!(
            terminal_poll_timeout(TerminalPollInput {
                agent_activity: true,
                ..TerminalPollInput::idle()
            }),
            Duration::ZERO
        );
        let wake = LoopWake::new();
        let cause = await_loop_wake(Duration::ZERO, &wake, std::future::pending::<()>()).await;
        assert_eq!(cause, LoopWakeCause::Tick);
        assert_eq!(tokio::time::Instant::now().elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn tty_event_wakes_the_idle_loop_before_the_safety_tick() {
        let wake = LoopWake::new();
        let (tx, rx) = oneshot::channel::<()>();
        let wait = tokio::spawn(async move {
            await_loop_wake(IDLE_SAFETY_TICK, &wake, async move { rx.await.ok() }).await
        });
        tokio::task::yield_now().await;
        tx.send(()).expect("tty signal");
        let cause = wait.await.expect("tty wait");
        assert_eq!(cause, LoopWakeCause::Terminal(Some(())));
    }

    #[tokio::test(start_paused = true)]
    async fn forwarded_channel_send_wakes_before_the_safety_tick() {
        let (wake, wakes, stop_tx, pump) = pump_for(TerminalPollInput::idle).await;
        let (tx, rx) = mpsc::unbounded_channel();
        let mut forwarded = forward_unbounded(rx, wake.clone());
        tokio::task::yield_now().await;
        let before = wakes.load(Ordering::Relaxed);
        tx.send(7_u8).expect("enqueue");
        for _ in 0..8 {
            tokio::task::yield_now().await;
            if wakes.load(Ordering::Relaxed) > before {
                break;
            }
        }
        assert!(
            wakes.load(Ordering::Relaxed) > before,
            "a channel send must wake the idle loop before the safety tick"
        );
        assert_eq!(forwarded.try_recv().expect("forwarded item"), 7);
        stop_pump(&wake, stop_tx, pump).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "manual one-minute process CPU sample; use --ignored --nocapture"]
    async fn notified_terminal_loop_idle_cpu_probe() {
        fn process_cpu() -> Duration {
            let mut clock = std::mem::MaybeUninit::<libc::timespec>::uninit();
            // SAFETY: clock points to writable timespec storage and the clock id
            // is the Linux process CPU clock.
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, clock.as_mut_ptr()) },
                0,
                "read process CPU clock"
            );
            // SAFETY: a successful clock_gettime initialized the whole timespec.
            let clock = unsafe { clock.assume_init() };
            Duration::new(clock.tv_sec as u64, clock.tv_nsec as u32)
        }

        let (wake, wakes, stop_tx, pump) = pump_for(TerminalPollInput::idle).await;
        let before_wakes = wakes.load(Ordering::Relaxed);
        let before_cpu = process_cpu();
        tokio::time::sleep(Duration::from_mins(1)).await;
        let cpu = process_cpu()
            .checked_sub(before_cpu)
            .expect("process CPU clock is monotonic");
        let idle_wakes = wakes.load(Ordering::Relaxed) - before_wakes;
        println!(
            "idle_60s_process_cpu_ms={} wake_count={idle_wakes}",
            cpu.as_millis()
        );
        stop_pump(&wake, stop_tx, pump).await;
    }
}
