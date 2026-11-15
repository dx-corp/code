# Skill Cookbook

## Minimal Skill

```bash
maestro skill new reviewing-prs --description "Review pull requests. Use when the user asks for PR review."
maestro skill lint .maestro/skills/reviewing-prs
```

The generated package includes:

```text
reviewing-prs/
  SKILL.md
  reference/overview.md
  scripts/README.md
  toolbox/README.md
  mcp.json.example
```

## First-Party Operational Skills

Maestro ships three system skills for common EvalOps operator workflows:

- `pr-review`: reviews pull requests for correctness, regressions, missing tests,
  and merge readiness.
- `release-verification`: checks release readiness and post-release health from
  CI, tags, deploy evidence, rollback notes, and operator artifacts.
- `incident-triage`: builds an incident timeline, scopes blast radius, identifies
  mitigation, and preserves evidence.

Each package includes `SKILL.md`, scoped `reference/` guidance, bounded
`mcp.json`, and executable `toolbox/` helpers for Unix and Windows shells. Use `maestro skill eval <package-path>` to inspect their package checks.
The current `package.json` has no `evals:skill-package` script.

## Bundled MCP Server

Copy `mcp.json.example` to `mcp.json` and keep the exposed tools filtered:

```json
{
  "github": {
    "command": "npx",
    "args": ["-y", "@modelcontextprotocol/server-github"],
    "includeTools": ["get_pull_request", "list_pull_request_files"]
  }
}
```

`maestro skill lint` fails packages that omit `includeTools`.

## Reference Files

Put long examples and troubleshooting notes under `reference/`. Keep `SKILL.md`
short enough for the agent to load quickly; references are Level 3 context and
should be read only when the task needs them.

## Toolbox Executables

Put executable tool commands under `toolbox/`. A toolbox command should describe
itself when `MAESTRO_TOOLBOX_ACTION=describe` is set:

```bash
MAESTRO_TOOLBOX_ACTION=describe .maestro/skills/reviewing-prs/toolbox/list-pr-files
```

Validate packages with:

```bash
maestro skill lint .maestro/skills/reviewing-prs
```

If a toolbox helper is shell-specific, include a same-stem Windows companion such
as `list-pr-files.cmd` or `list-pr-files.ps1`. The linter validates the runnable
entry for the target platform and ignores the sibling meant for the other shell.

## Runtime Activation

Loading a skill through the `Skill` tool returns a `skillRuntimeActivation`
object in the tool result details. The same shape is available locally:

```bash
maestro skill inspect reviewing-prs --json
```

Use this manifest in harnesses and adapters when you need to see which
references, toolbox executables, MCP servers, and tool bounds become active. MCP
environment values remain in `mcp.json`; they are not copied into the activation
manifest. Servers with missing or malformed `includeTools` are reported as
warnings and omitted from the available server list.

## Eval Harness

Use `maestro skill eval` when a package needs pass/fail evidence rather than
plain lint output:

```bash
maestro skill eval .maestro/skills/reviewing-prs --describe-toolbox
maestro skill eval .maestro/skills/reviewing-prs --json
```

The eval harness scores each package against the local Agent Core constraints:
loadable `SKILL.md`, bounded MCP `includeTools`, runnable toolbox entries, and
the progressive-disclosure budget. Evaluate the selected package with:

```bash
maestro skill eval .maestro/skills/reviewing-prs --json
```

## OSS Publish And Install

Distribute OSS skill packs as normal npm or git packages with this
`package.json` shape:

```json
{
  "name": "@acme/maestro-review-skills",
  "version": "1.0.0",
  "keywords": ["maestro-package", "maestro-skill-package"],
  "maestro": {
    "skills": ["./skills"]
  }
}
```

Before publishing, run the contract check from the package root or a consumer
workspace:

```bash
maestro skill publish-check ./packages/review-skills --describe-toolbox
maestro skill publish-check ./packages/review-skills --json
```

The emitted install hint mirrors the checked source. Local checks print a
`local:` command, git checks print the stable `git:` specification, and npm checks print
the npm install specification rather than inferring one from package metadata.

Consumers install through the same package runtime used by the TUI package
commands:

```bash
maestro skill install npm:@acme/maestro-review-skills@1.0.0 --scope user
maestro skill install git:github.com/acme/maestro-review-skills@v1.0.0 --scope project
maestro skill install ./packages/review-skills --scope local
```

`maestro skill install` validates the publish contract first, writes the package
source into the selected config scope, and leaves toolbox execution disabled
during validation unless `publish-check --describe-toolbox` is used explicitly.

## Model And Mode Hints

Skills can declare model and mode preferences:

```yaml
model: gpt-5.5
mode: review
isolatedContext: true
```

These fields are advisory until the runtime can enforce skill-scoped dispatch.
