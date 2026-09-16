# Tool-runtime IR experiments

This offline suite tests three hypotheses independently of the native runtime:

- an up-front typed tool plan can expose safe cross-turn read parallelism beyond
  Maestro's existing same-turn read-only wave;
- speculative results can be admitted only on exact read/pure matches; and
- a revision-addressed Rust symbol projection can reduce localization payloads
  relative to literal ripgrep output.

Run the tests and experiment from the Mono repository root:

```sh
python3 -m unittest discover -s products/maestro/scripts/experiments/tool_runtime_ir -v
python3 products/maestro/scripts/experiments/tool_runtime_ir/run.py \
  --repo-root "$PWD" \
  --output /absolute/path/to/new-tool-runtime-ir-results.json
```

The output file must not already exist. The runner performs no network calls,
uses no model credentials, and scans only eligible Rust source below
`products/maestro/packages`. Fixture latency and token values are modeled;
repository response bytes and local timings are measured. The report always
sets `promotion_allowed` to false because offline replay cannot establish an
online task-success lift. The adversarial speculation fixture can pass its
safety gate, but it deliberately cannot pass the separate predictor-utility
gate.

The native follow-up, if justified, belongs in Maestro's turn-local runtime.
Platform remains authoritative for tenant policy, budgets, credentials,
privileged effect admission, durable evidence, and terminal acceptance.

## Native follow-up status

The symbol-graph result has been promoted behind the experimental
`repository_symbols` tool. Its Rust-only, revision-addressed index is built
lazily, refreshes incrementally, reports incomplete projections explicitly,
and stays out of initial tool profiles until selected through `tool_search`.
The plan compiler and speculative admission candidates remain offline-only.
