# Safety & Firewall System Design

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. The action firewall now lives in `packages/local-host-rs/src/safety/`. Some file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.


The action firewall is the central decision point for tool safety. Before any tool executes, it passes through the firewall which evaluates rules and returns a verdict: allow, `require_approval`, or block.

## Overview

The safety system provides:

- **Rule-Based Evaluation**: Sequential rule matching with priority
- **Enterprise Policy**: Organizational restrictions on tools/paths
- **Dangerous Command Detection**: Regex and tree-sitter analysis
- **Workspace Containment**: Prevent writes outside project
- **PII Protection**: Block egress of sensitive data
- **Semantic Analysis**: Optional LLM-based intent verification

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Action Firewall                               │
│                                                                      │
│  Tool Call ──→ ActionFirewall.evaluate() ──→ Rules (sequential)     │
│                        │                           │                 │
│                        ▼                           ▼                 │
│                 Context Object              Verdict + Reason         │
│                 - toolName                  - allow                  │
│                 - args                      - require_approval       │
│                 - metadata                  - block                  │
│                 - user/session                                       │
│                                                                      │
│  Rule Priority (first match wins):                                  │
│  1. Enterprise policy (hard blocks)                                 │
│  2. System path protection (hard blocks)                            │
│  3. Workspace containment (soft approval)                           │
│  4. Dangerous command patterns (soft approval)                      │
│  5. Semantic judge (optional, slow path)                            │
└─────────────────────────────────────────────────────────────────────┘
```

## Verdict Types

| Verdict | Description | User Action |
|---------|-------------|-------------|
| `allow` | Tool executes immediately | None required |
| `require_approval` | User must approve | Accept/Reject prompt |
| `block` | Tool is prevented entirely | Cannot proceed |

## Rule Interface

```typescript
// src/safety/action-firewall.ts:134-147
interface ActionFirewallRule {
  id: string;                    // Unique identifier for logging
  description: string;           // Human-readable explanation
  action?: "allow" | "require_approval" | "block";  // Default: require_approval
  match: (context: ActionApprovalContext) => boolean | Promise<boolean>;
  reason?: (context: ActionApprovalContext) => string | Promise<string>;
  remediation?: (context: ActionApprovalContext) => string | Promise<string>;
}
```

## Context Object

```typescript
interface ActionApprovalContext {
  toolName: string;
  args: Record<string, unknown>;
  metadata?: {
    workflowState?: WorkflowStateSnapshot;
    annotations?: ToolAnnotations;
  };
  user?: { id: string; orgId: string };
  session?: { id: string };
  userIntent?: string;  // For semantic judge
}
```

## Default Rules

### 1. Enterprise Policy (Hard Block)

```typescript
// src/safety/action-firewall.ts:492-508
{
  id: "enterprise-policy",
  description: "Enforce enterprise policies on tools and dependencies",
  action: "block",
  match: async (ctx) => {
    const result = await checkPolicy(ctx);
    policyCheckCache.set(ctx, result);  // Cache for reason()
    return !result.allowed;
  },
  reason: async (ctx) => {
    const cached = policyCheckCache.get(ctx);
    return cached?.reason ?? "Action blocked by enterprise policy";
  }
}
```

### 2. System Path Protection (Hard Block)

Protected paths that should never be modified:

```typescript
// src/safety/path-containment.ts (simplified)
const LINUX_SYSTEM_PATHS = [
  "/etc", "/usr", "/var", "/boot", "/sys", "/proc",
  "/dev", "/bin", "/sbin", "/lib", "/lib64", "/opt",
];
const MAC_SYSTEM_PATHS = [
  "/etc", "/usr", "/var", "/System", "/Library",
  "/private/etc", "/private/var", "/bin", "/sbin", "/dev",
];
const WINDOWS_SYSTEM_PATHS = [
  "C:\\Windows", "C:\\Program Files", "C:\\Program Files (x86)"
];
const SYSTEM_PATHS = process.platform === "darwin"
  ? MAC_SYSTEM_PATHS
  : process.platform === "win32"
    ? WINDOWS_SYSTEM_PATHS
    : LINUX_SYSTEM_PATHS;
```

```typescript
// src/safety/action-firewall.ts:509-537
{
  id: "system-path-protection",
  description: "Prevent modification of critical system directories",
  action: "block",
  match: (ctx) => {
    if (!["write", "edit", "delete_file", "move_file", "copy_file"]
        .includes(ctx.toolName)) {
      return false;
    }

    const paths = extractFilePaths(ctx);
    // Exclude paths that are safely in temp directories
    const unsafePaths = paths.filter(p => !isContainedInWorkspace(p));
    return unsafePaths.some(isSystemPath);
  },
  reason: () => "Modification of critical system directories is blocked.",
  remediation: () => "Use the workspace directory or a temporary folder."
}
```

### 3. Workspace Containment (Approval Required)

```typescript
// src/safety/path-containment.ts (simplified)
const resolvedPath = resolve(filePath);
const realFilePath = resolveRealPath(resolvedPath) ?? resolvedPath; // uses nearest existing parent

const workspaceRoot = resolve(process.cwd());
const workspaceRootReal = tryRealpath(workspaceRoot);
const tempDir = tmpdir();
const tempDirReal = tryRealpath(tempDir);
const tempRoots = new Set([tempDirReal, tryRealpath("/tmp")].filter(Boolean));

const isInsideWorkspace =
  isWithin(workspaceRoot, resolvedPath) &&
  isWithin(workspaceRootReal, realFilePath);

const isInsideTemp =
  [...tempRoots].some((root) => isWithin(root, realFilePath));

if (isInsideWorkspace || isInsideTemp) return true;

for (const trustedPath of trustedPaths) {
  const trustedReal = tryRealpath(trustedPath);
  if (isWithin(trustedPath, resolvedPath) && isWithin(trustedReal, realFilePath)) {
    return true;
  }
}

return false;
```

Notes:
- `resolveRealPath` resolves the nearest existing parent to prevent symlink escapes
  while still supporting new (non-existent) files.
- Both logical and real paths must be contained in a safe zone.

### 4. Dangerous Command Patterns

```typescript
// src/safety/action-firewall.ts:249-268
const dangerousCommandRules: ActionFirewallRule[] = Object.entries(
  dangerousPatterns
).map(([key, pattern]) => ({
  id: `command-${key}`,
  description: dangerousPatternDescriptions[key],
  action: "require_approval",
  match: (ctx) => {
    const command = getStringArg(ctx, "command");
    return !!command && pattern.test(command);
  },
  reason: (ctx) => {
    const command = getStringArg(ctx, "command") ?? "";
    return `Detected ${dangerousPatternDescriptions[key]}: ${command.trim()}`;
  }
}));
```

### 5. Tree-Sitter Command Analysis

More accurate than regex patterns:

```typescript
// src/safety/action-firewall.ts:280-317
const treeSitterCommandRule: ActionFirewallRule = {
  id: "command-treesitter-analysis",
  description: "Tree-sitter based command safety analysis",
  action: "require_approval",
  match: (ctx) => {
    if (!isParserAvailable()) return false;
    if (ctx.toolName !== "bash") return false;

    let command = getStringArg(ctx, "command");
    if (!command) return false;

    // Unwrap bash -c "..." style commands
    const unwrapped = unwrapShellCommand(command);
    if (unwrapped) command = unwrapped;

    const analysis = analyzeCommandSafety(command);
    treeSitterAnalysisCache.set(ctx, {
      safe: analysis.safe,
      reason: analysis.reason
    });
    return !analysis.safe;
  },
  reason: (ctx) => {
    const cached = treeSitterAnalysisCache.get(ctx);
    return cached?.reason ?? "Command failed safety analysis";
  }
};
```

### 6. PII Protection Rule

```typescript
// src/safety/action-firewall.ts:647-668
{
  id: HUMAN_EGRESS_PII_RULE_ID,
  description: "PII must be redacted before human-facing tools execute",
  action: "require_approval",
  match: (ctx) => {
    if (!isHumanFacingTool(ctx.toolName)) return false;
    const pending = getPendingUnredactedPii(ctx);
    return pending.length > 0;
  },
  reason: (ctx) => {
    const pending = getPendingUnredactedPii(ctx);
    const offenders = pending
      .map(a => `${a.label} (artifact: ${a.id})`)
      .join("; ");
    return `Unredacted PII (${offenders}) detected before human-facing tool.`;
  }
}
```

## Policy Check Caching

```typescript
// src/safety/action-firewall.ts:103-106
// WeakMap ensures automatic garbage collection
const policyCheckCache = new WeakMap<
  ActionApprovalContext,
  PolicyCheckResult
>();
```

## ActionFirewall Class

```typescript
// src/safety/action-firewall.ts:671-744
class ActionFirewall {
  private semanticJudge?: SemanticJudge;

  constructor(
    private readonly rules: ActionFirewallRule[] = defaultFirewallRules
  ) {}

  setSemanticJudge(judge: SemanticJudge) {
    this.semanticJudge = judge;
  }

  async evaluate(context: ActionApprovalContext): Promise<ActionFirewallVerdict> {
    // 1. Evaluate rules sequentially
    for (const rule of this.rules) {
      if (await rule.match(context)) {
        const action = rule.action ?? "require_approval";
        const reason = await rule.reason?.(context)
          ?? `Action matched rule: ${rule.description}`;
        const remediation = await rule.remediation?.(context);

        if (action === "allow") {
          return { action: "allow" };
        }

        if (action === "block") {
          return { action, ruleId: rule.id, reason, remediation };
        }

        return { action, ruleId: rule.id, reason };
      }
    }

    // 2. Run semantic judge if available (slow path)
    if (this.semanticJudge && context.userIntent) {
      const SENSITIVE_TOOLS = ["bash", "write", "edit", "delete_file"];
      if (SENSITIVE_TOOLS.includes(context.toolName)) {
        const judgment = await this.semanticJudge.evaluate({
          userIntent: context.userIntent,
          toolName: context.toolName,
          toolArgs: context.args
        });

        if (!judgment.safe) {
          return {
            action: "require_approval",
            ruleId: "semantic-judge",
            reason: judgment.reason
          };
        }
      }
    }

    return { action: "allow" };
  }
}
```

## Firewall Verdict

```typescript
interface ActionFirewallVerdict {
  action: "allow" | "require_approval" | "block";
  ruleId?: string;
  reason?: string;
  remediation?: string;
}
```

## Environment Configuration

```typescript
// Environment variables for policy configuration
const isStrictUntaggedEgress = () =>
  process.env.MAESTRO_FAIL_UNTAGGED_EGRESS === "1";

const isBackgroundShellBlocked = () =>
  process.env.MAESTRO_BACKGROUND_SHELL_DISABLE === "1";

// Plan mode requires approval for all mutations
if (process.env.MAESTRO_PLAN_MODE === "1") {
  // Require approval for write, edit, bash, todo, gh_pr, gh_issue
}
```

## Firewall Configuration File

Users can configure trusted paths in `~/.maestro/firewall.json`:

```json
{
  "containment": {
    "trustedPaths": [
      "/home/user/shared-workspace",
      "/data/projects"
    ]
  }
}
```

## Tool Tags for Egress Control

```typescript
// src/safety/workflow-state.ts
const TOOL_TAGS: Record<string, { egress?: "human" | "http" }> = {
  "write": {},           // No egress
  "bash": {},            // No egress
  "send_email": { egress: "human" },
  "post_slack": { egress: "human" },
  "http_request": { egress: "http" }
};

function isHumanFacingTool(toolName: string): boolean {
  const tags = TOOL_TAGS[toolName];
  if (tags?.egress === "human") return true;
  if (!tags && looksLikeEgress(toolName)) {
    warnUntaggedEgress(toolName);
    return true;
  }
  return false;
}
```

## MCP Tool Safety

```typescript
// src/safety/action-firewall.ts:593-606
{
  id: "mcp-destructive-tool",
  description: "MCP tools marked as destructive require approval",
  action: "require_approval",
  match: (ctx) => {
    if (!ctx.toolName.startsWith("mcp__")) return false;
    const annotations = ctx.metadata?.annotations;
    return annotations?.destructiveHint === true
        && !annotations?.readOnlyHint;
  },
  reason: (ctx) => `MCP tool "${ctx.toolName}" is marked as destructive`
}
```

## Semantic Judge (Optional)

For sensitive tools, an LLM-based judge can verify intent:

```typescript
interface SemanticJudge {
  evaluate(context: SemanticJudgeContext): Promise<{
    safe: boolean;
    reason: string;
  }>;
}

interface SemanticJudgeContext {
  userIntent: string;
  toolName: string;
  toolArgs: unknown;
}
```

## Approval Service Integration

The firewall integrates with the approval service:

```typescript
// src/safety/approval-service.ts
class ApprovalService {
  mode: "prompt" | "auto" | "fail";

  async requestApproval(verdict: ActionFirewallVerdict): Promise<boolean> {
    switch (this.mode) {
      case "auto":
        return true;  // Auto-approve all
      case "fail":
        return false;  // Reject all
      case "prompt":
        return await this.promptUser(verdict);
    }
  }
}
```

## Performance Considerations

1. **WeakMap Caching**: Policy results cached per context
2. **Short-Circuit Evaluation**: First matching rule wins
3. **Lazy Tree-Sitter**: Only loaded when needed
4. **Async Rules**: Rules can be async for policy lookups

## Related Documentation

- [Enterprise RBAC](ENTERPRISE_RBAC.md) - Enterprise policy details
- [Hooks System](HOOKS_SYSTEM.md) - PreToolUse hook integration
- [Tool System](TOOL_SYSTEM.md) - Tool annotations and execution
