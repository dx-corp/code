# Hooks System Design

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. The hooks system now lives in `packages/local-host-rs/src/hooks/`. Some file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.

The current native loader reads `~/.composer/hooks.{toml,json}` and project
`.composer/hooks.{toml,json}`. Project configuration requires workspace trust
from the global configuration. `IntegratedHookSystem` invokes script and WASM
executors for `PreToolUse`; its post-tool path dispatches the native registry.
Standalone executors having post-tool methods does not establish integrated
post-tool execution. See the [native hook guide](../../packages/local-host-rs/src/hooks/README.md) for current setup.



The hooks system allows external programs to intercept, modify, or block agent operations at various lifecycle points. This enables custom workflows, validation, and integrations.

## Overview

Hooks provide:

- **Lifecycle Interception**: Pre/post tool execution, session events
- **Modification Capabilities**: Transform inputs/outputs
- **Blocking**: Prevent operations based on custom logic
- **External Integration**: Shell scripts, HTTP endpoints, agent prompts

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Hooks Architecture                            │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                     Hook Configuration                       │    │
│  │  - Environment variables: MAESTRO_HOOKS_*                  │    │
│  │  - User config: ~/.maestro/hooks.json                      │    │
│  │  - Project config: .maestro/hooks.json                     │    │
│  │  - Programmatic: registerHook()                             │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                              │                                       │
│                              ▼                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                      Hook Executor                           │    │
│  │  - Pattern matching against tool/event                      │    │
│  │  - JSON input via stdin                                     │    │
│  │  - JSON output parsing                                      │    │
│  │  - Async hook support                                       │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                              │                                       │
│           ┌──────────────────┼──────────────────┐                   │
│           ▼                  ▼                  ▼                   │
│  ┌────────────────┐  ┌────────────────┐  ┌────────────────┐        │
│  │ Command Hook   │  │ Prompt Hook    │  │ Agent Hook     │        │
│  │ (shell script) │  │ (user prompt)  │  │ (LLM agent)    │        │
│  └────────────────┘  └────────────────┘  └────────────────┘        │
└─────────────────────────────────────────────────────────────────────┘
```

## Hook Events

| Event | Description | Can Block | Can Modify |
|-------|-------------|-----------|------------|
| `PreToolUse` | Before tool execution | ✅ | ✅ Input |
| `PostToolUse` | After successful execution | ❌ | ✅ Context |
| `PostToolUseFailure` | After failed execution | ❌ | ❌ |
| `SessionStart` | When session begins | ❌ | ❌ |
| `SessionEnd` | When session ends | ❌ | ❌ |
| `SubagentStart` | Before spawning subagent | ✅ | ✅ |
| `SubagentStop` | When subagent completes | ❌ | ❌ |
| `UserPromptSubmit` | When user submits prompt | ✅ | ✅ |
| `Notification` | On various notifications | ❌ | ❌ |
| `PreCompact` | Before context compaction | ✅ | ❌ |
| `PermissionRequest` | When permission needed | ✅ | ❌ |
| `EvalGate` | After a tool call, to score it | ✅ Marks failed | ✅ Context |

`EvalGate` runs immediately after `PostToolUse` on the same tool call and
receives the same name, arguments, and raw output. It is the place for scoring
and assertion hooks. Its `block` cannot un-run the tool, so it is reported to
the model as a failed tool result with the reason appended, rather than as a
prevented call.

`SessionStart` and `SessionEnd` fire on the real session transitions: starting a
new session, creating the session file for one, resuming a session, forking, and
an orderly exit. Both are advisory—their results are logged and cannot stop a
transition the user has already made.

`SessionEnd` is best-effort. A crash, a `SIGKILL`, or a terminal that goes away
cannot run it, so do not build cleanup that depends on `SessionEnd` always
arriving; treat it as a notification, not a guarantee.

`PermissionRequest` runs at the single point that decides whether a tool needs
approval, after the action firewall and the safety checks have already run
against the call's arguments. A `block` result denies the call and the user is
never asked. It cannot modify the input: the checks above it ran against the
original arguments, so rewriting them here would let a hook route a call around
them. Use `PreToolUse` to modify tool input.

A failed tool call is reported to `PostToolUseFailure` hooks with
`hookEventName` set to `PostToolUseFailure`, not `PostToolUse`.

## Configuration

### Environment Variables

```bash
# Command-based hooks
export MAESTRO_HOOKS_PRE_TOOL_USE="./hooks/pre-tool.sh"
export MAESTRO_HOOKS_POST_TOOL_USE="./hooks/post-tool.sh"
export MAESTRO_HOOKS_USER_PROMPT_SUBMIT="./hooks/validate-prompt.sh"
```

### Command Hook Process Environment

A command hook is started with these variables set:

| Variable | Meaning |
| --- | --- |
| `HOOK_EVENT_NAME` | The event that fired the hook, for example, `PreToolUse` |
| `TOOL_NAME` | The tool being invoked; set only for tool events |
| `INPUT_JSON` | Copy of the JSON payload, set only when it is at most 64 KiB |
| `INPUT_JSON_OMITTED` | Byte length of the payload when it was too large for `INPUT_JSON`; unset otherwise |

Standard input is the only complete transport for the payload and has no size limit;
read it (`INPUT=$(cat)`) rather than relying on `INPUT_JSON`. The environment
copy is bounded because a single environment entry over the operating system
limit (`MAX_ARG_STRLEN`, 128 KiB on Linux) fails the spawn with `E2BIG`, which
would stop the hook from running at all.

The payload's top-level fields use the `camelCase` names in "Hook Input Format"
below (`toolName`, `toolInput`, `hookEventName`, `durationMs`). The equivalent
`snake_case` names (`tool_name`, `tool_input`) are also present because earlier
builds emitted only those; write new hooks against the `camelCase` names. Keys
inside `toolInput` are the tool's own arguments and are passed through
unchanged.

A command hook runs in its own process group and is killed as a group when it
exceeds its timeout, so anything it starts is terminated with it. A hook that
needs a process to outlive it must detach that process from the group itself
(for example with `setsid`).

### Configuration File

```json
// ~/.maestro/hooks.json
{
  "hooks": [
    {
      "event": "PreToolUse",
      "type": "command",
      "command": "./hooks/validate-tool.sh",
      "matcher": {
        "toolName": "bash"
      },
      "timeout": 5000
    },
    {
      "event": "PostToolUse",
      "type": "command",
      "command": "./hooks/log-tool.sh",
      "matcher": {
        "toolName": "*"
      }
    },
    {
      "event": "UserPromptSubmit",
      "type": "prompt",
      "message": "Confirm sending prompt?",
      "matcher": {}
    }
  ]
}
```

## Hook Types

### Command Hook

Executes a shell command with JSON input/output:

```typescript
// src/hooks/types.ts
interface HookCommandConfig {
  type: "command";
  command: string;
  timeout?: number;
  env?: Record<string, string>;
}
```

### Prompt Hook

Prompts the user for confirmation:

```typescript
interface HookPromptConfig {
  type: "prompt";
  message: string;
  options?: {
    allowAlways?: boolean;
    default?: "approve" | "reject";
  };
}
```

### Agent Hook

Runs an LLM agent for complex validation:

```typescript
interface HookAgentConfig {
  type: "agent";
  model?: string;
  systemPrompt: string;
  maxTokens?: number;
}
```

### Callback Hook

Programmatic hook registration:

```typescript
interface HookCallbackConfig {
  type: "callback";
  callback: (input: HookInput) => Promise<HookJsonOutput>;
}
```

## Hook Input Format

### PreToolUse Input

```typescript
// src/hooks/types.ts
interface PreToolUseHookInput {
  type: "PreToolUse";
  toolName: string;
  toolInput: Record<string, unknown>;
  toolCallId: string;
  sessionId: string;
  timestamp: string;
}
```

### PostToolUse Input

```typescript
interface PostToolUseHookInput {
  type: "PostToolUse";
  toolName: string;
  toolInput: Record<string, unknown>;
  toolOutput: {
    content: Array<{ type: string; text?: string }>;
    isError: boolean;
  };
  toolCallId: string;
  durationMs: number;
  sessionId: string;
  timestamp: string;
}
```

The payload also carries the flat `tool_output` string and `is_error` boolean
that external hooks have shipped against. Both spellings describe the same
result; `toolOutput.isError` is the documented one.

`durationMs` is the wall-clock time of the tool execution itself, excluding the
hook dispatch that reports it. Two cases report something coarser, and a hook
that makes decisions on timing should know which it is looking at:

- Tools executed together in a parallel read-only batch report the batch's
  elapsed time, which is an upper bound on any single call in it.
- Sub-operations of one tool call, and results supplied by the approval UI
  rather than executed here, report `0`.

### SessionEnd Input

```typescript
interface SessionEndHookInput {
  hookEventName: "SessionEnd";
  cwd: string;
  sessionId?: string;
  transcriptPath?: string;
  timestamp: string;
  reason: string;
  durationMs: number;
  turnCount: number;
}
```

`transcriptPath` is the absolute path to the active native JSONL when the
session has a persistent writer. Observation-only adapters may copy and redact
that file, but it is not restore or continuation authority. External hooks also
receive the compatibility `transcript_path` spelling.

### UserPromptSubmit Input

```typescript
interface UserPromptSubmitHookInput {
  type: "UserPromptSubmit";
  prompt: string;
  attachments?: Attachment[];
  sessionId: string;
  timestamp: string;
}
```

## Hook Output Format

### Standard Output

```typescript
// src/hooks/types.ts
interface HookJsonOutput {
  continue: boolean;
  decision?: "approve" | "reject" | "skip";
  message?: string;
  hookSpecificOutput?: HookSpecificOutput;
}
```

### PreToolUse Output

```typescript
interface PreToolUseHookOutput {
  hookEventName: "PreToolUse";
  permissionDecision?: "allow" | "deny" | "ask";
  modifiedInput?: Record<string, unknown>;
  contextToAdd?: string;
}
```

### PostToolUse Output

```typescript
interface PostToolUseHookOutput {
  hookEventName: "PostToolUse";
  contextToAdd?: string;
  suppressOutput?: boolean;
}

### EvalGate Output

```typescript
interface EvalGateHookOutput {
  hookEventName: "EvalGate";
  score?: number;
  threshold?: number;
  passed?: boolean;
  rationale?: string;
  assertions?: Array<{
    name: string;
    passed?: boolean;
    score?: number;
    threshold?: number;
    evidence?: string;
  }>;
}
```
```

## Hook Execution

### Executor (`src/hooks/executor.ts`)

```typescript
async function executeHook(
  hookConfig: HookConfig,
  input: HookInput
): Promise<HookExecutionResult> {
  switch (hookConfig.type) {
    case "command":
      return executeCommandHook(hookConfig, input);
    case "prompt":
      return executePromptHook(hookConfig, input);
    case "agent":
      return executeAgentHook(hookConfig, input);
    case "callback":
      return hookConfig.callback(input);
  }
}

async function executeCommandHook(
  config: HookCommandConfig,
  input: HookInput
): Promise<HookExecutionResult> {
  const proc = spawn(config.command, [], {
    env: { ...process.env, ...config.env },
    stdio: ["pipe", "pipe", "pipe"]
  });

  // Send input as JSON via stdin
  proc.stdin.write(JSON.stringify(input));
  proc.stdin.end();

  // Collect output
  let stdout = "";
  proc.stdout.on("data", (data) => {
    stdout += data.toString();
  });

  // Wait for completion with timeout
  await Promise.race([
    new Promise((resolve) => proc.on("close", resolve)),
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error("Hook timeout")), config.timeout)
    )
  ]);

  // Parse output
  return parseHookOutput(stdout);
}
```

### Pattern Matching

```typescript
// src/hooks/config.ts
interface HookMatcher {
  toolName?: string | string[];  // Glob patterns
  sessionId?: string;
  custom?: (input: HookInput) => boolean;
}

function matchesPattern(
  input: HookInput,
  matcher: HookMatcher
): boolean {
  // Check tool name pattern
  if (matcher.toolName) {
    const patterns = Array.isArray(matcher.toolName)
      ? matcher.toolName
      : [matcher.toolName];

    const toolName = getMatchTarget(input);
    if (!patterns.some(p => minimatch(toolName, p))) {
      return false;
    }
  }

  // Check session ID
  if (matcher.sessionId && input.sessionId !== matcher.sessionId) {
    return false;
  }

  // Check custom predicate
  if (matcher.custom && !matcher.custom(input)) {
    return false;
  }

  return true;
}
```

## Tool Integration

### ToolHookService

```typescript
// src/hooks/tool-integration.ts
class ToolHookService {
  async beforeToolExecution(
    toolName: string,
    toolInput: Record<string, unknown>,
    toolCallId: string
  ): Promise<{
    proceed: boolean;
    modifiedInput?: Record<string, unknown>;
    reason?: string;
  }> {
    const input: PreToolUseHookInput = {
      type: "PreToolUse",
      toolName,
      toolInput,
      toolCallId,
      sessionId: this.sessionId,
      timestamp: new Date().toISOString()
    };

    const hooks = getMatchingHooks("PreToolUse", input);

    for (const hook of hooks) {
      const result = await executeHook(hook, input);

      if (!result.continue) {
        return {
          proceed: false,
          reason: result.message ?? "Blocked by hook"
        };
      }

      if (result.hookSpecificOutput?.modifiedInput) {
        return {
          proceed: true,
          modifiedInput: result.hookSpecificOutput.modifiedInput
        };
      }
    }

    return { proceed: true };
  }

  async afterToolExecution(
    toolName: string,
    toolInput: Record<string, unknown>,
    toolOutput: ToolOutput,
    durationMs: number
  ): Promise<{ contextToAdd?: string }> {
    const input: PostToolUseHookInput = {
      type: "PostToolUse",
      toolName,
      toolInput,
      toolOutput,
      toolCallId: this.currentToolCallId,
      durationMs,
      sessionId: this.sessionId,
      timestamp: new Date().toISOString()
    };

    const hooks = getMatchingHooks("PostToolUse", input);
    let contextToAdd = "";

    for (const hook of hooks) {
      const result = await executeHook(hook, input);

      if (result.hookSpecificOutput?.contextToAdd) {
        contextToAdd += result.hookSpecificOutput.contextToAdd + "\n";
      }
    }

    return { contextToAdd: contextToAdd || undefined };
  }
}
```

## Session Integration

```typescript
// src/hooks/session-integration.ts
class SessionHookService {
  async onSessionStart(sessionId: string): Promise<void> {
    const input: SessionStartHookInput = {
      type: "SessionStart",
      sessionId,
      timestamp: new Date().toISOString()
    };

    const hooks = getMatchingHooks("SessionStart", input);
    await Promise.all(hooks.map(h => executeHook(h, input)));
  }

  async onSessionEnd(sessionId: string): Promise<void> {
    const input: SessionEndHookInput = {
      type: "SessionEnd",
      sessionId,
      timestamp: new Date().toISOString()
    };

    const hooks = getMatchingHooks("SessionEnd", input);
    await Promise.all(hooks.map(h => executeHook(h, input)));
  }
}
```

## Notification Hooks

```typescript
// src/hooks/notification-hooks.ts
interface NotificationPayload {
  type: NotificationEventType;
  title: string;
  body: string;
  metadata?: Record<string, unknown>;
}

type NotificationEventType =
  | "task_complete"
  | "error"
  | "approval_required"
  | "session_summary";

async function sendNotification(
  payload: NotificationPayload
): Promise<void> {
  if (!isNotificationEnabled()) return;

  const input: NotificationHookInput = {
    type: "Notification",
    notification: payload,
    timestamp: new Date().toISOString()
  };

  const hooks = getMatchingHooks("Notification", input);
  await Promise.all(hooks.map(h => executeHook(h, input)));
}
```

## Async Hooks

```typescript
// src/hooks/types.ts
interface AsyncHookResponse {
  async: true;
  hookId: string;
  pollUrl?: string;
}

function isAsyncHookResponse(
  response: unknown
): response is AsyncHookResponse {
  return (
    typeof response === "object" &&
    response !== null &&
    (response as AsyncHookResponse).async === true
  );
}

// Async hook tracking
let asyncHookCount = 0;

function getAsyncHookCount(): number {
  return asyncHookCount;
}

function cleanupAsyncHooks(): void {
  asyncHookCount = 0;
}
```

## Output Parsing

```typescript
// src/hooks/output.ts
function parseHookOutput(stdout: string): HookExecutionResult {
  try {
    const parsed = JSON.parse(stdout.trim());
    return validateHookOutput(parsed);
  } catch (error) {
    // Non-JSON output treated as message
    return {
      continue: true,
      message: stdout.trim()
    };
  }
}

function validateHookOutput(output: unknown): HookExecutionResult {
  // Validate against schema
  const schema = getHookOutputSchema();
  const valid = ajv.validate(schema, output);

  if (!valid) {
    throw new Error(`Invalid hook output: ${ajv.errorsText()}`);
  }

  return output as HookExecutionResult;
}

function safeParseHookOutput(stdout: string): HookExecutionResult {
  try {
    return parseHookOutput(stdout);
  } catch {
    return {
      continue: true,
      message: "Hook output parsing failed"
    };
  }
}
```

## Example Hook Scripts

### Validate Bash Commands

```bash
#!/bin/bash
# hooks/validate-bash.sh

# Read JSON input from stdin
INPUT=$(cat)

TOOL_NAME=$(echo "$INPUT" | jq -r '.toolName')
COMMAND=$(echo "$INPUT" | jq -r '.toolInput.command')

# Block dangerous commands
if echo "$COMMAND" | grep -qE 'rm -rf|:(){:|fork'; then
  echo '{"continue": false, "message": "Dangerous command blocked"}'
  exit 0
fi

# Allow safe commands
echo '{"continue": true}'
```

### Log All Tool Executions

```bash
#!/bin/bash
# hooks/log-tool.sh

INPUT=$(cat)

# Log to file
echo "$(date): $(echo "$INPUT" | jq -c '{tool: .toolName, duration: .durationMs}')" \
  >> ~/.maestro/tool-log.jsonl

# Continue without modification
echo '{"continue": true}'
```

### Prompt for Sensitive Operations

```json
{
  "hooks": [
    {
      "event": "PreToolUse",
      "type": "prompt",
      "message": "This will modify files. Continue?",
      "matcher": {
        "toolName": ["write", "edit", "delete_file"]
      }
    }
  ]
}
```

## Related Documentation

- [Tool System](TOOL_SYSTEM.md) - Tool execution lifecycle
- [Safety & Firewall](SAFETY_FIREWALL.md) - Permission integration
- [Session Persistence](SESSION_PERSISTENCE.md) - Session events
