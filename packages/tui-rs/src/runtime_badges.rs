//! Runtime badges for the Rust TUI status bar.
//!
//! Mirrors the TypeScript TUI runtime badges for environment and safety hints.
//!
//! # Performance
//!
//! Status-bar badges are rebuilt every frame. Anything that touches the
//! filesystem (especially directory scans of `/lib*`) must be **cached** —
//! magic-trace / `perf` on Linux showed `build_runtime_badges` → `read_dir`
//! dominating idle render cycles via `is_musl_env()`.

use std::env;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use crate::path_utils::{legacy_composer_home_dir, maestro_home_dir, resolve_env_path};
use crate::safety::is_safe_mode_enabled;
use crate::sandbox::SANDBOX_ENV_VAR;
use crate::session::ThinkingLevel;
use crate::state::ApprovalMode;
use crate::terminal_info::{is_ssh_session, is_wsl};
use crate::tools::background_process_count;

pub struct RuntimeBadges {
    pub core: Vec<String>,
    pub env: Vec<String>,
}

pub struct RuntimeBadgeParams {
    pub approval_mode: ApprovalMode,
    pub thinking_level: ThinkingLevel,
    pub mcp_connected: usize,
    pub mcp_tool_count: usize,
    pub mcp_failed: usize,
    pub alert_count: usize,
    /// Session sandbox policy label (e.g. `workspace-write`, `read-only`, `none`).
    pub sandbox_policy: Option<String>,
    /// Whether the workspace is trusted in global config for project skills/plugins.
    pub workspace_trusted: bool,
    /// Pending tool-approval count for the status bar.
    pub pending_approvals: usize,
}

pub fn build_runtime_badges(params: RuntimeBadgeParams) -> RuntimeBadges {
    let mut core = Vec::new();
    let mut env_badges = Vec::new();

    if is_safe_mode_enabled() {
        core.push("safe:on".to_string());
    }

    if env::var("MAESTRO_PLAN_MODE").ok().as_deref() == Some("1") {
        core.push("plan:on".to_string());
    }

    if replay_mode_enabled() {
        core.push("replay:on".to_string());
    }

    core.extend(enterprise_runtime_badges());

    core.push(format!(
        "approvals:{}",
        approval_label(params.approval_mode)
    ));

    if params.pending_approvals > 0 {
        core.push(maestro_ui::localization::format(
            "pending:{0}",
            &[(params.pending_approvals).to_string()],
        ));
    }

    core.push(if params.workspace_trusted {
        "trust:yes".to_string()
    } else {
        "trust:no".to_string()
    });

    if let Some(policy) = params
        .sandbox_policy
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        core.push(format!("sandbox:{policy}"));
    } else if let Ok(value) = env::var(SANDBOX_ENV_VAR) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            core.push(format!("sandbox:{trimmed}"));
        }
    } else {
        core.push("sandbox:none".to_string());
    }

    if params.alert_count > 0 {
        core.push(maestro_ui::localization::format(
            "alerts:{0}",
            &[(params.alert_count).to_string()],
        ));
    }

    if let Some(label) = thinking_badge_label(params.thinking_level) {
        core.push(format!("think:{label}"));
    }

    if params.mcp_connected > 0 || params.mcp_failed > 0 {
        let tool_suffix = if params.mcp_connected > 0 {
            format!("({})", params.mcp_tool_count)
        } else {
            String::new()
        };
        let failure_suffix = if params.mcp_failed > 0 {
            format!("!{}", params.mcp_failed)
        } else {
            String::new()
        };
        core.push(format!(
            "mcp:{}{}{}",
            params.mcp_connected, tool_suffix, failure_suffix
        ));
    }

    let background_count = background_process_count();
    if background_count > 0 {
        core.push(format!("bg:{background_count}"));
    }

    // Host/term environment is process-static — never re-scan the filesystem.
    env_badges.extend(cached_env_badges().iter().cloned());

    RuntimeBadges {
        core,
        env: env_badges,
    }
}

/// Process-lifetime cache of env/term badges (podman/docker/wsl/ssh/musl/tmux/…).
fn cached_env_badges() -> &'static [String] {
    static CACHE: OnceLock<Vec<String>> = OnceLock::new();
    CACHE.get_or_init(compute_env_badges).as_slice()
}

fn compute_env_badges() -> Vec<String> {
    let mut env_badges = Vec::new();

    if is_podman_env() {
        env_badges.push("env:podman".to_string());
    } else if is_docker_env() {
        env_badges.push("env:docker".to_string());
    } else if is_wsl() {
        env_badges.push("env:wsl".to_string());
    }

    if is_ssh_session() {
        env_badges.push("env:ssh".to_string());
    }
    if is_flatpak_env() {
        env_badges.push("env:flatpak".to_string());
    }
    if is_bubblewrap_env() {
        env_badges.push("env:bwrap".to_string());
    }
    if is_musl_env() {
        env_badges.push("env:musl".to_string());
    }

    if is_tmux_env() {
        env_badges.push("term:tmux".to_string());
    } else if is_screen_env() {
        env_badges.push("term:screen".to_string());
    }

    if is_jetbrains_terminal() {
        env_badges.push("term:jetbrains".to_string());
    }

    env_badges
}

fn approval_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Yolo => "yolo",
        ApprovalMode::Selective => "selective",
        ApprovalMode::Safe => "safe",
    }
}

fn thinking_badge_label(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some(maestro_ui::localization::tr("minimal")),
        ThinkingLevel::Low => Some(maestro_ui::localization::tr("low")),
        ThinkingLevel::Medium => Some(maestro_ui::localization::tr("medium")),
        ThinkingLevel::High => Some(maestro_ui::localization::tr("high")),
        ThinkingLevel::XHigh => Some("xhigh"),
        ThinkingLevel::Max => Some("max"),
    }
}

fn replay_mode_enabled() -> bool {
    env::var("MAESTRO_MODE")
        .map(|value| value.trim().eq_ignore_ascii_case("replay"))
        .unwrap_or(false)
        || env::var("MAESTRO_SCENARIO_PATH")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
}

pub fn enterprise_badges_from_sources(
    enterprise_mode: bool,
    policy_present: bool,
    enterprise_mcp_present: bool,
) -> Vec<String> {
    let mut badges = Vec::new();
    if enterprise_mode || policy_present || enterprise_mcp_present {
        badges.push("ent:on".to_string());
    }
    if policy_present {
        badges.push("ent:policy".to_string());
    }
    if enterprise_mcp_present {
        badges.push("ent:mcp".to_string());
    }
    badges
}

fn enterprise_runtime_badges() -> Vec<String> {
    // Config files don't change mid-session often enough to re-stat every frame.
    static CACHE: OnceLock<Vec<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            enterprise_badges_from_sources(
                truthy_env("MAESTRO_ENTERPRISE_MODE")
                    || has_env_value("MAESTRO_ENTERPRISE_ORG_ID")
                    || has_env_value("MAESTRO_ENTERPRISE_TOKEN"),
                rust_policy_config_exists(),
                rust_enterprise_mcp_config_exists(),
            )
        })
        .clone()
}

fn truthy_env(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn has_env_value(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn rust_policy_config_exists() -> bool {
    env_path_exists("MAESTRO_ENTERPRISE_POLICY_PATH")
        || env_path_exists("MAESTRO_POLICY_PATH")
        || maestro_home_dir()
            .map(|home| home.join("policy.json").is_file())
            .unwrap_or(false)
        || legacy_composer_home_dir()
            .map(|home| home.join("policy.json").is_file())
            .unwrap_or(false)
}

fn rust_enterprise_mcp_config_exists() -> bool {
    env_path_exists("MAESTRO_ENTERPRISE_MCP_PATH")
        || maestro_home_dir()
            .map(|home| home.join("enterprise").join("mcp.json").is_file())
            .unwrap_or(false)
        || legacy_composer_home_dir()
            .map(|home| home.join("enterprise").join("mcp.json").is_file())
            .unwrap_or(false)
}

fn env_path_exists(name: &str) -> bool {
    env::var(name)
        .ok()
        .and_then(|value| resolve_env_path(&value))
        .map(|path| path.is_file())
        .unwrap_or(false)
}

fn is_docker_env() -> bool {
    if env::var("DOCKER_CONTAINER").ok().as_deref() == Some("1") {
        return true;
    }
    if Path::new("/.dockerenv").exists() {
        return true;
    }
    if let Ok(contents) = fs::read_to_string("/proc/1/cgroup") {
        if contents.contains("docker") || contents.contains("kubepods") {
            return true;
        }
    }
    false
}

fn is_podman_env() -> bool {
    if env::var("CONTAINER_RUNTIME")
        .ok()
        .map(|value| value.to_lowercase())
        .as_deref()
        == Some("podman")
    {
        return true;
    }
    if env::var("CONTAINER")
        .ok()
        .map(|value| value.to_lowercase())
        .as_deref()
        == Some("podman")
    {
        return true;
    }
    if Path::new("/.containerenv").exists() {
        return true;
    }
    if let Ok(contents) = fs::read_to_string("/proc/1/cgroup") {
        if contents.contains("libpod") {
            return true;
        }
    }
    false
}

fn is_flatpak_env() -> bool {
    if env::var("FLATPAK_ID").is_ok() || env::var("FLATPAK_SANDBOX_DIR").is_ok() {
        return true;
    }
    Path::new("/.flatpak-info").exists()
}

fn is_bubblewrap_env() -> bool {
    env::var("BWRAP_ARGS")
        .ok()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn is_tmux_env() -> bool {
    env::var("TMUX").is_ok()
}

fn is_screen_env() -> bool {
    env::var("STY").is_ok()
}

fn is_jetbrains_terminal() -> bool {
    if let Ok(term) = env::var("TERMINAL_EMULATOR") {
        if term.to_lowercase().contains("jediterm") {
            return true;
        }
    }
    env::var("JEDITERM_LOG_DIR").is_ok()
}

fn is_musl_env() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    // Cheap checks first — never scan /lib* on every frame (caller must cache).
    if Path::new("/lib/ld-musl-x86_64.so.1").exists()
        || Path::new("/lib/ld-musl-aarch64.so.1").exists()
        || Path::new("/lib64/ld-musl-x86_64.so.1").exists()
    {
        return true;
    }
    // Fallback: look for a musl dynamic linker without full directory walks when possible.
    for path in [
        "/lib/ld-musl-x86_64.so.1",
        "/lib/ld-musl-aarch64.so.1",
        "/lib64/ld-musl-x86_64.so.1",
        "/usr/lib/ld-musl-x86_64.so.1",
        "/usr/lib/ld-musl-aarch64.so.1",
    ] {
        if Path::new(path).exists() {
            return true;
        }
    }
    // Last resort (once, via cached_env_badges): small scan of /lib only.
    if let Ok(entries) = fs::read_dir("/lib") {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with("ld-musl-") {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn restore_env_var(name: &str, previous: Option<String>) {
        if let Some(value) = previous {
            env::set_var(name, value);
        } else {
            env::remove_var(name);
        }
    }

    #[test]
    fn test_runtime_badges_show_mcp_failures_without_connected_servers() {
        let badges = build_runtime_badges(RuntimeBadgeParams {
            approval_mode: ApprovalMode::Selective,
            thinking_level: ThinkingLevel::Off,
            mcp_connected: 0,
            mcp_tool_count: 0,
            mcp_failed: 2,
            alert_count: 0,
            sandbox_policy: None,
            workspace_trusted: true,
            pending_approvals: 0,
        });

        assert!(badges.core.contains(&"mcp:0!2".to_string()));
    }

    #[test]
    fn test_runtime_badges_show_connected_and_failed_mcp_servers() {
        let badges = build_runtime_badges(RuntimeBadgeParams {
            approval_mode: ApprovalMode::Selective,
            thinking_level: ThinkingLevel::Off,
            mcp_connected: 1,
            mcp_tool_count: 3,
            mcp_failed: 2,
            alert_count: 0,
            sandbox_policy: Some("workspace-write".to_string()),
            workspace_trusted: false,
            pending_approvals: 2,
        });

        assert!(badges.core.contains(&"mcp:1(3)!2".to_string()));
        assert!(badges.core.contains(&"trust:no".to_string()));
        assert!(badges.core.contains(&"sandbox:workspace-write".to_string()));
        assert!(badges.core.contains(&"pending:2".to_string()));
    }

    #[test]
    fn test_runtime_badges_mark_scripted_replay_sessions() {
        let _lock = crate::config::test_process_env_lock();
        let previous_mode = env::var("MAESTRO_MODE").ok();
        let previous_scenario_path = env::var("MAESTRO_SCENARIO_PATH").ok();
        env::set_var("MAESTRO_MODE", "replay");
        env::remove_var("MAESTRO_SCENARIO_PATH");

        let badges = build_runtime_badges(RuntimeBadgeParams {
            approval_mode: ApprovalMode::Selective,
            thinking_level: ThinkingLevel::Off,
            mcp_connected: 0,
            mcp_tool_count: 0,
            mcp_failed: 0,
            alert_count: 0,
            sandbox_policy: None,
            workspace_trusted: true,
            pending_approvals: 0,
        });

        restore_env_var("MAESTRO_MODE", previous_mode);
        restore_env_var("MAESTRO_SCENARIO_PATH", previous_scenario_path);

        assert!(badges.core.contains(&"replay:on".to_string()));
    }

    #[test]
    fn test_enterprise_badges_from_sources_stay_compact() {
        let badges = enterprise_badges_from_sources(true, true, true);

        assert_eq!(
            badges,
            vec![
                "ent:on".to_string(),
                "ent:policy".to_string(),
                "ent:mcp".to_string()
            ]
        );
    }

    #[test]
    fn test_enterprise_badges_stay_hidden_without_sources() {
        let badges = enterprise_badges_from_sources(false, false, false);

        assert!(badges.is_empty());
    }

    #[test]
    fn test_enterprise_config_detection_checks_default_maestro_home() {
        let _lock = crate::config::test_process_env_lock();
        let temp = tempdir().expect("tempdir");
        let maestro_home = temp.path().join(".maestro");
        fs::create_dir_all(maestro_home.join("enterprise")).expect("create maestro dirs");
        fs::write(maestro_home.join("policy.json"), "{}").expect("write policy");
        fs::write(maestro_home.join("enterprise").join("mcp.json"), "{}").expect("write mcp");

        let previous_home = env::var("HOME").ok();
        let previous_maestro_home = env::var("MAESTRO_HOME").ok();
        let previous_enterprise_policy = env::var("MAESTRO_ENTERPRISE_POLICY_PATH").ok();
        let previous_policy = env::var("MAESTRO_POLICY_PATH").ok();
        let previous_enterprise_mcp = env::var("MAESTRO_ENTERPRISE_MCP_PATH").ok();

        env::set_var("HOME", temp.path());
        env::remove_var("MAESTRO_HOME");
        env::remove_var("MAESTRO_ENTERPRISE_POLICY_PATH");
        env::remove_var("MAESTRO_POLICY_PATH");
        env::remove_var("MAESTRO_ENTERPRISE_MCP_PATH");

        assert!(rust_policy_config_exists());
        assert!(rust_enterprise_mcp_config_exists());

        restore_env_var("HOME", previous_home);
        restore_env_var("MAESTRO_HOME", previous_maestro_home);
        restore_env_var("MAESTRO_ENTERPRISE_POLICY_PATH", previous_enterprise_policy);
        restore_env_var("MAESTRO_POLICY_PATH", previous_policy);
        restore_env_var("MAESTRO_ENTERPRISE_MCP_PATH", previous_enterprise_mcp);
    }

    #[test]
    fn test_enterprise_config_detection_checks_maestro_home_override() {
        let _lock = crate::config::test_process_env_lock();
        let temp = tempdir().expect("tempdir");
        let maestro_home = temp.path().join("custom-maestro");
        fs::create_dir_all(maestro_home.join("enterprise")).expect("create maestro dirs");
        fs::write(maestro_home.join("policy.json"), "{}").expect("write policy");
        fs::write(maestro_home.join("enterprise").join("mcp.json"), "{}").expect("write mcp");

        let previous_home = env::var("HOME").ok();
        let previous_maestro_home = env::var("MAESTRO_HOME").ok();

        env::set_var("HOME", temp.path());
        env::set_var("MAESTRO_HOME", "~/custom-maestro");

        assert!(rust_policy_config_exists());
        assert!(rust_enterprise_mcp_config_exists());

        restore_env_var("HOME", previous_home);
        restore_env_var("MAESTRO_HOME", previous_maestro_home);
    }

    #[test]
    fn test_enterprise_config_detection_checks_env_override_paths() {
        let _lock = crate::config::test_process_env_lock();
        let temp = tempdir().expect("tempdir");
        let policy_path = temp.path().join("custom-policy.json");
        let fallback_policy_path = temp.path().join("fallback-policy.json");
        let mcp_path = temp.path().join("custom-enterprise-mcp.json");
        fs::write(&policy_path, "{}").expect("write enterprise policy");
        fs::write(&fallback_policy_path, "{}").expect("write fallback policy");
        fs::write(&mcp_path, "{}").expect("write enterprise mcp");

        let previous_enterprise_policy = env::var("MAESTRO_ENTERPRISE_POLICY_PATH").ok();
        let previous_policy = env::var("MAESTRO_POLICY_PATH").ok();
        let previous_enterprise_mcp = env::var("MAESTRO_ENTERPRISE_MCP_PATH").ok();
        let previous_maestro_home = env::var("MAESTRO_HOME").ok();

        env::remove_var("MAESTRO_HOME");
        env::set_var("MAESTRO_ENTERPRISE_POLICY_PATH", &policy_path);
        env::set_var("MAESTRO_POLICY_PATH", &fallback_policy_path);
        env::set_var("MAESTRO_ENTERPRISE_MCP_PATH", &mcp_path);

        assert!(rust_policy_config_exists());
        assert!(rust_enterprise_mcp_config_exists());

        restore_env_var("MAESTRO_ENTERPRISE_POLICY_PATH", previous_enterprise_policy);
        restore_env_var("MAESTRO_POLICY_PATH", previous_policy);
        restore_env_var("MAESTRO_ENTERPRISE_MCP_PATH", previous_enterprise_mcp);
        restore_env_var("MAESTRO_HOME", previous_maestro_home);
    }
}
