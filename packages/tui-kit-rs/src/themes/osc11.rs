//! OSC 11 terminal background-color probing for the `auto` theme.
//!
//! The interactive app uses uncurses as its sole tty reader, so OSC 11 and
//! DEC light/dark replies arrive as typed events instead of being mistaken
//! for Alt-modified input by crossterm 0.28. [`AutoThemeFollower`] applies
//! hysteresis to those live readings. The bounded one-time probe remains as
//! a compatibility fallback when the controlling tty cannot be opened by
//! uncurses.

use std::io::Write;
use std::time::Duration;

/// OSC 11 query for the terminal's background color.
pub const OSC11_QUERY: &[u8] = b"\x1b]11;?\x1b\\";

/// Luminance above which the terminal reads as light.
pub const LIGHT_THRESHOLD: f64 = 0.6;
/// Luminance below which the terminal reads as dark.
pub const DARK_THRESHOLD: f64 = 0.4;
/// Consecutive readings past a threshold required to flip themes, so a
/// single transient sample never repaints the whole UI.
pub const REQUIRED_CONSECUTIVE_READINGS: u8 = 2;

/// How long the startup probe waits for a reply before giving up.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

/// Parse an OSC 11 reply (`ESC ] 11 ; rgb:RR/GG/BB` terminated by BEL or
/// `ESC \`) out of a byte buffer that may also contain unrelated input.
///
/// Terminals report 1–4 hex digits per component (e.g. `rgb:28/2c/34` or
/// `rgb:2828/2c2c/3434`); each is scaled to 0–255. Returns `None` when no
/// complete, well-formed reply is present.
#[must_use]
pub fn parse_osc11_reply(buf: &[u8]) -> Option<(u8, u8, u8)> {
    const PREFIX: &[u8] = b"\x1b]11;rgb:";
    let start = buf.windows(PREFIX.len()).position(|w| w == PREFIX)? + PREFIX.len();
    let rest = &buf[start..];

    // Read until BEL or ST (ESC \); reject anything malformed.
    let mut end = 0;
    loop {
        match rest.get(end) {
            Some(b'\x07') => break,
            Some(b'\x1b') if rest.get(end + 1) == Some(&b'\\') => break,
            Some(_) => end += 1,
            None => return None, // reply not fully received yet
        }
    }
    let payload = std::str::from_utf8(&rest[..end]).ok()?;

    let mut components = payload.split('/');
    let mut rgb = [0u8; 3];
    for slot in &mut rgb {
        let hex = components.next()?;
        if hex.is_empty() || hex.len() > 4 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        let max = (1u32 << (4 * hex.len())) - 1;
        *slot = (value * 255 / max) as u8;
    }
    if components.next().is_some() {
        return None; // extra components
    }
    Some((rgb[0], rgb[1], rgb[2]))
}

/// Relative luminance of an sRGB color (Rec. 709 coefficients), 0.0–1.0.
#[must_use]
pub fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    0.2126 * (f64::from(r) / 255.0)
        + 0.7152 * (f64::from(g) / 255.0)
        + 0.0722 * (f64::from(b) / 255.0)
}

/// Decide the theme for a single luminance sample, with a dead band between
/// [`DARK_THRESHOLD`] and [`LIGHT_THRESHOLD`] that keeps `current`.
#[must_use]
pub fn theme_for_luminance(luminance: f64, current: &'static str) -> &'static str {
    if luminance > LIGHT_THRESHOLD {
        "light"
    } else if luminance < DARK_THRESHOLD {
        "dark"
    } else {
        current
    }
}

/// Hysteresis state machine for following the terminal background over a
/// stream of luminance samples: a flip requires
/// [`REQUIRED_CONSECUTIVE_READINGS`] consecutive samples past the threshold.
///
/// Driven by the uncurses terminal event reader when live theme following is
/// enabled.
pub struct AutoThemeFollower {
    current: &'static str,
    pending: Option<&'static str>,
    consecutive: u8,
}

impl AutoThemeFollower {
    /// Start following from the given resolved theme (`"dark"` or `"light"`).
    #[must_use]
    pub fn new(initial: &'static str) -> Self {
        Self {
            current: initial,
            pending: None,
            consecutive: 0,
        }
    }

    /// The theme currently in effect.
    #[must_use]
    pub fn current(&self) -> &'static str {
        self.current
    }

    /// Feed one luminance sample. Returns the new theme when it flips.
    pub fn observe_luminance(&mut self, luminance: f64) -> Option<&'static str> {
        let proposed = theme_for_luminance(luminance, self.current);
        if proposed == self.current {
            self.pending = None;
            self.consecutive = 0;
            return None;
        }
        if self.pending == Some(proposed) {
            self.consecutive += 1;
        } else {
            self.pending = Some(proposed);
            self.consecutive = 1;
        }
        if self.consecutive >= REQUIRED_CONSECUTIVE_READINGS {
            self.current = proposed;
            self.pending = None;
            self.consecutive = 0;
            Some(proposed)
        } else {
            None
        }
    }
}

/// Query the terminal's background color once via OSC 11, waiting up to
/// `timeout` for the reply. Returns `None` on any failure or when the
/// terminal does not answer.
///
/// # Safety / concurrency
///
/// This opens its own `/dev/tty` handle and reads the reply from it. It must
/// only run *before* the crossterm event loop starts consuming input
/// (crossterm's reader is created lazily on the first `event::poll`); after
/// that, two readers on the same tty race and crossterm mangles the reply —
/// see the module docs.
#[cfg(unix)]
#[must_use]
pub fn probe_terminal_background(timeout: Duration) -> Option<(u8, u8, u8)> {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    tty.write_all(OSC11_QUERY).ok()?;
    tty.flush().ok()?;

    let fd = tty.as_raw_fd();
    let mut buf = [0u8; 256];
    let mut filled = 0usize;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
        // Raw fd is borrowed from `tty`, which outlives this call.
        let ready = unsafe { libc::poll(&raw mut pfd, 1, millis) };
        if ready <= 0 {
            return None; // timeout or error
        }
        let n = tty.read(&mut buf[filled..]).ok()?;
        if n == 0 {
            return None;
        }
        filled += n;
        if let Some(rgb) = parse_osc11_reply(&buf[..filled]) {
            return Some(rgb);
        }
        if filled == buf.len() {
            return None;
        }
    }
}

/// Query the terminal's background color once via OSC 11.
///
/// Always `None` on non-Unix platforms (no `/dev/tty`).
#[cfg(not(unix))]
#[must_use]
pub fn probe_terminal_background(_timeout: Duration) -> Option<(u8, u8, u8)> {
    None
}

/// Resolve the initial `auto` theme: `COLORFGBG` first, refined by a
/// one-time OSC 11 probe when the terminal answers in time.
///
/// Compatibility fallback used when the protocol-aware terminal reader cannot
/// be opened. It must run before the crossterm event loop begins.
pub fn apply_auto_theme_from_terminal() {
    let seed = super::resolve_auto_theme_name();
    let resolved = probe_terminal_background(PROBE_TIMEOUT)
        .map(|(r, g, b)| theme_for_luminance(relative_luminance(r, g, b), seed))
        .unwrap_or(seed);
    let _ = super::set_theme_by_name(resolved);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reply_two_digit_components() {
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:28/2c/34\x1b\\"),
            Some((0x28, 0x2c, 0x34))
        );
    }

    #[test]
    fn parse_reply_four_digit_components_bel_terminated() {
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
            Some((255, 255, 255))
        );
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:0000/0000/0000\x07"),
            Some((0, 0, 0))
        );
    }

    #[test]
    fn parse_reply_single_digit_components() {
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:f/0/8\x1b\\"),
            Some((255, 0, 0x88))
        );
    }

    #[test]
    fn parse_reply_embedded_in_other_input() {
        // Keystrokes that arrived ahead of the reply are skipped.
        assert_eq!(
            parse_osc11_reply(b"abc\x1b]11;rgb:11/22/33\x1b\\rest"),
            Some((0x11, 0x22, 0x33))
        );
    }

    #[test]
    fn parse_reply_rejects_malformed() {
        // Incomplete (no terminator yet).
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:28/2c"), None);
        // Foreground (OSC 10), not background.
        assert_eq!(parse_osc11_reply(b"\x1b]10;rgb:ff/ff/ff\x07"), None);
        // Not hex.
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:zz/00/00\x07"), None);
        // Too many components.
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:1/2/3/4\x07"), None);
        // Garbage.
        assert_eq!(parse_osc11_reply(b"hello world"), None);
        assert_eq!(parse_osc11_reply(b""), None);
    }

    #[test]
    fn luminance_bounds_and_ordering() {
        assert!(relative_luminance(0, 0, 0).abs() < 1e-9);
        assert!((relative_luminance(255, 255, 255) - 1.0).abs() < 1e-9);
        // Green contributes more than blue (Rec. 709).
        assert!(relative_luminance(0, 255, 0) > relative_luminance(0, 0, 255));
    }

    #[test]
    fn theme_for_luminance_thresholds_and_dead_band() {
        assert_eq!(theme_for_luminance(0.7, "dark"), "light");
        assert_eq!(theme_for_luminance(0.3, "light"), "dark");
        // Dead band keeps the current theme.
        assert_eq!(theme_for_luminance(0.5, "dark"), "dark");
        assert_eq!(theme_for_luminance(0.5, "light"), "light");
        // Exact thresholds do not flip.
        assert_eq!(theme_for_luminance(LIGHT_THRESHOLD, "dark"), "dark");
        assert_eq!(theme_for_luminance(DARK_THRESHOLD, "light"), "light");
    }

    #[test]
    fn follower_requires_consecutive_readings_to_flip() {
        let mut follower = AutoThemeFollower::new("dark");
        assert_eq!(follower.observe_luminance(0.9), None);
        assert_eq!(follower.current(), "dark");
        assert_eq!(follower.observe_luminance(0.9), Some("light"));
        assert_eq!(follower.current(), "light");
    }

    #[test]
    fn follower_resets_pending_on_interleaved_readings() {
        let mut follower = AutoThemeFollower::new("dark");
        assert_eq!(follower.observe_luminance(0.9), None);
        // A dead-band reading clears the pending flip.
        assert_eq!(follower.observe_luminance(0.5), None);
        assert_eq!(follower.observe_luminance(0.9), None);
        assert_eq!(follower.observe_luminance(0.9), Some("light"));

        // And the same for flipping back to dark.
        assert_eq!(follower.observe_luminance(0.2), None);
        assert_eq!(follower.observe_luminance(0.2), Some("dark"));
    }
}
