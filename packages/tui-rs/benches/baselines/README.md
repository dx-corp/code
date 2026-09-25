# Perf baselines

Versioned per-platform JSON baselines for maestro-tui hot paths, adopted from
[xai-org/grok-build's pty-bench gate](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager-pty-harness/benches/pty_baselines/README.md).

CI compares the current run against the matching platform file and flags any
scenario that regresses by more than 15% (`--threshold` overrides). The gate is
**advisory**: the `perf-baselines` workflow warns and fails open, and is not a
required status check.

The separate `work-counts.json` records deterministic work for 96 scripted
text turns with a growing history. Each prompt is 365–366 ASCII bytes and each
response is 334–335 ASCII bytes. The fixture uses `ScriptedClient`, requires
96 completed turns and zero tool ends, and issues no tool calls or file-tool
dispatches.
`provider_history_vault_passes` counts full-history credential scans: 192
with two passes per provider request. The benchmark emits the fixture and
metric as JSON with `--work-counts-json`, so CI can enforce a fixed work ceiling
without relying on machine-dependent timing. The 32-tool-turn timing scenario
remains advisory because file-tool dispatch obscures this request-preparation
cost.

In four balanced candidate/control pairs on a busy 16-core Linux host (debug
build), the candidate's 96-turn wall-clock p50/p75/p95 was
32.49/34.18/35.73 seconds versus 36.00/37.09/39.57 seconds for the old
three-pass path. User CPU p50/p75/p95 was 22.15/23.19/25.04 seconds versus
26.01/26.35/26.70 seconds. All four paired wall and CPU results favored the
two-pass path. The p95 values are interpolated from only four samples; these
synthetic provider-loop timings do not measure user-visible latency. The old
path made 288 full-history vault passes on the same fixture, versus 192 now.

File naming: `<platform>.json` where `<platform>` is `<os>-<arch>` —
`linux-x86_64`, `linux-aarch64`, `macos-aarch64`.

## Scenarios

| Scenario | Hot path |
| --- | --- |
| `session_read_full` | `SessionReader::read_file` over a ~2k-entry JSONL session |
| `session_read_header` | `SessionReader::read_header` fast-scan of the same session |
| `session_wire_roundtrip` | `SessionEntry` JSONL serialize + parse roundtrip |
| `execpolicy_eval` | `Policy::check` over 500 parsed commands |
| `message_layout_steady` | Steady-state `ChatView` redraw over 1,000 messages |
| `model_selector_local_refresh_search` | Replace 100 discovered local rows, open the focused selector, and search |
| `agent_loop_32_turns` | 32 scripted text turns through `NativeAgent` |
| `agent_loop_32_tool_turns` | 32 scripted tool turns with varied, successful dispatches |
| `agent_loop_16_multi_tool_turns` | 16 scripted turns with two varied tool calls each |
| `agent_loop_96_long_history_turns` | 96 scripted turns with a growing transcript |

## Running the bench

```
cargo run -p maestro-tui --release --locked --features test-support --bin maestro-perf-bench
cargo run -p maestro-tui --locked --features test-support --bin maestro-perf-bench -- --work-counts-json
```

## Producing or refreshing a baseline

Run on a quiet machine of the target platform:

```
cargo run -p maestro-tui --release --locked --features test-support --bin maestro-perf-bench -- \
  --write-baseline packages/tui-rs/benches/baselines/<platform>.json
```

A PR that intentionally shifts a hot path (either direction) must refresh the
affected baselines and include the `maestro-perf-bench` output from a clean
run in the PR body so reviewers can sanity-check the new numbers.

## Comparing against a baseline

```
cargo run -p maestro-tui --release --locked --features test-support --bin maestro-perf-bench -- \
  --baseline packages/tui-rs/benches/baselines/<platform>.json
```

Exits 1 and prints the regressed scenarios when any slowdown exceeds the
threshold; a missing baseline file or a required scenario on either side fails
loudly with instructions.

## Notes

- Baselines are per-platform, not per-machine: numbers seeded on a fast dev
  box may drift on shared CI runners. That is tolerable while the gate is
  advisory; recalibrate with `--write-baseline` on representative hardware if
  the warnings get noisy.
- These scenarios are also covered by the binary's unit tests for the
  comparison logic (`cargo test -p maestro-tui --features test-support --bin maestro-perf-bench`).
