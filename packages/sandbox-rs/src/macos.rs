use super::{
    HashMap, Path, PathBuf, SANDBOX_ENV_VAR, SEATBELT_BASE_POLICY, SEATBELT_EXECUTABLE,
    SEATBELT_NETWORK_POLICY, SandboxPolicy, SandboxResult,
};
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use tokio::process::{Child, Command};

/// Compare native path prefixes without treating case-sensitive volumes as
/// case-insensitive. Check each differing component in its parent filesystem:
/// mounts can change case behavior partway through a path.
pub(super) fn path_starts_with(path: &Path, root: &Path) -> std::io::Result<bool> {
    let mut actual = path.components();
    let mut parent = PathBuf::new();
    for expected in root.components() {
        let Some(component) = actual.next() else {
            return Ok(false);
        };
        if component != expected
            && (!component
                .as_os_str()
                .as_bytes()
                .eq_ignore_ascii_case(expected.as_os_str().as_bytes())
                || !is_case_insensitive_filesystem(&parent)?)
        {
            return Ok(false);
        }
        parent.push(expected.as_os_str());
    }
    Ok(true)
}

fn is_case_insensitive_filesystem(path: &Path) -> std::io::Result<bool> {
    // New metadata paths inherit their nearest existing parent's semantics.
    for ancestor in path.ancestors() {
        let path = CString::new(ancestor.as_os_str().as_bytes())?;
        // SAFETY: path is NUL-terminated; __error addresses this thread's errno.
        // Reset errno to distinguish an unsupported pathconf name from failure.
        let result = unsafe {
            *libc::__error() = 0;
            libc::pathconf(path.as_ptr(), libc::_PC_CASE_SENSITIVE)
        };
        match result {
            0 => return Ok(true),
            1 => return Ok(false),
            -1 => {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(0) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "filesystem case sensitivity is unavailable",
                    ));
                }
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
            value => {
                return Err(std::io::Error::other(format!(
                    "unexpected filesystem case sensitivity: {value}"
                )));
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no existing filesystem parent",
    ))
}

/// Get Darwin user cache directory via confstr.
///
/// This function uses FFI (Foreign Function Interface) to call the C
/// standard library function `confstr()` to retrieve the macOS user cache
/// directory path. This directory is used for network-related caching.
///
/// # FFI and Unsafe Rust
///
/// Rust's FFI allows calling C functions, but requires `unsafe` blocks
/// because the compiler cannot verify memory safety across the language
/// boundary. This function demonstrates proper unsafe usage:
///
/// 1. Create a buffer sized to `PATH_MAX` + 1 (max path length on Unix)
/// 2. Call `confstr()` to write the path to the buffer
/// 3. Check the return value (0 indicates error)
/// 4. Convert the C string (null-terminated) to Rust String
/// 5. Canonicalize the path to resolve symlinks
///
/// # Safety Justification
///
/// The unsafe blocks are safe because:
/// 1. The buffer is sized to `PATH_MAX+1`, which is sufficient for any path
/// 2. confstr writes a null-terminated string to the buffer
/// 3. We check the return value before using the buffer
/// 4. `CStr::from_ptr` is given a valid null-terminated buffer that lives
///    for the duration of the call
///
/// # Return Value
///
/// Returns Some(PathBuf) if the cache directory is found, None otherwise.
/// This follows Rust's Option pattern for optional values instead of
/// returning null pointers like C would.
fn get_darwin_user_cache_dir() -> Option<PathBuf> {
    let mut buf = vec![0_i8; (libc::PATH_MAX as usize) + 1];
    // SAFETY: buf is properly sized and mutable. confstr returns 0 on error.
    let len =
        unsafe { libc::confstr(libc::_CS_DARWIN_USER_CACHE_DIR, buf.as_mut_ptr(), buf.len()) };
    if len == 0 {
        return None;
    }
    // SAFETY: confstr writes a null-terminated C string to buf.
    // buf lives for the duration of this call.
    let cstr = unsafe { CStr::from_ptr(buf.as_ptr()) };
    cstr.to_str()
        .ok()
        .map(PathBuf::from)
        .and_then(|p| dunce::canonicalize(&p).ok())
}

/// Build Seatbelt command arguments
///
/// This function generates the complete argument list for sandbox-exec based
/// on the provided `SandboxPolicy`. The result is a Vec<String> that can be
/// passed to `Command::args()`.
///
/// # Arguments
///
/// - `command`: The command and arguments to execute (e.g., `["ls", "-la"]`)
/// - `policy`: The security policy to enforce
/// - `cwd`: Current working directory (needed for writable roots calculation)
///
/// # Return Value
///
/// Returns a Vec<String> structured as:
/// ```text
/// ["-p", "<SBPL_POLICY>", "-DPARAM1=value1", "-DPARAM2=value2", "--", "command", "arg1", "arg2"]
/// ```
///
/// # SBPL Policy Generation
///
/// The function constructs the SBPL policy by concatenating:
/// 1. `SEATBELT_BASE_POLICY` (always included)
/// 2. File read policy (if policy allows disk reads)
/// 3. File write policy (generated from `writable_roots`)
/// 4. `SEATBELT_NETWORK_POLICY` (if `network_access` is true)
///
/// # Parameter Substitution
///
/// Seatbelt supports parameterized policies via -D flags:
/// - `-DWRITABLE_ROOT_0=/workspace` defines a parameter named `WRITABLE_ROOT_0`
/// - In SBPL: `(subpath (param "WRITABLE_ROOT_0"))` references the parameter
///
/// This approach:
/// - Avoids string injection vulnerabilities
/// - Allows sandbox-exec to canonicalize paths
/// - Keeps the policy generation logic separate from path values
///
/// # Path Canonicalization
///
/// All paths are canonicalized (resolved to absolute paths without symlinks)
/// before being passed as parameters. This prevents sandbox escapes via
/// symlinks that point outside the allowed directories.
///
/// # Example
///
/// ```rust,ignore
/// let policy = SandboxPolicy::WorkspaceWrite {
///     writable_roots: vec!["/tmp".into()],
///     network_access: false,
///     exclude_tmpdir_env_var: true,
///     exclude_slash_tmp: true,
/// };
///
/// let args = create_seatbelt_args(
///     vec!["ls".into(), "-la".into()],
///     &policy,
///     Path::new("/workspace"),
/// );
///
/// // Result:
/// // ["-p", "<policy>", "-DWRITABLE_ROOT_0=/tmp",
/// //  "-DWRITABLE_ROOT_1=/workspace", "--", "ls", "-la"]
/// ```
pub fn create_seatbelt_args(
    command: Vec<String>,
    policy: &SandboxPolicy,
    cwd: &Path,
) -> Vec<String> {
    let (file_write_policy, mut params) = if policy.has_full_disk_write_access() {
        // Full write access
        (
            r#"(allow file-write* (regex #"^/"))"#.to_string(),
            Vec::new(),
        )
    } else {
        let writable_roots = policy.get_writable_roots_with_cwd(cwd);
        let mut policies = Vec::new();
        let mut params = Vec::new();

        for (index, wr) in writable_roots.iter().enumerate() {
            let canonical_root = dunce::canonicalize(&wr.root).unwrap_or_else(|_| wr.root.clone());
            let root_param = format!("WRITABLE_ROOT_{index}");
            params.push((root_param.clone(), canonical_root));

            if wr.read_only_subpaths.is_empty() {
                policies.push(format!(r#"(subpath (param "{root_param}"))"#));
            } else {
                // Build require-not clauses for read-only subpaths
                let mut require_parts = vec![format!(r#"(subpath (param "{root_param}"))"#)];
                for (subpath_index, ro) in wr.read_only_subpaths.iter().enumerate() {
                    let canonical_ro = dunce::canonicalize(ro).unwrap_or_else(|_| ro.clone());
                    let ro_param = format!("WRITABLE_ROOT_{index}_RO_{subpath_index}");
                    require_parts.push(format!(r#"(require-not (subpath (param "{ro_param}")))"#));
                    params.push((ro_param, canonical_ro));
                }
                policies.push(format!("(require-all {} )", require_parts.join(" ")));
            }
        }

        if policies.is_empty() {
            (String::new(), params)
        } else {
            let file_write_policy = format!("(allow file-write*\n{}\n)", policies.join(" "));
            (file_write_policy, params)
        }
    };

    // Every `SandboxPolicy` variant allows reads everywhere; only writes
    // are scoped by policy (see the enum's own docs: `ReadOnly` means
    // "no writes allowed", not "no filesystem access"). This must not
    // key off `has_full_disk_read_access()` -- despite its name, that
    // method returns `false` specifically for `ReadOnly` (see its own
    // "the name is slightly misleading" doc comment), which previously
    // made this Seatbelt translation omit `(allow file-read*)` for
    // `ReadOnly` and turn it into no-access-at-all on macOS: an
    // ordinary `cat Cargo.toml` would fail under a policy documented,
    // and correctly implemented on Linux (Landlock grants read access
    // to `/` unconditionally), as read-only rather than no-access.
    let file_read_policy = "; allow read-only file operations\n(allow file-read*)";

    let network_policy = if policy.has_full_network_access() {
        SEATBELT_NETWORK_POLICY
    } else {
        ""
    };

    // Add Darwin cache dir if available
    if let Some(cache_dir) = get_darwin_user_cache_dir() {
        params.push(("DARWIN_USER_CACHE_DIR".to_string(), cache_dir));
    }

    let full_policy = format!(
        "{SEATBELT_BASE_POLICY}\n{file_read_policy}\n{file_write_policy}\n{network_policy}"
    );

    let mut args = vec!["-p".to_string(), full_policy];

    // Add parameter definitions
    for (key, value) in params {
        args.push(format!("-D{key}={}", value.to_string_lossy()));
    }

    args.push("--".to_string());
    args.extend(command);

    args
}

/// Spawn a command under Seatbelt sandbox
///
/// This function is the entry point for spawning sandboxed commands on macOS.
/// It uses Tokio's async process spawning for non-blocking operation.
///
/// # Arguments
///
/// - `command`: Command and arguments (e.g., vec!["ls", "-la"])
/// - `cwd`: Working directory for the sandboxed process
/// - `policy`: Security policy to enforce
/// - `env`: Environment variables to pass to the process
///
/// # Process Spawning
///
/// The function:
/// 1. Generates Seatbelt arguments via `create_seatbelt_args()`
/// 2. Adds `SANDBOX_ENV_VAR=seatbelt` to environment
/// 3. Spawns sandbox-exec with:
///    - stdin/stdout/stderr as pipes (for parent communication)
///    - Current directory set to cwd
///    - Custom environment variables
///
/// # Tokio Process
///
/// Uses `tokio::process::Command` instead of `std::process::Command` to
/// enable async I/O operations. This allows the caller to await the process
/// without blocking the async runtime.
///
/// # Return Value
///
/// Returns `SandboxResult`<Child> where Child is a `tokio::process::Child` handle.
/// The caller can use this handle to:
/// - Read stdout/stderr via `child.stdout.take()`
/// - Write to stdin via `child.stdin.take()`
/// - Wait for completion via child.wait().await
/// - Kill the process via child.kill().await
///
/// # Error Handling
///
/// Returns `SandboxError::SpawnFailed` if `Command::spawn()` fails. This can
/// happen if:
/// - sandbox-exec binary doesn't exist
/// - Invalid SBPL policy syntax
/// - Permission denied
/// - Command in the command vec doesn't exist
pub async fn spawn_under_seatbelt(
    command: Vec<String>,
    cwd: PathBuf,
    policy: &SandboxPolicy,
    mut env: HashMap<String, String>,
) -> SandboxResult<Child> {
    let args = create_seatbelt_args(command, policy, &cwd);
    env.insert(SANDBOX_ENV_VAR.to_string(), "seatbelt".to_string());

    let child = Command::new(SEATBELT_EXECUTABLE)
        .args(&args)
        .current_dir(cwd)
        // `Command` inherits the parent's full environment by default;
        // without `env_clear()` here, `env` (the caller's filtered map,
        // e.g. `resolve_shell_environment`'s output with secrets like
        // `OPENAI_API_KEY`/`GITHUB_TOKEN` stripped) would only be
        // overlaid on top of -- not replace -- every variable Maestro
        // itself inherited, silently undoing that filtering for every
        // "sandboxed" command. The unsandboxed bash path already does
        // this (see `tools/bash/mod.rs`); sandboxed spawns must match.
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    Ok(child)
}
