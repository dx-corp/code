# Native Codex restart replay fixtures

`restart.ndjson` is a deterministic, synthesized and scrubbed transcript. It
adapts the restart/resume scenario from T3 Code commit
`a5b34b25378fbbf90ce36ee101a11528ccf11c7e`, especially
`provider_thread_resume/codex_transcript.ndjson` and
`claude_subagent_resume_after_restart/claude_transcript.ndjson`. No customer
transcript, credentials, model reasoning, signatures, filesystem roots, or
provider installation identifiers were copied. Fixed session, call, item, and
thread identifiers and benign prompts replace original data.

The Codex paginated thread result, turn lifecycle, agent-message delta, and
completed item shapes follow the provider thread-resume recording. The
capability declaration, dynamic subagent/read calls, approval denial, and
transport-unavailable error are synthesized against Maestro's current native
wire contract. Claude's restart scenario contributes the child-identity
invariant only; this is not a Claude SDK replay or a recorded Codex subagent
transcript. There is no verified recorded missing-thread error in this corpus.

Tests use the existing Codex app-server client and turn-session adapter,
spawning the Rust test binary as a strict stdio transcript peer. Fresh parent
and keyed child sessions store real Codex thread bindings. Both processes and
turn-session adapters are recreated against those bindings; restored messages
must not be injected, accepted prompts must not be resubmitted, and the parent
addresses the same child identity. Ordered outbound checks and audit assertions
reject repeated tool replies or starts. A bounded child completion signal is
emitted only after stdin is exhausted; an explicit negative test sends an
extra action after the final wire fence and requires failure, preventing a
premature completion marker from masking replay. Streamed and completed assistant text
must reconcile to a single message. Approval requests pass through the native
server-request queue and receive one denial; no command executes in the fixture.

This covers transport parsing and persistent provider binding recreation. It
does not claim live provider operation, actual subagent scheduler recovery,
production command execution, or whole-actor session recovery.

Run from the repository root on a host that passes the build capacity guard:

```sh
cargo test --manifest-path products/maestro/Cargo.toml -p maestro-runtime provider_replay --locked
```

The ignored `provider_replay_subprocess` test is the child entry point and is
invoked by the replay tests, not manually in CI. Each parent test has a bounded
outer timeout; no sleeps, model calls, or external network services are needed.
A test-only loopback socket acknowledges bounded child completion.
