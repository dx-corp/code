# Deixic Code TUI (Rust)

Native terminal UI for Deixic Code, built with Rust using ratatui and crossterm. Inspired by [OpenAI Codex TUI](https://github.com/openai/codex/tree/main/codex-rs).

## User Guide

Numbered end-user docs for install, auth, shortcuts, slash commands, config, MCP, skills, hooks, sessions, safety, headless mode, and worktrees:

- **[TUI User Guide](docs/user-guide/README.md)**

## Why Rust?

The TypeScript TUI has limitations with SSH sessions where content that scrolls above the viewport becomes inaccessible. This Rust implementation:

1. **Native terminal scrollback**: Pushes content into the terminal's scrollback buffer using ANSI scroll regions (DECSTBM), persisting even over SSH
2. **Differential rendering**: Only sends changed cells, minimizing bytes over slow connections
3. **Native performance**: Rust + crossterm handles terminal rendering directly
4. **Standalone binary**: Single executable with native AI provider integrations - no Node.js runtime required

## Architecture

`maestro-tui` composes the CLI, interactive application, native agent, tools,
sessions, and hosted entrypoints. Reusable code has independent Cargo boundaries:

| Crate | Responsibility |
| --- | --- |
| `maestro-ui` | Terminal widgets, text layout, ANSI parsing, input editing, scrolling, and rendering helpers |
| `maestro-presentation` | Product-specific appearance and widgets |
| `maestro-interaction` | Transport-neutral interaction state and typed actions |
| `maestro-sandbox` | Native child-process and file-write enforcement; separate Linux and macOS implementations |
| `maestro-workspace` | File discovery, indexing, fuzzy search, Git inspection, and worktree operations |
| `maestro-codex` | Codex app-server transport and persisted thread bindings |
| `maestro-swarm` | Task graph parsing, dispatch types, and concurrent execution |
| `maestro-ai` | Provider clients |
| `maestro-execpolicy` | Command execution policy parsing and evaluation |
| `maestro-runtime-contracts` | Runtime identity, protocols, receipts, and telemetry contracts |
| `maestro-runtime` | Compatibility facade and future native runtime loop owner |

The extracted crates do not depend on `maestro-tui`. Historical imports such as
`maestro_tui::wrapping`, `maestro_tui::sandbox`, and `maestro_tui::files` remain
available through re-exports. Their tests run in the owning crate:

```bash
cargo test -p maestro-ui -p maestro-sandbox -p maestro-workspace -p maestro-codex -p maestro-swarm --locked
```

`agent`, `tools`, `session`, `config`, and the application still have mutual
dependencies. Separating the agent from the application requires moving the
shared event/state contracts and replacing application callbacks first.
`sandbox_policy` remains in the application because it resolves product
configuration; `maestro-sandbox` enforces the resulting policy. Codex login,
profile selection, and CLI commands stay with application configuration;
`maestro-codex` owns the protocol client and thread bindings. `maestro-swarm`
executes task graphs through a caller-provided task function; it does not own
the native agent lifecycle. `maestro-runtime-contracts` owns the dependency-light
runtime contracts; `maestro-runtime` retains the historical facade and will own
the native loop.

## Features

- **Native AI Integration**: Direct API calls to Anthropic (Claude) and OpenAI (GPT)
- **Tool Execution**: bash, read, write, edit, glob, grep - all native Rust
- **Chat Interface**: Message rendering with markdown support, syntax highlighting
- **Extended Thinking**: Support for Claude's extended thinking mode (`/thinking`)
- **Slash Commands**: `/help`, `/clear`, `/theme`, `/model`, `/thinking`, `/zen`, etc.
- **Resource Palette**: Ctrl+K for commands, files, sessions, models, and themes
- **File Search Modal**: `@` for fuzzy file search with workspace indexing
- **Session Management**: Ctrl+O to browse/switch sessions, auto-save
- **Tool Approval**: Interactive approve/deny for tool calls (`/approvals`)
- **Themes**: Built-in themes with custom theme support
- **Multi-line Input**: Shift+Enter for newlines
- **Clipboard Copy/Paste**: Enable with `cargo build --features clipboard`
- **Native Cursor**: Proper terminal cursor positioning (adapted from Codex)

## Building

```bash
cd packages/tui-rs
cargo build --release
```

The binary will be at `target/release/maestro-tui`.

## Running

```bash
# Set your API key
export ANTHROPIC_API_KEY=sk-...
# or
export OPENAI_API_KEY=sk-...

# Run with default settings (uses Claude by default)
./target/release/maestro-tui

# Specify a model
./target/release/maestro-tui --model gpt-4o
./target/release/maestro-tui --model claude-sonnet-4-5-20250514

# Resume last session
./target/release/maestro-tui --resume
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic API key (for Claude models) |
| `OPENAI_API_KEY` | OpenAI API key (for GPT models) |
| `MAESTRO_MODEL` | Override default model |
| `MAESTRO_BACKGROUND_TASK_LOG_BYTES` | Max log file size before rotation (default 5MB) |
| `MAESTRO_BACKGROUND_TASK_LOG_SEGMENTS` | Number of rotated log segments to keep (default 2) |

## Background Task Logs

Background task output is written to `~/.maestro/logs` with rotation. Defaults are 5MB per log file and 2 rotated segments.

The `background_tasks` tool supports `action=waitForRotation` with an optional `timeoutMs` (default 5000) to wait for a rotation event:

```text
background_tasks action=waitForRotation taskId=<id> timeoutMs=10000
```

## Conductor Bridge Helpers

The Rust crate exposes helper types for probing a Deixic Code web server bridge:

```rust
use maestro_tui::bridge::fetch_bridge_status;

let status = fetch_bridge_status("http://localhost:8080").await?;
println!("Deixic Code version: {:?}", status.version);
```

This is useful for tooling or diagnostics that need to confirm the bridge is
online before connecting from Conductor.

## Module Structure

```
src/
├── agent/           # Native agent implementation
│   ├── native.rs    # Agent loop, tool handling, AI communication
│   └── protocol.rs  # Message types (FromAgent, ToAgent)
├── ai/              # AI provider clients
│   ├── anthropic.rs # Claude API (Messages API)
│   ├── openai.rs    # OpenAI API (Responses API)
│   └── client.rs    # Unified client interface
├── tools/           # Native tool implementations
│   ├── registry.rs  # Tool definitions and execution
│   └── bash.rs      # Shell command execution
├── app.rs           # Main application & event loop
├── commands/        # Slash command system
│   ├── registry.rs  # Command registration
│   ├── matcher.rs   # Fuzzy matching & tab completion
│   └── types.rs     # Command definitions
├── components/      # UI widgets (ratatui)
│   ├── message.rs   # Chat view, input, status bar
│   ├── approval.rs  # Tool approval modal
│   ├── command_palette.rs
│   ├── file_search.rs
│   ├── session_switcher.rs
│   └── textarea.rs  # Text input with cursor (from Codex)
├── files/           # Workspace file indexing
│   ├── workspace.rs # File discovery
│   └── search.rs    # Fuzzy file search
├── headless/        # Headless mode for scripting
├── session/         # Session persistence
│   ├── manager.rs   # List/load/save sessions
│   ├── reader.rs    # JSONL parsing
│   └── writer.rs    # JSONL writing
├── terminal/        # Terminal setup & history
│   ├── setup.rs     # Raw mode, alternate screen
│   └── history.rs   # Scrollback buffer management
├── themes/          # Theme system
├── state.rs         # Application state
├── markdown.rs      # Markdown rendering
├── syntax.rs        # Syntax highlighting (tree-sitter)
├── diff.rs          # Diff display
└── wrapping.rs      # Text wrapping utilities
```

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Enter` | Send message (steer while running) |
| `Alt+Enter` | Queue follow-up (while running) |
| `Shift+Enter` | Insert newline (multi-line input) |
| `Esc` | Cancel/close modal |
| `Ctrl+C` | Interrupt agent / Quit |
| `Ctrl+K` | Unified resource palette |
| `Ctrl+O` | Session switcher |
| `Ctrl+T` | Toggle last tool call details |
| `Tab` | Toggle thinking / Cycle completions |
| `@` | File search (in input) |
| `/` | Slash command (in input) |
| `↑/↓` | Navigate history/lists |
| `g/G` | Jump to top/bottom |

## Slash Commands

| Command | Description |
|---------|-------------|
| `/help` | Show help |
| `/clear` | Clear conversation |
| `/model [name]` | Change or view model |
| `/thinking [level]` | Set thinking level (off/minimal/low/medium/high/max) |
| `/approvals` | Cycle approval mode (YOLO/Selective/Safe) |
| `/zen` | Toggle zen mode (minimal UI) |
| `/theme [name]` | Change theme |
| `/diag` | Show diagnostics |
| `/compact` | Summarize older messages |
| `/mcp` | Show MCP server status |
| `/plugins` | List discovered plugins (skills/commands/hooks/MCP packages) |
| `/plugins [name]` | Show details for one plugin |

## Plugins

Deixic Code discovers filesystem plugins under `.maestro/plugins/*` and
`~/.maestro/plugins/*` (plus legacy `.composer/plugins` paths). Each plugin
may bundle skills, command templates, hooks, and MCP configs. See
[docs/plugins.md](docs/plugins.md) for layout, `plugin.json` schema, and
examples.

## Status

**Current feature set:**
- Native AI integration (Anthropic Claude, OpenAI GPT)
- Full tool suite (bash, read, write, edit, glob, grep)
- Extended thinking support
- Streaming responses
- Slash command system with fuzzy matching
- Modal system (file search, commands, sessions, approval)
- Session persistence and management
- Multi-line input with Shift+Enter
- Theme support
- Native cursor positioning

## Credits

Cursor positioning and text area implementation adapted from [OpenAI Codex](https://github.com/openai/codex) (MIT License).
