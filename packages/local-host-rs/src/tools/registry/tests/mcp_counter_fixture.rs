use std::path::Path;

pub(super) fn failing_counter_server(server_name: &str, counter_path: &Path) -> serde_json::Value {
    serde_json::json!({
        "name": server_name,
        "transport": "stdio",
        "command": "sh",
        // Leave enough headroom for a loaded parallel test process to
        // schedule the child before the handshake timeout expires.
        "timeout": 500,
        // These tests exercise retry/reload mechanics, not the workspace
        // trust gate, so opt the server out of the approval requirement.
        "requiresProjectApproval": false,
        "args": [
            "-c",
            // Record the attempt using shell builtins before failing. A
            // separate `cat` process can outlive the handshake deadline on
            // a loaded host, leaving the attempt uncounted when the shell
            // is killed. The retry assertions require the counter itself
            // to be independent of external executable scheduling.
            "count=0; if [ -f \"$1\" ]; then IFS= read -r count < \"$1\"; fi; printf '%s\\n' \"$((count + 1))\" > \"$1\"; exit 1",
            "sh",
            counter_path.display().to_string()
        ]
    })
}

pub(super) fn read_counter(counter_path: &Path) -> usize {
    std::fs::read_to_string(counter_path)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

#[test]
fn failing_counter_server_does_not_require_external_path_utilities() {
    let temp = tempfile::tempdir().expect("counter fixture directory");
    let counter_path = temp.path().join("counter.txt");
    let fixture = failing_counter_server("counter-fixture", &counter_path);
    let shell = std::env::split_paths(&std::env::var_os("PATH").expect("shell search path"))
        .map(|directory| directory.join("sh"))
        .find(|candidate| candidate.is_file())
        .expect("test shell");
    let args: Vec<_> = fixture["args"]
        .as_array()
        .expect("fixture arguments")
        .iter()
        .map(|argument| argument.as_str().expect("string argument"))
        .collect();
    for expected in 1..=2 {
        let output = std::process::Command::new(&shell)
            .args(&args)
            .env("PATH", temp.path())
            .output()
            .expect("run failing server fixture");
        assert_eq!(output.status.code(), Some(1), "fixture must still fail");
        assert_eq!(read_counter(&counter_path), expected);
    }
}
