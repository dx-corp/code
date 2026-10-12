# Agent Development Guidelines for tui-rs

This document provides context for AI agents working on the Rust TUI codebase.

## Quick Reference

```bash
# Build
cargo build --release

# Test
cargo test

# Run
./target/release/composer-tui
```

## Architecture Overview

This is a **native Rust TUI** with a fully native agent runtime, AI provider clients, and tool execution. It does **not** require a Node.js subprocess for normal operation. A small Node bridge is used only for optional TypeScript hook execution.

### Key Design Decisions

1. **Native Agent Architecture**: AI communication and streaming live in maestro-runtime; local tool execution and runtime construction live in maestro-local-host. The TUI reexports historical module paths. This enables a standalone binary and avoids cross-process latency. A Node hook bridge exists only for running TS hooks when enabled.

2. **Cursor Positioning**: Uses native terminal cursor (adapted from OpenAI Codex) rather than inline cursor rendering. See `components/textarea.rs` for the implementation.

3. **Modal System**: Single `ActiveModal` enum in `app.rs` controls which modal is visible. Only one modal can be active at a time.

4. **Session Persistence**: Sessions are JSONL files in `~/.maestro/sessions/`. The format matches the TypeScript TUI for compatibility.

### ⚠️ Critical: TypeScript vs Rust Architecture Differences

**Do NOT blindly port TypeScript patterns to Rust.** The architectures are fundamentally different:

| Aspect | TypeScript TUI | Rust TUI |
|--------|---------------|----------|
| **Rendering** | React-like component model with lifecycle hooks | Immediate-mode rendering (Ratatui) - stateless widgets |
| **State** | Distributed across components with subscriptions | Centralized `AppState` struct passed by `&mut` reference |
| **Async** | Event emitters, subscriptions, observers | MPSC channels (`tokio::sync::mpsc`) |
| **Modals** | Lifecycle-based (mount/unmount/dispose) | Enum-based (`ActiveModal`) with direct field access |
| **Updates** | Reactive (state changes trigger re-renders) | Imperative (explicit render calls each frame) |

#### Patterns That Do NOT Apply to Rust TUI

1. **Component Lifecycle Hooks** (`onMount`, `onUnmount`, `dispose`)
   - Ratatui widgets are **stateless** - they render and are discarded each frame
   - State lives in `AppState`, not in components
   - No cleanup callbacks needed - Rust's ownership handles resource cleanup

2. **Subscription/Observer Pattern** (`SubscriptionManager`, event buses)
   - Use MPSC channels instead: `mpsc::UnboundedSender<FromAgent>`
   - State updates go through `AppState::handle_agent_message()`
   - No need to track subscriptions - channels are type-safe and ownership-managed

3. **Distributed Component State**
   - All mutable state lives in `AppState` (see `state.rs`)
   - Components receive state as constructor arguments or render parameters
   - No component-local state that persists across renders

4. **Session Metadata Cache** (for tracking model/thinking level)
   - `SessionInfo` already contains `thinking_level` (see `session/manager.rs:331`)
   - `AppState` has `model: Option<String>` for current model
   - Session headers contain all metadata - just read them

#### How to Properly Extend the Rust TUI

**Adding State:**
```rust
// In state.rs - add to AppState struct
pub struct AppState {
    // ... existing fields
    pub my_new_field: Option<String>,
}

// Initialize in AppState::new()
Self {
    // ... existing fields
    my_new_field: None,
}
```

**Adding a Tool:**
1. Add a module under `products/maestro/packages/local-host-rs/src/tools/`
2. Add to `ToolExecutor` in maestro-local-host
3. Add match arm in `ToolExecutor::execute()`
4. Register schema in `ToolRegistry::new()`

**Handling Agent Events:**
```rust
// In state.rs - add match arm to handle_agent_message()
FromAgent::MyNewEvent { data } => {
    self.my_new_field = Some(data);
}
```

**Adding UI Components:**
```rust
// Widgets are stateless - they borrow data and render
pub struct MyWidget<'a> {
    data: &'a str,  // Borrow from AppState
}

impl<'a> Widget for MyWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Draw to buffer - widget is consumed
    }
}
```

### Key Files to Read First

Before making changes, understand these foundational files:

| File | Purpose | Read When |
|------|---------|-----------|
| `state.rs` | Central `AppState` struct with all mutable state | Adding any new state or modifying state handling |
| `app.rs` | Main event loop, keyboard handling, modal management | Adding modals, changing input handling |
| `products/maestro/packages/local-host-rs/src/tools/registry.rs` | Tool definitions, schemas, execution dispatch | Adding or modifying tools |
| `products/maestro/packages/runtime-rs/src/agent/native.rs` | Agent lifecycle, tool handling, provider orchestration | Changing agent behavior |
| `products/maestro/packages/runtime-rs/src/agent/protocol.rs` | `FromAgent` enum - all agent→UI events | Adding new agent events |
| `products/maestro/packages/session-rs/src/session/manager.rs` | Session CRUD, `SessionInfo` struct | Working with sessions |
| `products/maestro/packages/session-rs/src/session/entries.rs` | JSONL entry types (`SessionEntry`, `ThinkingLevel`, etc.) | Modifying session format |
| `components/message.rs` | Chat UI widgets (`ChatView`, `MessageWidget`, etc.) | Changing message rendering |

## Module Guide

### `app.rs` - Main Application
- Event loop, keyboard handling, modal management
- `App::run()` is the main entry point
- `App::render()` draws the UI and positions cursor

### `components/` - UI Widgets
All widgets implement ratatui's `Widget` trait.

- **`message.rs`**: `ChatView`, `ChatInputWidget`, `MessageWidget`, `StatusBarWidget`
- **`approval.rs`**: `ApprovalModal`, `ApprovalController` for tool approval flow
- **`command_palette.rs`**: Ctrl+K typed resource palette
- **`file_search.rs`**: `@` file search modal
- **`session_switcher.rs`**: Ctrl+O session browser
- **`textarea.rs`**: Text input with cursor positioning (from Codex)

### `commands/` - Slash Commands
- **`registry.rs`**: Command registration and lookup
- **`matcher.rs`**: Fuzzy matching, scoring, tab completion (`SlashCommandMatcher`, `SlashCycleState`)
- **`types.rs`**: `Command`, `CommandContext` definitions

### `agent/` - Runtime Compatibility Exports
- **`mod.rs`**: Reexports the shared host constructors.
- Local tools, hooks, credentials, and the runtime adapter live in `products/maestro/packages/local-host-rs/src`.
- Child-agent lifecycle and the injectable child factory live in the shared host `products/maestro/packages/local-host-rs/src/subagents/` module.
- The native actor and provider loop live in `products/maestro/packages/runtime-rs/src/agent/`. Shared events live in maestro-runtime-contracts.

### `products/maestro/packages/session-rs/src/session/` - Persistence
- **`manager.rs`**: `SessionManager` lists/loads sessions
- **`reader.rs`**: Parses JSONL session files
- **`writer.rs`**: Writes session entries

### `products/maestro/packages/tui-kit-rs/src/terminal/` - Terminal Setup
- **`setup.rs`**: Raw mode, alternate screen, mouse capture
- **`history.rs`**: Scrollback buffer management

### `state.rs` - Application State
`AppState` holds all UI state: messages, input, cursor, busy flag, etc.

## Common Tasks

### Adding a New Slash Command

1. Add command definition in `commands/registry.rs`:
```rust
Command::new("mycommand", "Description")
    .alias("mc")
    .handler(|ctx| {
        // Implementation
        Ok(())
    })
```

2. Register in `build_command_registry()` function

### Adding a New Modal

1. Create component in `components/my_modal.rs`
2. Add variant to `ActiveModal` enum in `app.rs`
3. Add field to `App` struct
4. Handle in `App::handle_key_event()` and `App::render()`

### Cursor Positioning

Cursor position is calculated in the widget and set via `frame.set_cursor_position()`:

```rust
// In render method
use unicode_width::UnicodeWidthStr;
let col = text[..cursor_pos].width() as u16;
frame.set_cursor_position((area.x + 1 + col, area.y + 1));
```

## Testing

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_name

# Run tests for a module
cargo test commands::
```

- `tests/pty_e2e.rs` runs the real `maestro-tui` binary in a pseudo-terminal
  against a mock OpenAI-compatible server (scripted SSE turns; no network, API
  key, or display needed) and asserts on the captured terminal output. Add
  interactive end-to-end scenarios there. The tests serialize on a shared
  mutex — concurrent TUI spinners starve the PTY reader thread and get
  keystrokes eaten by the app's per-frame cursor-position probes.

## Code Style

- Use `rustfmt` defaults
- Prefer explicit error handling over `.unwrap()`
- Document public APIs with `///` doc comments
- Keep widgets stateless when possible (pass data as constructor args)

## Dependencies

Key crates:
- `ratatui` - Terminal UI framework
- `crossterm` - Cross-platform terminal manipulation
- `unicode-width` - Proper character width calculation
- `textwrap` - Text wrapping
- `serde`/`serde_json` - JSON serialization
- `chrono` - Time handling

## Debugging

```bash
# Build with debug symbols
cargo build

# Run with RUST_BACKTRACE
RUST_BACKTRACE=1 ./target/debug/composer-tui

# Log to file (if logging is enabled)
RUST_LOG=debug ./target/debug/composer-tui 2> debug.log
```

## Advanced Architecture Patterns

### Agent Communication: Actor Model with Channels

The agent uses a **channel-based actor model** with a lightweight handle and background task:

```
┌─────────────┐                              ┌────────────────────┐
│   App/TUI   │ ── ToAgent ──► mpsc ──────► │ NativeAgentRunner  │
│             │                              │   (tokio::spawn)   │
│             │ ◄── FromAgent ── mpsc ◄──── │                    │
└─────────────┘                              └────────────────────┘
```

**Key Types** (`products/maestro/packages/runtime-rs/src/agent/protocol.rs`):
- `ToAgent`: `Prompt`, `ToolResponse`, `Cancel`, `SetModel`, `SetThinkingLevel`
- `FromAgent`: `ResponseChunk`, `ToolCall`, `ResponseEnd`, `Error`, `Ready`

**Why Unbounded Channels**: Commands are user-initiated (low volume), events are streamed but backpressure handled by TUI renderer. No risk of unbounded growth.

### Tool Execution Flow

```
Agent Request
    ↓
ToolExecutor::execute()
    ├─ Validate args against JSON Schema
    ├─ Check approval (static or dynamic analysis)
    └─ Route to implementation
        ├─ BashTool → command analysis, timeout
        ├─ ReadTool → line numbers, offset/limit
        ├─ WriteTool → create dirs, write content
        ├─ EditTool → exact string replacement
        ├─ GlobTool → pattern matching
        └─ GrepTool → ripgrep integration
    ↓
Emit events (ToolStart, ToolOutput, ToolEnd)
    ↓
Return ToolResult { success, output, error }
```

**Tool Result Caching** (`products/maestro/packages/local-host-rs/src/tools/cache.rs`):
- LRU cache for read-only tools (read, glob, grep)
- Excluded: bash, write, edit (side effects)
- Configurable TTL and max entries

### Safety Systems

**Action Firewall** (`products/maestro/packages/local-host-rs/src/safety/`):
- Path containment checks (no escaping workspace)
- Dangerous pattern detection (regex-based)
- Severity levels: Low, Medium, High, Critical
- Returns `FirewallVerdict`: Block, RequireApproval, Allow

**Doom Loop Detection** (`products/maestro/packages/runtime-rs/src/agent/safety.rs`):
- Sliding window of recent tool calls
- Detects repeated identical calls (same tool + args hash)
- Blocks if threshold exceeded (default: 3 identical)
- Prevents infinite retry loops

**Rate Limiting**:
- Per-tool rate limits with sliding time window
- Prevents token waste and API throttling

### Hooks System (`products/maestro/packages/local-host-rs/src/hooks/`)

Multi-backend extensibility system:

| Backend | Use Case | Implementation |
|---------|----------|----------------|
| **Rust traits** | Zero overhead, inline hooks | Direct trait impl |
| **Lua scripts** | Lightweight scripting | `mlua` crate |
| **WASM plugins** | Sandboxed, polyglot | WASM runtime |
| **Node.js bridge** | TypeScript hook compat | IPC subprocess |

**Hook Types**:
- `PreToolUse` / `PostToolUse` - intercept/modify tool calls
- `SessionStart` / `SessionEnd` - lifecycle hooks
- `PreMessage` / `PostMessage` - message interception
- `Overflow` - context overflow detection

### Skills System (`products/maestro/packages/local-host-rs/src/skills/`)

Dynamic capability activation without code changes:

```rust
pub struct SkillDefinition {
    pub id: String,
    pub name: String,
    pub trigger_patterns: Vec<String>,      // Auto-activation
    pub system_prompt_additions: String,    // Injected into prompt
    pub provided_tools: Vec<ToolDef>,       // Additional tools
}
```

**Activation**: Skills auto-activate via case-insensitive trigger matching on user input. Example: "frontend-design" skill activates on "build a landing page".

### Swarm Mode (`maestro-swarm`)

Multi-agent orchestration for complex tasks:

- `SwarmTask`: Individual task with dependencies
- `SwarmPlan`: Task graph with topological ordering
- `SwarmExecutor`: Parallel execution with dependency resolution

**Plan Format**: Markdown-based natural language:
```markdown
[task-1] Set up project structure
[task-2] Implement API routes (depends on: task-1)
[task-3] Add tests (depends on: task-2)
```

### Configuration Layering (`config.rs`)

Precedence (highest to lowest):
1. CLI flags (`--model`, `--config key=value`)
2. Environment variables (`MAESTRO_*`)
3. Active profile settings
4. Project config (`.maestro/config.toml`)
5. Global config (`~/.maestro/config.toml`)
6. Built-in defaults

Uses `once_cell::sync::Lazy<RwLock<MaestroConfig>>` for thread-safe global state.

### Session Format (`products/maestro/packages/session-rs/src/session/entries.rs`)

Tagged enum with serde discriminator:

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Session { id, timestamp, model, thinking_level, ... },
    Message { timestamp, message },
    ToolCall { call_id, tool, args },
    ToolResult { call_id, output, is_error },
    SessionMeta { title, tags },
    ModelChange { old_model, new_model },
    ThinkingLevelChange { level },
    Compaction { reason },
}
```

**Durability**: Append-only JSONL format. Each line is valid JSON. Crash-safe without transactions.

## Extracted Module Owners

Shared agent commands, tool results, and execution receipt values live in
`products/maestro/packages/runtime-contracts-rs/src`. Live execution and policy enforcement remain
in their existing host modules; receipt values preserve their serialized formats.

Reusable terminal primitives live in `products/maestro/packages/ui-rs/src`. Native OS enforcement lives
in `products/maestro/packages/sandbox-rs/src`, with separate `linux.rs`, `macos.rs`, and `tests.rs`
modules. File discovery, indexing, search, Git inspection, and worktree operations
live in `products/maestro/packages/workspace-rs/src`. Codex transport and thread bindings
live in `products/maestro/packages/codex-rs/src`; task graph scheduling lives in
`products/maestro/packages/swarm-rs/src`. Token accounting and compaction live in
`products/maestro/packages/context-rs/src`. Session persistence lives in
`products/maestro/packages/session-rs/src`. The native actor lives in
`products/maestro/packages/runtime-rs/src`; maestro-local-host composes its local execution host. This crate reexports the historical module paths.

Run their tests with `cargo test -p maestro-ui -p maestro-sandbox -p
maestro-workspace -p maestro-codex -p maestro-swarm -p maestro-context --locked` from the Maestro workspace. The application-owned
`sandbox_policy` module lives in maestro-local-host alongside configuration and approval policy.

## Known Issues / TODOs

1. Headless/RPC mode is routed through the native Rust entrypoint and runtime;
   keep its protocol and request-parity tests aligned with the interactive agent.
2. MCP support exists, but parity and UX with the TypeScript surfaces is still evolving
