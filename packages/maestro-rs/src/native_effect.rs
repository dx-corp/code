//! Snapshot and command execution occur together inside one approved argv.
use anyhow::{Context, Result, bail};
use maestro_local_host::checkpoints::{
    CheckpointStore, finalize_turn_snapshot, native_effect::begin_or_resume,
};
use std::{
    ffi::OsString,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
};

const OUTPUT_CAP: usize = 4096;
const RECEIPT_CAP: usize = 60 * 1024;

fn drain(mut pipe: impl Read) -> (String, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                truncated = true;
                break;
            }
            Ok(count) => {
                let remaining = OUTPUT_CAP.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..count.min(remaining)]);
                truncated |= count > remaining;
            }
        }
    }
    let mut output = String::from_utf8_lossy(&kept).into_owned();
    if output.len() > OUTPUT_CAP {
        let mut boundary = OUTPUT_CAP;
        while !output.is_char_boundary(boundary) {
            boundary -= 1;
        }
        output.truncate(boundary);
        truncated = true;
    }
    (output, truncated)
}

pub(crate) fn run(args: &[OsString]) -> Result<i32> {
    if args.len() == 3 && args[2] == "--protocol-version" {
        println!("maestro-native-code-effect-v1");
        return Ok(0);
    }
    if args.len() != 4 {
        bail!("native-code-effect requires its checkpoint key and command");
    }
    let key = args[2]
        .to_str()
        .context("native checkpoint key must be UTF-8")?;
    let digest = key
        .strip_prefix("native-code-checkpoint-")
        .context("invalid native checkpoint key")?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid native checkpoint key");
    }
    let command = args[3].to_str().context("native command must be UTF-8")?;
    if command.is_empty() || command.len() > 64 * 1024 || command.contains('\0') {
        bail!("invalid native command");
    }
    let cwd = std::env::current_dir()?;
    ensure_repository(&cwd)?;
    // The execution owner fixes production cwd to /workspace. Tests use their
    // own temporary worktree through the same snapshot and command path.
    let root = std::env::temp_dir().join("maestro-native-effects");
    std::fs::create_dir_all(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    let pending = begin_or_resume(&cwd, &root, key);
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().context("native command stdout")?;
    let stderr = child.stderr.take().context("native command stderr")?;
    let out = std::thread::spawn(move || drain(stdout));
    let err = std::thread::spawn(move || drain(stderr));
    let status = child.wait()?;
    let (stdout, out_truncated) = out
        .join()
        .map_err(|_| anyhow::anyhow!("native output reader failed"))?;
    let (stderr, err_truncated) = err
        .join()
        .map_err(|_| anyhow::anyhow!("native error reader failed"))?;
    let exit_code = status.code().unwrap_or(128);
    let mut changes = match pending
        .and_then(|pending| finalize_turn_snapshot(pending).ok().flatten())
    {
        Some(_) => captured_changes(&cwd, &root, key),
        None => {
            serde_json::json!({"availability":"unavailable","files":[],"reason":"The governed command could not capture this turn's complete file snapshot"})
        }
    };
    let base = serde_json::json!({"version":1,"checkpointKey":key,"exitCode":exit_code,"stdout":stdout,"stderr":stderr,"outputTruncated":out_truncated || err_truncated});
    let mut receipt = base;
    receipt["changes"] = changes.clone();
    if serde_json::to_vec(&receipt)?.len() > RECEIPT_CAP {
        if let Some(files) = changes["files"].as_array_mut() {
            for file in files {
                if file["availability"] == "patch" {
                    file["availability"] = serde_json::json!("truncated");
                }
                file["beforeContent"] = serde_json::Value::Null;
                file["afterContent"] = serde_json::Value::Null;
            }
        }
        receipt["changes"] = changes;
    }
    if serde_json::to_vec(&receipt)?.len() > RECEIPT_CAP {
        receipt["changes"] = serde_json::json!({"availability":"unavailable","files":[],"reason":"The saved file list exceeds the bounded command receipt"});
    }
    let bytes = serde_json::to_vec(&receipt)?;
    if bytes.len() > RECEIPT_CAP {
        bail!("native command receipt exceeds its bound");
    }
    std::io::stdout().write_all(&bytes)?;
    Ok(exit_code)
}

/// Initialization is itself inside this approved executable. A broken or
/// inaccessible existing repository never becomes permission to replace it.
fn ensure_repository(cwd: &Path) -> Result<()> {
    let probe = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .env("LC_ALL", "C")
        .current_dir(cwd)
        .output()?;
    if probe.status.success() {
        return Ok(());
    }
    for parent in cwd.ancestors() {
        match std::fs::symlink_metadata(parent.join(".git")) {
            Ok(_) => bail!("The existing repository could not be opened for governed capture"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => bail!("Repository ownership could not be verified for governed capture"),
        }
    }
    let error = std::str::from_utf8(&probe.stderr).unwrap_or("");
    if probe.status.code() != Some(128) || !error.starts_with("fatal: not a git repository (") {
        bail!("Repository discovery failed for governed capture");
    }
    let initialized = Command::new("git")
        .args(["init", "--quiet"])
        .env("LC_ALL", "C")
        .current_dir(cwd)
        .output()?;
    if !initialized.status.success() {
        bail!("Cannot initialize the governed workspace repository");
    }
    Ok(())
}

fn captured_changes(cwd: &Path, root: &Path, key: &str) -> serde_json::Value {
    match CheckpointStore::new(root, key).turn_snapshot(Some(0), cwd) {
        Ok(Some((_, files))) => serde_json::json!({"availability":"ready","files":files}),
        _ => {
            serde_json::json!({"availability":"unavailable","files":[],"reason":"The governed checkpoint is missing or exceeds its review bound"})
        }
    }
}
