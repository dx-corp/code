# Getting Started

Deixic Code is a coding agent for real software work. The interactive surface is a native terminal UI (`deixic-code`) that can inspect code, edit files, run shell commands, and stream tool use with approvals and sandbox controls.

---

## Install

One-line install (macOS / Linux):

```bash
curl -fsSL https://raw.githubusercontent.com/evalops/maestro/main/scripts/install.sh | bash
```

Package managers (contributors and source workflows):

```bash
npm install -g @evalops/maestro
```

Verify:

```bash
deixic-code --version
```

The installer keeps prior verified releases under the compatibility-named Maestro data directory so a failed update does not replace the active launcher. Use `MAESTRO_REQUIRE_SIGNED_INSTALL=1` when your environment should reject older unsigned releases.

See the root [README Install section](../../../../README.md#install) for manual release binary downloads.

From a source checkout you must build the native TUI before interactive use:

```bash
npm install
npm run tui-rs:build
# optional test/development override: export MAESTRO_TUI_BIN=target/release/maestro-tui
```

---

## First launch

Run the read-only setup check once after installation. It checks the selected model, credentials, local configuration, and Codex transport, then prints the next command without exposing secret values:

```bash
deixic-code setup
```

Use `deixic-code setup --live` only when you want the optional provider metadata probe. Use `deixic-code setup --json` for scripts.

```bash
deixic-code
```

Or with an initial prompt (opens the TUI and submits once the agent is ready):

```bash
deixic-code "Audit this repository and suggest the next refactor"
```

Other common entrypoints:

```bash
deixic-code "…"                 # interactive TUI with trailing prompt
deixic-code serve --port 8080   # HTTP runtime gateway on http://localhost:8080
deixic-code --resume            # interactive session picker
deixic-code --continue          # resume most recent session for this cwd
```

---

## Authenticate

On first launch, Deixic Code opens `/setup` and requires an EvalOps Identity
sign-in. After that, choose managed inference or add a local API key (BYOK).
You can reopen it later with `/setup`.

Default path for Codex subscription models:

```bash
deixic-code codex login
```

Bare `deixic-code` defaults toward `openai-codex/gpt-5.5` when Codex auth is available. You can also set provider API keys (for example `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) or store keys in the retained `~/.maestro/keys.json` compatibility path. See [Authentication](02-authentication.md).

---

## Basic interaction

The TUI has a chat timeline, status/footer badges, and a multi-line editor:

- Type a message and press `Enter` to send.
- Press `Shift+Enter` for a newline.
- Type `/` for slash commands (see [Slash Commands](04-slash-commands.md)).
- Type `@` for fuzzy file search.
- Prefix a line with `!` for persistent bash mode, or `!! cmd` for a one-off shell command.

While the agent is running:

- `Enter` steers the current turn.
- `Alt+Enter` queues a follow-up.
- `Ctrl+C` interrupts / quits according to context.

---

## Modes at a glance

| Mode | How | Behavior |
|------|-----|----------|
| Normal | default | Selective approvals for risky tools |
| Plan | `/plan` or Shift+Tab cycle | Prefer a plan before mutating tools |
| Always-approve | `/always-approve` (`/yolo`) | Auto-approve tool executions |
| Ask-all | `/ask` | Require approval for all tools |
| Auto (selective) | `/auto` | Safe tools free, risky tools prompt |

See [Plan Mode](10-plan-mode.md) and [Sandbox and Safety](12-sandbox-and-safety.md).

---

## Next steps

1. [Authentication](02-authentication.md)
2. [Keyboard Shortcuts](03-keyboard-shortcuts.md)
3. [Slash Commands](04-slash-commands.md)
4. [Configuration](05-configuration.md)
