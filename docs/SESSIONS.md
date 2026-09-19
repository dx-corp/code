# Session Storage & Continuation

Audience: users/operators managing session persistence.  
Nav: [Docs index](README.md) · [Quickstart](QUICKSTART.md) · [Safety](SAFETY.md) · [Features](FEATURES.md)

Deixic Code persists conversation history in JSONL files under
`~/.composer/agent/sessions/--<cwd>--`. Understanding the format helps when you
want to inspect, back up, or clean up sessions.

## Directory Layout

```
~/.composer/agent/
└─ sessions/
   └─ --Users-me-project--/
      ├─ 2025-01-15T18-05-23.982Z_<uuid>.jsonl
      └─ ...
```

The `--<cwd>--` naming is derived from your project path with slashes replaced
by dashes, so different repos never collide.

## JSONL Format

Each line is a JSON object describing a session event:

```json
{ "type": "session", "id": "uuid", "timestamp": "...", "cwd": "...", "model": "anthropic/claude-opus-4-6" }
{ "type": "message", "timestamp": "...", "message": { "role": "user", "content": "..." } }
{ "type": "thinking_level_change", "timestamp": "...", "thinkingLevel": "deep" }
{ "type": "model_change", "timestamp": "...", "model": "openai/gpt-4o", "modelMetadata": { ... } }
{ "type": "session_meta", "timestamp": "...", "summary": "..." }
```

Important types:

- `session` – header entry with model + cwd
- `message` – serialized `AppMessage` (user, assistant, tool result, etc.)
- `thinking_level_change` – records `/thinking` adjustments
- `model_change` – tracks mid-session provider/model switches
- `session_meta` – favorites, manual summaries, future metadata

## CLI Flags

| Flag            | Effect                                       |
| --------------- | --------------------------------------------- |
| `--continue`    | Load the most recent session for the cwd     |
| `--resume-session <id>` | Resume a saved session by ID |
| `--session path`| Use a specific JSONL file (absolute or relative) |
| `--no-session`  | Disable persistence entirely for this run    |

## Find and resume saved conversations

Open `/sessions`, or press Ctrl+Alt+R, and type a phrase from a saved
conversation. Search includes names, IDs, working directories, user messages,
assistant replies, and tool results. Matching conversations show an excerpt.
Search runs locally in the background; opening the picker does not send a
model request. Enter resumes the selected conversation. Escape closes the
picker and keeps your draft.
For very large messages, search covers the first 50,000 characters.

## Forks and session changes

Run `/fork` to save a branch of the current conversation. The parent stays
selected, and an active response keeps running. The result includes a command
you can copy to resume the fork:

```sh
deixic-code --resume-session <fork-id>
```

Run `/tree` to browse the current conversation's saved branches. In `/sessions`,
Ctrl+F switches between all conversations and the selected conversation's
branches. Each fork shows its parent's ID. Enter resumes the selection; Escape
returns to your draft. Branches remain available after restarting Deixic Code.

During an active response, `/new` and `/rewind <turns>` ask before stopping it.
Escape on the confirmation keeps the response running. Enter stops the response, waits for tool
cleanup, saves its final events, and then opens the new conversation or saved
rewind branch. Queued prompts return to the composer. If cleanup cannot be
confirmed, the current conversation stays selected and an error is shown.
Keeping the current conversation during cleanup does not undo cancellation.
Session changes remain blocked until cleanup is acknowledged, including on a retry.

`/rewind <turns>` keeps the original conversation and leaves files unchanged.
`/rewind <turns> --files` also restores the affected file checkpoints. Use
`--dry-run` to preview a rewind without changing the conversation or files.

## Secure Transfer

For moving a session family between trusted installations, use the explicit
signed/encrypted `secure-json` format. It redacts credentials before encryption
and requires operator-supplied key files; Maestro never stores or uploads those
keys:

```sh
maestro sessions export <session-id> session.secure.json \
  --format secure-json \
  --encryption-key-file /secure/path/recipient.key \
  --signing-key-file /secure/path/signer.pk8 \
  --recipient-key-id workstation-a \
  --signing-key-id operator-2026-08

maestro sessions import session.secure.json \
  --encryption-key-file /secure/path/recipient.key \
  --verify-key-file /secure/path/signer.pub \
  --recipient-key-id workstation-a
```

The full envelope, key, redaction, replay, and migration contract is in
[`secure-session-transfer.md`](protocols/secure-session-transfer.md).

## Hosted Computer handoff

When the active account has a managed hosted Computer connection, a running Computer
task can freeze a bounded workspace handoff for another same-tenant Computer thread.
Computer owns the workspace bytes, artifact authorization, immutable storage, and
SHA-256 package digest; Maestro only selects the source task, destination
thread, and explicit items:

```sh
maestro computer handoff create <source-task-id> <target-thread-id> \
  --file src/lib.rs --include-diff
maestro computer handoff list <target-thread-id>
maestro computer handoff read <target-thread-id> <package-id>
```

The TUI exposes the same controls as `/computer handoff ...`; `/orb` remains a
compatibility alias. Handoffs are remote
and tenant-scoped: Maestro does not upload arbitrary dirty local files, copy
credentials, or create a local shadow package. A missing or changed managed
Computer owner binding fails closed.

`/handoff` sends work to the default A2A peer and follows the remote task
without blocking the TUI. The full command remainder is the prompt, so quotes
are optional:

```text
/handoff check the release queue and report blockers
```

Use `/handoff --peer <name> <prompt>` to override the default peer. When the
selected peer has a configured session identity, the handoff resumes that
persistent Maestro session. Otherwise it receives a new context-specific
binding.

When the work needs bytes from an existing hosted Computer task, create and
attach the Computer-owned package in the same command:

```text
/handoff --source-task <source-task-id> --target-thread <target-thread-id> \
  --file src/lib.rs --include-diff -- continue the implementation
```

The source task ID and target thread ID are explicit because an A2A session ID
is not a hosted Computer thread ID. The package form fails before sending the
A2A task if Computer cannot create or validate the package reference.

The TUI also offers `/sessions` to list + load by index. When loading, the agent
replays the stored messages into its state and restores model/thinking settings.

## Favorites & Summaries

`session_meta` entries can include `favorite: true` or a `summary` string. Add
these without touching the JSONL by using:

- `/session favorite` or `/session unfavorite` to toggle the active session
- `/session summary "<text>"` to attach a manual blurb to the active session
- `/sessions summarize <number>` to auto-summarize a saved session by index

## Cleaning Up

- Delete the `--<cwd>--` directory to wipe all sessions for a repo.
- Use `--no-session` in CI or ephemeral workspaces to avoid clutter.

Future enhancements (continuous context, shared KBs) will reuse this directory,
so keep it tidy but don’t remove unrelated files under `~/.composer/agent/`.
