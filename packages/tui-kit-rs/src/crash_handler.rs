//! Async-signal-safe crash handler for fatal memory faults (SIGSEGV/SIGBUS).
//!
//! Adapted from grok-build's `xai-crash-handler` crate
//! (<https://github.com/xai-org/grok-build/tree/main/crates/codegen/xai-crash-handler>),
//! trimmed to maestro's needs: Unix signals only, and crash reports keep raw
//! instruction pointers with an offline-symbolication note instead of pulling
//! in a symbolication dependency.
//!
//! This complements the panic hook in `terminal::setup`: the hook covers Rust
//! panics, this handler covers hard crashes the hook cannot intercept. Note
//! that the release profile sets `panic = "abort"` — a panic runs the hook
//! (which restores the terminal) and then dies via SIGABRT, so the two paths
//! are covered between them: panics by the hook, SIGSEGV/SIGBUS by this
//! handler.
//!
//! The signal handler itself only performs async-signal-safe operations:
//! raw pointer reads, `write(2)` to a pre-opened fd, `tcsetattr(3)` with a
//! pre-saved termios, and a re-raise with `SIG_DFL` (preserving core dumps).
//! No allocation, no locks, no Rust runtime services.
//!
//! Crash records live under `~/.composer/crash/`:
//! - `last-crash.bin` — compact binary blob written from the signal handler
//! - `crash-<timestamp>.txt` — human-readable report rendered on the next
//!   startup; the last [`MAX_REPORTS`] reports are kept

use std::path::{Path, PathBuf};

/// Directory holding crash records for the current user (`~/.composer/crash`).
#[must_use]
pub fn default_crash_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".composer").join("crash"))
}

/// Number of human-readable crash reports retained under the crash directory.
const MAX_REPORTS: usize = 5;

/// Name of the binary blob the signal handler writes and the next run reads.
const BLOB_FILENAME: &str = "last-crash.bin";

/// Precomputed terminal restore escape sequence, written to the terminal
/// device from the signal handler.
///
/// Mirrors the set `terminal::setup::restore_impl` disables, in the order
/// grok-build uses: end synchronized updates first (so multiplexers flush
/// before later resets arrive), pop the kitty keyboard protocol stack, then
/// disable mouse tracking, bracketed paste, and focus reporting, and finally
/// show the cursor. Raw mode itself is undone separately via a saved termios.
/// There is no `\x1b[?1049l` here because the TUI uses an inline viewport, not
/// the alternate screen.
const TERMINAL_RESTORE_SEQ: &[u8] = b"\x1b[?2026l\x1b[<1u\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1004l\x1b[?25h";

/// Information about a crash from the previous session.
#[derive(Debug)]
pub struct CrashReport {
    /// Human-readable signal name (e.g. "SIGSEGV (Segmentation fault)").
    pub signal_name: &'static str,
    /// The `si_code` from `siginfo_t`.
    pub si_code: i32,
    /// The faulting memory address.
    pub faulting_address: u64,
    /// Unix timestamp of the crash.
    pub timestamp: u64,
    /// Application version at crash time.
    pub app_version: String,
    /// Raw backtrace instruction pointers (symbolize offline).
    pub frames: Vec<usize>,
    /// Path to the saved human-readable crash report.
    pub report_path: PathBuf,
}

/// One-line notice shown on startup after a crash, per the task contract.
#[must_use]
pub fn crash_notice(report: &CrashReport) -> String {
    format!(
        "maestro crashed last run ({}); report at {}",
        report.signal_name,
        report.report_path.display()
    )
}

/// Check for a crash from the previous session.
///
/// Reads `crash_dir/last-crash.bin`, renders a human-readable report,
/// prunes old reports to the last [`MAX_REPORTS`], and removes the blob so it
/// is not re-processed. Returns `Some` if a valid crash blob was found.
///
/// Must run before [`install`], which truncates the blob.
pub fn check_previous_crash(crash_dir: &Path) -> Option<CrashReport> {
    let crash_file = crash_dir.join(BLOB_FILENAME);
    let data = std::fs::read(&crash_file).ok()?;
    let blob = format::CrashBlob::parse(&data)?;

    let report_text = format_report(&blob);
    let report_path = crash_dir.join(format!("crash-{}.txt", blob.timestamp));
    let _ = write_owner_only(&report_path, report_text.as_bytes());
    prune_reports(crash_dir, MAX_REPORTS);

    // Remove the binary blob so it's not re-processed.
    let _ = std::fs::remove_file(&crash_file);

    Some(CrashReport {
        signal_name: signal_name(blob.signal),
        si_code: blob.si_code,
        faulting_address: blob.si_addr,
        timestamp: blob.timestamp,
        app_version: blob.app_version,
        frames: blob.frames,
        report_path,
    })
}

/// Render a human-readable crash report from a parsed blob.
fn format_report(blob: &format::CrashBlob) -> String {
    let mut out = String::with_capacity(1024);

    out.push_str("=== Deixic Code Crash Report ===\n\n");
    out.push_str(&format!("Signal:  {}\n", signal_name(blob.signal)));
    out.push_str(&format!(
        "si_code: {} ({})\n",
        blob.si_code,
        si_code_name(blob.signal, blob.si_code)
    ));
    out.push_str(&format!("Address: {:#018x}\n", blob.si_addr));
    out.push_str(&format!("PID:     {}\n", blob.pid));
    out.push_str(&format!("Version: {}\n", blob.app_version));
    out.push_str(&format!("Time:    {} (unix)\n", blob.timestamp));

    if blob.frames.is_empty() {
        out.push_str("\nBacktrace: unavailable (no frames captured safely)\n");
    } else {
        out.push_str(&format!(
            "\nBacktrace ({} frames, raw instruction pointers):\n",
            blob.frames.len()
        ));
        for (i, ip) in blob.frames.iter().enumerate() {
            out.push_str(&format!("  {i:>3}: {ip:#018x}\n"));
        }
        // Release binaries are stripped, so resolve addresses against a
        // debug build (or the `.dSYM`/debuginfo of the release) offline.
        out.push_str(
            "\nSymbolize offline, e.g.:\n  addr2line -e <maestro-binary> -f -C <addr>...\n",
        );
    }

    out.push_str("\n=== End Report ===\n");
    out
}

fn signal_name(sig: u8) -> &'static str {
    match sig as i32 {
        4 => "SIGILL (Illegal instruction)",
        // SIGBUS is 7 on Linux, 10 on macOS
        7 | 10 => "SIGBUS (Bus error)",
        11 => "SIGSEGV (Segmentation fault)",
        _ => "Unknown signal",
    }
}

fn si_code_name(sig: u8, code: i32) -> &'static str {
    if sig == 7 || sig == 10 {
        match code {
            1 => "BUS_ADRALN - invalid address alignment",
            2 => "BUS_ADRERR - non-existent physical address",
            3 => "BUS_OBJERR - object-specific hardware error",
            _ => "unknown",
        }
    } else {
        match code {
            1 => "SEGV_MAPERR - address not mapped",
            2 => "SEGV_ACCERR - invalid permissions",
            _ => "unknown",
        }
    }
}

/// Remove all but the `keep` newest `crash-*.txt` reports in `crash_dir`.
fn prune_reports(crash_dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(crash_dir) else {
        return;
    };
    let mut reports: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "txt")
                && p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().starts_with("crash-"))
        })
        .collect();
    // Timestamp-named reports sort oldest-first lexicographically.
    reports.sort();
    if reports.len() > keep {
        for old in &reports[..reports.len() - keep] {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// Write `contents` with owner-only permissions when the platform allows it.
///
/// Crash reports may include fault addresses and instruction pointers; they
/// must not be world-readable under the user's home directory.
fn write_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // mode() only applies on create — force owner-only before writing so a
        // preexisting 0644 file never holds sensitive content while world-readable.
        let mut perms = file.metadata()?.permissions();
        perms.set_mode(0o600);
        file.set_permissions(perms)?;
        file.write_all(contents)?;
        file.flush()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

/// Compact binary crash blob format ("MCRX").
///
/// The signal handler writes this format using only `libc::write` into a
/// pre-allocated static buffer (no allocation). The startup reader parses it
/// in normal Rust context.
mod format {
    /// Magic bytes identifying a valid crash file.
    pub const MAGIC: [u8; 4] = *b"MCRX";

    /// Current format version.
    pub const VERSION: u8 = 1;

    /// Maximum backtrace frames captured in the signal handler.
    pub const MAX_FRAMES: usize = 64;

    /// Length of the null-padded version string field.
    pub const VERSION_STRING_LEN: usize = 32;

    /// Fixed header size (before the variable-length frames array).
    ///
    /// Layout:
    /// - magic:        4 bytes
    /// - version:      1 byte
    /// - signal:       1 byte
    /// - si_code:      4 bytes (i32, little-endian)
    /// - si_addr:      8 bytes (u64, little-endian)
    /// - pid:          4 bytes (u32, little-endian)
    /// - timestamp:    8 bytes (u64, little-endian)
    /// - n_frames:     2 bytes (u16, little-endian)
    /// - app_version: 32 bytes (null-padded UTF-8)
    pub const HEADER_SIZE: usize = 4 + 1 + 1 + 4 + 8 + 4 + 8 + 2 + VERSION_STRING_LEN;

    /// Total maximum file size: header + 64 frames * 8 bytes each.
    pub const MAX_FILE_SIZE: usize = HEADER_SIZE + MAX_FRAMES * 8;

    /// Parsed crash data from a `last-crash.bin` file.
    #[derive(Debug, Clone)]
    pub struct CrashBlob {
        pub signal: u8,
        pub si_code: i32,
        pub si_addr: u64,
        pub pid: u32,
        pub timestamp: u64,
        pub frames: Vec<usize>,
        pub app_version: String,
    }

    impl CrashBlob {
        /// Parse a crash blob from bytes. Returns `None` if the data is invalid.
        pub fn parse(data: &[u8]) -> Option<Self> {
            if data.len() < HEADER_SIZE {
                return None;
            }
            if data[0..4] != MAGIC {
                return None;
            }
            if data[4] != VERSION {
                return None;
            }

            let signal = data[5];
            let si_code = i32::from_le_bytes([data[6], data[7], data[8], data[9]]);
            let si_addr = u64::from_le_bytes([
                data[10], data[11], data[12], data[13], data[14], data[15], data[16], data[17],
            ]);
            let pid = u32::from_le_bytes([data[18], data[19], data[20], data[21]]);
            let timestamp = u64::from_le_bytes([
                data[22], data[23], data[24], data[25], data[26], data[27], data[28], data[29],
            ]);
            let n_frames = u16::from_le_bytes([data[30], data[31]]) as usize;

            let version_bytes = &data[32..32 + VERSION_STRING_LEN];
            let app_version = std::str::from_utf8(version_bytes)
                .unwrap_or("")
                .trim_end_matches('\0')
                .to_string();

            if n_frames > MAX_FRAMES {
                return None;
            }
            let frames_start = HEADER_SIZE;
            let frames_end = frames_start + n_frames * 8;
            if data.len() < frames_end {
                return None;
            }

            let mut frames = Vec::with_capacity(n_frames);
            for i in 0..n_frames {
                let offset = frames_start + i * 8;
                let addr = u64::from_le_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                    data[offset + 4],
                    data[offset + 5],
                    data[offset + 6],
                    data[offset + 7],
                ]);
                frames.push(addr as usize);
            }

            Some(CrashBlob {
                signal,
                si_code,
                si_addr,
                pid,
                timestamp,
                frames,
                app_version,
            })
        }
    }

    /// Helpers for writing fields in the signal handler using raw byte copies.
    /// All operations are on a pre-allocated static buffer — no allocation.
    pub mod writer {
        use super::{MAGIC, VERSION, VERSION_STRING_LEN};

        /// Write the crash blob header into `buf`, returning the number of bytes written.
        /// The caller must ensure `buf` is at least `HEADER_SIZE` bytes.
        ///
        /// # Safety
        ///
        /// Called from a signal handler. The buffer must be valid and large enough.
        // Fixed binary wire format — the fields mirror the header layout and
        // stay flat so the signal handler passes them without any struct init.
        #[allow(clippy::too_many_arguments)]
        pub unsafe fn write_header(
            buf: &mut [u8],
            signal: u8,
            si_code: i32,
            si_addr: u64,
            pid: u32,
            timestamp: u64,
            n_frames: u16,
            app_version: &[u8],
        ) -> usize {
            buf[0..4].copy_from_slice(&MAGIC);
            buf[4] = VERSION;
            buf[5] = signal;
            buf[6..10].copy_from_slice(&si_code.to_le_bytes());
            buf[10..18].copy_from_slice(&si_addr.to_le_bytes());
            buf[18..22].copy_from_slice(&pid.to_le_bytes());
            buf[22..30].copy_from_slice(&timestamp.to_le_bytes());
            buf[30..32].copy_from_slice(&n_frames.to_le_bytes());

            // Null-pad the version string field.
            let version_field = &mut buf[32..32 + VERSION_STRING_LEN];
            version_field.fill(0);
            let copy_len = app_version.len().min(VERSION_STRING_LEN);
            version_field[..copy_len].copy_from_slice(&app_version[..copy_len]);

            32 + VERSION_STRING_LEN
        }

        /// Write a single frame pointer into `buf` at the given offset.
        /// Returns the new offset.
        ///
        /// # Safety
        ///
        /// The caller must ensure `buf[offset..offset+8]` is valid.
        pub unsafe fn write_frame(buf: &mut [u8], offset: usize, addr: usize) -> usize {
            buf[offset..offset + 8].copy_from_slice(&(addr as u64).to_le_bytes());
            offset + 8
        }
    }
}

/// Save the current (pre-raw-mode) terminal state for the crash handler.
///
/// Must be called before raw mode is enabled so the saved termios reflects
/// the user's original terminal configuration. No-op on non-Unix platforms.
pub fn save_terminal_state(tty: &std::fs::File) {
    imp::save_terminal_state(tty);
}

/// Install the SIGSEGV/SIGBUS crash handler.
///
/// Pre-opens `crash_dir/last-crash.bin` (owner-only) so the signal handler
/// can write without allocating, allocates an alternate signal stack
/// (surviving stack overflow), and registers an async-signal-safe handler
/// that writes the crash blob, restores the terminal, then re-raises with
/// the default disposition. Returns `false` on unsupported platforms or if
/// the crash file cannot be opened.
pub fn install(crash_dir: &Path, app_version: &str) -> bool {
    imp::install(crash_dir, app_version)
}

#[cfg(unix)]
mod imp {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    use super::TERMINAL_RESTORE_SEQ;
    use super::format::{self, MAX_FILE_SIZE, MAX_FRAMES};

    // ── Platform-specific ucontext access ────────────────────────────────
    //
    // The libc crate does not expose ucontext_t on macOS. We define minimal
    // repr(C) types covering only the fields we need (PC and frame pointer).

    /// Extract the crash instruction pointer and frame pointer from the
    /// signal handler's context parameter.
    ///
    /// Returns `(instruction_pointer, frame_pointer)`. Both may be 0 if
    /// the context is null or the platform is unsupported.
    unsafe fn extract_pc_and_fp(ctx: *mut libc::c_void) -> (usize, usize) {
        if ctx.is_null() {
            return (0, 0);
        }

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        unsafe {
            let uc = ctx as *const libc::ucontext_t;
            let gregs = &(*uc).uc_mcontext.gregs;
            let ip = gregs[libc::REG_RIP as usize] as usize;
            let fp = gregs[libc::REG_RBP as usize] as usize;
            return (ip, fp);
        }

        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        unsafe {
            let uc = ctx as *const libc::ucontext_t;
            let mc = &(*uc).uc_mcontext;
            let ip = mc.pc as usize;
            let fp = mc.regs[29] as usize; // x29 = frame pointer
            return (ip, fp);
        }

        // macOS does not expose ucontext_t in the libc crate.
        // Define minimal repr(C) types for the fields we need.
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            #[repr(C)]
            struct Arm64ThreadState {
                regs: [u64; 29], // x0-x28
                fp: u64,         // x29
                lr: u64,         // x30
                sp: u64,
                pc: u64,
                cpsr: u32,
                _pad: u32,
            }
            #[repr(C)]
            struct MachMcontext {
                _es: [u8; 16], // __darwin_arm_exception_state64 (far:u64 + esr:u32 + exception:u32)
                ss: Arm64ThreadState,
                // neon state follows but we don't need it
            }
            #[repr(C)]
            struct DarwinUcontext {
                _onstack: i32,
                _sigmask: u32,
                _stack: libc::stack_t,
                _link: *mut libc::c_void,
                _mcsize: usize,
                mctx: *const MachMcontext,
            }
            let (ip, fp) = unsafe {
                let uc = ctx as *const DarwinUcontext;
                let mctx = (*uc).mctx;
                if mctx.is_null() {
                    return (0, 0);
                }
                ((*mctx).ss.pc as usize, (*mctx).ss.fp as usize)
            };
            return (ip, fp);
        }

        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            #[repr(C)]
            struct X86ThreadState {
                _rax: u64,
                _rbx: u64,
                _rcx: u64,
                _rdx: u64,
                _rdi: u64,
                _rsi: u64,
                rbp: u64,
                _rsp: u64,
                _r8: u64,
                _r9: u64,
                _r10: u64,
                _r11: u64,
                _r12: u64,
                _r13: u64,
                _r14: u64,
                _r15: u64,
                rip: u64,
                _rflags: u64,
                _cs: u64,
                _fs: u64,
                _gs: u64,
            }
            #[repr(C)]
            struct MachMcontext {
                _es: [u8; 16], // __darwin_x86_exception_state64
                ss: X86ThreadState,
            }
            #[repr(C)]
            struct DarwinUcontext {
                _onstack: i32,
                _sigmask: u32,
                _stack: libc::stack_t,
                _link: *mut libc::c_void,
                _mcsize: usize,
                mctx: *const MachMcontext,
            }
            let (ip, fp) = unsafe {
                let uc = ctx as *const DarwinUcontext;
                let mctx = (*uc).mctx;
                if mctx.is_null() {
                    return (0, 0);
                }
                ((*mctx).ss.rip as usize, (*mctx).ss.rbp as usize)
            };
            return (ip, fp);
        }

        // Unsupported platform — no frames.
        #[allow(unreachable_code)]
        (0, 0)
    }

    /// Walk the frame-pointer chain, collecting return addresses.
    ///
    /// Fully async-signal-safe: only raw pointer reads, no library calls.
    /// Stops at the first invalid (null, misaligned, or suspiciously small)
    /// frame pointer.
    unsafe fn walk_frame_pointers(initial_fp: usize, out: &mut [usize], max: usize) -> usize {
        let mut fp = initial_fp;
        let mut count = 0;

        while count < max {
            // Validate: non-null, pointer-aligned, not in the zero page.
            if fp == 0 || fp < 4096 || !fp.is_multiple_of(core::mem::size_of::<usize>()) {
                break;
            }
            // On both x86_64 and aarch64, the frame layout is:
            //   [fp+0] = previous frame pointer
            //   [fp+8] = return address
            let prev_fp = unsafe { *(fp as *const usize) };
            let ret_addr = unsafe { *((fp + core::mem::size_of::<usize>()) as *const usize) };

            if ret_addr == 0 || ret_addr < 4096 {
                break;
            }
            out[count] = ret_addr;
            count += 1;

            // Frame pointer must move upward (toward higher addresses on
            // most architectures) to avoid infinite loops.
            if prev_fp <= fp {
                break;
            }
            fp = prev_fp;
        }

        count
    }

    /// File descriptor for the pre-opened crash file.
    static CRASH_FD: AtomicI32 = AtomicI32::new(-1);

    /// File descriptor of the terminal device (`/dev/tty`), for restore writes.
    static TTY_FD: AtomicI32 = AtomicI32::new(-1);

    /// Pre-allocated write buffer (lives in .bss, zero cost when not crashing).
    static mut CRASH_BUF: [u8; MAX_FILE_SIZE] = [0; MAX_FILE_SIZE];

    /// Saved pre-raw-mode terminal state for restoration in the signal handler.
    static mut ORIGINAL_TERMIOS: libc::termios = unsafe { std::mem::zeroed() };

    /// Whether we successfully saved the original termios.
    static mut HAS_TERMIOS: bool = false;

    /// Application version string, set at install time.
    static mut APP_VERSION: [u8; format::VERSION_STRING_LEN] = [0; format::VERSION_STRING_LEN];

    /// Alternate signal stack memory (16 KiB via mmap).
    const ALT_STACK_SIZE: usize = 16 * 1024;

    /// Guards against double-allocating the alternate signal stack when
    /// the terminal is re-initialized.
    static ALT_STACK_INSTALLED: AtomicBool = AtomicBool::new(false);

    /// Save the current terminal state for restoration in the signal handler.
    pub fn save_terminal_state(tty: &std::fs::File) {
        let fd = tty.as_raw_fd();
        TTY_FD.store(fd, Ordering::Relaxed);
        unsafe {
            let termios = &mut *std::ptr::addr_of_mut!(ORIGINAL_TERMIOS);
            if libc::tcgetattr(fd, termios) == 0 {
                *std::ptr::addr_of_mut!(HAS_TERMIOS) = true;
            }
        }
    }

    /// Allocate an alternate signal stack via mmap (survives stack overflow).
    fn setup_alt_stack() {
        if ALT_STACK_INSTALLED.swap(true, Ordering::AcqRel) {
            return;
        }
        unsafe {
            let stack_mem = libc::mmap(
                std::ptr::null_mut(),
                ALT_STACK_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if stack_mem != libc::MAP_FAILED {
                let ss = libc::stack_t {
                    ss_sp: stack_mem,
                    ss_flags: 0,
                    ss_size: ALT_STACK_SIZE,
                };
                libc::sigaltstack(&raw const ss, std::ptr::null_mut());
            }
        }
    }

    /// Restore terminal escapes + termios, then re-raise with `SIG_DFL`.
    ///
    /// Re-raising with the default disposition terminates the process with
    /// the original signal, preserving exit status and core dumps.
    ///
    /// # Safety
    ///
    /// Must only be called from a signal handler context.
    unsafe fn restore_terminal_and_reraise(sig: libc::c_int) {
        unsafe {
            let tty_fd = TTY_FD.load(Ordering::Relaxed);
            if tty_fd >= 0 {
                libc::write(
                    tty_fd,
                    TERMINAL_RESTORE_SEQ.as_ptr().cast::<libc::c_void>(),
                    TERMINAL_RESTORE_SEQ.len(),
                );
                if *std::ptr::addr_of!(HAS_TERMIOS) {
                    libc::tcsetattr(tty_fd, libc::TCSANOW, std::ptr::addr_of!(ORIGINAL_TERMIOS));
                }
            }
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = libc::SIG_DFL;
            sa.sa_flags = 0;
            libc::sigemptyset(&raw mut sa.sa_mask);
            libc::sigaction(sig, &raw const sa, std::ptr::null_mut());
            libc::raise(sig);
        }
    }

    /// Write crash blob to the pre-opened fd.
    ///
    /// # Safety
    ///
    /// Signal handler context. Only async-signal-safe operations.
    unsafe fn write_crash_blob(
        sig: libc::c_int,
        info: *mut libc::siginfo_t,
        ctx: *mut libc::c_void,
    ) {
        unsafe {
            let fd = CRASH_FD.load(Ordering::Relaxed);
            if fd < 0 {
                return;
            }
            let si_code = if info.is_null() { 0 } else { (*info).si_code };

            #[cfg(target_os = "macos")]
            let si_addr = if info.is_null() {
                0
            } else {
                (*info).si_addr as u64
            };
            #[cfg(target_os = "linux")]
            let si_addr = if info.is_null() {
                0
            } else {
                (*info).si_addr() as u64
            };
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            let si_addr: u64 = 0;

            let pid = libc::getpid() as u32;
            let timestamp = libc::time(std::ptr::null_mut()) as u64;

            let mut frames: [usize; MAX_FRAMES] = [0; MAX_FRAMES];
            let mut n_frames: u16 = 0;
            let buf = &mut *std::ptr::addr_of_mut!(CRASH_BUF);
            let version = &*std::ptr::addr_of!(APP_VERSION);

            let (crash_pc, crash_fp) = extract_pc_and_fp(ctx);
            if crash_pc != 0 {
                frames[0] = crash_pc;
                n_frames = 1;
            }

            // Write the blob with the crash PC before walking frames.
            // Frame walking dereferences arbitrary pointers and can fault;
            // SA_RESETHAND would kill us without writing anything.
            let mut offset = format::writer::write_header(
                buf, sig as u8, si_code, si_addr, pid, timestamp, n_frames, version,
            );
            for frame in frames.iter().take(n_frames as usize) {
                offset = format::writer::write_frame(buf, offset, *frame);
            }
            libc::write(fd, buf.as_ptr().cast::<libc::c_void>(), offset);

            // Best-effort: walk frame pointers for additional context.
            // If this faults, the 1-frame blob above is already on disk.
            if crash_fp != 0 && crash_pc != 0 {
                let walked = walk_frame_pointers(crash_fp, &mut frames[1..], MAX_FRAMES - 1);
                if walked > 0 {
                    n_frames += walked as u16;
                    let mut offset = format::writer::write_header(
                        buf, sig as u8, si_code, si_addr, pid, timestamp, n_frames, version,
                    );
                    for frame in frames.iter().take(n_frames as usize) {
                        offset = format::writer::write_frame(buf, offset, *frame);
                    }
                    libc::lseek(fd, 0, libc::SEEK_SET);
                    libc::write(fd, buf.as_ptr().cast::<libc::c_void>(), offset);
                }
            }

            CRASH_FD.store(-1, Ordering::Relaxed);
            libc::close(fd);
        }
    }

    /// Crash handler: record the crash, restore the terminal, re-raise.
    ///
    /// `alarm(3)` bounds the handler: if anything here hangs, the process
    /// still dies shortly after via SIGALRM.
    unsafe extern "C" fn crash_handler(
        sig: libc::c_int,
        info: *mut libc::siginfo_t,
        ctx: *mut libc::c_void,
    ) {
        unsafe {
            libc::alarm(3);
            write_crash_blob(sig, info, ctx);
            restore_terminal_and_reraise(sig);
        }
    }

    /// Install the crash handler for SIGSEGV and SIGBUS.
    ///
    /// Flags: `SA_SIGINFO | SA_ONSTACK | SA_RESETHAND`. `SA_RESETHAND`
    /// resets disposition to `SIG_DFL` after delivery, preventing recursive
    /// faults in the handler from looping.
    pub fn install(crash_dir: &Path, app_version: &str) -> bool {
        let crash_file = crash_dir.join(super::BLOB_FILENAME);

        // Create the crash directory if it doesn't exist.
        if std::fs::create_dir_all(crash_dir).is_err() {
            return false;
        }

        // Open crash file (pre-opened fd for the signal handler).
        let c_path = match CString::new(crash_file.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(_) => return false,
        };
        // Owner-only: crash blobs hold stack IPs / fault addresses.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o600,
            )
        };
        if fd < 0 {
            return false;
        }
        // open's mode is create-only; tighten upgrades of older 0644 blobs.
        if unsafe { libc::fchmod(fd, 0o600) } != 0 {
            unsafe {
                libc::close(fd);
            }
            return false;
        }
        CRASH_FD.store(fd, Ordering::Relaxed);

        // Store version string.
        unsafe {
            let version = &mut *std::ptr::addr_of_mut!(APP_VERSION);
            version.fill(0);
            let copy_len = app_version.len().min(format::VERSION_STRING_LEN);
            version[..copy_len].copy_from_slice(&app_version.as_bytes()[..copy_len]);
        }

        setup_alt_stack();

        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = crash_handler as *const () as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESETHAND;
            libc::sigemptyset(&raw mut sa.sa_mask);

            libc::sigaction(libc::SIGBUS, &raw const sa, std::ptr::null_mut());
            libc::sigaction(libc::SIGSEGV, &raw const sa, std::ptr::null_mut());
        }

        true
    }
}

#[cfg(not(unix))]
mod imp {
    /// No-op: no termios to save on non-Unix platforms.
    pub fn save_terminal_state(_tty: &std::fs::File) {}

    /// Crash handler is Unix-only; returns `false` elsewhere.
    pub fn install(_crash_dir: &std::path::Path, _app_version: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::Command;

    /// Build a blob in a normal (non-signal) context using the same writer
    /// the signal handler uses.
    fn build_blob(
        signal: u8,
        si_code: i32,
        si_addr: u64,
        timestamp: u64,
        version: &str,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; format::MAX_FILE_SIZE];
        let frames: [usize; 2] = [0xdead_beef, 0xcafe_babe];
        unsafe {
            let mut offset = format::writer::write_header(
                &mut buf,
                signal,
                si_code,
                si_addr,
                42,
                timestamp,
                frames.len() as u16,
                version.as_bytes(),
            );
            for &frame in &frames {
                offset = format::writer::write_frame(&mut buf, offset, frame);
            }
            buf.truncate(offset);
        }
        buf
    }

    // ── Blob format ─────────────────────────────────────────────────────

    #[test]
    fn crash_blob_roundtrip() {
        let data = build_blob(11, 1, 0x7f8a_1234_0000, 1_712_678_587, "0.1.0");
        let blob = format::CrashBlob::parse(&data).expect("parse should succeed");
        assert_eq!(blob.signal, 11);
        assert_eq!(blob.si_code, 1);
        assert_eq!(blob.si_addr, 0x7f8a_1234_0000);
        assert_eq!(blob.pid, 42);
        assert_eq!(blob.timestamp, 1_712_678_587);
        assert_eq!(blob.frames, vec![0xdead_beef, 0xcafe_babe]);
        assert_eq!(blob.app_version, "0.1.0");
    }

    #[test]
    fn crash_blob_rejects_bad_magic() {
        let mut data = build_blob(11, 1, 0, 1, "0.1.0");
        data[0..4].copy_from_slice(b"NOPE");
        assert!(format::CrashBlob::parse(&data).is_none());
    }

    #[test]
    fn crash_blob_rejects_truncated_data() {
        assert!(format::CrashBlob::parse(&[]).is_none());
        assert!(format::CrashBlob::parse(&format::MAGIC).is_none());
        // Header truncated mid-version-string.
        let data = build_blob(11, 1, 0, 1, "0.1.0");
        assert!(format::CrashBlob::parse(&data[..20]).is_none());
        // Frames truncated.
        assert!(format::CrashBlob::parse(&data[..format::HEADER_SIZE + 8]).is_none());
    }

    // ── Report rendering & notice ───────────────────────────────────────

    #[test]
    fn crash_signal_names() {
        assert_eq!(signal_name(11), "SIGSEGV (Segmentation fault)");
        assert_eq!(signal_name(7), "SIGBUS (Bus error)");
        assert_eq!(signal_name(10), "SIGBUS (Bus error)");
        assert_eq!(signal_name(4), "SIGILL (Illegal instruction)");
        assert_eq!(signal_name(99), "Unknown signal");
    }

    #[test]
    fn crash_report_formats_fields_and_frames() {
        let blob = format::CrashBlob::parse(&build_blob(11, 1, 0xdead, 1_712_678_587, "0.1.0"))
            .expect("parse");
        let report = format_report(&blob);
        assert!(report.contains("SIGSEGV (Segmentation fault)"));
        assert!(report.contains("SEGV_MAPERR"));
        assert!(report.contains("0x000000000000dead"));
        assert!(report.contains("Version: 0.1.0"));
        assert!(report.contains("0x00000000deadbeef"));
        assert!(report.contains("addr2line"));
    }

    #[test]
    fn crash_report_notes_missing_backtrace() {
        let blob = format::CrashBlob {
            signal: 11,
            si_code: 1,
            si_addr: 0,
            pid: 1,
            timestamp: 1,
            frames: Vec::new(),
            app_version: "0.1.0".to_string(),
        };
        let report = format_report(&blob);
        assert!(report.contains("Backtrace: unavailable"));
    }

    #[test]
    fn crash_notice_renders_one_line_with_path() {
        let report = CrashReport {
            signal_name: "SIGSEGV (Segmentation fault)",
            si_code: 1,
            faulting_address: 0,
            timestamp: 1,
            app_version: "0.1.0".to_string(),
            frames: Vec::new(),
            report_path: PathBuf::from("/home/user/.composer/crash/crash-1.txt"),
        };
        let notice = crash_notice(&report);
        assert_eq!(notice.lines().count(), 1);
        assert!(notice.contains("crashed last run"));
        assert!(notice.contains("SIGSEGV"));
        assert!(notice.contains("/home/user/.composer/crash/crash-1.txt"));
    }

    // ── Startup check & report rotation ─────────────────────────────────

    #[test]
    fn check_previous_crash_returns_none_without_blob() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(check_previous_crash(dir.path()).is_none());
    }

    #[test]
    fn check_previous_crash_ignores_empty_blob_from_clean_run() {
        // install() truncates last-crash.bin every run, so a clean exit
        // leaves a zero-byte blob behind; it must not be treated as a crash.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(BLOB_FILENAME), b"").expect("write");
        assert!(check_previous_crash(dir.path()).is_none());
    }

    #[test]
    fn check_previous_crash_writes_report_and_removes_blob() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = build_blob(11, 1, 0xdead, 1_712_678_587, "0.1.0");
        std::fs::write(dir.path().join(BLOB_FILENAME), &data).expect("write");

        let report = check_previous_crash(dir.path()).expect("crash should be detected");
        assert_eq!(report.signal_name, "SIGSEGV (Segmentation fault)");
        assert_eq!(report.faulting_address, 0xdead);
        assert_eq!(report.timestamp, 1_712_678_587);
        assert_eq!(report.frames, vec![0xdead_beef, 0xcafe_babe]);
        assert!(
            !dir.path().join(BLOB_FILENAME).exists(),
            "blob must be consumed"
        );

        let text = std::fs::read_to_string(&report.report_path).expect("report readable");
        assert!(text.contains("Deixic Code Crash Report"));
        assert!(text.contains("SIGSEGV"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&report.report_path)
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "report must be owner-only");
        }
    }

    #[test]
    fn crash_report_rotation_keeps_last_five() {
        let dir = tempfile::tempdir().expect("tempdir");
        for ts in [100, 101, 102, 103, 104, 105, 106] {
            std::fs::write(dir.path().join(format!("crash-{ts}.txt")), b"x").expect("write");
        }
        // Unrelated files must survive pruning.
        std::fs::write(dir.path().join(BLOB_FILENAME), b"").expect("write");
        std::fs::write(dir.path().join("notes.md"), b"").expect("write");

        prune_reports(dir.path(), MAX_REPORTS);

        for ts in [100, 101] {
            assert!(
                !dir.path().join(format!("crash-{ts}.txt")).exists(),
                "oldest report crash-{ts}.txt should be pruned"
            );
        }
        for ts in [102, 103, 104, 105, 106] {
            assert!(
                dir.path().join(format!("crash-{ts}.txt")).exists(),
                "report crash-{ts}.txt should be kept"
            );
        }
        assert!(dir.path().join(BLOB_FILENAME).exists());
        assert!(dir.path().join("notes.md").exists());
    }

    // ── Terminal restore sequence ───────────────────────────────────────

    /// The restore sequence must mirror the modes `terminal::setup` enables,
    /// using crossterm's own ANSI output as the source of truth.
    #[test]
    fn restore_seq_matches_crossterm_teardown_set() {
        fn ansi(cmd: &impl Command) -> String {
            let mut s = String::new();
            cmd.write_ansi(&mut s).expect("ansi write");
            s
        }

        let seq = std::str::from_utf8(TERMINAL_RESTORE_SEQ).expect("restore seq is utf8");
        for fragment in [
            ansi(&crossterm::event::PopKeyboardEnhancementFlags),
            ansi(&crossterm::event::DisableMouseCapture),
            ansi(&crossterm::event::DisableBracketedPaste),
            ansi(&crossterm::event::DisableFocusChange),
            ansi(&crossterm::cursor::Show),
            "\x1b[?2026l".to_string(), // EndSynchronizedUpdate (sync_output.rs)
        ] {
            assert!(
                seq.contains(&fragment),
                "TERMINAL_RESTORE_SEQ must contain {fragment:?}"
            );
        }
        // The TUI uses an inline viewport — no alternate screen to leave.
        assert!(!seq.contains("\x1b[?1049l"));
    }

    #[test]
    fn restore_seq_ends_synchronized_update_first() {
        // Multiplexers (tmux/zellij) must stop buffering before subsequent
        // resets arrive, otherwise they get batched onto the wrong screen.
        assert!(TERMINAL_RESTORE_SEQ.starts_with(b"\x1b[?2026l"));
    }
}
