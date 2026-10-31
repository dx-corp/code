# MCP Server Integration Guide

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. MCP integration now lives in `packages/tui-rs/src/mcp/`. Some file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.


Contents: [What is MCP?](#what-is-mcp) · [Quick Start](#quick-start) · [Configuration](#configuration) · [Usage](#usage) · [Troubleshooting](#troubleshooting)

This guide explains how to create, configure, and use custom MCP (Model Context Protocol) servers with Deixic Code.

## What is MCP?

MCP (Model Context Protocol) is an open protocol that allows AI assistants to interact with external tools and data sources. Deixic Code supports MCP servers, enabling you to extend its capabilities with custom tools.

## Quick Start

### 1. Install an existing MCP server

```bash
# Example: GitHub MCP server
npm install -g @modelcontextprotocol/server-github
```

### 2. Configure Deixic Code to use it

Create `~/.maestro/mcp.json`:

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": {
        "GITHUB_TOKEN": "${GITHUB_TOKEN}"
      }
    }
  }
}
```

### 3. Verify connection

```bash
maestro
/mcp
```

You should see:
```
Model Context Protocol

● github
    Tools: list_issues, create_issue, get_repository, ...
```

## Configuration

### File Locations

- **Global config**: `~/.maestro/mcp.json` (applies to all projects)
- **Project config**: `.maestro/mcp.json` (project-specific, overrides global)

### Configuration Formats

Deixic Code supports two configuration formats:

#### Format 1: Claude Desktop Style (Recommended)

```json
{
  "mcpServers": {
    "server-name": {
      "command": "node",
      "args": ["path/to/server.js"],
      "env": {
        "API_KEY": "your-key"
      },
      "cwd": "/path/to/working/directory"
    }
  }
}
```

#### Format 2: Array Style

```json
{
  "servers": [
    {
      "name": "server-name",
      "transport": "stdio",
      "command": "node",
      "args": ["path/to/server.js"],
      "env": {
        "API_KEY": "your-key"
      }
    }
  ]
}
```

### Configuration Options

| Option | Type | Description | Required |
|--------|------|-------------|----------|
| `command` | string | Executable to run | Yes (for stdio) |
| `args` | string[] | Command arguments | No |
| `env` | object | Environment variables | No |
| `cwd` | string | Working directory | No |
| `url` | string | Server URL (for HTTP/SSE) | Yes (for HTTP/SSE) |
| `headers` | object | HTTP headers | No |
| `disabled` | boolean | Disable this server | No |
| `timeout` | number | Connection timeout (ms) | No (default: 30000) |

### Transport Types

Deixic Code auto-detects the transport type:

- **stdio**: Default when `command` is provided
- **sse**: When URL contains `/sse` or `sse` subdomain
- **http**: For other URLs

### EvalOps Knowledge MCP

Managed EvalOps launches can attach the Knowledge world-model MCP server without
adding a project config file. Configure one of:

- `MAESTRO_PLATFORM_MCP_URL`
- `MAESTRO_AGENT_MCP_URL`
- `MAESTRO_EVALOPS_AGENT_MCP_URL`
- `MAESTRO_PLATFORM_MCP_MANIFEST_URL`

The URL may be the public app base URL, the `/mcp` endpoint, or the EvalOps
agent MCP manifest at `/.well-known/evalops/agent-mcp.json`; Deixic Code normalizes
those forms to the HTTP MCP endpoint. Deixic Code forwards the bearer token from `MAESTRO_PLATFORM_MCP_TOKEN`,
`MAESTRO_AGENT_MCP_TOKEN`, `MAESTRO_EVALOPS_ACCESS_TOKEN`, or `EVALOPS_TOKEN`.
It also forwards `X-EvalOps-Workspace-Id`, `X-EvalOps-Session-Id`,
`X-EvalOps-Agent-Id`, `X-EvalOps-Agent-Run-Id`, trace/request IDs, and
`X-EvalOps-Scopes`.

For Knowledge, set scopes deliberately:

- `cerebro:read` exposes `knowledge_search`, `knowledge_gather_facts`,
  `knowledge_debug_beliefs`, and the other read tools.
- `cerebro:assert` additionally exposes `knowledge_assert_fact` for explicit,
  evidence-backed session learnings.

Agents should search or gather facts before asserting. Use
`knowledge_assert_fact` only when the session learned durable context that future
agents should recall, and always include a stable dimension, confidence reason,
and evidence.

### Fathom CUA MCP

Deixic Code can attach the local Fathom computer-use MCP shim as a plugin-managed
stdio server. This keeps desktop-control tools available through the same
Deixic Code MCP manager and agent tool bridge used for other MCP servers.

Enable it for a local run:

```bash
export MAESTRO_FATHOM_CUA_ENABLED=1
export MAESTRO_FATHOM_CUA_REPO=/path/to/fathom
export MAESTRO_FATHOM_CUA_WORKSPACE_ID=workspace_1
```

Optional runtime settings:

| Variable | Purpose |
| --- | --- |
| `MAESTRO_FATHOM_CUA_MCP_NAME` | MCP server name; defaults to `fathom-cua`. |
| `MAESTRO_FATHOM_CUA_CLIENT_COMMAND` | Installed `fathom-client` command. If omitted and a repo is configured or found, Deixic Code uses `go run ./cmd/fathom-client`. |
| `MAESTRO_FATHOM_CUA_CLIENT_ARGS_JSON` | Extra JSON string array of `fathom-client` arguments. |
| `MAESTRO_FATHOM_CUA_TOOL_PROFILE` | Agent-facing Fathom tool profile. Defaults to `canonical`; set to `full` or `debug` only for parity/debugging. |
| `MAESTRO_FATHOM_CUA_IPC_ROOT` | Helper IPC root for live desktop actions. |
| `MAESTRO_FATHOM_CUA_SESSION_ID` / `MAESTRO_FATHOM_CUA_TURN_ID` | Receipt lineage IDs propagated into Fathom. |
| `MAESTRO_FATHOM_CUA_DISABLE_IPC` | Set to `1` for negotiation/tool-list checks without Helper IPC. |

Deixic Code launches Fathom with the `canonical` profile by default so desktop
computer-use stays focused. The canonical profile keeps observation,
activation, native click/context-menu, focused text entry, visible-text
selection, structured controls, keyboard, and tool-search primitives. Broad
diagnostic primitives such as raw drag, paste, move-mouse, low-level scroll, and
window mutation stay behind the explicit `full`/`debug` profiles. File
operations are not exposed through Fathom: Deixic Code keeps them in the normal
file lane (`read`, `list`, `find`, `search`, `diff`, `apply_patch`, `edit`,
`write`, and `notebook_edit`) so filesystem mutations remain specific and
auditable.

To prove the full local path, run the live smoke on a Mac with Accessibility
permission granted:

```bash
MAESTRO_RUN_LIVE_FATHOM_CUA_MCP=1 npm run smoke:fathom-cua-mcp
```

The smoke opens focused Fathom AppKit dogfood targets, connects `fathom-cua`
through Deixic Code's MCP manager in the canonical profile, proves `list_apps`,
`tool_search`, and `activate_app`, then calls `get_app_state` and a distinct
action on separate app bundles: `set_value`, `type_text`, `press_key`,
`select_text`, `click`, `press_element`, `open_context_menu`,
`select_context_menu_item`, `focus_element`, `set_toggle_state`,
`set_slider_value`, and `select_menu_option`. It verifies each application
state change without printing raw typed values, asserts the broad full-profile
tools are absent, and briefly waits after state capture before each mutating
action so Fathom's helper-side `user_active` guard can observe an idle desktop.
The report includes the Fathom profile, tool count, and capability summary so
profile drift is visible in CI or local proof logs.

## Creating a Custom MCP Server

### Minimal TypeScript Server

```typescript
// my-tools-server.ts
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";

const server = new Server(
  { name: "my-tools", version: "1.0.0" },
  { capabilities: { tools: {} } }
);

// Define available tools
server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [
    {
      name: "greet",
      description: "Generate a greeting message",
      inputSchema: {
        type: "object",
        properties: {
          name: {
            type: "string",
            description: "Name to greet",
          },
        },
        required: ["name"],
      },
    },
    {
      name: "calculate",
      description: "Perform basic math operations",
      inputSchema: {
        type: "object",
        properties: {
          operation: {
            type: "string",
            enum: ["add", "subtract", "multiply", "divide"],
          },
          a: { type: "number" },
          b: { type: "number" },
        },
        required: ["operation", "a", "b"],
      },
    },
  ],
}));

// Handle tool execution
server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args } = request.params;

  switch (name) {
    case "greet":
      return {
        content: [
          { type: "text", text: `Hello, ${args.name}! Welcome to Deixic Code.` },
        ],
      };

    case "calculate": {
      const { operation, a, b } = args as {
        operation: string;
        a: number;
        b: number;
      };
      let result: number;
      switch (operation) {
        case "add":
          result = a + b;
          break;
        case "subtract":
          result = a - b;
          break;
        case "multiply":
          result = a * b;
          break;
        case "divide":
          result = b !== 0 ? a / b : NaN;
          break;
        default:
          throw new Error(`Unknown operation: ${operation}`);
      }
      return {
        content: [{ type: "text", text: `Result: ${result}` }],
      };
    }

    default:
      throw new Error(`Unknown tool: ${name}`);
  }
});

// Start the server
const transport = new StdioServerTransport();
await server.connect(transport);
```

### Build and Run

```bash
# Install dependencies
npm install @modelcontextprotocol/sdk

# Build
npx tsc my-tools-server.ts --module nodenext --moduleResolution nodenext

# Test locally
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | node my-tools-server.js
```

### Register with Deixic Code

```json
{
  "mcpServers": {
    "my-tools": {
      "command": "node",
      "args": ["/path/to/my-tools-server.js"]
    }
  }
}
```

## Tool Annotations

MCP tools can include behavior hints that Deixic Code respects:

```typescript
{
  name: "delete_file",
  description: "Delete a file from the filesystem",
  inputSchema: { /* ... */ },
  annotations: {
    destructiveHint: true,   // May perform destructive actions
    readOnlyHint: false,     // Modifies environment
    idempotentHint: false,   // Multiple calls have different effects
    openWorldHint: true,     // Interacts with external systems
  }
}
```

| Annotation | Meaning |
|------------|---------|
| `readOnlyHint` | Tool doesn't modify its environment |
| `destructiveHint` | Tool may perform destructive updates |
| `idempotentHint` | Safe to call repeatedly with same args |
| `openWorldHint` | Tool interacts with external systems |

## Resources and Prompts

MCP servers can also provide resources (data) and prompts (templates):

### Listing Resources

```bash
/mcp resources
```

### Reading a Resource

```bash
/mcp resources my-server resource://path/to/resource
```

### Listing Prompts

```bash
/mcp prompts
```

### Getting a Prompt

```bash
/mcp prompts my-server prompt-name
```

## Example MCP Servers

### Database Query Server

```typescript
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { Pool } from "pg";

const pool = new Pool({ connectionString: process.env.DATABASE_URL });

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  if (request.params.name === "query") {
    const { sql } = request.params.arguments as { sql: string };

    // Safety: Only allow SELECT queries
    if (!sql.trim().toLowerCase().startsWith("select")) {
      return {
        content: [{ type: "text", text: "Error: Only SELECT queries allowed" }],
        isError: true,
      };
    }

    const result = await pool.query(sql);
    return {
      content: [
        { type: "text", text: JSON.stringify(result.rows, null, 2) },
      ],
    };
  }
});
```

### API Integration Server

```typescript
server.setRequestHandler(CallToolRequestSchema, async (request) => {
  if (request.params.name === "fetch_weather") {
    const { city } = request.params.arguments as { city: string };

    const response = await fetch(
      `https://api.weatherapi.com/v1/current.json?key=${process.env.WEATHER_API_KEY}&q=${city}`
    );
    const data = await response.json();

    return {
      content: [
        {
          type: "text",
          text: `Weather in ${city}: ${data.current.condition.text}, ${data.current.temp_c}°C`,
        },
      ],
    };
  }
});
```

### File Watcher Server

```typescript
import { watch } from "fs";

// Notify Deixic Code when files change
watch("./src", { recursive: true }, (event, filename) => {
  server.notification({
    method: "notifications/resources/list_changed",
  });
});
```

## Troubleshooting

### Server not connecting

1. **Check server is executable**:
   ```bash
   node /path/to/server.js
   ```

2. **Verify config syntax**:
   ```bash
   cat ~/.maestro/mcp.json | jq .
   ```

3. **Check Deixic Code logs**:
   ```bash
   MAESTRO_LOG_LEVEL=debug maestro
   ```

### Tools not appearing

1. **Verify server implements tools/list**:
   ```bash
   echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | node server.js
   ```

2. **Check for connection errors** in `/mcp` output

### Environment variables not passed

- Only explicitly configured env vars are passed to stdio servers
- System env vars are NOT inherited (security measure)
- Add required vars to the `env` config block

### Server crashes on startup

- Check stderr output from the server
- Ensure all dependencies are installed
- Verify the working directory (`cwd`) is correct

## Security Considerations

1. **Environment Isolation**: MCP servers only receive explicitly configured env vars
2. **Input Validation**: Always validate tool inputs before execution
3. **Principle of Least Privilege**: Only expose necessary tools
4. **Secrets Management**: Use env vars for API keys, never hardcode

## API Reference

### MCP SDK

```bash
npm install @modelcontextprotocol/sdk
```

Key imports:
```typescript
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
  ListResourcesRequestSchema,
  ReadResourceRequestSchema,
  ListPromptsRequestSchema,
  GetPromptRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
```

### Deixic Code MCP Commands

| Command | Description |
|---------|-------------|
| `/mcp` | Show server status and tools |
| `/mcp resources` | List all resources |
| `/mcp resources <server> <uri>` | Read a specific resource |
| `/mcp prompts` | List all prompts |
| `/mcp prompts <server> <name>` | Get a specific prompt |

## Further Resources

- [MCP Specification](https://spec.modelcontextprotocol.io/)
- [MCP SDK Documentation](https://github.com/modelcontextprotocol/sdk)
- [Example MCP Servers](https://github.com/modelcontextprotocol/servers)
