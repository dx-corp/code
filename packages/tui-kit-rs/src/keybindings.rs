use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "test-support"))]
use std::sync::OnceLock;

use crossterm::event::KeyCode;
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(any(test, feature = "test-support"))]
use tokio::sync::Mutex;

use crate::key_hints::{KeyBinding, alt, ctrl, shift};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RustTuiKeybindingAction {
    CommandPalette,
    FileSearch,
    ToggleToolOutputs,
    EditLastQueuedFollowUp,
    CycleModel,
    CycleModelBackward,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RustTuiKeybindingShortcut {
    CtrlK,
    CtrlP,
    CtrlO,
    CtrlT,
    AltUp,
    ShiftLeft,
    AltP,
    CtrlShiftP,
    AltShiftP,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustTuiKeybindingLabels {
    pub cycle_model: String,
    pub cycle_model_backward: String,
    pub command_palette: String,
    pub file_search: String,
    pub toggle_tool_outputs: String,
    pub edit_last_queued_follow_up: String,
}

impl Default for RustTuiKeybindingLabels {
    fn default() -> Self {
        Self {
            cycle_model: ctrl(KeyCode::Char('p')).display(),
            cycle_model_backward: alt(KeyCode::Char('p')).display(),
            command_palette: ctrl(KeyCode::Char('k')).display(),
            file_search: ctrl(KeyCode::Char('o')).display(),
            toggle_tool_outputs: ctrl(KeyCode::Char('t')).display(),
            edit_last_queued_follow_up: alt(KeyCode::Up).display(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RustTuiKeybindings {
    pub cycle_model: KeyBinding,
    pub cycle_model_backward: KeyBinding,
    pub command_palette: KeyBinding,
    pub file_search: KeyBinding,
    pub toggle_tool_outputs: KeyBinding,
    pub edit_last_queued_follow_up: KeyBinding,
}

impl RustTuiKeybindings {
    #[must_use]
    pub fn labels(&self) -> RustTuiKeybindingLabels {
        RustTuiKeybindingLabels {
            cycle_model: self.cycle_model.display(),
            cycle_model_backward: self.cycle_model_backward.display(),
            command_palette: self.command_palette.display(),
            file_search: self.file_search.display(),
            toggle_tool_outputs: self.toggle_tool_outputs.display(),
            edit_last_queued_follow_up: self.edit_last_queued_follow_up.display(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct StoredRustTuiKeybindingsFile {
    version: Option<u8>,
    #[serde(default, rename = "rustBindings")]
    rust_bindings: HashMap<String, String>,
}

const RUST_TUI_KEYBINDING_ACTIONS: [RustTuiKeybindingAction; 6] = [
    RustTuiKeybindingAction::CommandPalette,
    RustTuiKeybindingAction::FileSearch,
    RustTuiKeybindingAction::ToggleToolOutputs,
    RustTuiKeybindingAction::EditLastQueuedFollowUp,
    RustTuiKeybindingAction::CycleModel,
    RustTuiKeybindingAction::CycleModelBackward,
];

const TUI_KEYBINDING_ACTIONS: [&str; 7] = [
    "cycle-model",
    "toggle-tool-outputs",
    "toggle-thinking-blocks",
    "external-editor",
    "suspend",
    "command-palette",
    "edit-last-follow-up",
];

const TUI_KEYBINDING_SHORTCUTS: [&str; 8] = [
    "ctrl+g",
    "ctrl+k",
    "ctrl+o",
    "ctrl+p",
    "ctrl+t",
    "ctrl+z",
    "alt+up",
    "shift+left",
];

#[derive(Debug, Clone)]
struct KeybindingConfigIssue {
    severity: &'static str,
    message: String,
}

#[derive(Debug, Clone)]
struct KeybindingConfigReport {
    path: PathBuf,
    exists: bool,
    tui_requested_overrides: usize,
    tui_active_overrides: usize,
    rust_requested_overrides: usize,
    rust_active_overrides: usize,
    issues: Vec<KeybindingConfigIssue>,
}

#[derive(Debug, Clone)]
pub struct InitializeKeybindingsResult {
    pub path: PathBuf,
    pub created: bool,
}

#[must_use]
pub fn queued_follow_up_edit_binding_for_terminal_name(
    terminal_name: &str,
    in_tmux: bool,
) -> KeyBinding {
    if in_tmux || terminal_name.eq_ignore_ascii_case("tmux") {
        return shift(KeyCode::Left);
    }

    match terminal_name.to_ascii_lowercase().as_str() {
        "apple_terminal" | "warp" | "warpterminal" | "vscode" => shift(KeyCode::Left),
        _ => alt(KeyCode::Up),
    }
}

#[must_use]
pub fn load_rust_tui_keybindings(terminal_name: &str, in_tmux: bool) -> RustTuiKeybindings {
    let config_path = Some(keybindings_config_path());
    load_rust_tui_keybindings_from_path(config_path.as_deref(), terminal_name, in_tmux)
}

#[must_use]
pub fn keybindings_config_path() -> PathBuf {
    std::env::var_os("MAESTRO_KEYBINDINGS_FILE")
        .map(PathBuf::from)
        .or_else(|| crate::path_utils::maestro_home_dir().map(|home| home.join("keybindings.json")))
        .unwrap_or_else(|| PathBuf::from("keybindings.json"))
}

pub fn initialize_keybindings_file(force: bool) -> Result<InitializeKeybindingsResult, String> {
    let path = keybindings_config_path();
    let created = initialize_keybindings_file_at_path(&path, force)?;
    Ok(InitializeKeybindingsResult { path, created })
}

#[must_use]
pub fn format_keybindings_config_report() -> String {
    format_keybinding_config_report(&inspect_keybindings_config_at_path(
        &keybindings_config_path(),
    ))
}

#[must_use]
pub fn summarize_keybindings_config_issues() -> Option<String> {
    summarize_keybindings_config_issues_at_path(&keybindings_config_path())
}

#[must_use]
pub fn is_keybindings_config_path(path: &Path) -> bool {
    let expected = keybindings_config_path();
    let canonical_expected = dunce::canonicalize(&expected).unwrap_or(expected.clone());
    let canonical_path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical_path == canonical_expected
        || path.file_name() == expected.file_name()
        || path.file_name() == canonical_expected.file_name()
}

#[cfg(any(test, feature = "test-support"))]
pub fn keybindings_test_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn load_rust_tui_keybindings_from_path(
    config_path: Option<&Path>,
    terminal_name: &str,
    in_tmux: bool,
) -> RustTuiKeybindings {
    let overrides = config_path
        .map(read_rust_tui_keybinding_overrides)
        .unwrap_or_default();
    let (resolved, _) = resolve_rust_tui_shortcuts(&overrides, terminal_name, in_tmux);
    shortcuts_to_bindings(&resolved)
}

fn resolve_rust_tui_shortcuts(
    overrides: &HashMap<RustTuiKeybindingAction, RustTuiKeybindingShortcut>,
    terminal_name: &str,
    in_tmux: bool,
) -> (
    HashMap<RustTuiKeybindingAction, RustTuiKeybindingShortcut>,
    Vec<KeybindingConfigIssue>,
) {
    let mut defaults = default_shortcuts(terminal_name, in_tmux);
    let mut resolved = defaults.clone();
    for (action, shortcut) in overrides {
        resolved.insert(*action, *shortcut);
    }

    // Preserve existing custom shortcuts when introducing model cycling.
    // An unconfigured new action takes a free alternative instead of evicting
    // an established file-search or palette binding.
    for action in [
        RustTuiKeybindingAction::CycleModel,
        RustTuiKeybindingAction::CycleModelBackward,
    ] {
        if !overrides.contains_key(&action)
            && overrides
                .values()
                .any(|shortcut| *shortcut == resolved[&action])
        {
            for candidate in [
                RustTuiKeybindingShortcut::CtrlShiftP,
                RustTuiKeybindingShortcut::AltShiftP,
                RustTuiKeybindingShortcut::AltP,
            ] {
                if !resolved.values().any(|shortcut| *shortcut == candidate) {
                    resolved.insert(action, candidate);
                    defaults.insert(action, candidate);
                    break;
                }
            }
        }
    }
    let overridden_actions: HashSet<RustTuiKeybindingAction> = overrides.keys().copied().collect();
    let mut issues = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        let mut actions_by_shortcut: HashMap<
            RustTuiKeybindingShortcut,
            Vec<RustTuiKeybindingAction>,
        > = HashMap::new();
        for action in RUST_TUI_KEYBINDING_ACTIONS {
            let shortcut = resolved[&action];
            actions_by_shortcut
                .entry(shortcut)
                .or_default()
                .push(action);
        }

        for actions in actions_by_shortcut.values() {
            if actions.len() < 2 {
                continue;
            }
            for action in actions {
                if overridden_actions.contains(action) && resolved[action] != defaults[action] {
                    let conflicts = actions
                        .iter()
                        .filter(|candidate| *candidate != action)
                        .map(|candidate| action_name(*candidate))
                        .collect::<Vec<_>>()
                        .join(", ");
                    issues.push(KeybindingConfigIssue {
                        severity: "warning",
                        message: format!(
                            "Rust TUI override \"{}: {}\" conflicts with {conflicts} and falls back to {}.",
                            action_name(*action),
                            shortcut_name(resolved[action]),
                            shortcut_name(defaults[action]),
                        ),
                    });
                    resolved.insert(*action, defaults[action]);
                    changed = true;
                }
            }
        }
    }

    (resolved, issues)
}

fn tui_edit_last_follow_up_shortcut_from_env() -> &'static str {
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if std::env::var_os("TMUX").is_some()
        || matches!(
            term_program.as_str(),
            "tmux" | "apple_terminal" | "warp" | "warpterminal" | "vscode"
        )
    {
        "shift+left"
    } else {
        "alt+up"
    }
}

fn default_tui_shortcuts() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("cycle-model", "ctrl+p"),
        ("toggle-tool-outputs", "ctrl+o"),
        ("toggle-thinking-blocks", "ctrl+t"),
        ("external-editor", "ctrl+g"),
        ("suspend", "ctrl+z"),
        ("command-palette", "ctrl+k"),
        (
            "edit-last-follow-up",
            tui_edit_last_follow_up_shortcut_from_env(),
        ),
    ])
}

fn default_rust_shortcuts() -> HashMap<&'static str, &'static str> {
    let queued_follow_up = shortcut_name(shortcut_for_binding(
        queued_follow_up_edit_binding_for_terminal_name(
            &std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "wezterm".to_string()),
            std::env::var_os("TMUX").is_some(),
        ),
    ));

    HashMap::from([
        ("cycle-model", "ctrl+p"),
        ("cycle-model-backward", "alt+p"),
        ("command-palette", "ctrl+k"),
        ("file-search", "ctrl+o"),
        ("toggle-tool-outputs", "ctrl+t"),
        ("edit-last-follow-up", queued_follow_up),
    ])
}

fn normalize_name(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn normalize_shortcut_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn parse_string_overrides(
    value: Option<&Value>,
    section_name: &str,
    supported_actions: &[&str],
    supported_shortcuts: &[&str],
    issues: &mut Vec<KeybindingConfigIssue>,
) -> HashMap<String, String> {
    let Some(value) = value else {
        return HashMap::new();
    };
    let Some(object) = value.as_object() else {
        issues.push(KeybindingConfigIssue {
            severity: "error",
            message: format!(
                "\"{section_name}\" must be an object of action-to-shortcut overrides."
            ),
        });
        return HashMap::new();
    };

    let action_set: HashSet<&str> = supported_actions.iter().copied().collect();
    let shortcut_set: HashSet<&str> = supported_shortcuts.iter().copied().collect();
    let mut overrides = HashMap::new();
    for (raw_action, raw_shortcut) in object {
        let action = normalize_name(raw_action);
        if !action_set.contains(action.as_str()) {
            issues.push(KeybindingConfigIssue {
                severity: "error",
                message: format!(
                    "Unknown {section_name} keybinding action \"{raw_action}\". Supported actions: {}.",
                    supported_actions.join(", ")
                ),
            });
            continue;
        }
        let Some(raw_shortcut) = raw_shortcut.as_str() else {
            issues.push(KeybindingConfigIssue {
                severity: "error",
                message: format!(
                    "{section_name} action \"{action}\" must map to a shortcut string."
                ),
            });
            continue;
        };
        let shortcut = normalize_shortcut_name(raw_shortcut);
        if !shortcut_set.contains(shortcut.as_str()) {
            issues.push(KeybindingConfigIssue {
                severity: "error",
                message: format!(
                    "Unsupported {section_name} shortcut \"{raw_shortcut}\" for \"{action}\". Supported shortcuts: {}.",
                    supported_shortcuts.join(", ")
                ),
            });
            continue;
        }
        overrides.insert(action, shortcut);
    }
    overrides
}

fn collect_conflict_issues(
    label: &str,
    overrides: &HashMap<String, String>,
    defaults: &HashMap<&'static str, &'static str>,
) -> (Vec<KeybindingConfigIssue>, HashMap<String, String>) {
    let mut resolved = defaults
        .iter()
        .map(|(action, shortcut)| ((*action).to_string(), (*shortcut).to_string()))
        .collect::<HashMap<_, _>>();
    for (action, shortcut) in overrides {
        resolved.insert(action.clone(), shortcut.clone());
    }

    let mut issues = Vec::new();
    let mut seen = HashSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        let mut actions_by_shortcut: HashMap<String, Vec<String>> = HashMap::new();
        for (action, shortcut) in &resolved {
            actions_by_shortcut
                .entry(shortcut.clone())
                .or_default()
                .push(action.clone());
        }

        for (shortcut, actions) in actions_by_shortcut {
            if actions.len() < 2 {
                continue;
            }
            for action in actions {
                let Some(override_shortcut) = overrides.get(&action) else {
                    continue;
                };
                let default_shortcut = defaults
                    .get(action.as_str())
                    .expect("default shortcut should exist for known action");
                if override_shortcut == default_shortcut {
                    continue;
                }
                let key = format!("{action}:{shortcut}");
                if seen.insert(key) {
                    let conflicts = resolved
                        .iter()
                        .filter_map(|(candidate, candidate_shortcut)| {
                            (candidate_shortcut == &shortcut && candidate != &action)
                                .then_some(candidate.clone())
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    issues.push(KeybindingConfigIssue {
                        severity: "warning",
                        message: format!(
                            "{label} override \"{action}: {shortcut}\" conflicts with {conflicts} and falls back to {default_shortcut}."
                        ),
                    });
                }
                resolved.insert(action.clone(), (*default_shortcut).to_string());
                changed = true;
            }
        }
    }

    (issues, resolved)
}

fn inspect_keybindings_config_at_path(path: &Path) -> KeybindingConfigReport {
    if !path.exists() {
        return KeybindingConfigReport {
            path: path.to_path_buf(),
            exists: false,
            tui_requested_overrides: 0,
            tui_active_overrides: 0,
            rust_requested_overrides: 0,
            rust_active_overrides: 0,
            issues: Vec::new(),
        };
    }

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => {
            return KeybindingConfigReport {
                path: path.to_path_buf(),
                exists: true,
                tui_requested_overrides: 0,
                tui_active_overrides: 0,
                rust_requested_overrides: 0,
                rust_active_overrides: 0,
                issues: vec![KeybindingConfigIssue {
                    severity: "error",
                    message: "Failed to read keybindings.json.".to_string(),
                }],
            };
        }
    };

    let parsed = match serde_json::from_str::<Value>(&content) {
        Ok(parsed) => parsed,
        Err(err) => {
            return KeybindingConfigReport {
                path: path.to_path_buf(),
                exists: true,
                tui_requested_overrides: 0,
                tui_active_overrides: 0,
                rust_requested_overrides: 0,
                rust_active_overrides: 0,
                issues: vec![KeybindingConfigIssue {
                    severity: "error",
                    message: err.to_string(),
                }],
            };
        }
    };

    let Some(root) = parsed.as_object() else {
        return KeybindingConfigReport {
            path: path.to_path_buf(),
            exists: true,
            tui_requested_overrides: 0,
            tui_active_overrides: 0,
            rust_requested_overrides: 0,
            rust_active_overrides: 0,
            issues: vec![KeybindingConfigIssue {
                severity: "error",
                message: "keybindings.json must contain a JSON object.".to_string(),
            }],
        };
    };

    let mut issues = Vec::new();
    if root.get("version").and_then(Value::as_u64) != Some(1) {
        issues.push(KeybindingConfigIssue {
            severity: "error",
            message: "keybindings.json must include \"version\": 1.".to_string(),
        });
    }

    let tui_overrides = parse_string_overrides(
        root.get("bindings"),
        "TUI",
        &TUI_KEYBINDING_ACTIONS,
        &TUI_KEYBINDING_SHORTCUTS,
        &mut issues,
    );
    let rust_overrides = parse_string_overrides(
        root.get("rustBindings"),
        "Rust TUI",
        &[
            "command-palette",
            "file-search",
            "toggle-tool-outputs",
            "edit-last-follow-up",
            "cycle-model",
            "cycle-model-backward",
        ],
        &[
            "ctrl+k",
            "ctrl+p",
            "ctrl+o",
            "ctrl+t",
            "alt+up",
            "shift+left",
            "alt+p",
            "ctrl+shift+p",
            "alt+shift+p",
        ],
        &mut issues,
    );

    let tui_defaults = default_tui_shortcuts();
    let typed_rust_overrides = rust_overrides
        .iter()
        .filter_map(|(action, shortcut)| {
            Some((parse_action_name(action)?, parse_shortcut_name(shortcut)?))
        })
        .collect();
    let (tui_conflicts, resolved_tui) =
        collect_conflict_issues("TUI", &tui_overrides, &tui_defaults);
    let (resolved_rust, rust_conflicts) = resolve_rust_tui_shortcuts(
        &typed_rust_overrides,
        &std::env::var("TERM_PROGRAM").unwrap_or_default(),
        std::env::var_os("TMUX").is_some(),
    );
    issues.extend(tui_conflicts);
    issues.extend(rust_conflicts);

    let tui_active_overrides = tui_overrides
        .iter()
        .filter(|(action, shortcut)| resolved_tui.get(*action) == Some(*shortcut))
        .count();
    let rust_active_overrides = typed_rust_overrides
        .iter()
        .filter(|(action, shortcut)| resolved_rust.get(action) == Some(shortcut))
        .count();

    KeybindingConfigReport {
        path: path.to_path_buf(),
        exists: true,
        tui_requested_overrides: tui_overrides.len(),
        tui_active_overrides,
        rust_requested_overrides: rust_overrides.len(),
        rust_active_overrides,
        issues,
    }
}

fn generate_keybindings_template() -> String {
    let bindings = json!({
        "cycle-model": "ctrl+p",
        "toggle-tool-outputs": "ctrl+o",
        "toggle-thinking-blocks": "ctrl+t",
        "external-editor": "ctrl+g",
        "suspend": "ctrl+z",
        "command-palette": "ctrl+k",
        "edit-last-follow-up": tui_edit_last_follow_up_shortcut_from_env(),
    });
    let rust_bindings = json!({
        "command-palette": "ctrl+k",
        "file-search": "ctrl+o",
        "toggle-tool-outputs": "ctrl+t",
        "edit-last-follow-up": default_rust_shortcuts()["edit-last-follow-up"],
        "cycle-model": "ctrl+p",
        "cycle-model-backward": "alt+p",
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&json!({
            "$docs": "https://github.com/evalops/maestro",
            "$comment": "Delete any entries you do not want to override, then run /hotkeys validate inside Deixic Code.",
            "version": 1,
            "bindings": bindings,
            "rustBindings": rust_bindings,
        }))
        .expect("keybindings template should serialize")
    )
}

fn initialize_keybindings_file_at_path(path: &Path, force: bool) -> Result<bool, String> {
    if path.exists() && !force {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(path, generate_keybindings_template()).map_err(|err| err.to_string())?;
    Ok(true)
}

fn format_keybinding_config_report(report: &KeybindingConfigReport) -> String {
    let mut lines = vec!["Keyboard Shortcuts Config:".to_string()];
    lines.push(format!("  Path: {}", report.path.display()));
    lines.push(format!(
        "  Status: {}",
        if report.exists { "present" } else { "missing" }
    ));
    if !report.exists {
        lines.push("  Hint: run /hotkeys init to create a starter file.".to_string());
        return lines.join("\n");
    }

    lines.push(format!(
        "  TUI overrides: {}/{} active",
        report.tui_active_overrides, report.tui_requested_overrides
    ));
    lines.push(format!(
        "  Rust TUI overrides: {}/{} active",
        report.rust_active_overrides, report.rust_requested_overrides
    ));
    if report.issues.is_empty() {
        lines.push("  Validation: OK".to_string());
        return lines.join("\n");
    }

    lines.push(format!("  Issues: {}", report.issues.len()));
    for issue in &report.issues {
        lines.push(format!(
            "  - {}: {}",
            issue.severity.to_ascii_uppercase(),
            issue.message
        ));
    }
    lines.join("\n")
}

fn summarize_keybindings_config_issues_at_path(path: &Path) -> Option<String> {
    let report = inspect_keybindings_config_at_path(path);
    if !report.exists || report.issues.is_empty() {
        return None;
    }

    Some(format!(
        "Keyboard shortcuts config has {} issue{}. Run /hotkeys validate.",
        report.issues.len(),
        if report.issues.len() == 1 { "" } else { "s" }
    ))
}

fn default_shortcuts(
    terminal_name: &str,
    in_tmux: bool,
) -> HashMap<RustTuiKeybindingAction, RustTuiKeybindingShortcut> {
    HashMap::from([
        (
            RustTuiKeybindingAction::CycleModel,
            RustTuiKeybindingShortcut::CtrlP,
        ),
        (
            RustTuiKeybindingAction::CycleModelBackward,
            RustTuiKeybindingShortcut::AltP,
        ),
        (
            RustTuiKeybindingAction::CommandPalette,
            RustTuiKeybindingShortcut::CtrlK,
        ),
        (
            RustTuiKeybindingAction::FileSearch,
            RustTuiKeybindingShortcut::CtrlO,
        ),
        (
            RustTuiKeybindingAction::ToggleToolOutputs,
            RustTuiKeybindingShortcut::CtrlT,
        ),
        (
            RustTuiKeybindingAction::EditLastQueuedFollowUp,
            shortcut_for_binding(queued_follow_up_edit_binding_for_terminal_name(
                terminal_name,
                in_tmux,
            )),
        ),
    ])
}

fn shortcuts_to_bindings(
    shortcuts: &HashMap<RustTuiKeybindingAction, RustTuiKeybindingShortcut>,
) -> RustTuiKeybindings {
    RustTuiKeybindings {
        cycle_model: binding_for_shortcut(shortcuts[&RustTuiKeybindingAction::CycleModel]),
        cycle_model_backward: binding_for_shortcut(
            shortcuts[&RustTuiKeybindingAction::CycleModelBackward],
        ),
        command_palette: binding_for_shortcut(shortcuts[&RustTuiKeybindingAction::CommandPalette]),
        file_search: binding_for_shortcut(shortcuts[&RustTuiKeybindingAction::FileSearch]),
        toggle_tool_outputs: binding_for_shortcut(
            shortcuts[&RustTuiKeybindingAction::ToggleToolOutputs],
        ),
        edit_last_queued_follow_up: binding_for_shortcut(
            shortcuts[&RustTuiKeybindingAction::EditLastQueuedFollowUp],
        ),
    }
}

fn read_rust_tui_keybinding_overrides(
    path: &Path,
) -> HashMap<RustTuiKeybindingAction, RustTuiKeybindingShortcut> {
    let Ok(content) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(parsed) = serde_json::from_str::<StoredRustTuiKeybindingsFile>(&content) else {
        return HashMap::new();
    };
    if parsed.version != Some(1) {
        return HashMap::new();
    }

    let mut overrides = HashMap::new();
    for (action, shortcut) in parsed.rust_bindings {
        let Some(action) = parse_action_name(&action) else {
            continue;
        };
        let Some(shortcut) = parse_shortcut_name(&shortcut) else {
            continue;
        };
        overrides.insert(action, shortcut);
    }
    overrides
}

fn action_name(value: RustTuiKeybindingAction) -> &'static str {
    match value {
        RustTuiKeybindingAction::CommandPalette => "command-palette",
        RustTuiKeybindingAction::FileSearch => "file-search",
        RustTuiKeybindingAction::ToggleToolOutputs => "toggle-tool-outputs",
        RustTuiKeybindingAction::EditLastQueuedFollowUp => "edit-last-follow-up",
        RustTuiKeybindingAction::CycleModel => "cycle-model",
        RustTuiKeybindingAction::CycleModelBackward => "cycle-model-backward",
    }
}

fn shortcut_name(value: RustTuiKeybindingShortcut) -> &'static str {
    match value {
        RustTuiKeybindingShortcut::CtrlK => "ctrl+k",
        RustTuiKeybindingShortcut::CtrlP => "ctrl+p",
        RustTuiKeybindingShortcut::CtrlO => "ctrl+o",
        RustTuiKeybindingShortcut::CtrlT => "ctrl+t",
        RustTuiKeybindingShortcut::AltUp => "alt+up",
        RustTuiKeybindingShortcut::ShiftLeft => "shift+left",
        RustTuiKeybindingShortcut::AltP => "alt+p",
        RustTuiKeybindingShortcut::CtrlShiftP => "ctrl+shift+p",
        RustTuiKeybindingShortcut::AltShiftP => "alt+shift+p",
    }
}

fn parse_action_name(value: &str) -> Option<RustTuiKeybindingAction> {
    match value {
        "cycle-model" => Some(RustTuiKeybindingAction::CycleModel),
        "cycle-model-backward" => Some(RustTuiKeybindingAction::CycleModelBackward),
        "command-palette" => Some(RustTuiKeybindingAction::CommandPalette),
        "file-search" => Some(RustTuiKeybindingAction::FileSearch),
        "toggle-tool-outputs" => Some(RustTuiKeybindingAction::ToggleToolOutputs),
        "edit-last-follow-up" => Some(RustTuiKeybindingAction::EditLastQueuedFollowUp),
        _ => None,
    }
}

fn parse_shortcut_name(value: &str) -> Option<RustTuiKeybindingShortcut> {
    let normalized = value
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    match normalized.as_str() {
        "alt+p" => Some(RustTuiKeybindingShortcut::AltP),
        "ctrl+shift+p" => Some(RustTuiKeybindingShortcut::CtrlShiftP),
        "alt+shift+p" => Some(RustTuiKeybindingShortcut::AltShiftP),
        "ctrl+k" => Some(RustTuiKeybindingShortcut::CtrlK),
        "ctrl+p" => Some(RustTuiKeybindingShortcut::CtrlP),
        "ctrl+o" => Some(RustTuiKeybindingShortcut::CtrlO),
        "ctrl+t" => Some(RustTuiKeybindingShortcut::CtrlT),
        "alt+up" => Some(RustTuiKeybindingShortcut::AltUp),
        "shift+left" => Some(RustTuiKeybindingShortcut::ShiftLeft),
        _ => None,
    }
}

fn binding_for_shortcut(shortcut: RustTuiKeybindingShortcut) -> KeyBinding {
    match shortcut {
        RustTuiKeybindingShortcut::AltP => alt(KeyCode::Char('p')),
        RustTuiKeybindingShortcut::CtrlShiftP => KeyBinding {
            key: KeyCode::Char('p'),
            modifiers: crossterm::event::KeyModifiers::CONTROL
                | crossterm::event::KeyModifiers::SHIFT,
        },
        RustTuiKeybindingShortcut::AltShiftP => KeyBinding {
            key: KeyCode::Char('p'),
            modifiers: crossterm::event::KeyModifiers::ALT | crossterm::event::KeyModifiers::SHIFT,
        },
        RustTuiKeybindingShortcut::CtrlK => ctrl(KeyCode::Char('k')),
        RustTuiKeybindingShortcut::CtrlP => ctrl(KeyCode::Char('p')),
        RustTuiKeybindingShortcut::CtrlO => ctrl(KeyCode::Char('o')),
        RustTuiKeybindingShortcut::CtrlT => ctrl(KeyCode::Char('t')),
        RustTuiKeybindingShortcut::AltUp => alt(KeyCode::Up),
        RustTuiKeybindingShortcut::ShiftLeft => shift(KeyCode::Left),
    }
}

fn shortcut_for_binding(binding: KeyBinding) -> RustTuiKeybindingShortcut {
    match (binding.key, binding.modifiers) {
        (KeyCode::Char('p'), crossterm::event::KeyModifiers::ALT) => {
            RustTuiKeybindingShortcut::AltP
        }
        (KeyCode::Char('p'), modifiers)
            if modifiers
                == (crossterm::event::KeyModifiers::CONTROL
                    | crossterm::event::KeyModifiers::SHIFT) =>
        {
            RustTuiKeybindingShortcut::CtrlShiftP
        }
        (KeyCode::Char('p'), modifiers)
            if modifiers
                == (crossterm::event::KeyModifiers::ALT
                    | crossterm::event::KeyModifiers::SHIFT) =>
        {
            RustTuiKeybindingShortcut::AltShiftP
        }
        (KeyCode::Char('k'), crossterm::event::KeyModifiers::CONTROL) => {
            RustTuiKeybindingShortcut::CtrlK
        }
        (KeyCode::Char('p'), crossterm::event::KeyModifiers::CONTROL) => {
            RustTuiKeybindingShortcut::CtrlP
        }
        (KeyCode::Char('o'), crossterm::event::KeyModifiers::CONTROL) => {
            RustTuiKeybindingShortcut::CtrlO
        }
        (KeyCode::Char('t'), crossterm::event::KeyModifiers::CONTROL) => {
            RustTuiKeybindingShortcut::CtrlT
        }
        (KeyCode::Up, crossterm::event::KeyModifiers::ALT) => RustTuiKeybindingShortcut::AltUp,
        (KeyCode::Left, crossterm::event::KeyModifiers::SHIFT) => {
            RustTuiKeybindingShortcut::ShiftLeft
        }
        _ => RustTuiKeybindingShortcut::AltUp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn model_cycle_bindings_preserve_existing_custom_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("keybindings.json");
        fs::write(
            &path,
            r#"{"version":1,"rustBindings":{"file-search":"ctrl+p"}}"#,
        )
        .unwrap();
        let bindings = load_rust_tui_keybindings_from_path(Some(&path), "wezterm", false);
        assert_eq!(bindings.file_search, ctrl(KeyCode::Char('p')));
        assert_ne!(bindings.cycle_model, bindings.file_search);
        assert!(inspect_keybindings_config_at_path(&path).issues.is_empty());
        assert_eq!(bindings.cycle_model_backward, alt(KeyCode::Char('p')));
        fs::write(&path, r#"{"version":1,"rustBindings":{"cycle-model":"alt+p","cycle-model-backward":"ctrl+p"}}"#).unwrap();
        let bindings = load_rust_tui_keybindings_from_path(Some(&path), "wezterm", false);
        assert_eq!(bindings.cycle_model, alt(KeyCode::Char('p')));
        assert_eq!(bindings.cycle_model_backward, ctrl(KeyCode::Char('p')));
        assert!(inspect_keybindings_config_at_path(&path).issues.is_empty());
    }

    #[test]
    fn validation_preserves_legacy_palette_override_like_runtime() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("keybindings.json");
        fs::write(&path, r#"{"version":1,"rustBindings":{"command-palette":"ctrl+p","file-search":"ctrl+o","toggle-tool-outputs":"ctrl+t","edit-last-follow-up":"alt+up"}}"#).unwrap();
        let bindings = load_rust_tui_keybindings_from_path(Some(&path), "ghostty", false);
        assert_eq!(bindings.command_palette, ctrl(KeyCode::Char('p')));
        assert_eq!(
            bindings.cycle_model,
            binding_for_shortcut(RustTuiKeybindingShortcut::CtrlShiftP)
        );
        let report = inspect_keybindings_config_at_path(&path);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(report.rust_active_overrides, 4);
        assert_eq!(summarize_keybindings_config_issues_at_path(&path), None);
    }

    #[test]
    fn validation_still_reports_explicit_model_cycle_conflicts() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("keybindings.json");
        fs::write(
            &path,
            r#"{"version":1,"rustBindings":{"command-palette":"ctrl+p","cycle-model":"ctrl+p"}}"#,
        )
        .unwrap();
        let bindings = load_rust_tui_keybindings_from_path(Some(&path), "ghostty", false);
        assert_eq!(bindings.command_palette, ctrl(KeyCode::Char('k')));
        assert_eq!(bindings.cycle_model, ctrl(KeyCode::Char('p')));
        let report = inspect_keybindings_config_at_path(&path);
        assert_eq!(report.rust_active_overrides, 1);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(
            report.issues[0].message,
            "Rust TUI override \"command-palette: ctrl+p\" conflicts with cycle-model and falls back to ctrl+k."
        );
    }

    #[test]
    fn uses_terminal_aware_default_queued_follow_up_binding() {
        assert_eq!(
            queued_follow_up_edit_binding_for_terminal_name("vscode", false),
            shift(KeyCode::Left)
        );
        assert_eq!(
            queued_follow_up_edit_binding_for_terminal_name("wezterm", false),
            alt(KeyCode::Up)
        );
    }

    #[test]
    fn loads_valid_rust_tui_keybinding_overrides() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("keybindings.json");
        fs::write(
            &path,
            r#"{
  "version": 1,
  "rustBindings": {
    "command-palette": "Ctrl+O",
    "file-search": "Ctrl+P",
    "toggle-tool-outputs": "Shift+Left",
    "edit-last-follow-up": "Ctrl+T"
  }
}"#,
        )
        .expect("write keybindings");

        let resolved = load_rust_tui_keybindings_from_path(Some(&path), "wezterm", false);

        assert_eq!(resolved.command_palette, ctrl(KeyCode::Char('o')));
        assert_eq!(resolved.file_search, ctrl(KeyCode::Char('p')));
        assert_eq!(resolved.toggle_tool_outputs, shift(KeyCode::Left));
        assert_eq!(
            resolved.edit_last_queued_follow_up,
            ctrl(KeyCode::Char('t'))
        );
    }

    #[test]
    fn resets_conflicting_partial_overrides_to_defaults() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("keybindings.json");
        fs::write(
            &path,
            r#"{
  "version": 1,
  "rustBindings": {
    "command-palette": "Ctrl+O"
  }
}"#,
        )
        .expect("write keybindings");

        let resolved = load_rust_tui_keybindings_from_path(Some(&path), "wezterm", false);

        assert_eq!(resolved.command_palette, ctrl(KeyCode::Char('k')));
        assert_eq!(resolved.file_search, ctrl(KeyCode::Char('o')));
    }

    #[test]
    fn summarizes_keybinding_config_issues() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("keybindings.json");
        fs::write(
            &path,
            r#"{
  "version": 1,
  "rustBindings": {
    "command-palette": "Ctrl+O",
    "file-search": "Ctrl+O"
  }
}"#,
        )
        .expect("write keybindings");

        assert_eq!(
            summarize_keybindings_config_issues_at_path(&path).as_deref(),
            Some("Keyboard shortcuts config has 1 issue. Run /hotkeys validate.")
        );
    }

    #[test]
    fn keybindings_path_follows_maestro_home() {
        let _env_guard = crate::config::test_process_env_lock();
        let _guard = keybindings_test_env_lock().blocking_lock();
        let temp = tempdir().expect("tempdir");
        let previous_file = std::env::var_os("MAESTRO_KEYBINDINGS_FILE");
        let previous_home = std::env::var_os("MAESTRO_HOME");
        std::env::remove_var("MAESTRO_KEYBINDINGS_FILE");
        std::env::set_var("MAESTRO_HOME", temp.path());
        let path = keybindings_config_path();
        match previous_file {
            Some(value) => std::env::set_var("MAESTRO_KEYBINDINGS_FILE", value),
            None => std::env::remove_var("MAESTRO_KEYBINDINGS_FILE"),
        }
        match previous_home {
            Some(value) => std::env::set_var("MAESTRO_HOME", value),
            None => std::env::remove_var("MAESTRO_HOME"),
        }
        assert_eq!(path, temp.path().join("keybindings.json"));
    }
}
