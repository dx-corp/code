//! Pre-main process hardening, adopted from codex's `codex-process-hardening`
//! crate (`codex-rs/process-hardening`).
//!
//! [`pre_main_hardening`] is invoked from the installed `maestro` binary via
//! `#[ctor::ctor]` so it runs before `main` — and therefore before the Tokio
//! runtime spawns worker threads, keeping the environment mutation and the
//! `prctl`/`setrlimit` calls race-free. On Unix it:
//!
//! - marks the process non-dumpable (`PR_SET_DUMPABLE, 0` on Linux,
//!   `PT_DENY_ATTACH` on macOS), which blocks ptrace attach,
//! - sets `RLIMIT_CORE` to 0, disabling OS core files,
//! - strips dynamic-loader control variables (`LD_*` / `DYLD_*`) from the
//!   live process environment so they are not inherited by child processes.
//!
//! The dynamic loader runs before Rust constructors, so this cannot undo
//! launch-time loader actions that already occurred. It does prevent those
//! variables from propagating to Maestro's children.
//!
//! Core dumps vs the crash handler: the panic hook in `terminal::setup` and
//! the fatal-signal crash handler re-raise with `SIG_DFL` so the OS can write
//! a core. With hardening enabled (the default) the handler still runs first —
//! it records the crash and restores the terminal before the re-raise — but
//! `RLIMIT_CORE=0` and the non-dumpable flag mean no OS core file is
//! produced. Set `MAESTRO_DISABLE_PROCESS_HARDENING=1` to opt out entirely
//! (e.g. when debugging a crash and a core dump is needed).

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

/// Environment variable that opts the process out of [`pre_main_hardening`].
pub const DISABLE_HARDENING_ENV_VAR: &str = "MAESTRO_DISABLE_PROCESS_HARDENING";

/// Returns true when an env var value opts out of hardening. Mirrors the
/// truthy convention used by `tools::shell_env` (`1|true|yes|on`,
/// case-insensitive, surrounding whitespace ignored).
fn hardening_disabled(value: Option<&str>) -> bool {
    matches!(
        value.map(|value| value.trim().to_ascii_lowercase()),
        Some(value) if matches!(value.as_str(), "1" | "true" | "yes" | "on")
    )
}

/// Performs pre-main process hardening; see the module docs. Call exactly
/// once, before any threads are spawned (the `maestro` binary does this from
/// a `#[ctor::ctor]` constructor).
pub fn pre_main_hardening() {
    if hardening_disabled(std::env::var(DISABLE_HARDENING_ENV_VAR).ok().as_deref()) {
        return;
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pre_main_hardening_linux();

    #[cfg(target_os = "macos")]
    pre_main_hardening_macos();

    // On FreeBSD and OpenBSD, apply similar hardening to Linux/macOS.
    #[cfg(any(target_os = "freebsd", target_os = "openbsd"))]
    pre_main_hardening_bsd();
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const PRCTL_FAILED_EXIT_CODE: i32 = 5;

#[cfg(target_os = "macos")]
const PTRACE_DENY_ATTACH_FAILED_EXIT_CODE: i32 = 6;

#[cfg(unix)]
const SET_RLIMIT_CORE_FAILED_EXIT_CODE: i32 = 7;

#[cfg(any(target_os = "linux", target_os = "android"))]
fn pre_main_hardening_linux() {
    // Disable ptrace attach / mark process non-dumpable.
    if let Err(err) = disable_process_dumping() {
        eprintln!("ERROR: prctl(PR_SET_DUMPABLE, 0) failed: {err}");
        std::process::exit(PRCTL_FAILED_EXIT_CODE);
    }

    // For "defense in depth," set the core file size limit to 0.
    set_core_file_size_limit_to_zero();

    remove_loader_injection_env_vars();
}

/// Mark the current Linux process non-dumpable so same-user processes cannot
/// attach with ptrace.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn disable_process_dumping() -> std::io::Result<()> {
    // SAFETY: `prctl(PR_SET_DUMPABLE, 0, 0, 0, 0)` takes no pointer arguments
    // this process doesn't already own; it only flips a per-process kernel
    // flag and cannot violate Rust's memory-safety invariants.
    let ret_code = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if ret_code == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn pre_main_hardening_macos() {
    // Prevent debuggers from attaching to this process.
    // SAFETY: `ptrace(PT_DENY_ATTACH, 0, NULL, 0)` is macOS's documented,
    // argument-free idiom for opting the calling process out of ptrace
    // attach; the null pointer is the request's expected `addr` value, not a
    // dereferenced pointer, so there is no memory-safety invariant to break.
    let ret_code = unsafe { libc::ptrace(libc::PT_DENY_ATTACH, 0, std::ptr::null_mut(), 0) };
    if ret_code == -1 {
        eprintln!(
            "ERROR: ptrace(PT_DENY_ATTACH) failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(PTRACE_DENY_ATTACH_FAILED_EXIT_CODE);
    }

    // Set the core file size limit to 0 to prevent core dumps.
    set_core_file_size_limit_to_zero();

    remove_loader_injection_env_vars();
}

#[cfg(any(target_os = "freebsd", target_os = "openbsd"))]
fn pre_main_hardening_bsd() {
    set_core_file_size_limit_to_zero();
    remove_loader_injection_env_vars();
}

#[cfg(unix)]
fn set_core_file_size_limit_to_zero() {
    let rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    // SAFETY: `rlim` is a valid, live `libc::rlimit` owned by this stack
    // frame for the duration of the call, and `setrlimit` only reads through
    // the pointer it's given.
    let ret_code = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const rlim) };
    if ret_code != 0 {
        eprintln!(
            "ERROR: setrlimit(RLIMIT_CORE) failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(SET_RLIMIT_CORE_FAILED_EXIT_CODE);
    }
}

/// Prefixes of environment variables that can inject code into a process at
/// load time. `tools::shell_env` applies the same policy to child shell
/// environments via its default excludes.
#[cfg(unix)]
const LOADER_INJECTION_ENV_PREFIXES: [&[u8]; 2] = [b"LD_", b"DYLD_"];

#[cfg(unix)]
fn remove_loader_injection_env_vars() {
    for prefix in LOADER_INJECTION_ENV_PREFIXES {
        for key in env_keys_with_prefix(std::env::vars_os(), prefix) {
            // SAFETY: only called from `pre_main_hardening`, which runs
            // pre-main via `#[ctor::ctor]` before any threads exist.
            unsafe {
                std::env::remove_var(key);
            }
        }
    }
}

#[cfg(unix)]
fn env_keys_with_prefix<I>(vars: I, prefix: &[u8]) -> Vec<OsString>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    vars.into_iter()
        .filter_map(|(key, _)| {
            key.as_os_str()
                .as_bytes()
                .starts_with(prefix)
                .then_some(key)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardening_disabled_recognizes_truthy_values() {
        for value in ["1", "true", "TRUE", " yes ", "on", "On"] {
            assert!(
                hardening_disabled(Some(value)),
                "{value:?} should disable hardening"
            );
        }
    }

    #[test]
    fn hardening_disabled_rejects_other_values() {
        assert!(!hardening_disabled(None));
        for value in ["", "0", "false", "no", "off", "2", "enabled"] {
            assert!(
                !hardening_disabled(Some(value)),
                "{value:?} should not disable hardening"
            );
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStringExt;

        #[test]
        fn env_keys_with_prefix_handles_non_utf8_entries() {
            // RÖDBURK
            let non_utf8_key1 = OsStr::from_bytes(b"R\xD6DBURK").to_os_string();
            assert!(non_utf8_key1.clone().into_string().is_err());
            let non_utf8_key2 = OsString::from_vec(vec![b'D', b'Y', b'L', b'D', b'_', 0xF0]);
            assert!(non_utf8_key2.clone().into_string().is_err());

            let non_utf8_value = OsString::from_vec(vec![0xF0, 0x9F, 0x92, 0xA9]);

            let keys = env_keys_with_prefix(
                vec![
                    (non_utf8_key1, non_utf8_value.clone()),
                    (non_utf8_key2.clone(), non_utf8_value),
                ],
                b"DYLD_",
            );
            assert_eq!(
                keys,
                vec![non_utf8_key2],
                "non-UTF-8 env entries with DYLD_ prefix should be retained"
            );
        }

        #[test]
        fn env_keys_with_prefix_filters_only_matching_keys() {
            let ld_preload = OsStr::from_bytes(b"LD_PRELOAD");
            let vars = vec![
                (OsString::from("PATH"), OsString::from("/usr/bin")),
                (ld_preload.to_os_string(), OsString::from("evil.so")),
                (OsString::from("DYLD_FOO"), OsString::from("bar")),
            ];

            let keys = env_keys_with_prefix(vars, b"LD_PRELOAD");
            assert_eq!(keys.len(), 1);
            assert_eq!(keys[0].as_os_str(), ld_preload);
        }

        #[test]
        fn loader_injection_prefixes_cover_linux_and_macos_loader_controls() {
            let vars = vec![
                (OsString::from("LD_PRELOAD"), OsString::from("a")),
                (OsString::from("LD_PRELOAD_32"), OsString::from("b")),
                (OsString::from("LD_PRELOAD_64"), OsString::from("c")),
                (OsString::from("LD_AUDIT"), OsString::from("d")),
                (OsString::from("LD_LIBRARY_PATH"), OsString::from("e")),
                (OsString::from("DYLD_INSERT_LIBRARIES"), OsString::from("f")),
                (OsString::from("DYLD_PRINT_LIBRARIES"), OsString::from("g")),
                (OsString::from("PATH"), OsString::from("h")),
            ];

            let stripped: Vec<OsString> = LOADER_INJECTION_ENV_PREFIXES
                .iter()
                .flat_map(|prefix| env_keys_with_prefix(vars.clone(), prefix))
                .collect();

            assert_eq!(
                stripped,
                vec![
                    OsString::from("LD_PRELOAD"),
                    OsString::from("LD_PRELOAD_32"),
                    OsString::from("LD_PRELOAD_64"),
                    OsString::from("LD_AUDIT"),
                    OsString::from("LD_LIBRARY_PATH"),
                    OsString::from("DYLD_INSERT_LIBRARIES"),
                    OsString::from("DYLD_PRINT_LIBRARIES"),
                ],
                "all dynamic-loader control variables should be stripped"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disable_process_dumping_marks_process_non_dumpable() {
        disable_process_dumping().expect("prctl(PR_SET_DUMPABLE, 0) should succeed");
        // This flips a flag on the test process itself; nothing else in the
        // test harness depends on being dumpable, so it is safe to assert on.
        // SAFETY: same argument-free `prctl` idiom as `disable_process_dumping`,
        // reading back the flag it just set.
        let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0) };
        assert_eq!(dumpable, 0, "process should be non-dumpable");
    }
}
