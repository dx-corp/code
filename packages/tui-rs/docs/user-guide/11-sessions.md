# Sessions

Maestro persists interactive conversations as JSONL so you can resume, fork, rewind, and export work. Full format notes: [Sessions](../../../../docs/SESSIONS.md).

---

## Storage layout

Primary layout (user-facing docs):

```text
~/.maestro/agent/sessions/
  └─ --Users-me-project--/
       └─ 2025-01-15T18-05-23.982Z_<uuid>.jsonl
```

The directory name is derived from the project cwd (slashes → dashes) so repos do not collide.

Some native modules document legacy `~/.composer/agent/sessions/` paths; prefer `~/.maestro` and set `MAESTRO_HOME` if you relocate config.

---

## CLI flags

| Flag | Effect |
|------|--------|
| `--continue` | Load the most recent session for this cwd |
| `--resume` | Interactive session picker |
| `--session <path>` | Use a specific JSONL file |
| `--no-session` | Disable persistence for this run |

```bash
maestro --continue
maestro --resume
maestro --resume
```

Native helpers also expose `maestro sessions` early-exit subcommands.

---

## In-session commands

| Command | Description |
|---------|-------------|
| `/sessions` | List / manage sessions (modal) |
| `/session [info\|new\|clear\|fork\|rewind\|cleanup]` | Session operations |
| `/continue` | Continue most recent for workspace |
| `/resume` | Open session list |
| `/clear` / `/new` | Start a fresh session |
| `/fork` | Branch transcript into a new session |
| `/rewind [n]` | Drop last N user turns and rebuild history |
| `/export [format] [path]` | Export (`markdown`, `html`, `json`, `text`) |
| `/history …` | Prompt history |
| `/compact` | Compact older context |

Keyboard: `Ctrl+Alt+R` opens the session switcher.

You can also open `/sessions` or `/resume` while startup is still preparing
cloud access. The list loads in the background, and closing it restores your
draft. Choosing a saved local-model session does not wait for cloud setup.
Reading a saved conversation does not authorize managed inference: cloud
models still require verified access when you use them.

---

## Favorites and summaries

Session metadata can store favorites and manual summaries (`session_meta` events). Use `/session` subcommands and `/sessions` management flows where available (see SESSIONS.md for favorite/summary helpers on the broader Maestro surface).

---

## Cleanup

- Delete the per-cwd sessions directory to wipe history for a repo.
- Use `--no-session` in CI or ephemeral workspaces.
- `/session cleanup` / prune paths when exposed in the session command group.
