# Minimal initial tools: exploratory paired trial

Opt-in `MAESTRO_TOOL_PROFILE=minimal` starts with read/bash/edit/write/grep/glob/tool_search/ask_user. Existing explicit/native grants, external tools, and Fast's RLM discovery restriction remain intact. No default changes. Both experimental arms run the same frozen native binary; only MAESTRO_TOOL_PROFILE differs (`fast` versus `minimal`).

## Frozen screen, not a promotion decision

Twelve task pairs: four independent Rust behavior repairs, four long-log investigations with tenant decoys, and four explicit native grep/glob search cases. Fixed seed 891 shuffles tasks and independently shuffles each pair's arm order. Older artifacts used alternating arm order; their frozen manifests remain authoritative. Report strata separately: explicit search tasks require the named tool and are not representative workload weights. The source author has seen the tasks. This is a development screen, not the 100-task independent holdout described in the research.

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
Production outcome joins, an independently authored holdout, and a powered
stopping plan remain separate work. Latency intervals use the same paired
resamples. Conservative Bonferroni Wilson bounds for the quality difference
remain nonzero even when both arms pass every task; a zero-width empirical
bootstrap interval is not evidence of equivalence.

Run all local regression tests (including real Rust grading):

```sh
python3 -m unittest discover -s products/maestro/scripts/experiments/minimal_tools -v
python3 -m unittest discover -s products/maestro/scripts/experiments/tool_compression -v
```

The component's Rust test lane runs both commands without inference credentials.

## Larger follow-up and gateway costs

`--suite followup --order-seed 20260915` runs 24 new author-visible evidence
tasks. This changes the task mix; compare Fast and Minimal within the cohort,
not the old cohort against the new one. These tasks are still exploratory, not
an independently authored holdout. The model, binary, settings and permissions
are fixed across arms. The manifest records the randomized order before any call.

`billing.py` exports a read-only, organization/workspace-scoped SQL query over
all observed gateway lineages. Run it using the existing authorized database
connection; it does not manage credentials or mutate production. Querying whole
lineages includes extra gateway calls rather than dropping them because they
are absent from the final response. Keep the private export with the raw study.

```sh
python3 products/maestro/scripts/experiments/minimal_tools/billing.py RUN_DIR \
  --organization ORG --workspace WORKSPACE --write-query query.sql
psql -X -qAt -v ON_ERROR_STOP=1 -f query.sql > gateway-records.json
python3 products/maestro/scripts/experiments/minimal_tools/billing.py RUN_DIR \
  --organization ORG --workspace WORKSPACE --records gateway-records.json \
  --rates rates.json --output cost-analysis.json
```

Rates require provider, exact model, source URL, observation date,
`basis: published_list_price_estimate`, and `usd_per_million` entries for
`input_tokens`, `cache_read_tokens`, `cache_write_tokens`, and `output_tokens`.
A missing rate with positive usage makes the estimate unavailable. Decimal
arithmetic avoids rounding each request to a whole cent. Failed task spend
stays in cost per successful task. Missing records, duplicate IDs, tenant or
identity mismatches, and incomplete provider-attempt coverage cannot produce a
complete estimate. Final answers are regraded independently before accounting.

The report separates published-list estimates, configured gateway recorded
costs, and billed spend. Unconfigured zero prices remain unavailable. Provider manifests enumerate gateway retries and failovers, but a single
terminal usage envelope does not price earlier failed attempts. Gateway costs
also do not establish account-specific discounts or invoice rounding.
Therefore `billed_cost_usd` and invoice verification remain unavailable until
authoritative provider billing/attempt evidence closes those gaps. Qualification
calls are exported and priced separately as study overhead; they never enter
the cohort success denominator. Do not sum
gateway records and their Meter/Platform mirrors: those are the same usage.

If an infrastructure stop is investigated and the decision is to finish the
remaining planned tasks, `continue_study.py RUN_DIR --binary BINARY --reason TEXT`
records an explicit protocol amendment before resuming. It checks frozen source
and binary hashes, requires existing rows to be an exact prefix of the planned
order, retains every failed attempt, and never retries an existing answer. The
original stop record is preserved. This is an exploratory amendment; cite it
whenever reporting the resulting cohort. `billing.py` includes it in its report.

When some spend is missing, the verified token samples still supply a declared
list-price lower bound, assuming unobserved token charges are nonnegative. Full
cost remains unavailable. If Minimal has complete spend and Fast has a positive
known-cost lower bound, the report can bound the Minimal/Fast cost-per-success
ratio from above. This deterministic bound is not a confidence interval, does
not impute a zero cost, and is never presented as account billing proof.

Optional `--timeline-logs` accepts a private Cloud Logging JSON export. A missing
usage envelope is classified as verified non-execution only when the failed
503/http_error ledger row has no provider attempts or usage body, and its exact
request/tenant timeline contains the complete ordered accepted phase prefix
ending in rejected `governance_input` or `budget`. Both stages precede provider
dispatch. Missing, truncated, conflicting, or post-dispatch evidence stays
unavailable. Report these requests separately; never treat an executed stream
without usage as zero cost.

## Verification mode

The verification entrypoint adds settled billing reconciliation and separate
`billing_verified`, `cheaper_verified`, `faster_verified`, and
`quality_preserved` verdicts. These describe the frozen study only.
`production_verified` stays false. No experiment or default is auto-enabled.

Provide an independently authored holdout as a JSON array of cases with `id`,
`family: investigation`, `prompt`, `files` (relative paths to string contents),
and `expected` (the exact JSON answer object). The current external adapter
supports investigation cases; executable repairs remain in the built-in screen.
Use independent tasks, not renamed copies or repeated measurements treated as
independent samples. Independence and realistic workload coverage need review;
the harness records the supplied provenance but cannot authenticate them.

Before inference, create a plan:

```json
{
  "schema": "maestro.verification-plan.v1",
  "pairs": 100,
  "minimum_cost_reduction": 0.05,
  "minimum_speed_reduction": 0.05,
  "maximum_quality_loss": 0.02,
  "sample_size_rationale": "Replace with a prospective power analysis for the chosen margins and task mix.",
  "holdout_provenance": "Identify the independent task author and frozen dataset revision.",
  "cache_policy": "Qualification warms caches; use the same policy in both arms.",
  "time_window": "Record the scheduled measurement window and host conditions.",
  "stopping_rule": "fixed_sample_no_optional_stopping"
}
```

The sample count is illustrative, not a claim that 100 pairs can establish a
2% quality margin. Choose it prospectively using pilot variance and the quality
margin; tight margins can require many more tasks. Do not keep adding tasks
until the report passes. A stopped or subsequently amended cohort cannot pass
verification. Each task still gets one answer attempt; all failures remain in
the denominator. A separate study is required after changing the plan.

```sh
python3 products/maestro/scripts/experiments/minimal_tools/trial.py \
  --holdout holdout.json --verification-plan plan.json \
  --binary /absolute/path/to/maestro --output /absolute/path/to/new-study \
  --model MODEL --live
python3 products/maestro/scripts/experiments/minimal_tools/verification.py RUN_DIR \
  --organization ORG --workspace WORKSPACE --output verification.json
```

The manifest freezes the plan, dataset, execution order, model, binary, and
analysis sources before inference. Controller-owned `timing.json` records
monotonic event arrivals, prompt submission and total elapsed time, bound to the
raw event hash. Total time includes initialization and shutdown, excluding
fixture creation and grading. It retains failed-attempt latency. Arrival times
support diagnosis of provider versus tool delays; they are not server-side
compute durations. Reanalysis regrades answers and validates timing evidence.

### Settled billing input

The authorized billing owner supplies a **trusted normalized export**, retaining
its original provider export privately. This importer validates consistency; it
does not contact the provider, authenticate a file, or prove that a manually
written mapping matches an invoice. Never feed model-authored billing evidence
into this boundary. `billing_verified` means reconciled against that supplied
settled export, not independently authenticated provider billing.

The normalized JSON has:

- `schema: maestro.settled-billing.v1`, `basis: settled_provider_charges`,
  `currency: USD`, `settled: true`, and `complete_scope: true`.
- Exact `organization_id`, `workspace_id`, `provider_account`,
  `source_reference`, timezone-qualified `period_start` and `period_end`, and
  `export_sha256` for the original provider export.
- `scoped_total_usd` as a nonnegative decimal string and `lines` containing one
  settled net charge per attempt, including retries, failures and qualification.
- Each line has `line_id`, `request_id`, `record_id`, `attempt_ordinal`,
  `provider`, `provider_request_id`, and decimal-string `net_charge_usd`.
  The owner must reconcile discounts, credits and rounding into each net charge;
  negative adjustments or unattributable account fees are not supported.
- Requests with no provider attempt require one line with
  `attempt_ordinal: null`, `net_charge_usd: "0"`, and
  `non_execution_confirmed: true`. An absent charge is never assumed free.

Use `billing.py --write-query` above to export the gateway lineage records.
The importer checks all native receipt identities and all gateway attempts,
including extra calls absent from the final response. Duplicate, missing,
unattributed, mismatched or nonsettled charges fail without leaving an old
verification report. Qualification spend is reported separately.

```sh
python3 products/maestro/scripts/experiments/minimal_tools/verification.py RUN_DIR \
  --organization ORG --workspace WORKSPACE --records gateway-records.json \
  --billing settled-billing.json --provider-export original-provider-export.csv \
  --output verification.json
```

Aggregate invoices without reliable per-attempt attribution cannot pass this
adapter. Isolated account/window billing needs a separately reviewed adapter;
do not spread an aggregate amount across requests. Current gateway exports do
not supply provider request IDs, so the billing owner must supply and audit that
mapping. This is an external evidence dependency, not a zero-cost fallback.

### Interpretation

The fixed analysis uses paired percentile bootstrap intervals for billed cost
per successful task and median end-to-end latency. Each endpoint uses
alpha=0.05/3; quality uses conservative Wilson bounds at the same error budget.
This adjusts for the three reported endpoints, but bootstrap coverage remains
approximate. It does not correct repeated peeking, dependent tasks or an
unrepresentative holdout. A sample with zero successes or zero baseline spend
makes the cost interval unavailable rather than discarding that resample.

Both efficiency claims require the declared quality-loss bound to pass. Speed
can pass without billing; billed savings cannot. False verdicts mean not
established and do not necessarily mean regression. Source changes, protocol
amendments, incomplete cohorts, missing timing or missing billing evidence are
reported or rejected explicitly. Old development studies are not retroactively
promoted by attaching a plan after execution.
