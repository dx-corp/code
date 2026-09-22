# Maestro TUI-RS Architecture Documentation

This document provides deep technical documentation for the Maestro TUI native Rust implementation. The codebase implements a terminal-based AI coding assistant with native API integrations to Anthropic and OpenAI.

## Table of Contents

1. [Overview](#overview)
2. [High-Level Architecture](#high-level-architecture)
3. [Module Structure](#module-structure)
4. [Core Subsystems](#core-subsystems)
5. [Data Flow](#data-flow)
6. [Key Design Patterns](#key-design-patterns)
7. [Module Deep Dives](#module-deep-dives)
8. [Extension Points](#extension-points)

---

## Overview

Maestro TUI-RS is a **pure Rust implementation** of the Maestro terminal interface. Unlike the TypeScript version, this implementation:

- **No Node.js dependency**: Communicates directly with AI providers via native HTTP clients
- **Native terminal handling**: Uses `crossterm` and `ratatui` for robust terminal management
- **Async-first architecture**: Built on `tokio` for concurrent I/O operations
- **SSH-friendly**: Proper scrollback buffer management that persists across SSH sessions

### Why Rust?

1. **Native terminal scrollback**: Pushes content into terminal's scrollback buffer using ANSI scroll regions (DECSTBM)
2. **Differential rendering**: Only sends changed cells, minimizing bytes over slow connections
3. **Native performance**: Reliable terminal handling without Node.js runtime overhead
4. **Standalone binary**: Single executable with all dependencies statically linked

---

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                         main.rs (Entry Point)                        │
│  - CLI parsing (clap)                                                │
│  - Provider inference from model name                                │
│  - Environment variable setup                                        │
└─────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────┐
│                              App (app.rs)                            │
│  - Main event loop                                                   │
│  - Input handling & routing                                          │
│  - Modal management                                                  │
│  - Command execution                                                 │
│  - Agent communication                                               │
└─────────────────────────────────────────────────────────────────────┘
         │                    │                    │
         ▼                    ▼                    ▼
┌─────────────┐    ┌─────────────────┐    ┌──────────────────┐
│  AppState   │    │   NativeAgent   │    │   Components     │
│  (state.rs) │    │   (agent/)      │    │   (components/)  │
│             │    │                 │    │                  │
│ - Messages  │    │ - AI client     │    │ - ChatView       │
│ - Input     │    │ - Tool exec     │    │ - FileSearch     │
│ - UI state  │    │ - Background    │    │ - Modals         │
│ - Approvals │    │   task loop     │    │ - Selectors      │
└─────────────┘    └─────────────────┘    └──────────────────┘
                           │
           ┌───────────────┼───────────────┐
           ▼               ▼               ▼
    ┌───────────┐   ┌───────────┐   ┌───────────┐
    │ Anthropic │   │  OpenAI   │   │  Tools    │
    │  Client   │   │  Client   │   │ Registry  │
    │  (ai/)    │   │  (ai/)    │   │ (tools/)  │
    └───────────┘   └───────────┘   └───────────┘
```

---

## Module Structure

```
src/
├── main.rs              # CLI entry point, provider inference
├── lib.rs               # Library entry point, public API exports
├── app.rs               # Main application, event loop, modals
├── state.rs             # Application state, messages, UI state
│
├── agent/               # Native agent implementation
│   ├── mod.rs           # Module exports
│   ├── native.rs        # NativeAgent + NativeAgentRunner
│   └── protocol.rs      # ToAgent/FromAgent message types
│
├── ai/                  # AI provider clients
│   ├── mod.rs           # Module exports
│   ├── client.rs        # UnifiedClient abstraction
│   ├── anthropic.rs     # Claude API (Messages API)
│   ├── openai.rs        # OpenAI API (Responses API)
│   └── types.rs         # Message, Tool, StreamEvent types
│
├── tools/               # Native tool implementations
│   ├── mod.rs           # Module exports
│   ├── registry.rs      # ToolRegistry + ToolExecutor
│   └── bash.rs          # BashTool with safety checks
│
├── commands/            # Slash command system
│   ├── mod.rs           # Module exports
│   ├── types.rs         # Command, CommandContext, outputs
│   ├── registry.rs      # build_command_registry()
│   └── matcher.rs       # Fuzzy matching, tab completion
│
├── components/          # Ratatui UI widgets
│   ├── mod.rs           # Module exports
│   ├── message.rs       # ChatView, MessageWidget, StatusBar
│   ├── input.rs         # InputWidget, EditorWidget
│   ├── textarea.rs      # Multi-line text input (Codex-style)
│   ├── approval.rs      # ApprovalModal, ApprovalController
│   ├── command_palette.rs
│   ├── file_search.rs
│   ├── session_switcher.rs
│   ├── model_selector.rs
│   ├── theme_selector.rs
│   ├── layout.rs        # Layout helpers
│   ├── scroll.rs        # Scrollbar rendering
│   └── text.rs          # Text widgets
│
├── session/             # Session persistence (JSONL)
│   ├── mod.rs           # Module exports
│   ├── manager.rs       # SessionManager, listing, loading
│   ├── entries.rs       # Session entry types
│   ├── reader.rs        # JSONL reading
│   └── writer.rs        # JSONL writing
│
├── terminal/            # Terminal setup and events
│   ├── mod.rs           # Module exports
│   ├── setup.rs         # init(), restore(), capabilities
│   ├── events.rs        # TerminalEventStream
│   └── history.rs       # Scrollback buffer management
│
├── headless/            # Headless protocol (Node.js IPC)
│   ├── mod.rs           # Module exports, usage docs
│   ├── messages.rs      # ToAgentMessage, FromAgentMessage
│   ├── framing.rs       # FrameReader, FrameWriter
│   ├── transport.rs     # AgentTransport (sync)
│   ├── async_transport.rs # AsyncAgentTransport
│   ├── session.rs       # SessionRecorder, SessionReader
│   └── supervisor.rs    # AgentSupervisor with reconnection
│
├── files/               # Workspace file handling
│   ├── mod.rs           # Module exports
│   ├── workspace.rs     # get_workspace_files()
│   └── search.rs        # FileSearch, fuzzy matching
│
├── effects/             # Visual effects
│   ├── mod.rs
│   ├── spinner.rs       # Loading spinners
│   └── shimmer.rs       # Shimmer effects
│
├── themes/              # Theme system
│   └── mod.rs           # Theme definitions, set_theme_by_name()
│
├── protocol/            # Protocol utilities
│   ├── mod.rs
│   └── styles.rs        # Style definitions
│
├── markdown.rs          # Markdown → ratatui Text rendering
├── syntax.rs            # Syntax highlighting (syntect)
├── diff.rs              # Diff generation and display
├── clipboard.rs         # Clipboard operations (arboard)
├── git.rs               # Git integration (branch detection)
├── wrapping.rs          # Text wrapping utilities
├── pager.rs             # Pager component
├── palette.rs           # Color palette management
├── key_hints.rs         # Keyboard shortcut hints
└── tooltips.rs          # Random tooltip messages
```

---

## Core Subsystems

### 1. Entry Point (`main.rs`)

The entry point handles:

- **CLI parsing**: Uses `clap` derive macros for argument parsing
- **Provider inference**: Automatically detects provider from model name (e.g., `gpt-*` → OpenAI, `claude-*` → Anthropic)
- **Environment setup**: Sets API keys from CLI args to environment variables
- **Application launch**: Creates `App` instance and runs the async event loop

```rust
#[derive(Parser, Debug)]
#[command(name = "composer-tui")]
struct Args {
    #[arg(long)]
    provider: Option<String>,
    #[arg(short, long)]
    model: Option<String>,
    #[arg(long)]
    api_key: Option<String>,
    #[arg(short, long)]
    r#continue: bool,
    #[arg(short, long)]
    resume: bool,
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,
}
```

### 2. Application Core (`app.rs`)

The `App` struct is the central coordinator:

```rust
pub struct App {
    state: AppState,                    // UI state
    native_agent: Option<NativeAgent>,  // Agent handle
    native_event_rx: Option<mpsc::UnboundedReceiver<FromAgent>>,
    tool_response_tx: Option<mpsc::UnboundedSender<(String, bool, Option<ToolResult>)>>,
    tool_executor: ToolExecutor,        // Tool execution
    terminal: Terminal,                 // Ratatui terminal
    command_registry: Arc<CommandRegistry>,
    slash_matcher: SlashCommandMatcher,
    active_modal: ActiveModal,          // Current modal state
    // ... modal instances
}
```

**Key responsibilities:**

1. **Event Loop**: 50ms polling with concurrent agent message checking
2. **Input Routing**: Delegates to modal handlers or main input handler
3. **Agent Communication**: Polls events from agent, handles tool approvals
4. **Rendering**: Orchestrates widget rendering via `ChatView`

### 3. Application State (`state.rs`)

`AppState` holds all mutable application state:

```rust
pub struct AppState {
    pub messages: Vec<Message>,          // Conversation history
    pub textarea: TextArea,              // Input buffer
    pub model: Option<String>,           // Current model
    pub provider: Option<String>,        // Current provider
    pub cwd: Option<String>,             // Working directory
    pub git_branch: Option<String>,      // Git branch
    pub session_id: Option<String>,      // Session ID
    pub busy: bool,                      // Processing state
    pub busy_since: Option<Instant>,     // For elapsed time display
    pub status: Option<String>,          // Status message
    pub scroll_offset: usize,            // Message list scroll
    pub expanded_tool_calls: HashSet<String>,
    pub error: Option<String>,           // Error display
    pub thinking_header: Option<String>, // Current thinking header
    pub zen_mode: bool,                  // Minimal UI mode
    pub approval_mode: ApprovalMode,     // YOLO/Selective/Safe
}
```

**Message structure:**

```rust
pub struct Message {
    pub id: String,
    pub role: MessageRole,           // User | Assistant
    pub content: String,             // Message text
    pub thinking: String,            // Thinking/reasoning content
    pub streaming: bool,             // Still being streamed
    pub tool_calls: Vec<ToolCallState>,
    pub usage: Option<TokenUsage>,   // Token stats
    pub timestamp: SystemTime,
    pub thinking_expanded: bool,     // Toggle state
}
```

### 4. Native Agent (`agent/native.rs`)

The agent uses a **handle + background task** architecture:

```
┌──────────────┐         ┌─────────────────────┐
│ NativeAgent  │ ──TX──▶ │ NativeAgentRunner   │
│ (handle)     │ ◀──RX── │ (background task)   │
│              │         │                     │
│ - prompt()   │         │ - AI client         │
│ - cancel()   │         │ - Message history   │
│ - set_model()│         │ - Tool execution    │
└──────────────┘         │ - Streaming loop    │
                         └─────────────────────┘
```

**NativeAgent** (handle, held by App):
- Lightweight, all methods return immediately
- Sends commands via `mpsc::UnboundedSender`
- Receives events via `mpsc::UnboundedReceiver`

**NativeAgentRunner** (background task):
- Owns mutable state (conversation history, AI client)
- Runs in `tokio::spawn`
- Processes commands in a loop
- Handles tool call flow with approval

**Agent loop flow:**

```
1. Receive Prompt command
2. Add user message to history
3. Call AI API (streaming)
4. Process stream events:
   - TextDelta → emit ResponseChunk
   - ThinkingDelta → emit ResponseChunk (is_thinking=true)
   - ToolUse → validate & emit ToolCall
5. If tool calls pending:
   - Wait for TUI approval response
   - Execute tool or send denial
   - Add tool result to history
   - Loop back to step 3
6. Emit ResponseEnd
```

### 5. AI Client Abstraction (`ai/`)

**UnifiedClient** provides a provider-agnostic interface:

```rust
pub enum UnifiedClient {
    Anthropic(AnthropicClient),
    OpenAI(OpenAiClient),
}

impl UnifiedClient {
    pub fn from_model(model: &str) -> Result<Self>;
    pub fn provider(&self) -> AiProvider;
    pub async fn stream(&self, messages: &[Message], config: &RequestConfig)
        -> Result<mpsc::UnboundedReceiver<StreamEvent>>;
}
```

**StreamEvent types:**

```rust
pub enum StreamEvent {
    MessageStart { id: String, model: String },
    ContentBlockStart { index: usize, block: ContentBlock },
    TextDelta { index: usize, text: String },
    ThinkingDelta { index: usize, thinking: String },
    InputJsonDelta { index: usize, partial_json: String },
    ContentBlockStop { index: usize },
    MessageStop,
    Usage { input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens },
    Error { message: String },
}
```

**Anthropic client features:**
- Messages API with SSE streaming
- Extended thinking support (`anthropic-beta` header)
- Prompt caching support
- Tool use with streaming JSON input

**RequestConfig:**

```rust
pub struct RequestConfig {
    pub model: String,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub system: Option<String>,
    pub tools: Vec<Tool>,
    pub thinking: Option<ThinkingConfig>,
}
```

### 6. Tool System (`tools/`)

**ToolRegistry** manages tool definitions:

```rust
pub struct ToolRegistry {
    tools: HashMap<String, ToolDefinition>,
}

impl ToolRegistry {
    pub fn new() -> Self;                    // Registers default tools
    pub fn get(&self, name: &str) -> Option<&ToolDefinition>;
    pub fn missing_required(&self, name: &str, args: &Value) -> Vec<String>;
    pub fn requires_approval(&self, name: &str, args: &Value) -> bool;
}
```

**Built-in tools:**

| Tool | Description | Requires Approval |
|------|-------------|-------------------|
| `bash` | Execute shell commands | Dynamic (based on command) |
| `read` | Read file contents | No |
| `write` | Write file contents | Yes |
| `edit` | Replace text in file | Yes |
| `glob` | Find files by pattern | No |
| `grep` | Search file contents | No |

**ToolExecutor** runs tools:

```rust
impl ToolExecutor {
    pub async fn execute(
        &self,
        tool_name: &str,
        args: &Value,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
    ) -> ToolResult;
}
```

**BashTool safety:**

```rust
impl BashTool {
    /// Safe commands (read-only, no approval needed)
    const SAFE_PREFIXES: &[&str] = [
        "ls ", "cat ", "head ", "tail ", "grep ", "find ", "pwd",
        "git status", "git log", "git diff", "cargo --version", ...
    ];

    /// Dangerous commands (blocked)
    fn is_dangerous(cmd: &str) -> Option<&'static str>;

    /// Dynamic approval check
    pub fn requires_approval(command: &str) -> bool;
}
```

### 7. Command System (`commands/`)

**Command structure:**

```rust
pub struct Command {
    pub name: String,
    pub description: String,
    pub usage: String,
    pub category: CommandCategory,
    pub aliases: Vec<String>,
    pub arguments: Vec<CommandArgument>,
    pub handler: CommandHandler,
    pub is_group: bool,
    pub subcommands: Vec<String>,
}
```

**Categories:**
- `Ui` - Theme, zen mode, etc.
- `Session` - Session management
- `Tools` - Tool management
- `Safety` - Approval settings
- `Diagnostics` - Status, diag
- `Config` - Configuration
- `Navigation` - Search, files
- `Context` - Context management

**CommandOutput variants:**

```rust
pub enum CommandOutput {
    Message(String),           // Display text
    Help(String),              // Help text
    Warning(String),           // Warning (non-fatal)
    OpenModal(ModalType),      // Open a modal
    Action(CommandAction),     // Modify state
    Silent,                    // No output
    Multi(Vec<CommandOutput>), // Multiple outputs
}
```

**SlashCommandMatcher:**

- Fuzzy matching for command names
- Tab completion with cycling
- Prefix matching for quick access

### 8. Components (`components/`)

**Widget hierarchy for main UI:**

```
ChatView (main container)
├── Messages area (scrollable)
│   └── MessageWidget (per message)
│       ├── Role prefix
│       ├── Content (markdown rendered)
│       ├── Thinking (collapsible)
│       └── ToolCallWidget (per tool call)
├── ChatInputWidget
│   └── TextArea (multi-line input)
└── StatusBarWidget
    ├── Model name
    ├── Busy indicator
    ├── Git branch
    └── Approval mode
```

**Modal system:**

```rust
pub enum ActiveModal {
    None,
    FileSearch,
    SessionSwitcher,
    CommandPalette,
    Approval,
    ModelSelector,
    ThemeSelector,
}
```

Each modal has:
- `show()` / `hide()` methods
- `render(frame, area)` method
- Input handling methods (`insert_char`, `move_up`, etc.)
- `confirm()` to get selection

**TextArea (Codex-style):**

Multi-line text input with:
- Cursor positioning
- Shift+Enter for newlines
- Unicode-aware navigation
- Line wrapping display

### 9. Session Management (`session/`)

**JSONL format:**

Sessions are stored as newline-delimited JSON in `~/.maestro/agent/sessions/`:

```json
{"type": "user", "content": "Hello", "timestamp": "2025-..."}
{"type": "assistant", "content": "Hi!", "timestamp": "2025-..."}
{"type": "tool_result", "tool_use_id": "...", "content": "..."}
```

**SessionManager:**

```rust
impl SessionManager {
    pub fn list_sessions(&self) -> Vec<SessionInfo>;
    pub fn load_session(&self, id: &str) -> Result<Session>;
    pub fn delete_session(&self, id: &str) -> Result<()>;
}
```

**ThinkingLevel:**

```rust
pub enum ThinkingLevel {
    Off,      // No thinking
    Minimal,  // 1,024 tokens
    Low,      // 4,096 tokens
    Medium,   // 10,000 tokens
    High,     // 20,000 tokens
    XHigh,    // 32,000 tokens
    Max,      // 50,000 tokens
}
```

### 10. Terminal Handling (`terminal/`)

**Initialization:**

```rust
pub fn init() -> Result<(Terminal, TerminalCapabilities)>;
pub fn restore() -> Result<()>;
```

**TerminalCapabilities:**

```rust
pub struct TerminalCapabilities {
    pub viewport_top: u16,      // Scrollback position
    pub viewport_height: u16,   // Terminal height
    pub supports_true_color: bool,
    pub supports_unicode: bool,
}
```

**Scrollback management:**

Uses ANSI scroll regions (DECSTBM) to push content into terminal scrollback buffer, ensuring history persists across SSH sessions.

### 11. Headless Protocol (`headless/`)

For Node.js agent communication (alternative architecture):

```rust
// Layers of abstraction:
// 1. Messages - ToAgentMessage, FromAgentMessage
// 2. Framing - Newline-delimited or length-prefixed
// 3. Transport - Subprocess stdin/stdout
// 4. Session - JSONL recording
// 5. Supervisor - Reconnection, health monitoring

let mut transport = AsyncAgentTransportBuilder::new()
    .cli_path("composer")
    .cwd("/path/to/project")
    .spawn()
    .await?;

transport.prompt("Hello!")?;
while let Ok(event) = transport.recv().await {
    // Handle events
}
```

---

## Data Flow

### User Input → AI Response

```
1. User types message
   └─▶ App::handle_key()
       └─▶ AppState::insert_char()

2. User presses Enter
   └─▶ App::submit_prompt()
       ├─▶ AppState::add_user_message()
       └─▶ NativeAgent::prompt()
           └─▶ command_tx.send(AgentCommand::Prompt)

3. Agent processes prompt
   └─▶ NativeAgentRunner::run_loop()
       ├─▶ UnifiedClient::stream()
       │   └─▶ AnthropicClient::stream_impl()
       │       └─▶ HTTP POST to API
       └─▶ Process StreamEvents
           └─▶ event_tx.send(FromAgent::ResponseChunk)

4. App receives events
   └─▶ App::poll_agent()
       └─▶ App::handle_agent_message()
           └─▶ AppState::handle_agent_message()
               └─▶ Update Message content

5. UI updates
   └─▶ App::render()
       └─▶ ChatView::render()
           └─▶ MessageWidget::render()
```

### Tool Execution Flow

```
1. Agent emits ToolCall
   └─▶ FromAgent::ToolCall { call_id, tool, args, requires_approval }

2. App receives tool call
   └─▶ App::handle_agent_message()
       ├─▶ Check approval mode
       └─▶ If requires_approval:
           ├─▶ ApprovalController::enqueue()
           └─▶ active_modal = ActiveModal::Approval

3. User approves (y/Y/Enter)
   └─▶ App::handle_approval_key()
       └─▶ App::handle_tool_approval(approved=true)
           └─▶ ToolExecutor::execute()
               └─▶ BashTool::execute() or file ops

4. Result sent back
   └─▶ tool_response_tx.send((call_id, true, Some(result)))

5. Agent continues
   └─▶ NativeAgentRunner receives tool result
       └─▶ Add ToolResult to history
       └─▶ Continue run_loop()
```

---

## Key Design Patterns

### 1. Handle + Background Task

The agent uses a **handle pattern** where `NativeAgent` is a lightweight handle that communicates with a background task via channels:

```rust
// Handle (lightweight, cloneable conceptually)
pub struct NativeAgent {
    command_tx: mpsc::UnboundedSender<AgentCommand>,
    event_tx: mpsc::UnboundedSender<FromAgent>,
    // ...
}

// Background task (owns mutable state)
struct NativeAgentRunner {
    client: UnifiedClient,
    messages: Vec<Message>,
    // ...
}
```

**Benefits:**
- Non-blocking API calls
- Clear ownership of mutable state
- Graceful cancellation via CancellationToken

### 2. Enum-Based Polymorphism

Provider abstraction uses enums instead of trait objects:

```rust
pub enum UnifiedClient {
    Anthropic(AnthropicClient),
    OpenAI(OpenAiClient),
}

impl UnifiedClient {
    pub async fn stream(&self, ...) {
        match self {
            Self::Anthropic(c) => c.stream(...).await,
            Self::OpenAI(c) => c.stream(...).await,
        }
    }
}
```

**Benefits:**
- No dynamic dispatch overhead
- Pattern matching for exhaustive handling
- Easy to extend with new variants

### 3. Event Sourcing for State

Agent communication uses event sourcing:

```rust
pub enum FromAgent {
    ResponseStart { response_id },
    ResponseChunk { response_id, content, is_thinking },
    ResponseEnd { response_id, usage },
    ToolCall { call_id, tool, args, requires_approval },
    ToolStart { call_id },
    ToolOutput { call_id, content },
    ToolEnd { call_id, success },
    Error { message, fatal },
    Status { message },
    SessionInfo { session_id, cwd, git_branch },
}
```

**Benefits:**
- Clear state transitions
- Easy to replay/record sessions
- Decoupled UI updates

### 4. Modal State Machine

UI modals form a state machine:

```rust
pub enum ActiveModal {
    None,
    FileSearch,
    SessionSwitcher,
    CommandPalette,
    Approval,
    ModelSelector,
    ThemeSelector,
}
```

Each modal state has:
- Dedicated key handler
- Dedicated render method
- Entry/exit actions

### 5. Layered Abstractions

The headless module demonstrates layered design:

```
┌──────────────┐
│  Supervisor  │  Health monitoring, reconnection
├──────────────┤
│  Transport   │  Subprocess management
├──────────────┤
│   Framing    │  Message boundaries
├──────────────┤
│   Messages   │  Type definitions
└──────────────┘
```

---

## Module Deep Dives

### AI Module (`ai/`)

**File: `ai/anthropic.rs`**

SSE stream parsing:

```rust
fn parse_sse_event(data: &str) -> Option<StreamEvent> {
    // Parse event type and data
    let event_type = ...;  // From "event: " line
    let event_data = ...;  // From "data: " line

    match event_type {
        "message_start" => ...,
        "content_block_start" => ...,
        "content_block_delta" => ...,
        "message_delta" => ...,
        "message_stop" => ...,
        _ => None,
    }
}
```

**File: `ai/types.rs`**

Core types for API communication:

```rust
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

pub enum ContentBlock {
    Text { text: String },
    Image { source: ImageSource },
    ToolUse { id, name, input },
    ToolResult { tool_use_id, content, is_error },
    Thinking { thinking: String },
}
```

### Tool Module (`tools/`)

**File: `tools/registry.rs`**

Tool JSON schema validation:

```rust
pub fn missing_required(&self, name: &str, args: &Value) -> Vec<String> {
    let def = self.tools.get(&name.to_lowercase())?;
    let required = def.tool.input_schema["required"].as_array()?;

    required.iter()
        .filter_map(|f| f.as_str())
        .filter(|field| !args.get(field).is_some())
        .map(String::from)
        .collect()
}
```

**File: `tools/bash.rs`**

Command execution with safety:

```rust
pub async fn execute(&self, args: BashArgs) -> ToolResult {
    // 1. Reject empty commands
    if args.command.trim().is_empty() { return error; }

    // 2. Check for dangerous patterns
    if let Some(warning) = Self::is_dangerous(&args.command) {
        return error;
    }

    // 3. Execute with timeout
    let child = Command::new(&self.shell)
        .arg("-c")
        .arg(&args.command)
        .current_dir(&self.cwd)
        .spawn()?;

    // 4. Handle timeout/output
    match timeout(Duration::from_millis(timeout_ms), wait_and_read).await {
        Ok((stdout, stderr, status)) => ...,
        Err(_) => { child.kill(); timeout_error; }
    }
}
```

### Component Module (`components/`)

**File: `components/message.rs`**

Main chat view layout:

```rust
impl Widget for ChatView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let chunks = Layout::vertical([
            Constraint::Min(3),     // Messages
            Constraint::Length(3),  // Input
            Constraint::Length(1),  // Status
        ]).split(area);

        // Render messages with scroll
        self.render_messages(chunks[0], buf);

        // Render input
        ChatInputWidget::new(...).render(chunks[1], buf);

        // Render status bar
        StatusBarWidget::new(...).render(chunks[2], buf);
    }
}
```

**File: `components/textarea.rs`**

Multi-line input (Codex-inspired):

```rust
pub struct TextArea {
    text: String,
    cursor: usize,  // Byte offset
}

impl TextArea {
    pub fn insert_char(&mut self, c: char);
    pub fn delete_char(&mut self);
    pub fn move_left(&mut self);
    pub fn move_right(&mut self);
    pub fn cursor_line_col(&self) -> (usize, usize);
}
```

### Markdown Module (`markdown.rs`)

Pulldown-cmark to ratatui conversion:

```rust
struct MarkdownRenderer {
    styles: MarkdownStyles,
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<Option<u64>>,
    in_code_block: bool,
    code_block_content: String,
    code_block_lang: Option<String>,
}

impl MarkdownRenderer {
    fn render(&mut self, parser: Parser) {
        for event in parser {
            match event {
                Event::Start(tag) => self.start_tag(tag),
                Event::End(tag) => self.end_tag(tag),
                Event::Text(text) => self.add_text(&text),
                Event::Code(code) => self.add_inline_code(&code),
                // ...
            }
        }
    }
}
```

Code blocks get syntax highlighting via `syntect`:

```rust
fn end_tag(&mut self, tag: TagEnd) {
    if matches!(tag, TagEnd::CodeBlock) {
        let highlighted = syntax::highlight_code(
            &self.code_block_content,
            self.code_block_lang.as_deref(),
        );
        // Add with border
    }
}
```

---

## Extension Points

### Adding a New AI Provider

1. Create `ai/newprovider.rs`:

```rust
pub struct NewProviderClient {
    client: reqwest::Client,
    api_key: String,
}

impl NewProviderClient {
    pub fn from_env() -> Result<Self>;

    pub async fn stream(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>>;
}
```

2. Add to `UnifiedClient` enum in `ai/client.rs`:

```rust
pub enum UnifiedClient {
    Anthropic(AnthropicClient),
    OpenAI(OpenAiClient),
    NewProvider(NewProviderClient),  // Add variant
}
```

3. Update `AiProvider::from_model()` for model detection

### Adding a New Tool

1. Add to `ToolRegistry::new()` in `tools/registry.rs`:

```rust
tools.insert(
    "mytool".to_string(),
    ToolDefinition {
        tool: Tool::new("mytool", "Description")
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": { ... },
                "required": [...]
            })),
        requires_approval: true,
    },
);
```

2. Add execution logic to `ToolExecutor::execute()`:

```rust
"mytool" | "MyTool" => {
    // Parse args, execute, return ToolResult
}
```

### Adding a New Slash Command

1. Define handler in `commands/registry.rs`:

```rust
fn handle_mycommand(ctx: &CommandContext) -> CommandResult {
    let arg = ctx.get_string("arg").unwrap_or("default");
    Ok(CommandOutput::Message(format!("Result: {}", arg)))
}
```

2. Register in `build_command_registry()`:

```rust
let mycommand = Command::new(
    "mycommand",
    "Description",
    CommandCategory::Tools,
    Box::new(handle_mycommand),
)
.alias("mc")
.arg(CommandArgument::string("arg", "Argument description"));

registry.register(mycommand);
```

### Adding a New Modal

1. Create widget in `components/mymodal.rs`:

```rust
pub struct MyModal {
    visible: bool,
    // state...
}

impl MyModal {
    pub fn new() -> Self;
    pub fn show(&mut self);
    pub fn hide(&mut self);
    pub fn render(&self, frame: &mut Frame, area: Rect);
    pub fn handle_key(&mut self, key: KeyCode) -> Option<Selection>;
}
```

2. Add to `ActiveModal` enum in `app.rs`
3. Add modal instance to `App` struct
4. Add key handler method to `App`
5. Add render call in `App::render()`

---

## Testing

Tests are co-located with modules. Run with:

```bash
cargo test
```

Test categories:
- **Unit tests**: Module-level tests in `#[cfg(test)]` blocks
- **Integration tests**: In `tests/` directory (if any)
- **Property tests**: Using `proptest` for fuzzy matching, etc.

Example test pattern:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_requires_approval_safe_commands() {
        assert!(!BashTool::requires_approval("ls -la"));
        assert!(!BashTool::requires_approval("git status"));
    }

    #[tokio::test]
    async fn test_execute_echo() {
        let tool = BashTool::new(".");
        let result = tool.execute(BashArgs {
            command: "echo hello".to_string(),
            ..Default::default()
        }).await;
        assert!(result.success);
    }
}
```

---

## Performance Considerations

1. **Streaming**: All AI responses are streamed, updating UI in real-time
2. **Differential rendering**: Ratatui only redraws changed cells
3. **Async I/O**: All blocking operations use `tokio` async
4. **Memory**: Messages are kept in memory; large conversations should use `/compact`
5. **Binary size**: Release builds use LTO and stripping for minimal size

Build profile:

```toml
[profile.release]
lto = true
codegen-units = 1
strip = true
```

---

## Security Model

### Tool Approval Modes

- **YOLO**: Auto-approve all tool calls (use only in trusted environments)
- **Selective**: Approve based on tool/command risk (default)
- **Safe**: Always require approval for all tool calls

### Bash Safety

1. **Safe prefixes**: Read-only commands auto-approved
2. **Dangerous patterns**: Blocked entirely (e.g., `rm -rf /`)
3. **Dynamic check**: Commands analyzed at runtime

### Workspace Containment

Write operations are checked against workspace boundaries (future enhancement).

---

## Configuration

### Environment Variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `OPENAI_API_KEY` | OpenAI API key |
| `MAESTRO_MODEL` | Override default model |
| `SHELL` | Shell for bash tool (default: `/bin/bash`, fallback: `/bin/sh`) |

### Runtime Configuration

Via slash commands:
- `/model <name>` - Change model
- `/thinking <level>` - Set thinking level
- `/approvals` - Cycle approval mode
- `/theme <name>` - Change theme
- `/zen` - Toggle zen mode

---

## Credits

- **OpenAI Codex**: TextArea and cursor positioning adapted from [OpenAI Codex](https://github.com/openai/codex) (MIT License)
- **Ratatui**: Terminal UI framework
- **Crossterm**: Cross-platform terminal handling
- **Syntect**: Syntax highlighting
- **Pulldown-cmark**: Markdown parsing
