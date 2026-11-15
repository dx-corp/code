# Tool System Architecture

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. The tool system now lives in `packages/local-host-rs/src/tools/`. Some file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.


The tool system is the largest module in Maestro (~335KB), providing a framework for defining, validating, executing, and caching tool operations that the LLM can invoke.

## Overview

Tools are discrete operations the LLM can request during a conversation. The tool system handles:

- **Tool Definition**: DSL for declaring tools with schemas and handlers
- **Input Validation**: JSON Schema validation via AJV
- **Execution**: Async execution with abort support
- **Caching**: LRU cache with git-aware invalidation
- **Error Handling**: Structured errors with retry support
- **Profiles**: Curated model-visible subsets layered over the full registry

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Tool System                                  │
│                                                                      │
│  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐  │
│  │   Tool DSL       │  │   Tool Registry  │  │   Tool Cache     │  │
│  │  - createTool()  │  │  - Built-in      │  │  - LRU eviction  │  │
│  │  - createText()  │  │  - MCP tools     │  │  - Git SHA track │  │
│  │  - createJson()  │  │  - Custom tools  │  │  - Invalidation  │  │
│  └────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘  │
│           │                     │                      │            │
│           ▼                     ▼                      ▼            │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                     Tool Executor                            │   │
│  │  - Schema validation (AJV)                                   │   │
│  │  - Abort signal handling                                     │   │
│  │  - Retry with exponential backoff                            │   │
│  │  - Sandbox integration                                       │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
└──────────────────────────────┼──────────────────────────────────────┘
                               ▼
                    ┌────────────────────┐
                    │   Tool Results     │
                    │  - Text content    │
                    │  - Image content   │
                    │  - Error responses │
                    │  - Details/metadata│
                    └────────────────────┘
```

## Model-visible profiles

The full registry remains the dispatch and explicit-selection surface. Provider
adapters can expose a smaller model-visible subset when a runtime benefits from a
focused tool vocabulary. The OpenAI Codex app-server adapter currently exposes
the `lean` profile by default, accepts `MAESTRO_CODEX_TOOL_PROFILE=read-only` or
`MAESTRO_CODEX_TOOL_PROFILE=extended`, and still lets `--tools` override the
profile entirely.

File mutation tools are intentionally not grouped behind one generic tool:
`apply_patch`, `edit`, and `write` stay separate so policy classification,
approval copy, receipts, and model planning can distinguish patch application,
structured find/replace, and whole-file creation or overwrite. This mirrors the
small explicit-tool pattern used by coding-agent systems such as Pi while
preserving Maestro's richer registry for opt-in flows.

## Tool Definition DSL

### createTool()

The base function for defining tools with full control over output format:

```typescript
// src/tools/tool-dsl.ts:131-217
const myTool = createTool({
  name: "my_tool",
  label: "My Tool",
  description: "Does something useful",
  schema: Type.Object({
    input: Type.String(),
    count: Type.Optional(Type.Number({ default: 10 }))
  }),
  annotations: {
    readOnlyHint: true,
    destructiveHint: false
  },
  maxRetries: 3,
  retryDelayMs: 1000,
  shouldRetry: (error) => error instanceof NetworkError,
  run: async (params, context) => {
    // Access abort signal
    if (context.signal?.aborted) {
      throw new Error("Operation aborted");
    }

    // Build response
    return context.respond
      .text("Result: " + params.input)
      .detail({ processed: true });
  }
});
```

### createTextTool()

Simplified variant for tools that return plain text:

```typescript
// src/tools/tool-dsl.ts:261-274
const readFile = createTextTool({
  name: "read",
  description: "Read file contents",
  schema: Type.Object({
    file_path: Type.String()
  }),
  run: async ({ file_path }) => {
    return await fs.readFile(file_path, "utf8");
  }
});
```

### createJsonTool()

Variant for tools that return structured JSON:

```typescript
// src/tools/tool-dsl.ts:294-312
const searchFiles = createJsonTool({
  name: "search",
  description: "Search for files",
  schema: Type.Object({
    pattern: Type.String()
  }),
  run: async ({ pattern }) => {
    return { files: await glob(pattern) };
  }
});
```

### Path expansion helper

`expandUserPath` lives in `src/utils/path-validation.ts` and is re-exported by the tool DSL. Use it instead of ad-hoc `~` handling to keep behavior consistent:

```typescript
import { expandUserPath } from "../../src/tools/tool-dsl.js";

const absolute = expandUserPath("~/projects/my-app");
```

## Tool Response Builder

The `ToolResponseBuilder` provides a fluent API for constructing tool results:

```typescript
// src/tools/tool-dsl.ts:50-86
class ToolResponseBuilder<Details> {
  // Add text content
  text(content: string): this;

  // Add image content (base64)
  image(base64: string, mimeType: string): this;

  // Add structured details
  detail(details: Details): this;

  // Mark response as error
  error(message: string): this;

  // Build final result
  build(): AgentToolResult<Details>;
}
```

## Schema Validation

Tools use AJV for JSON Schema validation with TypeBox type definitions:

```typescript
// src/tools/tool-dsl.ts:13-48
// Singleton AJV instance with format validation
const ajv = new Ajv({
  allErrors: false,      // Stop at first error for performance
  strict: false,         // Allow unknown keywords
  useDefaults: true      // Apply schema defaults
});
addFormats(ajv);

// Validator cache (WeakMap for automatic GC)
const validatorCache = new WeakMap<TSchema, ValidatorFn>();

function getOrCompileValidator(schema: TSchema): ValidatorFn | null {
  let validate = validatorCache.get(schema);
  if (!validate) {
    validate = ajv.compile(schema);
    validatorCache.set(schema, validate);
  }
  return validate;
}
```

### Validation Error Format

```typescript
// src/tools/tool-dsl.ts:156-173
throw new ToolError(
  `Validation failed for tool "${options.name}":\n${errors}`,
  "VALIDATION_ERROR",
  { params }
);

// Error message example:
// Validation failed for tool "write":
//   - file_path: must be string
//   - content: is required
```

## Execution Flow

```
Tool Call Received
        │
        ▼
┌───────────────────┐
│ Schema Validation │ ─── Invalid ──→ Return ToolError
└─────────┬─────────┘
          │ Valid
          ▼
┌───────────────────┐
│ Check Abort Signal│ ─── Aborted ──→ Throw AbortError
└─────────┬─────────┘
          │
          ▼
┌───────────────────┐
│ Execute Handler   │
│ with Context:     │
│  - toolCallId     │
│  - signal         │
│  - respond builder│
│  - sandbox        │
└─────────┬─────────┘
          │
          ▼
    ┌─────┴─────┐
    │  Success? │
    └─────┬─────┘
          │
     No   │   Yes
      ▼   │    ▼
┌─────────┐│┌───────────────┐
│ Retry?  │││ Return Result │
└────┬────┘│└───────────────┘
     │     │
   Yes     │
     ▼     │
┌──────────┴┐
│ Backoff   │
│ & Retry   │
└───────────┘
```

## Retry Logic

```typescript
// src/tools/tool-dsl.ts:176-215
const maxRetries = options.maxRetries ?? 0;
const retryDelayMs = options.retryDelayMs ?? 1000;
const shouldRetry = options.shouldRetry ?? (() => true);

for (let attempt = 0; attempt <= maxRetries; attempt++) {
  if (attempt > 0) {
    // Exponential backoff: delay * 2^(attempt-1)
    const delay = retryDelayMs * Math.pow(2, attempt - 1);
    await new Promise(resolve => setTimeout(resolve, delay));

    if (signal?.aborted) {
      throw new Error("Operation aborted");
    }
  }

  try {
    return await options.run(params, context);
  } catch (error) {
    if (attempt < maxRetries && shouldRetry(error)) {
      continue;
    }
    throw error;
  }
}
```

## Tool Result Caching

### Cache Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Tool Result Cache                         │
│                                                              │
│  ┌───────────────┐  ┌──────────────────┐  ┌──────────────┐ │
│  │ Cache Key     │  │ Cache Entry      │  │ Invalidation │ │
│  │ - tool name   │  │ - result         │  │ - File watch │ │
│  │ - params hash │  │ - git SHA        │  │ - Git status │ │
│  │ - cwd         │  │ - timestamp      │  │ - TTL expiry │ │
│  └───────────────┘  └──────────────────┘  └──────────────┘ │
│                                                              │
│  LRU Eviction: Least Recently Used entries removed first    │
└─────────────────────────────────────────────────────────────┘
```

### Cache Key Generation

```typescript
// Cache keys are generated from:
// 1. Tool name
// 2. Serialized parameters (sorted keys for consistency)
// 3. Current working directory
// 4. Git HEAD SHA (for file-dependent tools)

function generateCacheKey(
  toolName: string,
  params: unknown,
  cwd: string
): string {
  const paramsStr = JSON.stringify(sortKeys(params));
  return `${toolName}:${hashString(paramsStr)}:${cwd}`;
}
```

### Invalidation Strategies

| Strategy | Trigger | Tools Affected |
|----------|---------|----------------|
| File Watch | File modified | read, edit |
| Git Status | Working tree changed | All file tools |
| TTL Expiry | Time elapsed | `websearch`, `webfetch` |
| Manual | User request | All |

## Built-in Tools

### File Operations

| Tool | Purpose | Key Parameters |
|------|---------|----------------|
| `read` | Read file contents | `file_path`, `offset`, `limit` |
| `write` | Create/overwrite file | `file_path`, `content` |
| `edit` | Replace text in file | `file_path`, `old_string`, `new_string` |
| `list` | List directory | `path`, `pattern` |

### Search Tools

| Tool | Purpose | Key Parameters |
|------|---------|----------------|
| `search` | `ripgrep` search | `pattern`, `path`, `type` |
| `websearch` | Web search via `Exa` | `query`, `domains` |
| `codesearch` | Semantic code search | `query`, `scope` |

### Shell Tools

| Tool | Purpose | Key Parameters |
|------|---------|----------------|
| `bash` | Execute shell command | `command`, `timeout` |
| `background_tasks` | Manage background shells | `action`, `id` |

### MCP Tools

MCP (Model Context Protocol) tools are dynamically loaded from external servers:

```typescript
// MCP tool naming convention
const mcpToolName = `mcp__${serverName}__${toolName}`;

// Example: mcp__filesystem__read_file
```

## Tool Annotations

Annotations provide metadata for safety and UI:

```typescript
interface ToolAnnotations {
  // Safety hints
  readOnlyHint?: boolean;      // Tool doesn't modify state
  destructiveHint?: boolean;   // Tool may cause data loss

  // UI hints
  progressHint?: boolean;      // Tool supports progress updates
  confirmationHint?: boolean;  // Require user confirmation
}
```

## Sandbox Integration

Tools can execute within a sandbox-protected environment:

```typescript
interface ToolRunContext<Details> {
  toolCallId: string;
  signal?: AbortSignal;
  respond: ToolResponseBuilder<Details>;
  sandbox?: Sandbox;  // Docker or local sandbox
}

// Usage in tool handler
run: async (params, { sandbox }) => {
  if (sandbox) {
    return await sandbox.exec(params.command);
  }
  return await exec(params.command);
}
```

## Error Handling

### ToolError Class

```typescript
// src/tools/tool-dsl.ts:120-129
class ToolError extends Error {
  constructor(
    message: string,
    public readonly code?: string,
    public readonly details?: unknown
  ) {
    super(message);
    this.name = "ToolError";
  }
}
```

### Error Codes

| Code | Description | Retryable |
|------|-------------|-----------|
| `VALIDATION_ERROR` | Schema validation failed | No |
| `PERMISSION_DENIED` | Access denied by firewall | No |
| `TIMEOUT` | Operation timed out | Yes |
| `NETWORK_ERROR` | Network request failed | Yes |
| `FILE_NOT_FOUND` | File does not exist | No |

## Path Utilities

```typescript
// src/tools/tool-dsl.ts:220-241

// Expand ~ to home directory
function expandUserPath(path: string): string {
  if (path === "~") return os.homedir();
  if (path.startsWith("~/")) {
    return path.replace("~", os.homedir());
  }
  return path;
}

// Interpolate environment variables
function interpolateContext(value: string): string {
  return value
    .replace(/\$\{env\.([^}]+)\}/g, (_, var) => process.env[var] ?? "")
    .replace(/\$\{cwd\}/g, process.cwd())
    .replace(/\$\{home\}/g, os.homedir());
}
```

## Performance Considerations

1. **Validator Caching**: Compiled validators are cached per schema
2. **WeakMap for GC**: Cache uses WeakMap to allow garbage collection
3. **Batch Validation**: `AJV` schema `allErrors: false` stops at first error
4. **LRU Cache**: Tool results use LRU eviction to bound memory
5. **Abort Signals**: All tools respect abort signals for fast cancellation

## Testing Tools

```typescript
// Test a tool directly
const result = await myTool.execute(
  "test-call-id",
  { input: "test" },
  new AbortController().signal
);

// Mock tool for testing
const mockTool = createTool({
  name: "mock_tool",
  schema: Type.Object({}),
  run: async () => {
    return { content: [{ type: "text", text: "mocked" }] };
  }
});
```

## Related Documentation

- [Agent State Machine](AGENT_STATE_MACHINE.md) - How tools are executed
- [Safety & Firewall](SAFETY_FIREWALL.md) - Tool permission checks
- [MCP Integration](MCP_INTEGRATION.md) - Dynamic tool loading
- [Hooks System](HOOKS_SYSTEM.md) - PreToolUse/PostToolUse hooks
