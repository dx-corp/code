use std::{env, path::Path, process::Command};

fn git(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(directory)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    let out = env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR");
    let directory = env::var_os("CARGO_MANIFEST_DIR").expect("Cargo supplies manifest directory");
    let directory = Path::new(&directory);
    // An archive may be unpacked inside an unrelated repository: do not report
    // that repository's revision unless it actually tracks this crate manifest.
    let tracked = git(directory, &["ls-files", "--error-unmatch", "Cargo.toml"]).is_some();
    let commit = tracked
        .then(|| git(directory, &["rev-parse", "--verify", "HEAD"]))
        .flatten()
        .filter(|value| {
            matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
        });
    let dirty = if commit.is_some() {
        // Track source files and Git state instead of forcing a relink on every
        // Cargo invocation. Dirty means tracked edits, as with git describe
        // --dirty; unrelated untracked build outputs do not change provenance.
        if let (Some(root), Some(files)) = (
            git(directory, &["rev-parse", "--show-toplevel"]),
            git(directory, &["ls-files", "--full-name", "-z", "--", ":/"]),
        ) {
            for file in files.split('\0').filter(|file| !file.is_empty()) {
                println!(
                    "cargo:rerun-if-changed={}",
                    Path::new(&root).join(file).display()
                );
            }
        }
        let mut inputs = vec!["HEAD".to_owned(), "index".to_owned()];
        if let Some(reference) = git(directory, &["symbolic-ref", "--quiet", "HEAD"]) {
            inputs.push(reference);
        }
        for name in inputs {
            if let Some(path) = git(directory, &["rev-parse", "--git-path", &name]) {
                let mut path = directory.join(path);
                // Watch only this checkout's branch. Watching all shared refs
                // makes another worktree's fetch or commit invalidate this build.
                if !path.exists() && name.starts_with("refs/") {
                    if let Some(packed) =
                        git(directory, &["rev-parse", "--git-path", "packed-refs"])
                    {
                        let packed = directory.join(packed);
                        if packed.exists() {
                            println!("cargo:rerun-if-changed={}", packed.display());
                        }
                    }
                    // A packed branch can acquire a loose ref on its next
                    // commit. Watch its existing parent until that ref exists.
                    while !path.exists() && path.pop() {}
                }
                // A missing watched input makes Cargo rebuild perpetually.
                if path.exists() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
        git(
            directory,
            &["status", "--porcelain", "--untracked-files=no"],
        )
        .map(|status| if status.is_empty() { "clean" } else { "dirty" })
        .unwrap_or("unknown")
    } else {
        "unknown"
    };
    println!(
        "cargo:rustc-env=MAESTRO_BUILD_COMMIT={}",
        commit.as_deref().unwrap_or("unknown")
    );
    println!("cargo:rustc-env=MAESTRO_BUILD_TREE={dirty}");
    println!(
        "cargo:rustc-env=MAESTRO_BUILD_TARGET={}",
        env::var("TARGET").expect("Cargo supplies TARGET")
    );
    println!(
        "cargo:rustc-env=MAESTRO_BUILD_PROFILE={}",
        // PROFILE collapses custom profiles to dev/release. Cargo's output
        // directory retains the selected profile name, including cross builds.
        Path::new(&out)
            .ancestors()
            .nth(3)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(|name| if name == "debug" { "dev" } else { name })
            .expect("Cargo output directory contains the selected profile")
    );
}
