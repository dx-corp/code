# Session Recovery & Persistence Design

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. Session persistence now lives in `packages/session-rs/src/session/`. Some file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.


The session management system enables conversation persistence, crash recovery, and session branching. Sessions are stored as JSONL files with buffered writes and atomic operations.

## Overview

Key capabilities:

- **JSONL Storage**: Human-readable, append-only format
- **Buffered Writing**: Batched I/O for performance
- **Crash Recovery**: Process exit handlers ensure no data loss
- **Session Branching**: Create new sessions from any point in history
- **Metadata Caching**: In-memory tracking of current model/settings
- **Lazy Initialization**: Session files created only when needed

## Storage Architecture

```
~/.maestro/agent/sessions/
└── --home-user-projects-myapp--/
    ├── 2024-01-15T10-30-00-000Z_uuid1.jsonl
    ├── 2024-01-15T14-45-00-000Z_uuid2.jsonl
    └── ...
```

### Directory Naming

The directory uses a sanitized version of the working directory:

```typescript
// src/session/manager.ts:611-627
private getSessionDirectory(): string {
  const cwd = process.cwd();
  // Replace path separators and drive letters with dashes
  const safePath = `--${cwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`;

  const configDir = resolve(
    process.env.MAESTRO_AGENT_DIR ??
    process.env.PLAYWRIGHT_AGENT_DIR ??
    process.env.CODING_AGENT_DIR ??
    join(homedir(), ".maestro/agent/")
  );

  return join(configDir, "sessions", safePath);
}
```

## JSONL Format

Each line is a JSON object representing an entry:

```jsonl
{"type":"session","id":"uuid","timestamp":"...","cwd":"/path","model":"anthropic/claude-opus-4-6"}
{"type":"message","timestamp":"...","message":{"role":"user","content":"Hello"}}
{"type":"message","timestamp":"...","message":{"role":"assistant","content":[...]}}
{"type":"thinking_level_change","timestamp":"...","thinkingLevel":"high"}
{"type":"model_change","timestamp":"...","model":"openai/gpt-4o"}
{"type":"session_meta","timestamp":"...","summary":"Discussed project setup","favorite":true}
```

## Entry Types

| Type | Purpose | Key Fields |
|------|---------|------------|
| `session` | Session header | `id`, `cwd`, `model`, `tools`, `thinkingLevel` |
| `message` | User/assistant message | `message` (AppMessage) |
| `thinking_level_change` | Thinking level changed | `thinkingLevel` |
| `model_change` | Model switched | `model`, `modelMetadata` |
| `session_meta` | Metadata update | `summary`, `title`, `tags`, `favorite` |

## Session Header Entry

```typescript
// src/session/manager.ts:712-726
interface SessionHeaderEntry {
  type: "session";
  id: string;
  timestamp: string;
  cwd: string;
  model: string;                    // "provider/modelId"
  modelMetadata?: SessionModelMetadata;
  thinkingLevel?: string;
  systemPrompt?: string;
  tools?: Array<{
    name: string;
    label: string;
    description: string;
  }>;
}
```

## Buffered Writing

### SessionFileWriter Class

```typescript
// src/session/manager.ts:128-258
class SessionFileWriter {
  // Global registry of all active writers
  private static readonly writers = new Set<SessionFileWriter>();
  private static beforeExitRegistered = false;

  private buffer: string[] = [];

  constructor(
    private readonly filePath: string,
    private readonly batchSize = SESSION_CONFIG.WRITE_BATCH_SIZE
  ) {
    SessionFileWriter.registerBeforeExit();
    SessionFileWriter.writers.add(this);
  }

  write(entry: SessionEntry): void {
    this.buffer.push(JSON.stringify(entry));
    if (this.buffer.length >= this.batchSize) {
      this.flushSync();
    }
  }

  flushSync(): void {
    const chunk = this.drainBuffer();
    if (chunk) {
      appendFileSync(this.filePath, chunk);
    }
  }
}
```

### Process Exit Handlers

```typescript
// src/session/manager.ts:157-191
private static registerBeforeExit(): void {
  if (SessionFileWriter.beforeExitRegistered) return;
  SessionFileWriter.beforeExitRegistered = true;

  const flushAll = (signal?: string) => {
    for (const writer of SessionFileWriter.writers) {
      try {
        writer.flushSync();
      } catch (error) {
        logger.error("Failed to flush session file on exit", error);
      }
    }
  };

  // Register handlers for various exit scenarios
  process.once("beforeExit", () => flushAll());
  process.once("SIGINT", () => {
    flushAll("SIGINT");
    process.exit();
  });
  process.once("SIGTERM", () => {
    flushAll("SIGTERM");
    process.exit();
  });
  process.once("uncaughtException", () => flushAll("uncaughtException"));
  process.once("unhandledRejection", () => flushAll("unhandledRejection"));
}
```

## Metadata Cache

Tracks current model and thinking level without re-reading files:

```typescript
// src/session/manager.ts:270-327
class SessionMetadataCache {
  private thinkingLevel = "off";
  private model: string | null = null;
  private metadata?: SessionModelMetadata;

  apply(entry: SessionEntry): void {
    if (entry.type === "session") {
      if (entry.thinkingLevel) this.thinkingLevel = entry.thinkingLevel;
      if (entry.model) this.model = entry.model;
      if (entry.modelMetadata) this.metadata = entry.modelMetadata;
    }
    if (entry.type === "thinking_level_change") {
      this.thinkingLevel = entry.thinkingLevel;
    }
    if (entry.type === "model_change") {
      if (entry.model) this.model = entry.model;
      if (entry.modelMetadata) this.metadata = entry.modelMetadata;
    }
  }

  seedFromFile(filePath: string): void {
    const entries = safeReadSessionEntries(filePath);
    for (const entry of entries) {
      this.apply(entry);
    }
  }
}
```

## Session Lifecycle

```
┌─────────────────────────────────────────────────────────────────┐
│                    Session Lifecycle                             │
│                                                                  │
│  ┌──────────────────┐                                           │
│  │ Pre-initialization│  Messages queued in memory               │
│  │ (no file yet)     │  Session ID generated                    │
│  └─────────┬─────────┘                                          │
│            │                                                     │
│            │  shouldInitializeSession() returns true            │
│            │  (1 user + 1 assistant message)                    │
│            ▼                                                     │
│  ┌──────────────────┐                                           │
│  │  Initialization  │  Session file created                     │
│  │                  │  Header entry written                     │
│  │                  │  Pending messages flushed                 │
│  └─────────┬────────┘                                           │
│            │                                                     │
│            ▼                                                     │
│  ┌──────────────────┐                                           │
│  │     Active       │  Messages written directly                │
│  │                  │  Model/thinking changes recorded          │
│  │                  │  Metadata updates appended                │
│  └─────────┬────────┘                                           │
│            │                                                     │
│            │  User exits or starts new session                  │
│            ▼                                                     │
│  ┌──────────────────┐                                           │
│  │   Completed      │  File remains for future resumption       │
│  │                  │  Can be loaded/branched anytime           │
│  └──────────────────┘                                           │
└─────────────────────────────────────────────────────────────────┘
```

## Session Manager

### Constructor

```typescript
// src/session/manager.ts:567-592
constructor(continueSession = false, customSessionPath?: string) {
  this.sessionDir = this.getSessionDirectory();

  if (customSessionPath) {
    // Use specific session file
    this.sessionFile = resolve(customSessionPath);
    this.loadSessionId();
    this.sessionInitialized = existsSync(this.sessionFile);
  } else if (continueSession) {
    // Load most recent session
    const mostRecent = this.findMostRecentlyModifiedSession();
    if (mostRecent) {
      this.sessionFile = mostRecent;
      this.loadSessionId();
      this.sessionInitialized = true;
    } else {
      this.initNewSession();
    }
  } else {
    this.initNewSession();
  }

  this.initializeWriter();
  this.metadataCache.seedFromFile(this.sessionFile);
}
```

### Message Saving

```typescript
// src/session/manager.ts:738-747
saveMessage(message: AppMessage): void {
  if (!this.enabled) return;

  const entry: SessionMessageEntry = {
    type: "message",
    timestamp: new Date().toISOString(),
    message
  };

  this.queueEntry(entry);
}

private queueEntry(entry: PendingSessionEntry): void {
  if (!this.sessionInitialized) {
    this.pendingMessages.push(entry);
    return;
  }
  this.writer?.write(entry);
}
```

### Session Branching

Create a new session from a specific point in history:

```typescript
// src/session/manager.ts:1010-1072
createBranchedSession(state: AgentState, branchFromIndex: number): string {
  // Validate bounds
  if (branchFromIndex < 0 || branchFromIndex > state.messages.length) {
    throw new Error(`Invalid branchFromIndex: ${branchFromIndex}`);
  }

  // Create new session
  const newSessionId = uuidv4();
  const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
  const newSessionFile = join(this.sessionDir, `${timestamp}_${newSessionId}.jsonl`);
  const tempFile = `${newSessionFile}.tmp`;

  try {
    // Write header
    const entry: SessionHeaderEntry = {
      type: "session",
      id: newSessionId,
      timestamp: new Date().toISOString(),
      cwd: process.cwd(),
      model: `${state.model.provider}/${state.model.id}`,
      thinkingLevel: state.thinkingLevel
    };
    appendFileSync(tempFile, `${JSON.stringify(entry)}\n`);

    // Write messages up to branch point
    for (const message of state.messages.slice(0, branchFromIndex)) {
      const messageEntry: SessionMessageEntry = {
        type: "message",
        timestamp: new Date().toISOString(),
        message
      };
      appendFileSync(tempFile, `${JSON.stringify(messageEntry)}\n`);
    }

    // Atomic rename
    renameSync(tempFile, newSessionFile);
  } catch (error) {
    // Cleanup temp file on failure
    try {
      if (existsSync(tempFile)) unlinkSync(tempFile);
    } catch {}
    throw error;
  }

  return newSessionFile;
}
```

## Session Loading

### Load All Sessions

```typescript
// src/session/manager.ts:910-967
loadAllSessions(): SessionMetadata[] {
  this.writer?.flushSync();  // Ensure current session is up to date
  const sessions: SessionMetadata[] = [];

  const files = readdirSync(this.sessionDir)
    .filter(f => f.endsWith(".jsonl"))
    .map(f => ({
      path: join(this.sessionDir, f),
      stats: statSync(join(this.sessionDir, f))
    }))
    .sort((a, b) => b.stats.mtime.getTime() - a.stats.mtime.getTime());

  for (const { path, stats } of files) {
    const entries = safeReadSessionEntries(path);
    const info = buildSessionFileInfo(entries, stats);
    if (!info) continue;

    sessions.push({
      path,
      id: info.id,
      created: info.created,
      modified: stats.mtime,
      size: stats.size,
      messageCount: info.messageCount,
      firstMessage: info.firstMessage || "(no messages)",
      summary: info.summary || info.firstMessage || "(no summary)",
      favorite: info.favorite,
      allMessagesText: info.allMessagesText
    });
  }

  return sessions;
}
```

### Load Messages

```typescript
// src/session/manager.ts:865-874
loadMessages(): AppMessage[] {
  this.writer?.flushSync();
  const entries = safeReadSessionEntries(this.sessionFile);

  return entries
    .filter((entry): entry is SessionMessageEntry =>
      entry.type === "message" && Boolean(entry.message)
    )
    .map(entry => entry.message as AppMessage);
}
```

## Session Metadata

### Setting Metadata

```typescript
// src/session/manager.ts:842-863
saveSessionSummary(summary: string, sessionPath?: string): void {
  const target = sessionPath ?? this.sessionFile;
  if (!target || !existsSync(target)) return;
  this.appendSessionMetaEntry(target, { summary: summary.trim() });
}

setSessionFavorite(sessionPath: string, favorite: boolean): void {
  if (!sessionPath || !existsSync(sessionPath)) return;
  this.appendSessionMetaEntry(sessionPath, { favorite });
}

setSessionTitle(sessionPath: string, title: string): void {
  if (!sessionPath || !existsSync(sessionPath)) return;
  this.appendSessionMetaEntry(sessionPath, { title });
}

setSessionTags(sessionPath: string, tags: string[]): void {
  if (!sessionPath || !existsSync(sessionPath)) return;
  this.appendSessionMetaEntry(sessionPath, { tags });
}
```

## Model Metadata

```typescript
// src/session/manager.ts:449-459
interface SessionModelMetadata {
  provider: string;
  modelId: string;
  providerName?: string;
  name?: string;
  baseUrl?: string;
  reasoning?: boolean;
  contextWindow?: number;
  maxTokens?: number;
  source?: "builtin" | "custom";
}
```

## Initialization Check

```typescript
// src/session/manager.ts:995-1002
shouldInitializeSession(messages: AppMessage[]): boolean {
  if (this.sessionInitialized) return false;

  const userMessages = messages.filter(m => m.role === "user");
  const assistantMessages = messages.filter(m => m.role === "assistant");

  // Initialize when we have at least 1 user + 1 assistant message
  return userMessages.length >= 1 && assistantMessages.length >= 1;
}
```

## Session Operations

### Start Fresh Session

```typescript
// src/session/manager.ts:638-649
startFreshSession(): void {
  if (!this.enabled) return;

  this.writer?.flushSync();
  this.writer?.dispose();
  this.writer = undefined;
  this.pendingMessages = [];
  this.sessionInitialized = false;
  this.metadataCache = new SessionMetadataCache();

  this.initNewSession();
  this.initializeWriter();
}
```

### Reset for /clear

```typescript
// src/session/manager.ts:654-671
reset(): void {
  this.writer?.flushSync();
  this.writer?.dispose();
  this.writer = undefined;

  this.pendingMessages = [];
  this.sessionInitialized = false;
  this.metadataCache = new SessionMetadataCache();
  this.agentSnapshot = undefined;
  this.lastModelMetadata = undefined;

  this.initNewSession();
  this.initializeWriter();
}
```

### Disable Sessions

```typescript
// src/session/manager.ts:594-600
disable(): void {
  this.enabled = false;
  this.writer?.flushSync();
  this.writer?.dispose();
  this.writer = undefined;
  this.pendingMessages = [];
}
```

## Error Handling

### Safe Entry Parsing

```typescript
// src/session/manager.ts:361-371
function safeReadSessionEntries(
  filePath: string,
  onError?: (error: unknown) => void
): SessionEntry[] {
  try {
    return readSessionEntries(filePath);
  } catch (error) {
    onError?.(error);
    return [];
  }
}
```

### Transactional Writes

Branch operations use temp files with atomic rename:

```typescript
try {
  appendFileSync(tempFile, content);
  renameSync(tempFile, finalFile);  // Atomic
} catch (error) {
  try {
    if (existsSync(tempFile)) unlinkSync(tempFile);
  } catch {}
  throw error;
}
```

## Performance Considerations

1. **Buffered Writes**: Batch multiple entries before disk I/O
2. **Sync I/O**: Uses sync I/O to prevent race conditions
3. **Lazy Init**: No file created until first message exchange
4. **Metadata Cache**: Avoids re-reading files for settings
5. **Sorted Reads**: Sessions sorted by modification time

## Related Documentation

- [Agent State Machine](AGENT_STATE_MACHINE.md) - How sessions integrate with agent
- [Database & Persistence](DATABASE_PERSISTENCE.md) - Enterprise session storage
- [TUI Rendering](TUI_RENDERING.md) - Session UI components
