# Configuration

Maestro loads configuration from environment variables, user files under `~/.maestro/`, and project files under `.maestro/`. Some native TUI loaders still accept **legacy** `~/.composer/` / `.composer/` paths for compatibility.

---

## Primary paths (`~/.maestro`)

| Path | Purpose |
|------|---------|
| `~/.maestro/keys.json` | Provider API keys |
| `~/.maestro/config.json` | Maestro config / model overrides |
| `~/.maestro/models.json` | Legacy models registry |
| `~/.maestro/mcp.json` | Global MCP servers |
| `~/.maestro/hooks.json` | Global hooks (JSON form) |
| `~/.maestro/hooks.toml` | Global hooks (TOML form; see CLI hints) |
| `~/.maestro/keybindings.json` | Native TUI keybindings (`/hotkeys`) |
| `~/.maestro/agent/AGENT.md` | Global agent instructions |
| `~/.maestro/agent/sessions/` | Session JSONL storage (see [Sessions](11-sessions.md)) |
| `~/.maestro/logs/` | Background task logs |
| `~/.maestro/bash-allow.json` | Bash allowlist patterns |

Project-scoped:

| Path | Purpose |
|------|---------|
| `.maestro/mcp.json` | Project MCP servers |
| `.maestro/hooks.json` / `.maestro/hooks.toml` | Project hooks |
| `.maestro/skills/` | Project skills |
| `.maestro/prompts/` / `.maestro/commands/` | Prompt / command templates |
| `.maestro/APPEND_SYSTEM.md` | Extra system prompt for the project |
| `.maestro/sandbox.json` | Sandbox mode configuration |
| `.maestro/bash-allow.json` | Project bash allowlist |
| `.maestro/worktrees/` | Git worktrees created by `--worktree` |
| `AGENT.md` / `AGENTS.md` / `CLAUDE.md` | Project agent instructions |

Override home with `MAESTRO_HOME` where supported.

---

## Legacy composer paths

Native loaders may still read:

- `~/.composer/config.toml` and `.composer/config.toml`
- `~/.composer/mcp.json`, `.composer/mcp.json`, `.composer/mcp.local.json`
- `~/.composer/hooks.toml`, `.composer/hooks.toml`
- `~/.composer/themes/*.json`, `.composer/themes/*.json`
- `~/.composer/skills/`, `.composer/skills/`
- `~/.composer/prompts/`

Prefer `~/.maestro` for new setup; keep legacy paths only if you already depend on them.

---

## Environment variables (common)

| Variable | Effect |
|----------|--------|
| `MAESTRO_MODEL` | Default model |
| `MAESTRO_APPROVAL_MODE` | `prompt` / `auto` / `fail` (CLI surface) |
| `MAESTRO_SAFE_MODE=1` | Safer defaults / extra prompts |
| `MAESTRO_SANDBOX_MODE` | `none` / `local` / `native` / `docker` |
| `MAESTRO_BASH_GUARD` | `0`/`1` YOLO vs full bash guard |
| `MAESTRO_NO_EGRESS_SHELL=1` | Require approval for network shell |
| `MAESTRO_REDUCED_MOTION=1` | Reduce UI motion |
| `MAESTRO_DISABLE_ANIMATIONS=1` | Disable TUI animations |
| `MAESTRO_UNCURSES_INPUT=0` | Disable protocol-aware terminal input and use the crossterm fallback |
| `MAESTRO_TUI_BIN` | Test/development override for the internal Cargo binary (`maestro-tui`) |
| `MAESTRO_KEYBINDINGS_FILE` | Override keybindings path |
| `MAESTRO_MODELS_FILE` | Custom models registry |
| `MAESTRO_PROFILE=prod` | Hardened profile (hosted/shared) |
| `MAESTRO_TUI_TOOL_MAX_CHARS` / `MAESTRO_TUI_TOOL_MAX_LINES` | Tool output limits |
| `MAESTRO_LSP_MAX_DIAGNOSTICS` | LSP diagnostics cap |

Inspect live limits with `/limits`.

---

## Native `config.toml` (legacy-compatible)

Some native settings still load via TOML profiles (Codex-style), historically under `.composer/config.toml`:

```toml
model = "gpt-5.1-codex-max"
model_provider = "openai"

[profiles.fast]
model = "claude-3-haiku"
```

Precedence for that loader (high → low): CLI flags → env → active profile → project TOML → global TOML → defaults.

---

## Agent instructions layering

Most specific wins:

1. Global: `~/.maestro/agent/AGENT.md`
2. Parent directories walking up the tree
3. Project: `AGENT.md`, `AGENTS.md`, or `CLAUDE.md`
4. Project append: `.maestro/APPEND_SYSTEM.md`

---

## See also

- [MCP Servers](07-mcp-servers.md)
- [Hooks](09-hooks.md)
- [Theming](06-theming.md)
- [Models](../../../../docs/MODELS.md)
