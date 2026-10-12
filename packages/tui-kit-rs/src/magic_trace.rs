//! Jane Street [magic-trace](https://github.com/janestreet/magic-trace) integration.
//!
//! magic-trace uses Intel Processor Trace via Linux `perf` to capture a ~10ms
//! ring buffer of **all control flow** leading up to a snapshot. It is the
//! right tool for "what did the TUI actually do in this 70ns–10ms window?"
//!
//! # Platform requirements
//!
//! - **Linux only** (uses `perf`)
//! - **Intel Skylake+** with Intel PT (`grep intel_pt /proc/cpuinfo`)
//! - Bare metal preferred (most VMs do not expose Intel PT)
//! - Unstripped binary with symbols (use Cargo profile `magic-trace`)
//!
//! This macOS/ARM host cannot run magic-trace natively. Capture on a Linux
//! Intel bare-metal host with:
//!
//! ```bash
//! scripts/magic-trace-tui.sh run
//! # or attach to a running process:
//! scripts/magic-trace-tui.sh attach
//! ```
//!
//! Open the resulting `trace.fxt.gz` at <https://magic-trace.org/>.
//!
//! # Stop indicator
//!
//! The default magic-trace trigger symbol is `magic_trace_stop_indicator`.
//! Call [`stop_indicator`] (or the C ABI export) from hot paths, slow-frame
//! detection, or `/magic-trace` so a running attach can snapshot.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Default magic-trace stop symbol (`-trigger .` / `magic_trace_stop_indicator`).
///
/// Must not be inlined. Cost is ~10µs **only** when magic-trace uses it to
/// take a snapshot; otherwise it is a cheap empty call.
#[inline(never)]
#[no_mangle]
pub extern "C" fn magic_trace_stop_indicator() {
    // Black-box so the empty body is not optimized away.
    std::hint::black_box(());
}

/// Rust-callable alias of [`magic_trace_stop_indicator`].
#[inline(never)]
pub fn stop_indicator() {
    magic_trace_stop_indicator();
}

/// When true, slow frames call the stop indicator once past the threshold.
static SLOW_FRAME_TRIGGER: AtomicBool = AtomicBool::new(false);

/// Frame budget in microseconds for slow-frame snapshots (default 16_000 ≈ 60fps).
static SLOW_FRAME_BUDGET_US: AtomicU64 = AtomicU64::new(16_000);

/// Whether the stop indicator has already fired this process (rate-limit to one
/// auto snapshot unless explicitly re-armed).
static AUTO_FIRED: AtomicBool = AtomicBool::new(false);

/// Enable slow-frame magic-trace snapshots from env.
///
/// - `MAESTRO_MAGIC_TRACE_SLOW_FRAME=1` — enable
/// - `MAESTRO_MAGIC_TRACE_FRAME_BUDGET_MS=16` — optional budget (ms)
#[must_use]
pub fn init_from_env() -> bool {
    let enabled = matches!(
        std::env::var("MAESTRO_MAGIC_TRACE_SLOW_FRAME").as_deref(),
        Ok("1" | "true" | "yes")
    );
    SLOW_FRAME_TRIGGER.store(enabled, Ordering::Relaxed);
    if let Ok(ms) = std::env::var("MAESTRO_MAGIC_TRACE_FRAME_BUDGET_MS") {
        if let Ok(v) = ms.parse::<u64>() {
            SLOW_FRAME_BUDGET_US.store(v.saturating_mul(1000).max(1), Ordering::Relaxed);
        }
    }
    enabled
}

/// Enable or disable slow-frame auto snapshots at runtime.
pub fn set_slow_frame_trigger(enabled: bool) {
    SLOW_FRAME_TRIGGER.store(enabled, Ordering::Relaxed);
    if enabled {
        AUTO_FIRED.store(false, Ordering::Relaxed);
    }
}

/// Whether slow-frame auto snapshots are enabled.
#[must_use]
pub fn slow_frame_trigger_enabled() -> bool {
    SLOW_FRAME_TRIGGER.load(Ordering::Relaxed)
}

/// Call after a frame render. If duration exceeds the budget and auto-trigger
/// is enabled, fire the stop indicator once.
pub fn maybe_stop_on_slow_frame(elapsed: Duration) {
    if !SLOW_FRAME_TRIGGER.load(Ordering::Relaxed) {
        return;
    }
    let budget = Duration::from_micros(SLOW_FRAME_BUDGET_US.load(Ordering::Relaxed));
    if elapsed <= budget {
        return;
    }
    if AUTO_FIRED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        stop_indicator();
    }
}

/// Measure a render and optionally fire the stop indicator.
pub fn time_render<R>(f: impl FnOnce() -> R) -> (R, Duration) {
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    maybe_stop_on_slow_frame(elapsed);
    (out, elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_indicator_is_callable() {
        stop_indicator();
        magic_trace_stop_indicator();
    }

    #[test]
    fn slow_frame_fires_once() {
        AUTO_FIRED.store(false, Ordering::Relaxed);
        set_slow_frame_trigger(true);
        SLOW_FRAME_BUDGET_US.store(1, Ordering::Relaxed); // 1µs budget
        maybe_stop_on_slow_frame(Duration::from_millis(5));
        assert!(AUTO_FIRED.load(Ordering::Relaxed));
        // second call should not panic / re-fire
        maybe_stop_on_slow_frame(Duration::from_millis(5));
        set_slow_frame_trigger(false);
    }
}
