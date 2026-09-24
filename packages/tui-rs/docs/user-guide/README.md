# Deixic Code TUI User Guide

Learn how to install, authenticate, configure, and extend the native Deixic Code terminal UI (`deixic-code` / `packages/tui-rs`).

Deixic Code uses this native binary. The `maestro` executable, `MAESTRO_*`
environment variables, and `.maestro` paths remain compatibility coordinates.
For architecture and contributor notes, see [TUI Architecture](../../../../docs/TUI_ARCHITECTURE.md) and the [package README](../../README.md).

---

## Guides

| # | Document | Description |
|---|----------|-------------|
| 1 | [Getting Started](01-getting-started.md) | Install, first launch, basic interaction |
| 2 | [Authentication](02-authentication.md) | Codex login, API keys, providers |
| 3 | [Keyboard Shortcuts](03-keyboard-shortcuts.md) | Default keybindings and customization |
| 4 | [Slash Commands](04-slash-commands.md) | Built-in `/` commands from the registry |
| 5 | [Configuration](05-configuration.md) | `~/.maestro` paths, env vars, config files |
| 6 | [Theming](06-theming.md) | Themes and `/theme` |
| 7 | [MCP Servers](07-mcp-servers.md) | Model Context Protocol setup |
| 8 | [Skills](08-skills.md) | SKILL.md packages and `/skills` |
| 9 | [Hooks](09-hooks.md) | Lifecycle hooks and `/hooks` |
| 11 | [Sessions](11-sessions.md) | Save, resume, fork, rewind, export |
| 12 | [Sandbox and Safety](12-sandbox-and-safety.md) | Approvals, firewall, sandbox modes |
| 13 | [Headless Mode](13-headless-mode.md) | Print mode, JSON/RPC, embedding |
| 14 | [Worktrees](14-worktrees.md) | Git worktree isolation with `--worktree` |

---

## Related docs

- [Feature Guide](../../../../docs/FEATURES.md)
- [Quickstart](../../../../docs/QUICKSTART.md)
- [Safety](../../../../docs/SAFETY.md)
- [MCP Guide](../../../../docs/MCP_GUIDE.md)
- [Sessions](../../../../docs/SESSIONS.md)
- [Skill Cookbook](../../../../docs/cookbook/skills/README.md)
- [Headless protocol](../../../../docs/protocols/headless.md)
