# Slash Commands

Type `/` in the editor for completions. Built-ins are registered in `packages/tui-rs/src/commands/registry.rs` and always win on name collisions with skill or prompt extensions.

This page lists **built-in** commands only. Skills and prompt templates can add extra `/name` entries at runtime; see [Skills](08-skills.md).

---

## Session and navigation

| Command | Aliases | Description |
|---------|---------|-------------|
| `/help [command]` | `/h`, `/?` | Show help modal or help for a command |
| `/clear` | `/cls`, `/new` | Start a new session (clear transcript) |
| `/fork` | | Fork the conversation into a new session branch |
| `/rewind [n]` | `/undo` | Rewind the last N user turns (default 1) |
| `/quit` | `/exit`, `/q` | Quit the application |
| `/continue` | `/c` | Continue the most recent session for this workspace |
| `/resume` | `/r` | Resume a specific session (opens session list) |
| `/session […]` | `/ss` | Session info / new / clear / fork / rewind / cleanup |
| `/sessions` | | List and manage sessions (modal) |
| `/export [format] [path]` | | Export session (`markdown`, `html`, `json`, `text`) |
| `/history [count\|query\|clear]` | `/hist` | Show or search prompt history |
| `/files` | | Search workspace files |
| `/commands` | | Open the unified resource palette |
| `/refresh` | | Refresh workspace files |

---

## Models, thinking, UI

| Command | Aliases | Description |
|---------|---------|-------------|
| `/model [name]` | `/m` | Change AI model (selector if no name) |
| `/thinking <level>` | | Levels: `off`, `minimal`, `low`, `medium`, `high`, `max` |
| `/theme [name]` | | Change color theme |
| `/zen` | | Toggle zen mode (minimal UI) |
| `/compact-tools [on\|off\|toggle]` | | Toggle tool output folding |
| `/footer [style]` | | Footer style: `rich`, `solo`, `history`, `clear` |
| `/copy [prompt\|response\|prompts\|responses\|turns <first-last>\|session\|all]` | | Copy conversation text or the session ID |
| `/language [en\|es\|fr\|de\|ja\|ko\|zh-CN]` | | Choose and save the display language |
| `/hotkeys […]` | `/keys`, `/shortcuts` | Show or manage keyboard shortcuts |

---

## Queue and steering

| Command | Description |
|---------|-------------|
| `/queue [list\|cancel <id>\|mode …]` | Manage queued prompts |
| `/steer <message>` | Send a steering message |

Queue modes: `/queue mode [steer|followup] <one|all>`.

---

## Setup and project bootstrap

| Command | Description |
|---------|-------------|
| `/setup` | First-run modal: EvalOps browser login, or a local provider API key (OpenRouter, Anthropic, OpenAI, Google, xAI). Completing setup starts the agent. |
| `/init [--force]` | Write `AGENTS.md` in the current workspace (same scaffold as `maestro agents init`). If the file already exists, prints a preview; `--force` overwrites and, on first create, submits a draft prompt. |

---

## Safety and plan

| Command | Aliases | Description |
|---------|---------|-------------|
| `/approvals [yolo\|selective\|safe]` | | Set approval mode (cycles if omitted) |
| `/always-approve` | `/yolo` | Auto-approve all tool executions |
| `/auto` | | Selective approvals (safe free, risky prompt) |
| `/ask` | | Require approval for all tools |

---

## Context, tools, MCP, skills, hooks

| Command | Aliases | Description |
|---------|---------|-------------|
| `/compact [instructions]` | | Compact conversation history |
| `/context` | | Show context summary |
| `/memory` | | Account / local / shared memory status |
| `/tools [list\|mcp\|lsp]` | | List built-in tools |
| `/mcp [resources\|prompts …]` | | MCP status and resources/prompts |
| `/hooks [list\|toggle\|reload\|metrics\|enable\|disable]` | `/hook` | Manage hooks |
| `/skills […]` | `/skill` | Manage skills (`list`, `activate`, `deactivate`, `reload`, `info`) |
| `/toolhistory […]` | `/th` | Tool execution history and stats |
| `/a2a …` | | Pair, inspect, and delegate to A2A peer agents |
| `/monitor [list\|add …\|remove …]` | | Manage regex monitors for existing background tasks |

---

## Diagnostics and git

| Command | Aliases | Description |
|---------|---------|-------------|
| `/about` | | Build and environment info |
| `/version` | `/v` | TUI version |
| `/status` | `/health` | System health overview |
| `/stats` | | Status + usage summary |
| `/diag [status\|stats\|about\|context\|mcp]` | | System diagnostics |
| `/limits [all\|tool\|lsp]` | | Configurable runtime limits |
| `/cost [summary\|detailed\|reset]` | `/usage`, `/tokens` | Token usage and cost |
| `/diff [path]` | | Git diff for working tree or path |
| `/review` | | Summarize git status and diff stats |
| `/git [status\|diff\|review]` | | Git helper group |

## Background task monitors

Attach a monitor to a task that is already running:

```text
/monitor add <task-id> <regex>
/monitor list
/monitor remove <monitor-id>
```

Monitors read the task's existing stdout and stderr streams. Monitor attachment creates no process. A match adds a system notification and a live row in `/operations`. Model prompt submission is disabled for monitor events.

Limits per process: 32 monitors total, 8 per task, 256 bytes per regex, 1 MiB compiled-regex size, 5 events per monitor per second, 512 displayed characters per match, 128 pending notifications, and 200 retained operation rows. Credential-shaped values are redacted before an event is stored or displayed.

---

## More built-ins

| Command | Description |
|---------|-------------|
| `/btw <question>` | Tool-free side question outside main history |
| `/workflow …` | List/run/pause/resume/stop durable workflows |
| `/decision …` | List, answer, or cancel background decisions |
| `/trust [status\|grant\|revoke]` | Project skills/plugins/hooks trust |
| `/sandbox` | Interactive sandbox policy for this session |
| `/operations` | Recent persisted tool executions |
| `/loop [stop\|<interval> <prompt>]` | Re-run a prompt on an interval |
| `/focus [on\|off\|toggle]` | Collapse tool-heavy turns |
| `/prompt-audit [--json]` | Prompt provenance without prompt content |
| `/alerts` | Recorded agent/API errors |
| `/mcp-config …` | MCP server wizard, list, add, remove |
| `/plugins …` | Installed plugins and marketplace |
| `/goal …` | Structured goal mode |
| `/harness …` | Durable prompt/memory/skill/subagent context |
| `/rlm …` | Named context variables |
| `/mailbox …` | Durable messages between agent sessions |
| `/attach …` | Queue local files for the next prompt |

---

## Extensions (not built-ins)

- Prompt templates: `.maestro/prompts/*.md`, `.maestro/commands/*.md`, and user-level equivalents under `~/.maestro/…` (plus legacy composer paths where configured).
- Skills: `.maestro/skills/<name>/SKILL.md` and user skill dirs; invocable as `/<name>` when `user_invocable` is enabled.

Built-in names always take precedence.

## Bug reports

`/bug` (alias `/feedback`) shows the current session's bug report draft.

- `/bug draft <description>` creates or edits what happened.
- `/bug expected <text>` records what should have happened.
- `/bug diagnostics on|off` opts into the app version; defaults to off.
- `/bug review` shows the exact report fields and destination workspace.
- `/bug send` submits the reviewed report and displays its `DX-…` reference.
- `/bug dismiss` dismisses the local draft. It cannot retract a report the service already accepted.

A non-retryable terminal failure suggests one local draft per session. Error
payloads, prompts, tool output, environment variables, and transcripts are not
attached automatically. Add reproduction details yourself and check the draft
for private information before sending. Drafts survive session resume, and
editing requires a new review. If delivery is uncertain, retrying uses the same
submission ID. An uncertain report cannot be edited; dismiss it before creating
a different report. Drafts are unavailable with `--no-session`.

Reports use the existing Deixic product-issue service, staff queue, and
notification outbox. Sign in and select a workspace to send. The default platform
origin is the same as product setup; `MAESTRO_EVALOPS_BASE_URL` supports a configured
HTTPS origin. New logins request the narrow `product_issues:write` permission;
existing web clients retain `console:write`. Installations with an explicit
`IDENTITY_ALLOWED_PRODUCT_SCOPES` override must include `product_issues:write`.
A missing permission leaves the draft saved and asks you to sign in again.

## Connection presets, copying, and display languages

`/mcp` opens the connection manager. Press `c` to browse 40 connection presets,
then type a name, category, or search words. Use the arrow keys and Enter to add
a connection to your user configuration. The detail area shows the endpoint or
local command and its setup documentation. A check mark means configured, not
authenticated. Existing connections are kept when you select the same preset
again. No preset is installed by browsing alone.

Back in the server list, `o` starts browser authentication through the existing
MCP OAuth flow. Use `r` to retry, Space to enable or disable a server, and Enter
to inspect its tools. Space in the tool view toggles the selected tool. Managed
connections remain controlled by their owning configuration. Some providers
require client registration or account permissions before authentication works;
consult the displayed documentation. Presets describe connection setup, not a
claim that your account is connected. Local presets require their listed tools.

The same presets are available through `deixic-code mcp registry list` and
`deixic-code mcp registry add linear`. Installation preserves other servers and
refuses to overwrite an existing definition.

`/copy` keeps its previous default: copy the latest assistant response. Use
`/copy prompt` for the latest user prompt, `/copy prompts` or `/copy responses`
for one side of the conversation, `/copy turns 2-4` for an inclusive turn range,
and `/copy session` for the saved session ID. Each turn starts with a user
prompt and includes the assistant replies before the next prompt. Numbering
starts at one in the current transcript; compacted-away history is not
reconstructed. `/copy all` copies the current conversation. Multi-message
copies include role labels. System notices, side questions, tool payloads, and
reasoning are excluded. Invalid or unavailable ranges leave the clipboard alone.

`/language` opens the language picker, also available under Settings → Appearance
and keyboard. You can enter `/language ja` directly. The preference is saved in
the existing `ui.json` file and applies immediately. Initial locales are English,
Spanish, French, German, Japanese, Korean, and Simplified Chinese. Translation
currently covers the connection manager's navigation and empty states, language
controls, and clipboard success/failure labels. Other text, including provider
errors and preset descriptions, remains English. Commands, copied conversation
text, model selection, and agent prompts are unchanged. Unknown saved language
codes fall back to English.
