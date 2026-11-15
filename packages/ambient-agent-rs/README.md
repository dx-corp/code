# Ambient Agent

A long-running GitHub agent that watches repositories, identifies work, and opens pull requests.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                            AMBIENT DAEMON                                     │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                               │
│  WATCHERS ──▶ EVENT BUS ──▶ DECIDER ──▶ CASCADER ──▶ EXECUTOR ──▶ CRITIC   │
│                                │                           │         │       │
│                                │                           ▼         ▼       │
│                                │                      CHECKPOINT    PR       │
│                                │                                    │        │
│                                └──────────────────────────────┐     │        │
│                                                               ▼     ▼        │
│                                                            LEARNER           │
│                                                               │              │
│                                                               ▼              │
│                                                           RETRAINER          │
└─────────────────────────────────────────────────────────────────────────────┘

Flow: WATCH → FILTER → DECIDE → PLAN → ROUTE → EXECUTE → CRITIQUE → PR → LEARN
```

## Core Philosophy

1. **PRs are the permission layer** - Agent can do anything, but nothing lands without human review
2. **Confidence-gated execution** - High confidence → act; low confidence → ask
3. **Learn from outcomes** - Merged PRs reinforce patterns; rejected PRs update priors
4. **Swarm for complexity** - Simple tasks = single agent; complex = spawn teammates

## Components

| Component | Purpose |
|-----------|---------|
| **EventBus** | Receives and normalizes events from GitHub (issues, PRs, comments) |
| **Decider** | Determines whether to act on an event and plans the task |
| **`Cascader`** | Routes tasks to appropriate model tier based on complexity |
| **Executor** | Executes tasks by calling language models and applying file changes |
| **FilePermissionPolicy** | Evaluates executor writes with ordered allow/ask/deny path rules |
| **PromptBuilder** | Renders system/user prompts from plans and bounded context |
| **Critic** | LLM-as-Judge that reviews agent outputs before shipping |
| **CheckpointManager** | Provides atomic operations with rollback capability |
| **Learner** | Records outcomes and learns from success/failure patterns |
| **IPC** | Unix socket communication between CLI and daemon |

## Prompt Iteration

Prompt rendering lives in `src/prompt.rs`, separate from executor I/O. Use
`PromptBuilder` tests for fast iteration on system rules, untrusted event-body
formatting, file-context limits, and deterministic current-year behavior before
running the full executor path.

## Confidence Thresholds

Ambient policy gates run before confidence thresholds. They block prompt-injection
signals and disabled capabilities, and they require human approval for protected
files, actions that are never safe to auto-execute, and work above the configured
complexity or file-count limits.

| Confidence | Action |
|------------|--------|
| ≥ 0.8 | Auto-execute (high confidence) |
| 0.5 - 0.8 | Ask human for approval |
| < 0.5 | Skip (too uncertain) |

## Frontier Model Routing

Ambient execution defaults to OpenRouter with the latest Claude Opus-family
frontier alias. This keeps long-running autonomous work on a model class suited
for multi-step repo changes while preserving explicit escape hatches for local
or Anthropic-direct testing.

| Tier | Default model | Provider | Use case |
|------|---------------|----------|----------|
| frontier | `~anthropic/claude-opus-latest` | OpenRouter chat completions | Ambient auto-execution, complex repo changes, security and architecture work |

Set `MAESTRO_AMBIENT_FRONTIER_MODEL` to pin a model such as
`anthropic/claude-opus-4.7` or `anthropic/claude-opus-4.5`. Set
`MAESTRO_AMBIENT_LLM_API=anthropic` only when intentionally testing the
Anthropic Messages API path.

## Build

From `products/maestro` in Mono, build this package and install the binary into
Cargo's configured bin directory:

```bash
cargo install --locked --path packages/ambient-agent-rs
```

Mono agents run Rust compilation on the
[approved build host](../../../../docs/development.md#rust-builds-for-agents).
For a build without installation, use `cargo build --locked -p ambient-agent --release`;
the `ambient` binary is in the configured workspace target directory.

## Usage

### Initialize configuration

```bash
ambient init
```

This creates `ambient.yaml` with default settings.

### Add repositories to watch

```bash
ambient watch owner/repo
```

### Start the daemon

```bash
ambient start
# Or run in foreground:
ambient start --foreground
```

### Check status

```bash
ambient status
ambient stats
```

### Stop the daemon

```bash
ambient stop
```

### List watched repositories

```bash
ambient list
```

### Remove a repository

```bash
ambient unwatch owner/repo
```

## Configuration

Edit `ambient.yaml`:

```yaml
repos:
  - name: owner/repo
    watchers:
      - issues
      - pull_requests

thresholds:
  auto_execute: 0.8  # Confidence threshold for auto-execution
  ask_human: 0.5     # Below this, ask for human approval

limits:
  max_complexity: medium
  max_files_changed: 20
  max_cost_per_task_usd: 5.0

capabilities:
  implement_features: true
  fix_bugs: true
  update_dependencies: true
  refactor: true
  add_tests: true
  update_docs: true
  security_patches: true

github_token: ghp_xxxxx  # Your GitHub token
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `OPENROUTER_API_KEY` | API key for the default OpenRouter frontier model path |
| `MAESTRO_AMBIENT_FRONTIER_MODEL` | Optional OpenRouter model override; defaults to `~anthropic/claude-opus-latest` |
| `MAESTRO_AMBIENT_LLM_API` | Optional provider override; set to `anthropic` for the Anthropic Messages API fallback |
| `ANTHROPIC_API_KEY` | API key for the Anthropic fallback path |
| `GITHUB_TOKEN` | GitHub personal access token |
| `MAESTRO_EVENT_BUS_URL` or `EVALOPS_NATS_URL` | NATS JetStream URL for publishing Ambient daemon session lifecycle CloudEvents |
| `MAESTRO_EVENT_BUS` | Set to `0` or `false` to disable audit-bus publishing even when bus routing is configured |

When bus routing is configured, the daemon publishes
`maestro.sessions.session.started`, `suspended`, `resumed`, and `closed`
events with `source=maestro.ambient-agent`. Plan routing and execution also
emit `maestro.ambient_agent.routing.selected`,
`maestro.ambient_agent.plan.cost_limited`, and
`maestro.ambient_agent.plan.completed` so Platform consumers and Maestro web can
join agent activity to model tier, cost, repository, and session correlation.

## Data Storage

By default, data is stored in:
- Linux: `~/.local/share/ambient-agent/`
- macOS: `~/Library/Application Support/ambient-agent/`
- Windows: `%APPDATA%\ambient-agent\`

Override with `--data-dir`:

```bash
ambient start --data-dir /path/to/data
```

## Development

### Run tests

```bash
cargo test --locked -p ambient-agent
```

### Run with debug logging

```bash
ambient --log-level debug start --foreground
```

## License

MIT
