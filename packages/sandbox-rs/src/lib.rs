//! Native Sandboxing for Command Execution
//!
//! Ported from OpenAI Codex (MIT License) sandbox implementation.
//! Provides OS-native sandboxing for tool execution with platform-specific mechanisms:
//!
//! - **macOS**: Seatbelt (sandbox-exec) with SBPL policies
//! - **Linux**: Landlock LSM + seccomp BPF filters
//! - **Other platforms**: Returns UnsupportedPlatform error
//!
//! -------------------------------------------------------------------------
//! Architecture Overview
//! -------------------------------------------------------------------------
//!
//! This module uses Rust's conditional compilation feature (#[cfg(...)]) to
//! provide platform-specific sandboxing implementations while maintaining a
//! unified public API. The architecture consists of three layers:
//!
//! 1. **Policy Layer**: Platform-agnostic SandboxPolicy enum that describes
//!    security constraints (read-only, workspace write, full access).
//!
//! 2. **Platform Modules**: Separate implementation modules for macOS and Linux,
//!    each implementing the policy in their native sandbox technology.
//!
//! 3. **Public API**: Cross-platform functions (spawn_sandboxed_command, etc.)
//!    that dispatch to the appropriate platform module at compile time.
//!
//! -------------------------------------------------------------------------
//! Conditional Compilation (#[cfg(...)])
//! -------------------------------------------------------------------------
//!
//! Rust's #[cfg] attribute enables compile-time feature gating. This module
//! uses it extensively to include platform-specific code only when building
//! for that target OS:
//!
//! - `#[cfg(target_os = "macos")]` - Only compiled on macOS
//! - `#[cfg(target_os = "linux")]` - Only compiled on Linux
//! - `#[cfg(not(any(...)))]` - Compiled when neither macOS nor Linux
//!
//! The conditional compilation ensures:
//! - No runtime overhead from unused platform code
//! - Dependencies are only included for relevant platforms (landlock on Linux,
//!   seccompiler on Linux, libc::confstr on macOS)
//! - Type-safe compile-time guarantees that platform-specific code won't
//!   accidentally run on the wrong OS
//!
//! Example:
//! ```rust,ignore
//! #[cfg(target_os = "macos")]
//! mod macos {
//!     // This entire module is only compiled on macOS builds
//!     // Other platforms won't see this code at all
//! }
//! ```
//!
//! -------------------------------------------------------------------------
//! macOS: Seatbelt Sandbox
//! -------------------------------------------------------------------------
//!
//! Seatbelt is macOS's application sandboxing mechanism based on the TrustedBSD
//! Mandatory Access Control (MAC) framework. It uses Scheme-like policy files
//! (SBPL - Sandbox Profile Language) to define allowed operations.
//!
//! Process spawning flow:
//! 1. Generate SBPL policy from SandboxPolicy struct
//! 2. Invoke /usr/bin/sandbox-exec with -p <policy>
//! 3. sandbox-exec applies MAC rules and then executes the target command
//!
//! The SBPL policy starts with "deny default" and selectively allows:
//! - Process operations (fork, exec, signal)
//! - Filesystem read/write based on policy
//! - Network access if policy permits
//! - System calls via sysctl whitelist
//!
//! Key implementation details:
//! - Uses Command::new(SEATBELT_EXECUTABLE) to spawn sandbox-exec
//! - Policy parameters are passed as -D<key>=<value> arguments
//! - Canonicalize paths to prevent symlink escapes
//!
//! -------------------------------------------------------------------------
//! Linux: Landlock + seccomp
//! -------------------------------------------------------------------------
//!
//! Linux sandboxing combines two kernel security modules:
//!
//! **Landlock LSM (Linux Security Module)**
//! - Kernel 5.13+ filesystem access control
//! - Unprivileged process self-restriction (no root required)
//! - Path-based access control (read/write permissions per directory)
//!
//! Landlock works by creating a "ruleset" with allowed filesystem access:
//! 1. Create ruleset with access rights (read-only or read-write)
//! 2. Add path rules for allowed directories
//! 3. Call restrict_self() to apply rules to current thread
//! 4. All future operations are restricted by these rules
//!
//! **seccomp BPF (Berkeley Packet Filter)**
//! - System call filtering at kernel level
//! - Used here to block network-related syscalls (connect, bind, listen, etc.)
//! - Allows AF_UNIX sockets but blocks AF_INET/AF_INET6
//! - Returns EPERM for blocked syscalls
//!
//! Process spawning flow:
//! 1. Clone SandboxPolicy and cwd for the pre_exec closure
//! 2. Use Command::pre_exec() to apply sandbox before exec()
//! 3. In pre_exec hook (child process, after fork, before exec):
//!    a. Apply seccomp filter if network is disabled
//!    b. Apply Landlock rules for filesystem restrictions
//! 4. exec() the target command (now sandboxed)
//!
//! SAFETY: pre_exec runs in a forked child process before exec. It must only
//! use async-signal-safe operations. Both Landlock and seccomp syscalls are
//! async-signal-safe, making this pattern safe.
//!
//! -------------------------------------------------------------------------
//! Error Handling
//! -------------------------------------------------------------------------
//!
//! This module uses Rust's Result type with a custom SandboxError enum.
//! The thiserror crate generates Display implementations automatically:
//!
//! - SandboxError::UnsupportedPlatform - Returned on Windows, BSD, etc.
//! - SandboxError::SpawnFailed(io::Error) - Process creation failure
//! - SandboxError::LandlockRestrict - Landlock restriction failed
//! - SandboxError::SeccompFailed(String) - seccomp filter application failed
//! - SandboxError::SeatbeltFailed(String) - Seatbelt execution failed
//!
//! The #[from] attribute on SpawnFailed enables automatic conversion from
//! io::Error using the ? operator:
//! ```rust,ignore
//! let child = Command::new(...).spawn()?; // io::Error -> SandboxError
//! ```
//!
//! -------------------------------------------------------------------------
//! Usage Example
//! -------------------------------------------------------------------------
//!
//! ```rust,ignore
//! use maestro_sandbox::{SandboxPolicy, spawn_sandboxed_command};
//!
//! // Define security policy
//! let policy = SandboxPolicy::WorkspaceWrite {
//!     writable_roots: vec!["/tmp".into()],
//!     network_access: false,
//!     exclude_tmpdir_env_var: false,
//!     exclude_slash_tmp: false,
//! };
//!
//! // Spawn sandboxed command (automatically uses Seatbelt or Landlock)
//! let child = spawn_sandboxed_command(
//!     vec!["ls".into(), "-la".into()],
//!     std::env::current_dir().unwrap(),
//!     &policy,
//!     HashMap::new(),
//! ).await?;
//!
//! // Wait for completion
//! let status = child.wait().await?;
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

// ─────────────────────────────────────────────────────────────
// Sandbox Policy Types
// ─────────────────────────────────────────────────────────────

/// Defines the sandbox restrictions for command execution
///
/// This enum uses Rust's powerful enum variant syntax to represent different
/// security policies. Each variant can carry associated data (like `WorkspaceWrite`'s
/// configuration fields).
///
/// # Serde Serialization
///
/// The `#[serde(rename_all = "kebab-case")]` attribute transforms variant names
/// from `PascalCase` to kebab-case for JSON serialization:
/// - `DangerFullAccess` -> `"danger-full-access"`
/// - `WorkspaceWrite` -> `"workspace-write"`
///
/// # Variants
///
/// - **`DangerFullAccess`**: Unrestricted access to filesystem, network, and all
///   system resources. Should only be used for trusted commands or when the
///   sandbox causes compatibility issues.
///
/// - **`ReadOnly`**: Filesystem is read-only everywhere. No writes permitted,
///   no network access. Useful for static analysis tools or read-only queries.
///
/// - **`WorkspaceWrite`**: The recommended default. Allows reads everywhere but
///   restricts writes to:
///   - The current working directory (cwd)
///   - Explicitly listed `writable_roots`
///   - /tmp (unless excluded)
///   - $TMPDIR (unless excluded or same as /tmp)
///
///   The .git directory within cwd is automatically marked read-only to prevent
///   accidental repository corruption.
///
/// # Platform Translation
///
/// This platform-agnostic policy is translated to:
/// - **macOS**: Seatbelt SBPL rules (allow/deny filesystem operations)
/// - **Linux**: Landlock `path_beneath` rules (read/write access per directory)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxPolicy {
    /// No restrictions whatsoever. Use with extreme caution.
    DangerFullAccess,

    /// Read-only access to the entire filesystem. No writes allowed.
    ReadOnly,

    /// Read-only by default, but allows writes to specific directories.
    ///
    /// This variant demonstrates Rust's struct-like enum variants, where each
    /// variant can have named fields. The #[serde(default)] attribute means
    /// these fields are optional in JSON and will use their type's Default
    /// implementation if omitted.
    WorkspaceWrite {
        /// Directories that should be writable (in addition to cwd)
        ///
        /// Paths will be canonicalized to absolute paths to prevent symlink
        /// escapes. If a path cannot be canonicalized (doesn't exist yet),
        /// the original path is used.
        #[serde(default)]
        writable_roots: Vec<PathBuf>,

        /// Whether outbound network access is allowed
        ///
        /// When false:
        /// - macOS: Omits network policy from Seatbelt SBPL
        /// - Linux: Applies seccomp filter blocking socket syscalls
        #[serde(default)]
        network_access: bool,

        /// Exclude TMPDIR environment variable from writable roots
        ///
        /// By default, $TMPDIR is added to writable roots (unless it equals /tmp).
        /// Set this to true to deny writes to $TMPDIR.
        #[serde(default)]
        exclude_tmpdir_env_var: bool,

        /// Exclude /tmp from writable roots
        ///
        /// By default, /tmp is writable for temporary files. Set this to true
        /// to deny writes to /tmp.
        #[serde(default)]
        exclude_slash_tmp: bool,
    },
}

impl Default for SandboxPolicy {
    /// Returns the recommended default: `WorkspaceWrite` with no network access
    ///
    /// This implementation of the Default trait provides a sensible default
    /// policy that balances security and functionality:
    /// - Allows writes to cwd, /tmp, and $TMPDIR
    /// - No network access
    /// - Read access to entire filesystem
    ///
    /// This is the conservative library default used by callers that don't
    /// have a specific product surface in mind. Interactive and exec sessions
    /// should use [`SandboxPolicy::workspace_write_default`] instead, which
    /// additionally allows network access and a curated set of package-manager
    /// cache directories — see that function's docs for the measurement
    /// behind the difference.
    fn default() -> Self {
        Self::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        }
    }
}

impl SandboxPolicy {
    /// Writable roots that toolchains need in order to build inside the
    /// sandbox: a single per-session cache directory under the system temp
    /// dir, created on demand.
    ///
    /// [`sandbox_cache_env`] points every toolchain cache variable at
    /// subdirectories of this root, so `cargo`, `npm`, `pip`, `go`, and the
    /// rest write their caches here instead of into the user's real caches.
    /// The root is per-session, so a session's builds stay warm.
    ///
    /// # Why the real caches are no longer granted
    ///
    /// This function used to return `$CARGO_HOME/registry`, `$CARGO_HOME/git`,
    /// `$CARGO_HOME/.package-cache`, `$CARGO_HOME/.global-cache`,
    /// `$XDG_CACHE_HOME/pip`, `$XDG_CACHE_HOME/uv`, `~/.npm/_cacache`, and
    /// `~/.npm/_logs`, and pre-create the missing ones. Those are the caches a
    /// *later, unsandboxed* build reads. A build script (`build.rs`, an npm
    /// install script, `setup.py`) running inside the sandbox could write a
    /// poisoned artifact into one of them and have it executed outside the
    /// sandbox on the next build. Redirecting the caches removes that path.
    ///
    /// The real caches stay *readable*: every policy allows reads everywhere
    /// and restricts only writes, so a toolchain can still read a populated
    /// `~/.cargo/registry`. It cannot write to it, so the first fetch in a
    /// sandboxed session repopulates the redirected cache instead — see the
    /// cold-cache cost noted on [`sandbox_cache_env`].
    #[must_use]
    pub fn dev_cache_writable_roots() -> Vec<PathBuf> {
        prepare_sandbox_cache_root(sandbox_cache_session_id())
            .map(|root| vec![root])
            .unwrap_or_default()
    }

    /// The default sandbox policy for interactive and exec sessions that do
    /// not explicitly opt out.
    ///
    /// This differs from [`SandboxPolicy::default`] (the conservative library
    /// default) in two measured ways:
    ///
    /// - **`network_access: true`.** Agentic coding sessions routinely need
    ///   outbound network access: `cargo`/`npm` fetching dependencies from a
    ///   registry, `git push`/`git fetch` against a remote, and calling the
    ///   model provider API itself. A `network_access: false` default was
    ///   measured to break all three immediately, which is exactly the
    ///   "users disable it globally on day one" failure mode a sandbox
    ///   default must avoid. This does mean network exfiltration is not
    ///   contained by the default policy — that is a deliberate, documented
    ///   trade-off, not an oversight. Structured allowlists are conservatively
    ///   mapped to no network access until the native policy supports them.
    /// - **`writable_roots: dev_cache_writable_roots()`.** Without these,
    ///   `cargo build` fails to fetch new dependencies (see that function's
    ///   docs).
    ///
    /// Filesystem *writes* outside the workspace/tmp/cache roots remain
    /// contained, which is what actually stops the two demonstrated
    /// allowlist bypasses (`find -fprintf ~/.ssh/authorized_keys`-style
    /// writes and `LD_PRELOAD`-injected writes to `~/.bashrc`-style targets):
    /// both are filesystem-write attacks, not network-exfiltration attacks.
    #[must_use]
    pub fn workspace_write_default() -> Self {
        Self::WorkspaceWrite {
            writable_roots: Self::dev_cache_writable_roots(),
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        }
    }
}

/// Directory name under the system temp dir holding redirected sandbox caches.
const SANDBOX_CACHE_DIR: &str = "maestro-sandbox-cache";

/// Stale process caches are reclaimed whenever a Maestro process prepares its
/// own cache. A live process is never removed, even when it runs longer than
/// this interval; the age is the fallback for incomplete directories which
/// never acquired a process marker.
const SANDBOX_CACHE_STALE_AFTER: std::time::Duration = std::time::Duration::from_hours(24);

/// Bound the number of exited-process caches retained for warm diagnostics.
const SANDBOX_CACHE_RETAINED_INACTIVE: usize = 2;

const SANDBOX_CACHE_PROCESS_MARKER: &str = ".maestro-process";

/// The cache session id for this process, generated once on first use.
///
/// One id per process keeps a session's toolchain caches warm across every
/// command it runs, and keeps them separate from any other Maestro process on
/// the same host.
pub fn sandbox_cache_session_id() -> &'static str {
    static SESSION_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SESSION_ID.get_or_init(|| {
        use rand::RngExt;
        use std::fmt::Write as _;
        let bytes: [u8; 16] = rand::rng().random();
        bytes
            .iter()
            .fold(String::with_capacity(32), |mut id, byte| {
                let _ = write!(id, "{byte:02x}");
                id
            })
    })
}

/// The directory that [`sandbox_cache_env`] points toolchain caches at.
#[must_use]
pub fn sandbox_cache_root(session_id: &str) -> PathBuf {
    let temp_dir = std::env::temp_dir();
    dunce::canonicalize(&temp_dir)
        .unwrap_or(temp_dir)
        .join(SANDBOX_CACHE_DIR)
        .join(session_id)
}

fn prepare_sandbox_cache_root(session_id: &str) -> std::io::Result<PathBuf> {
    prepare_sandbox_cache_root_in(&std::env::temp_dir(), session_id)
}

fn prepare_sandbox_cache_root_in(temp_dir: &Path, session_id: &str) -> std::io::Result<PathBuf> {
    if Path::new(session_id).components().count() != 1
        || !matches!(
            Path::new(session_id).components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sandbox cache session id must be one path component",
        ));
    }

    let temp_dir = dunce::canonicalize(temp_dir)?;
    ensure_trusted_temp_directory(&temp_dir)?;
    let parent = temp_dir.join(SANDBOX_CACHE_DIR);
    ensure_private_directory(&parent)?;
    reclaim_inactive_sandbox_caches(&parent, session_id)?;

    let root = parent.join(session_id);
    ensure_private_directory(&root)?;
    write_process_marker(&root)?;
    Ok(root)
}

fn ensure_trusted_temp_directory(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "sandbox temp path is not a trusted directory: {}",
                path.display()
            ),
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let mode = metadata.mode();
        let private_to_owner = metadata.uid() == unsafe { libc::geteuid() } && mode & 0o022 == 0;
        let sticky_directory = mode & 0o1000 != 0;
        if !private_to_owner && !sticky_directory {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "sandbox temp directory must be private or sticky: {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                match builder.create(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            #[cfg(not(unix))]
            {
                match std::fs::create_dir(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Err(error) => return Err(error),
    }

    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "sandbox cache path is not a trusted directory: {}",
                path.display()
            ),
        ));
    }

    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

        // Pin and inspect the directory without following a last-component
        // symlink. The fd also makes the permission migration race-free.
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = directory.metadata()?;
        let mode = metadata.mode() & 0o777;
        if metadata.uid() != unsafe { libc::geteuid() } || mode & 0o022 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "sandbox cache directory must be owned by the current user and private: {}",
                    path.display()
                ),
            ));
        }
        // Older Maestro releases created these directories with 0755. Such a
        // directory was never writable by another user, so it can be safely
        // migrated before any credentials are copied into it. Group/world
        // writable directories were rejected above and are never repaired.
        if mode != 0o700 && unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(())
}

fn write_process_marker(root: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    let marker = root.join(SANDBOX_CACHE_PROCESS_MARKER);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(marker)?;
    writeln!(file, "{}", std::process::id())
}

fn reclaim_inactive_sandbox_caches(parent: &Path, current_session: &str) -> std::io::Result<()> {
    let mut inactive = Vec::new();
    let now = std::time::SystemTime::now();

    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        if entry.file_name() == current_session {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        // Multiple Maestro/test processes reclaim this shared parent in
        // parallel. Another process may remove an inactive entry after our
        // `read_dir` snapshot but before this metadata lookup; that is already
        // the desired outcome, not a cache-preparation failure.
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let modified = metadata
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let age = now.duration_since(modified).unwrap_or_default();
        let pid = std::fs::read_to_string(entry.path().join(SANDBOX_CACHE_PROCESS_MARKER))
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok());
        // Another process may be between creating its directory and writing
        // the marker. Give incomplete directories the full stale interval so
        // concurrent startup can never reclaim an active cache.
        if pid.is_none() && age < SANDBOX_CACHE_STALE_AFTER {
            continue;
        }
        #[cfg(unix)]
        if pid.is_some_and(process_is_alive) {
            continue;
        }
        #[cfg(not(unix))]
        if age < SANDBOX_CACHE_STALE_AFTER {
            // There is no dependency-free cross-platform process probe. Keep
            // recent marked directories and reclaim them by age instead.
            continue;
        }
        inactive.push((entry.path(), modified, age));
    }

    inactive.sort_by_key(|(_, modified, _)| *modified);
    let excess = inactive
        .len()
        .saturating_sub(SANDBOX_CACHE_RETAINED_INACTIVE);
    for (index, (path, _, age)) in inactive.into_iter().enumerate() {
        if index < excess || age >= SANDBOX_CACHE_STALE_AFTER {
            if let Err(error) = std::fs::remove_dir_all(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Environment variables that redirect toolchain caches into the sandbox's own
/// cache directory.
///
/// # Why this exists
///
/// The alternative is granting the sandbox write access to the user's real
/// caches (`~/.cargo/registry`, `~/.npm/_cacache`, `~/.cache/pip`, ...), which
/// is what [`SandboxPolicy::dev_cache_writable_roots`] used to do. A build
/// script running inside the sandbox could then write a poisoned artifact into
/// a cache that a later *unsandboxed* build reads and executes. Pointing each
/// toolchain at a sandbox-private directory removes that path while leaving
/// the real caches readable.
///
/// # What is deliberately absent
///
/// `RUSTUP_HOME`, `GEM_HOME`, `VOLTA_HOME`, `PIPX_HOME`, and `DENO_DIR` are
/// not redirected because they hold installed executables. `CARGO_HOME` is
/// redirected because Cargo stores registry and Git downloads there; the
/// command's existing PATH and RUSTUP_HOME still select the installed Cargo
/// and Rust toolchain, while [`seed_toolchain_user_config`] preserves Cargo
/// configuration and credentials in the isolated home.
///
/// # Cost
///
/// The first build in a session runs against a cold cache and re-downloads
/// dependencies. Subsequent commands in the same session hit the warm
/// redirected download cache. Cargo build artifacts remain in the caller's
/// workspace-visible target directory.
#[must_use]
pub fn sandbox_cache_env(session_id: &str) -> Vec<(String, String)> {
    let root = sandbox_cache_root(session_id);
    let entry = |name: &str, subdir: &str| {
        (
            name.to_string(),
            root.join(subdir).to_string_lossy().into_owned(),
        )
    };
    vec![
        // npm
        entry("NPM_CONFIG_CACHE", "npm"),
        // pnpm
        entry("npm_config_store_dir", "pnpm-store"),
        // Go
        entry("GOCACHE", "go-build"),
        entry("GOMODCACHE", "go-mod"),
        // Cargo downloads. Build artifacts remain workspace-visible.
        entry("CARGO_HOME", "cargo-home"),
        // Python
        entry("PIP_CACHE_DIR", "pip"),
        entry("UV_CACHE_DIR", "uv"),
        entry("POETRY_CACHE_DIR", "poetry"),
        entry("CONDA_PKGS_DIRS", "conda"),
        // Bun
        entry("BUN_INSTALL_CACHE_DIR", "bun"),
        // Yarn
        entry("YARN_CACHE_FOLDER", "yarn"),
        entry("YARN_GLOBAL_FOLDER", "yarn-global"),
        // node-gyp
        entry("npm_config_devdir", "node-gyp"),
        // Browser automation downloads
        entry("PLAYWRIGHT_BROWSERS_PATH", "playwright"),
        entry("PUPPETEER_CACHE_DIR", "puppeteer"),
        entry("CYPRESS_CACHE_FOLDER", "cypress"),
        // JS monorepo build caches
        entry("TURBO_CACHE_DIR", "turbo"),
        entry("NX_CACHE_DIRECTORY", "nx"),
        // JVM
        entry("GRADLE_USER_HOME", "gradle"),
        // Ruby: spec cache and bundle path only; GEM_HOME holds executables.
        entry("GEM_SPEC_CACHE", "gem-specs"),
        entry("BUNDLE_PATH", "bundle"),
        // PHP
        entry("COMPOSER_CACHE_DIR", "composer"),
        // macOS package manager
        entry("HOMEBREW_CACHE", "homebrew"),
        // .NET
        entry("NUGET_PACKAGES", "nuget"),
        // C/C++
        entry("CCACHE_DIR", "ccache"),
        // iOS
        entry("CP_HOME_DIR", "cocoapods"),
    ]
}

/// Overlay [`sandbox_cache_env`] onto a command environment.
///
/// The redirected download-cache values win over anything the caller inherited.
/// `CARGO_TARGET_DIR` is removed so Cargo uses its normal workspace-local
/// target directory rather than a host path the sandbox cannot safely write.
/// Maven has no dedicated cache environment variable, so its local repository
/// override is appended to `MAVEN_OPTS` instead.
pub fn apply_sandbox_cache_env(
    mut env: HashMap<String, String>,
    session_id: &str,
) -> std::io::Result<HashMap<String, String>> {
    let root = prepare_sandbox_cache_root(session_id)?;
    seed_toolchain_user_config(&env, &root)?;
    for (name, value) in sandbox_cache_env(session_id) {
        env.insert(name, value);
    }
    env.remove("CARGO_TARGET_DIR");
    let maven_repository = root.join("maven").to_string_lossy().into_owned();
    let maven_override = format!("-Dmaven.repo.local={maven_repository}");
    env.entry("MAVEN_OPTS".to_string())
        .and_modify(|value| {
            if !value.is_empty() {
                value.push(' ');
            }
            value.push_str(&maven_override);
        })
        .or_insert(maven_override);
    Ok(env)
}

fn seed_toolchain_user_config(env: &HashMap<String, String>, root: &Path) -> std::io::Result<()> {
    let home = env
        .get("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .or_else(dirs::home_dir);

    let cargo_source = env
        .get("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
        .or_else(|| home.as_ref().map(|home| home.join(".cargo")));
    if let Some(source) = cargo_source {
        let destination = root.join("cargo-home");
        ensure_private_directory(&destination)?;
        copy_named_config_files(
            &source,
            &destination,
            &["config", "config.toml", "credentials", "credentials.toml"],
        )?;
    }

    let gradle_source = env
        .get("GRADLE_USER_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("GRADLE_USER_HOME").map(PathBuf::from))
        .or_else(|| home.map(|home| home.join(".gradle")));
    if let Some(source) = gradle_source {
        let destination = root.join("gradle");
        ensure_private_directory(&destination)?;
        copy_named_config_files(
            &source,
            &destination,
            &["gradle.properties", "init.gradle", "init.gradle.kts"],
        )?;
        copy_config_directory(&source.join("init.d"), &destination.join("init.d"))?;
    }
    Ok(())
}

fn copy_named_config_files(
    source: &Path,
    destination: &Path,
    names: &[&str],
) -> std::io::Result<()> {
    for name in names {
        let source_file = source.join(name);
        let destination_file = destination.join(name);
        match std::fs::symlink_metadata(&source_file) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => {
                remove_path_if_present(&destination_file)?;
                std::fs::copy(&source_file, destination_file)?;
            }
            Ok(_) => remove_path_if_present(&destination_file)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remove_path_if_present(&destination_file)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn copy_config_directory(source: &Path, destination: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            remove_path_if_present(destination)?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        remove_path_if_present(destination)?;
        return Ok(());
    }
    ensure_private_directory(destination)?;
    let mut copied = std::collections::HashSet::new();
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        copied.insert(entry.file_name());
        let destination_file = destination.join(entry.file_name());
        remove_path_if_present(&destination_file)?;
        std::fs::copy(entry.path(), destination_file)?;
    }
    for entry in std::fs::read_dir(destination)? {
        let entry = entry?;
        if !copied.contains(&entry.file_name()) {
            remove_path_if_present(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_path_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)
        }
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Represents a writable directory with optional read-only subdirectories
///
/// This struct is used internally when translating `SandboxPolicy` to platform-specific
/// sandbox rules. It enables fine-grained control like "allow writes to /workspace
/// but deny writes to /workspace/.git".
///
/// # Platform Usage
///
/// - **macOS**: Converted to Seatbelt (require-all (subpath root) (require-not (subpath ro)))
/// - **Linux (stage-1)**: roots with exclusions expand to existing non-excluded
///   children for full RW, plus Make*/Remove* on the root itself. Landlock
///   grants are OR'd within a layer (there is no "most specific wins" /
///   deny-rule), so a RW grant on the root necessarily covers `.git`. Stage-1
///   never grants WriteFile on the root, which keeps `.git` unwritable at the
///   cost of new root children staying empty on content write (MakeReg may
///   still create the name). True stage-2 (create+write new root children
///   without `.git` write) needs bind-mount or a non-`path_beneath` design.
#[derive(Debug, Clone)]
pub struct WritableRoot {
    /// The root directory that should be writable
    pub root: PathBuf,

    /// Subdirectories within root that should remain read-only
    ///
    /// Example: root=/workspace, `read_only_subpaths`=[/workspace/.git]
    /// Result: Can write to /workspace/src but not /workspace/.git/
    pub read_only_subpaths: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// Kebab-case label for this policy's variant, matching the
    /// `MAESTRO_SANDBOX_MODE`/`sandbox_mode` config grammar
    /// (`"danger-full-access"` / `"read-only"` / `"workspace-write"`; see
    /// `parse_sandbox_mode_env_override` in `config.rs`).
    ///
    /// Intended for user-facing messages that need to name the *actual*
    /// active policy rather than assuming one -- e.g. a sandboxed command's
    /// failure guidance. Hard-coding a mode name in that kind of message is
    /// a trap: `MAESTRO_SANDBOX_MODE=read-only` is a real, documented
    /// escape hatch, and a message that always says "workspace-write"
    /// regardless of the actual policy misleads a `ReadOnly` session's user
    /// into thinking in-workspace writes should have worked.
    #[must_use]
    pub fn mode_label(&self) -> &'static str {
        match self {
            Self::DangerFullAccess => "danger-full-access",
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite { .. } => "workspace-write",
        }
    }

    /// Check if policy allows full disk write access
    ///
    /// Returns true only for `DangerFullAccess` variant.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let policy = SandboxPolicy::DangerFullAccess;
    /// assert!(policy.has_full_disk_write_access());
    ///
    /// let policy = SandboxPolicy::default();
    /// assert!(!policy.has_full_disk_write_access());
    /// ```
    #[must_use]
    pub fn has_full_disk_write_access(&self) -> bool {
        matches!(self, Self::DangerFullAccess)
    }

    /// Check if policy allows full disk read access
    ///
    /// Returns true for `DangerFullAccess` and `WorkspaceWrite` (which allows
    /// reads everywhere). Only `ReadOnly` variant restricts reads.
    ///
    /// Note: The name is slightly misleading - `ReadOnly` doesn't restrict
    /// reads, it just denies writes.
    #[must_use]
    pub fn has_full_disk_read_access(&self) -> bool {
        !matches!(self, Self::ReadOnly)
    }

    /// Check if policy allows network access
    ///
    /// # Pattern Matching
    ///
    /// This method demonstrates Rust's match expression for extracting data
    /// from enum variants:
    /// - Use `..` to ignore other fields in `WorkspaceWrite` variant
    /// - Dereference `network_access` with * to get bool value
    #[must_use]
    pub fn has_full_network_access(&self) -> bool {
        match self {
            Self::DangerFullAccess => true,
            Self::ReadOnly => false,
            Self::WorkspaceWrite { network_access, .. } => *network_access,
        }
    }

    /// Get writable roots including cwd and optionally TMPDIR/tmp
    ///
    /// This method computes the complete list of writable directories based on
    /// the policy configuration and current working directory.
    ///
    /// # Behavior by Policy Type
    ///
    /// - **`DangerFullAccess`**: Returns empty vec (everything writable)
    /// - **`ReadOnly`**: Returns empty vec (nothing writable)
    /// - **`WorkspaceWrite`**: Returns vec containing:
    ///   1. User-specified `writable_roots`
    ///   2. /tmp (unless `exclude_slash_tmp` is true)
    ///   3. $TMPDIR (unless `exclude_tmpdir_env_var` is true or equals /tmp)
    ///   4. Current working directory with .git as read-only subpath
    ///
    /// # .git Protection
    ///
    /// The .git directory is automatically marked read-only to prevent:
    /// - Accidental corruption of git metadata
    /// - Sandbox escape via git hooks
    /// - Loss of version control data
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let policy = SandboxPolicy::WorkspaceWrite {
    ///     writable_roots: vec![PathBuf::from("/custom")],
    ///     network_access: false,
    ///     exclude_tmpdir_env_var: false,
    ///     exclude_slash_tmp: false,
    /// };
    ///
    /// let roots = policy.get_writable_roots_with_cwd(Path::new("/workspace"));
    /// // Returns: [/custom, /tmp, $TMPDIR, /workspace (with .git read-only)]
    /// ```
    #[must_use]
    pub fn get_writable_roots_with_cwd(&self, cwd: &Path) -> Vec<WritableRoot> {
        let mut roots = Vec::new();

        match self {
            Self::DangerFullAccess => {
                // Everything is writable
            }
            Self::ReadOnly => {
                // Nothing is writable
            }
            Self::WorkspaceWrite {
                writable_roots,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
                ..
            } => {
                // Add user-specified roots
                for root in writable_roots {
                    roots.push(WritableRoot {
                        root: root.clone(),
                        read_only_subpaths: Vec::new(),
                    });
                }

                // Add /tmp unless excluded
                if !exclude_slash_tmp {
                    roots.push(WritableRoot {
                        root: PathBuf::from("/tmp"),
                        read_only_subpaths: Vec::new(),
                    });
                }

                // Add TMPDIR unless excluded
                if !exclude_tmpdir_env_var {
                    if let Ok(tmpdir) = std::env::var("TMPDIR") {
                        let tmpdir_path = PathBuf::from(tmpdir);
                        if tmpdir_path.as_path() != Path::new("/tmp") {
                            roots.push(WritableRoot {
                                root: tmpdir_path,
                                read_only_subpaths: Vec::new(),
                            });
                        }
                    }
                }

                // Add cwd with .git as read-only subpath if present
                let git_dir = cwd.join(".git");
                let read_only_subpaths = if git_dir.exists() {
                    vec![git_dir]
                } else {
                    Vec::new()
                };

                roots.push(WritableRoot {
                    root: cwd.to_path_buf(),
                    read_only_subpaths,
                });
            }
        }

        roots
    }

    /// Check whether `path` may be written under this policy, evaluated
    /// against `cwd`.
    ///
    /// # Why this exists
    ///
    /// The OS-level sandbox (Seatbelt on macOS, Landlock on Linux) only
    /// contains processes spawned through [`spawn_sandboxed_command`]. Tools
    /// that mutate files *in the Maestro process itself* — `write`, `edit`,
    /// `notebook_edit` — never go through that spawn path, so the kernel
    /// sandbox provides them no containment at all. Those tools check this
    /// method explicitly (via [`preflight_native_write`]) before any side
    /// effects and then perform the mutation through
    /// [`commit_native_write`], which revalidates this check against the
    /// directory descriptor it writes through so a path swap between check
    /// and write cannot redirect the mutation.
    ///
    /// # Behavior by policy
    ///
    /// - `DangerFullAccess`: always allowed.
    /// - `ReadOnly`: never allowed.
    /// - `WorkspaceWrite`: allowed only if `path` resolves under one of
    ///   [`Self::get_writable_roots_with_cwd`]'s roots and not under one of
    ///   that root's `read_only_subpaths` (e.g. `cwd/.git`).
    ///
    /// Paths are canonicalized with `dunce::canonicalize` (falling back to
    /// the original path when the target does not exist yet, e.g. a new
    /// file being created) so that symlinks and `..` segments cannot be
    /// used to escape a writable root.
    #[must_use]
    pub fn allows_write_to(&self, cwd: &Path, path: &Path) -> bool {
        match self {
            Self::DangerFullAccess => true,
            Self::ReadOnly => false,
            Self::WorkspaceWrite { .. } => {
                if contains_dangling_symlink(path) {
                    return false;
                }
                let candidate = canonicalize_best_effort(path);
                self.get_writable_roots_with_cwd(cwd).iter().any(|wr| {
                    let root = canonicalize_best_effort(&wr.root);
                    if !candidate.starts_with(&root) {
                        return false;
                    }
                    !wr.read_only_subpaths.iter().any(|ro| {
                        let ro = canonicalize_best_effort(ro);
                        candidate.starts_with(&ro)
                    })
                })
            }
        }
    }
}

fn contains_dangling_symlink(path: &Path) -> bool {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if std::fs::symlink_metadata(candidate).is_ok_and(|meta| meta.file_type().is_symlink())
            && std::fs::metadata(candidate).is_err()
        {
            return true;
        }
        current = candidate.parent();
    }
    false
}

/// Canonicalize `path`, falling back to canonicalizing the longest existing
/// ancestor and rejoining the remaining (not-yet-created) components when
/// `path` itself does not exist.
///
/// [`allows_write_to`](SandboxPolicy::allows_write_to) is most often asked
/// about a file the caller is about to *create* (e.g. `write` on a new
/// file), so a plain `dunce::canonicalize(path)` — which requires every
/// component to exist — would fail for the overwhelming common case. Falling
/// back to the raw, uncanonicalized path in that case is unsound on systems
/// where an *existing* ancestor is itself reached through a symlink (for
/// example macOS's `/tmp` -> `/private/tmp` and `$TMPDIR` under
/// `/var/folders` -> `/private/var/folders`): the writable root would
/// canonicalize to the `/private/...` form while the candidate path would
/// not, so `starts_with` would always fail even for legitimate in-root
/// writes. Canonicalizing the nearest existing ancestor keeps the symlink
/// resolution while still tolerating a path that doesn't exist yet.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if let Ok(canonical) = dunce::canonicalize(path) {
        return canonical;
    }

    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    while let Some(parent) = current.parent().map(Path::to_path_buf) {
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Some(name) = current.file_name() {
            suffix.push(name.to_os_string());
        }
        if let Ok(canonical_parent) = dunce::canonicalize(&parent) {
            let mut resolved = canonical_parent;
            for part in suffix.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        current = parent;
    }

    path.to_path_buf()
}

// ─────────────────────────────────────────────────────────────
// Atomic native (in-process) file writes
// ─────────────────────────────────────────────────────────────

/// Preflight check for a native (in-process) file mutation under `policy`.
///
/// Returns `Err` with a user-facing message when `policy` denies a write to
/// `path`; `Ok(())` when there is no policy or the write is allowed. This is
/// the early, good-error-message check — callers must still route the actual
/// mutation through [`commit_native_write`], which re-validates against the
/// directory it actually writes into (see its docs for why the preflight
/// alone is not sufficient).
pub fn preflight_native_write(
    policy: Option<&SandboxPolicy>,
    cwd: &Path,
    path: &Path,
) -> Result<(), String> {
    let Some(policy) = policy else {
        return Ok(());
    };
    if policy.allows_write_to(cwd, path) {
        return Ok(());
    }
    Err(format!(
        "Tool blocked by sandbox policy: {} is outside the sandbox's writable roots",
        path.display()
    ))
}

/// Atomically check-and-write `contents` to `path` under `policy`.
///
/// # Why this exists (TOCTOU)
///
/// [`preflight_native_write`] validates a *path*; the write that follows it
/// is a separate filesystem operation. A background process can swap an
/// in-workspace directory for a symlink pointing outside the writable roots
/// between the two, and a plain `fs::write` would then follow the symlink
/// and write outside the sandbox. To make check-and-write atomic, this
/// function pins the parent directory with an `O_DIRECTORY | O_NOFOLLOW`
/// descriptor, reads the descriptor's *actual* path back from the kernel,
/// runs the policy check against that, and then performs the whole mutation
/// (temp file + rename) relative to the pinned descriptor via
/// `openat`/`renameat`. Swapping the path after the descriptor is opened
/// cannot redirect the write: the directory the bytes land in is the exact
/// directory that was validated.
///
/// With `policy: None` the descriptor-relative write still happens (it is
/// also what gives the write its temp-file-plus-rename atomicity), but no
/// containment check is applied.
///
/// On non-Unix platforms this falls back to a preflight check followed by a
/// path-based temp-file write; the kernel sandbox is unsupported there
/// anyway.
pub fn commit_native_write(
    policy: Option<&SandboxPolicy>,
    cwd: &Path,
    path: &Path,
    contents: &[u8],
) -> Result<(), String> {
    commit_native_write_impl(policy, cwd, path, contents)
}

#[cfg(unix)]
fn commit_native_write_impl(
    policy: Option<&SandboxPolicy>,
    cwd: &Path,
    path: &Path,
    contents: &[u8],
) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| format!("path has no parent directory: {}", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("path has no file name: {}", path.display()))?;

    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create directory {}: {error}", parent.display()))?;

    // Pin the parent directory. O_NOFOLLOW rejects a symlink in the final
    // component; ancestor symlinks are resolved at open time and then
    // validated below via the descriptor's real path.
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)
        .map_err(|error| format!("Failed to open directory {}: {error}", parent.display()))?;
    let real_parent = fd_resolved_path(dir.as_raw_fd())
        .map_err(|error| format!("Failed to resolve directory {}: {error}", parent.display()))?;

    // The containment check runs against the directory we actually hold
    // open, so a path swap racing the earlier preflight is caught here
    // before any byte is written.
    preflight_native_write(policy, cwd, &real_parent.join(file_name))?;

    let dir_fd = dir.as_raw_fd();
    let tmp_name = format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        uuid::Uuid::new_v4()
    );
    let tmp_c = std::ffi::CString::new(tmp_name.as_str())
        .map_err(|_| "file name contains NUL byte".to_string())?;
    let final_c = std::ffi::CString::new(file_name.as_bytes())
        .map_err(|_| "file name contains NUL byte".to_string())?;

    let write_result = (|| -> std::io::Result<()> {
        // O_EXCL + O_NOFOLLOW: never follow or clobber anything that
        // already exists at the temp name.
        let raw = unsafe {
            libc::openat(
                dir_fd,
                tmp_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o644,
            )
        };
        if raw == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        if unsafe { libc::renameat(dir_fd, tmp_c.as_ptr(), dir_fd, final_c.as_ptr()) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    })();

    if let Err(error) = write_result {
        unsafe { libc::unlinkat(dir_fd, tmp_c.as_ptr(), 0) };
        return Err(format!("Failed to write file {}: {error}", path.display()));
    }
    Ok(())
}

/// Read the real, kernel-resolved path of an open descriptor back from the
/// OS. Used by [`commit_native_write`] to validate the directory it pinned
/// rather than the (raceable) path string it was opened from.
#[cfg(target_os = "linux")]
fn fd_resolved_path(fd: std::os::fd::RawFd) -> std::io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
}

#[cfg(target_os = "macos")]
fn fd_resolved_path(fd: std::os::fd::RawFd) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;

    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let len = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len])))
}

// Neither Seatbelt nor Landlock exists on other Unix targets; fall back to
// canonicalizing the pinned descriptor's open path via /proc-less means.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn fd_resolved_path(_fd: std::os::fd::RawFd) -> std::io::Result<PathBuf> {
    Err(std::io::Error::other(
        "fd path resolution unsupported on this platform",
    ))
}

#[cfg(not(unix))]
fn commit_native_write_impl(
    policy: Option<&SandboxPolicy>,
    cwd: &Path,
    path: &Path,
    contents: &[u8],
) -> Result<(), String> {
    preflight_native_write(policy, cwd, path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create directory {}: {error}", parent.display()))?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&tmp, contents)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|error| {
            let _ = std::fs::remove_file(&tmp);
            format!("Failed to write file {}: {error}", path.display())
        })
}

// ─────────────────────────────────────────────────────────────
// Sandbox Errors
// ─────────────────────────────────────────────────────────────

/// Error types for sandbox operations
///
/// This enum uses the thiserror crate to automatically derive the Error trait
/// and Display implementation. The #[error("...")] attribute defines the
/// display message for each variant.
///
/// # Error Conversion with #[from]
///
/// The #[from] attribute on `SpawnFailed` creates an automatic From<io::Error>
/// implementation, enabling the ? operator to convert `io::Error` into `SandboxError`:
///
/// ```rust,ignore
/// let child = Command::new(...).spawn()?; // io::Error auto-converts
/// ```
///
/// Without #[from], you would need to manually map errors:
/// ```rust,ignore
/// let child = Command::new(...).spawn()
///     .map_err(SandboxError::SpawnFailed)?;
/// ```
#[derive(Debug, Error)]
pub enum SandboxError {
    /// Sandbox not supported on this platform (Windows, BSD, etc.)
    #[error("Sandbox not supported on this platform")]
    UnsupportedPlatform,

    /// Process spawning failed (command not found, permission denied, etc.)
    ///
    /// The #[from] attribute enables automatic conversion from `std::io::Error`
    #[error("Failed to spawn sandboxed process: {0}")]
    SpawnFailed(#[from] std::io::Error),

    /// Landlock restriction failed (Linux only)
    ///
    /// This can occur if:
    /// - Landlock is not supported by the kernel (< 5.13)
    /// - Path rules cannot be created
    /// - `restrict_self()` syscall fails
    #[error("Landlock restriction failed")]
    LandlockRestrict,

    /// seccomp filter application failed (Linux only)
    ///
    /// Possible causes:
    /// - Unsupported architecture (not `x86_64` or aarch64)
    /// - Invalid BPF program
    /// - seccomp syscall failed
    #[error("Seccomp filter failed: {0}")]
    SeccompFailed(String),

    /// Seatbelt execution failed (macOS only)
    ///
    /// Usually indicates:
    /// - sandbox-exec binary not found
    /// - Invalid SBPL policy syntax
    /// - Permission denied
    #[error("Seatbelt execution failed: {0}")]
    SeatbeltFailed(String),
}

/// Type alias for Result with `SandboxError`
///
/// This pattern is common in Rust to reduce boilerplate. Instead of writing
/// Result<Child, `SandboxError`> everywhere, we can write `SandboxResult`<Child>.
pub type SandboxResult<T> = Result<T, SandboxError>;

// ─────────────────────────────────────────────────────────────
// Seatbelt Policy (macOS)
// ─────────────────────────────────────────────────────────────

/// Base Seatbelt policy - starts with deny-all and allows basic operations
///
/// This constant contains the Seatbelt Profile Language (SBPL) baseline policy.
/// SBPL is a Scheme-like language for defining security policies on macOS.
///
/// # Policy Structure
///
/// The policy follows a deny-by-default approach:
/// 1. `(deny default)` - Block everything by default
/// 2. `(allow ...)` - Selectively permit operations
///
/// # Allowed Operations
///
/// - **Process management**: fork, exec, signal within same sandbox
/// - **Basic I/O**: Read user preferences, write to /dev/null
/// - **System info**: Read hardware info via sysctl (CPU, memory, etc.)
/// - **`IOKit`**: Access `RootDomainUserClient` for power management
/// - **Mach services**: Directory services, power management
/// - **Pseudo-terminals**: `openpty()` for interactive commands
///
/// # The #[`allow(dead_code)`] Attribute
///
/// This attribute suppresses compiler warnings about unused code. It's needed
/// because this constant is only referenced in the `#[cfg(target_os = "macos")]`
/// module. On Linux builds, the constant exists but is never used, triggering
/// a warning without this attribute.
///
/// # Raw String Literals (r#"..."#)
///
/// The r#"..."# syntax is a raw string literal that:
/// - Doesn't require escaping backslashes or quotes
/// - Preserves formatting exactly as written
/// - Useful for embedding other languages (here: Scheme/SBPL)
#[allow(dead_code)] // Only used on macOS
const SEATBELT_BASE_POLICY: &str = r#"(version 1)

; start with closed-by-default
(deny default)

; child processes inherit the policy of their parent
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))

; Allow cf prefs to work.
(allow user-preference-read)

; process-info
(allow process-info* (target same-sandbox))

(allow file-write-data
  (require-all
    (path "/dev/null")
    (vnode-type CHARACTER-DEVICE)))

; sysctls permitted.
(allow sysctl-read
  (sysctl-name "hw.activecpu")
  (sysctl-name "hw.busfrequency_compat")
  (sysctl-name "hw.byteorder")
  (sysctl-name "hw.cacheconfig")
  (sysctl-name "hw.cachelinesize_compat")
  (sysctl-name "hw.cpufamily")
  (sysctl-name "hw.cpufrequency_compat")
  (sysctl-name "hw.cputype")
  (sysctl-name "hw.l1dcachesize_compat")
  (sysctl-name "hw.l1icachesize_compat")
  (sysctl-name "hw.l2cachesize_compat")
  (sysctl-name "hw.l3cachesize_compat")
  (sysctl-name "hw.logicalcpu_max")
  (sysctl-name "hw.machine")
  (sysctl-name "hw.memsize")
  (sysctl-name "hw.ncpu")
  (sysctl-name "hw.nperflevels")
  (sysctl-name-prefix "hw.optional.arm.")
  (sysctl-name-prefix "hw.optional.armv8_")
  (sysctl-name "hw.packages")
  (sysctl-name "hw.pagesize_compat")
  (sysctl-name "hw.pagesize")
  (sysctl-name "hw.physicalcpu")
  (sysctl-name "hw.physicalcpu_max")
  (sysctl-name "hw.tbfrequency_compat")
  (sysctl-name "hw.vectorunit")
  (sysctl-name "kern.argmax")
  (sysctl-name "kern.hostname")
  (sysctl-name "kern.maxfilesperproc")
  (sysctl-name "kern.maxproc")
  (sysctl-name "kern.osproductversion")
  (sysctl-name "kern.osrelease")
  (sysctl-name "kern.ostype")
  (sysctl-name "kern.osvariant_status")
  (sysctl-name "kern.osversion")
  (sysctl-name "kern.secure_kernel")
  (sysctl-name "kern.usrstack64")
  (sysctl-name "kern.version")
  (sysctl-name "sysctl.proc_cputype")
  (sysctl-name "vm.loadavg")
  (sysctl-name-prefix "hw.perflevel")
  (sysctl-name-prefix "kern.proc.pgrp.")
  (sysctl-name-prefix "kern.proc.pid.")
  (sysctl-name-prefix "net.routetable.")
)

; Allow Java to read some CPU info.
(allow sysctl-write
  (sysctl-name "kern.grade_cputype"))

; IOKit
(allow iokit-open
  (iokit-registry-entry-class "RootDomainUserClient")
)

; needed to look up user info
(allow mach-lookup
  (global-name "com.apple.system.opendirectoryd.libinfo")
)

; Needed for python multiprocessing on MacOS for the SemLock
(allow ipc-posix-sem)

(allow mach-lookup
  (global-name "com.apple.PowerManagement.control")
)

; allow openpty()
(allow pseudo-tty)
(allow file-read* file-write* file-ioctl (literal "/dev/ptmx"))
(allow file-read* file-write*
  (require-all
    (regex #"^/dev/ttys[0-9]+")
    (extension "com.apple.sandbox.pty")))
(allow file-ioctl (regex #"^/dev/ttys[0-9]+"))
"#;

/// Network policy for Seatbelt
///
/// This SBPL policy fragment is appended to the base policy when
/// `network_access: true` in `WorkspaceWrite` or when using `DangerFullAccess`.
///
/// # Allowed Operations
///
/// - **network-outbound**: Create outbound network connections
/// - **network-inbound**: Accept inbound network connections
/// - **system-socket**: Create system-level sockets
/// - **mach-lookup**: Access network-related system services:
///   - DNS configuration
///   - Network daemon (networkd)
///   - Certificate validation (ocspd, trustd)
///   - Security framework
///
/// # File Operations
///
/// Network-only Seatbelt rules for policies with `network_access: true`.
///
/// Intentionally does **not** grant `file-write*` on the whole
/// `DARWIN_USER_CACHE_DIR`: that root is shared with other apps on the Mac,
/// so a recursive write grant would let a sandboxed command poison another
/// process's cache outside every configured writable root (review finding
/// on #3144). Network sockets and the mach/sysctl lookups below are enough
/// for HTTPS; any process that needs a private cache must write under a
/// workspace/tmp/curated writable root instead.
///
/// The cache directory path is still passed as `DARWIN_USER_CACHE_DIR` by
/// the caller for any future scoped subpath grants; it is not used for a
/// blanket write rule.
#[allow(dead_code)] // Only used on macOS
const SEATBELT_NETWORK_POLICY: &str = r#"
; Network access policies
(allow network-outbound)
(allow network-inbound)
(allow system-socket)

(allow mach-lookup
    (global-name "com.apple.bsd.dirhelper")
    (global-name "com.apple.system.opendirectoryd.membership")
    (global-name "com.apple.SecurityServer")
    (global-name "com.apple.networkd")
    (global-name "com.apple.ocspd")
    (global-name "com.apple.trustd.agent")
    (global-name "com.apple.SystemConfiguration.DNSConfiguration")
    (global-name "com.apple.SystemConfiguration.configd")
)

(allow sysctl-read
  (sysctl-name-regex #"^net.routetable")
)
"#;

/// Path to macOS sandbox-exec binary
///
/// The #[`cfg(target_os` = "macos")] attribute means this constant only exists
/// in macOS builds. Attempting to use this constant on Linux would result in
/// a compile error.
#[cfg(target_os = "macos")]
pub const SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";

/// Environment variable set when running inside sandbox
///
/// This variable is set to "seatbelt" on macOS or "landlock" on Linux when
/// a command is executed in the sandbox. Commands can check this variable
/// to detect sandbox execution and adjust their behavior accordingly.
pub const SANDBOX_ENV_VAR: &str = "MAESTRO_SANDBOX";

// ─────────────────────────────────────────────────────────────
// macOS Seatbelt Implementation
// ─────────────────────────────────────────────────────────────

/// Platform-specific implementation for macOS Seatbelt sandbox
///
/// This module is only compiled when building for macOS (`target_os` = "macos").
/// It contains all Seatbelt-specific logic for policy generation and process
/// spawning.
///
/// # Module Organization
///
/// Using a module (mod macos) rather than inline #[cfg] blocks provides:
/// - Better code organization and namespace separation
/// - IDE support (autocomplete, navigation) on macOS
/// - Clear separation of platform-specific dependencies
///
/// # Key Functions
///
/// - `get_darwin_user_cache_dir()`: FFI call to get macOS cache directory
/// - `create_seatbelt_args()`: Generate sandbox-exec command arguments
/// - `spawn_under_seatbelt()`: Spawn a process under Seatbelt sandbox
#[cfg(target_os = "macos")]
mod macos;

// ─────────────────────────────────────────────────────────────
// Linux Landlock + seccomp Implementation
// ─────────────────────────────────────────────────────────────

/// Platform-specific implementation for Linux Landlock + seccomp sandbox
///
/// This module is only compiled when building for Linux (target_os = "linux").
/// It uses two complementary kernel security features:
///
/// 1. **Landlock LSM**: Filesystem access control
/// 2. **seccomp BPF**: System call filtering
///
/// # Why Two Mechanisms?
///
/// - Landlock handles filesystem restrictions (read/write permissions)
/// - seccomp handles network restrictions (blocking socket syscalls)
///
/// Neither alone provides complete sandboxing, but together they offer
/// defense-in-depth similar to macOS Seatbelt.
///
/// # Key Functions
///
/// - `install_landlock_rules()`: Apply filesystem restrictions
/// - `install_network_seccomp_filter()`: Block network syscalls
/// - `apply_sandbox_policy()`: Combine both mechanisms
/// - `spawn_sandboxed()`: Spawn process with pre_exec sandbox application
#[cfg(target_os = "linux")]
mod linux;

// ─────────────────────────────────────────────────────────────
// Cross-Platform API
// ─────────────────────────────────────────────────────────────

/// Check if sandboxing is available on this platform
///
/// This function performs runtime checks to determine if the native sandbox
/// mechanism is available. The implementation is platform-specific via
/// conditional compilation.
///
/// # Platform Implementations
///
/// **macOS**: Checks if /usr/bin/sandbox-exec exists
/// - Seatbelt is built into macOS, so this file should always exist
/// - If missing, the system may be damaged or extremely old
///
/// **Linux**: Checks if Landlock is enabled in the kernel
/// - Reads /sys/kernel/security/lsm to get active LSM list
/// - Returns true if the string contains "landlock"
/// - Landlock requires kernel 5.13+ and `CONFIG_SECURITY_LANDLOCK=y`
///
/// **Other platforms**: Always returns false
/// - Windows, BSD, etc. are not supported
///
/// # Usage
///
/// This function should be called before attempting to spawn sandboxed
/// commands to provide graceful degradation:
///
/// ```rust,ignore
/// if is_sandbox_available() {
///     spawn_sandboxed_command(...).await?;
/// } else {
///     // Fall back to unsandboxed execution or warn the user
///     spawn_unsandboxed_command(...).await?;
/// }
/// ```
///
/// # Return Value
///
/// Returns true if sandboxing is available, false otherwise.
#[must_use]
pub fn is_sandbox_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::path::Path::new(SEATBELT_EXECUTABLE).exists()
    }
    #[cfg(target_os = "linux")]
    {
        // Check if Landlock is supported
        use std::fs;
        fs::read_to_string("/sys/kernel/security/lsm")
            .map(|s| s.contains("landlock"))
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// Human-readable reason the native sandbox is unavailable, or `None` when
/// [`is_sandbox_available`] returns `true`.
///
/// Callers that need to *enforce* sandboxing (rather than just report a
/// status) should treat unavailability as requiring an explicit, visible
/// decision from the user rather than a silent fallback — see the
/// interactive-TUI sandbox default for how this is used to gate a fail-closed
/// startup message instead of quietly running every command unsandboxed.
#[must_use]
pub fn sandbox_unavailable_reason() -> Option<String> {
    if is_sandbox_available() {
        return None;
    }

    #[cfg(target_os = "macos")]
    {
        Some(format!(
            "{SEATBELT_EXECUTABLE} is missing. This is unexpected on macOS; \
             check whether System Integrity Protection or an MDM profile \
             removed it."
        ))
    }
    #[cfg(target_os = "linux")]
    {
        Some(
            "Landlock is not present in this kernel's active LSM list \
             (/sys/kernel/security/lsm). This is common inside containers \
             (Docker, LXC/Proxmox) and on hardened kernels that boot with a \
             `security=`/`lsm=` parameter that excludes it."
                .to_string(),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Some("This platform has no native sandbox implementation (Windows/BSD).".to_string())
    }
}

/// Get the sandbox type name for the current platform
///
/// Returns a string identifying the sandbox technology used on this platform.
/// This is useful for logging, diagnostics, and user-facing messages.
///
/// # Return Values
///
/// - **macOS**: "seatbelt"
/// - **Linux**: "landlock"
/// - **Other platforms**: "none"
///
/// # Compile-Time Selection
///
/// The return value is determined at compile time via #[cfg] attributes.
/// There is no runtime overhead - the compiler includes only the branch
/// for the target platform.
///
/// # Static Lifetime
///
/// The return type is &'static str, meaning the string slice lives for
/// the entire program duration. This is possible because string literals
/// are stored in the program's read-only data section.
#[must_use]
pub fn sandbox_type() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "seatbelt"
    }
    #[cfg(target_os = "linux")]
    {
        "landlock"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "none"
    }
}

/// Spawn a sandboxed command
///
/// This is the main public API for spawning sandboxed commands. It automatically
/// dispatches to the appropriate platform-specific implementation.
///
/// # Platform Dispatch
///
/// The function body contains multiple #[cfg] blocks that are resolved at compile
/// time. Only one implementation is included in the final binary:
///
/// - **macOS builds**: Calls `macos::spawn_under_seatbelt()`
/// - **Linux builds**: Calls `linux::spawn_sandboxed()`
/// - **Other platforms**: Returns `Err(SandboxError::UnsupportedPlatform)`
///
/// # Arguments
///
/// - `command`: Command and arguments as Vec<String> (e.g., vec!["ls", "-la"])
/// - `cwd`: Working directory for the command (must exist)
/// - `policy`: Security policy to enforce (see `SandboxPolicy` enum)
/// - `env`: Environment variables as `HashMap`<String, String>
///
/// # Return Value
///
/// Returns `SandboxResult`<Child> where Child is a `tokio::process::Child`.
/// The child process is already running when this function returns.
///
/// # Error Handling
///
/// Possible errors:
/// - **`UnsupportedPlatform`**: Called on Windows, BSD, etc.
/// - **`SpawnFailed`**: Command doesn't exist, permission denied, sandbox binary missing
/// - **`LandlockRestrict`**: Landlock not supported or failed to apply (Linux only)
/// - **`SeccompFailed`**: seccomp filter application failed (Linux only)
/// - **`SeatbeltFailed`**: Invalid SBPL policy or sandbox-exec failed (macOS only)
///
/// # Usage Example
///
/// ```rust,ignore
/// use maestro_sandbox::{SandboxPolicy, spawn_sandboxed_command};
/// use std::collections::HashMap;
///
/// let policy = SandboxPolicy::default();
/// let cwd = std::env::current_dir()?;
/// let env = HashMap::new();
///
/// let mut child = spawn_sandboxed_command(
///     vec!["echo".to_string(), "Hello, sandboxed world!".to_string()],
///     cwd,
///     &policy,
///     env,
/// ).await?;
///
/// let status = child.wait().await?;
/// println!("Exit status: {}", status);
/// ```
///
/// # The #[`allow(unused_variables)`] Attribute
///
/// This attribute suppresses warnings about unused parameters. It's needed because:
/// - On unsupported platforms, all parameters are unused (only returns error)
/// - The compiler would warn about unused `command`, `cwd`, `policy`, `env`
/// - The attribute tells the compiler this is intentional, not a mistake
#[allow(unused_variables)]
pub async fn spawn_sandboxed_command(
    command: Vec<String>,
    cwd: PathBuf,
    policy: &SandboxPolicy,
    env: HashMap<String, String>,
) -> SandboxResult<tokio::process::Child> {
    // Point toolchain caches at the sandbox's own cache directory rather than
    // the user's real ones. `DangerFullAccess` is not sandboxed in any
    // meaningful sense, so it keeps the caller's environment untouched.
    let env = if matches!(policy, SandboxPolicy::DangerFullAccess) {
        env
    } else {
        apply_sandbox_cache_env(env, sandbox_cache_session_id())?
    };

    #[cfg(target_os = "macos")]
    {
        macos::spawn_under_seatbelt(command, cwd, policy, env).await
    }

    #[cfg(target_os = "linux")]
    {
        linux::spawn_sandboxed(command, cwd, policy, env).await
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(SandboxError::UnsupportedPlatform)
    }
}

/// Spawn a command without sandboxing (for comparison/fallback)
///
/// This function spawns a command without any sandbox restrictions. It's
/// provided as a fallback for situations where sandboxing is unavailable
/// or causes compatibility issues.
///
/// # Use Cases
///
/// - **Unsupported platforms**: Windows, BSD, etc. where native sandboxing unavailable
/// - **Fallback**: When `is_sandbox_available()` returns false
/// - **Testing**: Compare behavior between sandboxed and unsandboxed execution
/// - **Compatibility**: Some programs don't work properly in sandboxes
///
/// # Security Warning
///
/// This function provides NO security isolation. The command has:
/// - Full filesystem access (read and write)
/// - Full network access
/// - Whatever environment variables the caller passes in `env` (the process
///   environment is cleared first, matching the plain unsandboxed `bash`
///   path -- callers are responsible for passing an already-filtered map)
/// - Ability to spawn child processes
///
/// Only use this for trusted commands or when sandboxing is impossible.
///
/// # Arguments
///
/// - `command`: Command and arguments (e.g., vec!["ls", "-la"])
/// - `cwd`: Working directory
/// - `env`: Environment variables
///
/// # Return Value
///
/// Returns `SandboxResult`<Child> for consistency with `spawn_sandboxed_command`.
/// The only error is `SpawnFailed` (e.g., command not found).
///
/// # Implementation
///
/// Directly uses `tokio::process::Command` with no wrapper or restrictions.
/// This is essentially the same as calling `Command::new()` directly, but
/// provides a consistent API with `spawn_sandboxed_command`.
pub async fn spawn_unsandboxed_command(
    command: Vec<String>,
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> SandboxResult<tokio::process::Child> {
    if command.is_empty() {
        return Err(SandboxError::SpawnFailed(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Empty command",
        )));
    }

    let child = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .current_dir(cwd)
        // The only caller (`bash`'s `bypass_sandbox` path) passes the same
        // filtered `resolve_shell_environment` map the plain unsandboxed
        // branch uses, and expects the same "exactly this env, nothing
        // inherited" semantics. Without `env_clear()`, the child would
        // additionally inherit Maestro's full environment underneath that
        // map, defeating the shell-environment-policy trust gate the caller
        // already applied.
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    Ok(child)
}

// ─────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────
// Kernel sandbox denial capture
// ─────────────────────────────────────────────────────────────

/// How a denial event's process relates to the command that was run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenyRelationship {
    /// The event's pid is the command's own root process.
    Related,
    /// The event's pid is neither the root process nor a known-unrelated one.
    /// Descendants of the command land here, and so do other sandboxed
    /// processes on the host that the kernel log does not let us separate.
    MaybeRelated,
    /// The event's pid is known not to belong to the command (currently the
    /// Maestro process itself).
    ProbablyUnrelated,
}

/// One kernel sandbox denial parsed out of the macOS unified log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenyEvent {
    /// Log timestamp, as the unified log reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Name of the process the kernel denied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    /// Pid of the process the kernel denied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The kernel's decision, lowercased (`deny`, `deny-file-write`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// The denied operation (`file-write-create`, `network-outbound`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// What the operation targeted (a path, an address, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// How many identical denials the kernel collapsed into this entry.
    pub duplicate_count: u32,
    /// The unparsed `eventMessage`, always present.
    pub raw: String,
    /// Whether this event belongs to the command that was run.
    pub relationship: DenyRelationship,
}

impl DenyEvent {
    /// A short `operation target` description for a one-line summary.
    #[must_use]
    pub fn short_description(&self) -> String {
        match (&self.operation, &self.target) {
            (Some(operation), Some(target)) => format!("{operation} {target}"),
            (Some(operation), None) => operation.clone(),
            _ => self.raw.clone(),
        }
    }
}

/// Unified-log predicate selecting kernel sandbox denial messages.
#[cfg(target_os = "macos")]
const DENY_PREDICATE: &str =
    r#"process=="kernel" AND eventMessage CONTAINS "Sandbox:" AND eventMessage contains "deny""#;

/// Upper bound on the `--last` window handed to `log show`, so a long-lived
/// command cannot ask the unified log for hours of history.
#[cfg(target_os = "macos")]
const MAX_DENY_LOOKBACK_SECS: u64 = 300;

/// Collect kernel sandbox denials recorded while a command ran.
///
/// # Why this exists
///
/// When Seatbelt or Landlock blocks a syscall, the program the agent ran sees
/// only its own errno — usually `Permission denied` — with nothing saying the
/// kernel sandbox caused it. The agent then debugs the wrong thing. The kernel
/// does log the denial with the operation and target, so reading it back turns
/// a bare errno into "the sandbox denied `file-write-create /etc/hosts`".
///
/// # Platform support
///
/// macOS reads the unified log (`/usr/bin/log show`). Linux returns an empty
/// list: Landlock denials are not recorded anywhere readable without an
/// auditd configuration Maestro does not control.
///
/// # Bounds
///
/// `budget` is a hard wall-clock cap on the `log show` subprocess. On timeout,
/// spawn failure, a non-zero exit, or unparseable output, this returns an
/// empty list — a missing diagnostic must never turn into a failed command.
/// The lookback window is `started.elapsed()` rounded up, capped at
/// `MAX_DENY_LOOKBACK_SECS`.
///
/// Uses the native Seatbelt log format and fails open for diagnostics only.
pub async fn capture_denies(
    pid: u32,
    started: std::time::Instant,
    budget: std::time::Duration,
) -> Vec<DenyEvent> {
    #[cfg(target_os = "macos")]
    {
        capture_denies_macos(pid, started, budget).await
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (pid, started, budget);
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
async fn capture_denies_macos(
    pid: u32,
    started: std::time::Instant,
    budget: std::time::Duration,
) -> Vec<DenyEvent> {
    let seconds = started
        .elapsed()
        .as_secs()
        .saturating_add(1)
        .min(MAX_DENY_LOOKBACK_SECS);

    let mut command = tokio::process::Command::new("/usr/bin/log");
    command
        .arg("show")
        .arg("--style")
        .arg("ndjson")
        .arg("--predicate")
        .arg(DENY_PREDICATE)
        .arg("--last")
        .arg(format!("{seconds}s"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    let Ok(child) = command.spawn() else {
        return Vec::new();
    };
    let Ok(Ok(output)) = tokio::time::timeout(budget, child.wait_with_output()).await else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_deny_events(&String::from_utf8_lossy(&output.stdout), pid)
}

/// Parse `log show --style ndjson` output into denial events.
///
/// Lines that are not JSON, carry no `eventMessage`, or do not match a
/// known Sandbox message shape are skipped or kept as raw text; nothing here
/// can fail.
#[must_use]
pub fn parse_deny_events(ndjson: &str, root_pid: u32) -> Vec<DenyEvent> {
    ndjson
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|entry| {
            let message = entry.get("eventMessage")?.as_str()?.to_string();
            let timestamp = entry
                .get("timestamp")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);
            Some(parse_deny_message(&message, timestamp, root_pid))
        })
        .collect()
}

/// Classify a denial's pid against the command's root pid.
#[must_use]
fn deny_relationship(event_pid: u32, root_pid: u32) -> DenyRelationship {
    if event_pid == root_pid {
        DenyRelationship::Related
    } else if event_pid == std::process::id() {
        // Maestro itself is sandboxed by nothing we spawned; a denial from
        // this pid never belongs to the command.
        DenyRelationship::ProbablyUnrelated
    } else {
        DenyRelationship::MaybeRelated
    }
}

/// Parse one `eventMessage` into a [`DenyEvent`].
fn parse_deny_message(message: &str, timestamp: Option<String>, root_pid: u32) -> DenyEvent {
    static DENY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^Sandbox:\s+([^(]+)\((\d+)\)\s+([a-zA-Z-]+)\((\d+)\)\s+(\S+)\s+(.+)$")
            .expect("deny regex")
    });
    static DUPLICATE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(\d+) duplicate reports? for Sandbox: (.+)$").expect("duplicate regex")
    });
    static DUPLICATE_BODY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^([^(]+)\((\d+)\)\s+([a-zA-Z-]+)\((\d+)\)\s+(\S+)\s+(.+)$")
            .expect("duplicate body regex")
    });

    let unparsed = |timestamp: Option<String>, duplicate_count: u32| DenyEvent {
        timestamp,
        process_name: None,
        pid: None,
        decision: None,
        operation: None,
        target: None,
        duplicate_count,
        raw: message.to_string(),
        relationship: DenyRelationship::MaybeRelated,
    };

    let build = |captures: &regex::Captures<'_>, duplicate_count: u32| {
        let event_pid: u32 = captures[2].parse().unwrap_or(0);
        DenyEvent {
            timestamp: timestamp.clone(),
            process_name: Some(captures[1].trim().to_string()),
            pid: Some(event_pid),
            decision: Some(captures[3].to_lowercase()),
            operation: Some(captures[5].to_string()),
            target: Some(captures[6].to_string()),
            duplicate_count,
            raw: message.to_string(),
            relationship: deny_relationship(event_pid, root_pid),
        }
    };

    if let Some(captures) = DENY.captures(message) {
        return build(&captures, 1);
    }
    if let Some(captures) = DUPLICATE.captures(message) {
        let count: u32 = captures[1].parse().unwrap_or(1);
        if let Some(body) = DUPLICATE_BODY.captures(&captures[2]) {
            return build(&body, count);
        }
        return unparsed(timestamp, count);
    }
    unparsed(timestamp, 1)
}

#[cfg(test)]
mod tests;
