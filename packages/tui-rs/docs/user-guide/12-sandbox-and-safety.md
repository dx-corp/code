# Sandbox and Safety

Deixic Code runs tools on your machine. Safety layers include the action firewall, approval modes, optional OS sandboxing, and safe mode. Canonical detail: [Safety](../../../../docs/SAFETY.md) and [Threat Model](../../../../docs/THREAT_MODEL.md).

---

## Approval modes (TUI)

| Command | Mode | Behavior |
|---------|------|----------|
| `/always-approve` (`/yolo`) | YOLO | Auto-approve tool executions |
| `/auto` | Selective | Safe tools free; risky tools prompt |
| `/ask` | Safe | Require approval for all tools |
| `/approvals [yolo\|selective\|safe]` | explicit / cycle | Set or advance mode |

CLI / env:

| Mode | Behavior |
|------|----------|
| `prompt` (default) | Ask in TUI; fail headless unless configured |
| `auto` | Auto-approve |
| `fail` | Reject high-risk commands |

```bash
maestro --approval-mode prompt
export MAESTRO_APPROVAL_MODE=auto
export MAESTRO_SAFE_MODE=1
```

---

## Action firewall

The firewall inspects tool names and arguments before execution. Default high-risk bash patterns include:

| Rule idea | Pattern family |
|-----------|----------------|
| Destructive delete | `rm -rf` variants |
| Filesystem format | `mkfs` |
| Disk wipe | `dd if=/dev/zero` |
| Permission lockout | `chmod 0000` variants |

Matches typically require approval. Extend carefully; do not disable wholesale outside trusted sandboxes.

### Bash guard

| `MAESTRO_BASH_GUARD` | Effect |
|----------------------|--------|
| unset | Guard on (default) |
| `1` / `on` / `true` | Force on |
| `0` / `off` / `false` | YOLO-ish: skip tree-sitter guard; hard regex rules remain |

### Bash allowlist

`~/.maestro/bash-allow.json` or `.maestro/bash-allow.json`:

```json
{
  "allow": [
    "git status",
    "npm run build"
  ]
}
```

Also: `MAESTRO_BASH_ALLOWLIST_PATHS`.

### Egress

`MAESTRO_NO_EGRESS_SHELL=1` requires approval for curl/wget/ssh/nc-style shell. Override with `MAESTRO_ALLOW_EGRESS_SHELL=1` or allowlists.

---

## Sandbox modes

| Mode | Description |
|------|-------------|
| `none` / `local` | Tools on host (default / minimal isolation) |
| `native` | OS-native sandbox when implemented |
| `docker` | Full isolation in a container |

```bash
maestro --sandbox docker
export MAESTRO_SANDBOX_MODE=docker
```

`.maestro/sandbox.json` example:

```json
{
  "mode": "docker",
  "docker": {
    "image": "node:20-slim",
    "workspaceMount": "/workspace"
  }
}
```

Platform enforcement (summary):

| Runtime | Platform | Native notes |
|---------|----------|--------------|
| Native Rust runtime | Linux | Landlock + seccomp backend |
| Native Rust runtime | macOS | Seatbelt path in sandbox module |

If `native` or `docker` cannot be enforced, Deixic Code fails closed unless unsafe fallback is explicitly enabled (`MAESTRO_ALLOW_UNSANDBOXED_SANDBOX_FALLBACK=1`).

---

## Production profile

- `MAESTRO_PROFILE=prod` — approval defaults to `fail`, stricter egress, safe-mode validators, and hardened web headers when applicable.

---

## Footer badges

Examples you may see: `approvals:auto|prompt|fail`, `sandbox:…`, `queue:…`, `think:medium`, `mcp:…`, `env:docker|ssh|…`.
