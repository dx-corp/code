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
