# MCP Protocol Integration Design

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. MCP integration now lives in `packages/local-host-rs/src/mcp/`. Some file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.


The Model Context Protocol (MCP) integration enables dynamic tool loading from external servers, extending Maestro's capabilities through a standardized interface.

## Overview

MCP provides:

- **Dynamic Tool Discovery**: Automatic tool registration from servers
- **Standardized Protocol**: JSON-RPC based communication
- **Server Management**: Lifecycle management for MCP processes
- **Tool Bridging**: Seamless integration with Maestro's tool system

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                       MCP Architecture                               │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                    MCP Manager                               │    │
│  │  - Server discovery                                         │    │
│  │  - Process lifecycle                                        │    │
│  │  - Tool registration                                        │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                              │                                       │
│           ┌──────────────────┼──────────────────┐                   │
│           ▼                  ▼                  ▼                   │
│  ┌────────────────┐  ┌────────────────┐  ┌────────────────┐        │
│  │ MCP Server 1   │  │ MCP Server 2   │  │ MCP Server N   │        │
│  │ (filesystem)   │  │ (git)          │  │ (custom)       │        │
│  └────────────────┘  └────────────────┘  └────────────────┘        │
│         │                   │                   │                   │
│         └───────────────────┼───────────────────┘                   │
│                             ▼                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                    Tool Bridge                               │    │
│  │  - MCP tool → Maestro tool conversion                      │    │
│  │  - Schema translation                                       │    │
│  │  - Result mapping                                           │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                             │                                       │
│                             ▼                                       │
│                    ┌────────────────┐                               │
│                    │  Agent Tools   │                               │
│                    └────────────────┘                               │
└─────────────────────────────────────────────────────────────────────┘
```

## Configuration

### Server Configuration File

```json
// ~/.maestro/mcp.json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/user/projects"],
      "env": {}
    },
    "git": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-git"],
      "cwd": "/home/user/projects/myapp"
    },
    "custom": {
      "command": "./my-mcp-server",
      "args": ["--config", "config.json"],
      "env": {
        "API_KEY": "${env.MY_API_KEY}"
      }
    }
  }
}
```

### Server Configuration Interface

```typescript
// src/mcp/types.ts
interface McpServerConfig {
  command: string;
  args?: string[];
  cwd?: string;
  env?: Record<string, string>;
  disabled?: boolean;
}

interface McpConfig {
  mcpServers: Record<string, McpServerConfig>;
}
```

## Server Management

### MCP Manager

```typescript
// src/mcp/manager.ts
class McpManager {
  private servers: Map<string, McpServer> = new Map();
  private tools: Map<string, McpTool> = new Map();

  async loadConfig(): Promise<void> {
    const configPath = join(homedir(), ".maestro", "mcp.json");
    const config = JSON.parse(await readFile(configPath, "utf8"));

    for (const [name, serverConfig] of Object.entries(config.mcpServers)) {
      if (serverConfig.disabled) continue;
      await this.startServer(name, serverConfig);
    }
  }

  async startServer(name: string, config: McpServerConfig): Promise<void> {
    const server = new McpServer(name, config);
    await server.start();
    this.servers.set(name, server);

    // Discover and register tools
    const tools = await server.listTools();
    for (const tool of tools) {
      const bridgedTool = this.bridgeTool(name, tool);
      this.tools.set(bridgedTool.name, bridgedTool);
    }
  }

  async stopServer(name: string): Promise<void> {
    const server = this.servers.get(name);
    if (server) {
      await server.stop();
      this.servers.delete(name);

      // Remove associated tools
      for (const [toolName, tool] of this.tools) {
        if (tool.serverName === name) {
          this.tools.delete(toolName);
        }
      }
    }
  }

  getTools(): McpTool[] {
    return Array.from(this.tools.values());
  }
}
```

### MCP Server Process

```typescript
// src/mcp/server.ts
class McpServer {
  private process: ChildProcess | null = null;
  private client: McpClient | null = null;
  private ready = false;

  constructor(
    public readonly name: string,
    private config: McpServerConfig
  ) {}

  async start(): Promise<void> {
    // Interpolate environment variables
    const env = this.interpolateEnv(this.config.env ?? {});

    // Spawn server process
    this.process = spawn(this.config.command, this.config.args ?? [], {
      cwd: this.config.cwd,
      env: { ...process.env, ...env },
      stdio: ["pipe", "pipe", "pipe"]
    });

    // Initialize JSON-RPC client
    this.client = new McpClient(
      this.process.stdin!,
      this.process.stdout!
    );

    // Wait for initialization
    await this.client.initialize();
    this.ready = true;

    // Handle process exit
    this.process.on("exit", (code) => {
      logger.info(`MCP server ${this.name} exited with code ${code}`);
      this.ready = false;
    });
  }

  async stop(): Promise<void> {
    if (this.process) {
      this.process.kill("SIGTERM");
      await new Promise((resolve) => {
        this.process?.on("exit", resolve);
        setTimeout(resolve, 5000);  // Timeout
      });
      this.process = null;
    }
  }

  async listTools(): Promise<McpToolDefinition[]> {
    if (!this.client) throw new Error("Server not started");
    return await this.client.request("tools/list", {});
  }

  async callTool(
    name: string,
    arguments_: Record<string, unknown>
  ): Promise<McpToolResult> {
    if (!this.client) throw new Error("Server not started");
    return await this.client.request("tools/call", { name, arguments: arguments_ });
  }

  private interpolateEnv(env: Record<string, string>): Record<string, string> {
    const result: Record<string, string> = {};
    for (const [key, value] of Object.entries(env)) {
      result[key] = value.replace(/\$\{env\.(\w+)\}/g, (_, varName) =>
        process.env[varName] ?? ""
      );
    }
    return result;
  }
}
```

## JSON-RPC Client

```typescript
// src/mcp/client.ts
class McpClient {
  private messageId = 0;
  private pendingRequests: Map<number, {
    resolve: (result: unknown) => void;
    reject: (error: Error) => void;
  }> = new Map();

  constructor(
    private stdin: Writable,
    private stdout: Readable
  ) {
    this.setupReader();
  }

  private setupReader(): void {
    let buffer = "";

    this.stdout.on("data", (data) => {
      buffer += data.toString();

      // Process complete JSON-RPC messages
      let newlineIndex;
      while ((newlineIndex = buffer.indexOf("\n")) !== -1) {
        const line = buffer.slice(0, newlineIndex);
        buffer = buffer.slice(newlineIndex + 1);

        try {
          const message = JSON.parse(line);
          this.handleMessage(message);
        } catch (error) {
          logger.error("Failed to parse MCP message", error);
        }
      }
    });
  }

  private handleMessage(message: JsonRpcMessage): void {
    if ("id" in message && message.id !== undefined) {
      const pending = this.pendingRequests.get(message.id);
      if (pending) {
        this.pendingRequests.delete(message.id);

        if ("error" in message) {
          pending.reject(new Error(message.error.message));
        } else {
          pending.resolve(message.result);
        }
      }
    }
  }

  async request<T>(method: string, params: unknown): Promise<T> {
    const id = ++this.messageId;

    return new Promise((resolve, reject) => {
      this.pendingRequests.set(id, {
        resolve: resolve as (result: unknown) => void,
        reject
      });

      const message = JSON.stringify({
        jsonrpc: "2.0",
        id,
        method,
        params
      });

      this.stdin.write(message + "\n");

      // Timeout
      setTimeout(() => {
        if (this.pendingRequests.has(id)) {
          this.pendingRequests.delete(id);
          reject(new Error(`MCP request timeout: ${method}`));
        }
      }, 30000);
    });
  }

  async initialize(): Promise<void> {
    await this.request("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {
        tools: {}
      },
      clientInfo: {
        name: "maestro",
        version: "0.10.0"
      }
    });

    await this.notify("initialized", {});
  }

  async notify(method: string, params: unknown): Promise<void> {
    const message = JSON.stringify({
      jsonrpc: "2.0",
      method,
      params
    });

    this.stdin.write(message + "\n");
  }
}
```

## Tool Bridging

### Tool Schema Translation

```typescript
// src/mcp/bridge.ts
function bridgeTool(serverName: string, mcpTool: McpToolDefinition): AgentTool {
  const toolName = `mcp__${serverName}__${mcpTool.name}`;

  return createTool({
    name: toolName,
    description: mcpTool.description,
    schema: convertJsonSchemaToTypebox(mcpTool.inputSchema),
    annotations: {
      readOnlyHint: mcpTool.annotations?.readOnlyHint,
      destructiveHint: mcpTool.annotations?.destructiveHint
    },
    run: async (params, context) => {
      const server = mcpManager.getServer(serverName);
      if (!server) {
        throw new Error(`MCP server not found: ${serverName}`);
      }

      const result = await server.callTool(mcpTool.name, params);
      return mapMcpResult(result, context.respond);
    }
  });
}
```

### Result Mapping

```typescript
function mapMcpResult(
  result: McpToolResult,
  respond: ToolResponseBuilder
): AgentToolResult {
  for (const content of result.content) {
    if (content.type === "text") {
      respond.text(content.text);
    } else if (content.type === "image") {
      respond.image(content.data, content.mimeType);
    }
  }

  if (result.isError) {
    respond.error(result.content[0]?.text ?? "MCP tool error");
  }

  return respond.build();
}
```

### Schema Conversion

```typescript
// src/mcp/schema.ts
function convertJsonSchemaToTypebox(jsonSchema: JsonSchema): TSchema {
  switch (jsonSchema.type) {
    case "string":
      return Type.String({
        description: jsonSchema.description,
        pattern: jsonSchema.pattern,
        minLength: jsonSchema.minLength,
        maxLength: jsonSchema.maxLength
      });

    case "number":
    case "integer":
      return Type.Number({
        description: jsonSchema.description,
        minimum: jsonSchema.minimum,
        maximum: jsonSchema.maximum
      });

    case "boolean":
      return Type.Boolean({ description: jsonSchema.description });

    case "array":
      return Type.Array(
        convertJsonSchemaToTypebox(jsonSchema.items),
        { description: jsonSchema.description }
      );

    case "object":
      const properties: Record<string, TSchema> = {};
      for (const [key, value] of Object.entries(jsonSchema.properties ?? {})) {
        properties[key] = convertJsonSchemaToTypebox(value);
      }
      return Type.Object(properties, {
        description: jsonSchema.description,
        additionalProperties: jsonSchema.additionalProperties
      });

    default:
      return Type.Unknown();
  }
}
```

## Server Discovery

### Auto-Discovery

```typescript
// src/mcp/discovery.ts
async function discoverMcpServers(): Promise<McpServerConfig[]> {
  const discovered: McpServerConfig[] = [];

  // Check standard locations
  const searchPaths = [
    join(homedir(), ".maestro", "mcp-servers"),
    join(process.cwd(), ".mcp-servers"),
    "/usr/local/share/maestro/mcp-servers"
  ];

  for (const searchPath of searchPaths) {
    if (!existsSync(searchPath)) continue;

    const entries = await readdir(searchPath, { withFileTypes: true });
    for (const entry of entries) {
      if (entry.isFile() && entry.name.endsWith(".json")) {
        const config = JSON.parse(
          await readFile(join(searchPath, entry.name), "utf8")
        );
        discovered.push(config);
      }
    }
  }

  return discovered;
}
```

### NPX Package Discovery

```typescript
async function discoverNpxServers(): Promise<McpServerConfig[]> {
  const knownServers = [
    { name: "filesystem", package: "@modelcontextprotocol/server-filesystem" },
    { name: "git", package: "@modelcontextprotocol/server-git" },
    { name: "sqlite", package: "@modelcontextprotocol/server-sqlite" }
  ];

  const available: McpServerConfig[] = [];

  for (const server of knownServers) {
    try {
      // Check if package is available
      await exec(`npm view ${server.package} version`);
      available.push({
        command: "npx",
        args: ["-y", server.package]
      });
    } catch {
      // Package not found, skip
    }
  }

  return available;
}
```

## Error Handling

### Server Errors

```typescript
class McpServerError extends Error {
  constructor(
    public readonly serverName: string,
    message: string,
    public readonly code?: number
  ) {
    super(`MCP server '${serverName}': ${message}`);
    this.name = "McpServerError";
  }
}

class McpToolError extends Error {
  constructor(
    public readonly serverName: string,
    public readonly toolName: string,
    message: string
  ) {
    super(`MCP tool '${serverName}/${toolName}': ${message}`);
    this.name = "McpToolError";
  }
}
```

### Retry Logic

```typescript
async function callToolWithRetry(
  server: McpServer,
  toolName: string,
  args: Record<string, unknown>,
  maxRetries = 3
): Promise<McpToolResult> {
  let lastError: Error | null = null;

  for (let attempt = 0; attempt < maxRetries; attempt++) {
    try {
      return await server.callTool(toolName, args);
    } catch (error) {
      lastError = error as Error;

      // Don't retry for certain errors
      if (error.message.includes("Invalid arguments")) {
        throw error;
      }

      // Exponential backoff
      await new Promise(resolve =>
        setTimeout(resolve, Math.pow(2, attempt) * 1000)
      );
    }
  }

  throw lastError;
}
```

## Monitoring

### Server Health

```typescript
class McpHealthMonitor {
  async checkHealth(server: McpServer): Promise<{
    healthy: boolean;
    latencyMs: number;
    error?: string;
  }> {
    const start = Date.now();

    try {
      await server.client.request("ping", {});
      return {
        healthy: true,
        latencyMs: Date.now() - start
      };
    } catch (error) {
      return {
        healthy: false,
        latencyMs: Date.now() - start,
        error: error.message
      };
    }
  }

  startMonitoring(interval = 30000): void {
    setInterval(async () => {
      for (const [name, server] of mcpManager.servers) {
        const health = await this.checkHealth(server);
        if (!health.healthy) {
          logger.warn(`MCP server unhealthy: ${name}`, health);
          // Attempt restart
          await mcpManager.restartServer(name);
        }
      }
    }, interval);
  }
}
```

## Related Documentation

- [Tool System](TOOL_SYSTEM.md) - Tool integration
- [Safety & Firewall](SAFETY_FIREWALL.md) - MCP tool permissions
- [Hooks System](HOOKS_SYSTEM.md) - MCP tool hooks
