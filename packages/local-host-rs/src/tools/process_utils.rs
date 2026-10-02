//! Shared process helpers for tool execution.

/// Return whether a Unix process group still has at least one member.
#[cfg(unix)]
pub(crate) fn process_group_exists(process_group_id: u32) -> bool {
    if process_group_id <= 1 {
        return false;
    }
    let Ok(process_group_id) = i32::try_from(process_group_id) else {
        return false;
    };
    // SAFETY: `kill` with signal 0 performs an existence/permission check and
    // only accepts integer arguments. A negative PID addresses the process
    // group whose ID is the absolute value.
    let result = unsafe { libc::kill(-process_group_id, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Kill a Unix process group by its stable group identity.
///
/// This works after the original leader exits while descendants remain.
#[cfg(unix)]
pub(crate) fn kill_process_group(process_group_id: u32) {
    if process_group_id <= 1 {
        return;
    }
    let Ok(process_group_id) = i32::try_from(process_group_id) else {
        return;
    };
    // SAFETY: `kill` only accepts integer arguments. A negative PID targets
    // the process group rather than an individual process.
    unsafe {
        let _ = libc::kill(-process_group_id, libc::SIGKILL);
    }
}

#[cfg(unix)]
fn descendant_processes(root_pid: u32) -> Vec<u32> {
    use std::process::Command;

    let mut descendants = Vec::new();
    let mut pending = vec![root_pid];
    while let Some(parent_pid) = pending.pop() {
        let Ok(output) = Command::new("pgrep")
            .args(["-P", &parent_pid.to_string()])
            .output()
        else {
            continue;
        };
        for child_pid in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
        {
            if !descendants.contains(&child_pid) {
                descendants.push(child_pid);
                pending.push(child_pid);
            }
        }
    }
    descendants
}

/// Kill an entire process tree by PID.
///
/// Signal parents before children: killing a child first can wake a waiting
/// shell and let it execute another command before shutdown reaches the shell.
/// Snapshot owned process groups before any leaders exit, then sweep those
/// groups for children forked after discovery.
#[cfg(unix)]
pub(crate) fn kill_process_tree_tracked(pid: u32) -> Vec<u32> {
    if pid <= 1 || i32::try_from(pid).is_err() {
        return Vec::new();
    }
    let processes: Vec<i32> = std::iter::once(pid)
        .chain(descendant_processes(pid))
        .filter_map(|pid| i32::try_from(pid).ok())
        .collect();
    let mut process_groups = Vec::new();
    for &pid in &processes {
        // SAFETY: `getpgid` accepts and returns integer process identifiers.
        let group = unsafe { libc::getpgid(pid) };
        // A group's leader must belong to this tree before we may kill the
        // whole group; a descendant can belong to an unrelated caller's group.
        if group == pid && !process_groups.contains(&(group as u32)) {
            process_groups.push(group as u32);
        }
    }
    for pid in processes {
        // SAFETY: `kill` takes integer identifiers. Each positive PID came
        // from the requested root or its descendant snapshot, parent first.
        unsafe {
            let _ = libc::kill(pid, libc::SIGKILL);
        }
    }
    for &group in &process_groups {
        kill_process_group(group);
    }
    process_groups
}

#[cfg(unix)]
pub(crate) fn kill_process_tree(pid: u32) {
    let _ = kill_process_tree_tracked(pid);
}

/// Establish a dedicated subprocess group and Linux orphan ownership before exec.
/// Containment failures are returned by the command's spawn operation.
#[cfg(unix)]
pub fn set_new_process_group(cmd: &mut tokio::process::Command) {
    set_std_process_group(cmd.as_std_mut());
}

/// Make the spawned Linux process a child subreaper.
///
/// Session-detached grandchildren are then reparented to the supervising shell
/// instead of escaping to the host process. The shell remains alive until its
/// adopted descendants exit, so cancellation can still discover and terminate
/// the complete tree.
#[cfg(target_os = "linux")]
pub(crate) fn set_child_subreaper(cmd: &mut tokio::process::Command) {
    // SAFETY: `pre_exec` runs after fork and before exec. `prctl` with
    // PR_SET_CHILD_SUBREAPER takes integer arguments only, performs no
    // allocation, and is safe to invoke in this restricted child context.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn set_child_subreaper(_cmd: &mut tokio::process::Command) {}

#[cfg(not(unix))]
pub(crate) fn kill_process_tree_tracked(pid: u32) -> Vec<u32> {
    use std::process::Command;

    // On Windows, use taskkill /T /F /PID <pid>
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .output();
    Vec::new()
}

#[cfg(not(unix))]
pub(crate) fn kill_process_tree(pid: u32) {
    let _ = kill_process_tree_tracked(pid);
}

#[cfg(not(unix))]
pub(crate) fn set_new_process_group(_cmd: &mut tokio::process::Command) {}

/// Establish containment before exec; failure is returned by spawn.
#[cfg(unix)]
pub(crate) fn set_std_process_group(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    if let Err(errno) = enable_descendant_reaping() {
        // Refuse the spawn if this host cannot own orphaned descendants.
        // SAFETY: the child closure only constructs an errno-backed error.
        unsafe {
            cmd.pre_exec(move || Err(std::io::Error::from_raw_os_error(errno)));
        }
    }
}

/// Killing a shell before its children reparents them. Own those orphans on
/// Linux instead of depending on PID 1 to reap them before cancellation ends.
#[cfg(target_os = "linux")]
fn enable_descendant_reaping() -> Result<(), i32> {
    static ENABLED: std::sync::OnceLock<Result<(), i32>> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        // SAFETY: this process-wide ownership flag takes integer arguments.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO))
        }
    })
}

/// Reap only adopted children in the groups this cancellation owns. The
/// direct child stays with its existing Child/wait owner; never steal its
/// exit status or reap a different tool's group.
#[cfg(target_os = "linux")]
pub(crate) fn reap_owned_process_groups(groups: &[u32], direct_child: Option<u32>) {
    let tasks = match std::fs::read_dir("/proc/self/task") {
        Ok(tasks) => tasks,
        Err(error) => {
            tracing::warn!(%error, "cannot inspect adopted tool children");
            return;
        }
    };
    for task in tasks.flatten() {
        // A worker thread can exit between listing its task and reading it.
        let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        for child in children
            .split_whitespace()
            .filter_map(|pid| pid.parse::<u32>().ok())
        {
            if Some(child) == direct_child {
                continue;
            }
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{child}/stat")) else {
                continue;
            };
            let Some((_, fields)) = stat.rsplit_once(") ") else {
                continue;
            };
            let mut fields = fields.split_whitespace();
            let state = fields.next();
            let parent = fields.next().and_then(|pid| pid.parse::<u32>().ok());
            let group = fields.next().and_then(|pid| pid.parse::<u32>().ok());
            if state != Some("Z")
                || parent != Some(std::process::id())
                || !group.is_some_and(|group| groups.contains(&group))
            {
                continue;
            }
            let Ok(child) = i32::try_from(child) else {
                continue;
            };
            // SAFETY: a positive, adopted zombie in an owned group only.
            // WNOHANG never waits for a live child; ECHILD means another
            // cleanup owner already reaped this process.
            if unsafe { libc::waitpid(child, std::ptr::null_mut(), libc::WNOHANG) } < 0 {
                let error = std::io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(libc::ECHILD | libc::EINTR)) {
                    tracing::warn!(%error, "cannot reap an adopted tool child");
                }
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn reap_owned_process_groups(_groups: &[u32], _direct_child: Option<u32>) {}

/// Preserve remaining descendant identities after the bounded signal sweep.
/// A dying leader may not orphan its children until after that sweep finishes.
/// This worker only reaps recorded zombies adopted by us; it never signals a
/// process, takes the direct Child's status, or follows a reused PID/group.
#[cfg(target_os = "linux")]
fn finish_owned_process_group_reaping(groups: &[u32], direct_child: Option<u32>) {
    reap_owned_process_groups(groups, direct_child);
    fn identity(pid: u32) -> Option<(char, u32, u32, u64)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, fields) = stat.rsplit_once(") ")?;
        let fields: Vec<_> = fields.split_whitespace().collect();
        Some((
            fields.first()?.chars().next()?,
            fields.get(1)?.parse().ok()?,
            fields.get(2)?.parse().ok()?,
            fields.get(19)?.parse().ok()?,
        ))
    }
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return;
    };
    let mut owned: Vec<_> = processes
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if Some(pid) == direct_child || pid <= 1 || i32::try_from(pid).is_err() {
                return None;
            }
            let (_, _, group, started) = identity(pid)?;
            groups.contains(&group).then_some((pid, group, started))
        })
        .collect();
    if owned.is_empty() {
        return;
    }
    let owner = std::process::id();
    if let Err(error) = std::thread::Builder::new()
        .name("maestro-owned-reaper".into())
        .spawn(move || {
            for _ in 0..100 {
                owned.retain(|&(pid, group, started)| {
                    let Some((state, parent, current_group, current_started)) = identity(pid)
                    else {
                        return false;
                    };
                    if current_group != group || current_started != started {
                        return false;
                    }
                    if state != 'Z' || parent != owner {
                        return true;
                    }
                    // SAFETY: a recorded, adopted zombie only. Positive PID
                    // and WNOHANG cannot wait for/reap a live or direct child.
                    let result =
                        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
                    if result == pid as i32 {
                        return false;
                    }
                    if result < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() == Some(libc::ECHILD) {
                            return false;
                        }
                        if error.raw_os_error() != Some(libc::EINTR) {
                            tracing::warn!(%error, pid, "cannot reap a late adopted tool child");
                        }
                    }
                    true
                });
                if owned.is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        })
    {
        tracing::warn!(%error, "cannot start late adopted tool child reaping");
    }
}

/// Own a group created before exec, including pipes inherited after leader exit.
#[cfg(unix)]
pub struct ProcessGroupGuard(Option<u32>);

#[cfg(unix)]
impl ProcessGroupGuard {
    /// Retain ownership of a child spawned in a dedicated process group.
    /// The direct child remains the caller's responsibility to wait.
    pub fn new(pid: Option<u32>) -> Self {
        Self(pid.filter(|pid| *pid > 1 && i32::try_from(*pid).is_ok()))
    }
    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
    /// Signal the owned group while retaining it for cleanup on drop.
    pub fn terminate(&mut self) {
        if let Some(pid) = self.0 {
            kill_process_group(pid);
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        // A shell can be in fork while the first group signal is delivered.
        // Keep the group identity through that boundary and sweep children
        // that inherit its pipes. Do not wait indefinitely for unreaped zombies.
        for _ in 0..20 {
            self.terminate();
            reap_owned_process_groups(&[pid], Some(pid));
            if !process_group_exists(pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        #[cfg(target_os = "linux")]
        finish_owned_process_group_reaping(&[pid], Some(pid));
        self.disarm();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn group_cleanup_reaps_orphans_without_stealing_the_direct_child_status() {
        use tokio::io::AsyncBufReadExt;
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "sleep 60 & printf '%s\n' \"$!\"; exit 7"]);
        command.stdout(std::process::Stdio::piped());
        set_new_process_group(&mut command);
        let mut child = command.spawn().expect("owned command");
        let root = child.id().expect("direct child pid");
        let guard = ProcessGroupGuard::new(Some(root));
        let mut reader = tokio::io::BufReader::new(child.stdout.take().expect("pid pipe"));
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("descendant pid");
        let descendant: i32 = line.trim().parse().expect("pid");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let stat =
                    std::fs::read_to_string(format!("/proc/{root}/stat")).expect("direct child");
                if stat
                    .rsplit_once(") ")
                    .is_some_and(|(_, fields)| fields.starts_with("Z "))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("direct child exits without being reaped");
        reap_owned_process_groups(&[root], Some(root));
        // The original Child remains the sole owner of its exit status.
        assert_eq!(
            child.wait().await.expect("direct child status").code(),
            Some(7)
        );
        drop(guard);
        // Reaping, not merely SIGKILL: signal 0 must see no zombie either.
        // SAFETY: signal 0 only probes this test's recorded descendant.
        assert_eq!(unsafe { libc::kill(descendant, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[tokio::test]
    async fn dropping_group_owner_stops_commands_after_readiness() {
        use tokio::io::AsyncReadExt;
        for _ in 0..32 {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("unexpected");
            let mut command = tokio::process::Command::new("sh");
            command
                .args(["-c", "printf ready; sleep 30; touch unexpected"])
                .current_dir(dir.path())
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true);
            set_new_process_group(&mut command);
            let mut child = command.spawn().unwrap();
            let guard = ProcessGroupGuard::new(child.id());
            let mut stdout = child.stdout.take().unwrap();
            let mut ready = [0; 5];
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stdout.read_exact(&mut ready),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(&ready, b"ready");
            drop(guard);
            let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap();
            assert!(!status.success());
            let mut tail = Vec::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stdout.read_to_end(&mut tail),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(tail.is_empty());
            assert!(!marker.exists());
        }
    }

    #[cfg(target_os = "linux")]
    fn late_adoption_fixture() -> (std::process::Child, i32, tempfile::TempDir) {
        const FIXTURE: &str = "MAESTRO_LATE_ADOPTION_FIXTURE";
        if let Some(path) = std::env::var_os(FIXTURE) {
            let path = std::path::PathBuf::from(path);
            let child = std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap();
            std::fs::write(path.join("pid"), child.id().to_string()).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !path.join("release").exists() {
                if std::time::Instant::now() >= deadline {
                    std::process::exit(8);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            // Exit without reaping this child, exactly as a killed shell does.
            std::process::exit(7);
        }
        let dir = tempfile::tempdir().unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                std::thread::current().name().expect("test name"),
                "--nocapture",
            ])
            .env(FIXTURE, dir.path());
        set_std_process_group(&mut command);
        let child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let descendant = loop {
            if let Some(pid) = std::fs::read_to_string(dir.path().join("pid"))
                .ok()
                .and_then(|text| text.parse::<i32>().ok())
            {
                break pid;
            }
            assert!(std::time::Instant::now() < deadline, "fixture pid missing");
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        (child, descendant, dir)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn late_adopted_zombie_is_reaped_without_stealing_direct_child_status() {
        let (mut child, descendant, dir) = late_adoption_fixture();
        let root = child.id();
        let guard = ProcessGroupGuard::new(Some(root));
        // SAFETY: only this fixture's recorded descendant is signaled.
        assert_eq!(unsafe { libc::kill(descendant, libc::SIGKILL) }, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{descendant}/stat")).unwrap();
            if stat.rsplit_once(") ").is_some_and(|(_, fields)| {
                let fields: Vec<_> = fields.split_whitespace().collect();
                fields[0] == "Z" && fields[1] == root.to_string()
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "descendant did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        finish_owned_process_group_reaping(&[root], Some(root));
        // Adopt only after the synchronous sweep has ended.
        std::fs::write(dir.path().join("release"), "").unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(7));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            // SAFETY: signal 0 probes only this fixture's recorded descendant.
            if unsafe { libc::kill(descendant, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "late adopted zombie was not reaped"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(guard);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deferred_reaping_never_terminates_a_live_descendant() {
        let (mut child, descendant, dir) = late_adoption_fixture();
        let root = child.id();
        let guard = ProcessGroupGuard::new(Some(root));
        finish_owned_process_group_reaping(&[root], Some(root));
        std::fs::write(dir.path().join("release"), "").unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(7));
        std::thread::sleep(std::time::Duration::from_millis(50));
        // SAFETY: signal 0 probes only this fixture's recorded descendant.
        assert_eq!(unsafe { libc::kill(descendant, 0) }, 0);
        let stat = std::fs::read_to_string(format!("/proc/{descendant}/stat")).unwrap();
        assert!(!stat.rsplit_once(") ").unwrap().1.starts_with("Z "));
        drop(guard);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            // SAFETY: signal 0 probes only this fixture's recorded descendant.
            if unsafe { libc::kill(descendant, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "owned live child was not killed and reaped"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn terminate_keeps_ownership_until_explicit_disarm() {
        let mut guard = ProcessGroupGuard::new(None);
        guard.terminate();
        assert!(guard.0.is_none());
        // Use an impossible PID to test ownership without signaling a live group.
        guard.0 = Some(i32::MAX as u32);
        guard.terminate();
        assert_eq!(guard.0, Some(i32::MAX as u32));
        guard.disarm();
        assert!(guard.0.is_none());
    }

    #[test]
    fn invalid_process_roots_do_not_signal_the_calling_group() {
        assert!(!process_group_exists(0));
        kill_process_group(0);
        assert!(!process_group_exists(1));
        kill_process_group(1);
        assert!(kill_process_tree_tracked(1).is_empty());
        assert!(kill_process_tree_tracked(0).is_empty());
        assert!(kill_process_tree_tracked(u32::MAX).is_empty());
    }
}
