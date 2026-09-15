# Tool-output compression pilot

An opt-in evaluation of compiler-result representations in the installed native
Maestro runtime. This directory is Python evaluation tooling; it does not add a
Python production runtime or change any product default.

## Run

Requires Python 3, rustc, a working `maestro` executable, and its normal managed
model login. No third-party Python packages are required.

```sh
python3 -m unittest discover -s products/maestro/scripts/experiments/tool_compression -v
python3 products/maestro/scripts/experiments/tool_compression/trial.py \
  --output /absolute/path/to/new-results --count 8 --seed 88713 --workers 2 --live
```

Omit `--live` to generate compiler fixtures without inference. The output
directory must not exist. The live command incurs normal model usage charges.

## What is compared

- **baseline:** ordinary rendered rustc diagnostics, read through native tools.
- **compact:** group common diagnostic metadata; retain primary and secondary
  locations and labels, messages, codes, children, and explicit group counts. Omit source excerpts,
  expansion metadata, span suggestions, and generic `rustc --explain` essays.
- **delta:** compact prior observation, then added/removed diagnostic groups and
  current code counts. The controller owns both observations from the same case.
- **adaptive:** a counts-only overview; the model can retrieve full originals.

Every arm has the same question, system prompt, model, minimal thinking setting,
raw observations, and access to original results. Views that would expand the
original fall back to the original. Native tool reads incur real context, tool
round trips, and provider usage. They are not strings placed in a user prompt.
There is no separate summarizer-model cost: all projections are deterministic.

`codec.delta` also supplies a lossless indexed representation with a checked
baseline digest. `delta_view` is the human-readable, lossy view used by the live
trial; it compares retained diagnostic fields, not changed byte offsets. Neither
is installed as a session cache or a hosted protocol change.

## Grading and evidence

The corpus is generated from Rust source with known changes. Real rustc produces
both observations. Expected line changes and diagnostic counts derive from source
generation and are checked against rustc before inference. Models see only four
observation files in a temporary directory, not the source-generation manifest or
grading answers. This is conventional benchmark separation, not a container
security boundary. All observations are synthetic and contain no customer data.

The full task denominator, arm order, prompts, source hashes, executable hash,
model, and timeout are frozen before the first inference. Arm order rotates by
case. Each trial is a fresh runtime session. Content correctness requires the exact typed answer and a terminal event; a
single fenced JSON answer may have surrounding prose. Strict JSON-only compliance
is reported separately and still requires the entire response to match. Missing usage remains unavailable, never zero. The existing
`prompt_benchmark.paired_report` supplies paired correctness comparisons.
Infrastructure failures invalidate the comparison; successful trials are not
silently substituted for failed ones. The smoke runs are separate from the pilot.

Artifacts include original structured compiler output, source fixtures, hidden
expected answers, views, runtime events, individual receipts, and a report.
Provider usage is summed across every response, including retrieval round trips.
The monetary value is the runtime-reported cost, not a reconciled invoice.

## Limits

This is one synthetic diagnostic family, not SWE-bench or an end-to-end repository
repair evaluation. Eight related cases cannot establish general coding-quality
lift or statistical noninferiority. Views are precomputed before native tools read
them, so this measures adoption at the tool-result boundary without modifying the
installed binary. Compression CPU time, managed rollout, cross-session delta
lifecycle, tool-call argument aliases, executable routines, learned compression,
and negotiated multi-level budgets remain outside this pilot. Provider cache
state is uncontrolled; rotating arm order reduces but cannot eliminate that
confound. Use task success and total token use alongside cost and latency.

Do not promote a candidate from byte reduction alone. The report always sets
`promotion_allowed: false`.

## Native opt-in projection

The native utility accepts explicit observations; it never intercepts tool calls
or changes session defaults:

```sh
maestro diagnostics current.jsonl
maestro diagnostics current.jsonl --previous previous.jsonl --previous-sha256 <previous-current_sha256>
```

Inputs are rustc `--error-format=json` or Cargo `--message-format=json` JSONL,
limited to 4 MiB each. Keep the originals accessible. The output is a **lossy
view**, not a replacement for compiler artifacts: rendered source excerpts,
macro expansion metadata, and reference essays are omitted. Locations, labels,
suggestions, child diagnostics, multiplicity, and Cargo package/target identity
are retained. Byte offsets do not define diagnostic identity; line/column and
label changes do. A mismatching baseline digest is an error. `build_succeeded`
reports an explicit Cargo `build-finished` observation and is null otherwise;
absence of error diagnostics alone does not prove command success.

The output may be larger than rendered diagnostics when little information is
shared. In particular, reporting all removals after a successful repair can be
more expensive than a plain success result. Measure the whole task before
choosing this representation.

## End-to-end repair experiment

```sh
python3 products/maestro/scripts/experiments/tool_compression/repair.py \
  --binary /absolute/path/to/built/maestro \
  --output /absolute/path/to/new-repair-results --count 8 --workers 2 --live
```

Both arms run the same native binary and model on the same initial Rust source
and specification. The fixed `./check` script returns ordinary rendered rustc
output for baseline and the native projection for delta. Only `src/lib.rs` edits
and the fixed check command are approved. Original observations stay available.
The grader separately compiles the final library and controller-owned tests;
it verifies each expected named test passed, rejecting disabled tests and early
process exits. Check-script integrity is also verified. Raw headless events,
compiler observations, final source, hidden-test logs, source/binary hashes,
provider usage, and randomized run order are retained.

This small synthetic corpus covers repeated type errors plus unsigned arithmetic
edge cases. It tests completed repairs, not broad repository-level coding
ability. At least two compiler invocations are required for a valid comparison;
inspect event receipts to confirm successful view delivery and initial-before-edit
ordering. Infrastructure failures invalidate comparisons. A timeout is a failed
repair. Do not discard failures or claim a quality lift from equal pass counts.

The repair harness retries only identity-introspection timeouts that occurred
before any model usage or tool call, up to three startup attempts. Every attempt
is retained; inference failures and failed repairs are never retried. Reports
include startup attempts and wall time including retries. The prompt spells out
the exact allowed check command to avoid treating denied shell pipelines as a
compression effect. Successful view delivery and initial-before-edit source
hashes are checked automatically.
