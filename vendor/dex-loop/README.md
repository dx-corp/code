# dex-loop

`dex-loop` implements the shared Dex agent loop. Hosts append user and control
events to a thread's log, supply model and tool adapters, and render the
resulting event stream.

The crate performs no I/O and spawns no tasks. Hosts implement the ports for
storage, inference, and tool execution. Surface integrations and coding-specific
tools live outside this crate.

## Execution

The host appends a `UserMessage`, rebuilds the context with `rehydrate`, and
calls `Engine::run(&mut ctx, &cancel)`. The same context reconstruction path
handles an active thread and recovery after a crash.

The main dispatch sequence is:

```text
loop {
    read control events (steer, interrupt, approval decisions, answers)
    StepStarted -> stream the model -> text to the log as it arrives
    ModelStepCompleted { text, calls }        // persisted model response
    no calls?  Final, return Done
    per call, in the model's order:
        policy (under the call's principal) -> deny? record refusal, continue
        NeedsApproval? record AutoApproved before dispatch
        NeedsConfirmation? return preview as a refusal result; dispatch nothing
        read-only: join the parallel wave     // results enter history in call order
        mutation:  Effects claim -> ToolStarted -> run -> record -> ToolFinished
}
```

Allowed reads can run concurrently; their results enter history in the model's
call order. Mutations use the effect ledger before dispatch and after execution.
See the [engine](src/engine.rs) and [context reconstruction
logic](src/rehydrate.rs).

### Policy and confirmation

Ordinary `NeedsApproval` verdicts are granted immediately and recorded as
`AutoApproved` before the effect. Deterministic denials and unavailable policy
checks still deny. In the hosted `dex-tools` policy, a guardian denial blocks
the reviewed call. Major destructive connector actions and saved artifact
revisions require typed confirmation bound to the exact call; headless turns
deny them. Interactive calls carrying an explicit confirmation checkpoint also
use that question path. See [`DexTools::policy`](../tools/src/lib.rs) and
[policy checks](../tools/src/policy.rs).

### Return states

`Engine::run` returns `Result<Exit, Fenced>`. A fenced write stops the engine;
the next lease holder rebuilds the context from durable events.

| Exit | Meaning |
| --- | --- |
| `Done` | The model answered without tool calls. |
| `Asked(call)` | Waiting for an `Answer` event for the call. |
| `AwaitingClientTool(call)` | Waiting for a `ClientToolResult` event for the call. |
| `Interrupted` | The turn was interrupted. |
| `Failed` | The engine appended an `Error` event. |
| `Parked(approval)` | Legacy compatibility variant; the current engine does not return it. |

After an answer or client-tool result, the host appends the event and calls
`run` again. Pending calls are reconstructed from the log.

Interrupts cancel the model stream and signal cancellation to running tools. The
engine waits for an in-flight mutation to return or reach its deadline, records
the result or an unknown outcome, and stops before the next effect. A tool that
honors cancellation may return a partial outcome.

## Host ports

The host supplies these interfaces, defined in [ports.rs](src/ports.rs) and
[compaction.rs](src/compaction.rs):

| Port | Job |
| --- | --- |
| `Log` | Append events (fenced by the thread lease), coalesce text into `TextDelta` rows, read control events after a cursor. |
| `Model` | Stream one response for the history and the offered tool schemas. |
| `Tools` | Catalog, `search`, `policy` (governance, grants, guardrails, guardian) and `run`. |
| `Effects` | Claim mutations before dispatch and record their results. Mutation dispatch is at most once per call ID within a thread; an unknown outcome is never replayed. |
| `Sanitizer` | Filter customer-visible text before writing deltas. `Lexicon` replaces tool names and internal names. |
| `Compactor` | Optionally compact older history. `Threshold` triggers summarization when history exceeds a configured size. |

The engine offers `tools.search` plus core tools on every step; tools that
search matches become available on the next step and are recorded as
`ToolsExposed`.

## Events

The [event definitions](src/event.rs) describe the persisted log vocabulary:

- Hosts write ingress events such as `UserMessage`, `Steer`, `Interrupt`,
  `ApprovalDecided`, `Answer`, `ClientToolResult`, and `ToolProgress`.
- The engine writes model steps, text deltas, usage, tool lifecycle events,
  approval receipts, questions, client-tool requests, compaction, and terminal
  events. `ToolFinished` records `succeeded`, `failed`, `running`, or `unknown`.

## Shared host recovery contracts

Host dev-dependencies can enable the `testing` feature and implement
[`testing::RecoveryHost`](src/testing/mod.rs) using their real `Log` and
`Effects` adapters. Run each `RecoveryScenario` with a fresh thread. `restart`
reopens durable storage and acquires a new log generation; it must fence the old
handle. The suite covers lost leases, crashes on either side of the dispatch
boundary, exact recorded result replay, duplicate claims, and cancellation that
persists the partial outcome. `testing::tenant_isolation` uses matching
thread/call IDs in separate organization and workspace scopes over shared
storage.

The [filesystem host
tests](../../../../products/maestro/packages/dex-host-rs/tests/recovery_contract.rs)
and [PostgreSQL host
tests](../../../services/dex-runtime/tests/postgres/recovery_contract.rs) run
the same assertions. The PostgreSQL tests use `PgLog` and `DexEffects` over
`PgLedger`. They require PostgreSQL and refuse external calls. The model and
mutation are deterministic probes. These tests prove port recovery and persisted
replay; they do not prove process-kill/fsync durability, live provider
reconciliation, or cross-process locking for the single-process local effect
ledger.

Run these commands from the repository root after the [build-capacity
check](../../../../docs/testing-ci.md):

```bash
cargo test --manifest-path products/maestro/Cargo.toml --locked -p maestro-dex-host --test recovery_contract
scripts/ci/run-with-postgres cargo test --manifest-path rust/Cargo.toml --locked -p dex-runtime --test suite postgres::recovery_contract
```

## Checks

From the repository root, after the same build-capacity check:

```bash
cargo test --manifest-path rust/Cargo.toml -p dex-loop
cargo clippy --manifest-path rust/Cargo.toml -p dex-loop --all-targets -- -D warnings
```
