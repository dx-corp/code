# Tools Reference

Audience: contributors and advanced users adding/debugging tools.  
Nav: [Docs index](README.md) · [Quickstart](QUICKSTART.md) · [Features](FEATURES.md) · [Safety](SAFETY.md)

Contents: [Validation](#parameter-validation) · [Error Handling](#error-handling-for-tool-authors) · [Built-in Tools](#built-in-tools) · [GitHub CLI Tools](#github-cli-tools) · [CLI Commands](#cli-commands) · [Common Errors](#common-errors--remedies)

The agent and CLI expose a consistent set of tools. Use this sheet when adding
new tools or debugging existing ones.

## Codex Tool Profiles

OpenAI Codex app-server sessions use a smaller curated profile by default so the
model-visible surface stays focused while the full Maestro registry remains
available for explicit selection. Experimental tools such as
`repository_symbols` stay out of the initial profiles and must be activated
through `tool_search` (or selected by an explicit all-tools configuration).

| Profile | Use | Tools |
| --- | --- | --- |
| `lean` / `default` | Normal Codex coding sessions. | `read`, `list`, `find`, `search`, `diff`, `bash`, `apply_patch`, `edit`, `write`, `todo`, `status`, `gh_pr` |
| `read-only` / `readonly` | Audits, planning, and explorer-style runs. | `read`, `list`, `find`, `search`, `diff`, `status` |
| `extended` | Compatibility profile for the previous broader Codex surface. | `read`, `list`, `find`, `search`, `parallel_ripgrep`, `diff`, `bash`, `background_tasks`, `apply_patch`, `edit`, `write`, `todo`, `status`, `gh_pr`, `gh_issue`, `gh_repo` |

Set `MAESTRO_CODEX_TOOL_PROFILE=read-only` or
`MAESTRO_CODEX_TOOL_PROFILE=extended` before launching a Codex app-server model.
The explicit `--tools` CLI selection still wins over profile selection.

File mutation stays intentionally split across `apply_patch`, `edit`, and
`write`. Do not replace those with a generic workspace or file tool; separate
tools give policy, approvals, audit receipts, and model planning a clearer
action boundary.

## Parameter Validation

Every native tool declares and validates its JSON input contract before
execution. Serde-backed argument types apply defaults and reject malformed or
incompatible fields with a structured tool error. Definitions and dispatch live
under `packages/tui-rs/src/tools`.

## Error Handling for Tool Authors

Implement tools through the native registry in `packages/tui-rs/src/tools/registry`.
Return typed errors instead of successful text that merely describes a failure;
the protocol layer maps them to `isError: true`. Sanitize secrets and absolute
paths before attaching diagnostic context, and preserve retryable versus terminal
error distinctions.

## Built-in Tools

| Tool | Description | Key Options / Notes |
| ---- | ----------- | ------------------- |
| `background_tasks` | Runs commands in the background and manages lifecycle. | `action` supports `start`, `list`, `stop`, `logs`, and `waitForRotation`. `/monitor` attaches bounded stdout/stderr regex notifications to existing tasks. Model turns are disabled for monitor matches. |
| `read` | Reads file contents with syntax-aware chunking. Supports text, images, PDFs, and Jupyter notebooks. | Accepts `path`, optional `startLine`/`endLine`. Images are optimized with Sharp if available. PDFs are extracted to text. Notebooks display formatted cells with outputs. |
| `list` | Lists files in a directory (non-recursive by default). | Supports glob filters and depth. Used for context discovery. |
| `search` | Ripgrep-style text search. | Args mirror `rg` (`pattern`, `path`, `glob`). Output includes file:line matches. Default max results now capped to avoid huge responses; oversized outputs are truncated and marked. |
| `repository_symbols` | Experimental Rust symbol localization backed by a session-local incremental index. | Takes one unqualified `symbol` and optional `maxResults` (1-100). Results include definitions, referencing files, a content revision, age, and explicit truncation/skipped-file metadata. It is a search projection, not authorization evidence. |
| `diff` | Wrapper around `git diff`. | Modes: workspace, staged, or custom ranges. Also supports `mode: "status"` (legacy) but prefer the dedicated `status` tool. |
| `status` | Structured `git status` (porcelain v2). | Options: `branchSummary` (-b), `includeIgnored` (`--ignored=matching`), `paths`. Returns parsed status in details + summary text. |
| `bash` | Executes shell commands (`bash -lc`). | Default timeout 90s (max 600s) and 40KB output cap; mutating commands require a plan when safe-mode is on. Runs from repo root; stdout/stderr streamed. In bash mode, `cd` is handled internally. |
| `apply_patch` | Applies Codex-native `*** Begin Patch` blocks. | Accepts `patch` and optional `dryRun`. Supports Add/Update/Delete File operations, reports touched files, diffs, hunk counts, diagnostic delta, and validator results. Failed hunks are retryable tool errors with conflict details. |
| `edit` | Structured find/replace writer. | Accepts `path`, `oldText`, `newText`. Supports `edits` array for multiple sequential edits, `replaceAll` for bulk replacements, and `dryRun` for previews. |
| `write` | Writes or overwrites files. | Takes `path` + `contents`. Creates directories automatically. |
| `todo` | Generates TodoWrite-style task lists. | Stored near the project (`~/.maestro/todos.json`). Integrates with `/plan`. |
| `notebook_edit` | Edit Jupyter notebook (.ipynb) files at the cell level. | Modes: `replace` (default), `insert`, `delete`. Identify cells by `cell_id` or `cell_index`. Specify `cell_type` (code/markdown) for inserts. |
| `ask_user` | Ask structured questions with predefined options. | 1-4 questions per call, each with 2-4 options. Supports `multiSelect` for non-exclusive choices. "Other" option auto-added. |
| `websearch` | Search the web via Exa AI for real-time information. | Supports neural/keyword search, domain filtering, date ranges. Requires `EXA_API_KEY` env var. Large result text is previewed with truncation and overall output is capped. |
| `codesearch` | Search GitHub/docs/Stack Overflow for code examples via Exa Code. | Returns working code snippets with context. Requires `EXA_API_KEY` env var. |
| `webfetch` | Fetch content from specific URLs via Exa. | Converts HTML to markdown, truncates very long content, and caps total output. Requires `EXA_API_KEY` env var. |

## GitHub CLI Tools

| Tool | Description | Actions / Options |
| ---- | ----------- | ----------------- |
| `gh_pr` | Pull request operations | **Actions:** `create`, `checkout`, `view`, `list`, `comment`<br>**Options:** `number`, `title`, `body`, `branch`, `base`, `draft`, `state`, `author`, `limit`, `json` |
| `gh_issue` | Issue operations | **Actions:** `create`, `view`, `list`, `comment`, `close`<br>**Options:** `number`, `title`, `body`, `labels`, `state`, `author`, `limit`, `json` |
| `gh_repo` | Repository operations | **Actions:** `view`, `fork`, `clone`<br>**Options:** `repository`, `directory`, `json` |

**Prerequisites:**
- GitHub CLI (`gh`) must be installed: `brew install gh` (macOS) or see [cli.github.com](https://cli.github.com)
- Must be authenticated: `gh auth login`
- Must be in a git repository with GitHub remote (for PR/issue operations)

**Batch Tool Usage:**
- Parallelism is native: emit multiple tool calls in one response when you need concurrent reads/searches; the runtime will execute independent calls in parallel without a batch wrapper.

**Examples:**
```json
// Create PR
{action: "create", title: "Fix auth bug", body: "Details...", base: "main"}

// Checkout PR for review
{action: "checkout", number: 123}

// Create issue with labels
{action: "create", title: "Bug report", body: "Steps...", labels: ["bug", "priority"]}

// List open issues by author
{action: "list", state: "open", author: "username", limit: 10}

// View repo info as JSON
{action: "view", json: true}
```

## CLI Commands

For install/build/test entrypoints, use `docs/QUICKSTART.md` (canonical). Key dev helpers are `cargo run --manifest-path packages/maestro-rs/Cargo.toml -- --help`, `npm run check:scenario-replay-gate`, and `npm test`.

The hosted Computer console is available from the CLI and TUI. A handoff freezes
explicit files, artifacts, and/or the current remote diff into Computer's
tenant-scoped immutable package store:

```sh
maestro computer handoff create <source-task-id> <target-thread-id> \
  [--file path] [--artifact id] [--include-diff]
maestro computer handoff list <target-thread-id>
maestro computer handoff read <target-thread-id> <package-id>
```

The source task must be a durable hosted Computer task in the active managed
account/workspace. The client rejects empty selections and path traversal before
the network call; Computer remains authoritative for tenant authorization, workspace
reads, package bounds, persistence, and digest verification.

The top-level TUI command `/handoff` sends a prompt to the default A2A peer,
records the accepted task in the shared A2A ledger, and posts its terminal
response back into the current transcript:

```text
/handoff <prompt>
/handoff --peer <name> <prompt>
/handoff --source-task <source-task-id> --target-thread <target-thread-id> \
  [--file path] [--artifact id] [--include-diff] -- <prompt>
```

The prompt does not require quotes. The package flags are optional as a group.
When any package flag is present, both IDs and at least one file, artifact, or
diff selection are required.

## Common Errors & Remedies

- **File not found** (read/write/list/search): resolve paths from the configured
  workspace root and sanitize user input before reading.
- **Diff shows nothing**: workspace clean. Use `/diff staged` or ensure `git`
  knows about the changes.
- **Bash tool blocked**: action firewall flagged a destructive command. Approve
  via the TUI prompt or adjust the safe-mode settings.
- **Tool queue stuck**: `/queue cancel <id>` removes stale prompts; loader now
  uses a subtle animation so you know when the agent is still working.

If you add a tool, expose it in:

1. `packages/tui-rs/src/tools/registry` (implementation and registration)
2. Docs (update this file + CLI help if needed)
3. Tests/evals if the behavior is user-facing
