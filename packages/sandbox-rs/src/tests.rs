use super::*;

#[cfg(unix)]
fn private_tempdir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    temp
}

/// One `log show --style ndjson` line, as the unified log emits them.
fn ndjson_line(message: &str) -> String {
    serde_json::json!({
        "timestamp": "2026-08-23 09:41:02.123456-0700",
        "processImagePath": "/kernel",
        "eventMessage": message,
    })
    .to_string()
}

#[test]
fn parse_deny_events_extracts_process_operation_and_target() {
    let ndjson =
        ndjson_line("Sandbox: bash(4242) deny(1) file-write-create /Users/dev/.cargo/registry/x");

    let events = parse_deny_events(&ndjson, 4242);

    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.process_name.as_deref(), Some("bash"));
    assert_eq!(event.pid, Some(4242));
    assert_eq!(event.decision.as_deref(), Some("deny"));
    assert_eq!(event.operation.as_deref(), Some("file-write-create"));
    assert_eq!(
        event.target.as_deref(),
        Some("/Users/dev/.cargo/registry/x")
    );
    assert_eq!(event.duplicate_count, 1);
    assert_eq!(event.relationship, DenyRelationship::Related);
    assert_eq!(
        event.timestamp.as_deref(),
        Some("2026-08-23 09:41:02.123456-0700")
    );
}

#[test]
fn parse_deny_events_reads_duplicate_reports() {
    let ndjson = ndjson_line(
        "17 duplicate reports for Sandbox: cargo(4243) deny(1) network-outbound 1.2.3.4:443",
    );

    let events = parse_deny_events(&ndjson, 4242);

    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.duplicate_count, 17);
    assert_eq!(event.pid, Some(4243));
    assert_eq!(event.operation.as_deref(), Some("network-outbound"));
    assert_eq!(event.target.as_deref(), Some("1.2.3.4:443"));
    assert_eq!(event.relationship, DenyRelationship::MaybeRelated);
}

#[test]
fn parse_deny_events_tags_relationship_by_pid() {
    let ndjson = [
        ndjson_line("Sandbox: bash(4242) deny(1) file-write-create /etc/hosts"),
        ndjson_line("Sandbox: rustc(9999) deny(1) file-write-create /etc/hosts"),
        ndjson_line(&format!(
            "Sandbox: maestro({}) deny(1) file-write-create /etc/hosts",
            std::process::id()
        )),
    ]
    .join("\n");

    let events = parse_deny_events(&ndjson, 4242);

    assert_eq!(events.len(), 3);
    assert_eq!(events[0].relationship, DenyRelationship::Related);
    assert_eq!(events[1].relationship, DenyRelationship::MaybeRelated);
    assert_eq!(events[2].relationship, DenyRelationship::ProbablyUnrelated);
}

#[test]
fn parse_deny_events_keeps_unrecognized_messages_as_raw() {
    let ndjson = ndjson_line("Sandbox: something the regex does not model");

    let events = parse_deny_events(&ndjson, 4242);

    assert_eq!(events.len(), 1);
    assert!(events[0].operation.is_none());
    assert!(events[0].pid.is_none());
    assert_eq!(events[0].raw, "Sandbox: something the regex does not model");
    assert_eq!(events[0].duplicate_count, 1);
}

#[test]
fn parse_deny_events_skips_lines_that_are_not_usable() {
    let ndjson = [
        "not json at all",
        "",
        "   ",
        &serde_json::json!({ "timestamp": "t" }).to_string(),
        &ndjson_line("Sandbox: bash(1) deny(1) file-write-create /x"),
    ]
    .join("\n");

    assert_eq!(parse_deny_events(&ndjson, 1).len(), 1);
}

#[test]
fn deny_event_short_description_prefers_operation_and_target() {
    let mut event = parse_deny_events(
        &ndjson_line("Sandbox: bash(1) deny(1) file-write-create /x"),
        1,
    )
    .remove(0);
    assert_eq!(event.short_description(), "file-write-create /x");

    event.operation = None;
    event.target = None;
    assert_eq!(event.short_description(), event.raw);
}

#[tokio::test]
async fn capture_denies_returns_empty_within_its_budget() {
    // No sandboxed command ran under this pid, so there is nothing to
    // find. The point of the assertion is the bound: the capture must
    // return promptly and must never fail the caller.
    let started = std::time::Instant::now();
    let events = capture_denies(
        std::process::id(),
        started,
        std::time::Duration::from_secs(5),
    )
    .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    for event in &events {
        assert!(!event.raw.is_empty());
    }
}

/// Integration probe: run a write the sandbox must deny and check that
/// the kernel denial is visible. Ignored by default because CI runners
/// commonly cannot read the unified log (`log show` needs a real macOS
/// host with the log daemon, which sandboxed/virtualized runners lack).
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "needs a macOS host where `log show` can read kernel messages"]
async fn capture_denies_sees_a_denied_write_under_read_only() {
    let started = std::time::Instant::now();
    let child = spawn_sandboxed_command(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "touch /etc/maestro-probe".to_string(),
        ],
        std::env::temp_dir(),
        &SandboxPolicy::ReadOnly,
        HashMap::new(),
    )
    .await
    .expect("sandboxed spawn should succeed");
    let pid = child.id().expect("child pid");
    let output = child.wait_with_output().await.expect("child should exit");
    assert!(!output.status.success(), "the write must be denied");

    let events = capture_denies(pid, started, std::time::Duration::from_secs(10)).await;
    assert!(
        events.iter().any(|event| event
            .operation
            .as_deref()
            .is_some_and(|operation| operation.starts_with("file-write"))),
        "expected a file-write denial: {events:?}"
    );
}

/// `CARGO_HOME`/`XDG_CACHE_HOME` are process-global env vars read by
/// `dev_cache_writable_roots()`; guard tests that set them so they don't
/// race other tests reading the ambient values, matching the
/// `env_lock()` pattern used for this class of test elsewhere in the
/// crate (e.g. `config_cli.rs`).
fn dev_cache_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn dev_cache_writable_roots_never_grants_the_real_cargo_cache() {
    // The sandbox no longer writes into the user's Cargo cache at all:
    // `sandbox_cache_env` redirects dependency caches into a
    // session-private directory, while Cargo build artifacts stay under
    // the writable workspace, so a build script cannot leave a poisoned
    // artifact in the user's reusable Cargo cache.
    let _guard = dev_cache_env_lock();
    let cargo_home = tempfile::tempdir().unwrap();
    std::env::set_var("CARGO_HOME", cargo_home.path());
    std::fs::create_dir_all(cargo_home.path().join("registry")).unwrap();
    std::fs::create_dir_all(cargo_home.path().join("git")).unwrap();

    let roots = SandboxPolicy::dev_cache_writable_roots();

    std::env::remove_var("CARGO_HOME");

    assert!(
        roots
            .iter()
            .all(|root| !root.starts_with(cargo_home.path())),
        "no writable root may live under $CARGO_HOME: {roots:?}"
    );
    assert_eq!(
        roots,
        vec![sandbox_cache_root(sandbox_cache_session_id())],
        "the only writable cache root is the session-private one: {roots:?}"
    );
}

#[test]
fn dev_cache_writable_roots_is_the_session_cache_root() {
    let roots = SandboxPolicy::dev_cache_writable_roots();
    assert_eq!(roots.len(), 1);
    assert!(
        roots[0].is_dir(),
        "the cache root must be created: {roots:?}"
    );
    let temp_dir = dunce::canonicalize(std::env::temp_dir()).unwrap();
    assert!(roots[0].starts_with(temp_dir));
}

#[test]
fn sandbox_cache_env_never_names_toolchain_home_variables() {
    // These hold installed executables, not caches. Redirecting them
    // would point the toolchain at an empty directory and break the
    // command, and is why the reference implementation excludes them.
    let env = sandbox_cache_env("test-session");
    for forbidden in [
        "RUSTUP_HOME",
        "GEM_HOME",
        "VOLTA_HOME",
        "PIPX_HOME",
        "DENO_DIR",
    ] {
        assert!(
            !env.iter().any(|(name, _)| name == forbidden),
            "{forbidden} must not be redirected: {env:?}"
        );
    }
}

#[test]
fn sandbox_cache_env_points_every_variable_inside_the_session_root() {
    let root = sandbox_cache_root("test-session");
    let env = sandbox_cache_env("test-session");
    assert!(env.len() >= 25, "expected the full cache variable set");
    for (name, value) in &env {
        assert!(
            PathBuf::from(value).starts_with(&root),
            "{name} points outside the session cache root: {value}"
        );
    }
    assert!(env.iter().any(|(name, _)| name == "NPM_CONFIG_CACHE"));
    assert!(env.iter().any(|(name, _)| name == "npm_config_store_dir"));
    assert!(!env.iter().any(|(name, _)| name == "PNPM_STORE_PATH"));
    assert!(env.iter().any(|(name, _)| name == "CARGO_HOME"));
    assert!(!env.iter().any(|(name, _)| name == "CARGO_TARGET_DIR"));
    assert!(env.iter().any(|(name, _)| name == "GOCACHE"));
    assert!(env.iter().any(|(name, _)| name == "PIP_CACHE_DIR"));
    assert!(env.iter().any(|(name, _)| name == "GRADLE_USER_HOME"));
    assert!(env.iter().any(|(name, _)| name == "YARN_CACHE_FOLDER"));
    assert!(env.iter().any(|(name, _)| name == "YARN_GLOBAL_FOLDER"));
    assert!(env.iter().any(|(name, _)| name == "COMPOSER_CACHE_DIR"));
    assert!(!env.iter().any(|(name, _)| name == "COMPOSER_HOME"));
}

#[test]
fn apply_sandbox_cache_env_uses_the_workspace_cargo_target() {
    let source = tempfile::tempdir().unwrap();
    let session_id = format!("cache-env-test-{}", uuid::Uuid::new_v4());
    let mut env = HashMap::new();
    for key in ["HOME", "CARGO_HOME", "GRADLE_USER_HOME"] {
        env.insert(
            key.to_string(),
            source.path().to_string_lossy().into_owned(),
        );
    }
    env.insert("CARGO_TARGET_DIR".to_string(), "/host/target".to_string());
    env.insert("COMPOSER_HOME".to_string(), "/host/composer".to_string());
    env.insert(
        "COMPOSER_CACHE_DIR".to_string(),
        "/host/composer-cache".to_string(),
    );
    env.insert(
        "MAVEN_OPTS".to_string(),
        "-Xmx2g -Dmaven.repo.local=/host/maven".to_string(),
    );
    env.insert("PATH".to_string(), "/usr/bin".to_string());

    let applied = apply_sandbox_cache_env(env, &session_id).unwrap();

    assert_eq!(applied.get("PATH").map(String::as_str), Some("/usr/bin"));
    assert!(!applied.contains_key("CARGO_TARGET_DIR"));
    assert_eq!(
        applied.get("COMPOSER_HOME").map(String::as_str),
        Some("/host/composer")
    );
    assert_eq!(
        applied.get("COMPOSER_CACHE_DIR").map(PathBuf::from),
        Some(sandbox_cache_root(&session_id).join("composer"))
    );
    let maven_opts = applied.get("MAVEN_OPTS").expect("MAVEN_OPTS set");
    assert!(maven_opts.starts_with("-Xmx2g -Dmaven.repo.local=/host/maven "));
    assert!(maven_opts.ends_with(&format!(
        "-Dmaven.repo.local={}",
        sandbox_cache_root(&session_id).join("maven").display()
    )));
}

#[cfg(unix)]
#[test]
fn prepare_sandbox_cache_root_is_owner_only() {
    use std::os::unix::fs::MetadataExt as _;

    let temp = private_tempdir();
    let root = prepare_sandbox_cache_root_in(temp.path(), "private-session").unwrap();
    let parent = root.parent().unwrap();

    assert_eq!(std::fs::metadata(parent).unwrap().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&root).unwrap().uid(), unsafe {
        libc::geteuid()
    });
}

#[cfg(unix)]
#[test]
fn prepare_sandbox_cache_root_rejects_a_symlink_parent() {
    use std::os::unix::fs::symlink;

    let temp = private_tempdir();
    let attacker = tempfile::tempdir().unwrap();
    symlink(attacker.path(), temp.path().join(SANDBOX_CACHE_DIR)).unwrap();

    let error = prepare_sandbox_cache_root_in(temp.path(), "session").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(!attacker.path().join("session").exists());
}

#[cfg(unix)]
#[test]
fn prepare_sandbox_cache_root_rejects_a_shared_parent() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = private_tempdir();
    let parent = temp.path().join(SANDBOX_CACHE_DIR);
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();

    let error = prepare_sandbox_cache_root_in(temp.path(), "session").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(!parent.join("session").exists());
}

#[cfg(unix)]
#[test]
fn prepare_sandbox_cache_root_reclaims_exited_process_caches_but_keeps_live_ones() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = private_tempdir();
    let parent = temp.path().join(SANDBOX_CACHE_DIR);
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();

    let live = parent.join("live");
    std::fs::create_dir(&live).unwrap();
    std::fs::write(
        live.join(SANDBOX_CACHE_PROCESS_MARKER),
        std::process::id().to_string(),
    )
    .unwrap();
    for index in 0..4 {
        let inactive = parent.join(format!("inactive-{index}"));
        std::fs::create_dir(&inactive).unwrap();
        std::fs::write(inactive.join(SANDBOX_CACHE_PROCESS_MARKER), "4294967295").unwrap();
    }

    prepare_sandbox_cache_root_in(temp.path(), "current").unwrap();

    assert!(
        live.exists(),
        "a live process cache must never be reclaimed"
    );
    let inactive_count = std::fs::read_dir(&parent)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("inactive-"))
        .count();
    assert_eq!(inactive_count, SANDBOX_CACHE_RETAINED_INACTIVE);
}

#[cfg(unix)]
#[test]
fn concurrent_cache_reclaim_tolerates_entries_removed_by_another_process() {
    use std::sync::{Arc, Barrier};

    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join(SANDBOX_CACHE_DIR);
    std::fs::create_dir(&parent).unwrap();
    for index in 0..64 {
        let inactive = parent.join(format!("inactive-{index}"));
        std::fs::create_dir(&inactive).unwrap();
        std::fs::write(inactive.join(SANDBOX_CACHE_PROCESS_MARKER), "4294967295").unwrap();
    }

    let barrier = Arc::new(Barrier::new(8));
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..8 {
            let barrier = Arc::clone(&barrier);
            let parent = parent.clone();
            workers.push(scope.spawn(move || {
                barrier.wait();
                reclaim_inactive_sandbox_caches(&parent, "current")
            }));
        }
        for worker in workers {
            worker
                .join()
                .expect("reclaim worker must not panic")
                .expect("a concurrently removed inactive entry is already reclaimed");
        }
    });
}

#[test]
fn seed_toolchain_user_config_preserves_cargo_and_gradle_configuration() {
    let temp = private_tempdir();
    let home = temp.path().join("home");
    let cargo = home.join(".cargo");
    let gradle = home.join(".gradle");
    std::fs::create_dir_all(&cargo).unwrap();
    std::fs::create_dir_all(gradle.join("init.d")).unwrap();
    std::fs::write(cargo.join("config.toml"), "[net]\noffline = true\n").unwrap();
    std::fs::write(
        cargo.join("credentials.toml"),
        "[registry]\ntoken = 'secret'\n",
    )
    .unwrap();
    std::fs::write(
        gradle.join("gradle.properties"),
        "org.gradle.daemon=false\n",
    )
    .unwrap();
    std::fs::write(gradle.join("init.d/company.gradle"), "// company init\n").unwrap();

    let cache = prepare_sandbox_cache_root_in(temp.path(), "session").unwrap();
    let env = HashMap::from([
        ("HOME".to_string(), home.to_string_lossy().into_owned()),
        (
            "CARGO_HOME".to_string(),
            cargo.to_string_lossy().into_owned(),
        ),
        (
            "GRADLE_USER_HOME".to_string(),
            gradle.to_string_lossy().into_owned(),
        ),
    ]);
    seed_toolchain_user_config(&env, &cache).unwrap();

    assert_eq!(
        std::fs::read_to_string(cache.join("cargo-home/config.toml")).unwrap(),
        "[net]\noffline = true\n"
    );
    assert!(cache.join("cargo-home/credentials.toml").is_file());
    assert_eq!(
        std::fs::read_to_string(cache.join("gradle/gradle.properties")).unwrap(),
        "org.gradle.daemon=false\n"
    );
    assert!(cache.join("gradle/init.d/company.gradle").is_file());

    std::fs::remove_file(cargo.join("config.toml")).unwrap();
    std::fs::remove_file(gradle.join("gradle.properties")).unwrap();
    std::fs::remove_file(gradle.join("init.d/company.gradle")).unwrap();
    std::fs::write(gradle.join("init.d/replacement.gradle"), "// replacement\n").unwrap();
    seed_toolchain_user_config(&env, &cache).unwrap();

    assert!(!cache.join("cargo-home/config.toml").exists());
    assert!(!cache.join("gradle/gradle.properties").exists());
    assert!(!cache.join("gradle/init.d/company.gradle").exists());
    assert!(cache.join("gradle/init.d/replacement.gradle").is_file());
}

#[test]
fn sandbox_cache_session_id_is_stable_within_the_process() {
    assert_eq!(sandbox_cache_session_id(), sandbox_cache_session_id());
    assert_eq!(sandbox_cache_session_id().len(), 32);
}

#[test]
fn dev_cache_writable_roots_never_grants_reusable_npx_installations() {
    let roots = SandboxPolicy::dev_cache_writable_roots();
    assert!(roots.iter().all(|root| {
        root.file_name().is_none_or(|name| name != ".npm")
            && !root.components().any(|part| part.as_os_str() == "_npx")
    }));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_writable_roots_exclude_git_metadata() {
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("src");
    let git = workspace.path().join(".git");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(git.join("hooks")).unwrap();

    let roots = vec![WritableRoot {
        root: workspace.path().to_path_buf(),
        read_only_subpaths: vec![git.clone()],
    }];
    let writable = super::linux::writable_paths_without_exclusions(roots);

    assert!(writable.full.iter().any(|path| path == &source));
    assert!(writable.full.iter().all(|path| !path.starts_with(&git)));
}

/// Stage-1: when the workspace contains `.git`, expansion grants full RW
/// only to existing non-excluded children, plus Make*/Remove* on the root
/// (never WriteFile on the root — that would OR-grant write under `.git`).
#[cfg(target_os = "linux")]
#[test]
fn linux_excluded_root_is_granted_creation_rights_for_new_children() {
    let workspace = tempfile::tempdir().unwrap();
    let git = workspace.path().join(".git");
    std::fs::create_dir_all(&git).unwrap();

    let roots = vec![WritableRoot {
        root: workspace.path().to_path_buf(),
        read_only_subpaths: vec![git.clone()],
    }];
    let writable = super::linux::writable_paths_without_exclusions(roots);

    assert!(
        writable
            .make_remove_only
            .iter()
            .any(|path| path == workspace.path()),
        "the excluded root must be granted creation/removal rights"
    );
    assert!(
        writable.full.iter().all(|path| !path.starts_with(&git)),
        ".git must not gain full read-write access"
    );
    assert!(
        !writable.full.iter().any(|path| path == workspace.path()),
        "the root must not be granted full read-write access while .git is excluded"
    );
}

#[cfg(unix)]
#[test]
fn workspace_write_rejects_dangling_symlink_targets() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let link = workspace.path().join("escape.ipynb");
    symlink(outside.path().join("created-outside.ipynb"), &link).unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: Vec::new(),
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    assert!(!policy.allows_write_to(workspace.path(), &link));
}

#[test]
fn commit_native_write_creates_and_replaces_files_inside_the_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: Vec::new(),
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    let target = workspace.path().join("nested").join("file.txt");

    commit_native_write(Some(&policy), workspace.path(), &target, b"first").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");

    commit_native_write(Some(&policy), workspace.path(), &target, b"second").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
}

#[test]
fn commit_native_write_denies_a_path_outside_every_writable_root() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: Vec::new(),
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    let target = outside.path().join("escaped.txt");

    let result = commit_native_write(Some(&policy), workspace.path(), &target, b"nope");
    assert!(result.is_err());
    assert!(!target.exists());
}

/// Regression test for the TOCTOU review finding on #3144: a preflight
/// path check and the write that follows it are separate filesystem
/// operations, so a directory swapped for a symlink to outside the
/// workspace between the two must still be denied. `commit_native_write`
/// pins the parent directory and revalidates the policy against the
/// directory descriptor it actually writes through.
#[cfg(unix)]
#[test]
fn commit_native_write_denies_a_parent_swapped_for_a_symlink_after_preflight() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: Vec::new(),
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    let subdir = workspace.path().join("sub");
    std::fs::create_dir_all(&subdir).unwrap();
    let target = subdir.join("escaped.txt");

    // Preflight sees a legitimate in-workspace directory and passes.
    preflight_native_write(Some(&policy), workspace.path(), &target).unwrap();

    // The swap: a background task replaces the directory with a symlink
    // pointing outside every writable root.
    std::fs::remove_dir(&subdir).unwrap();
    symlink(outside.path(), &subdir).unwrap();

    let result = commit_native_write(Some(&policy), workspace.path(), &target, b"nope");
    assert!(result.is_err(), "the swapped parent must be denied");
    assert!(
        !outside.path().join("escaped.txt").exists(),
        "no byte may be written outside the workspace"
    );
}

#[test]
fn dev_cache_writable_roots_never_grants_the_xdg_cache() {
    // The XDG cache root holds installed tool environments (e.g.
    // `~/.cache/pre-commit/<hash>/.../bin/`). No part of it is granted
    // now that `PIP_CACHE_DIR` and `UV_CACHE_DIR` are redirected into the
    // session-private cache root instead.
    let _guard = dev_cache_env_lock();
    let xdg_cache_home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(xdg_cache_home.path().join("pip")).unwrap();
    std::fs::create_dir_all(xdg_cache_home.path().join("pre-commit")).unwrap();
    std::env::set_var("XDG_CACHE_HOME", xdg_cache_home.path());
    std::env::remove_var("CARGO_HOME");

    let roots = SandboxPolicy::dev_cache_writable_roots();

    std::env::remove_var("XDG_CACHE_HOME");

    assert!(
        roots
            .iter()
            .all(|root| !root.starts_with(xdg_cache_home.path())),
        "no writable root may live under $XDG_CACHE_HOME: {roots:?}"
    );
}

#[test]
fn mode_label_matches_the_config_grammar() {
    assert_eq!(
        SandboxPolicy::DangerFullAccess.mode_label(),
        "danger-full-access"
    );
    assert_eq!(SandboxPolicy::ReadOnly.mode_label(), "read-only");
    assert_eq!(
        SandboxPolicy::workspace_write_default().mode_label(),
        "workspace-write"
    );
}

#[test]
fn test_sandbox_policy_defaults() {
    let policy = SandboxPolicy::default();
    assert!(!policy.has_full_disk_write_access());
    assert!(policy.has_full_disk_read_access());
    assert!(!policy.has_full_network_access());
}

#[test]
fn test_danger_full_access() {
    let policy = SandboxPolicy::DangerFullAccess;
    assert!(policy.has_full_disk_write_access());
    assert!(policy.has_full_disk_read_access());
    assert!(policy.has_full_network_access());
}

#[test]
fn test_read_only() {
    let policy = SandboxPolicy::ReadOnly;
    assert!(!policy.has_full_disk_write_access());
    assert!(!policy.has_full_disk_read_access());
    assert!(!policy.has_full_network_access());
}

#[test]
fn test_workspace_write_with_network() {
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![PathBuf::from("/custom")],
        network_access: true,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
    };
    assert!(!policy.has_full_disk_write_access());
    assert!(policy.has_full_disk_read_access());
    assert!(policy.has_full_network_access());
}

#[test]
fn test_get_writable_roots() {
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![PathBuf::from("/custom")],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    let cwd = PathBuf::from("/workspace");
    let roots = policy.get_writable_roots_with_cwd(&cwd);

    // Should include /custom and cwd
    assert!(
        roots
            .iter()
            .any(|r| r.root.as_path() == Path::new("/custom"))
    );
    assert!(
        roots
            .iter()
            .any(|r| r.root.as_path() == Path::new("/workspace"))
    );
}

#[test]
fn allows_write_to_danger_full_access_allows_anything() {
    let policy = SandboxPolicy::DangerFullAccess;
    assert!(policy.allows_write_to(Path::new("/workspace"), Path::new("/etc/shadow")));
}

#[test]
fn allows_write_to_read_only_denies_everything() {
    let policy = SandboxPolicy::ReadOnly;
    let workspace = tempfile::tempdir().unwrap();
    assert!(!policy.allows_write_to(workspace.path(), &workspace.path().join("in_cwd.txt")));
}

#[test]
fn canonicalize_best_effort_resolves_ancestor_symlinks_for_nonexistent_targets() {
    let real_dir = tempfile::tempdir().unwrap();
    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("link-to-real");
    #[cfg(unix)]
    std::os::unix::fs::symlink(real_dir.path(), &link).unwrap();
    #[cfg(not(unix))]
    return;

    // The target file does not exist yet, only its ancestor (the
    // symlink) does. The resolved path must follow the symlink, matching
    // what `dunce::canonicalize` would return if the file already
    // existed.
    let resolved = canonicalize_best_effort(&link.join("new_file.txt"));
    let expected = dunce::canonicalize(real_dir.path())
        .unwrap()
        .join("new_file.txt");
    assert_eq!(resolved, expected);
}

#[test]
fn allows_write_to_workspace_write_allows_cwd_and_denies_outside() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: Vec::new(),
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    assert!(policy.allows_write_to(workspace.path(), &workspace.path().join("src/main.rs")));
    // A native write/edit tool call must not be able to escape the
    // workspace via an absolute path outside every writable root, e.g.
    // `~/.bashrc` or `~/.ssh/authorized_keys` (the exact bypasses
    // flagged in review for #3144).
    assert!(!policy.allows_write_to(workspace.path(), &outside.path().join("bashrc")));
}

#[test]
fn allows_write_to_denies_read_only_subpath_inside_writable_root() {
    let workspace = tempfile::tempdir().unwrap();
    let git_dir = workspace.path().join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: Vec::new(),
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    // .git is writable-root territory but carved out as read-only.
    assert!(!policy.allows_write_to(workspace.path(), &git_dir.join("HOOKS")));
    // A sibling file in the same cwd remains writable.
    assert!(policy.allows_write_to(workspace.path(), &workspace.path().join("README.md")));
}

#[test]
fn allows_write_to_workspace_write_allows_extra_writable_root() {
    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![cache.path().to_path_buf()],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    assert!(policy.allows_write_to(workspace.path(), &cache.path().join("registry/lock")));
}

#[test]
fn test_sandbox_type() {
    let t = sandbox_type();
    #[cfg(target_os = "macos")]
    assert_eq!(t, "seatbelt");
    #[cfg(target_os = "linux")]
    assert_eq!(t, "landlock");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    assert_eq!(t, "none");
}

#[test]
fn test_sandbox_unavailable_reason_matches_availability() {
    // The two functions must never disagree: `Some(reason)` iff the
    // sandbox is unavailable. A future edit to either function that
    // forgets to update the other should fail this test.
    assert_eq!(
        sandbox_unavailable_reason().is_none(),
        is_sandbox_available()
    );
}

#[test]
fn test_workspace_write_default_has_network_and_no_writable_roots_on_a_bare_machine() {
    // dev_cache_writable_roots() returns the session cache root, which it
    // creates, so this assertion holds regardless of whether the test
    // runner happens to have a Rust/Node toolchain installed.
    let policy = SandboxPolicy::workspace_write_default();
    assert!(policy.has_full_network_access());
    assert!(!policy.has_full_disk_write_access());
    let SandboxPolicy::WorkspaceWrite { writable_roots, .. } = &policy else {
        panic!("workspace_write_default() must return WorkspaceWrite");
    };
    for root in writable_roots {
        assert!(
            root.exists(),
            "dev_cache_writable_roots() must only return roots that exist: {root:?}"
        );
    }
}

#[test]
fn dev_cache_writable_roots_never_grants_rustup_home() {
    // Regression guard for the review finding on #3144: granting
    // $RUSTUP_HOME lets a sandboxed build script overwrite an installed
    // toolchain binary (rustc, cargo, clippy-driver under
    // toolchains/*/bin) and persist code execution across later
    // sandboxed sessions. Ordinary dependency fetching never needs to
    // write there, so it must never appear in this list, regardless of
    // whether RUSTUP_HOME is set in the test environment.
    let roots = SandboxPolicy::dev_cache_writable_roots();
    if let Some(rustup_home) = std::env::var_os("RUSTUP_HOME") {
        let rustup_home = PathBuf::from(rustup_home);
        assert!(
            !roots.iter().any(|root| root == &rustup_home),
            "dev_cache_writable_roots() must not grant $RUSTUP_HOME: {roots:?}"
        );
    }
    if let Some(home) = dirs::home_dir() {
        let default_rustup_home = home.join(".rustup");
        assert!(
            !roots.iter().any(|root| root == &default_rustup_home),
            "dev_cache_writable_roots() must not grant ~/.rustup: {roots:?}"
        );
    }
}

#[test]
fn test_default_and_workspace_write_default_diverge_on_network_access() {
    // Regression guard for the documented split between the conservative
    // library `Default` (network off) and the product default used by
    // interactive/exec sessions (network on) — see both doc comments.
    assert!(!SandboxPolicy::default().has_full_network_access());
    assert!(SandboxPolicy::workspace_write_default().has_full_network_access());
}

#[cfg(target_os = "macos")]
#[test]
fn test_seatbelt_args_basic() {
    use super::macos::create_seatbelt_args;

    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    let cwd = std::env::temp_dir();
    let args = create_seatbelt_args(vec!["echo".to_string(), "hello".to_string()], &policy, &cwd);

    // Should start with -p (policy)
    assert_eq!(args[0], "-p");
    // Should end with -- echo hello
    assert!(args.contains(&"--".to_string()));
    assert!(args.contains(&"echo".to_string()));
    assert!(args.contains(&"hello".to_string()));
}

/// Regression test for the review finding on #3144: `ReadOnly` means
/// "no writes allowed", not "no filesystem access" (see the enum's own
/// docs), and must keep granting reads on macOS exactly as the Linux
/// Landlock implementation already does. Before this fix, the Seatbelt
/// translation omitted `(allow file-read*)` for `ReadOnly` because it
/// (wrongly) keyed the read allow off `has_full_disk_read_access()`,
/// which returns `false` specifically for `ReadOnly`.
#[cfg(target_os = "macos")]
#[test]
fn test_seatbelt_read_only_still_allows_reads() {
    use super::macos::create_seatbelt_args;

    let cwd = std::env::temp_dir();
    let args = create_seatbelt_args(
        vec!["cat".to_string(), "Cargo.toml".to_string()],
        &SandboxPolicy::ReadOnly,
        &cwd,
    );
    let policy_text = args
        .iter()
        .find(|arg| arg.contains("allow file-read*"))
        .expect("-p <policy> argument must contain the policy text");
    assert!(
        policy_text.contains("(allow file-read*)"),
        "ReadOnly must still allow reads on macOS: {policy_text}"
    );
}

#[test]
fn test_policy_serialization() {
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![PathBuf::from("/tmp")],
        network_access: true,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
    };

    let json = serde_json::to_string(&policy).unwrap();
    let parsed: SandboxPolicy = serde_json::from_str(&json).unwrap();
    assert_eq!(policy, parsed);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_workspace_write_blocks_home_writes() {
    if !is_sandbox_available() {
        return;
    }

    let workspace = tempfile::tempdir().unwrap();
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() || home.starts_with("/tmp") {
        return;
    }

    let probe = PathBuf::from(home).join(format!(
        "maestro-rust-sandbox-should-not-write-{}",
        std::process::id()
    ));
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: Vec::new(),
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    let child = spawn_sandboxed_command(
        vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("printf blocked > {}", probe.to_string_lossy()),
        ],
        workspace.path().to_path_buf(),
        &policy,
        HashMap::new(),
    )
    .await
    .unwrap();
    let output = child.wait_with_output().await.unwrap();

    assert!(!output.status.success());
    assert!(!probe.exists());
    let _ = std::fs::remove_file(probe);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_workspace_write_denies_writes_outside_writable_roots() {
    if !is_sandbox_available() {
        return;
    }

    let workspace = tempfile::tempdir().unwrap();
    let writable = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();

    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![writable.path().to_path_buf()],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    // Writes inside a writable root succeed.
    let allowed = writable.path().join("allowed.txt");
    let child = match spawn_sandboxed_command(
        vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("printf ok > {}", allowed.to_string_lossy()),
        ],
        workspace.path().to_path_buf(),
        &policy,
        HashMap::new(),
    )
    .await
    {
        Ok(child) => child,
        // Enforcement unavailable on this runner (e.g. Landlock ABI
        // missing despite the LSM listing): skip rather than fail CI.
        Err(_) => return,
    };
    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    assert_eq!(std::fs::read_to_string(&allowed).unwrap(), "ok");

    // Writes outside every writable root are denied.
    let denied = outside.path().join("denied.txt");
    let child = match spawn_sandboxed_command(
        vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("printf blocked > {}", denied.to_string_lossy()),
        ],
        workspace.path().to_path_buf(),
        &policy,
        HashMap::new(),
    )
    .await
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let output = child.wait_with_output().await.unwrap();
    assert!(!output.status.success());
    assert!(!denied.exists());
}

/// Stage-1 residual: existing non-git trees stay fully writable, `.git`
/// content stays unwritable, and writing *content* into a brand-new root
/// child fails closed (no WriteFile on the root). Shell redirection may
/// still create an empty name via MakeReg.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_workspace_write_existing_children_ok_git_and_new_root_writes_denied() {
    if !is_sandbox_available() {
        return;
    }

    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join(".git")).unwrap();
    std::fs::write(
        workspace.path().join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .unwrap();
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src").join("lib.rs"), "before\n").unwrap();

    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: Vec::new(),
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    let child = match spawn_sandboxed_command(
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'after\\n' > src/lib.rs".to_string(),
        ],
        workspace.path().to_path_buf(),
        &policy,
        HashMap::new(),
    )
    .await
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "writing an existing non-git child must work: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("src").join("lib.rs")).unwrap(),
        "after\n"
    );

    let lock_path = workspace.path().join("Cargo.lock");
    let child = match spawn_sandboxed_command(
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf lock > Cargo.lock".to_string(),
        ],
        workspace.path().to_path_buf(),
        &policy,
        HashMap::new(),
    )
    .await
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let output = child.wait_with_output().await.unwrap();
    assert!(
        !output.status.success(),
        "writing a new root child must fail closed without WriteFile on root"
    );
    if lock_path.exists() {
        let body = std::fs::read(&lock_path).unwrap_or_default();
        assert!(
            body.is_empty(),
            "stage-1 may create an empty root name via MakeReg, but WriteFile must stay denied (got {:?})",
            String::from_utf8_lossy(&body)
        );
    }

    let child = match spawn_sandboxed_command(
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf evil > .git/HEAD".to_string(),
        ],
        workspace.path().to_path_buf(),
        &policy,
        HashMap::new(),
    )
    .await
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let output = child.wait_with_output().await.unwrap();
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(".git").join("HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_read_only_blocks_network_sockets() {
    if !is_sandbox_available() {
        return;
    }

    // The probe needs a program that can open a network socket; skip when
    // no Python interpreter is available on this runner.
    let python = ["python3", "python"].iter().find(|name| {
        std::process::Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    });
    let Some(python) = python else {
        return;
    };

    let workspace = tempfile::tempdir().unwrap();
    let probe_args = vec![
        (*python).to_string(),
        "-c".to_string(),
        "import socket; socket.socket(socket.AF_INET, socket.SOCK_STREAM)".to_string(),
    ];

    // Control: the probe works without a sandbox; otherwise it cannot
    // prove enforcement and the test is inconclusive on this runner.
    let control = spawn_unsandboxed_command(
        probe_args.clone(),
        workspace.path().to_path_buf(),
        HashMap::new(),
    )
    .await
    .unwrap();
    if !control.wait_with_output().await.unwrap().status.success() {
        return;
    }

    // Under ReadOnly the seccomp filter denies socket() for non-AF_UNIX
    // domains, so the probe must fail.
    let policy = SandboxPolicy::ReadOnly;
    let child = match spawn_sandboxed_command(
        probe_args,
        workspace.path().to_path_buf(),
        &policy,
        HashMap::new(),
    )
    .await
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let output = child.wait_with_output().await.unwrap();
    assert!(!output.status.success());
}

/// Regression test for the review finding: `spawn_unsandboxed_command`
/// (the `bypass_sandbox` helper) must not leak variables filtered out of
/// its caller's `env` map by inheriting Maestro's own environment.
///
/// This deliberately avoids `std::env::set_var`/`remove_var`: those
/// mutate real, process-wide state that every other test in this binary
/// shares, which is exactly the kind of cross-test race the existing
/// `env_lock()` helpers elsewhere in this crate (`config_cli.rs`) exist
/// to paper over. Instead this reads `HOME`,
/// a variable already guaranteed to be set in the process running the
/// test suite, without ever writing to the environment. `HOME` (unlike
/// `PATH`) has no shell-assigned fallback value, so an unset `$HOME`
/// inside the child unambiguously proves the environment was cleared
/// rather than merely overlaid.
#[tokio::test]
async fn spawn_unsandboxed_command_does_not_leak_process_environment() {
    assert!(
        std::env::var_os("HOME").is_some(),
        "test precondition: HOME must be set in the current process"
    );

    let mut filtered_env = HashMap::new();
    filtered_env.insert("ONLY_THIS_VAR".to_string(), "present".to_string());

    let child = spawn_unsandboxed_command(
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s|%s' \"${HOME:-absent}\" \"${ONLY_THIS_VAR:-absent}\"".to_string(),
        ],
        std::env::temp_dir(),
        filtered_env,
    )
    .await
    .unwrap();
    let output = child.wait_with_output().await.unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "absent|present",
        "spawn_unsandboxed_command must clear the parent environment before \
         applying the caller's filtered env map (HOME must not leak through), \
         not merely overlay the filtered map on top of it"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn spawn_sandboxed_command_does_not_leak_process_environment() {
    if !is_sandbox_available() {
        return;
    }
    assert!(
        std::env::var_os("HOME").is_some(),
        "test precondition: HOME must be set in the current process"
    );

    let workspace = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: Vec::new(),
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    let mut filtered_env = HashMap::new();
    filtered_env.insert("ONLY_THIS_VAR".to_string(), "present".to_string());

    let child = spawn_sandboxed_command(
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s|%s' \"${HOME:-absent}\" \"${ONLY_THIS_VAR:-absent}\"".to_string(),
        ],
        workspace.path().to_path_buf(),
        &policy,
        filtered_env,
    )
    .await;

    let child = match child {
        Ok(child) => child,
        // Enforcement unavailable on this runner: skip rather than fail CI.
        Err(_) => return,
    };
    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "absent|present",
        "a policy-sandboxed spawn must not inherit Maestro's own process \
         environment underneath its filtered env map (HOME must not leak through)"
    );
}

#[test]
fn metadata_protection_survives_broader_writable_roots() {
    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("workspace");
    let git = workspace.join(".git");
    std::fs::create_dir_all(&git).unwrap();
    let mut policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![parent.path().to_path_buf()],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    assert!(policy.allows_write_to(&workspace, &workspace.join("source.rs")));
    assert!(!policy.allows_write_to(&workspace, &git.join("config")));
    // An intentional narrow metadata grant still permits exactly that subtree.
    if let SandboxPolicy::WorkspaceWrite { writable_roots, .. } = &mut policy {
        writable_roots.push(git.join("allowed"));
    }
    assert!(policy.allows_write_to(&workspace, &git.join("allowed/new")));
    assert!(!policy.allows_write_to(&workspace, &git.join("private/new")));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_native_metadata_case_aliases_preserve_restrictions_and_grants() {
    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).unwrap();
    let case_insensitive = workspace.join(".GIT").exists();
    let mut policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![parent.path().to_path_buf()],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    for root in [&workspace, &parent.path().join("WORKSPACE")] {
        assert_eq!(
            policy.allows_write_to(&workspace, &root.join(".GIT/config")),
            !case_insensitive,
            "case aliases must not bypass the metadata exclusion: {root:?}",
        );
    }
    if let SandboxPolicy::WorkspaceWrite { writable_roots, .. } = &mut policy {
        writable_roots.push(workspace.join(".git/allowed"));
    }
    assert!(policy.allows_write_to(&workspace, &workspace.join(".git/allowed/new")));
    assert!(policy.allows_write_to(&workspace, &workspace.join(".GIT/ALLOWED/new")));
    assert_eq!(
        policy.allows_write_to(&workspace, &workspace.join(".GIT/private/new")),
        !case_insensitive,
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn sandbox_is_installed_before_executing_replaceable_lookup_paths() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    if !is_sandbox_available() {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let bin = workspace.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    // Sticky parents still allow their owner to replace these symlinks.
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o1777)).unwrap();
    let script = bin.join("payload");
    std::fs::write(&script, "#!/bin/sh\nprintf compromised > \"$PROBE\"\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&script, bin.join("sandbox-exec")).unwrap();
    symlink(&script, bin.join("bwrap")).unwrap();
    symlink("/bin/sh", bin.join("shell")).unwrap();
    let alias = workspace.path().join("alias");
    symlink(&bin, &alias).unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    for shell in [
        PathBuf::from("/bin/sh"),
        bin.join("shell"),
        alias.join("shell"),
    ] {
        let probe = outside.path().join("must-not-exist");
        let env = HashMap::from([
            ("PATH".to_string(), bin.to_string_lossy().into_owned()),
            ("PROBE".to_string(), probe.to_string_lossy().into_owned()),
        ]);
        let output = spawn_sandboxed_command(
            vec![
                shell.to_string_lossy().into_owned(),
                "-c".to_string(),
                "printf denied > \"$PROBE\"".to_string(),
            ],
            workspace.path().to_path_buf(),
            &policy,
            env,
        )
        .await
        .expect("available sandbox must start")
        .wait_with_output()
        .await
        .unwrap();
        assert!(
            !output.status.success(),
            "outside write through {shell:?} succeeded"
        );
        assert!(
            !probe.exists(),
            "lookup executable ran before sandbox enforcement"
        );
    }
    // The protected system executable remains usable under the same policy.
    let output = spawn_sandboxed_command(
        vec!["/bin/sh".into(), "-c".into(), "printf protected".into()],
        workspace.path().to_path_buf(),
        &policy,
        HashMap::new(),
    )
    .await
    .unwrap()
    .wait_with_output()
    .await
    .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"protected");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_seatbelt_metadata_aliases_cannot_use_broader_grant() {
    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("workspace");
    let git = workspace.join(".git");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("config"), "original").unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![parent.path().to_path_buf()],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    let case_insensitive = workspace.join(".GIT").exists();
    for spelling in [".git/config", ".GIT/config"] {
        if !case_insensitive && spelling.starts_with(".GIT") {
            continue;
        }
        let output = spawn_sandboxed_command(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf changed > \"$TARGET\"".into(),
            ],
            workspace.clone(),
            &policy,
            HashMap::from([("TARGET".into(), spelling.into())]),
        )
        .await
        .unwrap()
        .wait_with_output()
        .await
        .unwrap();
        assert!(
            !output.status.success(),
            "metadata alias {spelling} was writable"
        );
        assert_eq!(
            std::fs::read_to_string(git.join("config")).unwrap(),
            "original"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_native_metadata_aliases_are_protected_before_creation() {
    let workspace = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    // Determine the containing filesystem without creating the metadata path.
    std::fs::write(workspace.path().join("case-probe"), "").unwrap();
    let case_insensitive = workspace.path().join("CASE-PROBE").exists();
    assert!(!policy.allows_write_to(workspace.path(), &workspace.path().join(".git/config")));
    assert_eq!(
        policy.allows_write_to(workspace.path(), &workspace.path().join(".GIT/config")),
        !case_insensitive,
    );
    assert!(policy.allows_write_to(
        workspace.path(),
        &workspace.path().join("nested/.git/config")
    ));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_seatbelt_metadata_aliases_are_protected_before_creation() {
    let workspace = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    std::fs::write(workspace.path().join("case-probe"), "").unwrap();
    let case_insensitive = workspace.path().join("CASE-PROBE").exists();
    for spelling in [".git", ".GIT"] {
        if !case_insensitive && spelling == ".GIT" {
            continue;
        }
        let output = spawn_sandboxed_command(
            vec!["/bin/mkdir".into(), spelling.into()],
            workspace.path().to_path_buf(),
            &policy,
            HashMap::new(),
        )
        .await
        .unwrap()
        .wait_with_output()
        .await
        .unwrap();
        assert!(
            !output.status.success(),
            "metadata alias {spelling} was created"
        );
        assert!(!workspace.path().join(spelling).exists());
    }
    let output = spawn_sandboxed_command(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf allowed > ordinary".into(),
        ],
        workspace.path().to_path_buf(),
        &policy,
        HashMap::new(),
    )
    .await
    .unwrap()
    .wait_with_output()
    .await
    .unwrap();
    assert!(output.status.success());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("ordinary")).unwrap(),
        "allowed"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_metadata_alias_query_failure_does_not_grant_access() {
    // An invalid parent cannot establish filesystem equivalence. It must not
    // be treated as a case-sensitive volume that skips the metadata denial.
    let workspace = Path::new("/invalid\0parent");
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    assert!(!policy.allows_write_to(workspace, &workspace.join(".GIT/config")));
}
