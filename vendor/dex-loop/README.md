# dex-loop

The one Dex agent loop. Web, Slack, Teams and every later surface run the same
engine over the same event log; a surface only writes ingress events and
renders the stream.

## The loop

```text
host appends UserMessage { principal, text }
ctx = rehydrate(thread, log)                  // warm or after a crash: same path
loop {
    read control events (steer, interrupt, approval decisions, answers)
    StepStarted -> stream the model -> text to the log as it arrives
    ModelStepCompleted { text, calls }        // commit point, before any policy
    no calls?  Final, return Done
    per call, in the model's order:
        policy (under the call's principal) -> deny? record refusal, continue
        NeedsApproval? record AutoApproved before dispatch
        NeedsConfirmation? return preview as a refusal result; dispatch nothing
        read-only: join the parallel wave     // results enter history in call order
        mutation:  Effects claim -> ToolStarted -> run -> record -> ToolFinished
}
```

Ordinary `NeedsApproval` verdicts are granted immediately and recorded as
`AutoApproved` before the effect. Deterministic denials and unavailable policy
checks still deny. In the hosted `dex-tools` policy, guardian objections block
connector writes and flag internal calls on their receipts. Major destructive
connector actions require a typed confirmation bound to the exact call;
headless turns deny them. Interactive calls carrying an explicit confirmation
checkpoint also use that question path. See
[`DexTools::policy`](../dex-tools/src/lib.rs) and
[policy checks](../dex-tools/src/policy.rs).

`Engine::run(&mut ctx, &cancel)` returns `Done`, `Asked(call)`,
`AwaitingClientTool(call)`, `Interrupted`, or `Failed` on active paths.
`Parked(approval)` remains a compatibility variant; ordinary approval verdicts
do not park. After a question answer or client-tool result, the host appends
the event and runs again; pending calls come from the log, not memory.

## Ports

| Port | Job |
| --- | --- |
| `Log` | Append events (fenced by the thread lease), coalesce text into `TextDelta` rows, read control events after a cursor. |
| `Model` | Stream one response for the history and the offered tool schemas. |
| `Tools` | Catalog, `search`, `policy` (governance, grants, guardrails, guardian) and `run`. |
| `Effects` | The durable ledger for mutations: claim before dispatch, record after. A mutation is dispatched at most once per call id; an unknown outcome is never replayed. |
| `Sanitizer` | Customer-safe text: replaces tool names and internal names before any delta is written. `Lexicon` is the built-in implementation. |
| `Compactor` | Optional. `Threshold` summarizes old history when it grows past a size. |

The engine offers `tools.search` plus core tools on every step; tools that
search matches are exposed from the next step and recorded as `ToolsExposed`.

## Events

Ingress (hosts write): `UserMessage`, `Steer`, `Interrupt`, `ApprovalDecided`,
`Answer`, `ToolProgress`. Each ingress event carries its principal.

Engine: `StepStarted`, `TextDelta`, `Usage`, `ModelStepCompleted`,
`ModelAttemptAbandoned`, `ToolStarted`, `ToolsExposed`, `ToolFinished`
(`succeeded | failed | running | unknown`), `AutoApproved`,
`Question`, `ClientToolRequested`,
`Compaction`, `Final`, `Error`, `Interrupted`.

Interrupt cancels the model stream and running reads; a mutation that has
started completes, and the turn stops before the next effect.

## What this crate will never contain

- Surface logic: no Slack, Teams, web or renderer code, and no per-surface
  behavior in the loop.
- Coding concepts: no coding task kinds, validation rules or workflows. Coding is
  tools run through this same loop.
- Provider or service clients: no HTTP, SQL, model SDKs or `tokio::spawn`.
  Hosts implement the ports.

## Checks

```bash
cargo test --manifest-path rust/Cargo.toml -p dex-loop
cargo clippy --manifest-path rust/Cargo.toml -p dex-loop --all-targets -- -D warnings
```
