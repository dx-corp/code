//! Safe mode validators.
//!
//! Runs configured validators after file mutations.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::lsp::{self, LspDiagnostic};
use crate::tools::resolve_shell_config;

pub use maestro_runtime::ValidatorResult;

#[derive(Debug, Clone)]
struct SafeModeConfig {
    enabled: bool,
    validators: Vec<String>,
    lsp_blocking_severity: u8,
}

static SAFE_MODE_CONFIG: std::sync::LazyLock<Mutex<SafeModeConfig>> =
    std::sync::LazyLock::new(|| {
        let enabled = std::env::var("MAESTRO_SAFE_MODE").ok().as_deref() == Some("1");
        let validators_raw = std::env::var("MAESTRO_SAFE_VALIDATORS").unwrap_or_default();
        let validators = validators_raw
            .split(',')
            .map(|entry| entry.trim().to_string())
            .filter(|entry| !entry.is_empty())
            .collect();
        let lsp_blocking_severity = std::env::var("MAESTRO_SAFE_LSP_SEVERITY")
            .ok()
            .and_then(|value| value.parse::<u8>().ok())
            .map_or_else(lsp::blocking_severity, |value| value.clamp(1, 4));

        Mutex::new(SafeModeConfig {
            enabled,
            validators,
            lsp_blocking_severity,
        })
    });

/// Return true if safe mode is enabled.
pub fn is_safe_mode_enabled() -> bool {
    SAFE_MODE_CONFIG
        .lock()
        .map(|cfg| cfg.enabled)
        .unwrap_or(false)
}

/// Run validators configured via `MAESTRO_SAFE_VALIDATORS`.
pub async fn run_validators(paths: &[String]) -> Result<Vec<ValidatorResult>, String> {
    run_validators_with_diagnostics(paths, None).await
}

pub async fn run_validators_with_diagnostics(
    paths: &[String],
    lsp_diagnostics: Option<&HashMap<String, Vec<LspDiagnostic>>>,
) -> Result<Vec<ValidatorResult>, String> {
    let cfg = SAFE_MODE_CONFIG
        .lock()
        .map_err(|_| "Safe mode config unavailable".to_string())?
        .clone();

    if !cfg.enabled {
        return Ok(Vec::new());
    }

    if let Some(diagnostics) = lsp_diagnostics {
        let blocking = find_blocking_diagnostics(diagnostics, cfg.lsp_blocking_severity);
        if !blocking.is_empty() {
            let summary = blocking
                .into_iter()
                .map(|entry| {
                    format!(
                        "{}:{}:{} {}",
                        entry.file,
                        entry.line + 1,
                        entry.character + 1,
                        entry.message
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!(
                "LSP diagnostics blocked safe-mode validators:\n{summary}"
            ));
        }
    }

    if cfg.validators.is_empty() {
        return Ok(Vec::new());
    }

    let (shell, shell_args) = resolve_shell_config()
        .map_err(|e| format!("Failed to resolve shell for validators: {e}"))?;

    let mut results = Vec::new();
    for command in cfg.validators {
        let mut cmd = tokio::process::Command::new(&shell);
        cmd.args(&shell_args)
            .arg(command.clone())
            .env("MAESTRO_SAFE_CHANGED_PATHS", paths.join("::"));

        let output = cmd
            .output()
            .await
            .map_err(|e| format!("Failed to run validator '{command}': {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        let result = ValidatorResult {
            command: command.clone(),
            stdout,
            stderr,
            exit_code,
        };

        if !output.status.success() {
            return Err(format!(
                "Validator failed ({}). Exit code: {}. Stderr: {}",
                command,
                exit_code,
                result.stderr.trim()
            ));
        }

        results.push(result);
    }

    Ok(results)
}

#[derive(Debug, Clone)]
struct BlockingDiagnostic {
    file: String,
    line: u32,
    character: u32,
    message: String,
}

fn find_blocking_diagnostics(
    diagnostics: &HashMap<String, Vec<LspDiagnostic>>,
    threshold: u8,
) -> Vec<BlockingDiagnostic> {
    let mut blocking = Vec::new();
    for (file, entries) in diagnostics {
        for diag in entries {
            let severity = diag.severity.unwrap_or(u8::MAX);
            if severity <= threshold {
                blocking.push(BlockingDiagnostic {
                    file: file.clone(),
                    line: diag.range.start.line,
                    character: diag.range.start.character,
                    message: diag.message.clone(),
                });
            }
        }
    }
    blocking
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{LspPosition, LspRange};

    // ========================================================================
    // ValidatorResult Tests
    // ========================================================================

    #[test]
    fn test_validator_result_serialization() {
        let result = ValidatorResult {
            command: "npm test".to_string(),
            stdout: "All tests passed".to_string(),
            stderr: String::new(),
            exit_code: 0,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["command"], "npm test");
        assert_eq!(json["exit_code"], 0);
    }

    #[test]
    fn test_validator_result_with_error() {
        let result = ValidatorResult {
            command: "cargo clippy".to_string(),
            stdout: String::new(),
            stderr: "error: unused variable".to_string(),
            exit_code: 1,
        };
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("unused variable"));
    }

    // ========================================================================
    // BlockingDiagnostic Tests
    // ========================================================================

    #[test]
    fn test_find_blocking_diagnostics_empty() {
        let diagnostics: HashMap<String, Vec<LspDiagnostic>> = HashMap::new();
        let blocking = find_blocking_diagnostics(&diagnostics, 2);
        assert!(blocking.is_empty());
    }

    #[test]
    fn test_find_blocking_diagnostics_with_error() {
        let mut diagnostics = HashMap::new();
        diagnostics.insert(
            "src/main.rs".to_string(),
            vec![LspDiagnostic {
                range: LspRange {
                    start: LspPosition {
                        line: 10,
                        character: 5,
                    },
                    end: LspPosition {
                        line: 10,
                        character: 15,
                    },
                },
                message: "undefined variable".to_string(),
                severity: Some(1), // Error
                source: None,
            }],
        );

        // Threshold 2 (warning) should include errors (severity 1)
        let blocking = find_blocking_diagnostics(&diagnostics, 2);
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].file, "src/main.rs");
        assert_eq!(blocking[0].line, 10);
        assert_eq!(blocking[0].character, 5);
        assert_eq!(blocking[0].message, "undefined variable");
    }

    #[test]
    fn test_find_blocking_diagnostics_filters_by_severity() {
        let mut diagnostics = HashMap::new();
        diagnostics.insert(
            "src/lib.rs".to_string(),
            vec![
                LspDiagnostic {
                    range: LspRange {
                        start: LspPosition {
                            line: 1,
                            character: 0,
                        },
                        end: LspPosition {
                            line: 1,
                            character: 10,
                        },
                    },
                    message: "error message".to_string(),
                    severity: Some(1), // Error
                    source: None,
                },
                LspDiagnostic {
                    range: LspRange {
                        start: LspPosition {
                            line: 5,
                            character: 0,
                        },
                        end: LspPosition {
                            line: 5,
                            character: 20,
                        },
                    },
                    message: "hint message".to_string(),
                    severity: Some(4), // Hint
                    source: None,
                },
            ],
        );

        // Threshold 1 (error only) should only include the error
        let blocking = find_blocking_diagnostics(&diagnostics, 1);
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].message, "error message");

        // Threshold 4 should include both
        let all = find_blocking_diagnostics(&diagnostics, 4);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_find_blocking_diagnostics_default_severity() {
        let mut diagnostics = HashMap::new();
        diagnostics.insert(
            "src/test.rs".to_string(),
            vec![LspDiagnostic {
                range: LspRange {
                    start: LspPosition {
                        line: 0,
                        character: 0,
                    },
                    end: LspPosition {
                        line: 0,
                        character: 0,
                    },
                },
                message: "no severity".to_string(),
                severity: None, // Will default to u8::MAX
                source: None,
            }],
        );

        // With any normal threshold, diagnostics without severity should not block
        let blocking = find_blocking_diagnostics(&diagnostics, 4);
        assert!(blocking.is_empty());
    }

    #[test]
    fn test_find_blocking_diagnostics_multiple_files() {
        let mut diagnostics = HashMap::new();
        diagnostics.insert(
            "src/a.rs".to_string(),
            vec![LspDiagnostic {
                range: LspRange {
                    start: LspPosition {
                        line: 1,
                        character: 0,
                    },
                    end: LspPosition {
                        line: 1,
                        character: 5,
                    },
                },
                message: "error in a".to_string(),
                severity: Some(1),
                source: None,
            }],
        );
        diagnostics.insert(
            "src/b.rs".to_string(),
            vec![LspDiagnostic {
                range: LspRange {
                    start: LspPosition {
                        line: 2,
                        character: 0,
                    },
                    end: LspPosition {
                        line: 2,
                        character: 5,
                    },
                },
                message: "error in b".to_string(),
                severity: Some(1),
                source: None,
            }],
        );

        let blocking = find_blocking_diagnostics(&diagnostics, 2);
        assert_eq!(blocking.len(), 2);
    }
}
