# Keyboard Shortcuts

Defaults from the native TUI. Some bindings are customizable via `~/.maestro/keybindings.json` (`/hotkeys`).

---

## Core editing and send

| Key | Action |
|-----|--------|
| `Enter` | Send message; while a turn is running, steer |
| `Alt+Enter` | Queue a follow-up while the agent is running |
| `Shift+Enter` | Insert newline (multi-line input) |
| `↑` / `↓` | Navigate prompt history or lists |
| `Tab` | Toggle thinking / cycle completions (context-dependent) |
| `Esc` | Cancel / close the active modal |

---

## Navigation and modals

| Key | Action |
|-----|--------|
| `/` | Start a slash command |
| `@` | Fuzzy workspace file search |
| `Ctrl+K` | Unified palette for commands, files, sessions, models, and themes |
| `Ctrl+O` | Session switcher (also bound as the default file-search related label in keybinding config — see `/hotkeys`) |
| `Ctrl+T` | Toggle last tool call details |
| `g` / `G` | Jump to top / bottom of scrollback |

Note: package docs list `Ctrl+O` for sessions and `@` / `/files` for file search. Use `/hotkeys show` for the effective binding table on your machine.

The palette accepts an unprefixed query across every resource type. Prefix a query to restrict results:

| Prefix | Resource |
|--------|----------|
| `>` or `command:` | Slash commands |
| `@` or `file:` | Workspace files |
| `#` or `session:` | Recent sessions |
| `:` or `model:` | Models |
| `%` or `theme:` | Themes |

The `@`, session, model, and theme modals remain available through their existing shortcuts and slash commands.

---

## Control and interrupt

| Key | Action |
|-----|--------|
| `Ctrl+C` | Interrupt the agent or quit |
| `Shift+Tab` | Cycle thinking effort |

Prompt queue behavior while a turn is running is documented in [Prompt Queue](../../../../docs/PROMPT_QUEUE.md): Enter steers; `Alt+Enter` queues a follow-up.

---

## Bash mode

| Input | Action |
|-------|--------|
| `!` prefix | Enter persistent bash mode |
| `!! <command>` | One-off shell command without entering bash mode |
| `exit` / `quit` / `leave` | Leave bash mode |
| `↑` / `↓` in bash mode | Shell command history |
| `Shift+Enter` in bash mode | Literal newline |

---

## Custom keybindings

```text
/hotkeys              # show shortcuts help
/hotkeys path         # print config path
/hotkeys init         # create ~/.maestro/keybindings.json
/hotkeys init --force # overwrite existing file
/hotkeys validate     # validate config
```

Aliases: `/keys`, `/shortcuts`.

Override path with `MAESTRO_KEYBINDINGS_FILE`. The schema uses a `rustBindings` map for native actions such as command palette, file search, toggle tool outputs, and edit last queued follow-up.

---

## Accessibility

| Env | Effect |
|-----|--------|
| `MAESTRO_REDUCED_MOTION=1` | Reduce animated UI (often auto-enabled on SSH/tmux/screen) |
| `MAESTRO_DISABLE_ANIMATIONS=1` | Hard-disable TUI animations |
