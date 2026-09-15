# Minimal initial tools: exploratory paired trial

Opt-in `MAESTRO_TOOL_PROFILE=minimal` starts with read/bash/edit/write/grep/glob/tool_search/ask_user. Existing explicit/native grants, external tools, and Fast's RLM discovery restriction remain intact. No default changes. Both experimental arms run the same frozen native binary; only MAESTRO_TOOL_PROFILE differs (`fast` versus `minimal`).

## Frozen screen, not a promotion decision

Twelve task pairs: four independent Rust behavior repairs, four long-log investigations with tenant decoys, and four explicit native grep/glob search cases. Fixed seed 891 shuffles tasks; arm order alternates. Report strata separately: explicit search tasks require the named tool and are not representative workload weights. The source author has seen the tasks. This is a development screen, not the 100-task independent holdout described in the research.

Repairs compile the final library separately from immutable controller-owned hidden tests; every expected test must actually pass. Broken/reference fixtures are verified before inference. Investigation answers require exact JSON types, keys, and values. Native search additionally requires a successful requested native tool call and matching evidence in its output. No required compiler-call count biases ordinary repairs. Raw outputs and model-visible truncation are unchanged in both arms.

Primary screening outcomes: final correctness and complete cached/uncached/output token totals per attempted task; report runtime dollar cost only when present, with cached/uncached/output token totals, provider/tool/discovery calls, and stratum breakdowns. A candidate-only task loss or cost regression rejects an unconditional rollout. A 15% cost reduction without observed task losses would justify a larger held-out trial, not establish noninferiority. Cost is runtime-reported, not an invoice. Missing usage is unavailable, not zero. The runner first qualifies one repair and one native-search task in both arms. All four must succeed with complete token receipts. Qualification is separate from the cohort and warms caches. Each task gets one runtime attempt; native identity verification retries transient failures once. A cohort infrastructure/usage failure stops further work after the current pair; retain every attempt and list unrun cases. Do not report an interrupted cohort as a completed comparison. Model-quality failures with complete execution remain in the denominator and do not stop the cohort. No model answer is retried. Timing is descriptive; other host/provider activity is not fully controlled.

Normal model credentials are used. The controller grants only workspace answer/source writes and the documented narrow shell commands; both arms have identical permissions. Hidden grading files are outside the workspace, but this is benchmark separation, not a hostile-code security sandbox. Treat raw event artifacts as private.

```sh
python3 -m unittest discover -s products/maestro/scripts/experiments/minimal_tools -v
python3 products/maestro/scripts/experiments/minimal_tools/trial.py \
  --binary /absolute/path/to/frozen/maestro \
  --output /absolute/path/to/new-output \
  --model evalops/accounts/fireworks/models/glm-5p3-flash --live
```

Omit --live to validate fixtures only. The output directory must not exist. The manifest, including binary hash, all cases, prompts, and run order, is written before inference. Reconcile recorded response_end usage and tool_end receipts before reporting results.

## Evidence suite (v3)

The original `screen` remains the same 12 cases. `--suite adversarial` runs the
four executable Rust repairs plus eight distinct evidence tasks: tenant decoys,
out-of-order revisions, embedded instructions, Unicode identities, missing
records, exact JSON types, cross-file joins, and long-output middle evidence.
These are author-visible development cases, **not an independent holdout**.
They do not simulate changing files between tool calls or context compaction;
those require a separate runtime treatment evaluation.

Validate either suite without credentials, inference, or a native binary:

```sh
python3 products/maestro/scripts/experiments/minimal_tools/trial.py \
  --suite adversarial --output /absolute/path/to/new-fixture-output
```

For inference, add `--live --binary /absolute/path/to/frozen/maestro` and the
intended `--model`. Qualification and stop-on-infrastructure-failure behavior
remain the same. Both arms retain identical permissions. This uses the existing
Fast/Minimal treatment; it does not enable tool-result reuse.

Before execution the v3 manifest freezes the suite, model, binary hash, compiler
version, analysis plan, and all runner/grader/report source hashes. Keep that
source revision with the artifacts. Reanalysis requires matching sources and
recompiles final repair files against controller-owned tests; it rechecks exact
JSON answers, original evidence files, and required native tool receipts.
Changing a success summary alone cannot change the result. These are local
artifact consistency checks, not a signed or hostile-code proof system.

The report derives provider-call counts from response starts and checks terminal
markers, failure events, usage, unique result rows, and planned artifact paths.
Missing or interrupted cohorts produce an explicit list of unrun arms and no
comparative intervals. A failed reanalysis removes the old report so stale
success cannot masquerade as the current result.

The primary descriptive measure is total spend across **all attempted tasks**
divided by verified successful tasks. Failed-task spend stays in the numerator.
Zero successes, missing prices, or incomplete usage produce unavailable values.
Total tokens include uncached input, cache reads, cache writes, and output.
A missing cache-write field makes token totals unavailable; older summary rows
can recover that field from their raw events. Report success
rate, candidate-only losses, and elapsed time alongside efficiency.

Exploratory intervals resample whole task pairs with a fixed seed. Cost/token
per-success intervals become unavailable if any resample has zero successes;
they never condition on surviving resamples. There are no population guarantees,
noninferiority claims, repeated-peeking decisions, or automatic promotion.
Gateway reconciliation, production outcome joins, an independently authored
holdout, and a powered stopping plan remain separate work.

Run all local regression tests (including real Rust grading):

```sh
python3 -m unittest discover -s products/maestro/scripts/experiments/minimal_tools -v
python3 -m unittest discover -s products/maestro/scripts/experiments/tool_compression -v
```

The component's Rust test lane runs both commands without inference credentials.
